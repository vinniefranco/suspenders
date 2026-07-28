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
/// sniffing the line's text. Like [`crate::ui::transcript::LineStyle`], the
/// tone is the semantic fact; the terminal color mapping lives in
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
