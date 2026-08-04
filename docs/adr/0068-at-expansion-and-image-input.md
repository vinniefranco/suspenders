# At Expansion: user image input by porting qwen's `@`-mention pipeline

Suspenders advertised drag-and-drop / clipboard images but never had them: the
Composer is text-only, the user-message content model has no image variant, and
`@path` was ported as *completion only* (`useAtCompletion` → `file_search.rs`) -
the mention is inserted as plain text and left for the model to `read_file`.
qwen-code does something different and more capable, and per the governing frame
(suspenders is a qwen-code port; qwen's architecture is authoritative) we adopt
it verbatim rather than invent our own attachment model.

Source of truth: qwen-code v0.21.4 (`/home/vinnie/Sandbox/qwen-code-v0.21.4`),
cross-checked against v0.16.0 for the unminified `atCommandProcessor`.

## What qwen actually does

There is one spine: the `@<path>` mention. Every source of media collapses onto
it.

- **Dragged or pasted file path** → the text buffer detects a valid path
  (`isValidPath(unescapePath(...))`) and rewrites the pasted text to `@<path> `
  in place (`text-buffer.ts`).
- **Clipboard image** (`Ctrl+V` with image bytes, no path) → `clipboardHasImage()`
  then `saveClipboardImage()` writes the bytes to a temp file under the global
  temp dir (Linux: `wl-paste`/`xclip`; the WSL2 BMP→PNG path via PIL), stages it
  as an "attachment" chip, and at submit converts it to `@<relpath-of-tempfile>`
  prepended to the message (`InputPrompt.tsx`). So clipboard bytes rejoin the
  same `@`-mention spine rather than being a second content path.
- **At submit**, `atCommandProcessor.ts` parses every `@<path>`, resolves and
  reads each one via `read_many_files`, and builds a multimodal `PartListUnion`:
  text files inline as text, images/PDFs as base64 `inlineData` parts on the
  user turn. It also emits a per-file "Read File"/"Read Directory" tool card so
  the user sees what was pulled in.

The confinement rule falls out of `atCommandProcessor`: a `@path` must resolve
inside the workspace, **with one exception - the global temp dir** (that is how
the clipboard temp file is allowed back in). git-ignored / qwen-ignored paths are
skipped and reported; a lone `@` is treated as text; an unresolved path is put
back verbatim.

**Confinement DIVERGENCE from qwen (deliberate).** qwen skips an out-of-workspace
`@path`. Suspenders does NOT: an At Mention is USER input, not a model tool call,
so a user who types their OWN absolute path is honored regardless of the Project
Root (a real user complained about `@/home/.../background-meme1.jpg` being
"Skipped (outside project root)"). The Project-Root confinement exists to bound
the MODEL's tools, not the user's explicit mentions - the user could paste the
file's contents themselves anyway. Concretely (`at_expansion::confine`): an
ABSOLUTE `@path` resolves as-is (lexically normalized, honored wherever it
points); a RELATIVE `@path` still resolves against the Project Root (the
completion convention) and is refused if it climbs out. A non-existent absolute
path is still skipped (the downstream not-found `stat`). `read_many_files` trusts
the At-Expansion-vetted absolute spec and does not re-confine.

## Decision

Port that pipeline. In suspenders' domain language (CONTEXT.md):

- **At Mention** - the `@<path>` in the draft (exists today as completion).
- **At Expansion** - the submit-time pass (qwen's `atCommandProcessor`) that
  reads each At Mention and turns it into content on the user Message: text
  inlines as text, image/PDF becomes a first-class `Image`/`Document` content
  block (base64), reusing read_file's encoder and mime/magic-byte detection.
  This is the spine suspenders never ported - the reason a dragged image is
  inert today.
- **Attachment** - narrowly qwen's clipboard-image staging: bytes written to a
  temp file and converted to an At Mention of that temp file at submit. A
  dragged/pasted path is NOT an Attachment; it is an At Mention directly.

Media reaches the model as **first-class user content** - an `Image`/`Document`
`ContentBlock` on the user `Message`, emitted by both wire adapters
(`anthropic_messages` `image`/`document` `source.base64`; the OpenAI
`image_url`/data-URL shape) - never a synthesized Tool Call and never a
fabricated Tool Result.

**User-attached media is sent to the wire UNCONDITIONALLY - never degraded.** A
user who explicitly attaches an image via `@path` has authoritative intent: the
attachment rides to the model even on a Model whose catalog says it accepts text
only (a custom/local model with no models.dev entry defaults all-modalities-false).
The client must NOT preemptively strip the attachment and lie "this model does not
support image input" - let the model/API answer honestly. So `llm::transform`
degrades ONLY autonomous **Tool Result** media (ADR-0059's cross-Model-history
safety net, e.g. a `read_file` image produced on a prior capable Model);
first-class user-message `Image`/`Document` blocks pass through untouched.

(The catalog was stale when this feature shipped - `qwen3.6-27b` resolved
`image:false` - which is what surfaced the degrade-and-lie behavior on a supported
model. The catalog has since been regenerated and correctly resolves `image:true`;
but the unconditional-send rule stands on its own, since custom/local models have
no catalog entry at all.)

## The deliberate deviation: At Expansion is uncapped (verbatim qwen)

qwen's `atCommandProcessor` inlines **every** `@path` at submit - a
`@src/foo.rs` mention is read and inlined *then*, not left for the model to
`read_file`. Ported verbatim, this bypasses the Result Cap and read_file's
line-windowing (ADR-0045, ADR-0059): a `@big.log` dumps the whole file, uncut,
into the user turn - exactly the stale-context bloat suspenders otherwise guards
against for small local models.

We accept that. The governing frame wins over ADR-0045's discipline here by
explicit choice: `@path` now means "inline this file now," matching qwen, not
"hint the model to read it." The Result Cap still governs actual Tool Results
(a model-driven `read_file` is unchanged); it simply does not reach across to
user-authored At Expansion, because that content is the user's, like any pasted
text. A capped variant was considered and rejected as a divergence from qwen.

## What this requires building (absent today)

1. **Content model**: an `Image`/`Document` variant on `ContentBlock` (user/
   assistant content), plus wire arms in both adapters. Today only `ResultBlock`
   (tool-result content) carries media.
2. **A structured user prompt**: `AgentCommand::Submit`/`Steer` carry a `String`
   end-to-end (Composer → Agent → `start_run` → `add_user_text`). They must
   carry a content-block list so At Expansion's media survives to the
   Conversation. Steering is symmetric - a mid-Run image is a user turn too.
3. **`read_many_files` equivalent**: a batch reader returning mixed text+media
   parts, reusing read_file's per-file readers. suspenders has only the single
   read_file.
4. **A global temp dir** for clipboard staging, plus the At-Expansion confinement
   exception that admits it.
5. **Capture layer**: enable **bracketed paste** (crossterm `EnableBracketedPaste`
   - not enabled today) and route a paste event; detect a valid path and rewrite
   to `@path`; a `Ctrl+V` clipboard-image handler behind the platform clipboard
   tools. Mouse capture is already on, but terminals deliver a file drop as a
   pasted path, not a mouse event - there is no drag protocol to add.

## Consequences

- `@path` semantics change for existing users: a mention is now inlined at
  submit, not deferred to the model. This is the intended qwen behavior.
- Clipboard support is platform-bound and dead over SSH; failure is graceful (no
  image staged, never a panic). Linux ports qwen's `wl-paste`/`xclip` subprocess
  path verbatim (tool detection from `XDG_SESSION_TYPE`/`WAYLAND_DISPLAY`/`DISPLAY`
  with WSL2 handling, PNG/BMP, the BMP->PNG PIL conversion) using the same
  `tokio::process` machinery `run_command` spawns with - NO new dependency. qwen's
  macOS/Windows path is a native npm module (`@teddyzhu/clipboard`) with no Rust
  analog; suspenders degrades gracefully there (the feature is unavailable) rather
  than port it, leaving a cross-platform Rust clipboard (e.g. `arboard`) as a
  possible follow-up. Dragged/pasted paths are portable and are the primary path.
- Temp clipboard files accumulate; port qwen's LRU cleanup
  (`cleanupOldClipboardImages`, keep 100 / drop 50).
- Clipboard staging joins `clipboard/` onto the GLOBAL temp dir exactly ONCE
  (qwen's `saveClipboardImage` joins `clipboard` onto `getGlobalTempDir()`):
  `save_clipboard_image` / `cleanup_old_clipboard_images` receive the global temp
  dir and land at `<global>/clipboard/clipboard-*.png`, while the At-Expansion
  confinement dir is that same landing dir. Passing the already-joined landing dir
  to the writer would double it to `clipboard/clipboard/`.
- The transcript / steering user line shows the **display text** the user typed
  (the `@path` residual), never the wire media projection: `UserPrompt::text`
  keeps `[image: <mime>]` for the wire (steering rides trailing user-text on the
  Tool Result), but `UserPrompt::display_text` omits media, and the user-facing
  broadcast reads the latter.
