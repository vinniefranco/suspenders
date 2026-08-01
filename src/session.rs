//! The Session's fixed facts (CONTEXT.md: Session), resolved and validated
//! once at launch: the Project Root, the Provider set, the launch Model, the
//! sampling temperature, the budget-cap knob, the Compaction slack, the
//! Compaction Keep, the Run Limit, the loop-detector stall limit, the
//! malformed-retry budget, the tool-call style, the command timeout, the
//! Extension list, and the LLM module. The Context Budget and the Result Cap
//! are NOT fixed facts: they derive from the Model each Run captures
//! (ADR-0037).
//!
//! This is the composition seam for configuration. [`Session::new`] resolves
//! these keys once (via [`SessionConfig::compose`]) by overlaying, in order, the
//! hardcoded [`SessionConfig::base`] defaults, the user's `config.json`, the
//! workspace `.suspenders/config.json` (ADR-0031, ADR-0065 Phase B), and the
//! `SUSPENDERS_*` environment (the files are the persistent baseline, workspace
//! shadowing user; the environment still wins per-invocation over both);
//! everything downstream receives values from this struct, so the cross-module
//! invariants live in one place:
//!
//! * the Context Budget, the reply reserve, and the Result Cap are NOT
//!   fixed facts (ADR-0037): they derive from the Model each Run captures,
//!   through [`Session::context_budget_for`] and [`Session::tool_ctx`]
//! * a Model's reply reserve is its (already window-clamped) output cap
//!   clamped again to half the effective budget ([`Session::reply_reserve_for`]),
//!   so the budget math always leaves a live window; the one budget check at a
//!   `/model` swap is that the Compaction
//!   Keep fits below the trigger ([`Session::validate_model_budget`])
//!
//! The Provider set (ADR-0037) resolves here too: custom Providers from the
//! config `providers` table, built-ins from the generated Catalog with each one's
//! own environment key. The launch `model` (a scoped `provider/model-id`)
//! resolves against that set - an unknown Provider fails launch loudly.
//!
//! Tests use [`Session::build`] with an explicit [`SessionConfig`] (no env
//! reads beyond the built-in Providers' credential keys, which tests never
//! assert on), so the config-default behavior is exercised without touching
//! the process environment.

pub mod log;

use std::collections::BTreeMap;

use crate::conversation;
use crate::llm::model::{Api, Model};
use crate::llm::provider::Provider;
use crate::llm::{ToolCallStyle, catalog, model};
use crate::tool::ToolCtx;
use serde::{Deserialize, Serialize};

/// The Session's fixed facts.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    /// The Project Root (captured once, never read from the cwd again).
    pub root: String,
    /// The module implementing the LLM boundary (a name; the trait wiring is a
    /// later phase - carried here as a module name).
    pub llm_module: String,
    /// The Session's Extension list (opaque here; entries carried as names).
    pub extensions: Vec<String>,
    /// The config `context_budget` knob, reinterpreted (ADR-0037, ADR-0031
    /// amendment): an optional global cap on every Model's effective budget,
    /// and the window figure for Models the Catalog does not know. The budget
    /// itself is NOT a fixed fact - [`Session::context_budget_for`] derives it
    /// from the Model each Run captures.
    pub context_budget: Option<u64>,
    pub compaction_slack: f64,
    pub compaction_keep: f64,
    pub run_limit: u64,
    /// The loop-detector's stall Setpoint (the passive circuit breaker): how
    /// many Passes in a row the model may emit the IDENTICAL Tool Call batch
    /// before the Run is terminated. A small number (default
    /// [`DEFAULT_LOOP_STALL_LIMIT`]); `1` would trip on the first repeat.
    pub loop_stall_limit: u64,
    /// The malformed-tool-call re-draw Setpoint (ADR-0030): at most this many
    /// in-band re-draws may follow a retryable generation error within one
    /// Run. `0` disables the mechanic entirely (the loud failure runs
    /// immediately, as before).
    pub malformed_retry_budget: u64,
    /// Skips the next-speaker check (ADR-0043, qwen-code's
    /// `getSkipNextSpeakerCheck`): when `true`, a no-tool-call Pass finishes the
    /// Run as it did before the check existed - no continuation, no side-query.
    /// Default `true` (the check is skipped), matching qwen-code's default.
    pub skip_next_speaker: bool,
    pub command_timeout_ms: u64,
    pub session_dir: String,
    /// The managed-auto-memory root (P5, ADR-0062): where the model's memory
    /// files live, resolved once at launch via [`default_memory_root`]. Threaded
    /// into every [`ToolCtx`] so the shared path seam extends confinement to
    /// this subtree (the trust-path allowance), and read by `init_agent` to
    /// build the memory prompt suffix and mkdir the dir.
    pub memory_root: String,
    /// The resolved Provider set (ADR-0037): custom Providers from config,
    /// built-ins from the generated Catalog with their environment credentials.
    pub providers: Vec<Provider>,
    /// The launch-resolved Model - the Active Model's seed (ADR-0033
    /// amendment). The budget figures derive from whichever Model each Run
    /// captures, this one until a `/model` swap.
    pub model: Model,
    /// The configured Theme name (ADR-0038), carried unvalidated: the UI
    /// resolves it at launch and falls back to `dark` with a notice, so a
    /// bad name never fails launch.
    pub theme: String,
    /// The sampling temperature every request carries; `None` leaves sampling
    /// to the server's own defaults. Resolved once here, applied by the
    /// request-building callers (ADR-0037: temperature belongs to the request).
    pub temperature: Option<f64>,
    /// The extended-thinking token budget the MAIN conversation request carries
    /// (qwen-code parity): `Some(n)` arms `thinking: {type: "enabled",
    /// budget_tokens: n}` on the Anthropic wire, which keeps the local
    /// reasoning model producing a Thinking block THEN a Tool Call every turn.
    /// `None` disables it (the model may think and stop). Resolved once here,
    /// applied by the request-building caller like temperature; the
    /// checkNextSpeaker side-query's `no_think` suppresses it, and Compaction
    /// never receives it.
    pub thinking_budget: Option<u64>,
    /// How every request resolves Tool Calls the model emits (qwen parity):
    /// [`ToolCallStyle::Auto`] recovers a text-emitted call from the content
    /// channel when the structured one is empty; `Structured` opts out. Resolved
    /// once here, applied by the request-building callers.
    pub tool_call_style: ToolCallStyle,
    /// The output cap for Models the Catalog does not know (the config knob):
    /// the synthesis fallback when a scoped id resolves at `/model` time.
    pub max_tokens: u64,
    /// The configured MCP servers, keyed by user-chosen name (F8, ADR-0056):
    /// each an external tool server the Agent attaches once at startup. A
    /// file-only map like `providers` (structure the env cannot express). Empty
    /// when the user configures none. Each entry's transport is validated at
    /// build (a malformed entry is a LOUD launch failure); the attach itself is
    /// fail-open (a server that will not connect is skipped, not fatal). Merged
    /// across settings scopes at composition (Phase B, ADR-0065): a workspace
    /// entry shadows a user one of the same name.
    pub mcp_servers: BTreeMap<String, crate::mcp::McpServerConfig>,
    /// The disabled MCP server names (Phase B, ADR-0065): the concatenation of
    /// every scope's `mcp.excluded` list (qwen `MergeStrategy.CONCAT`). A server
    /// named here is shown in the `/mcp` dialog but never attached. File-only,
    /// like `mcp_servers` (the env cannot express the list).
    pub mcp_excluded: Vec<String>,
    /// Which settings scope declared each MCP server (Phase B, ADR-0065): the
    /// [`McpSource`](crate::mcp::McpSource) of the lowest scope that named it
    /// (workspace shadows user). Keyed like `mcp_servers`; a server absent here
    /// defaults to [`McpSource::User`](crate::mcp::McpSource::User) when the plan
    /// map is built.
    pub mcp_sources: BTreeMap<String, crate::mcp::McpSource>,
}

/// Raised (returned) when a Session's fixed facts fail validation. The message
/// carries the validation-failure text so callers can match on the reason.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SessionError(pub String);

