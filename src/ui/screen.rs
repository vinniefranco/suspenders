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

use crate::approvals::ApprovalMode;
use crate::conversation::compaction_target;
use crate::event::Event;
use crate::extensions::Registered;
use crate::llm::response::StopReason;
use crate::ui::composer::{Composer, EventOutcome, KeyOutcome};
use crate::ui::selection::{SelectionKey, SelectionList, SelectionOutcome};
use crate::ui::transcript::Transcript;
use crate::view_model::Tone;
use crate::view_model::{DiffHunk, DiffLine, DiffSide, TranscriptItem};

/// The greeting line a fresh Screen opens its Transcript with.
const GREETING: &str = "suspenders ready. Enter submits, Esc cancels a running turn, Ctrl-T toggles thinking, Ctrl-C quits";

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
            "run_command" => ConfirmKind::Exec,
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
    /// Mouse wheel up - list-nav ONLY, for the pre-agent Session Picker's own
    /// alt-screen list (`ui::pick_loop` mints it via `map_mouse`). The main
    /// inline loop no longer captures the mouse (ADR-0046: native scrollback owns
    /// history), so it never mints this into the Screen/Composer.
    WheelUp,
    /// Mouse wheel down - list-nav ONLY for the Session Picker, like
    /// [`Key::WheelUp`]; never minted into the main inline loop (ADR-0046).
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
    /// Shift+Tab (win32: Tab): rotate the Approval mode one step in the cycle
    /// (ADR-0050). Named (not `Char`) so the intent reads at the mapping and
    /// routing seams alike.
    CycleApprovalMode,
    /// Bare Tab: accepts the highlighted `/` palette suggestion (ADR-0051
    /// System B, qwen `handleAutocomplete`). Inert outside the palette (the
    /// editing fall-through refuses it), so it never types a literal tab.
    Tab,
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
    /// Move keyboard focus back to the composer.
    FocusComposer,
    /// Freeze `count` leading pending items into native scrollback (ADR-0046,
    /// the inline `insert_before` seam). Carries only the COUNT - the rendering
    /// belongs to the adapter/components (ADR-0019): the adapter reads the items
    /// through [`Screen::transcript`] + its `RenderCache` and blits the slice
    /// `[committed_high_water(), committed_high_water() + count)` above the
    /// pending region, then - and ONLY on a successful blit - advances the
    /// high-water mark by `count` (the freeze is TRANSACTIONAL; the pure fold
    /// does not move the mark). Emitted only when `count > 0`. Replaces the old
    /// `PinBottom` effect - pinning is meaningless with native scrollback.
    Commit { count: usize },
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
    pub compaction_slack: f64,
    pub extensions: Vec<Registered>,
    pub history: Vec<String>,
    /// Launch-time info lines the adapter authors (context-file skips today):
    /// news from before the event loop existed, recorded right after the
    /// greeting so it is visible without ever entering the Conversation.
    pub notices: Vec<String>,
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
        title: "edit_file src/lexer.rs (+4 -2)".into(),
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
            approval_mode: ApprovalMode::default(),
            token_estimate: None,
            context_budget: opts.context_budget,
            compaction_slack: opts.compaction_slack,
            pressure_level: PressureLevel::Ok,
            session_cost: INITIAL_SESSION_COST,
            composer: Composer::new(opts.history),
            thinking_expanded: false,
            tools_expanded: false,
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
        t.push(tool("list_files", ".", ".claude/ (+19 more lines)", false));
        t.push(tool(
            "grep",
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
        t.push(tool("list_files", "src", "agent/ (+32 more lines)", false));
        t.push(tool("list_files", "docs", "adr/ (+2 more lines)", false));
        t.marker("» [reading file after file fills your context - grep for the symbol you actually need first instead; then read only what you will change]", Tone::Aid);
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
        // items into the Transcript, so the whole dispatch is wrapped in the
        // commit seam (ADR-0046): whatever newly-terminal prefix the fold
        // produced is frozen into scrollback on the way out.
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

            event @ (Event::SteeringQueued { .. } | Event::SteeringDelivered { .. }) => {
                self.apply_steering(event)
            }

            event @ (Event::SessionLogError { .. }
            | Event::Retry { .. }
            | Event::LoopStall { .. }) => self.apply_voice(event),

            event @ (Event::RunFinished { .. } | Event::RunCancelled | Event::RunError { .. }) => {
                self.apply_settlement(event)
            }

            // The selector fills are the Composer's own (ADR-0034): they are
            // consumed by `self.composer.apply_event` at the top of this fold
            // (line 304) and never reach this dispatch. Listed explicitly, with
            // no wildcard, so the match stays EXHAUSTIVE over the Event
            // vocabulary - a future variant is a compile error here, not a
            // silent fallthrough.
            Event::SelectorReady { .. } | Event::SelectorFailed { .. } => (self, vec![]),
        };
        screen.with_commit(effects)
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
                // No PinBottom (ADR-0046): native scrollback owns history and
                // the inline pending region always follows the tail.
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

            // An Extension crashed and was skipped (fail-open, ADR-0007) - the
            // same report line the store's own Presentment failures use.
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
                self.pending_approval = Some(PendingApproval {
                    approval_id,
                    command,
                    kind,
                    selection: SelectionList::new(APPROVAL_OPTION_COUNT),
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

            // The Agent rotated its Approval mode (ADR-0050): mirror it for the
            // footer indicator. Display-only - the Screen never decides the mode.
            Event::ApprovalModeChanged { mode } => {
                self.approval_mode = mode;
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
    /// While an Approval is pending, the inline radio holds the keyboard
    /// (ADR-0049): the arrow keys + Enter drive the [`SelectionList`], the
    /// numbered digits quick-select, and the legacy `y`/`n`/`a` quick-keys stay
    /// as a SUPERSET (approve-once / deny / approve-always). Escape DENIES THIS
    /// TOOL and lets the Run continue (qwen-faithful, matching the `(esc)` option
    /// label), NOT cancel the Run. Every other key is swallowed; in particular,
    /// plain chars must NOT edit the Composer while the block is open. Escape
    /// only cancels the whole Run in the no-approval, streaming case below.
    pub fn handle_key(mut self, key: Key) -> (Self, Vec<Effect>) {
        if self.pending_approval.is_some() {
            return self.handle_approval_key(key);
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
                return self.with_commit(effects);
            }
            KeyOutcome::Refused(key) => key,
        };

        let (screen, effects) = match key {
            // Escape means Cancellation while a Run runs; the Composer has
            // already refused it (no overlay was open to close).
            Key::Escape if self.status == Status::Running => {
                (self, vec![Effect::Agent(AgentCommand::Cancel)])
            }

            // PageUp/PageDown and the mouse wheel no longer scroll the
            // transcript (ADR-0046): native scrollback owns history, so these
            // fall through to the no-op arm below. They remain in [`Key`] for
            // the pre-agent Session Picker (its own alt-screen list still
            // navigates by wheel/page).

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

            // Shift+Tab: rotate the Approval mode (ADR-0050). A fire-through to
            // the Agent; the new mode returns via `Event::ApprovalModeChanged`
            // and updates the footer. Works with no Run in flight (Session-
            // scoped) and with no Approval open (the gate above already returned
            // when one was).
            Key::CycleApprovalMode => (self, vec![Effect::Agent(AgentCommand::CycleApprovalMode)]),

            _ => (self, vec![]),
        };
        screen.with_commit(effects)
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
        t.with_commit(out)
    }

    /// Records how the `Submit` effect went: `Ok` appends the user line and
    /// routes through the commit seam (ADR-0046: the settled User line freezes
    /// on this exit), and hands the Composer its success -
    /// [`Composer::submitted_ok`] records the prompt into the ring, clears the
    /// draft, and mints the on-disk `HistoryAppend`. `Err(Busy)`
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
                // Route through the commit seam (ADR-0046): the settled User line
                // is terminal, so this public transcript-mutating exit advances
                // the commit seam uniformly like the two folds do, instead of
                // waiting for the next event to freeze it.
                let effects = self.composer.submitted_ok(&prompt);
                self.with_commit(effects)
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
                // Route through the commit seam (ADR-0046) for uniformity: this
                // hook mutates no terminal item itself (the pending steering
                // marker arrives via `steering_queued` and is non-terminal), but
                // routing it keeps "every public transcript-mutating fold exit
                // advances the commit seam" a uniform rule rather than an
                // exception - it freezes any already-terminal prefix.
                self.with_commit(vec![])
            }
            Err(Idle) => {
                self.status = Status::Idle;
                (self, vec![Effect::Agent(AgentCommand::Submit(text))])
            }
        }
    }

    /// Appends an info line (Resume drift notes, adapter-side news). The info
    /// line is terminal, so this routes through the commit seam (ADR-0046) like
    /// every other public transcript-mutating exit; the returned effects carry
    /// the `Commit` the adapter drains.
    pub fn info(mut self, text: impl Into<String>) -> (Self, Vec<Effect>) {
        self.transcript.info(text);
        self.with_commit(vec![])
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

    /// Advances the committed high-water mark by `n` (ADR-0046): the adapter
    /// half of the TRANSACTIONAL commit seam. The pure fold emits `Commit { n }`
    /// but does NOT move the mark; the adapter (`ui::commit_items`) blits the
    /// slice into native scrollback and calls THIS only on a successful freeze,
    /// so a failed blit leaves the items uncommitted (they redraw pending). This
    /// is the ONE mutable door on the Transcript outside the folds, kept narrow
    /// on purpose: it moves only the mark (never `items`/`revision`, see
    /// [`Transcript::mark_committed`]), so the TEA invariant holds.
    pub fn mark_committed(&mut self, n: usize) {
        self.transcript.mark_committed(n);
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

    // The Commit seam at a public fold exit (ADR-0046): appends an
    // [`Effect::Commit`] carrying the count of newly-committable leading items
    // (`committable_upto() - committed_high_water()`) when it is positive, so
    // the adapter can freeze that slice into native scrollback.
    //
    // TRANSACTIONAL (ADR-0046): the pure fold does NOT advance the high-water
    // mark - it only computes and EMITS the count. Advancing the mark is the
    // adapter's job, done ONLY after `insert_before` succeeds (`ui::commit_items`
    // calls `mark_committed`). This keeps the freeze atomic: a failed blit
    // leaves the items uncommitted so they redraw in the pending region rather
    // than being silently dropped. At most ONE `Commit` is appended per fold (the
    // trailing position), so the emitted count is unambiguous. Both `apply_event`
    // and `handle_key` route their final effect vector through here.
    fn with_commit(self, mut effects: Vec<Effect>) -> (Self, Vec<Effect>) {
        debug_assert!(
            !effects.iter().any(|e| matches!(e, Effect::Commit { .. })),
            "with_commit must be the sole minter of Effect::Commit (at-most-one per fold)"
        );
        let hw = self.transcript.committed_high_water();
        let count = self.transcript.committable_upto().saturating_sub(hw);
        if count > 0 {
            effects.push(Effect::Commit { count });
        }
        (self, effects)
    }

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
mod tests {
    use super::*;
    use crate::content::ContentBlock;
    use crate::event::Stage;
    use crate::view_model::Tone;
    use crate::view_model::TranscriptItem;
    use std::collections::HashMap;

    // --- helpers mirroring transcript_test.exs -----------------------------

    fn fresh() -> Screen {
        Screen::new(ScreenOpts::default())
    }

    fn fresh_opts(opts: ScreenOpts) -> Screen {
        Screen::new(opts)
    }

    // Drops a trailing [`Effect::Commit`] (ADR-0046) so a fold's OWN effects
    // can be asserted without threading the commit-seam count through every
    // pre-existing effect test. A fresh Screen opens with an uncommitted
    // greeting, so the first public fold exit legitimately appends a
    // `Commit { count }`; the seam has its own dedicated tests below, and these
    // orthogonal assertions strip it. Only ever drops from the END (the seam
    // appends there) and only a Commit, so a mislaid effect still fails.
    fn sans_commit(mut effects: Vec<Effect>) -> Vec<Effect> {
        if matches!(effects.last(), Some(Effect::Commit { .. })) {
            effects.pop();
        }
        effects
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

    // A PendingApproval as `apply_event(ApprovalRequest)` builds it with no live
    // ToolCall in the transcript: ConfirmKind falls back to `Info` and the radio
    // is a fresh 3-row SelectionList. Tests that compare `pending_approval`
    // against this must open the modal the same way (`with_pending_approval`).
    fn approval_with(command: &str) -> PendingApproval {
        PendingApproval {
            approval_id: format!("ref-{command}"),
            command: command.to_string(),
            kind: ConfirmKind::Info,
            selection: SelectionList::new(APPROVAL_OPTION_COUNT),
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

    // Asserts that pressing `key` while the approval modal is open produces no
    // effects and leaves the pending approval untouched. Shared by the modal
    // swallow tests so the loop shape is written once.
    fn assert_key_swallowed_while_modal_open(key: Key) {
        let label = format!("{key:?}");
        let a = approval();
        let t = with_pending_approval(fresh(), &a);
        let pending_before = t.pending_approval.clone();
        let (t, effects) = t.handle_key(key);
        assert_eq!(effects, vec![], "expected no effects for {label}");
        assert_eq!(
            t.pending_approval, pending_before,
            "pending approval changed for {label}"
        );
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
    fn run_started_marks_running_and_clears_snapshot() {
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
        // No PinBottom (ADR-0046); the greeting may still commit here.
        assert_eq!(sans_commit(effects), vec![]);
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
                compaction_slack: 0.10,
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
            fresh_opts(ScreenOpts {
                context_budget: Some(100),
                compaction_slack: 0.0,
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
        assert_eq!(sans_commit(effects), vec![Effect::FocusModal]);
    }

    // P2: a `run_command` approval derives `ConfirmKind::Exec` (not the Info
    // fallback the bare `approval_with` helper hard-codes). The kind comes from
    // the newest live ToolCall's name (ADR-0049), so we must emit that call
    // first, then the ApprovalRequest. This proves the exec question path.
    #[test]
    fn a_run_command_approval_derives_confirm_kind_exec() {
        let t = fold(
            fresh(),
            vec![
                Event::run_started("r1"),
                Event::tool_call(
                    "t1",
                    "run_command",
                    serde_json::json!({"command": "cargo test"}),
                ),
                Event::approval_request("approval-0", "cargo test"),
            ],
        );
        let pending = t.pending_approval.as_ref().expect("an open approval");
        // Exec (not the Info fallback): this is the kind the render reads to draw
        // the `Allow execution of: '{command}'?` question (ADR-0049), so the exec
        // question path is exercised - not only the Info fallback the other
        // Screen tests hard-code.
        assert_eq!(pending.kind, ConfirmKind::Exec);
        assert_eq!(pending.command, "cargo test");
    }

    #[test]
    fn y_approves_clears_and_refocuses() {
        let a = approval();
        let t = with_pending_approval(fresh(), &a);
        let (t, effects) = t.handle_key(Key::Char('y'));
        assert_eq!(t.pending_approval, None);
        assert_eq!(
            sans_commit(effects),
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
            sans_commit(effects),
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
            sans_commit(effects),
            vec![
                Effect::Agent(AgentCommand::Approve(
                    a.approval_id,
                    Decision::ApproveAlways
                )),
                Effect::FocusComposer,
            ]
        );
    }

    // Escape while an Approval is open DENIES this tool and the Run continues
    // (P1, qwen `ToolConfirmationMessage.tsx:106-114`; matches the
    // `No, suggest changes (esc)` label) - it does NOT cancel the Run.
    #[test]
    fn escape_while_modal_open_denies_the_tool_not_the_run() {
        let a = approval();
        let t = with_pending_approval(fresh(), &a);
        let (t, effects) = t.handle_key(Key::Escape);
        assert_eq!(t.pending_approval, None);
        assert_eq!(
            sans_commit(effects),
            vec![
                Effect::Agent(AgentCommand::Approve(a.approval_id, Decision::Deny)),
                Effect::FocusComposer,
            ]
        );
    }

    // The counterpart (P1): with NO approval open and a Run streaming, Escape
    // STILL cancels the whole Run (qwen's `esc to cancel` spinner + suspenders'
    // global cancel). This behavior is unchanged.
    #[test]
    fn escape_while_streaming_without_an_approval_cancels_the_run() {
        let mut t = fresh();
        t.status = Status::Running;
        assert!(t.pending_approval.is_none());
        let (_t, effects) = t.handle_key(Key::Escape);
        assert_eq!(
            sans_commit(effects),
            vec![Effect::Agent(AgentCommand::Cancel)]
        );
    }

    // Keys the radio does not act on (non-digit chars, page keys) are swallowed
    // with no effect and no change to the pending Approval. Enter and the arrows
    // are NOT here - they now drive the radio (asserted below).
    #[test]
    fn every_other_key_swallowed_while_modal_open() {
        for key in [Key::Char('x'), Key::PageUp, Key::PageDown, Key::Char('q')] {
            assert_key_swallowed_while_modal_open(key);
        }
    }

    // The inline radio (ADR-0049): Enter selects the active row (option 0,
    // Approve, by default), so it resolves the Approval and refocuses.
    #[test]
    fn enter_selects_the_active_radio_row_which_is_approve_once() {
        let a = approval();
        let t = with_pending_approval(fresh(), &a);
        let (t, effects) = t.handle_key(Key::Enter);
        assert_eq!(t.pending_approval, None);
        assert_eq!(
            sans_commit(effects),
            vec![
                Effect::Agent(AgentCommand::Approve(a.approval_id, Decision::Approve)),
                Effect::FocusComposer,
            ]
        );
    }

    // ArrowDown moves the radio to row 1 (Always allow); Enter there resolves as
    // ApproveAlways. The move itself emits no effect but changes the selection.
    #[test]
    fn arrow_down_then_enter_selects_approve_always() {
        let a = approval();
        let t = with_pending_approval(fresh(), &a);
        let (t, moved) = t.handle_key(Key::ArrowDown);
        assert_eq!(sans_commit(moved), vec![], "a move emits no effect");
        assert_eq!(
            t.pending_approval.as_ref().unwrap().selection.active(),
            1,
            "the radio moved to row 1"
        );
        let (t, effects) = t.handle_key(Key::Enter);
        assert_eq!(t.pending_approval, None);
        assert_eq!(
            sans_commit(effects),
            vec![
                Effect::Agent(AgentCommand::Approve(
                    a.approval_id,
                    Decision::ApproveAlways
                )),
                Effect::FocusComposer,
            ]
        );
    }

    // The numbered digits quick-select (3 rows, so a digit always resolves
    // immediately): `2` → Always allow (row 1, ApproveAlways), `3` → No/Deny.
    #[test]
    fn digit_two_quick_selects_approve_always() {
        let a = approval();
        let t = with_pending_approval(fresh(), &a);
        let (t, effects) = t.handle_key(Key::Char('2'));
        assert_eq!(t.pending_approval, None);
        assert_eq!(
            sans_commit(effects),
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
    fn digit_three_quick_selects_deny() {
        let a = approval();
        let t = with_pending_approval(fresh(), &a);
        let (t, effects) = t.handle_key(Key::Char('3'));
        assert_eq!(t.pending_approval, None);
        assert_eq!(
            sans_commit(effects),
            vec![
                Effect::Agent(AgentCommand::Approve(a.approval_id, Decision::Deny)),
                Effect::FocusComposer,
            ]
        );
    }

    // Shift+Tab (Key::CycleApprovalMode) fires the cycle command through the
    // Agent - even with an Approval open it does NOT disturb the pending block,
    // and with no Approval open it still fires.
    #[test]
    fn cycle_approval_mode_key_emits_the_cycle_command() {
        let (t, effects) = fresh().handle_key(Key::CycleApprovalMode);
        assert_eq!(
            sans_commit(effects),
            vec![Effect::Agent(AgentCommand::CycleApprovalMode)]
        );
        assert_eq!(t.pending_approval, None);
    }

    // The host-driven expire seam (ADR-0049): with the 3-row approval no digit
    // ever buffers, so expire_approval is a no-op - the block stays open and no
    // command fires however far the clock advances.
    #[test]
    fn expire_approval_is_a_no_op_for_the_three_row_radio() {
        let a = approval();
        let t = with_pending_approval(fresh(), &a);
        // A digit press resolves immediately (never buffers), so before any press
        // the buffer is empty and a far-future tick fires nothing.
        let (t, effects) = t.expire_approval(10_000);
        assert_eq!(sans_commit(effects), vec![]);
        assert_eq!(t.pending_approval, Some(a));
    }

    // With no Approval open, expire is inert.
    #[test]
    fn expire_approval_with_no_pending_is_inert() {
        let (t, effects) = fresh().expire_approval(10_000);
        assert_eq!(sans_commit(effects), vec![]);
        assert_eq!(t.pending_approval, None);
    }

    // The mirror event (ADR-0050): ApprovalModeChanged updates the Screen's
    // display-only copy and touches nothing else.
    #[test]
    fn approval_mode_changed_mirrors_the_mode_silently() {
        let (t, effects) = fresh().apply_event(Event::approval_mode_changed(ApprovalMode::Yolo));
        assert_eq!(t.approval_mode, ApprovalMode::Yolo);
        assert_eq!(sans_commit(effects), vec![]);
        assert_eq!(items(&t), vec![], "the mirror is never a Transcript item");
    }

    // Cycling the mode while an Approval is open (a Shift+Tab press) fires the
    // command and leaves the pending Approval whole - the block keeps holding the
    // keyboard.
    #[test]
    fn cycling_the_mode_while_the_approval_is_open_leaves_it_pending() {
        let a = approval();
        let t = with_pending_approval(fresh(), &a);
        // Shift+Tab is swallowed by the Approval gate (only the radio keys +
        // y/n/a + Escape act), so the block stays open and no command fires.
        let (t, effects) = t.handle_key(Key::CycleApprovalMode);
        assert_eq!(sans_commit(effects), vec![]);
        assert_eq!(t.pending_approval, Some(a));
    }

    #[test]
    fn approval_auto_appends_standing_info_without_touching_modal() {
        let (t, effects) = fresh().apply_event(Event::approval_auto("mix test"));
        assert_eq!(
            t.transcript().items().last(),
            Some(&info("auto-approved (standing): mix test"))
        );
        assert_eq!(t.pending_approval, None);
        assert_eq!(sans_commit(effects), vec![]);
    }

    // A bounded re-draw (ADR-0030) is silent to the Conversation but never to
    // the operator: one info line names the attempt against the budget.
    #[test]
    fn a_retry_recedes_one_bounded_redraw_info_line() {
        let (t, effects) = fresh().apply_event(Event::retry("unknown tool", 1, 3));
        assert_eq!(sans_commit(effects), vec![]);
        assert_eq!(
            items(&t),
            vec![info("malformed tool call - re-drawing (1/3)")]
        );
    }

    #[test]
    fn approval_resolved_clears_only_matching_pending() {
        let a = approval();
        let t = with_pending_approval(fresh(), &a);

        // Stale id: nothing happens (the greeting's Commit is orthogonal).
        let (t, effects) = t.apply_event(Event::approval_resolved("some-other-ref", true));
        assert_eq!(sans_commit(effects), vec![]);
        assert_eq!(t.pending_approval, Some(a.clone()));

        // Matching id: cleared, composer refocused.
        let (t, effects) = t.apply_event(Event::approval_resolved(a.approval_id.clone(), true));
        assert_eq!(t.pending_approval, None);
        assert_eq!(sans_commit(effects), vec![Effect::FocusComposer]);
    }

    // --- submit / steer outcomes --------------------------------------------
    //
    // Enter's submit-vs-steer decision lives in the Composer (`ui::composer`,
    // ADR-0034); these pin the SEAM - the submitted/steered outcome hooks and
    // the retry pair, which stay here because they touch the Transcript and
    // the Agent status, and because the draft must survive a retry.

    #[test]
    fn successful_submit_appends_user_clears_and_records_history() {
        let t = press(fresh(), typed("fix the bug"));
        let (t, effects) = t.submitted("fix the bug", Ok(()));
        assert_eq!(items(&t), vec![user("fix the bug")]);
        assert_eq!(t.composer().view().draft, "");
        // The on-disk HistoryAppend, then the commit seam (ADR-0046): submitted
        // now routes through `with_commit`, so the terminal greeting + the new
        // User line freeze on THIS exit (count 2), not the next event.
        assert_eq!(
            sans_commit(effects.clone()),
            vec![Effect::HistoryAppend("fix the bug".into())]
        );
        assert_eq!(commit_count(&effects), Some(2));
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
    // (`ui::transcript`); this pins the ARM - queued and delivered are both
    // silent of scroll effects now (native scrollback follows the tail), and
    // both land in the store.

    #[test]
    fn steering_events_delegate_and_land_in_the_store() {
        let (t, effects) = fresh().apply_event(Event::steering_queued("check the README"));
        // The greeting commits on this first fold; the Steering marker itself
        // is non-terminal, so it stays pending (ADR-0046). No PinBottom.
        assert_eq!(sans_commit(effects), vec![]);

        let (t, effects) = t.apply_event(Event::steering_delivered("check the README"));
        // Delivery promotes the marker to a terminal User line, which now
        // commits.
        assert_eq!(sans_commit(effects), vec![]);
        assert_eq!(items(&t), vec![user("check the README")]);
    }

    // --- context visibility (Bundle A) -------------------------------------

    // A Session cost update refreshes the bar figure and nothing else: no
    // Transcript item, no effects - and later totals replace, never add.
    #[test]
    fn session_cost_refreshes_the_bar_figure_silently() {
        let t = fresh();
        assert_eq!(t.session_cost, 0.0);
        let (t, effects) = t.apply_event(Event::session_cost(0.007));
        assert_eq!(sans_commit(effects), vec![]);
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
        assert_eq!(sans_commit(effects), vec![]);
        assert_eq!(
            items(&t),
            vec![marker(
                "⟨ compaction: working → summary ⟩",
                Tone::Housekeeping
            )]
        );
    }

    #[test]
    fn successful_steer_clears_composer() {
        let (t, _) = fresh().apply_event(Event::run_started("r1"));
        let t = press(t, typed("check the README"));
        let (t, effects) = t.steered("check the README", Ok(()));
        assert_eq!(t.composer().view().draft, "");
        // steered_ok adds no terminal item of its own, but routes through the
        // commit seam (ADR-0046) for uniformity: it freezes the still-pending
        // greeting, so the exit carries a Commit. No effect of its own beyond it.
        assert_eq!(sans_commit(effects), vec![]);
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
            vec![Event::extension_error("diff", Stage::PreRun, "boom")],
        );
        let items = items(&t);
        assert_eq!(items.len(), 1);
        match &items[0] {
            TranscriptItem::Info { text } => {
                assert!(text.contains("diff"));
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
        assert_eq!(sans_commit(effects), vec![]);
        assert_eq!(
            items(&t),
            vec![TranscriptItem::ToolCall {
                id: "t1".into(),
                name: "read_file".into(),
                summary: "src/main.rs".into(),
            }]
        );
    }

    // --- the Commit seam at the fold exits (ADR-0046) ------------------------

    // Returns the count a trailing Commit carries, or None when the fold
    // emitted no commit.
    fn commit_count(effects: &[Effect]) -> Option<usize> {
        effects.iter().find_map(|e| match e {
            Effect::Commit { count } => Some(*count),
            _ => None,
        })
    }

    // A fold that leaves a live ToolCall at the pending front commits only the
    // terminal items BEFORE it - here just the greeting - and never the call.
    #[test]
    fn a_pending_tool_call_blocks_the_commit_after_it() {
        let (_t, effects) = fresh().apply_event(Event::tool_call(
            "t1",
            "read_file",
            serde_json::json!({"path": "src/main.rs"}),
        ));
        // Greeting commits (1); the ToolCall stays pending.
        assert_eq!(commit_count(&effects), Some(1));
    }

    // Folds one event and returns the count a trailing Commit carried (or
    // None), ADVANCING the store's high-water mark by that count to mimic the
    // adapter's post-blit `mark_committed` - the pure fold no longer moves the
    // mark (ADR-0046, transactional commit), so a test that chains folds must
    // stand in for the adapter here or every subsequent Commit re-counts the
    // same leading items.
    fn fold_and_commit(mut t: Screen, event: Event) -> (Screen, Option<usize>) {
        let (mut next, effects) = t.apply_event(event);
        let count = commit_count(&effects);
        if let Some(n) = count {
            next.transcript.mark_committed(n);
        }
        t = next;
        (t, count)
    }

    // committed==pending identity for the inline approval (ADR-0049): the
    // confirming ToolCall carries no result while the Approval is open, so it is
    // non-terminal and BLOCKS the commit after it - the approval rows (which
    // render off `pending_approval`, not the item) can therefore never freeze
    // into scrollback. Once the decision resolves and the ToolResult supersedes
    // the call, the tail becomes terminal and commits - as a plain ToolResult,
    // with the approval gone.
    #[test]
    fn a_confirming_tool_call_blocks_commit_until_the_approval_resolves() {
        // Greeting commits; the gated ToolCall stays pending.
        let (t, greeting) = fold_and_commit(
            fresh(),
            Event::tool_call("t1", "run_command", serde_json::json!({"command": "ls"})),
        );
        assert_eq!(greeting, Some(1));

        // The Approval opens on the live call: still non-terminal, nothing new
        // commits, and the approval lives on `pending_approval` (never an item).
        let (t, opened) = fold_and_commit(t, Event::approval_request("approval-0", "ls"));
        assert_eq!(opened, None, "the confirming call blocks the commit");
        assert!(t.pending_approval.is_some());

        // Resolve: the pending Approval clears. The call is still an unresolved
        // ToolCall item (no result yet), so it STILL blocks the commit - the
        // approval rows are already gone (pending_approval is None).
        let (t, resolved) = fold_and_commit(t, Event::approval_resolved("approval-0", true));
        assert_eq!(t.pending_approval, None);
        assert_eq!(
            resolved, None,
            "the bare call still blocks until its result"
        );

        // The result supersedes the call → a terminal ToolResult, which commits.
        let (t, committed) = fold_and_commit(
            t,
            Event::tool_result("t1", "run_command", "ok", false, HashMap::new()),
        );
        assert_eq!(committed, Some(1), "the resolved call commits as a result");
        // The committed item is a plain ToolResult - no approval trace.
        assert_eq!(
            items(&t),
            vec![TranscriptItem::ToolResult {
                name: "run_command".into(),
                summary: "ok".into(),
                is_error: false,
                key_arg: Some("ls".into()),
            }]
        );
    }

    // Once the result merges the call away, the whole run tail becomes terminal
    // and the next fold exit commits it.
    #[test]
    fn a_tool_result_merge_lets_the_run_commit() {
        // First fold commits the greeting; the call stays pending.
        let (t, first) = fold_and_commit(
            fresh(),
            Event::tool_call(
                "t1",
                "run_command",
                serde_json::json!({"command": "cargo test"}),
            ),
        );
        assert_eq!(first, Some(1));
        // The result supersedes the call: the merged ToolResult is terminal, so
        // it now commits (count 1 - the greeting was already committed).
        let (_t, second) = fold_and_commit(
            t,
            Event::tool_result("t1", "run_command", "ok", false, HashMap::new()),
        );
        assert_eq!(second, Some(1));
    }

    // message_end settles the streamed answer into a terminal Assistant item,
    // which the fold exit commits.
    #[test]
    fn message_end_commits_the_settled_answer() {
        let (t, _) = fold_and_commit(fresh(), Event::run_started("r1"));
        let (t, _) = fold_and_commit(t, Event::message_start(1));
        let (t, _) = fold_and_commit(
            t,
            Event::message_update(
                crate::llm::Delta::Text("Done.".into()),
                vec![text_block("Done.")],
            ),
        );
        // The greeting committed on the first fold; the streaming snapshot is
        // not an item, so message_end is what settles the terminal answer.
        let (_t, count) = fold_and_commit(
            t,
            Event::message_end(vec![text_block("Done.")], StopReason::EndTurn),
        );
        assert_eq!(count, Some(1));
    }

    // Steering delivery promotes the pending marker to a terminal User line;
    // the delivering fold exit commits it. The queuing fold does not commit the
    // marker (it is non-terminal).
    #[test]
    fn steering_delivery_commits_the_promoted_user_line() {
        // Only the greeting commits on queue; the marker stays pending.
        let (t, queued) = fold_and_commit(fresh(), Event::steering_queued("check the README"));
        assert_eq!(queued, Some(1));
        // The promoted User line commits (count 1 - the greeting was already
        // committed).
        let (_t, delivered) = fold_and_commit(t, Event::steering_delivered("check the README"));
        assert_eq!(delivered, Some(1));
    }

    // TRANSACTIONAL commit (ADR-0046): a fold that EMITS `Commit { count }` must
    // NOT advance the high-water mark itself - the mark moves only when the
    // adapter's `insert_before` succeeds (`ui::commit_items` -> `mark_committed`).
    // So folding the same event twice through the pure core (without the adapter
    // running in between) re-emits the SAME commit: the mark never budged.
    #[test]
    fn the_pure_fold_does_not_advance_the_high_water_mark() {
        let t = fresh();
        assert_eq!(t.transcript().committed_high_water(), 0);
        // The greeting is committable; the fold emits Commit { 1 } but must leave
        // the mark at 0 (the adapter has not blitted yet).
        let (t, first) = t.apply_event(Event::run_started("r1"));
        assert_eq!(commit_count(&first), Some(1));
        assert_eq!(
            t.transcript().committed_high_water(),
            0,
            "the pure fold must not move the mark - the adapter does, post-blit"
        );
        // A second fold, still no adapter: the same greeting is STILL uncommitted,
        // so it re-emits Commit { 1 } rather than dropping the count to zero.
        let (t, second) = t.apply_event(Event::message_start(1));
        assert_eq!(commit_count(&second), Some(1));
        assert_eq!(t.transcript().committed_high_water(), 0);
    }

    // A single fold can turn MORE than one leading item terminal at once: here a
    // pending ToolCall is superseded by its result while a second call had
    // already settled behind it, so the fold that merges the first result frees a
    // batch. The emitted count covers all newly-committable leading items.
    #[test]
    fn one_fold_can_commit_a_batch_of_newly_terminal_items() {
        // Greeting + two tool calls in flight; the greeting commits, both calls
        // stay pending (the first blocks the second).
        let (t, _) = fold_and_commit(
            fresh(),
            Event::tool_call("t1", "read_file", serde_json::json!({"path": "a.rs"})),
        );
        let (t, blocked) = fold_and_commit(
            t,
            Event::tool_call("t2", "read_file", serde_json::json!({"path": "b.rs"})),
        );
        // The leading ToolCall (t1) is non-terminal, so nothing new commits.
        assert_eq!(blocked, None);
        // Resolve t2 first (behind the still-pending t1): still blocked by t1.
        let (t, still_blocked) = fold_and_commit(
            t,
            Event::tool_result("t2", "read_file", "ok", false, HashMap::new()),
        );
        assert_eq!(still_blocked, None);
        // Now resolve t1: t1's result AND t2's already-settled result both become
        // leading terminal items - ONE fold commits the batch of two.
        let (_t, batch) = fold_and_commit(
            t,
            Event::tool_result("t1", "read_file", "ok", false, HashMap::new()),
        );
        assert_eq!(batch, Some(2), "one fold committed both freed results");
    }

    // `newest_live_tool_name` (ADR-0049) is what the inline approval attaches
    // to. With two live ToolCalls it picks the NEWEST by position; and a
    // resolved (superseded → ToolResult) call is skipped so the block never
    // binds to a call that already has a result.
    #[test]
    fn newest_live_tool_name_picks_the_newest_live_call_and_skips_resolved_ones() {
        // Two live calls, neither resolved: the newest (t2) is the live one.
        let t = fold(
            fresh(),
            vec![
                Event::tool_call(
                    "t1",
                    "run_command",
                    serde_json::json!({"command": "echo one"}),
                ),
                Event::tool_call("t2", "web_fetch", serde_json::json!({"url": "https://x"})),
            ],
        );
        assert_eq!(t.newest_live_tool_name(), Some("web_fetch"));

        // Resolve the newer call (t2 supersedes to a ToolResult): the only
        // surviving live ToolCall is the older t1, so the attach falls to it -
        // a resolved call is never chosen.
        let t = fold(
            t,
            vec![Event::tool_result(
                "t2",
                "web_fetch",
                "ok",
                false,
                HashMap::new(),
            )],
        );
        assert_eq!(t.newest_live_tool_name(), Some("run_command"));

        // With no live ToolCall at all (t1 also resolved), it is None.
        let t = fold(
            t,
            vec![Event::tool_result(
                "t1",
                "run_command",
                "ok",
                false,
                HashMap::new(),
            )],
        );
        assert_eq!(t.newest_live_tool_name(), None);
    }

    // Negative case: a fold that only ADDS a still-pending leading ToolCall (with
    // nothing terminal ahead of it) emits no Commit at all.
    #[test]
    fn a_fold_that_only_adds_a_pending_tool_call_commits_nothing_new() {
        // Commit the greeting first (via the adapter stand-in).
        let (t, greeting) = fold_and_commit(fresh(), Event::run_started("r1"));
        assert_eq!(greeting, Some(1));
        // Now the only uncommitted item added is a live ToolCall: nothing new is
        // terminal, so no Commit is emitted.
        let (_t, none) = fold_and_commit(
            t,
            Event::tool_call("t1", "read_file", serde_json::json!({"path": "a.rs"})),
        );
        assert_eq!(none, None);
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
        use crate::view_model::SelectorRow;

        // Commit `/model` through the Screen: a Loading overlay opens and one
        // Command effect carries the activation generation to echo back.
        let t = press(fresh(), typed("/model"));
        let (t, effects) = t.handle_key(Key::Enter);
        let effects = sans_commit(effects);
        let generation = match effects.as_slice() {
            [Effect::Command { name, generation }] if name == "model" => *generation,
            other => panic!("expected one Command effect, got {other:?}"),
        };

        let rows = vec![SelectorRow::new("qwen", "qwen", None)];
        let (t, effects) = t.apply_event(Event::selector_ready(generation, rows.clone()));
        assert_eq!(effects, vec![]);
        assert_eq!(items(&t), vec![], "never a Transcript item");
        match t.composer().view().overlay {
            Some(OverlayView::Dialog {
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
        assert_eq!(
            sans_commit(effects),
            vec![],
            "no Cancel - the Composer consumed Escape"
        );
        assert!(t.composer().view().overlay.is_none());
        assert_eq!(t.status, Status::Running, "the Turn is untouched");
        // With the Composer emptied, Escape is refused and Cancellation fires.
        let (_t, effects) = t.handle_key(Key::Escape);
        assert_eq!(
            sans_commit(effects),
            vec![Effect::Agent(AgentCommand::Cancel)]
        );
    }

    // A refused key comes back BY VALUE and still reaches the arms below,
    // mid-draft included: refusal returns the key, it does not drop it. PageUp
    // no longer scrolls (ADR-0046), so it falls through with no effect - but the
    // draft stays untouched, proving the key was refused (not consumed as text).
    #[test]
    fn a_refused_key_reaches_the_arms_below_mid_draft() {
        let t = press(fresh(), typed("half a thought"));
        let (t, effects) = t.handle_key(Key::PageUp);
        assert_eq!(sans_commit(effects), vec![]);
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
        // The unknown-command info line commits on this exit (ADR-0046).
        assert_eq!(sans_commit(effects), vec![], "no Turn, no command effect");
        assert_eq!(items(&t), vec![info("unknown command: /nope")]);
        assert_eq!(t.composer().view().draft, "", "draft cleared");
    }

    // --- Cancellation and errors -------------------------------------------

    #[test]
    fn escape_while_running_no_modal_cancels() {
        let (t, _) = fresh().apply_event(Event::run_started("r1"));
        let (_t, effects) = t.handle_key(Key::Escape);
        assert_eq!(
            sans_commit(effects),
            vec![Effect::Agent(AgentCommand::Cancel)]
        );
    }

    #[test]
    fn escape_while_idle_does_nothing() {
        let (_t, effects) = fresh().handle_key(Key::Escape);
        assert_eq!(sans_commit(effects), vec![]);
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
        assert_eq!(sans_commit(effects), vec![Effect::FocusComposer]);
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
        let (t, effects) = fresh().info("resume: 2 turns replayed with drift");
        assert_eq!(items(&t), vec![info("resume: 2 turns replayed with drift")]);
        // The info line routes through the commit seam (ADR-0046): the greeting
        // and the new info line are both terminal, so the exit emits a Commit.
        assert_eq!(commit_count(&effects), Some(2));
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
        // The greeting was still uncommitted (the selector fill returned via
        // the Composer without a fold exit), so this no-op key commits it.
        assert_eq!(sans_commit(effects), vec![]);
    }

    // --- scroll keys are inert in the Screen (ADR-0046) --------------------
    //
    // Native scrollback owns history now: PageUp/PageDown and the mouse wheel no
    // longer emit a scroll effect from the Screen. They stay in [`Key`] for the
    // pre-agent Session Picker (its alt-screen list still navigates by them),
    // but the transcript fold produces nothing for them.

    #[test]
    fn page_and_wheel_keys_are_inert_idle_and_running() {
        for key in [Key::PageUp, Key::PageDown, Key::WheelUp, Key::WheelDown] {
            let (_t, effects) = fresh().handle_key(key.clone());
            assert_eq!(sans_commit(effects), vec![], "{key:?} idle is inert");

            let (t, _) = fresh().apply_event(Event::run_started("r1"));
            let (_t, effects) = t.handle_key(key.clone());
            assert_eq!(sans_commit(effects), vec![], "{key:?} running is inert");
        }
    }

    #[test]
    fn wheel_keys_swallowed_while_modal_open() {
        for key in [Key::WheelUp, Key::WheelDown] {
            assert_key_swallowed_while_modal_open(key);
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
        // The first fold on a fresh Screen commits the greeting (ADR-0046);
        // the toggle itself emits nothing.
        assert_eq!(sans_commit(effects), vec![]);

        let (t, effects) = t.handle_key(Key::ToggleThinking);
        assert!(!t.thinking_expanded);
        // The greeting is STILL uncommitted (the pure fold never advances the
        // mark - the adapter does, post-blit), so this fold re-emits the same
        // Commit; the toggle itself still emits nothing.
        assert_eq!(sans_commit(effects), vec![]);
    }

    #[test]
    fn modal_swallows_toggle_thinking() {
        assert_key_swallowed_while_modal_open(Key::ToggleThinking);
        // The flag must not have flipped; a fresh Screen starts collapsed.
        assert!(!fresh().thinking_expanded);
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
        // The first fold on a fresh Screen commits the greeting (ADR-0046).
        assert_eq!(sans_commit(effects), vec![]);

        let (t, effects) = t.handle_key(Key::ToggleTools);
        assert!(!t.tools_expanded);
        // Still uncommitted (the adapter advances the mark, not the fold), so the
        // greeting's Commit repeats; the toggle emits nothing of its own.
        assert_eq!(sans_commit(effects), vec![]);
    }

    #[test]
    fn modal_swallows_toggle_tools() {
        assert_key_swallowed_while_modal_open(Key::ToggleTools);
        // The flag must not have flipped; a fresh Screen starts collapsed.
        assert!(!fresh().tools_expanded);
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
