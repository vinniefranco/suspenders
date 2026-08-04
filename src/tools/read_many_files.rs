//! `read_many_files`: the batch reader At Expansion (ADR-0068) drives - qwen's
//! `read_many_files` tool, narrowed to what At Expansion needs. Given a list of
//! already-resolved, Project-Root-confined path specs (each a file or a
//! directory), it expands every directory via the shared gitignore-aware walk
//! ([`crate::walk::walk_files`]) and reads each file by REUSING read_file's
//! per-file readers ([`crate::tools::read_file::reader::read_full`]) - the same
//! mime/magic-byte detection, text-window slicer, and image/PDF/svg/notebook
//! branches, so a `@shot.png` mention rides as an Image block and a `@big.log`
//! inlines uncapped as Text, exactly as read_file would emit them.
//!
//! It does NOT resolve or confine paths itself: the caller ([`super::at_expansion`])
//! has already resolved each `@path` against the Project Root and skipped the ones
//! that escape it or are ignored, so this module trusts its input specs. The
//! confinement predicate lives once, at the resolve site.
//!
//! The read is UNCAPPED (ADR-0068's deliberate deviation): a text file inlines in
//! full, media rides as a media block. The Result Cap governs model-driven Tool
//! Results, not user-authored At Expansion content.

use std::path::{Path, PathBuf};

use crate::content::{ContentBlock, Modalities};

/// One resolved path spec to read: an absolute path already confined to the
/// Project Root, whether it is a directory (glob-expanded) or a file (read
/// directly), and the display label the caller reported the mention under (the
/// original relative `pathName`, for the read display).
#[derive(Debug, Clone)]
pub(crate) struct Spec {
    /// The resolved absolute path (confined to the Project Root by the caller).
    pub abs: PathBuf,
    /// Whether this spec is a directory (walked) or a file (read directly).
    pub is_dir: bool,
    /// The mention's display label (qwen's `pathName` / `contentLabelsForDisplay`).
    pub display: String,
}

/// The outcome of reading one spec: the display label, whether it was a
/// directory, and the error message if the read failed. Mirrors qwen's per-file
/// `filesRead` entry that feeds the "Read File" / "Read Directory" display cards.
/// The blocks a spec produced are appended to the shared [`BatchRead::blocks`]
/// stream (in spec order), not held per-spec.
#[derive(Debug, Clone)]
pub(crate) struct FileRead {
    /// The mention's display label (the original relative path).
    pub display: String,
    /// Whether this spec was a directory (a "Read Directory" card) or a file.
    pub is_dir: bool,
    /// The read error, if the spec failed to read.
    pub error: Option<String>,
}

/// The full batch outcome: the ordered content blocks across every spec (the
/// list At Expansion appends after the residual query text) and the per-spec
/// read descriptions (the read display, qwen's tool-call cards).
#[derive(Debug, Clone, Default)]
pub(crate) struct BatchRead {
    pub blocks: Vec<ContentBlock>,
    pub reads: Vec<FileRead>,
}

/// Read every spec into a mixed content-block list, expanding a directory spec
/// via the gitignore-aware walk and reading each file via read_file's reader.
/// `root` is the Project Root the walk is confined to; `modalities` gates media
/// on the captured Model (an image on an image-blind Model degrades to the
/// verbatim placeholder at read time, ADR-0059). Blocks are emitted in spec
/// order, and within a directory in the walk's deterministic order.
pub(crate) async fn read(specs: &[Spec], root: &Path, modalities: Modalities) -> BatchRead {
    let mut out = BatchRead::default();
    for spec in specs {
        if spec.is_dir {
            out.reads
                .push(read_directory(spec, root, modalities, &mut out.blocks).await);
        } else {
            out.reads
                .push(read_one_file(spec, modalities, &mut out.blocks).await);
        }
    }
    out
}

/// Expand a directory spec via the shared walk (gitignore-aware, symlink-safe,
/// deterministic) and read each file into `blocks`. The walk is already confined
/// to the Project Root (the spec's `abs` is root-confined and `walk_files` never
/// follows symlinks out), so a directory mention pulls in exactly the files glob
/// would - honoring `.gitignore` and the vendored SKIP_DIRS. A file that fails to
/// read is skipped from the block stream but does not fail the whole directory.
async fn read_directory(
    spec: &Spec,
    _root: &Path,
    modalities: Modalities,
    blocks: &mut Vec<ContentBlock>,
) -> FileRead {
    let mut first_error: Option<String> = None;
    let mut any_read = false;
    for file in crate::walk::walk_files(&spec.abs) {
        let display = relative_display(&spec.abs, &file);
        match read_path(&file, &display, modalities).await {
            Ok(mut produced) => {
                any_read = true;
                blocks.append(&mut produced);
            }
            Err(err) => {
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        }
    }
    // A directory that yielded no readable file at all surfaces its first error
    // (an empty directory reads cleanly with no blocks and no error).
    let error = if any_read { None } else { first_error };
    FileRead {
        display: spec.display.clone(),
        is_dir: true,
        error,
    }
}

/// Read a single file spec into `blocks`.
async fn read_one_file(
    spec: &Spec,
    modalities: Modalities,
    blocks: &mut Vec<ContentBlock>,
) -> FileRead {
    match read_path(&spec.abs, &spec.display, modalities).await {
        Ok(mut produced) => {
            blocks.append(&mut produced);
            FileRead {
                display: spec.display.clone(),
                is_dir: false,
                error: None,
            }
        }
        Err(err) => FileRead {
            display: spec.display.clone(),
            is_dir: false,
            error: Some(err),
        },
    }
}

/// Read one resolved file via read_file's reader and map its [`crate::tool::ToolOutput`]
/// blocks into user-content [`ContentBlock`]s (ADR-0068): a text/svg/notebook read
/// becomes a Text block, an image an Image block, a native PDF a Document block. The
/// read is FULL (no line window, no PDF `pages`) - At Expansion inlines each mention
/// whole.
async fn read_path(
    abs: &Path,
    display: &str,
    modalities: Modalities,
) -> Result<Vec<ContentBlock>, String> {
    let display_name = basename(display);
    let output =
        crate::tools::read_file::reader::read_full(abs, display, &display_name, modalities).await?;
    Ok(output.blocks.into_iter().map(result_to_content).collect())
}

/// Map a read_file [`crate::content::ResultBlock`] (tool-result content) into the
/// matching user-content [`ContentBlock`] (ADR-0068): At Expansion produces
/// first-class USER content, so an image/PDF becomes a user Image/Document block,
/// never a Tool Result.
fn result_to_content(block: crate::content::ResultBlock) -> ContentBlock {
    match block {
        crate::content::ResultBlock::Text { text } => ContentBlock::Text { text },
        crate::content::ResultBlock::Image { mime, data } => ContentBlock::Image { mime, data },
        crate::content::ResultBlock::Document { mime, data } => {
            ContentBlock::Document { mime, data }
        }
    }
}

/// The basename of a `/`-or-`\`-separated display path (qwen's `path.basename`).
fn basename(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}

/// A walked file's path RELATIVE to the directory spec it came from, `/`-joined
/// for the display label. Falls back to the file's lossy display form if it will
/// not strip (never - the walk returns paths under the spec).
fn relative_display(dir: &Path, file: &Path) -> String {
    file.strip_prefix(dir)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
#[path = "../../tests/tools/read_many_files.rs"]
mod tests;
