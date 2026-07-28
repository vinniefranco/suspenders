//! The Session's fixed facts (CONTEXT.md: Session), resolved and validated
//! once at launch: the Project Root, the Provider set, the launch Model, the
//! sampling temperature, the budget-cap knob, the Eviction slack, the Dead
//! Mass fraction, the Compaction Keep, the Run Limit, the Anchor cadence and
//! stale-plan threshold, the Scout Pass cap, the no-think knobs, the command
//! timeout, the Extension list, and the LLM module. The Context Budget and the
//! Result Cap are NOT fixed facts: they derive from the Model each Run
//! captures (ADR-0037).
//!
//! This is the composition seam for configuration. [`Session::new`] resolves
//! these keys once (via [`SessionConfig::load`]) by overlaying, in order, the
//! hardcoded [`SessionConfig::base`] defaults, the user's `config.json`
//! (ADR-0031), and the `SUSPENDERS_*` environment (the file is the persistent
//! baseline; the environment still wins per-invocation over it); everything
//! downstream receives values from this struct, so the cross-module invariants
//! live in one place:
//!
//! * the Context Budget, the Eviction reserve, and the Result Cap are NOT
//!   fixed facts (ADR-0037): they derive from the Model each Run captures,
//!   through [`Session::context_budget_for`] and [`Session::tool_ctx`]
//! * a Model's output cap must leave room in its effective budget - checked
//!   here at launch for the launch Model and by the Agent at a `/model` swap
//!   for the picked Model ([`Session::validate_model_budget`])
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
use crate::llm::{catalog, model};
use crate::tool::ToolCtx;
use serde::{Deserialize, Serialize};

/// The shape of a Recovery Run (CONTEXT.md: Recovery Run, Continuation,
/// Handoff): [`RecoveryShape::Handoff`] retires the Conversation and seeds a
/// fresh one from the compaction machinery; [`RecoveryShape::Continuation`]
/// keeps it and appends the recovery prompt. A Setpoint value the Endgame
/// Governor owns; defined here beside the Session facts that resolve it (the
/// same direction as [`log::StopReason`], which the Endgame also reads).
///
/// `rename_all = "lowercase"` makes the serde forms exactly the lowercase
/// strings the env seam already parses (`"handoff"` / `"continuation"`), so the
/// [`FileConfig`] serialization and [`RecoveryShape::parse`]/[`as_str`] agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecoveryShape {
    Handoff,
    Continuation,
}

impl RecoveryShape {
    pub fn as_str(self) -> &'static str {
        match self {
            RecoveryShape::Handoff => "handoff",
            RecoveryShape::Continuation => "continuation",
        }
    }

    pub fn parse(s: &str) -> Option<RecoveryShape> {
        match s {
            "handoff" => Some(RecoveryShape::Handoff),
            "continuation" => Some(RecoveryShape::Continuation),
            _ => None,
        }
    }
}

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
    pub eviction_slack: f64,
    /// The Eviction mechanic's Dead Mass Setpoint: the fraction of the
    /// Context Budget that elidable dead content may occupy before a wave
    /// fires without budget pressure.
    pub dead_mass_fraction: f64,
    pub compaction_keep: f64,
    pub run_limit: u64,
    pub anchor_interval: u64,
    /// The anchor Governor's stale-plan Setpoint: the Passes a Plan may sit
    /// unchanged - while writes land - before each Anchor carries the
    /// stale-plan line.
    pub plan_stale_after: u64,
    /// The Endgame Governor's recovery Setpoint: at most this many Recovery
    /// Runs may serve one user request. `0` disables the mechanic entirely.
    pub recovery_limit: u64,
    /// The Endgame Governor's recovery-shape Setpoint: which arm a Recovery
    /// Run takes (CONTEXT.md: Handoff is the default shape).
    pub recovery_shape: RecoveryShape,
    /// The malformed-tool-call re-draw Setpoint (ADR-0030): at most this many
    /// in-band re-draws may follow a retryable generation error within one
    /// Run. `0` disables the mechanic entirely (the loud failure runs
    /// immediately, as before).
    pub malformed_retry_budget: u64,
    pub scout_pass_limit: u64,
    pub scout_no_think: bool,
    pub no_think_rescue: bool,
    pub command_timeout_ms: u64,
    pub session_dir: String,
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
    /// The output cap for Models the Catalog does not know (the config knob):
    /// the synthesis fallback when a scoped id resolves at `/model` time.
    pub max_tokens: u64,
}

/// Raised (returned) when a Session's fixed facts fail validation. The message
/// carries the validation-failure text so callers can match on the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionError(pub String);

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for SessionError {}

