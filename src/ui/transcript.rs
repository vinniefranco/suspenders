//! UI Transcript - the display-side history of a Session (CONTEXT.md), as a
//! store (ADR-0034): the settled items, the revision counter the render cache
//! keys on, and the in-flight [`Streaming`] snapshot (a private child module).
//! The [`crate::ui::screen`] fold delegates one verb per event arm; no caller
//! can reach the items Vec directly.
//!
//! Pure like the rest of the core (ADR-0001/0019): no terminal, no async, no
//! IO, no ratatui types.
//!
//! ## Tool Result display swaps (CONTEXT.md: Presentment)
//!
//! A tool that shapes its own transcript display attaches a display Artifact to
//! its Tool Result (CONTEXT.md: Artifact), which rides the `:tool_result` event
//! here. [`Transcript::tool_result`] reads those Artifacts and swaps the
//! one-line summary for a first-class item: a `diff` Artifact (from edit_file /
//! write_file) becomes a [`TranscriptItem::Diff`], a `todos` Artifact (from
//! todo_write) becomes a [`TranscriptItem::Todo`], and run_shell_command's
//! exit-code / timeout Artifact rewrites the summary to the `✓ exit 0` / `✗ exit
//! N` / `✗ timed out` badge. No Artifact (or any other tool) keeps the plain
//! summary. The swap is a pure read over the Artifacts - no IO.
//!
//! ## The invariants, held at this seam
//!
//! * **The RenderCache contract**: between two reads, an unchanged
//!   [`Transcript::revision`] means the earlier [`Transcript::items`] is a
//!   PREFIX of the later - appends never bump, and the only two non-append
//!   edits (a Tool Result removing its paired call, a delivered Steering
//!   removing its pending marker) always bump. Both route through the one
//!   private supersede funnel, so no verb can remove without bumping.
//! * **Pairing is by `id`**, never by position - parallel Tool Calls
//!   interleave.
//! * A paired result's `key_arg` is stamped as the successor is built, so the
//!   merged one-line result carries the call's salient arg.
//! * The pending-Steering marker is authored HERE, by both
//!   [`Transcript::steering_queued`] and [`Transcript::steering_delivered`],
//!   so the delivered removal-by-equality can never desync from the queued
//!   text.
//! * **A new public verb enrolls in the property test**: the prefix-or-bump
//!   test's verb list is MANUAL (Rust cannot reflect over methods), so a new
//!   verb must be added to it and its expected-bumps guard revisited - the
//!   test cannot notice a verb it was never told about.
//!
//! Voice strings stay with the Screen (the startup Header, stop reasons, launch
//! notices - recorded through [`Transcript::header`]/[`Transcript::info`]);
//! the store authors only the two lines its own invariants require verbatim: the
//! pending Steering marker and the tooling-failure line.

mod streaming;
mod thought;

use std::collections::HashMap;

use serde_json::Value;

use crate::content::ContentBlock;
use crate::tools::{file_diff, run_command, todo_write};
use crate::view_model::{Tone, TranscriptItem};
use streaming::Streaming;

