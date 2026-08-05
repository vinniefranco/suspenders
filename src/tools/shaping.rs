//! Applies the Result Cap (CONTEXT.md): the size ceiling one Tool Result may
//! occupy in the Conversation, derived from the Context Budget once per
//! Session.
//!
//! `Tools::run` shapes every Tool Result through here; individual tools carry
//! no size logic - each DECLARES its [`CutPolicy`] on its spec (like
//! [`Kind`](crate::approvals::Kind)), and this module applies the declared
//! policy with zero knowledge of tool names. Callers pass the raw Tool Call
//! input alongside the policy; a [`CutPolicy::HeadWithResume`] cut reads the
//! input's `offset` so its resume point is file-absolute. [`CutPolicy::HeadTail`]
//! keeps the start AND end (run_shell_command: the exit code and last errors
//! live at the end); [`CutPolicy::HeadWithResume`] cuts at a line boundary and
//! its marker names the exact 0-based `offset` that continues the read
//! (read_file - qwen's param, ADR-0060); the default [`CutPolicy::Head`] keeps
//! the start.
//!
//! This Result-Cap fold is a suspenders concern with no qwen equivalent (qwen
//! truncates by a per-line char limit inside the tool). The marker only needs to
//! name the real qwen param the model would pass to continue: `offset` (0-based),
//! plus `limit` to page a bounded window.

use crate::content::ResultBlock;
use crate::tool::CutPolicy;
use crate::voice;
use serde_json::Value;

/// Even a tiny Context Budget gets a usable file read (~1.1k tokens).
const FLOOR_CHARS: usize = 4_000;

/// The [`CutPolicy::HeadTail`] head:tail split. The tail carries the signal.
const HEAD_QUARTER: usize = 4;

/// The Result Cap numerator: `3.5 chars/token * 1/16 of the window` expressed as
/// the exact rational `7 / 32` (`3.5 = 7/2`, times `1/16`, is `7/32`), applied
/// as integer `window * CAP_NUMERATOR / CAP_DENOMINATOR` to avoid float rounding.
const CAP_NUMERATOR: u64 = 7;
/// The Result Cap denominator (see [`CAP_NUMERATOR`]).
const CAP_DENOMINATOR: u64 = 32;

/// Derives the Result Cap in chars from the Context Budget and the reply
/// reserve: a sixteenth of the Conversation window, at 3.5 chars per token,
/// floored at 4000 chars (`window * 7 / 32`).
pub fn cap_for(context_budget: u64, max_tokens_reserve: u64) -> usize {
    let window_tokens = context_budget.saturating_sub(max_tokens_reserve);
    // window_tokens * 3.5 chars/token, a sixteenth of it: window * 7 / 32.
    ((window_tokens * CAP_NUMERATOR / CAP_DENOMINATOR) as usize).max(FLOOR_CHARS)
}

/// Shapes one Tool Result's block list to the Result Cap (ADR-0059). The cap
/// applies to the TEXT blocks only - the Text blocks are concatenated, cut as
/// before, and returned as one Text block; media blocks (image, PDF document)
/// pass through uncapped, keeping their position relative to the text. `policy`
/// is the [`CutPolicy`] the tool declared on its spec; `input` is the raw Tool
/// Call input: a [`CutPolicy::HeadWithResume`] cut reads its `offset` so the
/// resume marker is file-absolute, every other policy ignores it.
///
/// The common case is a single Text block in, a single (possibly cut) Text block
/// out - byte-identical to the old `&str` shaping.
pub fn shape(
    policy: CutPolicy,
    input: &Value,
    blocks: Vec<ResultBlock>,
    cap: usize,
) -> Vec<ResultBlock> {
    let (text, media, text_first) = split_text_and_media(blocks);
    let shaped_text = shape_text(policy, input, &text, cap);

    // A text-only result (the common case) is one Text block. When media rides,
    // keep the text ahead of the media unless the media led the original list.
    if media.is_empty() {
        return vec![ResultBlock::text(shaped_text)];
    }
    let text_block = ResultBlock::text(shaped_text);
    let mut out = Vec::with_capacity(media.len() + 1);
    if text_first {
        out.push(text_block);
        out.extend(media);
    } else {
        out.extend(media);
        out.push(text_block);
    }
    out
}

