//! UI Transcript — the pure functional core of the TUI (ADR-0001, The Elm
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
//!   snapshot, so the in-flight view is replaced wholesale per event — no delta
//!   accumulation. [`Event::MessageEnd`] materializes the snapshot into
//!   discrete items (Thinking first, then assistant text); a cancel/crash
//!   mid-stream materializes whatever the last snapshot held.
//! * Enter submits when idle and STEERS when running (the composer never
//!   locks). The submit/steer race at the Turn boundary is retried the other
//!   way via [`Transcript::submitted`] and [`Transcript::steered`].
//! * The Composer is edited HERE, not in the adapter: chars insert at the
//!   cursor (a char index), Alt-Enter and a trailing-backslash Enter insert
//!   hard newlines, Home/End work within the current line, and Up/Down are
//!   edge-triggered — history recall only from the draft's first/last line,
//!   cursor movement everywhere else.
//! * A pending Approval swallows every key except `y`, `n`, `a`, and `Escape`;
//!   `a` is approve-always (Standing Approval); Escape means Cancellation,
//!   which wins over the Approval.
//! * Presentment (CONTEXT.md): `ToolCall`/`ToolResult` items pass through
//!   [`crate::plugins::present`]; a crashing plugin is skipped with an info line
//!   (fail-open, ADR-0007), as is every `plugin_error` event the Turn reports.

use crate::content::ContentBlock;
use crate::conversation::compaction_target;
use crate::event::{Event, Stage};
use crate::llm::response::StopReason;
use crate::plugins::{self, Registered};

/// The greeting line a fresh Transcript opens with.
const GREETING: &str = "suspenders ready. Enter submits, Esc cancels a running turn, Ctrl-T toggles thinking, Ctrl-C quits";

/// The in-memory prompt-history ring cap.
const MAX_HISTORY: usize = 100;

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
/// * `User { text }` — `{:user, text}`.
/// * `Assistant { text }` — `{:assistant, text}`.
/// * `Thinking { text }` — `{:thinking, text}`.
/// * `ToolCall { name, summary }` — `{:tool_call, name, summary}`.
/// * `ToolResult { name, summary, is_error }` — `{:tool_result, name, summary,
///   is_error}`, the default one-line summary a plugin's `present` may replace.
/// * `Block { title, lines }` — `{:block, title, lines}`: a titled block of
///   [`StyledLine`]s, the semantic display vocabulary (ADR-0008).
/// * `Info { text }` — `{:info, text}`.
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
        name: String,
        summary: String,
    },
    ToolResult {
        name: String,
        summary: String,
        is_error: bool,
    },
    Block {
        title: String,
        lines: Vec<StyledLine>,
    },
    Info {
        text: String,
    },
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

