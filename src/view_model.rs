//! The Presentment value vocabulary (CONTEXT.md): the pure value types the
//! functional core hands to the display so it can render them. Presentment
//! decides WHAT a Transcript item is - its marker [`Tone`], a selector's
//! [`SelectorRow`]s - while the terminal drawing (a Theme slot per tone, a
//! popup row per selector row) is a separate, later concern that lives in
//! `ui`. Keeping these types in a dependency-free leaf lets the core produce
//! them (an [`crate::event::Event`] carries a Tone or a row list) and the `ui`
//! render them without the core ever depending on the rendering layer.

/// The semantic TONE of a harness-authored marker (ADR-0040): names WHO acted
/// and in what spirit, so the adapter tints the marker plane without ever
/// sniffing the line's text. Like a diff's [`DiffSide`], the tone is the
/// semantic fact; the terminal color mapping lives in
/// `ui/components` (a Theme slot per tone). Stamped at the firing site (the
/// Event that voiced the marker), carried into
/// [`crate::ui::transcript::Transcript::marker`]; the store never classifies
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tone {
    /// A budget mechanic tidying the Conversation: Eviction, Compaction,
    /// Result-Cap cuts. Not a Governor's judgment - neutral gray.
    Housekeeping,
    /// A Governor helping the model along: a Nudge, a plan/anchor refresh, a
    /// Recovery Run. Warm amber (chosen away from error-red).
    Aid,
    /// A Governor limiting the model: tool-narrowing, the Endgame's run-close
    /// schedule. Cool blue (chosen away from success-green).
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
    /// cursor stop that Enter refuses: landing on a group's trailing note
    /// anchors the popup window - which ends at the highlight - so the whole
    /// group above it scrolls into view.
    Note,
    /// A pickable row - the only role Enter resolves.
    Member,
    /// A greyed, unpickable member (a credential-less built-in's Catalog
    /// model): hidden at the empty filter, revealed when its label matches,
    /// at most a per-group reveal cap (see
    /// [`crate::ui::selector::COLLAPSED_REVEAL_CAP`]). Not a stop.
    Collapsed,
}

/// One row in a [`crate::ui::selector::Selector`]: `value` is what a
/// [`crate::ui::selector::SelectorOutcome::Select`] returns, `label` is shown
/// and filtered on, and `hint` is optional secondary text (a command's help, a
/// "(current)" marker, a note's terse reason) that never affects filtering.
/// `role` places the row in its group and decides stop-ness and pickability -
/// see [`RowRole`].
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
/// noise) and the run of tagged code lines. Structure the Presenter carries and
/// the adapter renders - the header is a location string, the lines are RAW code
/// tagged by [`DiffSide`].
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
///   summary a extension's `present` may replace; `key_arg` is the salient input
///   arg (path/command/pattern) carried over from the paired call so the merged
///   line reads `name  <key_arg> · <result>`, `None` for an unpaired result.
/// * `Diff { title, lang, hunks, elided }` - a first-class diff (ADR-0008): a
///   title, the source language (derived from the file path, `None` when
///   unknown), the tagged hunks, and the count of lines elided by the display
///   cap. The adapter renders the marker glyph, the added/removed background
///   tint, and the syntect foreground.
/// * `Info { text }` - `{:info, text}`: adapter-authored news with no marker
///   plane (the greeting, launch notices, the extension-failure line).
/// * `Marker { text, tone }` - a harness-authored line in the tinted marker
///   plane (ADR-0040): eviction, compaction, Governor Interventions, Steering.
///   The [`Tone`] tints it in the adapter; the store only carries the fact.
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
        /// (e.g. governor-injected) - the line falls back to `name → result`.
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
    pub fn fold_title(&self) -> Option<&str> {
        match self {
            TranscriptItem::Diff { title, .. } => Some(title),
            _ => None,
        }
    }
}
