//! UI Screen - the pure fold root of the TUI (ADR-0001, The Elm Architecture;
//! ADR-0034).
//!
//! The Screen owns everything the terminal shows: the Transcript (the
//! display-side history, behind [`crate::ui::transcript`]'s store seam), the
//! Composer, the Approval modal, the Agent status, and the status-bar figures.
//! No terminal, no async, no IO: [`Screen::apply_event`] folds agent
//! [`Event`]s in and [`Screen::handle_key`] folds key presses in, each
//! returning a new `Screen` plus a `Vec<Effect>` the adapter carries out. This
//! is the seam ADR-0001 asks for: the ratatui view (`ui.rs`) is one adapter
//! over this core, and the test suite is the second.
//!
//! ## Rules encoded here
//!
//! * The display history is the Transcript STORE's (ADR-0034): every event arm
//!   that shows something delegates one store verb. The store holds the
//!   history's invariants (appends never bump the revision, Tool Result
//!   pairing by id, the tool-display swap on a Tool Result); this fold holds the
//!   choreography - which event means which verb - and the Voice: the startup
//!   Header, stop reasons, cancellation notes, and wave lines are authored
//!   HERE and recorded through the store.
//! * Enter submits when idle and STEERS when running (the Composer never
//!   locks). The submit/steer race at the Run boundary is retried the other
//!   way via [`Screen::submitted`] and [`Screen::steered`] - the retry lives
//!   here because it touches only Agent status, and the draft must survive it.
//! * The Composer is its own module (ADR-0034): [`crate::ui::composer`] owns
//!   the draft, the overlays, and the prompt-history ring. Keys and events
//!   route in a FIXED ORDER - the Approval gate first, then the Composer's
//!   first refusal, then this fold's own arms (scroll, toggles,
//!   Escape-as-Cancellation) - so a consumed key never reaches the arms
//!   below and a refused one comes back untouched.
//! * A pending Approval swallows every key except `y`, `n`, `a`, and `Escape`;
//!   `a` is approve-always (Standing Approval); Escape means Cancellation,
//!   which wins over the Approval.

use crate::approvals::ApprovalMode;
use crate::conversation::compaction_target;
use crate::event::Event;
use crate::llm::response::StopReason;
use crate::tool::caps::Question;
use crate::ui::composer::{Composer, EventOutcome, KeyOutcome};
use crate::ui::selection::{SelectionKey, SelectionList, SelectionOutcome};
use crate::ui::transcript::Transcript;
use crate::view_model::Tone;
use crate::view_model::{DiffHunk, DiffLine, DiffSide, TranscriptItem};

/// The brand title the startup [`TranscriptItem::Header`] shows, bold in the
/// accent colour after the `>_` prompt glyph (qwen `Header` `>_ Qwen Code`).
const HEADER_TITLE: &str = "suspenders";

/// The startup tips (qwen `Tips`): a small faithful registry, each accurate to
/// suspenders' real keybindings (Enter submits, Esc cancels a running turn,
/// Ctrl-O toggles compact mode, Ctrl-C quits). A fresh Screen shows one, picked
/// deterministically by [`pick_startup_tip`] (the pure core has no RNG/clock).
const STARTUP_TIPS: &[&str] = &[
    "Type / to see all available commands.",
    "Use @path/to/file to add files as context.",
    "Press Esc to cancel a running turn.",
    "Press Ctrl-O to toggle compact mode; Ctrl-C to quit.",
];

/// Picks a startup tip deterministically from [`STARTUP_TIPS`] by `seed`: the
/// pure core has no RNG or wall clock (ADR-0019), so the adapter injects a seed
/// (the prompt-history length at launch) and this indexes into the registry.
/// Never panics - the registry is non-empty and the modulo keeps the index in
/// range; the guard is defensive should the registry ever be emptied.
fn pick_startup_tip(seed: usize) -> &'static str {
    if STARTUP_TIPS.is_empty() {
        return "Type / to see all available commands.";
    }
    STARTUP_TIPS[seed % STARTUP_TIPS.len()]
}

/// The initial cumulative session cost before any priced response arrives.
const INITIAL_SESSION_COST: f64 = 0.0;

/// The semantic pressure level (ADR-0008): how full the live context window is,
/// computed against the Eviction marks. `Ok` below the low-water mark,
/// `Elevated` between it and the target, `Critical` above the target. The view
/// maps it to color/emphasis; the core only names the level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PressureLevel {
    #[default]
    Ok,
    Elevated,
    Critical,
}

/// The Agent's status as the Screen sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    Idle,
    Running,
}

/// Which confirmation shape a pending Approval wears (ADR-0049), derived from
/// the confirming ToolCall's name: `run_command` reads `Exec` (the command is
/// arbitrary code), `web_fetch` reads `Info` (a plain proceed prompt). Drives
/// the inline block's question line - `Allow execution of: '{command}'?` for
/// `Exec`, `Do you want to proceed?` for `Info` - both offering the same three
/// options. Edit/plan/mcp are future exhaustive arms (stubbed generic today via
/// the fallback in [`ConfirmKind::from_tool_name`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmKind {
    /// A shell/exec confirmation (`run_command`).
    Exec,
    /// A generic proceed confirmation (`web_fetch`, and the fallback).
    Info,
}

impl ConfirmKind {
    /// The ConfirmKind for a gated tool name (ADR-0049). Only `run_command` and
    /// `web_fetch` gate today (`approvals::GATED`); everything else falls back
    /// to the generic `Info` prompt, so a future gated tool never renders an
    /// empty block.
    pub fn from_tool_name(name: &str) -> ConfirmKind {
        match name {
            "run_shell_command" => ConfirmKind::Exec,
            _ => ConfirmKind::Info,
        }
    }
}

/// A pending Approval (ADR-0049): the id to resolve, the command/URL shown, the
/// confirmation shape, and the pure [`SelectionList`] the radio rows drive. The
/// three options are fixed (`Yes, allow once` / `Always allow in this project` /
/// `No, suggest changes (esc)`), so the list is always length 3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    pub approval_id: String,
    pub command: String,
    pub kind: ConfirmKind,
    /// The radio selection state (active row, digit quick-select buffer).
    pub selection: SelectionList,
}

/// The number of Approval options (`Yes, allow once` / `Always allow in this
/// project` / `No, suggest changes (esc)`) - the fixed length of a pending
/// Approval's [`SelectionList`].
pub const APPROVAL_OPTION_COUNT: usize = 3;

/// The auto-appended "Other" row label (ADR-0057, qwen `askUserQuestion`): every
/// question ALWAYS offers this on top of its own options, so the user can answer
/// free-form. Selecting it focuses the composer and the next submit fills the
/// answer.
pub const OTHER_OPTION_LABEL: &str = "Other";

/// A pending question round-trip (ADR-0057, qwen `ask_user_question`): the id to
/// resolve, the [`Question`]s to render, and the per-question selection state.
/// Runs parallel to [`PendingApproval`] but has NO auto/standing path - every
/// question opens a modal.
///
/// The modal walks the questions in order: `cursor` is the current question
/// index; `per_question[i]` is that question's radio (its options PLUS the
/// auto-appended "Other" row); `answers[i]` holds the recorded answer once given.
/// `collecting_other` is `Some(i)` while the composer is capturing a free-form
/// answer for question `i` (the user picked "Other"); the next composer submit
/// fills `answers[i]` and advances the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingQuestion {
    pub question_id: String,
    pub questions: Vec<Question>,
    /// The current question index (`0..questions.len()`); when it reaches
    /// `questions.len()` every question is answered and the modal resolves.
    pub cursor: usize,
    /// Per-question radios, each `options.len() + 1` rows (the trailing "Other").
    pub per_question: Vec<SelectionList>,
    /// `Some(i)` while the composer captures a free-form answer for question `i`.
    pub collecting_other: Option<usize>,
    /// The recorded answer per question (`None` until answered).
    pub answers: Vec<Option<String>>,
}

impl PendingQuestion {
    /// Builds the modal state for a `question_request`: one radio per question
    /// (its options plus the auto-appended "Other" row), the cursor at the first
    /// question, and no answers yet.
    pub fn new(question_id: String, questions: Vec<Question>) -> Self {
        let per_question = questions
            .iter()
            // options + 1 for the auto-"Other" row (qwen ALWAYS appends it).
            .map(|q| SelectionList::new(q.options.len() + 1))
            .collect();
        let answers = vec![None; questions.len()];
        PendingQuestion {
            question_id,
            questions,
            cursor: 0,
            per_question,
            collecting_other: None,
            answers,
        }
    }

    /// The label for row `index` of question `q`: a real option's label, or the
    /// auto-appended "Other" row (the last row, `options.len()`).
    fn option_label(question: &Question, index: usize) -> Option<String> {
        match question.options.get(index) {
            Some(opt) => Some(opt.label.clone()),
            None if index == question.options.len() => Some(OTHER_OPTION_LABEL.to_string()),
            None => None,
        }
    }

    /// Whether row `index` of question `q` is the auto-"Other" row.
    fn is_other_row(question: &Question, index: usize) -> bool {
        index == question.options.len()
    }
}

/// Maps a selected Approval option index to its [`Decision`] (ADR-0049): row 0
/// approves once, row 1 approves-always (session-scoped standing, ADR-0005), row
/// 2 denies. Out of range is `None` (defensive - the list is length 3).
fn decision_for_option(index: usize) -> Option<Decision> {
    match index {
        0 => Some(Decision::Approve),
        1 => Some(Decision::ApproveAlways),
        2 => Some(Decision::Deny),
        _ => None,
    }
}