/// The settled item a successor replaces - the ONE way anything leaves the
/// history. Both structural edits route through [`Transcript::supersede`], so
/// the revision rule (remove ⇒ bump) has exactly one body. Private on purpose:
/// the public interface is the named verbs; promote only when a third
/// pair-merge rule exists.
enum Locator<'a> {
    /// The pending [`TranscriptItem::ToolCall`] with this id, NEWEST match:
    /// parallel calls interleave, and the latest call with the id is the live
    /// one.
    ToolCall { id: &'a str },
    /// The [`TranscriptItem::Marker`] with exactly this text, OLDEST match:
    /// the first queued Steering marker is the first delivered. Tone is not in
    /// the key - both queued and delivered fix it to [`Tone::Steering`], and
    /// [`pending_steering_line`] is the single author of the text - so text
    /// equality alone locates the pending marker.
    Marker { text: &'a str },
}

/// The Transcript (CONTEXT.md): the display-side history of a Session, oldest
/// first. Owns the settled items, the revision counter, and the in-flight
/// [`Streaming`] snapshot - see the module doc for the invariants this seam
/// holds.
pub struct Transcript {
    /// The settled items. Private: every write routes through the append
    /// funnel or the supersede funnel (the revision rule).
    items: Vec<TranscriptItem>,
    /// Bumped by the two structural edits, NEVER by an append. The render
    /// cache extends incrementally while this holds still and rebuilds when it
    /// moves - appends are the hot path, structural edits the rare one.
    revision: u64,
    /// The in-flight streaming snapshot and its materialize rules (the private
    /// `streaming` child module owns the end/flush asymmetry).
    streaming: Streaming,
}

impl Default for Transcript {
    fn default() -> Self {
        Transcript::new()
    }
}

impl Transcript {
    /// An empty Transcript. The caller authors any opening line (the startup
    /// Header is the Screen's Voice, recorded through [`Transcript::header`]).
    pub fn new() -> Self {
        Transcript {
            items: Vec::new(),
            revision: 0,
            streaming: Streaming::idle(),
        }
    }

    // ---- Streaming lifecycle -----------------------------------------------

    /// A message began: an empty snapshot, ready for the first update.
    pub fn message_start(&mut self) {
        self.streaming.start();
    }

    /// Stateless streaming (ADR-0001): the snapshot replaces the in-flight
    /// view wholesale - no delta accumulation.
    pub fn message_update(&mut self, content: Vec<ContentBlock>) {
        self.streaming.update(content);
    }

    /// Settle a finished message into discrete items: Thinking from the last
    /// snapshot (the final content never repeats it), text from
    /// `final_content` - Thinking first, empties skipped. Appends only, so the
    /// revision holds still.
    pub fn message_end(&mut self, final_content: &[ContentBlock]) {
        for item in self.streaming.end(final_content) {
            self.append(item);
        }
    }

    /// Run-boundary reset: discard the snapshot without settling it. The
    /// settled items are untouched.
    pub fn discard_streaming(&mut self) {
        self.streaming.clear();
    }

    /// Close out a Run: settle whatever the live snapshot holds (both
    /// Thinking and text - a cancel/crash mid-stream has no final content),
    /// THEN record `note` as an info line if there is one. The order is the
    /// point - the closing note always lands after the salvaged content. The
    /// caller authors the note (stop reasons and cancellation lines are the
    /// Screen's Voice); a clean close passes `None`. Idempotent when idle:
    /// nothing flushes twice.
    pub fn close(&mut self, note: Option<String>) {
        for item in self.streaming.flush() {
            self.append(item);
        }
        if let Some(text) = note {
            self.info(text);
        }
    }

    // ---- Settled writes ----------------------------------------------------

    /// The generic append: push, NEVER bumps the revision. The public generic
    /// verb - like every named verb it routes through the one private append
    /// funnel - so a new [`TranscriptItem`] kind needs a variant and a render
    /// arm, never a new store method.
    pub fn push(&mut self, item: TranscriptItem) {
        self.append(item);
    }

    /// Appends a user prompt (a submit, or Steering promoted on delivery).
    pub fn user(&mut self, text: impl Into<String>) {
        self.push(TranscriptItem::User { text: text.into() });
    }

    /// Appends an info line. The caller authors the text (Voice stays with the
    /// Screen); the store only records it.
    pub fn info(&mut self, text: impl Into<String>) {
        self.push(TranscriptItem::Info { text: text.into() });
    }

    /// Appends the startup [`TranscriptItem::Header`] banner (qwen `AppHeader`):
    /// the brand title + version, the scoped Model id, the working directory, and
    /// the deterministically-picked startup tip. The caller authors the facts
    /// (they are the Screen's Voice, drawn from the launch Model + cwd); the store
    /// only records them. An APPEND - never bumps the revision.
    pub fn header(
        &mut self,
        title: impl Into<String>,
        version: impl Into<String>,
        model: impl Into<String>,
        cwd: impl Into<String>,
        tip: impl Into<String>,
    ) {
        self.push(TranscriptItem::Header {
            title: title.into(),
            version: version.into(),
            model: model.into(),
            cwd: cwd.into(),
            tip: tip.into(),
        });
    }

    /// Appends a harness marker: the caller
    /// authors both the text (glyph included) and the [`Tone`] at the firing
    /// site; the store only records the pair. An APPEND - never bumps the
    /// revision. The Steering pending marker takes the same path through
    /// [`Transcript::steering_queued`], which fixes the tone to
    /// [`Tone::Steering`].
    pub fn marker(&mut self, text: impl Into<String>, tone: Tone) {
        self.push(TranscriptItem::Marker {
            text: text.into(),
            tone,
        });
    }

    /// Presents a Tool Call: stamps the `id` (for later result-pairing) and
    /// gives the live in-flight line a clean summary - the salient key arg
    /// (path/command/pattern by tool name), falling back to the raw
    /// `key=value` summary only when no arg stands out. Appends; never bumps.
    pub fn tool_call(&mut self, id: String, name: String, input: &Value) {
        let summary = call_summary(&name, input);
        self.append(TranscriptItem::ToolCall { id, name, summary });
    }

    /// Merges a Tool Result with its call into ONE line: the pending
    /// [`TranscriptItem::ToolCall`] is found by `id` (NEVER by position -
    /// parallel tool calls interleave), its summary is recovered as the
    /// `key_arg` (it already IS the salient arg - [`key_arg`] never yields an
    /// empty string; the render layer normalizes any empty value once), and the
    /// redundant call line is removed (a structural edit - the revision bumps).
    /// An unpaired result (a Voice answer to an orphaned call, no live call)
    /// removes nothing, does not bump, and carries no `key_arg` - a defined
    /// case, not an error.
    ///
    /// The tool's display Artifacts (ADR-0007) swap the one-line summary for a
    /// first-class item: a `diff` for edit_file / write_file, a `todos` list for
    /// todo_write, or the exit-code / timeout badge for run_shell_command. No
    /// Artifact keeps the plain summary. See [`swap_for_display`].
    pub fn tool_result(
        &mut self,
        id: &str,
        name: String,
        content: &str,
        is_error: bool,
        artifacts: &HashMap<String, Value>,
    ) {
        let summary = summarize_result(content);
        self.supersede(Locator::ToolCall { id }, |call| {
            let key_arg = call.map(|item| match item {
                TranscriptItem::ToolCall { summary, .. } => summary,
                other => unreachable!("Locator::ToolCall matched {other:?}"),
            });
            let base = TranscriptItem::ToolResult {
                name,
                summary,
                is_error,
                key_arg,
            };
            swap_for_display(base, artifacts)
        });
    }

    /// Appends the pending-Steering marker (a [`Tone::Steering`]
    /// marker, the user's own voice). Its text is authored HERE
    /// so [`Transcript::steering_delivered`]'s removal-by-equality can never
    /// desync from it.
    pub fn steering_queued(&mut self, text: &str) {
        self.marker(pending_steering_line(text), Tone::Steering);
    }

    /// Promotes delivered Steering to a user line (the text is now in the
    /// Conversation): removes the pending marker if present (a structural
    /// edit - the revision bumps), then appends the User item. A delivery
    /// whose marker was never queued removes nothing and does not bump. The
    /// removal anchors on the [`TranscriptItem::Marker`] by text
    /// ([`Locator::Marker`]), matching what [`Transcript::steering_queued`]
    /// appended.
    pub fn steering_delivered(&mut self, text: impl Into<String>) {
        let text = text.into();
        let marker = pending_steering_line(&text);
        self.supersede(Locator::Marker { text: &marker }, |_| {
            TranscriptItem::User { text }
        });
    }

    /// Records a fail-open report line from the Run's `fail_open_report`
    /// event: Hook failures and decisions (ADR-0066), MCP connect failures
    /// (ADR-0056), broken-Skill notices (ADR-0058), and the plan-mode block
    /// (ADR-0067), all under the fail-open-with-visibility principle
    /// (ADR-0018). A plain append; it is not a Tool Result and carries no
    /// display swap.
    pub fn fail_open_report(&mut self, source: &str, message: &str) {
        self.items.push(TranscriptItem::Info {
            text: fail_open_line(source, message),
        });
    }

    // ---- Reads (exactly what the render draws and caches) -------------------

    /// The settled items, oldest first.
    pub fn items(&self) -> &[TranscriptItem] {
        &self.items
    }

    /// The structural-edit counter - the render cache's key. The contract:
    /// between two reads, an unchanged revision means the earlier
    /// [`Transcript::items`] is a prefix of the later (appends only), so a
    /// per-item cache extends incrementally; a moved revision means settled
    /// items shifted and the cache rebuilds.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The in-flight assistant text, from the latest streaming snapshot.
    pub fn streaming_text(&self) -> String {
        self.streaming.text()
    }

    /// The latest task list on screen (ADR-0048): the items of the newest
    /// [`TranscriptItem::Todo`] in the history, or `&[]` when none has landed.
    /// The sticky "Current tasks" box DERIVES from this (qwen
    /// `findLatestTodoSnapshot`) rather than a parallel Agent→view Plan channel -
    /// the Todo item IS the single source of truth the committed render and the
    /// sticky box both read, so the two can never disagree. Also returns the
    /// item's index (its position in [`Transcript::items`]) so the caller can
    /// gate the sticky box against the high-water mark (show only once the inline
    /// copy has committed, avoiding a double-render). `None` when no Todo exists.
    pub fn latest_todo(&self) -> Option<(usize, &[crate::plan::TodoItem])> {
        self.items
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, item)| match item {
                TranscriptItem::Todo { items } => Some((i, items.as_slice())),
                _ => None,
            })
    }

    /// The in-flight Thinking text, from the latest streaming snapshot.
    pub fn streaming_thinking(&self) -> String {
        self.streaming.thinking()
    }

    /// The rolling thought SUBJECT for the running spinner (qwen
    /// `LoadingIndicator.tsx:72` `thought?.subject || currentLoadingPhrase`): the
    /// short head of the live reasoning the spinner shows in place of the lull
    /// phrase. A pure read over the streaming snapshot, SPINNER-ONLY - the
    /// committed history keeps the raw Thinking text verbatim.
    ///
    /// Three fallbacks, in order (the divergence recorded in ADR-0046): (1) the
    /// bold subject qwen's `parseThought` parses from the FIRST `**…**`
    /// pair, when the reasoning emits one; else (2) the last non-empty line of the
    /// streaming reasoning (the live head - suspenders' reasoning streams do NOT
    /// reliably emit `**bold**` subjects); else (3) `None`, so the spinner falls
    /// back to the lull phrase.
    ///
    /// Clear-timing is FREE: [`streaming_thinking`](Self::streaming_thinking)
    /// empties between messages (subject → `None` automatically), and the spinner
    /// only renders while the Run is Running, so it vanishes at Idle with no
    /// manual reset. The parse itself lives in the `thought` child module (a
    /// pure text concern, split from the store's history-invariant duty).
    pub fn thought_subject(&self) -> Option<String> {
        thought::thought_subject_of(&self.streaming.thinking())
    }

    /// The spinner's thought subject RESTRICTED to a DISTINCT bold `**subject**`
    /// (no last-line head fallback). The pending body uses this when the live
    /// `✦ Thinking` tail is on screen (non-compact): the tail already shows the
    /// reasoning head, so [`thought_subject`](Self::thought_subject)'s head
    /// fallback would duplicate it onto the spinner line. `None` -> the spinner
    /// shows the lull phrase instead of echoing the tail.
    pub fn thought_subject_bold(&self) -> Option<String> {
        thought::bold_subject_of(&self.streaming.thinking())
    }

    // ---- Internals ----------------------------------------------------------

    // The one append funnel: push, never bump. A tool's display swap already
    // happened in the verb that built the item (Tool Result -> Diff/Todo/badge
    // from its Artifacts), so this is a plain push.
    fn append(&mut self, item: TranscriptItem) {
        self.items.push(item);
    }

    // The one supersede funnel: remove-maybe, bump-iff-removed, build the
    // successor FROM the removed anchor, append. Every structural edit routes
    // through here, so a verb cannot remove without bumping - the RenderCache
    // contract is held by construction, not by discipline.
    fn supersede(
        &mut self,
        locator: Locator<'_>,
        successor: impl FnOnce(Option<TranscriptItem>) -> TranscriptItem,
    ) {
        let removed = self.locate(&locator).map(|pos| {
            let item = self.items.remove(pos);
            // A non-append edit: settled items shifted, so any cached
            // per-item render state upstream is stale.
            self.revision += 1;
            item
        });
        self.append(successor(removed));
    }

    fn locate(&self, locator: &Locator<'_>) -> Option<usize> {
        match locator {
            Locator::ToolCall { id } => self.items.iter().rposition(
                |m| matches!(m, TranscriptItem::ToolCall { id: call_id, .. } if call_id == id),
            ),
            Locator::Marker { text } => self
                .items
                .iter()
                .position(|m| matches!(m, TranscriptItem::Marker { text: t, .. } if t == text)),
        }
    }
}

