//! Classifies a `run_command` command string as search-shaped
//! (hand-exploration) or not, for the Explore Nudge (baud:
//! `Baud.Turn.Nudges.SearchCommand`; CONTEXT.md: Nudge; docs/DESIGN.md).
//!
//! A small model can bypass the Explore Nudge by hand-exploring through
//! `run_command` pipelines (`find ... | xargs grep ...`) instead of the
//! read-only Tools (read_file, list_files, grep) the streak counts. This
//! classifier lets the Explore Nudge count a search-shaped `run_command` as
//! exploration too, and treat everything else - `mix test`, `git`, `echo`, a
//! redirect - as non-exploration, resetting the streak. `mix test` MUST reset:
//! it is verification, the behavior the harness wants.
//!
//! This is policy, not wording: it lives beside the explore Governor's
//! trigger bookkeeping ([`super`]), never in [`crate::voice`].
//!
//! ## Classification
//!
//! Split the command on pipeline/sequence operators (`|`, `&&`, `;`) and take
//! each segment's leading program word. The command is search-shaped only when
//! EVERY segment's program is in a conservative read-only set. `xargs` counts
//! only when the word after it is itself a read-only program (`xargs grep`,
//! `xargs cat`); `xargs rm` resets. Any segment carrying an output redirect
//! (`>`), or any other program, resets the streak.

// Read-only programs whose sole effect is to search or read. `xargs` is here
// but judged specially - its payload is the next word.
const READ_ONLY: &[&str] = &[
    "grep", "rg", "find", "ls", "cat", "head", "tail", "tree", "wc", "file", "stat", "xargs",
];

/// True when the command string is search-shaped: every pipeline/sequence
/// segment's leading program is read-only (and any `xargs` feeds a read-only
/// program). An empty command, an output redirect, or any non-read-only program
/// makes it false.
pub fn search_shaped(command: &str) -> bool {
    let segments = segments(command);
    if segments.is_empty() {
        false
    } else {
        segments.iter().all(|s| search_segment(s))
    }
}

// Split on |, && and ; into non-empty, trimmed segments. `&&` and `||` reduce
// to the same single-char splits, matching baud's regex `\|\||[|&;]`.
fn segments(command: &str) -> Vec<String> {
    command
        .split(['|', '&', ';'])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

// A redirect anywhere in the segment writes to disk - never search-shaped
// (`>` also catches `>>`).
fn search_segment(segment: &str) -> bool {
    if segment.contains('>') {
        return false;
    }
    let words = words(segment);
    match words.split_first() {
        Some((first, rest)) if first == "xargs" => xargs_read_only(rest),
        Some((program, _)) => READ_ONLY.contains(&program.as_str()),
        None => false,
    }
}

// xargs is judged by the word it feeds: xargs grep / xargs cat count, xargs rm
// does not. A bare xargs (no payload) is not search-shaped.
fn xargs_read_only(rest: &[String]) -> bool {
    match rest.first() {
        Some(payload) => READ_ONLY.contains(&program_name(payload).as_str()),
        None => false,
    }
}

// Words of a segment; the leading word is reduced to its program basename so an
// absolute path (/usr/bin/grep) classifies like its command name.
fn words(segment: &str) -> Vec<String> {
    let mut parts: Vec<String> = segment.split_whitespace().map(|w| w.to_string()).collect();
    if let Some(first) = parts.first_mut() {
        *first = program_name(first);
    }
    parts
}

fn program_name(word: &str) -> String {
    // Path.basename: the final path component. A trailing slash yields the last
    // non-empty component; the read-only words here never end in one.
    word.rsplit('/').next().unwrap_or(word).to_string()
}

#[cfg(test)]
mod tests {
    use super::search_shaped;

    // single-program commands
    #[test]
    fn read_only_search_programs_are_search_shaped() {
        assert!(search_shaped("grep -rn foo lib"));
        assert!(search_shaped("rg foo"));
        assert!(search_shaped("find . -name '*.ex'"));
        assert!(search_shaped("ls -la"));
        assert!(search_shaped("cat lib/baud.ex"));
        assert!(search_shaped("head -20 mix.exs"));
        assert!(search_shaped("tail -f log"));
        assert!(search_shaped("tree lib"));
        assert!(search_shaped("wc -l lib/baud.ex"));
        assert!(search_shaped("file mix.exs"));
        assert!(search_shaped("stat mix.exs"));
    }

    #[test]
    fn verification_and_mutation_programs_are_not_search_shaped() {
        assert!(!search_shaped("mix test"));
        assert!(!search_shaped("mix compile"));
        assert!(!search_shaped("git status"));
        assert!(!search_shaped("echo hi"));
        assert!(!search_shaped("sed -i s/a/b/ f"));
        assert!(!search_shaped("awk '{print}' f"));
        assert!(!search_shaped("rm -rf lib"));
    }

    #[test]
    fn mix_test_resets_the_streak() {
        assert!(!search_shaped("mix test"));
    }

    // pipelines and sequences
    #[test]
    fn an_all_search_pipeline_is_search_shaped() {
        assert!(search_shaped("find . -name '*.ex' | xargs grep foo"));
        assert!(search_shaped("grep -rl foo lib | head"));
        assert!(search_shaped("cat f | grep bar | wc -l"));
    }

    #[test]
    fn a_pipeline_is_search_shaped_only_when_every_segment_is_read_only() {
        assert!(!search_shaped("grep foo lib | mix test"));
        assert!(!search_shaped("cat f | sed s/a/b/"));
        assert!(!search_shaped("find . | xargs rm"));
    }

    #[test]
    fn and_and_semicolon_sequences_are_split_too() {
        assert!(search_shaped("grep foo lib && ls"));
        assert!(!search_shaped("grep foo lib && mix test"));
        assert!(search_shaped("ls ; cat mix.exs"));
        assert!(!search_shaped("ls ; git commit"));
    }

    // xargs
    #[test]
    fn xargs_grep_cat_count() {
        assert!(search_shaped("find . | xargs grep foo"));
        assert!(search_shaped("grep -rl x lib | xargs cat"));
    }

    #[test]
    fn xargs_feeding_a_mutation_resets() {
        assert!(!search_shaped("find . | xargs rm"));
        assert!(!search_shaped("find . | xargs sed -i s/a/b/"));
    }

    // redirects
    #[test]
    fn a_segment_with_an_output_redirect_is_not_search_shaped() {
        assert!(!search_shaped("grep foo lib > out.txt"));
        assert!(!search_shaped("ls >> listing"));
    }

    // edge cases
    #[test]
    fn an_empty_or_whitespace_command_is_not_search_shaped() {
        assert!(!search_shaped(""));
        assert!(!search_shaped("   "));
    }

    #[test]
    fn a_leading_path_to_a_read_only_program_classifies_by_basename() {
        assert!(search_shaped("/usr/bin/grep foo"));
        assert!(search_shaped("/bin/ls"));
    }
}
