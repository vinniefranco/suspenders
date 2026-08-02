//! The Composer (CONTEXT.md) - the input area where the user authors the next
//! prompt - deepened into its own pure module (ADR-0034): the draft and its
//! CHAR-index cursor, the Slash Command menu cursor (ADR-0032), the selector
//! overlay (ADR-0033), and the prompt-history ring, plus every rule about
//! them. No terminal, no async, no IO (ADR-0001/0019): the fold root offers
//! keys and events in, [`Effect`]s come out.
//!
//! ## First refusal
//!
//! The Composer does not own the keyboard - it gets FIRST REFUSAL on it.
//! [`Composer::handle_key`] folds a key and returns [`KeyOutcome::Consumed`],
//! or hands the key back untouched as [`KeyOutcome::Refused`] for the
//! caller's own arms (scroll, display toggles, Escape-as-Cancellation).
//! Contextual ownership therefore lives in ONE place: the wheel navigates an
//! open overlay here but scrolls the viewport there; Escape closes an open
//! overlay here but cancels the Run there. [`Composer::apply_event`] is the
//! same shape over events - the overlay-filling deliveries are consumed
//! (stale ones absorbed), everything else refused untouched - so a future
//! overlay fed by a new event slots in without a new arm in the fold root.
//!
//! ## Overlays are Composer states
//!
//! Both popups - the Slash Command menu and a committed command's selector -
//! are DRAFT-DERIVED states of the Composer, not modals (the Approval modal
//! is the only modal, and it is gated ABOVE this module): the draft is the
//! one filter, backspacing out of one sub-state re-enters the other, and
//! closing an overlay empties the Composer. The render adapter reads them
//! through [`Composer::view`] as [`OverlayView`] - one enum, one render
//! match, so a third overlay kind is a new variant here, never a fold-root
//! change.
//!
//! ## Layout math
//!
//! The module's second half is the pure wrapping and cursor-position math
//! behind the growing Composer box ([`layout`], [`max_visible_rows`],
//! [`first_visible_row`]). It stays a free function over `(draft, cursor,
//! width)` - the render adapter owns the width, so terminal geometry never
//! enters [`Composer::view`]. The wrapping is CHAR-based, not word-based, on
//! purpose: the view places a REAL terminal cursor
//! (`frame.set_cursor_position`) at the exact cell of the draft cursor,
//! which needs row/column math the renderer can reproduce exactly -
//! `Paragraph`'s word-wrap points cannot be queried cheaply. Char-per-cell
//! is also how the rest of the codebase measures text.
//!
//! The layout contract:
//!
//! * **Rows** are the draft split on hard '\n', each hard line then chunked
//!   into `width`-char rows. A hard line whose length is an exact multiple of
//!   `width` (the empty line included) yields one EXTRA empty row - the cell
//!   the cursor occupies at that line's end, exactly like a terminal that has
//!   just wrapped. So `cursor_row/cursor_col` are total functions of the
//!   cursor: `offset / width` and `offset % width` within the hard line.
//! * **Every cursor position is a real cell**: `cursor_col < width` always,
//!   so the view never places the terminal cursor outside the Composer.
//! * **Height is capped** at `min(8, terminal_height / 3)` rows - never
//!   below one - so a tall draft never starves the transcript viewport; when
//!   the draft overflows the cap, [`first_visible_row`] scrolls the Composer
//!   internally so the cursor row stays visible, pinned to the BOTTOM of the
//!   box like a terminal.
//!
//! The LOGICAL line/column of the cursor has ONE owner, `ui::draft`, shared
//! by the edit fold and the layout math, so the cell the user edits and the
//! cell the view paints can never drift apart.

use crate::event::{Event, FileSuggestion};
use crate::ui::completion::{self, Completion, Suggestion};
use crate::ui::draft;
use crate::ui::history::History;
use crate::ui::mcp_command::{self, McpDialog, McpDialogView, McpFold, McpKey};
use crate::ui::screen::{AgentCommand, Effect, Key, Status, UngatedKey};
use crate::ui::selection::{Millis, SelectionKey, SelectionList, SelectionOutcome};
use crate::ui::slash;
use crate::view_model::SelectorRow;

/// How the Composer answered an offered key (first refusal, ADR-0034).
#[must_use = "a dropped KeyOutcome is a dropped key: fold the effects, or fold the refused key"]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOutcome {
    /// The Composer folded the key; run `effects` and surface `notice` (one
    /// info line for the transcript - today only the unknown-command line).
    /// `effects` may be empty: a consumed no-op (Enter on a blank draft) is
    /// still consumed, and the caller must not re-fold the key.
    Consumed {
        effects: Vec<Effect>,
        notice: Option<String>,
    },
    /// Not mine: the SAME key, returned by value so the caller's own arms
    /// fold it - no clone, no chance of matching a different key.
    Refused(Key),
}

/// How the Composer answered an offered event (the same first-refusal shape
/// as [`KeyOutcome`]). `Consumed` carries effects for uniformity - the
/// selector fills emit none today, but an overlay that answers an event with
/// an Effect needs no new outcome shape. (No `Eq` only because [`Event`]
/// carries floats; otherwise derived like [`KeyOutcome`].)
#[must_use = "a dropped EventOutcome is a dropped event: fold the effects, or fold the refused event"]
#[derive(Debug, Clone, PartialEq)]
pub enum EventOutcome {
    Consumed(Vec<Effect>),
    Refused(Event),
}

/// The lifecycle of a committed selector-opening command's row list
/// (ADR-0033, ADR-0051 System A). `Idle` is the NULL-OBJECT state - no command
/// has committed a selector, so the always-present [`CommandSelector`] carries
/// nothing drawable and every fold is a no-op; `Loading` after commit while the
/// adapter fetches; `Ready` once [`Event::SelectorReady`] delivered rows into a
/// [`DialogList`]; `Failed` on [`Event::SelectorFailed`]. Only `Ready` accepts
/// navigation/selection - an `Idle`/`Loading`/`Failed` overlay swallows Enter.
/// Private: the view exposes the lifecycle as [`OverlayStatus`], which never
/// carries the owned [`DialogList`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum DialogStatus {
    /// No selector open (the Null-Object rest state). `command`/`generation`/
    /// `filter_mode` on the enclosing [`CommandSelector`] are stale and unread.
    Idle,
    Loading,
    Ready(DialogList),
    Failed(String),
}

/// The selector overlay's lifecycle as render needs it (ADR-0033): `Loading`
/// and `Failed` draw a one-line status, `Ready` draws the rows beside it. So
/// `Ready` carries no payload - the `rows`/`active` in [`OverlayView::Dialog`]
/// are the whole drawable surface, and the owned [`DialogList`] never crosses
/// the seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayStatus {
    Loading,
    Ready,
    Failed(String),
}

/// Whether a committed command's DIALOG (System A) carries an editable fuzzy
/// filter (ADR-0051). The model dialog does (`Filtered` - suspenders surfaces
/// hundreds of catalog models, a deliberate divergence from qwen's filter-less
/// dialog); the theme dialog does not (`Frozen` - few themes, qwen-faithful).
/// A `Frozen` dialog swallows editing keys, so the normalized `/<name> ` draft
/// never grows past the trailing space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DialogFilter {
    /// Editable filter (the model dialog): typed chars after `/<name> ` narrow
    /// the rows via a case-insensitive SUBSTRING match over the row label (NOT
    /// nucleo - whole-group retention needs the header/note travel that a
    /// substring test gives, see [`filter_rows`]).
    Filtered,
    /// No filter (the theme dialog): editing keys are swallowed.
    Frozen,
}

/// A committed command's numbered `›` DIALOG rows (ADR-0051 System A): the raw
/// [`SelectorRow`]s from the fetch, the currently VISIBLE (optionally
/// fuzzy-filtered) rows, and a [`SelectionList`] navigating them over a
/// disabled mask (headers/greyed/broken rows are disabled). The list is rebuilt
/// whenever the visible set changes (a filter keystroke). `filter` is the
/// last-applied filter string, kept so a redundant rebuild is skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DialogList {
    /// The unfiltered rows from the fetch, in listing order.
    raw: Vec<SelectorRow>,
    /// The rows currently shown (all of `raw` when `Frozen` or the filter is
    /// empty; a fuzzy-narrowed subset otherwise).
    visible: Vec<SelectorRow>,
    /// Navigation over `visible`, disabled mask = non-cursor-stop rows.
    list: SelectionList,
    /// Whether an editable filter narrows the rows (model) or not (theme).
    mode: DialogFilter,
    /// The last filter applied to produce `visible` (empty for `Frozen`).
    filter: String,
}

impl DialogList {
    // Builds a dialog over `raw` rows in the given filter mode, with the empty
    // filter (every row visible) and the active row snapped onto the first
    // navigable (cursor-stop) row.
    fn new(raw: Vec<SelectorRow>, mode: DialogFilter) -> Self {
        let visible = raw.clone();
        let list = Self::list_for(&visible);
        DialogList {
            raw,
            visible,
            list,
            mode,
            filter: String::new(),
        }
    }

    // A [`SelectionList`] over `visible`: a row is disabled (unnavigable) when
    // it is not a cursor stop (headers and greyed collapsed rows), matching the
    // Selector's old stop rules. Notes stay navigable (Enter refuses them).
    fn list_for(visible: &[SelectorRow]) -> SelectionList {
        let disabled: Vec<bool> = visible.iter().map(|r| !r.is_stop()).collect();
        SelectionList::with_active(disabled, 0)
    }

    // Applies `filter` (the draft `rest`), rebuilding the visible rows and the
    // navigation list only when it changed. A `Frozen` dialog ignores the
    // filter entirely. Filtering keeps a group-agnostic fuzzy match over the
    // row LABEL (the nucleo matcher, confined to completion.rs is not reused
    // here - the dialog needs whole-row retention including headers/notes, so a
    // simple case-insensitive substring over the visible label is used, the
    // same test qwen's dialog filter would apply were it present).
    fn refilter(&mut self, filter: &str) {
        if let Some(next) = self.rebuilt_for(filter) {
            *self = next;
        }
    }

    // The fresh dialog `filter` narrows this to, or `None` when the rebuild is a
    // no-op that must NOT disturb the current navigation - a `Frozen` dialog
    // (never filters) or an unchanged filter. A single `.then` combinator over
    // the pure [`DialogList::rebuilds_for`] predicate (Operation → the call-only
    // recipe [`DialogList::over`]); no `if`/`match` interleaved with the call.
    fn rebuilt_for(&self, filter: &str) -> Option<DialogList> {
        self.rebuilds_for(filter)
            .then(|| Self::over(self.raw.clone(), self.mode, filter))
    }

