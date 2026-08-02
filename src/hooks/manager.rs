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
//! ## Session-scoped skill hooks: interior-mutable, registered on invocation
//!
//! The standing `config.json` source is resolved once at build. A skill's
//! `hooks:` are SESSION-scoped and registered LATER, the moment the model invokes
//! that skill (Phase 4c, ADR-0066). Because the live manager is held behind an
//! `Arc` (shared by every Run's [`crate::run::hooks::Hooks`] handle), registration
//! cannot take `&mut self`. So the skill sources live in a shared, interior-mutable
//! store ([`SkillHooks`]) behind an `Arc<Mutex<..>>`, mirroring
//! [`crate::skills::SkillManager`]'s activation registry. [`register_skill`] is
//! idempotent (a second invocation of the same skill re-registers nothing), and
//! [`hooks_for`] merges the standing source with the locked skill sources.
//!
//! This is a LEAF: it depends only on the sibling `event` / `config` modules,
//! `serde_json`, `regex`, and `std::sync` - never on run/agent/ui/session.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use regex::Regex;

use crate::hooks::config::{Hook, HookConfig, HookDefinition, parse_hooks};
use crate::hooks::event::HookEvent;

/// One resolved source of hooks, tagged so a skill hook can carry its skill root
/// to the runner (ADR-0066: a skill-sourced command hook sees
/// `SUSPENDERS_SKILL_ROOT`). A `config.json` hook has no root.
#[derive(Debug, Clone)]
struct HookSource {
    /// The parsed hooks from this source.
    config: HookConfig,
    /// The skill's directory for a skill source (so the runner sets
    /// `SUSPENDERS_SKILL_ROOT`), or `None` for the standing `config.json` source.
    skill_root: Option<String>,
}

/// One hook selected for an event: the [`Hook`] itself plus the skill root of its
/// source (for the runner's env), so [`hooks_for`](HookManager::hooks_for)'s
/// caller (Phase 3) knows which command hooks get a `SUSPENDERS_SKILL_ROOT`. Owned
/// (not borrowed) because a skill source is read out from behind a `Mutex`: a
/// returned reference could not outlive the lock guard, so a selection clones the
/// hook (a few strings per fired event) to be usable after the lock releases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedHook {
    /// The resolved hook to run.
    pub hook: Hook,
    /// The skill root of the hook's source, or `None` for a `config.json` hook.
    pub skill_root: Option<String>,
}

/// The shared, interior-mutable session-scoped skill hooks (ADR-0066, Phase 4c),
/// mirroring [`crate::skills`]'s `ActivationRegistry`. Registered when the model
/// invokes a skill (not at discovery), so a skill that is never invoked
/// contributes nothing. Wrapped in `Arc<Mutex<..>>` on the manager so the run
/// layer can register through the shared `Arc<HookManager>` while a Run's
/// [`hooks_for`](HookManager::hooks_for) reads the same set.
#[derive(Debug, Default)]
struct SkillHooks {
    /// The skill sources in invocation order, each fired AFTER the standing source
    /// (a standing hook fires before a same-event skill hook).
    sources: Vec<HookSource>,
    /// The per-skill parse failures `(context, reason)`, fail-open (ADR-0066), the
    /// same report a config parse failure takes.
    failures: Vec<(String, String)>,
    /// The names already registered, so a second invocation of the same skill is a
    /// no-op (idempotent registration, ADR-0066).
    registered: HashSet<String>,
}

/// The fail-open resolution front (ADR-0066): the standing `config.json` source
/// (resolved once at build) plus the session-scoped skill sources (registered on
/// invocation), and the accumulated parse failures. Mirrors
/// [`crate::skills::SkillManager`]'s `(source, reason)` failure list. The derived
/// [`Default`] (no config source, no skills) is the empty shape a Session with no
/// hooks holds.
#[derive(Default, Clone)]
pub struct HookManager {
    /// The standing `config.json` source, resolved once at build (`None` when the
    /// config declared no `hooks` block - the common case, and the `Default`).
    config_source: Option<HookSource>,
    /// The `config.json` parse failures `(context, reason)`, surfaced as launch
    /// notices, the same fail-open report a skill/MCP failure takes.
    config_failures: Vec<(String, String)>,
    /// The session-scoped skill hooks, interior-mutable behind an `Arc` so the run
    /// layer can [`register_skill`](HookManager::register_skill) through the shared
    /// `Arc<HookManager>` (which cannot take `&mut self`).
    skills: Arc<Mutex<SkillHooks>>,
}

