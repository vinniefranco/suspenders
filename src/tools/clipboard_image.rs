//! Clipboard image capture (ADR-0068 P5, the Attachment flow): a faithful port
//! of qwen's `clipboardUtils.ts` Linux path. A `Ctrl+V` with image bytes on the
//! system clipboard (a screenshot, NO file on disk) is written to a temp PNG
//! under the global clipboard temp dir; the caller inserts an `@<temppath>` At
//! Mention, and At Expansion (P3) reads it back via the temp-dir confinement
//! exception. A clipboard image REJOINS the one At Expansion spine rather than
//! being a second content path.
//!
//! The IMPURE edge is the subprocess spawn (`wl-paste`/`xclip` via
//! `tokio::process::Command`, the same crate `run_command` spawns with) and the
//! filesystem. Everything the impure edge decides ON is factored into PURE,
//! unit-tested helpers so the logic is testable WITHOUT spawning real processes:
//!
//!   * [`select_linux_tool`] - PURE. The `wl-paste`/`xclip` choice from the
//!     `XDG_SESSION_TYPE`/`WAYLAND_DISPLAY`/`DISPLAY` env (qwen's
//!     `getLinuxClipboardTool`, WSL2 handling included).
//!   * [`parse_wl_paste_image_types`] - PURE. Filter `wl-paste --list-types`
//!     stdout to the `image/png`/`image/bmp` lines (qwen's `getWlPasteImageTypes`).
//!   * [`xclip_targets_have_image`] - PURE. Whether `xclip -t TARGETS -o` stdout
//!     advertises `image/png` or `image/bmp` (qwen's `checkClipboardForImage`).
//!   * [`temp_file_name`] - PURE. The `clipboard-<ts>-<uuid>.png` filename shape.
//!   * [`select_lru_removals`] - PURE. The LRU cleanup selection (keep 100, drop
//!     the 50 oldest by atime; qwen's `cleanupOldClipboardImages`).
//!
//! Off Linux (macOS/Windows) qwen uses the `@teddyzhu/clipboard` native npm
//! module, which has no Rust analog. Suspenders DEGRADES GRACEFULLY there: the
//! feature is unavailable (no image staged), never a panic. A cross-platform Rust
//! clipboard (e.g. `arboard`) is a possible follow-up; the subprocess approach is
//! kept for Linux to match qwen's observable behavior with NO new dependency.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// The per-subprocess timeout (qwen's `PROCESS_TIMEOUT_MS`): a clipboard tool
/// that hangs must not wedge the capture. 5s, verbatim from qwen.
const PROCESS_TIMEOUT: Duration = Duration::from_millis(5000);

/// The two image MIME types qwen detects on the clipboard (`clipboardUtils.ts`):
/// `image/png`, and `image/bmp` (WSL2's Windows-clipboard shape, converted to
/// PNG via python3 PIL). The one place the set lives so detection and save agree.
const IMAGE_MIME_PNG: &str = "image/png";
const IMAGE_MIME_BMP: &str = "image/bmp";

/// The Linux clipboard tool (qwen's `'wl-paste' | 'xclip'`): the Wayland reader
/// or the X11 reader, chosen from the session env.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClipboardTool {
    /// Wayland: `wl-paste`.
    WlPaste,
    /// X11: `xclip`.
    Xclip,
}

impl ClipboardTool {
    /// The executable name (what `command -v` probes and `Command::new` spawns).
    fn command(self) -> &'static str {
        match self {
            ClipboardTool::WlPaste => "wl-paste",
            ClipboardTool::Xclip => "xclip",
        }
    }
}

/// The session-type env inputs [`select_linux_tool`] reads (qwen's
/// `process.env['XDG_SESSION_TYPE' | 'WAYLAND_DISPLAY' | 'DISPLAY']`). A struct so
/// the PURE selection is exercised without touching `std::env`; the impure edge
/// fills it from the real environment via [`ClipboardEnv::from_process`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ClipboardEnv {
    pub session_type: Option<String>,
    pub wayland_display: Option<String>,
    pub display: Option<String>,
}

