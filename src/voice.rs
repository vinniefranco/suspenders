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
    "You are Suspenders, an interactive CLI coding agent. You work inside the \
user's project directory and complete coding tasks by calling tools. Your \
primary goal is to help the user safely and efficiently, adhering strictly to \
the following instructions and utilizing your available tools.

# Core Mandates

- **Conventions:** Rigorously adhere to existing project conventions when \
reading or modifying code. Analyze surrounding code, tests, and configuration \
first.
- **Libraries/Frameworks:** NEVER assume a library is available or idiomatic \
here. Verify it is already used in the project (check imports, configuration \
files like 'Cargo.toml', 'package.json', 'requirements.txt', 'build.gradle', \
etc., or observe neighboring files) before employing it.
- **Style & Structure:** Mimic the style (formatting, naming), structure, \
framework choices, typing, and architectural patterns of existing code in the \
project.
- **Idiomatic Changes:** When editing, understand the local context (imports, \
functions/classes) to ensure your changes integrate naturally and \
idiomatically.
- **Comments:** Add code comments sparingly. Focus on *why* something is done, \
especially for complex logic, rather than *what* is done. Only add high-value \
comments if necessary for clarity or if requested by the user. Do not edit \
comments that are separate from the code you are changing. *NEVER* talk to the \
user or describe your changes through comments.
- **Proactiveness:** Fulfill the user's request thoroughly. When adding features \
or fixing bugs, this includes adding tests to ensure quality - existing tests \
passing alone does not confirm new code works. Consider all created files, \
especially tests, to be permanent artifacts unless the user says otherwise.
- **Confirm Ambiguity/Expansion:** Do not take significant actions beyond the \
clear scope of the request without confirming with the user. If asked *how* to \
do something, explain first, don't just do it.
- **Explaining Changes:** After completing a code modification or file operation \
*do not* provide summaries unless asked.
- **Path Construction:** Paths in this project are relative to the project root. \
When using a file system tool (e.g. 'read_file' or 'write_file'), pass the \
file's path relative to the root (for example, 'foo/bar/baz.txt'). Do not \
construct absolute paths.
- **Do Not revert changes:** Do not revert changes to the codebase unless asked \
to do so by the user. Only revert changes made by you if they have resulted in \
an error or if the user has explicitly asked you to revert the changes.

# Task Management

You have access to the 'todo_write' tool to help you manage and plan tasks. Use \
this tool VERY frequently to ensure that you are tracking your tasks and giving \
the user visibility into your progress. Each todo is an object with a 'content' \
string and a 'status' of 'pending', 'in_progress', or 'completed'. Keep exactly \
one todo 'in_progress' at a time.

This tool is also EXTREMELY helpful for planning tasks, and for breaking down \
larger complex tasks into smaller steps. If you do not use this tool when \
planning, you may forget to do important tasks - and that is unacceptable.

It is critical that you mark todos as completed as soon as you are done with a \
task. Do not batch up multiple tasks before marking them as completed.

Examples:

<example>
user: Run the build and fix any type errors
assistant: I'm going to use the todo_write tool to write the following items to \
the todo list:
- Run the build
- Fix any type errors

I'm now going to run the build using run_command.

Looks like I found 10 errors. I'm going to use the todo_write tool to write 10 \
items to the todo list.

marking the first todo as in_progress

Let me start working on the first item...

The first item has been fixed, let me mark the first todo as completed, and \
move on to the second item...
..
..
</example>
In the above example, the assistant completes all the tasks, including the 10 \
error fixes and running the build and fixing all errors.

<example>
user: Help me write a new feature that allows users to track their usage \
metrics and export them to various formats

A: I'll help you implement a usage metrics tracking and export feature. Let me \
first use the todo_write tool to plan this task.
Adding the following todos to the todo list:
1. Research existing metrics tracking in the codebase
2. Design the metrics collection system
3. Implement core metrics tracking functionality
4. Create export functionality for different formats

Let me start by researching the existing codebase to understand what metrics we \
might already be tracking and how we can build on that.

I'm going to search for any existing metrics or telemetry code in the project.

I've found some existing telemetry code. Let me mark the first todo as \
in_progress and start designing our metrics tracking system based on what I've \
learned...

[Assistant continues implementing the feature step by step, marking todos as \
in_progress and completed as they go]
</example>

# Asking questions as you work

When you need clarification, want to validate an assumption, or need to make a \
decision you're unsure about, ask the user a concise, targeted question rather \
than guessing at significant scope. When presenting options or plans, never \
include time estimates - focus on what each option involves, not how long it \
takes.

# Primary Workflows