/// The resolved config defaults a Session is built against.
/// [`SessionConfig::load`] composes it (base → file → env) and tests pass
/// [`SessionConfig::test_defaults`] explicitly (no file/env reads).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionConfig {
    /// Custom Providers, keyed by id (ADR-0031 amendment): file-only structure
    /// the env cannot express. A custom entry shadows a built-in with the
    /// same id.
    pub providers: BTreeMap<String, ProviderConfig>,
    /// The MCP servers, keyed by user-chosen name (F8, ADR-0056): a file-only
    /// map like `providers`. Default empty; MERGED across settings scopes at
    /// composition (Phase B, ADR-0065), a workspace entry shadowing a user one.
    pub mcp_servers: BTreeMap<String, crate::mcp::McpServerConfig>,
    /// The disabled MCP server names (Phase B, ADR-0065): every scope's
    /// `mcp.excluded` list concatenated (qwen `MergeStrategy.CONCAT`). Default
    /// empty; file-only, extended (not replaced) per scope by the composer.
    pub mcp_excluded: Vec<String>,
    /// Which scope declared each MCP server (Phase B, ADR-0065): the composer
    /// records the lowest scope that named it (workspace shadows user). Default
    /// empty; never file-settable directly - it is a byproduct of composition.
    pub mcp_sources: BTreeMap<String, crate::mcp::McpSource>,
    /// The scoped `provider/model-id` the launch Model resolves from.
    pub model: String,
    /// The configured Theme name (ADR-0038): a built-in (`dark`, `light`) or a
    /// user file's stem in the themes directory. Carried unvalidated - the UI
    /// resolves it at launch and falls back to `dark` with a notice, so a bad
    /// name never fails launch.
    pub theme: String,
    pub max_tokens: u64,
    pub temperature: Option<f64>,
    /// The extended-thinking budget the main request carries (qwen-code
    /// parity); `None` disables it. Env/file-settable like the scalars, with
    /// `0` (or empty) mapping to `None` to turn it off.
    pub thinking_budget: Option<u64>,
    /// The Tool Call resolution style every request carries (qwen parity):
    /// [`ToolCallStyle::Auto`] by default. Env/file-settable like the scalars.
    pub tool_call_style: ToolCallStyle,
    /// The optional global budget cap and catalog-less window figure
    /// (ADR-0037); `None` leaves every Model's own window uncapped.
    pub context_budget: Option<u64>,
    pub compaction_slack: f64,
    pub compaction_keep: f64,
    pub llm_module: String,
    pub command_timeout_ms: u64,
    pub run_limit: u64,
    pub loop_stall_limit: u64,
    pub malformed_retry_budget: u64,
    /// Skips the next-speaker check (ADR-0043); `false` runs it.
    pub skip_next_speaker: bool,
    pub extensions: Vec<String>,
    pub session_dir: String,
}

// ---- Base-config defaults (named constants so magic numbers appear once) ----

/// The default output cap (max_tokens) every request sends.
const DEFAULT_MAX_TOKENS: u64 = 8_000;

/// The default sampling temperature: a mild value that avoids both the
/// deterministic floor and the high-entropy ceiling.
const DEFAULT_TEMPERATURE: f64 = 0.7;

/// The default extended-thinking budget (qwen-code sends exactly this): arming
/// the thinking param keeps the local reasoning model producing a Thinking
/// block THEN a Tool Call every turn, rather than thinking and stopping.
const DEFAULT_THINKING_BUDGET: u64 = 32_000;

/// The default Compaction slack: the headroom below the Context Budget, as a
/// fraction of it, that lowers the compaction target.
const DEFAULT_COMPACTION_SLACK: f64 = 0.2;

/// The default Compaction Keep fraction.
const DEFAULT_COMPACTION_KEEP: f64 = 0.5;

/// The default command timeout in milliseconds (2 minutes).
const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 120_000;

/// The default Run Limit (maximum Passes / turns per user request). Sized for a
/// real multi-step task under the Governor-free ReAct loop: qwen-code completed
/// a task in ~41 turns and its own session-turn ceiling is ~100, so 100 leaves
/// a legitimate task uncut while still bounding a runaway. A config knob - the
/// loop-detector (`loop_stall_limit`) catches a stuck model well before this.
const DEFAULT_RUN_LIMIT: u64 = 100;

/// The default loop-detector stall limit: how many Passes in a row the model
/// may emit the IDENTICAL Tool Call batch before the Run is terminated.
const DEFAULT_LOOP_STALL_LIMIT: u64 = 5;

/// The default malformed-tool-call re-draw budget per Run.
const DEFAULT_MALFORMED_RETRY_BUDGET: u64 = 3;

/// The valid temperature range upper bound (inclusive).
const TEMPERATURE_MAX: f64 = 2.0;

/// The valid compaction-slack range upper bound (exclusive).
const FRACTION_UPPER_BOUND: f64 = 1.0;

/// The valid fraction range lower bound (inclusive for left-closed, exclusive
/// for open intervals).
const FRACTION_LOWER_BOUND: f64 = 0.0;

impl SessionConfig {
    /// The base config the app ships: the `local` custom Provider carrying the
    /// out-of-the-box endpoint (local servers speak the Anthropic protocol
    /// today, ADR-0002), and the default model scoped to it.
    pub fn base() -> Self {
        SessionConfig {
            providers: BTreeMap::from([(
                "local".to_string(),
                ProviderConfig {
                    base_url: "http://localhost:8888/v1".into(),
                    api: Api::AnthropicMessages,
                    // No shipped window: the local server reports its REAL
                    // loaded window at discovery, and enrichment (ADR-0037)
                    // makes that authoritative. Force-setting a figure here
                    // would shadow the server value; `fallback_window` still
                    // covers the server-silent case.
                    context_window: None,
                    token: None,
                },
            )]),
            // No MCP servers out of the box (F8, ADR-0056): the user adds them
            // by hand. Empty means the Agent attaches none.
            mcp_servers: BTreeMap::new(),
            // No exclusions and no recorded sources at base (Phase B, ADR-0065):
            // both fill in as the composer merges the settings scopes.
            mcp_excluded: Vec::new(),
            mcp_sources: BTreeMap::new(),
            model: "local/qwen/Qwen3.6-27B-MTP-GGUF".into(),
            theme: "dark".into(),
            max_tokens: DEFAULT_MAX_TOKENS,
            temperature: Some(DEFAULT_TEMPERATURE),
            // Armed by default (qwen-code parity): the main request carries the
            // thinking budget so the local reasoning model stays on the rails.
            thinking_budget: Some(DEFAULT_THINKING_BUDGET),
            // Auto by default: a text-emitted Tool Call is recovered, nothing
            // changes for a host whose structured channel already works.
            tool_call_style: ToolCallStyle::Auto,
            // No global cap by default: every Model's own window is its
            // budget, so a wide-window Catalog model works out of the box.
            context_budget: None,
            compaction_slack: DEFAULT_COMPACTION_SLACK,
            compaction_keep: DEFAULT_COMPACTION_KEEP,
            llm_module: "Suspenders.LLM".into(),
            command_timeout_ms: DEFAULT_COMMAND_TIMEOUT_MS,
            run_limit: DEFAULT_RUN_LIMIT,
            loop_stall_limit: DEFAULT_LOOP_STALL_LIMIT,
            malformed_retry_budget: DEFAULT_MALFORMED_RETRY_BUDGET,
            // The next-speaker check is SKIPPED by default (ADR-0043, matching
            // qwen-code's `skipNextSpeakerCheck` default of true): a no-tool-call
            // Pass finishes the Run without the side-query.
            skip_next_speaker: true,
            extensions: vec![
                "diff".into(),
                "run_shell_command".into(),
                "condense".into(),
                "todo".into(),
            ],
            session_dir: default_session_dir(),
        }
    }

