//! The Myers shortest-edit-script over two line lists (baud's
//! `List.myers_difference/2`). This is the pure diff algorithm - it knows
//! nothing of Hunks or line numbering; it emits an ordered [`Chunk`] script that
//! [`super::number`] flattens into tagged, numbered [`super::Line`]s.

/// One chunk of the Myers edit script (baud's `{:eq | :del | :ins, texts}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Chunk {
    Eq(Vec<String>),
    Del(Vec<String>),
    Ins(Vec<String>),
}

/// A Myers shortest-edit-script over two line lists, emitted as an ordered
/// chunk script matching Elixir's `List.myers_difference/2`: `Eq` for common
/// runs, `Del` for old-only lines, `Ins` for new-only lines, with all `Del`s of
/// a change region preceding their paired `Ins`s.
pub(super) fn difference(old: &[String], new: &[String]) -> Vec<Chunk> {
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
