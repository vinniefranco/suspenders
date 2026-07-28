//! UI Transcript - the display-side history of a Session (CONTEXT.md), as a
//! store (ADR-0034): the settled items, the revision counter the render cache
//! keys on, the in-flight [`Streaming`] snapshot (a private child module), and
//! Presentment (the Extension list lives here, so `present` runs on every append
//! by construction). The [`crate::ui::screen`] fold delegates one verb per
//! event arm; no caller can reach the items Vec directly.
//!
//! Pure like the rest of the core (ADR-0001/0019): no terminal, no async, no
//! IO, no ratatui types. Not `Clone`/`PartialEq` - extensions aren't.
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
//! * **Presentment runs before every append**, and a paired result's
//!   `key_arg` is stamped BEFORE Presentment (a extension may swap the item for
//!   a Block; stamping after would stamp a dropped item).
//! * **Fail-open lines bypass Presentment** (the recursion bound): a extension
//!   cannot re-present its own failure report, so a extension that panics on
//!   every item still terminates in one item plus one raw info line.
//! * The pending-Steering marker is authored HERE, by both
//!   [`Transcript::steering_queued`] and [`Transcript::steering_delivered`],
//!   so the delivered removal-by-equality can never desync from the queued
//!   text.
//! * **A new public verb enrolls in the property test**: the prefix-or-bump
//!   test's verb list is MANUAL (Rust cannot reflect over methods), so a new
//!   verb must be added to it and its expected-bumps guard revisited - the
//!   test cannot notice a verb it was never told about.
//!
//! Voice strings stay with the Screen (the greeting, stop reasons, wave
//! lines, nudges - recorded through [`Transcript::info`]); the store authors
//! only the two lines its own invariants require verbatim: the pending
//! Steering marker and the extension-failure line.

mod streaming;

use std::collections::HashMap;

use serde_json::Value;

use crate::content::ContentBlock;
use crate::event::Stage;
use crate::extensions::{self, Registered};
use streaming::Streaming;

/// The semantic style of one display line inside a [`TranscriptItem::Block`]
/// (ADR-0008). Names WHAT the line is; the terminal color mapping is
/// `ui/components`. Mirrors baud's `block_style :: :added | :removed | :context
/// | :emphasis | :muted`, plus a plain [`LineStyle::Default`] for unstyled text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineStyle {
    /// An added line (a `+` line in a diff).
    Added,
    /// A removed line (a `-` line in a diff).
    Removed,
    /// A context line (unchanged, shown for orientation).
    Context,
    /// Emphasised text.
    Emphasis,
    /// De-emphasised / secondary text (diff headers, elision tails).
    Muted,
    /// Plain, unstyled text.
    #[default]
    Default,
}

/// The semantic TONE of a harness-authored [`TranscriptItem::Marker`]
/// (ADR-0040): names WHO acted and in what spirit, so the adapter tints the
/// marker plane without ever sniffing the line's text. Like [`LineStyle`], the
/// tone is the semantic fact; the terminal color mapping lives in
/// `ui/components` (a Theme slot per tone). Stamped at the firing site (the
/// Event that voiced the marker), carried into [`Transcript::marker`]; the
/// store never classifies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tone {
    /// A budget mechanic tidying the Conversation: Eviction, Compaction,
    /// Result-Cap cuts. Not a Governor's judgment - neutral gray.
    Housekeeping,
    /// A Governor helping the model along: a Nudge, a plan/anchor refresh, a
    /// Recovery Run. Warm amber (chosen away from error-red).
    Aid,
    /// A Governor limiting the model: tool-narrowing, the Endgame's run-close
    /// schedule. Cool blue (chosen away from success-green).
    Constrain,
    /// The user's own voice reaching a running Run (the pending-Steering
    /// marker): the prompt color, never the harness plane.
    Steering,
    /// A marker with no assigned tone - the default a plain `push`ed marker or
    /// an older Session's line reads as. Muted, like an Info line.
    #[default]
    Plain,
}

/// One styled display line inside a [`TranscriptItem::Block`]: a semantic
/// [`LineStyle`] plus its text. Mirrors baud's `{style, text}` line tuple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledLine {
    pub style: LineStyle,
    pub text: String,
}

impl StyledLine {
    /// A styled line from any style and text.
    pub fn new(style: LineStyle, text: impl Into<String>) -> Self {
        StyledLine {
            style,
            text: text.into(),
        }
    }
}