// ---------------------------------------------------------------------------
// Free functions (pure helpers).
// ---------------------------------------------------------------------------

// The pending-Steering marker. Private and shared by `steering_queued` and
// `steering_delivered`, so the two can never disagree about the text the
// delivery removes.
fn pending_steering_line(text: &str) -> String {
    format!("↳ queued: {text}")
}

// The fail-open report line (fail-open-with-visibility, ADR-0018) - sourced
// once, so every `fail_open_report` event reads identically. The source label
// names its own subsystem ("hook Stop", "mcp server foo", "skill bar", "plan
// mode") and the message narrates what happened, so the line adds no noun or
// verb of its own (the retired "failed in <stage>" phrasing named the retired
// extension-pipeline stages, and read wrong for a Hook Decision).
fn fail_open_line(source: &str, message: &str) -> String {
    format!("{source}: {message}")
}

// Swaps a Tool Result item for its first-class display when the tool attached a
// display Artifact (ADR-0007), else returns it unchanged. Each tool owns a
// distinct Artifact key and tool name, so the arms are mutually exclusive:
//
// - edit_file / write_file attach a `diff` Artifact -> a [`TranscriptItem::Diff`]
//   (only on a successful result; a failed edit attaches none).
// - todo_write attaches a `todos` Artifact -> a [`TranscriptItem::Todo`].
// - run_shell_command attaches an exit-code / timeout Artifact -> the summary is
//   rewritten to the `✓ exit 0` / `✗ exit N` / `✗ timed out` badge (in place;
//   every other field of the item is kept).
//
// A successful result with no Artifact (a malformed-but-schema-passing todo_write,
// an edit with no textual change) keeps its plain summary.
fn swap_for_display(item: TranscriptItem, artifacts: &HashMap<String, Value>) -> TranscriptItem {
    let TranscriptItem::ToolResult {
        name,
        mut summary,
        is_error,
        key_arg,
    } = item
    else {
        return item;
    };

    // The exit badge applies even to a failed (nonzero-exit / timed-out) run, so
    // it is checked before the is_error gate the diff / todo swaps use. It only
    // rewrites the summary, so it falls through to the single rebuild below.
    if name == run_command::SHELL {
        if let Some(badge) = run_command_badge(artifacts) {
            summary = badge;
        }
    } else if !is_error {
        // The diff / todo swaps produce a whole new item, and fire only on a
        // successful result.
        if file_diff::EDIT_TOOLS.contains(&name.as_str())
            && let Some(diff) = file_diff::read_artifact(artifacts)
        {
            return file_diff::to_diff_item(&name, &diff);
        }
        if name == todo_write::TOOL
            && let Some(todos) = todo_write::read_todos_artifact(artifacts)
        {
            return TranscriptItem::Todo { items: todos.items };
        }
    }

    TranscriptItem::ToolResult {
        name,
        summary,
        is_error,
        key_arg,
    }
}

