//! The authoring contract Suspenders tools share.
//!
//! A tool exposes a spec (Anthropic tool format; `input_schema` is a JSON
//! Schema map) and a run function that gets the decoded input plus the
//! [`ToolCtx`] carrying the Session's Project Root, the Result Cap (applied by
//! the Tools dispatch, not by the tools), and the command timeout. The ctx is
//! built by the Session; nothing in a tool reads the cwd or config.
//!
//! ## The authoring contract
//!
//! * **Input arrives validated.** [`validate`] runs against the tool's schema
//!   before every call: required fields present, string-typed fields are
//!   strings, unknown fields rejected.
//! * **Model-supplied paths go through [`with_path`]**, which confines them to
//!   the Project Root - or the trusted managed-auto-memory subtree the ctx
//!   carries (P5, ADR-0062).
//! * **Failed file operations are worded by [`file_error`]**, which formats the
//!   POSIX reason and appends closest-match suggestions on ENOENT.
//! * **Errors return, never raise.**
//! * **Size is not a tool concern** - `tools::shaping` cuts every result. A
//!   tool only DECLARES how its result cuts ([`CutPolicy`], like
//!   [`Tool::kind`]); the applier stays tool-name-free.

use std::path::PathBuf;

pub mod caps;
pub mod path;
pub mod read_cache;
pub mod registry;

/// The tool's wire spec, re-exported from the [`crate::content`] leaf where it
/// lives alongside the other wire tool-shapes. Kept re-exported here so the tool
/// authoring contract still reads as one home (`crate::tool::ToolSpec`) - the
/// type moved out only to break the `tool <-> llm` cycle the P2b SideQuery
/// capability (which names `Model`) would otherwise close (see ADR-0055).
pub use crate::content::ToolSpec;

use std::collections::HashMap;

use serde_json::Value;

use crate::content::{Modalities, ResultBlock};

/// A Tool Result: the content that enters the Conversation, whether it was an
/// error, and the display-side Artifacts. Mirrors baud's `Baud.Tools.result/0`.
/// Lives with the Tool authoring contract (it is what a Tool's `run` ultimately
/// becomes), so the Registry can dispatch to a result without depending on the
/// concrete tool set - which keeps the tool -> tool_registry -> tools module
/// graph acyclic.
///
/// `content` is a [`ResultBlock`] list (ADR-0059): the common case is a single
/// Text block, media reaches it only from a tool that overrides
/// [`Tool::run_rich`] (P3 3b's read_file). Text tools return `Result<String, _>`
/// through [`Tool::run`] and become one Text block.
///
/// `artifacts` is display-side data (CONTEXT.md: Artifact): a `HashMap<String,
/// Value>`, `{}` for the common case, populated only by a tool that shapes its
/// own transcript display (edit_file/write_file's `diff`, todo_write's `todos`,
/// run_shell_command's exit-code/timeout). It rides the `:tool_result` event to
/// the Transcript store and NEVER enters the Conversation - it is never shaped,
/// evicted, or sent on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub content: Vec<ResultBlock>,
    pub is_error: bool,
    pub artifacts: HashMap<String, Value>,
}

/// The rich output a Tool may produce (ADR-0059): an ordered block list, the
/// error flag, and the display-side Artifacts the tool attaches. A text tool's
/// `String` return becomes one non-error Text block with no Artifacts through
/// [`From<String>`]; a tool overriding [`Tool::run_rich`] yields more (P3 3b's
/// read_file's media blocks, or the display Artifacts the file-editing / shell
/// tools attach).
///
/// `is_error` lets a tool report a completed-but-failed run WHILE carrying
/// Artifacts: a nonzero-exit `run_shell_command` is not a tool failure (the tool
/// ran fine), so it returns `Ok(ToolOutput { is_error: true, .. })` with the
/// exit-code Artifact, rather than the `Err(String)` path (which the Registry
/// maps to an error result with no Artifacts). A genuine tool error - bad input,
/// spawn failure - still returns `Err`.
///
/// Named in `tool.rs` (not `content`) so the tool authoring contract carries its
/// own return shape; its blocks are the shared [`ResultBlock`] leaf, so no `tool
/// -> llm` edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub blocks: Vec<ResultBlock>,
    pub is_error: bool,
    pub artifacts: HashMap<String, Value>,
}