    /// The config the test env resolves to: fakes injected, empty extension
    /// list, tmp session dir. The next-speaker check is skipped here (ADR-0043,
    /// now the base default too) so the loop and agent tests exercise the tool
    /// loop without a side-query firing on every text reply; the check's own
    /// behavior is covered by the tests that opt back in
    /// (`skip_next_speaker: Some(false)`). Set explicitly so the intent stays
    /// legible even though it matches `base`.
    pub fn test_defaults() -> Self {
        let mut cfg = SessionConfig::base();
        cfg.providers
            .get_mut("local")
            .expect("base ships local")
            .base_url = "http://localhost:0/v1".into();
        cfg.llm_module = "Suspenders.FakeLLM".into();
        cfg.extensions = vec![];
        cfg.skip_next_speaker = true;
        cfg.session_dir = std::env::temp_dir()
            .join("suspenders_test_sessions")
            .to_string_lossy()
            .into_owned();
        cfg
    }

    /// The composition entry (ADR-0031): [`base`](Self::base) defaults overlaid
    /// by the user's `config.json`, then by the `SUSPENDERS_*` environment.
    /// [`try_load`](Self::try_load) is the fallible form; this is the panicking
    /// convenience over it.
    pub fn load() -> Self {
        SessionConfig::try_load().expect("invalid suspenders configuration (file or SUSPENDERS_*)")
    }

    /// The fallible composition entry over the user scope alone: `base()` →
    /// user `config.json` overlay → env overlay (ADR-0031). [`Session::new`]
    /// calls [`compose`](Self::compose) directly to add the workspace scope; this
    /// stays the no-workspace convenience the panicking [`load`](Self::load) and
    /// tests reach for.
    pub fn try_load() -> Result<Self, SessionError> {
        SessionConfig::compose(&default_config_path(), None)
    }

    /// The scope-aware composition (Phase B, ADR-0065): `base()` overlaid by the
    /// user file, then the workspace file, then the `SUSPENDERS_*` environment,
    /// returning the reason on the first malformed value (a bad file, a bad env
    /// var). The files are the persistent baseline; the environment still wins
    /// per-invocation over both, so it is applied LAST (ADR-0031's env-over-file
    /// precedence, unchanged).
    ///
    /// The two settings scopes compose per ADR-0031's workspace-beats-user rule,
    /// but the two MCP fields do NOT simply replace: `mcp_servers` merges by key
    /// (a later scope's entry shadows an earlier one, and its Source is recorded)
    /// and `mcp_excluded` CONCATENATES (qwen `MergeStrategy.CONCAT`). Everything
    /// else is a plain scalar overlay through [`FileConfig::apply`], so a later
    /// scope replaces it. An absent scope is an empty overlay (no error); a
    /// present-but-malformed file is an error naming its path.
    pub fn compose(
        user_path: &str,
        workspace_path: Option<&str>,
    ) -> Result<SessionConfig, SessionError> {
        let mut cfg = SessionConfig::base();
        // Sources accumulate as each scope's servers land, so the LAST scope to
        // name a server owns its Source (workspace shadows user).
        let mut sources: BTreeMap<String, crate::mcp::McpSource> = BTreeMap::new();

        // User first, workspace second: the scalar overlay makes a later scope
        // win, and the MCP merge makes a later scope's server + Source shadow the
        // earlier one, per ADR-0031's workspace-beats-user precedence.
        let scopes = [
            (Some(user_path), crate::mcp::McpSource::User),
            (workspace_path, crate::mcp::McpSource::Workspace),
        ];
        for (path, source) in scopes {
            let Some(path) = path else { continue };
            let Some(file) = read_file_config(path)? else {
                continue;
            };
            // The scalars/providers replace (later scope wins); the two MCP
            // fields merge, so they are landed by hand rather than by `apply`.
            file.apply(&mut cfg);
            if let Some(servers) = &file.mcp_servers {
                for (name, server) in servers {
                    cfg.mcp_servers.insert(name.clone(), server.clone());
                    sources.insert(name.clone(), source);
                }
            }
            if let Some(excluded) = &file.mcp_excluded {
                cfg.mcp_excluded.extend(excluded.iter().cloned());
            }
        }
        cfg.mcp_sources = sources;

        // Env LAST so it wins per-invocation over both files (ADR-0031). The
        // env-over-file precedence is not unit-tested here: the env seam is
        // covered through `apply_env` directly - see the env-overlay tests, which
        // lean on nextest's process-per-test isolation.
        SessionConfig::apply_env(&mut cfg)?;
        Ok(cfg)
    }

    /// The env-overlay step (formerly `try_from_env`): reads each `SUSPENDERS_*`
    /// override and validates it, overlaying present values onto `cfg` and
    /// returning the reason on the first malformed value. A malformed value is a
    /// hard error (carried as a [`SessionError`]) rather than a silent fallback.
    ///
    /// This and [`FileConfig`] are the two serializations of one schema; a new
    /// user-tunable knob is added to both seams or to neither (ADR-0031: "the
    /// file and env seams must be kept in lockstep"). [`ENV_OVERRIDES`] IS the
    /// env half of that lockstep: one row per knob, walked in row order.
    fn apply_env(cfg: &mut SessionConfig) -> Result<(), SessionError> {
        for (name, set) in ENV_OVERRIDES {
            if let Ok(v) = std::env::var(name) {
                set(cfg, &v)?;
            }
        }
        Ok(())
    }

    /// Resolves a `--write-config` target for [`write_template`]: an empty
    /// path means the XDG default ([`default_config_path`]), anything else is
    /// used verbatim. Lives here so the empty-means-default rule is a config
    /// seam fact (ADR-0031), not a branch in `main`.
    ///
    /// [`write_template`]: Self::write_template
    pub fn resolve_template_path(path: &str) -> String {
        if path.is_empty() {
            default_config_path()
        } else {
            path.to_string()
        }
    }

    /// Writes a fully-populated `config.json` template to `path` from the
    /// [`base`](Self::base) defaults (ADR-0031): every schema key present and
    /// self-documenting, EXCEPT `token`, which stays absent so no secret is ever
    /// persisted by the tool. Refuses an existing target unless `force`, creates
    /// parent dirs as needed, and serializes pretty.
    pub fn write_template(path: &str, force: bool) -> Result<(), SessionError> {
        if !force && std::path::Path::new(path).exists() {
            return Err(SessionError(format!(
                "refusing to overwrite existing config at {path} (pass --force to replace it)"
            )));
        }

        let base = SessionConfig::base();
        let template = FileConfig {
            // Per-provider tokens are deliberately absent from base(): the
            // template never persists a secret.
            providers: Some(base.providers),
            // The MCP servers map (F8, ADR-0056): base ships none, so this is an
            // empty map in the template - the key is present + self-documenting,
            // the user fills it with `command`/`http_url` entries by hand.
            mcp_servers: Some(base.mcp_servers),
            // The disabled-server list (Phase B, ADR-0065): base excludes none,
            // so this is an empty array in the template - present so a user knows
            // the `/mcp` dialog's disable toggle has a home here.
            mcp_excluded: Some(base.mcp_excluded),
            model: Some(base.model),
            theme: Some(base.theme),
            max_tokens: Some(base.max_tokens),
            temperature: base.temperature,
            thinking_budget: base.thinking_budget,
            tool_call_style: Some(base.tool_call_style),
            // Absent from the template on purpose (ADR-0037): the base config
            // carries no global cap, and baking one in would pin wide-window
            // Catalog models under it.
            context_budget: base.context_budget,
            compaction_slack: Some(base.compaction_slack),
            compaction_keep: Some(base.compaction_keep),
            loop_stall_limit: Some(base.loop_stall_limit),
            malformed_retry_budget: Some(base.malformed_retry_budget),
            skip_next_speaker: Some(base.skip_next_speaker),
        };

        let json = serde_json::to_string_pretty(&template)
            .map_err(|e| SessionError(format!("failed to serialize config template: {e}")))?;

        if let Some(parent) = std::path::Path::new(path).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                SessionError(format!("failed to create config directory {parent:?}: {e}"))
            })?;
        }

        std::fs::write(path, json)
            .map_err(|e| SessionError(format!("failed to write config to {path}: {e}")))
    }

    /// Persists the Active Model choice by a sparse read-modify-write of the
    /// config file (ADR-0033, ADR-0031 amendment): only the `"model"` key is
    /// set, through the shared [`persist_key`] machinery.
    pub fn persist_model(path: &str, model: &str) -> Result<(), SessionError> {
        persist_key(path, "model", model)
    }

    /// Persists the active Theme choice (ADR-0038): only the `"theme"` key is
    /// set - `/theme` shares `/model`'s sanctioned create-if-absent exception,
    /// through the same [`persist_key`] machinery.
    pub fn persist_theme(path: &str, theme: &str) -> Result<(), SessionError> {
        persist_key(path, "theme", theme)
    }

    /// Scope-aware sparse persist for the `/mcp` dialog's enable/disable toggle
    /// (Phase B, ADR-0065): sets only the `"mcp_excluded"` array in the given
    /// scope's config file, preserving every other key. `path` names the scope
    /// (user or workspace config), so a disable can land in whichever scope the
    /// dialog targets; the file is created if absent, the same sanctioned
    /// create-if-absent exception `/model` and `/theme` take. Shares the atomic
    /// write-then-rename with the string persists through [`persist_json_key`].
    pub fn persist_excluded(path: &str, names: &[String]) -> Result<(), SessionError> {
        let value = serde_json::Value::Array(
            names
                .iter()
                .map(|n| serde_json::Value::String(n.clone()))
                .collect(),
        );
        persist_json_key(path, "mcp_excluded", value)
    }
}