/// A PURE key press, defined here so the core stays crossterm-free (ADR-0019):
/// the adapter (`ui.rs`) maps a crossterm `KeyEvent` to one of these. `Char`
/// carries a typed grapheme; the navigation/edit keys are named variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Enter,
    Escape,
    PageUp,
    PageDown,
    /// Mouse wheel up - list-nav for the pre-agent Session Picker's alt-screen
    /// list (`ui::pick_loop` mints it via `map_mouse`); in the main fullscreen
    /// loop it scrolls the transcript UP a small step, detaching from the tail
    /// (ADR-0046, Stage 2). The adapter routes wheel events through `map_mouse`.
    WheelUp,
    /// Mouse wheel down - the counterpart of [`Key::WheelUp`]: drives the Session
    /// Picker list, and in the main loop scrolls the transcript DOWN toward the
    /// tail, re-attaching when it reaches the bottom (ADR-0046, Stage 2).
    WheelDown,
    ArrowUp,
    ArrowDown,
    Backspace,
    /// Move the Composer cursor one char left (clamped at the start).
    Left,
    /// Move the Composer cursor one char right (clamped at the end).
    Right,
    /// Jump to the start of the CURRENT LINE of the draft (readline behavior
    /// within a line, not the whole draft) - or, on an EMPTY draft, scroll the
    /// transcript to the TOP (ADR-0046, Stage 2).
    Home,
    /// Jump to the end of the CURRENT LINE of the draft - or, on an EMPTY draft,
    /// RE-ATTACH the transcript scroll to the tail (ADR-0046, Stage 2).
    End,
    /// Alt-Enter: insert a hard newline into the draft at the cursor. Named
    /// (rather than `Char('\n')`) so the modal's swallow-everything rule and
    /// the adapter's mapping both read as intent.
    InsertNewline,
    /// Ctrl-O (qwen `TOGGLE_COMPACT_MODE`): toggle compact mode, which hides
    /// settled Thinking items entirely and tool RESULT bodies (headers stay).
    /// Named (not `Char`) so the intent reads at the mapping and routing seams.
    ToggleCompact,
    /// Shift+Tab (win32: Tab): rotate the Approval mode one step in the cycle
    /// (ADR-0050). Named (not `Char`) so the intent reads at the mapping and
    /// routing seams alike.
    CycleApprovalMode,
    /// Bare Tab: accepts the highlighted `/` palette suggestion (ADR-0051
    /// System B, qwen `handleAutocomplete`). Inert outside the palette (the
    /// editing fall-through refuses it), so it never types a literal tab.
    Tab,
    /// Ctrl-S (ADR-0046, qwen `ShowMoreLines`): a keyboard PAGE-UP for the app-owned
    /// transcript scroll - the same "show me the rows that scrolled off the top"
    /// the wheel/PageUp do, reachable without a mouse. Scrolls behind an open
    /// modal too (the body renders behind it). Named (not `Char`) so the intent
    /// reads at the mapping and routing seams.
    ShowMore,
    Char(char),
    /// A key the core does not act on (function keys, etc.).
    Other,
    /// A named key that only matters while an Approval modal is open (`y`,
    /// `n`, `a` are `Char`; this is any other named key we want to name).
    Named(String),
}

/// A key the Approval gate has already declined to swallow. The field is
/// private and the only production mint sits inside the gate itself
/// ([`Screen::handle_key`]), so [`Composer::handle_key`] - which takes this
/// type - cannot run while the modal holds the keyboard: the FIXED routing
/// order (ADR-0034) is a compile-time fact, not a rule callers must remember.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UngatedKey(Key);

impl UngatedKey {
    /// Unwraps the key for the Composer's own arms.
    pub(crate) fn into_key(self) -> Key {
        self.0
    }

    /// Test-only mint, so Composer unit tests can fold keys without standing
    /// up a Screen. Production code has exactly one mint: the gate.
    #[cfg(test)]
    pub(crate) fn for_test(key: Key) -> Self {
        UngatedKey(key)
    }
}

/// The Agent command an [`Effect::Agent`] carries (baud's `{:agent, ...}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentCommand {
    Submit(String),
    Steer(String),
    /// Resolve a pending Approval with a decision.
    Approve(String, Decision),
    /// Rotate the Approval mode one step (ADR-0050, the Shift+Tab cycle). A
    /// fire-through to the Agent; the new mode returns via
    /// [`Event::ApprovalModeChanged`].
    CycleApprovalMode,
    /// Resolve a pending question (ADR-0057, `ask_user_question`): the id and the
    /// user's answer - `Ok(answers)` carries `(question_index, answer_value)`
    /// picks, `Err(decline)` the VERBATIM decline string. Mirrors
    /// [`AgentCommand::Approve`] but carries the full answer set.
    AnswerQuestion(String, Result<Vec<(usize, String)>, String>),
    Cancel,
}

/// The Approval decision an [`AgentCommand::Approve`] carries. Mirrors baud's
/// `:approve | :deny | :approve_always`; kept local to the pure core so the
/// tests need no other module (it maps to [`crate::approvals::Decision`] in the
/// adapter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Deny,
    ApproveAlways,
}

/// An Effect the adapter carries out after a fold (baud's `effect` type).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Call the Agent (submit/steer/approve/cancel).
    Agent(AgentCommand),
    /// Move keyboard focus to the Approval modal.
    FocusModal,
    /// Alert the operator that the app is now waiting on them - emitted with
    /// `FocusModal` when an ask (Approval or Question) opens, so a backgrounded
    /// terminal raises a desktop notification (and rings its bell). The String
    /// is the notification body (e.g. the gated command or the question). Not
    /// emitted for user-opened overlays (Help/menus), which are not asks.
    Notify(String),
    /// Move keyboard focus back to the composer.
    FocusComposer,
    /// Persist a submitted prompt into the on-disk history file.
    HistoryAppend(String),
    /// A committed Slash Command (ADR-0032): the Composer recognized `/name`
    /// and hands it to the adapter to run. Commands carry no inline arg today -
    /// a selector-opening command's sub-filter comes from the draft `rest`
    /// (the Composer's overlay view), not from this payload. The core does not
    /// know what any command does - this payload is command-agnostic.
    /// `generation` is the Composer's activation counter: the adapter echoes
    /// it back on the fill events (SelectorReady/SelectorFailed) so a late
    /// fill can never land on a later activation's overlay. Meaningful only
    /// for selector-opening commands; a fire-and-run command has no fill to
    /// tag.
    Command { name: String, generation: u64 },
    /// A row was chosen from a committed command's selector (ADR-0033): the
    /// opaque command `name` and the selected row's `value`. The adapter
    /// interprets it (e.g. `/model` swaps the Active Model and persists); the
    /// pure core neither knows nor cares. Phase 4b implements the arm.
    SelectorChosen { command: String, value: String },
    /// A `@path` AT pattern changed and needs a fresh file search (Phase C2,
    /// qwen `useAtCompletion`). The composer emits one per AT keystroke: the
    /// adapter walks the (cached, gitignore-aware) project tree, ranks it
    /// against `query`, caps it, and posts the rows back as
    /// [`Event::FileSearchReady`](crate::event::Event::FileSearchReady) through
    /// `ctx.selector_tx`. `generation` is the composer's AT activation counter,
    /// echoed on the fill so a stale keystroke's result can never overwrite a
    /// newer one - the per-keystroke analog of the selector's one-shot
    /// [`Effect::Command`]. The core does not walk or rank (ADR-0019).
    FileSearch { query: String, generation: u64 },
    /// Open the `/mcp` management dialog (ADR-0065 Phase E): the composer emits
    /// this to kick the async `Agent::mcp_views()` fetch. The adapter posts the
    /// result back as [`Event::McpDialogReady`](crate::event::Event::McpDialogReady)
    /// through `ctx.selector_tx`, echoing `generation` so a stale fetch can never
    /// land on a later open (the same guard as [`Effect::Command`]). The pure core
    /// neither fetches nor renders the views (ADR-0019); these are opaque server
    /// views the dialog reads.
    McpCommand { generation: u64 },
    /// Run a picked `/mcp` dialog action against the Agent (ADR-0065 Phase E): the
    /// dialog's `McpFold::Act` resolved to an action for a named server
    /// (Reconnect / Enable / Disable / Authenticate / Clear Authentication). The
    /// adapter calls the matching Agent method OFF the loop and re-fetches views
    /// (posting a fresh [`Event::McpDialogReady`](crate::event::Event::McpDialogReady)
    /// tagged with `generation`) so the dialog reflects the change. The action is
    /// carried as the pure [`McpAction`](crate::ui::mcp_command::McpAction) the
    /// dialog minted; the core does not know what any of them does (ADR-0032's
    /// command-agnostic seam).
    McpAction {
        action: crate::ui::mcp_command::McpAction,
        server: String,
        generation: u64,
    },
    /// Copy a string to the terminal clipboard via the OSC52 escape (ADR-0065
    /// Phase E, qwen `AuthenticateStep`'s `copyToClipboardViaOsc52`): the `/mcp`
    /// AUTHENTICATE step's `c` key resolved to the shown auth URL. The adapter
    /// base64-encodes it and writes `\x1b]52;c;<base64>\x07` to the terminal
    /// (stderr if a TTY, else stdout), then posts an
    /// [`Event::McpCopyResult`](crate::event::Event::McpCopyResult) carrying
    /// whether the write reached a TTY so the pure core can set the copy-feedback
    /// hint. The pure core neither encodes nor writes (ADR-0019).
    ClipboardOsc52(String),
}

