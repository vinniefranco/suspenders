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
//! ## The leaf, and what is wired above it
//!
//! Phase 2 delivered this fully unit-tested foundation. Phase 3a WIRED the four
//! tool-dispatch + permission events (`PreToolUse`, `PostToolUse`,
//! `PostToolUseFailure`, `PermissionRequest`) into the live run loop at
//! [`crate::run::hooks`] (the firing facade + production ShellExec/HttpPost
//! capabilities, which live ABOVE this leaf so the leaf never reaches up into
//! `run_command`), fired from the tool-dispatch seam [`crate::run::batch`]. Phase
//! 3b WIRES the remaining twelve lifecycle events through the SAME facade: the
//! Run-layer events fire from the loop (`UserPromptSubmit` at Run start, `Stop` /
//! `StopFailure` at the finish/error paths, `Pre`/`PostCompact` around the compact
//! Dep, `TodoCreated`/`TodoCompleted` off the Plan fold, `SubagentStart`/
//! `SubagentStop` bracketing the `agent` tool dispatch), and the Agent-layer events
//! fire from the actor (`SessionStart` in `init_agent`, `SessionEnd` at actor-loop
//! end, `Notification` at the ask-request broadcast). Skill-hook REGISTRATION on
//! skill invocation is Phase 4 - still out of scope. The rejected `function` hook
//! type and qwen's deferred surface (async hooks, the `sequential` flag,
//! extension/system scopes, `allowedEnvVars` / `env` / `headers` / `once` / `if`)
//! are recorded as OUT in ADR-0066.

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