/// The sparse sticky write behind [`SessionConfig::persist_model`] and
/// [`SessionConfig::persist_theme`] (ADR-0033, ADR-0038): the user's other
/// keys are preserved and `token` is never introduced by the tool. This is
/// the one sanctioned exception to ADR-0031's no-auto-create - an explicit
/// pick is a deliberate act, so the file is created if absent. A thin string
/// front over [`persist_json_key`].
fn persist_key(path: &str, key: &str, value: &str) -> Result<(), SessionError> {
    persist_json_key(path, key, serde_json::Value::String(value.to_string()))
}

/// The sparse sticky write for a JSON `value` (ADR-0033, ADR-0038; ADR-0065
/// Phase B's `mcp_excluded`): reads the file, sets only `key`, and writes it
/// back through the atomic write-then-rename below, preserving every other key.
/// If `path` exists it is parsed as a JSON object and only `key` is set; if
/// absent, a `{"<key>": ...}` file (and its parent dirs) is created. Malformed
/// existing JSON is an [`Err`] naming `path` (mirroring [`read_file_config`]'s
/// error style). Splits into the pure [`merge_json_key`] and this thin impure
/// reader/writer, the same split as [`FileConfig::parse`] vs [`read_file_config`].
fn persist_json_key(path: &str, key: &str, value: serde_json::Value) -> Result<(), SessionError> {
    let existing = match std::fs::read_to_string(path) {
        Ok(raw) => Some(raw),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(SessionError(format!(
                "failed to read config at {path}: {e}"
            )));
        }
    };

    let json = merge_json_key(existing.as_deref(), key, value)
        .map_err(|e| SessionError(format!("invalid config at {path}: {e}")))?;

    if let Some(parent) = std::path::Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| {
            SessionError(format!("failed to create config directory {parent:?}: {e}"))
        })?;
    }

    // Write-then-rename, never in place: a crash mid-write must not leave a
    // torn config.json. The temp file sits in the SAME directory, because a
    // same-directory rename is atomic on POSIX (a cross-filesystem one is
    // not even a rename).
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, json)
        .map_err(|e| SessionError(format!("failed to write config to {tmp}: {e}")))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| SessionError(format!("failed to write config to {path}: {e}")))
}

/// Pure sparse merge of one JSON `key` into a config file's JSON (ADR-0033,
/// ADR-0038; ADR-0065 Phase B): `existing` is the current file contents (or
/// `None` when absent), and the result is the pretty JSON to write back. Every
/// other key is preserved; a `token` key is never introduced (only the caller's
/// existing one, if any, survives). A malformed or non-object existing file is
/// an [`Err`] carrying the path-agnostic reason (the caller wraps it with the
/// resolved path). Path-free and side-effect-free, so it unit-tests with
/// literals like [`FileConfig::parse`].
fn merge_json_key(
    existing: Option<&str>,
    key: &str,
    value: serde_json::Value,
) -> Result<String, String> {
    let mut root = match existing {
        None => serde_json::Value::Object(serde_json::Map::new()),
        Some(raw) => serde_json::from_str(raw).map_err(|e| e.to_string())?,
    };

    let obj = root
        .as_object_mut()
        .ok_or_else(|| "config root must be a JSON object".to_string())?;
    obj.insert(key.into(), value);

    serde_json::to_string_pretty(&root).map_err(|e| e.to_string())
}

/// One custom Provider's config entry (ADR-0031 amendment, ADR-0037): the
/// host's endpoint, the Api its Models speak, the window its Models
/// synthesize from, and an optional credential. File-only - structure the env
/// cannot express - and `deny_unknown_fields` like its parent.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub base_url: String,
    pub api: Api,
    /// The window this Provider's Models synthesize from. Absent, the global
    /// `context_budget` figure supplies it (a present entry beats the global
    /// figure for this Provider's models - the ADR-0037 precedence).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// The credential; absent means none (local servers ignore it). Never
    /// written by the tool - the user adds it by hand.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

/// The user config file's schema (ADR-0031): the env-settable scalar knobs
/// plus the file-only `providers` table (ADR-0037 narrowed the lockstep rule
/// to the scalars). Every field `Option<T>` so an absent key is an empty
/// overlay. The deliberately excluded fields (`session_dir`, `llm_module`,
/// `turn_limit`, `extensions`) are simply
/// absent, so `deny_unknown_fields` rejects them for free - as it now rejects
/// the retired flat `base_url` and `token` keys.
///
/// The scalar fields and [`SessionConfig::apply_env`] are the two
/// serializations of one schema; a new user-tunable knob is added to both
/// seams or to neither (ADR-0031: "the file and env seams must be kept in
/// lockstep").
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    providers: Option<BTreeMap<String, ProviderConfig>>,
    /// The MCP servers map (F8, ADR-0056): the Suspenders-native snake_case key
    /// is `mcp_servers`. qwen-code names the same map `mcpServers` (camelCase);
    /// the divergence is deliberate - a config port stays in Suspenders' idiom.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mcp_servers: Option<BTreeMap<String, crate::mcp::McpServerConfig>>,
    /// The per-scope disabled-server list (Phase B, ADR-0065): the snake_case
    /// flattening of qwen's `mcp.excluded`, matching how `mcpServers` became
    /// `mcp_servers`. Unlike a scalar, this MERGES across scopes (concatenation),
    /// so the composer - not [`apply`](FileConfig::apply) - lands it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mcp_excluded: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    theme: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
    /// Presence sets `Some`; the file cannot null it out (same limitation as the
    /// env seam).
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    /// The extended-thinking budget (qwen-code parity). Presence sets `Some`;
    /// like the env seam, the file cannot null it out - set the env's `0` to
    /// disable it per-invocation instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_budget: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_style: Option<ToolCallStyle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_budget: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compaction_slack: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compaction_keep: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    loop_stall_limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    malformed_retry_budget: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skip_next_speaker: Option<bool>,
}

impl FileConfig {
    /// Pure parse of the config file's JSON (ADR-0031): syntax errors, unknown
    /// keys, and type mismatches each surface as a [`SessionError`]. The message
    /// is path-agnostic (the caller, [`read_file_config`], wraps it with the
    /// resolved path). Range checks are NOT done here - a bad-but-typed value is
    /// caught later by `validate()` on the final [`Session`].
    fn parse(raw: &str) -> Result<FileConfig, SessionError> {
        serde_json::from_str(raw).map_err(|e| SessionError(e.to_string()))
    }