/// The pure Screen state (ADR-0034; the renamed fold root of baud's
/// `%Baud.UI.Transcript{}`).
///
/// The Transcript store holds a live [`Streaming`] snapshot and is not
/// `Clone`/`PartialEq`, so the core is not `Clone`; the fold takes and returns
/// an owned `Screen` by value, mirroring the Elixir struct-threading style.
pub struct Screen {
    /// The Transcript (ADR-0034): the display-side history and the streaming
    /// snapshot, behind [`crate::ui::transcript`]'s store
    /// seam. Private on purpose - reads go through [`Screen::transcript`]
    /// (the render adapter's window), mutation only through the folds and the
    /// submitted/steered outcome hooks.
    transcript: Transcript,
    pub status: Status,
    pub pending_approval: Option<PendingApproval>,
    /// A pending question round-trip (ADR-0057, `ask_user_question`): the modal
    /// state while the user answers one or more structured questions. Runs
    /// parallel to `pending_approval` - opened by [`Event::QuestionRequest`],
    /// cleared when the last question is answered (or on Escape/decline). Unlike
    /// an Approval there is NO auto path; every question opens this modal.
    pub pending_question: Option<PendingQuestion>,
    /// The current Approval mode (ADR-0050), a DISPLAY-ONLY mirror of the
    /// Agent's authoritative `Approvals::mode`, fed by
    /// [`Event::ApprovalModeChanged`]. The Screen never decides the mode - it
    /// only reflects it, so the footer AutoAcceptIndicator (and, in a later
    /// phase, the composer chrome) can render it. `Default` shows nothing.
    pub approval_mode: ApprovalMode,
    pub token_estimate: Option<u64>,
    pub context_budget: Option<u64>,
    pub compaction_slack: f64,
    pub pressure_level: PressureLevel,
    /// The Session's cumulative dollar cost from the most recent
    /// [`Event::SessionCost`] (ADR-0037: pricing rides the Catalog Model;
    /// surfacing is display-side only). Stays 0.0 on unpriced (local/custom)
    /// Models - the metered boundary emits nothing - and the status bar hides
    /// its cost segment at zero, so such a Session looks exactly as before.
    pub session_cost: f64,
    /// The footer MCP-health count (ADR-0065 Phase F, qwen `MCPHealthPill`): how
    /// many managed servers are disconnected and not disabled. Held here because
    /// the footer renders synchronously; refreshed by [`Event::McpHealth`] at
    /// startup and on each `/mcp` dialog fetch (the live 30s health loop is
    /// DEFERRED). `0` hides the pill. Display-only.
    pub mcp_offline: usize,
    /// The Composer (ADR-0034): the draft, the overlays, and the
    /// prompt-history ring, behind [`crate::ui::composer`]'s seam. Private on
    /// purpose - reads go through [`Screen::composer`] (the render
    /// adapter's window), mutation only through the folds and the
    /// submitted/steered outcome hooks.
    composer: Composer,
    /// Compact mode (qwen `compactMode`, Ctrl+O `TOGGLE_COMPACT_MODE`): when
    /// `true`, settled Thinking items are HIDDEN entirely and tool RESULT bodies
    /// are hidden (their headers stay), keeping the transcript terse. Default
    /// `false` = show everything. Toggled by [`Key::ToggleCompact`]; the single
    /// display toggle that replaced suspenders' two expand flags (ADR-0052).
    /// Note the inversion from the retired flags: they showed FULL when
    /// `expanded == true`; this shows full when `compact_mode == false`.
    pub compact_mode: bool,
    /// The keyboard-shortcuts Help overlay (qwen `Help`, the `?` affordance the
    /// footer's `? for shortcuts` hint promises). When `true` the bordered panel
    /// draws in the pending region and holds the keyboard like the Approval modal:
    /// [`Screen::handle_help_key`] swallows every key with no effect except the
    /// closers (`Esc`, `?`, `q`). Opened by `?` on an EMPTY draft (a non-empty
    /// draft keeps `?` typeable) - the interception sits above the Composer's
    /// first refusal, mirroring the `pending_approval` gate. Default `false`.
    pub help_open: bool,
    /// The scroll INTENT (ADR-0046, Stage 2): how many wrapped rows the user has
    /// scrolled UP from the tail. `0` while following the tail; grows as the user
    /// wheels/pages up. The pure core holds only the intent - it is geometry-free,
    /// so the render clamps it against the live viewport each frame
    /// ([`components::anchor_clip`]): a value past the top pins to the top, and a
    /// growing terminal auto-re-attaches. Paired with `follow_tail` so appended
    /// streaming content never yanks a detached view back down.
    pub scroll_lines: usize,
    /// Whether the transcript view FOLLOWS THE TAIL (qwen/chat-UI default): `true`
    /// shows the newest rows at the bottom and ignores `scroll_lines` entirely, so
    /// new content stays pinned to the bottom. Scrolling up sets it `false`
    /// (detached); reaching the bottom again (`scroll_lines == 0`) or pressing End
    /// re-attaches. Default `true`.
    pub follow_tail: bool,
    /// The last body zone height the renderer drew (wrapped rows), recorded by the
    /// adapter each frame through [`Screen::note_body_height`]. The pure core is
    /// geometry-free, so PageUp/PageDown need a page step: this is that page. Read
    /// only by the page-scroll arms; `0` until the first frame is measured (a
    /// pre-frame PageUp then no-ops, which is fine - there is nothing drawn yet).
    pub last_body_height: usize,
}

/// The wheel scroll step (rows per wheel tick, qwen `SCROLL_STEP`): how far one
/// [`Key::WheelUp`]/[`Key::WheelDown`] moves the detached view.
const WHEEL_STEP: usize = 3;

/// The options a fresh Screen is opened with (baud's `new/1` keyword opts).
#[derive(Default)]
pub struct ScreenOpts {
    pub context_budget: Option<u64>,
    pub compaction_slack: f64,
    pub history: Vec<String>,
    /// Launch-time info lines the adapter authors (context-file skips today):
    /// news from before the event loop existed, recorded right after the
    /// startup Header so it is visible without ever entering the Conversation.
    pub notices: Vec<String>,
    /// The startup Header facts (qwen `AppHeader`): the crate `version`, the
    /// launch Model's scoped `model` id, and the tilde-abbreviated working
    /// directory `cwd`. Empty by default (tests that don't care about the banner
    /// open with a bare Header); the `ui` adapter fills them at launch.
    pub header: HeaderFacts,
}

/// The facts the startup [`TranscriptItem::Header`] shows (qwen `AppHeader`):
/// the crate version, the launch Model's scoped id, and the working directory.
/// A value object so [`ScreenOpts`] threads them as one named-field carrier and
/// a new banner field is a field, not another opt. `tip_seed` picks the startup
/// tip deterministically (the pure core has no RNG/clock, ADR-0019) - the
/// adapter injects the prompt-history length.
#[derive(Default)]
pub struct HeaderFacts {
    pub version: String,
    pub model: String,
    pub cwd: String,
    pub tip_seed: usize,
}

// ---- diff-demo fixtures (Screen::demo_diffs) ----------------------------
//
// These build `TranscriptItem::Diff`s directly, in the shape the diff
// extension's Presenter emits: raw marker-free code lines tagged by `DiffSide`,
// a header per hunk (`None` for a created file), and a `lang` from the file
// extension. Used only by [`Screen::demo_diffs`] for the live `diff-demo`.

// One tagged code line (raw text, no `+`/`-` marker - the adapter adds it).
fn diff_line(side: DiffSide, text: &str) -> DiffLine {
    DiffLine::new(side, text)
}

// An edit_file diff of a Rust file with an interleaved context/removed/added
// hunk, so both tint bands and the two-pass highlighting are on screen.
fn rust_edit_diff() -> TranscriptItem {
    let lines = vec![
        diff_line(DiffSide::Context, "/// Splits `src` into tokens."),
        diff_line(
            DiffSide::Context,
            "pub fn tokenize(src: &str) -> Vec<Token> {",
        ),
        diff_line(DiffSide::Removed, "    let mut out = Vec::new();"),
        diff_line(DiffSide::Removed, "    // TODO: actually tokenize"),
        diff_line(DiffSide::Added, "    let mut out = Vec::with_capacity(16);"),
        diff_line(DiffSide::Added, "    for word in src.split_whitespace() {"),
        diff_line(DiffSide::Added, "        out.push(Token::word(word));"),
        diff_line(DiffSide::Added, "    }"),
        diff_line(DiffSide::Context, "    out"),
        diff_line(DiffSide::Context, "}"),
    ];
    TranscriptItem::Diff {
        title: "edit src/lexer.rs (+4 -2)".into(),
        lang: Some("rs".into()),
        hunks: vec![DiffHunk {
            header: Some("@@ -1,6 +1,8 @@".into()),
            lines,
        }],
        elided: 0,
    }
}

// A created JavaScript file: one all-added hunk = the whole file (header None),
// leading with a multi-line /** … */ JSDoc block so the created-file coherent
// comment coloring shows across every line, plus strings/keywords/numbers.
fn js_created_diff() -> TranscriptItem {
    let body = [
        "/**",
        " * Greets a user by name.",
        " * @param {string} name - who to greet",
        " * @returns {string} the greeting",
        " */",
        "export function greet(name) {",
        "  const times = 3;",
        "  const hi = `Hello, ${name}!`;",
        "  return Array(times).fill(hi).join(\" \");",
        "}",
    ];
    let lines = body.iter().map(|t| diff_line(DiffSide::Added, t)).collect();
    TranscriptItem::Diff {
        title: "write_file src/greet.js (new file, +10)".into(),
        lang: Some("js".into()),
        hunks: vec![DiffHunk {
            header: None,
            lines,
        }],
        elided: 0,
    }
}

// A created JSON file (a package.json fragment) - a second language on screen.
fn json_created_diff() -> TranscriptItem {
    let body = [
        "{",
        "  \"name\": \"greet\",",
        "  \"version\": \"1.0.0\",",
        "  \"type\": \"module\",",
        "  \"scripts\": {",
        "    \"test\": \"node --test\"",
        "  },",
        "  \"license\": \"MIT\"",
        "}",
    ];
    let lines = body.iter().map(|t| diff_line(DiffSide::Added, t)).collect();
    TranscriptItem::Diff {
        title: "write_file package.json (new file, +9)".into(),
        lang: Some("json".into()),
        hunks: vec![DiffHunk {
            header: None,
            lines,
        }],
        elided: 0,
    }
}

// A capped diff: `elided > 0` renders the muted "… N more lines" tail, and one
// very long added line exercises the display-width clip.
fn capped_diff() -> TranscriptItem {
    let long = format!(
        "const HAY = \"{}\"; // a very long line to clip",
        "x".repeat(160)
    );
    let lines = vec![
        diff_line(DiffSide::Context, "// generated - do not edit"),
        diff_line(DiffSide::Added, &long),
        diff_line(DiffSide::Added, "const B = 2;"),
    ];
    TranscriptItem::Diff {
        title: "write_file src/generated.js (new file, +2)".into(),
        lang: Some("js".into()),
        hunks: vec![DiffHunk {
            header: None,
            lines,
        }],
        elided: 37,
    }
}

