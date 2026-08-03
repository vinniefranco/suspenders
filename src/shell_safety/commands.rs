//! The per-command safety evaluators - the branchy heart of qwen's classifier
//! (`shellAstParser.ts:790-1025`), one function per command family. Each takes the
//! command's already-extracted, quote-stripped argument strings and returns a
//! [`ShellCommandSafety`] purely from the option/subcommand grammar (no AST). The
//! AST walk ([`super`]) calls [`dispatch_command`] once per command node; the
//! sed/awk script rules live in [`super::rules`], reached from the dispatch.
//!
//! This module depends one-way on [`super::sets`] (the tables + arg helpers) and
//! [`super::rules`]; it holds no tree-sitter types, so it is testable as pure
//! string logic. Split from the walk so the "what does THIS command mean" table
//! reads on its own, separate from "how do we fold the tree".

use super::rules;
use super::sets::{
    BLOCKED_FIND_PREFIXES, READ_ONLY_GIT_SUBCOMMANDS, READ_ONLY_ROOT_COMMANDS, ShellCommandSafety,
    UNIQ_VALUE_OPTIONS, before_terminator, evaluate_output_option, has_help, merge2, re,
    without_option_values,
};
use ShellCommandSafety::{ReadOnly, Unknown, Write};


/// The command-name dispatch (shellAstParser.ts:984-1025): pick the per-command
/// evaluator for `root` over its `args`. An unrecognized root falls to
/// [`evaluate_other_root`] (the READ_ONLY_ROOT_COMMANDS membership test).
pub fn dispatch_command(root: &str, args: &[String]) -> ShellCommandSafety {
    if re::WRITE_ROOT_COMMAND.is_match(root) {
        return if has_help(args, &[]) { Unknown } else { Write };
    }
    if root == "kill" || root == "killall" || root == "pkill" {
        return process_safety(root, args);
    }
    match root {
        "git" => evaluate_git_safety(args),
        "find" => evaluate_find_safety(args),
        "sed" => rules::classify_sed_command_safety(args),
        "awk" => rules::classify_awk_command_safety(args),
        "sort" | "tree" => evaluate_sort_or_tree(root, args),
        "uniq" => {
            if has_help(args, &[]) {
                Unknown
            } else {
                evaluate_uniq_safety(args)
            }
        }
        "tee" => evaluate_tee(args),
        "dd" => {
            if args.iter().any(|a| a.starts_with("of=")) {
                Write
            } else {
                Unknown
            }
        }
        _ => evaluate_other_root(root, args),
    }
}

/// `tee` (shellAstParser.ts:1006): a non-flag arg (or an arg after `--`) is a file
/// to write -> write; a flag-only invocation is unknown.
fn evaluate_tee(args: &[String]) -> ShellCommandSafety {
    let writes_file = args
        .iter()
        .enumerate()
        .any(|(index, arg)| !arg.starts_with('-') || (index > 0 && args[index - 1] == "--"));
    if writes_file { Write } else { Unknown }
}

/// The residual command cases (shellAstParser.ts:1013-1025): `printf -v` (writes a
/// variable), `less`/`more` (a pager can shell out), `rg`/`ripgrep` search-zip /
/// preprocessor flags (can exec) -> unknown; otherwise the READ_ONLY_ROOT_COMMANDS
/// membership test decides.
fn evaluate_other_root(root: &str, args: &[String]) -> ShellCommandSafety {
    let printf_v =
        root == "printf" && before_terminator(args).iter().any(|a| re::PRINTF_V.is_match(a));
    let pager = root == "less" || root == "more";
    let rg_exec = (root == "rg" || root == "ripgrep")
        && before_terminator(args).iter().any(|a| re::RG_EXEC.is_match(a));
    if printf_v || pager || rg_exec {
        return Unknown;
    }
    if READ_ONLY_ROOT_COMMANDS.contains(&root) {
        ReadOnly
    } else {
        Unknown
    }
}

