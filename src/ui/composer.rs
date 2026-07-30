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

use crate::event::Event;
use crate::ui::completion::{self, Completion, Suggestion};
use crate::ui::draft;
use crate::ui::history::History;
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

    // Whether a selector is open at all (anything but the Null-Object `Idle`) -
    // a test observability seam over the private status.
    #[cfg(test)]
    fn is_open(&self) -> bool {
        !matches!(self.status, DialogStatus::Idle)
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
/// prove it.
#[derive(Debug, Clone, PartialEq, Eq)]
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
}

impl Composer {
    /// A fresh Composer: empty draft, no overlay, the history ring seeded
    /// with `history` (oldest first, from the on-disk file).
    pub fn new(history: Vec<String>) -> Self {
        Composer {
            value: String::new(),
            cursor: 0,
            menu: Completion::new(),
            selector: CommandSelector::closed(),
            selector_generation: 0,
            history: History::new(history),
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
        let suggestions = rank_menu(name);
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

    // The System B palette view: the ranked suggestions, the active index +
    // scroll window (re-clamped to the ranked length), and the query for the
    // inverted-highlight render.
    fn menu_view(&self, query: &str) -> OverlayView {
        let suggestions = rank_menu(query);
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

// The ALWAYS-refused rows of the routing table (pure predicate): keys the
// Composer never folds whatever its state - page scroll, the display toggles,
// and named/other keys. Extracted so the fold root stays an integration step.
fn always_refused(key: &Key) -> bool {
    matches!(
        key,
        Key::PageUp | Key::PageDown | Key::ToggleCompact | Key::Named(_) | Key::Other
    )
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

// Ranks the registry against the query token (ADR-0051 System B). No recency
// store exists in suspenders yet, so an empty recent map + a constant clock are
// passed - the `now` seam is honored in [`completion::rank`]'s signature for
// when a store lands.
fn rank_menu(query: &str) -> Vec<Suggestion> {
    completion::rank(query, &|_| None, 0)
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
mod tests {
    use super::*;
    use crate::view_model::RowRole;

    // =======================================================================
    // The Composer fold (state, first refusal, overlays, history).
    // =======================================================================

    fn fresh() -> Composer {
        Composer::new(vec![])
    }

    fn with_history(entries: &[&str]) -> Composer {
        Composer::new(entries.iter().map(|s| s.to_string()).collect())
    }

    // A Composer holding `value` with the cursor at char index `cursor` -
    // direct field access (same module) stands in for the typing that
    // produced it.
    fn with_draft(value: &str, cursor: usize) -> Composer {
        let mut c = fresh();
        set_draft(&mut c, value, cursor);
        c
    }

    fn set_draft(c: &mut Composer, value: &str, cursor: usize) {
        c.value = value.to_string();
        c.cursor = cursor;
    }

    // A slash draft with the cursor at the end - the menu/selector openers.
    fn slashing(draft: &str) -> Composer {
        with_draft(draft, draft.chars().count())
    }

    // Folds `key` when idle, asserting the Composer consumed it without a
    // notice; returns the effects. (Named apart from the module's `consumed`
    // outcome constructor - this one FOLDS, that one builds.)
    fn fold_consumed(c: &mut Composer, key: Key) -> Vec<Effect> {
        match c.handle_key(UngatedKey::for_test(key), Status::Idle) {
            KeyOutcome::Consumed {
                effects,
                notice: None,
            } => effects,
            other => panic!("expected a plain consume, got {other:?}"),
        }
    }

    fn press(c: &mut Composer, keys: Vec<Key>) {
        for key in keys {
            fold_consumed(c, key);
        }
    }

    fn typed(text: &str) -> Vec<Key> {
        text.chars().map(Key::Char).collect()
    }

    fn overlay(c: &Composer) -> Option<OverlayView> {
        c.view().overlay
    }

    // Delivers an event, asserting the Composer consumed it with no effects.
    fn deliver(c: &mut Composer, event: Event) {
        match c.apply_event(event) {
            EventOutcome::Consumed(effects) => assert_eq!(effects, vec![]),
            EventOutcome::Refused(event) => panic!("expected a consume, got refusal of {event:?}"),
        }
    }

    // --- Slash Command menu (ADR-0032) --------------------------------------

    // The palette's suggestion VALUES (System B) for the open Menu overlay.
    fn menu_values(c: &Composer) -> Vec<String> {
        match overlay(c) {
            Some(OverlayView::Menu { suggestions, .. }) => {
                suggestions.iter().map(|s| s.value.clone()).collect()
            }
            other => panic!("expected the menu, got {other:?}"),
        }
    }

    #[test]
    fn a_leading_slash_opens_the_palette_showing_every_command() {
        match overlay(&slashing("/")) {
            Some(OverlayView::Menu {
                suggestions,
                active,
                ..
            }) => {
                assert_eq!(
                    suggestions
                        .iter()
                        .map(|s| s.value.clone())
                        .collect::<Vec<_>>(),
                    vec!["model", "theme"]
                );
                assert_eq!(active, 0);
            }
            other => panic!("expected the palette on '/', got {other:?}"),
        }
    }

    #[test]
    fn a_non_slash_draft_has_no_overlay() {
        assert_eq!(overlay(&with_draft("fix the bug", 11)), None);
        assert_eq!(overlay(&fresh()), None);
    }

    #[test]
    fn typing_ranks_the_palette_by_the_command_token() {
        // "/mod" prefix-matches only "model".
        assert_eq!(menu_values(&slashing("/mod")), vec!["model"]);
        // A token that matches nothing leaves an empty (but open) palette.
        match overlay(&slashing("/zzz")) {
            Some(OverlayView::Menu { suggestions, .. }) => assert!(suggestions.is_empty()),
            other => panic!("expected the open empty palette, got {other:?}"),
        }
    }

    #[test]
    fn up_down_move_the_palette_highlight_wrapping() {
        // Two commands: the arrows move between them and WRAP (System B nav).
        let mut c = slashing("/");
        assert_eq!(fold_consumed(&mut c, Key::ArrowDown), vec![]);
        assert_eq!(menu_highlight(&c), 1);
        assert_eq!(fold_consumed(&mut c, Key::ArrowDown), vec![]);
        assert_eq!(menu_highlight(&c), 0, "wraps to the first");
        assert_eq!(fold_consumed(&mut c, Key::ArrowUp), vec![]);
        assert_eq!(menu_highlight(&c), 1, "wraps to the last");

        // Typing narrows the palette to one row: the highlight clamps onto it.
        let mut c = slashing("/");
        assert_eq!(fold_consumed(&mut c, Key::ArrowDown), vec![]);
        assert_eq!(menu_highlight(&c), 1);
        for key in [Key::Char('m'), Key::Char('o'), Key::Char('d')] {
            assert_eq!(fold_consumed(&mut c, key), vec![]);
        }
        assert_eq!(menu_highlight(&c), 0, "clamped to the one ranked row");
    }

    fn menu_highlight(c: &Composer) -> usize {
        match overlay(c) {
            Some(OverlayView::Menu { active, .. }) => active,
            other => panic!("expected the menu, got {other:?}"),
        }
    }

    // `/model` opens a selector (ADR-0033), so committing it does NOT clear
    // the draft the way a fire-and-run command would. It normalizes the draft
    // to `"/model "`, sets a Loading overlay, and emits ONE Effect::Command -
    // the selector-activation path is exercised separately below.
    #[test]
    fn enter_commits_the_highlighted_command() {
        let mut c = slashing("/model");
        let effects = fold_consumed(&mut c, Key::Enter);
        assert_eq!(
            effects,
            vec![Effect::Command {
                name: "model".into(),
                generation: 1,
            }]
        );
        // Selector-opening: draft normalized, NOT cleared; overlay is Loading.
        assert_eq!(c.view().draft, "/model ");
        assert_eq!(c.view().cursor, 7);
        assert!(matches!(
            overlay(&c),
            Some(OverlayView::Dialog {
                status: OverlayStatus::Loading,
                ..
            })
        ));
    }

    #[test]
    fn committing_a_partial_token_uses_the_highlighted_full_command_name() {
        // "/mod" filters to the one command; Enter commits "model", not "mod".
        let mut c = slashing("/mod");
        assert_eq!(
            fold_consumed(&mut c, Key::Enter),
            vec![Effect::Command {
                name: "model".into(),
                generation: 1,
            }]
        );
    }

    #[test]
    fn enter_on_an_unknown_command_yields_a_notice_and_no_effects() {
        let mut c = slashing("/nope");
        match c.handle_key(UngatedKey::for_test(Key::Enter), Status::Idle) {
            KeyOutcome::Consumed { effects, notice } => {
                assert_eq!(effects, vec![], "no Turn, no command effect");
                assert_eq!(notice, Some("unknown command: /nope".into()));
            }
            other => panic!("expected a consumed notice, got {other:?}"),
        }
        assert_eq!(c.view().draft, "", "draft cleared");
        assert_eq!(overlay(&c), None);
    }

    #[test]
    fn escape_closes_the_menu_by_clearing_the_draft() {
        let mut c = slashing("/model");
        assert_eq!(fold_consumed(&mut c, Key::Escape), vec![]);
        assert_eq!(c.view().draft, "");
        assert_eq!(overlay(&c), None);
    }

    #[test]
    fn typing_and_backspace_fall_through_to_the_draft_while_slashing() {
        // A char extends the draft (and refilters the menu).
        let mut c = slashing("/mode");
        assert_eq!(fold_consumed(&mut c, Key::Char('l')), vec![]);
        assert_eq!(c.view().draft, "/model");
        match overlay(&c) {
            Some(OverlayView::Menu { suggestions, .. }) => assert_eq!(
                suggestions
                    .iter()
                    .map(|s| s.value.clone())
                    .collect::<Vec<_>>(),
                vec!["model"]
            ),
            other => panic!("expected the menu, got {other:?}"),
        }

        // Backspace erases back toward the slash; the menu stays open.
        assert_eq!(fold_consumed(&mut c, Key::Backspace), vec![]);
        assert_eq!(c.view().draft, "/mode");
        assert!(overlay(&c).is_some());

        // Backspacing away the slash closes the menu; the remaining text is a
        // normal draft again.
        let mut c = slashing("/");
        fold_consumed(&mut c, Key::Backspace);
        assert_eq!(c.view().draft, "");
        assert_eq!(overlay(&c), None);
    }

    #[test]
    fn a_slash_draft_never_submits_or_steers_even_while_running() {
        // Idle: Enter commits a command, never a Submit.
        let mut c = slashing("/model");
        assert!(matches!(
            fold_consumed(&mut c, Key::Enter).as_slice(),
            [Effect::Command { .. }]
        ));

        // Running: the leading `/` still opens the menu and Enter commits the
        // command - it is NOT Steering text.
        let mut c = slashing("/model");
        assert!(overlay(&c).is_some(), "menu opens while running");
        match c.handle_key(UngatedKey::for_test(Key::Enter), Status::Running) {
            KeyOutcome::Consumed { effects, .. } => assert_eq!(
                effects,
                vec![Effect::Command {
                    name: "model".into(),
                    generation: 1,
                }]
            ),
            other => panic!("expected a consumed commit, got {other:?}"),
        }
    }

    // --- Slash Command selector overlay (ADR-0033) --------------------------

    // A model row for the injected SelectorReady events (value = label).
    fn model_row(id: &str) -> SelectorRow {
        SelectorRow::new(id, id, None)
    }

    // The generation the one Command effect of a commit carried - the echo
    // the fill events must repeat to land.
    fn command_generation(effects: &[Effect]) -> u64 {
        match effects {
            [Effect::Command { generation, .. }] => *generation,
            other => panic!("expected one Command effect, got {other:?}"),
        }
    }

    // The overlay after committing `/model` and delivering rows. The draft is
    // left at `"/model "` (rest = Some("")), the sub-state.
    fn model_selector_ready(rows: Vec<SelectorRow>) -> Composer {
        let mut c = slashing("/model");
        let generation = command_generation(&fold_consumed(&mut c, Key::Enter));
        deliver(&mut c, Event::selector_ready(generation, rows));
        c
    }

    // The highlighted index of a selector overlay.
    fn highlight_of(c: &Composer) -> usize {
        match overlay(c) {
            Some(OverlayView::Dialog { active, .. }) => active,
            other => panic!("expected a selector overlay, got {other:?}"),
        }
    }

    // --- selector_highlight (the non-allocating per-frame preview read) -----

    #[test]
    fn selector_highlight_names_the_command_and_the_highlighted_filtered_row() {
        let mut c = model_selector_ready(vec![model_row("qwen"), model_row("llama")]);
        let (command, row) = c.selector_highlight().expect("a Ready selector");
        assert_eq!(command, "model");
        assert_eq!(row, &model_row("qwen"));

        // It tracks the cursor and the rest filter, like the OverlayView.
        fold_consumed(&mut c, Key::ArrowDown);
        assert_eq!(c.selector_highlight().unwrap().1, &model_row("llama"));
        press(&mut c, typed("qw"));
        assert_eq!(
            c.selector_highlight().unwrap().1,
            &model_row("qwen"),
            "the filter narrowed and the highlight snapped"
        );
    }

    #[test]
    fn selector_highlight_is_none_outside_a_ready_selector() {
        // No overlay, the menu, and a Loading selector all read None.
        assert_eq!(fresh().selector_highlight(), None);
        assert_eq!(with_draft("fix the bug", 11).selector_highlight(), None);
        assert_eq!(slashing("/model").selector_highlight(), None, "the menu");
        let mut c = slashing("/model");
        fold_consumed(&mut c, Key::Enter);
        assert_eq!(c.selector_highlight(), None, "Loading has no rows");
    }

    #[test]
    fn selector_highlight_is_none_when_the_filter_leaves_nothing() {
        let mut c = model_selector_ready(vec![model_row("qwen")]);
        press(&mut c, typed("zzz"));
        assert_eq!(c.selector_highlight(), None, "an empty filtered view");
    }

    #[test]
    fn committing_a_selector_command_by_enter_loads_normalizes_and_fetches_once() {
        let mut c = slashing("/model");
        // Exactly one Effect::Command (the adapter fetches).
        assert_eq!(
            fold_consumed(&mut c, Key::Enter),
            vec![Effect::Command {
                name: "model".into(),
                generation: 1,
            }]
        );
        // Draft normalized to `/model ` (rest = Some("")) - NOT cleared.
        assert_eq!(c.view().draft, "/model ");
        // Overlay is Loading for `model`.
        assert!(matches!(
            overlay(&c),
            Some(OverlayView::Dialog {
                status: OverlayStatus::Loading,
                ..
            })
        ));
    }

    #[test]
    fn committing_a_selector_command_by_typing_a_space_loads_and_fetches_once() {
        // Typing the space after `/model` commits it the same way Enter does.
        let mut c = slashing("/model");
        assert_eq!(
            fold_consumed(&mut c, Key::Char(' ')),
            vec![Effect::Command {
                name: "model".into(),
                generation: 1,
            }]
        );
        assert_eq!(c.view().draft, "/model ");
        assert!(matches!(
            overlay(&c),
            Some(OverlayView::Dialog {
                status: OverlayStatus::Loading,
                ..
            })
        ));
    }

    #[test]
    fn selector_ready_flips_loading_to_ready_and_the_rest_filters_the_rows() {
        let rows = vec![model_row("qwen"), model_row("llama"), model_row("gpt")];
        let mut c = model_selector_ready(rows);
        // Ready, all rows shown (rest is "").
        match overlay(&c) {
            Some(OverlayView::Dialog {
                status: OverlayStatus::Ready,
                rows,
                active,
                command,
                ..
            }) => {
                assert_eq!(command, "model");
                assert_eq!(rows.len(), 3);
                assert_eq!(active, 0);
            }
            other => panic!("expected Ready selector, got {other:?}"),
        }
        // Typing after the space filters via `rest` (the draft owns the filter).
        fold_consumed(&mut c, Key::Char('q'));
        assert_eq!(c.view().draft, "/model q");
        match overlay(&c) {
            Some(OverlayView::Dialog { rows, .. }) => {
                assert_eq!(rows, vec![model_row("qwen")], "only 'qwen' contains 'q'");
            }
            other => panic!("expected filtered selector, got {other:?}"),
        }
    }

    #[test]
    fn a_model_filter_reveals_a_matching_greyed_group_with_its_trailing_note() {
        // System A (ADR-0051): the numbered dialog scrolls instead of collapsing,
        // so a greyed-group filter reveals every matching collapsed row and keeps
        // the trailing note - no "· N more" cap. Typing the provider name shows
        // its whole catalog plus the "unavailable" note.
        let mut rows = vec![SelectorRow::header("openrouter")];
        rows.extend((0..8).map(|i| SelectorRow::collapsed(format!("openrouter/m{i}"))));
        rows.push(SelectorRow::note(
            "  unavailable",
            Some("set OPENROUTER_API_KEY".into()),
        ));
        let mut c = model_selector_ready(rows);
        press(&mut c, typed("openrouter"));
        match overlay(&c) {
            Some(OverlayView::Dialog { rows, .. }) => {
                // header + 8 collapsed + note, none collapsed away.
                assert_eq!(rows.len(), 10);
                let note = rows.last().expect("the note trails the group");
                assert_eq!(note.hint.as_deref(), Some("set OPENROUTER_API_KEY"));
            }
            other => panic!("expected a Ready selector, got {other:?}"),
        }
    }

    #[test]
    fn arrows_move_within_the_filtered_rows_of_a_ready_overlay() {
        // Arrow-only nav (ADR-0046): the wheel no longer reaches the Composer.
        // System A (ADR-0051, qwen `useSelectionList`) WRAPS at the ends - a
        // divergence from the retired saturating Selector.
        let rows = vec![model_row("qwen"), model_row("llama"), model_row("gpt")];
        let mut c = model_selector_ready(rows);
        assert_eq!(
            fold_consumed(&mut c, Key::ArrowDown),
            vec![],
            "arrows drive the overlay, not a scroll"
        );
        assert_eq!(highlight_of(&c), 1);
        fold_consumed(&mut c, Key::ArrowDown);
        assert_eq!(highlight_of(&c), 2);
        fold_consumed(&mut c, Key::ArrowDown);
        assert_eq!(highlight_of(&c), 0, "wraps from the last row to the first");
        fold_consumed(&mut c, Key::ArrowUp);
        assert_eq!(highlight_of(&c), 2, "wraps back to the last");
    }

    #[test]
    fn enter_on_a_ready_overlay_chooses_the_highlighted_row_and_closes() {
        let rows = vec![model_row("qwen"), model_row("llama")];
        let mut c = model_selector_ready(rows);
        // Move to the second row, then Enter.
        fold_consumed(&mut c, Key::ArrowDown);
        assert_eq!(
            fold_consumed(&mut c, Key::Enter),
            vec![Effect::SelectorChosen {
                command: "model".into(),
                value: "llama".into(),
            }]
        );
        // Overlay closed, draft cleared.
        assert_eq!(c.view().draft, "");
        assert_eq!(overlay(&c), None);
    }

    #[test]
    fn enter_selects_the_filtered_highlighted_row() {
        let rows = vec![model_row("qwen"), model_row("llama"), model_row("gpt")];
        let mut c = model_selector_ready(rows);
        // Filter to just "llama" via `rest`, then Enter selects it.
        press(&mut c, typed("ll"));
        assert_eq!(
            fold_consumed(&mut c, Key::Enter),
            vec![Effect::SelectorChosen {
                command: "model".into(),
                value: "llama".into(),
            }]
        );
    }

    #[test]
    fn selector_failed_shows_a_failed_overlay_and_enter_does_nothing() {
        let mut c = slashing("/model");
        let generation = command_generation(&fold_consumed(&mut c, Key::Enter));
        deliver(&mut c, Event::selector_failed(generation, "no server"));
        assert!(matches!(
            overlay(&c),
            Some(OverlayView::Dialog {
                status: OverlayStatus::Failed(_),
                ..
            })
        ));
        // Enter on a Failed overlay does nothing (no rows to pick).
        assert_eq!(fold_consumed(&mut c, Key::Enter), vec![]);
        assert!(matches!(
            overlay(&c),
            Some(OverlayView::Dialog {
                status: OverlayStatus::Failed(_),
                ..
            })
        ));
    }

    #[test]
    fn escape_closes_the_selector_overlay_and_clears_the_draft() {
        let mut c = model_selector_ready(vec![model_row("qwen")]);
        assert_eq!(fold_consumed(&mut c, Key::Escape), vec![]);
        assert_eq!(c.view().draft, "");
        assert_eq!(overlay(&c), None);
    }

    // --- the Frozen (theme) dialog (ADR-0051 System A, filter-less) ---------

    // A theme (Frozen) dialog Ready with `rows` - the mirror of
    // `model_selector_ready` but for `/theme`, which commits to a `Frozen`
    // flavour that swallows editing keys.
    fn theme_selector_ready(rows: Vec<SelectorRow>) -> Composer {
        let mut c = slashing("/theme");
        let generation = command_generation(&fold_consumed(&mut c, Key::Enter));
        deliver(&mut c, Event::selector_ready(generation, rows));
        c
    }

    // The visible dialog rows of a Ready overlay.
    fn dialog_rows_of(c: &Composer) -> Vec<SelectorRow> {
        match overlay(c) {
            Some(OverlayView::Dialog { rows, .. }) => rows,
            other => panic!("expected a Ready selector, got {other:?}"),
        }
    }

    #[test]
    fn a_typed_char_in_a_frozen_theme_dialog_is_swallowed() {
        // Typing in a Frozen dialog must NOT grow the `/theme ` draft nor
        // refilter the rows (qwen's filter-less dialog).
        let mut c = theme_selector_ready(vec![model_row("dark"), model_row("light")]);
        let before = c.clone();
        fold_consumed(&mut c, Key::Char('d'));
        assert_eq!(c.view().draft, "/theme ", "the draft did not grow");
        assert_eq!(
            dialog_rows_of(&c).len(),
            2,
            "the rows were not refiltered by the swallowed char"
        );
        assert_eq!(c, before, "a swallowed char leaves the Composer unchanged");
    }

    #[test]
    fn a_digit_quick_selects_in_a_frozen_theme_dialog() {
        // In a Frozen dialog a digit is a quick-select (not a filter char).
        let mut c = theme_selector_ready(vec![model_row("dark"), model_row("light")]);
        // '2' picks the second row.
        assert_eq!(
            fold_consumed(&mut c, Key::Char('2')),
            vec![Effect::SelectorChosen {
                command: "theme".into(),
                value: "light".into(),
            }]
        );
        assert_eq!(overlay(&c), None, "picking closed the dialog");
    }

    #[test]
    fn backspace_and_cursor_moves_pass_through_a_frozen_theme_dialog() {
        // Frozen swallows text INSERTION, but Backspace-out and cursor moves
        // still work (so the user can back out of the sub-state).
        // A cursor move (Left) is consumed and leaves the draft TEXT intact
        // (only the cursor position changes).
        let mut c = theme_selector_ready(vec![model_row("dark")]);
        let cursor_before = c.view().cursor;
        fold_consumed(&mut c, Key::Left);
        assert_eq!(c.view().draft, "/theme ", "a cursor move never edits text");
        assert_eq!(c.view().cursor, cursor_before - 1, "the cursor moved left");
        // From the trailing-space cursor, Backspace removes the space, dropping
        // back to the menu (rest goes None).
        let mut c = theme_selector_ready(vec![model_row("dark")]);
        fold_consumed(&mut c, Key::Backspace);
        assert_eq!(c.view().draft, "/theme");
        assert!(matches!(overlay(&c), Some(OverlayView::Menu { .. })));
    }

    #[test]
    fn selector_highlight_repoints_to_the_frozen_dialogs_active_row() {
        // The theme live-preview reads `selector_highlight` (the active row of
        // the frozen dialog); arrowing repoints it, so the preview follows.
        let mut c = theme_selector_ready(vec![model_row("dark"), model_row("light")]);
        let (command, row) = c.selector_highlight().expect("a Ready theme dialog");
        assert_eq!(command, "theme");
        assert_eq!(row, &model_row("dark"), "the first row previews first");
        fold_consumed(&mut c, Key::ArrowDown);
        assert_eq!(
            c.selector_highlight().unwrap().1,
            &model_row("light"),
            "arrowing repoints the preview"
        );
    }

    // --- filter_rows travel rules (ADR-0051 group retention) ----------------

    #[test]
    fn filter_rows_travels_a_header_and_notes_when_a_member_matches() {
        // A group whose MEMBER matches keeps its header and its trailing note,
        // even though neither the header nor the note text contains the needle.
        let raw = vec![
            SelectorRow::header("openai"),
            SelectorRow::new("gpt-4", "gpt-4", None),
            SelectorRow::note("  note", Some("info".into())),
        ];
        let out = filter_rows(&raw, "gpt");
        assert_eq!(out.len(), 3, "header + member + note all travel");
        assert_eq!(out[0].role, RowRole::Header);
        assert_eq!(out[2].role, RowRole::Note);
    }

    #[test]
    fn filter_rows_keeps_header_and_notes_on_a_header_match_alone() {
        // The header label itself matches: the whole group's header + notes are
        // shown even if no member matches the needle.
        let raw = vec![
            SelectorRow::header("anthropic"),
            SelectorRow::new("claude", "claude", None),
            SelectorRow::note("  note", Some("info".into())),
        ];
        let out = filter_rows(&raw, "anthropic");
        // header hit → group_hit true → header + note kept; the member does not
        // contain "anthropic" so it is NOT kept (member keeps only on its own).
        let roles: Vec<_> = out.iter().map(|r| r.role).collect();
        assert!(roles.contains(&RowRole::Header));
        assert!(roles.contains(&RowRole::Note));
    }

    #[test]
    fn filter_rows_drops_a_group_that_matches_nothing() {
        let raw = vec![
            SelectorRow::header("openai"),
            SelectorRow::new("gpt-4", "gpt-4", None),
            SelectorRow::header("anthropic"),
            SelectorRow::new("claude", "claude", None),
        ];
        let out = filter_rows(&raw, "claude");
        // Only the anthropic group survives.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].label, "anthropic");
        assert_eq!(out[1].label, "claude");
    }

    #[test]
    fn filter_rows_reveals_a_collapsed_member_by_its_name() {
        // A collapsed (greyed) member is revealed when its own label matches.
        let raw = vec![
            SelectorRow::header("openrouter"),
            SelectorRow::collapsed("openrouter/mixtral"),
            SelectorRow::collapsed("openrouter/llama"),
        ];
        let out = filter_rows(&raw, "mixtral");
        // header travels (member matched) + the matching collapsed row; the
        // non-matching collapsed row is dropped.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, RowRole::Header);
        assert_eq!(out[1].label, "openrouter/mixtral");
    }

    #[test]
    fn a_model_dialog_nav_skips_the_disabled_header_end_to_end() {
        // End-to-end (is_stop → disabled → header-skip): the header is not a
        // cursor stop, so ArrowDown from the first member lands on the next
        // member, jumping the header row between the groups.
        let rows = vec![
            SelectorRow::header("openai"),
            model_row("gpt-4"),
            SelectorRow::header("anthropic"),
            model_row("claude"),
        ];
        let mut c = model_selector_ready(rows);
        // Initial active snaps onto the first navigable row (index 1, the
        // member - index 0 is the disabled header).
        assert_eq!(highlight_of(&c), 1, "active snaps off the header");
        // ArrowDown from the first member skips the second header (index 2) and
        // lands on the second member (index 3).
        fold_consumed(&mut c, Key::ArrowDown);
        assert_eq!(highlight_of(&c), 3, "nav skipped the disabled header");
    }

    // --- the `←/→` expand mechanic (ADR-0051 System B, qwen expandedIndex) --

    #[test]
    fn arrows_expand_a_long_active_palette_row_but_move_the_cursor_on_a_short_one() {
        // A short active row (a command name) never expands: `←/→` fall through
        // to plain cursor moves, so the draft cursor changes and the palette's
        // expanded flag stays false.
        let mut c = slashing("/m");
        let before_cursor = c.view().cursor;
        fold_consumed(&mut c, Key::Left);
        assert_ne!(c.view().cursor, before_cursor, "short row: cursor moved");
        match overlay(&c) {
            Some(OverlayView::Menu { expanded, .. }) => {
                assert!(!expanded, "a short active row never expands")
            }
            other => panic!("expected the palette, got {other:?}"),
        }
    }

    #[test]
    fn backspacing_the_space_returns_to_the_menu_and_reactivation_refetches() {
        let mut c = model_selector_ready(vec![model_row("qwen")]);
        // Backspace removes the trailing space: `/model ` → `/model`, so rest
        // goes None and we are back in the COMMAND MENU (overlay dropped).
        assert_eq!(fold_consumed(&mut c, Key::Backspace), vec![]);
        assert_eq!(c.view().draft, "/model");
        assert!(matches!(overlay(&c), Some(OverlayView::Menu { .. })));
        // Re-committing is a fresh activation: it re-emits Effect::Command,
        // with a NEW generation (the helper's commit was generation 1).
        assert_eq!(
            fold_consumed(&mut c, Key::Enter),
            vec![Effect::Command {
                name: "model".into(),
                generation: 2,
            }]
        );
        assert!(matches!(
            overlay(&c),
            Some(OverlayView::Dialog {
                status: OverlayStatus::Loading,
                ..
            })
        ));
    }

    #[test]
    fn backspacing_the_slash_exits_slash_mode_entirely() {
        let mut c = model_selector_ready(vec![model_row("qwen")]);
        // Erase the whole `/model ` draft: selector → menu → gone.
        for _ in 0.."/model ".chars().count() {
            fold_consumed(&mut c, Key::Backspace);
        }
        assert_eq!(c.view().draft, "");
        assert_eq!(overlay(&c), None, "no longer a slash draft");
    }

    #[test]
    fn a_stale_selector_ready_after_the_overlay_closed_is_ignored() {
        // Commit, then Escape to close the overlay.
        let mut c = slashing("/model");
        let generation = command_generation(&fold_consumed(&mut c, Key::Enter));
        fold_consumed(&mut c, Key::Escape);
        assert_eq!(overlay(&c), None);
        // A late SelectorReady is still CONSUMED (by variant - the caller
        // never sees it) but must not resurrect the popup - even with the
        // matching generation: there is no overlay to fill.
        deliver(
            &mut c,
            Event::selector_ready(generation, vec![model_row("qwen")]),
        );
        assert_eq!(overlay(&c), None, "stale event ignored");
    }

    #[test]
    fn selector_ready_is_ignored_when_no_overlay_is_loading() {
        // No slash draft at all: the event is consumed but changes nothing.
        let mut c = fresh();
        deliver(&mut c, Event::selector_ready(1, vec![model_row("qwen")]));
        assert_eq!(overlay(&c), None);
    }

    #[test]
    fn a_second_selector_ready_does_not_overwrite_a_ready_overlay() {
        // Guard: once Ready, a duplicate delivery must not reset the cursor.
        // The duplicate carries the SAME generation (1, the helper's commit),
        // so it is the Loading check alone that absorbs it.
        let mut c = model_selector_ready(vec![model_row("qwen"), model_row("llama")]);
        fold_consumed(&mut c, Key::ArrowDown);
        assert_eq!(highlight_of(&c), 1);
        // A second (stale) ready arrives - the overlay is no longer Loading.
        deliver(&mut c, Event::selector_ready(1, vec![model_row("gpt")]));
        match overlay(&c) {
            Some(OverlayView::Dialog { rows, active, .. }) => {
                assert_eq!(rows.len(), 2, "kept the first delivery");
                assert_eq!(active, 1, "cursor untouched");
            }
            other => panic!("expected Ready selector, got {other:?}"),
        }
    }

    #[test]
    fn a_fill_from_a_previous_activation_never_lands_on_a_fresh_overlay() {
        // The race the generation exists for: backspace out of a Loading
        // sub-state, re-commit, and only THEN the first fetch delivers.
        let mut c = slashing("/model");
        let first = command_generation(&fold_consumed(&mut c, Key::Enter));
        // Backspace removes the trailing space: back to the MENU, overlay
        // dropped on the next consumed key. Re-commit: a SECOND activation.
        fold_consumed(&mut c, Key::Backspace);
        let second = command_generation(&fold_consumed(&mut c, Key::Enter));
        assert_ne!(first, second, "each activation gets its own generation");
        // The FIRST activation's fill finally arrives: dropped - the fresh
        // overlay stays Loading, it never asked for these rows.
        deliver(
            &mut c,
            Event::selector_ready(first, vec![model_row("stale")]),
        );
        assert!(matches!(
            overlay(&c),
            Some(OverlayView::Dialog {
                status: OverlayStatus::Loading,
                ..
            })
        ));
        // The SECOND activation's own fill lands.
        deliver(
            &mut c,
            Event::selector_ready(second, vec![model_row("qwen")]),
        );
        match overlay(&c) {
            Some(OverlayView::Dialog {
                status: OverlayStatus::Ready,
                rows,
                ..
            }) => assert_eq!(rows, vec![model_row("qwen")], "the second fetch's rows"),
            other => panic!("expected Ready selector, got {other:?}"),
        }
    }

    #[test]
    fn a_failure_echo_applies_only_to_its_own_activation() {
        // Same backspace-out-and-re-commit race, failure edition.
        let mut c = slashing("/model");
        let first = command_generation(&fold_consumed(&mut c, Key::Enter));
        fold_consumed(&mut c, Key::Backspace);
        let second = command_generation(&fold_consumed(&mut c, Key::Enter));
        // The first activation's failure is stale: dropped, still Loading.
        deliver(&mut c, Event::selector_failed(first, "no server"));
        assert!(matches!(
            overlay(&c),
            Some(OverlayView::Dialog {
                status: OverlayStatus::Loading,
                ..
            })
        ));
        // The second activation's own failure lands.
        deliver(&mut c, Event::selector_failed(second, "no server"));
        assert!(matches!(
            overlay(&c),
            Some(OverlayView::Dialog {
                status: OverlayStatus::Failed(_),
                ..
            })
        ));
    }

    #[test]
    fn events_other_than_the_selector_fills_are_refused_untouched() {
        // The event first-refusal contract: everything that is not an
        // overlay fill comes back exactly as offered.
        let mut c = fresh();
        let event = Event::run_started("r1");
        match c.apply_event(event.clone()) {
            EventOutcome::Refused(back) => assert_eq!(back, event),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    // --- Submit vs Steer (the Status parameter) ------------------------------

    #[test]
    fn enter_with_a_blank_draft_is_consumed_as_a_no_op() {
        // Consumed, not refused: Enter is never the caller's key.
        let mut c = with_draft("   ", 3);
        assert_eq!(fold_consumed(&mut c, Key::Enter), vec![]);
    }

    #[test]
    fn enter_submits_the_trimmed_prompt_when_idle() {
        let mut c = with_draft("  fix the bug  ", 15);
        assert_eq!(
            fold_consumed(&mut c, Key::Enter),
            vec![Effect::Agent(AgentCommand::Submit("fix the bug".into()))]
        );
    }

    #[test]
    fn enter_steers_when_running_instead_of_submitting() {
        let mut c = with_draft("  also check the README  ", 10);
        match c.handle_key(UngatedKey::for_test(Key::Enter), Status::Running) {
            KeyOutcome::Consumed { effects, .. } => assert_eq!(
                effects,
                vec![Effect::Agent(AgentCommand::Steer(
                    "also check the README".into()
                ))]
            ),
            other => panic!("expected a consumed steer, got {other:?}"),
        }
    }

    // --- The refusal set (the routing contract's "not mine" rows) -----------
    //
    // The refusal CONTRACT: a Refused key hands back the SAME key and leaves
    // the Composer BIT-IDENTICAL (`PartialEq` over the whole state - history
    // ring and stash included). Checked across tricky states on purpose: a
    // mid-recall ring with a stashed draft, and a leftover selector after
    // backspacing out of the sub-state - the states where a stray mutation on
    // the refusal path would hide.

    // Asserts the refusal contract for `key` against a clone of `state`.
    fn assert_refusal_is_pure(state: &Composer, key: Key, status: Status) {
        let mut c = state.clone();
        let outcome = c.handle_key(UngatedKey::for_test(key.clone()), status);
        assert_eq!(outcome, KeyOutcome::Refused(key), "expected a refusal");
        assert_eq!(
            &c, state,
            "a refused key must leave the Composer bit-identical"
        );
    }

    // A Composer parked mid-recall: "typing..." stashed, "b" recalled.
    fn mid_recall() -> Composer {
        let mut c = with_history(&["a", "b"]);
        set_draft(&mut c, "typing...", 9);
        fold_consumed(&mut c, Key::ArrowUp);
        assert_eq!(c.view().draft, "b");
        c
    }

    // A Composer back in the MENU with the selector overlay left over from
    // backspacing out of the sub-state. The leftover is dropped lazily, on
    // the next CONSUMED menu key - never on a refusal.
    fn leftover_selector() -> Composer {
        let mut c = model_selector_ready(vec![model_row("qwen")]);
        fold_consumed(&mut c, Key::Backspace); // `/model ` → `/model`: menu again
        assert!(c.selector.is_open(), "the overlay must linger for the test");
        c
    }

    #[test]
    fn always_refused_keys_leave_any_composer_bit_identical() {
        let states = [
            fresh(),
            with_draft("mid-draft", 3),
            slashing("/model"),
            mid_recall(),
            leftover_selector(),
        ];
        for state in &states {
            for key in [
                Key::PageUp,
                Key::PageDown,
                Key::ToggleCompact,
                Key::Named("f1".into()),
                Key::Other,
            ] {
                assert_refusal_is_pure(state, key, Status::Idle);
            }
        }
    }

    #[test]
    fn escape_and_the_wheel_are_refused_pure_when_no_overlay_is_open() {
        // With no overlay, Escape belongs to Cancellation; the wheel is inert
        // (ADR-0046: no mouse capture, native scrollback owns history), so the
        // Composer refuses both to the caller, whatever the draft or ring holds.
        for state in [fresh(), with_draft("plain draft", 5), mid_recall()] {
            for key in [Key::Escape, Key::WheelUp, Key::WheelDown] {
                assert_refusal_is_pure(&state, key, Status::Running);
            }
        }
        // The ring outlives a refusal: Down after a refused Escape still
        // restores the stash (the coverage the old toggle-cross-contamination
        // test held).
        let mut c = mid_recall();
        assert!(matches!(
            c.handle_key(UngatedKey::for_test(Key::Escape), Status::Running),
            KeyOutcome::Refused(_)
        ));
        fold_consumed(&mut c, Key::ArrowDown);
        assert_eq!(c.view().draft, "typing...");
    }

    // --- The submitted/steered outcome hooks --------------------------------

    #[test]
    fn submitted_ok_records_clears_and_emits_the_on_disk_append() {
        let mut c = with_history(&["a"]);
        set_draft(&mut c, "b", 1);
        // The in-memory record and the on-disk append are one invariant.
        assert_eq!(c.submitted_ok("b"), vec![Effect::HistoryAppend("b".into())]);
        assert_eq!(c.view().draft, "");
        assert_eq!(c.view().cursor, 0);
        // Recorded AND the recall position reset: Up walks b, then a.
        fold_consumed(&mut c, Key::ArrowUp);
        assert_eq!(c.view().draft, "b");
        fold_consumed(&mut c, Key::ArrowUp);
        assert_eq!(c.view().draft, "a");
    }

    #[test]
    fn steered_ok_clears_the_draft_without_recording() {
        let mut c = fresh();
        press(&mut c, typed("mid-turn note"));
        c.steered_ok();
        assert_eq!(c.view().draft, "");
        // Not a prompt: nothing recorded, so Up recalls nothing.
        assert_eq!(fold_consumed(&mut c, Key::ArrowUp), vec![]);
        assert_eq!(c.view().draft, "");
    }

    // --- Draft editing -------------------------------------------------------

    #[test]
    fn a_typed_char_appends_at_the_end_of_the_draft() {
        let mut c = fresh();
        press(&mut c, typed("hi"));
        assert_eq!(c.view().draft, "hi");
        assert_eq!(c.view().cursor, 2);
    }

    #[test]
    fn a_typed_char_inserts_at_the_cursor_mid_draft() {
        let mut c = with_draft("hllo", 1);
        press(&mut c, vec![Key::Char('e')]);
        assert_eq!(c.view().draft, "hello");
        assert_eq!(c.view().cursor, 2);
    }

    #[test]
    fn backspace_deletes_the_char_before_the_cursor() {
        let mut c = with_draft("hello", 3);
        press(&mut c, vec![Key::Backspace]);
        assert_eq!(c.view().draft, "helo");
        assert_eq!(c.view().cursor, 2);
    }

    #[test]
    fn backspace_at_the_start_of_the_draft_is_a_noop() {
        let mut c = with_draft("hi", 0);
        press(&mut c, vec![Key::Backspace]);
        assert_eq!(c.view().draft, "hi");
        assert_eq!(c.view().cursor, 0);
    }

    // The cursor is a CHAR index: multi-byte chars must neither split nor
    // panic under insert/delete around them.
    #[test]
    fn multibyte_chars_insert_and_delete_without_splitting() {
        let mut c = with_draft("héllo", 2);
        press(&mut c, vec![Key::Char('🎩')]);
        assert_eq!(c.view().draft, "hé🎩llo");
        assert_eq!(c.view().cursor, 3);

        press(&mut c, vec![Key::Backspace, Key::Backspace]);
        assert_eq!(c.view().draft, "hllo");
        assert_eq!(c.view().cursor, 1);
    }

    #[test]
    fn left_and_right_move_the_cursor_clamped_at_both_ends() {
        let mut c = with_draft("ab", 1);
        press(&mut c, vec![Key::Left]);
        assert_eq!(c.view().cursor, 0);
        press(&mut c, vec![Key::Left]);
        assert_eq!(c.view().cursor, 0); // clamped at the start

        press(&mut c, vec![Key::Right, Key::Right]);
        assert_eq!(c.view().cursor, 2);
        press(&mut c, vec![Key::Right]);
        assert_eq!(c.view().cursor, 2); // clamped at the end
        assert_eq!(c.view().draft, "ab"); // movement never edits
    }

    #[test]
    fn home_and_end_jump_within_the_current_line_not_the_whole_draft() {
        // "ab\ncdef\ng", cursor mid second line (index 5, on 'e').
        let mut c = with_draft("ab\ncdef\ng", 5);
        press(&mut c, vec![Key::Home]);
        assert_eq!(c.view().cursor, 3); // start of "cdef"
        press(&mut c, vec![Key::End]);
        assert_eq!(c.view().cursor, 7); // end of "cdef", before its '\n'
    }

    #[test]
    fn home_and_end_on_a_single_line_draft_reach_both_ends() {
        let mut c = with_draft("hello", 3);
        press(&mut c, vec![Key::Home]);
        assert_eq!(c.view().cursor, 0);
        press(&mut c, vec![Key::End]);
        assert_eq!(c.view().cursor, 5);
    }

    #[test]
    fn insert_newline_adds_a_hard_newline_at_the_cursor() {
        let mut c = with_draft("ab", 1);
        assert_eq!(fold_consumed(&mut c, Key::InsertNewline), vec![]);
        assert_eq!(c.view().draft, "a\nb");
        assert_eq!(c.view().cursor, 2);
    }

    #[test]
    fn enter_on_a_trailing_backslash_continues_the_draft_instead_of_submitting() {
        let mut c = with_draft("first line\\", 11);
        assert_eq!(fold_consumed(&mut c, Key::Enter), vec![]);
        assert_eq!(c.view().draft, "first line\n");
        assert_eq!(c.view().cursor, 11); // cursor to the end
    }

    #[test]
    fn enter_on_a_trailing_backslash_continues_while_running_too() {
        let mut c = with_draft("steer me\\", 9);
        match c.handle_key(UngatedKey::for_test(Key::Enter), Status::Running) {
            KeyOutcome::Consumed { effects, .. } => assert_eq!(effects, vec![]),
            other => panic!("expected a consumed continuation, got {other:?}"),
        }
        assert_eq!(c.view().draft, "steer me\n");
    }

    // Only a LITERAL trailing backslash - the LAST char of the draft -
    // triggers the continuation.
    #[test]
    fn a_backslash_anywhere_else_still_submits() {
        let mut c = with_draft("a\\b", 3);
        assert_eq!(
            fold_consumed(&mut c, Key::Enter),
            vec![Effect::Agent(AgentCommand::Submit("a\\b".into()))]
        );

        // Trailing whitespace after the backslash: the backslash is not the
        // last char, so Enter submits (trimmed).
        let mut c = with_draft("a\\ ", 3);
        assert_eq!(
            fold_consumed(&mut c, Key::Enter),
            vec![Effect::Agent(AgentCommand::Submit("a\\".into()))]
        );
    }

    #[test]
    fn enter_submits_a_multi_line_draft_whole() {
        let mut c = with_draft("first\nsecond", 12);
        assert_eq!(
            fold_consumed(&mut c, Key::Enter),
            vec![Effect::Agent(AgentCommand::Submit("first\nsecond".into()))]
        );
    }

    // --- Prompt history ------------------------------------------------------
    //
    // Ring internals (dedup, cap, the one-stash rule) are covered in
    // `ui::history`; these pin the Composer's wiring - seeding, the recall
    // landing spot, and the edge-triggered Up/Down on multi-line drafts.

    #[test]
    fn new_seeds_the_ring_oldest_first() {
        // Seeded oldest-first and parked: the first Up recalls the newest.
        let mut c = with_history(&["a", "b", "c"]);
        fold_consumed(&mut c, Key::ArrowUp);
        assert_eq!(c.view().draft, "c");
    }

    #[test]
    fn arrow_up_from_empty_history_is_a_consumed_no_op() {
        let mut c = fresh();
        assert_eq!(fold_consumed(&mut c, Key::ArrowUp), vec![]);
        assert_eq!(c.view().draft, "");
    }

    #[test]
    fn arrow_up_moves_backward_saving_draft() {
        let mut c = with_history(&["a", "b", "c"]);
        set_draft(&mut c, "typing...", 9);

        assert_eq!(fold_consumed(&mut c, Key::ArrowUp), vec![]);
        assert_eq!(c.view().draft, "c");

        fold_consumed(&mut c, Key::ArrowUp);
        assert_eq!(c.view().draft, "b");

        fold_consumed(&mut c, Key::ArrowUp);
        assert_eq!(c.view().draft, "a");

        fold_consumed(&mut c, Key::ArrowUp);
        assert_eq!(c.view().draft, "a"); // at the oldest - a no-op
    }

    #[test]
    fn arrow_down_from_parked_does_nothing() {
        let mut c = with_history(&["a", "b"]);
        assert_eq!(fold_consumed(&mut c, Key::ArrowDown), vec![]);
        assert_eq!(c.view().draft, "");
    }

    #[test]
    fn arrow_down_moves_forward_restoring_draft_at_end() {
        let mut c = with_history(&["a", "b", "c"]);

        fold_consumed(&mut c, Key::ArrowUp);
        assert_eq!(c.view().draft, "c");

        fold_consumed(&mut c, Key::ArrowUp);
        assert_eq!(c.view().draft, "b");

        assert_eq!(fold_consumed(&mut c, Key::ArrowDown), vec![]);
        assert_eq!(c.view().draft, "c");

        fold_consumed(&mut c, Key::ArrowDown);
        assert_eq!(c.view().draft, ""); // past the newest: the empty draft returns
    }

    #[test]
    fn arrow_down_from_oldest_restores_draft_off_the_end() {
        let mut c = with_history(&["a", "b"]);
        set_draft(&mut c, "my draft", 8);

        press(&mut c, vec![Key::ArrowUp, Key::ArrowUp, Key::ArrowUp]);
        assert_eq!(c.view().draft, "a");

        fold_consumed(&mut c, Key::ArrowDown);
        assert_eq!(c.view().draft, "b");

        fold_consumed(&mut c, Key::ArrowDown);
        assert_eq!(c.view().draft, "my draft"); // stash restored off the end
    }

    // --- edge-triggered history (Up/Down on a multi-line draft) --------------

    #[test]
    fn arrow_up_off_the_first_line_moves_the_cursor_not_history() {
        // Cursor on the second line: Up is cursor movement, history untouched.
        let mut c = with_history(&["old"]);
        set_draft(&mut c, "ab\ncd", 4); // on 'd' (line 1, col 1)
        assert_eq!(fold_consumed(&mut c, Key::ArrowUp), vec![]);
        assert_eq!(c.view().draft, "ab\ncd"); // draft intact - no recall happened
        assert_eq!(c.view().cursor, 1); // line 0, col 1
    }

    #[test]
    fn arrow_up_on_the_first_line_of_a_multi_line_draft_recalls_history() {
        let mut c = with_history(&["old"]);
        set_draft(&mut c, "ab\ncd", 1); // line 0
        fold_consumed(&mut c, Key::ArrowUp);
        assert_eq!(c.view().draft, "old");
        assert_eq!(c.view().cursor, 3); // recall puts the cursor at the end
        // The multi-line draft was stashed: Down off the recalled entry's end
        // restores it.
        fold_consumed(&mut c, Key::ArrowDown);
        assert_eq!(c.view().draft, "ab\ncd");
    }

    #[test]
    fn arrow_down_off_the_last_line_moves_the_cursor_not_history() {
        let mut c = with_history(&["old"]);
        set_draft(&mut c, "ab\ncd", 1); // line 0, col 1 - not the last line
        assert_eq!(fold_consumed(&mut c, Key::ArrowDown), vec![]);
        assert_eq!(c.view().draft, "ab\ncd"); // draft intact - cursor moved, no recall
        assert_eq!(c.view().cursor, 4); // line 1, col 1
    }

    #[test]
    fn arrow_down_on_the_last_line_of_a_multi_line_draft_recalls_history() {
        // Recall history, then Down from the recalled entry's last line
        // restores the stashed draft - the pre-multi-line behavior.
        let mut c = with_history(&["old"]);
        set_draft(&mut c, "draft", 5);
        fold_consumed(&mut c, Key::ArrowUp);
        assert_eq!(c.view().draft, "old");
        fold_consumed(&mut c, Key::ArrowDown);
        assert_eq!(c.view().draft, "draft");
    }

    #[test]
    fn up_and_down_clamp_the_column_to_the_target_lines_length() {
        // "long line\nab\nlonger": from the end of "longer", Up lands at the
        // end of the shorter "ab"; Up again keeps col 2 into "long line".
        let mut c = with_draft("long line\nab\nlonger", 19);
        fold_consumed(&mut c, Key::ArrowUp);
        assert_eq!(c.view().cursor, 12); // end of "ab" (col clamped 6 → 2)
        fold_consumed(&mut c, Key::ArrowUp);
        assert_eq!(c.view().cursor, 2); // "long line", col 2
        fold_consumed(&mut c, Key::ArrowDown);
        assert_eq!(c.view().cursor, 12); // back down: "ab" clamps col 2 → 2
    }

    // =======================================================================
    // Layout math.
    // =======================================================================

    fn rows(value: &str, width: usize) -> Vec<String> {
        layout(value, 0, width).rows
    }

    fn cursor(value: &str, cursor: usize, width: usize) -> (usize, usize) {
        let l = layout(value, cursor, width);
        (l.cursor_row, l.cursor_col)
    }

    // --- wrapping ------------------------------------------------------------

    #[test]
    fn an_empty_draft_is_one_empty_row_with_the_cursor_at_the_origin() {
        let l = layout("", 0, 10);
        assert_eq!(l.rows, vec![String::new()]);
        assert_eq!((l.cursor_row, l.cursor_col), (0, 0));
    }

    #[test]
    fn a_short_draft_is_a_single_row() {
        assert_eq!(rows("hello", 10), vec!["hello"]);
    }

    #[test]
    fn hard_newlines_split_rows_and_empty_lines_survive_as_blank_rows() {
        assert_eq!(rows("a\n\nb", 10), vec!["a", "", "b"]);
    }

    #[test]
    fn a_long_line_wraps_at_the_width_char_by_char() {
        assert_eq!(rows("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn an_exact_multiple_line_gains_the_empty_row_the_cursor_lands_on() {
        // "abcd" at width 4 fills its row exactly; the cursor at its end (4)
        // needs the next row's first cell, so that row exists.
        assert_eq!(rows("abcd", 4), vec!["abcd", ""]);
        assert_eq!(cursor("abcd", 4, 4), (1, 0));
    }

    #[test]
    fn hard_newlines_and_wrapping_compose() {
        assert_eq!(rows("abcdef\nxy", 4), vec!["abcd", "ef", "xy"]);
    }

    #[test]
    fn multibyte_chars_wrap_by_char_count_not_bytes() {
        assert_eq!(rows("héllo wörld", 6), vec!["héllo ", "wörld"]);
        assert_eq!(cursor("héllo wörld", 11, 6), (1, 5));
    }

    #[test]
    fn a_degenerate_zero_width_is_treated_as_one() {
        assert_eq!(rows("ab", 0), vec!["a", "b", ""]);
    }

    // --- cursor position -------------------------------------------------------

    #[test]
    fn the_cursor_at_start_middle_and_end_of_one_row() {
        assert_eq!(cursor("hello", 0, 10), (0, 0));
        assert_eq!(cursor("hello", 3, 10), (0, 3));
        assert_eq!(cursor("hello", 5, 10), (0, 5));
    }

    #[test]
    fn the_cursor_on_a_wrapped_continuation_row() {
        // "abcdefghij" at width 4: rows abcd / efgh / ij.
        assert_eq!(cursor("abcdefghij", 4, 4), (1, 0));
        assert_eq!(cursor("abcdefghij", 7, 4), (1, 3));
        assert_eq!(cursor("abcdefghij", 10, 4), (2, 2));
    }

    #[test]
    fn the_cursor_on_hard_newline_rows() {
        // "ab\ncd\nef": the cursor ON the '\n' is the end of the line before.
        assert_eq!(cursor("ab\ncd\nef", 2, 10), (0, 2));
        assert_eq!(cursor("ab\ncd\nef", 3, 10), (1, 0));
        assert_eq!(cursor("ab\ncd\nef", 5, 10), (1, 2));
        assert_eq!(cursor("ab\ncd\nef", 8, 10), (2, 2));
    }

    #[test]
    fn a_wide_draft_of_hard_lines_and_wraps_places_the_cursor_exactly() {
        // width 4: "abcdef" wraps to abcd/ef (rows 0-1), "" is row 2,
        // "ghijklm" wraps to ghij/klm (rows 3-4).
        let value = "abcdef\n\nghijklm";
        assert_eq!(rows(value, 4), vec!["abcd", "ef", "", "ghij", "klm"]);
        assert_eq!(cursor(value, 6, 4), (1, 2)); // end of "abcdef"
        assert_eq!(cursor(value, 7, 4), (2, 0)); // the empty line
        assert_eq!(cursor(value, 12, 4), (4, 0)); // start of "klm"
        assert_eq!(cursor(value, 15, 4), (4, 3)); // end of the draft
    }

    #[test]
    fn the_cursor_column_is_always_inside_the_width() {
        for cur in 0..=12 {
            let l = layout("abcd\nefghijkl", cur, 4);
            assert!(
                l.cursor_col < 4,
                "cursor {cur} produced col {}",
                l.cursor_col
            );
            assert!(l.cursor_row < l.rows.len());
        }
    }

    #[test]
    fn a_cursor_past_the_draft_clamps_to_the_end() {
        assert_eq!(cursor("hi", 99, 10), (0, 2));
    }

    // --- edit/render agreement ------------------------------------------------
    //
    // The property this whole extraction exists to pin: the render path
    // (`layout`) and the edit path (`ui::draft`) resolve the cursor to the
    // SAME cell for the same `(draft, cursor)`. At a width wide enough that
    // every hard line fits in one row, a `layout` cell maps back to logical
    // geometry exactly: `cursor_row` IS the logical line and `cursor_col` the
    // column. That is `draft::line_col` - and running it back through
    // `draft::cursor_at` must return the original cursor.

    fn agree(value: &str) {
        // Width past the longest hard line, so one row == one logical line.
        let width = value.chars().count() + 1;
        let chars = value.chars().count();
        for cursor in 0..=chars {
            let l = layout(value, cursor, width);
            // Render side, read as (logical line, column).
            let (render_line, render_col) = (l.cursor_row, l.cursor_col);
            // Edit side, from the shared owner.
            let (edit_line, edit_col) = draft::line_col(value, cursor);
            assert_eq!(
                (render_line, render_col),
                (edit_line, edit_col),
                "render/edit cursor disagree for value={value:?} cursor={cursor}"
            );
            // And the shared inverse returns exactly where we started.
            assert_eq!(
                draft::cursor_at(value, edit_line, edit_col),
                cursor,
                "cursor_at round trip failed for value={value:?} cursor={cursor}"
            );
        }
    }

    #[test]
    fn render_and_edit_agree_on_an_empty_draft() {
        agree("");
    }

    #[test]
    fn render_and_edit_agree_with_the_cursor_at_the_end() {
        agree("hello");
    }

    #[test]
    fn render_and_edit_agree_across_a_trailing_hard_newline() {
        agree("hello\n");
    }

    #[test]
    fn render_and_edit_agree_across_consecutive_newlines() {
        agree("a\n\n\nb");
    }

    #[test]
    fn render_and_edit_agree_on_an_empty_line_between_two_newlines() {
        // The cursor sitting alone on the blank middle line.
        let value = "a\n\nb";
        let empty_line_cursor = draft::cursor_at(value, 1, 0);
        let l = layout(value, empty_line_cursor, 10);
        assert_eq!((l.cursor_row, l.cursor_col), (1, 0));
        assert_eq!(draft::line_col(value, empty_line_cursor), (1, 0));
        agree(value);
    }

    #[test]
    fn render_and_edit_agree_on_multibyte_chars() {
        // Char index 2 sits after "hé" (é is two bytes): both sides must read
        // that in chars, not bytes.
        agree("héllo wörld");
        let l = layout("héllo", 2, 10);
        assert_eq!((l.cursor_row, l.cursor_col), (0, 2));
        assert_eq!(draft::line_col("héllo", 2), (0, 2));
    }

    #[test]
    fn render_and_edit_agree_when_a_wrapped_row_carries_the_cursor() {
        // Narrow width forces wrapping: the render side divides the logical
        // column into row/col, but the logical column it divides is still the
        // one the edit side would compute.
        let value = "abcdefghij\nkl";
        let chars = value.chars().count();
        for cursor in 0..=chars {
            let l = layout(value, cursor, 4);
            let (line, col) = draft::line_col(value, cursor);
            // The visual col must be the logical col modulo the width, and the
            // visual row must advance by the logical col / width beyond the
            // line's base row.
            assert_eq!(l.cursor_col, col % 4);
            assert!(l.cursor_col < 4);
            // Round-trips through the shared inverse regardless of wrapping.
            assert_eq!(draft::cursor_at(value, line, col), cursor);
        }
    }

    // --- height cap and internal scroll ---------------------------------------

    #[test]
    fn max_visible_rows_is_a_third_of_the_terminal_capped_at_eight() {
        assert_eq!(max_visible_rows(60), 8); // 60/3 = 20, capped
        assert_eq!(max_visible_rows(24), 8); // 24/3 = 8, exactly the cap
        assert_eq!(max_visible_rows(12), 4);
        assert_eq!(max_visible_rows(3), 1);
        assert_eq!(max_visible_rows(0), 1); // never starves to zero
    }

    #[test]
    fn first_visible_row_pins_the_cursor_to_the_bottom_of_the_box() {
        assert_eq!(first_visible_row(9, 4), 6); // cursor on the box's last row
        assert_eq!(first_visible_row(6, 4), 3);
    }

    #[test]
    fn first_visible_row_shows_the_top_while_the_cursor_fits() {
        assert_eq!(first_visible_row(0, 4), 0);
        assert_eq!(first_visible_row(3, 4), 0);
        assert_eq!(first_visible_row(0, 0), 0); // degenerate box
    }
}
