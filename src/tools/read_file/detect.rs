//! File-type detection for read_file's dispatch - a confined port of qwen
//! v0.16.0 `fileUtils.ts` `detectFileType`, narrowed to what P3 3b needs:
//! text, svg, notebook, image, and pdf. No `infer` crate; detection is by
//! extension plus a few magic-byte matches (image mimes, `%PDF`, `<svg`).
//!
//! Suspenders' read_file does not run qwen's full binary-vs-text content
//! sampler (the `isBinaryFile` 4KB heuristic and the KNOWN_TEXT_* allowlists):
//! anything that is not a recognized image / pdf / svg / notebook falls through
//! to `Text`, matching the tool's "reads text files" contract - a genuinely
//! binary file just reads back as lossy UTF-8, as it did before this phase.

/// The file kinds read_file dispatches on (the subset of qwen's `FileType` this
/// phase handles). `Text` is the catch-all fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Text,
    Svg,
    Notebook,
    Image,
    Pdf,
}

/// The image mime for a recognized image extension (qwen's `image/*` branch,
/// enumerated for the formats read_file advertises: PNG, JPEG, GIF, WEBP, BMP).
/// SVG is TEXT, not image (qwen returns `'svg'`), so it is absent here.
fn image_mime_for_ext(ext: &str) -> Option<&'static str> {
    match ext {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

/// The image mime for a recognized image extension, exposed so read_file can
/// stamp the [`crate::content::ResultBlock::Image`] mime from the same table
/// detection used. Falls back to `application/octet-stream` (qwen's fallback for
/// an unmapped `mime.getType`).
pub fn image_mime(path: &str) -> &'static str {
    image_mime_for_ext(&extension(path)).unwrap_or("application/octet-stream")
}

/// The lowercased extension (no dot) of a path's basename, or `""` for a
/// dotfile / extensionless name (matching `path.extname`).
fn extension(path: &str) -> String {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match name.rsplit_once('.') {
        // A leading-dot dotfile (`.gitignore`) has an empty stem, so `rsplit_once`
        // yields ("", "gitignore") - qwen's extname returns "" there.
        Some((stem, ext)) if !stem.is_empty() => ext.to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// Detect the file kind from the path and a head sample (qwen `detectFileType`,
/// narrowed). Extension wins first (TypeScript-family, svg, ipynb, then image /
/// pdf mimes); a few magic-byte matches recover a mislabeled or extensionless
/// image / pdf / svg. Everything else is `Text`.
pub fn detect(path: &str, head: &[u8]) -> FileType {
    let ext = extension(path);

    // TypeScript-family extensions are text (qwen's early return keeps them off
    // the `video/mp2t` mime .ts maps to).
    if matches!(ext.as_str(), "ts" | "mts" | "cts" | "tsx") {
        return FileType::Text;
    }
    if ext == "svg" {
        return FileType::Svg;
    }
    if ext == "ipynb" {
        return FileType::Notebook;
    }
    if image_mime_for_ext(&ext).is_some() {
        return FileType::Image;
    }
    if ext == "pdf" {
        return FileType::Pdf;
    }

    // Magic-byte recovery for files whose extension did not decide it.
    if head.starts_with(b"%PDF") {
        return FileType::Pdf;
    }
    if let Some(mime) = magic_image_mime(head) {
        // Keep the mime table and the magic detector agreed; the caller re-derives
        // the mime from the ext, so a magic-only image reads as octet-stream.
        let _ = mime;
        return FileType::Image;
    }
    if is_svg_magic(head) {
        return FileType::Svg;
    }

    FileType::Text
}

/// The image mime for a head sample's magic bytes (a subset covering the
/// advertised formats), or `None`. The signatures are file-format magic bytes,
/// named so the branches read by format rather than by hex.
fn magic_image_mime(head: &[u8]) -> Option<&'static str> {
    /// The 8-byte PNG file signature (the `\x89PNG\r\n\x1a\n` header).
    const PNG_SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    /// The 3-byte JPEG SOI + marker prefix every JFIF/Exif file opens with.
    const JPEG_SIG: [u8; 3] = [0xff, 0xd8, 0xff];

    if head.starts_with(&PNG_SIG) {
        return Some("image/png");
    }
    if head.starts_with(&JPEG_SIG) {
        return Some("image/jpeg");
    }
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    // WEBP is a RIFF container: the `RIFF` fourcc, 4 size bytes, then the `WEBP`
    // fourcc - so the format-fourcc header is the first 12 bytes.
    const RIFF_HEADER_LEN: usize = 12;
    const RIFF_FOURCC: std::ops::Range<usize> = 0..4;
    const RIFF_FORMAT_FOURCC: std::ops::Range<usize> = 8..12;
    if head.len() >= RIFF_HEADER_LEN
        && head[RIFF_FOURCC] == *b"RIFF"
        && head[RIFF_FORMAT_FOURCC] == *b"WEBP"
    {
        return Some("image/webp");
    }
    if head.starts_with(b"BM") {
        return Some("image/bmp");
    }
    None
}

/// Whether a head sample looks like SVG (an `<svg` or an XML prolog followed by
/// `<svg` shortly after). Cheap heuristic for an extensionless / mislabeled SVG.
fn is_svg_magic(head: &[u8]) -> bool {
    let text = String::from_utf8_lossy(&head[..head.len().min(512)]);
    let trimmed = text.trim_start();
    trimmed.starts_with("<svg") || (trimmed.starts_with("<?xml") && text.contains("<svg"))
}

#[cfg(test)]
#[path = "../../../tests/tools/read_file/detect.rs"]
mod tests;