impl Screen {
    /// A fresh Screen, opened with the startup Header banner and idle status.
    pub fn new(opts: ScreenOpts) -> Self {
        // The startup banner is this fold's Voice (qwen `AppHeader`): the store
        // opens empty and records what its owner authors. The title is fixed
        // (`suspenders`); the version/model/cwd ride in on the opts, and the tip
        // is picked deterministically from the seed.
        let HeaderFacts {
            version,
            model,
            cwd,
            tip_seed,
        } = opts.header;
        let mut transcript = Transcript::new();
        transcript.header(
            HEADER_TITLE,
            version,
            model,
            cwd,
            pick_startup_tip(tip_seed),
        );
        for notice in opts.notices {
            transcript.info(notice);
        }
        Screen {
            transcript,
            status: Status::Idle,
            pending_approval: None,
            pending_question: None,
            approval_mode: ApprovalMode::default(),
            token_estimate: None,
            context_budget: opts.context_budget,
            compaction_slack: opts.compaction_slack,
            pressure_level: PressureLevel::Ok,
            session_cost: INITIAL_SESSION_COST,
            mcp_offline: 0,
            composer: Composer::new(opts.history),
            compact_mode: false,
            help_open: false,
            scroll_lines: 0,
            follow_tail: true,
            last_body_height: 0,
        }
    }

    /// A representative populated Screen for eyeballing the render (the `--demo`
    /// harness and the render tests): one user request whose run interleaves
    /// several Thinking passes, tool machinery, harness markers, and an answer
    /// with a code fence - the exact shape that exposed the fold / separator /
    /// blank-line bugs. No IO, no events; the transcript is authored directly.
    // qual:test_helper - called only from render tests in ui::components
    pub fn demo() -> Self {
        let mut screen = Screen::new(ScreenOpts::default());
        let t = &mut screen.transcript;
        let tool = |name: &str, arg: &str, summary: &str, err: bool| TranscriptItem::ToolResult {
            name: name.into(),
            summary: summary.into(),
            is_error: err,
            key_arg: Some(arg.into()),
        };
        let thought = |text: &str| TranscriptItem::Thinking { text: text.into() };

        t.user("evaluate this project");
        t.push(TranscriptItem::Assistant {
            text: "I'll evaluate this project by exploring its structure, dependencies, and code quality. Let me start by getting an overview.".into(),
        });
        t.push(thought("The user wants me to evaluate this project. Let me start by understanding what kind of project this is and its structure before forming an evaluation, then look at dependencies and code quality."));
        t.push(tool(
            "list_directory",
            ".",
            ".claude/ (+19 more lines)",
            false,
        ));
        t.push(tool(
            "grep_search",
            "fn main|pub fn run",
            "src/main.rs:12 (+29 more lines)",
            false,
        ));
        t.push(thought("Let me gather more details about the project structure, dependencies, code quality, and documentation."));
        t.push(tool(
            "read_file",
            "README.md",
            "# Suspenders (+61 more lines)",
            false,
        ));
        t.push(tool(
            "read_file",
            "Cargo.toml",
            "[package] (+49 more lines)",
            false,
        ));
        t.push(thought("Let me continue exploring the project structure to understand the codebase more thoroughly."));
        t.push(tool(
            "list_directory",
            "src",
            "agent/ (+32 more lines)",
            false,
        ));
        t.push(tool(
            "list_directory",
            "docs",
            "adr/ (+2 more lines)",
            false,
        ));
        t.marker("» [reading file after file fills your context - grep for the symbol you actually need first instead; then read only what you will change]", Tone::Aid);
        t.push(thought("Let me explore more of the project structure to understand the codebase depth, test coverage, and ADRs."));
        t.push(tool(
            "list_directory",
            "docs/adr",
            "0001-ratatui-for-the-tui.md (+39 more lines)",
            false,
        ));
        t.push(tool(
            "list_directory",
            "src/turn",
            "governor/ (+7 more lines)",
            false,
        ));
        t.marker("⚑ plan refreshed", Tone::Aid);
        t.push(thought("Let me set up a plan for this evaluation, then continue gathering information to form a comprehensive assessment."));
        t.push(tool(
            "plan",
            "GOAL: Evaluate the Suspenders project - a Rust terminal cod…",
            "[plan recorded]",
            false,
        ));
        t.push(thought("Let me check the build health and test coverage."));
        t.push(tool(
            "run_shell_command",
            "cargo build 2>&1",
            "√ exit 0",
            false,
        ));
        t.push(tool(
            "run_shell_command",
            "cargo test --lib -- --quiet 2>&1",
            "× [command denied by user]",
            true,
        ));
        t.push(TranscriptItem::Assistant {
            text: "The project is a well-structured Rust terminal agent. Here is the tokenizer entry point:\n\n```rust\nfn tokenize(src: &str) -> Vec<Token> {\n    let mut out = Vec::new();\n    out\n}\n```\n\nOverall the codebase is clean and well-tested.".into(),
        });
        screen
    }

    /// A populated Screen that showcases the first-class `Diff` item (ADR-0008):
    /// its marker glyph, the added/removed background tint, and the two-pass,
    /// hunk-coherent syntect highlighting. Built for the `diff-demo` binary to
    /// eyeball the LIVE render path; authored directly (no IO, no events). NOT a
    /// test helper - the render snapshot tests pin [`Screen::demo`], so this one
    /// is free to grow without churning them.
    ///
    /// One user request; the diffs follow it in the transcript and fold
    /// under Ctrl-O exactly like a real edit would. Each Diff is shaped as the
    /// diff extension's Presenter emits it (raw marker-free lines, a
    /// `display::title`-style title, the file extension as `lang`).
    pub fn demo_diffs() -> Self {
        let mut screen = Screen::new(ScreenOpts::default());
        let t = &mut screen.transcript;

        t.user("clean up the tokenizer and scaffold the package");
        t.push(TranscriptItem::Assistant {
            text: "I'll tighten the tokenizer, add a small JS helper with docs, and drop in a package.json.".into(),
        });

        // 1. An edit_file diff of a RUST file: interleaved context/removed/added
        //    lines, so both tint bands and the two-pass highlighting show.
        t.push(rust_edit_diff());

        // 2. A created JavaScript file with a multi-line /** … */ JSDoc block
        //    (proves the created-file coherent multi-line comment coloring) plus
        //    strings, keywords, and numbers.
        t.push(js_created_diff());

        // 3. A created JSON file (a package.json fragment) - a second language.
        t.push(json_created_diff());

        // 4. A capped diff: `elided > 0` shows the muted "… N more lines" tail,
        //    and one very long line exercises the display-width clip.
        t.push(capped_diff());

        screen
    }

    // ---- Agent events ------------------------------------------------------

    /// Folds one [`Event`] into the Screen. The event vocabulary is enumerated
    /// in [`crate::event`] and this dispatch is EXHAUSTIVE over it: every
    /// variant names a family (the Composer-consumed selector fills reach an
    /// explicit no-op arm), so a new event kind is a compile error here until
    /// it is placed - it can never silently fall through.
    pub fn apply_event(mut self, event: Event) -> (Self, Vec<Effect>) {
        // The Composer gets first refusal on events too (ADR-0034): the
        // overlay-filling deliveries (SelectorReady/SelectorFailed) are its
        // own, stale fills included - this fold never sees them, and a future
        // overlay fed by a new event needs no new arm here. The Composer's own
        // effects never touch the Transcript, so no commit is due for them.
        let event = match self.composer.apply_event(event) {
            EventOutcome::Consumed(effects) => return (self, effects),
            EventOutcome::Refused(event) => event,
        };

        // The flat family dispatch: each arm names one event family and hands
        // the whole event to that family's fold below. Every arm can settle
        // items into the Transcript, which the fullscreen renderer redraws whole
        // each frame (ADR-0046); the arms just return their own effects.
        let (screen, effects) = match event {
            event @ (Event::RunStarted(..)
            | Event::MessageStart { .. }
            | Event::MessageUpdate { .. }
            | Event::MessageEnd { .. }) => self.apply_streaming(event),

            event @ (Event::ContextPressure { .. }
            | Event::CompactionProgress { .. }
            | Event::SessionCost { .. }) => self.apply_pressure(event),

            event @ (Event::ToolCall { .. }
            | Event::ToolResult { .. }
            | Event::ExtensionError { .. }) => self.apply_tooling(event),

            event @ (Event::ApprovalRequest { .. }
            | Event::ApprovalResolved { .. }
            | Event::ApprovalAuto { .. }
            | Event::ApprovalModeChanged { .. }) => self.apply_approval(event),

            event @ (Event::QuestionRequest { .. } | Event::QuestionResolved { .. }) => {
                self.apply_question(event)
            }

            event @ (Event::SteeringQueued { .. } | Event::SteeringDelivered { .. }) => {
                self.apply_steering(event)
            }

            event @ (Event::SessionLogError { .. }
            | Event::Retry { .. }
            | Event::LoopStall { .. }
            | Event::BackgroundNotification { .. }
            | Event::BackgroundTaskFinished { .. }
            | Event::McpAuthProgress { .. }) => self.apply_voice(event),

            // The footer MCP-health count (ADR-0065 Phase F): a display-only field
            // the footer pill reads. No Transcript effect - the count just moves.
            Event::McpHealth { offline } => {
                self.mcp_offline = offline;
                (self, vec![])
            }

            event @ (Event::RunFinished { .. } | Event::RunCancelled | Event::RunError { .. }) => {
                self.apply_settlement(event)
            }

            // The selector fills and the AT file-search fill are the Composer's
            // own (ADR-0034): they are consumed by `self.composer.apply_event`
            // at the top of this fold and never reach this dispatch. Listed
            // explicitly, with no wildcard, so the match stays EXHAUSTIVE over
            // the Event vocabulary - a future variant is a compile error here,
            // not a silent fallthrough.
            Event::SelectorReady { .. }
            | Event::SelectorFailed { .. }
            | Event::FileSearchReady { .. }
            | Event::McpDialogReady { .. }
            | Event::McpCopyResult { .. } => (self, vec![]),
        };
        (screen, effects)
    }

    // ---- Event families ------------------------------------------------------
    //
    // One private method per event family: [`Screen::apply_event`] stays the
    // flat dispatch and each family holds its own branches. The dispatch owns
    // family membership, so each method's trailing `_` arm restates the
    // dispatch's own ignore rule - it is never a reachable behavior of its own.

    // Streaming / message events: the Run opening and the three-phase
    // assistant stream, each delegating one store verb.
    fn apply_streaming(mut self, event: Event) -> (Self, Vec<Effect>) {
        match event {
            Event::RunStarted(_reference) => {
                self.status = Status::Running;
                self.transcript.discard_streaming();
                // No PinBottom (ADR-0046): the fullscreen body is bottom-anchored
                // and always follows the tail.
                (self, vec![])
            }

            Event::MessageStart { .. } => {
                self.transcript.message_start();
                (self, vec![])
            }

            Event::MessageUpdate { content, .. } => {
                self.transcript.message_update(content);
                (self, vec![])
            }

            Event::MessageEnd { content, .. } => {
                self.transcript.message_end(&content);
                (self, vec![])
            }

            _ => (self, vec![]),
        }
    }

