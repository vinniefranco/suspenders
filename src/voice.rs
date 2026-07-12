//! The Voice (CONTEXT.md): every Suspenders-voiced string the model reads —
//! the system prompt, the Nudges, and the markers that enter the
//! Conversation. The boundary is voice, not arity: wording may be
//! parameterized, but Suspenders authors it.
//!
//! Wording is the highest-leverage tuning surface for small local models;
//! owning it in one module means swapping wordings model-by-model touches one
//! file. Strings a tool produces about its own execution (path-bearing errors)
//! stay in that tool — only Suspenders' own steering text lives here.
//!
//! Everything returned here either enters the Conversation or is the system
//! prompt. Markers are bracketed (`[...]`) so a small model can tell the Voice
//! from tool output.

use crate::content::{ContentBlock, Message, Role};

const SYSTEM_PROMPT: &str =
    "You are Suspenders, an expert coding agent. You work inside the user's project \
directory and complete coding tasks by calling tools.

Follow this workflow for every task:
1. Plan. Call the plan tool early with your goal and steps; update it as steps complete.
2. Explore. Call the explore tool with one focused question (\"where is X \
handled?\", \"how does Y work?\") - a Scout searches the code and reports back \
so your own context stays small. Ask several explore questions for a broad \
task, one question each. Use grep or list_files yourself only for a single \
quick lookup.
3. Read. Use read_file only on files you will change or must quote exactly.
4. Edit. Make small, targeted edits with edit_file. Use write_file only for new files.
5. Verify. After meaningful changes, run the tests or the compiler with run_command. If you added new behavior, write a test that exercises it and run the full suite - existing tests passing alone does not confirm new code works.

Rules:
- Keep your plan current with the plan tool as you finish each step.
- Delegate searching to explore; do not read file after file yourself.
- Never fabricate file contents, paths, or command results. Trust only tool output.
- Fix the code under test, not the tests; change a test only when the task says the test is wrong. Adding new tests for new behavior is always correct and expected.
- If a tool returns an error, adjust your input and try again.
- Keep edits minimal. Do not rewrite a whole file to change one line.
- Work step by step. One tool call at a time is fine.
- When the task is done, reply with a concise summary of what changed.
";

/// The default system prompt: who the model is and how to work, nothing else.
/// Tool calling is native tool_use, so there is no tool-format teaching here
/// (ADR-0003).
pub fn system_prompt() -> &'static str {
    SYSTEM_PROMPT
}

/// Eviction's elision marker: replaces an evicted Tool Result's content.
pub fn elision_marker() -> &'static str {
    "[result elided - re-run the tool if needed]"
}

/// Tool Result for a run_command Tool Call the user denied.
pub fn command_denied() -> &'static str {
    "[command denied by user]"
}

// The Anchor's opening line. Anchor blocks are recognized by this prefix, so
// Eviction can tell a stale Anchor from an ordinary message.
const ANCHOR_PREFIX: &str = "[anchor - current goal and plan";

/// The Voice-neutral confirmation the plan Tool returns as its Tool Result.
pub fn plan_confirmation() -> &'static str {
    "[plan recorded]"
}

/// The Anchor (CONTEXT.md): the Voice-framed wrapper around the verbatim
/// original task statement and the current Plan. The framing is Suspenders'
/// voice; `task` and `plan` ride verbatim. A `None` plan means the model has
/// not set one yet.
pub fn anchor(task: &str, plan: Option<&str>) -> String {
    let plan_text = plan.unwrap_or("- no plan set yet; call the plan tool");
    format!(
        "{ANCHOR_PREFIX} - keep working toward this]\n\nOriginal task:\n{task}\n\nCurrent plan:\n{plan_text}"
    )
}

/// The marker a superseded Anchor is elided to when Eviction reclaims it.
/// Distinct from the tool-result elision marker so the two never collide, and
/// still recognized by [`is_anchor`] so Eviction never re-elides it.
pub fn anchor_elision_marker() -> &'static str {
    "[stale anchor elided - a fresher anchor is below]"
}

/// Whether a content block is an Anchor block (a live Anchor or an elided one).
pub fn is_anchor(block: &ContentBlock) -> bool {
    match block {
        ContentBlock::Text { text } => {
            text.starts_with(ANCHOR_PREFIX) || text == anchor_elision_marker()
        }
        _ => false,
    }
}