/// `sort`/`tree` (shellAstParser.ts:992) - a `-o FILE`/`--output` write, with a
/// help query and an ambiguous bundled-`o` short-flag both escalating to unknown.
fn evaluate_sort_or_tree(root: &str, args: &[String]) -> ShellCommandSafety {
    let mut result = evaluate_output_option(args, root == "sort", true).unwrap_or(ReadOnly);
    if has_help(args, &["-o", "--output"]) {
        result = Unknown;
    }
    if before_terminator(args).iter().any(|arg| {
        re::SORT_TREE_BUNDLED_O.is_match(arg) || (root == "sort" && arg.starts_with("--co"))
    }) {
        result = merge2(result, Unknown);
    }
    result
}


// --- git ----------------------------------------------------------------------

fn evaluate_git_safety(args: &[String]) -> ShellCommandSafety {
    let Some(first) = args.first() else {
        return ReadOnly;
    };
    if first == "--version" {
        return ReadOnly;
    }
    if first == "--help" {
        return if args.len() == 1 { ReadOnly } else { Unknown };
    }
    if first.starts_with('-') {
        return Unknown;
    }
    let subcommand = first.to_lowercase();
    let rest = &args[1..];
    let options = before_terminator(rest);
    let invokes_helper = options.iter().any(|arg| re::GIT_EXTERNAL_HELPER_OPTION.is_match(arg))
        || (subcommand == "grep" && options.iter().any(|arg| arg.starts_with("-O")))
        || ((subcommand == "log" || subcommand == "show")
            && options.iter().any(|arg| percent_g_signature(arg)));

    if re::WRITE_GIT_SUBCOMMAND.is_match(&subcommand) {
        return evaluate_git_write_subcommand(&subcommand, rest);
    }
    if !READ_ONLY_GIT_SUBCOMMANDS.contains(&subcommand.as_str()) {
        return Unknown;
    }
    if ["diff", "log", "show"].contains(&subcommand.as_str())
        && let Some(output) = evaluate_output_option(rest, true, false)
    {
        return output;
    }
    if subcommand == "blame"
        && before_terminator(rest)
            .iter()
            .any(|arg| arg == "--output" || arg.starts_with("--output="))
    {
        return Unknown;
    }
    if subcommand != "branch" && has_help(rest, &[]) {
        return Unknown;
    }
    match subcommand.as_str() {
        "remote" => evaluate_git_remote(rest, invokes_helper),
        "branch" => evaluate_git_branch(rest, invokes_helper),
        _ if invokes_helper => Unknown,
        _ => ReadOnly,
    }
}

/// A write git subcommand (shellAstParser.ts:803): `write` unless it is a `--help`
/// query or a `--dry-run`/`-n` preview (which are indeterminate, not writes).
fn evaluate_git_write_subcommand(subcommand: &str, rest: &[String]) -> ShellCommandSafety {
    let effective_args: Vec<String> = if subcommand == "commit" {
        without_option_values(rest, &re::GIT_COMMIT_VALUE_OPTION)
    } else {
        rest.to_vec()
    };
    let effective_options = before_terminator(&effective_args);
    let help = has_help(&effective_args, &[]);
    let dry_run = effective_options.iter().any(|a| a == "--dry-run")
        || (effective_options.iter().any(|a| a == "-n")
            && ["add", "clean", "mv", "push", "rm"].contains(&subcommand));
    if help || dry_run { Unknown } else { Write }
}

/// `git remote <action>` (shellAstParser.ts:827): a read-only `show`/`get-url`
/// (unless it carries a mutating token), a mutating action -> write, `prune` ->
/// write unless a dry-run, an actionless `remote` -> read-only.
fn evaluate_git_remote(rest: &[String], invokes_helper: bool) -> ShellCommandSafety {
    let Some(action) = rest
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(|a| a.to_lowercase())
    else {
        return if invokes_helper { Unknown } else { ReadOnly };
    };
    if action == "show" || action == "get-url" {
        let mutates = rest.iter().any(|arg| re::GIT_REMOTE_MUTATING_ARG.is_match(arg));
        return if mutates || invokes_helper { Unknown } else { ReadOnly };
    }
    if re::WRITE_GIT_REMOTE_ACTION.is_match(&action) {
        return Write;
    }
    if action == "prune" {
        return if rest.iter().any(|arg| arg == "-n" || arg == "--dry-run") {
            Unknown
        } else {
            Write
        };
    }
    Unknown
}

