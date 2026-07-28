# Presentment speaks a semantic display vocabulary, not raw markup

A Presenter's Presentment callback (ADR-0007) needs to show rich output - a
syntax-highlighted diff, not a one-line summary - without breaking ADR-0001's
boundary: the Screen core (and the Transcript store it owns, ADR-0034) is pure
and semantic, and terminal markup exists only in the adapter. So the core
carries **structure and semantics** for rich items, and the adapter renders
them. Two rules hold the line:

1. **The core never names a terminal color.** A Presenter produces diff
   STRUCTURE - each line's `DiffSide` (`Added`, `Removed`, `Context`), the diff's
   language, and its hunks - and one adapter module owns the single mapping from
   a side to a terminal color, the same move as the Steering Vocabulary. The side
   is structure, not a display style: the adapter reads it for the marker glyph,
   the tint, AND the two-pass highlighting split (below).
2. **Lexical color is the adapter's, layered over the semantic tag.** Syntax
   highlighting is not semantic - it is a token's lexical class, sourced from the
   active syntect theme, not from a Theme's semantic slots. It therefore lives
   entirely in the adapter, layered as a foreground over the side's meaning. The
   same machinery highlights assistant markdown code fences.

## The diff is a first-class Transcript item

The Transcript's rich item is `Diff { title, lang, hunks, elided }` - a
purpose-built item, not a generic block of pre-styled lines. `lang` is the
source language derived from the file path (`Option<String>`, `None` when
unknown); `hunks` is `Vec<DiffHunk>` where `DiffHunk { header: Option<String>,
lines: Vec<DiffLine> }` and `DiffLine { side: DiffSide, text: String }` carries
RAW code (no `+`/`-` marker); `elided` is the count of code lines the display cap
cut. The Presenter decides WHAT to show (this is a diff of a JS file, these lines
changed, this much elided) and the adapter decides HOW (the `+`/`-` marker glyph,
the added/removed background tint, and the syntect foreground). The `@@ … @@`
header and the `… N more lines` tail are adapter CHROME (muted-italic, no marker
or tint): the header rides as `DiffHunk.header` structure and the tail as
`elided`, so neither is a styled line. The two color sources stay cleanly split:
semantic *meaning* becomes a background tint owned by the Theme's slots; lexical
*token* becomes a foreground owned by the syntect theme; neither contaminates the
other.

## Multi-line syntax highlighting is hunk-coherent (two-pass)

Syntect carries parse state across a slice, so a multi-line construct - a block
comment, a raw string - only colors correctly when its lines are highlighted
together in order. A diff interleaves two file versions, so the adapter
highlights each hunk TWICE: the AFTER image (context + added lines, in file
order) as one slice, and the BEFORE image (context + removed lines) as another.
Each line then draws its foreground from the image it belongs to - an added line
from the after pass, a removed line from the before, a context line from the
after pass while advancing both cursors. A created file is one all-added hunk =
the whole file, so its `/** … */` JSDoc colors as a comment across EVERY line,
not just the first. Per-line-independent highlighting is rejected: it would
color only a comment's opening line.

The scheme has one inherent limit, accepted: a construct a single hunk STRADDLES
via a removed opener and an added closer (`/*` removed, `*/` added) lives in two
different images and cannot color coherently. The common cases - whole created
files, and comments that survive an edit as context - are coherent.

## Considered Options

- **A generic titled block of semantically-styled lines
  (`Block { title, lines }`).** The original decision here: one vocabulary, no
  per-Presenter item type. Retired. In practice the diff was its only user for
  the life of the codebase, and forcing a diff through a flat list of
  single-styled lines was lossy - the marker had to be baked into the text and
  the language discarded, which made syntax highlighting impossible. A diff has
  structure (language, hunks, per-token lexical color) a flat styled-line list
  cannot express. The generic block was speculative generality; it is inlined
  into its one true user.
- **Presenter-owned rendering (a render callback returning terminal markup).**
  Maximum display freedom, rejected: markup leaks into every Presenter, the pure
  core holds opaque payloads it cannot inspect or test, and ADR-0001's boundary
  dies quietly.
- **Per-token RGB spans baked into the core's line type.** Rejected: it drags a
  concrete syntect color into the pure core (ADR-0019), and re-derives in the
  core what the adapter already does for markdown. Lexical color is layered in
  the adapter instead.

## Consequences

Presenters express structure and semantics; the adapter renders. A future
Presenter with a genuinely new rich shape (a table, an interactive element) adds
its own first-class item at this chokepoint and the adapter grows a renderer for
it - the same move the `Diff` item makes. This is a deliberate reversal of the
original "one generic item for all Presenters": one first-class item per rich
shape is clearer than one generic item stretched thin, given how few rich shapes
exist and how much structure each needs. The fold/collapse machinery keys on the
semantic `has_foldable_body`/`fold_title` predicates, not on a concrete variant,
so a new rich item folds without touching the fold rule.
