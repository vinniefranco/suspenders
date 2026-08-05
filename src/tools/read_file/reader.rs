//! The per-file reader dispatch, factored out of `read_file.rs`'s `run_rich` so
//! it is shared with `read_many_files` (ADR-0068, At Expansion). Given a resolved
//! absolute path, its display path, an offset/limit [`Window`], an optional PDF
//! `pages` range, and the captured Model's [`Modalities`], detect the file kind
//! and emit the right block list: text/svg/notebook become one Text block, an
//! image or native PDF becomes a media block when the Model accepts the modality,
//! else the verbatim unsupported-modality placeholder (read-time degrade,
//! ADR-0059).
//!
//! This is the ONE place the kind→reader switch lives. read_file's `run_rich`
//! keeps the read-cache fast-path and media-param validation (both driven by the
//! model-supplied params), then delegates the actual read here; read_many_files
//! calls [`read_blocks`] with a full window and no `pages`, so a `@big.log`
//! inlines uncapped and a `@shot.png` rides as an Image block. The read cache is
//! read_file's concern (F6, ADR-0060) and stays at that call site - a batch
//! At Expansion read does not touch the Run's read cache.

use super::detect::FileType;
use super::params::Window;
use super::{media, notebook, params};
use crate::content::Modalities;
use crate::tool::ToolOutput;

/// The resolved identity of ONE file read: the root-confined absolute path the
/// I/O touches, the caller-facing `path` errors and placeholders name (the wire
/// path for read_file; the display path for an At Expansion read), and the
/// derived basename the media branches use in skip messages. Built once at the
/// read entry, so "abs was resolved from path, display_name is its basename"
/// is a constructor fact - not a three-parameter convention every reader
/// signature re-states.
pub(crate) struct SourceFile<'a> {
    pub(crate) abs: &'a std::path::Path,
    pub(crate) path: &'a str,
    display_name: String,
}

impl<'a> SourceFile<'a> {
    /// Pairs a resolved absolute path with the caller-facing path it came from
    /// and derives the basename the media branches display.
    pub(crate) fn new(abs: &'a std::path::Path, path: &'a str) -> SourceFile<'a> {
        SourceFile {
            abs,
            path,
            display_name: params::basename(path),
        }
    }

    /// The basename the media branches name in placeholders / skip messages.
    pub(super) fn display_name(&self) -> &str {
        &self.display_name
    }

    /// Detect the file kind from the head sample (extension + magic bytes).
    /// Lives on the value so detection always reads the same `abs`/`path`
    /// pairing the readers will.
    pub(super) fn detect(&self) -> Result<FileType, String> {
        params::detect_file_type(self.abs, self.path)
    }
}

/// Read a resolved path in FULL (no line window, no PDF `pages`) into a block
/// list - the read_many_files / At Expansion entry point (ADR-0068), where every
/// mention is inlined whole (text uncapped, media as an Image/Document block).
/// Reuses read_file's per-kind readers via [`read_blocks`]; the `full` flag it
/// returns is not needed at the batch site (no read cache), so this yields only
/// the blocks.
pub(crate) async fn read_full(
    abs: &std::path::Path,
    path: &str,
    modalities: Modalities,
) -> Result<ToolOutput, String> {
    let source = SourceFile::new(abs, path);
    let file_type = source.detect()?;
    let window = Window {
        offset: None,
        limit: None,
    };
    let (output, _full) = read_blocks(file_type, &source, window, None, modalities).await?;
    Ok(output)
}

/// Read a kind-detected file into a block list, returning the output and whether
/// the read was FULL (the whole current content, not windowed or internally
/// truncated). `file_type` is passed in beside the [`SourceFile`] (read_file
/// detects it once for its fast-path gate; read_many_files via [`read_full`]) so
/// the head sample is read once, not twice.
pub(super) async fn read_blocks(
    file_type: FileType,
    source: &SourceFile<'_>,
    window: Window,
    pages: Option<&str>,
    modalities: Modalities,
) -> Result<(ToolOutput, bool), String> {
    Ok(match file_type {
        FileType::Text => {
            let (text, truncated) = media::read_from(source, window)?;
            (ToolOutput::text(text), window.is_full() && !truncated)
        }
        FileType::Svg => (ToolOutput::text(media::read_svg(source)?), true),
        FileType::Notebook => {
            let raw = media::read_text(source)?;
            let read = notebook::read_with_meta(&raw)?;
            // A notebook whose rendered cell listing was truncated is NOT a full
            // read: notebook_edit refuses to cell-edit it (qwen's
            // `lastReadWasFull = !isTruncated`).
            (ToolOutput::text(read.content), !read.is_truncated)
        }
        FileType::Image => (media::read_image(source, modalities)?, true),
        FileType::Pdf => (media::read_pdf(source, pages, modalities).await?, true),
    })
}