/// `git branch` (shellAstParser.ts:845): a mutating branch flag -> write (or
/// unknown if the flag is only after `--`), a list flag -> read-only, a bare
/// positional -> write (creating a branch), an empty `branch` -> read-only.
fn evaluate_git_branch(rest: &[String], invokes_helper: bool) -> ShellCommandSafety {
    let actions = without_option_values(rest, &re::GIT_BRANCH_SORT_OR_FORMAT);
    let action_options = before_terminator(&actions);
    if has_help(&actions, &[]) {
        return Unknown;
    }
    if actions.iter().any(|arg| re::WRITE_GIT_BRANCH_FLAG.is_match(arg)) {
        return if action_options.iter().any(|arg| re::WRITE_GIT_BRANCH_FLAG.is_match(arg)) {
            Write
        } else {
            Unknown
        };
    }
    if actions.len() != rest.len() {
        return Unknown;
    }
    if action_options.iter().any(|arg| re::GIT_BRANCH_LIST_FLAG.is_match(arg)) {
        return ReadOnly;
    }
    if rest.iter().any(|arg| !arg.starts_with('-')) {
        return Write;
    }
    if rest.iter().any(|a| a == "--") {
        return Unknown;
    }
    if invokes_helper {
        return Unknown;
    }
    if rest.is_empty() { ReadOnly } else { Unknown }
}

/// `%G[?GKFPST]` anywhere in an arg (git log/show pretty-format signature check,
/// shellAstParser.ts:802). No anchor, so a substring scan.
fn percent_g_signature(arg: &str) -> bool {
    let b = arg.as_bytes();
    for i in 0..b.len() {
        if b[i] == b'%'
            && b.get(i + 1) == Some(&b'G')
            && let Some(c) = b.get(i + 2)
            && b"?GKFPST".contains(c)
        {
            return true;
        }
    }
    false
}

// --- find ---------------------------------------------------------------------

/// shellAstParser.ts:865 `evaluateFindSafety`.
fn evaluate_find_safety(args: &[String]) -> ShellCommandSafety {
    let mut result = ReadOnly;
    let mut i = 0usize;
    while i < args.len() {
        let lower = args[i].to_lowercase();
        if lower == "--" {
            return merge2(result, Unknown);
        }
        if lower == "--help" || lower == "--version" {
            return Unknown;
        }
        if re::FIND_VALUE_PREDICATE.is_match(&lower) {
            i += 1;
            // `if (!args[++i]?.match(/^[^-]/))` - the value must exist and not
            // start with `-`, else the predicate is malformed -> unknown.
            let value_ok = args
                .get(i)
                .is_some_and(|v| v.as_bytes().first().is_some_and(|c| *c != b'-'));
            if !value_ok {
                result = merge2(result, Unknown);
            }
            i += 1;
            continue;
        }
        if lower == "-delete" {
            result = Write;
            i += 1;
            continue;
        }
        if BLOCKED_FIND_PREFIXES.iter().any(|p| lower.starts_with(p)) {
            result = Write;
            i += if lower.starts_with("-fprintf") { 2 } else { 1 };
            i += 1;
            continue;
        }
        if ["-exec", "-execdir", "-ok", "-okdir"].contains(&lower.as_str()) {
            let (nested, end) = find_exec_safety(args, i);
            result = merge2(result, nested);
            i = end;
        }
        i += 1;
    }
    result
}