    // Context pressure / Eviction / Compaction (ADR-0008; CONTEXT.md: Eviction):
    // the status-bar figures and the receded machinery lines.
    fn apply_pressure(mut self, event: Event) -> (Self, Vec<Effect>) {
        match event {
            // Live context-pressure indication: refresh the status bar's token
            // estimate and budget mid-Run and name the semantic pressure level
            // (ADR-0008). NEVER a Transcript item.
            Event::ContextPressure {
                token_estimate,
                context_budget,
                max_tokens_reserve,
            } => {
                self.token_estimate = Some(token_estimate);
                self.context_budget = Some(context_budget);
                self.pressure_level = pressure_level(
                    token_estimate,
                    context_budget,
                    max_tokens_reserve,
                    self.compaction_slack,
                );
                (self, vec![])
            }

            // Compaction made progress: recede one Housekeeping marker.
            Event::CompactionProgress { status } => {
                self.transcript
                    .marker(compaction_line(&status), Tone::Housekeeping);
                (self, vec![])
            }

            // A priced Response moved the Session's cumulative cost: refresh
            // the status bar's figure. NEVER a Transcript item - cost is a
            // bar fact, like the token estimate.
            Event::SessionCost { total } => {
                self.session_cost = total;
                (self, vec![])
            }

            _ => (self, vec![]),
        }
    }

    // Tool machinery: calls, results (paired by id in the store), and the
    // Extension failure report.
    fn apply_tooling(mut self, event: Event) -> (Self, Vec<Effect>) {
        match event {
            Event::ToolCall { id, name, input } => {
                self.transcript.tool_call(id, name, &input);
                (self, vec![])
            }

            Event::ToolResult {
                id,
                name,
                content,
                is_error,
                artifacts,
            } => {
                self.transcript
                    .tool_result(&id, name, &content, is_error, &artifacts);
                (self, vec![])
            }

            // A tool-side subsystem (MCP init/ops today) failed and was skipped
            // fail-open (ADR-0007) - recorded as one visible report line.
            Event::ExtensionError {
                extension,
                stage,
                message,
            } => {
                self.transcript
                    .extension_failure(&extension, stage, &message);
                (self, vec![])
            }

            _ => (self, vec![]),
        }
    }

    // The Approval lifecycle: request opens the modal, resolved clears the
    // matching pending, auto notes the Standing Approval.
    fn apply_approval(mut self, event: Event) -> (Self, Vec<Effect>) {
        match event {
            Event::ApprovalRequest {
                approval_id,
                command,
            } => {
                // The inline approval attaches to the NEWEST pending ToolCall by
                // position (ADR-0049): batch tools run sequentially, so the only
                // live ToolCall (one with no result yet) is the one being gated.
                // Its name derives the ConfirmKind (run_command → Exec, else
                // Info); no tool_use id is plumbed through the event.
                let kind = self
                    .newest_live_tool_name()
                    .map(ConfirmKind::from_tool_name)
                    .unwrap_or(ConfirmKind::Info);
                let body = format!("Approval needed: {command}");
                self.pending_approval = Some(PendingApproval {
                    approval_id,
                    command,
                    kind,
                    selection: SelectionList::new(APPROVAL_OPTION_COUNT),
                });
                (self, vec![Effect::FocusModal, Effect::Notify(body)])
            }

            Event::ApprovalResolved { approval_id, .. } => match &self.pending_approval {
                Some(pending) if pending.approval_id == approval_id => self.clear_approval(),
                _ => (self, vec![]),
            },

            // A Standing Approval covered the command: no modal was ever shown.
            Event::ApprovalAuto { command } => {
                self.transcript
                    .info(format!("auto-approved (standing): {command}"));
                (self, vec![])
            }

            // The Agent rotated its Approval mode (ADR-0050): mirror it for the
            // footer indicator. Display-only - the Screen never decides the mode.
            Event::ApprovalModeChanged { mode } => {
                self.approval_mode = mode;
                (self, vec![])
            }

            _ => (self, vec![]),
        }
    }

    // The Question lifecycle (ADR-0057, qwen `ask_user_question`): a request
    // opens the modal (no auto path - every question opens one), resolved is the
    // operator-visible settlement (the modal was already cleared on the answering
    // keypress).
    fn apply_question(mut self, event: Event) -> (Self, Vec<Effect>) {
        match event {
            Event::QuestionRequest {
                question_id,
                questions,
            } => {
                let body = questions
                    .first()
                    .map(|q| q.question.clone())
                    .unwrap_or_else(|| "A question is waiting".to_string());
                self.pending_question = Some(PendingQuestion::new(question_id, questions));
                (self, vec![Effect::FocusModal, Effect::Notify(body)])
            }

            // The round-trip settled: the modal was cleared when the user
            // answered (mirroring how ApprovalResolved arrives AFTER
            // clear_approval). A stray resolved for an already-cleared modal is a
            // no-op. Display-only; no Transcript item.
            Event::QuestionResolved { .. } => (self, vec![]),

            _ => (self, vec![]),
        }
    }

    // Steering: queued shows a pending line; delivered promotes it to a
    // user line (the text is now in the Conversation). The marker text
    // and the promotion are the store's rule.
    fn apply_steering(mut self, event: Event) -> (Self, Vec<Effect>) {
        match event {
            Event::SteeringQueued { text } => {
                self.transcript.steering_queued(&text);
                // No PinBottom (ADR-0046): the inline pending region follows the
                // tail; the queued marker shows at the bottom by construction.
                (self, vec![])
            }

            Event::SteeringDelivered { text } => {
                self.transcript.steering_delivered(text);
                (self, vec![])
            }

            _ => (self, vec![]),
        }
    }

    // Voiced / operator-news lines: everything whose display is one authored
    // info line - the Session Log failure, the tools-narrowed marker, and the
    // bounded re-draw marks.
    fn apply_voice(mut self, event: Event) -> (Self, Vec<Effect>) {
        match event {
            // The Session Log died (IO failure); the Session continues
            // unpersisted. Adapter news, not a harness marker - stays Info.
            Event::SessionLogError { message } => {
                self.transcript.info(format!(
                    "session log failed ({message}); this session will not resume"
                ));
                (self, vec![])
            }

            // A malformed-tool-call re-draw (ADR-0030): silent to the model's
            // Conversation, never silent to the operator - an info line marks
            // each bounded re-draw.
            Event::Retry {
                attempt, budget, ..
            } => {
                self.transcript.info(format!(
                    "malformed tool call - re-drawing ({attempt}/{budget})"
                ));
                (self, vec![])
            }

            // The loop-detector tripped (CONTEXT.md: the passive circuit
            // breaker): the model repeated the same Tool Call batch `count`
            // times, so the Run was ended. A Constrain marker - a guard
            // limiting the model, not the model's own Voice.
            Event::LoopStall { count } => {
                self.transcript.marker(
                    format!("loop detected - stopped after {count} identical tool batches"),
                    Tone::Constrain,
                );
                (self, vec![])
            }

            // A background subagent reached a terminal state (P4b, ADR-0063): an
            // operator-visible info line noting the task and its lifecycle word.
            // The full envelope reaches the model on its next Run via the queued
            // notification; this is the "it finished" marker for the operator.
            Event::BackgroundTaskFinished { task_id, status } => {
                self.transcript
                    .info(format!("background agent {task_id} {status}"));
                (self, vec![])
            }

            // The queued notification's envelope (P4b, ADR-0063): already surfaced
            // by the BackgroundTaskFinished info line, and it enters the model's
            // Conversation as a user-role message on the next Run - so nothing
            // extra to render here, but the arm keeps the match exhaustive.
            Event::BackgroundNotification { .. } => (self, vec![]),

            // An `/mcp` Authenticate progress line (ADR-0065 Phase D): surfaced as
            // an operator-visible info line as the browser flow runs. The dedicated
            // AUTHENTICATE dialog step (Phase E) consumes these directly; the info
            // line is where the copy-the-URL hint + the auth URL also show.
            Event::McpAuthProgress {
                server, message, ..
            } => {
                self.transcript.info(format!("mcp {server}: {message}"));
                (self, vec![])
            }

            _ => (self, vec![]),
        }
    }

    // Settlement: how a Run ends - finished, cancelled, or errored.
    fn apply_settlement(mut self, event: Event) -> (Self, Vec<Effect>) {
        match event {
            // A finished Run: salvage anything still streaming and note an
            // abnormal stop reason - the note is this fold's Voice, the
            // flush-before-note ordering the store's `close` - then record
            // the closing estimate and budget.
            Event::RunFinished {
                stop_reason,
                token_estimate,
                context_budget,
            } => {
                self.transcript.close(stop_reason_note(stop_reason));
                self.status = Status::Idle;
                self.token_estimate = Some(token_estimate);
                self.context_budget = Some(context_budget);
                // Robustness: a Run should never finish with the question modal
                // still open, but if a future finish-with-open-modal path arises,
                // clear it here so it can't leave a dangling modal.
                self.pending_question = None;
                (self, vec![])
            }

            Event::RunCancelled => self.close_abnormally("turn cancelled".to_string()),

            Event::RunError { reason } => self.close_abnormally(format!("turn error: {reason}")),

            _ => (self, vec![]),
        }
    }

    // ---- User intents ------------------------------------------------------

