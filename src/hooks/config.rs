//! The hook config model and its parser (ADR-0066), a pure LEAF.
//!
//! A hook is CONFIGURED, never coded (ADR-0066). It is declared under a `hooks`
//! key in one of two places, both of which parse into the SAME shape here:
//!
//! - `config.json`, as a `serde_json::Value` (ADR-0031's `FileConfig` carries it
//!   verbatim), the standing hooks a user always wants.
//! - a `SKILL.md` frontmatter `hooks:` block, as YAML (ADR-0058 parses-and-ignores
//!   it today; this ADR makes it live). The flat skill fields stay hand-rolled
//!   (`crate::skills`); only this nested block parses via `serde_yaml_ng`.
//!
//! Both sources deserialize to a `serde_json::Value`, so ONE parser
//! ([`parse_hooks`]) serves both (a `SKILL.md`'s YAML block is converted to the
//! same `Value` shape first). The on-disk shape is faithful to qwen's settings
//! shape (`hooks/types.ts` `HookDefinition` + the `HookConfig` union): each `hooks`
//! key maps an event name to an array of `{ matcher?, hooks: [ { type, <field>,
//! timeout? } ] }` definitions.
//!
//! Fail-open (ADR-0066, mirroring `crate::skills` and `crate::mcp`): a malformed
//! event name, definition, or hook entry is SKIPPED and recorded as a
//! `(context, reason)` failure, never fatal. A single bad hook does not drop the
//! rest of the block.

use serde_json::Value;

use crate::hooks::event::HookEvent;

/// The three hook implementation types (ADR-0066), ported from qwen's `HookType`
/// MINUS the rejected `function` type (an in-process SDK callback with no host in
/// a compiled Rust CLI, ADR-0066). Each type names the one config field it needs:
/// `command` / `url` / `prompt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookKind {
    /// Shells out: the JSON event payload is delivered on stdin, and stdout is
    /// parsed back as the hook's JSON decision.
    Command {
        /// The shell command to run.
        command: String,
    },
    /// POSTs the JSON event payload to `url` and reads the response body as the
    /// hook's JSON decision.
    Http {
        /// The endpoint the event payload is POSTed to.
        url: String,
    },
    /// Evaluates via the LLM: the payload is spliced into `prompt` (at the
    /// `$ARGUMENTS` placeholder, qwen parity) and sent as a single-turn
    /// completion; the reply is read back as the hook's decision.
    Prompt {
        /// The prompt template, with `$ARGUMENTS` replaced by the payload JSON.
        prompt: String,
    },
}

/// One resolved hook: its [`HookKind`] (carrying the type-specific field) and an
/// optional per-hook `timeout` in SECONDS (qwen's unit; the runner converts to
/// ms). Mirrors one element of qwen's `HookDefinition.hooks[]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hook {
    /// How the hook runs plus its type-specific field.
    pub kind: HookKind,
    /// The per-hook timeout in seconds, or `None` to take the runner's default.
    pub timeout_secs: Option<u64>,
}

/// One hook definition (qwen's `HookDefinition`): an optional tool-name `matcher`
/// scoping the definition to matching tools on a tool event (inert on other
/// events, ADR-0066), and the `hooks` it fires. Grouped under an event by
/// [`parse_hooks`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookDefinition {
    /// The tool-name matcher (a regex, qwen parity; falls back to an exact match
    /// on an invalid pattern). Absent or empty matches all tools.
    pub matcher: Option<String>,
    /// The hooks this definition fires when its event fires and its matcher hits.
    pub hooks: Vec<Hook>,
}

/// The parsed hook config: for each [`HookEvent`], the definitions declared under
/// it (in source order). The output shape of [`parse_hooks`], grouped by event so
/// the manager can index `hooks_for(event, tool_name)` directly. Empty when the
/// source had no `hooks` block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookConfig {
    /// Event -> its definitions, in declaration order. An event with no hooks is
    /// simply absent from the map.
    pub by_event: Vec<(HookEvent, Vec<HookDefinition>)>,
}

