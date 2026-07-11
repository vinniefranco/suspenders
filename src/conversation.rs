//! Pure functional core holding the Conversation: the ordered message history
//! sent to the model, plus the Context Budget bookkeeping for Eviction. No
//! processes, no IO, no config reads - every option is passed explicitly by
//! the composition root (from the Session).
//!
//! ## Eviction
//!
//! When the estimate goes over `context_budget - max_tokens_reserve`,
//! [`Conversation::evict`] replaces the contents of old Tool Results, oldest
//! first, with the elision marker. The last two tool-result-bearing user
//! messages, the system prompt, and everything that is not a `tool_result` are
//! never touched. Running it twice changes nothing. Eviction also reclaims
//! stale Anchors (never the most recent) and overshoots to the low-water mark
//! (hysteresis, ADR-0006).

use crate::content::{ContentBlock, Message, Role};
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
    pub eviction_slack: f64,
    pub compaction_keep: f64,
}

/// A usage signal from the API. Only `input_tokens` is load-bearing for the
/// token estimate floor; the rest is carried opaquely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
}

impl Usage {
    pub fn with_input_tokens(input_tokens: u64) -> Self {
        Usage {
            input_tokens: Some(input_tokens),
        }
    }

    pub fn empty() -> Self {
        Usage { input_tokens: None }
    }
}

/// The explicit options a Conversation is built from. `context_budget` and
/// `max_tokens_reserve` are required (baud raised `KeyError` when they were
/// absent; here the type system requires them). The rest carry baud's
/// defaults via [`Default`].
#[derive(Debug, Clone)]
pub struct ConversationOpts {
    pub context_budget: u64,
    pub max_tokens_reserve: u64,
    pub overhead_chars: u64,
    pub eviction_slack: f64,
    pub compaction_keep: f64,
}

impl ConversationOpts {
    /// The two required knobs, with baud's defaults for the rest
    /// (`overhead_chars: 0`, `eviction_slack: 0.0`, `compaction_keep: 0.5`).
    pub fn new(context_budget: u64, max_tokens_reserve: u64) -> Self {
        ConversationOpts {
            context_budget,
            max_tokens_reserve,
            overhead_chars: 0,
            eviction_slack: 0.0,
            compaction_keep: 0.5,
        }
    }

    pub fn overhead_chars(mut self, v: u64) -> Self {
        self.overhead_chars = v;
        self
    }

    pub fn eviction_slack(mut self, v: f64) -> Self {
        self.eviction_slack = v;
        self
    }

    pub fn compaction_keep(mut self, v: f64) -> Self {
        self.compaction_keep = v;
        self
    }
}

