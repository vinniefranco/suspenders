//! The shared vocabulary of the plan-mode shell classifier: the three-valued
//! [`ShellCommandSafety`] verdict, the verbatim command/option TABLES qwen keys on
//! (`READ_ONLY_ROOT_COMMANDS`, `READ_ONLY_GIT_SUBCOMMANDS`, the `WRITE_*` regexes,
//! ...), the [`merge`] lattice, and the small argument helpers
//! (`before_terminator`/`has_help`/`without_option_values`/`evaluate_output_option`)
//! that every per-command evaluator ([`super::commands`]) and the AST walk
//! ([`super`]) reuse. It is the leaf of the module: it depends on nothing else in
//! `shell_safety`, so both the evaluators and the walk depend one-way on it.
//!
//! Ported from qwen `shellAstParser.ts` (the constants at lines 82-183 and the arg
//! helpers at 747-788); the JS RegExp constants are recreated in [`re`] with the
//! `regex` crate (no backtracking/lookaround needed).

/// The three-valued safety of a shell command (qwen `ShellCommandSafety`):
/// read-only, state-modifying, or indeterminate. `Unknown` is the DEFAULT for any
/// command the walk does not positively recognize as read-only or write - it is
/// never a guess in either direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellCommandSafety {
    ReadOnly,
    Write,
    Unknown,
}

use ShellCommandSafety::{ReadOnly, Unknown, Write};

/// Root commands considered read-only by default (qwen `READ_ONLY_ROOT_COMMANDS`,
/// shellAstParser.ts:82). Verbatim membership.
pub const READ_ONLY_ROOT_COMMANDS: &[&str] = &[
    "awk", "basename", "cat", "cd", "column", "cut", "df", "dirname", "du", "echo", "find", "git",
    "grep", "head", "less", "ls", "more", "printenv", "printf", "ps", "pwd", "rg", "ripgrep", "sed",
    "sort", "stat", "tail", "tree", "uniq", "wc", "which", "where", "whoami",
];

/// Git sub-commands considered read-only (qwen `READ_ONLY_GIT_SUBCOMMANDS`,
/// shellAstParser.ts:121). Verbatim membership.
pub const READ_ONLY_GIT_SUBCOMMANDS: &[&str] = &[
    "blame", "branch", "cat-file", "diff", "grep", "log", "ls-files", "remote", "rev-parse", "show",
    "status", "describe",
];

/// Write-redirection operators (qwen `WRITE_REDIRECT_OPERATORS`,
/// shellAstParser.ts:161). Input-only redirects (`<`, `<<`, `<<<`) are safe.
pub const WRITE_REDIRECT_OPERATORS: &[&str] = &[">", ">>", "&>", "&>>", ">|"];

/// `find` predicate prefixes that write to a named file (qwen
/// `BLOCKED_FIND_PREFIXES`, shellAstParser.ts:150).
pub const BLOCKED_FIND_PREFIXES: &[&str] = &["-fls", "-fprint", "-fprintf"];

/// `uniq` value-bearing options (qwen `UNIQ_VALUE_OPTIONS`, shellAstParser.ts:154).
pub const UNIQ_VALUE_OPTIONS: &[&str] = &[
    "-f",
    "--skip-fields",
    "-s",
    "--skip-chars",
    "-w",
    "--check-chars",
];

// ---------------------------------------------------------------------------
// The lazily-compiled regexes (qwen's module-level RegExp constants). Rust regex
// has no backtracking/lookaround, but none of these patterns need it.
// ---------------------------------------------------------------------------
pub mod re {
    use regex::Regex;
    use std::sync::LazyLock;