impl ToolOutput {
    /// A single non-error Text block output with no Artifacts - the common case.
    pub fn text(s: impl Into<String>) -> Self {
        ToolOutput {
            blocks: vec![ResultBlock::text(s)],
            is_error: false,
            artifacts: HashMap::new(),
        }
    }

    /// Flags this output as a completed-but-failed run (the model sees
    /// `is_error: true`) while keeping its blocks and Artifacts. Used by
    /// `run_shell_command` for a nonzero-exit / timed-out command.
    pub fn error(mut self, is_error: bool) -> Self {
        self.is_error = is_error;
        self
    }

    /// Attaches a display Artifact under `key` (CONTEXT.md: Artifact), threading
    /// through a builder. Used by the file-editing and shell tools to carry their
    /// diff / todos / exit-code display data to the Transcript store.
    pub fn with_artifact(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.artifacts.insert(key.into(), value.into());
        self
    }
}

// qual:allow(dry, boilerplate) reason: "not derivable - this wraps the String in
// a single ResultBlock::Text inside the `blocks` Vec, not a newtype passthrough;
// a derive macro would move the String verbatim into the field."
impl From<String> for ToolOutput {
    /// A text tool's `String` return becomes one non-error Text block with no
    /// Artifacts.
    fn from(s: String) -> Self {
        ToolOutput::text(s)
    }
}

/// How Shaping cuts this Tool's oversized Tool Result to the Result Cap - the
/// per-Tool declaration `tools::shaping` applies, mirroring [`Tool::kind`]: the
/// Tool states the fact about itself, the deep module folds it with zero
/// knowledge of tool names. The DEFAULT is [`CutPolicy::Head`] - keep the
/// start, append the truncated-output marker - which is right for every tool
/// whose output reads front-to-back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CutPolicy {
    /// Keep the first `cap` chars and append the truncated-output marker.
    #[default]
    Head,
    /// Keep the start AND the end, with an omitted-middle marker between: for a
    /// tool whose signal lives at the end (run_shell_command's exit code and
    /// last errors).
    HeadTail,
    /// Cut at a line boundary and name the file-absolute 0-based `offset` that
    /// continues the read (read_file): Shaping reads the Tool Call input's
    /// `offset` so the resume marker is file-absolute. A first line wider than
    /// the whole cap falls back to [`CutPolicy::Head`].
    HeadWithResume,
}

