//! UI Transcript - the pure functional core of the TUI (ADR-0001, The Elm
//! Architecture).
//!
//! The Transcript is the display-side history of the Session and every rule
//! about it. No terminal, no async, no IO: [`Transcript::apply_event`] folds
//! agent [`Event`]s in and [`Transcript::handle_key`] folds key presses in,
//! each returning a new `Transcript` plus a `Vec<Effect>` the adapter carries
//! out. This is the seam ADR-0001 asks for: the ratatui view (`ui.rs`) is one
//! adapter over this core, and the test suite is the second.
//!
//! ## Transcript items
//!
//! `messages` holds render items, oldest first (see [`TranscriptItem`]):
//! `User`, `Assistant`, `Thinking`, `ToolCall`, `ToolResult`, `Block` (the
//! semantic display vocabulary, ADR-0008), and `Info`.
//!
//! ## Rules encoded here
//!
//! * Streaming is STATELESS: [`Event::MessageUpdate`] carries the accumulated
//!   snapshot, so the in-flight view is replaced wholesale per event - no delta
//!   accumulation. [`Event::MessageEnd`] materializes the snapshot into
//!   discrete items (Thinking first, then assistant text); a cancel/crash
//!   mid-stream materializes whatever the last snapshot held.
//! * Enter submits when idle and STEERS when running (the composer never
//!   locks). The submit/steer race at the Turn boundary is retried the other
//!   way via [`Transcript::submitted`] and [`Transcript::steered`].
//! * The Composer is edited HERE, not in the adapter: chars insert at the
//!   cursor (a char index), Alt-Enter and a trailing-backslash Enter insert
//!   hard newlines, Home/End work within the current line, and Up/Down are
//!   edge-triggered - history recall only from the draft's first/last line,
//!   cursor movement everywhere else.
//! * A pending Approval swallows every key except `y`, `n`, `a`, and `Escape`;
//!   `a` is approve-always (Standing Approval); Escape means Cancellation,
//!   which wins over the Approval.
//! * Presentment (CONTEXT.md): `ToolCall`/`ToolResult` items pass through
//!   [`crate::plugins::present`]; a crashing plugin is skipped with an info line
//!   (fail-open, ADR-0007), as is every `plugin_error` event the Turn reports.

use crate::conversation::{WaveStats, compaction_target, dead_mass_pct};
use crate::event::{Event, Stage};
use crate::llm::response::StopReason;
use crate::plugins::{self, Registered};
use crate::ui::draft;
use crate::ui::history::History;
use crate::ui::selector::{Selector, SelectorOutcome, SelectorRow};
use crate::ui::slash;
use crate::ui::streaming::Streaming;

/// The greeting line a fresh Transcript opens with.
const GREETING: &str = "suspenders ready. Enter submits, Esc cancels a running turn, Ctrl-T toggles thinking, Ctrl-C quits";

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
///   pair the call with its later `ToolResult` in the fold - the display never
///   interprets it.
/// * `ToolResult { name, summary, is_error, key_arg }` -
///   `{:tool_result, name, summary, is_error, key_arg}`, the default one-line
///   summary a plugin's `present` may replace; `key_arg` is the salient input
///   arg (path/command/pattern) carried over from the paired call so the merged
///   line reads `name  <key_arg> · <result>`, `None` for an unpaired result.
/// * `Block { title, lines }` - `{:block, title, lines}`: a titled block of
///   [`StyledLine`]s, the semantic display vocabulary (ADR-0008).
/// * `Info { text }` - `{:info, text}`.
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
        /// fold. The view never interprets or renders it.
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

/// The Agent's status as the Transcript sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    Idle,
    Running,
}

/// A pending run_command Approval: the id to resolve and the command shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    pub approval_id: String,
    pub command: String,
}

/// The open Slash Command menu (ADR-0032), exposed for rendering the way
/// [`PendingApproval`] is: the view reads it through [`Transcript::slash_menu`]
/// and draws the inline popup. `rows` are the commands matching the typed token
/// (via the generic selector's row shape); `highlight` is the index into `rows`
/// of the highlighted command. Empty `rows` means the typed token matches no
/// command - the popup shows "no matches" and Enter is an unknown-command
/// no-Turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashMenu {
    pub rows: Vec<SelectorRow>,
    pub highlight: usize,
}

/// The lifecycle of a committed selector-opening command's row list (ADR-0033),
/// owned by [`CommandSelector`]. `Loading` after commit while the adapter
/// fetches; `Ready` once [`Event::SelectorReady`] delivered rows into a
/// [`Selector`]; `Failed` on [`Event::SelectorFailed`]. Only `Ready` accepts
/// navigation/selection - a `Loading`/`Failed` overlay swallows Enter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectorStatus {
    Loading,
    Ready(Selector),
    Failed(String),
}

/// The owned command-selector overlay (ADR-0033): the sub-state entered when a
/// selector-opening command (`/model`) is committed. `command` is the opaque
/// command name the pure core carries back out on selection (it never learns
/// what the command does); `status` is the row list's lifecycle. Modeled on
/// [`PendingApproval`]: owned modal state the view reads through
/// [`Transcript::slash_view`]. The overlay does NOT own its filter - the draft
/// `rest` (after `/<name> `) filters the rows, consistent with Phase 3's menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSelector {
    pub command: String,
    pub status: SelectorStatus,
}

/// The current inline-popup view for the adapter to draw (ADR-0032/0033), one
/// query folding the two slash sub-states so the adapter matches once. `Menu`
/// is Phase 3's command palette (`rest = None`): the registry filtered by the
/// command token. `Selector` is the committed-command sub-state (`rest =
/// Some`): the overlay `status` plus, when `Ready`, the rows filtered by the
/// draft `rest` and the highlighted index into that filtered view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashView {
    Menu {
        rows: Vec<SelectorRow>,
        highlight: usize,
    },
    Selector {
        command: String,
        status: SelectorStatus,
        rows: Vec<SelectorRow>,
        highlight: usize,
    },
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
    /// Mouse wheel up - scrolls by a few lines where [`Key::PageUp`] scrolls
    /// by a whole page; otherwise handled identically in every state.
    WheelUp,
    /// Mouse wheel down - scrolls by a few lines where [`Key::PageDown`]
    /// scrolls by a whole page; otherwise handled identically in every state.
    WheelDown,
    ArrowUp,
    ArrowDown,
    Backspace,
    /// Move the Composer cursor one char left (clamped at the start).
    Left,
    /// Move the Composer cursor one char right (clamped at the end).
    Right,
    /// Jump to the start of the CURRENT LINE of the draft (readline behavior
    /// within a line, not the whole draft).
    Home,
    /// Jump to the end of the CURRENT LINE of the draft.
    End,
    /// Alt-Enter: insert a hard newline into the draft at the cursor. Named
    /// (rather than `Char('\n')`) so the modal's swallow-everything rule and
    /// the adapter's mapping both read as intent.
    InsertNewline,
    /// Ctrl-T: toggle the expanded rendering of settled Thinking items.
    ToggleThinking,
    /// Ctrl-O: toggle the expanded rendering of settled tool Blocks.
    ToggleTools,
    Char(char),
    /// A key the core does not act on (function keys, etc.).
    Other,
    /// A named key that only matters while an Approval modal is open (`y`,
    /// `n`, `a` are `Char`; this is any other named key we want to name).
    Named(String),
}

/// The Agent command an [`Effect::Agent`] carries (baud's `{:agent, ...}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentCommand {
    Submit(String),
    Steer(String),
    /// Resolve a pending Approval with a decision.
    Approve(String, Decision),
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

/// How far one scroll Effect moves: the wheel steps by [`ScrollStep::Line`]s,
/// the page keys by whole viewport [`ScrollStep::Page`]s. The core only names
/// the granularity - the adapter's `ui::viewport` knows the geometry and turns
/// it into an actual line count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollStep {
    /// A few lines (one mouse-wheel tick).
    Line,
    /// One viewport page (PageUp/PageDown).
    Page,
}

/// An Effect the adapter carries out after a fold (baud's `effect` type).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Call the Agent (submit/steer/approve/cancel).
    Agent(AgentCommand),
    /// Move keyboard focus to the Approval modal.
    FocusModal,
    /// Move keyboard focus back to the composer.
    FocusComposer,
    /// Pin the viewport to the bottom (follow the tail).
    PinBottom,
    /// Scroll the viewport up by one [`ScrollStep`].
    ScrollUp(ScrollStep),
    /// Scroll the viewport down by one [`ScrollStep`].
    ScrollDown(ScrollStep),
    /// Persist a submitted prompt into the on-disk history file.
    HistoryAppend(String),
    /// A committed Slash Command (ADR-0032): the pure core recognized `/name`
    /// and hands it to the adapter to run. Commands carry no inline arg today -
    /// a selector-opening command's sub-filter comes from the draft `rest`
    /// (`slash_view`), not from this payload. The core does not know what any
    /// command does - this payload is command-agnostic.
    Command { name: String },
    /// A row was chosen from a committed command's selector (ADR-0033): the
    /// opaque command `name` and the selected row's `value`. The adapter
    /// interprets it (e.g. `/model` swaps the Active Model and persists); the
    /// pure core neither knows nor cares. Phase 4b implements the arm.
    SelectorChosen { command: String, value: String },
}

/// The pure Transcript state (baud's `%Baud.UI.Transcript{}`).
///
/// `plugins` are not `Clone`/`PartialEq`, so the core is not `Clone`; the fold
/// takes and returns an owned `Transcript` by value, mirroring the Elixir
/// struct-threading style.
pub struct Transcript {
    pub messages: Vec<TranscriptItem>,
    /// The configured Plugins whose pure `present` runs inside the fold.
    pub plugins: Vec<Registered>,
    /// The in-flight streaming snapshot and its materialize rules. Owned by
    /// [`crate::ui::streaming`]; read through [`Transcript::streaming_text`] and
    /// [`Transcript::streaming_thinking`].
    streaming: Streaming,
    pub status: Status,
    pub pending_approval: Option<PendingApproval>,
    pub token_estimate: Option<u64>,
    pub context_budget: Option<u64>,
    pub eviction_slack: f64,
    pub pressure_level: PressureLevel,
    /// The live Dead Mass share (integer percent) from the most recent
    /// [`Event::ContextPressure`], for the status bar. `None` until the first
    /// pressure event; folded into the Tokens segment as a `· N% dead` tail so
    /// context reclamation is legible AS IT STANDS - not the pre-reclaim
    /// snapshot a past wave found (which a wave clears the instant it fires).
    pub dead_mass_pct: Option<u64>,
    pub input_value: String,
    pub input_cursor: usize,
    /// The highlighted command in the Slash Command menu (ADR-0032), an index
    /// into the FILTERED rows. Only meaningful while the draft `is_slash`; the
    /// menu itself is derived on demand ([`Transcript::slash_menu`]) from the
    /// draft (the filter) and the `&'static` registry, so this holds just the
    /// cursor. Clamped to the filtered length as typing narrows the menu.
    slash_cursor: usize,
    /// The open command-selector overlay (ADR-0033), or `None` when no
    /// selector-opening command is active. Set when a command whose descriptor
    /// `opens_selector` is committed (to a `Loading` overlay), folded to
    /// `Ready`/`Failed` by [`Transcript::apply_event`], and cleared on
    /// selection, Escape, or backspacing out of the sub-state. Owned modal
    /// state, mirroring [`Transcript::pending_approval`]; the filter is NOT
    /// owned here - the draft `rest` filters the rows.
    command_selector: Option<CommandSelector>,
    /// The prompt-history ring and its Readline-style recall rules. Owned by
    /// [`crate::ui::history`]; navigated from the Up/Down arms of
    /// [`Transcript::handle_key`] and appended to by [`Transcript::record_submit`].
    history: History,
    /// Whether settled [`TranscriptItem::Thinking`] items render expanded (the
    /// full text) instead of the collapsed one-line form. Toggled by
    /// [`Key::ToggleThinking`] (Ctrl-T); defaults collapsed.
    pub thinking_expanded: bool,
    /// Whether settled [`TranscriptItem::Block`] items (diffs, tool output)
    /// render expanded (the full body) instead of the collapsed one-line
    /// title. Toggled by [`Key::ToggleTools`] (Ctrl-O); defaults collapsed -
    /// the same detail-on-demand rule as `thinking_expanded`, applied to the
    /// machinery plane so a burst of tool output can't eat the window.
    pub tools_expanded: bool,
    /// Bumped whenever `messages` changes OTHER than by appending (today only
    /// `SteeringDelivered`, which removes its pending info line from wherever
    /// it sits). The frontend's per-item render cache extends incrementally
    /// while this holds still and rebuilds when it moves - appends are the hot
    /// path, structural edits the rare one. Every other `messages` mutation is
    /// a push and must stay one (or bump this).
    pub messages_revision: u64,
}

/// The options a fresh Transcript is opened with (baud's `new/1` keyword opts).
#[derive(Default)]
pub struct TranscriptOpts {
    pub context_budget: Option<u64>,
    pub eviction_slack: f64,
    pub plugins: Vec<Registered>,
    pub history: Vec<String>,
}

impl Transcript {
    /// A fresh Transcript, opened with the greeting info line and idle status.
    pub fn new(opts: TranscriptOpts) -> Self {
        Transcript {
            messages: vec![TranscriptItem::Info {
                text: GREETING.to_string(),
            }],
            plugins: opts.plugins,
            streaming: Streaming::idle(),
            status: Status::Idle,
            pending_approval: None,
            token_estimate: None,
            context_budget: opts.context_budget,
            eviction_slack: opts.eviction_slack,
            pressure_level: PressureLevel::Ok,
            dead_mass_pct: None,
            input_value: String::new(),
            input_cursor: 0,
            slash_cursor: 0,
            command_selector: None,
            history: History::new(opts.history),
            thinking_expanded: false,
            tools_expanded: false,
            messages_revision: 0,
        }
    }

    // ---- Agent events ------------------------------------------------------