    macro_rules! re {
        ($name:ident, $pat:expr) => {
            pub static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($pat).unwrap());
        };
    }

    // shellAstParser.ts:118 WRITE_ROOT_COMMAND
    re!(
        WRITE_ROOT_COMMAND,
        r"^(chgrp|chmod|chown|cp|install|ln|mkdir|mkfifo|mknod|mv|rename|rm|rmdir|shred|touch|truncate|unlink)$"
    );
    // shellAstParser.ts:135 WRITE_GIT_SUBCOMMAND
    re!(
        WRITE_GIT_SUBCOMMAND,
        r"^(add|am|checkout|cherry-pick|clean|clone|commit|fetch|gc|init|merge|mv|pull|push|rebase|reset|restore|revert|rm|stash|switch)$"
    );
    // shellAstParser.ts:138 WRITE_GIT_REMOTE_ACTION
    re!(
        WRITE_GIT_REMOTE_ACTION,
        r"^(add|remove|rm|rename|set-branches|set-head|set-url|update)$"
    );
    // shellAstParser.ts:140 GIT_EXTERNAL_HELPER_OPTION
    re!(
        GIT_EXTERNAL_HELPER_OPTION,
        r"^--(?:ext-diff|filters|show-signature|textconv|open-files-in-pager)(?:=|$)"
    );
    // shellAstParser.ts:142 GIT_COMMIT_VALUE_OPTION
    re!(
        GIT_COMMIT_VALUE_OPTION,
        r"^(?:-[CcFmt]|--(?:author|cleanup|date|file|fixup|message|pathspec-from-file|reedit-message|reuse-message|squash|template|trailer))$"
    );
    // shellAstParser.ts:145 WRITE_GIT_BRANCH_FLAG
    re!(
        WRITE_GIT_BRANCH_FLAG,
        r"^(?:-[cCdDmMu](?:.|$)|--(?:delete|move|copy|set-upstream(?:-to)?|unset-upstream|create-reflog|edit-description)(?:=|$))"
    );
    // shellAstParser.ts:147 GIT_BRANCH_LIST_FLAG
    re!(
        GIT_BRANCH_LIST_FLAG,
        r"^(?:-[alr]|--(?:all|list|remotes|show-current|contains|no-contains|merged|no-merged|points-at))(?:=|$)"
    );
    // shellAstParser.ts:151 FIND_VALUE_PREDICATE
    re!(
        FIND_VALUE_PREDICATE,
        r"^-(?:[ac]?newer|newer[a-z]{2}|[acm](?:min|time)|context|fstype|gid|group|i?(?:lname|name|path|regex)|inum|links|maxdepth|mindepth|path|perm|printf|regextype|samefile|size|type|uid|used|user|wholename|xtype)$"
    );
    // shellAstParser.ts:758 hasHelp inner test `/^(?:--help|--version)$/i`
    re!(HELP_OR_VERSION, r"^(?i:--help|--version)$");
    // git remote show/get-url mutating-arg test (shellAstParser.ts:831)
    re!(
        GIT_REMOTE_MUTATING_ARG,
        r"^(?i:add|remove|rm|rename|set-branches|set-head|set-url|update|prune)$"
    );
    // characters that make a `>&` destination / a signal option indeterminate
    re!(SHELL_METACHAR, r"[$`*?()\[\]{}]");
    // git branch: value-options whose argument is skipped (shellAstParser.ts:846)
    re!(GIT_BRANCH_SORT_OR_FORMAT, r"^--(?:format|sort)$");
    // commands whose read-only result escalates to unknown under a shell
    // expansion in an arg (shellAstParser.ts:1029)
    re!(
        PATTERN_SENSITIVE,
        r"^(awk|find|git|printf|rg|ripgrep|sed|sort|tree|uniq)$"
    );
    // sort/tree bundled-`o` short-flag (shellAstParser.ts:998)
    re!(SORT_TREE_BUNDLED_O, r"^(?:--o|-[^-]+o)");
    // printf `-v` (writes a variable) (shellAstParser.ts:1014)
    re!(PRINTF_V, r"^-[^-]*v");
    // rg/ripgrep flags that can exec (shellAstParser.ts:1018)
    re!(
        RG_EXEC,
        r"^(?:--(?:hostname-bin|pre)(?:=|$)|--search-zip$|-[^-]*z)"
    );
    // kill/pkill/killall: signal-0 spellings and list forms (shellAstParser.ts:929)
    re!(SIGNAL_ZERO, r"^(?i:(?:SIG)?0+)$");
    re!(KILL_LIST_FORM, r"^-(?:[lL0]|-(?:.*list|table)(?:=|$))");
    re!(DASH_SIG_ZERO, r"^-(?i:(?:SIG)?0+)$");
    re!(KILL_SN_ZERO, r"^-[sn](?i:(?:SIG)?0+)$");
    re!(KILLALL_S_ZERO, r"^-s(?i:(?:SIG)?0+)$");
}

