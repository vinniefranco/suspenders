//! The plan-mode shell classifier - a faithful port of qwen v0.21.4's
//! tree-sitter-bash safety walk (`packages/core/src/utils/shellAstParser.ts`,
//! `classifyShellCommandSafety`).
//!
//! Given a shell command string, [`classify_shell_command_safety`] parses it with
//! `tree-sitter-bash` and walks the AST to a three-valued verdict:
//! [`ShellCommandSafety::ReadOnly`] (`ls`, `cat`, `git status`, `grep -r x .`),
//! [`ShellCommandSafety::Write`] (`rm f`, `git commit`, `sed -i`, `echo x > f`),
//! or [`ShellCommandSafety::Unknown`] (an unrecognized binary, an ambiguous
//! option - the default, NOT a guess). Plan mode ([`crate::approvals`], ADR-0067)
//! uses this to allow a read-only shell command while blocking a mutating one.
//!
//! SYNC by design (ADR-0067, Phase 4b). qwen's `classifyShellCommandSafety` is
//! `async` ONLY because web-tree-sitter loads its WASM grammar asynchronously. The
//! native `tree-sitter` crate parses synchronously with a `cc`-built grammar, so
//! this is a pure, deterministic, side-effect-free function that plugs directly
//! into the sync `Approvals::classify` fold - no async, no scheduler plumbing. We
//! therefore port the CLASSIFICATION LOGIC (this file + [`rules`]) and NOT qwen's
//! async orchestration (`evaluatePlanModeShellPolicy`/`raceWithAbort`), which the
//! [`crate::approvals`] fold subsumes. `extractCommandRules` is likewise omitted:
//! suspenders' Standing Approvals are exact-string (ADR-0005), not qwen's
//! minimum-scope wildcard rules, so that half of shellAstParser.ts has no consumer
//! here.
//!
//! Parser lifetime: a tree-sitter `Parser` is not `Sync`, so a shared singleton
//! is out. It is constructed PER CALL (`Parser::new()` + `set_language`, both
//! cheap - microseconds - and classification is off the hot path: it runs once per
//! plan-mode shell Call, not per token). Per-call construction keeps the function
//! a pure `&str -> enum` with no shared mutable state, no `thread_local` teardown
//! to reason about, and no lock. Faithful to qwen's decisions, simpler than its
//! singleton (which exists only to amortize WASM init we do not pay).
//!
//! Grammar node-kind parity: the Rust `tree-sitter-bash` 0.25.1 grammar emits the
//! SAME node kinds as qwen's web-tree-sitter grammar for every construct this walk
//! touches (`command`, `command_name`, `redirected_statement`, `file_redirect`,
//! `command_substitution`, `process_substitution`, `variable_assignment(s)`,
//! `pipeline`/`list`/`subshell`/`compound_statement`/`negated_command`, the
//! `simple_expansion`/`expansion`/`arithmetic_expansion` set, and the redirect
//! operators `>`/`>>`/`&>`/`&>>`/`>|`/`>&`). Two representational differences from
//! qwen's JS API are bridged in the [`node`] helpers below and are the ONLY
//! divergences: (1) the `name` field child is a `command_name` WRAPPER whose text
//! equals the command name (qwen's `childForFieldName('name').text` reads the same
//! string); (2) tree-sitter's Rust API exposes fields via `field_name_for_child`,
//! matching qwen's `fieldNameForChild`, so `argument`-field extraction is 1:1.

mod commands;
mod rules;
mod sets;

use serde_json::Value;
use tree_sitter::{Node, Parser, Tree};

// The public verdict enum lives in `sets` (the classifier's shared vocabulary),
// re-exported here so `crate::shell_safety::ShellCommandSafety` stays the public
// path the approvals fold and tests import.
#[doc(inline)]
pub use sets::ShellCommandSafety;
use sets::{
    WRITE_REDIRECT_OPERATORS, has_help, merge, merge2, merge_with, re, strip_outer_quotes,
};
use ShellCommandSafety::{ReadOnly, Unknown, Write};

