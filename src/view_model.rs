//! The Presentment value vocabulary (CONTEXT.md): the pure value types the
//! functional core hands to the display so it can render them. Presentment
//! decides WHAT a Transcript item is - its marker [`Tone`], a selector's
//! [`SelectorRow`]s - while the terminal drawing (a Theme slot per tone, a
//! popup row per selector row) is a separate, later concern that lives in
//! `ui`. Keeping these types in a dependency-free leaf lets the core produce
//! them (an [`crate::event::Event`] carries a Tone or a row list) and the `ui`
//! render them without the core ever depending on the rendering layer.

/// The semantic TONE of a harness-authored marker: names WHO acted and in
/// what spirit, so the adapter styles the marker without ever sniffing the
/// line's text. Like a diff's [`DiffSide`], the tone is the semantic fact;
/// the glyph/style mapping lives in `ui/components` (qwen's status-message
/// roles). Stamped at the firing site (the Event that voiced the marker),
/// carried into [`crate::ui::transcript::Transcript::marker`]; the store
/// never classifies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tone {
    /// A budget mechanic tidying the Conversation: Compaction, Result-Cap
    /// cuts. Routine tidying, not a judgment - neutral gray.
    Housekeeping,
    /// A guard limiting the model: the loop-detector's run-close. Drawn as
    /// the `△` warning status.
    Constrain,
    /// The user's own voice reaching a running Run (the pending-Steering
    /// marker): the prompt color, never the harness plane.
    Steering,
    /// A marker with no assigned tone - the default a plain `push`ed marker or
    /// an older Session's line reads as. Muted, like an Info line.
    #[default]
    Plain,
}

/// The role a [`SelectorRow`] plays in its group - it decides stop-ness and
/// pickability (see [`SelectorRow::is_stop`] and [`SelectorRow::pickable`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowRole {
    /// Starts a group (a Provider's name): the rows after it, until the next
    /// header, belong to it. Not a stop, never picked.
    Header,
    /// An annotation that travels with its group's header (an "unavailable"
    /// note); on its own with no header before it (a broken theme in the
    /// `/theme` list), it is its own group and filters on its own label. A
    /// cursor STOP that Enter refuses (so its terse reason is reachable), but
    /// never a pick.
    Note,
    /// A pickable row - the only role Enter resolves.
    Member,
    /// A greyed, unpickable member (a credential-less built-in's Catalog
    /// model): shown greyed in the numbered `›` dialog (ADR-0051 System A), an
    /// editable model filter reveals it when its label matches. Not a stop -
    /// nav skips it, Enter refuses it.
    Collapsed,
}

/// One row in a committed command's numbered `›` dialog (ADR-0051 System A,
/// [`crate::ui::selection`]): `value` is what a selection resolves to, `label`
/// is shown and filtered on, and `hint` is optional secondary text (a
/// "(current)" marker, a note's terse reason) that never affects filtering.
/// `role` places the row in its group and decides stop-ness (nav) and
/// pickability (Enter) - see [`RowRole`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorRow {
    pub value: String,
    pub label: String,
    pub hint: Option<String>,
    pub role: RowRole,
}

impl SelectorRow {
    /// A pickable row from a value, label, and optional hint.
    pub fn new(value: impl Into<String>, label: impl Into<String>, hint: Option<String>) -> Self {
        SelectorRow {
            value: value.into(),
            label: label.into(),
            hint,
            role: RowRole::Member,
        }
    }

    /// A group-starting header (a Provider's name): shown (and it shows its
    /// group) but never picked, never a stop. No hint of its own - a group's
    /// annotations belong to its note.
    pub fn header(label: impl Into<String>) -> Self {
        SelectorRow::unpickable(RowRole::Header, label, None)
    }

    /// A group's annotation (an "unavailable" note whose hint is the terse
    /// reason): a cursor stop that Enter refuses.
    pub fn note(label: impl Into<String>, hint: Option<String>) -> Self {
        SelectorRow::unpickable(RowRole::Note, label, hint)
    }

