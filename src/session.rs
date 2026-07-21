//! The Session's fixed facts (CONTEXT.md: Session), resolved and validated
//! once at launch: the Project Root, the Context Budget, the Eviction slack,
//! the Dead Mass fraction, the Compaction Keep, the Result Cap, the Turn
//! Limit, the Anchor cadence and stale-plan threshold, the Scout Pass cap,
//! the no-think knobs, the command timeout, the
//! Plugin list, the LLM module, and the model connection.
//!
//! This is the composition seam for configuration. [`Session::new`] resolves
//! these keys once (via [`SessionConfig::load`]) by overlaying, in order, the
//! hardcoded [`SessionConfig::base`] defaults, the user's `config.json`
//! (ADR-0031), and the `SUSPENDERS_*` environment (the file is the persistent
//! baseline; the environment still wins per-invocation over it); everything
//! downstream receives values from this struct, so the cross-module invariants
//! live in one constructor:
//!
//! * the Eviction reserve IS `connection.max_tokens` - one field, read by the
//!   Conversation and the LLM request alike, so they cannot drift
//! * `connection.max_tokens` must leave room in the Context Budget
//! * the Result Cap derives from the same two numbers, once, here
//!
//! Tests use [`Session::build`] with an explicit [`SessionConfig`] (no env
//! reads), so the config-default behavior is exercised without touching the
//! process environment.

pub mod connection;
pub mod log;

use crate::conversation;
use crate::tool::ToolCtx;
use connection::Connection;
use serde::{Deserialize, Serialize};

/// The shape of a Recovery Turn (CONTEXT.md: Recovery Turn, Continuation,
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
    /// The Session's Plugin list (opaque here; entries carried as names).
    pub plugins: Vec<String>,
    pub context_budget: u64,
    pub eviction_slack: f64,
    /// The Eviction mechanic's Dead Mass Setpoint: the fraction of the
    /// Context Budget that elidable dead content may occupy before a wave
    /// fires without budget pressure.
    pub dead_mass_fraction: f64,
    pub compaction_keep: f64,
    pub turn_limit: u64,
    pub anchor_interval: u64,
    /// The anchor Governor's stale-plan Setpoint: the Passes a Plan may sit
    /// unchanged - while writes land - before each Anchor carries the
    /// stale-plan line.
    pub plan_stale_after: u64,
    /// The Endgame Governor's recovery Setpoint: at most this many Recovery
    /// Turns may serve one user request. `0` disables the mechanic entirely.
    pub recovery_limit: u64,
    /// The Endgame Governor's recovery-shape Setpoint: which arm a Recovery
    /// Turn takes (CONTEXT.md: Handoff is the default shape).
    pub recovery_shape: RecoveryShape,
    /// The malformed-tool-call re-draw Setpoint (ADR-0030): at most this many
    /// in-band re-draws may follow a retryable generation error within one
    /// Turn. `0` disables the mechanic entirely (the loud failure runs
    /// immediately, as before).
    pub malformed_retry_budget: u64,
    pub scout_pass_limit: u64,
    pub scout_no_think: bool,
    pub no_think_rescue: bool,
    pub command_timeout_ms: u64,
    /// DERIVED: `cap_for(context_budget, connection.max_tokens)`.
    pub result_cap: usize,
    pub session_dir: String,
    pub connection: Connection,
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
    pub base_url: String,
    pub token: String,
    pub model: String,
    pub max_tokens: u64,
    pub temperature: Option<f64>,
    pub context_budget: u64,
    pub eviction_slack: f64,
    pub dead_mass_fraction: f64,
    pub compaction_keep: f64,
    pub llm_module: String,
    pub command_timeout_ms: u64,
    pub turn_limit: u64,
    pub anchor_interval: u64,
    pub plan_stale_after: u64,
    pub recovery_limit: u64,
    pub recovery_shape: RecoveryShape,
    pub malformed_retry_budget: u64,
    pub scout_pass_limit: u64,
    pub scout_no_think: bool,
    pub no_think_rescue: bool,
    pub plugins: Vec<String>,
    pub session_dir: String,
}

