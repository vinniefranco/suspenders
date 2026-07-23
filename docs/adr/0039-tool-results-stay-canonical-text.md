# Tool Results stay canonical text (TOON rejected)

A session attempted to render structured Tool Results as
[TOON](https://toonformat.dev/) (Token-Oriented Object Notation) to reduce
tokens for small local models: an `OutputFormat` fixed fact on the Session,
threaded through `ToolCtx`, with grep and list_files building a `Serialize`
value rendered as either the canonical text or TOON. The work was reviewed and
rejected; this ADR records why, so the experiment is not repeated.

## Decision

**Tool Results keep their canonical text forms.** TOON's measured token
savings are against JSON, which Suspenders never emitted. The existing text
forms are already denser than TOON for the tools that were converted:

- grep: `path:12: text` versus a TOON header line, tab rows, plus
  `truncated:`/`literal_fallback:`/`status:` scalar lines on every result.
- list_files: a `/` suffix marks a directory in one character; TOON spends an
  `is_dir` column per row plus header and flag lines.

On both tools TOON was a small token *regression*, so the stated objective
could not be met by this design.

## What TOON genuinely offered

Not density but legibility: an explicit `[N]` array length and named flags
make truncation self-detecting for a small model that mis-reads silently cut
text. That property conflicts with the shaping layer's char-slice cut (a
sliced TOON string claims more rows than it shows), which forced size
management back into the tools - contradicting the invariant that size is not
a tool concern (`tools::shaping` cuts every result).

## Revisit if

An A/B on a real local model shows a *task-success* win (not a token count)
from length-explicit output. Any future attempt must also preserve the
"there was more" signal when rows are dropped to fit the Result Cap, and take
the encoder dependency without its CLI feature set.
