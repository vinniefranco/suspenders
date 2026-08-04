//! The framing around a Compaction summary (CONTEXT.md: Voice, Compaction): the
//! compression system prompt (a verbatim port of qwen v0.21.4's
//! `getCompressionPrompt()`), the mechanical facts appended after the LLM
//! summary, the summary content block itself (carrying qwen's post-compact
//! resume trailer), and the flat serialization fed to the compaction call.

use crate::content::{ContentBlock, Message, Role};

/// The compression system prompt, VERBATIM from qwen v0.21.4's
/// `getCompressionPrompt()`. The model wraps its chain-of-thought in an
/// `<analysis>` block (stripped by [`crate::compaction`] before the summary
/// enters history) and then emits a `<state_snapshot>` XML envelope with nine
/// sub-sections aligned to claude-code's compaction format.
const COMPACTION_PROMPT: &str = r#"You are the component that summarizes a conversation when its context window is about to overflow. The summary you produce will become the agent's ONLY memory of everything that happened before this point. The agent will resume its work based solely on this summary plus a small number of restored file / image attachments that follow.

First, wrap your reasoning in an <analysis> block. Inside it, walk through the conversation chronologically and identify, for each section: the user's explicit requests and intent, your approach to those requests, key decisions / technical concepts / code patterns, specific details (file names, code snippets, function signatures, file edits), errors and how they were fixed, and any specific user feedback — especially when the user told you to do something differently. The <analysis> block is stripped before the summary reaches the next agent; it is purely a drafting scratchpad to improve the summary that follows.

Then produce the final summary as the EXACT XML structure below. Be dense. Omit conversational filler.

<state_snapshot>
    <primary_request_and_intent>
        <!-- Capture all of the user's explicit requests and intents in detail. Quote the user's exact phrasing where intent is at stake. -->
    </primary_request_and_intent>

    <key_technical_concepts>
        <!-- List all important technical concepts, technologies, and frameworks discussed. -->
    </key_technical_concepts>

    <files_and_code_sections>
        <!-- Enumerate specific files and code sections examined, modified, or created. Pay special attention to the most recent messages. Include full code snippets where applicable, and a summary of why this file read or edit is important. -->
    </files_and_code_sections>

    <errors_and_fixes>
        <!-- List every error encountered and how it was fixed. Include the verbatim error message when it was quoted to the agent. Pay special attention to specific user feedback on the error, especially if the user told you to do something differently. -->
    </errors_and_fixes>

    <problem_solving>
        <!-- Document problems solved and any ongoing troubleshooting efforts. -->
    </problem_solving>

    <all_user_messages>
        <!-- List ALL user messages that are not tool results, in chronological order. These are critical for understanding the user's feedback and shifting intent. Include short messages like "ok" or "continue" — they are signal. -->
    </all_user_messages>

    <pending_tasks>
        <!-- Outline any pending tasks that the user has explicitly asked the agent to work on but that are not yet complete. -->
    </pending_tasks>

    <current_work>
        <!-- Describe in detail precisely what the agent was working on immediately before this summary was requested, paying special attention to the most recent messages from both user and assistant. Include file names and code snippets where applicable. -->
    </current_work>

    <next_step>
        <!-- List the single next step the agent will take, related to the most recent work. The step MUST be DIRECTLY in line with the user's most recent explicit request and the task the agent was working on immediately before this summary. If the last task was concluded, list a next step only if it is explicitly in line with the user's request — do NOT start tangential or older work without confirming with the user first. If there is a next step, include direct quotes from the most recent conversation showing exactly what task you were working on and where you left off. -->
    </next_step>
</state_snapshot>"#;

/// The compression system prompt sent to the LLM for compaction (summarizing
/// old messages), a verbatim port of qwen v0.21.4's `getCompressionPrompt()`.
pub fn compaction_prompt() -> &'static str {
    COMPACTION_PROMPT
}

/// The trailer appended to the post-compact summary message, VERBATIM from qwen
/// v0.21.4's `RESUME_TRAILER` (`postCompactAttachments.ts`). It lives in the
/// re-injection wrapper (not the compression prompt) so the summary model does
/// not regenerate it every compaction.
const RESUME_TRAILER: &str = "Resume the prior task using the summary above. Continue from the last in-flight step; do not acknowledge the summary, do not re-introduce, do not greet the user again.";

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
/// the summary produced by the LLM, followed by qwen's [`RESUME_TRAILER`] so the
/// resuming agent continues from the last in-flight step without re-greeting.
pub fn summary_block(summary: &str) -> ContentBlock {
    ContentBlock::Text {
        text: format!("{summary}\n\n{RESUME_TRAILER}"),
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
                    let text = crate::content::result_blocks_text(content);
                    Some(format!("{label}: {}", truncate_for_serialization(&text)))
                }
                // First-class user media (ADR-0068): summarize its short
                // `[image: mime]`/`[document: mime]` placeholder, not the
                // multi-MB base64 - the summary reasons over what was attached.
                other => other
                    .media_placeholder()
                    .map(|placeholder| format!("User: {placeholder}")),
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
