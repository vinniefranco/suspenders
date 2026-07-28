//! The condense Extension: collapses noise runs in run_command results.
//!
//! Measurement across real Session Logs put run_command results at ~11% of
//! Conversation token mass, and roughly half of that is noise lines: cargo
//! compile progress and per-test PASS lines. A passing `cargo test` is ~70%
//! noise, `cargo nextest` ~99%. [`post_run`](Condense::post_run) rewrites the
//! model-facing content BEFORE Shaping: each maximal run of consecutive
//! same-class noise lines keeps its first line (the model still sees the shape
//! of what was omitted) followed by an exact-count marker,
//! `[condense: N more <class> lines omitted]`.
//!
//! The count is exact - never present less output as if it were all
//! (ADR-0039's honesty principle: omission must be self-detecting, not a
//! silent cut). Everything outside a qualifying run passes through verbatim:
//! FAILED/error/warning lines, blanks, and the `[exit code: N]` / timeout tail
//! that [`crate::tools::run_command::report`] owns and the exit-badge extension
//! parses. The marker wording is this Extension's own (CONTEXT.md: strings an
//! Extension produces about its own decisions stay in that Extension).
//!
//! This is a Middleware-only Extension (ADR-0042): it rewrites model-facing
//! content in `post_run` and has no display-side Presenter role.

use serde_json::Value;

use crate::middleware::{Middleware, Token};

/// The one tool this extension acts on.
const TOOL: &str = "run_command";

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

/// The condense extension.
pub struct Condense;

impl Middleware for Condense {
    fn post_run(&self, mut token: Token, _opts: &Value) -> Token {
        // Pass and fail alike: a failing run carries compile-progress noise too.
        if token.tool != TOOL {
            return token;
        }
        if let Some(result) = token.result.as_mut() {
            result.content = condense(&result.content);
        }
        token
    }
}