impl SessionConfig {
    /// The base config the app ships.
    pub fn base() -> Self {
        SessionConfig {
            base_url: "http://localhost:8888/v1".into(),
            token: "".into(),
            model: "qwen/Qwen3.6-27B-MTP-GGUF".into(),
            max_tokens: 8_000,
            temperature: Some(0.7),
            context_budget: 64_000,
            eviction_slack: 0.2,
            dead_mass_fraction: 0.15,
            compaction_keep: 0.5,
            llm_module: "Suspenders.LLM".into(),
            command_timeout_ms: 120_000,
            turn_limit: 32,
            anchor_interval: 5,
            plan_stale_after: 8,
            recovery_limit: 1,
            recovery_shape: RecoveryShape::Handoff,
            malformed_retry_budget: 3,
            scout_pass_limit: 8,
            scout_no_think: true,
            no_think_rescue: true,
            plugins: vec!["diff".into(), "run_command".into()],
            session_dir: default_session_dir(),
        }
    }

    /// The config the test env resolves to: fakes injected, empty plugin
    /// list, tmp session dir.
    pub fn test_defaults() -> Self {
        let mut cfg = SessionConfig::base();
        cfg.base_url = "http://localhost:0/v1".into();
        cfg.llm_module = "Suspenders.FakeLLM".into();
        cfg.plugins = vec![];
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
        // the suite deliberately never touches process env (edition 2024 makes
        // `set_var` unsafe/racy), so proving it would mean mutating the ambient
        // environment other tests share.
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
    /// file and env seams must be kept in lockstep").
    fn apply_env(cfg: &mut SessionConfig) -> Result<(), SessionError> {
        // Plain string overrides, set whenever present.
        if let Ok(v) = std::env::var("SUSPENDERS_URL") {
            cfg.base_url = v;
        }
        if let Ok(v) = std::env::var("SUSPENDERS_TOKEN") {
            cfg.token = v;
        }
        if let Ok(v) = std::env::var("SUSPENDERS_MODEL") {
            cfg.model = v;
        }

        // Integer.
        if let Ok(v) = std::env::var("SUSPENDERS_CONTEXT_BUDGET") {
            cfg.context_budget = parse_int(&v, "SUSPENDERS_CONTEXT_BUDGET")?;
        }

        // Positive integer.
        if let Ok(v) = std::env::var("SUSPENDERS_MAX_TOKENS") {
            cfg.max_tokens = parse_positive_int(&v)?;
        }

        // Float in [0.0, 2.0].
        if let Ok(v) = std::env::var("SUSPENDERS_TEMPERATURE") {
            cfg.temperature = Some(parse_temperature(&v)?);
        }

        // Fraction in [0.0, 1.0).
        if let Ok(v) = std::env::var("SUSPENDERS_EVICTION_SLACK") {
            cfg.eviction_slack = parse_eviction_slack(&v)?;
        }

        // Fraction in (0.0, 1.0).
        if let Ok(v) = std::env::var("SUSPENDERS_DEAD_MASS_FRACTION") {
            cfg.dead_mass_fraction = parse_dead_mass_fraction(&v)?;
        }

        // Fraction in (0.0, 1.0).
        if let Ok(v) = std::env::var("SUSPENDERS_COMPACTION_KEEP") {
            cfg.compaction_keep = parse_compaction_keep(&v)?;
        }

        // Positive integer.
        if let Ok(v) = std::env::var("SUSPENDERS_PLAN_STALE_AFTER") {
            cfg.plan_stale_after = parse_plan_stale_after(&v)?;
        }

        // Non-negative integer; 0 disables the Recovery Turn mechanic.
        if let Ok(v) = std::env::var("SUSPENDERS_RECOVERY_LIMIT") {
            cfg.recovery_limit = parse_int(&v, "SUSPENDERS_RECOVERY_LIMIT")?;
        }

        // "handoff" | "continuation". Note: the env parser trims whitespace
        // (via `parse_recovery_shape`), but the JSON path does not - serde
        // matches the string exactly. Accepted, not fixed: a stray space in
        // a hand-typed env var is likelier than in an editor-formatted file.
        if let Ok(v) = std::env::var("SUSPENDERS_RECOVERY_SHAPE") {
            cfg.recovery_shape = parse_recovery_shape(&v)?;
        }

        // Non-negative integer; 0 disables the malformed-retry re-draw.
        if let Ok(v) = std::env::var("SUSPENDERS_MALFORMED_RETRY_BUDGET") {
            cfg.malformed_retry_budget = parse_int(&v, "SUSPENDERS_MALFORMED_RETRY_BUDGET")?;
        }

        // Booleans.
        if let Ok(v) = std::env::var("SUSPENDERS_SCOUT_NO_THINK") {
            cfg.scout_no_think = parse_bool(&v, "SUSPENDERS_SCOUT_NO_THINK")?;
        }
        if let Ok(v) = std::env::var("SUSPENDERS_NO_THINK_RESCUE") {
            cfg.no_think_rescue = parse_bool(&v, "SUSPENDERS_NO_THINK_RESCUE")?;
        }

        Ok(())
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
            base_url: Some(base.base_url),
            // token is deliberately left None: never persist a secret.
            token: None,
            model: Some(base.model),
            max_tokens: Some(base.max_tokens),
            temperature: base.temperature,
            context_budget: Some(base.context_budget),
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
    /// config file (ADR-0033, ADR-0031 amendment): the user's other keys are
    /// preserved and `token` is never introduced by the tool. This is the one
    /// sanctioned exception to ADR-0031's no-auto-create - an explicit `/model`
    /// pick is a deliberate act, so the file is created if absent.
    ///
    /// If `path` exists it is parsed as a JSON object and only the `"model"` key
    /// is set; if absent, a `{"model": "..."}` file (and its parent dirs) is
    /// created. Malformed existing JSON is an [`Err`] naming `path` (mirroring
    /// [`load_file_overlay`]'s error style). Parsing splits into the pure
    /// [`merge_model`] and this thin impure reader/writer, the same split as
    /// [`FileConfig::parse`] vs [`load_file_overlay`].
    pub fn persist_model(path: &str, model: &str) -> Result<(), SessionError> {
        let existing = match std::fs::read_to_string(path) {
            Ok(raw) => Some(raw),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(SessionError(format!(
                    "failed to read config at {path}: {e}"
                )));
            }
        };

        let json = merge_model(existing.as_deref(), model)
            .map_err(|e| SessionError(format!("invalid config at {path}: {e}")))?;

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
}

/// Pure sparse merge of the `model` key into a config file's JSON (ADR-0033):
/// `existing` is the current file contents (or `None` when absent), and the
/// result is the pretty JSON to write back. Every other key is preserved; a
/// `token` key is never introduced (only the caller's existing one, if any,
/// survives). A malformed or non-object existing file is an [`Err`] carrying the
/// path-agnostic reason (the caller wraps it with the resolved path). Path-free
/// and side-effect-free, so it unit-tests with literals like [`FileConfig::parse`].
fn merge_model(existing: Option<&str>, model: &str) -> Result<String, String> {
    let mut value = match existing {
        None => serde_json::Value::Object(serde_json::Map::new()),
        Some(raw) => serde_json::from_str(raw).map_err(|e| e.to_string())?,
    };

    let obj = value
        .as_object_mut()
        .ok_or_else(|| "config root must be a JSON object".to_string())?;
    obj.insert("model".into(), serde_json::Value::String(model.to_string()));

    serde_json::to_string_pretty(&value).map_err(|e| e.to_string())
}

/// The user config file's schema (ADR-0031): exactly the env-settable key set,
/// every field `Option<T>` so an absent key is an empty overlay. The DTO and the
/// env seam are two serializations of one schema; the deliberately excluded
/// fields (`session_dir`, `llm_module`, `turn_limit`, `anchor_interval`,
/// `scout_pass_limit`, `plugins`) are simply absent, so `deny_unknown_fields`
/// rejects them for free. `token` is `Option` so [`write_template`] can omit it
/// while the user may still add it by hand.
///
/// This and [`SessionConfig::apply_env`] are the two serializations of one
/// schema; a new user-tunable knob is added to both seams or to neither
/// (ADR-0031: "the file and env seams must be kept in lockstep").
///
/// [`write_template`]: SessionConfig::write_template
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
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
    /// `cfg` untouched. Private on purpose: a `SessionConfig` mutated this way
    /// must still pass through `validate()` on the built [`Session`].
    fn apply(&self, cfg: &mut SessionConfig) {
        if let Some(v) = &self.base_url {
            cfg.base_url = v.clone();
        }
        if let Some(v) = &self.token {
            cfg.token = v.clone();
        }
        if let Some(v) = &self.model {
            cfg.model = v.clone();
        }
        if let Some(v) = self.max_tokens {
            cfg.max_tokens = v;
        }
        if let Some(v) = self.temperature {
            cfg.temperature = Some(v);
        }
        if let Some(v) = self.context_budget {
            cfg.context_budget = v;
        }
        if let Some(v) = self.eviction_slack {
            cfg.eviction_slack = v;
        }
        if let Some(v) = self.dead_mass_fraction {
            cfg.dead_mass_fraction = v;
        }
        if let Some(v) = self.compaction_keep {
            cfg.compaction_keep = v;
        }
        if let Some(v) = self.plan_stale_after {
            cfg.plan_stale_after = v;
        }
        if let Some(v) = self.recovery_limit {
            cfg.recovery_limit = v;
        }
        if let Some(v) = self.recovery_shape {
            cfg.recovery_shape = v;
        }
        if let Some(v) = self.malformed_retry_budget {
            cfg.malformed_retry_budget = v;
        }
        if let Some(v) = self.scout_no_think {
            cfg.scout_no_think = v;
        }
        if let Some(v) = self.no_think_rescue {
            cfg.no_think_rescue = v;
        }
    }
}

// ---- SUSPENDERS_* env parsing/validation (mirrors the runtime overrides) ----

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
    pub plugins: Option<Vec<String>>,
    pub context_budget: Option<u64>,
    pub eviction_slack: Option<f64>,
    pub dead_mass_fraction: Option<f64>,
    pub compaction_keep: Option<f64>,
    pub turn_limit: Option<u64>,
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
    pub connection: Option<Connection>,
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
        let connection = opts.connection.clone().unwrap_or_else(|| {
            Connection::new(
                config.base_url.clone(),
                config.token.clone(),
                config.model.clone(),
                config.max_tokens,
            )
            .with_temperature(config.temperature)
        });