    // Whether a `refilter(filter)` rebuilds: NOT a `Frozen` dialog (which never
    // filters) and NOT an unchanged filter (which needs no rebuild). Pure
    // predicate.
    fn rebuilds_for(&self, filter: &str) -> bool {
        self.mode != DialogFilter::Frozen && filter != self.filter
    }

    // A dialog over `raw` in `mode`, narrowed to `filter` (call-only recipe):
    // the visible rows, a fresh navigation list, the applied filter. The
    // `filter`-carrying twin of [`DialogList::new`] (which is the empty
    // filter).
    fn over(raw: Vec<SelectorRow>, mode: DialogFilter, filter: &str) -> DialogList {
        let visible = filter_rows(&raw, filter);
        let list = Self::list_for(&visible);
        DialogList {
            raw,
            visible,
            list,
            mode,
            filter: filter.to_string(),
        }
    }

    // The active (highlighted) visible row, if any.
    fn active_row(&self) -> Option<&SelectorRow> {
        self.visible.get(self.list.active())
    }
}

// A group-aware-ish filter for the model DIALOG (ADR-0051 divergence): keep a
// header when any row in its group matches, keep a matching member/collapsed
// row, keep a group's trailing note when the group matched. A case-insensitive
// substring test over the label - the same shape the retired Selector used,
// minus the reveal cap (the numbered dialog scrolls instead of collapsing).
fn filter_rows(raw: &[SelectorRow], filter: &str) -> Vec<SelectorRow> {
    if filter.is_empty() {
        return raw.to_vec();
    }
    use crate::view_model::RowRole;
    let needle = filter.to_lowercase();
    let hits = |row: &SelectorRow| row.label.to_lowercase().contains(&needle);
    let mut out = Vec::new();
    let mut start = 0;
    while start < raw.len() {
        let end = if raw[start].role == RowRole::Header {
            raw[start + 1..]
                .iter()
                .position(|r| r.role == RowRole::Header)
                .map(|off| start + 1 + off)
                .unwrap_or(raw.len())
        } else {
            start + 1
        };
        let group = &raw[start..end];
        let group_hit = group.iter().any(&hits);
        for row in group {
            let keep = match row.role {
                RowRole::Header | RowRole::Note => group_hit,
                RowRole::Member | RowRole::Collapsed => hits(row),
            };
            if keep {
                out.push(row.clone());
            }
        }
        start = end;
    }
    out
}

// The owned command DIALOG overlay (ADR-0033, ADR-0051 System A), an
// ALWAYS-PRESENT Null Object: `Idle` is the closed rest state, so the Composer
// never juggles an `Option` and every call site is an unconditional
// delegation (IOSP - no Option-guard-then-delegate). `command` is the opaque
// command name carried back out on selection; `status` is the row list's
// lifecycle; `generation` is the activation counter this overlay was opened
// with - the fill events must echo it back or [`Composer::apply_event`] drops
// them. Private: the view exposes it as [`OverlayView::Dialog`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandSelector {
    command: String,
    status: DialogStatus,
    generation: u64,
    /// The filter mode the fill will build the [`DialogList`] with, decided at
    /// commit time from the command (model = `Filtered`, theme = `Frozen`).
    filter_mode: DialogFilter,
}

impl CommandSelector {
    // The closed Null-Object selector: `Idle`, carrying no command. The stale
    // `command`/`filter_mode`/`generation` are never read while `Idle`.
    fn closed() -> Self {
        CommandSelector {
            command: String::new(),
            status: DialogStatus::Idle,
            generation: 0,
            filter_mode: DialogFilter::Frozen,
        }
    }

    // Opens this selector on a fresh activation: a `Loading` overlay for
    // `command` stamped with `generation`, fetching in `filter_mode`. Replaces
    // whatever state (Idle or a leftover) was here.
    fn open(&mut self, command: String, generation: u64, filter_mode: DialogFilter) {
        *self = CommandSelector {
            command,
            status: DialogStatus::Loading,
            generation,
            filter_mode,
        };
    }

    // Returns this selector to the closed Null-Object state.
    fn close(&mut self) {
        *self = Self::closed();
    }

    // The open Ready dialog, if this overlay has fetched its rows.
    fn ready(&mut self) -> Option<&mut DialogList> {
        match &mut self.status {
            DialogStatus::Ready(dialog) => Some(dialog),
            _ => None,
        }
    }

    // The open Ready dialog by shared ref (the view side).
    fn ready_ref(&self) -> Option<&DialogList> {
        match &self.status {
            DialogStatus::Ready(dialog) => Some(dialog),
            _ => None,
        }
    }

    // Re-narrows the Ready dialog's rows to `filter` (a no-op when `Idle`,
    // before the fetch, or for a `Frozen` dialog). Unconditional at the call
    // site: the state-branch lives here, in the one owner.
    fn refilter(&mut self, filter: &str) {
        if let Some(dialog) = self.ready() {
            dialog.refilter(filter);
        }
    }

    // Flips a `Loading` overlay to `Ready` over a fresh dialog seeded with
    // `filter` - but only for `generation` (the activation guard) - else a
    // no-op. The whole generation/Loading branch lives HERE so the caller is a
    // single unconditional call (IOSP).
    fn fill_ready(&mut self, generation: u64, rows: Vec<SelectorRow>, filter: &str) {
        if self.is_loading_for(generation) {
            let mut dialog = DialogList::new(rows, self.filter_mode);
            dialog.refilter(filter);
            self.status = DialogStatus::Ready(dialog);
        }
    }

    // Flips a `Loading` overlay to `Failed` - same activation guard as
    // [`CommandSelector::fill_ready`], else a no-op.
    fn fill_failed(&mut self, generation: u64, message: String) {
        if self.is_loading_for(generation) {
            self.status = DialogStatus::Failed(message);
        }
    }

    // Whether this overlay is `Loading` for exactly `generation` - the fill
    // guard shared by the two `fill_*` transitions (pure predicate).
    fn is_loading_for(&self, generation: u64) -> bool {
        matches!(self.status, DialogStatus::Loading) && self.generation == generation
    }

    // Folds a [`SelectionKey`] onto the Ready dialog's numbered rows, returning
    // the chosen row's `SelectorChosen` effect on a pickable selection (nav and
    // non-pick outcomes return `None`). An Idle/Loading/Failed overlay ignores
    // it.
    fn fold_selection(&mut self, key: SelectionKey) -> Option<Effect> {
        let command = self.command.clone();
        let dialog = self.ready()?;
        match dialog.list.handle(key, NOW_UNUSED) {
            SelectionOutcome::Selected(i) => {
                dialog
                    .visible
                    .get(i)
                    .filter(|r| r.pickable())
                    .map(|r| Effect::SelectorChosen {
                        command,
                        value: r.value.clone(),
                    })
            }
            _ => None,
        }
    }

    // Whether this is a Ready `Frozen` (theme) dialog - digits are quick-select
    // and editing chars are swallowed.
    fn is_ready_frozen(&self) -> bool {
        matches!(&self.status, DialogStatus::Ready(d) if d.mode == DialogFilter::Frozen)
    }

    // The render projection for this overlay (System A), the compute-plan
    // behind [`Composer::dialog_view`]: the command label, the drawable
    // [`OverlayStatus`], and (only when Ready) the visible rows, active index,
    // and the active row's detail hint. `Idle` renders as a `Loading`
    // placeholder for `fallback_command` (a fresh `/model ` before the next
    // fold activates the overlay) - the ONE place the Null-Object rest state
    // maps back onto a drawable frame.
    fn view_parts(&self, fallback_command: &str) -> DialogParts {
        match &self.status {
            DialogStatus::Ready(dialog) => DialogParts {
                command: self.command.clone(),
                status: OverlayStatus::Ready,
                rows: dialog.visible.clone(),
                active: dialog.list.active(),
                detail: dialog.active_row().and_then(|r| r.hint.clone()),
            },
            DialogStatus::Loading => {
                DialogParts::status(self.command.clone(), OverlayStatus::Loading)
            }
            DialogStatus::Failed(message) => {
                DialogParts::status(self.command.clone(), OverlayStatus::Failed(message.clone()))
            }
            DialogStatus::Idle => {
                DialogParts::status(fallback_command.to_string(), OverlayStatus::Loading)
            }
        }
    }
}

// The render projection of a [`CommandSelector`] (the compute-plan / parameter
// object behind [`Composer::dialog_view`]): the fields [`OverlayView::Dialog`]
// draws. Built by [`CommandSelector::view_parts`] so the view fn is a call-only
// assembler (IOSP).
struct DialogParts {
    command: String,
    status: OverlayStatus,
    rows: Vec<SelectorRow>,
    active: usize,
    detail: Option<String>,
}

impl DialogParts {
    // A row-less status frame (Loading/Failed, or the Idle placeholder): no
    // visible rows, the active index at 0, no detail.
    fn status(command: String, status: OverlayStatus) -> Self {
        DialogParts {
            command,
            status,
            rows: Vec::new(),
            active: 0,
            detail: None,
        }
    }
}

/// The `@path` file picker's overlay state (Phase C2, qwen `useAtCompletion`),
/// an ALWAYS-PRESENT Null Object like [`CommandSelector`]: `Closed` is the rest
/// state, so the Composer never juggles an `Option` and the fill/nav are
/// unconditional delegations. Opened lazily from the draft (the AT context is
/// draft-derived, not committed), it stores the async-fetched rows guarded by
/// `generation` + `query`, and navigates them with a reused [`Completion`].
///
/// Unlike the selector (opened once on commit), the AT fetch fires PER
/// KEYSTROKE: each pattern change bumps `generation` and emits a fresh
/// [`Effect::FileSearch`], and only a [`Event::FileSearchReady`] echoing the
/// live generation AND query is folded (a stale keystroke's result is dropped).
#[derive(Debug, Clone, PartialEq, Eq)]
struct AtFiles {
    /// The last-fetched suggestions and the query they were fetched for. `None`
    /// until the first fill lands (the popup shows "searching…" until then).
    fetched: Option<AtFetched>,
    /// The highlight + scroll window over the fetched rows (reused palette nav).
    nav: Completion,
    /// The AT activation counter, bumped on each pattern change; stamped on the
    /// emitted [`Effect::FileSearch`] and echoed by the fill.
    generation: u64,
    /// The pattern the last [`Effect::FileSearch`] was emitted for, so a
    /// no-change keystroke (a cursor move that leaves the pattern intact) does
    /// not re-fetch, and the fill's query guard has something to match against.
    requested_query: Option<String>,
    /// Whether the user dismissed the popup with Esc (keeping the draft). Reset
    /// when the pattern next changes, so typing re-opens it.
    dismissed: bool,
}

/// The rows a [`Event::FileSearchReady`] delivered and the query they answer,
/// stored so a later fill for a DIFFERENT query is dropped by the guard.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AtFetched {
    query: String,
    suggestions: Vec<Suggestion>,
}