// ---------------------------------------------------------------------------
// mergeSafety and small arg helpers (shellAstParser.ts:747-788)
// ---------------------------------------------------------------------------

/// shellAstParser.ts:747 `mergeSafety` - a write anywhere -> write, else an
/// unknown anywhere -> unknown, else read-only. The lattice combination used for
/// pipelines, lists, redirects, and substitutions.
pub fn merge(results: &[ShellCommandSafety]) -> ShellCommandSafety {
    if results.contains(&Write) {
        Write
    } else if results.contains(&Unknown) {
        Unknown
    } else {
        ReadOnly
    }
}

/// The two-argument [`merge`].
pub fn merge2(a: ShellCommandSafety, b: ShellCommandSafety) -> ShellCommandSafety {
    merge(&[a, b])
}

/// Prepend a `floor` safety to a slice (so `merge` sees the floor plus the parts).
/// A tiny leaf that lets the walk functions build their `merge` argument by
/// composition rather than an inline `vec![floor, ...]` + push loop.
pub fn merge_with(
    floor: ShellCommandSafety,
    parts: &[ShellCommandSafety],
) -> Vec<ShellCommandSafety> {
    let mut out = Vec::with_capacity(parts.len() + 1);
    out.push(floor);
    out.extend_from_slice(parts);
    out
}

/// shellAstParser.ts:753 `beforeTerminator` - args up to (not including) `--`.
pub fn before_terminator(args: &[String]) -> &[String] {
    match args.iter().position(|a| a == "--") {
        Some(end) => &args[..end],
        None => args,
    }
}

/// shellAstParser.ts:758 `hasHelp` - a `--help`/`--version` before `--` that is
/// not the VALUE of a preceding value-option.
pub fn has_help(args: &[String], value_options: &[&str]) -> bool {
    let head = before_terminator(args);
    head.iter().enumerate().any(|(index, arg)| {
        re::HELP_OR_VERSION.is_match(arg)
            && !(index > 0 && value_options.contains(&head[index - 1].as_str()))
    })
}

/// shellAstParser.ts:766 `withoutOptionValues` - drop the token AFTER each option
/// matching `value_option` (so an option's value is not mistaken for an action).
pub fn without_option_values(args: &[String], value_option: &regex::Regex) -> Vec<String> {
    let mut result = Vec::new();
    let mut i = 0usize;
    while i < args.len() {
        result.push(args[i].clone());
        if value_option.is_match(&args[i]) {
            i += 1;
        }
        i += 1;
    }
    result
}

/// shellAstParser.ts:775 `evaluateOutputOption` - detect a `-o FILE`/`--output=FILE`
/// write. `None` means no output option seen; `Some(Write)`/`Some(Unknown)` mirror
/// qwen's `'write'`/`'unknown'` (unknown = the option present but its value absent).
pub fn evaluate_output_option(
    args: &[String],
    long: bool,
    short: bool,
) -> Option<ShellCommandSafety> {
    for (i, arg) in args.iter().enumerate() {
        if arg == "--" {
            break;
        }
        if (short && arg == "-o") || (long && arg == "--output") {
            return Some(if args.get(i + 1).is_some() { Write } else { Unknown });
        }
        if short && arg.starts_with("-o") && arg.len() > 2 {
            return Some(Write);
        }
        if let Some(value) = arg.strip_prefix("--output=").filter(|_| long) {
            // `--output=FILE` writes; a bare `--output=` (empty value) is unknown.
            return Some(if value.is_empty() { Unknown } else { Write });
        }
    }
    None
}

/// shellAstParser.ts:727 `stripOuterQuotes` - strip a matching pair of outer
/// single/double quotes so a quoted arg matches unquoted patterns.
pub fn strip_outer_quotes(text: &str) -> String {
    let bytes = text.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'\'' && last == b'\'') || (first == b'"' && last == b'"') {
            return text[1..text.len() - 1].to_string();
        }
    }
    text.to_string()
}