        let context_budget = opts.context_budget.unwrap_or(config.context_budget);

        let session = Session {
            root: opts.root.unwrap_or_else(default_root),
            llm_module: opts.llm_module.unwrap_or_else(|| config.llm_module.clone()),
            plugins: opts.plugins.unwrap_or_else(|| config.plugins.clone()),
            context_budget,
            eviction_slack: opts.eviction_slack.unwrap_or(config.eviction_slack),
            dead_mass_fraction: opts.dead_mass_fraction.unwrap_or(config.dead_mass_fraction),
            compaction_keep: opts.compaction_keep.unwrap_or(config.compaction_keep),
            turn_limit: opts.turn_limit.unwrap_or(config.turn_limit),
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
            result_cap: crate::tools::shaping::cap_for(context_budget, connection.max_tokens),
            session_dir: opts
                .session_dir
                .unwrap_or_else(|| config.session_dir.clone()),
            connection,
        };

        validate(&session)?;
        Ok(session)
    }

    /// The ctx every Tool Call executes with: the Project Root, the Result
    /// Cap, and the command timeout. (The `scout` capture is added later
    /// without changing tool signatures.)
    pub fn tool_ctx(&self) -> ToolCtx {
        ToolCtx {
            root: std::path::PathBuf::from(&self.root),
            result_cap: self.result_cap,
            command_timeout_ms: self.command_timeout_ms,
            scout: None,
        }
    }
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
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            format!("{home}/.config")
        });
    format!("{base}/suspenders/config.json")
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
    validate_budget_fits(s)?;
    validate_keep_below_trigger(s)?;
    Ok(())
}

