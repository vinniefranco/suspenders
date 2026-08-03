//! Tests for the plan-mode shell classifier ([`crate::shell_safety`], ADR-0067
//! Phase 4b) - a faithful port of qwen's `shellAstParser.ts`. These pin the
//! read-only / write / unknown classification of the fn directly; the mapping to
//! `Verdict` through `Approvals::classify` in Plan mode is pinned in
//! `tests/approvals.rs`.

use super::*;
use ShellCommandSafety::{ReadOnly, Unknown, Write};

fn c(command: &str) -> ShellCommandSafety {
    classify_shell_command_safety(command)
}

// ---- read-only roots ----

#[test]
fn read_only_basic_commands() {
    assert_eq!(c("ls -la"), ReadOnly);
    assert_eq!(c("ls -la /tmp"), ReadOnly);
    assert_eq!(c("cat f"), ReadOnly);
    assert_eq!(c("cat /etc/hosts"), ReadOnly);
    assert_eq!(c("pwd"), ReadOnly);
    assert_eq!(c("whoami"), ReadOnly);
    assert_eq!(c("head -n 5 f"), ReadOnly);
    assert_eq!(c("wc -l f"), ReadOnly);
    assert_eq!(c("echo hello"), ReadOnly);
}

#[test]
fn read_only_grep_recursive() {
    assert_eq!(c("grep -r x ."), ReadOnly);
    assert_eq!(c("grep -rn pattern src"), ReadOnly);
}

#[test]
fn read_only_git_subcommands() {
    assert_eq!(c("git status"), ReadOnly);
    assert_eq!(c("git log"), ReadOnly);
    assert_eq!(c("git diff"), ReadOnly);
    assert_eq!(c("git show HEAD"), ReadOnly);
    assert_eq!(c("git branch"), ReadOnly);
    assert_eq!(c("git branch --list"), ReadOnly);
    assert_eq!(c("git remote"), ReadOnly);
    assert_eq!(c("git remote show origin"), ReadOnly);
    assert_eq!(c("git --version"), ReadOnly);
}

// ---- write roots ----

#[test]
fn write_mutating_roots() {
    assert_eq!(c("rm f"), Write);
    assert_eq!(c("rm -rf dir"), Write);
    assert_eq!(c("mv a b"), Write);
    assert_eq!(c("cp a b"), Write);
    assert_eq!(c("mkdir d"), Write);
    assert_eq!(c("touch f"), Write);
    assert_eq!(c("chmod +x f"), Write);
}

#[test]
fn write_git_mutations() {
    assert_eq!(c("git commit -m x"), Write);
    assert_eq!(c("git push"), Write);
    assert_eq!(c("git add ."), Write);
    assert_eq!(c("git checkout main"), Write);
    assert_eq!(c("git reset --hard"), Write);
    assert_eq!(c("git remote add x y"), Write);
    assert_eq!(c("git branch -d topic"), Write);
    assert_eq!(c("git branch topic"), Write);
}

#[test]
fn write_git_dry_run_is_unknown() {
    // a dry-run mutation is not a write but not clearly read-only either.
    assert_eq!(c("git push --dry-run"), Unknown);
    assert_eq!(c("git add -n ."), Unknown);
}

#[test]
fn write_sed_in_place() {
    assert_eq!(c("sed -i s/a/b/ f"), Write);
    assert_eq!(c("sed -i 's/a/b/g' f"), Write);
    assert_eq!(c("sed --in-place=bak s/a/b/ f"), Write);
}

#[test]
fn read_only_sed_substitution() {
    assert_eq!(c("sed 's/a/b/' f"), ReadOnly);
    assert_eq!(c("sed -n '1,5p' f"), ReadOnly);
}

#[test]
fn write_sed_w_flag() {
    assert_eq!(c("sed 's/a/b/w out' f"), Write);
}

#[test]
fn write_redirects() {
    assert_eq!(c("echo x > f"), Write);
    assert_eq!(c("echo x >> f"), Write);
    assert_eq!(c("cat a > b"), Write);
    assert_eq!(c("ls &> out"), Write);
    assert_eq!(c("ls >| f"), Write);
}

#[test]
fn read_only_input_redirect() {
    // input redirection (`<`) is safe.
    assert_eq!(c("cat < in"), ReadOnly);
    // fd dup to a number/`-` is safe.
    assert_eq!(c("ls >&2"), ReadOnly);
}

#[test]
fn write_find_delete_and_exec() {
    assert_eq!(c("find . -delete"), Write);
    assert_eq!(c("find . -exec rm {} ;"), Write);
    assert_eq!(c("find . -name '*.tmp' -delete"), Write);
}

#[test]
fn read_only_find_plain() {
    assert_eq!(c("find . -name '*.rs'"), ReadOnly);
    assert_eq!(c("find src -type f"), ReadOnly);
}

