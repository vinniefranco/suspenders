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
//! Voice strings stay with the Screen (the startup Header, stop reasons, wave
//! lines, nudges - recorded through [`Transcript::header`]/[`Transcript::info`]);
//! the store authors only the two lines its own invariants require verbatim: the
//! pending Steering marker and the extension-failure line.

mod streaming;
mod thought;

use std::collections::HashMap;

use serde_json::Value;

use crate::content::ContentBlock;
use crate::event::Stage;
use crate::extensions::{self, Registered};
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
    /// How many leading items the adapter has already frozen into native
    /// scrollback (ADR-0046, the inline `insert_before` seam). A monotonic
    /// high-water mark into [`Transcript::items`]: everything below it has been
    /// committed and is never redrawn; everything at/above it is still the
    /// pending region. Advanced by [`Transcript::mark_committed`], never
    /// regressed - and it moves neither `items` nor `revision`, so it is not a
    /// structural edit (it enrolls in the prefix-or-bump property test with a
    /// "neither" expectation).
    committed: usize,
}

impl Transcript {
    /// An empty Transcript. The caller authors any opening line (the startup
    /// Header is the Screen's Voice, recorded through [`Transcript::header`]).
    pub fn new(extensions: Vec<Registered>) -> Self {
        Transcript {
            items: Vec::new(),
            revision: 0,
            streaming: Streaming::idle(),
            extensions,
            committed: 0,
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
    /// unpaired result (a Voice answer to an orphaned call, no live call)
    /// removes nothing, does not bump, and carries no `key_arg` - a defined
    /// case, not an error.
    pub fn tool_result(
        &mut self,
        id: &str,
        name: String,
        content: &str,
        is_error: bool,
        artifacts: &HashMap<String, Value>,
    ) {
        let summary = summarize_result(content);
        self.supersede(Locator::ToolCall { id }, artifacts, |call| {
            TranscriptItem::ToolResult {
                name,
                summary,
                is_error,
                key_arg: call.map(|item| match item {
                    TranscriptItem::ToolCall { summary, .. } => summary,
                    other => unreachable!("Locator::ToolCall matched {other:?}"),
                }),
            }
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
        self.supersede(Locator::Marker { text: &marker }, &HashMap::new(), |_| {
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

    /// Whether flipping compact mode would CHANGE what the frozen scrollback
    /// shows (qwen `compactToggleHasVisualEffect`, `mergeCompactToolGroups.ts`):
    /// `true` iff any COMMITTED item is one compact hides or reveals - a
    /// [`TranscriptItem::Thinking`] (compact hides it entirely) or a tool-group
    /// member (`ToolCall`/`ToolResult`/`Diff`/`Todo`, whose result BODY compact
    /// hides). A transcript of only User/Assistant/Info/Marker items has nothing
    /// compact touches, so the Ctrl+O handler skips the expensive scrollback
    /// redraw (ADR-0052) - a plain chat toggles with no flicker.
    ///
    /// Committed-only on purpose: the pending region redraws every frame at the
    /// new compact for free, so only the FROZEN prefix `[0, committed_high_water)`
    /// needs the [`crate::ui::screen::Effect::RedrawScrollback`] re-blit. Pure -
    /// no ratatui, a testable predicate.
    pub fn compact_toggle_has_visual_effect(&self) -> bool {
        self.items[..self.committed]
            .iter()
            .any(compact_hides_or_reveals)
    }

    // ---- The Commit seam (ADR-0046) ----------------------------------------

    /// How many leading items have been frozen into native scrollback - the
    /// monotonic high-water mark the inline adapter's `insert_before` seam
    /// keeps. Items `[0, committed_high_water())` are committed and never
    /// redrawn; the pending region is `items()[committed_high_water()..]`.
    // qual:allow(dry, boilerplate) reason: "the `committed` mark is the pure
    // core's own state; making the field pub would let the adapter regress it and
    // break committed immutability (ADR-0046). The read stays behind this getter,
    // the same encapsulation boundary as `items`/`revision`."
    pub fn committed_high_water(&self) -> usize {
        self.committed
    }

    /// How far the pending region may be committed right now: the count of
    /// leading items that are FINAL and in order, stopping at the first
    /// non-terminal one (a live [`TranscriptItem::ToolCall`] awaiting its
    /// result, or a [`Tone::Steering`] marker awaiting delivery). Clamped to
    /// never fall below the high-water mark, so the seam only ever advances.
    ///
    /// `User`/`Info`/`ToolResult`/`Assistant`/`Thinking`/`Diff` are terminal:
    /// the live stream lives in [`Streaming`], never in `items`, so any settled
    /// `Assistant`/`Thinking` is final. A `ToolCall` is the boundary - a later
    /// `ToolResult` supersedes it (a structural edit that would rewrite frozen
    /// scrollback). A `Tone::Steering` marker is non-terminal - delivery removes
    /// it - so it must never be committed.
    pub fn committable_upto(&self) -> usize {
        self.items
            .iter()
            .take_while(|it| item_terminal(it))
            .count()
            .max(self.committed)
    }

    /// Advances the high-water mark by `n` committed items (the adapter has
    /// frozen them into scrollback). Monotonic: it never regresses, and it
    /// mutates neither `items` nor `revision` - committing is not a structural
    /// edit, so the RenderCache contract is untouched.
    // qual:allow(dry, boilerplate) reason: "not a plain setter - it CLAMPS
    // (saturating add, capped at len) to keep the mark monotonic and in-bounds,
    // the load-bearing half of the transactional commit seam (ADR-0046). The
    // field must stay private so this clamp cannot be bypassed."
    pub fn mark_committed(&mut self, n: usize) {
        self.committed = self.committed.saturating_add(n).min(self.items.len());
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
        locator: Locator<'_>,
        artifacts: &HashMap<String, Value>,
        successor: impl FnOnce(Option<TranscriptItem>) -> TranscriptItem,
    ) {
        let removed = self.locate(&locator).map(|pos| {
            let item = self.items.remove(pos);
            // A non-append edit: settled items shifted, so any cached
            // per-item render state upstream is stale.
            self.revision += 1;
            item
        });
        self.append(successor(removed), artifacts);
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

// Whether an item is one compact mode hides or reveals (qwen: a Thinking item,
// hidden entirely, or a tool-group member, whose result body is hidden). A
// User/Assistant/Info/Marker item is untouched by compact, so a transcript of
// only those toggles with no visual effect. Pure - the predicate behind
// [`Transcript::compact_toggle_has_visual_effect`].
fn compact_hides_or_reveals(item: &TranscriptItem) -> bool {
    matches!(
        item,
        TranscriptItem::Thinking { .. }
            | TranscriptItem::ToolCall { .. }
            | TranscriptItem::ToolResult { .. }
            | TranscriptItem::Diff { .. }
            | TranscriptItem::Todo { .. }
    )
}

// Whether a settled item is FINAL - safe to freeze into native scrollback
// (ADR-0046). A `ToolCall` awaits its result (a later `ToolResult` supersedes
// it), and a `Tone::Steering` marker awaits delivery (which removes it); both
// are structural edits that would rewrite frozen output, so neither commits.
// Everything else is terminal.
fn item_terminal(item: &TranscriptItem) -> bool {
    !matches!(
        item,
        TranscriptItem::ToolCall { .. }
            | TranscriptItem::Marker {
                tone: Tone::Steering,
                ..
            }
    )
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
mod tests {
    use super::*;
    use crate::presenter::Presenter;
    use crate::view_model::{DiffHunk, DiffLine, DiffSide};
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
            key_arg("run_shell_command", &json!({"command": "cargo test"})),
            Some("cargo test".to_string())
        );
        assert_eq!(
            key_arg("grep_search", &json!({"pattern": "TODO", "path": "src"})),
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
        assert_eq!(key_arg("run_shell_command", &json!({"command": ""})), None);
        // The live call line then falls back to summarize_input, not a blank arg.
        let mut t = fresh();
        t.tool_call("t1".into(), "run_shell_command".into(), &json!({"command": ""}));
        assert_eq!(
            t.items(),
            vec![tool_call_item("t1", "run_shell_command", "command=")]
        );
    }

    // The raw-JSON leak fix (P0): a `todo_write` NEVER shows its `todos` array in
    // a summary - not the in-flight call, not a schema-passing-but-semantically-
    // malformed result that drops all items (no Todo artifact, so the Tool Result
    // passes through). Both paths draw their summary from `call_summary`, which is
    // empty for `todo_write` (the list is the Todo body, not a description).
    #[test]
    fn todo_write_never_leaks_raw_json_in_any_summary() {
        // The salient-arg pick and its fallback BOTH JSON-format the array; the
        // structural fix is a clean empty summary.
        let malformed = json!({"todos": [{"content": "", "status": "bogus"}]});
        assert_eq!(call_summary("todo_write", &malformed), "");
        // The raw pick DOES carry the JSON (this is what would have leaked).
        assert!(key_arg("mystery", &malformed).unwrap().contains("content"));

        // (b) The in-flight call reads a bare `todo_write` (empty summary).
        let mut t = fresh();
        t.tool_call("t1".into(), "todo_write".into(), &malformed);
        assert_eq!(t.items(), vec![tool_call_item("t1", "todo_write", "")]);

        // (a) A malformed result that passes through (no Todo artifact) recovers
        // the call's clean summary as its key_arg - no `todos=[...]` anywhere.
        t.tool_result(
            "t1",
            "todo_write".into(),
            "Recorded.",
            false,
            &HashMap::new(),
        );
        let rendered = format!("{:?}", t.items());
        assert!(
            !rendered.contains("todos") && !rendered.contains("content:"),
            "todo_write summary leaked raw JSON: {rendered}"
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
        t.tool_result("t1", "grep_search".into(), "a\nb\nc", false, &HashMap::new());
        assert_eq!(
            t.items(),
            vec![tool_result_item("grep_search", "a (+2 more lines)", false)]
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
        t.tool_call("t1".into(), "run_shell_command".into(), &json!({"command": "ls"}));
        assert_eq!(t.items(), vec![tool_call_item("t1", "run_shell_command", "ls")]);
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

    // A result with no live call (a Voice answer to an orphaned call) removes
    // nothing, does not bump, and carries no key_arg.
    #[test]
    fn an_unpaired_result_does_not_bump_and_has_no_key_arg() {
        let mut t = fresh();
        t.tool_result(
            "orphan",
            "run_shell_command".into(),
            "injected",
            false,
            &HashMap::new(),
        );
        assert_eq!(
            t.items(),
            vec![tool_result_item("run_shell_command", "injected", false)]
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
            "run_shell_command".into(),
            &json!({"command": "cargo test"}),
        );
        t.tool_result("t1", "run_shell_command".into(), "boom", true, &HashMap::new());
        assert_eq!(
            t.items(),
            vec![tool_result_merged(
                "run_shell_command",
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

    // --- the Commit seam (ADR-0046) --------------------------------------------

    // Leading terminal items commit; a live ToolCall is the boundary and
    // everything from it on stays pending.
    #[test]
    fn committable_stops_at_the_first_live_tool_call() {
        let mut t = fresh();
        t.user("do a thing");
        t.push(assistant("on it"));
        t.tool_call("t1".into(), "read_file".into(), &json!({"path": "a.rs"}));
        t.push(info("news after the call"));
        // Two leading terminals (User, Assistant); the ToolCall blocks the rest.
        assert_eq!(t.committable_upto(), 2);
    }

    // A Tone::Steering marker is non-terminal (delivery removes it) and blocks
    // the commit; a plain marker is terminal and commits.
    #[test]
    fn committable_treats_a_steering_marker_as_non_terminal() {
        let mut t = fresh();
        t.info("a");
        t.marker("housekeeping", Tone::Housekeeping);
        t.steering_queued("steer me");
        t.info("b");
        // Info, Marker(Housekeeping) commit; the Steering marker is the boundary.
        assert_eq!(t.committable_upto(), 2);
    }

    // Delivery promotes the pending Steering marker to a terminal User item, so
    // the whole prefix becomes committable.
    #[test]
    fn committable_advances_once_steering_is_delivered() {
        let mut t = fresh();
        t.info("a");
        t.steering_queued("steer me");
        assert_eq!(t.committable_upto(), 1);
        t.steering_delivered("steer me");
        // Now Info, User - both terminal.
        assert_eq!(t.committable_upto(), 2);
    }

    // mark_committed is monotonic, clamps to the item count, and moves neither
    // the items nor the revision.
    #[test]
    fn mark_committed_is_monotonic_and_clamped() {
        let mut t = fresh();
        t.info("a");
        t.info("b");
        let rev = t.revision();
        let items = t.items().to_vec();

        assert_eq!(t.committed_high_water(), 0);
        t.mark_committed(1);
        assert_eq!(t.committed_high_water(), 1);
        // Advances, never regresses.
        t.mark_committed(1);
        assert_eq!(t.committed_high_water(), 2);
        // Clamps to the item count - an over-advance cannot outrun the items.
        t.mark_committed(10);
        assert_eq!(t.committed_high_water(), 2);

        // Committing is not a structural edit.
        assert_eq!(t.revision(), rev);
        assert_eq!(t.items(), items.as_slice());
    }

    // committable_upto never falls below the high-water mark, even when the
    // pending front becomes non-terminal after a commit.
    #[test]
    fn committable_never_regresses_below_the_high_water_mark() {
        let mut t = fresh();
        t.info("a");
        t.info("b");
        t.mark_committed(2);
        // A live ToolCall now leads the pending region, but the two already
        // committed items keep committable at the high-water mark.
        t.tool_call("t1".into(), "grep_search".into(), &json!({"pattern": "x"}));
        assert_eq!(t.committable_upto(), 2);
    }

    // --- presentment -----------------------------------------------------------

    // A first-class Diff item as the Diff extension's Presenter would emit.
    fn diff_item(name: &str) -> TranscriptItem {
        TranscriptItem::Diff {
            title: format!("diff {name}"),
            lang: None,
            hunks: vec![DiffHunk {
                header: None,
                lines: vec![DiffLine::new(DiffSide::Added, "new line")],
            }],
            elided: 0,
        }
    }

    struct DiffPresenter;
    impl Presenter for DiffPresenter {
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
                } if artifacts.contains_key("diff") => diff_item(name),
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
        let mut t = Transcript::new(vec![reg("DiffPresenter", Box::new(DiffPresenter))]);
        t.tool_result(
            "t1",
            "edit".into(),
            "edited x",
            false,
            &diff_artifacts(),
        );
        assert_eq!(t.items(), vec![diff_item("edit")]);
    }

    #[test]
    fn without_matching_artifact_default_summary_survives() {
        let mut t = Transcript::new(vec![reg("DiffPresenter", Box::new(DiffPresenter))]);
        t.tool_result("t1", "edit".into(), "edited x", false, &HashMap::new());
        assert_eq!(
            t.items(),
            vec![tool_result_item("edit", "edited x", false)]
        );
    }

    #[test]
    fn tool_call_items_pass_through_present() {
        let mut t = Transcript::new(vec![reg("DiffPresenter", Box::new(DiffPresenter))]);
        t.tool_call("t1".into(), "grep_search".into(), &json!({}));
        assert_eq!(t.items(), vec![tool_call_item("t1", "grep_search", "")]);
    }

    #[test]
    fn crashing_present_falls_back_to_default_with_info_line() {
        let mut t = Transcript::new(vec![reg("PresentCrasher", Box::new(PresentCrasher))]);
        t.tool_result(
            "t1",
            "edit".into(),
            "edited x",
            false,
            &diff_artifacts(),
        );
        let items = t.items();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], tool_result_item("edit", "edited x", false));
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

    // The diff redundancy case: because the paired call is removed, the Diff
    // extension's Diff item (whose title summarizes the call) stands alone.
    #[test]
    fn a_diff_stands_alone_after_the_paired_call_is_removed() {
        let mut t = Transcript::new(vec![reg("DiffPresenter", Box::new(DiffPresenter))]);
        t.tool_call(
            "t1".into(),
            "edit".into(),
            &json!({"path": "src/x.rs"}),
        );
        t.tool_result("t1", "edit".into(), "edited", false, &diff_artifacts());
        // Only the Diff remains - the redundant call line is gone.
        assert_eq!(t.items(), vec![diff_item("edit")]);
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
                "header",
                Box::new(|t| t.header("suspenders", "0.1.0", "p/m", "~/x", "tip")),
            ),
            (
                "marker",
                Box::new(|t| t.marker("✂ evicted 3 stale results", Tone::Housekeeping)),
            ),
            ("user", Box::new(|t| t.user("hello"))),
            ("push", Box::new(|t| t.push(diff_item("edit")))),
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
                Box::new(|t| t.tool_call("t1".into(), "grep_search".into(), &json!({"pattern": "x"}))),
            ),
            (
                "tool_result",
                Box::new(|t| t.tool_result("t1", "grep_search".into(), "hit", false, &HashMap::new())),
            ),
            (
                "extension_failure",
                Box::new(|t| t.extension_failure("P", Stage::Present, "boom")),
            ),
            // mark_committed moves neither items nor revision (ADR-0046): it is
            // enrolled with a "neither" expectation - the prefix half of the
            // property holds vacuously (items are identical), and it must not
            // bump. Runs after a couple of appends so there is something to
            // commit.
            ("mark_committed", Box::new(|t| t.mark_committed(1))),
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

    // COMMITTED IMMUTABILITY (ADR-0046): once the high-water mark covers a
    // prefix, NO verb may ever change an item in `items[0..committed]`. Frozen
    // scrollback is written once and never redrawn, so a structural edit that
    // touched a committed item would silently diverge the live pending region
    // from what is already on screen. The Screen never marks a non-terminal item
    // committed (a `ToolCall`/`Steering` marker is excluded by `committable_upto`
    // and is the only thing structural verbs remove), so this holds - but it is a
    // load-bearing invariant, so pin it as a property over every verb.
    #[test]
    fn no_committed_item_is_ever_removed_or_changed_by_any_verb() {
        type Step = (&'static str, Box<dyn FnOnce(&mut Transcript)>);
        // The same verb list as the revision property, minus the ones that
        // append a NON-terminal item the mark would (correctly) never cover; each
        // runs against a transcript whose committed prefix is exactly its
        // terminal leading items.
        let steps: Vec<Step> = vec![
            ("info", Box::new(|t| t.info("news"))),
            (
                "header",
                Box::new(|t| t.header("suspenders", "0.1.0", "p/m", "~/x", "tip")),
            ),
            ("user", Box::new(|t| t.user("hello"))),
            ("push", Box::new(|t| t.push(diff_item("edit")))),
            ("message_start", Box::new(|t| t.message_start())),
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
                Box::new(|t| t.tool_call("t1".into(), "grep_search".into(), &json!({"pattern": "x"}))),
            ),
            (
                "tool_result",
                Box::new(|t| t.tool_result("t1", "grep_search".into(), "hit", false, &HashMap::new())),
            ),
            (
                "extension_failure",
                Box::new(|t| t.extension_failure("P", Stage::Present, "boom")),
            ),
            (
                "close",
                Box::new(|t| t.close(Some("turn cancelled".into()))),
            ),
            ("discard_streaming", Box::new(|t| t.discard_streaming())),
        ];

        for (name, step) in steps {
            let mut t = fresh();
            // Seed a couple of appends and commit everything terminal, so the
            // committed prefix is non-empty before the verb under test runs.
            t.info("committed A");
            t.user("committed B");
            let committed = t.committable_upto();
            t.mark_committed(committed);
            let frozen = t.items()[..committed].to_vec();
            assert!(!frozen.is_empty(), "{name}: seeded a committed prefix");

            step(&mut t);

            // The committed prefix is byte-for-byte identical after the verb: no
            // reorder, no supersede, no removal ever touches frozen items.
            assert_eq!(
                &t.items()[..committed],
                &frozen[..],
                "{name} changed a COMMITTED item - frozen scrollback would diverge"
            );
            // And the mark itself never regresses.
            assert!(
                t.committed_high_water() >= committed,
                "{name} regressed the high-water mark"
            );
        }
    }

    // --- markers -----------------------------------------------------------------

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

    // --- latest_todo (ADR-0048: the sticky box's single source of truth) ---------

    fn todo_item(contents: &[&str]) -> TranscriptItem {
        TranscriptItem::Todo {
            items: contents
                .iter()
                .map(|c| crate::plan::TodoItem {
                    content: (*c).into(),
                    status: crate::plan::TodoStatus::Pending,
                })
                .collect(),
        }
    }

    #[test]
    fn latest_todo_returns_the_newest_todo_with_its_index() {
        let mut t = fresh();
        assert_eq!(t.latest_todo(), None, "no todo yet");

        t.user("do it");
        t.push(todo_item(&["read", "edit"]));
        t.push(info("working"));
        // A later todo_write supersedes the earlier list (a fresh append, not a
        // structural edit) - latest_todo returns the NEWEST one and its index.
        t.push(todo_item(&["read", "edit", "ship"]));

        let (idx, items) = t.latest_todo().expect("a todo is on screen");
        assert_eq!(idx, 3, "the newest todo item's index");
        assert_eq!(items.len(), 3);
        assert_eq!(items[2].content, "ship");
    }

    // --- compact_toggle_has_visual_effect (ADR-0052) -----------------------------

    // An empty transcript, and one of only User/Info items, has nothing compact
    // hides in the COMMITTED prefix -> no visual effect (the Ctrl+O handler skips
    // the scrollback redraw).
    #[test]
    fn compact_toggle_is_inert_for_an_empty_or_plain_committed_prefix() {
        let mut t = fresh();
        assert!(!t.compact_toggle_has_visual_effect(), "empty");

        t.info("a");
        t.user("hi");
        t.push(assistant("an answer"));
        t.mark_committed(t.committable_upto());
        assert!(
            !t.compact_toggle_has_visual_effect(),
            "only plain items committed (Info / User / Assistant are not compact-affected)"
        );
    }

    // A committed Thinking / tool-group member DOES change under compact, so the
    // predicate is true once one is frozen.
    #[test]
    fn compact_toggle_has_effect_for_committed_thinking_or_tool_items() {
        for item in [
            thinking("hmm"),
            tool_call_item("id1", "grep_search", "hit"),
            tool_result_item("grep_search", "hit", false),
            diff_item("edit"),
            todo_item(&["read"]),
        ] {
            let mut t = fresh();
            t.push(item);
            // A bare `ToolCall` is not terminal (`committable_upto` stops at it),
            // so force the mark past it - the predicate reads `items[..committed]`
            // regardless of terminality, and a committed ToolCall header is
            // compact-affected (it is a tool-group member the predicate lists).
            t.mark_committed(t.committable_upto().max(1));
            assert!(
                t.compact_toggle_has_visual_effect(),
                "a committed compact-affected item has a visual effect"
            );
        }
    }

    // The predicate reads the COMMITTED prefix only: a Thinking item still in the
    // pending region (uncommitted) has no scrollback effect - the pending body
    // redraws at the new compact for free.
    #[test]
    fn compact_toggle_ignores_uncommitted_items() {
        let mut t = fresh();
        t.push(thinking("pending thought"));
        // Nothing committed yet.
        assert_eq!(t.committed_high_water(), 0);
        assert!(
            !t.compact_toggle_has_visual_effect(),
            "an uncommitted Thinking item does not need a scrollback redraw"
        );
    }

    // --- thought_subject (the spinner's rolling reasoning head) ------------------
    // The pure parse ladder is tested in the `thought` child module; here we pin
    // only the store read that threads the streaming snapshot through it.

    // The store read threads the streaming snapshot through: subject shows
    // mid-message, then empties to None between messages (clear-timing is free).
    #[test]
    fn thought_subject_reads_the_live_snapshot_and_clears_between_messages() {
        let mut t = fresh();
        assert_eq!(t.thought_subject(), None, "idle: no reasoning");
        t.message_start();
        t.message_update(vec![thinking_block("**Planning** the next move")]);
        assert_eq!(t.thought_subject(), Some("Planning".to_string()));
        // Settling the message empties the streaming snapshot; a new message
        // starts from nothing, so the subject clears with no manual reset.
        t.message_end(&[text_block("done")]);
        assert_eq!(t.thought_subject(), None);
    }

    // --- item vocabulary ---------------------------------------------------------

    // The predicate and its title travel together in the pure core (S1): a
    // non-empty Diff has both; everything else (empty Diff, one-line result)
    // has neither, so the view never re-implements the fold rule.
    #[test]
    fn foldable_body_and_fold_title_agree_on_what_collapses() {
        let diff = TranscriptItem::Diff {
            title: "edit x (+1 -1)".into(),
            lang: None,
            hunks: vec![DiffHunk {
                header: None,
                lines: vec![DiffLine::new(DiffSide::Added, "a")],
            }],
            elided: 0,
        };
        assert!(diff.has_foldable_body());
        assert_eq!(diff.fold_title(), Some("edit x (+1 -1)"));

        // A Diff with no hunk lines: no body to fold. (It still HAS a title, but
        // the view gates on `has_foldable_body()`, so it never collapses.)
        let empty = TranscriptItem::Diff {
            title: "empty".into(),
            lang: None,
            hunks: vec![],
            elided: 0,
        };
        assert!(!empty.has_foldable_body());

        // A merged one-line ToolResult: neither.
        let result = TranscriptItem::ToolResult {
            name: "read_file".into(),
            summary: "340 lines".into(),
            is_error: false,
            key_arg: Some("src/foo.rs".into()),
        };
        assert!(!result.has_foldable_body());
        assert_eq!(result.fold_title(), None);
    }
}