impl AtFiles {
    // The closed rest state: no fetch, fresh nav, generation 0, nothing
    // requested or dismissed.
    fn closed() -> Self {
        AtFiles {
            fetched: None,
            nav: Completion::new(),
            generation: 0,
            requested_query: None,
            dismissed: false,
        }
    }

    // Returns to the closed rest state (accept, or leaving the AT context).
    fn close(&mut self) {
        *self = Self::closed();
    }

    // The suggestions to render for the live `query`: the fetched rows when they
    // answer exactly this query, else an empty list (a stale/absent fetch shows
    // as "searching…"). Never renders rows fetched for a different pattern.
    fn suggestions_for(&self, query: &str) -> &[Suggestion] {
        match &self.fetched {
            Some(f) if f.query == query => &f.suggestions,
            _ => &[],
        }
    }

    // Whether a fetch for `query` is still outstanding (no rows for it yet), so
    // the view shows a subtle "searching…" line instead of an empty list.
    fn is_loading_for(&self, query: &str) -> bool {
        !matches!(&self.fetched, Some(f) if f.query == query)
    }

    // Re-clamps the nav to the fetched-row count for `query` and returns that
    // count (the shared "sync the highlight to the visible rows" step, so the
    // nav never dangles past a shrunk list). Ties `nav` to `fetched`.
    fn clamp_nav(&mut self, query: &str) -> usize {
        let len = self.suggestions_for(query).len();
        self.nav.clamp(len);
        len
    }

    // Moves the highlight up/down over the fetched rows for `query` (wraps, qwen
    // `useCompletion` nav).
    fn nav_up(&mut self, query: &str) {
        let len = self.suggestions_for(query).len();
        self.nav.up(len);
    }
    fn nav_down(&mut self, query: &str) {
        let len = self.suggestions_for(query).len();
        self.nav.down(len);
    }

    // The highlighted suggestion for `query`, if any - the accept target.
    fn highlighted(&self, query: &str) -> Option<&Suggestion> {
        self.suggestions_for(query).get(self.nav.active())
    }

    // The highlight index + scroll window for the view (re-clamped to the rows).
    fn view_cursor(&self, query: &str) -> (usize, usize) {
        let mut nav = self.nav.clone();
        nav.clamp(self.suggestions_for(query).len());
        (nav.active(), nav.scroll())
    }

    // Opens a fresh search for `query`: bump the activation counter, record the
    // requested pattern (so a later fill's guard matches), un-dismiss the popup
    // (a pattern change re-opens it), and return the new generation to stamp on
    // the emitted effect. The single owner of the generation/requested/dismissed
    // trio, so those flag fields connect to the rest of the struct (cohesion).
    fn request(&mut self, query: String) -> u64 {
        self.generation += 1;
        self.requested_query = Some(query);
        self.dismissed = false;
        self.generation
    }

    // Whether a search for `query` has NOT been requested yet (the entry-search
    // guard: emit one the first time an AT context opens on this pattern).
    fn needs_request(&self, query: &str) -> bool {
        self.requested_query.as_deref() != Some(query)
    }

    // Marks the popup dismissed (Esc): closed until the pattern next changes.
    fn dismiss(&mut self) {
        self.dismissed = true;
    }

    // Whether the popup was dismissed (Esc) and not yet re-opened by a pattern
    // change.
    fn is_dismissed(&self) -> bool {
        self.dismissed
    }

    // Folds a delivered fill: store the rows only when its `generation` AND
    // `query` match the live activation (the staleness guard), then re-clamp the
    // nav to the new row count. A stale fill is a no-op.
    fn fill(&mut self, generation: u64, query: String, suggestions: Vec<Suggestion>) {
        if generation != self.generation || self.requested_query.as_deref() != Some(&query) {
            return;
        }
        let len = suggestions.len();
        self.fetched = Some(AtFetched { query, suggestions });
        self.nav.clamp(len);
    }
}

/// The open Composer overlay for rendering (ADR-0051), one enum the adapter
/// matches once. The TWO systems are DISTINCT variants:
///
/// * `Menu` - System B, the fuzzy `/` palette: color-only suggestions (no `›`,
///   no numbers), the fuzzy match window per row for the inverted highlight,
///   the active index and the scroll window, the query for the highlight.
/// * `Dialog` - System A, a committed command's numbered `›` list: the overlay
///   `status` plus, when `Ready`, the (optionally filtered) rows, the active
///   row, and a detail string (the model context window; empty for theme).
///
/// A new overlay kind is a new variant here plus a render arm - never a
/// fold-root change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayView {
    Menu {
        suggestions: Vec<Suggestion>,
        active: usize,
        scroll: usize,
        query: String,
        /// Whether the active row is expanded (`←/→`, qwen `expandedIndex`): a
        /// long active row (label chars `>= MAX_WIDTH`) shows collapsed with a
        /// ` → ` affordance unless this is `true`, when it shows in full with
        /// ` ← `.
        expanded: bool,
    },
    Dialog {
        command: String,
        status: OverlayStatus,
        rows: Vec<SelectorRow>,
        active: usize,
        detail: Option<String>,
    },
    /// The `@path` file picker (Phase C2, qwen `useAtCompletion`): a fuzzy,
    /// color-only file list rendered like `Menu` (no numbers) but sourced from
    /// the async project walk, not the static registry. Carries the fetched
    /// [`Suggestion`]s (repo-relative path labels with the fuzzy highlight
    /// window, the escaped path as `value`), the active index + scroll window,
    /// the `query` for the highlight, and whether the async fetch is still in
    /// flight with nothing to show yet (`loading` - a subtle "searching…" row).
    AtFiles {
        suggestions: Vec<Suggestion>,
        active: usize,
        scroll: usize,
        query: String,
        loading: bool,
    },
    /// The `/mcp` management dialog (ADR-0065 Phase E, System A): a distinct
    /// navigation-stack overlay, NOT a filterable list. Carries the active step's
    /// whole render surface ([`McpDialogView`] - header + content + footer) the
    /// adapter draws in a bordered box, faithful to qwen's `MCPManagementDialog`.
    /// A new overlay kind is a new variant here plus a render arm - never a
    /// fold-root change.
    McpDialog(McpDialogView),
}

/// Everything render needs from the Composer, in one read: the draft, the
/// CHAR-index cursor (feed both to [`layout`] with the width the view owns),
/// and the open overlay, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerView<'a> {
    pub draft: &'a str,
    /// A CHAR index into `draft` - the codebase counts chars, not bytes.
    pub cursor: usize,
    pub overlay: Option<OverlayView>,
}

/// The Composer's whole state. All fields private: reads go through
/// [`Composer::view`], mutation only through the folds and the
/// submitted/steered outcome hooks, so every rule about the draft stays
/// behind this seam. `PartialEq` is part of the contract: a refusal must
/// leave the Composer bit-identical, and the tests compare whole states to
/// prove it. Only `PartialEq` (no `Eq`): the `/mcp` dialog overlay
/// ([`McpDialog`]) holds fetched server views whose tool `input_schema` is a
/// `serde_json::Value` (`PartialEq`, not `Eq`) - `assert_eq!` needs only
/// `PartialEq`, so the whole-state comparison still holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Composer {
    /// The draft text.
    value: String,
    /// The draft cursor, a CHAR index into `value` (never a byte offset), so
    /// multi-byte input never splits or panics. Always clamped to the draft's
    /// char count.
    cursor: usize,
    /// The fuzzy `/` palette state (ADR-0051 System B): the highlighted
    /// suggestion + the scroll window. Only meaningful while the draft
    /// `is_slash` and no command has committed; the suggestions themselves are
    /// derived on demand ([`Composer::view`]) by ranking the `&'static`
    /// registry against the draft (the query), so this holds just the cursor
    /// and scroll. Re-clamped to the ranked length as typing narrows the list.
    menu: Completion,
    /// The command-selector overlay (ADR-0033), an ALWAYS-PRESENT Null Object:
    /// `Idle` (closed) when no selector-opening command is active. Opened when
    /// a command whose descriptor `opens_selector` is committed (to a
    /// `Loading` overlay), folded to `Ready`/`Failed` by
    /// [`Composer::apply_event`], and closed back to `Idle` on selection,
    /// Escape, or backspacing out of the sub-state. Because it is never
    /// `None`, every fold is an UNCONDITIONAL delegation (no Option-guard) -
    /// the state-branch lives once, inside [`CommandSelector`]'s methods.
    selector: CommandSelector,
    /// The selector activation counter: bumped each time a selector-opening
    /// command commits, stamped onto the overlay and the [`Effect::Command`]
    /// it emits. The fill events echo it back, and only a matching echo fills
    /// the overlay - so a late fill from an earlier activation can never land
    /// on a later one's `Loading` overlay.
    selector_generation: u64,
    /// The prompt-history ring and its Readline-style recall rules. Owned by
    /// [`crate::ui::history`]; navigated from the Up/Down arms of
    /// [`Composer::handle_key`] and appended to by [`Composer::submitted_ok`].
    history: History,
    /// The `@path` file-picker overlay (Phase C2, qwen `useAtCompletion`), an
    /// ALWAYS-PRESENT Null Object like `selector`. Opened lazily whenever the
    /// draft+cursor sit in an AT context ([`at_context`]); holds the async
    /// file-search rows (generation+query guarded), navigated by its own
    /// [`Completion`]. AT takes precedence over the slash overlays.
    at_files: AtFiles,
    /// The `/mcp` management dialog overlay (ADR-0065 Phase E), a distinct
    /// System-A overlay `Option` - NOT a Null Object like `selector`, because it
    /// is opened by committing `/mcp` (not draft-derived) and, once open, OWNS the
    /// keyboard until it closes (its own navigation stack is not a filter over the
    /// draft). Mutually exclusive with an open `selector`: committing `/mcp`
    /// clears the draft (closing any menu/selector) and opens this. `Some` while
    /// the dialog is up, `None` otherwise. Keys route to it with FIRST REFUSAL
    /// above the flat selector.
    mcp_dialog: Option<McpDialog>,
    /// The dynamic command-source layer (ADR-0032/0058): one [`SkillCommand`]
    /// per discovered skill, fed in at launch from the
    /// [`crate::skills::SkillManager`] (the same way history is fed in). The
    /// palette ranks and lookup resolves over the UNION of the built-in
    /// [`slash::COMMANDS`] and this list, so a `/<skill>` command sits in the
    /// same menu as `/model`. The Composer stays command-agnostic: these are
    /// opaque descriptors it ranks/renders, never learning what a skill DOES -
    /// committing one emits a plain [`Effect::Command`] the adapter maps to the
    /// submit-prompt injection.
    skill_commands: Vec<slash::SkillCommand>,
}