    /// Folds one key press into the Screen. ALL keys route through here -
    /// Composer editing included - so every rule lives in the pure core
    /// (ADR-0001); the adapter only maps crossterm events to [`Key`]s. The
    /// routing order is FIXED (ADR-0034): the Approval gate first, then the
    /// Composer's first refusal, then this fold's own arms. The gate-first
    /// leg is compiler-enforced: [`Composer::handle_key`] takes an
    /// [`UngatedKey`], and only the gate below can mint one.
    ///
    /// While an Approval is pending, the inline radio holds the keyboard
    /// (ADR-0049): the arrow keys + Enter drive the [`SelectionList`], the
    /// numbered digits quick-select, and the legacy `y`/`n`/`a` quick-keys stay
    /// as a SUPERSET (approve-once / deny / approve-always). Escape DENIES THIS
    /// TOOL and lets the Run continue (qwen-faithful, matching the `(esc)` option
    /// label), NOT cancel the Run. Every other key is swallowed; in particular,
    /// plain chars must NOT edit the Composer while the block is open. Escape
    /// only cancels the whole Run in the no-approval, streaming case below.
    pub fn handle_key(mut self, key: Key) -> (Self, Vec<Effect>) {
        // Ctrl-S (ADR-0046): repurposed as a page-up now that the app owns
        // scrolling - a keyboard reach for the same "show me more of what scrolled
        // off the top" the wheel/PageUp do. Intercepted here, ABOVE the modal
        // gates, so it scrolls the transcript behind an open Approval/question too
        // (the body still renders behind the modal). Detaches from the tail.
        if key == Key::ShowMore {
            return (self.page_up(), vec![]);
        }
        if self.pending_approval.is_some() {
            return self.handle_approval_key(key);
        }

        // The question modal (ADR-0057) gates like the Approval modal, with one
        // twist: while it is COLLECTING an "Other" free-form answer the composer
        // must stay editable (the user is typing the answer), so the gate defers
        // to the composer below and only intercepts the eventual submit
        // ([`Screen::handle_other_capture_key`]). Otherwise the modal holds the
        // keyboard and drives the current question's radio.
        if let Some(pending) = &self.pending_question {
            if pending.collecting_other.is_some() {
                return self.handle_other_capture_key(key);
            }
            return self.handle_question_key(key);
        }

        // The Help overlay (qwen `Help`) gates like the Approval modal (Approval
        // wins if somehow both, hence its gate above): while it is open it holds
        // the keyboard, so no stray key leaks to the Composer/Agent behind it.
        if self.help_open {
            return self.handle_help_key(key);
        }

        // `?` on an EMPTY draft opens the Help overlay (the footer's `? for
        // shortcuts` promise); on a NON-empty draft it stays a typed char (users
        // write `?` in prompts), so this interception sits ABOVE the Composer's
        // first refusal and defers to it whenever the draft is not empty. No Run
        // state changes and the overlay is a modal, so it wants `FocusModal` like
        // an Approval (the adapter draws it as an overlay).
        if key == Key::Char('?') && self.composer.view().draft.is_empty() {
            self.help_open = true;
            return (self, vec![Effect::FocusModal]);
        }

        // Home/End scroll the transcript to the top / re-attach to the tail
        // (ADR-0046, Stage 2), but ONLY on an EMPTY draft: with text in the draft
        // they stay the Composer's readline line-nav (jump to line start/end), so
        // this interception sits ABOVE the Composer's first refusal and defers to
        // it whenever the draft is not empty - the same empty-draft guard `?` uses.
        if self.composer.view().draft.is_empty() {
            match key {
                Key::Home => return (self.scroll_to_top(), vec![]),
                Key::End => return (self.scroll_to_bottom(), vec![]),
                _ => {}
            }
        }

        // The Composer gets first refusal (ADR-0034): every key the modal did
        // not swallow is offered to it - the UngatedKey minted here is the
        // gate's receipt, and this is its ONLY production mint - and only a
        // Refused key reaches the arms below. This ordering is load-bearing -
        // it is what lets a slash
        // draft intercept Enter and the arrows, and an open overlay intercept
        // Escape and the wheel, without this fold knowing any overlay exists.
        // A notice is the Composer's one info line (the unknown-command case),
        // recorded through the store like every other adapter-side news.
        let key = match self.composer.handle_key(UngatedKey(key), self.status) {
            KeyOutcome::Consumed { effects, notice } => {
                if let Some(text) = notice {
                    self.transcript.info(text);
                }
                return (self, effects);
            }
            KeyOutcome::Refused(key) => key,
        };

        let (screen, effects) = match key {
            // Escape means Cancellation while a Run runs; the Composer has
            // already refused it (no overlay was open to close).
            Key::Escape if self.status == Status::Running => {
                (self, vec![Effect::Agent(AgentCommand::Cancel)])
            }

            // Transcript scrolling (ADR-0046, Stage 2): the app owns history, so
            // these move the app's own scroll intent. Wheel is a small step, Page a
            // body-height page; scrolling up DETACHES from the tail (so streaming
            // content no longer yanks the view down), and reaching the bottom
            // re-attaches. The render clamps the intent to the live viewport, so
            // these arms stay geometry-free. The Session Picker still maps the same
            // keys for its own alt-screen list navigation.
            Key::WheelUp => (self.scroll_up(WHEEL_STEP), vec![]),
            Key::WheelDown => (self.scroll_down(WHEEL_STEP), vec![]),
            Key::PageUp => (self.page_up(), vec![]),
            Key::PageDown => (self.page_down(), vec![]),

            // Ctrl-O (qwen `TOGGLE_COMPACT_MODE`): flip compact mode. The
            // fullscreen renderer redraws the WHOLE transcript at the new compact
            // next frame (ADR-0046), so the flip needs no effect - there is no
            // frozen scrollback to un-draw. The status bar's compact segment
            // renders the flag, so the flip is visible even on a plain chat.
            Key::ToggleCompact => {
                self.compact_mode = !self.compact_mode;
                (self, vec![])
            }

            // Shift+Tab: rotate the Approval mode (ADR-0050). A fire-through to
            // the Agent; the new mode returns via `Event::ApprovalModeChanged`
            // and updates the footer. Works with no Run in flight (Session-
            // scoped) and with no Approval open (the gate above already returned
            // when one was).
            Key::CycleApprovalMode => (self, vec![Effect::Agent(AgentCommand::CycleApprovalMode)]),

            _ => (self, vec![]),
        };
        (screen, effects)
    }

    // The Approval-block key gate (ADR-0049): the arrow/Enter keys drive the
    // pure [`SelectionList`], the numbered digits quick-select, and `y`/`n`/`a`
    // stay as a legacy superset. Escape DENIES THIS TOOL and lets the Run
    // CONTINUE (qwen `ToolConfirmationMessage.tsx:106-114`: Escape →
    // `ToolConfirmationOutcome.Cancel` = deny the call, not abort the Run) -
    // matching the `No, suggest changes (esc)` option label. (Escape only
    // cancels the whole Run when NO approval is open and a Run is streaming; that
    // arm lives in `handle_key`.) A digit here always resolves immediately (the
    // radio has 3 rows, so the buffered path in `SelectionList` never arises),
    // so the `now` fed to the fold is irrelevant - passed as 0. Every other key
    // is swallowed with no effect, so a stray key can never leak to the Composer.
    fn handle_approval_key(mut self, key: Key) -> (Self, Vec<Effect>) {
        // Bind the pending Approval once (the caller only enters here when one is
        // open); a defensive `None` swallows the key with no effect, so no
        // `unwrap` is needed below.
        let Some(pending) = self.pending_approval.as_mut() else {
            return (self, vec![]);
        };
        // Escape denies THIS tool and the Run continues: route it through the
        // widget's `Cancelled` outcome to the `No, suggest changes (esc)` option
        // (`Decision::Deny`), matching qwen and the label. The Run is NOT
        // cancelled here.
        if key == Key::Escape {
            let id = pending.approval_id.clone();
            return self.resolve_approval(AgentCommand::Approve(id, Decision::Deny));
        }
        // The legacy quick-keys stay a superset of the radio.
        let quick = match key {
            Key::Char('y') => Some(Decision::Approve),
            Key::Char('n') => Some(Decision::Deny),
            Key::Char('a') => Some(Decision::ApproveAlways),
            _ => None,
        };
        if let Some(decision) = quick {
            let id = pending.approval_id.clone();
            return self.resolve_approval(AgentCommand::Approve(id, decision));
        }

        // Otherwise drive the SelectionList with the mapped key.
        let Some(sel_key) = approval_selection_key(&key) else {
            // A key the radio does not act on: swallowed, no effect.
            return (self, vec![]);
        };
        match pending.selection.handle(sel_key, 0) {
            SelectionOutcome::Selected(i) => match decision_for_option(i) {
                Some(decision) => {
                    let id = pending.approval_id.clone();
                    self.resolve_approval(AgentCommand::Approve(id, decision))
                }
                // Out of range (never, the list is length 3): swallow.
                None => (self, vec![]),
            },
            // A move redraws the radio; the cancel/ignore paths (Escape is
            // handled above) leave the block open.
            SelectionOutcome::Moved | SelectionOutcome::Cancelled | SelectionOutcome::Ignored => {
                (self, vec![])
            }
        }
    }

    // The question-modal key gate (ADR-0057): the arrow/Enter keys drive the
    // CURRENT question's [`SelectionList`], the numbered digits quick-select, and
    // Escape DECLINES the whole round-trip (the qwen `Cancel` outcome = "User
    // declined to answer the questions."). Selecting a real option records its
    // label and advances the cursor; selecting the auto-"Other" row focuses the
    // composer to capture a free-form answer (handled by
    // [`Screen::handle_other_capture_key`] once `collecting_other` is set). Every
    // other key is swallowed, so a stray key never leaks to the Composer while
    // the modal holds the keyboard.
    fn handle_question_key(mut self, key: Key) -> (Self, Vec<Effect>) {
        let Some(pending) = self.pending_question.as_mut() else {
            return (self, vec![]);
        };
        // Escape declines the whole round-trip - the modal's counterpart of the
        // Approval's `No, suggest changes (esc)`, but here it ends the questions
        // (qwen returns "User declined to answer the questions.").
        if key == Key::Escape {
            return self.decline_question();
        }

        let cursor = pending.cursor;
        // Defensive: a cursor past the last question means the modal should have
        // resolved already; swallow rather than index out of range.
        let Some(sel_key) = question_selection_key(&key) else {
            return (self, vec![]);
        };
        let Some(selection) = pending.per_question.get_mut(cursor) else {
            return (self, vec![]);
        };
        match selection.handle(sel_key, 0) {
            SelectionOutcome::Selected(index) => self.answer_option(cursor, index),
            // A move redraws the radio; cancel is handled above (Escape).
            SelectionOutcome::Moved | SelectionOutcome::Cancelled | SelectionOutcome::Ignored => {
                (self, vec![])
            }
        }
    }

