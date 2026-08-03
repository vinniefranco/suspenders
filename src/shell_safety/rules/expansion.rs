//! Shell brace / pattern expansion detection - a faithful port of the
//! `hasShellBraceExpansion`/`hasShellPatternExpansion` helpers in qwen v0.21.4's
//! `shell-safety-rules.ts`. The AST walk ([`super::super`]) uses these to escalate
//! a pattern-sensitive command's read-only result to unknown when an arg carries a
//! glob (`[`/`*`/`?`) or a brace expansion (`{a,b}`/`{1..3}`).

// --- shell expansion (brace / pattern) ----------------------------------------

/// shell-safety-rules.ts:319 `hasShellBraceExpansion` - a `{a,b}` or `{1..3}`
/// brace-expansion (a `,` or `..` inside braces), scanned char-by-char.
pub fn has_shell_brace_expansion(text: &str) -> bool {
    let mut brace_depth: i32 = 0;
    let mut previous_dot = false;
    for ch in text.chars() {
        if ch == '{' {
            brace_depth += 1;
            previous_dot = false;
        } else if ch == '}' {
            brace_depth = (brace_depth - 1).max(0);
            previous_dot = false;
        } else if brace_depth > 0 {
            if ch == ',' || (ch == '.' && previous_dot) {
                return true;
            }
            previous_dot = ch == '.';
        }
    }
    false
}

/// shell-safety-rules.ts:337 `hasShellPatternExpansion` - a glob (`[`/`*`/`?`) or
/// a brace expansion.
pub fn has_shell_pattern_expansion(text: &str) -> bool {
    text.contains(['[', '*', '?']) || has_shell_brace_expansion(text)
}