impl ClipboardEnv {
    /// Reads the three session vars from the process environment (the impure
    /// edge). An empty var is treated as unset, matching the XDG idiom the rest
    /// of the codebase uses for env reads.
    fn from_process() -> Self {
        let read = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
        ClipboardEnv {
            session_type: read("XDG_SESSION_TYPE"),
            wayland_display: read("WAYLAND_DISPLAY"),
            display: read("DISPLAY"),
        }
    }
}

/// PURE. The Linux clipboard tool the session env selects (qwen's
/// `getLinuxClipboardTool`): Wayland (`XDG_SESSION_TYPE=wayland` OR any
/// `WAYLAND_DISPLAY`) picks `wl-paste`; X11 (`XDG_SESSION_TYPE=x11` OR any
/// `DISPLAY`) picks `xclip`; neither yields `None`. The `WAYLAND_DISPLAY`
/// fallback handles WSL2, where `XDG_SESSION_TYPE` may be unset but the Wayland
/// socket is present - Wayland is checked FIRST so a WSL2 session with both vars
/// still resolves to `wl-paste`, matching qwen's ordering.
///
/// This decides only WHICH tool; whether the tool is actually installed is the
/// impure edge's `command -v` probe (qwen's `execSync('command -v ...')`).
pub(crate) fn select_linux_tool(env: &ClipboardEnv) -> Option<ClipboardTool> {
    if env.session_type.as_deref() == Some("wayland") || env.wayland_display.is_some() {
        Some(ClipboardTool::WlPaste)
    } else if env.session_type.as_deref() == Some("x11") || env.display.is_some() {
        Some(ClipboardTool::Xclip)
    } else {
        None
    }
}

/// PURE. The image MIME types present in `wl-paste --list-types` stdout (qwen's
/// `getWlPasteImageTypes`): each line trimmed, kept only if it is exactly
/// `image/png` or `image/bmp`. The presence of any decides "has image"; the set
/// decides which save path (PNG direct, BMP-via-conversion) `save` takes.
pub(crate) fn parse_wl_paste_image_types(stdout: &str) -> Vec<String> {
    stdout
        .trim()
        .lines()
        .map(str::trim)
        .filter(|t| *t == IMAGE_MIME_PNG || *t == IMAGE_MIME_BMP)
        .map(str::to_string)
        .collect()
}

/// PURE. Whether `xclip -selection clipboard -t TARGETS -o` stdout advertises an
/// image target (qwen's `checkClipboardForImage` xclip arm): any line equal to
/// `image/png` or `image/bmp`.
pub(crate) fn xclip_targets_have_image(stdout: &str) -> bool {
    stdout
        .lines()
        .map(str::trim)
        .any(|line| line == IMAGE_MIME_PNG || line == IMAGE_MIME_BMP)
}

/// PURE. The staged temp file name (qwen's `clipboard-${timestamp}-${randomUUID()}.png`):
/// the epoch-millis timestamp and a UUID-shaped hex id join into one PNG name.
/// Kept a pure fn (the ts + id are injected) so the shape is testable without a
/// clock or RNG.
pub(crate) fn temp_file_name(timestamp_ms: u128, uuid: &str) -> String {
    format!("clipboard-{timestamp_ms}-{uuid}.png")
}

/// One staged clipboard file with the atime the LRU sort keys on (qwen's
/// `{name, path, atime}`). A plain record so [`select_lru_removals`] is pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StagedFile {
    pub path: PathBuf,
    /// Access time as epoch millis (qwen's `stats.atimeMs`).
    pub atime_ms: u128,
}

/// The LRU cap (qwen's `MAX_IMAGES`): keep at most this many staged images.
const MAX_IMAGES: usize = 100;
/// The LRU batch size (qwen's `CLEANUP_COUNT`): how many oldest images to drop
/// once the cap is exceeded.
const CLEANUP_COUNT: usize = 50;

/// PURE. The staged files to delete under the LRU policy (qwen's
/// `cleanupOldClipboardImages` selection): when the count exceeds
/// [`MAX_IMAGES`], drop the oldest by atime. The count removed is
/// `min(CLEANUP_COUNT, len - MAX_IMAGES + CLEANUP_COUNT)` - qwen's exact
/// arithmetic, which for any `len > MAX_IMAGES` in practice yields
/// [`CLEANUP_COUNT`] (the `min` guards a degenerate overflow). Returns the files
/// to remove, oldest first; an empty vec when at or below the cap.
pub(crate) fn select_lru_removals(mut files: Vec<StagedFile>) -> Vec<StagedFile> {
    if files.len() <= MAX_IMAGES {
        return Vec::new();
    }
    // Oldest first (ascending atime), qwen's `sort((a, b) => a.atime - b.atime)`.
    files.sort_by_key(|f| f.atime_ms);
    let remove_count = CLEANUP_COUNT.min(files.len() - MAX_IMAGES + CLEANUP_COUNT);
    files.truncate(remove_count);
    files
}