    // Row `index` of question `cursor` was selected. A REAL option records its
    // label and advances; the auto-"Other" row instead arms free-form capture
    // (focus the composer, set `collecting_other`) so the user types the answer.
    fn answer_option(mut self, cursor: usize, index: usize) -> (Self, Vec<Effect>) {
        let Some(pending) = self.pending_question.as_mut() else {
            return (self, vec![]);
        };
        let Some(question) = pending.questions.get(cursor).cloned() else {
            return (self, vec![]);
        };
        if PendingQuestion::is_other_row(&question, index) {
            // Focus the composer to capture the free-form answer; the next submit
            // fills it (handled by `handle_other_capture_key`).
            pending.collecting_other = Some(cursor);
            // Clear any pre-existing draft FIRST (M2): a stale in-progress message
            // the user had typed before the modal opened must NOT leak into - or be
            // committed as - the "Other" answer. `steered_ok` resets the whole
            // composer (draft + menu + overlay) without recording it as a prompt.
            self.composer.steered_ok();
            return (self, vec![Effect::FocusComposer]);
        }
        match PendingQuestion::option_label(&question, index) {
            Some(label) => self.record_answer(cursor, label),
            // Out of range (never in practice): swallow.
            None => (self, vec![]),
        }
    }

    // The "Other" capture gate (ADR-0057): while `collecting_other` is set the
    // composer edits the free-form answer, so this defers ALL keys to it EXCEPT
    // the eventual submit, which it intercepts to fill the answer instead of
    // sending a prompt/steer. Escape here cancels the capture and returns to the
    // radio (the answer is not yet given), not the whole round-trip.
    fn handle_other_capture_key(mut self, key: Key) -> (Self, Vec<Effect>) {
        // Escape backs out of the free-form capture: drop `collecting_other` and
        // hand the keyboard back to the radio (the user can pick again).
        if key == Key::Escape {
            if let Some(pending) = self.pending_question.as_mut() {
                pending.collecting_other = None;
            }
            return (self, vec![]);
        }

        // Offer the key to the composer (the free-form answer is a draft). A
        // Submit/Steer effect is the "answer is ready" signal: intercept it and
        // fill the answer from the draft instead of prompting/steering.
        let cursor = self
            .pending_question
            .as_ref()
            .and_then(|p| p.collecting_other);
        let key = match self.composer.handle_key(UngatedKey(key), self.status) {
            KeyOutcome::Consumed { effects, notice } => {
                if let Some(text) = notice {
                    self.transcript.info(text);
                }
                if let Some(cursor) = cursor
                    && effects.iter().any(is_submit_or_steer)
                {
                    // The composer would have submitted/steered: capture the draft
                    // as the free-form answer instead. Read then clear the draft
                    // (steered_ok clears without recording it as a prompt).
                    let answer = self.composer.view().draft.trim().to_string();
                    if answer.is_empty() {
                        // An empty "Other" submit is a no-op: keep collecting.
                        return (self, vec![]);
                    }
                    self.composer.steered_ok();
                    return self.record_answer(cursor, answer);
                }
                // ONLY Submit/Steer (the answer-ready signal above) and pure text
                // entry act during "Other" capture. Any OTHER composer effect - a
                // leading `/` opening the slash menu, a client command firing - must
                // NOT leak out while the question modal is open, so commit the text
                // edit but SWALLOW the effects rather than firing them. (Rendering
                // the draft is enough; the slash menu/command would break the modal.)
                let non_editing = effects.iter().any(|e| !is_composer_edit(e));
                if non_editing {
                    return (self, vec![]);
                }
                return (self, effects);
            }
            KeyOutcome::Refused(key) => key,
        };
        // A refused key (the composer did not act): swallow it while the modal is
        // up, so nothing leaks to the fold arms below.
        let _ = key;
        (self, vec![])
    }

    // Records `answer` for question `cursor`, advances the cursor, and - if every
    // question is now answered - resolves the round-trip (emit the answers, clear
    // the modal, refocus the composer). Mirrors `resolve_approval`.
    fn record_answer(mut self, cursor: usize, answer: String) -> (Self, Vec<Effect>) {
        let Some(pending) = self.pending_question.as_mut() else {
            return (self, vec![]);
        };
        if let Some(slot) = pending.answers.get_mut(cursor) {
            *slot = Some(answer);
        }
        pending.collecting_other = None;
        pending.cursor = cursor + 1;
        if pending.cursor >= pending.questions.len() {
            return self.resolve_question();
        }
        (self, vec![])
    }

    // Every question answered: build the `(index, value)` answer set, emit the
    // AnswerQuestion command, clear the modal, and refocus the composer (mirrors
    // `resolve_approval` -> `clear_approval` -> FocusComposer).
    fn resolve_question(mut self) -> (Self, Vec<Effect>) {
        let Some(pending) = self.pending_question.take() else {
            return (self, vec![]);
        };
        let answers: Vec<(usize, String)> = pending
            .answers
            .into_iter()
            .enumerate()
            .filter_map(|(i, a)| a.map(|value| (i, value)))
            .collect();
        let command = AgentCommand::AnswerQuestion(pending.question_id, Ok(answers));
        (self, vec![Effect::Agent(command), Effect::FocusComposer])
    }

    // Escape declined the whole round-trip: emit the decline and clear the modal.
    fn decline_question(mut self) -> (Self, Vec<Effect>) {
        let Some(pending) = self.pending_question.take() else {
            return (self, vec![]);
        };
        let command = AgentCommand::AnswerQuestion(
            pending.question_id,
            Err("User declined to answer the questions.".to_string()),
        );
        (self, vec![Effect::Agent(command), Effect::FocusComposer])
    }

    // The Help-overlay key gate (qwen `Help` `useKeypress`): while the panel is up
    // it holds the keyboard like the Approval modal - `Esc` closes it, and `?`/`q`
    // are the same convenience closers qwen offers; EVERY other key is swallowed
    // with no effect, so nothing leaks to the Composer/Agent behind the overlay.
    // Closing hands focus back to the composer (the modal counterpart of the
    // Approval's `FocusModal`).
    fn handle_help_key(mut self, key: Key) -> (Self, Vec<Effect>) {
        match key {
            Key::Escape | Key::Char('?') | Key::Char('q') => {
                self.help_open = false;
                (self, vec![Effect::FocusComposer])
            }
            // Every other key is swallowed with no effect while Help is open.
            _ => (self, vec![]),
        }
    }

    /// Drives the pending Approval radio's digit quick-select timeout at host
    /// time `now` (ADR-0049, the host-driven `expire` seam - no background
    /// timer). The adapter's tick calls this each frame; when a buffered digit
    /// reaches its deadline the radio auto-selects the buffered row and this
    /// resolves the Approval with that option's Decision. With the 3-row
    /// approval every digit selects immediately, so the buffer never fills and
    /// this is a no-op today - but the seam is live so Phase 5's longer dialogs
    /// reuse the same mechanic. No pending Approval, or nothing buffered: no
    /// effect.
    pub fn expire_approval(mut self, now: u64) -> (Self, Vec<Effect>) {
        let Some(pending) = self.pending_approval.as_mut() else {
            return (self, vec![]);
        };
        match pending.selection.expire(now) {
            SelectionOutcome::Selected(i) => match decision_for_option(i) {
                Some(decision) => {
                    let id = pending.approval_id.clone();
                    self.resolve_approval(AgentCommand::Approve(id, decision))
                }
                None => (self, vec![]),
            },
            _ => (self, vec![]),
        }
    }

    // Clears the Approval block and emits the resolving Agent command, ahead of
    // the FocusComposer + commit seam (the confirming ToolCall commits later,
    // when its ToolResult supersedes it - never with the Approval rows).
    fn resolve_approval(self, command: AgentCommand) -> (Self, Vec<Effect>) {
        let (t, mut effects) = self.clear_approval();
        let mut out = vec![Effect::Agent(command)];
        out.append(&mut effects);
        (t, out)
    }

    /// Records how the `Submit` effect went: `Ok` appends the user line (which
    /// the fullscreen renderer draws next frame, ADR-0046) and hands the Composer
    /// its success - [`Composer::submitted_ok`] records the prompt into the ring,
    /// clears the draft, and mints the on-disk `HistoryAppend`. `Err(Busy)`
    /// means the submit raced a starting Run - retry as Steering. The retry
    /// lives HERE (ADR-0034): it touches only Agent status, and the draft
    /// must survive it (the Composer is not told, so nothing clears).
    pub fn submitted(
        mut self,
        prompt: impl Into<String>,
        result: Result<(), Busy>,
    ) -> (Self, Vec<Effect>) {
        let prompt = prompt.into();
        match result {
            Ok(()) => {
                self.transcript.user(prompt.clone());
                // The settled User line renders in the fullscreen body next frame
                // (ADR-0046); this exit just hands the Composer its success and
                // returns the resulting effects (the on-disk `HistoryAppend`).
                let effects = self.composer.submitted_ok(&prompt);
                (self, effects)
            }
            Err(Busy) => {
                self.status = Status::Running;
                (self, vec![Effect::Agent(AgentCommand::Steer(prompt))])
            }
        }
    }

    /// Records how the `Steer` effect went: `Ok` clears the Composer's draft
    /// (the pending line arrives via `steering_queued`); `Err(Idle)` means
    /// the Run ended between keypress and call - retry as a submit. Same
    /// retry rule as [`Screen::submitted`]: only status flips, the draft
    /// survives.
    pub fn steered(
        mut self,
        text: impl Into<String>,
        result: Result<(), Idle>,
    ) -> (Self, Vec<Effect>) {
        let text = text.into();
        match result {
            Ok(()) => {
                self.composer.steered_ok();
                // Nothing terminal to record here (the pending steering marker
                // arrives via `steering_queued`); the fullscreen renderer redraws
                // the transcript next frame (ADR-0046), so no effect is due.
                (self, vec![])
            }
            Err(Idle) => {
                self.status = Status::Idle;
                (self, vec![Effect::Agent(AgentCommand::Submit(text))])
            }
        }
    }

    /// Appends an info line (Resume drift notes, adapter-side news). The info
    /// line renders in the fullscreen body next frame (ADR-0046), so no effect is
    /// due.
    pub fn info(mut self, text: impl Into<String>) -> (Self, Vec<Effect>) {
        self.transcript.info(text);
        (self, vec![])
    }