// ---------------------------------------------------------------------------
// The AST walk (shellAstParser.ts:961-1116)
// ---------------------------------------------------------------------------

/// Node-kind helpers bridging the two representational differences from qwen's JS
/// tree-sitter API (see the module doc). Everything else is the raw grammar.
mod node {
    use tree_sitter::Node;

    /// The `command_name` field's text, lowercased (qwen `getCommandName`,
    /// shellAstParser.ts:702). The Rust grammar wraps the name in a `command_name`
    /// node, but `.utf8_text()` yields the same string qwen reads.
    pub fn command_name<'a>(command: Node<'a>, src: &'a [u8]) -> Option<String> {
        let name = command.child_by_field_name("name")?;
        Some(name.utf8_text(src).ok()?.to_lowercase())
    }

    /// The raw (un-lowercased) `name` text (qwen `rawRoot`, shellAstParser.ts:977).
    pub fn raw_command_name<'a>(command: Node<'a>, src: &'a [u8]) -> Option<String> {
        let name = command.child_by_field_name("name")?;
        Some(name.utf8_text(src).ok()?.to_string())
    }

    /// The `argument`-field children of a `command` node (qwen `getArgumentNodes`,
    /// shellAstParser.ts:711), in source order.
    pub fn argument_nodes<'a>(command: Node<'a>) -> Vec<Node<'a>> {
        let mut out = Vec::new();
        let mut cursor = command.walk();
        for (i, child) in command.children(&mut cursor).enumerate() {
            if command.field_name_for_child(i as u32) == Some("argument") {
                out.push(child);
            }
        }
        out
    }

    /// Named children of a node, in order (qwen `namedChildren`).
    pub fn named_children<'a>(node: Node<'a>) -> Vec<Node<'a>> {
        let mut out = Vec::new();
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            out.push(child);
        }
        out
    }

    /// All children (named + anonymous), in order (qwen `.children`).
    pub fn all_children<'a>(node: Node<'a>) -> Vec<Node<'a>> {
        let mut out = Vec::new();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            out.push(child);
        }
        out
    }
}

const SHELL_EXPANSION_TYPES: &[&str] =
    &["simple_expansion", "expansion", "arithmetic_expansion"];

/// shellAstParser.ts:678 `collectDescendants` - all descendants of `node` (incl.
/// itself) whose kind is in `types`. `outermost_only` stops descending into a
/// matched node (used for substitutions).
fn collect_descendants<'a>(
    node: Node<'a>,
    types: &[&str],
    outermost_only: bool,
) -> Vec<Node<'a>> {
    let mut result = Vec::new();
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if types.contains(&current.kind()) {
            result.push(current);
            if outermost_only {
                continue;
            }
        }
        // push children in reverse so the traversal order matches qwen's
        // (which pushes `child(childCount-1..0)` then pops).
        let count = current.child_count() as u32;
        for i in (0..count).rev() {
            if let Some(child) = current.child(i) {
                stack.push(child);
            }
        }
    }
    result
}

/// shellAstParser.ts:739 `hasShellExpansion` - a command-substitution-free
/// expansion (`$VAR`, `${...}`, `$((...))`) or a glob/brace pattern in a bare
/// word/concatenation.
fn has_shell_expansion(node: Node, src: &[u8]) -> bool {
    if !collect_descendants(node, SHELL_EXPANSION_TYPES, false).is_empty() {
        return true;
    }
    let kind = node.kind();
    (kind == "word" || kind == "concatenation")
        && node
            .utf8_text(src)
            .map(rules::has_shell_pattern_expansion)
            .unwrap_or(false)
}

/// The safeties of every substitution BODY under `node` (the operation half of
/// [`evaluate_substitutions`]): each command-/process-substitution's named
/// children, classified. Split out so the caller stays pure integration.
fn substitution_body_safeties(node: Node, src: &[u8]) -> Vec<ShellCommandSafety> {
    let substitutions =
        collect_descendants(node, &["command_substitution", "process_substitution"], true);
    substitutions
        .into_iter()
        .flat_map(|sub| node::named_children(sub))
        .map(|child| evaluate_statement_safety(child, src))
        .collect()
}