    /// Folds one [`Event`] into the Transcript. The event vocabulary is
    /// enumerated in [`crate::event`]; unknown events are ignored (a new event
    /// must not break an old subscriber).
    pub fn apply_event(mut self, event: Event) -> (Self, Vec<Effect>) {
        match event {
            Event::TurnStarted(_reference) => {
                self.status = Status::Running;
                self.streaming.clear();
                (self, vec![Effect::PinBottom])
            }

            Event::MessageStart { .. } => {
                self.streaming.start();
                (self, vec![])
            }

            // Stateless streaming: the snapshot replaces the in-flight view.
            Event::MessageUpdate { content, .. } => {
                self.streaming.update(content);
                (self, vec![])
            }

            // Materialize the finished message into discrete items (Thinking
            // from the last snapshot, text from the final content); the seam
            // in `ui::streaming` owns that asymmetry.
            Event::MessageEnd { content, .. } => {
                for item in self.streaming.end(&content) {
                    self.messages.push(item);
                }
                (self, vec![])
            }

            // Live context-pressure indication: refresh the status bar's token
            // estimate, budget, and LIVE Dead Mass share mid-Turn and name the
            // semantic pressure level (ADR-0008). NEVER a Transcript item. The
            // Dead Mass here is the current figure, refreshed every pass - the
            // bar tracks it, not a wave's cleared snapshot.
            Event::ContextPressure {
                token_estimate,
                context_budget,
                max_tokens_reserve,
                dead_mass,
            } => {
                self.token_estimate = Some(token_estimate);
                self.context_budget = Some(context_budget);
                self.dead_mass_pct = Some(dead_mass_pct(dead_mass));
                self.pressure_level = pressure_level(
                    token_estimate,
                    context_budget,
                    max_tokens_reserve,
                    self.eviction_slack,
                );
                (self, vec![])
            }

            // Stamp the call's `id` (for later result-pairing) and give the
            // live in-flight line a clean summary: the salient `key_arg`
            // (path/command/pattern), falling back to the raw `key=value`
            // summary only when no arg stands out.
            Event::ToolCall { id, name, input } => {
                let summary =
                    key_arg(&name, &input).unwrap_or_else(|| summarize_input(&input));
                let item = TranscriptItem::ToolCall { id, name, summary };
                self.present(item, &std::collections::HashMap::new());
                (self, vec![])
            }

            // Merge the result with its call into ONE line: find the pending
            // `ToolCall` by `id` (NEVER by position - parallel tool calls
            // interleave), recover its `key_arg`, remove the redundant call
            // line, and stamp the arg onto the result BEFORE Presentment (a
            // plugin's `present` may replace the item with a Block, so stamping
            // after would stamp a dropped item). Removing the call is a
            // NON-append structural edit, so it bumps `messages_revision` - the
            // RenderCache desyncs without it (mirrors `SteeringDelivered`). An
            // unpaired result (governor-injected, no live call) removes nothing,
            // does not bump, and carries no `key_arg`.
            Event::ToolResult {
                id,
                name,
                content,
                is_error,
                artifacts,
            } => {
                // Recover the paired call's `key_arg` (its summary already IS the
                // salient arg - `key_arg` never yields an empty string, so no
                // re-check here; the render layer normalizes any empty value once).
                let key_arg = self
                    .messages
                    .iter()
                    .rposition(|m| matches!(m, TranscriptItem::ToolCall { id: call_id, .. } if *call_id == id))
                    .map(|pos| {
                        let arg = match &self.messages[pos] {
                            TranscriptItem::ToolCall { summary, .. } => summary.clone(),
                            _ => unreachable!("rposition matched a ToolCall"),
                        };
                        self.messages.remove(pos);
                        self.messages_revision += 1;
                        arg
                    });
                let item = TranscriptItem::ToolResult {
                    name,
                    summary: summarize_result(&content),
                    is_error,
                    key_arg,
                };
                self.present(item, &artifacts);
                (self, vec![])
            }

            // A Plugin crashed and was skipped (fail-open, ADR-0007).
            Event::PluginError {
                plugin,
                stage,
                message,
            } => {
                let line = plugin_failure_line(&plugin, stage, &message);
                self.push_info(line);
                (self, vec![])
            }

            Event::ApprovalRequest {
                approval_id,
                command,
            } => {
                self.pending_approval = Some(PendingApproval {
                    approval_id,
                    command,
                });
                (self, vec![Effect::FocusModal])
            }

            Event::ApprovalResolved { approval_id, .. } => match &self.pending_approval {
                Some(pending) if pending.approval_id == approval_id => self.clear_approval(),
                _ => (self, vec![]),
            },

            // A Standing Approval covered the command: no modal was ever shown.
            Event::ApprovalAuto { command } => {
                self.push_info(format!("auto-approved (standing): {command}"));
                (self, vec![])
            }

            // Steering: queued shows a pending line; delivered promotes it to a
            // user line (the text is now in the Conversation).
            Event::SteeringQueued { text } => {
                self.push_info(pending_steering_line(&text));
                (self, vec![Effect::PinBottom])
            }

            Event::SteeringDelivered { text } => {
                let pending = TranscriptItem::Info {
                    text: pending_steering_line(&text),
                };
                if let Some(pos) = self.messages.iter().position(|m| m == &pending) {
                    self.messages.remove(pos);
                    // A non-append edit: settled items shifted, so any cached
                    // per-item render state upstream is stale.
                    self.messages_revision += 1;
                }
                self.messages.push(TranscriptItem::User { text });
                (self, vec![])
            }

            // The Session Log died (IO failure); the Session continues
            // unpersisted.
            Event::SessionLogError { message } => {
                self.push_info(format!(
                    "session log failed ({message}); this session will not resume"
                ));
                (self, vec![])
            }

            // A Nudge / Endgame rider entered the Conversation; it is always
            // visible, so the Transcript shows it as an info line.
            Event::VerifyNudge { text }
            | Event::VerifyFailedNudge { text }
            | Event::EmptyResponseNudge { text }
            | Event::ExploreNudge { text }
            | Event::WrapUpWarning { text }
            | Event::VerificationPass { text }
            | Event::FinalPass { text } => {
                self.push_info(text);
                (self, vec![])
            }

            Event::TurnFinished {
                stop_reason,
                token_estimate,
                context_budget,
            } => {
                self.flush_streaming();
                self.status = Status::Idle;
                self.token_estimate = Some(token_estimate);
                self.context_budget = Some(context_budget);
                self.note_stop_reason(stop_reason);
                (self, vec![])
            }

            // A Recovery Turn opened: its Voice prompt entered the
            // Conversation, so the Transcript shows it like every Nudge.
            Event::RecoveryTurn { text, .. } => {
                self.push_info(text);
                self.status = Status::Running;
                (self, vec![Effect::PinBottom])
            }

            // A malformed-tool-call re-draw (ADR-0030): silent to the model's
            // Conversation, never silent to the operator - an info line marks
            // each bounded re-draw.
            Event::Retry {
                attempt, budget, ..
            } => {
                self.push_info(format!(
                    "malformed tool call - re-drawing ({attempt}/{budget})"
                ));
                (self, vec![])
            }

            // The adapter delivered a committed command's selector rows: flip a
            // Loading overlay to Ready over a fresh Selector. Guarded so a stale
            // event that arrives after the overlay closed (Escape/selection) or
            // was never Loading is ignored - it must not resurrect a closed
            // popup.
            Event::SelectorReady(rows) => {
                if let Some(cs) = self.command_selector.as_mut()
                    && matches!(cs.status, SelectorStatus::Loading)
                {
                    cs.status = SelectorStatus::Ready(Selector::new(rows));
                }
                (self, vec![])
            }

            // The adapter could not produce the rows: flip a Loading overlay to
            // Failed. Same staleness guard as SelectorReady.
            Event::SelectorFailed(message) => {
                if let Some(cs) = self.command_selector.as_mut()
                    && matches!(cs.status, SelectorStatus::Loading)
                {
                    cs.status = SelectorStatus::Failed(message);
                }
                (self, vec![])
            }

            Event::TurnCancelled => self.close_abnormally("turn cancelled".to_string()),

            Event::TurnError { reason } => self.close_abnormally(format!("turn error: {reason}")),

            // An Eviction wave rewrote the request copy (CONTEXT.md: Eviction,
            // Dead Mass): recede ONE terse Info line naming the wave and its
            // at-wave (pre-reclaim) snapshot. The status bar does NOT derive from
            // this - it tracks the LIVE Dead Mass off `ContextPressure`, and this
            // wave has just cleared what it found. APPEND-ONLY - `push_info` is a
            // push, so this must NOT bump `messages_revision` (the wave line
            // keeps the RenderCache incremental; only a non-append edit bumps).
            Event::EvictionWave { stats } => {
                self.push_info(eviction_wave_line(&stats));
                (self, vec![])
            }

            // Compaction made progress: recede one Info line. Append-only, same
            // no-bump contract as the Eviction wave.
            Event::CompactionProgress { status } => {
                self.push_info(format!("compaction: {status}"));
                (self, vec![])
            }

            // Unknown / display-irrelevant events are ignored.
            _ => (self, vec![]),
        }
    }

    // ---- User intents ------------------------------------------------------

    /// Folds one key press into the Transcript. ALL keys route through here -
    /// Composer editing included - so every rule lives in the pure core
    /// (ADR-0001); the adapter only maps crossterm events to [`Key`]s.
    ///
    /// While an Approval is pending, only `y`, `n`, `a` and `Escape` do
    /// anything; every other key is swallowed - in particular, plain chars
    /// must NOT edit the Composer while the modal is open. Escape is
    /// Cancellation, which wins over the Approval.
    ///
    /// The Composer cursor (`input_cursor`) is a CHAR index into
    /// `input_value` - the codebase counts chars, not bytes, so multi-byte
    /// input never splits or panics.
    pub fn handle_key(mut self, key: Key) -> (Self, Vec<Effect>) {
        // Modal-open handling swallows everything but y/n/a/Escape.
        if let Some(pending) = self.pending_approval.clone() {
            let id = pending.approval_id;
            return match key {
                Key::Char('y') => {
                    let (t, mut effects) = self.clear_approval();
                    let mut out = vec![Effect::Agent(AgentCommand::Approve(id, Decision::Approve))];
                    out.append(&mut effects);
                    (t, out)
                }
                Key::Char('n') => {
                    let (t, mut effects) = self.clear_approval();
                    let mut out = vec![Effect::Agent(AgentCommand::Approve(id, Decision::Deny))];
                    out.append(&mut effects);
                    (t, out)
                }
                Key::Char('a') => {
                    let (t, mut effects) = self.clear_approval();
                    let mut out = vec![Effect::Agent(AgentCommand::Approve(
                        id,
                        Decision::ApproveAlways,
                    ))];
                    out.append(&mut effects);
                    (t, out)
                }
                Key::Escape => {
                    let (t, mut effects) = self.clear_approval();
                    let mut out = vec![Effect::Agent(AgentCommand::Cancel)];
                    out.append(&mut effects);
                    (t, out)
                }
                // Every other key is swallowed.
                _ => (self, vec![]),
            };
        }

        // Slash Command overlay (ADR-0032/0033): a leading `/` opens the popup
        // whatever the Agent is doing (Idle or Running) - a slash draft is NEVER
        // a prompt or Steering. The draft parses into `(name, rest)`; the popup
        // is in one of two sub-states, keyed by whether the command committed:
        //
        //   * COMMAND MENU (`rest = None`, or `name` is not a known
        //     selector-opening command): Phase 3's palette. Arrows move the
        //     highlight, Enter/space commits, Escape closes; editing keys fall
        //     through so typing filters the menu.
        //   * SELECTOR (`rest = Some` and `name` is a known `opens_selector`
        //     command): the committed command's own value list. Arrows move
        //     within the `rest`-filtered rows, Enter chooses, Escape closes;
        //     editing keys fall through so `rest` keeps filtering.
        //
        // This sits before the submit/steer/nav/edit arms precisely so `/`
        // intercepts Enter and the arrows.
        if slash::is_slash(&self.input_value) {
            let draft = slash::parse(&self.input_value);
            let in_selector = draft.rest.is_some()
                && slash::lookup(&draft.name).is_some_and(|c| c.opens_selector);

            if in_selector {
                // -- SELECTOR sub-state (`/model qw`) --
                let rest = draft.rest.clone().unwrap_or_default();
                match key {
                    Key::ArrowUp | Key::WheelUp | Key::ArrowDown | Key::WheelDown => {
                        if let Some(CommandSelector {
                            status: SelectorStatus::Ready(sel),
                            ..
                        }) = self.command_selector.as_mut()
                        {
                            sel.handle_nav(key, &rest);
                        }
                        return (self, vec![]);
                    }
                    Key::Enter => {
                        // Only a Ready overlay with a highlighted row resolves;
                        // Loading/Failed swallow Enter (no fetch to pick from).
                        let chosen = match self.command_selector.as_mut() {
                            Some(CommandSelector {
                                command,
                                status: SelectorStatus::Ready(sel),
                            }) => sel.handle_nav(Key::Enter, &rest).and_then(
                                |outcome| match outcome {
                                    SelectorOutcome::Select(value) => {
                                        Some(Effect::SelectorChosen {
                                            command: command.clone(),
                                            value,
                                        })
                                    }
                                    SelectorOutcome::Cancel => None,
                                },
                            ),
                            _ => None,
                        };
                        match chosen {
                            Some(effect) => {
                                self.close_selector();
                                return (self, vec![effect]);
                            }
                            None => return (self, vec![]),
                        }
                    }
                    Key::Escape => {
                        // Close the overlay and empty the Composer (no Turn to
                        // cancel - the overlay is a Composer state).
                        self.close_selector();
                        return (self, vec![]);
                    }
                    // Editing keys (chars, Backspace, newline, cursor moves)
                    // fall through so `rest` keeps filtering the rows. Note:
                    // backspacing away the space (rest → None) drops us back to
                    // the COMMAND MENU next fold, and `sync_selector` there
                    // closes this overlay so a re-activation re-fetches.
                    _ => {}
                }
            } else {
                // -- COMMAND MENU sub-state (`/mod`) --
                // A menu keystroke means we are not in a selector sub-state; drop
                // any overlay left over from backspacing out of one so the next
                // commit is a fresh activation (re-emits Effect::Command).
                self.command_selector = None;
                let rows = slash::rows(&draft.name);
                self.slash_cursor = self.slash_cursor.min(rows.len().saturating_sub(1));
                match key {
                    Key::ArrowUp | Key::WheelUp => {
                        self.slash_cursor = self.slash_cursor.saturating_sub(1);
                        return (self, vec![]);
                    }
                    Key::ArrowDown | Key::WheelDown => {
                        if self.slash_cursor + 1 < rows.len() {
                            self.slash_cursor += 1;
                        }
                        return (self, vec![]);
                    }
                    // Commit the highlighted command. An empty filtered menu means
                    // the typed token matches no command: surface an
                    // unknown-command info line, start no Turn, and clear the draft.
                    Key::Enter => {
                        let row = rows.get(self.slash_cursor).cloned();
                        return self.commit_command(row.as_ref());
                    }
                    // Typing a space after a command token also commits it (the
                    // palette convention): the space is the menu→command boundary,
                    // so it commits the highlighted row rather than editing the
                    // draft. Only when a row is highlighted - a bare/space on an
                    // empty menu falls through as a normal edit.
                    Key::Char(' ') if rows.get(self.slash_cursor).is_some() => {
                        let row = rows.get(self.slash_cursor).cloned();
                        return self.commit_command(row.as_ref());
                    }
                    // Escape closes the menu by clearing the draft - the same
                    // "back to an empty Composer" the running-Turn Escape does NOT
                    // do (that Cancels), but here there is no Turn to cancel: the
                    // menu is a Composer state, so leaving it empties the Composer.
                    Key::Escape => {
                        self.clear_draft();
                        return (self, vec![]);
                    }
                    // Every other key (chars, Backspace, newline, cursor moves)
                    // falls through to the Composer editing below, so typing
                    // filters the menu live.
                    _ => {}
                }
            }
        }

        match key {
            // Trailing-backslash continuation: Enter on a draft whose LAST
            // char is a literal `\` replaces that backslash with a hard
            // newline (cursor to the end) instead of submitting - the
            // fallback for terminals whose Alt-Enter never reaches us. Checked
            // before the submit/steer arms so it applies in both states.
            Key::Enter if self.input_value.ends_with('\\') => {
                self.input_value.pop();
                self.input_value.push('\n');
                self.input_cursor = self.input_value.chars().count();
                (self, vec![])
            }

            // Enter submits when idle, steers when running - the composer never
            // locks.
            Key::Enter if self.status == Status::Running => match self.input_value.trim() {
                "" => (self, vec![]),
                text => {
                    let text = text.to_string();
                    (self, vec![Effect::Agent(AgentCommand::Steer(text))])
                }
            },
            Key::Enter => match self.input_value.trim() {
                "" => (self, vec![]),
                prompt => {
                    let prompt = prompt.to_string();
                    (self, vec![Effect::Agent(AgentCommand::Submit(prompt))])
                }
            },

            Key::Escape if self.status == Status::Running => {
                (self, vec![Effect::Agent(AgentCommand::Cancel)])
            }

            // Both scroll in every non-modal state; the wheel steps by lines,
            // the page keys by whole pages.
            Key::PageUp => (self, vec![Effect::ScrollUp(ScrollStep::Page)]),
            Key::PageDown => (self, vec![Effect::ScrollDown(ScrollStep::Page)]),
            Key::WheelUp => (self, vec![Effect::ScrollUp(ScrollStep::Line)]),
            Key::WheelDown => (self, vec![Effect::ScrollDown(ScrollStep::Line)]),

            // Edge-triggered history: Up on the FIRST hard line of the draft
            // recalls history (the pre-multi-line behavior, draft stashing
            // included); anywhere else it moves the cursor up one line, the
            // column clamped to that line's length. Down mirrors from the
            // LAST line. No goal-column memory - a simple clamp, on purpose.
            Key::ArrowUp => {
                let (line, col) = draft::line_col(&self.input_value, self.input_cursor);
                if line == 0 {
                    if let Some(text) = self.history.up(&self.input_value) {
                        self.recall(text);
                    }
                    (self, vec![])
                } else {
                    let clamped = col.min(draft::line_lengths(&self.input_value)[line - 1]);
                    self.input_cursor = draft::cursor_at(&self.input_value, line - 1, clamped);
                    (self, vec![])
                }
            }
            Key::ArrowDown => {
                let (line, col) = draft::line_col(&self.input_value, self.input_cursor);
                let last = draft::line_lengths(&self.input_value).len() - 1;
                if line >= last {
                    if let Some(text) = self.history.down() {
                        self.recall(text);
                    }
                    (self, vec![])
                } else {
                    let clamped = col.min(draft::line_lengths(&self.input_value)[line + 1]);
                    self.input_cursor = draft::cursor_at(&self.input_value, line + 1, clamped);
                    (self, vec![])
                }
            }

            // -- Composer editing (cursor is a char index; see the fn doc) --
            Key::Char(c) => {
                self.input_value = insert_char(&self.input_value, self.input_cursor, c);
                self.input_cursor += 1;
                (self, vec![])
            }
            Key::InsertNewline => {
                self.input_value = insert_char(&self.input_value, self.input_cursor, '\n');
                self.input_cursor += 1;
                (self, vec![])
            }
            Key::Backspace => {
                if self.input_cursor > 0 {
                    self.input_cursor -= 1;
                    self.input_value = remove_char(&self.input_value, self.input_cursor);
                }
                (self, vec![])
            }
            Key::Left => {
                self.input_cursor = self.input_cursor.saturating_sub(1);
                (self, vec![])
            }
            Key::Right => {
                let len = self.input_value.chars().count();
                self.input_cursor = (self.input_cursor + 1).min(len);
                (self, vec![])
            }
            Key::Home => {
                let (line, _) = draft::line_col(&self.input_value, self.input_cursor);
                self.input_cursor = draft::cursor_at(&self.input_value, line, 0);
                (self, vec![])
            }
            Key::End => {
                let (line, _) = draft::line_col(&self.input_value, self.input_cursor);
                let len = draft::line_lengths(&self.input_value)[line];
                self.input_cursor = draft::cursor_at(&self.input_value, line, len);
                (self, vec![])
            }

            // Ctrl-T: flip the Thinking expansion; a pure display toggle, no
            // effects. The status bar's thinking segment renders this flag,
            // so the flip is visible even with no Thinking items on screen.
            Key::ToggleThinking => {
                self.thinking_expanded = !self.thinking_expanded;
                (self, vec![])
            }

            // Ctrl-O: flip the tool-Block expansion; a pure display toggle, no
            // effects. Mirrors Ctrl-T for the machinery plane - the status
            // bar's tools segment renders this flag, so the flip is visible
            // even with no Blocks on screen.
            Key::ToggleTools => {
                self.tools_expanded = !self.tools_expanded;
                (self, vec![])
            }

            _ => (self, vec![]),
        }
    }