// The exit-code / timeout badge for a run_shell_command result's Artifacts, or
// `None` when neither marker is present (the summary passes through). A timeout
// wins over an exit code (a timed-out command has no meaningful code). The tool
// attaches the Artifacts; this store renders them (ADR-0007: tool behaviors
// live in their Tools, and Presentment is role-less).
fn run_command_badge(artifacts: &HashMap<String, Value>) -> Option<String> {
    if artifacts
        .get(run_command::keys::TIMED_OUT)
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Some("✗ timed out".to_string());
    }
    let code = artifacts
        .get(run_command::keys::EXIT_CODE)
        .and_then(Value::as_i64)?;
    Some(if code == 0 {
        "✓ exit 0".to_string()
    } else {
        format!("✗ exit {code}")
    })
}

/// Maximum display width for a whole summary line (e.g. `key=value ...`).
const SUMMARY_WIDTH: usize = 100;
/// Maximum display width for a single field value inside a summary line.
const VALUE_WIDTH: usize = 60;

// The Tool Call / merged-result summary for a Tool Call input map. Special-cases
// `todo_write`: its `todos` array is rendered as the BODY of a
// [`TranscriptItem::Todo`] (the circle list), so the call/result summary is
// deliberately empty (qwen shows no description on a TodoWrite header). This is
// the STRUCTURAL fix for the raw-JSON leak: without it, both [`key_arg`] and its
// [`summarize_input`] fallback JSON-format the `todos` array, so an in-flight
// `todo_write` call OR a schema-passing-but-semantically-malformed one that
// drops all items (no Todo artifact → the Tool Result passes through) would show
// `todos=[{"content"...}]` in its summary. Every other tool defers to the
// salient-arg pick with its `key=value` fallback.
fn call_summary(name: &str, input: &Value) -> String {
    if name == "todo_write" {
        return String::new();
    }
    key_arg(name, input).unwrap_or_else(|| summarize_input(input))
}