/// Duplicate Tool Call Nudge: rides as the repeated call's Tool Result.
pub fn duplicate_call_nudge() -> &'static str {
    "[identical Tool Call repeated - its result is already above; act on it instead of re-running]"
}

/// Assistant marker closing a Turn that hit its Turn Limit.
pub fn turn_limit_marker() -> &'static str {
    "[turn limit reached - reply to continue]"
}

/// One-shot warning riding the tool-results user message when the Turn Limit
/// is `passes_left` Passes away.
pub fn wrap_up_warning(passes_left: u64) -> String {
    format!(
        "[only {passes_left} passes left in this turn - wrap up now: deliver your conclusion, or state plainly what remains undone]"
    )
}

/// User message riding the tool-results message before the Verification Pass
/// (ADR-0016).
pub fn verification_pass_prompt() -> &'static str {
    "[only 2 passes left and your changes are unverified - the next pass offers run_command ONLY: run your verification now]"
}

/// User message riding the tool-results message before the Turn's final Pass
/// (ADR-0015).
pub fn final_pass_prompt() -> &'static str {
    "[last pass - tools are withdrawn; state what you accomplished, what remains undone, and whether your changes are verified]"
}

/// Tool Result answering a Tool Call from a max_tokens-truncated response
/// (ADR-0009).
pub fn truncated_call_nudge() -> &'static str {
    "[response was cut by max_tokens - the call may be incomplete, re-issue it]"
}

/// Assistant marker closing a Turn an after-Pass hook stopped.
pub fn turn_stopped_marker() -> &'static str {
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

/// Verify Nudge: user-role message sent when writes went unverified.
pub fn verify_nudge() -> &'static str {
    "[files changed but nothing verified - run the tests or compile, then summarize]"
}

/// Verify-failed Nudge: sent when the model finishes while its most recent
/// run_command failed.
pub fn verify_failed_nudge() -> &'static str {
    "[the last command you ran failed - if that was your verification, fix the problem and re-run it; if you are stuck, say so plainly instead of finishing]"
}

/// Empty-response Nudge: sent when a Pass's response carried zero content
/// blocks. Fires at most once per Turn.
pub fn empty_response_nudge() -> &'static str {
    "[your reply was empty - continue with your next step, or state plainly what is blocking you]"
}

/// Explore Nudge: user-role text redirecting inline reading to the explore
/// Tool.
pub fn explore_nudge() -> &'static str {
    "[reading file after file fills your context - dispatch explore with one focused question instead; a Scout searches and reports back]"
}

/// Assistant marker closing a cancelled Turn (keeps roles alternating).
pub fn turn_cancelled_marker() -> &'static str {
    "[turn cancelled by user]"
}

/// Assistant marker closing a failed or crashed Turn.
pub fn turn_failed_marker() -> &'static str {
    "[turn failed]"
}

/// An error category for the consecutive-failure Nudge summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCategory {
    Enoent,
    InputError,
    NotFound,
    Timeout,
    Denied,
    PathError,
    CommandError,
    Unknown,
}

/// Consecutive-failure Nudge suffix, appended to the Nth failing Tool Result
/// for one tool. `categories` summarises the error types seen so far as
/// `(category, tally)` pairs, so the nudge can tell the model *what kind* of
/// failure pattern it is in.
pub fn failure_nudge(count: u64, tool_name: &str, categories: &[(FailureCategory, u64)]) -> String {
    // Sort by tally descending (stable, as Enum.sort_by/-tally in baud); the
    // dominant category is whatever this sort puts first.
    let mut sorted: Vec<(FailureCategory, u64)> = categories.to_vec();
    sorted.sort_by_key(|b| std::cmp::Reverse(b.1));

    let categories_desc = describe_categories(&sorted);
    // A CommandError streak means the code under test keeps failing, not the
    // tool call itself, so "step back" alone tends to produce another blind
    // re-run; this variant prescribes the narrow debug loop instead.
    if let Some((FailureCategory::CommandError, _)) = sorted.first() {
        format!(
            "\n[{count} consecutive {tool_name} failures - {categories_desc}. Pick the single simplest failing case, trace its exact input through your code step by step, and make one targeted fix for it before re-running]"
        )
    } else {
        format!("\n[{count} consecutive {tool_name} failures - step back: {categories_desc}]")
    }
}

