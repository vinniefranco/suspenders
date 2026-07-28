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
//!   pairing by id, Presentment on every append); this fold holds the
//!   choreography - which event means which verb - and the Voice: the
//!   greeting, stop reasons, cancellation notes, and wave lines are authored
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

use crate::conversation::{WaveStats, compaction_target, dead_mass_pct};
use crate::event::Event;
use crate::extensions::Registered;
use crate::llm::response::StopReason;
use crate::ui::composer::{Composer, EventOutcome, KeyOutcome};
use crate::ui::transcript::{Tone, Transcript, TranscriptItem};

/// The greeting line a fresh Screen opens its Transcript with.
const GREETING: &str = "suspenders ready. Enter submits, Esc cancels a running turn, Ctrl-T toggles thinking, Ctrl-C quits";

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
/// the granularity - the adapter's `ui::viewport` knows the geometry and runs
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
}

/// The pure Screen state (ADR-0034; the renamed fold root of baud's
/// `%Baud.UI.Transcript{}`).
///
/// The Transcript store's extensions are not `Clone`/`PartialEq`, so the core
/// is not `Clone`; the fold takes and returns an owned `Screen` by value,
/// mirroring the Elixir struct-threading style.
pub struct Screen {
    /// The Transcript (ADR-0034): the display-side history, the streaming
    /// snapshot, and Presentment, behind [`crate::ui::transcript`]'s store
    /// seam. Private on purpose - reads go through [`Screen::transcript`]
    /// (the render adapter's window), mutation only through the folds and the
    /// submitted/steered outcome hooks.
    transcript: Transcript,
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
    /// The Session's cumulative dollar cost from the most recent
    /// [`Event::SessionCost`] (ADR-0037: pricing rides the Catalog Model;
    /// surfacing is display-side only). Stays 0.0 on unpriced (local/custom)
    /// Models - the metered boundary emits nothing - and the status bar hides
    /// its cost segment at zero, so such a Session looks exactly as before.
    pub session_cost: f64,
    /// The Composer (ADR-0034): the draft, the overlays, and the
    /// prompt-history ring, behind [`crate::ui::composer`]'s seam. Private on
    /// purpose - reads go through [`Screen::composer`] (the render
    /// adapter's window), mutation only through the folds and the
    /// submitted/steered outcome hooks.
    composer: Composer,
    /// Whether settled Thinking items render expanded (the full text) instead
    /// of the collapsed one-line form. Toggled by [`Key::ToggleThinking`]
    /// (Ctrl-T); defaults collapsed.
    pub thinking_expanded: bool,
    /// Whether settled Block items (diffs, tool output) render expanded (the
    /// full body) instead of the collapsed one-line title. Toggled by
    /// [`Key::ToggleTools`] (Ctrl-O); defaults collapsed - the same
    /// detail-on-demand rule as `thinking_expanded`, applied to the machinery
    /// plane so a burst of tool output can't eat the window.
    pub tools_expanded: bool,
}

/// The options a fresh Screen is opened with (baud's `new/1` keyword opts).
#[derive(Default)]
pub struct ScreenOpts {
    pub context_budget: Option<u64>,
    pub eviction_slack: f64,
    pub extensions: Vec<Registered>,
    pub history: Vec<String>,
    /// Launch-time info lines the adapter authors (context-file skips today):
    /// news from before the event loop existed, recorded right after the
    /// greeting so it is visible without ever entering the Conversation.
    pub notices: Vec<String>,
}

impl Screen {
    /// A fresh Screen, opened with the greeting info line and idle status.
    pub fn new(opts: ScreenOpts) -> Self {
        // The greeting is this fold's Voice: the store opens empty and
        // records what its owner authors.
        let mut transcript = Transcript::new(opts.extensions);
        transcript.info(GREETING);
        for notice in opts.notices {
            transcript.info(notice);
        }
        Screen {
            transcript,
            status: Status::Idle,
            pending_approval: None,
            token_estimate: None,
            context_budget: opts.context_budget,
            eviction_slack: opts.eviction_slack,
            pressure_level: PressureLevel::Ok,
            dead_mass_pct: None,
            session_cost: 0.0,
            composer: Composer::new(opts.history),
            thinking_expanded: false,
            tools_expanded: false,
        }
    }

