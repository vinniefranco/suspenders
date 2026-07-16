//! Line-level diff hunks for [`crate::plugins::diff`], computed with a
//! Myers/LCS line diff - no dependency.
//!
//! A hunk is a run of changed lines plus up to 3 context lines on each side;
//! changed runs closer than a context gap merge into one hunk, the classic
//! unified-diff grouping. Every changed line lands in exactly one hunk, so
//! [`stats`] over the hunks is complete.
//!
//! ## The line diff (judgment call)
//!
//! baud computes the script with Elixir's stdlib `List.myers_difference/2`,
//! which yields an ordered edit script of `{:eq | :del | :ins, lines}` chunks.
//! Rust has no stdlib equivalent, so [`myers_difference`] reimplements it: a
//! classic Myers shortest-edit-script over the two line lists, walked back into
//! the same `Eq | Del | Ins` chunk sequence. The chunk order matches
//! `List.myers_difference` for the tested inputs - within a change region all
//! deletions precede all insertions (a `Del` chunk before its paired `Ins`
//! chunk), which the `number`/`group` passes and every ported hunks test rely
//! on.

use serde::{Deserialize, Serialize};

const CONTEXT: usize = 3;

/// The tag of one diff line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tag {
    Context,
    Added,
    Removed,
}

/// One diff line: its tag, old-file line number, new-file line number, text.
/// `old`/`new` are `None` where the line has no counterpart on that side
/// (mirrors baud's `nil` line numbers).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    pub tag: Tag,
    pub old: Option<usize>,
    pub new: Option<usize>,
    pub text: String,
}

impl Line {
    fn new(tag: Tag, old: Option<usize>, new: Option<usize>, text: impl Into<String>) -> Self {
        Line {
            tag,
            old,
            new,
            text: text.into(),
        }
    }
}

/// A diff hunk: a slice of tagged lines with old/new start line numbers and
/// counts. Mirrors baud's `hunk` map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub lines: Vec<Line>,
}

/// Total added/removed line counts across the hunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub added: usize,
    pub removed: usize,
}

/// One chunk of the Myers edit script (baud's `{:eq | :del | :ins, texts}`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Chunk {
    Eq(Vec<String>),
    Del(Vec<String>),
    Ins(Vec<String>),
}

/// Diffs two file contents into hunks. Identical contents yield `[]`.
pub fn compute(before: &str, after: &str) -> Vec<Hunk> {
    let before_lines: Vec<String> = before.split('\n').map(str::to_string).collect();
    let after_lines: Vec<String> = after.split('\n').map(str::to_string).collect();

    let script = myers_difference(&before_lines, &after_lines);
    group(&number(&script))
}

/// A created file as one all-added hunk. Diffing against the empty string would
/// show a phantom removed empty line (the empty file IS one empty line to a line
/// differ); a new file has no old side at all.
pub fn all_added(content: &str) -> Vec<Hunk> {
    let trimmed = content.strip_suffix('\n').unwrap_or(content);
    let lines: Vec<Line> = trimmed
        .split('\n')
        .enumerate()
        .map(|(i, text)| Line::new(Tag::Added, None, Some(i + 1), text))
        .collect();
    let count = lines.len();

    vec![Hunk {
        old_start: 0,
        old_count: 0,
        new_start: 1,
        new_count: count,
        lines,
    }]
}

/// Total added/removed line counts across the hunks.
pub fn stats(hunks: &[Hunk]) -> Stats {
    let mut added = 0;
    let mut removed = 0;
    for hunk in hunks {
        for line in &hunk.lines {
            match line.tag {
                Tag::Added => added += 1,
                Tag::Removed => removed += 1,
                Tag::Context => {}
            }
        }
    }
    Stats { added, removed }
}

/// Classic unified-diff text (`@@` headers, `+`/`-`/space prefixes), capped at
/// `max_lines` lines with a trailing elision note when cut.
pub fn to_unified(hunks: &[Hunk], max_lines: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    for hunk in hunks {
        lines.push(format!(
            "@@ -{},{} +{},{} @@",
            hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
        ));
        for line in &hunk.lines {
            let prefixed = match line.tag {
                Tag::Context => format!(" {}", line.text),
                Tag::Added => format!("+{}", line.text),
                Tag::Removed => format!("-{}", line.text),
            };
            lines.push(prefixed);
        }
    }

    if lines.len() <= max_lines {
        lines.join("\n")
    } else {
        let rest = lines.len() - max_lines;
        let mut shown: Vec<String> = lines.into_iter().take(max_lines).collect();
        shown.push(format!("({rest} more diff lines)"));
        shown.join("\n")
    }
}