impl HookConfig {
    /// The definitions declared for `event`, or an empty slice. Read by the
    /// manager to filter by matcher.
    pub fn definitions(&self, event: HookEvent) -> &[HookDefinition] {
        self.by_event
            .iter()
            .find(|(e, _)| *e == event)
            .map(|(_, defs)| defs.as_slice())
            .unwrap_or(&[])
    }
}

/// qwen's `HOOKS_CONFIG_FIELDS` (`hooks/types.ts`): keys under `hooks` that are
/// NOT event names and must be skipped without a failure. Parsing these as events
/// would spuriously fail; qwen filters them, so we do too.
const NON_EVENT_FIELDS: [&str; 3] = ["enabled", "disabled", "notifications"];

/// Parses a `hooks` block (from `config.json` or a converted `SKILL.md` YAML
/// block) into a [`HookConfig`], fail-open. `context` labels the source in any
/// failure line (e.g. `"config.json"` or `"skill <name>"`), mirroring the
/// `(context, reason)` failure convention of [`crate::skills`] and
/// [`crate::mcp`].
///
/// The `hooks` value must be a JSON object mapping event names to arrays of
/// definitions (qwen's settings shape). A non-object `hooks` value, an unknown
/// event name, a non-array definition list, a malformed definition, and a
/// malformed hook entry are each SKIPPED and pushed onto `failures`; the rest of
/// the block parses. qwen's non-event `hooks` fields
/// (`enabled`/`disabled`/`notifications`) are skipped silently.
pub fn parse_hooks(
    hooks: &Value,
    context: &str,
    failures: &mut Vec<(String, String)>,
) -> HookConfig {
    let Some(map) = hooks.as_object() else {
        failures.push((
            context.to_string(),
            "`hooks` is not an object; skipping the whole block".to_string(),
        ));
        return HookConfig::default();
    };

    let mut by_event: Vec<(HookEvent, Vec<HookDefinition>)> = Vec::new();

    for (key, value) in map {
        // qwen's HOOKS_CONFIG_FIELDS: not events, skipped without a failure.
        if NON_EVENT_FIELDS.contains(&key.as_str()) {
            continue;
        }

        let Some(event) = HookEvent::parse(key) else {
            failures.push((
                context.to_string(),
                format!("unknown hook event name \"{key}\"; skipping"),
            ));
            continue;
        };

        let Some(defs_array) = value.as_array() else {
            failures.push((
                context.to_string(),
                format!("hook definitions for \"{key}\" are not an array; skipping"),
            ));
            continue;
        };

        let mut defs: Vec<HookDefinition> = Vec::new();
        for (i, def_value) in defs_array.iter().enumerate() {
            match parse_definition(def_value, key, i, context, failures) {
                Some(def) => defs.push(def),
                None => continue,
            }
        }

        if !defs.is_empty() {
            by_event.push((event, defs));
        }
    }

    HookConfig { by_event }
}

/// Parses one definition object (`{ matcher?, hooks: [...] }`), fail-open. A
/// non-object definition, an absent/non-array `hooks` list, or a definition whose
/// every hook entry is malformed yields `None` (skipped with a failure). Each
/// individual malformed hook entry is skipped-with-failure but does not sink the
/// whole definition (qwen keeps the surviving hooks of a definition).
fn parse_definition(
    value: &Value,
    event_key: &str,
    index: usize,
    context: &str,
    failures: &mut Vec<(String, String)>,
) -> Option<HookDefinition> {
    let Some(obj) = value.as_object() else {
        failures.push((
            context.to_string(),
            format!("hook definition {index} for \"{event_key}\" is not an object; skipping"),
        ));
        return None;
    };

    // matcher is optional; a non-string matcher is a soft failure (dropped to
    // None) rather than sinking the definition, matching the fail-open ethos.
    let matcher = match obj.get("matcher") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => {
            failures.push((
                context.to_string(),
                format!(
                    "matcher on definition {index} for \"{event_key}\" is not a string; ignoring it"
                ),
            ));
            None
        }
    };

    let Some(hooks_array) = obj.get("hooks").and_then(Value::as_array) else {
        failures.push((
            context.to_string(),
            format!("hook definition {index} for \"{event_key}\" has no `hooks` array; skipping"),
        ));
        return None;
    };

    let mut hooks: Vec<Hook> = Vec::new();
    for (j, hook_value) in hooks_array.iter().enumerate() {
        match parse_hook(hook_value) {
            Ok(hook) => hooks.push(hook),
            Err(reason) => failures.push((
                context.to_string(),
                format!("hook {j} in definition {index} for \"{event_key}\": {reason}; skipping"),
            )),
        }
    }

    // A definition whose every hook was malformed contributes nothing.
    if hooks.is_empty() {
        return None;
    }

    Some(HookDefinition { matcher, hooks })
}