    /// A greyed member hidden until the filter matches its label (a
    /// credential-less built-in's Catalog model): never picked, never a stop.
    pub fn collapsed(label: impl Into<String>) -> Self {
        SelectorRow::unpickable(RowRole::Collapsed, label, None)
    }

    /// Whether Up/Down/Wheel may land here: members and notes.
    pub fn is_stop(&self) -> bool {
        matches!(self.role, RowRole::Member | RowRole::Note)
    }

    /// Whether Enter may resolve here: members only.
    pub fn pickable(&self) -> bool {
        self.role == RowRole::Member
    }

    // The shared base of every row Enter cannot pick: no value, role and
    // hint per constructor.
    fn unpickable(role: RowRole, label: impl Into<String>, hint: Option<String>) -> Self {
        SelectorRow {
            value: String::new(),
            label: label.into(),
            hint,
            role,
        }
    }
}

/// Which side of a diff one line sits on (ADR-0008): diff STRUCTURE, not a
/// display style. Mirrors baud's `{:eq | :del | :ins}` tag. The adapter maps a
/// side to its marker glyph (`+`/`-`/` `), its background tint, and its fallback
/// foreground; the two-pass syntect highlighting keys on it (an added or context
/// line belongs to the after-image, a removed or context line to the before).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffSide {
    /// An added line (a `+` line in a diff).
    Added,
    /// A removed line (a `-` line in a diff).
    Removed,
    /// A context line (unchanged, shown for orientation on both sides).
    Context,
}

/// One code line inside a [`DiffHunk`] (ADR-0008): the [`DiffSide`] it sits on
/// and its RAW code text - no `+`/`-` marker prefix (the adapter adds it), so
/// the same text also feeds the syntect highlighter. Mirrors baud's
/// `{tag, text}` line tuple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub side: DiffSide,
    pub text: String,
}

impl DiffLine {
    /// A diff line from a side and its raw text.
    pub fn new(side: DiffSide, text: impl Into<String>) -> Self {
        DiffLine {
            side,
            text: text.into(),
        }
    }
}