impl Composer {
    /// A fresh Composer: empty draft, no overlay, the history ring seeded
    /// with `history` (oldest first, from the on-disk file), and the dynamic
    /// `skill_commands` layer seeded from the discovered skills (ADR-0032/0058),
    /// so `/<skill>` sits in the palette beside the built-ins.
    pub fn new(history: Vec<String>, skill_commands: Vec<slash::SkillCommand>) -> Self {
        Composer {
            value: String::new(),
            cursor: 0,
            menu: Completion::new(),
            selector: CommandSelector::closed(),
            selector_generation: 0,
            history: History::new(history),
            at_files: AtFiles::closed(),
            mcp_dialog: None,
            skill_commands,
        }
    }

    /// Offers one key to the Composer (first refusal, ADR-0034). `status` is
    /// the Agent's status at this fold, so the Composer decides Submit vs
    /// Steer at the keypress; it is a read-only input - the Composer never
    /// flips it. `key` is an [`UngatedKey`]: only the caller's Approval gate
    /// can mint one, so this fold never sees a key the modal should have
    /// swallowed - the Composer needs no modal knowledge.
    ///
    /// The routing contract - the interface the fold root leans on:
    ///
    /// * ALWAYS consumed: chars, Backspace, [`Key::InsertNewline`],
    ///   Left/Right/Home/End, Enter (submit when idle, Steer when running,
    ///   commit inside an overlay - consumed even on a blank draft, as a
    ///   no-op), and Up/Down (edge-triggered history recall from the draft's
    ///   first/last line, cursor movement everywhere else).
    /// * Consumed ONLY while an overlay is open: Escape (closes it, emptying
    ///   the Composer - there is no Run to cancel, the overlay is a Composer
    ///   state) and the wheel (navigates its rows). Refused otherwise, so
    ///   Escape-as-Cancellation and wheel-scroll stay the caller's.
    /// * ALWAYS refused: PageUp/PageDown, the display toggles,
    ///   [`Key::Named`], [`Key::Other`].
    ///
    /// The refusal contract: a Refused key leaves the Composer BIT-IDENTICAL -
    /// refusal never reads a rule against mutated state, and the caller may
    /// treat it as "this fold never happened".
    pub fn handle_key(&mut self, key: UngatedKey, status: Status) -> KeyOutcome {
        let key = key.into_key();
        // The ALWAYS-refused rows of the routing table, decided before any
        // state is touched (the refusal contract above). Every key past this
        // gate is consumed whenever a slash sub-state is open - the overlay
        // arms and the editing fall-through both consume - which is what lets
        // those blocks tidy sub-state eagerly; only Escape and the wheel can
        // still be refused below, and only when no sub-state ran at all.
        if always_refused(&key) {
            return KeyOutcome::Refused(key);
        }

        // The `/mcp` management dialog (ADR-0065 Phase E) gets FIRST REFUSAL above
        // every other overlay: once it is open it OWNS the keyboard (a navigation
        // stack, not a filter over the draft), so no key leaks to the AT/slash
        // forks or the draft behind it. It folds Up/Down/Enter/Escape and swallows
        // everything else (a consumed no-op) - the routing order (Approval ->
        // McpDialog -> CommandSelector/draft -> Screen) lives here, ABOVE the
        // draft-derived overlays.
        if self.mcp_dialog.is_some() {
            return self.mcp_key(key);
        }

        // `@path` file completion (Phase C2, qwen `useAtCompletion`): an `@`
        // before the cursor (before an unescaped space) on the current line
        // opens the file picker, WHATEVER else the draft is - AT takes
        // PRECEDENCE over SLASH (qwen checks AT first), so `@` after a
        // `/command ` still triggers the file search. Checked before the slash
        // fork for exactly that reason. When the popup is not dismissed, arrows
        // navigate it and Enter/Tab accept; every other key falls through to
        // editing so typing re-filters (and re-emits the search).
        if let Some(at) = at_context(&self.value, self.cursor) {
            return self.at_key(key, at, status);
        }
        // The cursor left every AT context (a space, a newline, or backspaced
        // past the `@`): drop any open picker so a later `@` re-opens fresh.
        self.at_files.close();

        // Not in an AT context YET: fold the key through the slash/editing
        // arms, then re-detect AT on the POST-edit draft (qwen derives the AT
        // mode from the updated buffer). Typing the `@` itself, or moving the
        // cursor INTO an existing `@token`, opens a fresh context whose initial
        // search must fire on THIS keypress - so weave that entry search in.
        let outcome = self.handle_key_non_at(key, status);
        self.open_at_after_edit(outcome)
    }

    // -- The `/mcp` management dialog sub-state (ADR-0065 Phase E) --
    // Dispatched by [`Composer::handle_key`] whenever the McpDialog overlay is
    // open; every key is consumed (the dialog owns the keyboard). Up/Down/Enter/
    // Escape fold the navigation stack; every other key is a swallowed no-op. The
    // fold resolves to a pure [`McpFold`]: a Close drops the overlay AND clears
    // the draft (the `/mcp ` draft that opened it lingers otherwise); an Act emits
    // an [`Effect::McpAction`] the adapter runs against the Agent; a bare
    // navigation move is a consumed no-op.
    fn mcp_key(&mut self, key: Key) -> KeyOutcome {
        let Some(dialog) = self.mcp_dialog.as_mut() else {
            return consumed(vec![]);
        };
        let Some(mcp_key) = to_mcp_key(&key) else {
            // A key the dialog does not act on (chars, cursor moves): swallowed,
            // so nothing leaks to the draft behind the open dialog.
            return consumed(vec![]);
        };
        let generation = self.selector_generation;
        match dialog.fold_key(mcp_key) {
            McpFold::None => consumed(vec![]),
            McpFold::Close => {
                self.close_mcp_dialog();
                consumed(vec![])
            }
            McpFold::Act(action, server) => consumed(vec![Effect::McpAction {
                action,
                server,
                generation,
            }]),
            // The AUTHENTICATE step's `c` copy (ADR-0065 Phase E): the adapter
            // writes the OSC52 escape for `url` and reports back with an
            // McpCopyResult that flips the copy-feedback hint.
            McpFold::CopyUrl(url) => consumed(vec![Effect::ClipboardOsc52(url)]),
        }
    }

    // Closes the `/mcp` dialog overlay AND clears the draft: the `/mcp ` draft
    // that committed it must not linger once the dialog closes (they opened
    // together, they close together, like the selector).
    fn close_mcp_dialog(&mut self) {
        self.mcp_dialog = None;
        self.clear();
    }

    // Emits the AT entry search if the POST-edit draft newly sits in an AT
    // context whose pattern hasn't been requested yet (typing `@`, or a cursor
    // move into an `@token`). A no-op when there is no AT context, or when the
    // live pattern was already requested (the AT fork above already handles
    // in-context keystrokes). Woven onto `outcome` so the opening keypress
    // carries both its own effects and the fresh `Effect::FileSearch`.
    fn open_at_after_edit(&mut self, outcome: KeyOutcome) -> KeyOutcome {
        let Some(at) = at_context(&self.value, self.cursor) else {
            return outcome;
        };
        if self.at_files.is_dismissed() || !self.at_files.needs_request(&at.query) {
            return outcome;
        }
        let search = self.request_file_search(at.query);
        let KeyOutcome::Consumed { effects, .. } = search else {
            return outcome;
        };
        prepend_effects(effects, outcome)
    }

    // The slash/editing dispatch, reached only when the draft is NOT (yet) in an
    // AT context. Split from [`Composer::handle_key`] so the AT re-detection can
    // wrap it (the entry-search weave above).
    fn handle_key_non_at(&mut self, key: Key, status: Status) -> KeyOutcome {
        // Slash Command overlay (ADR-0032/0033): a leading `/` opens the popup
        // whatever the Agent is doing (Idle or Running) - a slash draft is
        // NEVER a prompt or Steering. The draft parses into `(name, rest)`;
        // the popup is in one of two sub-states, keyed by whether the command
        // committed:
        //
        //   * COMMAND MENU (`rest = None`, or `name` is not a known
        //     selector-opening command): the palette. Arrows move the
        //     highlight, Enter/space commits, Escape closes; editing keys
        //     fall through so typing filters the menu.
        //   * SELECTOR (`rest = Some` and `name` is a known `opens_selector`
        //     command): the committed command's own value list. Arrows move
        //     within the `rest`-filtered rows, Enter chooses, Escape closes;
        //     editing keys fall through so `rest` keeps filtering.
        //
        // This sits before the submit/steer/edit arms precisely so `/`
        // intercepts Enter and the arrows.
        if slash::is_slash(&self.value) {
            let draft = slash::parse(&self.value);
            let in_selector = draft.rest.is_some()
                && slash::lookup(&draft.name).is_some_and(|c| c.opens_selector);

            return if in_selector {
                // The DIALOG sub-state re-derives the `rest` filter from the
                // draft (via `refilter_from_draft`), so nothing from `draft`
                // crosses into the dispatch.
                self.selector_key(key, status)
            } else {
                self.menu_key(key, &draft.name, status)
            };
        }

        self.editing_key(key, status)
    }

    // -- AT file completion sub-state (Phase C2, qwen `useAtCompletion`) --
    // Dispatched by [`Composer::handle_key`] whenever the draft+cursor sit in an
    // `@path` context; `at` is the detected context (the `@`..pattern span and
    // the pattern). Navigation is the reused palette [`Completion`]; Enter/Tab
    // accept the highlighted path, Esc dismisses (keeping the draft), every
    // editing key falls through so typing re-filters. After an editing key
    // changes the pattern, a fresh [`Effect::FileSearch`] is emitted (the
    // per-keystroke analog of the selector's one-shot fetch).
    fn at_key(&mut self, key: Key, at: AtContext, status: Status) -> KeyOutcome {
        // A dismissed popup (Esc) stays closed until the pattern changes: while
        // dismissed, keys edit normally (no nav, no re-open) - only a pattern
        // change below clears the flag and re-emits the search.
        if self.at_files.is_dismissed() {
            return self.at_edit(key, &at, status);
        }
        // Ensure a search is outstanding for the live pattern: the FIRST time an
        // AT context opens (typing `@`, or moving the cursor into an existing
        // `@token`) no fetch has fired yet, so emit one now (qwen's
        // enabled-effect that searches the pattern on entry). Once requested,
        // `requested_query == query`, so this is a no-op on nav/accept keys.
        let mut effects = Vec::new();
        if self.at_files.needs_request(&at.query)
            && let KeyOutcome::Consumed { effects: e, .. } =
                self.request_file_search(at.query.clone())
        {
            effects = e;
        }
        let len = self.at_files.clamp_nav(&at.query);
        // Fold nav/accept/dismiss/edit; then prepend any entry-fetch effect so a
        // freshly-opened popup requests its rows even on this first keypress.
        let outcome = self.at_key_inner(key, &at, len, status);
        prepend_effects(effects, outcome)
    }