/// The authoring contract a Suspenders tool implements (baud's `Baud.Tool`
/// behaviour). `spec` is the Anthropic tool format; `run` gets the decoded
/// input (an open edge - a `serde_json::Value`) plus the [`ToolCtx`].
///
/// `run` is async so the object-safe registry can hold `Box<dyn Tool>` (via
/// `async-trait`); most tools implement a sync heuristic core and make `run` a
/// thin wrapper. Errors return (`Err`), never raise - `Tools::execute` maps an
/// `Err` to an `is_error` Tool Result.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    /// The kind of effect this tool has (qwen `Kind`, ADR-0067): the per-Tool
    /// self-description the plan-mode verdict fold ([`crate::approvals::classify`])
    /// reads to decide whether a Call mutates. The DEFAULT is
    /// [`Kind::Other`](crate::approvals::Kind::Other) - the fail-safe: an
    /// undeclared tool is treated as non-read-only, so plan mode BLOCKS it rather
    /// than risk slipping a mutation through (`Other` is not a mutator, but it is
    /// also not read-only). Every built-in tool declares its real Kind by
    /// matching qwen's corresponding tool file; the MCP adapter answers
    /// `Kind::Other` (its side effects are opaque to us).
    fn kind(&self) -> crate::approvals::Kind {
        crate::approvals::Kind::Other
    }

    /// How Shaping cuts this tool's oversized Tool Result ([`CutPolicy`]),
    /// declared the way [`kind`](Tool::kind) is: a per-Tool self-description the
    /// deep applier (`tools::shaping`) folds without knowing any tool's name.
    /// The default is [`CutPolicy::Head`]; only a tool whose output reads
    /// differently (run_shell_command's tail-heavy output, read_file's resumable
    /// line window) overrides it.
    fn cut_policy(&self) -> CutPolicy {
        CutPolicy::Head
    }

    async fn run(&self, input: &serde_json::Value, ctx: &ToolCtx) -> Result<String, String>;

    /// The rich variant of [`run`](Tool::run) (ADR-0059): the block-list output a
    /// tool produces. The default delegates to `run` and wraps the `String` in one
    /// Text block, so every text tool is unchanged. Only a tool that emits media -
    /// P3 3b's read_file, which reads an image or PDF and gates it on the Model's
    /// modalities via [`ToolCtx::input_modalities`] - overrides this. The default
    /// lives here so the Registry dispatches through one method.
    async fn run_rich(
        &self,
        input: &serde_json::Value,
        ctx: &ToolCtx,
    ) -> Result<ToolOutput, String> {
        self.run(input, ctx).await.map(ToolOutput::from)
    }

    /// When true, the tool is hidden from the wire list the model sees at the
    /// start of a Run - it is discovered on demand via `tool_search`, which
    /// reveals its full schema into the next request. qwen carries `shouldDefer`
    /// as a constructor field; Suspenders' tools are unit structs, so the flag
    /// rides a default-provided trait method instead (behaviourally identical).
    fn should_defer(&self) -> bool {
        false
    }

    /// When true, the tool stays in the wire list even where deferral is the
    /// default. Used for meta tools like `tool_search` itself. Mirrors qwen's
    /// `alwaysLoad`.
    fn always_load(&self) -> bool {
        false
    }

    /// Optional space-separated keywords `tool_search`'s keyword scoring folds
    /// in alongside the tool's name and description. Mirrors qwen's `searchHint`.
    fn search_hint(&self) -> Option<&str> {
        None
    }

    /// When true, the tool was discovered from an MCP server (F8, ADR-0056), so
    /// `tool_search` weighs it slightly higher - it is always deferred, and
    /// discovery is the only way the model reaches it. Every built-in tool
    /// answers false; only [`crate::mcp::adapter::McpTool`] overrides it.
    fn is_mcp(&self) -> bool {
        false
    }
}

/// The ctx every Tool Call executes with: the Session's Project Root, the
/// Result Cap, the command timeout, and the [`caps::Capabilities`] carrier - the
/// Run-scoped [`ToolRegistry`] plus the `dyn` effect seams a tool reaches its
/// host through (F1, ADR-0055). `tool_search` reaches through the registry to
/// reveal deferred tools; every other tool ignores it. The [`ToolRegistry`] is
/// not `Debug`-derivable, but [`caps::Capabilities`] hand-writes its own `Debug`,
/// so this stays on `#[derive(Debug)]`.
#[derive(Clone, Debug)]
pub struct ToolCtx {
    pub root: PathBuf,
    pub result_cap: usize,
    pub command_timeout_ms: u64,
    /// The input modalities the Run's captured Model accepts (ADR-0059): a copied
    /// fact, stamped at ctx-build like `result_cap`. read_file (P3 3b) reads it to
    /// decide whether an image/PDF rides as media or degrades to a text
    /// placeholder at read time; every other tool ignores it.
    pub input_modalities: Modalities,
    /// The trusted managed-auto-memory subtree (P5, ADR-0062): when `Some`, the
    /// shared path seam ([`path::resolve_path`]) confines a model-supplied path
    /// to the Project Root OR this subtree, so the memory-writing tools reach
    /// memory files uniformly. `None` (tests) keeps the Project-Root-only
    /// confinement; child Runs / subagents build their ctx through
    /// `Session::tool_ctx`, which stamps `Some(...)`, so they inherit the parent
    /// Session's memory root. A resolved subtree, never a general escape.
    pub memory_root: Option<PathBuf>,
    /// The Session's session directory (Phase 9, ADR-0063): where background-shell
    /// capture files live (`<session_dir>/background-shells/<id>.output`). Stamped
    /// at ctx-build like `root` so `run_command`'s background branch can name the
    /// capture file in its "Background shell started." block. A copied Session
    /// fact, not an effect.
    pub session_dir: PathBuf,
    pub caps: caps::Capabilities,
}