/// A PURE key press, defined here so the core stays crossterm-free (ADR-0019):
/// the adapter (`ui.rs`) maps a crossterm `KeyEvent` to one of these. `Char`
/// carries a typed grapheme; the navigation/edit keys are named variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Enter,
    Escape,
    PageUp,
    PageDown,
    /// Mouse wheel up — scrolls by a few lines where [`Key::PageUp`] scrolls
    /// by a whole page; otherwise handled identically in every state.
    WheelUp,
    /// Mouse wheel down — scrolls by a few lines where [`Key::PageDown`]
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
/// the granularity — the adapter's `ui::viewport` knows the geometry and turns
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
    /// The latest streaming snapshot (`None` when not streaming). Stateless
    /// streaming: each `message_update` replaces this wholesale.
    pub streaming: Option<Vec<ContentBlock>>,
    pub status: Status,
    pub pending_approval: Option<PendingApproval>,
    pub token_estimate: Option<u64>,
    pub context_budget: Option<u64>,
    pub eviction_slack: f64,
    pub pressure_level: PressureLevel,
    pub input_value: String,
    pub input_cursor: usize,
    pub history: Vec<String>,
    pub history_idx: Option<usize>,
    pub history_draft: String,
    /// Whether settled [`TranscriptItem::Thinking`] items render expanded (the
    /// full text) instead of the collapsed one-line form. Toggled by
    /// [`Key::ToggleThinking`] (Ctrl-T); defaults collapsed.
    pub thinking_expanded: bool,
    /// Bumped whenever `messages` changes OTHER than by appending (today only
    /// `SteeringDelivered`, which removes its pending info line from wherever
    /// it sits). The frontend's per-item render cache extends incrementally
    /// while this holds still and rebuilds when it moves — appends are the hot
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
            streaming: None,
            status: Status::Idle,
            pending_approval: None,
            token_estimate: None,
            context_budget: opts.context_budget,
            eviction_slack: opts.eviction_slack,
            pressure_level: PressureLevel::Ok,
            input_value: String::new(),
            input_cursor: 0,
            history: opts.history,
            history_idx: None,
            history_draft: String::new(),
            thinking_expanded: false,
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
                self.streaming = None;
                (self, vec![Effect::PinBottom])
            }

            Event::MessageStart { .. } => {
                self.streaming = Some(Vec::new());
                (self, vec![])
            }

            // Stateless streaming: the snapshot replaces the in-flight view.
            Event::MessageUpdate { content, .. } => {
                self.streaming = Some(content);
                (self, vec![])
            }

            // Materialize the finished message: Thinking (from the last
            // snapshot — the final content never carries it) first, then the
            // assistant text (from the final content).
            Event::MessageEnd { content, .. } => {
                let thinking = blocks_text(
                    self.streaming.as_deref().unwrap_or(&[]),
                    BlockKind::Thinking,
                );
                let text = blocks_text(&content, BlockKind::Text);
                if !thinking.is_empty() {
                    self.messages
                        .push(TranscriptItem::Thinking { text: thinking });
                }
                if !text.is_empty() {
                    self.messages.push(TranscriptItem::Assistant { text });
                }
                self.streaming = None;
                (self, vec![])
            }

            // Live context-pressure indication: refresh the status bar's token
            // estimate and budget mid-Turn and name the semantic pressure level
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
                    self.eviction_slack,
                );
                (self, vec![])
            }

            Event::ToolCall { name, input, .. } => {
                let item = TranscriptItem::ToolCall {
                    name,
                    summary: summarize_input(&input),
                };
                self.present(item, &std::collections::HashMap::new());
                (self, vec![])
            }

            Event::ToolResult {
                name,
                content,
                is_error,
                artifacts,
                ..
            } => {
                let item = TranscriptItem::ToolResult {
                    name,
                    summary: summarize_result(&content),
                    is_error,
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

            Event::TurnCancelled => self.close_abnormally("turn cancelled".to_string()),

            Event::TurnError { reason } => self.close_abnormally(format!("turn error: {reason}")),

            // Unknown / display-irrelevant events are ignored.
            _ => (self, vec![]),
        }
    }

    // ---- User intents ------------------------------------------------------

    /// Folds one key press into the Transcript. ALL keys route through here —
    /// Composer editing included — so every rule lives in the pure core
    /// (ADR-0001); the adapter only maps crossterm events to [`Key`]s.
    ///
    /// While an Approval is pending, only `y`, `n`, `a` and `Escape` do
    /// anything; every other key is swallowed — in particular, plain chars
    /// must NOT edit the Composer while the modal is open. Escape is
    /// Cancellation, which wins over the Approval.
    ///
    /// The Composer cursor (`input_cursor`) is a CHAR index into
    /// `input_value` — the codebase counts chars, not bytes, so multi-byte
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

        match key {
            // Trailing-backslash continuation: Enter on a draft whose LAST
            // char is a literal `\` replaces that backslash with a hard
            // newline (cursor to the end) instead of submitting — the
            // fallback for terminals whose Alt-Enter never reaches us. Checked
            // before the submit/steer arms so it applies in both states.
            Key::Enter if self.input_value.ends_with('\\') => {
                self.input_value.pop();
                self.input_value.push('\n');
                self.input_cursor = self.input_value.chars().count();
                (self, vec![])
            }

            // Enter submits when idle, steers when running — the composer never
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
            // LAST line. No goal-column memory — a simple clamp, on purpose.
            Key::ArrowUp => {
                let (line, col) = line_col(&self.input_value, self.input_cursor);
                if line == 0 {
                    self.history_up()
                } else {
                    let clamped = col.min(line_lengths(&self.input_value)[line - 1]);
                    self.input_cursor = cursor_at(&self.input_value, line - 1, clamped);
                    (self, vec![])
                }
            }
            Key::ArrowDown => {
                let (line, col) = line_col(&self.input_value, self.input_cursor);
                let last = line_lengths(&self.input_value).len() - 1;
                if line >= last {
                    self.history_down()
                } else {
                    let clamped = col.min(line_lengths(&self.input_value)[line + 1]);
                    self.input_cursor = cursor_at(&self.input_value, line + 1, clamped);
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
                let (line, _) = line_col(&self.input_value, self.input_cursor);
                self.input_cursor = cursor_at(&self.input_value, line, 0);
                (self, vec![])
            }
            Key::End => {
                let (line, _) = line_col(&self.input_value, self.input_cursor);
                let len = line_lengths(&self.input_value)[line];
                self.input_cursor = cursor_at(&self.input_value, line, len);
                (self, vec![])
            }

            // Ctrl-T: flip the Thinking expansion; a pure display toggle, no
            // effects. The status bar's thinking segment renders this flag,
            // so the flip is visible even with no Thinking items on screen.
            Key::ToggleThinking => {
                self.thinking_expanded = !self.thinking_expanded;
                (self, vec![])
            }

            _ => (self, vec![]),
        }
    }

    // ArrowUp: navigate backward through prompt history (Readline-style).
    fn history_up(mut self) -> (Self, Vec<Effect>) {
        if self.history.is_empty() {
            return (self, vec![]);
        }
        match self.history_idx {
            None => {
                let idx = self.history.len() - 1;
                let text = self.history[idx].clone();
                self.history_idx = Some(idx);
                self.history_draft = std::mem::take(&mut self.input_value);
                self.input_cursor = text.chars().count();
                self.input_value = text;
                (self, vec![])
            }
            Some(0) => (self, vec![]),
            Some(idx) => {
                let new_idx = idx - 1;
                let text = self.history[new_idx].clone();
                self.history_idx = Some(new_idx);
                self.input_cursor = text.chars().count();
                self.input_value = text;
                (self, vec![])
            }
        }
    }

    // ArrowDown: navigate forward through prompt history.
    fn history_down(mut self) -> (Self, Vec<Effect>) {
        if self.history.is_empty() {
            return (self, vec![]);
        }
        let idx = match self.history_idx {
            None => return (self, vec![]),
            Some(idx) => idx,
        };
        let last_idx = self.history.len() - 1;
        if idx >= last_idx {
            let draft = std::mem::take(&mut self.history_draft);
            self.history_idx = None;
            self.input_cursor = draft.chars().count();
            self.input_value = draft;
            (self, vec![])
        } else {
            let new_idx = idx + 1;
            let text = self.history[new_idx].clone();
            self.history_idx = Some(new_idx);
            self.input_cursor = text.chars().count();
            self.input_value = text;
            (self, vec![])
        }
    }

    /// Mirrors the composer's value and cursor (from the input's change event).
    pub fn input_changed(mut self, value: impl Into<String>, cursor: usize) -> Self {
        self.input_value = value.into();
        self.input_cursor = cursor;
        self
    }

    /// Records how the `Submit` effect went: `Ok` appends the user line and
    /// clears the composer; `Err(Busy)` means the submit raced a starting Turn
    /// — retry as Steering.
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
    /// ended between keypress and call — retry as a submit.
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
        blocks_text(
            self.streaming.as_deref().unwrap_or(&[]),
            BlockKind::Thinking,
        )
    }

    /// The in-flight assistant text, from the latest streaming snapshot.
    pub fn streaming_text(&self) -> String {
        blocks_text(self.streaming.as_deref().unwrap_or(&[]), BlockKind::Text)
    }

    // ---- Internals ---------------------------------------------------------

    fn push_info(&mut self, text: String) {
        self.messages.push(TranscriptItem::Info { text });
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

    // Materialize a live snapshot (cancel/crash mid-stream); Thinking first.
    fn flush_streaming(&mut self) {
        let snapshot = match self.streaming.take() {
            None => return,
            Some(blocks) => blocks,
        };
        let thinking = blocks_text(&snapshot, BlockKind::Thinking);
        let text = blocks_text(&snapshot, BlockKind::Text);
        if !thinking.is_empty() {
            self.messages
                .push(TranscriptItem::Thinking { text: thinking });
        }
        if !text.is_empty() {
            self.messages.push(TranscriptItem::Assistant { text });
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

    /// Records a successfully submitted prompt into the in-memory history ring.
    /// Deduplicates consecutive identical entries and caps at [`MAX_HISTORY`].
    pub fn record_submit(&mut self, prompt: &str) {
        self.history_idx = None;
        self.history_draft = String::new();
        append_history(&mut self.history, prompt);
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
}

fn blocks_text(blocks: &[ContentBlock], kind: BlockKind) -> String {
    let mut out = String::new();
    for block in blocks {
        match (kind, block) {
            (BlockKind::Text, ContentBlock::Text { text }) => out.push_str(text),
            (BlockKind::Thinking, ContentBlock::Thinking { text }) => out.push_str(text),
            _ => {}
        }
    }
    out
}

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
// The Composer cursor is a CHAR index (the codebase counts chars, not bytes);
// these helpers translate it to a byte offset exactly once, at the mutation
// site, so multi-byte input never splits a char or panics. Lines are HARD
// lines (split on '\n') — width-wrapping is the view's concern
// (`ui::composer`), not the core's.

/// The byte offset of char index `cursor` (the string's length when the
/// cursor sits past the last char).
fn byte_of(value: &str, cursor: usize) -> usize {
    value
        .char_indices()
        .nth(cursor)
        .map(|(i, _)| i)
        .unwrap_or(value.len())
}

/// `value` with `c` inserted at char index `cursor`.
fn insert_char(value: &str, cursor: usize, c: char) -> String {
    let mut out = value.to_string();
    out.insert(byte_of(value, cursor), c);
    out
}

/// `value` with the char at char index `cursor` removed. `cursor` must be in
/// range (the Backspace arm guards it).
fn remove_char(value: &str, cursor: usize) -> String {
    let mut out = value.to_string();
    out.remove(byte_of(value, cursor));
    out
}

/// The `(hard line, column)` of char index `cursor` — both in chars. A cursor
/// sitting ON a '\n' counts as the end of the line before it.
fn line_col(value: &str, cursor: usize) -> (usize, usize) {
    let mut line = 0;
    let mut col = 0;
    for c in value.chars().take(cursor) {
        if c == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// Each hard line's length in chars. Never empty: an empty draft is one
/// zero-length line.
fn line_lengths(value: &str) -> Vec<usize> {
    value.split('\n').map(|l| l.chars().count()).collect()
}

/// The char index of `(line, col)` — the inverse of [`line_col`], counting
/// one char per '\n' between lines. `col` must already be clamped to the
/// line's length.
fn cursor_at(value: &str, line: usize, col: usize) -> usize {
    line_lengths(value)
        .iter()
        .take(line)
        .map(|len| len + 1)
        .sum::<usize>()
        + col
}

fn append_history(history: &mut Vec<String>, prompt: &str) {
    if history.last().map(|s| s.as_str()) == Some(prompt) {
        return;
    }
    history.push(prompt.to_string());
    if history.len() > MAX_HISTORY {
        let drop = history.len() - MAX_HISTORY;
        history.drain(0..drop);
    }
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
    fn tool_call(name: &str, summary: &str) -> TranscriptItem {
        TranscriptItem::ToolCall {
            name: name.into(),
            summary: summary.into(),
        }
    }
    fn tool_result_item(name: &str, summary: &str, is_error: bool) -> TranscriptItem {
        TranscriptItem::ToolResult {
            name: name.into(),
            summary: summary.into(),
            is_error,
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
        assert!(t.streaming.is_none());
    }

    // --- streaming ---------------------------------------------------------

    #[test]
    fn turn_started_marks_running_clears_snapshot_and_pins() {
        let mut t = fresh();
        t.streaming = Some(vec![text_block("stale")]);
        let (t, effects) = t.apply_event(Event::turn_started("r1"));
        assert_eq!(t.status, Status::Running);
        assert!(t.streaming.is_none());
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
                tool_call("read_file", "path=lib/baud.ex"),
            ]
        );
        assert!(t.streaming.is_none());
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
            vec![assistant("no thinking here"), tool_call("list_files", "")]
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
            vec![Event::context_pressure(estimate, 1200, 200)],
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
            vec![Event::context_pressure(1500, 2000, 200)],
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
        assert_eq!(t.history, vec!["fix the bug".to_string()]);
        assert_eq!(
            effects,
            vec![
                Effect::PinBottom,
                Effect::HistoryAppend("fix the bug".into())
            ]
        );
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
        // CompactionProgress is not folded into a Transcript item.
        let (t, effects) = t.apply_event(Event::compaction_progress("working"));
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
        assert_eq!(t.history, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(t.history_idx, Some(1));
        assert_eq!(t.history_draft, "typing...");
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
        assert_eq!(items(&t), vec![tool_call("grep", "")]);
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

    // --- Prompt history ----------------------------------------------------

    #[test]
    fn new_accepts_history_option_oldest_first() {
        let t = fresh_opts(TranscriptOpts {
            history: vec!["a".into(), "b".into(), "c".into()],
            ..Default::default()
        });
        assert_eq!(t.history, vec!["a", "b", "c"]);
        assert_eq!(t.history_idx, None);
        assert_eq!(t.history_draft, "");
    }

    #[test]
    fn submitted_appends_deduplicating_consecutive() {
        let t = fresh_opts(TranscriptOpts {
            history: vec!["a".into()],
            ..Default::default()
        });
        let (t, _) = t.submitted("b", Ok(()));
        assert_eq!(t.history, vec!["a", "b"]);
        let (t, _) = t.submitted("b", Ok(()));
        assert_eq!(t.history, vec!["a", "b"]);
        let (t, _) = t.submitted("c", Ok(()));
        assert_eq!(t.history, vec!["a", "b", "c"]);
    }

    #[test]
    fn record_submit_resets_position_and_draft() {
        let mut t = fresh_opts(TranscriptOpts {
            history: vec!["a".into()],
            ..Default::default()
        });
        t.history_idx = Some(0);
        t.history_draft = "draft".into();
        t.record_submit("b");
        assert_eq!(t.history_idx, None);
        assert_eq!(t.history_draft, "");
        assert_eq!(t.history, vec!["a", "b"]);
    }

    #[test]
    fn history_capped_at_100() {
        let history: Vec<String> = (1..=100).map(|n| format!("prompt {n}")).collect();
        let t = fresh_opts(TranscriptOpts {
            history,
            ..Default::default()
        });
        let (t, _) = t.submitted("prompt 101", Ok(()));
        assert_eq!(t.history.len(), 100);
        assert_eq!(t.history[0], "prompt 2");
        assert_eq!(t.history[99], "prompt 101");
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
        assert_eq!(t.history_idx, Some(2));
        assert_eq!(t.history_draft, "typing...");
        assert_eq!(t.input_value, "c");

        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(t.history_idx, Some(1));
        assert_eq!(t.input_value, "b");

        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(t.history_idx, Some(0));
        assert_eq!(t.input_value, "a");

        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(t.history_idx, Some(0));
        assert_eq!(t.input_value, "a");
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
        assert_eq!(t.history_idx, Some(2));
        assert_eq!(t.input_value, "c");

        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(t.history_idx, Some(1));
        assert_eq!(t.input_value, "b");

        let (t, effects) = t.handle_key(Key::ArrowDown);
        assert_eq!(effects, vec![]);
        assert_eq!(t.history_idx, Some(2));
        assert_eq!(t.input_value, "c");

        let (t, _) = t.handle_key(Key::ArrowDown);
        assert_eq!(t.history_idx, None);
        assert_eq!(t.input_value, "");
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
        assert_eq!(t.history_draft, "my draft");

        let (t, _) = t.handle_key(Key::ArrowDown);
        assert_eq!(t.history_idx, Some(1));
        assert_eq!(t.input_value, "b");

        let (t, _) = t.handle_key(Key::ArrowDown);
        assert_eq!(t.history_idx, None);
        assert_eq!(t.input_value, "my draft");
        assert_eq!(t.history_draft, "");
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

    // Only a LITERAL trailing backslash — the LAST char of the draft —
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
        assert_eq!(t.input_value, "ab\ncd");
        assert_eq!(t.input_cursor, 1); // line 0, col 1
        assert_eq!(t.history_idx, None);
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
        assert_eq!(t.history_idx, Some(0));
        assert_eq!(t.history_draft, "ab\ncd"); // the draft is stashed
    }

    #[test]
    fn arrow_down_off_the_last_line_moves_the_cursor_not_history() {
        let t = fresh_opts(TranscriptOpts {
            history: vec!["old".into()],
            ..Default::default()
        })
        .input_changed("ab\ncd", 1); // line 0, col 1 — not the last line
        let (t, effects) = t.handle_key(Key::ArrowDown);
        assert_eq!(effects, vec![]);
        assert_eq!(t.input_cursor, 4); // line 1, col 1
        assert_eq!(t.history_idx, None);
    }

    #[test]
    fn arrow_down_on_the_last_line_of_a_multi_line_draft_recalls_history() {
        // Recall history, then Down from the recalled entry's last line
        // restores the stashed draft — the pre-multi-line behavior.
        let t = fresh_opts(TranscriptOpts {
            history: vec!["old".into()],
            ..Default::default()
        })
        .input_changed("draft", 5);
        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(t.input_value, "old");
        let (t, _) = t.handle_key(Key::ArrowDown);
        assert_eq!(t.input_value, "draft");
        assert_eq!(t.history_idx, None);
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
        assert_eq!(t.history, vec!["a", "b"]);
        assert_eq!(t.history_idx, None);
        assert_eq!(
            effects,
            vec![Effect::PinBottom, Effect::HistoryAppend("b".into())]
        );
    }
}