impl HookManager {
    /// Builds a manager from the standing `config.json` `hooks` value (ADR-0031's
    /// `FileConfig` carries it as a `serde_json::Value`). `None` when the config
    /// declares no `hooks` block - the common case. A malformed block is parsed
    /// fail-open: the good hooks load, the bad entries land on
    /// [`failures`](HookManager::failures).
    pub fn from_config(hooks: Option<&serde_json::Value>) -> HookManager {
        let mut config_failures: Vec<(String, String)> = Vec::new();
        let config_source = hooks.map(|value| HookSource {
            config: parse_hooks(value, "config.json", &mut config_failures),
            skill_root: None,
        });
        HookManager {
            config_source,
            config_failures,
            skills: Arc::new(Mutex::new(SkillHooks::default())),
        }
    }

    /// Registers a skill's session-scoped hooks (ADR-0066, Phase 4c): parsed from
    /// the skill's frontmatter `hooks:` value (already converted to a
    /// `serde_json::Value` by [`crate::hooks::config::hooks_value_from_yaml`]),
    /// tagged with the skill's `skill_root` so its command hooks get a
    /// `SUSPENDERS_SKILL_ROOT`. Called when the model invokes the skill (the run
    /// layer's tool-success seam), not at discovery time, so the hooks fire for the
    /// rest of the session. Idempotent: a second invocation of the same skill
    /// re-registers nothing. Takes `&self` (interior mutability) because the live
    /// manager is shared behind an `Arc`. A malformed block is fail-open, labeled
    /// `"skill <name>"` in any failure.
    pub fn register_skill(&self, name: &str, skill_root: &str, hooks: &serde_json::Value) {
        let Ok(mut store) = self.skills.lock() else {
            return;
        };
        // Idempotent: a skill invoked twice registers its hooks once.
        if !store.registered.insert(name.to_string()) {
            return;
        }
        let context = format!("skill {name}");
        // parse_hooks borrows the failures list mutably; take it out to satisfy the
        // borrow checker against the same `store`, then put it back.
        let mut failures = std::mem::take(&mut store.failures);
        let config = parse_hooks(hooks, &context, &mut failures);
        store.failures = failures;
        store.sources.push(HookSource {
            config,
            skill_root: Some(skill_root.to_string()),
        });
    }

    /// The hooks that should fire for `event`, in source order (standing before
    /// skill), each already filtered by its definition's matcher against
    /// `tool_name` (ADR-0066). `tool_name` is the tool being dispatched on a tool
    /// event; pass `None` on a non-tool event (where a matcher is inert and every
    /// hook fires). Each result carries (owned) its source's skill root for the
    /// runner's env. The standing source is read first, then the session-scoped
    /// skill sources under the lock.
    pub fn hooks_for(&self, event: HookEvent, tool_name: Option<&str>) -> Vec<SelectedHook> {
        let mut selected: Vec<SelectedHook> = Vec::new();
        if let Some(source) = &self.config_source {
            select_from(source, event, tool_name, &mut selected);
        }
        if let Ok(store) = self.skills.lock() {
            for source in &store.sources {
                select_from(source, event, tool_name, &mut selected);
            }
        }
        selected
    }

    /// The per-source parse failures `(context, reason)`: the `config.json` skips
    /// (read at launch) followed by any skill skips registered since. Surfaced as
    /// one launch notice each, the same fail-open report line a skill/MCP failure
    /// takes. Returns an owned list because the skill failures live behind the
    /// `Mutex`.
    pub fn failures(&self) -> Vec<(String, String)> {
        let mut all = self.config_failures.clone();
        if let Ok(store) = self.skills.lock() {
            all.extend(store.failures.iter().cloned());
        }
        all
    }
}

/// Selects the hooks a single [`HookSource`] contributes for `event`/`tool_name`,
/// appending an owned [`SelectedHook`] (carrying the source's skill root) per
/// matching hook. Shared by the standing and skill source scans so both filter by
/// the same matcher rule.
fn select_from(
    source: &HookSource,
    event: HookEvent,
    tool_name: Option<&str>,
    out: &mut Vec<SelectedHook>,
) {
    for def in source.config.definitions(event) {
        if !definition_matches(def, event, tool_name) {
            continue;
        }
        for hook in &def.hooks {
            out.push(SelectedHook {
                hook: hook.clone(),
                skill_root: source.skill_root.clone(),
            });
        }
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
