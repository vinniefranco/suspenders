use super::*;
use std::path::PathBuf;

// ---- select_linux_tool (PURE tool selection from env) ------------------------

fn env(session: Option<&str>, wayland: Option<&str>, display: Option<&str>) -> ClipboardEnv {
    ClipboardEnv {
        session_type: session.map(str::to_string),
        wayland_display: wayland.map(str::to_string),
        display: display.map(str::to_string),
    }
}

#[test]
fn wayland_session_type_selects_wl_paste() {
    assert_eq!(
        select_linux_tool(&env(Some("wayland"), None, None)),
        Some(ClipboardTool::WlPaste)
    );
}

#[test]
fn wayland_display_selects_wl_paste_even_without_session_type() {
    // WSL2: XDG_SESSION_TYPE unset, WAYLAND_DISPLAY present.
    assert_eq!(
        select_linux_tool(&env(None, Some("wayland-0"), None)),
        Some(ClipboardTool::WlPaste)
    );
}

#[test]
fn x11_session_type_selects_xclip() {
    assert_eq!(
        select_linux_tool(&env(Some("x11"), None, None)),
        Some(ClipboardTool::Xclip)
    );
}

#[test]
fn display_selects_xclip_when_not_wayland() {
    assert_eq!(
        select_linux_tool(&env(None, None, Some(":0"))),
        Some(ClipboardTool::Xclip)
    );
}

#[test]
fn wayland_wins_over_display_when_both_present() {
    // A WSL2 session may carry both WAYLAND_DISPLAY and DISPLAY; Wayland is
    // checked first (qwen's ordering), so wl-paste wins.
    assert_eq!(
        select_linux_tool(&env(None, Some("wayland-0"), Some(":0"))),
        Some(ClipboardTool::WlPaste)
    );
}

#[test]
fn no_session_env_selects_no_tool() {
    assert_eq!(select_linux_tool(&env(None, None, None)), None);
}

// ---- parse_wl_paste_image_types (PURE type parsing) --------------------------

#[test]
fn parses_png_and_bmp_lines_and_drops_others() {
    let stdout = "text/plain\nimage/png\nimage/bmp\ntext/html\n";
    assert_eq!(
        parse_wl_paste_image_types(stdout),
        vec!["image/png".to_string(), "image/bmp".to_string()]
    );
}

#[test]
fn parses_no_image_types_when_absent() {
    assert!(parse_wl_paste_image_types("text/plain\ntext/html\n").is_empty());
}

#[test]
fn parse_wl_paste_types_trims_whitespace() {
    assert_eq!(
        parse_wl_paste_image_types("  image/png  \n"),
        vec!["image/png".to_string()]
    );
}

// ---- xclip_targets_have_image (PURE TARGETS parsing) -------------------------

#[test]
fn xclip_targets_detect_png() {
    assert!(xclip_targets_have_image("TIMESTAMP\nTARGETS\nimage/png\n"));
}

#[test]
fn xclip_targets_detect_bmp() {
    assert!(xclip_targets_have_image("image/bmp\n"));
}

#[test]
fn xclip_targets_no_image_when_only_text() {
    assert!(!xclip_targets_have_image(
        "TARGETS\nUTF8_STRING\ntext/plain\n"
    ));
}

// ---- temp_file_name (PURE temp-path construction) ----------------------------