    // The AT key dispatch proper (nav / accept / dismiss / edit), split from
    // [`Composer::at_key`] so the entry-fetch effect can be woven in around it.
    // `len` is the fetched-row count for the pattern (0 disables accept).
    fn at_key_inner(&mut self, key: Key, at: &AtContext, len: usize, status: Status) -> KeyOutcome {
        match key {
            // Arrow-only nav over the fetched rows (wraps, qwen `useCompletion`).
            Key::ArrowUp => {
                self.at_files.nav_up(&at.query);
                consumed(vec![])
            }
            Key::ArrowDown => {
                self.at_files.nav_down(&at.query);
                consumed(vec![])
            }
            // Enter / Tab accept the highlighted path (qwen `handleAutocomplete`
            // / the Enter path). With no rows there is nothing to accept: fall
            // through so Enter still submits/steers a draft that merely contains
            // an unmatched `@token`.
            Key::Enter | Key::Tab if len > 0 => self.at_accept(at),
            // Esc dismisses the popup but KEEPS the draft (unlike the slash menu,
            // which clears): the `@token` stays typed, the popup just closes.
            Key::Escape => {
                self.at_files.dismiss();
                consumed(vec![])
            }
            // Everything else edits, then (if the pattern moved) re-searches.
            other => self.at_edit(other, at, status),
        }
    }

    // An editing key inside an AT context: edit the draft, then - if the pattern
    // changed - request a fresh file search (bumping the generation) and un-dismiss
    // the popup. A cursor move that leaves the pattern intact requests nothing.
    fn at_edit(&mut self, key: Key, before: &AtContext, status: Status) -> KeyOutcome {
        let outcome = self.editing_key(key, status);
        // Re-detect after the edit: the cursor may have left the AT context
        // entirely (backspaced past `@`, typed a space), in which case close.
        match at_context(&self.value, self.cursor) {
            // A pattern CHANGE re-searches (and `request_file_search` clears any
            // dismissal, so typing after Esc re-opens the popup). A dismissed
            // popup that sees no pattern change - a bare cursor move - stays
            // dismissed: Esc sticks until the pattern actually moves.
            Some(after) if after.query != before.query => self.request_file_search(after.query),
            Some(_) => outcome, // same pattern (a cursor move): no re-fetch.
            None => {
                self.at_files.close();
                outcome
            }
        }
    }

    // Bumps the AT generation and emits an [`Effect::FileSearch`] for `query`,
    // recording it as the requested pattern (so the fill's guard matches) and
    // clearing the dismissed flag (a pattern change re-opens the popup). Returns
    // the effect as a consumed outcome - the sole AT effect the composer emits.
    fn request_file_search(&mut self, query: String) -> KeyOutcome {
        let generation = self.at_files.request(query.clone());
        consumed(vec![Effect::FileSearch { query, generation }])
    }

    // Accepts the highlighted AT path: replace the `@`+pattern span (from the
    // `@` through `completion_end`) with `@<escaped-path>` and a trailing space,
    // placing the cursor after the space. Escaping the path's spaces (qwen
    // `escapePath`) keeps [`at_context`]'s unescaped-space rule round-tripping;
    // the trailing space naturally ends the AT context, so the popup closes.
    fn at_accept(&mut self, at: &AtContext) -> KeyOutcome {
        let Some(row) = self.at_files.highlighted(&at.query).cloned() else {
            return consumed(vec![]);
        };
        let chars: Vec<char> = self.value.chars().collect();
        let before: String = chars[..at.at].iter().collect();
        let after: String = chars[at.end..].iter().collect();
        // `row.value` is already the escaped path (the adapter escaped it); build
        // `<before>@<escaped> <after>` and drop the cursor just after the space.
        let inserted = format!("@{} ", row.value);
        self.value = format!("{before}{inserted}{after}");
        self.cursor = before.chars().count() + inserted.chars().count();
        self.at_files.close();
        consumed(vec![])
    }

    // -- DIALOG sub-state (System A, `/model qw` / `/theme`, ADR-0051) --
    // Dispatched by [`Composer::handle_key`] once a selector-opening command
    // committed; `rest` is the draft filter (applied to the model dialog's
    // rows; the theme dialog is `Frozen` and swallows editing keys). Every key
    // offered here is consumed. Navigation is qwen's numbered `›`
    // [`SelectionList`] (arrows skip disabled header/greyed/broken rows,
    // digits quick-select).
    fn selector_key(&mut self, key: Key, status: Status) -> KeyOutcome {
        // Keep the visible rows in step with the draft filter before folding a
        // nav/select key (a `Frozen` theme dialog ignores it).
        self.refilter_from_draft();
        match self.classify_dialog_key(key) {
            DialogKey::Nav(sel_key) => self.dialog_nav(sel_key),
            DialogKey::Pick(sel_key) => self.dialog_pick(sel_key),
            DialogKey::Close => {
                self.close_selector();
                consumed(vec![])
            }
            DialogKey::Swallow => consumed(vec![]),
            DialogKey::Edit(other) => self.dialog_edit(other, status),
        }
    }

    // Classifies a key inside the DIALOG sub-state (System A) into its intent,
    // reading the open dialog's flavour: arrows navigate; a digit on a `Frozen`
    // (theme) dialog quick-selects; Enter picks; Escape closes; an editing char
    // on a `Frozen` dialog is swallowed; everything else edits (filters a model
    // dialog). Pure classification - no mutation.
    fn classify_dialog_key(&self, key: Key) -> DialogKey {
        let frozen = self.selector.is_ready_frozen();
        match key {
            Key::ArrowUp => DialogKey::Nav(SelectionKey::Up),
            Key::ArrowDown => DialogKey::Nav(SelectionKey::Down),
            Key::Char(c) if c.is_ascii_digit() && frozen => {
                DialogKey::Pick(SelectionKey::Digit(c as u8 - b'0'))
            }
            Key::Enter => DialogKey::Pick(SelectionKey::Enter),
            Key::Escape => DialogKey::Close,
            other if frozen && is_text_edit(&other) => DialogKey::Swallow,
            other => DialogKey::Edit(other),
        }
    }

    // Folds a navigation key onto the open dialog (no selection possible).
    fn dialog_nav(&mut self, sel_key: SelectionKey) -> KeyOutcome {
        let _ = self.selector.fold_selection(sel_key);
        consumed(vec![])
    }

    // Folds a selecting key (Enter or a quick-select digit) onto the open
    // dialog; a pickable resolution closes the overlay and emits the effect.
    fn dialog_pick(&mut self, sel_key: SelectionKey) -> KeyOutcome {
        let chosen = self.selector.fold_selection(sel_key);
        match chosen {
            Some(effect) => {
                self.close_selector();
                consumed(vec![effect])
            }
            None => consumed(vec![]),
        }
    }

    // An editing key inside a `Filtered` (model) dialog: edit the draft, then
    // re-narrow the rows against the new `rest` so the next view is fresh.
    fn dialog_edit(&mut self, key: Key, status: Status) -> KeyOutcome {
        let outcome = self.editing_key(key, status);
        self.refilter_from_draft();
        outcome
    }

    // Re-narrows the open dialog's rows to the draft's `rest` filter (the
    // shared parse→rest→refilter dance): parse the draft, take its `rest`
    // (empty when none), and hand it to the open selector. A no-op before the
    // fetch or for a `Frozen` dialog. The single owner of "sync the dialog to
    // the draft filter", called by `selector_key` (before folding a nav/select
    // key) and `dialog_edit` (after an editing key mutated the draft).
    fn refilter_from_draft(&mut self) {
        let rest = slash::parse(&self.value).rest.unwrap_or_default();
        self.selector.refilter(&rest);
    }

    // -- COMMAND MENU sub-state (`/mod`, ADR-0032) --
    // Dispatched by [`Composer::handle_key`] while the command token is still
    // being typed; `name` is the token filtering the palette. Every key
    // reaching this block is CONSUMED (an arm below or the editing
    // fall-through - the refusable keys bounced at the gate), so tidying
    // eagerly is safe under the refusal contract: drop any overlay left over
    // from backspacing out of a selector (the next commit must be a fresh
    // activation, re-emitting Effect::Command) and clamp the highlight to the
    // filtered rows.
    fn menu_key(&mut self, key: Key, name: &str, status: Status) -> KeyOutcome {
        self.selector.close();
        let suggestions = rank_menu(&self.skill_commands, name);
        self.menu.clamp(suggestions.len());
        match key {
            // Arrow-only nav (ADR-0046): no Key::Wheel* is minted into the
            // Composer anymore (mouse capture removed), so the palette moves by
            // the arrows alone. Up/Down WRAP (qwen `useCompletion` nav).
            Key::ArrowUp => {
                self.menu.up(suggestions.len());
                consumed(vec![])
            }
            Key::ArrowDown => {
                self.menu.down(suggestions.len());
                consumed(vec![])
            }
            // `←/→` toggle EXPAND of a long active row (qwen
            // COLLAPSE_SUGGESTION / EXPAND_SUGGESTION, PrepareLabel MAX_WIDTH).
            // Consumed ONLY when the active label is long (>= MAX_WIDTH) - a
            // long row shows collapsed (` → `) until `→` expands it (` ← `) and
            // `←` collapses it again. On a short active row the arrow is NOT
            // ours: it falls through to the editing arms as a plain cursor move
            // (qwen returns false, letting the buffer handle it).
            Key::Right if active_label_is_long(&suggestions, self.menu.active()) => {
                self.menu.expand();
                consumed(vec![])
            }
            Key::Left if active_label_is_long(&suggestions, self.menu.active()) => {
                self.menu.collapse();
                consumed(vec![])
            }
            // Commit the highlighted suggestion. An empty ranked list means the
            // typed query matches no command: surface an unknown-command
            // notice, start no Run, clear the draft.
            Key::Enter => self.commit_command(self.picked_suggestion(&suggestions)),
            // Tab accepts too (qwen `handleAutocomplete`): the same commit as
            // Enter when a suggestion is highlighted.
            Key::Tab if !suggestions.is_empty() => {
                self.commit_command(self.picked_suggestion(&suggestions))
            }
            // Typing a space after a command token also commits it (the palette
            // convention): the space is the query→command boundary, so it
            // commits the highlighted row rather than editing the draft. Only
            // when a suggestion is highlighted.
            Key::Char(' ') if !suggestions.is_empty() => {
                self.commit_command(self.picked_suggestion(&suggestions))
            }
            // Escape closes the palette by clearing the draft - there is no Run
            // to cancel: the palette is a Composer state.
            Key::Escape => {
                self.clear();
                consumed(vec![])
            }
            // Every other key (chars, Backspace, newline, cursor moves) falls
            // through to the editing arms, so typing re-ranks the palette live.
            other => self.editing_key(other, status),
        }
    }

