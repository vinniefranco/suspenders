//! The Voice (CONTEXT.md): every Suspenders-voiced string the model reads -
//! the system prompt, the compaction prompt, and the markers that enter the
//! Conversation (malformed input, tool-result/error answers, run-close). The
//! boundary is voice, not arity: wording may be parameterized, but Suspenders
//! authors it.
//!
//! Wording is the highest-leverage tuning surface for small local models;
//! owning it in one module means swapping wordings model-by-model touches one
//! file. Strings a tool produces about its own execution (path-bearing errors)
//! stay in that tool - only Suspenders' own authored text lives here.
//!
//! Everything returned here either enters the Conversation or is the system
//! prompt. Markers are bracketed (`[...]`) so a small model can tell the Voice
//! from tool output.

use crate::content::{ContentBlock, Message, Role};

const SYSTEM_PROMPT: &str =
    "You are Suspenders, an interactive CLI coding agent. You work inside the user's \
project directory and complete coding tasks by calling tools. Your goal is to \
complete the task safely and correctly, adhering to the conventions of the \
project you are working in.

# Core Mandates

- Conventions: rigorously adhere to existing project conventions. Analyze the \
surrounding code, tests, and configuration before you change anything.
- Libraries and frameworks: never assume a library is available or idiomatic \
here. Verify it is already used in the project (Cargo.toml, imports, \
neighboring files) before you use it.
- Style and structure: mimic the style, structure, typing, naming, and \
patterns of the existing code.
- Comments: default to none. Add a comment only when the why cannot be shown in \
the code. Never narrate your changes or address the user in comments.
- Proactiveness: fulfill the request thoroughly. When you change code, add a \
test that exercises the new behavior - existing tests passing alone does not \
confirm new code works.
- Do not revert: do not revert changes you did not make; revert your own only \
if they caused an error or the user asked.

# Workflow

- Understand. Before changing code, find the relevant parts: use glob to find \
files by name or pattern, grep to search for symbols and strings, list_files to \
see a directory, and read_file to read what you will change or must quote \
exactly. Read only what you need - avoid reading whole files or trees you will \
not touch.
- Plan. For multi-step work, call todo_write to record your task list, then \
keep it current - mark one item in_progress and check items off as you finish. \
Skip it for simple one-step tasks.
- Implement. Make small, targeted edits with edit_file; use write_file only for \
new files. Do not add features or refactor beyond what was asked. Three similar \
lines beat a premature abstraction. Prefer editing existing files over creating \
new ones.
- Verify. After meaningful changes, run the tests or the compiler with \
run_command. Never assume the test command - find it (README, Cargo.toml, \
neighboring patterns). Report faithfully: if tests fail, say so with the \
output; if you did not verify, say so - never claim green when it is not.

# Rules

- Never fabricate file contents, paths, or command results. Trust only tool output.
- When you refer to code, name the file and the function - never a line number. You do not see line numbers, so any line number you write is made up. Quoting a line number printed by a compiler or test error is fine.
- Fix the code under test, not the tests; change a test only when the task says the test is wrong. Adding new tests for new behavior is always correct and expected.
- When building something new from a spec, grow it in verified steps: start with the smallest slice that compiles and passes at least one test, then add one behavior at a time, re-running the tests after each addition, until every behavior in the spec is covered. If the code stops compiling, fix that before adding anything else - a tree that will not build makes every other step blind.
- Run commands whole; never pipe their output through head, tail, or wc to shorten it. The harness already truncates long output while keeping the exit code, and under pipefail an early-closing consumer like head can kill the command and make a passing run report failure.
- Prefer a build or test runner's own quiet flags so passing runs stay short: for example `cargo nextest run --status-level fail` or `cargo build -q`. Progress lines and per-test PASS lines waste your context; failures still print in full.
- If a tool returns an error, adjust your input and try again.
- Keep edits minimal. Do not rewrite a whole file to change one line.

# Tone and style

- Concise and direct. This is a CLI and the output is monospace: no chitchat, \
no preamble or postamble.
- Use tools for actions, text only to communicate. When the task is done, reply \
with a concise summary of what changed.
- If you cannot or will not do something, say so briefly without over-explaining.

# Examples

user: 1 + 2
model: 3