    // Place a recalled (or restored) history entry into the composer, cursor at
    // the end - the landing spot the Up/Down arms share.
    fn recall(&mut self, text: String) {
        self.input_cursor = text.chars().count();
        self.input_value = text;
    }

    /// Mirrors the composer's value and cursor (from the input's change event).
    pub fn input_changed(mut self, value: impl Into<String>, cursor: usize) -> Self {
        self.input_value = value.into();
        self.input_cursor = cursor;
        self
    }

    /// Records how the `Submit` effect went: `Ok` appends the user line and
    /// clears the composer; `Err(Busy)` means the submit raced a starting Turn
    /// - retry as Steering.
    pub fn submitted(
        mut self,
        prompt: impl Into<String>,
        result: Result<(), Busy>,
    ) -> (Self, Vec<Effect>) {
        let prompt = prompt.into();
        match result {
            Ok(()) => {
                self.messages.push(TranscriptItem::User {
                    text: prompt.clone(),
                });
                self.record_submit(&prompt);
                self.input_value = String::new();
                self.input_cursor = 0;
                (self, vec![Effect::PinBottom, Effect::HistoryAppend(prompt)])
            }
            Err(Busy) => {
                self.status = Status::Running;
                (self, vec![Effect::Agent(AgentCommand::Steer(prompt))])
            }
        }
    }

    /// Records how the `Steer` effect went: `Ok` clears the composer (the
    /// pending line arrives via `steering_queued`); `Err(Idle)` means the Turn
    /// ended between keypress and call - retry as a submit.
    pub fn steered(
        mut self,
        text: impl Into<String>,
        result: Result<(), Idle>,
    ) -> (Self, Vec<Effect>) {
        let text = text.into();
        match result {
            Ok(()) => {
                self.input_value = String::new();
                self.input_cursor = 0;
                (self, vec![])
            }
            Err(Idle) => {
                self.status = Status::Idle;
                (self, vec![Effect::Agent(AgentCommand::Submit(text))])
            }
        }
    }

    /// Appends an info line (Resume drift notes, adapter-side news).
    pub fn info(mut self, text: impl Into<String>) -> Self {
        self.push_info(text.into());
        self
    }

    /// Resets to a truthful state after the Agent crashed and was restarted:
    /// its subscriber map and Conversation are gone, so the Transcript must not
    /// claim a Turn is still running or an Approval is still pending.
    pub fn agent_down(mut self) -> (Self, Vec<Effect>) {
        self.flush_streaming();
        self.status = Status::Idle;
        let (mut t, effects) = self.clear_approval();
        t.push_info("agent restarted; session history was reset".to_string());
        (t, effects)
    }

    /// The in-flight Thinking text, from the latest streaming snapshot.
    pub fn streaming_thinking(&self) -> String {
        self.streaming.thinking()
    }

    /// The in-flight assistant text, from the latest streaming snapshot.
    pub fn streaming_text(&self) -> String {
        self.streaming.text()
    }

    /// The open Slash Command MENU (ADR-0032) for rendering, or `None` when the
    /// draft is not a slash draft OR the popup is in the selector sub-state
    /// (`rest = Some` on a known selector-opening command - read that through
    /// [`Transcript::slash_view`]). Exposed like [`Transcript::pending_approval`]
    /// so the view reads it and draws the inline popup: `rows` are the commands
    /// matching the typed token, `highlight` the (clamped) highlighted index.
    ///
    /// Kept beside the unified [`Transcript::slash_view`] because Phase 3's tests
    /// and callers read the menu directly; `slash_view` is the one query the
    /// adapter matches to draw either sub-state.
    pub fn slash_menu(&self) -> Option<SlashMenu> {
        match self.slash_view() {
            Some(SlashView::Menu { rows, highlight }) => Some(SlashMenu { rows, highlight }),
            _ => None,
        }
    }

    /// The current inline-popup view (ADR-0032/0033), or `None` when the draft is
    /// not a slash draft. One query the adapter matches once: `Menu` while the
    /// command token is being typed (`rest = None`, or the token is not a known
    /// selector-opening command), `Selector` once such a command committed
    /// (`rest = Some`). The Selector's `rows`/`highlight` are the overlay rows
    /// filtered by the draft `rest` (the filter is the draft's, not the
    /// Selector's - consistent with the menu), so they reflect live typing.
    pub fn slash_view(&self) -> Option<SlashView> {
        if !slash::is_slash(&self.input_value) {
            return None;
        }
        let draft = slash::parse(&self.input_value);
        let in_selector =
            draft.rest.is_some() && slash::lookup(&draft.name).is_some_and(|c| c.opens_selector);
        if in_selector {
            let rest = draft.rest.unwrap_or_default();
            let (command, status, rows, highlight) = match &self.command_selector {
                Some(cs) => {
                    let (rows, highlight) = match &cs.status {
                        SelectorStatus::Ready(sel) => (
                            sel.filtered(&rest).into_iter().cloned().collect(),
                            sel.cursor.min(sel.filtered(&rest).len().saturating_sub(1)),
                        ),
                        _ => (Vec::new(), 0),
                    };
                    (cs.command.clone(), cs.status.clone(), rows, highlight)
                }
                // No overlay yet (a fresh `/model ` before the next fold
                // activates it): show a Loading placeholder for the command.
                None => (draft.name.clone(), SelectorStatus::Loading, Vec::new(), 0),
            };
            Some(SlashView::Selector {
                command,
                status,
                rows,
                highlight,
            })
        } else {
            let rows = slash::rows(&draft.name);
            let highlight = self.slash_cursor.min(rows.len().saturating_sub(1));
            Some(SlashView::Menu { rows, highlight })
        }
    }

    // ---- Internals ---------------------------------------------------------

    fn push_info(&mut self, text: String) {
        self.messages.push(TranscriptItem::Info { text });
    }

    // Empties the Composer, resets the Slash Command highlight, and closes any
    // command-selector overlay - the landing spot after a committed/closed slash
    // draft.
    fn clear_draft(&mut self) {
        self.input_value = String::new();
        self.input_cursor = 0;
        self.slash_cursor = 0;
        self.command_selector = None;
    }

    // Closes the selector overlay AND clears the draft (they open together, they
    // close together). The named alias reads as intent at the call sites where a
    // selection/Escape resolves the sub-state.
    fn close_selector(&mut self) {
        self.clear_draft();
    }

    // Commits the highlighted command row from the COMMAND MENU (ADR-0032/0033).
    // `row` is `None` when the filtered menu is empty (unknown command). A
    // selector-opening command switches the popup to its selector sub-state:
    // the draft is normalized to `"/<name> "`, a `Loading` overlay is set, and
    // `Effect::Command` is emitted ONCE (the overlay's presence guards against
    // re-emitting on later keystrokes - the menu block only runs when there is
    // no overlay). A fire-and-run command keeps Phase 3 behavior: emit
    // `Effect::Command` and clear the draft.
    fn commit_command(mut self, row: Option<&SelectorRow>) -> (Self, Vec<Effect>) {
        let row = match row {
            Some(row) => row.clone(),
            None => {
                let filter = slash::parse(&self.input_value).name;
                self.push_info(format!("unknown command: /{filter}"));
                self.clear_draft();
                return (self, vec![]);
            }
        };
        let opens_selector = slash::lookup(&row.value).is_some_and(|c| c.opens_selector);
        if opens_selector {
            // Enter the selector sub-state: normalize to `/<name> ` so `rest`
            // becomes `Some("")`, set the Loading overlay, and fetch once.
            self.input_value = format!("/{} ", row.value);
            self.input_cursor = self.input_value.chars().count();
            self.slash_cursor = 0;
            self.command_selector = Some(CommandSelector {
                command: row.value.clone(),
                status: SelectorStatus::Loading,
            });
            (self, vec![Effect::Command { name: row.value }])
        } else {
            self.clear_draft();
            (self, vec![Effect::Command { name: row.value }])
        }
    }

    // Presentment (CONTEXT.md): the configured Plugins may replace the default
    // summary item. Crashed plugins fall back to the item from before they ran
    // and leave an info line.
    fn present(
        &mut self,
        item: TranscriptItem,
        artifacts: &std::collections::HashMap<String, serde_json::Value>,
    ) {
        let (item, failures) = plugins::present(&self.plugins, item, artifacts);
        self.messages.push(item);
        for failure in failures {
            let line = plugin_failure_line(&failure.plugin, failure.stage, &failure.message);
            self.push_info(line);
        }
    }

    // Materialize a live snapshot (cancel/crash mid-stream); the seam in
    // `ui::streaming` takes both Thinking and text from it, Thinking first.
    fn flush_streaming(&mut self) {
        for item in self.streaming.flush() {
            self.messages.push(item);
        }
    }