#[test]
fn write_tee_and_dd() {
    assert_eq!(c("tee out"), Write);
    assert_eq!(c("dd if=a of=b"), Write);
}

// ---- unknown ----

#[test]
fn unknown_unrecognized_binary() {
    assert_eq!(c("frobnicate --do-thing"), Unknown);
    assert_eq!(c("mysterytool"), Unknown);
    assert_eq!(c("curl https://x"), Unknown);
    assert_eq!(c("node script.js"), Unknown);
}

#[test]
fn unknown_empty_and_blank() {
    assert_eq!(c(""), Unknown);
    assert_eq!(c("   "), Unknown);
}

#[test]
fn unknown_git_unknown_subcommand() {
    assert_eq!(c("git bisect start"), Unknown);
    assert_eq!(c("git config user.name"), Unknown);
}

#[test]
fn unknown_env_prefix_escalates() {
    // an environment prefix makes even a read-only command unknown.
    assert_eq!(c("FOO=1 ls"), Unknown);
}

#[test]
fn unknown_function_definition() {
    assert_eq!(c("func() { ls; }"), Unknown);
}

#[test]
fn read_only_bare_assignment() {
    // a sole env assignment (no command) is read-only.
    assert_eq!(c("FOO=1"), ReadOnly);
    assert_eq!(c("A=1 B=2"), ReadOnly);
}

// ---- command substitution ----

#[test]
fn command_substitution_escalates() {
    // a substitution makes the enclosing command at least unknown, and a write
    // inside makes it write.
    assert_eq!(c("echo $(rm f)"), Write);
    assert_eq!(c("echo `rm x`"), Write);
    assert_eq!(c("echo $(ls)"), Unknown);
    assert_eq!(c("cat $(ls)"), Unknown);
}

// ---- compound: pipelines, lists, subshells ----

#[test]
fn pipeline_all_read_only_is_read_only() {
    assert_eq!(c("cat a | grep b"), ReadOnly);
    assert_eq!(c("ls | wc -l"), ReadOnly);
}

#[test]
fn pipeline_with_write_is_write() {
    assert_eq!(c("cat a | tee b"), Write);
}

#[test]
fn list_and_with_write_is_write() {
    assert_eq!(c("ls && rm f"), Write);
    assert_eq!(c("ls || rm f"), Write);
}

#[test]
fn list_semicolon_git_status_then_push_is_write() {
    assert_eq!(c("git status; git push"), Write);
}

#[test]
fn list_all_read_only_is_read_only() {
    assert_eq!(c("ls && cat f"), ReadOnly);
    assert_eq!(c("cd /tmp && ls"), ReadOnly);
}

#[test]
fn subshell_with_write_is_write() {
    assert_eq!(c("(cd /tmp && rm f)"), Write);
    assert_eq!(c("(ls; cat f)"), ReadOnly);
}

#[test]
fn negated_command_read_only() {
    assert_eq!(c("! ls"), ReadOnly);
    assert_eq!(c("! rm f"), Write);
}

// ---- shell-expansion escalation on pattern-sensitive commands ----

#[test]
fn glob_on_git_escalates_to_unknown() {
    // a variable/glob in a pattern-sensitive command's arg is unknown.
    assert_eq!(c("grep -r $x ."), ReadOnly); // grep is not pattern-sensitive here
    assert_eq!(c("git log $BRANCH"), Unknown);
}

// ---- classify_shell_input (the JSON entry point) ----

#[test]
fn classify_input_reads_command_field() {
    assert_eq!(
        classify_shell_input(&serde_json::json!({"command": "ls"})),
        ReadOnly
    );
    assert_eq!(
        classify_shell_input(&serde_json::json!({"command": "rm f"})),
        Write
    );
}

#[test]
fn classify_input_missing_command_is_unknown() {
    assert_eq!(classify_shell_input(&serde_json::json!({})), Unknown);
    assert_eq!(
        classify_shell_input(&serde_json::json!({"command": 42})),
        Unknown
    );
}

#[test]
fn write_redirect_to_variable_and_find_exec_plus() {
    assert_eq!(c("ls > $FILE"), Write);
    assert_eq!(c("find . -exec rm {} +"), Write);
    // the shell-consumed `\;` form: `;` becomes a list separator, find's
    // -exec still finds `rm` as the invoked write command.
    assert_eq!(c("find . -name x -exec rm {} ;"), Write);
}

// ---- deeper git coverage: branch flags, remote actions, output options ----

#[test]
fn git_branch_flags() {
    assert_eq!(c("git branch -a"), ReadOnly); // list flag
    assert_eq!(c("git branch -r"), ReadOnly);
    assert_eq!(c("git branch -m old new"), Write); // move
    assert_eq!(c("git branch -D topic"), Write); // force delete
    assert_eq!(c("git branch --list 'feat/*'"), ReadOnly);
    // --set-upstream-to matches WRITE_GIT_BRANCH_FLAG in action position -> write.
    assert_eq!(c("git branch --set-upstream-to=origin/main"), Write);
}