/// The safety of a `find ... -exec CMD ... ;/+` clause starting at index `i`
/// (shellAstParser.ts:884), plus the index of its terminator (or the args end):
/// a write command invoked -> write (unless a help query), a kill-family command
/// -> [`process_safety`], anything else -> unknown.
fn find_exec_safety(args: &[String], i: usize) -> (ShellCommandSafety, usize) {
    let invoked = args.get(i + 1).map(|a| a.to_lowercase());
    let end = args[(i + 2).min(args.len())..]
        .iter()
        .position(|a| [";", "\\;", "+"].contains(&a.as_str()))
        .map(|p| i + 2 + p);
    let invoked_end = end.unwrap_or(args.len());
    let invoked_args: Vec<String> = args[(i + 2).min(invoked_end)..invoked_end].to_vec();
    let nested = match invoked.as_deref() {
        Some(inv) if re::WRITE_ROOT_COMMAND.is_match(inv) => {
            if has_help(&invoked_args, &[]) { Unknown } else { Write }
        }
        Some(inv @ ("kill" | "killall" | "pkill")) => process_safety(inv, &invoked_args),
        _ => Unknown,
    };
    (nested, end.unwrap_or(args.len()))
}

// --- uniq ---------------------------------------------------------------------

/// shellAstParser.ts:914 `evaluateUniqSafety` - two-or-more positional args means
/// a second positional is an OUTPUT file -> write.
fn evaluate_uniq_safety(args: &[String]) -> ShellCommandSafety {
    let mut positional = 0usize;
    let mut i = 0usize;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            return if args.len() - i + positional > 2 { Write } else { ReadOnly };
        } else if UNIQ_VALUE_OPTIONS.contains(&arg.as_str()) {
            i += 1;
            if args.get(i).is_none() {
                return Unknown;
            }
        } else if arg == "-" || !arg.starts_with('-') {
            positional += 1;
        }
        i += 1;
    }
    if positional >= 2 { Write } else { ReadOnly }
}

// --- kill / pkill / killall ---------------------------------------------------

/// shellAstParser.ts:927 `processSafety` - kill/killall/pkill. A signal-0 or a
/// list form (`-l`, `--list`) is a QUERY (unknown), otherwise it delivers a
/// signal -> write.
fn process_safety(root: &str, args: &[String]) -> ShellCommandSafety {
    let options = before_terminator(args);
    // signalValueOptions: --signal, (-s unless pkill), (-n if kill)
    let mut signal_value_options: Vec<&str> = vec!["--signal"];
    if root != "pkill" {
        signal_value_options.push("-s");
    }
    if root == "kill" {
        signal_value_options.push("-n");
    }

    if args.is_empty()
        || has_help(args, &[])
        || options.iter().any(|arg| ["-h", "-V", "-help", "-version"].contains(&arg.as_str()))
    {
        return Unknown;
    }

    let indeterminate = options.iter().enumerate().any(|(index, arg)| {
        let prev = (index > 0).then(|| options[index - 1].as_str());
        kill_arg_is_query(root, arg, prev, &signal_value_options)
    });
    if indeterminate { Unknown } else { Write }
}

/// Whether a single kill/pkill/killall option makes the invocation a QUERY rather
/// than a delivery (shellAstParser.ts:942's `options.some(...)` predicate,
/// per-arg): a metachar-bearing signal, a list form (`-l`/`--list`), or a
/// signal-0 in any of its spellings. A `prev` value-option lets a bare `0`/metachar
/// argument count as the signal value.
fn kill_arg_is_query(
    root: &str,
    arg: &str,
    prev: Option<&str>,
    signal_value_options: &[&str],
) -> bool {
    let prev_is_signal_option = prev.is_some_and(|p| signal_value_options.contains(&p));
    let metachar_signalled =
        re::SHELL_METACHAR.is_match(arg) && (arg.starts_with('-') || prev_is_signal_option);
    // `--signal=0` / `--signal=SIG0` - the value after `--signal=` is signal-0.
    // `strip_prefix` is panic-free.
    let signal_eq_zero = arg
        .strip_prefix("--signal=")
        .is_some_and(|value| re::SIGNAL_ZERO.is_match(value));
    let value_is_zero = prev_is_signal_option && re::SIGNAL_ZERO.is_match(arg);
    metachar_signalled
        || re::KILL_LIST_FORM.is_match(arg)
        || signal_eq_zero
        || re::DASH_SIG_ZERO.is_match(arg)
        || (root == "kill" && re::KILL_SN_ZERO.is_match(arg))
        || (root == "killall" && re::KILLALL_S_ZERO.is_match(arg))
        || value_is_zero
}
