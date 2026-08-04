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

/// Read a resolved path in FULL (no line window, no PDF `pages`) into a block
/// list - the read_many_files / At Expansion entry point (ADR-0068), where every
/// mention is inlined whole (text uncapped, media as an Image/Document block).
/// Reuses read_file's per-kind readers via [`read_blocks`]; the `full` flag it
/// returns is not needed at the batch site (no read cache), so this yields only
/// the blocks.
pub(crate) async fn read_full(
    abs: &std::path::Path,
    path: &str,
    display_name: &str,
    modalities: Modalities,
) -> Result<ToolOutput, String> {
    let file_type = params::detect_file_type(abs, path)?;
    let window = Window {
        offset: None,
        limit: None,
    };
    let (output, _full) =
        read_blocks(file_type, abs, path, display_name, window, None, modalities).await?;
    Ok(output)
}

/// Read a kind-detected file into a block list, returning the output and whether
/// the read was FULL (the whole current content, not windowed or internally
/// truncated). `display_name` is the basename read_file's media branches name in
/// their placeholders / skip messages. `file_type` is passed in (read_file
/// detects it once for its fast-path gate; read_many_files via [`read_full`]) so
/// the head sample is read once, not twice.
pub(super) async fn read_blocks(
    file_type: FileType,
    abs: &std::path::Path,
    path: &str,
    display_name: &str,
    window: Window,
    pages: Option<&str>,
    modalities: Modalities,
) -> Result<(ToolOutput, bool), String> {
    Ok(match file_type {
        FileType::Text => {
            let (text, truncated) = media::read_from(abs, path, window)?;
            (ToolOutput::text(text), window.is_full() && !truncated)
        }
        FileType::Svg => (
            ToolOutput::text(media::read_svg(abs, path, display_name)?),
            true,
        ),
        FileType::Notebook => {
            let raw = media::read_text(abs, path)?;
            let read = notebook::read_with_meta(&raw)?;
            // A notebook whose rendered cell listing was truncated is NOT a full
            // read: notebook_edit refuses to cell-edit it (qwen's
            // `lastReadWasFull = !isTruncated`).
            (ToolOutput::text(read.content), !read.is_truncated)
        }
        FileType::Image => (
            media::read_image(abs, path, display_name, modalities)?,
            true,
        ),
        FileType::Pdf => (
            media::read_pdf(abs, path, display_name, pages, modalities).await?,
            true,
        ),
    })
}