    /// Resets to a truthful state after the Agent crashed and was restarted:
    /// its subscriber map and Conversation are gone, so the Screen must not
    /// claim a Run is still running or an Approval is still pending.
    pub fn agent_down(self) -> (Self, Vec<Effect>) {
        self.close_abnormally("agent restarted; session history was reset".to_string())
    }

    /// The Transcript, read-only - the render adapter's window (ADR-0034),
    /// like [`Screen::composer`]. The view reads everything it draws and
    /// caches through it: the settled [`items`], the [`revision`] the
    /// RenderCache keys on, and the two streaming reads. No `&mut`
    /// counterpart on purpose: the store mutates only inside the folds and
    /// the submitted/steered outcome hooks, so the TEA invariant holds.
    ///
    /// [`items`]: Transcript::items
    /// [`revision`]: Transcript::revision
    pub fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    /// Whether live model output is currently streaming - reasoning under the
    /// `✦ Thinking` tail or assistant answer text. The adapter resets the lull
    /// clock while this holds, and the render gate shows the idle animation only
    /// while it does NOT - one predicate, so the two can never disagree.
    pub fn has_live_stream(&self) -> bool {
        !self.transcript().streaming_thinking().is_empty()
            || !self.transcript().streaming_text().is_empty()
    }

    /// The Composer, read-only - the render adapter's window (ADR-0034). It
    /// reads everything it draws through [`Composer::view`]: the draft, the
    /// char-index cursor, and the open overlay. No `&mut` counterpart on
    /// purpose: the Composer mutates only inside the folds and the
    /// submitted/steered hooks, so the TEA invariant holds.
    pub fn composer(&self) -> &Composer {
        &self.composer
    }

    // ---- Transcript scrolling (ADR-0046, Stage 2) --------------------------
    //
    // The pure core holds only the scroll INTENT (`scroll_lines` + `follow_tail`),
    // never geometry: these helpers move the intent, and the render clamps it to
    // the live viewport each frame ([`components::anchor_clip`]). Scrolling up
    // detaches from the tail; reaching the bottom re-attaches. `last_body_height`
    // supplies the page step, recorded by the adapter through
    // [`Screen::note_body_height`].

    /// Records the last body zone height the renderer drew (ADR-0046): the adapter
    /// calls this each frame so the geometry-free core has a page step for
    /// PageUp/PageDown. Pure state-carry, not a fold - no effects, no Transcript
    /// touch; it only caches a viewport fact the render already computed.
    pub fn note_body_height(&mut self, height: usize) {
        self.last_body_height = height;
    }

    /// Scrolls the transcript UP by `step` wrapped rows, DETACHING from the tail:
    /// new streaming content no longer yanks the view down. The render clamps
    /// `scroll_lines` to the top, so an over-scroll simply pins to the oldest row.
    fn scroll_up(mut self, step: usize) -> Self {
        self.follow_tail = false;
        self.scroll_lines = self.scroll_lines.saturating_add(step);
        self
    }

    /// Scrolls the transcript DOWN by `step` wrapped rows toward the tail. Reaching
    /// the bottom (`scroll_lines == 0`) RE-ATTACHES to the tail, so the view
    /// resumes following new content.
    fn scroll_down(mut self, step: usize) -> Self {
        self.scroll_lines = self.scroll_lines.saturating_sub(step);
        if self.scroll_lines == 0 {
            self.follow_tail = true;
        }
        self
    }

    /// Scrolls UP one page (the last drawn body height, min one row so a pre-frame
    /// press still moves): the keyboard/`Ctrl-S` counterpart of a wheel-up burst.
    fn page_up(self) -> Self {
        let page = self.last_body_height.max(1);
        self.scroll_up(page)
    }

    /// Scrolls DOWN one page, re-attaching at the bottom like [`Screen::scroll_down`].
    fn page_down(self) -> Self {
        let page = self.last_body_height.max(1);
        self.scroll_down(page)
    }

    /// Jumps to the TOP of the transcript (End-of-scroll-up): detaches and asks for
    /// the maximum scroll, which the render clamps to the oldest row. `usize::MAX`
    /// is the "scroll as far up as possible" sentinel the clamp saturates.
    pub fn scroll_to_top(mut self) -> Self {
        self.follow_tail = false;
        self.scroll_lines = usize::MAX;
        self
    }

    /// RE-ATTACHES to the tail (End / bottom-of-scroll): follow the newest content
    /// again, scroll intent back to zero.
    pub fn scroll_to_bottom(mut self) -> Self {
        self.follow_tail = true;
        self.scroll_lines = 0;
        self
    }

    // ---- Internals ---------------------------------------------------------

    // The name of the newest live ToolCall - a `TranscriptItem::ToolCall` still
    // awaiting its result (a ToolResult supersedes the call, so any surviving
    // ToolCall item is unresolved). Because the batch runs sequentially
    // (`run::batch`), at most one call is live at a time, and it is the one the
    // pending Approval gates (ADR-0049). `None` when no call is live (defensive:
    // the gate is only reached mid-call, so in practice one always is).
    fn newest_live_tool_name(&self) -> Option<&str> {
        self.transcript()
            .items()
            .iter()
            .rev()
            .find_map(|item| match item {
                TranscriptItem::ToolCall { name, .. } => Some(name.as_str()),
                _ => None,
            })
    }

    fn clear_approval(mut self) -> (Self, Vec<Effect>) {
        match self.pending_approval {
            None => (self, vec![]),
            Some(_) => {
                self.pending_approval = None;
                (self, vec![Effect::FocusComposer])
            }
        }
    }

    // An abnormal close (cancel, error, agent-down): salvage whatever was
    // still streaming and note WHY, go idle, and resolve any pending Approval.
    // The note is this fold's Voice; the flush-before-note ordering is the
    // store's [`Transcript::close`].
    fn close_abnormally(mut self, note: String) -> (Self, Vec<Effect>) {
        self.transcript.close(Some(note));
        self.status = Status::Idle;
        // A cancel/error/agent-down clears any open question modal too (its
        // reply oneshot dies with the aborted Run, so the tool call unwinds); it
        // must not linger claiming an answer is still due.
        self.pending_question = None;
        self.clear_approval()
    }
}

/// The submit raced a starting Run (baud's `{:error, :busy}`). Marker so
/// [`Screen::submitted`]'s signature reads like baud's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy;

/// The Run ended between keypress and steer (baud's `{:error, :idle}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Idle;

// ---------------------------------------------------------------------------
// Free functions (pure helpers).
// ---------------------------------------------------------------------------

// Maps a Screen [`Key`] to the [`SelectionKey`] the Approval radio acts on
// (ADR-0049), or `None` for a key the radio ignores. ArrowUp/Down navigate;
// Enter selects; a `Char` digit `1`-`9` quick-selects (the numbered rows). `y`,
// `n`, `a` and Escape are handled by the caller BEFORE this (the legacy superset
// + Run-cancel), so they never reach here.
fn approval_selection_key(key: &Key) -> Option<SelectionKey> {
    match key {
        Key::ArrowUp => Some(SelectionKey::Up),
        Key::ArrowDown => Some(SelectionKey::Down),
        Key::Enter => Some(SelectionKey::Enter),
        Key::Char(c) if c.is_ascii_digit() => Some(SelectionKey::Digit(*c as u8 - b'0')),
        _ => None,
    }
}

// Maps a Screen [`Key`] to the [`SelectionKey`] the question radio acts on
// (ADR-0057), or `None` for a key it ignores. The same mapping as the Approval
// radio (ArrowUp/Down navigate, Enter selects, a digit quick-selects); Escape is
// handled by the caller (decline) before this, so it never reaches here.
fn question_selection_key(key: &Key) -> Option<SelectionKey> {
    approval_selection_key(key)
}

// Whether an effect is a composer Submit or Steer (ADR-0057): the "answer is
// ready" signal the "Other" capture intercepts to fill the free-form answer
// instead of sending a prompt/steer.
fn is_submit_or_steer(effect: &Effect) -> bool {
    matches!(
        effect,
        Effect::Agent(AgentCommand::Submit(_)) | Effect::Agent(AgentCommand::Steer(_))
    )
}

// Whether an effect is a pure composer text-edit / redraw effect that is SAFE to
// fire while the "Other" capture modal is up (ADR-0057). Text entry commits the
// draft and repaints; anything else the composer might mint - the slash menu
// (`Command`), a chosen selector row (`SelectorChosen`), a `@path` file search
// (`FileSearch`), or a history append/agent call - is composer MACHINERY that must
// not leak out while the question modal owns the screen, so it is swallowed. Only
// the answer-ready Submit/Steer (handled above) and these edits act during capture.
fn is_composer_edit(effect: &Effect) -> bool {
    matches!(effect, Effect::FocusComposer | Effect::FocusModal)
}

// The closing note a stop reason earns: nothing for the two normal ends, a
// terse `turn stopped: :{reason}` otherwise. This fold's Voice - the store
// records what it is handed.
fn stop_reason_note(stop_reason: StopReason) -> Option<String> {
    match stop_reason {
        StopReason::EndTurn | StopReason::ToolUse => None,
        other => Some(format!("turn stopped: :{other}")),
    }
}

// The semantic pressure level (ADR-0008) against the live window: the target is
// the live window (budget - reserve), the mark Eviction fires at; the low-water
// mark is the Compaction Target. Above the target is Critical, between the marks
// Elevated, at/below the low-water mark Ok. The bounds are inclusive on the
// lower side.
fn pressure_level(estimate: u64, budget: u64, reserve: u64, slack: f64) -> PressureLevel {
    let target = budget.saturating_sub(reserve);
    let low_water = compaction_target(budget, reserve, slack);
    if estimate > target {
        PressureLevel::Critical
    } else if estimate > low_water {
        PressureLevel::Elevated
    } else {
        PressureLevel::Ok
    }
}

// One Compaction-progress marker line (CONTEXT.md: Compaction). The `⟨ … ⟩`
// glyph pair marks it as a summary fold in the Housekeeping tone,
// distinct from the `✂` eviction glyph; the tint comes from the tone, never
// from this text.
fn compaction_line(status: &str) -> String {
    format!("⟨ compaction: {status} → summary ⟩")
}

#[cfg(test)]
#[path = "../../tests/ui/screen.rs"]
mod tests;
