//! The sed / awk / shell-expansion safety rules - a faithful port of qwen
//! v0.21.4's `packages/core/src/utils/shell-safety-rules.ts`, split by concern:
//! [`sed`] (the sed script/option classifier), [`awk`] (the awk program
//! classifier), and [`expansion`] (brace/glob detection). These are pure
//! string-scanning classifiers with no tree-sitter types; the AST walk
//! ([`super`]) and the command dispatch ([`super::commands`]) reach them through
//! the re-exports below.

mod awk;
mod expansion;
mod sed;

/// The three-valued safety of a sed/awk script (qwen `SedScriptSafety`), an alias
/// for the classifier's [`ShellCommandSafety`](super::sets::ShellCommandSafety).
pub use super::sets::ShellCommandSafety as Safety;

pub use awk::classify_awk_command_safety;
pub use expansion::has_shell_pattern_expansion;
pub use sed::classify_sed_command_safety;
