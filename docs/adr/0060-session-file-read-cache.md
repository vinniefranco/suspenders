# The Session file-read cache as Run-scoped Capability state

A cell-level notebook edit is unsafe unless the model has actually SEEN the
notebook this session, and seen ALL of it: a `cell_id` the model quotes must be
one from a real read, not a hallucination, and the bytes must not have drifted
on disk since. qwen enforces this with a Session-scoped `FileReadCache` that
tracks which files the model Read or wrote plus the `(mtime, size)` snapshot at
that operation; `notebook-edit.ts` consults it (`checkPriorNotebookRead`) before
mutating. P3 3c ports that enforcement.

## The shape (F6)

`FileReadCache` (in `tool/read_cache.rs`, a leaf in the `tool` module so it takes
no `llm`/`agent`/`run` edge) is a pure, filesystem-free in-memory map:

```rust
struct FileReadCache { by_path: Mutex<HashMap<PathBuf, FileReadEntry>> }
struct FileReadEntry {
    mtime_ms, size_bytes,          // the fingerprint check() compares
    last_read_at, last_write_at,   // Option<u128> - Some once read / written
    last_read_was_full,            // the whole current content, not windowed/truncated
    last_read_cacheable,           // plain text (vs. media/notebook/native-PDF)
}
enum ReadState { Unknown, Stale, Fresh }
```

Callers `stat` the file and pass `(mtime_ms, size)` in; the cache never touches
the filesystem, matching qwen and keeping it trivially testable. `record_read`
stamps the read flags; `record_write` refreshes the fingerprint to the
post-write stat (else the writer would see its OWN write as a stale external
change) and marks the entry full + non-text-cacheable. `check` returns `Fresh`
only when an entry exists AND the fingerprint matches the current stat.

## Concrete Run-scoped Capability state, like the registry

The cache rides `Capabilities` (ADR-0055) as a concrete `Arc<FileReadCache>`,
NOT a `dyn` effect seam. It is not an effect - it is Run-scoped state the tools
read and write - so it stays concrete, exactly like the `ToolRegistry` beside
it. `run::run` builds a fresh, empty cache per Run and threads it onto
`Capabilities`; `ToolCtx::read_cache()` is the accessor. A fresh cache per Run
means a read in a prior Run does not clear this Run's enforcement, mirroring
qwen's Config-per-session invariant. The `Mutex` makes it `Send + Sync` behind
the `Arc` so it crosses the `tokio::spawn` at the Agent.

## Path-keyed, not inode-keyed (the Suspenders simplification)

qwen keys entries by `${dev}:${ino}` so symlinks / hardlinks / case-variant
paths collapse onto one file. Suspenders confines every tool path to the Project
Root (`tool::path::with_path`) and records the ABSOLUTE resolved path, so a plain
`PathBuf` key suffices: root confinement already removes the symlink-escape
surface the inode key defends against, and a `read_file` then `notebook_edit` on
the same relative path resolve to the same absolute path. This is a deliberate
narrowing, recorded here because a future non-confined caller (an escape hatch
past the root) would need the inode key back.

## Enforcement only; the fast-path is DEFERRED

qwen's cache has TWO consumers: `priorReadEnforcement` (read-before-edit) and a
ReadFile `file_unchanged` fast-path (short-circuit a repeated full read of an
unchanged file). P3 3c ports ONLY enforcement, and only `notebook_edit` consumes
it. The `file_unchanged` fast-path is not ported: it is a token-saving
optimisation, not a correctness gate, and it drags in the whole
`readResidentInHistory` / microcompaction-eviction machinery (qwen's issue-#4239
surface) that Suspenders has no equivalent of yet. So the fast-path-only fields
(`readResidentInHistory`, the FIFO eviction / `MAX_ENTRIES`,
`markReadEvictedFromHistory`) are left out with it, and the sticky-on-true
flag preservation across a `Read full -> Read partial` sequence is omitted -
enforcement records exactly what the most recent read produced (qwen's own
drift-reset arm), which is correct because enforcement keys on the on-disk
fingerprint, not on read-rights persistence.

`edit_file` and `write_file` do NOT yet consult the cache. qwen's own history
here is instructive (PR #3932 wired a `requireFullRead` into WriteFile's
overwrite path; PR #4002 removed it because the truncate-tool-output limit makes
"fully read" an impossible precondition on large files). Suspenders' `edit_file`
already has its own read-before-edit safety net - the `0 occurrences` /
closest-region failure mode - so adopting the cache there is a separate,
deferred decision, not a P3 3c requirement.