    /// Overlays only the present (`Some`) SCALAR/provider fields onto `cfg`;
    /// absent fields leave `cfg` untouched. One [`overlay`]/[`overlay_opt`] call
    /// per key, so the present-wins branch exists (and is covered) once, not once
    /// per field. The two MCP fields (`mcp_servers`, `mcp_excluded`) are handled
    /// by [`SessionConfig::compose`], NOT here: they MERGE across settings scopes
    /// (map-union / concatenation, Phase B ADR-0065) rather than replace, so a
    /// plain overlay would wrongly drop the earlier scope's entries. Private on
    /// purpose: a `SessionConfig` mutated this way must still pass through
    /// `validate()` on the built [`Session`].
    fn apply(&self, cfg: &mut SessionConfig) {
        overlay(&self.providers, &mut cfg.providers);
        overlay(&self.model, &mut cfg.model);
        overlay(&self.theme, &mut cfg.theme);
        overlay(&self.max_tokens, &mut cfg.max_tokens);
        overlay_opt(&self.temperature, &mut cfg.temperature);
        overlay_opt(&self.thinking_budget, &mut cfg.thinking_budget);
        overlay(&self.tool_call_style, &mut cfg.tool_call_style);
        overlay_opt(&self.context_budget, &mut cfg.context_budget);
        overlay(&self.compaction_slack, &mut cfg.compaction_slack);
        overlay(&self.compaction_keep, &mut cfg.compaction_keep);
        overlay(&self.loop_stall_limit, &mut cfg.loop_stall_limit);
        overlay(
            &self.malformed_retry_budget,
            &mut cfg.malformed_retry_budget,
        );
        overlay(&self.skip_next_speaker, &mut cfg.skip_next_speaker);
    }
}

/// One file key onto its config slot: a present value wins, an absent one
/// leaves the target untouched (ADR-0031's overlay rule, stated once).
fn overlay<T: Clone>(value: &Option<T>, target: &mut T) {
    if let Some(v) = value {
        *target = v.clone();
    }
}

/// The [`overlay`] variant for `Option` targets: presence sets `Some` - the
/// file cannot null a value out (the same limitation as the env seam).
fn overlay_opt<T: Clone>(value: &Option<T>, target: &mut Option<T>) {
    if let Some(v) = value {
        *target = Some(v.clone());
    }
}

// ---- SUSPENDERS_* env parsing/validation (mirrors the runtime overrides) ----

/// One row's setter: validates the raw env value and lands it on the config,
/// or carries the reason it is malformed.
type EnvSetter = fn(&mut SessionConfig, &str) -> Result<(), SessionError>;

/// The `SUSPENDERS_*` overlay table (ADR-0031): every env-settable knob, its
/// name paired with the setter that validates and lands it.
/// [`SessionConfig::apply_env`] walks the rows in order, so first-error-wins
/// follows row order. This table and [`FileConfig`]'s field list are the two
/// serializations of one schema, kept in lockstep: a new knob adds a row here
/// and a field there, or neither.
const ENV_OVERRIDES: &[(&str, EnvSetter)] = &[
    // The scoped `provider/model-id` (ADR-0037); resolution validates it at
    // launch. The retired SUSPENDERS_URL / SUSPENDERS_TOKEN moved into the
    // file-only `providers` table.
    ("SUSPENDERS_MODEL", |cfg, v| {
        cfg.model = v.into();
        Ok(())
    }),
    // The Theme name (ADR-0038); unvalidated here, like the model - the UI
    // resolves it at launch and falls back to `dark` with a notice. A
    // set-but-empty value is UNSET (the XDG idiom `xdg_config_base` uses):
    // "" would otherwise become a theme named "" and a per-launch notice.
    ("SUSPENDERS_THEME", |cfg, v| {
        if !v.is_empty() {
            cfg.theme = v.into();
        }
        Ok(())
    }),
    // Integer: the optional global budget cap (ADR-0037).
    ("SUSPENDERS_CONTEXT_BUDGET", |cfg, v| {
        cfg.context_budget = Some(parse_int(v, "SUSPENDERS_CONTEXT_BUDGET")?);
        Ok(())
    }),
    // Positive integer.
    ("SUSPENDERS_MAX_TOKENS", |cfg, v| {
        cfg.max_tokens = parse_positive_int(v)?;
        Ok(())
    }),
    // Float in [0.0, 2.0].
    ("SUSPENDERS_TOOL_CALL_STYLE", |cfg, v| {
        cfg.tool_call_style = parse_tool_call_style(v)?;
        Ok(())
    }),
    ("SUSPENDERS_TEMPERATURE", |cfg, v| {
        cfg.temperature = Some(parse_temperature(v)?);
        Ok(())
    }),
    // Integer, with a disable convention (qwen-code parity): `0` or empty maps
    // to `None` (extended thinking off); any positive integer arms the budget.
    ("SUSPENDERS_THINKING_BUDGET", |cfg, v| {
        cfg.thinking_budget = parse_thinking_budget(v)?;
        Ok(())
    }),
    // Fraction in [0.0, 1.0).
    ("SUSPENDERS_COMPACTION_SLACK", |cfg, v| {
        cfg.compaction_slack = parse_compaction_slack(v)?;
        Ok(())
    }),
    // Fraction in (0.0, 1.0).
    ("SUSPENDERS_COMPACTION_KEEP", |cfg, v| {
        cfg.compaction_keep = parse_compaction_keep(v)?;
        Ok(())
    }),
    // Positive integer: at least one repeat before the loop-detector fires.
    ("SUSPENDERS_LOOP_STALL_LIMIT", |cfg, v| {
        cfg.loop_stall_limit = parse_loop_stall_limit(v)?;
        Ok(())
    }),
    // Non-negative integer; 0 disables the malformed-retry re-draw.
    ("SUSPENDERS_MALFORMED_RETRY_BUDGET", |cfg, v| {
        cfg.malformed_retry_budget = parse_int(v, "SUSPENDERS_MALFORMED_RETRY_BUDGET")?;
        Ok(())
    }),
    // Boolean; `true` skips the next-speaker check (ADR-0043).
    ("SUSPENDERS_SKIP_NEXT_SPEAKER", |cfg, v| {
        cfg.skip_next_speaker = parse_bool(v, "SUSPENDERS_SKIP_NEXT_SPEAKER")?;
        Ok(())
    }),
];

fn parse_int(raw: &str, name: &str) -> Result<u64, SessionError> {
    raw.trim()
        .parse::<u64>()
        .map_err(|_| SessionError(format!("{name} must be an integer, got: {raw:?}")))
}

fn parse_bool(raw: &str, name: &str) -> Result<bool, SessionError> {
    match raw.trim() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(SessionError(format!(
            "{name} must be \"true\" or \"false\", got: {raw:?}"
        ))),
    }
}

fn parse_positive_int(raw: &str) -> Result<u64, SessionError> {
    match raw.trim().parse::<u64>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(SessionError(format!(
            "SUSPENDERS_MAX_TOKENS must be a positive integer, got: {raw:?}"
        ))),
    }
}

fn parse_temperature(raw: &str) -> Result<f64, SessionError> {
    match raw.trim().parse::<f64>() {
        Ok(v) if (FRACTION_LOWER_BOUND..=TEMPERATURE_MAX).contains(&v) => Ok(v),
        _ => Err(SessionError(format!(
            "SUSPENDERS_TEMPERATURE must be a float in [0.0, 2.0], got: {raw:?}"
        ))),
    }
}

/// Parses the extended-thinking budget with a disable convention (qwen-code
/// parity): an empty string or `0` yields `None` (thinking off); any other
/// value must be a positive integer, yielding `Some(n)`. A non-integer is an
/// error naming the accepted forms.
fn parse_thinking_budget(raw: &str) -> Result<Option<u64>, SessionError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "0" {
        return Ok(None);
    }
    match trimmed.parse::<u64>() {
        Ok(n) => Ok(Some(n)),
        Err(_) => Err(SessionError(format!(
            "SUSPENDERS_THINKING_BUDGET must be a non-negative integer (0 or empty disables), got: {raw:?}"
        ))),
    }
}