/// Parses one hook entry (`{ type, <command|url|prompt>, timeout? }`) into a
/// [`Hook`], or an `Err(reason)` naming what was wrong (the caller turns it into a
/// skip-with-failure). The `type` selects which field is required: `command`
/// needs `command`, `http` needs `url`, `prompt` needs `prompt`. An absent/
/// non-string `type`, an unknown type (including the rejected `function`, ADR-0066),
/// and a missing/empty required field are all errors. `timeout` is an optional
/// number of seconds; a present-but-non-numeric timeout is an error.
fn parse_hook(value: &Value) -> Result<Hook, String> {
    let obj = value.as_object().ok_or("hook entry is not an object")?;

    let type_str = obj
        .get("type")
        .and_then(Value::as_str)
        .ok_or("missing or non-string `type`")?;

    let timeout_secs = match obj.get("timeout") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .ok_or("`timeout` is not a non-negative number of seconds")?,
        ),
    };

    let kind = match type_str {
        "command" => HookKind::Command {
            command: required_str(obj, "command")?,
        },
        "http" => HookKind::Http {
            url: required_str(obj, "url")?,
        },
        "prompt" => HookKind::Prompt {
            prompt: required_str(obj, "prompt")?,
        },
        // The function type is rejected (ADR-0066): an in-process SDK callback has
        // no host in a compiled binary. Any other type is unknown.
        "function" => return Err("`function` hook type is not supported".to_string()),
        other => return Err(format!("unknown hook type \"{other}\"")),
    };

    Ok(Hook { kind, timeout_secs })
}

/// Reads a required non-empty string field from a hook entry, or an `Err` naming
/// it. A present-but-empty value is treated as missing (a `command: ""` cannot
/// run), matching the skill parser's present-but-empty-is-missing rule.
fn required_str(obj: &serde_json::Map<String, Value>, field: &str) -> Result<String, String> {
    match obj.get(field).and_then(Value::as_str) {
        Some(s) if !s.is_empty() => Ok(s.to_string()),
        _ => Err(format!("missing or empty `{field}`")),
    }
}

/// Converts a `SKILL.md` frontmatter `hooks:` YAML block into the SAME
/// `serde_json::Value` shape [`parse_hooks`] consumes for `config.json`, so one
/// parser serves both sources (ADR-0066). The `yaml` text is the raw nested block
/// under the `hooks:` key. A YAML that will not parse is a fail-open skip: the
/// caller records the failure and drops the skill's hooks, never crashing the
/// skill load (mirrors [`crate::skills`]'s parse-and-ignore of the flat fields).
pub fn hooks_value_from_yaml(yaml: &str) -> Result<Value, String> {
    // serde_yaml_ng deserializes directly into serde_json::Value, so a skill's
    // YAML hooks block lands in the exact shape config.json's JSON hooks value has.
    serde_yaml_ng::from_str::<Value>(yaml)
        .map_err(|e| format!("invalid YAML in `hooks:` block: {e}"))
}

#[cfg(test)]
#[path = "../../tests/hooks/config.rs"]
mod tests;