// The single salient input arg for a merged one-liner, picked by tool: the
// `path` for read/edit/write, the `command` for run_command, the `pattern`/
// `query` for grep/search; otherwise the first value in alphabetical key order.
// `None` when the input carries no object values OR the picked value formats
// empty - the ONE emptiness rule, sourced here (so the caller falls back to the
// full `key=value` summary and never treats an empty arg as present). Truncated
// like [`format_value`] so a long path/command cannot blow out the line.
fn key_arg(name: &str, input: &Value) -> Option<String> {
    let obj = match input.as_object() {
        Some(obj) if !obj.is_empty() => obj,
        _ => return None,
    };
    let salient: &[&str] = match name {
        "read_file" | "edit" | "write_file" => &["file_path"],
        "notebook_edit" => &["notebook_path"],
        "list_directory" => &["path"],
        "run_shell_command" => &["command"],
        "grep_search" | "glob" => &["pattern", "query"],
        _ => &[],
    };
    let value = salient.iter().find_map(|key| obj.get(*key)).or_else(|| {
        // No named arg matched: fall back to the first value in sorted key
        // order, so the pick is stable regardless of map ordering.
        let mut keys: Vec<&String> = obj.keys().collect();
        keys.sort();
        keys.first().and_then(|k| obj.get(*k))
    })?;
    let formatted = format_value(value);
    (!formatted.is_empty()).then_some(formatted)
}