/// shellAstParser.ts:961 `evaluateSubstitutions` - a command-/process-substitution
/// makes the enclosing command at least `unknown`, plus the substitution's own
/// body is classified (a `$(rm f)` inside an otherwise read-only command is write).
/// No substitution -> read-only.
fn evaluate_substitutions(node: Node, src: &[u8]) -> ShellCommandSafety {
    let bodies = substitution_body_safeties(node, src);
    if bodies.is_empty() {
        return ReadOnly;
    }
    merge(&merge_with(Unknown, &bodies))
}

/// The command's OWN safety before its redirects/substitutions are folded in:
/// the per-command dispatch plus qwen's three post-dispatch escalations
/// (shellAstParser.ts:981-1043's `result` up to the final `mergeSafety`). Pure
/// over the extracted name/args, so [`evaluate_command_safety`] stays integration.
fn command_own_safety(
    command: Node,
    root: &Option<String>,
    args: &[String],
    arg_nodes: &[Node],
    src: &[u8],
) -> ShellCommandSafety {
    let raw_root = node::raw_command_name(command, src);
    let dispatched = match root.as_deref() {
        None => ReadOnly,
        // a non-lowercase or substitution-bearing name (`$(rm f)`) -> unknown;
        // its substitution body is still folded in by the caller.
        Some(_) if raw_root.as_deref() != root.as_deref() => Unknown,
        Some(r) => commands::dispatch_command(r, args),
    };
    let escalated = escalate_pattern_expansion(dispatched, root, arg_nodes, src);
    let escalated = escalate_write_help(escalated, root, args);
    escalate_environment(escalated, command, root)
}

/// read-only escalation: a shell expansion in an arg of a pattern-sensitive
/// command (`git log $x`, `find $glob`) is `unknown` - a glob could match
/// anything (shellAstParser.ts:1026).
fn escalate_pattern_expansion(
    result: ShellCommandSafety,
    root: &Option<String>,
    arg_nodes: &[Node],
    src: &[u8],
) -> ShellCommandSafety {
    let Some(root) = root else { return result };
    let escalate = result == ReadOnly
        && re::PATTERN_SENSITIVE.is_match(root)
        && arg_nodes.iter().any(|n| has_shell_expansion(*n, src));
    if escalate { Unknown } else { result }
}

/// a `write` command called with `--help`/`--version` (except find/git/sed/sort/
/// tree, which handle help themselves) is a help query -> unknown
/// (shellAstParser.ts:1034).
fn escalate_write_help(
    result: ShellCommandSafety,
    root: &Option<String>,
    args: &[String],
) -> ShellCommandSafety {
    let escalate = result == Write
        && !["find", "git", "sed", "sort", "tree"].contains(&root.as_deref().unwrap_or(""))
        && has_help(args, &[]);
    if escalate { Unknown } else { result }
}

/// an environment prefix (`FOO=1 cmd`) makes even a read-only command unknown -
/// the assignment could carry a substitution the walk cannot fully reason about
/// (shellAstParser.ts:1040).
fn escalate_environment(
    result: ShellCommandSafety,
    command: Node,
    root: &Option<String>,
) -> ShellCommandSafety {
    let has_environment = node::named_children(command)
        .iter()
        .any(|c| c.kind() == "variable_assignment");
    if root.is_some() && has_environment {
        merge2(result, Unknown)
    } else {
        result
    }
}

/// The safeties of a command node's non-redirect children (its substitution
/// bodies): the operation half of [`evaluate_command_safety`]'s final fold.
fn command_child_substitution_safeties(command: Node, src: &[u8]) -> Vec<ShellCommandSafety> {
    node::named_children(command)
        .into_iter()
        .filter(|child| !child.kind().ends_with("_redirect"))
        .map(|child| evaluate_substitutions(child, src))
        .collect()
}