fn parse_compaction_slack(raw: &str) -> Result<f64, SessionError> {
    match raw.trim().parse::<f64>() {
        Ok(v) if (FRACTION_LOWER_BOUND..FRACTION_UPPER_BOUND).contains(&v) => Ok(v),
        _ => Err(SessionError(format!(
            "SUSPENDERS_COMPACTION_SLACK must be a fraction in [0.0, 1.0), got: {raw:?}"
        ))),
    }
}

fn parse_loop_stall_limit(raw: &str) -> Result<u64, SessionError> {
    match raw.trim().parse::<u64>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(SessionError(format!(
            "SUSPENDERS_LOOP_STALL_LIMIT must be a positive integer, got: {raw:?}"
        ))),
    }
}

fn parse_tool_call_style(raw: &str) -> Result<ToolCallStyle, SessionError> {
    ToolCallStyle::parse(raw.trim()).ok_or_else(|| {
        SessionError(format!(
            "SUSPENDERS_TOOL_CALL_STYLE must be \"auto\", \"structured\", or \"text\", got: {raw:?}"
        ))
    })
}

fn parse_compaction_keep(raw: &str) -> Result<f64, SessionError> {
    match raw.trim().parse::<f64>() {
        Ok(v) if v > FRACTION_LOWER_BOUND && v < FRACTION_UPPER_BOUND => Ok(v),
        _ => Err(SessionError(format!(
            "SUSPENDERS_COMPACTION_KEEP must be a fraction in (0.0, 1.0), got: {raw:?}"
        ))),
    }
}

/// The overrides a caller supplies to [`Session::build`]; any `None` falls back
/// to the [`SessionConfig`]. These are the constructor's keyword-style opts.
#[derive(Debug, Clone, Default)]
pub struct SessionOpts {
    pub root: Option<String>,
    pub llm_module: Option<String>,
    pub extensions: Option<Vec<String>>,
    pub context_budget: Option<u64>,
    pub compaction_slack: Option<f64>,
    pub compaction_keep: Option<f64>,
    pub run_limit: Option<u64>,
    pub loop_stall_limit: Option<u64>,
    pub malformed_retry_budget: Option<u64>,
    pub skip_next_speaker: Option<bool>,
    pub command_timeout_ms: Option<u64>,
    pub session_dir: Option<String>,
    /// A prebuilt launch Model, bypassing scoped-id resolution (the test seam;
    /// the Provider set still resolves from config).
    pub model: Option<Model>,
    pub temperature: Option<Option<f64>>,
    pub thinking_budget: Option<Option<u64>>,
    pub tool_call_style: Option<ToolCallStyle>,
}

impl Session {
    /// Builds and validates the Session's fixed facts, resolving config through
    /// the scope-aware composition seam (base → user file → workspace file → env;
    /// ADR-0031, ADR-0065 Phase B). `root` defaults to the current dir.
    pub fn new(mut opts: SessionOpts) -> Result<Session, SessionError> {
        // The Project Root resolves FIRST, before composition: the workspace
        // config lives under it (`workspace_config_path`), so the compose and the
        // build must agree on the same root. Feeding it back into `opts` keeps
        // `build`'s root-resolution unchanged (it just no longer re-defaults).
        let root = opts.root.clone().unwrap_or_else(default_root);
        let config =
            SessionConfig::compose(&default_config_path(), Some(&workspace_config_path(&root)))?;
        opts.root = Some(root);
        Session::build(opts, &config)
    }

    /// Builds and validates against an explicit [`SessionConfig`] - the
    /// no-env path tests use. Every `None` opt falls back to `config`.
    pub fn build(opts: SessionOpts, config: &SessionConfig) -> Result<Session, SessionError> {
        let context_budget = opts.context_budget.or(config.context_budget);

        // The Provider set: custom entries from config, then every built-in
        // the customs do not shadow (ADR-0037).
        let providers = resolve_providers(&config.providers);

        // The launch Model: a prebuilt one from the opts (the test seam), or
        // the scoped config id resolved against the set - an unknown Provider
        // fails launch loudly (ADR-0031).
        let launch_model = match opts.model.clone() {
            Some(m) => m,
            None => model::resolve(
                &config.model,
                &providers,
                context_budget.unwrap_or(FALLBACK_WINDOW),
                config.max_tokens,
            )
            .map_err(SessionError)?,
        };

        // The Project Root resolves first: the memory root (P5, ADR-0062)
        // derives from it, so both land as fixed facts here.
        let root = opts.root.unwrap_or_else(default_root);
        let memory_root = default_memory_root(&root);

        let session = Session {
            root,
            memory_root,
            llm_module: opts.llm_module.unwrap_or_else(|| config.llm_module.clone()),
            extensions: opts.extensions.unwrap_or_else(|| config.extensions.clone()),
            context_budget,
            compaction_slack: opts.compaction_slack.unwrap_or(config.compaction_slack),
            compaction_keep: opts.compaction_keep.unwrap_or(config.compaction_keep),
            run_limit: opts.run_limit.unwrap_or(config.run_limit),
            loop_stall_limit: opts.loop_stall_limit.unwrap_or(config.loop_stall_limit),
            malformed_retry_budget: opts
                .malformed_retry_budget
                .unwrap_or(config.malformed_retry_budget),
            skip_next_speaker: opts.skip_next_speaker.unwrap_or(config.skip_next_speaker),
            command_timeout_ms: opts.command_timeout_ms.unwrap_or(config.command_timeout_ms),
            session_dir: opts
                .session_dir
                .unwrap_or_else(|| config.session_dir.clone()),
            providers,
            model: launch_model,
            theme: config.theme.clone(),
            temperature: opts.temperature.unwrap_or(config.temperature),
            thinking_budget: opts.thinking_budget.unwrap_or(config.thinking_budget),
            tool_call_style: opts.tool_call_style.unwrap_or(config.tool_call_style),
            max_tokens: config.max_tokens,
            // The MCP servers ride from config verbatim (F8, ADR-0056): a
            // file-only map like `providers`, no opts override. Each entry's
            // transport is validated below. The disabled set and per-server
            // Sources ride alongside (Phase B, ADR-0065): the composer merged
            // them across scopes, so build just carries them through.
            mcp_servers: config.mcp_servers.clone(),
            mcp_excluded: config.mcp_excluded.clone(),
            mcp_sources: config.mcp_sources.clone(),
        };

        validate(&session)?;
        Ok(session)
    }

    /// Resolves a scoped `provider/model-id` against the Session's fixed
    /// Provider set (the `/model` swap path, ADR-0033 amendment): Catalog
    /// figures for known built-in models, the Provider's config window plus
    /// the Session's `max_tokens` knob for everything else.
    pub fn resolve_model(&self, scoped: &str) -> Result<Model, String> {
        model::resolve(
            scoped,
            &self.providers,
            self.context_budget.unwrap_or(FALLBACK_WINDOW),
            self.max_tokens,
        )
    }

    /// The effective Context Budget for `model` (ADR-0037): its own context
    /// window, capped by the config `context_budget` when set. Derived from
    /// whichever Model the Run captured, never a fixed Session fact.
    pub fn context_budget_for(&self, model: &Model) -> u64 {
        match self.context_budget {
            Some(cap) => cap.min(model.context_window),
            None => model.context_window,
        }
    }

    /// The reply reserve for `model` (ADR-0037): the slice of the effective
    /// Context Budget held back for the Model's reply, from which the live
    /// window and Compaction figures derive. It is the Model's wire output cap
    /// (already clamped at resolution to leave prompt room in the window),
    /// clamped AGAIN to half the effective Context Budget so it can never
    /// swallow the whole budget when the config caps the budget below the
    /// window. A live window - and therefore a usable `/model` switch - always
    /// survives.
    pub fn reply_reserve_for(&self, model: &Model) -> u64 {
        let budget = self.context_budget_for(model);
        model.max_tokens.min(budget / 2)
    }