// One-line summary of a Tool Call input map, e.g. `path=lib/baud.ex`. Keys are
// sorted for a stable line (baud's `Enum.sort`).
fn summarize_input(input: &Value) -> String {
    let obj = match input.as_object() {
        Some(obj) if !obj.is_empty() => obj,
        _ => return String::new(),
    };
    let mut keys: Vec<&String> = obj.keys().collect();
    keys.sort();
    let joined = keys
        .iter()
        .map(|key| format!("{key}={}", format_value(&obj[*key])))
        .collect::<Vec<_>>()
        .join(" ");
    truncate(&joined, SUMMARY_WIDTH)
}

// One-line summary of a Tool Result content string.
fn summarize_result(content: &str) -> String {
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    match lines.as_slice() {
        [] => "(empty)".to_string(),
        [line] => truncate(line, SUMMARY_WIDTH),
        [line, rest @ ..] => {
            format!(
                "{} (+{} more lines)",
                truncate(line, SUMMARY_WIDTH),
                rest.len()
            )
        }
    }
}

fn format_value(value: &Value) -> String {
    match value {
        Value::String(s) => {
            let cleaned = s.replace(['\n', '\r'], "⏎");
            truncate(&cleaned, VALUE_WIDTH)
        }
        other => truncate(&inspect_value(other), VALUE_WIDTH),
    }
}

// Mirrors Elixir's `inspect/1` for the shapes a tool input carries: strings
// quoted, everything else its JSON-ish form.
fn inspect_value(value: &Value) -> String {
    match value {
        Value::String(s) => format!("{s:?}"),
        other => other.to_string(),
    }
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() > width {
        let keep = width.saturating_sub(1).max(1);
        let prefix: String = text.chars().take(keep).collect();
        format!("{prefix}…")
    } else {
        text.to_string()
    }
}

#[cfg(test)]
#[path = "../../tests/ui/transcript.rs"]
mod tests;
