use super::*;
use crate::middleware::token::TokenResult;
use crate::tool::ToolCtx;
use serde_json::json;

fn ctx() -> ToolCtx {
    ToolCtx::for_test("/nowhere".into(), 10_000)
}

fn token_with(tool: &str, content: &str, is_error: bool) -> Token {
    let mut token = Token::new(tool, json!({"command": "cargo test"}), ctx());
    token.result = Some(TokenResult::text(content, is_error));
    token
}

fn run(content: &str, is_error: bool) -> String {
    Condense
        .post_run(token_with(TOOL, content, is_error), &json!({}))
        .result
        .unwrap()
        .text_of()
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

    assert_eq!(token.result.unwrap().text_of(), noisy);
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
