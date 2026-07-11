//! The Session's fixed facts (CONTEXT.md: Session), resolved and validated
//! once at launch: the Project Root, the Context Budget, the Eviction slack,
//! the Compaction Keep, the Result Cap, the Turn Limit, the Scout Pass cap,
//! the no-think knobs, the command timeout, the Plugin list, the LLM module,
//! and the model connection.
//!
//! This is the composition seam for configuration. [`Session::new`] is the
//! only place Suspenders reads the environment for these keys (via
//! [`SessionConfig::from_env`]); everything downstream receives values from
//! this struct, so the cross-module invariants live in one constructor:
//!
//! * the Eviction reserve IS `connection.max_tokens` — one field, read by the
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

/// The Session's fixed facts.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    /// The Project Root (captured once, never read from the cwd again).
    pub root: String,
    /// The module implementing the LLM boundary (a name; the trait wiring is a
    /// later phase — carried here as a module name).
    pub llm_module: String,
    /// The Session's Plugin list (opaque here; entries carried as names).
    pub plugins: Vec<String>,
    pub context_budget: u64,
    pub eviction_slack: f64,
    pub compaction_keep: f64,
    pub turn_limit: u64,
    pub anchor_interval: u64,
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
/// [`SessionConfig::from_env`] is the single env seam and tests pass
/// [`SessionConfig::test_defaults`] explicitly (no env reads).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionConfig {
    pub base_url: String,
    pub token: String,
    pub model: String,
    pub max_tokens: u64,
    pub temperature: Option<f64>,
    pub context_budget: u64,
    pub eviction_slack: f64,
    pub compaction_keep: f64,
    pub llm_module: String,
    pub command_timeout_ms: u64,
    pub turn_limit: u64,
    pub anchor_interval: u64,
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
            base_url: "http://studio-win.local:8888/v1".into(),
            token: "".into(),
            model: "qwen/qwen3.5-9b".into(),
            max_tokens: 8_000,
            temperature: Some(0.7),
            context_budget: 64_000,
            eviction_slack: 0.2,
            compaction_keep: 0.5,
            llm_module: "Baud.LLM".into(),
            command_timeout_ms: 120_000,
            turn_limit: 25,
            anchor_interval: 5,
            scout_pass_limit: 8,
            scout_no_think: true,
            no_think_rescue: true,
            plugins: vec!["diff".into()],
            session_dir: default_session_dir(),
        }
    }

    /// The config the test env resolves to: fakes injected, empty plugin
    /// list, tmp session dir.
    pub fn test_defaults() -> Self {
        let mut cfg = SessionConfig::base();
        cfg.base_url = "http://localhost:0/v1".into();
        cfg.llm_module = "Baud.FakeLLM".into();
        cfg.plugins = vec![];
        cfg.session_dir = std::env::temp_dir()
            .join("suspenders_test_sessions")
            .to_string_lossy()
            .into_owned();
        cfg
    }

    /// The single env seam: overlays `SUSPENDERS_*` environment variables on
    /// the base config, with the same parsing and validation the runtime
    /// documents. A malformed value is a hard error (the same reasons carried
    /// as a [`SessionError`]) rather than a silent fallback.
    pub fn from_env() -> Self {
        SessionConfig::try_from_env().expect("invalid SUSPENDERS_* environment configuration")
    }

    /// The fallible env seam: reads each `SUSPENDERS_*` override and validates
    /// it, returning the reason on the first malformed value. [`from_env`] is
    /// the panicking convenience over this.
    pub fn try_from_env() -> Result<Self, SessionError> {
        let mut cfg = SessionConfig::base();

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
        if let Ok(v) = std::env::var("SUSPENDERS_COMPACTION_KEEP") {
            cfg.compaction_keep = parse_compaction_keep(&v)?;
        }

        // Booleans.
        if let Ok(v) = std::env::var("SUSPENDERS_SCOUT_NO_THINK") {
            cfg.scout_no_think = parse_bool(&v, "SUSPENDERS_SCOUT_NO_THINK")?;
        }
        if let Ok(v) = std::env::var("SUSPENDERS_NO_THINK_RESCUE") {
            cfg.no_think_rescue = parse_bool(&v, "SUSPENDERS_NO_THINK_RESCUE")?;
        }

        Ok(cfg)
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
    pub compaction_keep: Option<f64>,
    pub turn_limit: Option<u64>,
    pub anchor_interval: Option<u64>,
    pub scout_pass_limit: Option<u64>,
    pub scout_no_think: Option<bool>,
    pub no_think_rescue: Option<bool>,
    pub command_timeout_ms: Option<u64>,
    pub session_dir: Option<String>,
    pub connection: Option<Connection>,
}

impl Session {
    /// Builds and validates the Session's fixed facts, reading config from the
    /// environment (the single env seam). `root` defaults to the current dir.
    pub fn new(opts: SessionOpts) -> Result<Session, SessionError> {
        Session::build(opts, &SessionConfig::from_env())
    }

    /// Builds and validates against an explicit [`SessionConfig`] — the
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
            compaction_keep: opts.compaction_keep.unwrap_or(config.compaction_keep),
            turn_limit: opts.turn_limit.unwrap_or(config.turn_limit),
            anchor_interval: opts.anchor_interval.unwrap_or(config.anchor_interval),
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

// Session Logs live outside the Project Root (ADR-0010): XDG data home.
fn default_session_dir() -> String {
    let base = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{home}/.local/share")
    });
    format!("{base}/suspenders/sessions")
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
    pos_int(s.scout_pass_limit, ":scout_pass_limit")?;
    pos_int(s.command_timeout_ms, ":command_timeout_ms")?;

    fraction_left_closed(s.eviction_slack, ":eviction_slack")?;
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
fn validate_keep_below_trigger(s: &Session) -> Result<(), SessionError> {
    let live_window = (s.context_budget - s.connection.max_tokens) as f64;
    let keep_amount = s.compaction_keep * live_window;
    let trigger = conversation::compaction_target(
        s.context_budget,
        s.connection.max_tokens,
        s.eviction_slack,
    ) as f64;

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
        assert_eq!(session.llm_module, "Baud.FakeLLM");
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
}