/// shellAstParser.ts:976 `evaluateCommandSafety` - the root of the per-command
/// dispatch: the command's own safety ([`command_own_safety`]) merged with its
/// redirections and substitutions.
fn evaluate_command_safety(command: Node, src: &[u8]) -> ShellCommandSafety {
    let root = node::command_name(command, src);
    let arg_nodes = node::argument_nodes(command);
    let args: Vec<String> = arg_nodes
        .iter()
        .map(|n| strip_outer_quotes(n.utf8_text(src).unwrap_or("")))
        .collect();

    let own = command_own_safety(command, &root, &args, &arg_nodes, src);
    let redirects = evaluate_redirection_safety(command, src);
    let substitutions = command_child_substitution_safeties(command, src);
    merge(&merge_with(own, &merge_with(redirects, &substitutions)))
}


/// The safety of ONE `_redirect` child (the operation half of
/// [`evaluate_redirection_safety`]): its substitutions merged with the write/
/// unknown escalation for a `file_redirect`'s operator and `>&` destination.
fn redirect_safety(redirect: Node, src: &[u8]) -> ShellCommandSafety {
    let subs = evaluate_substitutions(redirect, src);
    if redirect.kind() != "file_redirect" {
        return subs;
    }
    // qwen: `operator = redirect.children.find(c => c.type !== 'file_descriptor')`
    let Some(operator) = node::all_children(redirect)
        .into_iter()
        .find(|c| c.kind() != "file_descriptor")
    else {
        return Unknown;
    };
    if WRITE_REDIRECT_OPERATORS.contains(&operator.kind()) {
        return Write;
    }
    if operator.kind() == ">&" {
        return merge2(subs, redirect_dup_safety(redirect, src));
    }
    subs
}

/// The safety of a `>&`-operator redirect's destination: a bare fd / `-` is a safe
/// dup or close; a metachar-bearing target is `unknown`; any other named target is
/// a write (shellAstParser.ts:1064).
fn redirect_dup_safety(redirect: Node, src: &[u8]) -> ShellCommandSafety {
    let Some(destination) = redirect.child_by_field_name("destination") else {
        return Unknown;
    };
    let target = strip_outer_quotes(destination.utf8_text(src).unwrap_or(""));
    if is_fd_or_dash(&target) {
        ReadOnly
    } else if re::SHELL_METACHAR.is_match(&target) {
        Unknown
    } else {
        Write
    }
}

/// shellAstParser.ts:1053 `evaluateRedirectionSafety` - merge the safety of every
/// `_redirect` child of `node`.
fn evaluate_redirection_safety(node: Node, src: &[u8]) -> ShellCommandSafety {
    let parts: Vec<ShellCommandSafety> = node::named_children(node)
        .into_iter()
        .filter(|child| child.kind().ends_with("_redirect"))
        .map(|redirect| redirect_safety(redirect, src))
        .collect();
    merge(&merge_with(ReadOnly, &parts))
}

/// `/^(?:\d+|-)$/` - a redirect destination that is a bare fd number or `-`.
fn is_fd_or_dash(target: &str) -> bool {
    target == "-" || (!target.is_empty() && target.bytes().all(|b| b.is_ascii_digit()))
}

const CHILD_STATEMENT: &[&str] = &[
    "pipeline",
    "list",
    "subshell",
    "compound_statement",
    "negated_command",
];

/// The safeties of every named child of `node`, classified (the operation half of
/// the child-fold used by wrappers and unrecognized constructs).
fn child_safeties(node: Node, src: &[u8]) -> Vec<ShellCommandSafety> {
    node::named_children(node)
        .into_iter()
        .map(|child| evaluate_statement_safety(child, src))
        .collect()
}

/// shellAstParser.ts:1078 `childrenSafety` - merge every named child's safety over
/// a `floor` (read-only for wrappers, unknown for unrecognized constructs).
fn children_safety(node: Node, src: &[u8], floor: ShellCommandSafety) -> ShellCommandSafety {
    merge(&merge_with(floor, &child_safeties(node, src)))
}

