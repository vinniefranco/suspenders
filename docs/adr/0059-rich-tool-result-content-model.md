# Rich Tool Result content model

A Tool Result was a `String`. To let read_file hand an image or PDF to a Model
that accepts it (P3 D3, FULL multimodal), a Tool Result's content becomes a
canonical list of blocks - `ResultBlock` - carrying text or base64 media. The
common case is a single Text block, so every text tool is unchanged.

## The shapes

`ResultBlock` (in `content.rs`, the shared leaf both `tool` and `llm` name with
no cycle):

```
enum ResultBlock { Text { text }, Image { mime, data }, Document { mime, data } }
```

`data` is base64. `Modalities { image, pdf }` (also a `content.rs` leaf,
default all-false) is the input-modality fact a Model and a `ToolCtx` carry.

- `ContentBlock::ToolResult.content` is `Vec<ResultBlock>`.
  `tool_result(id, str, is_error)` still builds a single Text block, so every
  construction site compiles unchanged; `tool_result_blocks` is the media path.
  The free `result_blocks_text(&[ResultBlock])` is the single text projection
  (Text blocks concatenated, a media block rendered as a short `[image: <mime>]`
  placeholder), read everywhere the old `content: String` was read as a `&str` -
  the UI, the loop-detector, summarize, the Session-Log projection, the
  transform's orphan path.
- `ToolOutput { blocks }` (in `tool.rs`) is what a Tool may produce.
  `From<String>` wraps a text tool's return in one Text block.

## run_rich, not run

`Tool::run` stays `Result<String, String>` - all ~10 text tools are untouched.
A new default `Tool::run_rich` delegates to `run` and wraps the `String` in one
Text block; only read_file overrides it (P3 3b), reading an image or PDF and
gating it on `ToolCtx::input_modalities`. The Registry dispatches through
`run_rich`, so there is one dispatch method and the media path is a single
override, not a fork of the whole tool contract. `run`-not-a-new-required-method
keeps the authoring contract a small model's tool authors already know.

## read_file's multimodal dispatch (P3 3b)

read_file's `run` still returns the text-branch `String` (a window of lines from
`start_line`), so a plain text read and the default `run_rich` path are
byte-identical to before. read_file overrides `run_rich` to dispatch on a
detected file kind (`tools::read_file::detect`, an extension + magic-byte port
of qwen `fileUtils.ts` `detectFileType`, confined - no `infer` crate):

- **text** - one Text block, the `start_line` window (unchanged).
- **svg** - one Text block; SVG is read AS TEXT (qwen returns `'svg'` and reads
  it with the text reader), capped at 1MB (`SVG_MAX_SIZE_BYTES`) with the
  verbatim skip message past the cap.
- **notebook** (`.ipynb`) - one Text block, the structured cell render
  (`tools::read_file::notebook`, a VERBATIM port of qwen `utils/notebook.ts`:
  the `Jupyter Notebook (<lang>, <N> cells)` header, `--- Code/Markdown/Raw Cell
  <id>[<count>] ---` markers, ANSI-stripped outputs, the per-output 10k cut and
  the whole-notebook 100k cell-budget truncation markers). The shared data model
  (`Notebook`/`Cell`/`Source` as an untagged `string | string[]`) lives in the
  crate-root `notebook` leaf so a future notebook_edit (P3 3c) reuses it with no
  tool-graph cycle.
- **image** - a `ResultBlock::Image` (base64) when `ctx.input_modalities.image`,
  else the read-time degrade: the VERBATIM `unsupported_modality_placeholder`
  as a Text block, never encoding the bytes. A base64 payload past 9.9MB is the
  verbatim `File exceeds the 10MB data URI limit after base64 encoding (...MB
  encoded).` error (qwen's margin under 10MB).
- **pdf** - a `ResultBlock::Document` (base64, `application/pdf`) when
  `ctx.input_modalities.pdf` AND no `pages`; otherwise text extraction via the
  `pdftotext` subprocess (`tools::read_file::pdf`, a port of qwen `utils/pdf.ts`
  reusing the run_command spawn+timeout pattern: 30s timeout, the on-disk
  `PDF_EXTRACTION_MAX_MB=100` guard, the 100k output cut, and the verbatim
  missing-binary / password / corrupt / timeout error wording). A failed
  extraction is the verbatim `[Cannot extract text from PDF: "<name>". <error>]`.

The pdftotext-text-vs-native-PDF-block decision is read_file's: `pages` forces
text (even on a PDF-capable Model), and a Model without PDF support always takes
the text path. Windowing params on a non-windowing kind are rejected -
`start_line`/`pages` on a `.ipynb` (qwen's verbatim notebook messages),
`start_line` on an image/PDF, and `pages` on a non-PDF.

`pages` joins read_file's schema (qwen's `pages` param, faithfully ported:
1-indexed, max 20 pages, open-ended `3-` rejected); `start_line` stays
Suspenders' windowing param (qwen's `offset`/`limit` equivalent) for the text
branch. `base64` (the crate) encodes image/PDF bytes; no decoding crate is
pulled in.

## Shaping caps Text only

`tools::shaping::shape` now takes and returns `Vec<ResultBlock>`: it folds the
Text blocks, cuts them with the existing char-slice logic (the resume-marker
rules for read_file and run_command unchanged), and passes media through
uncapped. A text-only result is byte-identical to the old `&str` shaping. The
text-editing Middleware (condense, diff, run_command) read and rewrite the text
through `TokenResult::text_of`/`set_text`, and media rides the fold untouched.

## Two degrade points, one verbatim placeholder

Media reaches an Anthropic Model whose Catalog `input_modalities` accept it; the
Anthropic visitor builds the wire array (`text` / `image` source.base64 /
`document` source.base64) - the explicit visitor, never the `ResultBlock` derive
(whose `{type:"image",data}` is the internal, not the wire, shape). OpenAI's
tool-role messages carry no media (out of scope for P3 - Anthropic is the media
target), so a media block there degrades to a placeholder.

When a Model lacks a modality, the media degrades to the VERBATIM qwen-code
unsupported-modality message (qwen v0.16.0 `fileUtils.ts`):

```
[Unsupported <modality> file: "<displayName>". This model does not support
<modality> input. The read_file tool cannot process this type of file either.
To handle this file, try using skills if applicable, or any tools installed at
system wide, or let the user know you cannot process this type of file.]
```

Two points fire it:

- **Read time** (P3 3b): read_file checks `ToolCtx::input_modalities` and never
  emits media the captured Model cannot read.
- **Wire-build time** (this phase): `transform::degrade_unsupported_media`, run
  after `normalize_request` in the Dispatcher, is the cross-Model-history safety
  net - a request may carry media a previous, capable Model produced, so it is
  degraded for the Model actually receiving the request.

The placeholder lives once in `content.rs` (`unsupported_modality_placeholder`),
shared by the degrade pass and the OpenAI visitor.

## Posture

Anthropic-primary, OpenAI-degrade. The media target is the Anthropic Messages
wire; the OpenAI dialect degrades media to text. MCP results still collapse to a
single Text block (ADR-0056). The whole change is backward-compatible: a text
result is one Text block, so the Session Log round-trips, and every existing
tool / result / compaction / transform test keeps passing.