/// Classifies one line, or `None` for anything this extension must not touch.
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
fn condense(content: &str) -> String {
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
mod tests {
    use super::*;
    use crate::middleware::token::TokenResult;
    use crate::tool::ToolCtx;
    use serde_json::json;

    fn ctx() -> ToolCtx {
        ToolCtx {
            root: "/nowhere".into(),
            result_cap: 10_000,
            command_timeout_ms: 120_000,
        }
    }

    fn token_with(tool: &str, content: &str, is_error: bool) -> Token {
        let mut token = Token::new(tool, json!({"command": "cargo test"}), ctx());
        token.result = Some(TokenResult {
            content: content.to_string(),
            is_error,
        });
        token
    }

    fn run(content: &str, is_error: bool) -> String {
        Condense
            .post_run(token_with(TOOL, content, is_error), &json!({}))
            .result
            .unwrap()
            .content
    }

    #[test]
    fn collapses_a_long_run_of_libtest_ok_lines_keeping_first_line_and_exact_count() {
        let content = "running 6 tests\n\
            test a::one ... ok\n\
            test a::two ... ok\n\
            test a::three ... ok\n\
            test a::four ... ok\n\
            test a::five ... ok\n\
            test a::six ... ok\n\
            test result: ok. 6 passed; 0 failed";

        let condensed = run(content, false);

        assert_eq!(
            condensed,
            "running 6 tests\n\
             test a::one ... ok\n\
             [condense: 5 more passing-test lines omitted]\n\
             test result: ok. 6 passed; 0 failed"
        );
    }

    #[test]
    fn collapses_nextest_pass_lines_and_cargo_compile_progress() {
        let content = "   Compiling serde v1.0.0\n\
            \x20  Compiling tokio v1.0.0\n\
            \x20  Compiling suspenders v0.1.0\n\
            \x20  Checking foo v0.1.0\n\
            \x20  Fresh bar v0.2.0\n\
            starting tests\n\
            \x20   PASS [ 0.01s] suspenders a::one\n\
            \x20   PASS [ 0.01s] suspenders a::two\n\
            \x20   PASS [ 0.02s] suspenders a::three\n\
            \x20   PASS [ 0.01s] suspenders a::four\n\
            \x20   PASS [ 0.03s] suspenders a::five\n\
            Summary [ 0.10s] 5 tests run: 5 passed";

        let condensed = run(content, false);

        assert_eq!(
            condensed,
            "   Compiling serde v1.0.0\n\
             [condense: 4 more compile-progress lines omitted]\n\
             starting tests\n\
             \x20   PASS [ 0.01s] suspenders a::one\n\
             [condense: 4 more passing-test lines omitted]\n\
             Summary [ 0.10s] 5 tests run: 5 passed"
        );
    }

    #[test]
    fn a_run_of_four_noise_lines_is_left_untouched() {
        let content = "test a ... ok\n\
            test b ... ok\n\
            test c ... ok\n\
            test d ... ok";

        assert_eq!(run(content, false), content);
    }

    #[test]
    fn a_failed_line_splits_the_block_and_survives_verbatim() {
        let content = "test a ... ok\n\
            test b ... ok\n\
            test c ... ok\n\
            test d ... ok\n\
            test e ... ok\n\
            test boom ... FAILED\n\
            test f ... ok\n\
            test g ... ok\n\
            test h ... ok\n\
            test i ... ok\n\
            test j ... ok\n\
            test k ... ok";

        let condensed = run(content, false);

        assert_eq!(
            condensed,
            "test a ... ok\n\
             [condense: 4 more passing-test lines omitted]\n\
             test boom ... FAILED\n\
             test f ... ok\n\
             [condense: 5 more passing-test lines omitted]"
        );
    }

    #[test]
    fn an_is_error_result_still_gets_its_compile_progress_preamble_collapsed() {
        let content = "   Compiling a v0.1.0\n\
            \x20  Compiling b v0.1.0\n\
            \x20  Compiling c v0.1.0\n\
            \x20  Compiling d v0.1.0\n\
            \x20  Compiling e v0.1.0\n\
            error[E0308]: mismatched types\n\
            [exit code: 101]";

        let condensed = run(content, true);

        assert_eq!(
            condensed,
            "   Compiling a v0.1.0\n\
             [condense: 4 more compile-progress lines omitted]\n\
             error[E0308]: mismatched types\n\
             [exit code: 101]"
        );
    }

    #[test]
    fn the_exit_code_tail_survives_and_still_parses() {
        let content = "test a ... ok\n\
            test b ... ok\n\
            test c ... ok\n\
            test d ... ok\n\
            test e ... ok\n\
            test f ... ok\n\
            [exit code: 0]";

        let condensed = run(content, false);

        assert!(condensed.ends_with("\n[exit code: 0]"));
        assert_eq!(
            crate::tools::run_command::parse_exit_code(&condensed),
            Some(0)
        );
    }

    #[test]
    fn a_non_run_command_token_passes_through_unchanged() {
        let noisy = "test a ... ok\n\
            test b ... ok\n\
            test c ... ok\n\
            test d ... ok\n\
            test e ... ok";
        let token = Condense.post_run(token_with("read_file", noisy, false), &json!({}));

        assert_eq!(token.result.unwrap().content, noisy);
    }

    #[test]
    fn libtest_and_nextest_passing_lines_do_not_share_a_run() {
        // 3 libtest + 3 nextest lines: 6 passing lines total, but two
        // heterogeneous runs of 3 - neither meets the threshold alone.
        let content = "test a ... ok\n\
            test b ... ok\n\
            test c ... ok\n\
            PASS [ 0.01s] crate a\n\
            PASS [ 0.01s] crate b\n\
            PASS [ 0.01s] crate c";

        assert_eq!(run(content, false), content);
    }

    #[test]
    fn registry_resolves_condense_and_the_default_config_ships_it() {
        let extensions = crate::extensions::configured(&["condense".to_string()]);
        assert_eq!(extensions.len(), 1);
        assert_eq!(extensions[0].name, "condense");

        let base = crate::session::SessionConfig::base();
        assert!(base.extensions.contains(&"condense".to_string()));
    }
}