fn describe_category(cat: FailureCategory, n: u64) -> String {
    match cat {
        FailureCategory::Enoent => format!("{n}x file not found (enoent)"),
        FailureCategory::InputError => format!("{n}x invalid input (wrong field names or types)"),
        FailureCategory::NotFound => format!("{n}x content not found in file"),
        FailureCategory::Timeout => format!("{n}x command timed out"),
        FailureCategory::Denied => format!("{n}x command denied"),
        FailureCategory::PathError => format!("{n}x path escapes project root"),
        FailureCategory::CommandError => format!("{n}x command exited with error"),
        FailureCategory::Unknown => format!("{n}x other error"),
    }
}

// Expects `categories` already sorted by tally descending (failure_nudge owns
// the sort so the dominant-category check and this description agree).
fn describe_categories(categories: &[(FailureCategory, u64)]) -> String {
    if categories.is_empty() {
        return "re-read the file or try a different approach".to_string();
    }

    let parts: Vec<String> = categories
        .iter()
        .map(|(cat, n)| describe_category(*cat, *n))
        .collect();

    match parts.len() {
        0 => "re-read the file or try a different approach".to_string(),
        1 => parts[0].clone(),
        2 => format!("{} and {}", parts[0], parts[1]),
        _ => {
            let (last, head) = parts.split_last().unwrap();
            format!("{}; and {}", head.join("; "), last)
        }
    }
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

const SCOUT_SYSTEM_PROMPT: &str =
    "You are a Scout: a disposable, read-only explorer working inside the \
user's project directory. Another agent dispatched you to answer one \
focused question about the codebase. You cannot edit files, run \
commands, or dispatch further Scouts - your only tools are read_file, \
list_files, and grep.

Search efficiently: grep and list_files to locate things, read_file to \
confirm. Do not read whole trees; follow the question. When you have \
enough to answer, stop calling tools and reply with your findings report \
in exactly this shape, keeping every heading:

## Locations
- Each relevant place as file:line (or file when a line makes no sense),
  one bullet per item.

## How it works
A few sentences on how the relevant code behaves.

## What to read next
- The files or functions the dispatcher should open, one bullet per item.

## Open questions
- Anything you could not determine, one bullet per item. Write \"- none\"
  if there are none.

Report only what you actually found in tool output. Never invent file \
paths, line numbers, or behavior. Be terse.
";

/// The Scout's system prompt (CONTEXT.md: Scout).
pub fn scout_system_prompt() -> &'static str {
    SCOUT_SYSTEM_PROMPT
}

/// Marker prefixed to a Scout report when the Scout's own model call errored.
pub fn scout_llm_error() -> &'static str {
    "[scout stopped: the model call failed - partial findings below]"
}

/// Marker for a Scout that returned no findings.
pub fn scout_empty_findings() -> &'static str {
    "[scout returned no findings - try a narrower or different task]"
}

/// User message injected on the Scout's forced report Pass.
pub fn scout_report_now() -> &'static str {
    "[exploration budget exhausted - write your findings report now from what you have seen]"
}