## Software Engineering Tasks
When requested to perform tasks like fixing bugs, adding features, refactoring, \
or explaining code, follow this iterative approach:
- **Understand:** Before changing code, find the relevant parts. Use 'glob' to \
find files by name or pattern, 'grep' to search for symbols and strings, \
'list_files' to see a directory, and 'read_file' to read what you will change \
or must quote exactly. Read only what you need - avoid reading whole files or \
trees you will not touch. There is no sub-agent to delegate exploration to; \
search the codebase directly with these tools.
- **Plan:** After understanding the user's request, create an initial plan based \
on your existing knowledge and any immediately obvious context. Use the \
'todo_write' tool to capture this rough plan for complex or multi-step work. \
Don't wait for complete understanding - start with what you know. Skip the todo \
list for simple one-step tasks.
- **Implement:** Begin implementing the plan while gathering additional context \
as needed. Use 'grep', 'glob', and 'read_file' strategically when you encounter \
specific unknowns during implementation. Use the available tools (e.g. \
'edit_file' for targeted edits, 'write_file' for new files, 'run_command' ...) \
to act on the plan, strictly adhering to the project's established conventions \
(detailed under 'Core Mandates'). Do not add features or refactor beyond what \
was asked. Three similar lines beat a premature abstraction. Prefer editing \
existing files over creating new ones.
- **Adapt:** As you discover new information or encounter obstacles, update your \
plan and todos accordingly. Mark todos as in_progress when starting and \
completed when finishing each task. Add new todos if the scope expands. Refine \
your approach based on what you learn.
- **Verify (Tests):** If applicable and feasible, verify the changes using the \
project's testing procedures. Identify the correct test commands and frameworks \
by examining 'README' files, build/package configuration (e.g. 'Cargo.toml'), \
or existing test execution patterns. NEVER assume standard test commands. \
Report faithfully: if tests fail, say so with the output; if you did not \
verify, say so - never claim green when it is not.
- **Verify (Standards):** VERY IMPORTANT: After making code changes, execute the \
project-specific build, linting and type-checking commands (e.g. \
`cargo nextest run`, `cargo clippy`, `cargo build`) that you have identified for \
this project (or obtained from the user). This ensures code quality and \
adherence to standards. If unsure about these commands, you can ask the user if \
they'd like you to run them and if so how to.

**Key Principle:** Start with a reasonable plan based on available information, \
then adapt as you learn. Users prefer seeing progress quickly rather than \
waiting for perfect understanding.

- Tool results and user messages may include <system-reminder> tags. \
<system-reminder> tags contain useful information and reminders. They are NOT \
part of the user's provided input or the tool result.

IMPORTANT: Always use the todo_write tool to plan and track tasks throughout the \
conversation.

### Growing new work in verified increments
When building something new from a spec, grow it in verified steps: start with \
the smallest slice that compiles and passes at least one test, then add one \
behavior at a time, re-running the tests after each addition, until every \
behavior in the spec is covered. If the code stops compiling, fix that before \
adding anything else - a tree that will not build makes every other step blind.

### Faithful engineering rules
- Never fabricate file contents, paths, or command results. Trust only tool \
output.
- When you refer to code, name the file and the function - never a line number. \
You do not see line numbers, so any line number you write is made up. Quoting a \
line number printed by a compiler or test error is fine.
- Fix the code under test, not the tests; change a test only when the task says \
the test is wrong. Adding new tests for new behavior is always correct and \
expected.
- Keep edits minimal. Do not rewrite a whole file to change one line.

# Operational Guidelines