fn validate_scalars(s: &Session) -> Result<(), SessionError> {
    pos_int(s.context_budget, ":context_budget")?;
    pos_int(s.connection.max_tokens, "connection :max_tokens")?;
    pos_int(s.turn_limit, ":turn_limit")?;
    pos_int(s.anchor_interval, ":anchor_interval")?;
    pos_int(s.plan_stale_after, ":plan_stale_after")?;
    pos_int(s.scout_pass_limit, ":scout_pass_limit")?;
    pos_int(s.command_timeout_ms, ":command_timeout_ms")?;

    fraction_left_closed(s.eviction_slack, ":eviction_slack")?;
    fraction_open(s.dead_mass_fraction, ":dead_mass_fraction")?;
    fraction_open(s.compaction_keep, ":compaction_keep")?;
    temperature(s.connection.temperature)?;
    Ok(())
}

// The reserve must leave room in the budget.
fn validate_budget_fits(s: &Session) -> Result<(), SessionError> {
    if s.connection.max_tokens < s.context_budget {
        Ok(())
    } else {
        Err(SessionError(
            "connection :max_tokens must leave room in :context_budget".into(),
        ))
    }
}

// Fire high, keep low: the Compaction Keep amount must sit below the trigger.
// Comparing in u64 matches the old f64 check: the trigger is integral, so
// trunc(keep) < trigger iff keep < trigger for nonnegative keep.
fn validate_keep_below_trigger(s: &Session) -> Result<(), SessionError> {
    let keep_amount = conversation::compaction_keep_amount(
        s.context_budget,
        s.connection.max_tokens,
        s.compaction_keep,
    );
    let trigger = conversation::compaction_target(
        s.context_budget,
        s.connection.max_tokens,
        s.eviction_slack,
    );

    if keep_amount < trigger {
        Ok(())
    } else {
        Err(SessionError(
            ":compaction_keep is too high - the Compaction Keep must sit below the compaction trigger (fire high, keep low)".into(),
        ))
    }
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

    // A valid explicit connection, so tests never depend on config for it.
    fn connection() -> Connection {
        Connection::new("http://localhost:0/v1", "", "test-model", 1_000)
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
        assert_eq!(session.connection.max_tokens, cfg().max_tokens);
        assert_eq!(session.plugins, Vec::<String>::new());
    }

    #[test]
    fn opts_override_config() {
        let o = SessionOpts {
            root: Some("/tmp".into()),
            llm_module: Some("SomeLLM".into()),
            plugins: Some(vec!["SomePlugin".into()]),
            context_budget: Some(5_000),
            eviction_slack: Some(0.1),
            compaction_keep: Some(0.4),
            turn_limit: Some(3),
            anchor_interval: Some(7),
            command_timeout_ms: Some(1_000),
            connection: Some(connection()),
            ..Default::default()
        };
        let session = Session::build(o, &cfg()).unwrap();
        assert_eq!(session.llm_module, "SomeLLM");
        assert_eq!(session.plugins, vec!["SomePlugin".to_string()]);
        assert_eq!(session.context_budget, 5_000);
        assert_eq!(session.eviction_slack, 0.1);
        assert_eq!(session.compaction_keep, 0.4);
        assert_eq!(session.turn_limit, 3);
        assert_eq!(session.anchor_interval, 7);
        assert_eq!(session.command_timeout_ms, 1_000);
        assert_eq!(session.connection.max_tokens, 1_000);
    }

    #[test]
    fn anchor_interval_defaults_to_5_and_must_be_positive() {
        let session = Session::build(opts(), &cfg()).unwrap();
        assert_eq!(session.anchor_interval, 5);

        let err = Session::build(
            SessionOpts {
                anchor_interval: Some(0),
                connection: Some(connection()),
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
                connection: Some(connection()),
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
                    connection: Some(connection()),
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
                connection: Some(connection()),
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
                connection: Some(connection()),
                ..opts()
            },
            &cfg(),
        )
        .unwrap_err();
        assert!(err.0.contains(":plan_stale_after"));
    }

    #[test]
    fn result_cap_is_derived_once() {
        let session = Session::build(
            SessionOpts {
                context_budget: Some(5_000),
                connection: Some(connection()),
                ..opts()
            },
            &cfg(),
        )
        .unwrap();
        assert_eq!(
            session.result_cap,
            shaping::cap_for(5_000, session.connection.max_tokens)
        );
    }

    #[test]
    fn max_tokens_must_leave_room_in_budget() {
        let err = Session::build(
            SessionOpts {
                context_budget: Some(1_000),
                connection: Some(connection()),
                ..opts()
            },
            &cfg(),
        )
        .unwrap_err();
        assert!(err.0.contains("leave room in :context_budget"));
    }

    #[test]
    fn out_of_range_values_raise() {
        let with = |o: SessionOpts| Session::build(o, &cfg());

        assert!(
            with(SessionOpts {
                context_budget: Some(0),
                connection: Some(connection()),
                ..opts()
            })
            .unwrap_err()
            .0
            .contains(":context_budget")
        );

        assert!(
            with(SessionOpts {
                eviction_slack: Some(1.0),
                connection: Some(connection()),
                ..opts()
            })
            .unwrap_err()
            .0
            .contains(":eviction_slack")
        );

        assert!(
            with(SessionOpts {
                turn_limit: Some(0),
                connection: Some(connection()),
                ..opts()
            })
            .unwrap_err()
            .0
            .contains(":turn_limit")
        );

        assert!(
            with(SessionOpts {
                command_timeout_ms: Some(0),
                connection: Some(connection()),
                ..opts()
            })
            .unwrap_err()
            .0
            .contains(":command_timeout_ms")
        );

        assert!(
            with(SessionOpts {
                connection: Some(Connection::new(
                    "http://localhost:0/v1",
                    "",
                    "test-model",
                    0
                )),
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
                    connection: Some(connection()),
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
                connection: Some(Connection::new(
                    "http://localhost:0/v1",
                    "",
                    "test-model",
                    1_000,
                )),
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
                connection: Some(Connection::new(
                    "http://localhost:0/v1",
                    "",
                    "test-model",
                    1_000,
                )),
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
                    connection: Some(connection()),
                    ..opts()
                },
                &cfg(),
            )
            .unwrap()
        };
        assert_eq!(with_limit(3).recovery_limit, 3);
        // 0 is valid: it disables the Recovery Turn mechanic entirely.
        assert_eq!(with_limit(0).recovery_limit, 0);
    }

    #[test]
    fn recovery_shape_defaults_to_handoff_and_opts_override() {
        let session = Session::build(opts(), &cfg()).unwrap();
        assert_eq!(session.recovery_shape, RecoveryShape::Handoff);

        let session = Session::build(
            SessionOpts {
                recovery_shape: Some(RecoveryShape::Continuation),
                connection: Some(connection()),
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
                    connection: Some(connection()),
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
                connection: Some(connection()),
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
                    connection: Some(connection()),
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
                connection: Some(connection()),
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
                connection: Some(connection()),
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
                connection: Some(connection()),
                ..opts()
            },
            &cfg(),
        )
        .unwrap();
        let ctx = session.tool_ctx();
        assert_eq!(ctx.root, std::path::PathBuf::from("/tmp"));
        assert_eq!(ctx.result_cap, session.result_cap);
        assert_eq!(ctx.command_timeout_ms, 1_234);
    }

    // ---- FileConfig (ADR-0031: user config file) ----

    #[test]
    fn file_config_parse_accepts_a_sparse_subset() {
        let fc = FileConfig::parse(r#"{"model": "custom/model", "max_tokens": 4096}"#).unwrap();
        assert_eq!(fc.model.as_deref(), Some("custom/model"));
        assert_eq!(fc.max_tokens, Some(4096));
        // Absent keys stay None.
        assert_eq!(fc.base_url, None);
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
        assert!(fc.token.is_none());

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
        assert_eq!(cfg.context_budget, 12345);

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

        assert!(fc.base_url.is_some());
        assert!(fc.token.is_none());
        assert!(fc.model.is_some());
        assert!(fc.max_tokens.is_some());
        assert!(fc.temperature.is_some());
        assert!(fc.context_budget.is_some());
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
    fn merge_model_starts_from_empty_when_absent() {
        // The pure seam: absent existing → a lone `model` object.
        let json = merge_model(None, "solo/model").unwrap();
        let fc = FileConfig::parse(&json).unwrap();
        assert_eq!(fc.model.as_deref(), Some("solo/model"));
    }
}