    /// A representative populated Screen for eyeballing the render (the `--demo`
    /// harness and the render tests): one user request whose run interleaves
    /// several Thinking passes, tool machinery, harness markers, and an answer
    /// with a code fence - the exact shape that exposed the fold / spine / blank
    /// -line bugs. No IO, no events; the transcript is authored directly.
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
        t.push(tool("list_files", ".", ".claude/ (+19 more lines)", false));
        t.push(tool(
            "explore",
            "What is the overall architecture and purpose of this projec…",
            "## Locations (+29 more lines)",
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
        t.push(tool("list_files", "src", "agent/ (+32 more lines)", false));
        t.push(tool("list_files", "docs", "adr/ (+2 more lines)", false));
        t.marker("» [reading file after file fills your context - dispatch explore with one focused question instead; a Scout searches and reports back]", Tone::Aid);
        t.push(thought("Let me explore more of the project structure to understand the codebase depth, test coverage, and ADRs."));
        t.push(tool(
            "list_files",
            "docs/adr",
            "0001-ratatui-for-the-tui.md (+39 more lines)",
            false,
        ));
        t.push(tool(
            "list_files",
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
        t.push(tool("run_command", "cargo build 2>&1", "√ exit 0", false));
        t.push(tool(
            "run_command",
            "cargo test --lib -- --quiet 2>&1",
            "× [command denied by user]",
            true,
        ));
        t.push(TranscriptItem::Assistant {
            text: "The project is a well-structured Rust terminal agent. Here is the tokenizer entry point:\n\n```rust\nfn tokenize(src: &str) -> Vec<Token> {\n    let mut out = Vec::new();\n    out\n}\n```\n\nOverall the codebase is clean and well-tested.".into(),
        });
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
        // overlay fed by a new event needs no new arm here.
        let event = match self.composer.apply_event(event) {
            EventOutcome::Consumed(effects) => return (self, effects),
            EventOutcome::Refused(event) => event,
        };

        // The flat family dispatch: each arm names one event family and hands
        // the whole event to that family's fold below.
        match event {
            event @ (Event::RunStarted(..)
            | Event::MessageStart { .. }
            | Event::MessageUpdate { .. }
            | Event::MessageEnd { .. }) => self.apply_streaming(event),

            event @ (Event::ContextPressure { .. }
            | Event::EvictionWave { .. }
            | Event::CompactionProgress { .. }
            | Event::SessionCost { .. }) => self.apply_pressure(event),

            event @ (Event::ToolCall { .. }
            | Event::ToolResult { .. }
            | Event::ExtensionError { .. }) => self.apply_tooling(event),

            event @ (Event::ApprovalRequest { .. }
            | Event::ApprovalResolved { .. }
            | Event::ApprovalAuto { .. }) => self.apply_approval(event),

            event @ (Event::SteeringQueued { .. } | Event::SteeringDelivered { .. }) => {
                self.apply_steering(event)
            }

            event @ (Event::SessionLogError { .. }
            | Event::VerifyNudge { .. }
            | Event::VerifyFailedNudge { .. }
            | Event::EmptyResponseNudge { .. }
            | Event::ExploreNudge { .. }
            | Event::WrapUpWarning { .. }
            | Event::VerificationPass { .. }
            | Event::FinalPass { .. }
            | Event::RecoveryRun { .. }
            | Event::Anchor { .. }
            | Event::ToolsNarrowed { .. }
            | Event::Retry { .. }) => self.apply_voice(event),

            event @ (Event::RunFinished { .. }
            | Event::RunCancelled
            | Event::RunError { .. }) => self.apply_settlement(event),

            // The selector fills are the Composer's own (ADR-0034): they are
            // consumed by `self.composer.apply_event` at the top of this fold
            // (line 304) and never reach this dispatch. Listed explicitly, with
            // no wildcard, so the match stays EXHAUSTIVE over the Event
            // vocabulary - a future variant is a compile error here, not a
            // silent fallthrough.
            Event::SelectorReady { .. } | Event::SelectorFailed { .. } => (self, vec![]),
        }
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
                (self, vec![Effect::PinBottom])
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

    // Context pressure / Eviction / Compaction (ADR-0008; CONTEXT.md: Eviction,
    // Dead Mass): the status-bar figures and the receded machinery lines.
    fn apply_pressure(mut self, event: Event) -> (Self, Vec<Effect>) {
        match event {
            // Live context-pressure indication: refresh the status bar's token
            // estimate, budget, and LIVE Dead Mass share mid-Run and name the
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

            // An Eviction wave rewrote the request copy (CONTEXT.md: Eviction,
            // Dead Mass): recede ONE terse Info line naming the wave and its
            // at-wave (pre-reclaim) snapshot. The status bar does NOT derive
            // from this - it tracks the LIVE Dead Mass off `ContextPressure`,
            // and this wave has just cleared what it found.
            Event::EvictionWave { stats } => {
                self.transcript
                    .marker(eviction_wave_line(&stats), Tone::Housekeeping);
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
    // Plugin failure report.
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

            // An Extension crashed and was skipped (fail-open, ADR-0007) - the
            // same report line the store's own Presentment failures use.
            Event::ExtensionError {
                extension,
                stage,
                message,
            } => {
                self.transcript.extension_failure(&extension, stage, &message);
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
                self.transcript
                    .info(format!("auto-approved (standing): {command}"));
                (self, vec![])
            }

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
                (self, vec![Effect::PinBottom])
            }

            Event::SteeringDelivered { text } => {
                self.transcript.steering_delivered(text);
                (self, vec![])
            }

            _ => (self, vec![]),
        }
    }

    // Voiced / operator-news lines: everything whose display is one authored
    // info line - the Session Log failure, the Nudges and Endgame riders, the
    // Recovery Run prompt, and the bounded re-draw marks.
    fn apply_voice(mut self, event: Event) -> (Self, Vec<Effect>) {
        // The rider's marker-plane tone (ADR-0040) is decided at the firing
        // site ([`VoicedTag::tone`]) and recovered here BEFORE the match moves
        // the event - the Screen carries it, it never classifies.
        let rider_tone = event.voiced_tone();
        match event {
            // The Session Log died (IO failure); the Session continues
            // unpersisted. Adapter news, not a harness marker - stays Info.
            Event::SessionLogError { message } => {
                self.transcript.info(format!(
                    "session log failed ({message}); this session will not resume"
                ));
                (self, vec![])
            }

            // A Nudge / Endgame rider entered the Conversation; it is always
            // visible, so the Transcript shows it as a toned marker. The glyph
            // rides the authored display line ([`marker_glyph`]), so the
            // adapter tints by tone alone and never sniffs the text.
            Event::VerifyNudge { text }
            | Event::VerifyFailedNudge { text }
            | Event::EmptyResponseNudge { text }
            | Event::ExploreNudge { text }
            | Event::WrapUpWarning { text }
            | Event::VerificationPass { text }
            | Event::FinalPass { text } => {
                // Every variant in this arm is a voiced rider, so
                // `voiced_tone` returned `Some` above; asserting it here runs a
                // future drift (a rider added to this arm but not to
                // `voiced_tone`) into a test failure instead of a silently
                // mistinted (Plain) marker (LOW-1).
                let tone = rider_tone.expect("a voiced rider must carry a tone");
                self.transcript.marker(marker_line(tone, &text), tone);
                (self, vec![])
            }

            // A Recovery Run opened: its Voice prompt entered the
            // Conversation, so the Transcript shows it like every Nudge - a
            // Governor aiding the model, so an Aid-toned marker.
            Event::RecoveryRun { text, .. } => {
                self.transcript
                    .marker(marker_line(Tone::Aid, &text), Tone::Aid);
                self.status = Status::Running;
                (self, vec![Effect::PinBottom])
            }

            // The anchor Governor refreshed the Plan (CONTEXT.md: Anchor): the
            // Session Log holds the full anchor block the model read; the
            // Transcript shows only a concise Aid marker, never the plan body.
            // The `⚑` glyph is intrinsic to this marker, so it is baked into
            // the fixed wording rather than derived from the tone.
            Event::Anchor { .. } => {
                self.transcript.marker("⚑ plan refreshed", Tone::Aid);
                (self, vec![])
            }

            // The Endgame narrowed the Offer (CONTEXT.md: Endgame): a Governor
            // limiting the model, so a Constrain marker. The `⊘` glyph is
            // intrinsic here; the wording names the surviving tools, or reads
            // "withdrawn" when the final Pass offers none.
            Event::ToolsNarrowed { tools } => {
                self.transcript
                    .marker(tools_narrowed_line(&tools), Tone::Constrain);
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
    /// While an Approval is pending, only `y`, `n`, `a` and `Escape` do
    /// anything; every other key is swallowed - in particular, plain chars
    /// must NOT edit the Composer while the modal is open. Escape is
    /// Cancellation, which wins over the Approval.
    pub fn handle_key(mut self, key: Key) -> (Self, Vec<Effect>) {
        // Modal-open handling swallows everything but y/n/a/Escape. A
        // swallowed key pays nothing - only a resolving key clones the id it
        // sends to the Agent.
        if let Some(pending) = &self.pending_approval {
            let command = match key {
                Key::Char('y') => {
                    AgentCommand::Approve(pending.approval_id.clone(), Decision::Approve)
                }
                Key::Char('n') => {
                    AgentCommand::Approve(pending.approval_id.clone(), Decision::Deny)
                }
                Key::Char('a') => {
                    AgentCommand::Approve(pending.approval_id.clone(), Decision::ApproveAlways)
                }
                // Escape is Cancellation, which wins over the Approval.
                Key::Escape => AgentCommand::Cancel,
                // Every other key is swallowed.
                _ => return (self, vec![]),
            };
            let (t, mut effects) = self.clear_approval();
            let mut out = vec![Effect::Agent(command)];
            out.append(&mut effects);
            return (t, out);
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

        match key {
            // Escape means Cancellation while a Run runs; the Composer has
            // already refused it (no overlay was open to close).
            Key::Escape if self.status == Status::Running => {
                (self, vec![Effect::Agent(AgentCommand::Cancel)])
            }

            // Both scroll in every non-modal state; the wheel steps by lines,
            // the page keys by whole pages. The wheel only reaches here when
            // no overlay is open - an open one consumes it as row navigation.
            Key::PageUp => (self, vec![Effect::ScrollUp(ScrollStep::Page)]),
            Key::PageDown => (self, vec![Effect::ScrollDown(ScrollStep::Page)]),
            Key::WheelUp => (self, vec![Effect::ScrollUp(ScrollStep::Line)]),
            Key::WheelDown => (self, vec![Effect::ScrollDown(ScrollStep::Line)]),

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

    /// Records how the `Submit` effect went: `Ok` appends the user line (with
    /// the PinBottom that belongs beside it) and hands the Composer its
    /// success - [`Composer::submitted_ok`] records the prompt into the ring,
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
                let mut effects = vec![Effect::PinBottom];
                effects.extend(self.composer.submitted_ok(&prompt));
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
        self.transcript.info(text);
        self
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

    // ---- Internals ---------------------------------------------------------

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
        format!("✂ context wave · {pct}% dead mass")
    } else {
        format!("✂ context wave · {pct}% dead mass · {}", parts.join(", "))
    }
}

// One Compaction-progress marker line (CONTEXT.md: Compaction). The `⟨ … ⟩`
// glyph pair marks it as a summary fold in the Housekeeping plane (ADR-0040),
// distinct from the `✂` eviction glyph; the tint comes from the tone, never
// from this text.
fn compaction_line(status: &str) -> String {
    format!("⟨ compaction: {status} → summary ⟩")
}

// One tools-narrowed marker line (CONTEXT.md: Endgame, Offer): the `⊘` glyph
// then the surviving tool names, or "withdrawn" when the final Pass offers
// none. The glyph is intrinsic to this Constrain marker, so it lives in the
// text rather than in `marker_glyph`; the tint still comes from the tone.
fn tools_narrowed_line(tools: &[String]) -> String {
    if tools.is_empty() {
        "⊘ tools withdrawn".to_string()
    } else {
        format!("⊘ tools narrowed to {}", tools.join(", "))
    }
}

// The marker-plane glyph the Screen's display Voice prefixes to a toned line
// (ADR-0040), chosen by TONE so the adapter tints by tone alone and never
// reads the text. Aid riders (Nudges, Recovery) wear `»`/`↺`-family warmth via
// `»`; the Endgame's run-closing Constrain riders wear `▪`. Housekeeping and
// Steering author their own glyphs at their own sites (`✂`/`⟨⟩`, `↳`).
fn marker_glyph(tone: Tone) -> &'static str {
    match tone {
        Tone::Aid => "»",
        Tone::Constrain => "▪",
        Tone::Housekeeping | Tone::Steering | Tone::Plain => "",
    }
}

// A toned marker's display line: the tone's glyph (when it carries one) then
// the authored text. Kept beside `marker_glyph` so the glyph choice and the
// spacing live in one place.
fn marker_line(tone: Tone, text: &str) -> String {
    let glyph = marker_glyph(tone);
    if glyph.is_empty() {
        text.to_string()
    } else {
        format!("{glyph} {text}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::ContentBlock;
    use crate::event::Stage;
    use crate::ui::transcript::{Tone, TranscriptItem};
    use std::collections::HashMap;

    // --- helpers mirroring transcript_test.exs -----------------------------

    fn fresh() -> Screen {
        Screen::new(ScreenOpts::default())
    }

    fn fresh_opts(opts: ScreenOpts) -> Screen {
        Screen::new(opts)
    }

    // Runs events through the fold, discarding effects.
    fn fold(mut t: Screen, events: Vec<Event>) -> Screen {
        for event in events {
            let (next, _effects) = t.apply_event(event);
            t = next;
        }
        t
    }

    // Folds keys through handle_key, discarding effects.
    fn press(mut t: Screen, keys: Vec<Key>) -> Screen {
        for key in keys {
            let (next, _effects) = t.handle_key(key);
            t = next;
        }
        t
    }

    // The key presses that type `text` into the Composer.
    fn typed(text: &str) -> Vec<Key> {
        text.chars().map(Key::Char).collect()
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

    fn with_pending_approval(t: Screen, a: &PendingApproval) -> Screen {
        let (t, _effects) = t.apply_event(Event::ApprovalRequest {
            approval_id: a.approval_id.clone(),
            command: a.command.clone(),
        });
        t
    }

    // items/1: everything after the greeting line.
    fn items(t: &Screen) -> Vec<TranscriptItem> {
        t.transcript().items().iter().skip(1).cloned().collect()
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

    fn text_block(text: &str) -> ContentBlock {
        ContentBlock::Text { text: text.into() }
    }
    fn thinking_block(text: &str) -> ContentBlock {
        ContentBlock::Thinking { text: text.into() }
    }

    // --- new/1 -------------------------------------------------------------

    #[test]
    fn new_opens_with_greeting_and_idle_status() {
        let t = fresh_opts(ScreenOpts {
            context_budget: Some(32_000),
            ..Default::default()
        });
        assert_eq!(t.transcript().items().len(), 1);
        match &t.transcript().items()[0] {
            TranscriptItem::Info { text } => {
                assert!(text.contains("suspenders ready"));
                assert!(text.contains("Ctrl-T toggles thinking"));
            }
            other => panic!("expected greeting info, got {other:?}"),
        }
        assert_eq!(t.status, Status::Idle);
        assert_eq!(t.context_budget, Some(32_000));
        assert_eq!(t.pending_approval, None);
        assert!(
            t.transcript().streaming_text().is_empty()
                && t.transcript().streaming_thinking().is_empty()
        );
    }

    #[test]
    fn new_records_launch_notices_after_the_greeting() {
        let t = fresh_opts(ScreenOpts {
            notices: vec![
                "context file .suspenders/SYSTEM.md exists but could not be read \
                 (permission denied); continuing without it"
                    .to_string(),
            ],
            ..Default::default()
        });
        assert_eq!(
            items(&t),
            vec![info(
                "context file .suspenders/SYSTEM.md exists but could not be read \
                 (permission denied); continuing without it"
            )]
        );
    }

    // --- has_live_stream (the render gate's one predicate) ------------------

    // The lull/tail gate: a fresh Screen streams nothing, a reasoning delta
    // trips the `streaming_thinking` operand, and an answer-text delta trips the
    // `streaming_text` operand - both `||` arms covered.
    #[test]
    fn has_live_stream_tracks_reasoning_and_answer_streams() {
        // Fresh: nothing on the wire.
        assert!(!fresh().has_live_stream(), "a fresh Screen streams nothing");

        // A reasoning delta => the thinking arm holds.
        let thinking_stream = fold(
            fresh(),
            vec![
                Event::run_started("r1"),
                Event::message_start(1),
                Event::message_update(
                    crate::llm::Delta::Thinking("half a thought".into()),
                    vec![thinking_block("half a thought")],
                ),
            ],
        );
        assert!(
            thinking_stream.has_live_stream(),
            "a streaming reasoning delta is a live stream"
        );

        // An answer-text delta => the text arm holds.
        let text_stream = fold(
            fresh(),
            vec![
                Event::run_started("r1"),
                Event::message_start(1),
                Event::message_update(
                    crate::llm::Delta::Text("half an ans".into()),
                    vec![text_block("half an ans")],
                ),
            ],
        );
        assert!(
            text_stream.has_live_stream(),
            "a streaming answer delta is a live stream"
        );
    }

    // --- streaming (the arms; the materialize rules live with the store) ----

    #[test]
    fn run_started_marks_running_clears_snapshot_and_pins() {
        let t = fold(
            fresh(),
            vec![
                Event::message_start(1),
                Event::message_update(
                    crate::llm::Delta::Text("stale".into()),
                    vec![text_block("stale")],
                ),
            ],
        );
        let (t, effects) = t.apply_event(Event::run_started("r1"));
        assert_eq!(t.status, Status::Running);
        assert!(
            t.transcript().streaming_text().is_empty()
                && t.transcript().streaming_thinking().is_empty()
        );
        assert_eq!(effects, vec![Effect::PinBottom]);
    }

    // --- run_finished -----------------------------------------------------

    #[test]
    fn run_finished_flushes_snapshot_goes_idle_records_estimate_and_budget() {
        let t = fold(
            fresh_opts(ScreenOpts {
                context_budget: Some(100),
                ..Default::default()
            }),
            vec![
                Event::run_started("r1"),
                Event::message_start(1),
                Event::message_update(
                    crate::llm::Delta::Text("Done.".into()),
                    vec![text_block("Done.")],
                ),
                Event::RunFinished {
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
    // Event, RunFinished always carries a budget; the Agent forwards the live
    // budget it holds. We reproduce baud's assertion by emitting the same
    // budget the Screen was opened with (the Agent's live value).
    #[test]
    fn run_finished_keeps_previous_budget_when_event_carries_it() {
        let t = fold(
            fresh_opts(ScreenOpts {
                context_budget: Some(100),
                ..Default::default()
            }),
            vec![Event::RunFinished {
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
            vec![Event::RunFinished {
                stop_reason: StopReason::EndTurn,
                token_estimate: 0,
                context_budget: 0,
            }],
        );
        assert_eq!(items(&normal), vec![]);

        let abnormal = fold(
            fresh(),
            vec![Event::RunFinished {
                stop_reason: StopReason::MaxTokens,
                token_estimate: 0,
                context_budget: 0,
            }],
        );
        assert_eq!(items(&abnormal), vec![info("turn stopped: :max_tokens")]);
    }

    // --- context pressure --------------------------------------------------

    fn pressurized(estimate: u64) -> Screen {
        fold(
            fresh_opts(ScreenOpts {
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
            fresh_opts(ScreenOpts {
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
            t.transcript().items().last(),
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
        // A Nudge is a Governor aiding the model: an Aid marker with the `»`
        // glyph, after the materialized assistant text.
        assert_eq!(
            items(&t),
            vec![
                assistant("all done"),
                marker("» [files changed but not verified]", Tone::Aid)
            ]
        );
        assert_eq!(effects, vec![]);
    }

    #[test]
    fn recovery_run_shows_its_prompt_as_an_aid_marker_and_marks_running() {
        let prompt = crate::voice::recovery_prompt(true);
        let (t, effects) = fresh().apply_event(Event::recovery_run(
            crate::session::RecoveryShape::Handoff,
            prompt,
        ));
        assert_eq!(items(&t), vec![marker(&format!("» {prompt}"), Tone::Aid)]);
        assert_eq!(t.status, Status::Running);
        assert_eq!(effects, vec![Effect::PinBottom]);
    }

    // A Plan refresh (CONTEXT.md: Anchor) shows a CONCISE Aid marker, never the
    // plan body: the full anchor text stays on the Session-Log emit seam.
    #[test]
    fn an_anchor_shows_a_concise_plan_refreshed_aid_marker() {
        let full_anchor = "[anchor - current goal and plan]\n\nOriginal task:\nfix it\n\n\
                           Current plan:\n- step one\n- step two";
        let (t, effects) = fresh().apply_event(Event::anchor(full_anchor));
        assert_eq!(effects, vec![]);
        // The concise marker, not the plan body.
        assert_eq!(items(&t), vec![marker("⚑ plan refreshed", Tone::Aid)]);
        let TranscriptItem::Marker { text, .. } = &items(&t)[0] else {
            panic!("expected a Marker");
        };
        assert!(
            !text.contains("step one"),
            "the plan body must not reach the Transcript"
        );
    }

    // The Endgame narrowing to run_command (the Verification Pass) shows a
    // Constrain marker naming the surviving tool.
    #[test]
    fn tools_narrowed_to_a_tool_shows_a_constrain_marker() {
        let (t, effects) =
            fresh().apply_event(Event::tools_narrowed(vec!["run_command".to_string()]));
        assert_eq!(effects, vec![]);
        assert_eq!(
            items(&t),
            vec![marker("⊘ tools narrowed to run_command", Tone::Constrain)]
        );
    }

    // The final Pass withdraws every Tool (empty narrowing): the Constrain
    // marker reads "withdrawn", not an empty "narrowed to ".
    #[test]
    fn tools_withdrawn_on_the_final_pass_shows_a_constrain_marker() {
        let (t, effects) = fresh().apply_event(Event::tools_narrowed(vec![]));
        assert_eq!(effects, vec![]);
        assert_eq!(
            items(&t),
            vec![marker("⊘ tools withdrawn", Tone::Constrain)]
        );
    }

    #[test]
    fn wrap_up_warning_shows_as_a_constrain_marker() {
        let warning = crate::voice::wrap_up_warning(2);
        let (t, effects) = fresh().apply_event(Event::voiced(
            crate::event::VoicedTag::WrapUpWarning,
            warning.clone(),
        ));
        // The Endgame run-close schedule limits the model: a Constrain marker.
        assert_eq!(
            items(&t),
            vec![marker(&format!("▪ {warning}"), Tone::Constrain)]
        );
        assert_eq!(effects, vec![]);
    }

    #[test]
    fn verification_pass_shows_as_a_constrain_marker() {
        let prompt = crate::voice::verification_pass_prompt();
        let (t, effects) = fresh().apply_event(Event::voiced(
            crate::event::VoicedTag::VerificationPass,
            prompt,
        ));
        assert_eq!(
            items(&t),
            vec![marker(&format!("▪ {prompt}"), Tone::Constrain)]
        );
        assert_eq!(effects, vec![]);
    }

    // The remaining voiced riders each land as ONE toned marker with no
    // effects, through the same shared arm: the corrective Nudges are Aid
    // (`»`), the Endgame's FinalPass is Constrain (`▪`).
    #[test]
    fn the_remaining_voiced_riders_show_as_toned_markers() {
        use crate::event::VoicedTag;
        for (tag, tone, glyph) in [
            (VoicedTag::VerifyFailedNudge, Tone::Aid, "»"),
            (VoicedTag::EmptyResponseNudge, Tone::Aid, "»"),
            (VoicedTag::ExploreNudge, Tone::Aid, "»"),
            (VoicedTag::FinalPass, Tone::Constrain, "▪"),
        ] {
            let (t, effects) = fresh().apply_event(Event::voiced(tag, "[rider text]"));
            assert_eq!(effects, vec![], "{tag:?} minted effects");
            assert_eq!(
                items(&t),
                vec![marker(&format!("{glyph} [rider text]"), tone)],
                "{tag:?}"
            );
        }
    }

    // A bounded re-draw (ADR-0030) is silent to the Conversation but never to
    // the operator: one info line names the attempt against the budget.
    #[test]
    fn a_retry_recedes_one_bounded_redraw_info_line() {
        let (t, effects) = fresh().apply_event(Event::retry("unknown tool", 1, 3));
        assert_eq!(effects, vec![]);
        assert_eq!(
            items(&t),
            vec![info("malformed tool call - re-drawing (1/3)")]
        );
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

    // --- submit / steer outcomes --------------------------------------------
    //
    // Enter's submit-vs-steer decision lives in the Composer (`ui::composer`,
    // ADR-0034); these pin the SEAM - the submitted/steered outcome hooks and
    // the retry pair, which stay here because they touch the Transcript and
    // the Agent status, and because the draft must survive a retry.

    #[test]
    fn successful_submit_appends_user_clears_records_history_and_pins() {
        let t = press(fresh(), typed("fix the bug"));
        let (t, effects) = t.submitted("fix the bug", Ok(()));
        assert_eq!(items(&t), vec![user("fix the bug")]);
        assert_eq!(t.composer().view().draft, "");
        assert_eq!(
            effects,
            vec![
                Effect::PinBottom,
                Effect::HistoryAppend("fix the bug".into())
            ]
        );
        // Recorded into the ring through the Composer's hook: Up recalls it.
        let (t, _) = t.handle_key(Key::ArrowUp);
        assert_eq!(t.composer().view().draft, "fix the bug");
    }

    #[test]
    fn busy_submit_retries_as_steering_and_the_draft_survives() {
        let t = press(fresh(), typed("another task"));
        let (t, effects) = t.submitted("another task", Err(Busy));
        assert_eq!(t.status, Status::Running);
        assert_eq!(
            effects,
            vec![Effect::Agent(AgentCommand::Steer("another task".into()))]
        );
        // Only a successful send clears the Composer - the retry must not.
        assert_eq!(t.composer().view().draft, "another task");
    }

    // --- steering ------------------------------------------------------------
    //
    // The marker text and the queued→delivered promotion are the store's rule
    // (`ui::transcript`); this pins the ARM - queued pins the viewport,
    // delivered is silent, and both land in the store.

    #[test]
    fn steering_events_delegate_and_queued_pins_bottom() {
        let (t, effects) = fresh().apply_event(Event::steering_queued("check the README"));
        assert_eq!(effects, vec![Effect::PinBottom]);

        let (t, effects) = t.apply_event(Event::steering_delivered("check the README"));
        assert_eq!(effects, vec![]);
        assert_eq!(items(&t), vec![user("check the README")]);
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

    // An Eviction wave recedes ONE Housekeeping marker. It must NOT touch the
    // status bar's `dead_mass_pct` - the bar tracks the LIVE figure off
    // ContextPressure, and this wave just cleared what it found (the S1 bug:
    // advertising the reclaimed snapshot).
    #[test]
    fn an_eviction_wave_recedes_one_marker_without_setting_the_live_bar() {
        let t = fresh();
        assert_eq!(t.dead_mass_pct, None);
        let (t, effects) = t.apply_event(Event::eviction_wave(wave_stats()));
        assert_eq!(effects, vec![]);
        assert_eq!(
            items(&t),
            vec![marker(
                "✂ context wave · 12% dead mass · 3 results, 1 read superseded, 2 husked",
                Tone::Housekeeping
            )]
        );
        // The wave did not set the live bar figure.
        assert_eq!(t.dead_mass_pct, None);
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

    // A Session cost update refreshes the bar figure and nothing else: no
    // Transcript item, no effects - and later totals replace, never add.
    #[test]
    fn session_cost_refreshes_the_bar_figure_silently() {
        let t = fresh();
        assert_eq!(t.session_cost, 0.0);
        let (t, effects) = t.apply_event(Event::session_cost(0.007));
        assert_eq!(effects, vec![]);
        assert_eq!(t.session_cost, 0.007);
        assert_eq!(items(&t), vec![], "never a Transcript item");

        // The event carries the cumulative total; the fold stores, not sums.
        let (t, _) = t.apply_event(Event::session_cost(0.42));
        assert_eq!(t.session_cost, 0.42);
    }

    // Compaction progress recedes one Housekeeping marker.
    #[test]
    fn compaction_progress_recedes_one_marker() {
        let t = fresh();
        let (t, effects) = t.apply_event(Event::compaction_progress("working"));
        assert_eq!(effects, vec![]);
        assert_eq!(
            items(&t),
            vec![marker(
                "⟨ compaction: working → summary ⟩",
                Tone::Housekeeping
            )]
        );
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
            "✂ context wave · 5% dead mass · 3 results"
        );

        assert_eq!(
            eviction_wave_line(&wave_stats()),
            "✂ context wave · 12% dead mass · 3 results, 1 read superseded, 2 husked"
        );

        // A wave with no reclaimable counts still names the Dead Mass share.
        let none = WaveStats {
            dead_mass: 0.20,
            ..WaveStats::default()
        };
        assert_eq!(eviction_wave_line(&none), "✂ context wave · 20% dead mass");
    }

    // Every tone's glyph branch (ADR-0040): Aid/Constrain wear a glyph; the
    // three tones that author their own glyph elsewhere (Housekeeping,
    // Steering, Plain) carry none here.
    #[test]
    fn marker_glyph_covers_every_tone() {
        assert_eq!(marker_glyph(Tone::Aid), "»");
        assert_eq!(marker_glyph(Tone::Constrain), "▪");
        assert_eq!(marker_glyph(Tone::Housekeeping), "");
        assert_eq!(marker_glyph(Tone::Steering), "");
        assert_eq!(marker_glyph(Tone::Plain), "");
    }

    // A toned line prefixes its glyph and a space when the tone carries one,
    // and passes the text through unchanged when it does not (the empty-glyph
    // branch).
    #[test]
    fn marker_line_prefixes_a_glyph_only_when_the_tone_has_one() {
        // Non-empty glyph: prefix plus a space.
        assert_eq!(marker_line(Tone::Aid, "plan refreshed"), "» plan refreshed");
        assert_eq!(
            marker_line(Tone::Constrain, "tools withdrawn"),
            "▪ tools withdrawn"
        );
        // Empty glyph: the text is returned verbatim, no leading space.
        assert_eq!(marker_line(Tone::Housekeeping, "✂ evicted"), "✂ evicted");
        assert_eq!(marker_line(Tone::Steering, "↳ queued: x"), "↳ queued: x");
        assert_eq!(marker_line(Tone::Plain, "news"), "news");
    }

    #[test]
    fn successful_steer_clears_composer() {
        let (t, _) = fresh().apply_event(Event::run_started("r1"));
        let t = press(t, typed("check the README"));
        let (t, effects) = t.steered("check the README", Ok(()));
        assert_eq!(t.composer().view().draft, "");
        assert_eq!(effects, vec![]);
    }

    #[test]
    fn steer_that_lost_race_retries_as_submit_and_the_draft_survives() {
        let (t, _) = fresh().apply_event(Event::run_started("r1"));
        let t = press(t, typed("check the README"));
        let (t, effects) = t.steered("check the README", Err(Idle));
        assert_eq!(t.status, Status::Idle);
        assert_eq!(
            effects,
            vec![Effect::Agent(AgentCommand::Submit(
                "check the README".into()
            ))]
        );
        // Same retry rule as the busy submit: nothing clears.
        assert_eq!(t.composer().view().draft, "check the README");
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

    #[test]
    fn extension_error_events_become_info_lines() {
        let t = fold(
            fresh(),
            vec![Event::extension_error(
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

    // --- tool calls (the arms; the summary and pairing rules live with the
    // store, tested at `ui::transcript`) ---------------------------------------

    #[test]
    fn a_tool_call_recedes_one_pending_call_line() {
        let (t, effects) = fresh().apply_event(Event::tool_call(
            "t1",
            "read_file",
            serde_json::json!({"path": "src/main.rs"}),
        ));
        assert_eq!(effects, vec![]);
        assert_eq!(
            items(&t),
            vec![TranscriptItem::ToolCall {
                id: "t1".into(),
                name: "read_file".into(),
                summary: "src/main.rs".into(),
            }]
        );
    }

    #[test]
    fn a_tool_result_merges_with_its_call_into_one_line() {
        let t = fold(
            fresh(),
            vec![
                Event::tool_call(
                    "t1",
                    "run_command",
                    serde_json::json!({"command": "cargo test"}),
                ),
                Event::tool_result("t1", "run_command", "ok", false, HashMap::new()),
            ],
        );
        let items = items(&t);
        assert_eq!(items.len(), 1, "the call line was superseded");
        match &items[0] {
            TranscriptItem::ToolResult {
                name,
                is_error,
                key_arg,
                ..
            } => {
                assert_eq!(name, "run_command");
                assert!(!is_error);
                assert_eq!(key_arg.as_deref(), Some("cargo test"));
            }
            other => panic!("expected a merged result line, got {other:?}"),
        }
    }

    // --- Composer first refusal (ADR-0034) ----------------------------------
    //
    // The Composer's own rules - menu, selector, editing, history recall -
    // are tested at its interface in `ui::composer`; these pin the ROUTING
    // this fold owns: the fixed gate → Composer → own-arms order, the notice
    // wiring, and the refused key coming back by value.

    // The Composer's first refusal covers EVENTS too: a selector fill
    // delivered through this fold is consumed by the Composer - the overlay
    // flips to Ready, no Transcript item, no effects - so a stale or
    // overlay-less fill can never leak into the arms below.
    #[test]
    fn a_selector_fill_is_consumed_by_the_composer_never_this_folds_arms() {
        use crate::ui::composer::{OverlayStatus, OverlayView};
        use crate::ui::selector::SelectorRow;

        // Commit `/model` through the Screen: a Loading overlay opens and one
        // Command effect carries the activation generation to echo back.
        let t = press(fresh(), typed("/model"));
        let (t, effects) = t.handle_key(Key::Enter);
        let generation = match effects.as_slice() {
            [Effect::Command { name, generation }] if name == "model" => *generation,
            other => panic!("expected one Command effect, got {other:?}"),
        };

        let rows = vec![SelectorRow::new("qwen", "qwen", None)];
        let (t, effects) = t.apply_event(Event::selector_ready(generation, rows.clone()));
        assert_eq!(effects, vec![]);
        assert_eq!(items(&t), vec![], "never a Transcript item");
        match t.composer().view().overlay {
            Some(OverlayView::Selector {
                status: OverlayStatus::Ready,
                rows: got,
                ..
            }) => assert_eq!(got, rows),
            other => panic!("expected a Ready selector overlay, got {other:?}"),
        }
    }

    // Escape with an open overlay closes the overlay - it must NOT cancel the
    // running Run (Escape is only Cancellation when the Composer refuses it).
    #[test]
    fn escape_with_an_open_overlay_closes_it_instead_of_cancelling_the_run() {
        let (t, _) = fresh().apply_event(Event::run_started("r1"));
        let t = press(t, vec![Key::Char('/')]);
        assert!(
            t.composer().view().overlay.is_some(),
            "menu opens while running"
        );
        let (t, effects) = t.handle_key(Key::Escape);
        assert_eq!(effects, vec![], "no Cancel - the Composer consumed Escape");
        assert!(t.composer().view().overlay.is_none());
        assert_eq!(t.status, Status::Running, "the Turn is untouched");
        // With the Composer emptied, Escape is refused and Cancellation fires.
        let (_t, effects) = t.handle_key(Key::Escape);
        assert_eq!(effects, vec![Effect::Agent(AgentCommand::Cancel)]);
    }

    // A refused key comes back BY VALUE and still reaches the arms below,
    // mid-draft included: refusal returns the key, it does not drop it.
    #[test]
    fn a_refused_key_reaches_the_scroll_arms_mid_draft() {
        let t = press(fresh(), typed("half a thought"));
        let (t, effects) = t.handle_key(Key::PageUp);
        assert_eq!(effects, vec![Effect::ScrollUp(ScrollStep::Page)]);
        assert_eq!(
            t.composer().view().draft,
            "half a thought",
            "the draft is untouched"
        );
    }

    // The Composer's notice (the unknown-command line) lands as a normal info
    // line through the store - never an Effect the adapter must interpret.
    #[test]
    fn a_composer_notice_becomes_an_info_line() {
        let t = press(fresh(), typed("/nope"));
        let (t, effects) = t.handle_key(Key::Enter);
        assert_eq!(effects, vec![], "no Turn, no command effect");
        assert_eq!(items(&t), vec![info("unknown command: /nope")]);
        assert_eq!(t.composer().view().draft, "", "draft cleared");
    }

    // --- Cancellation and errors -------------------------------------------

    #[test]
    fn escape_while_running_no_modal_cancels() {
        let (t, _) = fresh().apply_event(Event::run_started("r1"));
        let (_t, effects) = t.handle_key(Key::Escape);
        assert_eq!(effects, vec![Effect::Agent(AgentCommand::Cancel)]);
    }

    #[test]
    fn escape_while_idle_does_nothing() {
        let (_t, effects) = fresh().handle_key(Key::Escape);
        assert_eq!(effects, vec![]);
    }

    #[test]
    fn run_cancelled_flushes_snapshot_goes_idle_notes_cancellation() {
        let t = fold(
            fresh(),
            vec![
                Event::run_started("r1"),
                Event::message_start(1),
                Event::message_update(
                    crate::llm::Delta::Thinking("half a thought".into()),
                    vec![thinking_block("half a thought")],
                ),
                Event::RunCancelled,
            ],
        );
        assert_eq!(t.status, Status::Idle);
        assert_eq!(
            items(&t),
            vec![thinking("half a thought"), info("turn cancelled")]
        );
    }

    #[test]
    fn run_cancelled_clears_pending_approval_and_refocuses() {
        let t = fold(fresh(), vec![Event::run_started("r1")]);
        let t = with_pending_approval(t, &approval());
        let (t, effects) = t.apply_event(Event::RunCancelled);
        assert_eq!(t.pending_approval, None);
        assert_eq!(effects, vec![Effect::FocusComposer]);
    }

    #[test]
    fn run_error_notes_reason_and_goes_idle() {
        let (t, _) = fresh().apply_event(Event::run_started("r1"));
        let (t, _) = t.apply_event(Event::RunError {
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
                Event::run_started("r1"),
                Event::message_start(1),
                Event::message_update(
                    crate::llm::Delta::Text("half an ans".into()),
                    vec![text_block("half an ans")],
                ),
            ],
        );
        let t = with_pending_approval(t, &approval());
        let (t, effects) = t.agent_down();
        assert_eq!(t.status, Status::Idle);
        assert_eq!(t.pending_approval, None);
        assert_eq!(
            t.transcript().items().last(),
            Some(&info("agent restarted; session history was reset"))
        );
        assert!(t.transcript().items().contains(&assistant("half an ans")));
        assert_eq!(effects, vec![Effect::FocusComposer]);
    }

    // --- info (adapter-side news) --------------------------------------------

    // The adapter's direct line in: Resume drift notes and other
    // adapter-authored news append as one info line through the store.
    #[test]
    fn info_appends_one_adapter_authored_line() {
        let t = fresh().info("resume: 2 turns replayed with drift");
        assert_eq!(items(&t), vec![info("resume: 2 turns replayed with drift")]);
    }

    // --- unknown input -----------------------------------------------------

    #[test]
    fn a_stale_selector_fill_and_an_unknown_key_are_ignored() {
        let t = fresh();
        // A selector fill with no overlay open is the Composer's own event
        // (ADR-0034): it is consumed there, changes nothing, and never reaches
        // a Transcript item.
        let (t, effects) = t.apply_event(Event::selector_ready(0, vec![]));
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
        let (t, _) = fresh().apply_event(Event::run_started("r1"));
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

    // --- the Approval gate vs the Composer ----------------------------------
    //
    // Editing itself is tested at the Composer's interface (`ui::composer`);
    // this pins the gate ORDER - the modal runs before the Composer's first
    // refusal, so a typed char must NOT edit the draft while it is open.

    #[test]
    fn typed_chars_do_not_edit_the_composer_while_modal_open() {
        let t = press(fresh(), typed("draft"));
        let t = with_pending_approval(t, &approval());
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
        assert_eq!(t.composer().view().draft, "draft");
        assert_eq!(t.composer().view().cursor, 5);
        assert_eq!(t.pending_approval, pending_before);
    }
}