/// File operations extracted from a set of messages.
pub type ConversationFileOps = FileOps;

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
            eviction_slack: opts.eviction_slack,
            compaction_keep: opts.compaction_keep,
        }
    }

    /// Appends the user's prompt as a user message with a single text block.
    pub fn add_user_text(&mut self, text: impl Into<String>) -> &mut Self {
        self.messages
            .push(Message::user(vec![ContentBlock::text(text)]));
        self
    }

    /// Appends the model's content blocks as one assistant message.
    pub fn add_assistant_blocks(&mut self, blocks: Vec<ContentBlock>) -> &mut Self {
        self.messages.push(Message::assistant(blocks));
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
        match self.messages.last_mut() {
            Some(last) if last.role == Role::User => {
                last.content.push(ContentBlock::text(text));
            }
            _ => {
                self.add_user_text(text);
            }
        }
        self
    }

    /// Injects an Anchor near the tail; rides the same seam as Steering.
    pub fn inject_anchor(&mut self, anchor_text: impl Into<String>) -> &mut Self {
        self.merge_user_text(anchor_text)
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
    /// `input_tokens` when present.
    pub fn token_estimate(&self) -> u64 {
        let estimate = self.char_estimate();
        match self.input_tokens() {
            Some(tokens) => estimate.max(tokens),
            None => estimate,
        }
    }

    /// The Compaction Target: the low-water mark shared by Eviction's
    /// overshoot floor, Compaction's trigger, and the Session's validation.
    pub fn compaction_target(&self) -> u64 {
        compaction_target(
            self.context_budget,
            self.max_tokens_reserve,
            self.eviction_slack,
        )
    }

    /// Applies Eviction, then returns wire-ready request data, or
    /// `Err(ContextBudgetExhausted)` when Eviction could not bring the
    /// estimate under `context_budget - max_tokens_reserve`. The fit check
    /// uses the char estimate, not `token_estimate`.
    pub fn for_request(&self) -> Result<Request, ContextBudgetExhausted> {
        let evicted = self.evict();
        let target = evicted
            .context_budget
            .saturating_sub(evicted.max_tokens_reserve);
        if evicted.char_estimate() <= target {
            Ok(Request {
                system: evicted.system_prompt,
                messages: evicted.messages,
            })
        } else {
            Err(ContextBudgetExhausted)
        }
    }

    /// Evicts old Tool Results (and stale Anchors) once `token_estimate`
    /// exceeds `context_budget - max_tokens_reserve`, eliding down to the
    /// low-water mark. Oldest first; idempotent.
    pub fn evict(&self) -> Conversation {
        let target = self.context_budget.saturating_sub(self.max_tokens_reserve);
        if self.token_estimate() <= target {
            self.clone()
        } else {
            // Hysteresis: overshoot to the low-water mark (the Compaction
            // Target - one number, one definition).
            let mut conv = self.clone();
            let low_water = conv.compaction_target();
            conv.do_evict(low_water);
            conv
        }
    }

    /// Prepares compaction: which messages to summarize and where the cutoff
    /// to kept messages falls. Walks backwards from the newest message,
    /// accumulating the char estimate; stops at the Compaction Keep, then
    /// adjusts the cutoff backward to the nearest turn-start user message.
    pub fn prepare_compaction(&self) -> Option<(Vec<Message>, usize, FileOps)> {
        let live_window = self.context_budget.saturating_sub(self.max_tokens_reserve) as f64;
        let keep_recent = ((self.compaction_keep * live_window).trunc()).max(0.0) as i64;

        let msg_count = self.messages.len();

        // Indexes of turn-start user messages (role=user, first block text).
        let turn_start_indexes: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                m.role == Role::User
                    && matches!(m.content.first(), Some(ContentBlock::Text { .. }))
            })
            .map(|(idx, _)| idx)
            .collect();

        // Walk backwards from the newest message, accumulating char estimate.
        // reversed idx increments; stop when accumulated >= keep_recent.
        let mut reversed_cutoff: i64 = 0;
        let mut acc: i64 = 0;
        for msg in self.messages.iter().rev() {
            let new_acc = acc + message_chars(msg) as i64;
            if new_acc >= keep_recent {
                acc = new_acc;
                break;
            } else {
                reversed_cutoff += 1;
                acc = new_acc;
            }
        }
        let _ = acc;

        // Convert reversed index to real message index.
        let computed_cutoff = msg_count as i64 - 1 - reversed_cutoff;

        // Nearest turn-start at or before the computed cutoff.
        let cutoff = turn_start_indexes
            .iter()
            .filter(|&&i| i as i64 <= computed_cutoff)
            .max()
            .copied();

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
    // ceil(chars / 3.5) integer-only (div(2*chars + 6, 7)). Eviction progress
    // is judged by this alone (no last_usage floor).
    fn char_estimate(&self) -> u64 {
        let chars: u64 = self.overhead_chars
            + self.system_prompt.chars().count() as u64
            + self
                .messages
                .iter()
                .map(|m| message_chars(m) as u64)
                .sum::<u64>();
        // ceil(chars / 3.5) - a 3.5 ratio, not a div_ceil by 7, so keep as-is.
        #[allow(clippy::manual_div_ceil)]
        {
            (2 * chars + 6) / 7
        }
    }

    fn input_tokens(&self) -> Option<u64> {
        self.last_usage.as_ref().and_then(|u| u.input_tokens)
    }

    fn do_evict(&mut self, target: u64) {
        loop {
            match self.next_evictable() {
                None => break,
                Some((msg_index, block_index, marker)) => {
                    self.elide(msg_index, block_index, &marker);
                    if self.char_estimate() <= target {
                        break;
                    }
                }
            }
        }
    }

    // The oldest evictable target: a Tool Result outside the last two
    // tool-result-bearing user messages, or a superseded Anchor. Oldest
    // position wins (strictly oldest-first, ADR-0006 prefix stability).
    fn next_evictable(&self) -> Option<(usize, usize, String)> {
        let mut targets: Vec<(usize, usize, String)> = Vec::new();
        if let Some(t) = self.tool_result_target() {
            targets.push(t);
        }
        if let Some(t) = self.stale_anchor_target() {
            targets.push(t);
        }
        targets
            .into_iter()
            .min_by_key(|(msg_index, block_index, _)| (*msg_index, *block_index))
    }

    // Oldest non-elided tool_result outside the last two tool-result-bearing
    // user messages.
    fn tool_result_target(&self) -> Option<(usize, usize, String)> {
        let bearing_indexes: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == Role::User && m.content.iter().any(is_tool_result))
            .map(|(idx, _)| idx)
            .collect();

        // Never touch the last two tool-result-bearing user messages.
        let candidates = if bearing_indexes.len() > 2 {
            &bearing_indexes[..bearing_indexes.len() - 2]
        } else {
            &[][..]
        };

        for &msg_index in candidates {
            let blocks = &self.messages[msg_index].content;
            if let Some(block_index) = blocks.iter().position(is_evictable_tool_result) {
                return Some((msg_index, block_index, voice::elision_marker().to_string()));
            }
        }
        None
    }

    // Oldest live (not yet elided) Anchor block, provided a more recent Anchor
    // exists to protect. The most recent Anchor is never elided.
    fn stale_anchor_target(&self) -> Option<(usize, usize, String)> {
        // live_anchor_positions returns ALL live anchors; baud drops the last
        // (most recent) and takes the first of the rest.
        let live = self.live_anchor_positions();
        match live.len() {
            0 | 1 => None,
            _ => live.into_iter().next(),
        }
    }

    // Positions of every live (not-yet-elided) Anchor block, in Conversation
    // order, as (msg_index, block_index, anchor_elision_marker).
    fn live_anchor_positions(&self) -> Vec<(usize, usize, String)> {
        let mut all: Vec<(usize, usize, String)> = Vec::new();
        for (msg_index, message) in self.messages.iter().enumerate() {
            for (block_index, block) in message.content.iter().enumerate() {
                if voice::is_anchor(block)
                    && let ContentBlock::Text { text } = block
                    && text != voice::anchor_elision_marker()
                {
                    all.push((
                        msg_index,
                        block_index,
                        voice::anchor_elision_marker().to_string(),
                    ));
                }
            }
        }
        all
    }

    fn elide(&mut self, msg_index: usize, block_index: usize, marker: &str) {
        let block = &mut self.messages[msg_index].content[block_index];
        match block {
            ContentBlock::ToolResult { content, .. } => *content = marker.to_string(),
            ContentBlock::Text { text } => *text = marker.to_string(),
            _ => {}
        }
    }
}

/// Wire-ready request data returned by [`Conversation::for_request`].
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub system: String,
    pub messages: Vec<Message>,
}