/// One hunk of a [`TranscriptItem::Diff`] (ADR-0008): an optional unified-diff
/// header (`@@ -a,b +c,d @@`, `None` for a created file where the header is
/// noise) and the run of tagged code lines. Structure the diff Artifact carries
/// and the adapter renders - the header is a location string, the lines are RAW
/// code tagged by [`DiffSide`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    /// The `@@ … @@` unified-diff header, or `None` for a created file.
    pub header: Option<String>,
    /// The hunk's tagged code lines, in display order.
    pub lines: Vec<DiffLine>,
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
///   summary the Transcript store may swap for a richer item when the Tool
///   Result carries an Artifact; `key_arg` is the salient input
///   arg (path/command/pattern) carried over from the paired call so the merged
///   line reads `name  <key_arg> · <result>`, `None` for an unpaired result.
/// * `Diff { title, lang, hunks, elided }` - a first-class diff (ADR-0008): a
///   title, the source language (derived from the file path, `None` when
///   unknown), the tagged hunks, and the count of lines elided by the display
///   cap. The adapter renders the marker glyph, the added/removed background
///   tint, and the syntect foreground.
/// * `Todo { items }` - a first-class task list (ADR-0048): the model's
///   `todo_write` items in order, the SAME [`crate::plan::TodoItem`] vocabulary
///   the Run-loop's Plan fold reads. The Transcript store swaps a successful
///   `todo_write` Tool Result for this item when the result carries the `todos`
///   Artifact the tool attached, so the committed render
///   draws the circle list (`○ ◐ ●`) instead of the raw JSON args. Pure - the
///   glyph/colour treatment lives in `ui/components` (ADR-0019).
/// * `Header { title, version, model, cwd, tip }` - the startup banner (qwen
///   `AppHeader`): the ASCII wordmark logo + a bordered info panel (title +
///   version, the scoped model id with a `(/model to change)` hint, the
///   tilde-abbreviated working directory) and the `Tips:` line below it. The
///   adapter draws the logo (theme accent), the single-border box, and the tip;
///   the core carries only the facts. Recorded ONCE, as the first item a fresh
///   Screen opens with.
/// * `Info { text }` - `{:info, text}`: adapter-authored news with no marker
///   plane (launch notices, the fail-open report line).
/// * `Marker { text, tone }` - a harness-authored status line: compaction,
///   result-cap cuts, the loop-detector close, Steering. The [`Tone`] picks
///   its glyph and style in the adapter; the store only carries the fact.
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
        /// (e.g. a Voice-authored answer to an orphaned Tool Call) - the line
        /// falls back to `name → result`.
        key_arg: Option<String>,
    },
    Diff {
        title: String,
        /// The source language token (a file extension like `rs`/`js`/`json`),
        /// or `None` when the path has no extension the adapter can resolve. The
        /// core never names a syntect syntax - it carries the language fact and
        /// the adapter resolves it (ADR-0019).
        lang: Option<String>,
        hunks: Vec<DiffHunk>,
        /// Lines the display cap elided, rendered as a muted `… N more lines`
        /// tail; `0` when nothing was cut.
        elided: usize,
    },
    /// A first-class task list (ADR-0048): the model's `todo_write` items in
    /// order, held as the pure [`crate::plan::TodoItem`] vocabulary. The
    /// Transcript store emits this in place of a successful `todo_write` Tool
    /// Result carrying the `todos` Artifact; the adapter draws the circle list.
    Todo {
        items: Vec<crate::plan::TodoItem>,
    },
    /// The startup banner (qwen `AppHeader` = `Header` + `Tips`): the ASCII
    /// wordmark logo the adapter draws in the theme accent, a single-border info
    /// panel, and a `Tips:` line below. The core carries only the facts; every
    /// glyph, colour, border, and the width gate that hides the logo on a narrow
    /// terminal live in the adapter (ADR-0019). Recorded once, as the first item
    /// a fresh Screen opens with.
    Header {
        /// The brand title (`suspenders`), shown bold in the accent colour with
        /// the `>_` prompt glyph, followed by ` (v<version>)` in secondary.
        title: String,
        /// The crate version (`CARGO_PKG_VERSION`), rendered as ` (v…)`.
        version: String,
        /// The active Model's scoped id (`provider/model-id`), shown in
        /// secondary with a ` (/model to change)` hint when it fits.
        model: String,
        /// The working directory, tilde-abbreviated and shortened to fit.
        cwd: String,
        /// The startup tip shown on the `Tips:` line (picked deterministically -
        /// the pure core has no RNG/clock).
        tip: String,
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
    /// Whether this item has a body that collapses under the global tools
    /// toggle (Ctrl-O), or `false` if it always renders in full.
    ///
    /// This is the SEMANTIC collapse predicate (Stage 2 review C2): the view's
    /// fold keys on `has_foldable_body()`, not on a structural
    /// `matches!(item, Diff)`, so the merge is free to choose an item's shape
    /// without re-implementing the fold rule. Today only a [`Diff`] with a
    /// non-empty hunk folds; a merged one-line `ToolResult` has no body, so it
    /// never collapses. Stays pure - inspects the pure-core structure, never a
    /// ratatui type (ADR-0019).
    ///
    /// [`Diff`]: TranscriptItem::Diff
    pub fn has_foldable_body(&self) -> bool {
        match self {
            TranscriptItem::Diff { hunks, .. } => hunks.iter().any(|hunk| !hunk.lines.is_empty()),
            _ => false,
        }
    }

    /// The title an item collapses TO under the global tools toggle (Ctrl-O):
    /// the one-liner the view shows in place of the folded body. Kept beside
    /// [`has_foldable_body`] so the collapse rule - predicate AND title - lives
    /// entirely in the pure core (Stage 2 review C2 / S1): the view composes the
    /// collapsed line from this accessor without matching on `Diff`, so a future
    /// non-Diff foldable item collapses the same way. Today only a [`Diff`] has
    /// a fold title.
    ///
    /// [`has_foldable_body`]: TranscriptItem::has_foldable_body
    /// [`Diff`]: TranscriptItem::Diff
    #[cfg(test)]
    pub fn fold_title(&self) -> Option<&str> {
        match self {
            TranscriptItem::Diff { title, .. } => Some(title),
            _ => None,
        }
    }
}
