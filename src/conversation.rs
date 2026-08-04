//! Pure functional core holding the Conversation: the ordered message history
//! sent to the model, plus the Context Budget bookkeeping. No processes, no IO,
//! no config reads - every option is passed explicitly by the composition root
//! (from the Session).
//!
//! ## Context budget
//!
//! [`Conversation::for_request`] is a pure fit-check: it returns the wire-ready
//! request when the char estimate fits under `context_budget -
//! max_tokens_reserve`, or `Err(ContextBudgetExhausted)` when it does not.
//! Reclaiming context is Compaction's job alone (the request path recovers by
//! summarizing on `Err`); there is no bespoke mechanical Eviction.

mod run_boundary;

use crate::content::{ContentBlock, Message, Provenance, Role, Usage};
use crate::voice::{self, FileOps};

/// The Conversation and its Context Budget bookkeeping.
#[derive(Debug, Clone, PartialEq)]
pub struct Conversation {
    pub system_prompt: String,
    pub messages: Vec<Message>,
    pub context_budget: u64,
    pub max_tokens_reserve: u64,
    pub last_usage: Option<Usage>,
    pub overhead_chars: u64,
    pub compaction_slack: f64,
    pub compaction_keep: f64,
}

/// The explicit options a Conversation is built from. `context_budget` and
/// `max_tokens_reserve` are required (baud raised `KeyError` when they were
/// absent); [`ConversationOpts::new`] takes them and [`Default`] fills the rest
/// with baud's defaults, so a caller overriding a tail knob writes
/// `ConversationOpts { compaction_slack: 0.3, ..ConversationOpts::new(b, r) }`.
#[derive(Debug, Clone)]
pub struct ConversationOpts {
    pub context_budget: u64,
    pub max_tokens_reserve: u64,
    pub overhead_chars: u64,
    pub compaction_slack: f64,
    pub compaction_keep: f64,
}

/// Clamp floor for float results that should never go below zero.
const FLOAT_CLAMP_ZERO: f64 = 0.0;
/// Default compaction slack fraction: no compaction headroom below the budget
/// by default.
const DEFAULT_COMPACTION_SLACK: f64 = 0.0;
/// Default compaction keep fraction: keep the newest 50% of the live window.
const DEFAULT_COMPACTION_KEEP: f64 = 0.5;

impl Default for ConversationOpts {
    /// baud's defaults, with a zero budget/reserve placeholder: prefer
    /// [`ConversationOpts::new`], which names the two required knobs.
    fn default() -> Self {
        ConversationOpts {
            context_budget: 0,
            max_tokens_reserve: 0,
            overhead_chars: 0,
            compaction_slack: DEFAULT_COMPACTION_SLACK,
            compaction_keep: DEFAULT_COMPACTION_KEEP,
        }
    }
}

impl ConversationOpts {
    /// The two required knobs, with baud's defaults for the rest
    /// (`overhead_chars: 0`, `compaction_slack: 0.0`, `compaction_keep: 0.5`).
    /// Override a tail knob with struct-update syntax over the result.
    pub fn new(context_budget: u64, max_tokens_reserve: u64) -> Self {
        ConversationOpts {
            context_budget,
            max_tokens_reserve,
            ..ConversationOpts::default()
        }
    }
}

impl Conversation {
    /// Builds a new Conversation from a system prompt and explicit options.
    pub fn new(system_prompt: impl Into<String>, opts: ConversationOpts) -> Self {
        Conversation {
            system_prompt: system_prompt.into(),
            messages: Vec::new(),
            context_budget: opts.context_budget,
            max_tokens_reserve: opts.max_tokens_reserve,
            last_usage: None,
            overhead_chars: opts.overhead_chars,
            compaction_slack: opts.compaction_slack,
            compaction_keep: opts.compaction_keep,
        }
    }

    /// Appends the user's prompt as a user message with a single text block.
    pub fn add_user_text(&mut self, text: impl Into<String>) -> &mut Self {
        self.messages
            .push(Message::user(vec![ContentBlock::text(text)]));
        self
    }

    /// Appends the user's prompt as a user message over its content-block list
    /// (ADR-0068): the media-capable sibling of [`Conversation::add_user_text`],
    /// carrying the Text/Image/Document blocks a submit or Steer brought from the
    /// Composer. A pure-text prompt is exactly `add_user_text`.
    pub fn add_user_content(&mut self, blocks: Vec<ContentBlock>) -> &mut Self {
        self.messages.push(Message::user(blocks));
        self
    }