#[test]
fn git_remote_actions() {
    assert_eq!(c("git remote -v"), ReadOnly);
    assert_eq!(c("git remote get-url origin"), ReadOnly);
    assert_eq!(c("git remote remove origin"), Write);
    assert_eq!(c("git remote set-url origin url"), Write);
    assert_eq!(c("git remote prune origin"), Write);
    assert_eq!(c("git remote prune -n origin"), Unknown); // dry-run
}

#[test]
fn git_output_option_on_diff_is_write() {
    assert_eq!(c("git diff --output=patch.txt"), Write);
    assert_eq!(c("git log --output=out"), Write);
    assert_eq!(c("git blame --output=x f"), Unknown);
}

#[test]
fn git_help_and_version() {
    assert_eq!(c("git --help"), ReadOnly);
    assert_eq!(c("git status --help"), Unknown);
    assert_eq!(c("git commit --help"), Unknown); // help on a write subcommand
}

// ---- kill / pkill / killall ----

#[test]
fn kill_delivers_signal_is_write() {
    assert_eq!(c("kill 1234"), Write);
    assert_eq!(c("kill -9 1234"), Write);
    assert_eq!(c("pkill firefox"), Write);
    assert_eq!(c("killall node"), Write);
}

#[test]
fn kill_signal_zero_is_unknown() {
    // signal 0 is a liveness QUERY, not a kill.
    assert_eq!(c("kill -0 1234"), Unknown);
    assert_eq!(c("kill -l"), Unknown); // list signals
}

// ---- awk ----

#[test]
fn awk_print_read_only() {
    assert_eq!(c("awk '{print $1}' f"), ReadOnly);
    assert_eq!(c("awk 'NR==1' f"), ReadOnly);
}

#[test]
fn awk_redirect_to_file_is_write() {
    assert_eq!(c(r#"awk '{print > "out"}' f"#), Write);
}

#[test]
fn awk_system_call_is_unknown() {
    assert_eq!(c(r#"awk 'BEGIN{system("rm x")}'"#), Unknown);
    assert_eq!(c("awk '{print | \"cmd\"}' f"), Unknown); // pipe to command
}

// ---- sed options & multi-command ----

#[test]
fn sed_e_expression_and_options() {
    // no trailing operand: the `-e` script alone is read-only.
    assert_eq!(c("sed -e 's/a/b/'"), ReadOnly);
    // a trailing file operand joins the `-e` flag as `"-e f"`, which qwen's
    // compatibility check (`/(?:^|[^\\])[ewr]\s/`) flags -> unknown. Faithful.
    assert_eq!(c("sed -e 's/a/b/' f"), Unknown);
    assert_eq!(c("sed -e 's/a/b/w out'"), Write);
}

#[test]
fn sed_file_option_is_unknown() {
    assert_eq!(c("sed -f script.sed f"), Unknown);
}

#[test]
fn sed_multi_command_semicolons() {
    assert_eq!(c("sed 's/a/b/;s/c/d/' f"), ReadOnly);
    assert_eq!(c("sed '1d;2d' f"), ReadOnly);
    assert_eq!(c("sed '1d;s/a/b/w out' f"), Write);
}

// ---- sort / tree / uniq / tee / dd variants ----

#[test]
fn sort_output_option_is_write() {
    assert_eq!(c("sort -o out.txt f"), Write);
    assert_eq!(c("sort f"), ReadOnly);
    assert_eq!(c("sort --output=out f"), Write);
}

#[test]
fn tree_output_option_is_write() {
    assert_eq!(c("tree"), ReadOnly);
    assert_eq!(c("tree -o out"), Write);
}

#[test]
fn uniq_second_positional_is_output_write() {
    assert_eq!(c("uniq f"), ReadOnly);
    assert_eq!(c("uniq in out"), Write); // second positional is an OUTPUT file
    assert_eq!(c("uniq -c f"), ReadOnly);
}

#[test]
fn tee_flag_only_is_unknown() {
    assert_eq!(c("tee -a out"), Write); // still writes a file
    assert_eq!(c("tee"), Unknown); // no file -> unknown
}

#[test]
fn dd_without_of_is_unknown() {
    assert_eq!(c("dd if=a"), Unknown);
}

// ---- printf / less / rg escalation ----

#[test]
fn printf_v_writes_variable() {
    assert_eq!(c("printf hello"), ReadOnly);
    assert_eq!(c("printf -v out '%s' x"), Unknown);
}

#[test]
fn pager_is_unknown() {
    assert_eq!(c("less f"), Unknown);
    assert_eq!(c("more f"), Unknown);
}

#[test]
fn find_value_predicate_missing_value() {
    assert_eq!(c("find . -name '*.rs' -type f"), ReadOnly);
    // an -exec invoking a non-write command is unknown (can't reason about it).
    assert_eq!(c("find . -exec echo {} ;"), Unknown);
}
