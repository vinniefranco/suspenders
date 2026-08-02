use super::*;

// The behavior is a pure text rewrite the tool applies to its output (both the
// Ok and the completed-but-failed arm); `is_error` is threaded through the test
// names for documentation, but condensing itself does not depend on it.
fn run(content: &str, _is_error: bool) -> String {
    condense(content)
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
fn non_noise_content_passes_through_verbatim() {
    // Condensing is conservative: ordinary output with no qualifying noise run
    // (compile-progress / passing-test) is returned byte-for-byte. Only
    // run_shell_command feeds this function (it is the sole caller), so no other
    // tool's output is ever touched - a structural guarantee, not a tool check.
    let plain = "hello world\n\
            building the thing\n\
            all done, shipping it\n\
            one last line";

    assert_eq!(run(plain, false), plain);
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
