//! The Suspenders-authored framing around a Compaction summary (CONTEXT.md:
//! Voice, Compaction): the mechanical facts appended after the LLM summary, the
//! summary content block itself, and the flat serialization fed to the
//! compaction call. Wording Suspenders owns, kept beside the compaction prompt.

use crate::content::{ContentBlock, Message, Role};

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
                    let text = crate::content::result_blocks_text(content);
                    Some(format!("{label}: {}", truncate_for_serialization(&text)))
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
