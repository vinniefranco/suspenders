//! The `--resume` picker's read model (ADR-0010): which Session Logs exist in a
//! dir and a one-line label for each, plus [`latest`] (the newest log). Carved
//! out of [`super::resume`] so the fold-back path keeps "reconstruct the
//! Conversation" and this owns "list what can be resumed".
//!
//! Every read here uses the fold's torn-line tolerance and skips unreadable or
//! foreign files silently: the picker shows what it can and stays quiet about
//! the rest. The public surface is re-exported from [`super`], so callers still
//! reach it as `crate::session::log::…`.

use super::{Entry, codec};

/// The newest log file in `dir`, by the sortable timestamp filename. `None`
/// when the dir has no `.jsonl` files or cannot be read.
pub fn latest(dir: &str) -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".jsonl"))
        .collect();
    names.sort();
    let newest = names.last()?;
    Some(
        std::path::Path::new(dir)
            .join(newest)
            .to_string_lossy()
            .into_owned(),
    )
}

/// One row of the `--resume` picker: a Session Log file, its filename-derived
/// timestamp (human-trimmed), and a label taken from the first user prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub path: String,
    pub stamp: String,
    pub label: String,
}

/// Every Session Log in `dir`, NEWEST first - keyed by the sortable stamp
/// filename, the same source [`latest`] sorts on. Unreadable or foreign files
/// (a torn header included) are skipped, never a panic: the picker shows what
/// it can and stays quiet about the rest.
pub fn list(dir: &str) -> Vec<SessionEntry> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".jsonl"))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names.reverse();
    names
        .into_iter()
        .filter_map(|name| list_entry(dir, &name))
        .collect()
}

// One picker row, or `None` for a file that cannot be read or whose header is
// not a Session Log header (foreign/torn - the same decode tolerance resume
// takes, minus the error reporting: the picker just skips it).
fn list_entry(dir: &str, name: &str) -> Option<SessionEntry> {
    let path = std::path::Path::new(dir)
        .join(name)
        .to_string_lossy()
        .into_owned();
    let content = std::fs::read_to_string(&path).ok()?;
    let mut lines = content.lines().filter(|l| !l.is_empty());
    let header = codec::decode_line(lines.next()?)?;
    if header.get("type").and_then(|v| v.as_str()) != Some("session") {
        return None;
    }
    Some(SessionEntry {
        path,
        stamp: human_stamp(name),
        label: first_user_label(lines),
    })
}

// The first user prompt's text as a one-line label; "(empty session)" when
// the log holds none. A torn line stops the scan, like resume's fold. A media
// prompt (`user_content`, ADR-0068) labels off its text projection - the Text
// blocks with each media block as its `[image: …]` placeholder.
fn first_user_label<'a>(lines: impl Iterator<Item = &'a str>) -> String {
    for line in lines {
        match codec::decode_line(line).and_then(|v| Entry::from_json(&v)) {
            Some(Entry::UserText(text)) => return label_from(&text),
            Some(Entry::UserContent(blocks)) => {
                return label_from(&crate::content::UserPrompt::from_blocks(blocks).text());
            }
            Some(_) => continue,
            None => break,
        }
    }
    "(empty session)".to_string()
}

/// How many label chars the picker shows before truncating with `…`.
const LABEL_CHARS: usize = 60;

fn label_from(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    if first.chars().count() > LABEL_CHARS {
        let mut out: String = first.chars().take(LABEL_CHARS).collect();
        out.push('…');
        out
    } else {
        first.to_string()
    }
}

// `20260711-140205-3.jsonl` -> `2026-07-11 14:02` (the `utc_stamp` shape,
// seconds and the uniquifier dropped). A name that doesn't carry that shape
// falls back to its bare stem.

/// Minimum bytes in the stamp prefix (YYYYMMDD-HHMMss = 15 chars).
const STAMP_PREFIX_LEN: usize = 15;
/// Byte offset of the date/time separator dash in the stamp.
const STAMP_DASH_POS: usize = 8;

fn human_stamp(name: &str) -> String {
    let stem = name.strip_suffix(".jsonl").unwrap_or(name);
    let raw = stem.as_bytes();
    let stamped = raw.len() >= STAMP_PREFIX_LEN
        && raw[STAMP_DASH_POS] == b'-'
        && raw[..STAMP_PREFIX_LEN]
            .iter()
            .enumerate()
            .all(|(i, b)| i == STAMP_DASH_POS || b.is_ascii_digit());
    if !stamped {
        return stem.to_string();
    }
    format!(
        "{}-{}-{} {}:{}",
        &stem[0..4],
        &stem[4..6],
        &stem[6..8],
        &stem[9..11],
        &stem[11..13]
    )
}