    // The suggestion under the palette cursor, if any (its canonical `value` is
    // the command to commit). `None` when the ranked list is empty.
    fn picked_suggestion(&self, suggestions: &[Suggestion]) -> Option<SelectorRow> {
        suggestions
            .get(self.menu.active())
            .map(|s| SelectorRow::new(s.value.clone(), s.label.clone(), None))
    }

    // -- Editing fall-through --
    // The submit/steer/edit arms: the landing spot for every key that cleared
    // the gate outside a slash sub-state, and for the editing keys the two
    // sub-states above decline (so typing keeps filtering their popups).
    fn editing_key(&mut self, key: Key, status: Status) -> KeyOutcome {
        match key {
            // Trailing-backslash continuation: Enter on a draft whose LAST
            // char is a literal `\` replaces that backslash with a hard
            // newline (cursor to the end) instead of submitting - the
            // fallback for terminals whose Alt-Enter never reaches us.
            // Checked before the submit/steer arm so it applies in both
            // states.
            Key::Enter if self.value.ends_with('\\') => {
                self.value.pop();
                self.value.push('\n');
                self.cursor = self.value.chars().count();
                consumed(vec![])
            }

            // Enter submits when idle, STEERS when running - the Composer
            // never locks. The Status parameter is the whole submit-vs-steer
            // rule; a blank draft is a consumed no-op either way.
            Key::Enter => match self.value.trim() {
                "" => consumed(vec![]),
                text => {
                    let text = text.to_string();
                    let command = match status {
                        Status::Running => AgentCommand::Steer(text),
                        Status::Idle => AgentCommand::Submit(text),
                    };
                    consumed(vec![Effect::Agent(command)])
                }
            },

            // Edge-triggered history: Up on the FIRST hard line of the draft
            // recalls history (the pre-multi-line behavior, draft stashing
            // included); anywhere else it moves the cursor up one line, the
            // column clamped to that line's length. Down mirrors from the
            // LAST line. No goal-column memory - a simple clamp, on purpose.
            Key::ArrowUp => {
                let (line, col) = draft::line_col(&self.value, self.cursor);
                if line == 0 {
                    if let Some(text) = self.history.up(&self.value) {
                        self.recall(text);
                    }
                } else {
                    let clamped = col.min(draft::line_lengths(&self.value)[line - 1]);
                    self.cursor = draft::cursor_at(&self.value, line - 1, clamped);
                }
                consumed(vec![])
            }
            Key::ArrowDown => {
                let (line, col) = draft::line_col(&self.value, self.cursor);
                let last = draft::line_lengths(&self.value).len() - 1;
                if line >= last {
                    if let Some(text) = self.history.down() {
                        self.recall(text);
                    }
                } else {
                    let clamped = col.min(draft::line_lengths(&self.value)[line + 1]);
                    self.cursor = draft::cursor_at(&self.value, line + 1, clamped);
                }
                consumed(vec![])
            }

            // -- Draft editing (the cursor is a char index; see the field
            // doc) --
            Key::Char(c) => {
                self.value = insert_char(&self.value, self.cursor, c);
                self.cursor += 1;
                consumed(vec![])
            }
            Key::InsertNewline => {
                self.value = insert_char(&self.value, self.cursor, '\n');
                self.cursor += 1;
                consumed(vec![])
            }
            Key::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.value = remove_char(&self.value, self.cursor);
                }
                consumed(vec![])
            }
            Key::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                consumed(vec![])
            }
            Key::Right => {
                let len = self.value.chars().count();
                self.cursor = (self.cursor + 1).min(len);
                consumed(vec![])
            }
            Key::Home => {
                let (line, _) = draft::line_col(&self.value, self.cursor);
                self.cursor = draft::cursor_at(&self.value, line, 0);
                consumed(vec![])
            }
            Key::End => {
                let (line, _) = draft::line_col(&self.value, self.cursor);
                let len = draft::line_lengths(&self.value)[line];
                self.cursor = draft::cursor_at(&self.value, line, len);
                consumed(vec![])
            }

            // Not mine: Escape and the wheel with no overlay open
            // (Cancellation and viewport scroll are the caller's). Only those
            // two reach here - the always-refused keys bounced at the gate,
            // and an open sub-state consumed Escape/wheel in its own arms -
            // and nothing above mutated on their path (the refusal contract).
            other => KeyOutcome::Refused(other),
        }
    }

    /// Offers one event to the Composer (the same first refusal as
    /// [`Composer::handle_key`]). The overlay-filling deliveries -
    /// [`Event::SelectorReady`] / [`Event::SelectorFailed`] - are consumed BY
    /// VARIANT, not by state: a stale fill that arrives after the overlay
    /// closed (Escape/selection) or was never `Loading` is consumed with no
    /// effects - it must not resurrect a closed popup, and the caller must
    /// never see it. Every other event is refused untouched.
    ///
    /// Staleness is decided by ACTIVATION, not by delivery order: a fill only
    /// lands when its `generation` echo matches the one the `Loading` overlay
    /// was opened with (stamped in [`Composer::commit_command`], carried
    /// through [`Effect::Command`]). A late fill from a FIRST activation
    /// arriving on a SECOND activation's fresh overlay (backspace out,
    /// re-commit, then the first delivery finally lands) is dropped by
    /// construction - no ordering coincidence involved.
    pub fn apply_event(&mut self, event: Event) -> EventOutcome {
        match event {
            Event::SelectorReady { generation, rows } => {
                self.fill_ready(generation, rows);
                EventOutcome::Consumed(vec![])
            }
            Event::SelectorFailed {
                generation,
                message,
            } => {
                self.fill_failed(generation, message);
                EventOutcome::Consumed(vec![])
            }
            Event::FileSearchReady {
                generation,
                query,
                suggestions,
            } => {
                self.fill_file_search(generation, query, suggestions);
                EventOutcome::Consumed(vec![])
            }
            // The `/mcp` dialog fills (ADR-0065 Phase E), the McpDialog analog of
            // the selector fills: consumed BY VARIANT (a stale fill for a closed
            // dialog is absorbed with no effect), generation-guarded inside the
            // dialog so a re-open's late fetch never lands.
            Event::McpDialogReady {
                generation,
                servers,
            } => {
                if let Some(dialog) = self.mcp_dialog.as_mut() {
                    dialog.fill_ready(generation, servers);
                }
                EventOutcome::Consumed(vec![])
            }
            // The `/mcp` OSC52 copy result (ADR-0065 Phase E): the adapter attempted
            // the write and reports whether it reached a TTY; the open AUTHENTICATE
            // step folds it into its copy-feedback hint. Consumed by the Composer
            // (it is the dialog's own); a result with no open dialog is a harmless
            // no-op (the user closed it before the report landed).
            Event::McpCopyResult { copied } => {
                if let Some(dialog) = self.mcp_dialog.as_mut() {
                    dialog.fold_copy_result(copied);
                }
                EventOutcome::Consumed(vec![])
            }
            // An OAuth progress line (ADR-0065 Phase D/E): folded into an OPEN
            // AUTHENTICATE step. When the dialog consumes it (the step is up for
            // this server), it is the Composer's own - the Screen never sees the
            // duplicate info line. When it does NOT (no dialog, or a different
            // step), it is REFUSED so `Screen::apply_voice` still surfaces the
            // operator-visible info line (the pre-Phase-E behaviour stands for the
            // no-dialog case).
            Event::McpAuthProgress {
                server,
                message,
                is_url,
            } => {
                let folded = self.mcp_dialog.as_mut().is_some_and(|dialog| {
                    dialog.fold_auth_progress(&server, message.clone(), is_url)
                });
                if folded {
                    EventOutcome::Consumed(vec![])
                } else {
                    EventOutcome::Refused(Event::McpAuthProgress {
                        server,
                        message,
                        is_url,
                    })
                }
            }
            other => EventOutcome::Refused(other),
        }
    }

    // Flips a Loading overlay to Ready over a fresh dialog - only for the
    // activation that requested this fill (the generation guard). The dialog is
    // seeded with the draft `rest` so a fill that lands after the user already
    // typed narrows immediately.
    fn fill_ready(&mut self, generation: u64, rows: Vec<SelectorRow>) {
        let rest = slash::parse(&self.value).rest.unwrap_or_default();
        self.selector.fill_ready(generation, rows, &rest);
    }

    // Flips a Loading overlay to Failed - same activation guard as
    // [`Composer::fill_ready`].
    fn fill_failed(&mut self, generation: u64, message: String) {
        self.selector.fill_failed(generation, message);
    }

    // Folds an AT file-search result into the picker overlay (Phase C2): map the
    // wire [`FileSuggestion`]s into render [`Suggestion`]s and hand them to the
    // guarded [`AtFiles::fill`], which drops a delivery whose generation OR query
    // no longer matches the live activation (a stale keystroke's result).
    fn fill_file_search(
        &mut self,
        generation: u64,
        query: String,
        suggestions: Vec<FileSuggestion>,
    ) {
        let rows = suggestions.into_iter().map(to_file_suggestion).collect();
        self.at_files.fill(generation, query, rows);
    }

    /// The Submit effect succeeded: record `prompt` into the history ring
    /// (dedup + cap live in [`crate::ui::history`]) and reset the WHOLE
    /// Composer - draft, menu highlight, and any open overlay close together.
    /// The full reset is the contract, not a convenience: a successful send
    /// resolves every Composer state, so nothing may linger into the next
    /// draft. Returns the follow-up effects - [`Effect::HistoryAppend`] - so
    /// the in-memory record and the on-disk append are one invariant, minted
    /// in one place. Call ONLY on `Ok`: on a Busy retry the draft must
    /// survive for the Steer (the caller owns that race - see the fold root's
    /// `submitted`).
    #[must_use = "carries the on-disk HistoryAppend effect - dropping it loses the persisted prompt"]
    pub fn submitted_ok(&mut self, prompt: &str) -> Vec<Effect> {
        self.history.record(prompt);
        self.clear();
        vec![Effect::HistoryAppend(prompt.to_string())]
    }

    /// The Steer effect succeeded: the same WHOLE-Composer reset as
    /// [`Composer::submitted_ok`] (draft, menu highlight, any open overlay) -
    /// by contract, not accident. No history record - steering text is not a
    /// prompt (it joins the Conversation unadorned; CONTEXT.md: Steering).
    pub fn steered_ok(&mut self) {
        self.clear();
    }

    /// Everything render needs, in one read: the draft, the char-index
    /// cursor, and the open overlay. The overlay is DERIVED from the draft
    /// (the one filter): `Menu` while the command token is being typed,
    /// `Selector` once a selector-opening command committed, `None` when the
    /// draft is not a slash draft.
    pub fn view(&self) -> ComposerView<'_> {
        ComposerView {
            draft: &self.value,
            cursor: self.cursor,
            overlay: self.overlay_view(),
        }
    }

    /// The open READY selector's highlight, without building the full
    /// [`OverlayView`] (no row cloning - this backs the per-frame `/theme`
    /// live preview): the command that opened the selector and the row under
    /// the cursor in the `rest`-filtered view, derived by the same rules as
    /// [`Composer::view`]. `None` for the menu, a Loading/Failed selector, an
    /// empty filtered view, or no overlay at all.
    pub fn selector_highlight(&self) -> Option<(&str, &SelectorRow)> {
        if !slash::is_slash(&self.value) {
            return None;
        }
        let draft = slash::parse(&self.value);
        if draft.rest.is_none() || !slash::lookup(&draft.name).is_some_and(|c| c.opens_selector) {
            return None;
        }
        let dialog = self.selector.ready_ref()?;
        let row = dialog.active_row()?;
        Some((self.selector.command.as_str(), row))
    }

    // ---- Internals ---------------------------------------------------------

    // The open overlay, derived from the draft (the one filter) and the owned
    // dialog state (ADR-0051). `Dialog` (System A) once a selector-opening
    // command committed; `Menu` (System B) while the command token is typed.
    fn overlay_view(&self) -> Option<OverlayView> {
        // The `/mcp` dialog (ADR-0065 Phase E) takes precedence over every
        // draft-derived overlay: once open it owns the keyboard AND the overlay
        // slot, whatever the (cleared) draft holds.
        if let Some(dialog) = &self.mcp_dialog {
            return Some(OverlayView::McpDialog(dialog.view()));
        }
        // AT takes precedence over SLASH (qwen checks AT first): an `@path`
        // context in the draft draws the file picker even after a `/command `.
        // A dismissed picker (Esc, draft kept) shows no overlay until the
        // pattern next changes.
        if let Some(at) = at_context(&self.value, self.cursor)
            && !self.at_files.is_dismissed()
        {
            return Some(self.at_view(&at));
        }
        if !slash::is_slash(&self.value) {
            return None;
        }
        let draft = slash::parse(&self.value);
        let in_selector =
            draft.rest.is_some() && slash::lookup(&draft.name).is_some_and(|c| c.opens_selector);
        if in_selector {
            Some(self.dialog_view(&draft.name))
        } else {
            Some(self.menu_view(&draft.name))
        }
    }

    // The `@path` file picker's render view (Phase C2): the fetched suggestions
    // for the live pattern (empty until the guarded fill lands), the active
    // index + scroll window (re-clamped), the pattern for the fuzzy highlight,
    // and whether a fetch for this pattern is still outstanding (the subtle
    // "searching…" line). Drawn like `Menu` (color-only, no numbers) but sourced
    // from the async walk.
    fn at_view(&self, at: &AtContext) -> OverlayView {
        let suggestions = self.at_files.suggestions_for(&at.query).to_vec();
        let (active, scroll) = self.at_files.view_cursor(&at.query);
        OverlayView::AtFiles {
            suggestions,
            active,
            scroll,
            query: at.query.clone(),
            loading: self.at_files.is_loading_for(&at.query),
        }
    }

    // The System B palette view: the ranked suggestions, the active index +
    // scroll window (re-clamped to the ranked length), and the query for the
    // inverted-highlight render.
    fn menu_view(&self, query: &str) -> OverlayView {
        let suggestions = rank_menu(&self.skill_commands, query);
        let mut menu = self.menu.clone();
        menu.clamp(suggestions.len());
        OverlayView::Menu {
            suggestions,
            active: menu.active(),
            scroll: menu.scroll(),
            query: query.to_string(),
            expanded: menu.active_expanded(),
        }
    }

    // The System A dialog view: the overlay status plus, when Ready, the
    // visible (filtered) rows, the active row, and a detail string. The detail
    // is the active model row's "(current)"/context hint if any (theme carries
    // none). The Composer stays command-agnostic - it surfaces the active
    // row's hint, and the model command's own row-building decides what that
    // says.
    fn dialog_view(&self, fallback_command: &str) -> OverlayView {
        let DialogParts {
            command,
            status,
            rows,
            active,
            detail,
        } = self.selector.view_parts(fallback_command);
        OverlayView::Dialog {
            command,
            status,
            rows,
            active,
            detail,
        }
    }

    // Commits the highlighted command row from the COMMAND MENU
    // (ADR-0032/0033). `row` is `None` when the filtered menu is empty
    // (unknown command). A selector-opening command switches the popup to its
    // selector sub-state: the draft is normalized to `"/<name> "`, a
    // `Loading` overlay is set, and `Effect::Command` is emitted ONCE (the
    // overlay's presence guards against re-emitting on later keystrokes - the
    // menu block only runs when there is no overlay). A fire-and-run command
    // emits `Effect::Command` and clears the draft.
    fn commit_command(&mut self, row: Option<SelectorRow>) -> KeyOutcome {
        let Some(row) = row else {
            let filter = slash::parse(&self.value).name;
            self.clear();
            return KeyOutcome::Consumed {
                effects: vec![],
                notice: Some(format!("unknown command: /{filter}")),
            };
        };
        let opens_selector = slash::lookup(&row.value).is_some_and(|c| c.opens_selector);
        if opens_selector {
            // Enter the selector sub-state: normalize to `/<name> ` so `rest`
            // becomes `Some("")`, set the Loading overlay, and fetch once.
            // Each activation gets a fresh generation, stamped on the overlay
            // AND the effect: the fill events echo it back, and only a
            // matching echo lands (see [`Composer::apply_event`]).
            self.selector_generation += 1;
            self.value = format!("/{} ", row.value);
            self.cursor = self.value.chars().count();
            self.menu = Completion::new();
            let filter_mode = filter_mode_for(&row.value);
            self.selector
                .open(row.value.clone(), self.selector_generation, filter_mode);
            consumed(vec![Effect::Command {
                name: row.value,
                generation: self.selector_generation,
            }])
        } else if row.value == mcp_command::NAME {
            // `/mcp` opens the navigation-stack McpDialog overlay (ADR-0065 Phase
            // E), a distinct System-A overlay - NOT the flat selector. Bump the
            // activation (the async `mcp_views()` fill echoes it), clear the draft
            // (the dialog owns the keyboard, no filter follows), open the overlay
            // to a Loading state, and emit the `McpCommand` effect that kicks the
            // fetch. Mirrors the selector's Loading-open-then-async-fill, but the
            // overlay is the dialog, not a filterable list.
            self.selector_generation += 1;
            let generation = self.selector_generation;
            self.clear();
            self.mcp_dialog = Some(McpDialog::open(generation));
            consumed(vec![mcp_command::open_effect(generation)])
        } else {
            // Fire-and-run: no fill will echo this generation, so the current
            // counter is carried unbumped (the payload field is uniform).
            self.clear();
            consumed(vec![Effect::Command {
                name: row.value,
                generation: self.selector_generation,
            }])
        }
    }

    // Place a recalled (or restored) history entry into the draft, cursor at
    // the end - the landing spot the Up/Down arms share.
    fn recall(&mut self, text: String) {
        self.cursor = text.chars().count();
        self.value = text;
    }

    // Empties the draft, resets the Slash Command highlight, and closes any
    // command-selector overlay - the landing spot after a submitted/steered
    // draft and a committed/closed slash sub-state alike.
    fn clear(&mut self) {
        self.value = String::new();
        self.cursor = 0;
        self.menu = Completion::new();
        self.selector.close();
        self.at_files.close();
        self.mcp_dialog = None;
    }

    // Closes the selector overlay AND clears the draft (they open together,
    // they close together). The named alias reads as intent at the call sites
    // where a selection/Escape resolves the sub-state.
    fn close_selector(&mut self) {
        self.clear();
    }
}