user: where is retry handled?
model: [calls grep for \"retry\" across src]
[calls read_file on src/net/client.rs]
Retries live in src/net/client.rs, in send_with_backoff. Want me to open it?

user: run_command times out on slow hosts; bump the default to 30s
model: [calls todo_write: [~] find the timeout default, [ ] bump it to 30s, [ ] update the test]
[calls grep for \"timeout\" across src/tools]
[calls read_file on src/tools/run_command.rs]
[calls edit_file on src/tools/run_command.rs]
[calls run_command: cargo nextest run --status-level fail]
Bumped the default timeout in run_command from 10s to 30s and updated the \
timeout test. Suite is green.

user: write tests for the parse_flags function
model: [calls read_file on src/cli/flags.rs]
[calls read_file on the existing flags tests to match their style]
[calls edit_file to add the new cases]
[calls run_command: cargo nextest run --status-level fail]
Added three cases to the flags tests covering the unknown-flag, empty-value, \
and repeated-flag paths. All green.
";

/// The default system prompt: who the model is and how to work, nothing else.
/// Tool calling is native tool_use, so there is no tool-format teaching here
/// (ADR-0003).
pub fn system_prompt() -> &'static str {
    SYSTEM_PROMPT
}

/// Tool Result for a run_command Tool Call the user denied.
pub fn command_denied() -> &'static str {
    "[command denied by user]"
}

/// The user-role nudge injected when the next-speaker check decides the model
/// should keep going after a no-tool-call Pass (ADR-0043, qwen-code's
/// "Please continue." continuation). It enters the Conversation as an ordinary
/// user message (unstamped), prompting the model to take the next action it
/// announced but did not execute.
pub fn please_continue() -> &'static str {
    "Please continue."
}

/// The Voice-neutral confirmation the todo_write Tool returns as its Tool
/// Result.
pub fn todos_confirmation() -> &'static str {
    "[todos updated]"
}

/// Assistant marker closing a Run that hit its Run Limit.
pub fn run_limit_marker() -> &'static str {
    "[turn limit reached - reply to continue]"
}

/// Tool Result answering a Tool Call from a max_tokens-truncated response
/// (ADR-0009).
pub fn truncated_call_reissue() -> &'static str {
    "[response was cut by max_tokens - the call may be incomplete, re-issue it]"
}

/// Tool Result answering a Tool Call left unanswered when the Conversation
/// crossed Providers (ADR-0037: the ADR-0004/0009 orphan machinery, relocated
/// to the transform pass). An error answer, like ADR-0009's: the model should
/// re-issue the call if it still needs the result.
pub fn orphaned_call_answer() -> &'static str {
    "[this call's result was lost in a model switch - re-issue the call if you still need it]"
}

/// Marker prefixed to an error Tool Result's content on the openai-completions
/// wire (ADR-0037): that dialect's `role:"tool"` message has no error slot, so
/// the failure fact rides in-band. The anthropic-messages wire keeps its
/// native `is_error` field and never carries this marker.
pub fn tool_error_marker() -> &'static str {
    "[tool error]"
}

/// Assistant marker closing a Run the loop-detector terminated: the model was
/// stuck emitting the identical Tool Call batch Pass after Pass (a passive
/// circuit breaker - no steering text is injected, only this close marker).
pub fn loop_stall_marker() -> &'static str {
    "[turn ended - the model was repeating itself; reply to continue]"
}

/// Assistant marker closing a Run an after-Pass hook stopped.
pub fn run_stopped_marker() -> &'static str {
    "[turn stopped - reply to continue]"
}

/// Assistant marker when max_tokens truncation left no usable content.
pub fn truncation_marker() -> &'static str {
    "[response truncated by max_tokens]"
}

/// Assistant marker for a response with zero content blocks.
pub fn empty_response_marker() -> &'static str {
    "[empty response]"
}

/// Assistant marker closing a cancelled Run (keeps roles alternating).
pub fn run_cancelled_marker() -> &'static str {
    "[turn cancelled by user]"
}

/// Assistant marker closing a failed or crashed Run.
pub fn run_failed_marker() -> &'static str {
    "[turn failed]"
}