/// A Transcript Item (CONTEXT.md): one entry in the display history.
///
/// Mirrors baud's `item` sum type:
///
/// * `User { text }` - `{:user, text}`.
/// * `Assistant { text }` - `{:assistant, text}`.
/// * `Thinking { text }` - `{:thinking, text}`.
/// * `ToolCall { id, name, summary }` - `{:tool_call, id, name, summary}`; `id`
///   is a display-opaque correlation token (the `tool_use_id`) used ONLY to
///   pair the call with its later `ToolResult` in the store - the display never
///   interprets it.
/// * `ToolResult { name, summary, is_error, key_arg }` -
///   `{:tool_result, name, summary, is_error, key_arg}`, the default one-line
///   summary a extension's `present` may replace; `key_arg` is the salient input
///   arg (path/command/pattern) carried over from the paired call so the merged
///   line reads `name  <key_arg> · <result>`, `None` for an unpaired result.
/// * `Block { title, lines }` - `{:block, title, lines}`: a titled block of
///   [`StyledLine`]s, the semantic display vocabulary (ADR-0008).
/// * `Info { text }` - `{:info, text}`: adapter-authored news with no marker
///   plane (the greeting, launch notices, the extension-failure line).
/// * `Marker { text, tone }` - a harness-authored line in the tinted marker
///   plane (ADR-0040): eviction, compaction, Governor Interventions, Steering.
///   The [`Tone`] tints it in the adapter; the store only carries the fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptItem {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        /// A display-opaque correlation token (the `tool_use_id`): used ONLY to
        /// pair this call with its later [`TranscriptItem::ToolResult`] in the
        /// store. The view never interprets or renders it.
        id: String,
        name: String,
        summary: String,
    },
    ToolResult {
        name: String,
        summary: String,
        is_error: bool,
        /// The salient input arg (path/command/pattern) carried from the paired
        /// [`TranscriptItem::ToolCall`], so the merged line can read
        /// `name  <key_arg> · <result>`. `None` for a result with no live call
        /// (e.g. governor-injected) - the line falls back to `name → result`.
        key_arg: Option<String>,
    },
    Block {
        title: String,
        lines: Vec<StyledLine>,
    },
    Info {
        text: String,
    },
    Marker {
        text: String,
        tone: Tone,
    },
}

impl TranscriptItem {
    /// The body this item collapses to under the global tools toggle (Ctrl-O),
    /// or `None` if the item has nothing to fold and always renders in full.
    ///
    /// This is the SEMANTIC collapse predicate (Stage 2 review C2): the view's
    /// fold keys on `foldable_body().is_some()`, not on a structural
    /// `matches!(item, Block)`, so the merge is free to choose an item's shape
    /// without re-implementing the fold rule. Today only a [`Block`] with a
    /// non-empty body folds; a merged one-line `ToolResult` has no body, so it
    /// never collapses. Stays pure - returns the pure-core [`StyledLine`] slice,
    /// never a ratatui type (ADR-0019).
    ///
    /// [`Block`]: TranscriptItem::Block
    pub fn foldable_body(&self) -> Option<&[StyledLine]> {
        match self {
            TranscriptItem::Block { lines, .. } if !lines.is_empty() => Some(lines),
            _ => None,
        }
    }

    /// The title an item collapses TO under the global tools toggle (Ctrl-O):
    /// the one-liner the view shows in place of the folded [`foldable_body`].
    /// Kept beside `foldable_body` so the collapse rule - predicate AND title -
    /// lives entirely in the pure core (Stage 2 review C2 / S1): the view
    /// composes the collapsed line from this accessor without matching on
    /// `Block`, so a future non-Block foldable item collapses the same way.
    /// Today only a [`Block`] has a fold title.
    ///
    /// [`foldable_body`]: TranscriptItem::foldable_body
    /// [`Block`]: TranscriptItem::Block
    pub fn fold_title(&self) -> Option<&str> {
        match self {
            TranscriptItem::Block { title, .. } => Some(title),
            _ => None,
        }
    }
}

/// The settled item a successor replaces - the ONE way anything leaves the
/// history. Both structural edits route through [`Transcript::supersede`], so
/// the revision rule (remove ⇒ bump) has exactly one body. Private on purpose:
/// the public interface is the named verbs; promote only when a third
/// pair-merge rule exists.
enum Anchor<'a> {
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
/// first. Owns the settled items, the revision counter, the in-flight
/// [`Streaming`] snapshot, and Presentment - see the module doc for the
/// invariants this seam holds.
///
/// `extensions` are not `Clone`/`PartialEq`, so neither is the store (nor the
/// Screen that owns it).
pub struct Transcript {
    /// The settled items. Private: every write routes through the append
    /// funnel (Presentment) or the supersede funnel (the revision rule).
    items: Vec<TranscriptItem>,
    /// Bumped by the two structural edits, NEVER by an append. The render
    /// cache extends incrementally while this holds still and rebuilds when it
    /// moves - appends are the hot path, structural edits the rare one.
    revision: u64,
    /// The in-flight streaming snapshot and its materialize rules (the private
    /// `streaming` child module owns the end/flush asymmetry).
    streaming: Streaming,
    /// The configured Extensions whose pure `present` runs on every append.
    extensions: Vec<Registered>,
}