// A key's intent inside the DIALOG sub-state (System A), the pure
// classification [`Composer::classify_dialog_key`] produces so the fold is a
// dispatch over it (IOSP): navigate, pick (Enter or a quick-select digit),
// close, swallow (an editing char on a `Frozen` theme dialog), or edit (filter
// a model dialog).
enum DialogKey {
    Nav(SelectionKey),
    Pick(SelectionKey),
    Close,
    Swallow,
    Edit(Key),
}

// A consumed key with no notice - the common case.
fn consumed(effects: Vec<Effect>) -> KeyOutcome {
    KeyOutcome::Consumed {
        effects,
        notice: None,
    }
}

// Prepends `head` effects onto an `outcome`, preserving its notice (the AT
// entry-fetch weaves its `Effect::FileSearch` in front of the inner fold's
// effects). A refusal never carries effects, so it passes through unchanged -
// but the AT fold never refuses (every key in an AT context is consumed).
fn prepend_effects(mut head: Vec<Effect>, outcome: KeyOutcome) -> KeyOutcome {
    match outcome {
        KeyOutcome::Consumed {
            mut effects,
            notice,
        } => {
            head.append(&mut effects);
            KeyOutcome::Consumed {
                effects: head,
                notice,
            }
        }
        KeyOutcome::Refused(key) => KeyOutcome::Refused(key),
    }
}

/// The digit-timeout clock the DIALOG's [`SelectionList`] never actually needs:
/// model/theme dialogs are short (< 10 navigable rows), so every digit selects
/// immediately and the buffered-timeout path is dead. A constant stands in for
/// the host tick the approval radio's list gets from ui.rs.
const NOW_UNUSED: Millis = 0;

// Whether `key` inserts or removes draft TEXT (so a `Frozen` theme dialog can
// swallow it while still letting Backspace-out and cursor moves through).
fn is_text_edit(key: &Key) -> bool {
    matches!(key, Key::Char(_) | Key::InsertNewline)
}

// -- AT file completion detection (Phase C2, qwen `useCommandCompletion`) --

/// An open `@path` completion context, detected from the draft + cursor by
/// [`at_context`] (qwen `useCommandCompletion` CompletionMode.AT). All indices
/// are CHAR indices into the whole draft `value` (not the line), so the accept
/// path can splice `value` directly.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AtContext {
    /// The char index of the `@` sigil in `value`.
    at: usize,
    /// The char index just after `@` where the pattern begins (qwen
    /// `completionStart`).
    start: usize,
    /// The char index of the pattern's end - the next UNESCAPED space or EOL
    /// (qwen `completionEnd`).
    end: usize,
    /// The pattern `value[start..end]` (qwen `partialPath`), backslash escapes
    /// included so the round-trip holds.
    query: String,
}