impl ToolCtx {
    /// The Run-scoped Tool Registry, read through the carrier. The three
    /// registry-reading tools (`tool_search`, the Tools dispatch, the Loop's wire
    /// list) go through this accessor rather than the carrier field, so the field
    /// path stays an internal detail of the carrier.
    pub fn registry(&self) -> &std::sync::Arc<crate::tool_registry::ToolRegistry> {
        &self.caps.registry
    }

    /// The Run-scoped file-read cache, read through the carrier (F6, ADR-0060).
    /// read_file records a successful read into it; notebook_edit checks it for
    /// a prior FULL read before mutating a notebook. Every other tool ignores it,
    /// like the registry.
    pub fn read_cache(&self) -> &std::sync::Arc<read_cache::FileReadCache> {
        &self.caps.read_cache
    }
}

/// Validates the model-supplied input against a tool's JSON Schema. Returns
/// `Ok(())` or `Err(reason)` with a precise description (unknown fields,
/// missing required fields, wrong types).
pub fn validate(
    input_schema: &serde_json::Value,
    input: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    let required: Vec<String> = input_schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let empty = serde_json::Map::new();
    let properties = input_schema
        .get("properties")
        .and_then(|v| v.as_object())
        .unwrap_or(&empty);

    let known: Vec<&String> = properties.keys().collect();

    // 1. Unknown fields first.
    let unknown: Vec<&String> = input.keys().filter(|k| !known.contains(k)).collect();
    if !unknown.is_empty() {
        return Err(format!(
            "unknown field(s): {}. Valid fields: {}",
            quote_join(unknown.iter().map(|s| s.as_str())),
            quote_join(known.iter().map(|s| s.as_str())),
        ));
    }

    // 2. Missing required fields.
    let missing: Vec<&String> = required
        .iter()
        .filter(|r| !input.contains_key(*r))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "missing required field(s): {}. Required: {}",
            quote_join(missing.iter().map(|s| s.as_str())),
            quote_join(required.iter().map(|s| s.as_str())),
        ));
    }

    // 3. Type mismatches (string fields that aren't strings).
    let type_errors: Vec<String> = properties
        .iter()
        .filter(|(name, prop)| {
            input.contains_key(*name)
                && prop.get("type").and_then(|t| t.as_str()) == Some("string")
                && !input.get(*name).map(|v| v.is_string()).unwrap_or(false)
        })
        .filter_map(|(name, _)| {
            // `contains_key` above guarantees `get` returns `Some`; use
            // `filter_map` to propagate that without `unwrap`.
            let val = input.get(name)?;
            Some(format!(
                "field {:?} should be a string, got: {} ({})",
                name,
                inspect(val),
                type_name(val)
            ))
        })
        .collect();

    if type_errors.is_empty() {
        Ok(())
    } else {
        Err(type_errors.join("; "))
    }
}

/// Read an OPTIONAL string parameter from a tool's decoded input.
///
/// The same three-way classification recurs across the read-only tools' `run`
/// (glob's `path`, grep's `path` and `glob`): a missing key or JSON `null` is
/// `None`, a string is `Some`, and any other JSON type is a caller error worded
/// `"{field} must be a string"`. Hoisted here so that data-clump lives in one
/// place instead of being rewritten per param.
pub fn optional_str<'a>(
    input: &'a serde_json::Value,
    field: &str,
) -> Result<Option<&'a str>, String> {
    match input.get(field) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(format!("{field} must be a string")),
    }
}

fn quote_join<'a, I: Iterator<Item = &'a str>>(items: I) -> String {
    items
        .map(|s| format!("{s:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn type_name(value: &serde_json::Value) -> &'static str {
    use serde_json::Value;
    match value {
        Value::String(_) => "string",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "float",
        Value::Bool(_) => "boolean",
        Value::Object(_) => "map",
        Value::Array(_) => "list",
        Value::Null => "atom",
    }
}

fn inspect(value: &serde_json::Value) -> String {
    value.to_string()
}

#[cfg(test)]
#[path = "../tests/tool.rs"]
mod tests;