    /// Checks the per-Model budget invariant for `model` (ADR-0037): the
    /// Compaction Keep must sit below the compaction trigger at that Model's
    /// clamped reserve ([`reply_reserve_for`]). Run at launch for the launch
    /// Model and by the Agent at a `/model` swap for the picked Model, so a
    /// pick whose Compaction Keep cannot fit is rejected with the reason
    /// instead of exploding on a later Run.
    pub fn validate_model_budget(&self, model: &Model) -> Result<(), String> {
        let budget = self.context_budget_for(model);
        let reserve = self.reply_reserve_for(model);

        // Fire high, keep low: the Compaction Keep amount must sit below the
        // trigger. Comparing in u64 matches the old f64 check: the trigger is
        // integral, so trunc(keep) < trigger iff keep < trigger.
        let keep_amount =
            conversation::compaction_keep_amount(budget, reserve, self.compaction_keep);
        let trigger = conversation::compaction_target(budget, reserve, self.compaction_slack);
        if keep_amount >= trigger {
            return Err(format!(
                "model {}: :compaction_keep is too high - the Compaction Keep must sit below the compaction trigger (fire high, keep low)",
                model.scoped_id()
            ));
        }
        Ok(())
    }

    /// The Provider a Model belongs to, from the Session's fixed set.
    pub fn provider_of(&self, model: &Model) -> Option<&Provider> {
        crate::llm::provider::find(&self.providers, &model.provider)
    }

    /// Overlays the server-reported context window onto a custom Provider's
    /// Model (ADR-0037: "server wins, period"). The sync [`resolve_model`] and
    /// [`Session::build`] cannot reach the network, so they seed a Model's
    /// window from config or the fallback; this async layer runs at every point
    /// the Active Model is captured (launch, `/model` swap, subagent) to let the
    /// host's live `n_ctx` override even an explicit config `context_window`.
    ///
    /// A Catalog-known built-in Model is left untouched - its window is the
    /// Catalog's fact, never discovered. A custom Model whose host reports an
    /// `n_ctx` for its id is rebuilt on that window with `max_tokens` re-derived
    /// ([`model::with_server_window`], keeping the prompt-room invariant). If the
    /// Provider is unreachable, or the host reports no window for the id, the
    /// sync-resolved Model rides through unchanged - the config/fallback window
    /// stands. Discovery failure is never fatal here: a down host still runs on
    /// its guessed window.
    ///
    /// [`resolve_model`]: Session::resolve_model
    pub async fn enrich_model_window(&self, llm: &dyn crate::llm::Llm, model: Model) -> Model {
        // The Catalog's window is authoritative for a built-in Model; only a
        // custom Provider's Model takes the server's live figure.
        let Some(provider) = self.provider_of(&model) else {
            return model;
        };
        if !provider.custom {
            return model;
        }
        let Ok(discovered) = llm.list_models(provider).await else {
            return model;
        };
        match discovered
            .iter()
            .find(|d| d.id == model.id)
            .and_then(|d| d.context_window)
        {
            Some(window) => model::with_server_window(&model, window, self.max_tokens),
            None => model,
        }
    }

    /// The ctx every Tool Call executes with: the Project Root, the Result
    /// Cap derived from `model` - the one the Run captured (ADR-0037) - and
    /// the command timeout.
    pub fn tool_ctx(&self, model: &Model, caps: crate::tool::caps::Capabilities) -> ToolCtx {
        ToolCtx {
            root: std::path::PathBuf::from(&self.root),
            result_cap: crate::tools::shaping::cap_for(
                self.context_budget_for(model),
                self.reply_reserve_for(model),
            ),
            command_timeout_ms: self.command_timeout_ms,
            // The captured Model's input modalities (ADR-0059): a copied fact,
            // stamped here like the Result Cap so read_file (P3 3b) can gate media
            // on it without reaching the llm layer.
            input_modalities: model.input_modalities,
            // The trusted memory subtree (P5, ADR-0062): stamped here like the
            // Result Cap so the shared path seam lets write_file/edit_file/
            // read_file reach memory files without per-tool duplication. A
            // resolved subtree, NOT a general escape.
            memory_root: Some(std::path::PathBuf::from(&self.memory_root)),
            // The session directory (Phase 9, ADR-0063): where background-shell
            // capture files live, so run_command's background branch can name the
            // capture file in its started block (the Agent builds them there too).
            session_dir: std::path::PathBuf::from(&self.session_dir),
            caps,
        }
    }

    /// The MCP attach plan map (Phase B, ADR-0065): one
    /// [`McpServerPlan`](crate::mcp::manager::McpServerPlan) per merged server,
    /// stamped with the Source that declared it (defaulting to
    /// [`McpSource::User`](crate::mcp::McpSource::User) when unrecorded) and the
    /// disabled flag (`true` iff the name is in `mcp_excluded`). The Agent hands
    /// this to [`McpManager::connect`](crate::mcp::manager::McpManager::connect),
    /// which shows a disabled server but never attaches it.
    pub fn mcp_plans(&self) -> BTreeMap<String, crate::mcp::manager::McpServerPlan> {
        self.mcp_servers
            .iter()
            .map(|(name, config)| {
                let source = self
                    .mcp_sources
                    .get(name)
                    .copied()
                    .unwrap_or(crate::mcp::McpSource::User);
                let disabled = self.mcp_excluded.contains(name);
                (
                    name.clone(),
                    crate::mcp::manager::McpServerPlan {
                        config: config.clone(),
                        source,
                        disabled,
                    },
                )
            })
            .collect()
    }
}

/// The window for Models nothing else supplies a figure for (ADR-0037): no
/// Catalog entry, no per-Provider `context_window`, no global `context_budget`.
/// The conservative small-local-model default the base config always shipped.
const FALLBACK_WINDOW: u64 = 64_000;

// The Provider set (ADR-0037): custom entries first (BTreeMap order keeps the
// set deterministic), then every built-in Provider a custom entry does not
// shadow, each with its credential resolved from its own environment key.
fn resolve_providers(customs: &BTreeMap<String, ProviderConfig>) -> Vec<Provider> {
    let mut providers: Vec<Provider> = customs
        .iter()
        .map(|(id, c)| Provider {
            id: id.clone(),
            base_url: c.base_url.clone(),
            token: c.token.clone().unwrap_or_default(),
            api: c.api,
            context_window: c.context_window,
            custom: true,
        })
        .collect();
    for builtin in catalog::builtin_providers() {
        if !providers.iter().any(|p| p.id == builtin.id) {
            providers.push(builtin);
        }
    }
    providers
}

fn default_root() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into())
}

// Session Logs live outside the Project Root (ADR-0010): XDG data home. Per the
// XDG spec, a set-but-empty var is treated as unset (else the path degrades to
// `/suspenders/sessions`).
fn default_session_dir() -> String {
    format!("{}/suspenders/sessions", xdg_data_base())
}

// The user config file lives beside the Session Logs (ADR-0031): XDG config
// home, mirroring `default_session_dir` (empty var == unset, per XDG).
pub fn default_config_path() -> String {
    format!("{}/suspenders/config.json", xdg_config_base())
}

/// The workspace (project-local) config file (Phase B, ADR-0065): the first
/// project-local settings scope, `<root>/.suspenders/config.json`, mirroring how
/// the project skills dir (`.suspenders/skills`) and the local memory root
/// (`.suspenders/memory`) hang off the Project Root. Resolved once at launch
/// alongside the user config path; an absent file is an empty overlay.
pub fn workspace_config_path(root: &str) -> String {
    format!("{root}/.suspenders/config.json")
}

/// The MCP OAuth token store (ADR-0065 Phase D, qwen `getMcpOAuthTokensPath`):
/// `mcp-oauth-tokens.json` beside `config.json` in the XDG config home. Resolved
/// once at the launch edge like the config path; the file is created (mode 0600)
/// on the first token save and absent means no stored credentials.
pub fn default_mcp_oauth_tokens_path() -> String {
    format!("{}/suspenders/mcp-oauth-tokens.json", xdg_config_base())
}

/// The user themes directory (ADR-0038): `themes/` beside `config.json` in
/// the XDG config home. Resolved once at the launch edge, like the config
/// path; a missing directory just means no user themes.
pub fn default_themes_dir() -> String {
    format!("{}/suspenders/themes", xdg_config_base())
}