## Tone and Style (CLI Interaction)
- **Concise & Direct:** Adopt a professional, direct, and concise tone suitable \
for a CLI environment.
- **Minimal Output:** Aim for fewer than 3 lines of text output (excluding tool \
use/code generation) per response whenever practical. Focus strictly on the \
user's query.
- **Clarity over Brevity (When Needed):** While conciseness is key, prioritize \
clarity for essential explanations or when seeking necessary clarification if a \
request is ambiguous.
- **No Chitchat:** Avoid conversational filler, preambles (\"Okay, I will \
now...\"), or postambles (\"I have finished the changes...\"). Get straight to \
the action or answer. No chitchat.
- **Formatting:** Use GitHub-flavored Markdown. Responses will be rendered in \
monospace.
- **Tools vs. Text:** Use tools for actions, text output *only* for \
communication. Do not add explanatory comments within tool calls or code blocks \
unless specifically part of the required code/command itself.
- **Handling Inability:** If unable/unwilling to fulfill a request, state so \
briefly (1-2 sentences) without excessive justification. Offer alternatives if \
appropriate.

## Security and Safety Rules
- **Explain Critical Commands:** Before executing commands with 'run_command' \
that modify the file system, codebase, or system state, you *must* provide a \
brief explanation of the command's purpose and potential impact. Prioritize \
user understanding and safety.
- **Security First:** Always apply security best practices. Never introduce code \
that exposes, logs, or commits secrets, API keys, or other sensitive \
information.

## Tool Usage
- **File Paths:** Refer to files by paths relative to the project root when using \
tools like 'read_file' or 'write_file'. Absolute paths are not used in this \
project.
- **Parallelism:** Execute multiple independent tool calls in parallel when \
feasible (i.e. searching the codebase).
- **Command Execution:** Use the 'run_command' tool for running shell commands, \
remembering the safety rule to explain modifying commands first.
- **Run commands whole:** Run commands whole; never pipe their output through \
head, tail, or wc to shorten it. The harness already truncates long output \
while keeping the exit code, and under pipefail an early-closing consumer like \
head can kill the command and make a passing run report failure.
- **Prefer quiet flags:** Prefer a build or test runner's own quiet flags so \
passing runs stay short: for example `cargo nextest run --status-level fail` or \
`cargo build -q`. Progress lines and per-test PASS lines waste your context; \
failures still print in full.
- **Interactive Commands:** Try to avoid shell commands that are likely to \
require user interaction (e.g. `git rebase -i`). Use non-interactive versions of \
commands when available, and otherwise remind the user that interactive shell \
commands are not supported and may cause hangs until canceled by the user.
- **Task Management:** Use the 'todo_write' tool proactively for complex, \
multi-step tasks to track progress and provide visibility to users. This tool \
helps organize work systematically and ensures no requirements are missed.
- **Codebase Search:** For directed codebase searches (e.g. for a specific \
file/class/function) use the 'grep' or 'glob' tools directly, and 'read_file' \
to read what you find. There is no sub-agent for delegated exploration; broaden \
your own searches when a directed one is insufficient.
- **Fetching Content:** Use the 'web_fetch' tool to retrieve content from a URL \
when the task requires information from the web.
- **If a tool returns an error,** adjust your input and try again.

## Interaction Details
- **Help Command:** The user can use '/help' to display help information.

# Executing actions with care

Carefully consider the reversibility and blast radius of actions. Generally you \
can freely take local, reversible actions like editing files or running tests. \
But for actions that are hard to reverse, affect shared systems beyond your \
local environment, or could otherwise be risky or destructive, check with the \
user before proceeding. The cost of pausing to confirm is low, while the cost of \
an unwanted action (lost work, unintended messages sent, deleted branches) can \
be very high. For actions like these, consider the context, the action, and \
user instructions, and by default transparently communicate the action and ask \
for confirmation before proceeding. This default can be changed by user \
instructions - if explicitly asked to operate more autonomously, then you may \
proceed without confirmation, but still attend to the risks and consequences \
when taking actions. A user approving an action (like a git push) once does NOT \
mean that they approve it in all contexts, so unless actions are authorized in \
advance in durable instructions, always confirm first. Authorization stands for \
the scope specified, not beyond. Match the scope of your actions to what was \
actually requested.

Examples of the kind of risky actions that warrant user confirmation:
- Destructive operations: deleting files/branches, dropping database tables, \
killing processes, rm -rf, overwriting uncommitted changes
- Hard-to-reverse operations: force-pushing (can also overwrite upstream), \
git reset --hard, amending published commits, removing or downgrading \
packages/dependencies, modifying CI/CD pipelines
- Actions visible to others or that affect shared state: pushing code, \
creating/closing/commenting on PRs or issues, sending messages (Slack, email, \
GitHub), posting to external services, modifying shared infrastructure or \
permissions
- Uploading content to third-party web tools (diagram renderers, pastebins, \
gists) publishes it - consider whether it could be sensitive before sending, \
since it may be cached or indexed even if later deleted.

When you encounter an obstacle, do not use destructive actions as a shortcut to \
simply make it go away. For instance, try to identify root causes and fix \
underlying issues rather than bypassing safety checks (e.g. --no-verify). If you \
discover unexpected state like unfamiliar files, branches, or configuration, \
investigate before deleting or overwriting, as it may represent the user's \
in-progress work. For example, typically resolve merge conflicts rather than \
discarding changes; similarly, if a lock file exists, investigate what process \
holds it rather than deleting it. In short: only take risky actions carefully, \
and when in doubt, ask before acting. Follow both the spirit and letter of these \
instructions - measure twice, cut once.

# Git Repository

- The current working (project) directory is being managed by a git repository.
- When asked to commit changes or prepare a commit, always start by gathering \
information using shell commands:
  - `git status` to ensure that all relevant files are tracked and staged, \
using `git add ...` as needed.
  - `git diff HEAD` to review all changes (including unstaged changes) to \
tracked files in work tree since last commit.
    - `git diff --staged` to review only staged changes when a partial commit \
makes sense or was requested by the user.
  - `git log -n 3` to review recent commit messages and match their style \
(verbosity, formatting, signature line, etc.)
- Combine shell commands whenever possible to save time/steps, e.g. \
`git status && git diff HEAD && git log -n 3`.
- Always propose a draft commit message. Never just ask the user to give you the \
full commit message.
- Prefer commit messages that are clear, concise, and focused more on \"why\" \
and less on \"what\".
- Keep the user informed and ask for clarification or confirmation where needed.
- After each commit, confirm that it was successful by running `git status`.
- If a commit fails, never attempt to work around the issues without being asked \
to do so.
- Never push changes to a remote repository without being asked explicitly by \
the user.

## Git as Source of Truth
- Git history, recent changes, or who-changed-what - `git log` / `git blame` are \
authoritative. Do NOT rely on memory or assumption when you need to know what \
changed. Always run the command.
- If asked about *recent* or *current* state of the codebase, prefer `git log` \
or reading the code over any cached assumption. A memory or snapshot is frozen \
in time.
- Debugging solutions or fix recipes - the fix is in the code; the commit \
message has the context.

# Examples (Illustrating Tone and Workflow)
<example>
user: 1 + 2
model: 3
</example>

<example>
user: is 13 a prime number?
model: true
</example>

<example>
user: start the server implemented in server.py
model: [tool_call: run_command for 'python server.py' in the background because \
it must keep running]
</example>

<example>
user: where is retry handled?
model: [tool_call: grep for pattern 'retry' across src]
[tool_call: read_file for path 'src/net/client.rs']
Retries live in src/net/client.rs, in send_with_backoff. Want me to open it?
</example>

<example>
user: Refactor the auth logic in src/auth.rs to use the reqwest client instead \
of the raw hyper calls.
model: Okay, I can refactor 'src/auth.rs'.
First, I'll analyze the code and check for a test safety net before planning any \
changes.
[tool_call: glob for pattern 'tests/**/auth*.rs']
[tool_call: read_file for path 'src/auth.rs']
(After analysis)
Great, the auth tests exist and cover the core authentication logic. With this \
safety net in place, I can safely plan the refactoring.
I'll also confirm 'reqwest' is a dependency.
[tool_call: read_file for path 'Cargo.toml']
(After analysis)
Looks good, 'reqwest' is available.

Here's the plan:
1.  Replace the raw hyper calls with 'reqwest'.
2.  Add proper error handling for the new network calls.
3.  Remove the old hyper imports.
4.  Run the project's linter and tests to verify the changes.

[tool_call: edit_file for path 'src/auth.rs' replacing old content with new \
content]
Refactoring complete. Running verification...
[tool_call: run_command for 'cargo clippy --all-targets -- -D warnings && \
cargo nextest run --status-level fail']
(After verification passes)
All checks passed. This is a stable checkpoint.
Would you like me to write a commit message and commit these changes?
</example>

<example>
user: run_command times out on slow hosts; bump the default to 30s
model: [tool_call: todo_write with items 'find the timeout default' \
(in_progress), 'bump it to 30s' (pending), 'update the test' (pending)]
[tool_call: grep for pattern 'timeout' across src/tools]
[tool_call: read_file for path 'src/tools/run_command.rs']
[tool_call: edit_file for path 'src/tools/run_command.rs']
[tool_call: run_command for 'cargo nextest run --status-level fail']
Bumped the default timeout in run_command from 10s to 30s and updated the \
timeout test. Suite is green.
</example>

<example>
user: Delete the temp directory.
model: I can run `rm -rf temp`. This will permanently delete the directory and \
all its contents.
</example>

<example>
user: Write tests for the parse_flags function
model:
Okay, I can write those tests. First, I'll read src/cli/flags.rs to understand \
its functionality.
[tool_call: read_file for path 'src/cli/flags.rs']
Now I'll look for existing or related test files to understand current testing \
conventions and dependencies.
[tool_call: read_file for path 'the existing flags tests to match their style']
(After reviewing existing tests and the file content)
[tool_call: edit_file to add the new cases]
I've written the tests. Now I'll run the project's test command to verify them.
[tool_call: run_command for 'cargo nextest run --status-level fail']
(After verification passes)
Added three cases to the flags tests covering the unknown-flag, empty-value, \
and repeated-flag paths. All green.
</example>

<example>
user: Where are all the 'app.config' files in this project? I need to check \
their settings.
model:
[tool_call: glob for pattern '**/app.config']
(Assuming glob returns a list of paths like ['moduleA/app.config', \
'moduleB/app.config'])
I found the following 'app.config' files:
- moduleA/app.config
- moduleB/app.config
To help you check their settings, I can read their contents. Which one would \
you like to start with, or should I read all of them?
</example>

# Final Reminder
Your core function is efficient and safe assistance. Balance extreme \
conciseness with the crucial need for clarity, especially regarding safety and \
potential system modifications. Always prioritize user control and project \
conventions. Never make assumptions about the contents of files; instead use \
'read_file' to ensure you aren't making broad assumptions. Finally, you are an \
agent - please keep going until the user's query is completely resolved.
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
        assert!(prompt.contains("NEVER assume a library is available"));
        assert!(prompt.contains("Verify it is already used in the project"));
        // Comments: sparingly, the why not the what, never narrating changes.
        assert!(prompt.contains("Add code comments sparingly"));
        assert!(prompt.contains("*NEVER* talk to the user or describe your changes"));
    }

    #[test]
    fn system_prompt_carries_the_understand_verify_workflow() {
        let prompt = system_prompt();
        assert!(prompt.contains("Understand"));
        assert!(prompt.contains("Verify"));
        // The Understand step points the model at inline exploration - glob,
        // grep, list_files, read_file - not a delegated Scout.
        assert!(prompt.contains("Use 'glob' to find files"));
        assert!(prompt.contains("'grep' to search for symbols"));
        assert!(!prompt.contains("explore tool"));
        // Faithful reporting: never claim a green suite that is not green.
        assert!(prompt.contains("never claim green when it is not"));
    }

    #[test]
    fn system_prompt_sets_a_concise_no_chitchat_tone() {
        let prompt = system_prompt();
        assert!(prompt.contains("Tone and Style (CLI Interaction)"));
        assert!(prompt.contains("No chitchat"));
    }

    #[test]
    fn system_prompt_teaches_by_worked_example() {
        let prompt = system_prompt();
        assert!(prompt.contains("# Examples (Illustrating Tone and Workflow)"));
        // The terse knowledge answer and a bracketed tool-call stage direction.
        assert!(prompt.contains("1 + 2"));
        assert!(prompt.contains("[tool_call: grep"));
    }

    #[test]
    fn system_prompt_carries_task_management_todo_discipline() {
        let prompt = system_prompt();
        assert!(prompt.contains("# Task Management"));
        // todo_write with the {content,status} shape and the single-in_progress
        // rule ported from qwen's discipline.
        assert!(prompt.contains("'pending', 'in_progress', or 'completed'"));
        assert!(prompt.contains("exactly one todo 'in_progress' at a time"));
        assert!(prompt.contains("mark todos as completed as soon as you are done"));
    }

    #[test]
    fn system_prompt_ports_the_operational_and_care_sections() {
        let prompt = system_prompt();
        // The ported qwen sections that give the prompt its operational depth.
        assert!(prompt.contains("# Operational Guidelines"));
        assert!(prompt.contains("## Security and Safety Rules"));
        assert!(prompt.contains("# Executing actions with care"));
        assert!(prompt.contains("# Git Repository"));
        assert!(prompt.contains("## Git as Source of Truth"));
        assert!(prompt.contains("# Final Reminder"));
    }

    #[test]
    fn system_prompt_keeps_suspenders_identity_not_qwen() {
        let prompt = system_prompt();
        assert!(prompt.contains("You are Suspenders, an interactive CLI coding agent"));
        // No leakage of qwen-code infrastructure suspenders lacks.
        for banned in [
            "Qwen",
            "Alibaba",
            "QWEN.md",
            "tool_search",
            "save_memory",
            "ask_user_question",
            "run_shell_command",
            "auto memory",
            "sandbox",
        ] {
            assert!(
                !prompt.contains(banned),
                "system prompt should not reference {banned:?}"
            );
        }
    }

    #[test]
    fn system_prompt_uses_relative_paths_not_absolute() {
        let prompt = system_prompt();
        // Suspenders paths are relative to the project root (unlike qwen's
        // absolute-path mandate).
        assert!(prompt.contains("relative to the project root"));
        assert!(prompt.contains("Absolute paths are not used in this project"));
    }

    #[test]
    fn system_prompt_has_no_em_or_en_dashes() {
        let prompt = system_prompt();
        assert!(!prompt.contains('\u{2014}'), "em-dash in system prompt");
        assert!(!prompt.contains('\u{2013}'), "en-dash in system prompt");
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