## The `last_read_was_full` gate

`notebook_edit` is the one enforcement consumer, and it requires MORE than a
prior read: it requires a FULL one. A cell-level edit needs the model to have
seen every current byte, so a windowed or (for a huge notebook) internally
truncated read does not qualify. read_file records `full` as: text -> the read
started at the top (`start_line == 1`); notebook -> the rendered cell listing was
not truncated (`!is_truncated`, plumbed out of the notebook formatter); media ->
always full. `notebook_edit` reads a `Fresh` entry and branches on
`last_read_was_full`, emitting the VERBATIM qwen rejections:

- Unknown / write-only entry -> "has not been fully read in this session ...";
- `Fresh` but `!last_read_was_full` -> "too large for cell-level editing because
  its rendered output was truncated when read ...";
- `Stale` -> "has been modified since you last read it ...".

read_file records `cacheable` as whether the result is a single Text block (a
media / native-PDF block is not text-cacheable); this flag is recorded for the
deferred `edit_file`/`write_file` adoption and is unused by `notebook_edit`.

## The read-after-write invalidation (qwen `requiresReadAfterWrite`)

Recording a write as `Fresh`/`full` after an edit means the model can do a
SECOND cell-level edit without re-reading. That is safe only while the display
ids the model quoted stayed valid. A structural edit (insert or delete) on a
notebook WITHOUT stable per-cell ids renumbers the `cell-N` fallback ids, so a
follow-up edit against a `cell-N` the model has not re-verified would land on the
wrong cell. So `apply_notebook_edit` computes `requires_read_after_write =
structural_edit AND NOT (original_has_stable_cell_ids AND updated_has_stable_cell_ids)`
- the exact qwen condition, evaluated on both the pre- and post-edit notebook -
and the tool wrapper then INVALIDATES the cache entry (drops it, so the next
`check` reads `Unknown`) instead of `record_write`, forcing a re-read before the
next edit. This is the sole caller of `Notebook::has_stable_cell_ids` and of
`FileReadCache::invalidate`.

## Deferred cap-truncation fold-in (a documented gap)

The deferred `edit_file`/`write_file` cache adoption must fold cap-truncation
into `full`. read_file's TEXT branch currently records `full = (start_line == 1)`
and ignores a later output-cap truncation - benign today, because `notebook_edit`
is the only consumer and it reads the NOTEBOOK branch, which records `full`
correctly (`!is_truncated`). When a text-file mutating tool starts consuming the
cache, a top-anchored but cap-truncated read must record `full = false`, or it
would wrongly qualify as a full read.

## Deliberate narrowings in the cell-edit port

Two faithfulness narrowings, both low blast radius, recorded so a future reader
does not mistake them for bugs:

- `Notebook::source_array_style` keys the "does this cell have a source" test on
  an EMPTY-STRING check (`Source::Text("")`), not on field PRESENCE. A cell whose
  source is genuinely the empty string reads as "no source" and is skipped when
  inferring the notebook-wide array style. Blast radius is tiny: this only feeds
  new-cell source shaping on an INSERT, and the empty-source cell is an edge that
  qwen's own presence check would still fall through past its neighbours for.
- `cell_type_str` folds an UNKNOWN on-disk `cell_type` to `code` on a replace
  that keeps the current type. qwen keeps `target.cell_type` verbatim (a raw
  string); Suspenders' typed helper only reaches this arm for an exotic
  non-`code`/`markdown`/`raw` notebook, where emitting `code`-shaped normalized
  output is the safe fallback. Well-formed notebooks never hit it.

## Considered and rejected

- **A `dyn` cache seam on `Capabilities`.** The cache is state, not an effect
  (nothing about it reaches back to the host for a decision); a `dyn` seam would
  hide a concrete type behind indirection that buys nothing, exactly the reason
  the registry stays concrete (ADR-0055).
- **Porting the `file_unchanged` fast-path now.** It is an optimisation, not a
  correctness gate, and it needs the history-residence machinery Suspenders lacks.
  Deferred with its fields so the port stays enforcement-shaped and small.
- **Inode keying.** Unnecessary under root confinement (see above); it would add
  a platform-specific `dev:ino` stat read for a collapse case the confinement
  already forecloses.
