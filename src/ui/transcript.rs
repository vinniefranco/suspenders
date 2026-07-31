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
                    // Infallible by construction: Locator::ToolCall's rposition
                    // predicate only matches TranscriptItem::ToolCall items, so
                    // `removed` is always a ToolCall when it is Some.
                    other => unreachable!("Locator::ToolCall matched non-ToolCall item: {other:?}"),
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

    /// The spinner's thought subject RESTRICTED to a DISTINCT bold `**subject**`
    /// (no last-line head fallback). The pending body uses this when the live
    /// `✦ Thinking` tail is on screen (non-compact): the tail already shows the
    /// reasoning head, so [`thought_subject`](Self::thought_subject)'s head
    /// fallback would duplicate it onto the spinner line. `None` -> the spinner
    /// shows the lull phrase instead of echoing the tail.
    pub fn thought_subject_bold(&self) -> Option<String> {
        thought::bold_subject_of(&self.streaming.thinking())
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
    format!("plugin {extension} failed in {stage}: {message}")
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
#[path = "../../tests/unit/ui/transcript.rs"]
mod tests;
