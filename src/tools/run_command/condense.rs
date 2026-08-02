//! Noise-run condensing for `run_shell_command` output.
//!
//! Measurement across real Session Logs put run_command results at ~11% of
//! Conversation token mass, and roughly half of that is noise lines: cargo
//! compile progress and per-test PASS lines. A passing `cargo test` is ~70%
//! noise, `cargo nextest` ~99%. [`condense`] rewrites the model-facing content
//! the tool returns (BEFORE Shaping caps it): each maximal run of consecutive
//! same-class noise lines keeps its first line (the model still sees the shape
//! of what was omitted) followed by an exact-count marker,
//! `[condense: N more <class> lines omitted]`.
//!
//! The count is exact - never present less output as if it were all
//! (ADR-0039's honesty principle: omission must be self-detecting, not a
//! silent cut). Everything outside a qualifying run passes through verbatim:
//! FAILED/error/warning lines, blanks, and the `[exit code: N]` / timeout tail
//! that [`super::report`] owns and the exit-badge parses. The marker wording is
//! the tool's own (CONTEXT.md: strings a tool produces about its own decisions
//! stay in that tool).

/// Minimum run length that collapses. Below 5 the first-line-plus-marker pair
/// saves two lines at most - not worth trading real output for a marker.
const MIN_RUN: usize = 5;

/// Cargo-style progress prefixes (after leading whitespace).
const COMPILE_PREFIXES: &[&str] = &[
    "Compiling ",
    "Checking ",
    "Downloading ",
    "Downloaded ",
    "Fresh ",
    "Documenting ",
];

/// The noise classes a run may hold. A run must be homogeneous: libtest and
/// nextest passing-test lines have different shapes, so they are distinct
/// classes and never share a run, even though they share a marker label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoiseClass {
    CompileProgress,
    PassingLibtest,
    PassingNextest,
}

impl NoiseClass {
    /// The `<class>` word in the marker.
    fn label(self) -> &'static str {
        match self {
            NoiseClass::CompileProgress => "compile-progress",
            NoiseClass::PassingLibtest | NoiseClass::PassingNextest => "passing-test",
        }
    }
}

/// Classifies one line, or `None` for anything condensing must not touch.
fn classify(line: &str) -> Option<NoiseClass> {
    let trimmed = line.trim_start();
    if COMPILE_PREFIXES.iter().any(|p| trimmed.starts_with(p)) {
        return Some(NoiseClass::CompileProgress);
    }
    // libtest passing line: `test <name> ... ok`, anchored at line start.
    if line.starts_with("test ") && line.ends_with(" ... ok") {
        return Some(NoiseClass::PassingLibtest);
    }
    if trimmed.starts_with("PASS [") {
        return Some(NoiseClass::PassingNextest);
    }
    None
}

/// Rewrites content: each maximal same-class run of >= [`MIN_RUN`] noise lines
/// becomes its first line plus an exact-count marker; every other line - and
/// the line structure around it - survives byte-for-byte.
pub(super) fn condense(content: &str) -> String {
    let lines: Vec<&str> = content.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let Some(class) = classify(lines[i]) else {
            out.push(lines[i].to_string());
            i += 1;
            continue;
        };
        let end = run_end(&lines, i, class);
        emit_run(&mut out, &lines[i..end], class);
        i = end;
    }
    out.join("\n")
}

/// The exclusive end index of the maximal run of `class` lines starting at
/// `start`. A line of any other class (or none) ends the run.
fn run_end(lines: &[&str], start: usize, class: NoiseClass) -> usize {
    let mut end = start + 1;
    while end < lines.len() && classify(lines[end]) == Some(class) {
        end += 1;
    }
    end
}

/// Emits one homogeneous run: collapsed to first line + marker when it meets
/// the threshold, verbatim otherwise. The count is exact (ADR-0039).
fn emit_run(out: &mut Vec<String>, run: &[&str], class: NoiseClass) {
    if run.len() >= MIN_RUN {
        out.push(run[0].to_string());
        out.push(format!(
            "[condense: {} more {} lines omitted]",
            run.len() - 1,
            class.label()
        ));
    } else {
        out.extend(run.iter().map(|s| s.to_string()));
    }
}

#[cfg(test)]
#[path = "../../../tests/tools/run_command/condense.rs"]
mod tests;