    /// Appends Voice-authored blocks (closing markers) as one assistant
    /// message, unknown Provenance. Response content goes through
    /// [`Conversation::add_assistant_response`] instead.
    pub fn add_assistant_blocks(&mut self, blocks: Vec<ContentBlock>) -> &mut Self {
        self.messages.push(Message::assistant(blocks));
        self
    }

    /// Appends a Response's content blocks as one assistant message stamped
    /// with the Provenance of the Model that produced them (CONTEXT.md:
    /// Provenance, ADR-0037).
    pub fn add_assistant_response(
        &mut self,
        blocks: Vec<ContentBlock>,
        provenance: Provenance,
    ) -> &mut Self {
        self.messages
            .push(Message::assistant_from(blocks, provenance));
        self
    }

    /// Appends all Tool Results for a Pass as ONE user message. Delivered
    /// Steering rides the same message as trailing text blocks, unadorned.
    pub fn add_tool_results(
        &mut self,
        results: Vec<ContentBlock>,
        steering_texts: Vec<String>,
    ) -> &mut Self {
        let mut content = results;
        content.extend(steering_texts.into_iter().map(ContentBlock::text));
        self.messages.push(Message::user(content));
        self
    }

    /// Appends user-voiced text without breaking role alternation: a trailing
    /// user-role message gains a text block; otherwise a new user message is
    /// appended.
    pub fn merge_user_text(&mut self, text: impl Into<String>) -> &mut Self {
        merge_user_text(&mut self.messages, text);
        self
    }

    /// The verbatim original task: the first user text of the Conversation.
    pub fn original_task(&self) -> Option<&str> {
        self.messages.iter().find_map(|m| {
            if m.role == Role::User {
                match m.content.first() {
                    Some(ContentBlock::Text { text }) => Some(text.as_str()),
                    _ => None,
                }
            } else {
                None
            }
        })
    }

    /// Stores the usage from the latest API response.
    pub fn note_usage(&mut self, usage: Usage) -> &mut Self {
        self.last_usage = Some(usage);
        self
    }

    /// Estimates the Conversation's token count: `ceil(total chars / 3.5)` over
    /// overhead, system prompt, and content, floored by `last_usage`'s
    /// context floor when present.
    pub fn token_estimate(&self) -> u64 {
        let estimate = self.char_estimate();
        match self.usage_floor() {
            Some(tokens) => estimate.max(tokens),
            None => estimate,
        }
    }

    /// The Compaction Target: the low-water mark shared by Compaction's
    /// trigger and the Session's validation. `compaction_slack` lowers it,
    /// carving compaction headroom below the Context Budget.
    pub fn compaction_target(&self) -> u64 {
        compaction_target(
            self.context_budget,
            self.max_tokens_reserve,
            self.compaction_slack,
        )
    }

    /// The Compaction Keep amount: the token-space figure of recent
    /// Conversation that survives a Compaction verbatim.
    pub fn compaction_keep_amount(&self) -> u64 {
        compaction_keep_amount(
            self.context_budget,
            self.max_tokens_reserve,
            self.compaction_keep,
        )
    }

    /// Returns wire-ready request data, or `Err(ContextBudgetExhausted)` when
    /// the char estimate does not fit under `context_budget -
    /// max_tokens_reserve`. A pure fit-check: reclaiming context is
    /// Compaction's job (the request path recovers by summarizing on `Err`).
    /// The fit check uses the char estimate, not `token_estimate` - the same
    /// final-fit threshold the retired Eviction path used.
    pub fn for_request(&self) -> Result<Request, ContextBudgetExhausted> {
        let target = self.context_budget.saturating_sub(self.max_tokens_reserve);
        if self.char_estimate() <= target {
            Ok(Request {
                system: self.system_prompt.clone(),
                messages: self.messages.clone(),
            })
        } else {
            Err(ContextBudgetExhausted)
        }
    }