/// The resolved config defaults a Session is built against.
/// [`SessionConfig::load`] composes it (base → file → env) and tests pass
/// [`SessionConfig::test_defaults`] explicitly (no file/env reads).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionConfig {
    /// Custom Providers, keyed by id (ADR-0031 amendment): file-only structure
    /// the env cannot express. A custom entry shadows a built-in with the
    /// same id.
    pub providers: BTreeMap<String, ProviderConfig>,
    /// The scoped `provider/model-id` the launch Model resolves from.
    pub model: String,
    /// The configured Theme name (ADR-0038): a built-in (`dark`, `light`) or a
    /// user file's stem in the themes directory. Carried unvalidated - the UI
    /// resolves it at launch and falls back to `dark` with a notice, so a bad
    /// name never fails launch.
    pub theme: String,
    pub max_tokens: u64,
    pub temperature: Option<f64>,
    /// The optional global budget cap and catalog-less window figure
    /// (ADR-0037); `None` leaves every Model's own window uncapped.
    pub context_budget: Option<u64>,
    pub eviction_slack: f64,
    pub dead_mass_fraction: f64,
    pub compaction_keep: f64,
    pub llm_module: String,
    pub command_timeout_ms: u64,
    pub run_limit: u64,
    pub anchor_interval: u64,
    pub plan_stale_after: u64,
    pub recovery_limit: u64,
    pub recovery_shape: RecoveryShape,
    pub malformed_retry_budget: u64,
    pub scout_pass_limit: u64,
    pub scout_no_think: bool,
    pub no_think_rescue: bool,
    pub extensions: Vec<String>,
    pub session_dir: String,
}

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
                    context_window: Some(64_000),
                    token: None,
                },
            )]),
            model: "local/qwen/Qwen3.6-27B-MTP-GGUF".into(),
            theme: "dark".into(),
            max_tokens: 8_000,
            temperature: Some(0.7),
            // No global cap by default: every Model's own window is its
            // budget, so a wide-window Catalog model works out of the box.
            context_budget: None,
            eviction_slack: 0.2,
            dead_mass_fraction: 0.15,
            compaction_keep: 0.5,
            llm_module: "Suspenders.LLM".into(),
            command_timeout_ms: 120_000,
            run_limit: 32,
            anchor_interval: 5,
            plan_stale_after: 8,
            recovery_limit: 1,
            recovery_shape: RecoveryShape::Handoff,
            malformed_retry_budget: 3,
            scout_pass_limit: 8,
            scout_no_think: true,
            no_think_rescue: true,
            extensions: vec!["diff".into(), "run_command".into(), "condense".into()],
            session_dir: default_session_dir(),
        }
    }

    /// The config the test env resolves to: fakes injected, empty extension
    /// list, tmp session dir.
    pub fn test_defaults() -> Self {
        let mut cfg = SessionConfig::base();
        cfg.providers
            .get_mut("local")
            .expect("base ships local")
            .base_url = "http://localhost:0/v1".into();
        cfg.llm_module = "Suspenders.FakeLLM".into();
        cfg.extensions = vec![];
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

    /// The fallible composition entry: `base()` → `config.json` overlay → env
    /// overlay, returning the reason on the first malformed value (a bad file,
    /// a bad env var). The file is the user's persistent baseline; the
    /// environment still wins per-invocation over it (ADR-0031).
    pub fn try_load() -> Result<Self, SessionError> {
        let mut cfg = SessionConfig::base();
        // Order is load-bearing (ADR-0031): the file overlay lands FIRST, then
        // `apply_env`, so the environment wins per-invocation over the file's
        // persistent baseline. The env-over-file precedence is not unit-tested:
        // `try_load` resolves the REAL XDG config path, so proving it would
        // read (and need to plant) the user's actual config file. The env seam
        // itself is covered through `apply_env` directly - see the env-overlay
        // tests, which lean on nextest's process-per-test isolation.
        load_file_overlay(&mut cfg, &default_config_path())?;
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
            model: Some(base.model),
            theme: Some(base.theme),
            max_tokens: Some(base.max_tokens),
            temperature: base.temperature,
            // Absent from the template on purpose (ADR-0037): the base config
            // carries no global cap, and baking one in would pin wide-window
            // Catalog models under it.
            context_budget: base.context_budget,
            eviction_slack: Some(base.eviction_slack),
            dead_mass_fraction: Some(base.dead_mass_fraction),
            compaction_keep: Some(base.compaction_keep),
            plan_stale_after: Some(base.plan_stale_after),
            recovery_limit: Some(base.recovery_limit),
            recovery_shape: Some(base.recovery_shape),
            malformed_retry_budget: Some(base.malformed_retry_budget),
            scout_no_think: Some(base.scout_no_think),
            no_think_rescue: Some(base.no_think_rescue),
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
}

/// The sparse sticky write behind [`SessionConfig::persist_model`] and
/// [`SessionConfig::persist_theme`] (ADR-0033, ADR-0038): the user's other
/// keys are preserved and `token` is never introduced by the tool. This is
/// the one sanctioned exception to ADR-0031's no-auto-create - an explicit
/// pick is a deliberate act, so the file is created if absent.
///
/// If `path` exists it is parsed as a JSON object and only `key` is set; if
/// absent, a `{"<key>": "..."}` file (and its parent dirs) is created.
/// Malformed existing JSON is an [`Err`] naming `path` (mirroring
/// [`load_file_overlay`]'s error style). Parsing splits into the pure
/// [`merge_key`] and this thin impure reader/writer, the same split as
/// [`FileConfig::parse`] vs [`load_file_overlay`].
fn persist_key(path: &str, key: &str, value: &str) -> Result<(), SessionError> {
    let existing = match std::fs::read_to_string(path) {
        Ok(raw) => Some(raw),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(SessionError(format!(
                "failed to read config at {path}: {e}"
            )));
        }
    };

    let json = merge_key(existing.as_deref(), key, value)
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

/// Pure sparse merge of one string `key` into a config file's JSON (ADR-0033,
/// ADR-0038): `existing` is the current file contents (or `None` when absent),
/// and the result is the pretty JSON to write back. Every other key is
/// preserved; a `token` key is never introduced (only the caller's existing
/// one, if any, survives). A malformed or non-object existing file is an
/// [`Err`] carrying the path-agnostic reason (the caller wraps it with the
/// resolved path). Path-free and side-effect-free, so it unit-tests with
/// literals like [`FileConfig::parse`].
fn merge_key(existing: Option<&str>, key: &str, value: &str) -> Result<String, String> {
    let mut root = match existing {
        None => serde_json::Value::Object(serde_json::Map::new()),
        Some(raw) => serde_json::from_str(raw).map_err(|e| e.to_string())?,
    };

    let obj = root
        .as_object_mut()
        .ok_or_else(|| "config root must be a JSON object".to_string())?;
    obj.insert(key.into(), serde_json::Value::String(value.to_string()));

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
/// `turn_limit`, `anchor_interval`, `scout_pass_limit`, `extensions`) are simply
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
    #[serde(skip_serializing_if = "Option::is_none")]
    context_budget: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    eviction_slack: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dead_mass_fraction: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compaction_keep: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plan_stale_after: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery_limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery_shape: Option<RecoveryShape>,
    #[serde(skip_serializing_if = "Option::is_none")]
    malformed_retry_budget: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scout_no_think: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    no_think_rescue: Option<bool>,
}

impl FileConfig {
    /// Pure parse of the config file's JSON (ADR-0031): syntax errors, unknown
    /// keys, and type mismatches each surface as a [`SessionError`]. The message
    /// is path-agnostic (the caller, [`load_file_overlay`], wraps it with the
    /// resolved path). Range checks are NOT done here - a bad-but-typed value is
    /// caught later by `validate()` on the final [`Session`].
    fn parse(raw: &str) -> Result<FileConfig, SessionError> {
        serde_json::from_str(raw).map_err(|e| SessionError(e.to_string()))
    }

    /// Overlays only the present (`Some`) fields onto `cfg`; absent fields leave
    /// `cfg` untouched. One [`overlay`]/[`overlay_opt`] call per key, so the
    /// present-wins branch exists (and is covered) once, not once per field.
    /// Private on purpose: a `SessionConfig` mutated this way must still pass
    /// through `validate()` on the built [`Session`].
    fn apply(&self, cfg: &mut SessionConfig) {
        overlay(&self.providers, &mut cfg.providers);
        overlay(&self.model, &mut cfg.model);
        overlay(&self.theme, &mut cfg.theme);
        overlay(&self.max_tokens, &mut cfg.max_tokens);
        overlay_opt(&self.temperature, &mut cfg.temperature);
        overlay_opt(&self.context_budget, &mut cfg.context_budget);
        overlay(&self.eviction_slack, &mut cfg.eviction_slack);
        overlay(&self.dead_mass_fraction, &mut cfg.dead_mass_fraction);
        overlay(&self.compaction_keep, &mut cfg.compaction_keep);
        overlay(&self.plan_stale_after, &mut cfg.plan_stale_after);
        overlay(&self.recovery_limit, &mut cfg.recovery_limit);
        overlay(&self.recovery_shape, &mut cfg.recovery_shape);
        overlay(
            &self.malformed_retry_budget,
            &mut cfg.malformed_retry_budget,
        );
        overlay(&self.scout_no_think, &mut cfg.scout_no_think);
        overlay(&self.no_think_rescue, &mut cfg.no_think_rescue);
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
    ("SUSPENDERS_TEMPERATURE", |cfg, v| {
        cfg.temperature = Some(parse_temperature(v)?);
        Ok(())
    }),
    // Fraction in [0.0, 1.0).
    ("SUSPENDERS_EVICTION_SLACK", |cfg, v| {
        cfg.eviction_slack = parse_eviction_slack(v)?;
        Ok(())
    }),
    // Fraction in (0.0, 1.0).
    ("SUSPENDERS_DEAD_MASS_FRACTION", |cfg, v| {
        cfg.dead_mass_fraction = parse_dead_mass_fraction(v)?;
        Ok(())
    }),
    // Fraction in (0.0, 1.0).
    ("SUSPENDERS_COMPACTION_KEEP", |cfg, v| {
        cfg.compaction_keep = parse_compaction_keep(v)?;
        Ok(())
    }),
    // Positive integer.
    ("SUSPENDERS_PLAN_STALE_AFTER", |cfg, v| {
        cfg.plan_stale_after = parse_plan_stale_after(v)?;
        Ok(())
    }),
    // Non-negative integer; 0 disables the Recovery Run mechanic.
    ("SUSPENDERS_RECOVERY_LIMIT", |cfg, v| {
        cfg.recovery_limit = parse_int(v, "SUSPENDERS_RECOVERY_LIMIT")?;
        Ok(())
    }),
    // "handoff" | "continuation". Note: the env parser trims whitespace
    // (via `parse_recovery_shape`), but the JSON path does not - serde
    // matches the string exactly. Accepted, not fixed: a stray space in
    // a hand-typed env var is likelier than in an editor-formatted file.
    ("SUSPENDERS_RECOVERY_SHAPE", |cfg, v| {
        cfg.recovery_shape = parse_recovery_shape(v)?;
        Ok(())
    }),
    // Non-negative integer; 0 disables the malformed-retry re-draw.
    ("SUSPENDERS_MALFORMED_RETRY_BUDGET", |cfg, v| {
        cfg.malformed_retry_budget = parse_int(v, "SUSPENDERS_MALFORMED_RETRY_BUDGET")?;
        Ok(())
    }),
    // Booleans.
    ("SUSPENDERS_SCOUT_NO_THINK", |cfg, v| {
        cfg.scout_no_think = parse_bool(v, "SUSPENDERS_SCOUT_NO_THINK")?;
        Ok(())
    }),
    ("SUSPENDERS_NO_THINK_RESCUE", |cfg, v| {
        cfg.no_think_rescue = parse_bool(v, "SUSPENDERS_NO_THINK_RESCUE")?;
        Ok(())
    }),
];

fn parse_int(raw: &str, name: &str) -> Result<u64, SessionError> {
    raw.trim()
        .parse::<u64>()
        .map_err(|_| SessionError(format!("{name} must be an integer, got: {raw:?}")))
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
        Ok(v) if (0.0..=2.0).contains(&v) => Ok(v),
        _ => Err(SessionError(format!(
            "SUSPENDERS_TEMPERATURE must be a float in [0.0, 2.0], got: {raw:?}"
        ))),
    }
}

fn parse_eviction_slack(raw: &str) -> Result<f64, SessionError> {
    match raw.trim().parse::<f64>() {
        Ok(v) if (0.0..1.0).contains(&v) => Ok(v),
        _ => Err(SessionError(format!(
            "SUSPENDERS_EVICTION_SLACK must be a fraction in [0.0, 1.0), got: {raw:?}"
        ))),
    }
}

fn parse_dead_mass_fraction(raw: &str) -> Result<f64, SessionError> {
    match raw.trim().parse::<f64>() {
        Ok(v) if v > 0.0 && v < 1.0 => Ok(v),
        _ => Err(SessionError(format!(
            "SUSPENDERS_DEAD_MASS_FRACTION must be a fraction in (0.0, 1.0), got: {raw:?}"
        ))),
    }
}

fn parse_plan_stale_after(raw: &str) -> Result<u64, SessionError> {
    match raw.trim().parse::<u64>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(SessionError(format!(
            "SUSPENDERS_PLAN_STALE_AFTER must be a positive integer, got: {raw:?}"
        ))),
    }
}

fn parse_recovery_shape(raw: &str) -> Result<RecoveryShape, SessionError> {
    RecoveryShape::parse(raw.trim()).ok_or_else(|| {
        SessionError(format!(
            "SUSPENDERS_RECOVERY_SHAPE must be \"handoff\" or \"continuation\", got: {raw:?}"
        ))
    })
}