/// The user skills directory (ADR-0058): `skills/` beside `config.json` in the
/// XDG config home, the user-level counterpart to the project's
/// `.suspenders/skills/`. Resolved at the launch edge like the themes dir; a
/// missing directory just means no user skills.
pub fn default_user_skills_dir() -> String {
    format!("{}/suspenders/skills", xdg_config_base())
}

/// The managed-auto-memory root for `project_root` (P5, ADR-0062; qwen
/// `getAutoMemoryRoot`): where the model's `MEMORY.md` index and topic files
/// live. Two shapes, both resolved once at the launch edge like the dirs above:
///
/// * `SUSPENDERS_MEMORY_LOCAL=1` -> in-root `<project_root>/.suspenders/memory`
///   (qwen's `QWEN_CODE_MEMORY_LOCAL` -> `<root>/.qwen/memory`).
/// * otherwise the GLOBAL, project-keyed default:
///   `<base>/projects/<slug(canonical_git_root)>/memory`, where `base` is the
///   XDG data home (`memory_base()`, `SUSPENDERS_MEMORY_BASE_DIR` overriding for
///   tests), the canonical git root is the `.git`-bearing ancestor of
///   `project_root` (falling back to `project_root` itself when none), and the
///   slug replaces every `[^a-zA-Z0-9]` with `-` (qwen `sanitizeCwd`).
///
/// Global-by-default keeps memory out of the working tree (it is not the
/// user's code) while still keying it to the project, so two checkouts of the
/// same repo share one memory.
pub fn default_memory_root(project_root: &str) -> String {
    if std::env::var("SUSPENDERS_MEMORY_LOCAL").as_deref() == Ok("1") {
        return format!("{project_root}/.suspenders/memory");
    }
    let canonical = canonical_git_root(project_root).unwrap_or_else(|| project_root.to_string());
    format!(
        "{}/projects/{}/memory",
        memory_base(),
        sanitize_cwd(&canonical)
    )
}

// The base directory for the global, project-keyed memory store (qwen
// `getMemoryBaseDir`): the XDG data home, mirroring `default_session_dir`, with
// SUSPENDERS_MEMORY_BASE_DIR overriding it (the test seam, empty var == unset).
fn memory_base() -> String {
    if let Some(base) = std::env::var("SUSPENDERS_MEMORY_BASE_DIR")
        .ok()
        .filter(|s| !s.is_empty())
    {
        return base;
    }
    format!("{}/suspenders", xdg_data_base())
}

// The project identifier slug (qwen `sanitizeCwd`): every non-alphanumeric char
// becomes `-`, so a filesystem path becomes one safe directory-name segment.
fn sanitize_cwd(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

// The canonical git root of `start` (qwen `findCanonicalGitRoot`, simplified):
// walk up for the first ancestor holding a `.git` entry. qwen additionally
// resolves a worktree's `.git` FILE (a `gitdir:` pointer) back to the main
// checkout so sibling worktrees of one repo share a memory; Suspenders takes the
// DOCUMENTED simplification of returning the worktree's own root (its `.git` is
// a file, `Path::exists` still finds it), so each worktree keys its own memory.
// That is a conservative, safe divergence: worktrees get separate memory rather
// than a mis-shared one, and the common non-worktree case is identical.
fn canonical_git_root(start: &str) -> Option<String> {
    let mut current = std::path::Path::new(start).to_path_buf();
    loop {
        if current.join(".git").exists() {
            return Some(current.to_string_lossy().into_owned());
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => return None,
        }
    }
}

// The XDG config home both paths above hang off (empty var == unset, per XDG).
fn xdg_config_base() -> String {
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            format!("{home}/.config")
        })
}

// The XDG data home the Session Logs and the memory store hang off (empty var
// == unset, per XDG). The data-home counterpart to `xdg_config_base`, so the
// `XDG_DATA_HOME` -> `~/.local/share` fallback lives once for both callers.
fn xdg_data_base() -> String {
    std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            format!("{home}/.local/share")
        })
}

// The thin impure reader (ADR-0031, ADR-0065): reads and parses the config at
// `path`, returning `Some(FileConfig)` when present, `None` when absent (an
// absent scope is an empty overlay, never an error), and a [`SessionError`]
// naming `path` on an IO or parse error. `path` is an argument (not resolved
// here) so the read is testable without the real XDG/workspace dirs. The pure
// [`FileConfig::parse`] vs this impure read split mirrors [`persist_key`].
fn read_file_config(path: &str) -> Result<Option<FileConfig>, SessionError> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(Some(FileConfig::parse(&raw).map_err(|e| {
            SessionError(format!("invalid config at {path}: {e}"))
        })?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(SessionError(format!(
            "failed to read config at {path}: {e}"
        ))),
    }
}

// All cross-key invariants live here, each in its own focused check so the
// reasons stay legible.
fn validate(s: &Session) -> Result<(), SessionError> {
    validate_scalars(s)?;
    validate_providers(s)?;
    // An MCP server entry's transport (F8, ADR-0056) is now a sum type resolved
    // AT PARSE TIME: a malformed entry - both `command` and `http_url`, or
    // neither - is a loud deserialize error when the config is parsed, so there
    // is no separate transport-validation pass here. A server that resolves but
    // will not connect is still skipped at attach (fail-open), not rejected.
    // The per-Model budget invariants, applied to the launch Model here; the
    // Agent re-applies them to every `/model` pick (ADR-0037).
    s.validate_model_budget(&s.model).map_err(SessionError)?;
    Ok(())
}

fn validate_scalars(s: &Session) -> Result<(), SessionError> {
    if let Some(cap) = s.context_budget {
        pos_int(cap, ":context_budget")?;
    }
    pos_int(s.model.max_tokens, "model :max_tokens")?;
    pos_int(s.run_limit, ":turn_limit")?;
    pos_int(s.loop_stall_limit, ":loop_stall_limit")?;
    pos_int(s.command_timeout_ms, ":command_timeout_ms")?;

    fraction_left_closed(s.compaction_slack, ":compaction_slack")?;
    fraction_open(s.compaction_keep, ":compaction_keep")?;
    temperature(s.temperature)?;
    Ok(())
}

// The Provider set is a fixed fact: every entry must be reachable data - a
// non-empty endpoint, and a positive window on custom entries (built-ins
// carry None; the Catalog owns their windows).
fn validate_providers(s: &Session) -> Result<(), SessionError> {
    for p in &s.providers {
        if p.base_url.is_empty() {
            return Err(SessionError(format!(
                "provider {:?} :base_url must be non-empty",
                p.id
            )));
        }
        if p.context_window == Some(0) {
            return Err(SessionError(format!(
                "provider {:?} :context_window must be a positive integer",
                p.id
            )));
        }
    }
    Ok(())
}

fn temperature(value: Option<f64>) -> Result<(), SessionError> {
    match value {
        None => Ok(()),
        Some(v) if (FRACTION_LOWER_BOUND..=TEMPERATURE_MAX).contains(&v) => Ok(()),
        Some(_) => Err(SessionError(
            "connection :temperature must be a float in [0.0, 2.0] or nil".into(),
        )),
    }
}

fn pos_int(value: u64, name: &str) -> Result<(), SessionError> {
    if value > 0 {
        Ok(())
    } else {
        Err(SessionError(format!(
            "{name} must be a positive integer, got: {value}"
        )))
    }
}

fn fraction_left_closed(value: f64, name: &str) -> Result<(), SessionError> {
    if (FRACTION_LOWER_BOUND..FRACTION_UPPER_BOUND).contains(&value) {
        Ok(())
    } else {
        Err(SessionError(format!(
            "{name} must be a float in [0.0, 1.0)"
        )))
    }
}

fn fraction_open(value: f64, name: &str) -> Result<(), SessionError> {
    if value > FRACTION_LOWER_BOUND && value < FRACTION_UPPER_BOUND {
        Ok(())
    } else {
        Err(SessionError(format!(
            "{name} must be a float in (0.0, 1.0)"
        )))
    }
}

#[cfg(test)]
#[path = "../tests/session.rs"]
mod tests;