    /// Prepares compaction: which messages to summarize and where the cutoff
    /// to kept messages falls. Walks backwards from the newest message,
    /// accumulating the char estimate; stops at the Compaction Keep, then
    /// adjusts the cutoff backward to the nearest run-start user message.
    pub fn prepare_compaction(&self) -> Option<(Vec<Message>, usize, FileOps)> {
        let keep_recent = self.compaction_keep_amount();

        // Indexes of run-start user messages (run_boundary owns the rule).
        let run_start_indexes: Vec<usize> =
            run_boundary::run_start_indices(&self.messages).collect();

        // Nearest run-start at or before the computed cutoff.
        let cutoff = keep_cutoff(&self.messages, keep_recent).and_then(|computed_cutoff| {
            run_start_indexes
                .iter()
                .filter(|&&i| i <= computed_cutoff)
                .max()
                .copied()
        });

        match cutoff {
            None => None,
            Some(0) => None,
            Some(idx) => {
                let to_summarize: Vec<Message> = self.messages[..idx].to_vec();
                let file_ops = extract_file_ops(&to_summarize);
                Some((to_summarize, idx, file_ops))
            }
        }
    }

    /// Applies compaction: replaces messages before `cutoff_index` with a
    /// single user-role summary message carrying the summary text.
    pub fn apply_compaction(&self, summary_text: &str, cutoff_index: usize) -> Conversation {
        let summary_msg = Message::user(vec![voice::summary_block(summary_text)]);
        let mut messages = vec![summary_msg];
        messages.extend_from_slice(&self.messages[cutoff_index..]);
        let mut conv = self.clone();
        conv.messages = messages;
        conv
    }

    // Char estimate: overhead + system prompt + all message content, as
    // ceil(chars / 3.5) integer-only (div(2*chars + 6, 7)). The request fit
    // check is judged by this alone (no last_usage floor).
    fn char_estimate(&self) -> u64 {
        let chars: u64 = self.overhead_chars
            + self.system_prompt.chars().count() as u64
            + self
                .messages
                .iter()
                .map(|m| message_chars(m) as u64)
                .sum::<u64>();
        tokens_for_chars(chars)
    }

    fn usage_floor(&self) -> Option<u64> {
        self.last_usage.as_ref().and_then(|u| u.context_floor())
    }
}

/// Wire-ready request data returned by [`Conversation::for_request`].
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub system: String,
    pub messages: Vec<Message>,
}

/// The error `for_request` returns when the request does not fit under the
/// Context Budget: an over-budget request is never sent (Compaction recovers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudgetExhausted;

/// [`Conversation::merge_user_text`] over bare messages: a trailing user-role
/// message gains a text block; otherwise a fresh user message is appended.
/// The one seam every tail rider crosses - Steering and any Voice-authored
/// marker that rides the tail - shared with Resume's fold so a logged rider
/// replays through the same code that placed it live.
pub fn merge_user_text(messages: &mut Vec<Message>, text: impl Into<String>) {
    match messages.last_mut() {
        Some(last) if last.role == Role::User => {
            last.content.push(ContentBlock::text(text));
        }
        _ => messages.push(Message::user(vec![ContentBlock::text(text)])),
    }
}

/// `compaction_target` over plain numbers, for callers that hold the Session
/// facts but no Conversation:
/// `max(context_budget - max_tokens_reserve - trunc(compaction_slack * context_budget), 0)`.
pub fn compaction_target(
    context_budget: u64,
    max_tokens_reserve: u64,
    compaction_slack: f64,
) -> u64 {
    let target = context_budget.saturating_sub(max_tokens_reserve);
    let slack = (context_budget as f64 * compaction_slack).trunc() as u64;
    target.saturating_sub(slack)
}

/// The Compaction Keep amount (CONTEXT.md: Compaction Keep) over plain
/// numbers, for callers that hold the Session facts but no Conversation:
/// `max(trunc(compaction_keep * (context_budget - max_tokens_reserve)), 0)`,
/// the token-space figure of recent Conversation that survives a Compaction
/// verbatim. Fire high, keep low: validation requires it below the compaction
/// trigger ([`compaction_target`]). The cutoff walk currently measures
/// progress toward it in raw chars (see the flagged ambiguity in CONTEXT.md).
pub fn compaction_keep_amount(
    context_budget: u64,
    max_tokens_reserve: u64,
    compaction_keep: f64,
) -> u64 {
    let live_window = context_budget.saturating_sub(max_tokens_reserve) as f64;
    // Clamp to zero: `saturating_sub` already ensures non-negative, but the
    // float multiply can produce a tiny negative value near zero.
    (compaction_keep * live_window)
        .trunc()
        .max(FLOAT_CLAMP_ZERO) as u64
}