/// The file-name prefix and extensions the LRU cleanup recognizes as a staged
/// clipboard image (qwen's `cleanupOldClipboardImages` filter): the
/// `clipboard-` prefix and any of the image extensions qwen lists. Only these
/// are eligible for deletion, so an unrelated file dropped in the dir is never
/// touched.
const STAGED_PREFIX: &str = "clipboard-";
const STAGED_EXTENSIONS: &[&str] = &[
    ".png", ".jpg", ".webp", ".heic", ".heif", ".tiff", ".gif", ".bmp",
];

/// PURE. Whether a file name is an eligible staged clipboard image for the LRU
/// cleanup (qwen's `file.startsWith('clipboard-') && (file.endsWith('.png') || ...)`).
pub(crate) fn is_staged_image_name(name: &str) -> bool {
    name.starts_with(STAGED_PREFIX) && STAGED_EXTENSIONS.iter().any(|ext| name.ends_with(ext))
}

// ---- impure edge (subprocess + fs) -------------------------------------------

/// Whether the system clipboard currently holds an image (ADR-0068 P5, qwen's
/// `clipboardHasImage`). Linux-only: probes the selected tool (`wl-paste
/// --list-types` / `xclip -t TARGETS -o`) for an `image/png` or `image/bmp`
/// target. Off Linux, or when no tool is available/installed, returns `false`
/// (graceful degrade - no image, never a panic).
#[cfg(target_os = "linux")]
pub(crate) async fn clipboard_has_image() -> bool {
    let Some(tool) = resolve_available_tool().await else {
        return false;
    };
    match tool {
        ClipboardTool::WlPaste => {
            let stdout = run_capture("wl-paste", &["--list-types"]).await;
            stdout
                .map(|out| !parse_wl_paste_image_types(&out).is_empty())
                .unwrap_or(false)
        }
        ClipboardTool::Xclip => {
            let stdout =
                run_capture("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"]).await;
            stdout
                .map(|out| xclip_targets_have_image(&out))
                .unwrap_or(false)
        }
    }
}

/// Off-Linux graceful degrade: no native clipboard-image reader is ported, so the
/// clipboard is treated as holding no image (never a panic). A cross-platform
/// Rust clipboard (e.g. `arboard`) is a possible follow-up.
#[cfg(not(target_os = "linux"))]
pub(crate) async fn clipboard_has_image() -> bool {
    false
}