#[test]
fn temp_file_name_has_the_qwen_shape() {
    let name = temp_file_name(1_700_000_000_000, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
    assert_eq!(
        name,
        "clipboard-1700000000000-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.png"
    );
    assert!(name.starts_with("clipboard-"));
    assert!(name.ends_with(".png"));
}

// ---- is_staged_image_name (PURE cleanup eligibility) -------------------------

#[test]
fn recognizes_staged_png_and_bmp() {
    assert!(is_staged_image_name("clipboard-1-abc.png"));
    assert!(is_staged_image_name("clipboard-2-def.bmp"));
    assert!(is_staged_image_name("clipboard-3-ghi.webp"));
}

#[test]
fn rejects_non_staged_names() {
    // Wrong prefix, or an extension not in the image set.
    assert!(!is_staged_image_name("notes.png"));
    assert!(!is_staged_image_name("clipboard-1-abc.txt"));
    assert!(!is_staged_image_name("screenshot.png"));
}

// ---- select_lru_removals (PURE LRU selection: keep 100, drop 50 oldest) ------

fn staged(atime_ms: u128) -> StagedFile {
    StagedFile {
        path: PathBuf::from(format!("/tmp/clipboard-{atime_ms}.png")),
        atime_ms,
    }
}

#[test]
fn lru_keeps_all_at_or_below_the_cap() {
    let files: Vec<StagedFile> = (0..100).map(|i| staged(i as u128)).collect();
    assert!(select_lru_removals(files).is_empty());

    let exactly_cap: Vec<StagedFile> = (0..100).map(|i| staged(i as u128)).collect();
    assert_eq!(exactly_cap.len(), 100);
    assert!(select_lru_removals(exactly_cap).is_empty());
}

#[test]
fn lru_drops_the_50_oldest_when_over_the_cap() {
    // 151 files: over the cap of 100, so 50 oldest are removed.
    let files: Vec<StagedFile> = (0..151).map(|i| staged(i as u128)).collect();
    let removed = select_lru_removals(files);
    assert_eq!(removed.len(), 50);
    // The removed set is exactly the 50 LOWEST atimes (0..50), oldest first.
    let atimes: Vec<u128> = removed.iter().map(|f| f.atime_ms).collect();
    assert_eq!(atimes, (0..50).collect::<Vec<u128>>());
}

#[test]
fn lru_selection_is_by_atime_not_input_order() {
    // Newest first in input; the LRU must still pick the OLDEST for removal.
    let files: Vec<StagedFile> = (0..151).rev().map(|i| staged(i as u128)).collect();
    let removed = select_lru_removals(files);
    let mut atimes: Vec<u128> = removed.iter().map(|f| f.atime_ms).collect();
    atimes.sort_unstable();
    assert_eq!(atimes, (0..50).collect::<Vec<u128>>());
}

#[test]
fn lru_just_over_cap_removes_the_min_batch() {
    // 101 files: qwen's `min(50, 101 - 100 + 50) = min(50, 51) = 50`.
    let files: Vec<StagedFile> = (0..101).map(|i| staged(i as u128)).collect();
    assert_eq!(select_lru_removals(files).len(), 50);
}

// ---- staging join lands at exactly ONE clipboard/ level (BUG 3) --------------

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cleanup_targets_a_single_clipboard_level_under_the_global_temp_dir() {
    use tempfile::TempDir;
    // `cleanup_old_clipboard_images` receives the GLOBAL temp dir and joins
    // `clipboard` ONCE (BUG 3: it must operate on `<global>/clipboard`, never the
    // doubled `<global>/clipboard/clipboard`). Stage 151 real files at the correct
    // one-level landing dir; the LRU pass must find and prune them there.
    let global = TempDir::new().unwrap();
    let landing = global.path().join("clipboard");
    tokio::fs::create_dir_all(&landing).await.unwrap();

    // Distinct atimes so the LRU order is deterministic: write oldest first.
    for i in 0..151u32 {
        let f = landing.join(format!("clipboard-{i}-x.png"));
        tokio::fs::write(&f, b"x").await.unwrap();
    }

    // Passing the GLOBAL dir must prune under `<global>/clipboard` (one level).
    cleanup_old_clipboard_images(global.path()).await;

    let mut remaining = 0usize;
    let mut entries = tokio::fs::read_dir(&landing).await.unwrap();
    while let Ok(Some(_)) = entries.next_entry().await {
        remaining += 1;
    }
    // 151 staged, 50 dropped -> 101 remain, all still in the SINGLE-level dir.
    assert_eq!(remaining, 101, "LRU must prune the one-level landing dir");
    // The doubled path must not exist (proves no `clipboard/clipboard/` join).
    assert!(
        !global.path().join("clipboard").join("clipboard").exists(),
        "there must be no doubled clipboard/clipboard/ directory"
    );
}