    fn note_stop_reason(&mut self, stop_reason: StopReason) {
        match stop_reason {
            StopReason::EndTurn | StopReason::ToolUse => {}
            other => self.push_info(format!("turn stopped: :{other}")),
        }
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

    fn close_abnormally(mut self, info: String) -> (Self, Vec<Effect>) {
        self.flush_streaming();
        self.status = Status::Idle;
        let (mut t, effects) = self.clear_approval();
        t.push_info(info);
        (t, effects)
    }

    // -- Prompt history --

    /// Records a successfully submitted prompt into the in-memory history ring
    /// (dedup + cap live in [`crate::ui::history`]).
    pub fn record_submit(&mut self, prompt: &str) {
        self.history.record(prompt);
    }
}

/// The submit raced a starting Turn (baud's `{:error, :busy}`). Marker so
/// [`Transcript::submitted`]'s signature reads like baud's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy;

/// The Turn ended between keypress and steer (baud's `{:error, :idle}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Idle;

// ---------------------------------------------------------------------------
// Free functions (pure helpers).
// ---------------------------------------------------------------------------

fn pending_steering_line(text: &str) -> String {
    format!("steering (queued): {text}")
}

fn plugin_failure_line(plugin: &str, stage: Stage, message: &str) -> String {
    format!("plugin {plugin} failed in {}: {message}", stage.as_str())
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

// -- Event summarising --

// One terse recede line for an Eviction wave (CONTEXT.md: Eviction, Dead Mass):
// the Dead Mass share plus ONLY the nonzero counts, by kind, so a wave that
// reclaimed one kind reads cleanly (`context wave · 12% dead mass · 3 results`)
// and a mixed wave stays single-line. Dead Mass is the AT-WAVE (pre-reclaim)
// fraction from [`WaveStats::dead_mass`] - correct for a historical line -
// rounded through the shared [`dead_mass_pct`] rule so this line and the status
// bar can never disagree. Kept quiet - this is machinery, and Info is already
// DarkGray italic.
fn eviction_wave_line(stats: &WaveStats) -> String {
    let counts = [
        (stats.results_elided, "results"),
        (stats.cmd_superseded, "cmd superseded"),
        (stats.read_superseded, "read superseded"),
        (stats.edits_husked, "husked"),
        (stats.anchors_elided, "anchors"),
    ];
    let parts: Vec<String> = counts
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, label)| format!("{n} {label}"))
        .collect();
    let pct = dead_mass_pct(stats.dead_mass);
    if parts.is_empty() {
        format!("context wave · {pct}% dead mass")
    } else {
        format!("context wave · {pct}% dead mass · {}", parts.join(", "))
    }
}