/// A Myers shortest-edit-script over two line lists, emitted as an ordered
/// chunk script matching Elixir's `List.myers_difference/2`: `Eq` for common
/// runs, `Del` for old-only lines, `Ins` for new-only lines, with all `Del`s of
/// a change region preceding their paired `Ins`s.
fn myers_difference(old: &[String], new: &[String]) -> Vec<Chunk> {
    let n = old.len();
    let m = new.len();
    let max = n + m;
    let offset = max as isize;

    // v[k + offset] = furthest-reaching x on diagonal k. Save a trace of v per
    // edit-distance d so we can walk the path back.
    let mut v = vec![0isize; 2 * max + 1];
    let mut trace: Vec<Vec<isize>> = Vec::new();

    let mut d_final = 0;
    'outer: for d in 0..=(max as isize) {
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let idx = (k + offset) as usize;
            // Arrive by a down move (insertion, from k+1) or a right move
            // (deletion, from k-1).
            let mut x = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) {
                v[idx + 1] // down: insertion
            } else {
                v[idx - 1] + 1 // right: deletion
            };
            let mut y = x - k;
            // Follow the diagonal (common lines).
            while (x as usize) < n && (y as usize) < m && old[x as usize] == new[y as usize] {
                x += 1;
                y += 1;
            }
            v[idx] = x;
            if x as usize >= n && y as usize >= m {
                d_final = d;
                break 'outer;
            }
            k += 2;
        }
    }

    coalesce(&backtrack(old, new, &trace, d_final, offset))
}

/// Kind code of a per-line step: 0 = Eq, 1 = Del, 2 = Ins.
type Step = (u8, String);

/// Walks the saved Myers trace back to the origin, collecting per-line steps in
/// forward order (Eq / Del / Ins).
fn backtrack(
    old: &[String],
    new: &[String],
    trace: &[Vec<isize>],
    d_final: isize,
    offset: isize,
) -> Vec<Step> {
    let mut x = old.len() as isize;
    let mut y = new.len() as isize;
    let mut steps: Vec<Step> = Vec::new(); // collected end-to-start

    for d in (0..=d_final).rev() {
        let v = &trace[d as usize];
        let k = x - y;
        let idx = (k + offset) as usize;

        // Which move produced this d-step: down (insertion) or right (deletion)?
        let prev_k = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) {
            k + 1 // came from a down move
        } else {
            k - 1 // came from a right move
        };
        let prev_x = v[(prev_k + offset) as usize];
        let prev_y = prev_x - prev_k;

        // Follow the diagonal (snake) back down to the move point: these are Eq.
        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
            steps.push((0, old[x as usize].clone()));
        }

        if d > 0 {
            if x == prev_x {
                // down move: an insertion of new[y-1].
                y -= 1;
                steps.push((2, new[y as usize].clone()));
            } else {
                // right move: a deletion of old[x-1].
                x -= 1;
                steps.push((1, old[x as usize].clone()));
            }
        }
    }

    steps.reverse();
    steps
}

/// Coalesces per-line steps into runs and orders `Del` before `Ins` inside each
/// change region, matching `List.myers_difference/2`. A Myers backtrack can
/// yield either del/ins order depending on move choice, so we normalize.
fn coalesce(steps: &[Step]) -> Vec<Chunk> {
    // Merge consecutive same-kind steps into runs.
    let mut runs: Vec<(u8, Vec<String>)> = Vec::new();
    for (kind, text) in steps {
        match runs.last_mut() {
            Some((k, texts)) if k == kind => texts.push(text.clone()),
            _ => runs.push((*kind, vec![text.clone()])),
        }
    }

    // Emit, gathering each contiguous change region and emitting its Dels then
    // Ins in that fixed order.
    let mut result: Vec<Chunk> = Vec::new();
    let mut i = 0;
    while i < runs.len() {
        if runs[i].0 == 0 {
            result.push(Chunk::Eq(runs[i].1.clone()));
            i += 1;
            continue;
        }
        let mut dels: Vec<String> = Vec::new();
        let mut inss: Vec<String> = Vec::new();
        while i < runs.len() && runs[i].0 != 0 {
            if runs[i].0 == 1 {
                dels.extend(runs[i].1.clone());
            } else {
                inss.extend(runs[i].1.clone());
            }
            i += 1;
        }
        if !dels.is_empty() {
            result.push(Chunk::Del(dels));
        }
        if !inss.is_empty() {
            result.push(Chunk::Ins(inss));
        }
    }

    result
}