/// Splits a block list into (concatenated text, media blocks in order, whether
/// text led the list). Text blocks fold into one string; media blocks (image,
/// document) keep their order. `text_first` is true unless a media block opens
/// the list, so a leading image keeps its place ahead of trailing text.
fn split_text_and_media(blocks: Vec<ResultBlock>) -> (String, Vec<ResultBlock>, bool) {
    let text_first = !matches!(blocks.first(), Some(ResultBlock::Image { .. }))
        && !matches!(blocks.first(), Some(ResultBlock::Document { .. }));
    let mut text = String::new();
    let mut media = Vec::new();
    for block in blocks {
        match block {
            ResultBlock::Text { text: t } => text.push_str(&t),
            media_block => media.push(media_block),
        }
    }
    (text, media, text_first)
}

/// Cuts the concatenated text to the cap (the original `&str` shaping). Text
/// within the cap passes through untouched.
fn shape_text(policy: CutPolicy, input: &Value, content: &str, cap: usize) -> String {
    let total = content.chars().count();
    if total <= cap {
        content.to_string()
    } else {
        cut(policy, content, cap, total, read_offset(policy, input))
    }
}

// Only a HeadWithResume cut resumes at an absolute line; every other policy's
// input carries nothing Shaping needs. The window param is `offset` (0-based,
// qwen's read_file param); a missing / non-integer offset is treated as 0.
fn read_offset(policy: CutPolicy, input: &Value) -> Option<i64> {
    if policy == CutPolicy::HeadWithResume {
        input.get("offset").and_then(|v| v.as_i64())
    } else {
        None
    }
}

fn cut(policy: CutPolicy, content: &str, cap: usize, total: usize, offset: Option<i64>) -> String {
    match policy {
        CutPolicy::HeadTail => {
            let head = cap / HEAD_QUARTER;
            let tail = cap - head;
            format!(
                "{}{}{}",
                char_slice(content, 0, head),
                voice::omitted_middle(total - cap, total),
                char_slice(content, total - tail, tail),
            )
        }
        CutPolicy::HeadWithResume => resume_cut(content, cap, total, offset),
        CutPolicy::Head => head_cut(content, cap, total),
    }
}

// The HeadWithResume cut: cut at a line boundary and name the absolute 0-based
// resume offset. A first line wider than the whole cap falls back to the
// generic head cut. `offset` is the Tool Call's 0-based offset (qwen's
// read_file param); the shaped content's first line is file-absolute line
// `offset + 1`.
fn resume_cut(content: &str, cap: usize, total: usize, offset: Option<i64>) -> String {
    let lines: Vec<&str> = content.split('\n').collect();
    let kept = whole_lines_within(&lines, cap);

    if kept == 0 {
        head_cut(content, cap, total)
    } else {
        // Convert the 0-based offset to a 1-based start line for the file-
        // absolute line numbers the marker displays.
        let start = resolve_offset(offset) + 1;
        let last_shown = start + kept - 1;
        let last_line = start + line_count(content, &lines) - 1;

        let body = lines[..kept].join("\n");
        // The marker names the 0-based offset that continues the read: line
        // `last_shown + 1` (1-based) is offset `last_shown` (0-based).
        format!("{}{}", body, voice::truncated_file(last_shown, last_line))
    }
}

fn head_cut(content: &str, cap: usize, total: usize) -> String {
    format!(
        "{}{}",
        char_slice(content, 0, cap),
        voice::truncated_output(total, cap)
    )
}

// How many whole lines (joined by newlines) fit within cap chars.
fn whole_lines_within(lines: &[&str], cap: usize) -> usize {
    // The -1 start pays back the first line's joining newline (as in baud).
    let mut chars: i64 = -1;
    let mut kept = 0usize;
    for line in lines {
        chars += line.chars().count() as i64 + 1;
        if chars <= cap as i64 {
            kept += 1;
        } else {
            break;
        }
    }
    kept
}

// A trailing newline splits into a final empty string that is not a line.
fn line_count(content: &str, lines: &[&str]) -> usize {
    if content.ends_with('\n') {
        lines.len() - 1
    } else {
        lines.len()
    }
}

// The Tool Call's 0-based `offset`, clamped to a non-negative usize. A
// missing / negative / non-integer offset is a full read from the top (0).
fn resolve_offset(offset: Option<i64>) -> usize {
    match offset {
        Some(o) if o >= 0 => o as usize,
        _ => 0,
    }
}

// Slice by chars (Elixir String.slice semantics), not bytes.
fn char_slice(s: &str, start: usize, len: usize) -> String {
    s.chars().skip(start).take(len).collect()
}

#[cfg(test)]
#[path = "../../tests/tools/shaping.rs"]
mod tests;