/// Marker prefixed to a Scout report when the Scout hit its hard Pass cap.
pub fn scout_pass_cap(limit: u64) -> String {
    format!(
        "[scout hit its {limit}-pass exploration limit before reporting - partial findings below]"
    )
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

fn truncate_for_serialization(content: &str) -> String {
    // baud measures byte_size and slices by chars at 2000; mirror the byte
    // gate but slice on a char boundary so the output stays valid UTF-8.
    if content.len() > 2000 {
        let head: String = content.chars().take(2000).collect();
        format!("{head}\n[... {} more chars]", content.len() - 2000)
    } else {
        content.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::ContentBlock;

    // ---- system_prompt/0 ----

    #[test]
    fn system_prompt_tells_the_model_to_maintain_its_plan() {
        assert!(system_prompt().contains("plan"));
    }

    #[test]
    fn system_prompt_tells_the_model_to_fix_the_code_under_test() {
        assert!(system_prompt().contains("Fix the code under test, not the tests"));
    }

    // ---- anchor/2 ----

    #[test]
    fn anchor_wraps_verbatim_task_and_plan_in_voiced_framing() {
        let a = anchor(
            "Fix the flaky test",
            Some("Goal: green tests. 1. read [x] 2. fix [ ]"),
        );

        assert!(a.contains('['));
        assert!(a.contains(']'));
        assert!(a.contains("Fix the flaky test"));
        assert!(a.contains("Goal: green tests. 1. read [x] 2. fix [ ]"));
    }

    #[test]
    fn anchor_carries_the_task_even_without_a_plan() {
        let a = anchor("Fix the flaky test", None);
        assert!(a.contains("Fix the flaky test"));
    }

    #[test]
    fn anchor_no_plan_fallback_does_not_end_in_a_stray_closing_bracket() {
        let a = anchor("Fix the flaky test", None);
        assert!(!a.ends_with(']'));
        assert!(a.ends_with("- no plan set yet; call the plan tool"));
    }

    #[test]
    fn anchor_is_recognizable_by_is_anchor() {
        let a = anchor("task", Some("plan"));

        assert!(is_anchor(&ContentBlock::text(a.clone())));
        assert!(!is_anchor(&ContentBlock::text("an ordinary message")));
        assert!(!is_anchor(&ContentBlock::tool_result("x", a, false)));
    }

    #[test]
    fn elided_anchor_marker_is_distinct_and_recognized() {
        let marker = anchor_elision_marker();
        assert!(is_anchor(&ContentBlock::text(marker)));
        assert!(marker.contains('['));
    }

    // ---- plan_confirmation/0 ----

    #[test]
    fn plan_confirmation_is_a_short_confirmation_string() {
        assert!(plan_confirmation().chars().count() < 120);
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

    // ---- scout_system_prompt/0 ----

    #[test]
    fn scout_system_prompt_forces_a_structured_report() {
        let prompt = scout_system_prompt();
        assert!(prompt.contains("file:line"));
        assert!(prompt.to_lowercase().contains("read next"));
        assert!(prompt.to_lowercase().contains("open question"));
    }

    #[test]
    fn scout_system_prompt_tells_the_scout_it_is_read_only() {
        assert!(scout_system_prompt().to_lowercase().contains("read-only"));
    }

    // ---- scout markers ----

    #[test]
    fn scout_llm_error_is_bracketed() {
        let m = scout_llm_error();
        assert!(m.contains('['));
        assert!(m.contains(']'));
    }

    #[test]
    fn scout_empty_findings_is_bracketed() {
        let m = scout_empty_findings();
        assert!(m.contains('['));
        assert!(m.contains(']'));
    }

    #[test]
    fn scout_pass_cap_names_the_cap() {
        let m = scout_pass_cap(8);
        assert!(m.contains('['));
        assert!(m.contains('8'));
    }

    // ---- failure_nudge/3 ----

    #[test]
    fn failure_nudge_command_error_dominant_prescribes_the_debug_loop() {
        let nudge = failure_nudge(
            3,
            "run_command",
            &[
                (FailureCategory::CommandError, 2),
                (FailureCategory::Enoent, 1),
            ],
        );

        assert!(nudge.contains("3 consecutive run_command failures"));
        assert!(nudge.contains("Pick the single simplest failing case"));
        assert!(nudge.contains("one targeted fix for it before re-running"));
        assert!(!nudge.contains("step back"));
    }

    #[test]
    fn failure_nudge_enoent_dominant_keeps_the_step_back_wording() {
        let nudge = failure_nudge(
            3,
            "read_file",
            &[
                (FailureCategory::Enoent, 2),
                (FailureCategory::CommandError, 1),
            ],
        );

        assert!(nudge.contains("3 consecutive read_file failures - step back:"));
        assert!(!nudge.contains("Pick the single simplest failing case"));
    }

    #[test]
    fn failure_nudge_empty_categories_keeps_the_step_back_wording() {
        let nudge = failure_nudge(3, "read_file", &[]);

        assert!(nudge.contains("3 consecutive read_file failures - step back:"));
        assert!(nudge.contains("re-read the file or try a different approach"));
        assert!(!nudge.contains("Pick the single simplest failing case"));
    }

    // ---- explore_nudge/0 ----

    #[test]
    fn explore_nudge_is_a_bracketed_marker_naming_explore() {
        let nudge = explore_nudge();
        assert!(nudge.contains('['));
        assert!(nudge.contains(']'));
        assert!(nudge.contains("explore"));
        assert!(!nudge.contains('\u{2014}')); // em-dash
        assert!(!nudge.contains('\u{2013}')); // en-dash
    }
}