/// The error `for_request` returns when Eviction ran dry and the request still
/// doesn't fit: an over-budget request is never sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudgetExhausted;

/// `compaction_target` over plain numbers, for callers that hold the Session
/// facts but no Conversation:
/// `max(context_budget - max_tokens_reserve - trunc(eviction_slack * context_budget), 0)`.
pub fn compaction_target(context_budget: u64, max_tokens_reserve: u64, eviction_slack: f64) -> u64 {
    let target = context_budget.saturating_sub(max_tokens_reserve);
    let slack = (context_budget as f64 * eviction_slack).trunc() as u64;
    target.saturating_sub(slack)
}

/// Extracts file operations from a list of messages. Scans tool_use blocks for
/// read_file/list_files (reads) and write_file/edit_file (modifies).
pub fn extract_file_ops(messages: &[Message]) -> FileOps {
    use std::collections::BTreeSet;
    let mut reads: BTreeSet<String> = BTreeSet::new();
    let mut modifies: BTreeSet<String> = BTreeSet::new();

    for msg in messages {
        for block in &msg.content {
            if let ContentBlock::ToolUse { name, input, .. } = block
                && let Some(path) = input.get("path").and_then(|p| p.as_str())
            {
                match name.as_str() {
                    "read_file" | "list_files" => {
                        reads.insert(path.to_string());
                    }
                    "write_file" | "edit_file" => {
                        modifies.insert(path.to_string());
                    }
                    _ => {}
                }
            }
        }
    }

    FileOps {
        read_files: reads.into_iter().collect(),
        modified_files: modifies.into_iter().collect(),
    }
}

fn is_tool_result(block: &ContentBlock) -> bool {
    matches!(block, ContentBlock::ToolResult { .. })
}

fn is_evictable_tool_result(block: &ContentBlock) -> bool {
    matches!(block, ContentBlock::ToolResult { content, .. } if content != voice::elision_marker())
}

fn message_chars(msg: &Message) -> usize {
    msg.content.iter().map(block_chars).sum()
}