// The single salient input arg for a merged one-liner, picked by tool: the
// `path` for read/edit/write, the `command` for run_command, the `pattern`/
// `query` for grep/search; otherwise the first value in alphabetical key order.
// `None` when the input carries no object values OR the picked value formats
// empty - the ONE emptiness rule, sourced here (so the caller falls back to the
// full `key=value` summary and never treats an empty arg as present). Truncated
// like [`format_value`] so a long path/command cannot blow out the line.
fn key_arg(name: &str, input: &serde_json::Value) -> Option<String> {
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
    let value = salient
        .iter()
        .find_map(|key| obj.get(*key))
        .or_else(|| {
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
fn summarize_input(input: &serde_json::Value) -> String {
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
    truncate(&joined, 100)
}

// One-line summary of a Tool Result content string.
fn summarize_result(content: &str) -> String {
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    match lines.as_slice() {
        [] => "(empty)".to_string(),
        [line] => truncate(line, 100),
        [line, rest @ ..] => {
            format!("{} (+{} more lines)", truncate(line, 100), rest.len())
        }
    }
}

fn format_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => {
            let cleaned = s.replace(['\n', '\r'], "⏎");
            truncate(&cleaned, 60)
        }
        other => truncate(&inspect_value(other), 60),
    }
}

// Mirrors Elixir's `inspect/1` for the shapes a tool input carries: strings
// quoted, everything else its JSON-ish form.
fn inspect_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => format!("{s:?}"),
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

// -- Composer draft editing (char-index string surgery) --
//
// The cursor is a CHAR index (the codebase counts chars, not bytes). The
// logical line/column geometry - `line_col`, `line_lengths`, `cursor_at`,
// `byte_of` - has ONE owner in `ui::draft`, shared with the render path
// (`ui::composer`) so the cursor the user edits and the cursor the view
// paints can never drift apart. These two helpers are the edit-side string
// surgery built on that geometry: they translate the char-index cursor to a
// byte offset (via `draft::byte_of`) exactly once, at the mutation site, so
// multi-byte input never splits a char or panics.

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::ContentBlock;
    use crate::plugin::Plugin;
    use serde_json::{Value, json};
    use std::collections::HashMap;

    // --- helpers mirroring transcript_test.exs -----------------------------

    fn fresh() -> Transcript {
        Transcript::new(TranscriptOpts::default())
    }

    fn fresh_opts(opts: TranscriptOpts) -> Transcript {
        Transcript::new(opts)
    }

    // Runs events through the fold, discarding effects.
    fn fold(mut t: Transcript, events: Vec<Event>) -> Transcript {
        for event in events {
            let (next, _effects) = t.apply_event(event);
            t = next;
        }
        t
    }

    fn approval_with(command: &str) -> PendingApproval {
        PendingApproval {
            approval_id: format!("ref-{command}"),
            command: command.to_string(),
        }
    }

    fn approval() -> PendingApproval {
        approval_with("mix test")
    }

    fn with_pending_approval(t: Transcript, a: &PendingApproval) -> Transcript {
        let (t, _effects) = t.apply_event(Event::ApprovalRequest {
            approval_id: a.approval_id.clone(),
            command: a.command.clone(),
        });
        t
    }

    // items/1: everything after the greeting line.
    fn items(t: &Transcript) -> Vec<TranscriptItem> {
        t.messages.iter().skip(1).cloned().collect()
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
    fn tool_call(id: &str, name: &str, summary: &str) -> TranscriptItem {
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

    // --- new/1 -------------------------------------------------------------

    #[test]
    fn new_opens_with_greeting_and_idle_status() {
        let t = fresh_opts(TranscriptOpts {
            context_budget: Some(32_000),
            ..Default::default()
        });
        assert_eq!(t.messages.len(), 1);
        match &t.messages[0] {
            TranscriptItem::Info { text } => {
                assert!(text.contains("suspenders ready"));
                assert!(text.contains("Ctrl-T toggles thinking"));
            }
            other => panic!("expected greeting info, got {other:?}"),
        }
        assert_eq!(t.status, Status::Idle);
        assert_eq!(t.context_budget, Some(32_000));
        assert_eq!(t.pending_approval, None);
        assert!(t.streaming_text().is_empty() && t.streaming_thinking().is_empty());
    }

    // --- streaming ---------------------------------------------------------

    #[test]
    fn turn_started_marks_running_clears_snapshot_and_pins() {
        let mut t = fresh();
        t.streaming.start();
        t.streaming.update(vec![text_block("stale")]);
        let (t, effects) = t.apply_event(Event::turn_started("r1"));
        assert_eq!(t.status, Status::Running);
        assert!(t.streaming_text().is_empty() && t.streaming_thinking().is_empty());
        assert_eq!(effects, vec![Effect::PinBottom]);
    }

    #[test]
    fn message_update_replaces_snapshot_without_touching_messages() {
        let t = fold(
            fresh(),
            vec![
                Event::turn_started("r1"),
                Event::message_start(1),
                Event::message_update(
                    crate::llm::stream::Delta::Text("Hel".into()),
                    vec![text_block("Hel")],
                ),
                Event::message_update(
                    crate::llm::stream::Delta::Text("lo".into()),
                    vec![thinking_block("hm"), text_block("Hello")],
                ),
            ],
        );
        assert_eq!(t.streaming_thinking(), "hm");
        assert_eq!(t.streaming_text(), "Hello");
        assert_eq!(items(&t), vec![]);
    }

    #[test]
    fn message_end_materializes_thinking_then_text() {
        let t = fold(
            fresh(),
            vec![
                Event::turn_started("r1"),
                Event::message_start(1),
                Event::message_update(
                    crate::llm::stream::Delta::Text("reading".into()),
                    vec![thinking_block("hmm"), text_block("reading")],
                ),
                Event::message_end(vec![text_block("reading")], StopReason::ToolUse),
                Event::tool_call("t1", "read_file", json!({"path": "lib/baud.ex"})),
            ],
        );
        assert_eq!(
            items(&t),
            vec![
                thinking("hmm"),
                assistant("reading"),
                tool_call("t1", "read_file", "lib/baud.ex"),
            ]
        );
        assert!(t.streaming_text().is_empty() && t.streaming_thinking().is_empty());
    }

    #[test]
    fn no_thinking_in_snapshot_yields_no_thinking_item() {
        let t = fold(
            fresh(),
            vec![
                Event::turn_started("r1"),
                Event::message_start(1),
                Event::message_end(vec![text_block("no thinking here")], StopReason::ToolUse),
                Event::tool_call("t1", "list_files", json!({})),
            ],
        );
        assert_eq!(
            items(&t),
            vec![assistant("no thinking here"), tool_call("t1", "list_files", "")]
        );
    }

    #[test]
    fn tool_result_appends_summary_with_error_flag() {
        let (t, effects) = fresh().apply_event(Event::tool_result(
            "t1",
            "grep",
            "a\nb\nc",
            false,
            HashMap::new(),
        ));
        assert_eq!(
            items(&t),
            vec![tool_result_item("grep", "a (+2 more lines)", false)]
        );
        assert_eq!(effects, vec![]);
    }

    // --- turn_finished -----------------------------------------------------

    #[test]
    fn turn_finished_flushes_snapshot_goes_idle_records_estimate_and_budget() {
        let t = fold(
            fresh_opts(TranscriptOpts {
                context_budget: Some(100),
                ..Default::default()
            }),
            vec![
                Event::turn_started("r1"),
                Event::message_start(1),
                Event::message_update(
                    crate::llm::stream::Delta::Text("Done.".into()),
                    vec![text_block("Done.")],
                ),
                Event::TurnFinished {
                    stop_reason: StopReason::EndTurn,
                    token_estimate: 42,
                    context_budget: 32_000,
                },
            ],
        );
        assert_eq!(t.status, Status::Idle);
        assert_eq!(t.token_estimate, Some(42));
        assert_eq!(t.context_budget, Some(32_000));
        assert_eq!(items(&t), vec![assistant("Done.")]);
    }

    // baud keeps the previous budget when the event lacks one. In the Rust
    // Event, TurnFinished always carries a budget; the Agent forwards the live
    // budget it holds. We reproduce baud's assertion by emitting the same
    // budget the Transcript was opened with (the Agent's live value).
    #[test]
    fn turn_finished_keeps_previous_budget_when_event_carries_it() {
        let t = fold(
            fresh_opts(TranscriptOpts {
                context_budget: Some(100),
                ..Default::default()
            }),
            vec![Event::TurnFinished {
                stop_reason: StopReason::EndTurn,
                token_estimate: 0,
                context_budget: 100,
            }],
        );
        assert_eq!(t.context_budget, Some(100));
    }

    #[test]
    fn normal_stop_reason_adds_no_info_abnormal_one_does() {
        let normal = fold(
            fresh(),
            vec![Event::TurnFinished {
                stop_reason: StopReason::EndTurn,
                token_estimate: 0,
                context_budget: 0,
            }],
        );
        assert_eq!(items(&normal), vec![]);

        let abnormal = fold(
            fresh(),
            vec![Event::TurnFinished {
                stop_reason: StopReason::MaxTokens,
                token_estimate: 0,
                context_budget: 0,
            }],
        );
        assert_eq!(items(&abnormal), vec![info("turn stopped: :max_tokens")]);
    }

    // --- context pressure --------------------------------------------------

    fn pressurized(estimate: u64) -> Transcript {
        fold(
            fresh_opts(TranscriptOpts {
                context_budget: Some(1200),
                eviction_slack: 0.10,
                ..Default::default()
            }),
            vec![Event::context_pressure(estimate, 1200, 200, 0.0)],
        )
    }

    #[test]
    fn pressure_updates_estimate_and_budget_live() {
        let t = pressurized(500);
        assert_eq!(t.token_estimate, Some(500));
        assert_eq!(t.context_budget, Some(1200));
    }

    #[test]
    fn pressure_ok_below_low_water() {
        assert_eq!(pressurized(0).pressure_level, PressureLevel::Ok);
        assert_eq!(pressurized(500).pressure_level, PressureLevel::Ok);
        assert_eq!(pressurized(880).pressure_level, PressureLevel::Ok);
    }

    #[test]
    fn pressure_elevated_between_low_water_and_target() {
        assert_eq!(pressurized(881).pressure_level, PressureLevel::Elevated);
        assert_eq!(pressurized(950).pressure_level, PressureLevel::Elevated);
        assert_eq!(pressurized(1000).pressure_level, PressureLevel::Elevated);
    }

    #[test]
    fn pressure_critical_above_target() {
        assert_eq!(pressurized(1001).pressure_level, PressureLevel::Critical);
        assert_eq!(pressurized(5000).pressure_level, PressureLevel::Critical);
    }

    #[test]
    fn pressure_comes_from_events_live_window_not_new_budget() {
        let t = fold(
            fresh_opts(TranscriptOpts {
                context_budget: Some(100),
                eviction_slack: 0.0,
                ..Default::default()
            }),
            vec![Event::context_pressure(1500, 2000, 200, 0.0)],
        );
        assert_eq!(t.context_budget, Some(2000));
        assert_eq!(t.pressure_level, PressureLevel::Ok);
    }

    // --- Approval lifecycle ------------------------------------------------

    #[test]
    fn approval_request_stores_pending_and_focuses_modal() {
        let a = approval_with("rm -rf ./tmp");
        let (t, effects) = fresh().apply_event(Event::ApprovalRequest {
            approval_id: a.approval_id.clone(),
            command: a.command.clone(),
        });
        assert_eq!(t.pending_approval, Some(a));
        assert_eq!(effects, vec![Effect::FocusModal]);
    }

    #[test]
    fn y_approves_clears_and_refocuses() {
        let a = approval();
        let t = with_pending_approval(fresh(), &a);
        let (t, effects) = t.handle_key(Key::Char('y'));
        assert_eq!(t.pending_approval, None);
        assert_eq!(
            effects,
            vec![
                Effect::Agent(AgentCommand::Approve(a.approval_id, Decision::Approve)),
                Effect::FocusComposer,
            ]
        );
    }

    #[test]
    fn n_denies() {
        let a = approval();
        let t = with_pending_approval(fresh(), &a);
        let (t, effects) = t.handle_key(Key::Char('n'));
        assert_eq!(t.pending_approval, None);
        assert_eq!(
            effects,
            vec![
                Effect::Agent(AgentCommand::Approve(a.approval_id, Decision::Deny)),
                Effect::FocusComposer,
            ]
        );
    }

    #[test]
    fn a_approves_always_clears_and_refocuses() {
        let a = approval();
        let t = with_pending_approval(fresh(), &a);
        let (t, effects) = t.handle_key(Key::Char('a'));
        assert_eq!(t.pending_approval, None);
        assert_eq!(
            effects,
            vec![
                Effect::Agent(AgentCommand::Approve(
                    a.approval_id,
                    Decision::ApproveAlways
                )),
                Effect::FocusComposer,
            ]
        );
    }

    #[test]
    fn escape_while_modal_open_is_cancellation_and_wins() {
        let t = with_pending_approval(fresh(), &approval());
        let (t, effects) = t.handle_key(Key::Escape);
        assert_eq!(t.pending_approval, None);
        assert_eq!(
            effects,
            vec![Effect::Agent(AgentCommand::Cancel), Effect::FocusComposer]
        );
    }

    #[test]
    fn every_other_key_swallowed_while_modal_open() {
        let a = approval();
        for key in [
            Key::Enter,
            Key::Char('x'),
            Key::PageUp,
            Key::PageDown,
            Key::Char('q'),
        ] {
            let t = with_pending_approval(fresh(), &a);
            let pending_before = t.pending_approval.clone();
            let (t, effects) = t.handle_key(key);
            assert_eq!(effects, vec![]);
            assert_eq!(t.pending_approval, pending_before);
        }
    }

    #[test]
    fn approval_auto_appends_standing_info_without_touching_modal() {
        let (t, effects) = fresh().apply_event(Event::approval_auto("mix test"));
        assert_eq!(
            t.messages.last(),
            Some(&info("auto-approved (standing): mix test"))
        );
        assert_eq!(t.pending_approval, None);
        assert_eq!(effects, vec![]);
    }

    #[test]
    fn verify_nudge_shows_info_after_materialized_assistant_text() {
        let t = fold(
            fresh(),
            vec![
                Event::message_start(1),
                Event::message_end(vec![text_block("all done")], StopReason::EndTurn),
            ],
        );
        let (t, effects) = t.apply_event(Event::voiced(
            crate::event::VoicedTag::VerifyNudge,
            "[files changed but not verified]",
        ));
        assert_eq!(
            items(&t),
            vec![
                assistant("all done"),
                info("[files changed but not verified]")
            ]
        );
        assert_eq!(effects, vec![]);
    }

    #[test]
    fn recovery_turn_shows_its_prompt_as_info_and_marks_running() {
        let prompt = crate::voice::recovery_prompt(true);
        let (t, effects) = fresh().apply_event(Event::recovery_turn(
            crate::session::RecoveryShape::Handoff,
            prompt,
        ));
        assert_eq!(items(&t), vec![info(prompt)]);
        assert_eq!(t.status, Status::Running);
        assert_eq!(effects, vec![Effect::PinBottom]);
    }

    #[test]
    fn wrap_up_warning_shows_as_info_line() {
        let warning = crate::voice::wrap_up_warning(2);
        let (t, effects) = fresh().apply_event(Event::voiced(
            crate::event::VoicedTag::WrapUpWarning,
            warning.clone(),
        ));
        assert_eq!(items(&t), vec![info(&warning)]);
        assert_eq!(effects, vec![]);
    }

    #[test]
    fn verification_pass_shows_as_info_line() {
        let prompt = crate::voice::verification_pass_prompt();
        let (t, effects) = fresh().apply_event(Event::voiced(
            crate::event::VoicedTag::VerificationPass,
            prompt,
        ));
        assert_eq!(items(&t), vec![info(prompt)]);
        assert_eq!(effects, vec![]);
    }

    #[test]
    fn approval_resolved_clears_only_matching_pending() {
        let a = approval();
        let t = with_pending_approval(fresh(), &a);

        // Stale id: nothing happens.
        let (t, effects) = t.apply_event(Event::approval_resolved("some-other-ref", true));
        assert_eq!(effects, vec![]);
        assert_eq!(t.pending_approval, Some(a.clone()));

        // Matching id: cleared, composer refocused.
        let (t, effects) = t.apply_event(Event::approval_resolved(a.approval_id.clone(), true));
        assert_eq!(t.pending_approval, None);
        assert_eq!(effects, vec![Effect::FocusComposer]);
    }

    // --- submit ------------------------------------------------------------

    #[test]
    fn enter_with_blank_composer_does_nothing() {
        let t = fresh().input_changed("   ", 3);
        let (_t, effects) = t.handle_key(Key::Enter);
        assert_eq!(effects, vec![]);
    }

    #[test]
    fn enter_with_text_asks_adapter_to_submit_trimmed_prompt() {
        let t = fresh().input_changed("  fix the bug  ", 15);
        let (_t, effects) = t.handle_key(Key::Enter);
        assert_eq!(
            effects,
            vec![Effect::Agent(AgentCommand::Submit("fix the bug".into()))]
        );
    }

    #[test]
    fn successful_submit_appends_user_clears_records_history_and_pins() {
        let t = fresh().input_changed("fix the bug", 11);
        let (t, effects) = t.submitted("fix the bug", Ok(()));
        assert_eq!(items(&t), vec![user("fix the bug")]);
        assert_eq!(t.input_value, "");
        assert_eq!(t.input_cursor, 0);
        assert_eq!(
            effects,
            vec![
                Effect::PinBottom,
                Effect::HistoryAppend("fix the bug".into())
            ]
        );
        // Recorded into the ring: Up recalls it.
        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(t.input_value, "fix the bug");
    }

    #[test]
    fn busy_submit_retries_as_steering() {
        let t = fresh().input_changed("another task", 12);
        let (t, effects) = t.submitted("another task", Err(Busy));
        assert_eq!(t.status, Status::Running);
        assert_eq!(
            effects,
            vec![Effect::Agent(AgentCommand::Steer("another task".into()))]
        );
    }

    // --- steering ----------------------------------------------------------

    #[test]
    fn enter_while_running_steers_instead_of_submitting() {
        let (t, _) = fresh().apply_event(Event::turn_started("r1"));
        let t = t.input_changed("  also check the README  ", 10);
        let (_t, effects) = t.handle_key(Key::Enter);
        assert_eq!(
            effects,
            vec![Effect::Agent(AgentCommand::Steer(
                "also check the README".into()
            ))]
        );
    }

    #[test]
    fn steering_queued_shows_pending_delivered_promotes_to_user() {
        let t = fold(fresh(), vec![Event::steering_queued("check the README")]);
        assert_eq!(items(&t), vec![info("steering (queued): check the README")]);

        let t = fold(t, vec![Event::steering_delivered("check the README")]);
        assert_eq!(items(&t), vec![user("check the README")]);
    }

    // The render cache's append-only contract: pushes leave the revision
    // alone; the ONE non-append edit (delivered steering removing its pending
    // line) bumps it. A delivery whose pending line was never queued removes
    // nothing and must not bump.
    #[test]
    fn only_a_delivered_steering_removal_bumps_the_messages_revision() {
        let t = fresh();
        assert_eq!(t.messages_revision, 0);

        // Appends (user submit, info) do not bump.
        let (t, _) = t.submitted("hello", Ok(()));
        let t = t.info("adapter news");
        assert_eq!(t.messages_revision, 0);

        // Queued (a push) does not bump; delivered (the remove) does.
        let t = fold(t, vec![Event::steering_queued("check the README")]);
        assert_eq!(t.messages_revision, 0);
        let t = fold(t, vec![Event::steering_delivered("check the README")]);
        assert_eq!(t.messages_revision, 1);

        // Delivered with no matching pending line removes nothing: no bump.
        let t = fold(t, vec![Event::steering_delivered("never queued")]);
        assert_eq!(t.messages_revision, 1);
    }

    // --- context visibility (Bundle A) -------------------------------------

    fn wave_stats() -> WaveStats {
        WaveStats {
            results_elided: 3,
            read_superseded: 1,
            edits_husked: 2,
            dead_mass: 0.12,
            ..WaveStats::default()
        }
    }

    // An Eviction wave recedes ONE Info line and, being append-only, must NOT
    // bump messages_revision (the precondition guard: the wave line keeps the
    // RenderCache incremental). It must NOT touch the status bar's `dead_mass_pct`
    // - the bar tracks the LIVE figure off ContextPressure, and this wave just
    // cleared what it found (the S1 bug: advertising the reclaimed snapshot).
    #[test]
    fn an_eviction_wave_pushes_one_info_line_without_bumping_or_setting_the_live_bar() {
        let t = fresh();
        assert_eq!(t.messages_revision, 0);
        assert_eq!(t.dead_mass_pct, None);
        let (t, effects) = t.apply_event(Event::eviction_wave(wave_stats()));
        assert_eq!(effects, vec![]);
        assert_eq!(
            items(&t),
            vec![info("context wave · 12% dead mass · 3 results, 1 read superseded, 2 husked")]
        );
        // The wave did not set the live bar figure.
        assert_eq!(t.dead_mass_pct, None);
        assert_eq!(t.messages_revision, 0);
    }

    // The status bar's Dead Mass is LIVE: ContextPressure refreshes it every
    // pass (pre-rounded through the shared rule), NOT the wave.
    #[test]
    fn context_pressure_sets_the_live_dead_mass_pct() {
        let t = fresh();
        let (t, _) = t.apply_event(Event::context_pressure(1500, 2000, 200, 0.128));
        assert_eq!(t.dead_mass_pct, Some(13));

        // A later pass with the dead mass reclaimed refreshes to the new figure.
        let (t, _) = t.apply_event(Event::context_pressure(1500, 2000, 200, 0.0));
        assert_eq!(t.dead_mass_pct, Some(0));
    }

    // S2 lock: the wave line's percent and the bar's percent are the SAME
    // function of the same fraction (both via `dead_mass_pct`), so they agree.
    #[test]
    fn the_wave_line_and_the_bar_round_the_dead_mass_the_same_way() {
        let fraction = 0.128;
        let (t, _) = fresh().apply_event(Event::context_pressure(1, 2, 0, fraction));
        let bar_pct = t.dead_mass_pct.expect("pressure set the live figure");

        let stats = WaveStats {
            dead_mass: fraction,
            ..WaveStats::default()
        };
        let line = eviction_wave_line(&stats);
        assert!(
            line.contains(&format!("{bar_pct}% dead mass")),
            "wave line {line:?} disagrees with bar percent {bar_pct}"
        );
    }

    // Compaction progress recedes one Info line, append-only (no bump).
    #[test]
    fn compaction_progress_pushes_one_info_line_without_bumping() {
        let t = fresh();
        let (t, effects) = t.apply_event(Event::compaction_progress("working"));
        assert_eq!(effects, vec![]);
        assert_eq!(items(&t), vec![info("compaction: working")]);
        assert_eq!(t.messages_revision, 0);
    }

    // The wave line names ONLY the nonzero counts, in kind order, with the Dead
    // Mass share as an integer percent - a single-kind wave reads cleanly.
    #[test]
    fn eviction_wave_line_names_only_nonzero_counts() {
        let one_kind = WaveStats {
            results_elided: 3,
            dead_mass: 0.05,
            ..WaveStats::default()
        };
        assert_eq!(
            eviction_wave_line(&one_kind),
            "context wave · 5% dead mass · 3 results"
        );

        assert_eq!(
            eviction_wave_line(&wave_stats()),
            "context wave · 12% dead mass · 3 results, 1 read superseded, 2 husked"
        );

        // A wave with no reclaimable counts still names the Dead Mass share.
        let none = WaveStats {
            dead_mass: 0.20,
            ..WaveStats::default()
        };
        assert_eq!(eviction_wave_line(&none), "context wave · 20% dead mass");
    }

    #[test]
    fn successful_steer_clears_composer() {
        let t = fresh().input_changed("check the README", 16);
        let (t, effects) = t.steered("check the README", Ok(()));
        assert_eq!(t.input_value, "");
        assert_eq!(effects, vec![]);
    }

    #[test]
    fn steer_that_lost_race_retries_as_submit() {
        let (t, _) = fresh().apply_event(Event::turn_started("r1"));
        let (t, effects) = t.steered("check the README", Err(Idle));
        assert_eq!(t.status, Status::Idle);
        assert_eq!(
            effects,
            vec![Effect::Agent(AgentCommand::Submit(
                "check the README".into()
            ))]
        );
    }

    #[test]
    fn session_log_error_becomes_info_line() {
        let t = fold(fresh(), vec![Event::session_log_error("disk full")]);
        let items = items(&t);
        assert_eq!(items.len(), 1);
        match &items[0] {
            TranscriptItem::Info { text } => assert!(text.contains("disk full")),
            other => panic!("expected info, got {other:?}"),
        }
    }

    // --- Slash Commands (ADR-0032) -----------------------------------------

    // A slash draft opens the menu; the menu is derived from the draft and the
    // registry, so this just sets the Composer to a slash draft.
    fn slashing(draft: &str) -> Transcript {
        fresh().input_changed(draft, draft.chars().count())
    }

    #[test]
    fn a_leading_slash_opens_the_menu_showing_every_command() {
        let menu = slashing("/").slash_menu().expect("menu open on '/'");
        assert_eq!(menu.rows, slash::rows(""));
        assert_eq!(menu.highlight, 0);
    }

    #[test]
    fn a_non_slash_draft_has_no_menu() {
        assert_eq!(slashing("fix the bug").slash_menu(), None);
        assert_eq!(fresh().slash_menu(), None);
    }

    #[test]
    fn typing_filters_the_menu_by_the_command_token() {
        let menu = slashing("/mod").slash_menu().expect("menu open");
        assert_eq!(menu.rows, slash::rows("mod"));
        assert_eq!(menu.rows.len(), 1);

        // A token that matches nothing leaves an empty (but open) menu.
        let empty = slashing("/zzz").slash_menu().expect("menu still open");
        assert!(empty.rows.is_empty());
    }

    #[test]
    fn up_down_move_the_menu_highlight_clamped_to_the_filtered_rows() {
        // One command today, so the highlight cannot leave row 0; the arrows are
        // still swallowed (no scroll effect) and saturate.
        let (t, effects) = slashing("/").handle_key(Key::ArrowDown);
        assert_eq!(effects, vec![], "arrows drive the menu, not a scroll");
        assert_eq!(t.slash_menu().unwrap().highlight, 0);
        let (t, effects) = t.handle_key(Key::ArrowUp);
        assert_eq!(effects, vec![]);
        assert_eq!(t.slash_menu().unwrap().highlight, 0);
    }

    // `/model` opens a selector (ADR-0033), so committing it does NOT clear the
    // draft the way a fire-and-run command would (Phase 3). It normalizes the
    // draft to `"/model "`, sets a Loading overlay, and emits ONE Effect::Command
    // - the selector-activation path is exercised separately below.
    #[test]
    fn enter_commits_the_highlighted_command_and_clears_the_draft() {
        let (t, effects) = slashing("/model").handle_key(Key::Enter);
        assert_eq!(
            effects,
            vec![Effect::Command {
                name: "model".into(),
            }]
        );
        // Selector-opening: draft normalized, NOT cleared; overlay is Loading.
        assert_eq!(t.input_value, "/model ");
        assert_eq!(t.input_cursor, 7);
        assert!(matches!(
            t.slash_view(),
            Some(SlashView::Selector {
                status: SelectorStatus::Loading,
                ..
            })
        ));
        // No user line, no Turn.
        assert_eq!(items(&t), vec![]);
    }

    #[test]
    fn committing_a_partial_token_uses_the_highlighted_full_command_name() {
        // "/mod" filters to the one command; Enter commits "model", not "mod".
        let (_t, effects) = slashing("/mod").handle_key(Key::Enter);
        assert_eq!(
            effects,
            vec![Effect::Command {
                name: "model".into(),
            }]
        );
    }

    #[test]
    fn enter_on_an_unknown_command_yields_an_info_line_and_no_turn() {
        let (t, effects) = slashing("/nope").handle_key(Key::Enter);
        assert_eq!(effects, vec![], "no Turn, no command effect");
        assert_eq!(items(&t), vec![info("unknown command: /nope")]);
        assert_eq!(t.input_value, "", "draft cleared");
        assert_eq!(t.slash_menu(), None);
    }

    #[test]
    fn escape_closes_the_menu_by_clearing_the_draft() {
        let (t, effects) = slashing("/model").handle_key(Key::Escape);
        assert_eq!(effects, vec![]);
        assert_eq!(t.input_value, "");
        assert_eq!(t.slash_menu(), None);
    }

    #[test]
    fn typing_and_backspace_fall_through_to_the_composer_while_slashing() {
        // A char extends the draft (and refilters the menu).
        let (t, effects) = slashing("/mode").handle_key(Key::Char('l'));
        assert_eq!(effects, vec![]);
        assert_eq!(t.input_value, "/model");
        assert_eq!(t.slash_menu().unwrap().rows, slash::rows("model"));

        // Backspace erases back toward the slash; the menu stays open.
        let (t, effects) = t.handle_key(Key::Backspace);
        assert_eq!(effects, vec![]);
        assert_eq!(t.input_value, "/mode");
        assert!(t.slash_menu().is_some());

        // Backspacing away the slash closes the menu; the remaining text is a
        // normal draft again.
        let t = slashing("/").input_changed("/", 1);
        let (t, _) = t.handle_key(Key::Backspace);
        assert_eq!(t.input_value, "");
        assert_eq!(t.slash_menu(), None);
    }

    #[test]
    fn a_slash_draft_never_submits_or_steers_even_while_running() {
        // Idle: Enter commits a command, never a Submit.
        let (_t, effects) = slashing("/model").handle_key(Key::Enter);
        assert!(matches!(effects.as_slice(), [Effect::Command { .. }]));

        // Running: the leading `/` still opens the menu and Enter commits the
        // command - it is NOT Steering text.
        let (t, _) = fresh().apply_event(Event::turn_started("r1"));
        let t = t.input_changed("/model", 6);
        assert!(t.slash_menu().is_some(), "menu opens while running");
        let (_t, effects) = t.handle_key(Key::Enter);
        assert_eq!(
            effects,
            vec![Effect::Command {
                name: "model".into(),
            }]
        );
    }

    #[test]
    fn a_normal_draft_still_submits_when_idle_and_steers_when_running() {
        // Idle submit is unchanged.
        let (_t, effects) = fresh()
            .input_changed("do a thing", 10)
            .handle_key(Key::Enter);
        assert_eq!(
            effects,
            vec![Effect::Agent(AgentCommand::Submit("do a thing".into()))]
        );

        // Running steer is unchanged.
        let (t, _) = fresh().apply_event(Event::turn_started("r1"));
        let (_t, effects) = t.input_changed("also this", 9).handle_key(Key::Enter);
        assert_eq!(
            effects,
            vec![Effect::Agent(AgentCommand::Steer("also this".into()))]
        );
    }

    // --- Slash Command selector overlay (ADR-0033) -------------------------

    // A model row for the injected SelectorReady events (value = label).
    fn model_row(id: &str) -> SelectorRow {
        SelectorRow::new(id, id, None)
    }

    // The overlay after committing `/model` and (optionally) delivering rows.
    // The draft is left at `"/model "` (rest = Some("")), the sub-state.
    fn model_selector_ready(rows: Vec<SelectorRow>) -> Transcript {
        let (t, _) = slashing("/model").handle_key(Key::Enter);
        let (t, _) = t.apply_event(Event::selector_ready(rows));
        t
    }

    #[test]
    fn committing_a_selector_command_by_enter_loads_normalizes_and_fetches_once() {
        let (t, effects) = slashing("/model").handle_key(Key::Enter);
        // Exactly one Effect::Command (the adapter fetches).
        assert_eq!(
            effects,
            vec![Effect::Command {
                name: "model".into(),
            }]
        );
        // Draft normalized to `/model ` (rest = Some("")) - NOT cleared.
        assert_eq!(t.input_value, "/model ");
        // Overlay is Loading for `model`.
        assert!(matches!(
            t.slash_view(),
            Some(SlashView::Selector {
                status: SelectorStatus::Loading,
                ..
            })
        ));
    }

    #[test]
    fn committing_a_selector_command_by_typing_a_space_loads_and_fetches_once() {
        // Typing the space after `/model` commits it the same way Enter does.
        let (t, effects) = slashing("/model").handle_key(Key::Char(' '));
        assert_eq!(
            effects,
            vec![Effect::Command {
                name: "model".into(),
            }]
        );
        assert_eq!(t.input_value, "/model ");
        assert!(matches!(
            t.slash_view(),
            Some(SlashView::Selector {
                status: SelectorStatus::Loading,
                ..
            })
        ));
    }

    #[test]
    fn selector_ready_flips_loading_to_ready_and_the_rest_filters_the_rows() {
        let rows = vec![model_row("qwen"), model_row("llama"), model_row("gpt")];
        let t = model_selector_ready(rows);
        // Ready, all rows shown (rest is "").
        match t.slash_view() {
            Some(SlashView::Selector {
                status: SelectorStatus::Ready(_),
                rows,
                highlight,
                command,
            }) => {
                assert_eq!(command, "model");
                assert_eq!(rows.len(), 3);
                assert_eq!(highlight, 0);
            }
            other => panic!("expected Ready selector, got {other:?}"),
        }
        // Typing after the space filters via `rest` (the draft owns the filter).
        let (t, _) = t.handle_key(Key::Char('q'));
        assert_eq!(t.input_value, "/model q");
        match t.slash_view() {
            Some(SlashView::Selector { rows, .. }) => {
                assert_eq!(rows, vec![model_row("qwen")], "only 'qwen' contains 'q'");
            }
            other => panic!("expected filtered selector, got {other:?}"),
        }
    }

    #[test]
    fn up_down_move_within_the_filtered_rows_of_a_ready_overlay() {
        let rows = vec![model_row("qwen"), model_row("llama"), model_row("gpt")];
        let t = model_selector_ready(rows);
        let (t, effects) = t.handle_key(Key::ArrowDown);
        assert_eq!(effects, vec![], "arrows drive the overlay, not a scroll");
        assert_eq!(highlight_of(&t), 1);
        let (t, _) = t.handle_key(Key::ArrowDown);
        assert_eq!(highlight_of(&t), 2);
        let (t, _) = t.handle_key(Key::ArrowDown);
        assert_eq!(highlight_of(&t), 2, "saturates at the last row");
        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(highlight_of(&t), 1);
    }

    // The highlighted index of a Ready selector overlay.
    fn highlight_of(t: &Transcript) -> usize {
        match t.slash_view() {
            Some(SlashView::Selector { highlight, .. }) => highlight,
            other => panic!("expected a selector overlay, got {other:?}"),
        }
    }

    #[test]
    fn enter_on_a_ready_overlay_chooses_the_highlighted_row_and_closes() {
        let rows = vec![model_row("qwen"), model_row("llama")];
        let t = model_selector_ready(rows);
        // Move to the second row, then Enter.
        let (t, _) = t.handle_key(Key::ArrowDown);
        let (t, effects) = t.handle_key(Key::Enter);
        assert_eq!(
            effects,
            vec![Effect::SelectorChosen {
                command: "model".into(),
                value: "llama".into(),
            }]
        );
        // Overlay closed, draft cleared.
        assert_eq!(t.input_value, "");
        assert_eq!(t.slash_view(), None);
    }

    #[test]
    fn enter_selects_the_filtered_highlighted_row() {
        let rows = vec![model_row("qwen"), model_row("llama"), model_row("gpt")];
        let t = model_selector_ready(rows);
        // Filter to just "llama" via `rest`, then Enter selects it.
        let (t, _) = t.handle_key(Key::Char('l'));
        let (t, _) = t.handle_key(Key::Char('l'));
        let (_t, effects) = t.handle_key(Key::Enter);
        assert_eq!(
            effects,
            vec![Effect::SelectorChosen {
                command: "model".into(),
                value: "llama".into(),
            }]
        );
    }

    #[test]
    fn selector_failed_shows_a_failed_overlay_and_enter_does_nothing() {
        let (t, _) = slashing("/model").handle_key(Key::Enter);
        let (t, _) = t.apply_event(Event::selector_failed("no server"));
        assert!(matches!(
            t.slash_view(),
            Some(SlashView::Selector {
                status: SelectorStatus::Failed(_),
                ..
            })
        ));
        // Enter on a Failed overlay does nothing (no rows to pick).
        let (t, effects) = t.handle_key(Key::Enter);
        assert_eq!(effects, vec![]);
        assert!(matches!(
            t.slash_view(),
            Some(SlashView::Selector {
                status: SelectorStatus::Failed(_),
                ..
            })
        ));
    }

    #[test]
    fn escape_closes_the_selector_overlay_and_clears_the_draft() {
        let rows = vec![model_row("qwen")];
        let t = model_selector_ready(rows);
        let (t, effects) = t.handle_key(Key::Escape);
        assert_eq!(effects, vec![]);
        assert_eq!(t.input_value, "");
        assert_eq!(t.slash_view(), None);
    }

    #[test]
    fn backspacing_the_space_returns_to_the_menu_and_reactivation_refetches() {
        let rows = vec![model_row("qwen")];
        let t = model_selector_ready(rows);
        // Backspace removes the trailing space: `/model ` → `/model`, so rest
        // goes None and we are back in the COMMAND MENU (overlay dropped).
        let (t, effects) = t.handle_key(Key::Backspace);
        assert_eq!(effects, vec![]);
        assert_eq!(t.input_value, "/model");
        assert!(matches!(t.slash_view(), Some(SlashView::Menu { .. })));
        // Re-committing is a fresh activation: it re-emits Effect::Command.
        let (t, effects) = t.handle_key(Key::Enter);
        assert_eq!(
            effects,
            vec![Effect::Command {
                name: "model".into(),
            }]
        );
        assert!(matches!(
            t.slash_view(),
            Some(SlashView::Selector {
                status: SelectorStatus::Loading,
                ..
            })
        ));
    }

    #[test]
    fn backspacing_the_slash_exits_slash_mode_entirely() {
        let t = model_selector_ready(vec![model_row("qwen")]);
        // Drive the draft down to a lone `/`, then backspace it away.
        let t = t.input_changed("/", 1);
        let (t, _) = t.handle_key(Key::Backspace);
        assert_eq!(t.input_value, "");
        assert_eq!(t.slash_view(), None, "no longer a slash draft");
    }

    #[test]
    fn a_stale_selector_ready_after_the_overlay_closed_is_ignored() {
        // Commit, then Escape to close the overlay.
        let (t, _) = slashing("/model").handle_key(Key::Enter);
        let (t, _) = t.handle_key(Key::Escape);
        assert_eq!(t.slash_view(), None);
        // A late SelectorReady must not resurrect the popup.
        let (t, effects) = t.apply_event(Event::selector_ready(vec![model_row("qwen")]));
        assert_eq!(effects, vec![]);
        assert_eq!(t.slash_view(), None, "stale event ignored");
    }

    #[test]
    fn selector_ready_is_ignored_when_no_overlay_is_loading() {
        // No slash draft at all: the event is folded but changes nothing.
        let (t, effects) = fresh().apply_event(Event::selector_ready(vec![model_row("qwen")]));
        assert_eq!(effects, vec![]);
        assert_eq!(t.slash_view(), None);
    }

    #[test]
    fn a_second_selector_ready_does_not_overwrite_a_ready_overlay() {
        // Guard: once Ready, a duplicate delivery must not reset the cursor.
        let t = model_selector_ready(vec![model_row("qwen"), model_row("llama")]);
        let (t, _) = t.handle_key(Key::ArrowDown);
        assert_eq!(highlight_of(&t), 1);
        // A second (stale) ready arrives - the overlay is no longer Loading.
        let (t, _) = t.apply_event(Event::selector_ready(vec![model_row("gpt")]));
        match t.slash_view() {
            Some(SlashView::Selector {
                rows, highlight, ..
            }) => {
                assert_eq!(rows.len(), 2, "kept the first delivery");
                assert_eq!(highlight, 1, "cursor untouched");
            }
            other => panic!("expected Ready selector, got {other:?}"),
        }
    }

    // --- Cancellation and errors -------------------------------------------

    #[test]
    fn escape_while_running_no_modal_cancels() {
        let (t, _) = fresh().apply_event(Event::turn_started("r1"));
        let (_t, effects) = t.handle_key(Key::Escape);
        assert_eq!(effects, vec![Effect::Agent(AgentCommand::Cancel)]);
    }

    #[test]
    fn escape_while_idle_does_nothing() {
        let (_t, effects) = fresh().handle_key(Key::Escape);
        assert_eq!(effects, vec![]);
    }

    #[test]
    fn turn_cancelled_flushes_snapshot_goes_idle_notes_cancellation() {
        let t = fold(
            fresh(),
            vec![
                Event::turn_started("r1"),
                Event::message_start(1),
                Event::message_update(
                    crate::llm::stream::Delta::Thinking("half a thought".into()),
                    vec![thinking_block("half a thought")],
                ),
                Event::TurnCancelled,
            ],
        );
        assert_eq!(t.status, Status::Idle);
        assert_eq!(
            items(&t),
            vec![thinking("half a thought"), info("turn cancelled")]
        );
    }

    #[test]
    fn turn_cancelled_clears_pending_approval_and_refocuses() {
        let t = fold(fresh(), vec![Event::turn_started("r1")]);
        let t = with_pending_approval(t, &approval());
        let (t, effects) = t.apply_event(Event::TurnCancelled);
        assert_eq!(t.pending_approval, None);
        assert_eq!(effects, vec![Effect::FocusComposer]);
    }

    #[test]
    fn turn_error_notes_reason_and_goes_idle() {
        let (t, _) = fresh().apply_event(Event::turn_started("r1"));
        let (t, _) = t.apply_event(Event::TurnError {
            reason: ":boom".into(),
        });
        assert_eq!(t.status, Status::Idle);
        assert_eq!(items(&t), vec![info("turn error: :boom")]);
    }

    // --- agent_down --------------------------------------------------------

    #[test]
    fn agent_down_resets_to_truthful_idle_and_reports_restart() {
        let t = fold(
            fresh(),
            vec![
                Event::turn_started("r1"),
                Event::message_start(1),
                Event::message_update(
                    crate::llm::stream::Delta::Text("half an ans".into()),
                    vec![text_block("half an ans")],
                ),
            ],
        );
        let t = with_pending_approval(t, &approval());
        let (t, effects) = t.agent_down();
        assert_eq!(t.status, Status::Idle);
        assert_eq!(t.pending_approval, None);
        assert_eq!(
            t.messages.last(),
            Some(&info("agent restarted; session history was reset"))
        );
        assert!(t.messages.contains(&assistant("half an ans")));
        assert_eq!(effects, vec![Effect::FocusComposer]);
    }

    // --- unknown input -----------------------------------------------------

    #[test]
    fn unknown_events_and_keys_are_ignored() {
        let t = fresh();
        // Anchor is display-irrelevant: not folded into a Transcript item.
        let (t, effects) = t.apply_event(Event::anchor("anchored"));
        assert_eq!(effects, vec![]);
        assert_eq!(items(&t), vec![]);

        let (_t, effects) = t.handle_key(Key::Other);
        assert_eq!(effects, vec![]);
    }

    // --- PageUp/PageDown ---------------------------------------------------

    #[test]
    fn page_keys_map_to_page_scroll_effects() {
        let (_t, effects) = fresh().handle_key(Key::PageUp);
        assert_eq!(effects, vec![Effect::ScrollUp(ScrollStep::Page)]);
        let (_t, effects) = fresh().handle_key(Key::PageDown);
        assert_eq!(effects, vec![Effect::ScrollDown(ScrollStep::Page)]);
    }

    // --- mouse wheel (line steps, where the page keys step pages) ------------

    #[test]
    fn wheel_keys_map_to_line_scroll_effects_while_idle() {
        let (_t, effects) = fresh().handle_key(Key::WheelUp);
        assert_eq!(effects, vec![Effect::ScrollUp(ScrollStep::Line)]);
        let (_t, effects) = fresh().handle_key(Key::WheelDown);
        assert_eq!(effects, vec![Effect::ScrollDown(ScrollStep::Line)]);
    }

    #[test]
    fn wheel_keys_scroll_while_running() {
        let (t, _) = fresh().apply_event(Event::turn_started("r1"));
        let (t, effects) = t.handle_key(Key::WheelUp);
        assert_eq!(effects, vec![Effect::ScrollUp(ScrollStep::Line)]);
        let (_t, effects) = t.handle_key(Key::WheelDown);
        assert_eq!(effects, vec![Effect::ScrollDown(ScrollStep::Line)]);
    }

    #[test]
    fn wheel_keys_swallowed_while_modal_open() {
        let a = approval();
        for key in [Key::WheelUp, Key::WheelDown] {
            let t = with_pending_approval(fresh(), &a);
            let pending_before = t.pending_approval.clone();
            let (t, effects) = t.handle_key(key);
            assert_eq!(effects, vec![]);
            assert_eq!(t.pending_approval, pending_before);
        }
    }

    // --- Ctrl-T thinking toggle ----------------------------------------------

    #[test]
    fn thinking_starts_collapsed() {
        assert!(!fresh().thinking_expanded);
    }

    #[test]
    fn toggle_thinking_flips_on_and_off_with_no_effects() {
        let (t, effects) = fresh().handle_key(Key::ToggleThinking);
        assert!(t.thinking_expanded);
        assert_eq!(effects, vec![]);

        let (t, effects) = t.handle_key(Key::ToggleThinking);
        assert!(!t.thinking_expanded);
        assert_eq!(effects, vec![]);
    }

    #[test]
    fn modal_swallows_toggle_thinking() {
        let a = approval();
        let t = with_pending_approval(fresh(), &a);
        let pending_before = t.pending_approval.clone();
        let (t, effects) = t.handle_key(Key::ToggleThinking);
        assert_eq!(effects, vec![]);
        assert_eq!(t.pending_approval, pending_before);
        assert!(!t.thinking_expanded);
    }

    #[test]
    fn toggle_thinking_leaves_composer_and_history_untouched() {
        let t = fresh_opts(TranscriptOpts {
            history: vec!["a".into(), "b".into()],
            ..Default::default()
        })
        .input_changed("typing...", 9);
        let (t, _) = t.handle_key(Key::ArrowUp); // park mid-history with a draft
        let (t, effects) = t.handle_key(Key::ToggleThinking);
        assert_eq!(effects, vec![]);
        assert!(t.thinking_expanded);
        assert_eq!(t.input_value, "b");
        assert_eq!(t.input_cursor, 1);
        // The ring survived the toggle: Down still restores the stashed draft.
        let (t, _) = t.handle_key(Key::ArrowDown);
        assert_eq!(t.input_value, "typing...");
    }

    // --- Ctrl-O tools toggle -------------------------------------------------
    // The machinery-plane twin of the Ctrl-T thinking toggle: a pure display
    // flip, no effects, swallowed by an open modal.

    #[test]
    fn tools_start_collapsed() {
        assert!(!fresh().tools_expanded);
    }

    #[test]
    fn toggle_tools_flips_on_and_off_with_no_effects() {
        let (t, effects) = fresh().handle_key(Key::ToggleTools);
        assert!(t.tools_expanded);
        assert_eq!(effects, vec![]);

        let (t, effects) = t.handle_key(Key::ToggleTools);
        assert!(!t.tools_expanded);
        assert_eq!(effects, vec![]);
    }

    #[test]
    fn modal_swallows_toggle_tools() {
        let a = approval();
        let t = with_pending_approval(fresh(), &a);
        let pending_before = t.pending_approval.clone();
        let (t, effects) = t.handle_key(Key::ToggleTools);
        assert_eq!(effects, vec![]);
        assert_eq!(t.pending_approval, pending_before);
        assert!(!t.tools_expanded);
    }

    // --- presentment -------------------------------------------------------

    struct BlockPresenter;
    impl Plugin for BlockPresenter {
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
    impl Plugin for PresentCrasher {
        fn present(
            &self,
            _item: TranscriptItem,
            _artifacts: &HashMap<String, Value>,
            _opts: &Value,
        ) -> TranscriptItem {
            panic!("render boom")
        }
    }

    fn reg(name: &str, plugin: Box<dyn Plugin>) -> Registered {
        Registered::new(name, plugin, Value::Null)
    }

    fn tool_result_event(artifacts: HashMap<String, Value>) -> Event {
        Event::tool_result("t1", "edit_file", "edited x", false, artifacts)
    }

    #[test]
    fn plugin_replaces_tool_result_summary_using_artifacts() {
        let t = fresh_opts(TranscriptOpts {
            plugins: vec![reg("BlockPresenter", Box::new(BlockPresenter))],
            ..Default::default()
        });
        let mut artifacts = HashMap::new();
        artifacts.insert("diff".to_string(), json!("some_diff"));
        let t = fold(t, vec![tool_result_event(artifacts)]);
        assert_eq!(
            items(&t),
            vec![TranscriptItem::Block {
                title: "diff edit_file".into(),
                lines: vec![StyledLine::new(LineStyle::Added, "+ new line")],
            }]
        );
    }

    #[test]
    fn without_matching_artifact_default_summary_survives() {
        let t = fresh_opts(TranscriptOpts {
            plugins: vec![reg("BlockPresenter", Box::new(BlockPresenter))],
            ..Default::default()
        });
        let t = fold(t, vec![tool_result_event(HashMap::new())]);
        assert_eq!(
            items(&t),
            vec![tool_result_item("edit_file", "edited x", false)]
        );
    }

    #[test]
    fn tool_result_without_artifacts_still_folds() {
        let t = fresh_opts(TranscriptOpts {
            plugins: vec![reg("BlockPresenter", Box::new(BlockPresenter))],
            ..Default::default()
        });
        let t = fold(
            t,
            vec![Event::tool_result(
                "t2",
                "grep",
                "no matches",
                false,
                HashMap::new(),
            )],
        );
        assert_eq!(
            items(&t),
            vec![tool_result_item("grep", "no matches", false)]
        );
    }

    #[test]
    fn tool_call_items_pass_through_present() {
        let t = fresh_opts(TranscriptOpts {
            plugins: vec![reg("BlockPresenter", Box::new(BlockPresenter))],
            ..Default::default()
        });
        let t = fold(t, vec![Event::tool_call("t1", "grep", json!({}))]);
        assert_eq!(items(&t), vec![tool_call("t1", "grep", "")]);
    }

    #[test]
    fn crashing_present_falls_back_to_default_with_info_line() {
        let t = fresh_opts(TranscriptOpts {
            plugins: vec![reg("PresentCrasher", Box::new(PresentCrasher))],
            ..Default::default()
        });
        let mut artifacts = HashMap::new();
        artifacts.insert("diff".to_string(), json!("d"));
        let t = fold(t, vec![tool_result_event(artifacts)]);
        let items = items(&t);
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

    #[test]
    fn plugin_error_events_become_info_lines() {
        let t = fold(
            fresh(),
            vec![Event::plugin_error(
                "Baud.Plugins.Diff",
                Stage::PreRun,
                "boom",
            )],
        );
        let items = items(&t);
        assert_eq!(items.len(), 1);
        match &items[0] {
            TranscriptItem::Info { text } => {
                assert!(text.contains("Baud.Plugins.Diff"));
                assert!(text.contains("pre_run"));
                assert!(text.contains("boom"));
            }
            other => panic!("expected info, got {other:?}"),
        }
    }

    // --- Stage 3: key_arg summaries ----------------------------------------

    #[test]
    fn key_arg_picks_the_salient_arg_by_tool() {
        // path for the file tools, command for run_command, pattern/query for
        // grep/search.
        assert_eq!(
            key_arg("read_file", &json!({"path": "src/foo.rs", "start_line": 10})),
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

    #[test]
    fn a_live_tool_call_line_reads_name_then_key_arg_not_key_equals_value() {
        let t = fold(
            fresh(),
            vec![Event::tool_call(
                "t1",
                "read_file",
                json!({"path": "src/foo.rs"}),
            )],
        );
        assert_eq!(items(&t), vec![tool_call("t1", "read_file", "src/foo.rs")]);
    }

    // --- Stage 3: call/result pairing merge --------------------------------

    // The paired call+result collapse to ONE result item carrying the call's
    // key_arg; the redundant call line is removed and the revision bumps.
    #[test]
    fn a_result_merges_with_its_call_removing_the_call_and_bumping_revision() {
        let t = fold(
            fresh(),
            vec![Event::tool_call(
                "t1",
                "read_file",
                json!({"path": "src/foo.rs"}),
            )],
        );
        let rev_after_call = t.messages_revision;
        assert_eq!(items(&t), vec![tool_call("t1", "read_file", "src/foo.rs")]);

        let t = fold(
            t,
            vec![Event::tool_result(
                "t1",
                "read_file",
                "340 lines",
                false,
                HashMap::new(),
            )],
        );
        // Call gone, ONE merged result with the recovered key_arg.
        assert_eq!(
            items(&t),
            vec![tool_result_merged("read_file", "340 lines", false, "src/foo.rs")]
        );
        // The removal is a non-append edit: revision moved.
        assert_eq!(t.messages_revision, rev_after_call + 1);
    }

    // An in-flight call with no result yet renders alone and never bumps.
    #[test]
    fn an_in_flight_call_renders_alone_without_bumping_revision() {
        let t = fold(
            fresh(),
            vec![Event::tool_call("t1", "run_command", json!({"command": "ls"}))],
        );
        assert_eq!(items(&t), vec![tool_call("t1", "run_command", "ls")]);
        assert_eq!(t.messages_revision, 0);
    }

    // Parallel/interleaved ids pair by id, not by position: the second result
    // matches the first call.
    #[test]
    fn parallel_calls_pair_by_id_not_by_position() {
        let t = fold(
            fresh(),
            vec![
                Event::tool_call("a", "read_file", json!({"path": "a.rs"})),
                Event::tool_call("b", "read_file", json!({"path": "b.rs"})),
                // Result for the FIRST call arrives second.
                Event::tool_result("a", "read_file", "10 lines", false, HashMap::new()),
            ],
        );
        // Call `a` merged away; call `b` still pending; result carries a.rs.
        assert_eq!(
            items(&t),
            vec![
                tool_call("b", "read_file", "b.rs"),
                tool_result_merged("read_file", "10 lines", false, "a.rs"),
            ]
        );
    }

    // A result with no live call (governor-injected) removes nothing, does not
    // bump, and carries no key_arg.
    #[test]
    fn an_unpaired_result_does_not_bump_and_has_no_key_arg() {
        let t = fold(
            fresh(),
            vec![Event::tool_result(
                "orphan",
                "run_command",
                "injected",
                false,
                HashMap::new(),
            )],
        );
        assert_eq!(
            items(&t),
            vec![tool_result_item("run_command", "injected", false)]
        );
        assert_eq!(t.messages_revision, 0);
    }

    // An error result keeps is_error, still removes the call, stamps key_arg,
    // and bumps.
    #[test]
    fn an_error_result_merges_keeping_the_error_flag_and_key_arg() {
        let t = fold(
            fresh(),
            vec![
                Event::tool_call("t1", "run_command", json!({"command": "cargo test"})),
                Event::tool_result("t1", "run_command", "boom", true, HashMap::new()),
            ],
        );
        assert_eq!(
            items(&t),
            vec![tool_result_merged("run_command", "boom", true, "cargo test")]
        );
        assert_eq!(t.messages_revision, 1);
    }

    // The diff-Block redundancy case: because the paired call is removed, the
    // Diff plugin's Block (whose title summarizes the call) stands alone.
    #[test]
    fn a_diff_block_stands_alone_after_the_paired_call_is_removed() {
        let t = fresh_opts(TranscriptOpts {
            plugins: vec![reg("BlockPresenter", Box::new(BlockPresenter))],
            ..Default::default()
        });
        let t = fold(
            t,
            vec![Event::tool_call(
                "t1",
                "edit_file",
                json!({"path": "src/x.rs"}),
            )],
        );
        let mut artifacts = HashMap::new();
        artifacts.insert("diff".to_string(), json!("d"));
        let t = fold(
            t,
            vec![Event::tool_result(
                "t1",
                "edit_file",
                "edited",
                false,
                artifacts,
            )],
        );
        // Only the Block remains - the redundant call line is gone.
        assert_eq!(
            items(&t),
            vec![TranscriptItem::Block {
                title: "diff edit_file".into(),
                lines: vec![StyledLine::new(LineStyle::Added, "+ new line")],
            }]
        );
        assert_eq!(t.messages_revision, 1);
    }

    // --- Stage 3: pure-core fold accessors ---------------------------------

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

    // The single emptiness rule (Nit-1): a salient arg that formats empty yields
    // None, so the live call line falls back to the full summary rather than a
    // dangling `name  ` with a blank arg.
    #[test]
    fn key_arg_maps_an_empty_formatted_value_to_none() {
        assert_eq!(key_arg("run_command", &json!({"command": ""})), None);
        // The live call line then falls back to summarize_input, not a blank arg.
        let t = fold(
            fresh(),
            vec![Event::tool_call("t1", "run_command", json!({"command": ""}))],
        );
        assert_eq!(items(&t), vec![tool_call("t1", "run_command", "command=")]);
    }

    // --- Prompt history ----------------------------------------------------

    #[test]
    fn new_accepts_history_option_oldest_first() {
        // Seeded oldest-first and parked (idx None): the first Up recalls the
        // newest entry. Ring internals are covered in `ui::history` tests.
        let t = fresh_opts(TranscriptOpts {
            history: vec!["a".into(), "b".into(), "c".into()],
            ..Default::default()
        });
        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(t.input_value, "c");
    }

    #[test]
    fn submitted_appends_deduplicating_consecutive() {
        // The submit path records into the ring; dedup of a repeated submit is
        // observable as the ring walking a, b (not a, b, b) on Up.
        let t = fresh_opts(TranscriptOpts {
            history: vec!["a".into()],
            ..Default::default()
        });
        let (t, _) = t.submitted("b", Ok(()));
        let (t, _) = t.submitted("b", Ok(())); // consecutive repeat: deduped
        let (t, _) = t.submitted("c", Ok(()));
        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(t.input_value, "c");
        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(t.input_value, "b");
        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(t.input_value, "a");
        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(t.input_value, "a"); // oldest - no further entry
    }

    #[test]
    fn record_submit_resets_position_and_draft() {
        // Park mid-history (recall stashes the draft, so input_value is now the
        // recalled "a"), then record_submit: the ring resets, so the next Up
        // starts fresh from the just-recorded newest ("b") and re-stashes the
        // CURRENT live draft - proving the prior stash was cleared, not carried.
        let mut t = fresh_opts(TranscriptOpts {
            history: vec!["a".into()],
            ..Default::default()
        });
        let (parked, _) = t.handle_key(Key::ArrowUp);
        t = parked;
        t.record_submit("b");
        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(t.input_value, "b"); // fresh from the newest, not the old idx
        let (t, _) = t.handle_key(Key::ArrowDown);
        assert_eq!(t.input_value, "a"); // the live draft at the reset Up, freshly stashed
    }

    #[test]
    fn history_capped_at_100() {
        // Submitting a 101st prompt drops the oldest: Up recalls the newest
        // ("prompt 101"), and walking to the oldest stops at "prompt 2" -
        // "prompt 1" fell off. Cap arithmetic itself is a `ui::history` test.
        let history: Vec<String> = (1..=100).map(|n| format!("prompt {n}")).collect();
        let t = fresh_opts(TranscriptOpts {
            history,
            ..Default::default()
        });
        let (mut t, _) = t.submitted("prompt 101", Ok(()));
        let (walked, _) = t.handle_key(Key::ArrowUp);
        t = walked;
        assert_eq!(t.input_value, "prompt 101"); // newest
        for _ in 0..200 {
            let (walked, _) = t.handle_key(Key::ArrowUp);
            t = walked;
        }
        assert_eq!(t.input_value, "prompt 2"); // oldest survivor, not "prompt 1"
    }

    #[test]
    fn arrow_up_from_empty_history_does_nothing() {
        let t = fresh();
        let (_t, effects) = t.handle_key(Key::ArrowUp);
        assert_eq!(effects, vec![]);
    }

    #[test]
    fn arrow_up_moves_backward_saving_draft() {
        let t = fresh_opts(TranscriptOpts {
            history: vec!["a".into(), "b".into(), "c".into()],
            ..Default::default()
        })
        .input_changed("typing...", 9);

        let (t, effects) = t.handle_key(Key::ArrowUp);
        assert_eq!(effects, vec![]);
        assert_eq!(t.input_value, "c");

        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(t.input_value, "b");

        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(t.input_value, "a");

        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(t.input_value, "a"); // at the oldest - a no-op
    }

    #[test]
    fn arrow_down_from_nil_position_does_nothing() {
        let t = fresh_opts(TranscriptOpts {
            history: vec!["a".into(), "b".into()],
            ..Default::default()
        });
        let (_t, effects) = t.handle_key(Key::ArrowDown);
        assert_eq!(effects, vec![]);
    }

    #[test]
    fn arrow_down_moves_forward_restoring_draft_at_end() {
        let t = fresh_opts(TranscriptOpts {
            history: vec!["a".into(), "b".into(), "c".into()],
            ..Default::default()
        });

        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(t.input_value, "c");

        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(t.input_value, "b");

        let (t, effects) = t.handle_key(Key::ArrowDown);
        assert_eq!(effects, vec![]);
        assert_eq!(t.input_value, "c");

        let (t, _) = t.handle_key(Key::ArrowDown);
        assert_eq!(t.input_value, ""); // past the newest: the empty draft returns
    }

    #[test]
    fn arrow_down_from_oldest_restores_draft_off_the_end() {
        let t = fresh_opts(TranscriptOpts {
            history: vec!["a".into(), "b".into()],
            ..Default::default()
        })
        .input_changed("my draft", 8);

        let (t, _) = t.handle_key(Key::ArrowUp);
        let (t, _) = t.handle_key(Key::ArrowUp);
        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(t.input_value, "a");

        let (t, _) = t.handle_key(Key::ArrowDown);
        assert_eq!(t.input_value, "b");

        let (t, _) = t.handle_key(Key::ArrowDown);
        assert_eq!(t.input_value, "my draft"); // stash restored off the end
    }

    // --- Composer editing ----------------------------------------------------
    //
    // All editing lives in the pure core (the adapter's `edit_composer` was
    // deleted): chars insert at the cursor, Backspace deletes before it, and
    // the arrows/Home/End move it. Migrated regressions from ui.rs keep their
    // intent: a typed char edits the Composer, Backspace edits it, and Enter
    // still reaches the submit rule below (`enter_with_text_asks_adapter_to_
    // submit_trimmed_prompt`); Ctrl-T maps to ToggleThinking in ui.rs's
    // `map_key` tests and toggles here (`toggle_thinking_flips_on_and_off`).

    // Folds keys through handle_key, discarding effects.
    fn press(mut t: Transcript, keys: Vec<Key>) -> Transcript {
        for key in keys {
            let (next, _effects) = t.handle_key(key);
            t = next;
        }
        t
    }

    #[test]
    fn a_typed_char_appends_at_the_end_of_the_draft() {
        let t = press(fresh(), vec![Key::Char('h'), Key::Char('i')]);
        assert_eq!(t.input_value, "hi");
        assert_eq!(t.input_cursor, 2);
    }

    #[test]
    fn a_typed_char_inserts_at_the_cursor_mid_draft() {
        let t = fresh().input_changed("hllo", 1);
        let t = press(t, vec![Key::Char('e')]);
        assert_eq!(t.input_value, "hello");
        assert_eq!(t.input_cursor, 2);
    }

    #[test]
    fn backspace_deletes_the_char_before_the_cursor() {
        let t = fresh().input_changed("hello", 3);
        let t = press(t, vec![Key::Backspace]);
        assert_eq!(t.input_value, "helo");
        assert_eq!(t.input_cursor, 2);
    }

    #[test]
    fn backspace_at_the_start_of_the_draft_is_a_noop() {
        let t = fresh().input_changed("hi", 0);
        let t = press(t, vec![Key::Backspace]);
        assert_eq!(t.input_value, "hi");
        assert_eq!(t.input_cursor, 0);
    }

    // The cursor is a CHAR index: multi-byte chars must neither split nor
    // panic under insert/delete around them.
    #[test]
    fn multibyte_chars_insert_and_delete_without_splitting() {
        let t = fresh().input_changed("héllo", 2);
        let t = press(t, vec![Key::Char('🎩')]);
        assert_eq!(t.input_value, "hé🎩llo");
        assert_eq!(t.input_cursor, 3);

        let t = press(t, vec![Key::Backspace, Key::Backspace]);
        assert_eq!(t.input_value, "hllo");
        assert_eq!(t.input_cursor, 1);
    }

    #[test]
    fn left_and_right_move_the_cursor_clamped_at_both_ends() {
        let t = fresh().input_changed("ab", 1);
        let t = press(t, vec![Key::Left]);
        assert_eq!(t.input_cursor, 0);
        let t = press(t, vec![Key::Left]);
        assert_eq!(t.input_cursor, 0); // clamped at the start

        let t = press(t, vec![Key::Right, Key::Right]);
        assert_eq!(t.input_cursor, 2);
        let t = press(t, vec![Key::Right]);
        assert_eq!(t.input_cursor, 2); // clamped at the end
        assert_eq!(t.input_value, "ab"); // movement never edits
    }

    #[test]
    fn home_and_end_jump_within_the_current_line_not_the_whole_draft() {
        // "ab\ncdef\ng", cursor mid second line (index 5, on 'e').
        let t = fresh().input_changed("ab\ncdef\ng", 5);
        let t = press(t, vec![Key::Home]);
        assert_eq!(t.input_cursor, 3); // start of "cdef"
        let t = press(t, vec![Key::End]);
        assert_eq!(t.input_cursor, 7); // end of "cdef", before its '\n'
    }

    #[test]
    fn home_and_end_on_a_single_line_draft_reach_both_ends() {
        let t = fresh().input_changed("hello", 3);
        let t = press(t, vec![Key::Home]);
        assert_eq!(t.input_cursor, 0);
        let t = press(t, vec![Key::End]);
        assert_eq!(t.input_cursor, 5);
    }

    // The Approval modal must keep swallowing everything except y/n/a/Escape:
    // in particular a typed char must NOT edit the Composer while it is open.
    #[test]
    fn typed_chars_do_not_edit_the_composer_while_modal_open() {
        let t = with_pending_approval(fresh(), &approval()).input_changed("draft", 5);
        let pending_before = t.pending_approval.clone();
        let t = press(
            t,
            vec![
                Key::Char('x'),
                Key::Backspace,
                Key::InsertNewline,
                Key::Left,
            ],
        );
        assert_eq!(t.input_value, "draft");
        assert_eq!(t.input_cursor, 5);
        assert_eq!(t.pending_approval, pending_before);
    }

    // --- multi-line drafts ---------------------------------------------------

    #[test]
    fn insert_newline_adds_a_hard_newline_at_the_cursor() {
        let t = fresh().input_changed("ab", 1);
        let (t, effects) = t.handle_key(Key::InsertNewline);
        assert_eq!(effects, vec![]);
        assert_eq!(t.input_value, "a\nb");
        assert_eq!(t.input_cursor, 2);
    }

    #[test]
    fn enter_on_a_trailing_backslash_continues_the_draft_instead_of_submitting() {
        let t = fresh().input_changed("first line\\", 11);
        let (t, effects) = t.handle_key(Key::Enter);
        assert_eq!(effects, vec![]);
        assert_eq!(t.input_value, "first line\n");
        assert_eq!(t.input_cursor, 11); // cursor to the end
    }

    #[test]
    fn enter_on_a_trailing_backslash_continues_while_running_too() {
        let (t, _) = fresh().apply_event(Event::turn_started("r1"));
        let t = t.input_changed("steer me\\", 9);
        let (t, effects) = t.handle_key(Key::Enter);
        assert_eq!(effects, vec![]);
        assert_eq!(t.input_value, "steer me\n");
    }

    // Only a LITERAL trailing backslash - the LAST char of the draft -
    // triggers the continuation.
    #[test]
    fn a_backslash_anywhere_else_still_submits() {
        let t = fresh().input_changed("a\\b", 3);
        let (_t, effects) = t.handle_key(Key::Enter);
        assert_eq!(
            effects,
            vec![Effect::Agent(AgentCommand::Submit("a\\b".into()))]
        );

        // Trailing whitespace after the backslash: the backslash is not the
        // last char, so Enter submits (trimmed).
        let t = fresh().input_changed("a\\ ", 3);
        let (_t, effects) = t.handle_key(Key::Enter);
        assert_eq!(
            effects,
            vec![Effect::Agent(AgentCommand::Submit("a\\".into()))]
        );
    }

    #[test]
    fn enter_submits_a_multi_line_draft_whole() {
        let t = fresh().input_changed("first\nsecond", 12);
        let (_t, effects) = t.handle_key(Key::Enter);
        assert_eq!(
            effects,
            vec![Effect::Agent(AgentCommand::Submit("first\nsecond".into()))]
        );
    }

    // --- edge-triggered history (Up/Down on a multi-line draft) --------------

    #[test]
    fn arrow_up_off_the_first_line_moves_the_cursor_not_history() {
        // Cursor on the second line: Up is cursor movement, history untouched.
        let t = fresh_opts(TranscriptOpts {
            history: vec!["old".into()],
            ..Default::default()
        })
        .input_changed("ab\ncd", 4); // on 'd' (line 1, col 1)
        let (t, effects) = t.handle_key(Key::ArrowUp);
        assert_eq!(effects, vec![]);
        assert_eq!(t.input_value, "ab\ncd"); // draft intact - no recall happened
        assert_eq!(t.input_cursor, 1); // line 0, col 1
    }

    #[test]
    fn arrow_up_on_the_first_line_of_a_multi_line_draft_recalls_history() {
        let t = fresh_opts(TranscriptOpts {
            history: vec!["old".into()],
            ..Default::default()
        })
        .input_changed("ab\ncd", 1); // line 0
        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(t.input_value, "old");
        assert_eq!(t.input_cursor, 3); // recall puts the cursor at the end
        // The multi-line draft was stashed: Down off the recalled entry's end
        // restores it.
        let (t, _) = t.handle_key(Key::ArrowDown);
        assert_eq!(t.input_value, "ab\ncd");
    }

    #[test]
    fn arrow_down_off_the_last_line_moves_the_cursor_not_history() {
        let t = fresh_opts(TranscriptOpts {
            history: vec!["old".into()],
            ..Default::default()
        })
        .input_changed("ab\ncd", 1); // line 0, col 1 - not the last line
        let (t, effects) = t.handle_key(Key::ArrowDown);
        assert_eq!(effects, vec![]);
        assert_eq!(t.input_value, "ab\ncd"); // draft intact - cursor moved, no recall
        assert_eq!(t.input_cursor, 4); // line 1, col 1
    }

    #[test]
    fn arrow_down_on_the_last_line_of_a_multi_line_draft_recalls_history() {
        // Recall history, then Down from the recalled entry's last line
        // restores the stashed draft - the pre-multi-line behavior.
        let t = fresh_opts(TranscriptOpts {
            history: vec!["old".into()],
            ..Default::default()
        })
        .input_changed("draft", 5);
        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(t.input_value, "old");
        let (t, _) = t.handle_key(Key::ArrowDown);
        assert_eq!(t.input_value, "draft");
    }

    #[test]
    fn up_and_down_clamp_the_column_to_the_target_lines_length() {
        // "long line\nab\nlonger": from the end of "longer", Up lands at the
        // end of the shorter "ab"; Up again keeps col 2 into "long line".
        let t = fresh().input_changed("long line\nab\nlonger", 19);
        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(t.input_cursor, 12); // end of "ab" (col clamped 6 → 2)
        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(t.input_cursor, 2); // "long line", col 2
        let (t, _) = t.handle_key(Key::ArrowDown);
        assert_eq!(t.input_cursor, 12); // back down: "ab" clamps col 2 → 2
    }

    #[test]
    fn submitting_appends_history_resets_position_and_emits_history_append() {
        let t = fresh_opts(TranscriptOpts {
            history: vec!["a".into()],
            ..Default::default()
        })
        .input_changed("b", 1);
        let (t, effects) = t.submitted("b", Ok(()));
        assert_eq!(
            effects,
            vec![Effect::PinBottom, Effect::HistoryAppend("b".into())]
        );
        // Recorded (Up walks a, b) and the recall position reset (first Up is
        // the newest, not wherever a prior recall parked).
        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(t.input_value, "b");
        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(t.input_value, "a");
    }
}
