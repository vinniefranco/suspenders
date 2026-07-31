# Tool Results are a canonical block list (TOON rejected)

A session attempted to render structured Tool Results as
[TOON](https://toonformat.dev/) (Token-Oriented Object Notation) to reduce
tokens for small local models: an `OutputFormat` fixed fact on the Session,
threaded through `ToolCtx`, with grep_search and list_directory building a `Serialize`
value rendered as either the canonical text or TOON. The work was reviewed and
rejected; this ADR records why, so the experiment is not repeated.

## Decision

**Tool Results keep their canonical text forms.** TOON's measured token
savings are against JSON, which Suspenders never emitted. The existing text
forms are already denser than TOON for the tools that were converted:

- grep_search: `path:12: text` versus a TOON header line, tab rows, plus
  `truncated:`/`literal_fallback:`/`status:` scalar lines on every result.
- list_directory: a `/` suffix marks a directory in one character; TOON spends an
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

## Amendment (ADR-0059): a canonical block list, common case single Text

A Tool Result's content is no longer a bare `String` - it is a canonical
`Vec<ResultBlock>` (`Text` / `Image` / `Document`), ADR-0059. The common case
is a single `Text` block, so the "canonical text" thesis this ADR defends
still holds for every text tool: their result is one Text block and reads,
caps, and logs exactly as before. What changed:

- **Media reaches the wire when the Model supports it.** An `Image` or PDF
  `Document` block rides to an Anthropic Model whose Catalog `input_modalities`
  accept it; otherwise it degrades to the verbatim unsupported-modality
  placeholder (read-time in read_file, and a wire-build-time safety-net pass
  for cross-Model history). The TOON rejection stands - this is not a
  re-encoding of text, it is first-class media.
- **Shaping caps Text only.** `tools::shaping::shape` now folds the Text
  blocks, cuts them as before, and passes media through uncapped - "size is
  not a tool concern" still holds, and the char-slice cut is untouched.
- **MCP still collapses to text.** An MCP tool's result stays a single Text
  block (ADR-0056); only read_file produces media (P3 3b).