/// Tool Result for a Tool Call whose input JSON did not decode.
pub fn malformed_input(raw: &str) -> String {
    format!("[tool input was not valid JSON - resend as valid JSON] {raw}")
}

/// Result Cap marker for head-only shaping: appended after the kept head of an
/// oversized Tool Result.
pub fn truncated_output(total: usize, kept: usize) -> String {
    format!("\n[truncated: output is {total} chars, showing the first {kept}]")
}

/// Result Cap marker for read_file's line-boundary shaping: appended after the
/// kept lines, naming the exact `start_line` that continues the read.
pub fn truncated_file(last_shown: usize, last_line: usize) -> String {
    format!(
        "\n[truncated at line {last_shown} of {last_line} - continue with read_file start_line {}]",
        last_shown + 1
    )
}

/// Result Cap marker for head+tail shaping (run_command): replaces the middle
/// of an oversized Tool Result.
pub fn omitted_middle(omitted: usize, total: usize) -> String {
    format!("\n[{omitted} of {total} chars omitted from the middle of this output]\n")
}

const COMPACTION_PROMPT: &str = "Summarize the coding session below. Extract only facts from the
conversation - do not invent or interpret. Fill in this markdown
skeleton exactly, keeping every section heading:

## Task
One sentence describing what the session is trying to accomplish.

## Completed
- Each finished step or change, one bullet per item.

## In progress
- Work that is started but not finished, one bullet per item.

## Decisions made
- Each choice and its reason, one bullet per item.

## Key identifiers
- Exact file paths, function names, error messages, and commands that
  appeared. Copy them verbatim - do not paraphrase.

## Next step
The single most important thing to do next.

Write \"- none\" under any section with nothing to report. Do not add
sections. Do not wrap the output in code fences.
";

/// The prompt sent to the LLM for compaction (summarizing old messages).
pub fn compaction_prompt() -> &'static str {
    COMPACTION_PROMPT
}

/// Accumulated file operations across a session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileOps {
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// The harness-owned mechanical framing appended to a compaction summary.
///
/// `original_task` is the verbatim first user text (or `None` before it is
/// known); `file_ops` is the accumulated read/modified files.
pub fn compaction_facts(original_task: Option<&str>, file_ops: &FileOps) -> String {
    let task = original_task.unwrap_or("- none");
    format!(
        "\n## Original task (verbatim)\n{task}\n\n## Files touched this session\n{}\n{}",
        file_list("Read", &file_ops.read_files),
        file_list("Modified", &file_ops.modified_files),
    )
}

fn file_list(label: &str, paths: &[String]) -> String {
    if paths.is_empty() {
        format!("{label}: - none")
    } else {
        let joined = paths
            .iter()
            .map(|p| format!("- {p}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("{label}:\n{joined}")
    }
}

/// The content block for a compaction summary: a single text block wrapping
/// the summary produced by the LLM, carrying a re-read caution.
pub fn summary_block(summary: &str) -> ContentBlock {
    ContentBlock::Text {
        text: format!(
            "[Session summary from compaction. The narrative is paraphrase, not source - re-read a file before citing or editing its specifics.]\n\n{summary}"
        ),
    }
}

/// Serializes a list of Conversation messages into a flat text block for the
/// LLM compaction call, optionally prefixed with the previous summary. Tool
/// Result content is truncated to keep the compaction call itself in budget.
pub fn serialize_for_compaction(messages: &[Message], previous_summary: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(prev) = previous_summary {
        parts.push(format!("[Previous summary]\n{prev}\n"));
    }
    parts.push("[Conversation to summarize]\n".to_string());
    for msg in messages {
        parts.push(serialize_message(msg));
    }
    parts.join("\n")
}

fn serialize_message(message: &Message) -> String {
    let texts: Vec<String> = match message.role {
        Role::User => message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(format!("User: {text}")),
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let label = if *is_error {
                        format!("Tool error ({tool_use_id})")
                    } else {
                        format!("Tool result ({tool_use_id})")
                    };
                    Some(format!("{label}: {}", truncate_for_serialization(content)))
                }
                _ => None,
            })
            .collect(),
        Role::Assistant => message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(format!("Assistant: {text}")),
                ContentBlock::ToolUse { name, input, .. } => {
                    Some(format!("Tool call: {name}({input})"))
                }
                _ => None,
            })
            .collect(),
    };
    texts.join("\n")
}