/// A `redirected_statement` (shellAstParser.ts:1085): its non-redirect body
/// statements plus its own redirection.
fn redirected_statement_safety(node: Node, src: &[u8]) -> ShellCommandSafety {
    let bodies: Vec<ShellCommandSafety> = node::named_children(node)
        .into_iter()
        .filter(|child| !child.kind().ends_with("_redirect"))
        .map(|child| evaluate_statement_safety(child, src))
        .collect();
    merge(&merge_with(
        evaluate_redirection_safety(node, src),
        &bodies,
    ))
}

/// A bare `variable_assignment(s)` (shellAstParser.ts:1092): read-only iff it is
/// the SOLE statement of its parent (an env-only line), else unknown; the
/// assignment's own substitutions are folded in either way.
fn variable_assignment_safety(node: Node, src: &[u8]) -> ShellCommandSafety {
    let sole = node
        .parent()
        .map(|p| p.named_child_count() == 1)
        .unwrap_or(false);
    let floor = if sole { ReadOnly } else { Unknown };
    merge2(floor, evaluate_substitutions(node, src))
}

/// shellAstParser.ts:1082 `evaluateStatementSafety` - dispatch on statement node
/// kind: a `command` -> the command evaluator; a pipeline/list/subshell/... ->
/// its children; a redirected_statement -> its body + redirection; a bare
/// `variable_assignment(s)` -> the env-line rule; a function definition ->
/// unknown; anything else -> its children floored at unknown.
fn evaluate_statement_safety(node: Node, src: &[u8]) -> ShellCommandSafety {
    let kind = node.kind();
    if kind == "command" {
        evaluate_command_safety(node, src)
    } else if CHILD_STATEMENT.contains(&kind) {
        children_safety(node, src, ReadOnly)
    } else if kind == "redirected_statement" {
        redirected_statement_safety(node, src)
    } else if kind == "variable_assignment" || kind == "variable_assignments" {
        variable_assignment_safety(node, src)
    } else if kind == "function_definition" {
        Unknown
    } else {
        children_safety(node, src, Unknown)
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Classify a shell command string (qwen `classifyShellCommandSafety`,
/// shellAstParser.ts:1111). A non-string/empty command, a parse error, or an
/// empty program is [`Unknown`] (qwen's default); otherwise the root's named
/// children are each classified and [`merge`]d.
pub fn classify_shell_command_safety(command: &str) -> ShellCommandSafety {
    if command.trim().is_empty() {
        return Unknown;
    }
    let Some(tree) = parse(command) else {
        return Unknown;
    };
    let root = tree.root_node();
    if root.named_child_count() == 0 || root.has_error() {
        return Unknown;
    }
    let parts: Vec<ShellCommandSafety> = node::named_children(root)
        .into_iter()
        .map(|child| evaluate_statement_safety(child, command.as_bytes()))
        .collect();
    merge(&parts)
}

/// Classify the `command` field of a `run_shell_command` input (the shape the
/// [`crate::approvals`] fold hands over). A missing / non-string `command` is
/// [`Unknown`] - the fail-safe default, matching qwen's `rawCommand = ''`
/// (which classifies to `unknown`) in `evaluatePlanModeShellPolicy`.
pub fn classify_shell_input(input: &Value) -> ShellCommandSafety {
    match input.get("command").and_then(Value::as_str) {
        Some(command) => classify_shell_command_safety(command),
        None => Unknown,
    }
}

/// Build a fresh `tree-sitter` parser and parse `command`. Per-call construction
/// (see the module doc): cheap, keeps the classifier a pure function with no
/// shared mutable state. Returns `None` if the grammar cannot be set or the parse
/// yields no tree (both classify to [`Unknown`] at the call site).
fn parse(command: &str) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .ok()?;
    parser.parse(command, None)
}

#[cfg(test)]
#[path = "../tests/shell_safety.rs"]
mod tests;