/// Flattens the Myers script into tagged lines carrying their old/new file line
/// numbers (baud's `number/1`).
fn number(script: &[Chunk]) -> Vec<Line> {
    let mut lines: Vec<Line> = Vec::new();
    let mut old = 1usize;
    let mut new = 1usize;

    for chunk in script {
        match chunk {
            Chunk::Eq(texts) => {
                for (i, text) in texts.iter().enumerate() {
                    lines.push(Line::new(Tag::Context, Some(old + i), Some(new + i), text));
                }
                old += texts.len();
                new += texts.len();
            }
            Chunk::Del(texts) => {
                for (i, text) in texts.iter().enumerate() {
                    lines.push(Line::new(Tag::Removed, Some(old + i), None, text));
                }
                old += texts.len();
            }
            Chunk::Ins(texts) => {
                for (i, text) in texts.iter().enumerate() {
                    lines.push(Line::new(Tag::Added, None, Some(new + i), text));
                }
                new += texts.len();
            }
        }
    }

    lines
}

/// Clusters changed line indices (merging runs whose expanded context would
/// touch) and slices each cluster with its context (baud's `group/1`).
fn group(lines: &[Line]) -> Vec<Hunk> {
    let changed: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.tag != Tag::Context)
        .map(|(i, _)| i)
        .collect();

    cluster(&changed)
        .into_iter()
        .map(|range| build_hunk(lines, range))
        .collect()
}

/// Clusters changed indices into `(first, last)` ranges, merging runs whose gap
/// is within a context window on each side (baud's `cluster/1`).
fn cluster(changed: &[usize]) -> Vec<(usize, usize)> {
    if changed.is_empty() {
        return Vec::new();
    }

    let mut clusters: Vec<(usize, usize)> = vec![(changed[0], changed[0])];
    for &index in &changed[1..] {
        let (start, last) = *clusters.last().unwrap();
        if index - last <= CONTEXT * 2 + 1 {
            *clusters.last_mut().unwrap() = (start, index);
        } else {
            clusters.push((index, index));
        }
    }
    clusters
}

