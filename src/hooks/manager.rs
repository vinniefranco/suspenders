//! [`HookManager`] - the fail-open resolution front for the hook subsystem
//! (ADR-0066), mirroring [`crate::skills::SkillManager`] and
//! [`crate::mcp::manager::McpManager`].
//!
//! The manager resolves hooks from the two ADR-0066 sources (the standing
//! `config.json` `hooks` value and the session-scoped hooks of the skills the
//! model has invoked), records every parse/discovery failure fail-open on
//! [`failures`](HookManager::failures), and exposes
//! [`hooks_for`](HookManager::hooks_for): the hooks that should fire for an event,
//! already filtered by the tool-name matcher on a tool event.
//!
//! A matcher is qwen's regex-or-exact (a valid regex is `test`ed against the tool
//! name; an invalid pattern falls back to an exact string compare, ADR-0066). An
//! absent/empty/`*` matcher matches all tools, and a matcher is inert on a
//! non-tool event (every non-tool hook fires).
//!
//! This is a LEAF: it depends only on the sibling `event` / `config` modules,
//! `serde_json`, and `regex` - never on run/agent/ui/session.

use regex::Regex;

use crate::hooks::config::{Hook, HookConfig, HookDefinition, parse_hooks};
use crate::hooks::event::HookEvent;

/// One resolved source of hooks, tagged so a skill hook can carry its skill root
/// to the runner (ADR-0066: a skill-sourced command hook sees
/// `SUSPENDERS_SKILL_ROOT`). A `config.json` hook has no root.
struct HookSource {
    /// The parsed hooks from this source.
    config: HookConfig,
    /// The skill's directory for a skill source (so the runner sets
    /// `SUSPENDERS_SKILL_ROOT`), or `None` for the standing `config.json` source.
    skill_root: Option<String>,
}

/// One hook selected for an event: the [`Hook`] itself plus the skill root of its
/// source (for the runner's env), so [`hooks_for`](HookManager::hooks_for)'s
/// caller (Phase 3) knows which command hooks get a `SUSPENDERS_SKILL_ROOT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedHook<'a> {
    /// The resolved hook to run.
    pub hook: &'a Hook,
    /// The skill root of the hook's source, or `None` for a `config.json` hook.
    pub skill_root: Option<&'a str>,
}

/// The fail-open resolution front (ADR-0066): the standing `config.json` source
/// plus the session-scoped skill sources, and the accumulated parse failures.
/// Mirrors [`crate::skills::SkillManager`]'s `(source, reason)` failure list. The
/// derived [`Default`] (no sources, no failures) is the empty shape a Session with
/// no hooks holds.
#[derive(Default)]
pub struct HookManager {
    /// The resolved sources, standing source first then skills in invocation
    /// order, so a standing hook fires before a same-event skill hook.
    sources: Vec<HookSource>,
    /// The per-source parse failures `(context, reason)`, surfaced as launch
    /// notices (Phase 3), the same fail-open report a skill/MCP failure takes.
    failures: Vec<(String, String)>,
}

impl HookManager {
    /// Builds a manager from the standing `config.json` `hooks` value (ADR-0031's
    /// `FileConfig` carries it as a `serde_json::Value`). `None` when the config
    /// declares no `hooks` block - the common case. A malformed block is parsed
    /// fail-open: the good hooks load, the bad entries land on
    /// [`failures`](HookManager::failures).
    pub fn from_config(hooks: Option<&serde_json::Value>) -> HookManager {
        let mut failures: Vec<(String, String)> = Vec::new();
        let config = match hooks {
            Some(value) => parse_hooks(value, "config.json", &mut failures),
            None => HookConfig::default(),
        };
        HookManager {
            sources: vec![HookSource {
                config,
                skill_root: None,
            }],
            failures,
        }
    }

    /// Registers a skill's session-scoped hooks (ADR-0066): parsed from the
    /// skill's frontmatter `hooks:` value (already converted to a
    /// `serde_json::Value` by [`crate::hooks::config::hooks_value_from_yaml`]),
    /// tagged with the skill's `skill_root` so its command hooks get a
    /// `SUSPENDERS_SKILL_ROOT`. Called when the model invokes the skill, not at
    /// discovery time (Phase 3 wiring). A malformed block is fail-open, labeled
    /// `"skill <name>"` in any failure.
    pub fn register_skill(
        &mut self,
        name: &str,
        skill_root: &str,
        hooks: &serde_json::Value,
    ) {
        let context = format!("skill {name}");
        let config = parse_hooks(hooks, &context, &mut self.failures);
        self.sources.push(HookSource {
            config,
            skill_root: Some(skill_root.to_string()),
        });
    }

    /// The hooks that should fire for `event`, in source order (standing before
    /// skill), each already filtered by its definition's matcher against
    /// `tool_name` (ADR-0066). `tool_name` is the tool being dispatched on a tool
    /// event; pass `None` on a non-tool event (where a matcher is inert and every
    /// hook fires). Each result carries its source's skill root for the runner's
    /// env.
    pub fn hooks_for(&self, event: HookEvent, tool_name: Option<&str>) -> Vec<SelectedHook<'_>> {
        let mut selected: Vec<SelectedHook<'_>> = Vec::new();
        for source in &self.sources {
            for def in source.config.definitions(event) {
                if !definition_matches(def, event, tool_name) {
                    continue;
                }
                for hook in &def.hooks {
                    selected.push(SelectedHook {
                        hook,
                        skill_root: source.skill_root.as_deref(),
                    });
                }
            }
        }
        selected
    }

    /// The per-source parse failures `(context, reason)`, surfaced as one launch
    /// notice each (Phase 3), the same fail-open report line a skill/MCP failure
    /// takes.
    pub fn failures(&self) -> &[(String, String)] {
        &self.failures
    }
}

/// Whether a definition's matcher admits `tool_name` for `event` (ADR-0066). On a
/// non-tool event a matcher is inert (every definition matches). On a tool event
/// an absent/empty/`*` matcher matches all; otherwise the matcher is tried as a
/// regex against the tool name, falling back to an exact string compare on an
/// invalid pattern (qwen's `matchesToolName`). A tool event with no `tool_name`
/// (a defensive caller) matches only the match-all definitions.
fn definition_matches(def: &HookDefinition, event: HookEvent, tool_name: Option<&str>) -> bool {
    // A matcher only scopes a tool event; on every other event all definitions
    // fire regardless of any (inert) matcher.
    if !event.is_tool_event() {
        return true;
    }

    let matcher = def.matcher.as_deref().map(str::trim).unwrap_or("");
    // Empty or `*` matches all tools (qwen).
    if matcher.is_empty() || matcher == "*" {
        return true;
    }

    // A tool event with a real matcher needs a tool name to compare against.
    let Some(tool_name) = tool_name else {
        return false;
    };

    match Regex::new(matcher) {
        // A valid regex is tested against the tool name (qwen `regex.test`).
        Ok(re) => re.is_match(tool_name),
        // An invalid regex falls back to an exact string compare (qwen).
        Err(_) => matcher == tool_name,
    }
}

#[cfg(test)]
#[path = "../../tests/hooks/manager.rs"]
mod tests;
