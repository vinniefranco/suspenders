//! Assembles a Myers edit script into displayable [`Hunk`]s (baud's
//! `number/1` -> `group/1` -> `build_hunk/2` pipeline): number every line with
//! its old/new file position, cluster the changed lines into context windows,
//! and slice each cluster into a hunk. The pure back half of [`super::compute`];
//! it knows the hunk model but not the diff algorithm.
//!
//! The pipeline is one cohesive unit around the numbered line list, so it is an
//! [`Assembler`] whose methods each work off that shared `lines` field rather
//! than a scatter of free functions threading it between them.

use super::{CONTEXT, Hunk, Line, Tag, myers};

/// Turns a Myers script into hunks: number the lines, then group the changes
/// into context-windowed hunks.
pub(super) fn hunks(script: &[myers::Chunk]) -> Vec<Hunk> {
    Assembler::from_script(script).group()
}

/// The numbered line list a Myers script produces, and the hunk-assembly over
/// it. One value so numbering, clustering, and slicing all read the same lines.
struct Assembler {
    lines: Vec<Line>,
}

impl Assembler {
    /// Flattens the Myers script into tagged lines carrying their old/new file
    /// line numbers (baud's `number/1`).
    fn from_script(script: &[myers::Chunk]) -> Self {
        use myers::Chunk;
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

        Assembler { lines }
    }

    /// Clusters the changed line indices (merging runs whose expanded context
    /// would touch) and slices each cluster with its context (baud's `group/1`).
    fn group(&self) -> Vec<Hunk> {
        let changed: Vec<usize> = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.tag != Tag::Context)
            .map(|(i, _)| i)
            .collect();

        self.cluster(&changed)
            .into_iter()
            .map(|range| self.build_hunk(range))
            .collect()
    }

    /// Clusters changed indices into `(first, last)` ranges, merging runs whose
    /// gap is within a context window on each side (baud's `cluster/1`).
    fn cluster(&self, changed: &[usize]) -> Vec<(usize, usize)> {
        let Some((&first, rest)) = changed.split_first() else {
            return Vec::new();
        };

        // Carry the open cluster in `current` so we never re-fetch the last
        // element: a run within the context window extends it, a wider gap seals
        // it and opens a new one.
        let mut clusters: Vec<(usize, usize)> = Vec::new();
        let mut current = (first, first);
        for &index in rest {
            let (start, last) = current;
            if index - last <= CONTEXT * 2 + 1 {
                current = (start, index);
            } else {
                clusters.push(current);
                current = (index, index);
            }
        }
        clusters.push(current);
        clusters
    }

    /// Slices a cluster with up to `CONTEXT` context lines each side and computes
    /// its old/new start/count (baud's `build_hunk/2`).
    fn build_hunk(&self, (first_changed, last_changed): (usize, usize)) -> Hunk {
        let lo = first_changed.saturating_sub(CONTEXT);
        let hi = (last_changed + CONTEXT).min(self.lines.len() - 1);
        let slice: Vec<Line> = self.lines[lo..=hi].to_vec();

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
}