/// Slices a cluster with up to `CONTEXT` context lines each side and computes
/// its old/new start/count (baud's `build_hunk/2`).
fn build_hunk(lines: &[Line], (first_changed, last_changed): (usize, usize)) -> Hunk {
    let lo = first_changed.saturating_sub(CONTEXT);
    let hi = (last_changed + CONTEXT).min(lines.len() - 1);
    let slice: Vec<Line> = lines[lo..=hi].to_vec();

    let old_nos: Vec<usize> = slice.iter().filter_map(|l| l.old).collect();
    let new_nos: Vec<usize> = slice.iter().filter_map(|l| l.new).collect();

    Hunk {
        old_start: old_nos.first().copied().unwrap_or(0),
        old_count: old_nos.len(),
        new_start: new_nos.first().copied().unwrap_or(0),
        new_count: new_nos.len(),
        lines: slice,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(lines: &[&str]) -> String {
        lines.join("\n")
    }

    // ---- compute/2 ----

    #[test]
    fn identical_contents_yield_no_hunks() {
        let content = file(&["a", "b", "c"]);
        assert_eq!(compute(&content, &content), Vec::new());
    }

    #[test]
    fn one_changed_line_gets_3_context_lines_each_side_with_line_numbers() {
        let before = file(&["l1", "l2", "l3", "l4", "l5", "l6", "l7", "l8", "l9", "l10"]);
        let changed = file(&["l1", "l2", "l3", "l4", "L5", "l6", "l7", "l8", "l9", "l10"]);

        let hunks = compute(&before, &changed);
        assert_eq!(hunks.len(), 1);
        let hunk = &hunks[0];
        assert_eq!(hunk.old_start, 2);
        assert_eq!(hunk.old_count, 7);
        assert_eq!(hunk.new_start, 2);
        assert_eq!(hunk.new_count, 7);

        assert_eq!(
            hunk.lines,
            vec![
                Line::new(Tag::Context, Some(2), Some(2), "l2"),
                Line::new(Tag::Context, Some(3), Some(3), "l3"),
                Line::new(Tag::Context, Some(4), Some(4), "l4"),
                Line::new(Tag::Removed, Some(5), None, "l5"),
                Line::new(Tag::Added, None, Some(5), "L5"),
                Line::new(Tag::Context, Some(6), Some(6), "l6"),
                Line::new(Tag::Context, Some(7), Some(7), "l7"),
                Line::new(Tag::Context, Some(8), Some(8), "l8"),
            ]
        );
    }

    #[test]
    fn a_change_at_the_top_has_no_leading_context_to_invent() {
        let before = file(&["l1", "l2", "l3", "l4", "l5"]);
        let changed = file(&["L1", "l2", "l3", "l4", "l5"]);

        let hunks = compute(&before, &changed);
        assert_eq!(hunks.len(), 1);
        let lines = &hunks[0].lines;
        assert_eq!(lines[0], Line::new(Tag::Removed, Some(1), None, "l1"));
        assert_eq!(lines[1], Line::new(Tag::Added, None, Some(1), "L1"));
        let context = &lines[2..];
        assert_eq!(context.len(), 3);
    }

    #[test]
    fn far_apart_changes_split_into_separate_hunks() {
        let before_lines: Vec<String> = (1..=20).map(|i| format!("line{i}")).collect();
        let mut changed_lines = before_lines.clone();
        changed_lines[1] = "CHANGED2".to_string();
        changed_lines[17] = "CHANGED18".to_string();

        let before = before_lines.join("\n");
        let changed = changed_lines.join("\n");
        let hunks = compute(&before, &changed);
        assert_eq!(hunks.len(), 2);
        assert!(
            hunks[0]
                .lines
                .iter()
                .any(|l| *l == Line::new(Tag::Added, None, Some(2), "CHANGED2"))
        );
        assert!(
            hunks[1]
                .lines
                .iter()
                .any(|l| *l == Line::new(Tag::Added, None, Some(18), "CHANGED18"))
        );
    }

    #[test]
    fn nearby_changes_merge_into_one_hunk() {
        let before_lines: Vec<String> = (1..=12).map(|i| format!("line{i}")).collect();
        let mut changed_lines = before_lines.clone();
        changed_lines[3] = "CHANGED4".to_string();
        changed_lines[7] = "CHANGED8".to_string();

        let hunks = compute(&before_lines.join("\n"), &changed_lines.join("\n"));
        assert_eq!(hunks.len(), 1);
        let lines = &hunks[0].lines;
        assert!(
            lines
                .iter()
                .any(|l| *l == Line::new(Tag::Added, None, Some(4), "CHANGED4"))
        );
        assert!(
            lines
                .iter()
                .any(|l| *l == Line::new(Tag::Added, None, Some(8), "CHANGED8"))
        );
    }

    #[test]
    fn pure_insertion_carries_only_added_and_context_lines() {
        let before = file(&["a", "b"]);
        let changed = file(&["a", "new", "b"]);

        let hunks = compute(&before, &changed);
        assert_eq!(hunks.len(), 1);
        let lines = &hunks[0].lines;
        assert!(
            lines
                .iter()
                .any(|l| *l == Line::new(Tag::Added, None, Some(2), "new"))
        );
        assert!(!lines.iter().any(|l| l.tag == Tag::Removed));
    }

    // ---- all_added/1 ----

    #[test]
    fn a_created_file_is_one_all_added_hunk_no_phantom_removed_line() {
        let hunks = all_added("a\nb\n");
        assert_eq!(hunks.len(), 1);
        let hunk = &hunks[0];
        assert_eq!(hunk.old_count, 0);
        assert_eq!(hunk.new_start, 1);
        assert_eq!(hunk.new_count, 2);
        assert_eq!(
            hunk.lines,
            vec![
                Line::new(Tag::Added, None, Some(1), "a"),
                Line::new(Tag::Added, None, Some(2), "b"),
            ]
        );
    }

    // ---- stats/1 ----

    #[test]
    fn counts_every_added_and_removed_line_across_hunks() {
        let before_lines: Vec<String> = (1..=20).map(|i| format!("line{i}")).collect();
        let mut changed_lines = before_lines.clone();
        changed_lines[1] = "CHANGED2".to_string();
        changed_lines.remove(17);

        let hunks = compute(&before_lines.join("\n"), &changed_lines.join("\n"));
        let stats = stats(&hunks);
        assert_eq!(
            stats,
            Stats {
                added: 1,
                removed: 2
            }
        );
    }

    // ---- to_unified/2 ----

    #[test]
    fn renders_headers_and_prefixed_lines() {
        let before = file(&["l1", "l2", "l3", "l4", "l5", "l6", "l7", "l8", "l9", "l10"]);
        let changed = file(&["l1", "l2", "l3", "l4", "L5", "l6", "l7", "l8", "l9", "l10"]);

        let unified = to_unified(&compute(&before, &changed), 100);

        assert_eq!(
            unified,
            "@@ -2,7 +2,7 @@\n l2\n l3\n l4\n-l5\n+L5\n l6\n l7\n l8"
        );
    }

    #[test]
    fn caps_at_max_lines_with_an_elision_note() {
        let before: String = (1..=30)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let changed: String = (1..=30)
            .map(|i| format!("LINE{i}"))
            .collect::<Vec<_>>()
            .join("\n");

        let unified = to_unified(&compute(&before, &changed), 5);

        assert_eq!(unified.split('\n').count(), 6);
        assert!(unified.contains("more diff lines)"));
    }
}