fn block_chars(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text } => text.chars().count(),
        ContentBlock::ToolResult { content, .. } => content.chars().count(),
        ContentBlock::ToolUse { name, input, .. } => {
            name.chars().count() + serde_json::to_string(input).unwrap_or_default().chars().count()
        }
        ContentBlock::Thinking { .. } => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::ContentBlock;
    use serde_json::json;

    const ELISION: &str = "[result elided - re-run the tool if needed]";

    fn tool_use(id: &str, name: &str) -> ContentBlock {
        ContentBlock::tool_use(id, name, json!({}))
    }

    fn tool_use_input(id: &str, name: &str, input: serde_json::Value) -> ContentBlock {
        ContentBlock::tool_use(id, name, input)
    }

    fn tool_result(id: &str, content: &str) -> ContentBlock {
        ContentBlock::tool_result(id, content, false)
    }

    fn tool_result_err(id: &str, content: &str, is_error: bool) -> ContentBlock {
        ContentBlock::tool_result(id, content, is_error)
    }

    // Three tool-result-bearing user messages, so the oldest is evictable.
    fn three_result_conv(opts: ConversationOpts, contents: [&str; 3]) -> Conversation {
        let mut conv = Conversation::new("sys", opts);
        conv.add_user_text("go");
        conv.add_assistant_blocks(vec![tool_use("t1", "read_file")]);
        conv.add_tool_results(vec![tool_result("t1", contents[0])], vec![]);
        conv.add_assistant_blocks(vec![tool_use("t2", "read_file")]);
        conv.add_tool_results(vec![tool_result("t2", contents[1])], vec![]);
        conv.add_assistant_blocks(vec![tool_use("t3", "read_file")]);
        conv.add_tool_results(vec![tool_result("t3", contents[2])], vec![]);
        conv
    }

    fn result_contents(conv: &Conversation) -> Vec<String> {
        let mut out = Vec::new();
        for m in &conv.messages {
            if m.role == Role::User {
                for b in &m.content {
                    if let ContentBlock::ToolResult { content, .. } = b {
                        out.push(content.clone());
                    }
                }
            }
        }
        out
    }

    // ---- new/2 ----

    #[test]
    fn new_builds_empty_conversation_from_explicit_values() {
        let conv = Conversation::new("You are Baud.", ConversationOpts::new(32_000, 45));
        assert_eq!(conv.system_prompt, "You are Baud.");
        assert!(conv.messages.is_empty());
        assert_eq!(conv.last_usage, None);
        assert_eq!(conv.context_budget, 32_000);
        assert_eq!(conv.max_tokens_reserve, 45);
    }

    // NOTE: baud's "context_budget and max_tokens_reserve are required
    // (KeyError)" test is enforced here by the type system: ConversationOpts
    // makes both fields non-optional, so a caller cannot omit them. No runtime
    // assertion is possible or needed - the equivalent guarantee is a compile
    // error. (Documented judgment call.)

    #[test]
    fn new_overhead_chars_defaults_to_0_and_is_settable() {
        let base = Conversation::new("sys", ConversationOpts::new(123, 0));
        let with_overhead =
            Conversation::new("sys", ConversationOpts::new(123, 0).overhead_chars(700));
        assert_eq!(base.overhead_chars, 0);
        assert_eq!(with_overhead.overhead_chars, 700);
    }

    #[test]
    fn new_eviction_slack_defaults_to_zero_and_is_settable() {
        let base = Conversation::new("sys", ConversationOpts::new(123, 0));
        let with_slack =
            Conversation::new("sys", ConversationOpts::new(123, 0).eviction_slack(0.5));
        assert_eq!(base.eviction_slack, 0.0);
        assert_eq!(with_slack.eviction_slack, 0.5);
    }

    #[test]
    fn new_compaction_keep_defaults_to_half_and_is_settable() {
        let base = Conversation::new("sys", ConversationOpts::new(123, 0));
        let with_keep =
            Conversation::new("sys", ConversationOpts::new(123, 0).compaction_keep(0.3));
        assert_eq!(base.compaction_keep, 0.5);
        assert_eq!(with_keep.compaction_keep, 0.3);
    }

    // ---- message appending ----

    #[test]
    fn add_user_text_appends_user_message_with_single_text_block() {
        let mut conv = Conversation::new("sys", ConversationOpts::new(1000, 0));
        conv.add_user_text("hello");
        assert_eq!(
            conv.messages,
            vec![Message::user(vec![ContentBlock::text("hello")])]
        );
    }

    #[test]
    fn add_assistant_blocks_appends_one_message_with_blocks_as_given() {
        let blocks = vec![
            ContentBlock::text("reading"),
            tool_use_input("t1", "read_file", json!({"path": "a"})),
        ];
        let mut conv = Conversation::new("sys", ConversationOpts::new(1000, 0));
        conv.add_user_text("hi");
        conv.add_assistant_blocks(blocks.clone());
        assert_eq!(conv.messages.last().unwrap(), &Message::assistant(blocks));
    }

    #[test]
    fn add_tool_results_appends_all_results_as_one_user_message() {
        let results = vec![
            tool_result("t1", "one"),
            tool_result_err("t2", "two", true),
        ];
        let mut conv = Conversation::new("sys", ConversationOpts::new(1000, 0));
        conv.add_user_text("hi");
        conv.add_assistant_blocks(vec![tool_use("t1", "grep"), tool_use("t2", "grep")]);
        conv.add_tool_results(results.clone(), vec![]);
        assert_eq!(conv.messages.len(), 3);
        assert_eq!(conv.messages.last().unwrap(), &Message::user(results));
    }

    #[test]
    fn messages_accumulate_in_order() {
        let mut conv = Conversation::new("sys", ConversationOpts::new(1000, 0));
        conv.add_user_text("first");
        conv.add_assistant_blocks(vec![ContentBlock::text("ok")]);
        conv.add_user_text("second");
        let roles: Vec<Role> = conv.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, vec![Role::User, Role::Assistant, Role::User]);
    }

    #[test]
    fn note_usage_stores_the_usage() {
        let mut conv = Conversation::new("sys", ConversationOpts::new(1000, 0));
        let usage = Usage::with_input_tokens(10);
        conv.note_usage(usage.clone());
        assert_eq!(conv.last_usage, Some(usage));
    }

    // ---- token_estimate/1 ----

    #[test]
    fn token_estimate_is_ceil_chars_over_35() {
        // 4 (system) + 5 (text) = 9 chars -> ceil(9 / 3.5) = 3
        let mut conv = Conversation::new("abcd", ConversationOpts::new(1000, 0));
        conv.add_user_text("hello");
        assert_eq!(conv.token_estimate(), 3);
    }

    #[test]
    fn token_estimate_counts_overhead_chars() {
        // 4 + 5 + 26 = 35 -> 35 / 3.5 = 10
        let mut conv = Conversation::new("abcd", ConversationOpts::new(1000, 0).overhead_chars(26));
        conv.add_user_text("hello");
        assert_eq!(conv.token_estimate(), 10);
    }

    #[test]
    fn token_estimate_counts_tool_use_and_tool_result_content() {
        let base = Conversation::new("", ConversationOpts::new(1000, 0));
        let mut with_blocks = base.clone();
        with_blocks.add_assistant_blocks(vec![tool_use_input(
            "t1",
            "grep",
            json!({"pattern": "x"}),
        )]);
        with_blocks.add_tool_results(vec![tool_result("t1", &"r".repeat(40))], vec![]);

        assert_eq!(base.token_estimate(), 0);
        assert!(with_blocks.token_estimate() > base.token_estimate());
        assert!(with_blocks.token_estimate() >= 10);
    }

    #[test]
    fn token_estimate_uses_input_tokens_when_larger() {
        let mut conv = Conversation::new("abcd", ConversationOpts::new(1000, 0));
        conv.add_user_text("hello");
        conv.note_usage(Usage::with_input_tokens(500));
        assert_eq!(conv.token_estimate(), 500);
    }

    #[test]
    fn token_estimate_keeps_char_estimate_when_input_tokens_smaller() {
        // 400 chars -> ceil(400 / 3.5) = 115
        let mut conv = Conversation::new("s".repeat(400), ConversationOpts::new(1000, 0));
        conv.note_usage(Usage::with_input_tokens(1));
        assert_eq!(conv.token_estimate(), 115);
    }

    #[test]
    fn token_estimate_accepts_atom_keyed_usage() {
        // In baud this distinguished atom- vs string-keyed maps; in Rust the
        // Usage type unifies both, so this simply confirms input_tokens wins.
        let mut conv = Conversation::new("abcd", ConversationOpts::new(1000, 0));
        conv.add_user_text("hello");
        conv.note_usage(Usage::with_input_tokens(500));
        assert_eq!(conv.token_estimate(), 500);
    }

    #[test]
    fn token_estimate_ignores_usage_without_input_tokens() {
        // 4 chars -> ceil(4 / 3.5) = 2
        let mut conv = Conversation::new("abcd", ConversationOpts::new(1000, 0));
        conv.note_usage(Usage::empty());
        assert_eq!(conv.token_estimate(), 2);
    }

    // ---- compaction_target/1 ----

    #[test]
    fn compaction_target_is_live_window_minus_slack() {
        let conv = Conversation::new("sys", ConversationOpts::new(1000, 200).eviction_slack(0.3));
        assert_eq!(conv.compaction_target(), 500);
        assert_eq!(compaction_target(1000, 200, 0.3), 500);
    }

    #[test]
    fn compaction_target_with_no_slack_equals_budget_target() {
        let conv = Conversation::new("sys", ConversationOpts::new(1000, 200));
        assert_eq!(conv.compaction_target(), 800);
    }

    #[test]
    fn compaction_target_clamps_at_zero() {
        assert_eq!(compaction_target(1000, 900, 0.5), 0);
    }

    // ---- for_request/1 ----

    #[test]
    fn for_request_returns_system_and_messages_wire_ready() {
        let mut conv = Conversation::new("sys", ConversationOpts::new(1000, 0));
        conv.add_user_text("hello");
        assert_eq!(
            conv.for_request(),
            Ok(Request {
                system: "sys".to_string(),
                messages: vec![Message::user(vec![ContentBlock::text("hello")])],
            })
        );
    }

    #[test]
    fn for_request_applies_eviction_before_returning() {
        let big = "x".repeat(400);
        let conv = three_result_conv(ConversationOpts::new(30, 0), [&big, "small2", "small3"]);
        let req = conv.for_request().unwrap();
        assert_eq!(req.system, "sys");
        let contents: Vec<String> = req
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .flat_map(|m| {
                m.content.iter().filter_map(|b| match b {
                    ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                    _ => None,
                })
            })
            .collect();
        assert_eq!(contents, vec![ELISION, "small2", "small3"]);
    }

    #[test]
    fn for_request_fails_loudly_when_eviction_cannot_fit() {
        let big = "x".repeat(400);
        let mut conv = Conversation::new("sys", ConversationOpts::new(10, 0));
        conv.add_user_text("go");
        conv.add_assistant_blocks(vec![tool_use("t1", "read_file")]);
        conv.add_tool_results(vec![tool_result("t1", &big)], vec![]);
        conv.add_assistant_blocks(vec![tool_use("t2", "read_file")]);
        conv.add_tool_results(vec![tool_result("t2", &big)], vec![]);
        assert_eq!(conv.for_request(), Err(ContextBudgetExhausted));
    }

    #[test]
    fn for_request_reply_reserve_counts_against_budget() {
        let mut conv = Conversation::new("sys", ConversationOpts::new(100, 99));
        conv.add_user_text("hello");
        assert_eq!(conv.for_request(), Err(ContextBudgetExhausted));
    }

    #[test]
    fn for_request_last_usage_floor_does_not_fail_evicted_request() {
        let mut conv = Conversation::new("sys", ConversationOpts::new(1000, 0));
        conv.add_user_text("hello");
        conv.note_usage(Usage::with_input_tokens(5000));
        assert!(conv.for_request().is_ok());
    }

    // ---- evict/1 ----

    #[test]
    fn evict_is_noop_when_under_budget() {
        let conv = three_result_conv(ConversationOpts::new(10_000, 100), ["r1", "r2", "r3"]);
        assert_eq!(conv.evict(), conv);
    }

    #[test]
    fn evict_evicts_the_oldest_tool_result_first() {
        let big = "a".repeat(400);
        let conv = three_result_conv(ConversationOpts::new(50, 0), [&big, "keep2", "keep3"]);
        let evicted = conv.evict();
        assert_eq!(result_contents(&evicted), vec![ELISION, "keep2", "keep3"]);
    }

    #[test]
    fn evict_never_evicts_the_last_two_bearing_messages() {
        let big = "b".repeat(400);
        let conv = three_result_conv(ConversationOpts::new(1, 0), [&big, &big, &big]);
        let evicted = conv.evict();
        assert_eq!(
            result_contents(&evicted),
            vec![ELISION.to_string(), big.clone(), big.clone()]
        );
        assert!(evicted.token_estimate() > 1);
    }

    #[test]
    fn evict_does_nothing_when_only_two_bearing_messages() {
        let big = "c".repeat(400);
        let mut conv = Conversation::new("sys", ConversationOpts::new(1, 0));
        conv.add_user_text("go");
        conv.add_assistant_blocks(vec![tool_use("t1", "read_file")]);
        conv.add_tool_results(vec![tool_result("t1", &big)], vec![]);
        conv.add_assistant_blocks(vec![tool_use("t2", "read_file")]);
        conv.add_tool_results(vec![tool_result("t2", &big)], vec![]);
        assert_eq!(conv.evict(), conv);
    }

    #[test]
    fn evict_never_touches_system_prompt_or_non_tool_result_blocks() {
        let big = "d".repeat(400);
        let mixed = Message::user(vec![
            ContentBlock::text("note from user"),
            tool_result("t0", &big),
        ]);
        let mut conv = three_result_conv(ConversationOpts::new(1, 0), [&big, "r2", "r3"]);
        let mut messages = vec![mixed];
        messages.extend(conv.messages.clone());
        conv.messages = messages;

        let orig_tail = conv.messages[1..].to_vec();
        let evicted = conv.evict();

        assert_eq!(evicted.system_prompt, "sys");

        assert_eq!(
            evicted.messages[0].content,
            vec![
                ContentBlock::text("note from user"),
                tool_result("t0", ELISION),
            ]
        );

        // Every non-tool_result block everywhere is untouched.
        let strip = |msgs: &[Message]| -> Vec<Vec<ContentBlock>> {
            msgs.iter()
                .map(|m| {
                    m.content
                        .iter()
                        .filter(|b| !is_tool_result(b))
                        .cloned()
                        .collect()
                })
                .collect()
        };
        assert_eq!(strip(&evicted.messages[1..]), strip(&orig_tail));
    }

    #[test]
    fn evict_with_zero_slack_stops_as_soon_as_it_fits() {
        let big = "e".repeat(400);
        let contents = [big.clone(), big.clone(), "r3".to_string(), "r4".to_string()];

        let build = |budget: u64| -> Conversation {
            let mut conv = Conversation::new(
                "sys",
                ConversationOpts::new(budget, 7).eviction_slack(0.0),
            );
            conv.add_user_text("go");
            conv.add_assistant_blocks(vec![tool_use("t1", "read_file")]);
            conv.add_tool_results(vec![tool_result("t1", &contents[0])], vec![]);
            conv.add_assistant_blocks(vec![tool_use("t2", "read_file")]);
            conv.add_tool_results(vec![tool_result("t2", &contents[1])], vec![]);
            conv.add_assistant_blocks(vec![tool_use("t3", "read_file")]);
            conv.add_tool_results(vec![tool_result("t3", &contents[2])], vec![]);
            conv.add_assistant_blocks(vec![tool_use("t4", "read_file")]);
            conv.add_tool_results(vec![tool_result("t4", &contents[3])], vec![]);
            conv
        };

        let mut after_one = build(1);
        after_one.messages[2].content = vec![tool_result("t1", ELISION)];
        let budget = after_one.token_estimate() + 7;
        let conv = build(budget);

        assert!(conv.token_estimate() > budget - 7);

        let evicted = conv.evict();
        assert_eq!(result_contents(&evicted), vec![ELISION, &big, "r3", "r4"]);
        assert!(evicted.token_estimate() <= budget - 7);
    }

    #[test]
    fn evict_with_slack_overshoots_to_low_water_mark() {
        let big = "e".repeat(400);
        let m100 = "m".repeat(100);

        let build = |budget: u64, slack: f64| -> Conversation {
            let mut conv = Conversation::new(
                "sys",
                ConversationOpts::new(budget, 7).eviction_slack(slack),
            );
            conv.add_user_text("go");
            conv.add_assistant_blocks(vec![tool_use("t1", "read_file")]);
            conv.add_tool_results(vec![tool_result("t1", &big)], vec![]);
            conv.add_assistant_blocks(vec![tool_use("t2", "read_file")]);
            conv.add_tool_results(vec![tool_result("t2", &big)], vec![]);
            conv.add_assistant_blocks(vec![tool_use("t3", "read_file")]);
            conv.add_tool_results(vec![tool_result("t3", &m100)], vec![]);
            conv.add_assistant_blocks(vec![tool_use("t4", "read_file")]);
            conv.add_tool_results(vec![tool_result("t4", "r4")], vec![]);
            conv.add_assistant_blocks(vec![tool_use("t5", "read_file")]);
            conv.add_tool_results(vec![tool_result("t5", "r5")], vec![]);
            conv
        };

        let mut after_one = build(1, 0.0);
        after_one.messages[2].content = vec![tool_result("t1", ELISION)];
        let budget = after_one.token_estimate() + 7;

        let minimal = build(budget, 0.0).evict();
        let wave = build(budget, 0.3).evict();

        assert_eq!(
            result_contents(&minimal),
            vec![ELISION, &big, &m100, "r4", "r5"]
        );
        assert_eq!(
            result_contents(&wave),
            vec![ELISION, ELISION, &m100, "r4", "r5"]
        );

        assert_eq!(wave.evict(), wave);
    }

    #[test]
    fn evict_multiple_tool_results_within_one_message_in_order() {
        let big = "f".repeat(400);
        let mut conv = Conversation::new("sys", ConversationOpts::new(1, 0));
        conv.add_user_text("go");
        conv.add_assistant_blocks(vec![tool_use("t1", "grep"), tool_use("t2", "grep")]);
        conv.add_tool_results(
            vec![tool_result("t1", &big), tool_result("t2", &big)],
            vec![],
        );
        conv.add_assistant_blocks(vec![tool_use("t3", "grep")]);
        conv.add_tool_results(vec![tool_result("t3", "r3")], vec![]);
        conv.add_assistant_blocks(vec![tool_use("t4", "grep")]);
        conv.add_tool_results(vec![tool_result("t4", "r4")], vec![]);

        let evicted = conv.evict();
        assert_eq!(
            result_contents(&evicted),
            vec![ELISION, ELISION, "r3", "r4"]
        );
    }

    #[test]
    fn evict_stale_input_tokens_floor_does_not_force_all_or_nothing() {
        let mut conv = Conversation::new("sys", ConversationOpts::new(200, 0));
        conv.add_user_text("go");
        conv.add_assistant_blocks(vec![tool_use("t1", "read_file")]);
        conv.add_tool_results(vec![tool_result("t1", &"a".repeat(4000))], vec![]);
        conv.add_assistant_blocks(vec![tool_use("t2", "read_file")]);
        conv.add_tool_results(vec![tool_result("t2", &"b".repeat(100))], vec![]);
        conv.add_assistant_blocks(vec![tool_use("t3", "read_file")]);
        conv.add_tool_results(vec![tool_result("t3", "c")], vec![]);
        conv.add_assistant_blocks(vec![tool_use("t4", "read_file")]);
        conv.add_tool_results(vec![tool_result("t4", "d")], vec![]);
        conv.note_usage(Usage::with_input_tokens(1_000_000));

        let evicted = conv.evict();
        assert_eq!(
            result_contents(&evicted),
            vec![ELISION.to_string(), "b".repeat(100), "c".to_string(), "d".to_string()]
        );
    }

    #[test]
    fn evict_is_idempotent() {
        let big = "g".repeat(400);
        let conv = three_result_conv(ConversationOpts::new(1, 0), [&big, &big, &big]);
        let once = conv.evict();
        let twice = once.evict();
        assert_eq!(twice, once);
        assert_eq!(
            result_contents(&twice),
            vec![ELISION.to_string(), big.clone(), big.clone()]
        );
    }

    #[test]
    fn evict_terminates_when_only_evictable_already_elided() {
        let conv = three_result_conv(ConversationOpts::new(1, 0), [ELISION, "r2", "r3"]);
        assert_eq!(conv.evict(), conv);
    }

    #[test]
    fn evict_handles_conversation_with_nothing_evictable() {
        let mut conv = Conversation::new("sys", ConversationOpts::new(1, 0));
        conv.add_user_text("h".repeat(400));
        conv.add_assistant_blocks(vec![ContentBlock::text("i".repeat(400))]);
        assert_eq!(conv.evict(), conv);
    }

    #[test]
    fn evict_handles_empty_conversation() {
        let conv = Conversation::new("sys", ConversationOpts::new(1, 0));
        assert_eq!(conv.evict(), conv);
    }

    // ---- prepare_compaction/1 ----

    #[test]
    fn prepare_compaction_noop_for_empty() {
        let conv = Conversation::new("sys", ConversationOpts::new(32_000, 1000));
        assert_eq!(conv.prepare_compaction(), None);
    }

    #[test]
    fn prepare_compaction_noop_for_one_user_message() {
        let mut conv = Conversation::new("sys", ConversationOpts::new(32_000, 1000));
        conv.add_user_text("hello");
        assert_eq!(conv.prepare_compaction(), None);
    }

    #[test]
    fn prepare_compaction_finds_cutoff_across_turns() {
        let mut conv =
            Conversation::new("sys", ConversationOpts::new(200, 0).eviction_slack(0.0));
        for (u, a) in [("a", "b"), ("c", "d"), ("e", "f"), ("g", "h")] {
            conv.add_user_text(u.repeat(100));
            conv.add_assistant_blocks(vec![ContentBlock::text(a.repeat(100))]);
        }

        let (to_summarize, cutoff_idx, _) = conv.prepare_compaction().unwrap();
        assert!(to_summarize.len() >= 2);
        assert!(cutoff_idx > 0);
        assert!(cutoff_idx < conv.messages.len());

        let cutoff_msg = &conv.messages[cutoff_idx];
        assert_eq!(cutoff_msg.role, Role::User);
        assert!(matches!(cutoff_msg.content.first(), Some(ContentBlock::Text { .. })));
    }

    #[test]
    fn prepare_compaction_keep_is_compaction_keep_of_window() {
        let build = |keep: f64| -> Conversation {
            let mut conv =
                Conversation::new("sys", ConversationOpts::new(10_000, 0).compaction_keep(keep));
            for (u, a) in [("a", "b"), ("c", "d"), ("e", "f"), ("g", "h")] {
                conv.add_user_text(u.repeat(700));
                conv.add_assistant_blocks(vec![ContentBlock::text(a.repeat(700))]);
            }
            conv
        };

        let (_, small_cutoff, _) = build(0.05).prepare_compaction().unwrap();
        let (_, large_cutoff, _) = build(0.3).prepare_compaction().unwrap();
        assert!(small_cutoff > large_cutoff);
    }

    #[test]
    fn prepare_compaction_eviction_slack_no_longer_affects_cutoff() {
        let build = |slack: f64| -> Conversation {
            let mut conv = Conversation::new(
                "sys",
                ConversationOpts::new(1_000, 0)
                    .compaction_keep(0.5)
                    .eviction_slack(slack),
            );
            for (u, a) in [("a", "b"), ("c", "d"), ("e", "f")] {
                conv.add_user_text(u.repeat(300));
                conv.add_assistant_blocks(vec![ContentBlock::text(a.repeat(300))]);
            }
            conv
        };

        let (_, cutoff_zero, _) = build(0.0).prepare_compaction().unwrap();
        let (_, cutoff_high, _) = build(0.9).prepare_compaction().unwrap();
        assert_eq!(cutoff_zero, cutoff_high);
    }

    #[test]
    fn prepare_compaction_cutoff_lands_on_turn_start_user_message() {
        let mut conv = Conversation::new("sys", ConversationOpts::new(1, 0));
        for (u, a) in [("turn 1", "a"), ("turn 2", "b"), ("turn 3", "c"), ("turn 4", "d")] {
            conv.add_user_text(u);
            conv.add_assistant_blocks(vec![ContentBlock::text(a)]);
        }

        let (_, cutoff_idx, _) = conv.prepare_compaction().unwrap();
        let cutoff_msg = &conv.messages[cutoff_idx];
        assert_eq!(cutoff_msg.role, Role::User);
        assert!(matches!(cutoff_msg.content.first(), Some(ContentBlock::Text { .. })));
    }

    // ---- apply_compaction/3 ----

    #[test]
    fn apply_compaction_replaces_old_messages_keeps_tail() {
        let mut conv = Conversation::new("sys", ConversationOpts::new(1, 0));
        conv.add_user_text("turn 1");
        conv.add_assistant_blocks(vec![ContentBlock::text("old response")]);
        conv.add_user_text("turn 2");
        conv.add_assistant_blocks(vec![ContentBlock::text("this should survive")]);

        let compacted = conv.apply_compaction("Summary of turn 1", 2);
        assert_eq!(compacted.messages.len(), 3);

        assert_eq!(compacted.messages[0].role, Role::User);
        match compacted.messages[0].content.first().unwrap() {
            ContentBlock::Text { text } => assert!(text.contains("Summary of turn 1")),
            _ => panic!("expected text summary block"),
        }

        assert_eq!(compacted.messages[1].role, Role::User);
        match compacted.messages[1].content.first().unwrap() {
            ContentBlock::Text { text } => assert_eq!(text, "turn 2"),
            _ => panic!("expected turn 2 text"),
        }

        assert_eq!(compacted.messages[2].role, Role::Assistant);
        match compacted.messages[2].content.first().unwrap() {
            ContentBlock::Text { text } => assert_eq!(text, "this should survive"),
            _ => panic!("expected surviving text"),
        }
    }

    // ---- extract_file_ops/1 ----

    #[test]
    fn extract_file_ops_extracts_read_and_write_ops() {
        let messages = vec![
            Message::user(vec![
                tool_use_input("_", "read_file", json!({"path": "foo.ex"})),
                tool_use_input("_", "write_file", json!({"path": "bar.ex"})),
            ]),
            Message::user(vec![
                tool_use_input("_", "edit_file", json!({"path": "bar.ex"})),
                tool_use_input("_", "list_files", json!({"path": "lib/"})),
            ]),
        ];

        let ops = extract_file_ops(&messages);
        let mut reads = ops.read_files.clone();
        reads.sort();
        let mut mods = ops.modified_files.clone();
        mods.sort();
        assert_eq!(reads, vec!["foo.ex", "lib/"]);
        assert_eq!(mods, vec!["bar.ex"]);
    }

    #[test]
    fn extract_file_ops_deduplicates() {
        let messages = vec![Message::user(vec![
            tool_use_input("_", "read_file", json!({"path": "foo.ex"})),
            tool_use_input("_", "read_file", json!({"path": "foo.ex"})),
        ])];
        let ops = extract_file_ops(&messages);
        assert_eq!(ops.read_files, vec!["foo.ex"]);
    }

    // ---- inject_anchor/2 ----

    #[test]
    fn inject_anchor_merges_into_trailing_user_message() {
        let mut conv = Conversation::new("sys", ConversationOpts::new(1000, 0));
        conv.add_user_text("go");
        conv.add_assistant_blocks(vec![tool_use("t1", "read_file")]);
        conv.add_tool_results(vec![tool_result("t1", "contents")], vec![]);
        conv.inject_anchor(voice::anchor("go", Some("plan text")));

        assert_eq!(conv.messages.len(), 3);
        let last = conv.messages.last().unwrap();
        assert_eq!(last.role, Role::User);
        let last_block = last.content.last().unwrap();
        assert!(voice::is_anchor(last_block));
    }

    #[test]
    fn inject_anchor_starts_fresh_user_message_when_tail_not_user() {
        let mut conv = Conversation::new("sys", ConversationOpts::new(1000, 0));
        conv.add_user_text("go");
        conv.add_assistant_blocks(vec![ContentBlock::text("working")]);
        conv.inject_anchor(voice::anchor("go", Some("plan text")));

        let last = conv.messages.last().unwrap();
        assert_eq!(last.role, Role::User);
        assert_eq!(last.content.len(), 1);
        assert!(voice::is_anchor(&last.content[0]));
    }

    // ---- eviction reclaims stale Anchors ----

    fn anchor_texts(conv: &Conversation) -> Vec<String> {
        let mut out = Vec::new();
        for m in &conv.messages {
            if m.role == Role::User {
                for b in &m.content {
                    if let ContentBlock::Text { text } = b
                        && voice::is_anchor(b) {
                            out.push(text.clone());
                        }
                }
            }
        }
        out
    }

    #[test]
    fn eviction_elides_older_anchor_but_never_most_recent() {
        let big = "x".repeat(4000);
        let anchor1 = voice::anchor("task", Some("plan v1"));
        let anchor2 = voice::anchor("task", Some(&format!("plan v2 {big}")));

        let mut conv = Conversation::new("sys", ConversationOpts::new(1200, 100));
        conv.add_user_text(format!("go {big}"));
        conv.inject_anchor(anchor1.clone());
        conv.add_assistant_blocks(vec![ContentBlock::text("ok")]);
        conv.add_user_text(format!("more {big}"));
        conv.inject_anchor(anchor2.clone());

        let evicted = conv.evict();
        let texts = anchor_texts(&evicted);

        assert!(texts.iter().any(|t| t == &anchor2));
        assert!(texts.iter().any(|t| t == voice::anchor_elision_marker()));
        assert!(!texts.iter().any(|t| t == &anchor1));
    }

    #[test]
    fn eviction_never_elides_most_recent_anchor_even_if_only_evictable() {
        let big = "x".repeat(8000);
        let anchor = voice::anchor("task", Some(&format!("plan {big}")));

        let mut conv = Conversation::new("sys", ConversationOpts::new(1000, 100));
        conv.add_user_text("go");
        conv.inject_anchor(anchor.clone());

        let evicted = conv.evict();
        assert_eq!(anchor_texts(&evicted), vec![anchor]);
    }

    #[test]
    fn eviction_idempotent_over_anchors() {
        let big = "x".repeat(4000);
        let anchor1 = voice::anchor("task", Some(&format!("plan v1 {big}")));
        let anchor2 = voice::anchor("task", Some(&format!("plan v2 {big}")));

        let mut conv = Conversation::new("sys", ConversationOpts::new(1200, 100));
        conv.add_user_text("go");
        conv.inject_anchor(anchor1);
        conv.add_assistant_blocks(vec![ContentBlock::text("ok")]);
        conv.add_user_text("more");
        conv.inject_anchor(anchor2);

        let once = conv.evict();
        let twice = once.evict();
        assert_eq!(once.messages, twice.messages);
    }
}