/// Saves the clipboard image to a temp PNG under `<global_temp_dir>/clipboard/`,
/// returning the absolute path on success (ADR-0068 P5, qwen's
/// `saveClipboardImage`, which joins `clipboard` onto the GLOBAL temp dir). The
/// caller passes the GLOBAL temp dir and this joins `clipboard` exactly ONCE (BUG
/// 3: passing the already-joined landing dir produced a doubled
/// `clipboard/clipboard/`). Runs the LRU cleanup around the save (qwen calls
/// `cleanupOldClipboardImages` on the same dir). Linux-only: writes `wl-paste
/// --type image/png` (or the BMP->PNG conversion for WSL2) / `xclip -t image/png
/// -o` stdout to the temp file. Returns `None` on no image / any failure
/// (graceful degrade).
#[cfg(target_os = "linux")]
pub(crate) async fn save_clipboard_image(global_temp_dir: &Path) -> Option<PathBuf> {
    let staging = global_temp_dir.join("clipboard");
    if tokio::fs::create_dir_all(&staging).await.is_err() {
        return None;
    }

    // LRU cleanup around the save (qwen calls it alongside the save).
    cleanup_old_clipboard_images(global_temp_dir).await;

    let tool = resolve_available_tool().await?;
    let png_path = staging.join(temp_file_name(now_millis(), &uuid_hex()));

    match tool {
        ClipboardTool::WlPaste => save_with_wl_paste(&png_path).await,
        ClipboardTool::Xclip => save_with_xclip(&png_path).await,
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) async fn save_clipboard_image(_dir: &Path) -> Option<PathBuf> {
    None
}

/// LRU cleanup of the staging dir (qwen's `cleanupOldClipboardImages`): joins
/// `clipboard` onto the GLOBAL temp dir (once, BUG 3), reads the
/// `<global_temp_dir>/clipboard/` entries, keeps the staged images, and deletes
/// the oldest batch by atime once the cap is exceeded. Best-effort - every error
/// is ignored (qwen's silent `catch`).
#[cfg(target_os = "linux")]
pub(crate) async fn cleanup_old_clipboard_images(global_temp_dir: &Path) {
    let staging = global_temp_dir.join("clipboard");
    let Ok(mut entries) = tokio::fs::read_dir(&staging).await else {
        return;
    };
    let mut staged: Vec<StagedFile> = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_staged_image_name(&name) {
            continue;
        }
        if let Ok(meta) = entry.metadata().await {
            staged.push(StagedFile {
                path: entry.path(),
                atime_ms: access_millis(&meta),
            });
        }
    }
    for file in select_lru_removals(staged) {
        let _ = tokio::fs::remove_file(&file.path).await;
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) async fn cleanup_old_clipboard_images(_dir: &Path) {}

/// Resolves the selected Linux tool AND probes that it is installed (`command -v`,
/// qwen's `execSync('command -v ...')`). `None` when no tool matches the session
/// env or the matched tool is not on `PATH`.
#[cfg(target_os = "linux")]
async fn resolve_available_tool() -> Option<ClipboardTool> {
    let tool = select_linux_tool(&ClipboardEnv::from_process())?;
    if tool_installed(tool.command()).await {
        Some(tool)
    } else {
        None
    }
}

/// Whether `command` is on `PATH` (qwen's `command -v <tool>` probe).
#[cfg(target_os = "linux")]
async fn tool_installed(command: &str) -> bool {
    tokio::process::Command::new("command")
        .arg("-v")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        // `command` is a shell builtin, not always an executable; fall back to
        // spawning the tool through the shell for the probe.
        .unwrap_or(false)
        || shell_command_v(command).await
}

/// The shell-builtin `command -v` probe fallback (`command -v` is a bash builtin,
/// not an executable, so the direct spawn above can fail with ENOENT on some
/// hosts). Runs it through `bash -c`, the same interpreter `run_command` uses.
#[cfg(target_os = "linux")]
async fn shell_command_v(command: &str) -> bool {
    tokio::process::Command::new("bash")
        .arg("-c")
        .arg(format!("command -v {command}"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Runs `command args...` and returns its stdout as a string, or `None` on
/// spawn error, non-zero exit, or timeout (qwen's `checkClipboardForImage`
/// promise shape). The timeout kills a hung tool (qwen's `PROCESS_TIMEOUT_MS`).
#[cfg(target_os = "linux")]
async fn run_capture(command: &str, args: &[&str]) -> Option<String> {
    let mut cmd = tokio::process::Command::new(command);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let child = cmd.spawn().ok()?;
    let output = tokio::time::timeout(PROCESS_TIMEOUT, child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Saves the clipboard image via `wl-paste` (qwen's `saveFileWithWlPaste`):
/// prefer `image/png` direct; else `image/bmp` converted to PNG via python3 PIL
/// (the WSL2 path). Returns the saved path on success (non-empty file), else
/// `None` (cleaning up any partial file).
#[cfg(target_os = "linux")]
async fn save_with_wl_paste(png_path: &Path) -> Option<PathBuf> {
    let types = run_capture("wl-paste", &["--list-types"])
        .await
        .map(|out| parse_wl_paste_image_types(&out))
        .unwrap_or_default();

    if types.iter().any(|t| t == IMAGE_MIME_PNG)
        && save_stdout_to_file(
            "wl-paste",
            &["--no-newline", "--type", "image/png"],
            png_path,
        )
        .await
    {
        return non_empty_or_cleanup(png_path).await;
    }

    if types.iter().any(|t| t == IMAGE_MIME_BMP) {
        let bmp_path = png_path.with_extension("bmp");
        if save_stdout_to_file(
            "wl-paste",
            &["--no-newline", "--type", "image/bmp"],
            &bmp_path,
        )
        .await
        {
            let converted = convert_bmp_to_png(&bmp_path, png_path).await;
            let _ = tokio::fs::remove_file(&bmp_path).await;
            if converted {
                return non_empty_or_cleanup(png_path).await;
            }
            // Conversion failed (python3/PIL absent): report clean failure -
            // downstream expects a PNG. qwen degrades the same way with a note.
            let _ = tokio::fs::remove_file(png_path).await;
        }
    }
    None
}

/// Saves the clipboard image via `xclip` (qwen's `saveFileWithXclip`):
/// `xclip -selection clipboard -t image/png -o` to the temp file. Returns the
/// path on success, else `None` (cleaning up a partial file).
#[cfg(target_os = "linux")]
async fn save_with_xclip(png_path: &Path) -> Option<PathBuf> {
    if save_stdout_to_file(
        "xclip",
        &["-selection", "clipboard", "-t", "image/png", "-o"],
        png_path,
    )
    .await
    {
        return non_empty_or_cleanup(png_path).await;
    }
    let _ = tokio::fs::remove_file(png_path).await;
    None
}

/// Pipes `command args...` stdout to `destination` (qwen's `saveFromCommand`),
/// with the process timeout. Returns whether the command exited 0 and the file
/// was written. A failure leaves cleanup to the caller.
#[cfg(target_os = "linux")]
async fn save_stdout_to_file(command: &str, args: &[&str], destination: &Path) -> bool {
    let mut cmd = tokio::process::Command::new(command);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let Ok(child) = cmd.spawn() else {
        return false;
    };
    let output = match tokio::time::timeout(PROCESS_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        _ => return false,
    };
    if !output.status.success() {
        return false;
    }
    tokio::fs::write(destination, &output.stdout).await.is_ok()
}

/// Converts a BMP to PNG via python3 PIL (qwen's WSL2 conversion), the faithful
/// nicety for the Windows-clipboard BMP path. Returns whether the conversion
/// succeeded; a missing python3/PIL is a clean `false` (the caller degrades with
/// a note, no panic).
#[cfg(target_os = "linux")]
async fn convert_bmp_to_png(bmp_path: &Path, png_path: &Path) -> bool {
    let mut cmd = tokio::process::Command::new("python3");
    cmd.arg("-c")
        .arg("import sys; from PIL import Image; Image.open(sys.argv[1]).save(sys.argv[2])")
        .arg(bmp_path)
        .arg(png_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let Ok(child) = cmd.spawn() else {
        return false;
    };
    matches!(
        tokio::time::timeout(PROCESS_TIMEOUT, child.wait_with_output()).await,
        Ok(Ok(output)) if output.status.success()
    )
}

/// Returns the path when the saved file is non-empty; else deletes it and returns
/// `None` (qwen's `stats.size > 0` guard).
#[cfg(target_os = "linux")]
async fn non_empty_or_cleanup(path: &Path) -> Option<PathBuf> {
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.len() > 0 => Some(path.to_path_buf()),
        _ => {
            let _ = tokio::fs::remove_file(path).await;
            None
        }
    }
}

/// The current epoch time in millis (qwen's `new Date().getTime()`), for the temp
/// file name. Saturates on the (impossible) pre-epoch clock rather than panic.
#[cfg(target_os = "linux")]
fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// A UUID-shaped hex id for the temp file name (qwen's `randomUUID()`), built
/// from the already-present `rand` crate so no new dependency is added. Not a
/// spec UUID - just 32 random hex digits in the `8-4-4-4-12` grouping, enough to
/// make the temp name collision-free per qwen's use.
#[cfg(target_os = "linux")]
fn uuid_hex() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32],
    )
}

/// A file's access time as epoch millis (qwen's `stats.atimeMs`), for the LRU
/// sort. Falls back to `0` (treated as oldest, so it is a cleanup candidate)
/// when the platform does not expose atime.
#[cfg(target_os = "linux")]
fn access_millis(meta: &std::fs::Metadata) -> u128 {
    meta.accessed()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "../../tests/tools/clipboard_image.rs"]
mod tests;