fn parse_compaction_keep(raw: &str) -> Result<f64, SessionError> {
    match raw.trim().parse::<f64>() {
        Ok(v) if v > 0.0 && v < 1.0 => Ok(v),
        _ => Err(SessionError(format!(
            "SUSPENDERS_COMPACTION_KEEP must be a fraction in (0.0, 1.0), got: {raw:?}"
        ))),
    }
}

fn parse_bool(raw: &str, name: &str) -> Result<bool, SessionError> {
    match raw {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(SessionError(format!(
            "{name} must be \"true\" or \"false\", got: {raw:?}"
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
    pub eviction_slack: Option<f64>,
    pub dead_mass_fraction: Option<f64>,
    pub compaction_keep: Option<f64>,
    pub run_limit: Option<u64>,
    pub anchor_interval: Option<u64>,
    pub plan_stale_after: Option<u64>,
    pub recovery_limit: Option<u64>,
    pub recovery_shape: Option<RecoveryShape>,
    pub malformed_retry_budget: Option<u64>,
    pub scout_pass_limit: Option<u64>,
    pub scout_no_think: Option<bool>,
    pub no_think_rescue: Option<bool>,
    pub command_timeout_ms: Option<u64>,
    pub session_dir: Option<String>,
    /// A prebuilt launch Model, bypassing scoped-id resolution (the test seam;
    /// the Provider set still resolves from config).
    pub model: Option<Model>,
    pub temperature: Option<Option<f64>>,
}

impl Session {
    /// Builds and validates the Session's fixed facts, resolving config through
    /// the composition seam (base → file → env; ADR-0031). `root` defaults to
    /// the current dir.
    pub fn new(opts: SessionOpts) -> Result<Session, SessionError> {
        Session::build(opts, &SessionConfig::load())
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

        let session = Session {
            root: opts.root.unwrap_or_else(default_root),
            llm_module: opts.llm_module.unwrap_or_else(|| config.llm_module.clone()),
            extensions: opts.extensions.unwrap_or_else(|| config.extensions.clone()),
            context_budget,
            eviction_slack: opts.eviction_slack.unwrap_or(config.eviction_slack),
            dead_mass_fraction: opts.dead_mass_fraction.unwrap_or(config.dead_mass_fraction),
            compaction_keep: opts.compaction_keep.unwrap_or(config.compaction_keep),
            run_limit: opts.run_limit.unwrap_or(config.run_limit),
            anchor_interval: opts.anchor_interval.unwrap_or(config.anchor_interval),
            plan_stale_after: opts.plan_stale_after.unwrap_or(config.plan_stale_after),
            recovery_limit: opts.recovery_limit.unwrap_or(config.recovery_limit),
            recovery_shape: opts.recovery_shape.unwrap_or(config.recovery_shape),
            malformed_retry_budget: opts
                .malformed_retry_budget
                .unwrap_or(config.malformed_retry_budget),
            scout_pass_limit: opts.scout_pass_limit.unwrap_or(config.scout_pass_limit),
            scout_no_think: opts.scout_no_think.unwrap_or(config.scout_no_think),
            no_think_rescue: opts.no_think_rescue.unwrap_or(config.no_think_rescue),
            command_timeout_ms: opts.command_timeout_ms.unwrap_or(config.command_timeout_ms),
            session_dir: opts
                .session_dir
                .unwrap_or_else(|| config.session_dir.clone()),
            providers,
            model: launch_model,
            theme: config.theme.clone(),
            temperature: opts.temperature.unwrap_or(config.temperature),
            max_tokens: config.max_tokens,
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

    /// Checks the per-Model budget invariants for `model` (ADR-0037): its
    /// output cap must leave room in its effective Context Budget, and the
    /// Compaction Keep must sit below the compaction trigger at those figures.
    /// Run at launch for the launch Model and by the Agent at a `/model` swap
    /// for the picked Model, so a pick that cannot fit is rejected with the
    /// reason instead of exploding on a later Run.
    pub fn validate_model_budget(&self, model: &Model) -> Result<(), String> {
        let budget = self.context_budget_for(model);
        if model.max_tokens >= budget {
            return Err(format!(
                "model {}: :max_tokens ({}) must leave room in the Context Budget ({})",
                model.scoped_id(),
                model.max_tokens,
                budget
            ));
        }

        // Fire high, keep low: the Compaction Keep amount must sit below the
        // trigger. Comparing in u64 matches the old f64 check: the trigger is
        // integral, so trunc(keep) < trigger iff keep < trigger.
        let keep_amount =
            conversation::compaction_keep_amount(budget, model.max_tokens, self.compaction_keep);
        let trigger =
            conversation::compaction_target(budget, model.max_tokens, self.eviction_slack);
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

    /// The ctx every Tool Call executes with: the Project Root, the Result
    /// Cap derived from `model` - the one the Run captured (ADR-0037) - and
    /// the command timeout. (The `scout` capture is added later without
    /// changing tool signatures.)
    pub fn tool_ctx(&self, model: &Model) -> ToolCtx {
        ToolCtx {
            root: std::path::PathBuf::from(&self.root),
            result_cap: crate::tools::shaping::cap_for(
                self.context_budget_for(model),
                model.max_tokens,
            ),
            command_timeout_ms: self.command_timeout_ms,
            scout: None,
        }
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
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            format!("{home}/.local/share")
        });
    format!("{base}/suspenders/sessions")
}

// The user config file lives beside the Session Logs (ADR-0031): XDG config
// home, mirroring `default_session_dir` (empty var == unset, per XDG).
pub fn default_config_path() -> String {
    format!("{}/suspenders/config.json", xdg_config_base())
}

/// The user themes directory (ADR-0038): `themes/` beside `config.json` in
/// the XDG config home. Resolved once at the launch edge, like the config
/// path; a missing directory just means no user themes.
pub fn default_themes_dir() -> String {
    format!("{}/suspenders/themes", xdg_config_base())
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

// The thin impure reader (ADR-0031): reads the config at `path` and overlays it
// onto `cfg`. An absent file is an empty overlay (Ok, base defaults, no file
// touched); any IO or parse error becomes a [`SessionError`] naming `path`.
// `path` is an argument (not resolved here) so the overlay is testable without
// the real XDG dir.
fn load_file_overlay(cfg: &mut SessionConfig, path: &str) -> Result<(), SessionError> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            FileConfig::parse(&raw)
                .map_err(|e| SessionError(format!("invalid config at {path}: {e}")))?
                .apply(cfg);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
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
    pos_int(s.anchor_interval, ":anchor_interval")?;
    pos_int(s.plan_stale_after, ":plan_stale_after")?;
    pos_int(s.scout_pass_limit, ":scout_pass_limit")?;
    pos_int(s.command_timeout_ms, ":command_timeout_ms")?;

    fraction_left_closed(s.eviction_slack, ":eviction_slack")?;
    fraction_open(s.dead_mass_fraction, ":dead_mass_fraction")?;
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
        Some(v) if (0.0..=2.0).contains(&v) => Ok(()),
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
    if (0.0..1.0).contains(&value) {
        Ok(())
    } else {
        Err(SessionError(format!(
            "{name} must be a float in [0.0, 1.0)"
        )))
    }
}

fn fraction_open(value: f64, name: &str) -> Result<(), SessionError> {
    if value > 0.0 && value < 1.0 {
        Ok(())
    } else {
        Err(SessionError(format!(
            "{name} must be a float in (0.0, 1.0)"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::shaping;

    // A valid explicit launch Model, so tests never depend on config for it.
    fn test_model() -> Model {
        model_with_cap(1_000)
    }

    fn model_with_cap(max_tokens: u64) -> Model {
        Model::new(
            "local",
            "test-model",
            Api::AnthropicMessages,
            64_000,
            max_tokens,
        )
    }

    fn cfg() -> SessionConfig {
        SessionConfig::test_defaults()
    }

    // Sugar mirroring baud's `Session.new(root: "/tmp", ...)`.
    fn opts() -> SessionOpts {
        SessionOpts {
            root: Some("/tmp".into()),
            ..Default::default()
        }
    }

    // ---- new/1 ----

    #[test]
    fn defaults_come_from_config() {
        let session = Session::build(opts(), &cfg()).unwrap();
        assert_eq!(session.llm_module, "Suspenders.FakeLLM");
        assert_eq!(session.context_budget, cfg().context_budget);
        assert_eq!(session.model.max_tokens, cfg().max_tokens);
        assert_eq!(session.extensions, Vec::<String>::new());
    }

    #[test]
    fn the_launch_model_resolves_the_scoped_default_against_the_local_provider() {
        // Out-of-the-box behavior (ADR-0037): the default custom `local`
        // Provider carries today's default endpoint, and the default model is
        // scoped to it - splitting on the FIRST slash only.
        let session = Session::build(opts(), &cfg()).unwrap();
        assert_eq!(session.model.provider, "local");
        assert_eq!(session.model.id, "qwen/Qwen3.6-27B-MTP-GGUF");
        assert_eq!(session.model.api, Api::AnthropicMessages);
        assert_eq!(session.model.max_tokens, cfg().max_tokens);
        assert_eq!(session.temperature, cfg().temperature);

        let local = session.provider_of(&session.model).expect("local resolves");
        assert_eq!(local.base_url, "http://localhost:0/v1");
        assert_eq!(local.api, Api::AnthropicMessages);
        assert_eq!(local.context_window, Some(64_000));
    }

    #[test]
    fn the_provider_set_carries_customs_and_unshadowed_builtins() {
        let session = Session::build(opts(), &cfg()).unwrap();
        let ids: Vec<&str> = session.providers.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.contains(&"local"));
        assert!(ids.contains(&"anthropic"));

        // A custom entry with a built-in's id shadows it (config wins).
        let mut config = cfg();
        config.providers.insert(
            "anthropic".to_string(),
            ProviderConfig {
                base_url: "http://proxy:9000/v1".into(),
                api: Api::AnthropicMessages,
                context_window: Some(100_000),
                token: Some("proxy-token".into()),
            },
        );
        let session = Session::build(opts(), &config).unwrap();
        let anthropic: Vec<_> = session
            .providers
            .iter()
            .filter(|p| p.id == "anthropic")
            .collect();
        assert_eq!(anthropic.len(), 1);
        assert_eq!(anthropic[0].base_url, "http://proxy:9000/v1");
        assert_eq!(anthropic[0].token, "proxy-token");
    }

    #[test]
    fn an_unresolvable_launch_model_fails_launch_loudly() {
        // Unknown provider.
        let mut config = cfg();
        config.model = "nowhere/some-model".into();
        let err = Session::build(opts(), &config).unwrap_err();
        assert!(err.0.contains("nowhere"), "error was: {err}");

        // An unscoped id (no provider part) fails too.
        let mut config = cfg();
        config.model = "bare-model".into();
        let err = Session::build(opts(), &config).unwrap_err();
        assert!(err.0.contains("scoped"), "error was: {err}");
    }

    #[test]
    fn resolve_model_synthesizes_from_the_session_knobs_for_unknown_models() {
        let session = Session::build(opts(), &cfg()).unwrap();
        let model = session.resolve_model("local/another-model").unwrap();
        assert_eq!(model.provider, "local");
        assert_eq!(model.id, "another-model");
        // The custom Provider's config window and the Session's output-cap knob.
        assert_eq!(model.context_window, 64_000);
        assert_eq!(model.max_tokens, session.max_tokens);

        assert!(session.resolve_model("nowhere/m").is_err());
    }

    #[test]
    fn opts_override_config() {
        let o = SessionOpts {
            root: Some("/tmp".into()),
            llm_module: Some("SomeLLM".into()),
            extensions: Some(vec!["SomePlugin".into()]),
            context_budget: Some(5_000),
            eviction_slack: Some(0.1),
            compaction_keep: Some(0.4),
            run_limit: Some(3),
            anchor_interval: Some(7),
            command_timeout_ms: Some(1_000),
            model: Some(test_model()),
            ..Default::default()
        };
        let session = Session::build(o, &cfg()).unwrap();
        assert_eq!(session.llm_module, "SomeLLM");
        assert_eq!(session.extensions, vec!["SomePlugin".to_string()]);
        assert_eq!(session.context_budget, Some(5_000));
        assert_eq!(session.eviction_slack, 0.1);
        assert_eq!(session.compaction_keep, 0.4);
        assert_eq!(session.run_limit, 3);
        assert_eq!(session.anchor_interval, 7);
        assert_eq!(session.command_timeout_ms, 1_000);
        assert_eq!(session.model.max_tokens, 1_000);
    }

    #[test]
    fn anchor_interval_defaults_to_5_and_must_be_positive() {
        let session = Session::build(opts(), &cfg()).unwrap();
        assert_eq!(session.anchor_interval, 5);

        let err = Session::build(
            SessionOpts {
                anchor_interval: Some(0),
                model: Some(test_model()),
                ..opts()
            },
            &cfg(),
        )
        .unwrap_err();
        assert!(err.0.contains(":anchor_interval"));
    }

    #[test]
    fn compaction_keep_defaults_from_config() {
        let session = Session::build(opts(), &cfg()).unwrap();
        assert_eq!(session.compaction_keep, cfg().compaction_keep);
    }

    #[test]
    fn dead_mass_fraction_defaults_to_015_and_opts_override() {
        let session = Session::build(opts(), &cfg()).unwrap();
        assert_eq!(session.dead_mass_fraction, 0.15);

        let session = Session::build(
            SessionOpts {
                dead_mass_fraction: Some(0.3),
                model: Some(test_model()),
                ..opts()
            },
            &cfg(),
        )
        .unwrap();
        assert_eq!(session.dead_mass_fraction, 0.3);
    }

    #[test]
    fn dead_mass_fraction_must_be_strictly_inside_open_interval() {
        let with_fraction = |f: f64| {
            Session::build(
                SessionOpts {
                    dead_mass_fraction: Some(f),
                    model: Some(test_model()),
                    ..opts()
                },
                &cfg(),
            )
        };
        assert!(
            with_fraction(0.0)
                .unwrap_err()
                .0
                .contains(":dead_mass_fraction")
        );
        assert!(
            with_fraction(1.0)
                .unwrap_err()
                .0
                .contains(":dead_mass_fraction")
        );
        assert_eq!(with_fraction(0.15).unwrap().dead_mass_fraction, 0.15);
    }

    #[test]
    fn plan_stale_after_defaults_to_8_and_opts_override() {
        let session = Session::build(opts(), &cfg()).unwrap();
        assert_eq!(session.plan_stale_after, 8);

        let session = Session::build(
            SessionOpts {
                plan_stale_after: Some(12),
                model: Some(test_model()),
                ..opts()
            },
            &cfg(),
        )
        .unwrap();
        assert_eq!(session.plan_stale_after, 12);
    }

    #[test]
    fn plan_stale_after_must_be_positive() {
        let err = Session::build(
            SessionOpts {
                plan_stale_after: Some(0),
                model: Some(test_model()),
                ..opts()
            },
            &cfg(),
        )
        .unwrap_err();
        assert!(err.0.contains(":plan_stale_after"));
    }

    // ---- the per-Model budget derivation (ADR-0037) ----

    #[test]
    fn the_context_budget_is_the_captured_models_window_capped_by_config() {
        // No cap: the Model's own window IS the budget.
        let session = Session::build(opts(), &cfg()).unwrap();
        assert_eq!(session.context_budget, None);
        assert_eq!(
            session.context_budget_for(&session.model),
            session.model.context_window
        );

        // A cap set: the effective budget is min(cap, window), per Model.
        let session = Session::build(
            SessionOpts {
                context_budget: Some(5_000),
                model: Some(test_model()),
                ..opts()
            },
            &cfg(),
        )
        .unwrap();
        assert_eq!(session.context_budget_for(&session.model), 5_000);
        let wide = Model::new("local", "wide", Api::AnthropicMessages, 1_000_000, 1_000);
        assert_eq!(session.context_budget_for(&wide), 5_000);
        let narrow = Model::new("local", "narrow", Api::AnthropicMessages, 3_000, 1_000);
        assert_eq!(session.context_budget_for(&narrow), 3_000);
    }

    #[test]
    fn a_wide_window_catalog_model_validates_out_of_the_box() {
        // The Stage A sharp edge, fixed (ADR-0037): the launch validation runs
        // against the resolved Model's OWN figures, so the 1M-window /
        // 128K-output fable needs no config surgery.
        let mut config = cfg();
        config.model = "anthropic/claude-fable-5".into();
        let session = Session::build(opts(), &config).unwrap();
        assert_eq!(session.context_budget_for(&session.model), 1_000_000);
        assert_eq!(session.model.max_tokens, 128_000);
    }

    #[test]
    fn validate_model_budget_rejects_a_model_whose_cap_cannot_fit() {
        // The `/model` swap check (ADR-0037): the Session's max_tokens knob
        // (8_000) synthesizes a pick that cannot fit a 2_000 budget cap.
        let session = Session::build(
            SessionOpts {
                context_budget: Some(2_000),
                model: Some(test_model()),
                ..opts()
            },
            &cfg(),
        )
        .unwrap();
        let picked = session.resolve_model("local/another-model").unwrap();
        let err = session.validate_model_budget(&picked).unwrap_err();
        assert!(err.contains("local/another-model"), "error was: {err}");
        assert!(err.contains("leave room"), "error was: {err}");

        // The launch Model itself passes the same check.
        assert_eq!(session.validate_model_budget(&session.model), Ok(()));
    }

    #[test]
    fn the_result_cap_derives_from_the_captured_models_figures() {
        let session = Session::build(
            SessionOpts {
                context_budget: Some(5_000),
                model: Some(test_model()),
                ..opts()
            },
            &cfg(),
        )
        .unwrap();
        let ctx = session.tool_ctx(&session.model);
        assert_eq!(
            ctx.result_cap,
            shaping::cap_for(5_000, session.model.max_tokens)
        );
    }

    #[test]
    fn max_tokens_must_leave_room_in_budget() {
        let err = Session::build(
            SessionOpts {
                context_budget: Some(1_000),
                model: Some(test_model()),
                ..opts()
            },
            &cfg(),
        )
        .unwrap_err();
        assert!(err.0.contains("leave room"), "error was: {err}");
    }

    #[test]
    fn out_of_range_values_raise() {
        let with = |o: SessionOpts| Session::build(o, &cfg());

        assert!(
            with(SessionOpts {
                context_budget: Some(0),
                model: Some(test_model()),
                ..opts()
            })
            .unwrap_err()
            .0
            .contains(":context_budget")
        );

        assert!(
            with(SessionOpts {
                eviction_slack: Some(1.0),
                model: Some(test_model()),
                ..opts()
            })
            .unwrap_err()
            .0
            .contains(":eviction_slack")
        );

        assert!(
            with(SessionOpts {
                run_limit: Some(0),
                model: Some(test_model()),
                ..opts()
            })
            .unwrap_err()
            .0
            .contains(":turn_limit")
        );

        assert!(
            with(SessionOpts {
                command_timeout_ms: Some(0),
                model: Some(test_model()),
                ..opts()
            })
            .unwrap_err()
            .0
            .contains(":command_timeout_ms")
        );

        assert!(
            with(SessionOpts {
                model: Some(model_with_cap(0)),
                ..opts()
            })
            .unwrap_err()
            .0
            .contains("max_tokens")
        );
    }

    #[test]
    fn compaction_keep_must_be_strictly_inside_open_interval() {
        let with_keep = |k: f64| {
            Session::build(
                SessionOpts {
                    compaction_keep: Some(k),
                    model: Some(test_model()),
                    ..opts()
                },
                &cfg(),
            )
        };
        assert!(with_keep(0.0).unwrap_err().0.contains(":compaction_keep"));
        assert!(with_keep(1.0).unwrap_err().0.contains(":compaction_keep"));
        // baud's `2` (an integer) case: any value >= 1.0 fails the same way.
        assert!(with_keep(2.0).unwrap_err().0.contains(":compaction_keep"));
    }

    #[test]
    fn compaction_keep_amount_must_sit_below_trigger() {
        // live window = 10_000 - 1_000 = 9_000. trigger = 9_000 - 0.1*10_000 =
        // 8_000. 0.95 * 9_000 = 8_550 >= 8_000, so it must raise.
        let err = Session::build(
            SessionOpts {
                context_budget: Some(10_000),
                eviction_slack: Some(0.1),
                compaction_keep: Some(0.95),
                model: Some(model_with_cap(1_000)),
                ..opts()
            },
            &cfg(),
        )
        .unwrap_err();
        assert!(
            err.0.contains("Compaction Keep")
                || err.0.contains("below")
                || err.0.contains("fire high")
        );

        let session = Session::build(
            SessionOpts {
                context_budget: Some(10_000),
                eviction_slack: Some(0.1),
                compaction_keep: Some(0.5),
                model: Some(model_with_cap(1_000)),
                ..opts()
            },
            &cfg(),
        )
        .unwrap();
        assert_eq!(session.compaction_keep, 0.5);
    }

    // ---- recovery_limit / recovery_shape ----

    #[test]
    fn recovery_limit_defaults_to_1_and_opts_override_including_the_off_value() {
        let session = Session::build(opts(), &cfg()).unwrap();
        assert_eq!(session.recovery_limit, 1);

        let with_limit = |n: u64| {
            Session::build(
                SessionOpts {
                    recovery_limit: Some(n),
                    model: Some(test_model()),
                    ..opts()
                },
                &cfg(),
            )
            .unwrap()
        };
        assert_eq!(with_limit(3).recovery_limit, 3);
        // 0 is valid: it disables the Recovery Run mechanic entirely.
        assert_eq!(with_limit(0).recovery_limit, 0);
    }

    #[test]
    fn recovery_shape_defaults_to_handoff_and_opts_override() {
        let session = Session::build(opts(), &cfg()).unwrap();
        assert_eq!(session.recovery_shape, RecoveryShape::Handoff);

        let session = Session::build(
            SessionOpts {
                recovery_shape: Some(RecoveryShape::Continuation),
                model: Some(test_model()),
                ..opts()
            },
            &cfg(),
        )
        .unwrap();
        assert_eq!(session.recovery_shape, RecoveryShape::Continuation);
    }

    #[test]
    fn env_recovery_limit_is_a_non_negative_integer() {
        assert_eq!(parse_int("0", "SUSPENDERS_RECOVERY_LIMIT").unwrap(), 0);
        assert_eq!(parse_int("2", "SUSPENDERS_RECOVERY_LIMIT").unwrap(), 2);
        assert!(
            parse_int("-1", "SUSPENDERS_RECOVERY_LIMIT")
                .unwrap_err()
                .0
                .contains("SUSPENDERS_RECOVERY_LIMIT must be an integer")
        );
    }

    // ---- malformed_retry_budget ----

    #[test]
    fn malformed_retry_budget_defaults_to_3_and_opts_override_including_the_off_value() {
        let session = Session::build(opts(), &cfg()).unwrap();
        assert_eq!(session.malformed_retry_budget, 3);

        let with_budget = |n: u64| {
            Session::build(
                SessionOpts {
                    malformed_retry_budget: Some(n),
                    model: Some(test_model()),
                    ..opts()
                },
                &cfg(),
            )
            .unwrap()
        };
        assert_eq!(with_budget(5).malformed_retry_budget, 5);
        // 0 is valid: it disables the in-band re-draw entirely.
        assert_eq!(with_budget(0).malformed_retry_budget, 0);
    }

    #[test]
    fn env_malformed_retry_budget_is_a_non_negative_integer() {
        assert_eq!(
            parse_int("0", "SUSPENDERS_MALFORMED_RETRY_BUDGET").unwrap(),
            0
        );
        assert_eq!(
            parse_int("3", "SUSPENDERS_MALFORMED_RETRY_BUDGET").unwrap(),
            3
        );
        assert!(
            parse_int("-1", "SUSPENDERS_MALFORMED_RETRY_BUDGET")
                .unwrap_err()
                .0
                .contains("SUSPENDERS_MALFORMED_RETRY_BUDGET must be an integer")
        );
    }

    #[test]
    fn env_recovery_shape_names_the_two_arms_only() {
        assert_eq!(
            parse_recovery_shape("handoff").unwrap(),
            RecoveryShape::Handoff
        );
        assert_eq!(
            parse_recovery_shape(" continuation ").unwrap(),
            RecoveryShape::Continuation
        );
        assert_eq!(
            parse_recovery_shape("retry").unwrap_err().0,
            "SUSPENDERS_RECOVERY_SHAPE must be \"handoff\" or \"continuation\", got: \"retry\""
        );
    }

    // ---- scout_pass_limit ----

    #[test]
    fn scout_pass_limit_defaults_to_8() {
        assert_eq!(Session::build(opts(), &cfg()).unwrap().scout_pass_limit, 8);
    }

    #[test]
    fn scout_pass_limit_opts_override_and_must_be_positive() {
        let session = Session::build(
            SessionOpts {
                scout_pass_limit: Some(3),
                model: Some(test_model()),
                ..opts()
            },
            &cfg(),
        )
        .unwrap();
        assert_eq!(session.scout_pass_limit, 3);

        assert!(
            Session::build(
                SessionOpts {
                    scout_pass_limit: Some(0),
                    model: Some(test_model()),
                    ..opts()
                },
                &cfg()
            )
            .unwrap_err()
            .0
            .contains(":scout_pass_limit")
        );
    }

    // ---- scout_no_think ----

    #[test]
    fn scout_no_think_defaults_to_true() {
        assert!(Session::build(opts(), &cfg()).unwrap().scout_no_think);
    }

    #[test]
    fn scout_no_think_opts_override() {
        let session = Session::build(
            SessionOpts {
                scout_no_think: Some(false),
                model: Some(test_model()),
                ..opts()
            },
            &cfg(),
        )
        .unwrap();
        assert!(!session.scout_no_think);
    }

    // ---- no_think_rescue ----

    #[test]
    fn no_think_rescue_defaults_to_true() {
        assert!(Session::build(opts(), &cfg()).unwrap().no_think_rescue);
    }

    #[test]
    fn no_think_rescue_opts_override() {
        let session = Session::build(
            SessionOpts {
                no_think_rescue: Some(false),
                model: Some(test_model()),
                ..opts()
            },
            &cfg(),
        )
        .unwrap();
        assert!(!session.no_think_rescue);
    }

    // ---- SUSPENDERS_* env parsing/validation ----

    #[test]
    fn env_positive_int_parses_and_rejects() {
        assert_eq!(parse_positive_int("8000").unwrap(), 8_000);
        assert_eq!(
            parse_positive_int("0").unwrap_err().0,
            "SUSPENDERS_MAX_TOKENS must be a positive integer, got: \"0\""
        );
        assert!(
            parse_positive_int("-5")
                .unwrap_err()
                .0
                .contains("must be a positive integer")
        );
        assert!(
            parse_positive_int("nope")
                .unwrap_err()
                .0
                .contains("must be a positive integer")
        );
    }

    #[test]
    fn env_temperature_bounds() {
        assert_eq!(parse_temperature("0.0").unwrap(), 0.0);
        assert_eq!(parse_temperature("2.0").unwrap(), 2.0);
        assert!(
            parse_temperature("2.1")
                .unwrap_err()
                .0
                .contains("SUSPENDERS_TEMPERATURE must be a float in [0.0, 2.0]")
        );
        assert!(
            parse_temperature("-0.1")
                .unwrap_err()
                .0
                .contains("[0.0, 2.0]")
        );
        assert!(
            parse_temperature("hot")
                .unwrap_err()
                .0
                .contains("[0.0, 2.0]")
        );
    }

    #[test]
    fn env_eviction_slack_left_closed() {
        assert_eq!(parse_eviction_slack("0.0").unwrap(), 0.0);
        assert!(
            parse_eviction_slack("1.0")
                .unwrap_err()
                .0
                .contains("SUSPENDERS_EVICTION_SLACK must be a fraction in [0.0, 1.0)")
        );
    }

    #[test]
    fn env_dead_mass_fraction_open_interval() {
        assert_eq!(parse_dead_mass_fraction("0.15").unwrap(), 0.15);
        assert!(
            parse_dead_mass_fraction("0.0")
                .unwrap_err()
                .0
                .contains("SUSPENDERS_DEAD_MASS_FRACTION must be a fraction in (0.0, 1.0)")
        );
        assert!(
            parse_dead_mass_fraction("1.0")
                .unwrap_err()
                .0
                .contains("(0.0, 1.0)")
        );
    }

    #[test]
    fn env_plan_stale_after_positive_integer() {
        assert_eq!(parse_plan_stale_after("12").unwrap(), 12);
        assert_eq!(parse_plan_stale_after(" 8 ").unwrap(), 8);
        assert!(
            parse_plan_stale_after("0")
                .unwrap_err()
                .0
                .contains("SUSPENDERS_PLAN_STALE_AFTER must be a positive integer")
        );
        assert!(parse_plan_stale_after("eight").is_err());
    }

    #[test]
    fn env_compaction_keep_open_interval() {
        assert_eq!(parse_compaction_keep("0.5").unwrap(), 0.5);
        assert!(
            parse_compaction_keep("0.0")
                .unwrap_err()
                .0
                .contains("SUSPENDERS_COMPACTION_KEEP must be a fraction in (0.0, 1.0)")
        );
        assert!(
            parse_compaction_keep("1.0")
                .unwrap_err()
                .0
                .contains("(0.0, 1.0)")
        );
    }

    #[test]
    fn env_bool_true_false_only() {
        assert!(parse_bool("true", "SUSPENDERS_SCOUT_NO_THINK").unwrap());
        assert!(!parse_bool("false", "SUSPENDERS_SCOUT_NO_THINK").unwrap());
        assert_eq!(
            parse_bool("yes", "SUSPENDERS_SCOUT_NO_THINK")
                .unwrap_err()
                .0,
            "SUSPENDERS_SCOUT_NO_THINK must be \"true\" or \"false\", got: \"yes\""
        );
    }

    #[test]
    fn env_context_budget_integer() {
        assert_eq!(
            parse_int("64000", "SUSPENDERS_CONTEXT_BUDGET").unwrap(),
            64_000
        );
        assert!(
            parse_int("x", "SUSPENDERS_CONTEXT_BUDGET")
                .unwrap_err()
                .0
                .contains("SUSPENDERS_CONTEXT_BUDGET must be an integer")
        );
    }

    // ---- tool_ctx/1 ----

    #[test]
    fn tool_ctx_carries_root_result_cap_and_timeout() {
        let session = Session::build(
            SessionOpts {
                command_timeout_ms: Some(1_234),
                model: Some(test_model()),
                ..opts()
            },
            &cfg(),
        )
        .unwrap();
        let ctx = session.tool_ctx(&session.model);
        assert_eq!(ctx.root, std::path::PathBuf::from("/tmp"));
        assert_eq!(
            ctx.result_cap,
            shaping::cap_for(
                session.context_budget_for(&session.model),
                session.model.max_tokens
            )
        );
        assert_eq!(ctx.command_timeout_ms, 1_234);
    }

    // ---- FileConfig (ADR-0031: user config file) ----

    #[test]
    fn file_config_parse_accepts_a_sparse_subset() {
        let fc = FileConfig::parse(r#"{"model": "custom/model", "max_tokens": 4096}"#).unwrap();
        assert_eq!(fc.model.as_deref(), Some("custom/model"));
        assert_eq!(fc.max_tokens, Some(4096));
        // Absent keys stay None.
        assert_eq!(fc.providers, None);
        assert_eq!(fc.recovery_shape, None);
    }

    #[test]
    fn file_config_parse_rejects_an_unknown_key() {
        // deny_unknown_fields: a misspelled/excluded key is a hard error. The
        // message is path-agnostic (the reader wraps it with the path) but still
        // names the offending key.
        let err = FileConfig::parse(r#"{"max_token": 4096}"#).unwrap_err();
        assert!(err.0.contains("max_token"));

        // An excluded field (never in the DTO) is rejected the same way.
        assert!(FileConfig::parse(r#"{"turn_limit": 10}"#).is_err());

        // The retired flat keys (ADR-0037: base_url and token moved into the
        // providers table) are rejected, not silently honored.
        assert!(FileConfig::parse(r#"{"base_url": "http://x/v1"}"#).is_err());
        assert!(FileConfig::parse(r#"{"token": "sekrit"}"#).is_err());
    }

    #[test]
    fn file_config_parses_a_providers_table() {
        let fc = FileConfig::parse(
            r#"{"providers": {"lmstudio": {
                "base_url": "http://localhost:1234/v1",
                "api": "openai-completions",
                "context_window": 32768
            }}}"#,
        )
        .unwrap();
        let providers = fc.providers.clone().unwrap();
        let lmstudio = &providers["lmstudio"];
        assert_eq!(lmstudio.base_url, "http://localhost:1234/v1");
        assert_eq!(lmstudio.api, Api::OpenaiCompletions);
        assert_eq!(lmstudio.context_window, Some(32_768));
        assert_eq!(lmstudio.token, None);

        // The window is optional (ADR-0037): an entry without one leaves its
        // Models to the global `context_budget` figure.
        let fc = FileConfig::parse(
            r#"{"providers": {"lmstudio": {
                "base_url": "http://localhost:1234/v1",
                "api": "openai-completions"
            }}}"#,
        )
        .unwrap();
        assert_eq!(fc.providers.unwrap()["lmstudio"].context_window, None);

        // A provider entry is deny_unknown_fields too.
        assert!(
            FileConfig::parse(
                r#"{"providers": {"x": {
                    "base_url": "http://x/v1",
                    "api": "anthropic-messages",
                    "context_window": 1000,
                    "endpoint": "nope"
                }}}"#,
            )
            .is_err()
        );
    }

    #[test]
    fn file_config_parse_rejects_a_wrong_typed_value() {
        assert!(FileConfig::parse(r#"{"max_tokens": "lots"}"#).is_err());
    }

    #[test]
    fn file_config_recovery_shape_round_trips_the_lowercase_strings() {
        let fc = FileConfig::parse(r#"{"recovery_shape": "continuation"}"#).unwrap();
        assert_eq!(fc.recovery_shape, Some(RecoveryShape::Continuation));
        // And the enum serializes to exactly those strings.
        let json = serde_json::to_string(&RecoveryShape::Handoff).unwrap();
        assert_eq!(json, "\"handoff\"");
    }

    #[test]
    fn file_config_apply_overlays_only_present_fields() {
        let mut cfg = SessionConfig::test_defaults();
        let before_budget = cfg.context_budget;
        let fc = FileConfig {
            model: Some("overlaid/model".into()),
            recovery_shape: Some(RecoveryShape::Continuation),
            ..Default::default()
        };
        fc.apply(&mut cfg);
        assert_eq!(cfg.model, "overlaid/model");
        assert_eq!(cfg.recovery_shape, RecoveryShape::Continuation);
        // Absent fields untouched.
        assert_eq!(cfg.context_budget, before_budget);
    }

    #[test]
    fn out_of_range_file_value_surfaces_via_the_build_path() {
        // Range errors are NOT caught by parse(); they surface at validate().
        let mut cfg = SessionConfig::test_defaults();
        FileConfig::parse(r#"{"eviction_slack": 1.0}"#)
            .unwrap()
            .apply(&mut cfg);
        let err = Session::build(opts(), &cfg).unwrap_err();
        assert!(err.0.contains(":eviction_slack"));
    }

    #[test]
    fn write_template_omits_token_and_refuses_existing_without_force() {
        let path = std::env::temp_dir()
            .join(format!(
                "suspenders_write_config_{}.json",
                std::process::id()
            ))
            .to_string_lossy()
            .into_owned();
        let _ = std::fs::remove_file(&path);

        SessionConfig::write_template(&path, false).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        // token is never persisted (the "token" substring in "max_tokens" is
        // fine; the standalone key must be absent).
        assert!(!written.contains("\"token\""));
        // The template is full: it parses and round-trips a known key.
        let fc = FileConfig::parse(&written).unwrap();
        assert_eq!(
            fc.model.as_deref(),
            Some(SessionConfig::base().model.as_str())
        );
        assert_eq!(
            fc.recovery_shape,
            Some(SessionConfig::base().recovery_shape)
        );
        // The providers table rides the template, tokenless.
        let providers = fc.providers.clone().unwrap();
        assert!(providers["local"].token.is_none());

        // Refuses an existing target without force.
        let err = SessionConfig::write_template(&path, false).unwrap_err();
        assert!(err.0.contains(&path));
        // force overwrites.
        SessionConfig::write_template(&path, true).unwrap();

        let _ = std::fs::remove_file(&path);
    }

    // A temp path namespaced by PID + a caller label, so parallel tests never
    // collide on the filesystem seam.
    fn temp_config_path(label: &str) -> String {
        std::env::temp_dir()
            .join(format!(
                "suspenders_cfg_{}_{}.json",
                label,
                std::process::id()
            ))
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn load_file_overlay_applies_a_value_onto_base() {
        let path = temp_config_path("overlay_applies");
        std::fs::write(&path, r#"{"model": "from/file", "context_budget": 12345}"#).unwrap();

        let mut cfg = SessionConfig::test_defaults();
        load_file_overlay(&mut cfg, &path).unwrap();
        assert_eq!(cfg.model, "from/file");
        assert_eq!(cfg.context_budget, Some(12345));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_file_overlay_missing_file_is_ok_and_leaves_config_unchanged() {
        // Proves "absent file = defaults, no file touched" (ADR-0031).
        let path = temp_config_path("missing");
        let _ = std::fs::remove_file(&path);

        let mut cfg = SessionConfig::test_defaults();
        let before = cfg.clone();
        load_file_overlay(&mut cfg, &path).unwrap();
        assert_eq!(cfg, before);
        assert!(!std::path::Path::new(&path).exists());
    }

    #[test]
    fn write_template_round_trips_every_non_token_field_as_some() {
        // Lockstep guard (ADR-0031): the writer emits every schema key, and the
        // DTO parses them all back. A field the writer forgets - or a serde
        // rename that drifts - trips this. `token` is the sole intended None.
        let path = temp_config_path("round_trip");
        SessionConfig::write_template(&path, true).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let fc = FileConfig::parse(&raw).unwrap();

        assert!(fc.providers.is_some());
        assert!(fc.model.is_some());
        assert!(fc.theme.is_some());
        assert!(fc.max_tokens.is_some());
        assert!(fc.temperature.is_some());
        // The one deliberate absence besides token (ADR-0037): the base config
        // carries no global budget cap, so the template writes none.
        assert!(fc.context_budget.is_none());
        assert!(fc.eviction_slack.is_some());
        assert!(fc.dead_mass_fraction.is_some());
        assert!(fc.compaction_keep.is_some());
        assert!(fc.plan_stale_after.is_some());
        assert!(fc.recovery_limit.is_some());
        assert!(fc.recovery_shape.is_some());
        assert!(fc.malformed_retry_budget.is_some());
        assert!(fc.scout_no_think.is_some());
        assert!(fc.no_think_rescue.is_some());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn resolve_template_path_defaults_an_empty_path_to_the_xdg_config_path() {
        assert_eq!(
            SessionConfig::resolve_template_path(""),
            default_config_path()
        );
        // A non-empty path is used verbatim.
        assert_eq!(
            SessionConfig::resolve_template_path("/tmp/custom.json"),
            "/tmp/custom.json"
        );
    }

    // ---- apply_env (ADR-0031: the SUSPENDERS_* overlay) ----------------------
    //
    // These tests mutate this process's environment, which edition 2024 marks
    // unsafe (`set_var`/`remove_var` can race a concurrent `getenv`). Safety is
    // the runner's execution model: cargo-nextest runs each test in its own
    // process, and these tests spawn no threads, so nothing reads the
    // environment concurrently.

    fn set_env(name: &str, value: &str) {
        // SAFETY: process-per-test (see the section comment above).
        unsafe { std::env::set_var(name, value) };
    }

    // Clears every SUSPENDERS_* override by walking ENV_OVERRIDES (a new table
    // row is cleared for free), so each env test starts ambient-free.
    fn clear_suspenders_env() {
        for (name, _) in ENV_OVERRIDES {
            // SAFETY: process-per-test (see the section comment above).
            unsafe { std::env::remove_var(name) };
        }
    }

    #[test]
    fn apply_env_overlays_the_scoped_model_onto_its_field() {
        clear_suspenders_env();
        set_env("SUSPENDERS_MODEL", "env/model");

        let mut cfg = SessionConfig::test_defaults();
        SessionConfig::apply_env(&mut cfg).unwrap();

        assert_eq!(cfg.model, "env/model");
    }

    #[test]
    fn apply_env_overlays_each_numeric_var_onto_its_field() {
        clear_suspenders_env();
        set_env("SUSPENDERS_CONTEXT_BUDGET", "48000");
        set_env("SUSPENDERS_MAX_TOKENS", "2048");
        set_env("SUSPENDERS_TEMPERATURE", "1.5");
        set_env("SUSPENDERS_EVICTION_SLACK", "0.25");
        set_env("SUSPENDERS_DEAD_MASS_FRACTION", "0.3");
        set_env("SUSPENDERS_COMPACTION_KEEP", "0.4");
        set_env("SUSPENDERS_PLAN_STALE_AFTER", "6");
        // The two 0-disables knobs prove non-negative (not positive) parsing.
        set_env("SUSPENDERS_RECOVERY_LIMIT", "0");
        set_env("SUSPENDERS_MALFORMED_RETRY_BUDGET", "0");

        let mut cfg = SessionConfig::test_defaults();
        SessionConfig::apply_env(&mut cfg).unwrap();

        assert_eq!(cfg.context_budget, Some(48_000));
        assert_eq!(cfg.max_tokens, 2048);
        assert_eq!(cfg.temperature, Some(1.5));
        assert_eq!(cfg.eviction_slack, 0.25);
        assert_eq!(cfg.dead_mass_fraction, 0.3);
        assert_eq!(cfg.compaction_keep, 0.4);
        assert_eq!(cfg.plan_stale_after, 6);
        assert_eq!(cfg.recovery_limit, 0);
        assert_eq!(cfg.malformed_retry_budget, 0);
    }

    #[test]
    fn apply_env_overlays_the_shape_and_bool_vars_onto_their_fields() {
        clear_suspenders_env();
        // The shape parser trims a hand-typed stray space (documented quirk).
        set_env("SUSPENDERS_RECOVERY_SHAPE", " continuation ");
        set_env("SUSPENDERS_SCOUT_NO_THINK", "false");
        set_env("SUSPENDERS_NO_THINK_RESCUE", "false");

        // test_defaults has Handoff/true/true, so each landing is visible.
        let mut cfg = SessionConfig::test_defaults();
        SessionConfig::apply_env(&mut cfg).unwrap();

        assert_eq!(cfg.recovery_shape, RecoveryShape::Continuation);
        assert!(!cfg.scout_no_think);
        assert!(!cfg.no_think_rescue);
    }

    #[test]
    fn apply_env_with_nothing_set_leaves_the_config_untouched() {
        clear_suspenders_env();
        let mut cfg = SessionConfig::test_defaults();
        let before = cfg.clone();
        SessionConfig::apply_env(&mut cfg).unwrap();
        assert_eq!(cfg, before);
    }

    #[test]
    fn apply_env_rejects_a_malformed_integer() {
        clear_suspenders_env();
        set_env("SUSPENDERS_CONTEXT_BUDGET", "soon");
        let err = SessionConfig::apply_env(&mut SessionConfig::test_defaults()).unwrap_err();
        assert_eq!(
            err.0,
            "SUSPENDERS_CONTEXT_BUDGET must be an integer, got: \"soon\""
        );
    }

    #[test]
    fn apply_env_rejects_a_non_positive_integer() {
        clear_suspenders_env();
        set_env("SUSPENDERS_MAX_TOKENS", "0");
        let err = SessionConfig::apply_env(&mut SessionConfig::test_defaults()).unwrap_err();
        assert_eq!(
            err.0,
            "SUSPENDERS_MAX_TOKENS must be a positive integer, got: \"0\""
        );

        clear_suspenders_env();
        set_env("SUSPENDERS_PLAN_STALE_AFTER", "0");
        let err = SessionConfig::apply_env(&mut SessionConfig::test_defaults()).unwrap_err();
        assert_eq!(
            err.0,
            "SUSPENDERS_PLAN_STALE_AFTER must be a positive integer, got: \"0\""
        );
    }

    #[test]
    fn apply_env_rejects_an_out_of_range_temperature() {
        clear_suspenders_env();
        set_env("SUSPENDERS_TEMPERATURE", "2.5");
        let err = SessionConfig::apply_env(&mut SessionConfig::test_defaults()).unwrap_err();
        assert_eq!(
            err.0,
            "SUSPENDERS_TEMPERATURE must be a float in [0.0, 2.0], got: \"2.5\""
        );
    }

    #[test]
    fn apply_env_rejects_an_out_of_range_fraction() {
        // eviction_slack is half-open [0.0, 1.0): 1.0 falls outside.
        clear_suspenders_env();
        set_env("SUSPENDERS_EVICTION_SLACK", "1.0");
        let err = SessionConfig::apply_env(&mut SessionConfig::test_defaults()).unwrap_err();
        assert_eq!(
            err.0,
            "SUSPENDERS_EVICTION_SLACK must be a fraction in [0.0, 1.0), got: \"1.0\""
        );

        // dead_mass_fraction and compaction_keep are open (0.0, 1.0): the
        // endpoints fall outside.
        clear_suspenders_env();
        set_env("SUSPENDERS_DEAD_MASS_FRACTION", "0.0");
        let err = SessionConfig::apply_env(&mut SessionConfig::test_defaults()).unwrap_err();
        assert_eq!(
            err.0,
            "SUSPENDERS_DEAD_MASS_FRACTION must be a fraction in (0.0, 1.0), got: \"0.0\""
        );

        clear_suspenders_env();
        set_env("SUSPENDERS_COMPACTION_KEEP", "1.0");
        let err = SessionConfig::apply_env(&mut SessionConfig::test_defaults()).unwrap_err();
        assert_eq!(
            err.0,
            "SUSPENDERS_COMPACTION_KEEP must be a fraction in (0.0, 1.0), got: \"1.0\""
        );
    }

    #[test]
    fn apply_env_rejects_an_unrecognized_recovery_shape() {
        clear_suspenders_env();
        set_env("SUSPENDERS_RECOVERY_SHAPE", "retry");
        let err = SessionConfig::apply_env(&mut SessionConfig::test_defaults()).unwrap_err();
        assert_eq!(
            err.0,
            "SUSPENDERS_RECOVERY_SHAPE must be \"handoff\" or \"continuation\", got: \"retry\""
        );
    }

    #[test]
    fn apply_env_rejects_a_non_boolean_flag() {
        clear_suspenders_env();
        set_env("SUSPENDERS_SCOUT_NO_THINK", "yes");
        let err = SessionConfig::apply_env(&mut SessionConfig::test_defaults()).unwrap_err();
        assert_eq!(
            err.0,
            "SUSPENDERS_SCOUT_NO_THINK must be \"true\" or \"false\", got: \"yes\""
        );

        clear_suspenders_env();
        set_env("SUSPENDERS_NO_THINK_RESCUE", "1");
        let err = SessionConfig::apply_env(&mut SessionConfig::test_defaults()).unwrap_err();
        assert_eq!(
            err.0,
            "SUSPENDERS_NO_THINK_RESCUE must be \"true\" or \"false\", got: \"1\""
        );
    }

    #[test]
    fn apply_env_reports_the_first_malformed_value_in_table_order() {
        // Two malformed values: the error names the one whose row comes first
        // (CONTEXT_BUDGET precedes SCOUT_NO_THINK in ENV_OVERRIDES).
        clear_suspenders_env();
        set_env("SUSPENDERS_CONTEXT_BUDGET", "nope");
        set_env("SUSPENDERS_SCOUT_NO_THINK", "yes");
        let err = SessionConfig::apply_env(&mut SessionConfig::test_defaults()).unwrap_err();
        assert!(err.0.contains("SUSPENDERS_CONTEXT_BUDGET"));
    }

    #[test]
    fn apply_env_treats_a_set_but_empty_theme_as_unset() {
        // The XDG idiom, applied to THEME only: SUSPENDERS_THEME="" must not
        // become a theme named "" (a guaranteed per-launch fallback notice).
        clear_suspenders_env();
        set_env("SUSPENDERS_THEME", "");

        let mut cfg = SessionConfig::test_defaults();
        SessionConfig::apply_env(&mut cfg).unwrap();

        assert_eq!(cfg.theme, SessionConfig::test_defaults().theme);
    }

    // ---- persist_model (ADR-0033: sparse, sticky /model write) --------------

    #[test]
    fn persist_model_creates_the_file_when_absent() {
        // The sanctioned exception to no-auto-create: an explicit pick writes a
        // fresh `{"model": ...}` (ADR-0033 / ADR-0031 amendment).
        let path = temp_config_path("persist_creates");
        let _ = std::fs::remove_file(&path);
        assert!(!std::path::Path::new(&path).exists());

        SessionConfig::persist_model(&path, "picked/model").unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let fc = FileConfig::parse(&raw).unwrap();
        assert_eq!(fc.model.as_deref(), Some("picked/model"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn persist_model_merges_preserving_another_key_and_never_adds_token() {
        // Sparse read-modify-write: only `model` changes; the user's other keys
        // survive and `token` is never introduced by the tool.
        let path = temp_config_path("persist_merges");
        std::fs::write(&path, r#"{"context_budget": 12345, "model": "old/model"}"#).unwrap();

        SessionConfig::persist_model(&path, "new/model").unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        // token is never persisted (the "token" substring in "max_tokens" is
        // fine; the standalone key must be absent).
        assert!(!raw.contains("\"token\""));
        // The result re-parses via the DTO, with the merge applied and the
        // pre-existing key preserved.
        let fc = FileConfig::parse(&raw).unwrap();
        assert_eq!(fc.model.as_deref(), Some("new/model"));
        assert_eq!(fc.context_budget, Some(12345));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn persist_model_errors_on_a_malformed_existing_file() {
        let path = temp_config_path("persist_malformed");
        std::fs::write(&path, "{ not json").unwrap();

        let err = SessionConfig::persist_model(&path, "picked/model").unwrap_err();
        assert!(err.0.contains(&path));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn merge_key_starts_from_empty_when_absent() {
        // The pure seam: absent existing → a lone `model` object.
        let json = merge_key(None, "model", "solo/model").unwrap();
        let fc = FileConfig::parse(&json).unwrap();
        assert_eq!(fc.model.as_deref(), Some("solo/model"));
    }

    // ---- persist_theme (ADR-0038: the same sparse sticky write as /model) ----

    #[test]
    fn persist_theme_creates_the_file_when_absent() {
        // `/theme` shares `/model`'s sanctioned create-if-absent exception.
        let path = temp_config_path("persist_theme_creates");
        let _ = std::fs::remove_file(&path);

        SessionConfig::persist_theme(&path, "gruvbox").unwrap();

        let fc = FileConfig::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(fc.theme.as_deref(), Some("gruvbox"));
        assert_eq!(fc.model, None, "nothing but the theme key is written");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn persist_theme_sets_only_the_theme_key_preserving_the_rest() {
        let path = temp_config_path("persist_theme_merges");
        std::fs::write(&path, r#"{"model": "kept/model", "theme": "light"}"#).unwrap();

        SessionConfig::persist_theme(&path, "gruvbox").unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("\"token\""));
        let fc = FileConfig::parse(&raw).unwrap();
        assert_eq!(fc.theme.as_deref(), Some("gruvbox"));
        assert_eq!(
            fc.model.as_deref(),
            Some("kept/model"),
            "other keys survive"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn persist_key_replaces_via_a_same_dir_temp_file_and_cleans_it_up() {
        // The atomic shape: write-then-rename, so a crash mid-write can tear
        // only the temp file, never config.json. Observable from outside: the
        // write lands whole and no `.tmp` residue survives a clean persist.
        let path = temp_config_path("persist_atomic");
        std::fs::write(&path, r#"{"model": "kept/model"}"#).unwrap();

        SessionConfig::persist_theme(&path, "gruvbox").unwrap();

        let fc = FileConfig::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(fc.theme.as_deref(), Some("gruvbox"));
        assert_eq!(fc.model.as_deref(), Some("kept/model"));
        assert!(
            !std::path::Path::new(&format!("{path}.tmp")).exists(),
            "the temp file was renamed away"
        );

        let _ = std::fs::remove_file(&path);
    }

    // ---- the theme key (ADR-0038: file + env, precedence like `model`) -------

    #[test]
    fn theme_defaults_to_dark_and_the_session_carries_it_unvalidated() {
        assert_eq!(cfg().theme, "dark");
        let session = Session::build(opts(), &cfg()).unwrap();
        assert_eq!(session.theme, "dark");

        // Any name rides through - resolution (and the dark fallback) is the
        // UI's launch concern, never a Session validation failure.
        let mut config = cfg();
        config.theme = "no-such-theme".into();
        let session = Session::build(opts(), &config).unwrap();
        assert_eq!(session.theme, "no-such-theme");
    }

    #[test]
    fn file_config_theme_overlays_like_model() {
        let mut cfg = SessionConfig::test_defaults();
        FileConfig::parse(r#"{"theme": "solarized"}"#)
            .unwrap()
            .apply(&mut cfg);
        assert_eq!(cfg.theme, "solarized");
    }

    #[test]
    fn apply_env_overlays_the_theme_onto_its_field() {
        clear_suspenders_env();
        set_env("SUSPENDERS_THEME", "gruvbox");

        let mut cfg = SessionConfig::test_defaults();
        SessionConfig::apply_env(&mut cfg).unwrap();

        assert_eq!(cfg.theme, "gruvbox");
    }

    #[test]
    fn env_theme_shadows_a_file_theme() {
        // The same precedence as `model`: the file overlay lands first, the
        // env overlay wins per-invocation over it (ADR-0031/0038).
        clear_suspenders_env();
        set_env("SUSPENDERS_THEME", "from-env");

        let path = temp_config_path("theme_precedence");
        std::fs::write(&path, r#"{"theme": "from-file"}"#).unwrap();

        let mut cfg = SessionConfig::test_defaults();
        load_file_overlay(&mut cfg, &path).unwrap();
        assert_eq!(cfg.theme, "from-file");
        SessionConfig::apply_env(&mut cfg).unwrap();
        assert_eq!(cfg.theme, "from-env");

        let _ = std::fs::remove_file(&path);
    }
}