impl Transcript {
    /// An empty Transcript. The caller authors any opening line (the greeting
    /// is the Screen's Voice, recorded through [`Transcript::info`]).
    pub fn new(extensions: Vec<Registered>) -> Self {
        Transcript {
            items: Vec::new(),
            revision: 0,
            streaming: Streaming::idle(),
            extensions,
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
            self.append(item, &HashMap::new());
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
            self.append(item, &HashMap::new());
        }
        if let Some(text) = note {
            self.info(text);
        }
    }

    // ---- Settled writes ----------------------------------------------------

    /// The generic append: Presentment, then push. NEVER bumps the revision.
    /// The public generic verb - like every named verb it routes through the
    /// one private append funnel - so a new [`TranscriptItem`] kind needs a
    /// variant and a render arm, never a new store method.
    pub fn push(&mut self, item: TranscriptItem) {
        self.append(item, &HashMap::new());
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

    /// Appends a harness marker in the tinted plane (ADR-0040): the caller
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
        let summary = key_arg(&name, input).unwrap_or_else(|| summarize_input(input));
        self.append(
            TranscriptItem::ToolCall { id, name, summary },
            &HashMap::new(),
        );
    }

    /// Merges a Tool Result with its call into ONE line: the pending
    /// [`TranscriptItem::ToolCall`] is found by `id` (NEVER by position -
    /// parallel tool calls interleave), its summary is recovered as the
    /// `key_arg` (it already IS the salient arg - [`key_arg`] never yields an
    /// empty string; the render layer normalizes any empty value once), the
    /// redundant call line is removed (a structural edit - the revision
    /// bumps), and the arg is stamped onto the result BEFORE Presentment. An
    /// unpaired result (governor-injected, no live call) removes nothing, does
    /// not bump, and carries no `key_arg` - a defined case, not an error.
    pub fn tool_result(
        &mut self,
        id: &str,
        name: String,
        content: &str,
        is_error: bool,
        artifacts: &HashMap<String, Value>,
    ) {
        let summary = summarize_result(content);
        self.supersede(Anchor::ToolCall { id }, artifacts, |call| {
            TranscriptItem::ToolResult {
                name,
                summary,
                is_error,
                key_arg: call.map(|item| match item {
                    TranscriptItem::ToolCall { summary, .. } => summary,
                    other => unreachable!("Anchor::ToolCall matched {other:?}"),
                }),
            }
        });
    }

    /// Appends the pending-Steering marker (ADR-0040: a [`Tone::Steering`]
    /// marker, the user's own voice in the plane). Its text is authored HERE
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
    /// ([`Anchor::Marker`]), matching what [`Transcript::steering_queued`]
    /// appended.
    pub fn steering_delivered(&mut self, text: impl Into<String>) {
        let text = text.into();
        let marker = pending_steering_line(&text);
        self.supersede(Anchor::Marker { text: &marker }, &HashMap::new(), |_| {
            TranscriptItem::User { text }
        });
    }