/// Extracts file operations from a list of messages. Scans tool_use blocks for
/// read_file/list_files (reads) and write_file/edit_file (modifies).
pub fn extract_file_ops(messages: &[Message]) -> FileOps {
    use std::collections::BTreeSet;
    let mut reads: BTreeSet<String> = BTreeSet::new();
    let mut modifies: BTreeSet<String> = BTreeSet::new();

    for msg in messages {
        for block in &msg.content {
            if let ContentBlock::ToolUse { name, input, .. } = block {
                // read_file/write_file/edit name their path arg "file_path";
                // list_directory still uses "path".
                let key = match name.as_str() {
                    "read_file" | "write_file" | "edit" => "file_path",
                    "list_directory" => "path",
                    _ => continue,
                };
                if let Some(path) = input.get(key).and_then(|p| p.as_str()) {
                    match name.as_str() {
                        "read_file" | "list_directory" => {
                            reads.insert(path.to_string());
                        }
                        "write_file" | "edit" => {
                            modifies.insert(path.to_string());
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    FileOps {
        read_files: reads.into_iter().collect(),
        modified_files: modifies.into_iter().collect(),
    }
}

// ceil(chars / 3.5): the numerator bias and denominator derive from the 3.5
// chars-per-token ratio encoded as integer arithmetic (2*chars + 6) / 7.
// These constants document that encoding; changing them changes the estimate.
const CHARS_PER_TOKEN_NUMER: u64 = 2; // 1 / 3.5 = 2 / 7
const CHARS_PER_TOKEN_BIAS: u64 = 6; // bias for ceil: 7 - 1
const CHARS_PER_TOKEN_DENOM: u64 = 7;

fn tokens_for_chars(chars: u64) -> u64 {
    #[allow(clippy::manual_div_ceil)]
    {
        (CHARS_PER_TOKEN_NUMER * chars + CHARS_PER_TOKEN_BIAS) / CHARS_PER_TOKEN_DENOM
    }
}

// The Compaction Keep walk: newest-first, accumulating raw `message_chars`
// (not tokens - the flagged ambiguity in CONTEXT.md). Returns the index of the
// message at which the running total first reaches `keep_recent` (that message
// lands on the kept side), or `None` when the whole history stays within it.
fn keep_cutoff(messages: &[Message], keep_recent: u64) -> Option<usize> {
    let mut acc: u64 = 0;
    for (idx, msg) in messages.iter().enumerate().rev() {
        acc += message_chars(msg) as u64;
        if acc >= keep_recent {
            return Some(idx);
        }
    }
    None
}

fn message_chars(msg: &Message) -> usize {
    msg.content.iter().map(block_chars).sum()
}

fn block_chars(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text } => text.chars().count(),
        ContentBlock::ToolResult { content, .. } => content.iter().map(result_block_chars).sum(),
        ContentBlock::ToolUse { name, input, .. } => {
            name.chars().count()
                + serde_json::to_string(input)
                    .unwrap_or_default()
                    .chars()
                    .count()
        }
        ContentBlock::Thinking { .. } => 0,
        // First-class user media (ADR-0068): the real wire payload is the
        // multi-MB base64 `data`, so count its length - counting the short
        // `[image: mime]` projection would wildly under-estimate context, the
        // same reasoning [`result_block_chars`] applies to Tool Result media.
        ContentBlock::Image { data, .. } | ContentBlock::Document { data, .. } => {
            data.chars().count()
        }
    }
}

/// The char cost of one Tool Result block for the context/compaction estimate.
/// A Text block counts its own chars; a media block counts its base64 `data`
/// length - the real wire payload is multi-MB base64, so counting the short
/// `[image: mime]` text projection would wildly under-estimate context once
/// media flows (latent today: the Catalog's modalities are all-false, so media
/// degrades to text before the wire, but the estimate must be right by shape).
fn result_block_chars(block: &crate::content::ResultBlock) -> usize {
    use crate::content::ResultBlock;
    match block {
        ResultBlock::Text { text } => text.chars().count(),
        ResultBlock::Image { data, .. } | ResultBlock::Document { data, .. } => data.len(),
    }
}

#[cfg(test)]
#[path = "../tests/conversation.rs"]
mod tests;