/// Detects an `@path` completion context at `cursor` in `value` (qwen
/// `useCommandCompletion`, lines 86-131, ported verbatim into char space).
///
/// Scans BACKWARD from `cursor-1` over the CURRENT logical line (the draft is
/// multi-line; qwen scans `buffer.lines[cursorRow]`): an UNESCAPED space (an
/// even count of immediately-preceding backslashes) breaks the scan with no AT
/// context; an `@` opens AT mode, its pattern running FORWARD from just after
/// the `@` to the next unescaped space (same even-backslash rule) or the line
/// end. Returns `None` when no `@` precedes the cursor on this line before an
/// unescaped space. AT takes precedence over SLASH (the caller checks this
/// first), so `@` after a `/command ` still triggers the file search.
fn at_context(value: &str, cursor: usize) -> Option<AtContext> {
    let chars: Vec<char> = value.chars().collect();
    let cursor = cursor.min(chars.len());
    // The current logical line's [line_start, line_end) char bounds (scan is
    // confined to it, like qwen's per-row buffer).
    let line_start = chars[..cursor]
        .iter()
        .rposition(|&c| c == '\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let line_end = chars[cursor..]
        .iter()
        .position(|&c| c == '\n')
        .map(|off| cursor + off)
        .unwrap_or(chars.len());

    // Backward from cursor-1 to the line start (qwen `i = cursorCol - 1`).
    let mut i = cursor;
    while i > line_start {
        i -= 1;
        match chars[i] {
            ' ' if is_unescaped_space(&chars, i) => return None,
            '@' => {
                let start = i + 1;
                // Forward from the cursor to the next unescaped space or EOL
                // (qwen scans from `cursorCol` forward for `end`).
                let end = (cursor..line_end)
                    .find(|&j| chars[j] == ' ' && is_unescaped_space(&chars, j))
                    .unwrap_or(line_end);
                let query: String = chars[start..end].iter().collect();
                return Some(AtContext {
                    at: i,
                    start,
                    end,
                    query,
                });
            }
            _ => {}
        }
    }
    None
}

// Whether the space at char index `i` is UNESCAPED: an EVEN count of
// consecutive backslashes immediately before it (qwen's backslash-parity rule,
// shared by the backward-break and forward-end scans).
fn is_unescaped_space(chars: &[char], i: usize) -> bool {
    let mut backslashes = 0;
    let mut j = i;
    while j > 0 && chars[j - 1] == '\\' {
        backslashes += 1;
        j -= 1;
    }
    backslashes % 2 == 0
}

// NOTE: the AT path VALUE arrives already space-escaped from the adapter's file
// search (qwen `useAtCompletion` maps `value: escapePath(p)` at fetch time and
// `handleAutocomplete` inserts it verbatim). So [`Composer::at_accept`] splices
// `row.value` as-is - the escaping lives beside the walk in
// [`crate::ui::file_search`], not here - and the escaped `\ ` round-trips
// through [`at_context`]'s unescaped-space rule.

// The ALWAYS-refused rows of the routing table (pure predicate): keys the
// Composer never folds whatever its state - page scroll, the display toggles,
// and named/other keys. Extracted so the fold root stays an integration step.
fn always_refused(key: &Key) -> bool {
    matches!(
        key,
        Key::PageUp | Key::PageDown | Key::ToggleCompact | Key::Named(_) | Key::Other
    )
}

// The [`McpKey`] a raw [`Key`] maps to inside the open `/mcp` dialog (ADR-0065
// Phase E): the dialog navigates by the arrows and Enter/Escape, plus `c` on the
// AUTHENTICATE step (qwen's copy-the-auth-URL). It has no editable filter, so
// every other key is `None` - the dialog swallows it as a no-op (nothing leaks to
// the draft behind it). `c` maps to [`McpKey::Copy`] whatever the step; the pure
// fold no-ops it off the AUTHENTICATE step or with no URL on screen, so it is only
// stolen where qwen binds it.
fn to_mcp_key(key: &Key) -> Option<McpKey> {
    match key {
        Key::ArrowUp => Some(McpKey::Up),
        Key::ArrowDown => Some(McpKey::Down),
        Key::Enter => Some(McpKey::Enter),
        Key::Escape => Some(McpKey::Escape),
        Key::Char('c') => Some(McpKey::Copy),
        _ => None,
    }
}

// The DIALOG filter mode a committed command opens with (ADR-0051): the
// `/model` dialog carries an editable fuzzy filter (suspenders surfaces
// hundreds of catalog models - a deliberate divergence from qwen's filter-less
// dialog); every other selector-opening command (`/theme`) is frozen-draft,
// qwen-faithful. Keyed by name here because the two-systems policy is a
// Composer-level decision, not something the command modules know about.
fn filter_mode_for(command: &str) -> DialogFilter {
    if command == "model" {
        DialogFilter::Filtered
    } else {
        DialogFilter::Frozen
    }
}

// Ranks the two-layer registry against the query token (ADR-0032/0051 System
// B): the built-in [`slash::COMMANDS`] UNION the runtime `skills` layer. No
// recency store exists in suspenders yet, so an empty recent map + a constant
// clock are passed - the `now` seam is honored in [`completion::rank`]'s
// signature for when a store lands.
fn rank_menu(skills: &[slash::SkillCommand], query: &str) -> Vec<Suggestion> {
    let commands = slash::commands_ref(skills);
    completion::rank(&commands, query, &|_| None, 0)
}

// Maps a wire [`FileSuggestion`] (the async AT fill) into a render
// [`Suggestion`]: the repo-relative path is the `label` (highlighted against
// the query via `matched`), the escaped path is the `value` inserted on accept.
// No description (a file path needs none).
fn to_file_suggestion(s: FileSuggestion) -> Suggestion {
    Suggestion {
        label: s.label,
        value: s.value,
        description: String::new(),
        argument_hint: None,
        matched: s.matched,
    }
}

// Whether the active palette row's label is "long" (chars >= MAX_WIDTH, qwen
// PrepareLabel): the gate for the `←/→` expand arms - a short active row lets
// the arrows fall through to plain cursor moves (qwen returns false).
fn active_label_is_long(suggestions: &[Suggestion], active: usize) -> bool {
    suggestions
        .get(active)
        .is_some_and(|s| s.label.chars().count() >= completion::MAX_WIDTH)
}

// -- Draft editing (char-index string surgery) --
//
// The cursor is a CHAR index (the codebase counts chars, not bytes). The
// logical line/column geometry - `line_col`, `line_lengths`, `cursor_at`,
// `byte_of` - has ONE owner in `ui::draft`, shared with the layout math
// below. These two helpers are the edit-side string surgery built on that
// geometry: they translate the char-index cursor to a byte offset (via
// `draft::byte_of`) exactly once, at the mutation site, so multi-byte input
// never splits a char or panics.

/// `value` with `c` inserted at char index `cursor`.
fn insert_char(value: &str, cursor: usize, c: char) -> String {
    let mut out = value.to_string();
    out.insert(draft::byte_of(value, cursor), c);
    out
}

/// `value` with the char at char index `cursor` removed. `cursor` must be in
/// range (the Backspace arm guards it).
fn remove_char(value: &str, cursor: usize) -> String {
    let mut out = value.to_string();
    out.remove(draft::byte_of(value, cursor));
    out
}

// ---------------------------------------------------------------------------
// Layout math - the wrapping and cursor-cell geometry the render adapter
// draws with (see the module doc's layout contract).
// ---------------------------------------------------------------------------

/// The most rows the Composer ever occupies, however tall the terminal.
/// Private: [`max_visible_rows`] is the one consumer (the popup's own row cap
/// lives with the popup, in `ui::components` - a different box, a different
/// owner, coincidentally the same number).
const MAX_ROWS: usize = 8;

/// The Composer's draft, wrapped: the display rows (hard newlines AND
/// width-wrapping both split) and the `(row, col)` cell the cursor occupies
/// within them. Plain data - the view adds the gutter and colors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerLayout {
    /// The wrapped display rows, top first. Never empty: an empty draft is
    /// one empty row (the cursor needs a cell to sit in).
    pub rows: Vec<String>,
    /// The row index (into `rows`) the cursor sits on.
    pub cursor_row: usize,
    /// The column (in chars, `< width`) the cursor sits at within its row.
    pub cursor_col: usize,
}

/// Wraps the draft at `width` chars per row and locates the cursor (a CHAR
/// index into `value`, clamped to its length). `width` is the text width the
/// view will draw the rows at - the same for every row, first and
/// continuation alike, since the "› " gutter and the 2-space indent are the
/// same 2 cells. A degenerate `width` of 0 is treated as 1.
pub fn layout(value: &str, cursor: usize, width: usize) -> ComposerLayout {
    let width = width.max(1);
    let cursor = cursor.min(value.chars().count());

    // The logical cell of the cursor - which hard line, and the char column
    // within it - comes from the ONE owner (`ui::draft`), the same source the
    // edit path reads. Wrapping that logical column into a visual row/col is
    // this function's only addition; it never re-derives the logical geometry.
    let (cursor_line, cursor_offset) = draft::line_col(value, cursor);

    let mut rows = Vec::new();
    let mut cursor_row = 0;
    let mut cursor_col = 0;

    for (index, line) in value.split('\n').enumerate() {
        let chars: Vec<char> = line.chars().collect();
        // `len / width + 1` rows: the extra row on an exact multiple is the
        // cell the cursor occupies at the line's end (see the module doc).
        let row_count = chars.len() / width + 1;
        let base_row = rows.len();
        for r in 0..row_count {
            let end = ((r + 1) * width).min(chars.len());
            rows.push(chars[r * width..end].iter().collect());
        }
        // When this is the cursor's logical line, wrap its char column into a
        // visual row/col. `cursor_offset < width` need not hold, so a wrapped
        // line divides it: `/ width` picks the continuation row, `% width` the
        // cell - and the exact-multiple extra row above catches the end cell.
        if index == cursor_line {
            cursor_row = base_row + cursor_offset / width;
            cursor_col = cursor_offset % width;
        }
    }

    ComposerLayout {
        rows,
        cursor_row,
        cursor_col,
    }
}

/// The most rows the Composer may occupy in a `terminal_height`-row terminal:
/// `min(8, terminal_height / 3)`, but never below 1 - the transcript viewport
/// keeps the lion's share, and the Composer never vanishes.
pub fn max_visible_rows(terminal_height: usize) -> usize {
    (terminal_height / 3).clamp(1, MAX_ROWS)
}

/// The first row a `visible`-row Composer box shows so the cursor row is
/// always inside it, preferring the cursor at the BOTTOM of the box (like a
/// terminal): rows above scroll away first, and only a draft shorter than the
/// window shows its tail below the cursor.
pub fn first_visible_row(cursor_row: usize, visible: usize) -> usize {
    cursor_row.saturating_sub(visible.saturating_sub(1))
}

#[cfg(test)]
#[path = "../../tests/ui/composer.rs"]
mod tests;
