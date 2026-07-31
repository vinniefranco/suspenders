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
/// absent; here the type system requires them). The rest carry baud's
/// defaults via [`Default`].
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

impl ConversationOpts {
    /// The two required knobs, with baud's defaults for the rest
    /// (`overhead_chars: 0`, `compaction_slack: 0.0`, `compaction_keep: 0.5`).
    pub fn new(context_budget: u64, max_tokens_reserve: u64) -> Self {
        ConversationOpts {
            context_budget,
            max_tokens_reserve,
            overhead_chars: 0,
            compaction_slack: DEFAULT_COMPACTION_SLACK,
            compaction_keep: DEFAULT_COMPACTION_KEEP,
        }
    }

    pub fn overhead_chars(mut self, v: u64) -> Self {
        self.overhead_chars = v;
        self
    }

    pub fn compaction_slack(mut self, v: f64) -> Self {
        self.compaction_slack = v;
        self
    }

    pub fn compaction_keep(mut self, v: f64) -> Self {
        self.compaction_keep = v;
        self
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
mod tests {
    use super::*;
    use crate::content::ContentBlock;
    use serde_json::json;

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

    // A minimal started conversation: system prompt "sys", budget 1000,
    // one user message "hi". Used wherever a test just needs a conversation
    // with at least one turn before appending assistant or result messages.
    fn started_conv() -> Conversation {
        let mut conv = Conversation::new("sys", ConversationOpts::new(1000, 0));
        conv.add_user_text("hi");
        conv
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
    fn new_compaction_slack_defaults_to_zero_and_is_settable() {
        let base = Conversation::new("sys", ConversationOpts::new(123, 0));
        let with_slack =
            Conversation::new("sys", ConversationOpts::new(123, 0).compaction_slack(0.5));
        assert_eq!(base.compaction_slack, 0.0);
        assert_eq!(with_slack.compaction_slack, 0.5);
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
        let mut conv = started_conv();
        conv.add_assistant_blocks(blocks.clone());
        assert_eq!(conv.messages.last().unwrap(), &Message::assistant(blocks));
    }

    #[test]
    fn add_assistant_response_stamps_the_message_add_assistant_blocks_does_not() {
        let stamp = Provenance::new("anthropic", "claude-fable-5");
        let mut conv = started_conv();
        conv.add_assistant_response(vec![ContentBlock::text("reply")], stamp.clone());
        conv.add_assistant_blocks(vec![ContentBlock::text("[marker]")]);
        assert_eq!(conv.messages[1].provenance, Some(stamp));
        assert_eq!(conv.messages[2].provenance, None);
    }

    #[test]
    fn add_tool_results_appends_all_results_as_one_user_message() {
        let results = vec![tool_result("t1", "one"), tool_result_err("t2", "two", true)];
        let mut conv = started_conv();
        conv.add_assistant_blocks(vec![tool_use("t1", "grep_search"), tool_use("t2", "grep_search")]);
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
            "grep_search",
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
        conv.note_usage(Usage::default());
        assert_eq!(conv.token_estimate(), 2);
    }

    #[test]
    fn token_estimate_floors_at_the_cache_inclusive_sum() {
        // Warm cache: a tiny uncached remainder over a six-figure cached
        // prefix. The floor holds at the cache-inclusive sum, not at
        // input_tokens (ADR-0036).
        let mut conv = Conversation::new("abcd", ConversationOpts::new(200_000, 0));
        conv.add_user_text("hello");
        conv.note_usage(Usage {
            input_tokens: Some(200),
            output_tokens: Some(300),
            cache_read_input_tokens: Some(90_000),
            cache_creation_input_tokens: None,
        });
        assert_eq!(conv.token_estimate(), 90_500);
    }

    // ---- compaction_target/1 ----

    #[test]
    fn compaction_target_is_live_window_minus_slack() {
        let conv = Conversation::new(
            "sys",
            ConversationOpts::new(1000, 200).compaction_slack(0.3),
        );
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

    // ---- compaction_keep_amount/1 ----

    #[test]
    fn compaction_keep_amount_is_keep_fraction_of_live_window() {
        let conv = Conversation::new("sys", ConversationOpts::new(1000, 200).compaction_keep(0.5));
        assert_eq!(conv.compaction_keep_amount(), 400);
        assert_eq!(compaction_keep_amount(1000, 200, 0.5), 400);
    }

    #[test]
    fn compaction_keep_amount_clamps_at_zero_when_reserve_exceeds_budget() {
        assert_eq!(compaction_keep_amount(1000, 1200, 0.5), 0);
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
    fn for_request_errs_when_char_estimate_exceeds_the_live_window() {
        // Pure fit-check on the char estimate against `budget - reserve` - the
        // same final-fit threshold the retired Eviction path used, so the
        // Compaction trigger point (loop_ recovers on this Err) is unchanged.
        let mut conv = Conversation::new("sys", ConversationOpts::new(50, 5));
        conv.add_user_text("x".repeat(400));
        assert!(conv.char_estimate() > 50 - 5);
        assert_eq!(conv.for_request(), Err(ContextBudgetExhausted));
    }

    #[test]
    fn for_request_reply_reserve_counts_against_budget() {
        let mut conv = Conversation::new("sys", ConversationOpts::new(100, 99));
        conv.add_user_text("hello");
        assert_eq!(conv.for_request(), Err(ContextBudgetExhausted));
    }

    #[test]
    fn for_request_last_usage_floor_does_not_fail_the_fit_check() {
        // The fit check uses the char estimate alone; a large usage floor does
        // not make a small request fail.
        let mut conv = Conversation::new("sys", ConversationOpts::new(1000, 0));
        conv.add_user_text("hello");
        conv.note_usage(Usage::with_input_tokens(5000));
        assert!(conv.for_request().is_ok());
    }

    // ---- keep_cutoff/2 ----

    fn user_msg_of_chars(n: usize) -> Message {
        Message::user(vec![ContentBlock::text("x".repeat(n))])
    }

    #[test]
    fn keep_cutoff_returns_the_index_of_the_crossing_message() {
        // Newest-first: 100 < 150, then 200 >= 150 crosses at index 1.
        let messages = vec![
            user_msg_of_chars(100),
            user_msg_of_chars(100),
            user_msg_of_chars(100),
        ];
        assert_eq!(keep_cutoff(&messages, 150), Some(1));
    }

    #[test]
    fn keep_cutoff_with_zero_keep_returns_the_newest_index() {
        let messages = vec![user_msg_of_chars(10), user_msg_of_chars(10)];
        assert_eq!(keep_cutoff(&messages, 0), Some(1));
    }

    #[test]
    fn keep_cutoff_returns_none_when_the_whole_history_fits_within_keep() {
        let messages = vec![user_msg_of_chars(100), user_msg_of_chars(100)];
        assert_eq!(keep_cutoff(&messages, 1000), None);
    }

    #[test]
    fn keep_cutoff_returns_none_for_empty_messages() {
        assert_eq!(keep_cutoff(&[], 0), None);
        assert_eq!(keep_cutoff(&[], 100), None);
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
    fn prepare_compaction_finds_cutoff_across_runs() {
        let mut conv =
            Conversation::new("sys", ConversationOpts::new(200, 0).compaction_slack(0.0));
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
        assert!(matches!(
            cutoff_msg.content.first(),
            Some(ContentBlock::Text { .. })
        ));
    }

    // Multi-run conversation for compaction tests: N pairs of (user,assistant)
    // messages, each padded to `chars_per_msg` characters, with the given opts.
    fn multi_run_conv(
        opts: ConversationOpts,
        pairs: &[(&str, &str)],
        chars_per_msg: usize,
    ) -> Conversation {
        let mut conv = Conversation::new("sys", opts);
        for (u, a) in pairs {
            conv.add_user_text(u.repeat(chars_per_msg));
            conv.add_assistant_blocks(vec![ContentBlock::text(a.repeat(chars_per_msg))]);
        }
        conv
    }

    #[test]
    fn prepare_compaction_keep_is_compaction_keep_of_window() {
        let pairs = [("a", "b"), ("c", "d"), ("e", "f"), ("g", "h")];
        let small = multi_run_conv(
            ConversationOpts::new(10_000, 0).compaction_keep(0.05),
            &pairs,
            700,
        );
        let large = multi_run_conv(
            ConversationOpts::new(10_000, 0).compaction_keep(0.3),
            &pairs,
            700,
        );

        let (_, small_cutoff, _) = small.prepare_compaction().unwrap();
        let (_, large_cutoff, _) = large.prepare_compaction().unwrap();
        assert!(small_cutoff > large_cutoff);
    }

    #[test]
    fn prepare_compaction_walk_measures_keep_in_chars_not_tokens() {
        // Pins the flagged ambiguity in CONTEXT.md (preserved deliberately,
        // pending a tuning decision): the Compaction Keep amount is a
        // token-space figure, but the walk accumulates raw chars, so the
        // executed keep is ~3.5x smaller than configured. Keep amount =
        // 0.05 * 10_000 = 500. The newest message alone is 600 chars
        // (~172 tokens): a char walk crosses on it, snapping the cutoff to
        // the last run start (index 6); a token walk would need three
        // messages and snap to index 4.
        let pairs = [("a", "b"), ("c", "d"), ("e", "f"), ("g", "h")];
        let conv = multi_run_conv(
            ConversationOpts::new(10_000, 0).compaction_keep(0.05),
            &pairs,
            600,
        );

        let (to_summarize, cutoff_idx, _) = conv.prepare_compaction().unwrap();
        assert_eq!(cutoff_idx, 6);
        assert_eq!(to_summarize.len(), 6);
    }

    #[test]
    fn prepare_compaction_compaction_slack_no_longer_affects_cutoff() {
        let pairs = [("a", "b"), ("c", "d"), ("e", "f")];
        let make_opts = || ConversationOpts::new(1_000, 0).compaction_keep(0.5);
        let zero = multi_run_conv(make_opts().compaction_slack(0.0), &pairs, 300);
        let high = multi_run_conv(make_opts().compaction_slack(0.9), &pairs, 300);

        let (_, cutoff_zero, _) = zero.prepare_compaction().unwrap();
        let (_, cutoff_high, _) = high.prepare_compaction().unwrap();
        assert_eq!(cutoff_zero, cutoff_high);
    }

    #[test]
    fn prepare_compaction_cutoff_lands_on_run_start_user_message() {
        let mut conv = Conversation::new("sys", ConversationOpts::new(1, 0));
        for (u, a) in [
            ("turn 1", "a"),
            ("turn 2", "b"),
            ("turn 3", "c"),
            ("turn 4", "d"),
        ] {
            conv.add_user_text(u);
            conv.add_assistant_blocks(vec![ContentBlock::text(a)]);
        }

        let (_, cutoff_idx, _) = conv.prepare_compaction().unwrap();
        let cutoff_msg = &conv.messages[cutoff_idx];
        assert_eq!(cutoff_msg.role, Role::User);
        assert!(matches!(
            cutoff_msg.content.first(),
            Some(ContentBlock::Text { .. })
        ));
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
                tool_use_input("_", "read_file", json!({"file_path": "foo.ex"})),
                tool_use_input("_", "write_file", json!({"file_path": "bar.ex"})),
            ]),
            Message::user(vec![
                tool_use_input("_", "edit", json!({"file_path": "bar.ex"})),
                tool_use_input("_", "list_directory", json!({"path": "lib/"})),
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
            tool_use_input("_", "read_file", json!({"file_path": "foo.ex"})),
            tool_use_input("_", "read_file", json!({"file_path": "foo.ex"})),
        ])];
        let ops = extract_file_ops(&messages);
        assert_eq!(ops.read_files, vec!["foo.ex"]);
    }
}
