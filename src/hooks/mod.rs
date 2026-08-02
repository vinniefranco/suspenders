//! The hook subsystem (ADR-0066) - user- and skill-configured actions fired at
//! named lifecycle events, a faithful port of qwen v0.16.0's hooks
//! (`hooks/types.ts`, `hookRunner.ts`, `hookRegistry.ts`, `hookPlanner.ts`).
//!
//! A *hook* is a CONFIGURED action (never a compiled callback), declared under a
//! `hooks` key in `config.json` (standing) or a `SKILL.md` frontmatter `hooks:`
//! block (session-scoped, registered when the model invokes the skill). Its JSON
//! output can observe, halt, decide, or enrich the surrounding Run.
//!
//! ## The subsystem, a self-contained leaf
//!
//! This is a LEAF module (ADR-0066): it depends DOWNWARD (on [`crate::llm`],
//! [`crate::content`]) but NOT on run/agent/ui/session. The pieces mirror the
//! domain (ADR-0022):
//!
//! - [`event`] - [`HookEvent`], qwen's sixteen lifecycle events, serde-mapped to
//!   their exact wire names.
//! - [`config`] - the config model ([`HookConfig`] / [`HookDefinition`] /
//!   [`Hook`] / [`HookKind`]) and its ONE fail-open parser
//!   ([`parse_hooks`](config::parse_hooks)), serving BOTH the `config.json`
//!   `serde_json::Value` and the `SKILL.md` YAML block (converted to the same
//!   `Value` shape first, [`hooks_value_from_yaml`](config::hooks_value_from_yaml)).
//! - [`outcome`] - [`HookOutcome`], the decision protocol: qwen's steering fields
//!   VERBATIM plus its helper methods (is-blocking, should-stop, additional-
//!   context escape, permission-decision with the base-`decision` fallback).
//! - [`runner`] - [`run_hook`](runner::run_hook), executing ONE hook to a
//!   [`HookOutcome`]. The command / http / prompt capabilities are INJECTED
//!   ([`ShellExec`](runner::ShellExec) / [`HttpPost`](runner::HttpPost) / the
//!   existing [`Llm`](crate::llm::Llm)) so the leaf never reaches up into
//!   `run_command`'s shell exec (which would drag in an `agent` edge and invert
//!   the SDP); the production impls (Phase 3) reuse ADR-0023 process-group
//!   isolation and `reqwest`.
//! - [`manager`] - [`HookManager`], the fail-open resolution front mirroring
//!   [`crate::skills::SkillManager`]: it resolves hooks from config + invoked
//!   skills, groups by event, exposes
//!   [`hooks_for`](manager::HookManager::hooks_for) with matcher filtering, and
//!   records [`failures`](manager::HookManager::failures).
//!
//! ## What is out of THIS phase (Phase 2)
//!
//! This phase delivers the fully unit-tested foundation. WIRING the sixteen
//! fire-points into the run loop and integrating the decision (approvals /
//! StopReason / tool dispatch) is Phase 3 (ADR-0066), out of scope here. The
//! rejected `function` hook type and qwen's deferred surface (async hooks, the
//! `sequential` flag, extension/system scopes, `allowedEnvVars` / `env` /
//! `headers` / `once` / `if`) are recorded as OUT in ADR-0066.

pub mod config;
pub mod event;
pub mod manager;
pub mod outcome;
pub mod runner;

pub use config::{Hook, HookConfig, HookDefinition, HookKind};
pub use event::HookEvent;
pub use manager::{HookManager, SelectedHook};
pub use outcome::{Decision, HookOutcome, PermissionDecision};
pub use runner::{HookCaps, HookRunContext, HttpPost, ShellExec, ShellResult, run_hook};