    /// Records the fail-open Extension report line (ADR-0007) - ONE format
    /// whether the failure came from this store's own Presentment fold or from
    /// the `extension_error` event the Run reports. Bypasses Presentment like
    /// every fail-open line (the recursion bound - see the module doc).
    pub fn extension_failure(&mut self, extension: &str, stage: Stage, message: &str) {
        self.items.push(TranscriptItem::Info {
            text: extension_failure_line(extension, stage, message),
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

    /// The in-flight Thinking text, from the latest streaming snapshot.
    pub fn streaming_thinking(&self) -> String {
        self.streaming.thinking()
    }

    // ---- Internals ----------------------------------------------------------

    // Presentment (CONTEXT.md), then push - the one append funnel. A crashing
    // Presenter is skipped fail-open (ADR-0007): the item from before it ran
    // survives, and its failure report lands as a RAW info line - never
    // re-presented, so an Extension that panics on every item cannot recurse on
    // its own report.
    fn append(&mut self, item: TranscriptItem, artifacts: &HashMap<String, Value>) {
        let (item, failures) = extensions::present(&self.extensions, item, artifacts);
        self.items.push(item);
        for failure in failures {
            self.extension_failure(&failure.extension, failure.stage, &failure.message);
        }
    }

    // The one supersede funnel: remove-maybe, bump-iff-removed, build the
    // successor FROM the removed anchor, present, append. Every structural
    // edit routes through here, so a verb cannot remove without bumping - the
    // RenderCache contract is held by construction, not by discipline.
    fn supersede(
        &mut self,
        anchor: Anchor<'_>,
        artifacts: &HashMap<String, Value>,
        successor: impl FnOnce(Option<TranscriptItem>) -> TranscriptItem,
    ) {
        let removed = self.locate(&anchor).map(|pos| {
            let item = self.items.remove(pos);
            // A non-append edit: settled items shifted, so any cached
            // per-item render state upstream is stale.
            self.revision += 1;
            item
        });
        self.append(successor(removed), artifacts);
    }

    fn locate(&self, anchor: &Anchor<'_>) -> Option<usize> {
        match anchor {
            Anchor::ToolCall { id } => self.items.iter().rposition(
                |m| matches!(m, TranscriptItem::ToolCall { id: call_id, .. } if call_id == id),
            ),
            Anchor::Marker { text } => self
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

// The fail-open Extension report (ADR-0007) - sourced once, so the store's own
// Presentment failures and the Run's `extension_error` events read identically.
fn extension_failure_line(extension: &str, stage: Stage, message: &str) -> String {
    format!("plugin {extension} failed in {}: {message}", stage.as_str())
}

/// Maximum display width for a whole summary line (e.g. `key=value ...`).
const SUMMARY_WIDTH: usize = 100;
/// Maximum display width for a single field value inside a summary line.
const VALUE_WIDTH: usize = 60;

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
        "read_file" | "edit_file" | "write_file" => &["path"],
        "run_command" => &["command"],
        "grep" | "search" => &["pattern", "query"],
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
mod tests {
    use super::*;
    use crate::presenter::Presenter;
    use serde_json::json;

    // --- helpers ------------------------------------------------------------

    fn fresh() -> Transcript {
        Transcript::new(Vec::new())
    }

    fn user(text: &str) -> TranscriptItem {
        TranscriptItem::User { text: text.into() }
    }
    fn assistant(text: &str) -> TranscriptItem {
        TranscriptItem::Assistant { text: text.into() }
    }
    fn thinking(text: &str) -> TranscriptItem {
        TranscriptItem::Thinking { text: text.into() }
    }
    fn info(text: &str) -> TranscriptItem {
        TranscriptItem::Info { text: text.into() }
    }
    fn marker(text: &str, tone: Tone) -> TranscriptItem {
        TranscriptItem::Marker {
            text: text.into(),
            tone,
        }
    }
    fn tool_call_item(id: &str, name: &str, summary: &str) -> TranscriptItem {
        TranscriptItem::ToolCall {
            id: id.into(),
            name: name.into(),
            summary: summary.into(),
        }
    }
    fn tool_result_item(name: &str, summary: &str, is_error: bool) -> TranscriptItem {
        TranscriptItem::ToolResult {
            name: name.into(),
            summary: summary.into(),
            is_error,
            key_arg: None,
        }
    }
    fn tool_result_merged(
        name: &str,
        summary: &str,
        is_error: bool,
        key_arg: &str,
    ) -> TranscriptItem {
        TranscriptItem::ToolResult {
            name: name.into(),
            summary: summary.into(),
            is_error,
            key_arg: Some(key_arg.into()),
        }
    }

    fn text_block(text: &str) -> ContentBlock {
        ContentBlock::Text { text: text.into() }
    }
    fn thinking_block(text: &str) -> ContentBlock {
        ContentBlock::Thinking { text: text.into() }
    }

    // --- streaming lifecycle -------------------------------------------------

    #[test]
    fn message_end_materializes_thinking_then_text() {
        let mut t = fresh();
        t.message_start();
        t.message_update(vec![thinking_block("hmm"), text_block("reading")]);
        t.message_end(&[text_block("reading")]);
        assert_eq!(t.items(), vec![thinking("hmm"), assistant("reading")]);
        assert!(t.streaming_text().is_empty() && t.streaming_thinking().is_empty());
    }

    #[test]
    fn no_thinking_in_snapshot_yields_no_thinking_item() {
        let mut t = fresh();
        t.message_start();
        t.message_end(&[text_block("no thinking here")]);
        assert_eq!(t.items(), vec![assistant("no thinking here")]);
    }

    #[test]
    fn discard_streaming_drops_the_snapshot_without_settling_it() {
        let mut t = fresh();
        t.message_start();
        t.message_update(vec![text_block("stale")]);
        t.discard_streaming();
        assert!(t.streaming_text().is_empty());
        // Nothing to salvage on a later close: the snapshot is gone.
        t.close(None);
        assert!(t.items().is_empty());
    }

    // --- close (the flush-before-note ordering) -------------------------------

    #[test]
    fn close_settles_the_live_snapshot_then_records_the_note() {
        let mut t = fresh();
        t.message_start();
        t.message_update(vec![thinking_block("mid"), text_block("partial")]);
        t.close(Some("turn cancelled".into()));
        assert_eq!(
            t.items(),
            vec![
                thinking("mid"),
                assistant("partial"),
                info("turn cancelled")
            ]
        );
        // The snapshot emptied: a second close salvages nothing and records
        // only its note.
        t.close(None);
        assert_eq!(t.items().len(), 3);
    }

    #[test]
    fn a_clean_close_with_no_note_is_silent_when_idle() {
        let mut t = fresh();
        t.close(None);
        assert!(t.items().is_empty());
    }

    // --- tool_call summaries --------------------------------------------------

    #[test]
    fn key_arg_picks_the_salient_arg_by_tool() {
        // path for the file tools, command for run_command, pattern/query for
        // grep/search.
        assert_eq!(
            key_arg(
                "read_file",
                &json!({"path": "src/foo.rs", "start_line": 10})
            ),
            Some("src/foo.rs".to_string())
        );
        assert_eq!(
            key_arg("run_command", &json!({"command": "cargo test"})),
            Some("cargo test".to_string())
        );
        assert_eq!(
            key_arg("grep", &json!({"pattern": "TODO", "path": "src"})),
            Some("TODO".to_string())
        );
        assert_eq!(
            key_arg("search", &json!({"query": "needle"})),
            Some("needle".to_string())
        );
    }

    #[test]
    fn key_arg_falls_back_to_the_first_sorted_value_and_none_when_empty() {
        // No named arg for this tool: the first value in sorted key order.
        assert_eq!(
            key_arg("mystery_tool", &json!({"zeta": "z", "alpha": "a"})),
            Some("a".to_string())
        );
        // An empty / non-object input has no salient arg.
        assert_eq!(key_arg("read_file", &json!({})), None);
        assert_eq!(key_arg("read_file", &json!("not an object")), None);
    }

    // The single emptiness rule (Nit-1): a salient arg that formats empty yields
    // None, so the live call line falls back to the full summary rather than a
    // dangling `name  ` with a blank arg.
    #[test]
    fn key_arg_maps_an_empty_formatted_value_to_none() {
        assert_eq!(key_arg("run_command", &json!({"command": ""})), None);
        // The live call line then falls back to summarize_input, not a blank arg.
        let mut t = fresh();
        t.tool_call("t1".into(), "run_command".into(), &json!({"command": ""}));
        assert_eq!(
            t.items(),
            vec![tool_call_item("t1", "run_command", "command=")]
        );
    }

    #[test]
    fn a_live_tool_call_line_reads_name_then_key_arg_not_key_equals_value() {
        let mut t = fresh();
        t.tool_call(
            "t1".into(),
            "read_file".into(),
            &json!({"path": "src/foo.rs"}),
        );
        assert_eq!(
            t.items(),
            vec![tool_call_item("t1", "read_file", "src/foo.rs")]
        );
    }

    // --- call/result pairing merge ---------------------------------------------

    #[test]
    fn tool_result_appends_summary_with_error_flag() {
        let mut t = fresh();
        t.tool_result("t1", "grep".into(), "a\nb\nc", false, &HashMap::new());
        assert_eq!(
            t.items(),
            vec![tool_result_item("grep", "a (+2 more lines)", false)]
        );
    }

    // The paired call+result collapse to ONE result item carrying the call's
    // key_arg; the redundant call line is removed and the revision bumps.
    #[test]
    fn a_result_merges_with_its_call_removing_the_call_and_bumping_revision() {
        let mut t = fresh();
        t.tool_call(
            "t1".into(),
            "read_file".into(),
            &json!({"path": "src/foo.rs"}),
        );
        let rev_after_call = t.revision();
        t.tool_result(
            "t1",
            "read_file".into(),
            "340 lines",
            false,
            &HashMap::new(),
        );
        // Call gone, ONE merged result with the recovered key_arg.
        assert_eq!(
            t.items(),
            vec![tool_result_merged(
                "read_file",
                "340 lines",
                false,
                "src/foo.rs"
            )]
        );
        // The removal is a non-append edit: revision moved.
        assert_eq!(t.revision(), rev_after_call + 1);
    }

    // An in-flight call with no result yet renders alone and never bumps.
    #[test]
    fn an_in_flight_call_renders_alone_without_bumping_revision() {
        let mut t = fresh();
        t.tool_call("t1".into(), "run_command".into(), &json!({"command": "ls"}));
        assert_eq!(t.items(), vec![tool_call_item("t1", "run_command", "ls")]);
        assert_eq!(t.revision(), 0);
    }

    // Parallel/interleaved ids pair by id, not by position: the second result
    // matches the first call.
    #[test]
    fn parallel_calls_pair_by_id_not_by_position() {
        let mut t = fresh();
        t.tool_call("a".into(), "read_file".into(), &json!({"path": "a.rs"}));
        t.tool_call("b".into(), "read_file".into(), &json!({"path": "b.rs"}));
        // Result for the FIRST call arrives second.
        t.tool_result("a", "read_file".into(), "10 lines", false, &HashMap::new());
        // Call `a` merged away; call `b` still pending; result carries a.rs.
        assert_eq!(
            t.items(),
            vec![
                tool_call_item("b", "read_file", "b.rs"),
                tool_result_merged("read_file", "10 lines", false, "a.rs"),
            ]
        );
    }

    // A result with no live call (governor-injected) removes nothing, does not
    // bump, and carries no key_arg.
    #[test]
    fn an_unpaired_result_does_not_bump_and_has_no_key_arg() {
        let mut t = fresh();
        t.tool_result(
            "orphan",
            "run_command".into(),
            "injected",
            false,
            &HashMap::new(),
        );
        assert_eq!(
            t.items(),
            vec![tool_result_item("run_command", "injected", false)]
        );
        assert_eq!(t.revision(), 0);
    }

    // An error result keeps is_error, still removes the call, stamps key_arg,
    // and bumps.
    #[test]
    fn an_error_result_merges_keeping_the_error_flag_and_key_arg() {
        let mut t = fresh();
        t.tool_call(
            "t1".into(),
            "run_command".into(),
            &json!({"command": "cargo test"}),
        );
        t.tool_result("t1", "run_command".into(), "boom", true, &HashMap::new());
        assert_eq!(
            t.items(),
            vec![tool_result_merged(
                "run_command",
                "boom",
                true,
                "cargo test"
            )]
        );
        assert_eq!(t.revision(), 1);
    }

    // --- steering (the marker equality, queued ↔ delivered) --------------------

    #[test]
    fn steering_queued_shows_the_pending_marker_delivered_promotes_it_to_user() {
        let mut t = fresh();
        t.steering_queued("check the README");
        // A Steering-toned marker in the plane, not a plain Info line.
        assert_eq!(
            t.items(),
            vec![marker("↳ queued: check the README", Tone::Steering)]
        );

        t.steering_delivered("check the README");
        assert_eq!(t.items(), vec![user("check the README")]);
    }

    // A Steering marker whose text matches an existing Info line is NOT
    // removed by delivery: the anchor targets the Marker variant only, so a
    // look-alike Info cannot be superseded by a Steering delivery.
    #[test]
    fn steering_delivered_anchors_on_the_marker_variant_not_a_look_alike_info() {
        let mut t = fresh();
        t.info("↳ queued: not really steering");
        t.steering_queued("not really steering");
        // Info first, then the real Steering marker.
        assert_eq!(
            t.items(),
            vec![
                info("↳ queued: not really steering"),
                marker("↳ queued: not really steering", Tone::Steering),
            ]
        );

        t.steering_delivered("not really steering");
        // The Marker was removed and promoted; the Info look-alike survives.
        assert_eq!(
            t.items(),
            vec![
                info("↳ queued: not really steering"),
                user("not really steering"),
            ]
        );
    }

    // The render cache's append-only contract: pushes leave the revision
    // alone; a delivered steering's marker removal bumps it. A delivery whose
    // marker was never queued removes nothing and must not bump.
    #[test]
    fn only_a_delivered_steering_removal_bumps_the_revision() {
        let mut t = fresh();
        t.user("hello");
        t.info("adapter news");
        assert_eq!(t.revision(), 0);

        // Queued (a push) does not bump; delivered (the remove) does.
        t.steering_queued("check the README");
        assert_eq!(t.revision(), 0);
        t.steering_delivered("check the README");
        assert_eq!(t.revision(), 1);

        // Delivered with no matching marker removes nothing: no bump.
        t.steering_delivered("never queued");
        assert_eq!(t.revision(), 1);
    }

    // --- presentment -----------------------------------------------------------

    struct BlockPresenter;
    impl Presenter for BlockPresenter {
        fn present(
            &self,
            item: TranscriptItem,
            artifacts: &HashMap<String, Value>,
            _opts: &Value,
        ) -> TranscriptItem {
            match &item {
                TranscriptItem::ToolResult {
                    name,
                    is_error: false,
                    ..
                } if artifacts.contains_key("diff") => TranscriptItem::Block {
                    title: format!("diff {name}"),
                    lines: vec![StyledLine::new(LineStyle::Added, "+ new line")],
                },
                _ => item,
            }
        }
    }

    struct PresentCrasher;
    impl Presenter for PresentCrasher {
        fn present(
            &self,
            _item: TranscriptItem,
            _artifacts: &HashMap<String, Value>,
            _opts: &Value,
        ) -> TranscriptItem {
            panic!("render boom")
        }
    }

    fn reg(name: &str, presenter: Box<dyn Presenter>) -> Registered {
        Registered::new(name, Value::Null).with_presenter(presenter)
    }

    fn diff_artifacts() -> HashMap<String, Value> {
        let mut artifacts = HashMap::new();
        artifacts.insert("diff".to_string(), json!("some_diff"));
        artifacts
    }

    #[test]
    fn extension_replaces_tool_result_summary_using_artifacts() {
        let mut t = Transcript::new(vec![reg("BlockPresenter", Box::new(BlockPresenter))]);
        t.tool_result(
            "t1",
            "edit_file".into(),
            "edited x",
            false,
            &diff_artifacts(),
        );
        assert_eq!(
            t.items(),
            vec![TranscriptItem::Block {
                title: "diff edit_file".into(),
                lines: vec![StyledLine::new(LineStyle::Added, "+ new line")],
            }]
        );
    }

    #[test]
    fn without_matching_artifact_default_summary_survives() {
        let mut t = Transcript::new(vec![reg("BlockPresenter", Box::new(BlockPresenter))]);
        t.tool_result("t1", "edit_file".into(), "edited x", false, &HashMap::new());
        assert_eq!(
            t.items(),
            vec![tool_result_item("edit_file", "edited x", false)]
        );
    }

    #[test]
    fn tool_call_items_pass_through_present() {
        let mut t = Transcript::new(vec![reg("BlockPresenter", Box::new(BlockPresenter))]);
        t.tool_call("t1".into(), "grep".into(), &json!({}));
        assert_eq!(t.items(), vec![tool_call_item("t1", "grep", "")]);
    }

    #[test]
    fn crashing_present_falls_back_to_default_with_info_line() {
        let mut t = Transcript::new(vec![reg("PresentCrasher", Box::new(PresentCrasher))]);
        t.tool_result(
            "t1",
            "edit_file".into(),
            "edited x",
            false,
            &diff_artifacts(),
        );
        let items = t.items();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], tool_result_item("edit_file", "edited x", false));
        match &items[1] {
            TranscriptItem::Info { text } => {
                assert!(text.contains("PresentCrasher"));
                assert!(text.contains("present"));
                assert!(text.contains("render boom"));
            }
            other => panic!("expected info, got {other:?}"),
        }
    }

    // The recursion bound: the fail-open report line is pushed RAW, never
    // re-presented - a extension that panics on EVERY item (this one) would
    // otherwise crash on its own failure report, report that, crash on the
    // report of the report, and never terminate.
    #[test]
    fn a_presentment_failure_line_bypasses_presentment() {
        let mut t = Transcript::new(vec![reg("PresentCrasher", Box::new(PresentCrasher))]);
        t.info("news");
        let items = t.items();
        // The pre-stage item survives fail-open, then exactly ONE raw report.
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], info("news"));
        match &items[1] {
            TranscriptItem::Info { text } => assert!(text.contains("PresentCrasher")),
            other => panic!("expected info, got {other:?}"),
        }
    }

    // The diff-Block redundancy case: because the paired call is removed, the
    // Diff extension's Block (whose title summarizes the call) stands alone.
    #[test]
    fn a_diff_block_stands_alone_after_the_paired_call_is_removed() {
        let mut t = Transcript::new(vec![reg("BlockPresenter", Box::new(BlockPresenter))]);
        t.tool_call(
            "t1".into(),
            "edit_file".into(),
            &json!({"path": "src/x.rs"}),
        );
        t.tool_result("t1", "edit_file".into(), "edited", false, &diff_artifacts());
        // Only the Block remains - the redundant call line is gone.
        assert_eq!(
            t.items(),
            vec![TranscriptItem::Block {
                title: "diff edit_file".into(),
                lines: vec![StyledLine::new(LineStyle::Added, "+ new line")],
            }]
        );
        assert_eq!(t.revision(), 1);
    }

    // --- the revision contract, as a property ----------------------------------

    // Every verb BELOW either leaves the settled items a prefix of what they
    // become (an append, revision still) or bumps the revision (a structural
    // edit). This is THE contract the render cache keys on. The verb list is
    // MANUAL - Rust cannot reflect over methods - so a new public verb must
    // enroll here and revisit the bumps-count guard at the end (see the
    // module-doc invariant list); the test cannot notice a verb it was never
    // told about.
    #[test]
    fn every_verb_preserves_the_items_prefix_or_bumps_the_revision() {
        type Step = (&'static str, Box<dyn FnOnce(&mut Transcript)>);
        let steps: Vec<Step> = vec![
            ("info", Box::new(|t| t.info("news"))),
            (
                "marker",
                Box::new(|t| t.marker("✂ evicted 3 stale results", Tone::Housekeeping)),
            ),
            ("user", Box::new(|t| t.user("hello"))),
            (
                "push",
                Box::new(|t| {
                    t.push(TranscriptItem::Block {
                        title: "block".into(),
                        lines: vec![StyledLine::new(LineStyle::Added, "+ a")],
                    })
                }),
            ),
            ("message_start", Box::new(|t| t.message_start())),
            (
                "message_update",
                Box::new(|t| t.message_update(vec![text_block("partial")])),
            ),
            (
                "message_end",
                Box::new(|t| t.message_end(&[text_block("done")])),
            ),
            (
                "steering_queued",
                Box::new(|t| t.steering_queued("steer me")),
            ),
            (
                "steering_delivered",
                Box::new(|t| t.steering_delivered("steer me")),
            ),
            (
                "tool_call",
                Box::new(|t| t.tool_call("t1".into(), "grep".into(), &json!({"pattern": "x"}))),
            ),
            (
                "tool_result",
                Box::new(|t| t.tool_result("t1", "grep".into(), "hit", false, &HashMap::new())),
            ),
            (
                "extension_failure",
                Box::new(|t| t.extension_failure("P", Stage::Present, "boom")),
            ),
            ("message_start (again)", Box::new(|t| t.message_start())),
            (
                "message_update (again)",
                Box::new(|t| t.message_update(vec![thinking_block("half")])),
            ),
            (
                "close",
                Box::new(|t| t.close(Some("turn cancelled".into()))),
            ),
            ("discard_streaming", Box::new(|t| t.discard_streaming())),
        ];

        let mut t = fresh();
        let mut bumps = 0;
        for (name, step) in steps {
            let rev_before = t.revision();
            let before = t.items().to_vec();
            step(&mut t);
            if t.revision() == rev_before {
                assert!(
                    t.items().starts_with(&before),
                    "{name} changed settled items without bumping the revision"
                );
            } else {
                assert!(
                    t.revision() > rev_before,
                    "{name} moved the revision backwards"
                );
                bumps += 1;
            }
        }
        // Both structural verbs actually removed something above - the prefix
        // half of the property was not satisfied vacuously.
        assert_eq!(bumps, 2, "steering_delivered and tool_result each bumped");
    }

    // --- marker plane (ADR-0040) -------------------------------------------------

    // A marker APPENDS with its carried tone and never bumps the revision - it
    // is an ordinary append, not a structural edit.
    #[test]
    fn marker_appends_with_its_tone_and_does_not_bump() {
        let mut t = fresh();
        t.marker("⟨ compacted 41 messages → summary ⟩", Tone::Housekeeping);
        t.marker("⚑ plan refreshed", Tone::Aid);
        assert_eq!(
            t.items(),
            vec![
                marker("⟨ compacted 41 messages → summary ⟩", Tone::Housekeeping),
                marker("⚑ plan refreshed", Tone::Aid),
            ]
        );
        assert_eq!(t.revision(), 0);
    }

    // --- item vocabulary ---------------------------------------------------------

    // The predicate and its title travel together in the pure core (S1): a
    // non-empty Block has both; everything else (empty Block, one-line result)
    // has neither, so the view never re-implements the fold rule.
    #[test]
    fn foldable_body_and_fold_title_agree_on_what_collapses() {
        let block = TranscriptItem::Block {
            title: "edit_file x (+1 -1)".into(),
            lines: vec![StyledLine::new(LineStyle::Added, "+ a")],
        };
        assert!(block.foldable_body().is_some());
        assert_eq!(block.fold_title(), Some("edit_file x (+1 -1)"));

        // An empty Block: no body to fold. (It still HAS a title, but the view
        // gates on `foldable_body().is_some()`, so it never collapses.)
        let empty = TranscriptItem::Block {
            title: "empty".into(),
            lines: vec![],
        };
        assert!(empty.foldable_body().is_none());

        // A merged one-line ToolResult: neither.
        let result = TranscriptItem::ToolResult {
            name: "read_file".into(),
            summary: "340 lines".into(),
            is_error: false,
            key_arg: Some("src/foo.rs".into()),
        };
        assert!(result.foldable_body().is_none());
        assert_eq!(result.fold_title(), None);
    }
}