/// Maximum chars kept per Tool Result during compaction serialization. Mirrors
/// baud's 2000-char gate; sliced on a char boundary to stay valid UTF-8.
const COMPACTION_SERIALIZE_CAP: usize = 2000;

fn truncate_for_serialization(content: &str) -> String {
    if content.len() > COMPACTION_SERIALIZE_CAP {
        let head: String = content.chars().take(COMPACTION_SERIALIZE_CAP).collect();
        format!(
            "{head}\n[... {} more chars]",
            content.len() - COMPACTION_SERIALIZE_CAP
        )
    } else {
        content.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- system_prompt/0 ----

    #[test]
    fn system_prompt_tells_the_model_to_maintain_its_task_list() {
        assert!(system_prompt().contains("todo_write"));
        assert!(system_prompt().contains("in_progress"));
    }

    #[test]
    fn system_prompt_tells_the_model_to_fix_the_code_under_test() {
        assert!(system_prompt().contains("Fix the code under test, not the tests"));
    }

    #[test]
    fn system_prompt_bans_invented_line_numbers_but_allows_quoting_tool_output() {
        let prompt = system_prompt();
        assert!(prompt.contains("name the file and the function - never a line number"));
        assert!(
            prompt.contains("Quoting a line number printed by a compiler or test error is fine")
        );
    }

    #[test]
    fn system_prompt_sequences_new_builds_without_capping_scope() {
        let prompt = system_prompt();
        // Sequencing: verified increments, compile errors fixed first.
        assert!(prompt.contains("smallest slice that compiles and passes at least one test"));
        assert!(prompt.contains("a tree that will not build makes every other step blind"));
        // Guard against the cycle-002 over-read ("do less"): the rule must
        // explicitly demand full spec coverage.
        assert!(prompt.contains("until every behavior in the spec is covered"));
    }

    #[test]
    fn system_prompt_steers_off_piping_command_output_through_head() {
        let prompt = system_prompt();
        assert!(prompt.contains("Run commands whole"));
        assert!(prompt.contains("never pipe their output through head, tail, or wc"));
        // Both reasons: the harness truncates, and pipefail runs an
        // early-closing consumer into a spurious failure.
        assert!(prompt.contains("truncates long output while keeping the exit code"));
        assert!(prompt.contains("pipefail"));
        assert!(prompt.contains("make a passing run report failure"));
    }

    #[test]
    fn system_prompt_steers_toward_quiet_flags_for_builds_and_tests() {
        let prompt = system_prompt();
        // The sanctioned way to shorten output (piping is the forbidden way):
        // the runner's own quiet flags, with a concrete example, and the
        // reassurance that failures are not silenced by them.
        assert!(prompt.contains("quiet flags"));
        assert!(prompt.contains("--status-level fail"));
        assert!(prompt.contains("failures still print in full"));
    }

    #[test]
    fn system_prompt_mandates_convention_and_library_verification() {
        let prompt = system_prompt();
        assert!(prompt.contains("Core Mandates"));
        // Do not assume a library is present; verify it is already used.
        assert!(prompt.contains("never assume a library is available"));
        assert!(prompt.contains("Verify it is already used in the project"));
        // Comments default to none - the why, not narration.
        assert!(prompt.contains("Comments: default to none"));
    }

    #[test]
    fn system_prompt_carries_the_understand_verify_workflow() {
        let prompt = system_prompt();
        assert!(prompt.contains("Understand"));
        assert!(prompt.contains("Verify"));
        // The Understand step points the model at inline exploration - glob,
        // grep, list_files, read_file - not a delegated Scout.
        assert!(prompt.contains("use glob to find files"));
        assert!(prompt.contains("grep to search for symbols"));
        assert!(!prompt.contains("explore tool"));
        // Faithful reporting: never claim a green suite that is not green.
        assert!(prompt.contains("never claim green when it is not"));
    }

    #[test]
    fn system_prompt_sets_a_concise_no_chitchat_tone() {
        let prompt = system_prompt();
        assert!(prompt.contains("Tone and style"));
        assert!(prompt.contains("no chitchat"));
    }

    #[test]
    fn system_prompt_teaches_by_worked_example() {
        let prompt = system_prompt();
        assert!(prompt.contains("# Examples"));
        // The terse knowledge answer and a bracketed tool-call stage direction.
        assert!(prompt.contains("1 + 2"));
        assert!(prompt.contains("[calls grep"));
    }

    // ---- todos_confirmation/0 ----

    #[test]
    fn todos_confirmation_is_a_short_confirmation_string() {
        assert!(todos_confirmation().chars().count() < 120);
    }

    // ---- please_continue/0 ----

    #[test]
    fn please_continue_is_the_next_speaker_nudge() {
        // The unstamped user nudge injected on a `model` next-speaker verdict
        // (ADR-0043): short, plain, no bracketed-marker framing (it is an
        // ordinary user turn, not a Voice marker).
        let nudge = please_continue();
        assert_eq!(nudge, "Please continue.");
        assert!(!nudge.starts_with('['));
        assert!(!nudge.contains('\u{2014}')); // em-dash
        assert!(!nudge.contains('\u{2013}')); // en-dash
    }

    // ---- compaction_prompt/0 ----

    #[test]
    fn compaction_prompt_demands_all_six_fixed_sections() {
        let prompt = compaction_prompt();
        for section in [
            "Task",
            "Completed",
            "In progress",
            "Decisions made",
            "Key identifiers",
            "Next step",
        ] {
            assert!(
                prompt.contains(section),
                "compaction prompt is missing the {section:?} section"
            );
        }
    }

    #[test]
    fn compaction_prompt_shows_the_markdown_skeleton() {
        let prompt = compaction_prompt();
        for heading in [
            "## Task",
            "## Completed",
            "## In progress",
            "## Decisions made",
            "## Key identifiers",
            "## Next step",
        ] {
            assert!(
                prompt.contains(heading),
                "compaction prompt is missing the heading {heading:?}"
            );
        }
    }

    #[test]
    fn compaction_prompt_names_the_mechanical_identifiers() {
        let prompt = compaction_prompt();
        assert!(prompt.contains("file path"));
        assert!(prompt.contains("function name"));
        assert!(prompt.contains("error message"));
        assert!(prompt.contains("command"));
    }

    // ---- compaction_facts/2 ----

    #[test]
    fn compaction_facts_carries_the_verbatim_original_task() {
        let facts = compaction_facts(
            Some("Fix the flaky test in user_test.exs"),
            &FileOps::default(),
        );
        assert!(facts.contains("Fix the flaky test in user_test.exs"));
    }

    #[test]
    fn compaction_facts_lists_accumulated_read_and_modified_files() {
        let facts = compaction_facts(
            Some("original task"),
            &FileOps {
                read_files: vec!["lib/a.ex".into(), "lib/b.ex".into()],
                modified_files: vec!["lib/c.ex".into()],
            },
        );
        assert!(facts.contains("lib/a.ex"));
        assert!(facts.contains("lib/b.ex"));
        assert!(facts.contains("lib/c.ex"));
    }

    #[test]
    fn compaction_facts_handles_absent_task_and_empty_ops() {
        let facts = compaction_facts(None, &FileOps::default());
        assert!(!facts.is_empty());
    }

    // ---- tool_error_marker/0 ----

    #[test]
    fn tool_error_marker_is_a_short_bracketed_marker() {
        let marker = tool_error_marker();
        assert!(marker.starts_with('['));
        assert!(marker.ends_with(']'));
        assert!(marker.chars().count() < 40);
        assert!(marker.contains("error"));
        assert!(!marker.contains('\u{2014}')); // em-dash
        assert!(!marker.contains('\u{2013}')); // en-dash
    }

    // ---- orphaned_call_answer/0 ----

    #[test]
    fn orphaned_call_answer_is_a_bracketed_error_telling_the_model_to_reissue() {
        let answer = orphaned_call_answer();
        assert!(answer.starts_with('['));
        assert!(answer.ends_with(']'));
        assert!(answer.contains("model switch"));
        assert!(answer.contains("re-issue"));
        assert!(!answer.contains('\u{2014}')); // em-dash
        assert!(!answer.contains('\u{2013}')); // en-dash
    }
}
