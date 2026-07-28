//! Session Log - the JSONL persistence of a Session (CONTEXT.md, ADR-0010).
//!
//! One append-only JSONL file per Session: a header line carrying the
//! Session's fixed facts, then one line per Conversation event, appended as
//! each happens. The log records the Conversation, not the Transcript:
//! Thinking and info lines are never in it.
//!
//! This module owns encode/decode/fold and the file handle; the Agent owns
//! WHEN to append. Entries are event-granular (a checkpoint's assistant
//! message grows as results land, so message-granular appends would rewrite
//! history):
//!
//!   * `user_text` - submit and Rollover alike
//!   * `assistant_blocks{blocks, provider, model}` - each message-end,
//!     tool_use included, stamped with the producing Model's Provenance
//!     (ADR-0037); the fold repairs a dangling batch
//!   * `tool_result` - per Tool Result
//!   * `steering` - delivered Steering (user-voiced)
//!   * `nudge` - a user-role Nudge (Verify Nudge, Explore Nudge). The fold
//!     merges it into an open tool-results batch when one is open (the Explore
//!     Nudge rode that message live), else stands it alone (the Verify Nudge)
//!   * `rider{tag, text}` - a results-tail rider the model read: the Anchor or
//!     an Endgame prompt (wrap-up warning, Verification Pass prompt, final-Pass
//!     prompt), logged as injected. The fold closes the open batch (every
//!     result of the Pass precedes its riders) and re-injects the text through
//!     the same merge seam the live Run used
//!   * `plan` - the model's Plan; held OUTSIDE the Conversation, so the fold
//!     never runs it into a message; [`plan`] reads the last one back
//!   * `message` - a verbatim Conversation message; seeds a fresh log on Resume
//!   * `settled{outcome, stop_reason, reason}` - Run Settlement; `reason` is
//!     forensic only (the fold ignores it)
//!   * `compacted{summary, skip_count, tokens_before, file_ops, original_task}` -
//!     Compaction: old messages replaced by a summary. On Resume the fold
//!     discards everything before this entry and emits just the reconstructed
//!     summary message.
//!
//! The fold ([`resume`]) mirrors the Loop's close rules by construction:
//! answered tool_use blocks are kept (ADR-0009), unanswered ones dropped
//! (ADR-0004), and a log that ends mid-Run settles as failed. A torn last
//! line is dropped: the expected crash mode of an append-only file.

use std::fmt;
use std::io::Write;

use serde::{Deserialize, Serialize};

use crate::content::{ContentBlock, Message, Provenance, Role};
use crate::conversation;
use crate::run::governor::endgame::ReopenReason;
use crate::session::{RecoveryShape, Session};
use crate::voice::{self, FileOps};

// ------------------------------------------------------------------
// Terminal stop reason + settled outcome (shared with Run Settlement).
// These types were introduced by the Settlement phase and are BUILT ON
// here, not redefined.
// ------------------------------------------------------------------

/// A Run's terminal stop reason as it enters the Session Log and the
/// settlement event. Spans the LLM-reported reasons that ride through a
/// completed Run (`end_turn`, `max_tokens`, ...) and the Run-Limit reasons
/// the Endgame mints (`turn_limit`, `turn_limit_stuck`). `Error`/`Unknown` are
/// the synthetic reasons Settlement writes for failed/cancelled Runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    /// The Run ran out of Passes productively (baud `:turn_limit`).
    RunLimit,
    /// The Run ran out of Passes while stuck in a failure loop.
    RunLimitStuck,
    /// A failed Run's synthetic reason (baud `:error`).
    Error,
    /// A cancelled Run's synthetic reason (baud `:unknown`).
    Unknown,
}

impl StopReason {
    fn as_str(&self) -> &'static str {
        match self {
            StopReason::EndTurn => "end_turn",
            StopReason::ToolUse => "tool_use",
            StopReason::MaxTokens => "max_tokens",
            StopReason::StopSequence => "stop_sequence",
            StopReason::RunLimit => "turn_limit",
            StopReason::RunLimitStuck => "turn_limit_stuck",
            StopReason::Error => "error",
            StopReason::Unknown => "unknown",
        }
    }

    // Unknown strings degrade to `Unknown` rather than minting reasons from
    // disk (baud: `String.to_existing_atom` guarded by a known set).
    fn from_str(s: &str) -> StopReason {
        match s {
            "end_turn" => StopReason::EndTurn,
            "tool_use" => StopReason::ToolUse,
            "max_tokens" => StopReason::MaxTokens,
            "stop_sequence" => StopReason::StopSequence,
            "turn_limit" => StopReason::RunLimit,
            "turn_limit_stuck" => StopReason::RunLimitStuck,
            "error" => StopReason::Error,
            _ => StopReason::Unknown,
        }
    }
}

impl fmt::Display for StopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a settled Run resolved (baud's `:completed | :failed | :cancelled`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settled {
    Completed,
    Failed,
    Cancelled,
}

impl Settled {
    fn as_str(&self) -> &'static str {
        match self {
            Settled::Completed => "completed",
            Settled::Failed => "failed",
            Settled::Cancelled => "cancelled",
        }
    }

    fn from_str(s: &str) -> Option<Settled> {
        match s {
            "completed" => Some(Settled::Completed),
            "failed" => Some(Settled::Failed),
            "cancelled" => Some(Settled::Cancelled),
            _ => None,
        }
    }
}

/// The `{settled, outcome, stop_reason, reason}` value a Run Settlement
/// produces. `reason` is the failure term formatted to a string; `None` for
/// completed/cancelled Runs and for failures with no reason. Forensic only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettledEntry {
    pub outcome: Settled,
    pub stop_reason: StopReason,
    pub reason: Option<String>,
}

impl SettledEntry {
    pub fn new(outcome: Settled, stop_reason: StopReason, reason: Option<String>) -> Self {
        SettledEntry {
            outcome,
            stop_reason,
            reason,
        }
    }
}

/// Which rider a `rider` entry carries: the Anchor or one of the Endgame's
/// tail prompts. Forensic - every kind replays through the one tail-merge
/// seam it rode live, so the tag never changes the fold's shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiderTag {
    Anchor,
    WrapUpWarning,
    VerificationPass,
    FinalPass,
}

impl RiderTag {
    fn as_str(&self) -> &'static str {
        match self {
            RiderTag::Anchor => "anchor",
            RiderTag::WrapUpWarning => "wrap_up_warning",
            RiderTag::VerificationPass => "verification_pass",
            RiderTag::FinalPass => "final_pass",
        }
    }

    fn from_str(s: &str) -> Option<RiderTag> {
        match s {
            "anchor" => Some(RiderTag::Anchor),
            "wrap_up_warning" => Some(RiderTag::WrapUpWarning),
            "verification_pass" => Some(RiderTag::VerificationPass),
            "final_pass" => Some(RiderTag::FinalPass),
            _ => None,
        }
    }
}

// ------------------------------------------------------------------
// The entry enum
// ------------------------------------------------------------------

/// One Session Log entry. The fold consumes these; [`append`] serializes them.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    UserText(String),
    /// One message-end's content blocks, stamped with the Provenance of the
    /// Model that produced them (ADR-0037). `None` decodes from a line
    /// missing the provenance fields: unknown Provenance, which the
    /// request-shaping transform treats as a cross-Provider mismatch.
    AssistantBlocks {
        blocks: Vec<ContentBlock>,
        provenance: Option<Provenance>,
    },
    ToolResult(ContentBlock),
    Steering(String),
    Nudge(String),
    Rider {
        tag: RiderTag,
        text: String,
    },
    Plan(String),
    Message(Message),
    Settled {
        outcome: Settled,
        stop_reason: StopReason,
        reason: Option<String>,
    },
    Compacted {
        summary: String,
        skip_count: u64,
        tokens_before: u64,
        file_ops: FileOps,
        original_task: Option<String>,
    },
    /// A Handoff seeded a fresh Conversation (CONTEXT.md: Handoff): the
    /// model's narrative (`None` when the summarization call failed and the
    /// seed degraded to the mechanical skeleton), the harness-owned facts,
    /// and the final verification result verbatim. Like `Compacted`, the fold
    /// discards everything before it and emits the recomposed seed message.
    Handoff {
        summary: Option<String>,
        file_ops: FileOps,
        original_task: Option<String>,
        verification: Option<String>,
    },
    /// The Voice-authored prompt that opened a Recovery Run (CONTEXT.md:
    /// Recovery Run) - a Run-starting prompt like `user_text`, but
    /// distinguishable as Suspenders' voice; `shape` is forensic. `reason`
    /// (ADR-0043) records which of the three evidences reopened the Run, so an
    /// Open-Plan continuation greps distinctly AND the fold restores the right
    /// per-request budget: a broken-state entry restores `recoveries_used`, an
    /// Open-Plan entry restores `advances_used`. The fold merges the prompt
    /// through the same seam the live path used.
    Recovery {
        shape: RecoveryShape,
        reason: ReopenReason,
        text: String,
    },
    /// A malformed-tool-call generation was re-drawn in-band (ADR-0030): the
    /// classified error and the attempt number against the budget, forensic
    /// only. Silent to the model's Conversation - the failed draw produced
    /// nothing to keep - so the fold emits no message; it is durable and
    /// visible so a silent-and-unlogged retry stays rejected.
    Retry {
        error: String,
        attempt: u64,
        budget: u64,
    },
}

// ------------------------------------------------------------------
// Wire codec (serde JSON with the "e" discriminator + baud's field names).
// A human can grep/diff the log, the load-bearing thesis of ADR-0010.
// ------------------------------------------------------------------

impl Entry {
    /// An `assistant_blocks` entry with unknown Provenance; the live Agent
    /// appends stamped entries via the struct form.
    pub fn assistant_blocks(blocks: Vec<ContentBlock>) -> Entry {
        Entry::AssistantBlocks {
            blocks,
            provenance: None,
        }
    }

    fn to_json(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            Entry::UserText(text) => json!({"e": "user_text", "text": text}),
            Entry::Steering(text) => json!({"e": "steering", "text": text}),
            Entry::Nudge(text) => json!({"e": "nudge", "text": text}),
            Entry::Rider { tag, text } => {
                json!({"e": "rider", "tag": tag.as_str(), "text": text})
            }
            Entry::Plan(text) => json!({"e": "plan", "text": text}),
            Entry::AssistantBlocks { blocks, provenance } => {
                let mut value = json!({"e": "assistant_blocks", "blocks": blocks});
                write_provenance(&mut value, provenance.as_ref());
                value
            }
            Entry::ToolResult(block) => json!({"e": "tool_result", "block": block}),
            Entry::Message(message) => {
                let mut value = json!({
                    "e": "message",
                    "role": role_str(message.role),
                    "content": message.content,
                });
                write_provenance(&mut value, message.provenance.as_ref());
                value
            }
            Entry::Settled {
                outcome,
                stop_reason,
                reason,
            } => json!({
                "e": "settled",
                "outcome": outcome.as_str(),
                "stop_reason": stop_reason.as_str(),
                "reason": reason,
            }),
            Entry::Compacted {
                summary,
                skip_count,
                tokens_before,
                file_ops,
                original_task,
            } => json!({
                "e": "compacted",
                "summary": summary,
                "skip_count": skip_count,
                "tokens_before": tokens_before,
                "read_files": file_ops.read_files,
                "modified_files": file_ops.modified_files,
                "original_task": original_task,
            }),
            Entry::Handoff {
                summary,
                file_ops,
                original_task,
                verification,
            } => json!({
                "e": "handoff",
                "summary": summary,
                "read_files": file_ops.read_files,
                "modified_files": file_ops.modified_files,
                "original_task": original_task,
                "verification": verification,
            }),
            Entry::Recovery {
                shape,
                reason,
                text,
            } => {
                json!({"e": "recovery", "shape": shape.as_str(), "reason": reason.as_str(), "text": text})
            }
            Entry::Retry {
                error,
                attempt,
                budget,
            } => json!({
                "e": "retry",
                "error": error,
                "attempt": attempt,
                "budget": budget,
            }),
        }
    }

    // Decode a JSON object into an entry. `None` means "valid JSON but not a
    // valid entry shape" - the fold stops there, like a torn line.
    fn from_json(m: &serde_json::Value) -> Option<Entry> {
        let e = m.get("e")?.as_str()?;
        match e {
            "user_text" => Some(Entry::UserText(string_field(m, "text")?)),
            "steering" => Some(Entry::Steering(string_field(m, "text")?)),
            "nudge" => Some(Entry::Nudge(string_field(m, "text")?)),
            "rider" => parse_rider(m),
            "plan" => Some(Entry::Plan(string_field(m, "text")?)),
            "assistant_blocks" => parse_assistant_blocks(m),
            "tool_result" => parse_tool_result(m),
            "message" => parse_message(m),
            "settled" => parse_settled(m),
            "compacted" => parse_compacted(m),
            "handoff" => parse_handoff(m),
            "recovery" => parse_recovery(m),
            "retry" => parse_retry(m),
            _ => None,
        }
    }
}

// Per-kind entry parsers. Each returns `None` on a shape mismatch - the same
// torn-line tolerance `from_json` carries to the fold.

fn parse_rider(m: &serde_json::Value) -> Option<Entry> {
    Some(Entry::Rider {
        tag: RiderTag::from_str(m.get("tag")?.as_str()?)?,
        text: string_field(m, "text")?,
    })
}

fn parse_assistant_blocks(m: &serde_json::Value) -> Option<Entry> {
    let blocks = decode_blocks(m.get("blocks")?)?;
    Some(Entry::AssistantBlocks {
        blocks,
        provenance: read_provenance(m),
    })
}

fn parse_tool_result(m: &serde_json::Value) -> Option<Entry> {
    let block: ContentBlock = serde_json::from_value(m.get("block")?.clone()).ok()?;
    Some(Entry::ToolResult(block))
}

fn parse_message(m: &serde_json::Value) -> Option<Entry> {
    let role = decode_role(m.get("role")?.as_str()?)?;
    let content = decode_blocks(m.get("content")?)?;
    Some(Entry::Message(Message {
        role,
        content,
        provenance: read_provenance(m),
    }))
}

// The Provenance codec, shared by the assistant_blocks and message entries:
// two flat keys beside the entry's own. Absent keys decode as `None`
// (unknown Provenance) with the same optional-field tolerance the settled
// entry's `reason` takes - the transform treats unknown as a mismatch, so a
// missing stamp degrades to normalization, never to a torn line.
fn write_provenance(value: &mut serde_json::Value, provenance: Option<&Provenance>) {
    if let (Some(p), Some(obj)) = (provenance, value.as_object_mut()) {
        obj.insert("provider".into(), p.provider.clone().into());
        obj.insert("model".into(), p.model.clone().into());
    }
}

fn read_provenance(m: &serde_json::Value) -> Option<Provenance> {
    Some(Provenance::new(
        m.get("provider")?.as_str()?,
        m.get("model")?.as_str()?,
    ))
}

fn parse_settled(m: &serde_json::Value) -> Option<Entry> {
    let outcome = Settled::from_str(m.get("outcome")?.as_str()?)?;
    let stop_reason = StopReason::from_str(m.get("stop_reason")?.as_str()?);
    // The old 3-element form has no "reason" key; it decodes as
    // None, the same tolerance the compacted entry took.
    let reason = m
        .get("reason")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Some(Entry::Settled {
        outcome,
        stop_reason,
        reason,
    })
}

fn parse_compacted(m: &serde_json::Value) -> Option<Entry> {
    let file_ops = FileOps {
        read_files: decode_str_list(m.get("read_files")),
        modified_files: decode_str_list(m.get("modified_files")),
    };
    Some(Entry::Compacted {
        summary: string_field(m, "summary").unwrap_or_default(),
        skip_count: m.get("skip_count").and_then(|v| v.as_u64()).unwrap_or(0),
        tokens_before: m.get("tokens_before").and_then(|v| v.as_u64()).unwrap_or(0),
        file_ops,
        original_task: m
            .get("original_task")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    })
}

fn parse_handoff(m: &serde_json::Value) -> Option<Entry> {
    Some(Entry::Handoff {
        summary: string_field(m, "summary"),
        file_ops: FileOps {
            read_files: decode_str_list(m.get("read_files")),
            modified_files: decode_str_list(m.get("modified_files")),
        },
        original_task: string_field(m, "original_task"),
        verification: string_field(m, "verification"),
    })
}

fn parse_recovery(m: &serde_json::Value) -> Option<Entry> {
    Some(Entry::Recovery {
        shape: RecoveryShape::parse(m.get("shape")?.as_str()?)?,
        // A pre-ADR-0043 entry (or a foreign token) has no valid reason: it
        // degrades to a broken-state recovery, never a torn line - the same
        // optional-field tolerance the settled entry's `reason` takes. Every
        // logged recovery before ADR-0043 was broken-state, so this is exact.
        reason: m
            .get("reason")
            .and_then(|v| v.as_str())
            .and_then(ReopenReason::parse)
            .unwrap_or(ReopenReason::UnverifiedWrites),
        text: string_field(m, "text")?,
    })
}

fn parse_retry(m: &serde_json::Value) -> Option<Entry> {
    Some(Entry::Retry {
        error: string_field(m, "error")?,
        attempt: m.get("attempt")?.as_u64()?,
        budget: m.get("budget")?.as_u64()?,
    })
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

fn decode_role(s: &str) -> Option<Role> {
    match s {
        "user" => Some(Role::User),
        "assistant" => Some(Role::Assistant),
        _ => None,
    }
}

fn string_field(m: &serde_json::Value, key: &str) -> Option<String> {
    Some(m.get(key)?.as_str()?.to_string())
}

fn decode_blocks(v: &serde_json::Value) -> Option<Vec<ContentBlock>> {
    let arr = v.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        out.push(serde_json::from_value(item.clone()).ok()?);
    }
    Some(out)
}

fn decode_str_list(v: Option<&serde_json::Value>) -> Vec<String> {
    match v.and_then(|v| v.as_array()) {
        Some(arr) => arr
            .iter()
            .filter_map(|x| x.as_str().map(|s| s.to_string()))
            .collect(),
        None => Vec::new(),
    }
}

// ------------------------------------------------------------------
// Resume errors + drift
// ------------------------------------------------------------------

/// A header fact that differs from the resuming Session's: `(key, logged,
/// current)`. The new Session's fact wins; the drift is reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drift {
    pub key: &'static str,
    pub logged: String,
    pub current: String,
}

/// Why a Resume failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeError {
    /// The header's Project Root differs from the resuming Session's: the
    /// Conversation is about another project's files.
    RootMismatch,
    /// The file could not be read.
    Read(String),
    /// The file was empty or its header did not decode.
    MalformedLog,
}

// ------------------------------------------------------------------
// The Log handle + file lifecycle
// ------------------------------------------------------------------

/// An open Session Log: its path and the append handle.
#[derive(Debug)]
pub struct Log {
    pub path: String,
    io: std::fs::File,
}

impl Log {
    /// Creates `<session_dir>/<utc-stamp>-<unique>.jsonl` and writes the header
    /// line. Fresh file (exclusive create).
    pub fn open(session: &Session) -> std::io::Result<Log> {
        std::fs::create_dir_all(&session.session_dir)?;

        let stamp = utc_stamp();
        let unique = next_unique();
        let path = std::path::Path::new(&session.session_dir)
            .join(format!("{stamp}-{unique}.jsonl"))
            .to_string_lossy()
            .into_owned();

        let mut io = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;

        let header = header(session);
        writeln!(io, "{}", serde_json::to_string(&header).unwrap())?;
        io.flush()?;

        Ok(Log { path, io })
    }

    /// Appends one entry as one line, flushed through immediately.
    pub fn append(&mut self, entry: Entry) -> &mut Self {
        let line = serde_json::to_string(&entry.to_json()).unwrap();
        let _ = writeln!(self.io, "{line}");
        let _ = self.io.flush();
        self
    }
}

fn next_unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn utc_stamp() -> String {
    use time::format_description::FormatItem;
    use time::macros::format_description;
    const FMT: &[FormatItem<'_>] = format_description!("[year][month][day]-[hour][minute][second]");
    time::OffsetDateTime::now_utc()
        .format(FMT)
        .unwrap_or_else(|_| "00000000-000000".into())
}

/// The header line's fixed facts - the DURABLE subset of the Session a Resume
/// must reconcile: `root` (must match, else `RootMismatch`) plus `model`,
/// `context_budget`, and `turn_limit` (drift-checked in [`drift`]; the resuming
/// Session's value wins and the difference is reported). Everything else the
/// Session resolves at launch is deliberately NOT persisted: Setpoints such as
/// `eviction_slack` and `compaction_keep` are user-tunable (ADR-0031) and simply
/// yield to the resuming Session's values, so they are neither logged nor
/// drift-checked. When adding a Session field, decide which it is - a durable
/// fact (add it here AND to [`drift`]) or a Setpoint (omit it; it takes the
/// resuming Session's value on Resume).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Header {
    #[serde(rename = "type")]
    kind: String,
    version: u32,
    root: String,
    model: String,
    context_budget: u64,
    #[serde(rename = "turn_limit")]
    run_limit: u64,
}

fn header(session: &Session) -> Header {
    Header {
        kind: "session".into(),
        version: 1,
        root: session.root.clone(),
        model: session.model.scoped_id(),
        // The launch Model's derived budget (ADR-0037): the budget is no
        // longer a fixed fact, so the header records the launch figure and
        // drift is judged against the resuming Session's launch figure.
        context_budget: session.context_budget_for(&session.model),
        run_limit: session.run_limit,
    }
}

/// The newest log file in `dir`, by the sortable timestamp filename. `None`
/// when the dir has no `.jsonl` files or cannot be read.
pub fn latest(dir: &str) -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".jsonl"))
        .collect();
    if names.is_empty() {
        return None;
    }
    names.sort();
    Some(
        std::path::Path::new(dir)
            .join(names.last().unwrap())
            .to_string_lossy()
            .into_owned(),
    )
}

/// One row of the `--resume` picker: a Session Log file, its filename-derived
/// timestamp (human-trimmed), and a label taken from the first user prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub path: String,
    pub stamp: String,
    pub label: String,
}

/// Every Session Log in `dir`, NEWEST first - keyed by the sortable stamp
/// filename, the same source [`latest`] sorts on. Unreadable or foreign files
/// (a torn header included) are skipped, never a panic: the picker shows what
/// it can and stays quiet about the rest.
pub fn list(dir: &str) -> Vec<SessionEntry> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".jsonl"))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names.reverse();
    names
        .into_iter()
        .filter_map(|name| list_entry(dir, &name))
        .collect()
}

// One picker row, or `None` for a file that cannot be read or whose header is
// not a Session Log header (foreign/torn - the same decode tolerance resume
// takes, minus the error reporting: the picker just skips it).
fn list_entry(dir: &str, name: &str) -> Option<SessionEntry> {
    let path = std::path::Path::new(dir)
        .join(name)
        .to_string_lossy()
        .into_owned();
    let content = std::fs::read_to_string(&path).ok()?;
    let mut lines = content.lines().filter(|l| !l.is_empty());
    let header = decode_line(lines.next()?)?;
    if header.get("type").and_then(|v| v.as_str()) != Some("session") {
        return None;
    }
    Some(SessionEntry {
        path,
        stamp: human_stamp(name),
        label: first_user_label(lines),
    })
}

// The first user_text entry's text as a one-line label; "(empty session)" when
// the log holds none. A torn line stops the scan, like resume's fold.
fn first_user_label<'a>(lines: impl Iterator<Item = &'a str>) -> String {
    for line in lines {
        match decode_line(line).and_then(|v| Entry::from_json(&v)) {
            Some(Entry::UserText(text)) => return label_from(&text),
            Some(_) => continue,
            None => break,
        }
    }
    "(empty session)".to_string()
}

/// How many label chars the picker shows before truncating with `…`.
const LABEL_CHARS: usize = 60;

fn label_from(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    if first.chars().count() > LABEL_CHARS {
        let mut out: String = first.chars().take(LABEL_CHARS).collect();
        out.push('…');
        out
    } else {
        first.to_string()
    }
}

// `20260711-140205-3.jsonl` → `2026-07-11 14:02` (the [`utc_stamp`] shape,
// seconds and the uniquifier dropped). A name that doesn't carry that shape
// falls back to its bare stem.
fn human_stamp(name: &str) -> String {
    let stem = name.strip_suffix(".jsonl").unwrap_or(name);
    let raw = stem.as_bytes();
    let stamped = raw.len() >= 15
        && raw[8] == b'-'
        && raw[..15]
            .iter()
            .enumerate()
            .all(|(i, b)| i == 8 || b.is_ascii_digit());
    if !stamped {
        return stem.to_string();
    }
    format!(
        "{}-{}-{} {}:{}",
        &stem[0..4],
        &stem[4..6],
        &stem[6..8],
        &stem[9..11],
        &stem[11..13]
    )
}

/// The last Plan logged in a Session Log file, or `None` when none was. Reads
/// under the fold's torn-line tolerance: a torn line stops the scan (like
/// [`resume`]), so this never returns a Plan the resumed Conversation would not
/// see - it yields the last Plan logged BEFORE the first tear (or `None`). The
/// Plan is a convenience, never load-bearing for Resume's correctness.
pub fn plan(path: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut last: Option<String> = None;
    // Skip the header line: it is not an entry.
    for line in content.lines().filter(|l| !l.is_empty()).skip(1) {
        match decode_line(line).and_then(|v| Entry::from_json(&v)) {
            Some(Entry::Plan(text)) => last = Some(text),
            Some(_) => {}
            None => break,
        }
    }
    last
}

/// Broken-state Recovery Runs the logged Session consumed serving its CURRENT
/// user request: `recovery` entries whose `reason` is NOT `OpenPlan`, since the
/// last `user_text` (a genuine or rolled-over prompt resets the count exactly
/// as the live Agent's does on a submit). Restores the `repair_limit` bound on
/// Resume so a resumed Session cannot re-trigger recoveries unboundedly. A torn
/// line stops the scan, like the fold.
pub fn recoveries_used(path: &str) -> u64 {
    recovery_counts(path).0
}

/// Open-Plan continuations the logged Session consumed serving its CURRENT user
/// request (ADR-0043): `recovery` entries whose `reason` IS `OpenPlan`, since
/// the last `user_text`. Restores the `advance_limit` bound on Resume,
/// symmetrically to [`recoveries_used`].
pub fn advances_used(path: &str) -> u64 {
    recovery_counts(path).1
}

// The two per-request recovery budgets a Resume restores, from one scan:
// `(recoveries, advances)` since the last `user_text`, split by the entry's
// reason. A torn line stops the scan, like the fold; a foreign/missing reason
// decoded as `UnverifiedWrites` counts as a broken-state recovery.
fn recovery_counts(path: &str) -> (u64, u64) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return (0, 0);
    };
    let mut recoveries = 0;
    let mut advances = 0;
    // Skip the header line: it is not an entry.
    for line in content.lines().filter(|l| !l.is_empty()).skip(1) {
        match decode_line(line).and_then(|v| Entry::from_json(&v)) {
            Some(Entry::UserText(_)) => {
                recoveries = 0;
                advances = 0;
            }
            Some(Entry::Recovery { reason, .. }) => {
                if reason == ReopenReason::OpenPlan {
                    advances += 1;
                } else {
                    recoveries += 1;
                }
            }
            Some(_) => {}
            None => break,
        }
    }
    (recoveries, advances)
}

// ------------------------------------------------------------------
// Resume: fold a log file into Conversation messages
// ------------------------------------------------------------------

/// A resumed log, folded once. Carries the Conversation `messages` and header
/// `drift` (the Transcript-facing facts), plus two governance facts derived from
/// the SAME entry stream the fold walked: the last logged `plan` and the
/// `recoveries` consumed by the logged request. Because all four come from one
/// pass that stops at the first torn line, `plan`/`recoveries` here match
/// [`plan`]/[`recoveries_used`] on the same file exactly, and stay consistent
/// with the Conversation the fold produced. Private: the Agent unpacks it and
/// keeps `plan`/`recoveries` outside the Transcript-facing `ResumeInfo`.
pub(crate) struct Resumed {
    pub(crate) messages: Vec<Message>,
    pub(crate) drift: Vec<Drift>,
    pub(crate) plan: Option<String>,
    pub(crate) recoveries: u64,
    pub(crate) advances: u64,
}

/// Folds a log file into the messages of a Conversation.
///
/// Returns `(messages, drift)` where `drift` lists header facts that differ
/// from the resuming Session's - the new Session's facts win.
/// `Err(RootMismatch)` when the Project Root differs.
pub fn resume(path: &str, session: &Session) -> Result<(Vec<Message>, Vec<Drift>), ResumeError> {
    let r = resume_governed(path, session)?;
    Ok((r.messages, r.drift))
}

/// Like [`resume`], but also returns the governance facts ([`Resumed::plan`],
/// [`Resumed::recoveries`]) computed in the SAME single fold - so the Agent
/// resumes the Plan and the recovery bound without re-reading the file. The two
/// facts are derived from `entries`, which the loop below stops populating at
/// the first torn line, so they inherit the fold's tolerance and match the
/// standalone [`plan`]/[`recoveries_used`] queries line for line.
pub(crate) fn resume_governed(path: &str, session: &Session) -> Result<Resumed, ResumeError> {
    let content = std::fs::read_to_string(path).map_err(|e| ResumeError::Read(e.to_string()))?;

    let mut lines = content.lines().filter(|l| !l.is_empty());
    let header_line = lines.next().ok_or(ResumeError::MalformedLog)?;
    let header: serde_json::Value = decode_line(header_line).ok_or(ResumeError::MalformedLog)?;

    check_root(&header, session)?;

    // A torn last line (crash mid-write) decodes as an error; stop at the first
    // undecodable line and drop everything after.
    let mut entries: Vec<Entry> = Vec::new();
    for line in lines {
        match decode_line(line).and_then(|v| Entry::from_json(&v)) {
            Some(entry) => entries.push(entry),
            None => break,
        }
    }

    let (plan, recoveries, advances) = governance_counts(&entries);

    Ok(Resumed {
        messages: fold(&entries),
        drift: drift(&header, session),
        plan,
        recoveries,
        advances,
    })
}

// Derive the governance facts from the same tolerated entry stream,
// mirroring `plan`/`recoveries_used`/`advances_used` exactly: `plan` is the
// last Plan seen; `recoveries` counts broken-state Recovery entries and
// `advances` counts Open-Plan ones since the last UserText (reset on a
// genuine or rolled-over prompt, as the live Agent resets on submit).
fn governance_counts(entries: &[Entry]) -> (Option<String>, u64, u64) {
    let mut plan: Option<String> = None;
    let mut recoveries: u64 = 0;
    let mut advances: u64 = 0;
    for entry in entries {
        match entry {
            Entry::Plan(text) => plan = Some(text.clone()),
            Entry::UserText(_) => {
                recoveries = 0;
                advances = 0;
            }
            Entry::Recovery { reason, .. } if *reason == ReopenReason::OpenPlan => advances += 1,
            Entry::Recovery { .. } => recoveries += 1,
            _ => {}
        }
    }
    (plan, recoveries, advances)
}

fn check_root(header: &serde_json::Value, session: &Session) -> Result<(), ResumeError> {
    if header.get("root").and_then(|r| r.as_str()) == Some(session.root.as_str()) {
        Ok(())
    } else {
        Err(ResumeError::RootMismatch)
    }
}

fn drift(header: &serde_json::Value, session: &Session) -> Vec<Drift> {
    let mut out = Vec::new();

    let logged_model = header.get("model").and_then(|v| v.as_str()).unwrap_or("");
    if logged_model != session.model.scoped_id() {
        out.push(Drift {
            key: "model",
            logged: logged_model.to_string(),
            current: session.model.scoped_id(),
        });
    }

    let current_budget = session.context_budget_for(&session.model);
    let logged_budget = header.get("context_budget").and_then(|v| v.as_u64());
    if logged_budget != Some(current_budget) {
        out.push(Drift {
            key: "context_budget",
            logged: opt_num(logged_budget),
            current: current_budget.to_string(),
        });
    }

    let logged_limit = header.get("turn_limit").and_then(|v| v.as_u64());
    if logged_limit != Some(session.run_limit) {
        out.push(Drift {
            key: "turn_limit",
            logged: opt_num(logged_limit),
            current: session.run_limit.to_string(),
        });
    }

    out
}

fn opt_num(n: Option<u64>) -> String {
    n.map(|v| v.to_string()).unwrap_or_default()
}

fn decode_line(line: &str) -> Option<serde_json::Value> {
    match serde_json::from_str::<serde_json::Value>(line) {
        Ok(v) if v.is_object() => Some(v),
        _ => None,
    }
}

// ------------------------------------------------------------------
// The fold: entries -> Conversation messages
// ------------------------------------------------------------------

// The open tool batch: the last assistant_blocks (with the Provenance it was
// logged under) and the results/steering that followed it, pending until the
// batch closes - mirroring how the Loop builds the live Conversation.
struct Batch {
    blocks: Vec<ContentBlock>,
    provenance: Option<Provenance>,
    results: Vec<ContentBlock>,
    steering: Vec<String>,
}

fn fold(entries: &[Entry]) -> Vec<Message> {
    let mut messages: Vec<Message> = Vec::new();
    let mut batch: Option<Batch> = None;

    for entry in entries {
        fold_entry(entry, &mut messages, &mut batch);
    }

    // A log whose last entry is a settlement (or a Resume seed, written only at
    // open) is complete; anything else died mid-Run and settles as failed.
    match entries.last() {
        Some(Entry::Settled { .. }) | Some(Entry::Message(_)) => messages,
        _ => {
            if messages.is_empty() && batch.is_none() {
                Vec::new()
            } else {
                flush(&mut messages, batch);
                close_with(&mut messages, voice::run_failed_marker());
                messages
            }
        }
    }
}

fn fold_entry(entry: &Entry, messages: &mut Vec<Message>, batch: &mut Option<Batch>) {
    match entry {
        Entry::UserText(text) => {
            flush(messages, batch.take());
            messages.push(user_message(vec![text_block(text)]));
        }
        // A Nudge is user-role text. With an open batch it folds INTO the
        // tool-results user message via the steering carrier (the Explore
        // Nudge rode that message live); standing alone (Verify Nudge) with no
        // results, flush emits `assistant + user([nudge])` identically.
        Entry::Nudge(text) => match batch {
            Some(b) => b.steering.push(text.clone()),
            None => messages.push(user_message(vec![text_block(text)])),
        },
        // A rider rode the trailing tool-results user message live, after
        // every result of its Pass - the open batch is complete, so it can
        // flush before the rider re-injects through the same merge seam
        // `apply_tail` used ([`conversation::merge_user_text`]; the Anchor's
        // `inject_anchor` IS that seam). The tag never varies the shape.
        Entry::Rider { text, .. } => {
            flush(messages, batch.take());
            conversation::merge_user_text(messages, text.clone());
        }
        Entry::Message(message) => {
            flush(messages, batch.take());
            messages.push(message.clone());
        }
        // The Plan is held outside the Conversation, so it never becomes a
        // message and never disturbs an open tool batch.
        Entry::Plan(_) => {}
        // A malformed-tool-call re-draw (ADR-0030) is silent to the model's
        // Conversation: the failed draw produced nothing to keep, so the entry
        // is forensic only and never becomes a message or disturbs an open
        // batch - the re-issued request lands as the next assistant_blocks.
        Entry::Retry { .. } => {}
        Entry::AssistantBlocks { blocks, provenance } => {
            flush(messages, batch.take());
            *batch = Some(Batch {
                blocks: blocks.clone(),
                provenance: provenance.clone(),
                results: Vec::new(),
                steering: Vec::new(),
            });
        }
        Entry::ToolResult(block) => {
            // A stray tool_result with no open batch: corrupt tail; ignore.
            if let Some(b) = batch {
                b.results.push(block.clone());
            }
        }
        Entry::Steering(text) => {
            if let Some(b) = batch {
                b.steering.push(text.clone());
            }
        }
        Entry::Compacted {
            summary,
            file_ops,
            original_task,
            ..
        } => {
            // Compaction replaces everything folded before this point with the
            // reconstructed summary; reappend the harness-owned mechanical
            // facts so the message matches the live one.
            let composed = compose_summary(summary, original_task.as_deref(), file_ops);
            messages.clear();
            *batch = None;
            messages.push(user_message(vec![voice::summary_block(&composed)]));
        }
        // A Handoff retired the Conversation and seeded a fresh one: like
        // Compacted, everything folded before this point is discarded and the
        // seed message is recomposed byte-identically to the live one. The
        // recovery prompt follows as its own `recovery` entry.
        Entry::Handoff {
            summary,
            file_ops,
            original_task,
            verification,
        } => {
            let composed = compose_handoff(
                summary.as_deref(),
                original_task.as_deref(),
                file_ops,
                verification.as_deref(),
            );
            messages.clear();
            *batch = None;
            messages.push(user_message(vec![voice::summary_block(&composed)]));
        }
        // The recovery prompt entered the Conversation on the same seam a
        // rider crosses: merged into a trailing user message (the Handoff's
        // seed) or standing as a fresh one (a Continuation, after the
        // run-limit marker).
        Entry::Recovery { text, .. } => {
            flush(messages, batch.take());
            conversation::merge_user_text(messages, text.clone());
        }
        Entry::Settled {
            outcome,
            stop_reason,
            ..
        } => {
            if let Some(open) = batch.take() {
                let stop = settle_stop(*outcome, *stop_reason);
                flush_batch(messages, open, stop);
            }
            close_settled(messages, *outcome, *stop_reason);
        }
    }
}

// The reconstructed compaction summary: the model's narrative plus the
// harness-owned mechanical facts. compose_summary lives here for now; the
// Compaction module (a later phase) reuses this exact composition.
pub fn compose_summary(narrative: &str, original_task: Option<&str>, file_ops: &FileOps) -> String {
    format!(
        "{narrative}\n{}",
        voice::compaction_facts(original_task, file_ops)
    )
}

/// The Handoff seed (CONTEXT.md: Handoff): the compaction composition plus the
/// final verification result verbatim. A `None` narrative is the degraded
/// mechanical skeleton (the summarization call failed - bounded downside, the
/// recovery still happens). One author for the live seeding
/// ([`crate::compaction::Compaction::seed_handoff`]) and the fold's
/// reconstruction, so Resume rebuilds the same bytes.
pub fn compose_handoff(
    narrative: Option<&str>,
    original_task: Option<&str>,
    file_ops: &FileOps,
    verification: Option<&str>,
) -> String {
    format!(
        "{}{}",
        compose_summary(
            narrative.unwrap_or(voice::handoff_no_narrative()),
            original_task,
            file_ops
        ),
        voice::handoff_verification(verification)
    )
}

fn settle_stop(outcome: Settled, stop_reason: StopReason) -> StopReason {
    match outcome {
        Settled::Completed => stop_reason,
        _ => StopReason::Error, // stand-in for baud's `:failed` batch-close marker path
    }
}

fn flush(messages: &mut Vec<Message>, batch: Option<Batch>) {
    if let Some(batch) = batch {
        flush_batch(messages, batch, StopReason::EndTurn);
    }
}

// Close an open batch the way the Loop would have: keep tool_use blocks a
// result answered (ADR-0009 error answers included), drop the rest (ADR-0004),
// and never leave an empty assistant message.
fn flush_batch(messages: &mut Vec<Message>, batch: Batch, stop: StopReason) {
    let answered: std::collections::HashSet<&str> = batch
        .results
        .iter()
        .filter_map(|r| match r {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect();

    let mut kept: Vec<ContentBlock> = batch
        .blocks
        .iter()
        .filter(|b| match b {
            ContentBlock::ToolUse { id, .. } => answered.contains(id.as_str()),
            _ => true,
        })
        .cloned()
        .collect();

    if kept.is_empty() {
        let marker = if stop == StopReason::MaxTokens {
            voice::truncation_marker()
        } else {
            voice::empty_response_marker()
        };
        kept = vec![text_block(marker)];
    }

    // The batch re-enters under the Provenance it was logged with, so a
    // resumed history normalizes at request-shaping exactly as the live one
    // would (ADR-0037).
    messages.push(Message {
        role: Role::Assistant,
        content: kept,
        provenance: batch.provenance,
    });

    let mut content = batch.results;
    content.extend(batch.steering.iter().map(|s| text_block(s)));
    if !content.is_empty() {
        messages.push(user_message(content));
    }
}

// A settled Run that ended on a user-role message (Run Limit, stop hook)
// closed with a marker live; restore it so roles keep alternating.
fn close_settled(messages: &mut Vec<Message>, outcome: Settled, stop_reason: StopReason) {
    match outcome {
        Settled::Completed => {
            if matches!(messages.last(), Some(m) if m.role == Role::User) {
                let marker = if stop_reason == StopReason::RunLimit
                    || stop_reason == StopReason::RunLimitStuck
                {
                    voice::run_limit_marker()
                } else {
                    voice::run_stopped_marker()
                };
                messages.push(Message::assistant(vec![text_block(marker)]));
            }
        }
        Settled::Failed => close_with(messages, voice::run_failed_marker()),
        Settled::Cancelled => close_with(messages, voice::run_cancelled_marker()),
    }
}

// Mirror the live fail path: the marker rides the trailing assistant message
// (the Loop appends kept text and marker as ONE message); a user-role tail gets
// a fresh assistant message, as Settlement does.
fn close_with(messages: &mut Vec<Message>, marker: &str) {
    let marker_block = text_block(marker);
    match messages.last_mut() {
        Some(last) if last.role == Role::Assistant => {
            if last.content.last() != Some(&marker_block) {
                last.content.push(marker_block);
            }
        }
        _ => messages.push(Message::assistant(vec![marker_block])),
    }
}

fn user_message(content: Vec<ContentBlock>) -> Message {
    Message::user(content)
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text {
        text: text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Session, SessionConfig, SessionOpts};
    use serde_json::json;
    use tempfile::TempDir;

    // A Session rooted at `dir`, session_dir under it, no-env config path.
    fn session_in(dir: &std::path::Path) -> Session {
        session_with(dir, None, None)
    }

    fn session_with(
        dir: &std::path::Path,
        context_budget: Option<u64>,
        run_limit: Option<u64>,
    ) -> Session {
        let root = dir.to_string_lossy().into_owned();
        let session_dir = dir.join("sessions").to_string_lossy().into_owned();
        Session::build(
            SessionOpts {
                root: Some(root),
                session_dir: Some(session_dir),
                context_budget,
                run_limit,
                ..Default::default()
            },
            &SessionConfig::test_defaults(),
        )
        .unwrap()
    }

    fn tool_use(id: &str, name: &str, input: serde_json::Value) -> ContentBlock {
        ContentBlock::tool_use(id, name, input)
    }

    fn tool_result(id: &str, content: &str) -> ContentBlock {
        ContentBlock::tool_result(id, content, false)
    }

    fn tool_result_err(id: &str, content: &str, is_error: bool) -> ContentBlock {
        ContentBlock::tool_result(id, content, is_error)
    }

    fn text(t: &str) -> ContentBlock {
        ContentBlock::text(t)
    }

    // ---- round trip ----

    #[test]
    fn a_settled_run_folds_back_into_the_exact_conversation_shape() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        log.append(Entry::UserText("list the files".into()));
        log.append(Entry::assistant_blocks(vec![
            text("Let me look."),
            tool_use("t1", "list_files", json!({"path": "."})),
        ]));
        log.append(Entry::ToolResult(tool_result("t1", "a.txt\nb.txt")));
        log.append(Entry::Steering("also check the README".into()));
        log.append(Entry::assistant_blocks(vec![text("Two files.")]));
        log.append(Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::EndTurn,
            reason: None,
        });

        let (messages, drift) = resume(&log.path, &session).unwrap();
        assert_eq!(drift, Vec::new());

        assert_eq!(
            messages,
            vec![
                user_message(vec![text("list the files")]),
                Message::assistant(vec![
                    text("Let me look."),
                    tool_use("t1", "list_files", json!({"path": "."})),
                ]),
                user_message(vec![
                    tool_result("t1", "a.txt\nb.txt"),
                    text("also check the README"),
                ]),
                Message::assistant(vec![text("Two files.")]),
            ]
        );
    }

    #[test]
    fn a_mixed_batch_keeps_answered_tool_calls_and_drops_unanswered_ones() {
        // ADR-0009 keeps a tool_use whose result landed; ADR-0004 drops one that
        // never answered. A batch with both must keep t1 (+ its result) and drop
        // t2 entirely.
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        log.append(Entry::UserText("go".into()));
        log.append(Entry::assistant_blocks(vec![
            tool_use("t1", "read_file", json!({"path": "a.rs"})),
            tool_use("t2", "read_file", json!({"path": "b.rs"})),
        ]));
        log.append(Entry::ToolResult(tool_result("t1", "ok")));
        log.append(Entry::assistant_blocks(vec![text("done")]));
        log.append(Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::EndTurn,
            reason: None,
        });

        let (messages, _) = resume(&log.path, &session).unwrap();

        assert_eq!(
            messages,
            vec![
                user_message(vec![text("go")]),
                // t2 is gone; only the answered t1 survives.
                Message::assistant(vec![tool_use("t1", "read_file", json!({"path": "a.rs"}))]),
                user_message(vec![tool_result("t1", "ok")]),
                Message::assistant(vec![text("done")]),
            ]
        );
    }

    #[test]
    fn an_all_unanswered_batch_collapses_to_the_empty_response_marker() {
        // Every tool_use dropped (ADR-0004) leaves no assistant content, so the
        // batch close emits the empty-response marker instead of an empty message.
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        log.append(Entry::UserText("go".into()));
        log.append(Entry::assistant_blocks(vec![
            tool_use("t1", "read_file", json!({"path": "a.rs"})),
            tool_use("t2", "read_file", json!({"path": "b.rs"})),
        ]));
        log.append(Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::EndTurn,
            reason: None,
        });

        let (messages, _) = resume(&log.path, &session).unwrap();

        assert_eq!(
            messages,
            vec![
                user_message(vec![text("go")]),
                Message::assistant(vec![text(voice::empty_response_marker())]),
            ]
        );
    }

    // ---- Plan survives Resume ----

    #[test]
    fn plan_restores_the_last_logged_plan_which_never_enters_the_folded_messages() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        log.append(Entry::UserText("do the thing".into()));
        log.append(Entry::Plan("Goal: A. 1. read [x] 2. edit [ ]".into()));
        log.append(Entry::assistant_blocks(vec![text("planned")]));
        log.append(Entry::Plan(
            "Goal: A. 1. read [x] 2. edit [x] 3. verify [ ]".into(),
        ));
        log.append(Entry::assistant_blocks(vec![text("done step 2")]));
        log.append(Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::EndTurn,
            reason: None,
        });

        assert_eq!(
            plan(&log.path),
            Some("Goal: A. 1. read [x] 2. edit [x] 3. verify [ ]".to_string())
        );

        let (messages, _) = resume(&log.path, &session).unwrap();
        assert!(!messages.iter().any(|m| m.content.iter().any(|b| matches!(
            b,
            ContentBlock::Text { text } if text.contains("Goal: A.")
        ))));
    }

    #[test]
    fn a_log_with_no_plan_entry_restores_a_nil_plan() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        log.append(Entry::UserText("hi".into()));
        log.append(Entry::assistant_blocks(vec![text("hello")]));
        log.append(Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::EndTurn,
            reason: None,
        });

        assert_eq!(plan(&log.path), None);
    }

    // ---- the fold's close rules ----

    #[test]
    fn an_adr_0009_truncated_batch_folds_back_intact() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        log.append(Entry::UserText("go".into()));
        log.append(Entry::assistant_blocks(vec![tool_use(
            "t1",
            "write_file",
            json!({"path": "a"}),
        )]));
        log.append(Entry::ToolResult(tool_result_err(
            "t1",
            "[response was cut...]",
            true,
        )));
        log.append(Entry::assistant_blocks(vec![text("re-issued")]));
        log.append(Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::EndTurn,
            reason: None,
        });

        let (messages, _) = resume(&log.path, &session).unwrap();

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].role, Role::Assistant);
        assert!(matches!(&messages[1].content[0], ContentBlock::ToolUse { id, .. } if id == "t1"));
        assert_eq!(messages[2].role, Role::User);
        assert!(matches!(
            &messages[2].content[0],
            ContentBlock::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == "t1"
        ));
        assert_eq!(messages[3].role, Role::Assistant);
        assert!(
            matches!(&messages[3].content[0], ContentBlock::Text { text } if text == "re-issued")
        );
    }

    #[test]
    fn a_log_ending_mid_run_settles_as_failed_dangling_tool_use_dropped_marker_appended() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        log.append(Entry::UserText("go".into()));
        log.append(Entry::assistant_blocks(vec![
            text("thinking..."),
            tool_use("t1", "grep", json!({})),
        ]));
        // No tool_result, no settled: the app died mid-batch.

        let (messages, _) = resume(&log.path, &session).unwrap();

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(
            messages[1].content,
            vec![text("thinking..."), text("[turn failed]")]
        );
    }

    #[test]
    fn a_run_limit_settlement_restores_the_closing_marker() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        log.append(Entry::UserText("go".into()));
        log.append(Entry::assistant_blocks(vec![tool_use(
            "t1",
            "grep",
            json!({}),
        )]));
        log.append(Entry::ToolResult(tool_result("t1", "hits")));
        log.append(Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::RunLimit,
            reason: None,
        });

        let (messages, _) = resume(&log.path, &session).unwrap();

        let last = messages.last().unwrap();
        assert_eq!(last.role, Role::Assistant);
        assert_eq!(
            last.content,
            vec![text("[turn limit reached - reply to continue]")]
        );
    }

    #[test]
    fn a_cancelled_settlement_closes_with_the_cancelled_marker() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        log.append(Entry::UserText("go".into()));
        log.append(Entry::assistant_blocks(vec![tool_use(
            "t1",
            "grep",
            json!({}),
        )]));
        log.append(Entry::ToolResult(tool_result("t1", "hits")));
        log.append(Entry::Settled {
            outcome: Settled::Cancelled,
            stop_reason: StopReason::Unknown,
            reason: None,
        });

        let (messages, _) = resume(&log.path, &session).unwrap();

        let last = messages.last().unwrap();
        assert_eq!(last.role, Role::Assistant);
        assert_eq!(last.content, vec![text("[turn cancelled by user]")]);
    }

    #[test]
    fn a_failed_settlement_carries_its_reason_string_forensically_the_fold_ignores_it() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        log.append(Entry::UserText("go".into()));
        log.append(Entry::assistant_blocks(vec![text("partial")]));
        log.append(Entry::Settled {
            outcome: Settled::Failed,
            stop_reason: StopReason::Error,
            reason: Some(r#"{:llm_error, "connection refused"}"#.into()),
        });

        let (messages, _) = resume(&log.path, &session).unwrap();

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0], user_message(vec![text("go")]));
        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(
            messages[1].content,
            vec![text("partial"), text("[turn failed]")]
        );
    }

    #[test]
    fn a_verify_nudge_entry_folds_as_a_user_message() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        log.append(Entry::UserText("write it".into()));
        log.append(Entry::assistant_blocks(vec![text("wrote it")]));
        log.append(Entry::Nudge(
            "[files changed but nothing verified - ...]".into(),
        ));
        log.append(Entry::assistant_blocks(vec![text("verified")]));
        log.append(Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::EndTurn,
            reason: None,
        });

        let (messages, _) = resume(&log.path, &session).unwrap();

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(messages[2].role, Role::User);
        assert!(
            matches!(&messages[2].content[0], ContentBlock::Text { text } if text.starts_with("[files changed"))
        );
        assert_eq!(messages[3].role, Role::Assistant);
    }

    #[test]
    fn an_explore_nudge_folds_into_the_tool_results_user_message_it_rode_live() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        log.append(Entry::UserText("evaluate this project".into()));
        log.append(Entry::assistant_blocks(vec![tool_use(
            "t1",
            "read_file",
            json!({"path": "a.txt"}),
        )]));
        log.append(Entry::ToolResult(tool_result("t1", "defmodule A")));
        log.append(Entry::Nudge(
            "[reading file after file - dispatch explore instead]".into(),
        ));
        log.append(Entry::assistant_blocks(vec![text("ok, exploring")]));
        log.append(Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::EndTurn,
            reason: None,
        });

        let (messages, _) = resume(&log.path, &session).unwrap();

        assert_eq!(messages.len(), 4);
        assert_eq!(
            messages[0],
            user_message(vec![text("evaluate this project")])
        );
        assert_eq!(messages[1].role, Role::Assistant);
        assert!(matches!(&messages[1].content[0], ContentBlock::ToolUse { id, .. } if id == "t1"));
        assert_eq!(messages[2].role, Role::User);
        assert!(matches!(
            &messages[2].content[0],
            ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t1"
        ));
        assert!(
            matches!(&messages[2].content[1], ContentBlock::Text { text } if text.starts_with("[reading file after file"))
        );
        assert_eq!(messages[3].role, Role::Assistant);
        assert!(
            matches!(&messages[3].content[0], ContentBlock::Text { text } if text == "ok, exploring")
        );
    }

    // ---- riders (the Anchor + the Endgame's tail prompts) ----

    #[test]
    fn riders_fold_into_the_tool_results_user_message_they_rode_live() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        log.append(Entry::UserText("go".into()));
        log.append(Entry::assistant_blocks(vec![tool_use(
            "t1",
            "list_files",
            json!({"path": "."}),
        )]));
        log.append(Entry::ToolResult(tool_result("t1", "a.txt")));
        log.append(Entry::Rider {
            tag: RiderTag::Anchor,
            text: "[anchor] the goal: go".into(),
        });
        log.append(Entry::Rider {
            tag: RiderTag::WrapUpWarning,
            text: "[2 passes remain - wrap up]".into(),
        });
        log.append(Entry::assistant_blocks(vec![text("wrapping")]));
        log.append(Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::EndTurn,
            reason: None,
        });

        let (messages, _) = resume(&log.path, &session).unwrap();

        assert_eq!(
            messages,
            vec![
                user_message(vec![text("go")]),
                Message::assistant(vec![tool_use("t1", "list_files", json!({"path": "."}))]),
                user_message(vec![
                    tool_result("t1", "a.txt"),
                    text("[anchor] the goal: go"),
                    text("[2 passes remain - wrap up]"),
                ]),
                Message::assistant(vec![text("wrapping")]),
            ]
        );
    }

    #[test]
    fn the_verification_and_final_pass_prompts_fold_on_the_same_seam() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        log.append(Entry::UserText("edit it".into()));
        log.append(Entry::assistant_blocks(vec![tool_use(
            "t1",
            "edit_file",
            json!({"path": "a.ex"}),
        )]));
        log.append(Entry::ToolResult(tool_result("t1", "edited")));
        log.append(Entry::Rider {
            tag: RiderTag::VerificationPass,
            text: "[verify your changes now]".into(),
        });
        log.append(Entry::assistant_blocks(vec![tool_use(
            "t2",
            "run_command",
            json!({"command": "mix test"}),
        )]));
        log.append(Entry::ToolResult(tool_result("t2", "0 failures")));
        log.append(Entry::Rider {
            tag: RiderTag::FinalPass,
            text: "[final pass - conclude]".into(),
        });
        log.append(Entry::assistant_blocks(vec![text("done, verified")]));
        log.append(Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::EndTurn,
            reason: None,
        });

        let (messages, _) = resume(&log.path, &session).unwrap();

        assert_eq!(
            messages,
            vec![
                user_message(vec![text("edit it")]),
                Message::assistant(vec![tool_use("t1", "edit_file", json!({"path": "a.ex"}))]),
                user_message(vec![
                    tool_result("t1", "edited"),
                    text("[verify your changes now]"),
                ]),
                Message::assistant(vec![tool_use(
                    "t2",
                    "run_command",
                    json!({"command": "mix test"}),
                )]),
                user_message(vec![
                    tool_result("t2", "0 failures"),
                    text("[final pass - conclude]"),
                ]),
                Message::assistant(vec![text("done, verified")]),
            ]
        );
    }

    #[test]
    fn every_rider_tag_survives_the_file_round_trip() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        log.append(Entry::UserText("go".into()));
        for (tag, text) in [
            (RiderTag::Anchor, "[a]"),
            (RiderTag::WrapUpWarning, "[w]"),
            (RiderTag::VerificationPass, "[v]"),
            (RiderTag::FinalPass, "[f]"),
        ] {
            log.append(Entry::Rider {
                tag,
                text: text.into(),
            });
        }

        // No open batch: each rider merges into the trailing user message -
        // the same role-alternation rule the live seam applies. The log ends
        // mid-Run, so the fold settles it as failed.
        let (messages, _) = resume(&log.path, &session).unwrap();
        assert_eq!(
            messages,
            vec![
                user_message(vec![
                    text("go"),
                    text("[a]"),
                    text("[w]"),
                    text("[v]"),
                    text("[f]"),
                ]),
                Message::assistant(vec![text("[turn failed]")]),
            ]
        );
    }

    // ---- crash modes ----

    #[test]
    fn a_torn_last_line_is_dropped() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        log.append(Entry::UserText("go".into()));
        log.append(Entry::assistant_blocks(vec![text("done")]));
        log.append(Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::EndTurn,
            reason: None,
        });

        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&log.path)
            .unwrap();
        f.write_all(br#"{"e": "user_text", "tex"#).unwrap();

        let (messages, _) = resume(&log.path, &session).unwrap();
        assert_eq!(messages.len(), 2);
    }

    // ---- resume rules ----

    #[test]
    fn a_different_project_root_refuses_to_resume() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();
        log.append(Entry::UserText("go".into()));

        let other_root = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&other_root).unwrap();
        let other = Session::build(
            SessionOpts {
                root: Some(other_root.to_string_lossy().into_owned()),
                session_dir: Some(session.session_dir.clone()),
                ..Default::default()
            },
            &SessionConfig::test_defaults(),
        )
        .unwrap();

        assert_eq!(resume(&log.path, &other), Err(ResumeError::RootMismatch));
    }

    #[test]
    fn every_other_fact_yields_reported_as_drift() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();
        log.append(Entry::UserText("go".into()));

        // A budget cap BELOW the model's window, so the derived launch budget
        // actually changes (a cap above the window is a no-op, ADR-0037).
        let logged_budget = session.context_budget_for(&session.model);
        let changed = session_with(
            tmp.path(),
            Some(logged_budget / 2),
            Some(session.run_limit + 5),
        );

        let (_messages, drift) = resume(&log.path, &changed).unwrap();

        assert!(drift.contains(&Drift {
            key: "context_budget",
            logged: logged_budget.to_string(),
            current: changed.context_budget_for(&changed.model).to_string(),
        }));
        assert!(drift.contains(&Drift {
            key: "turn_limit",
            logged: session.run_limit.to_string(),
            current: changed.run_limit.to_string(),
        }));
    }

    #[test]
    fn a_setpoint_like_eviction_slack_yields_on_resume_and_never_drifts() {
        // eviction_slack is a Setpoint (ADR-0031), not a durable header fact: it
        // is never persisted, so a resuming Session with a different value
        // reports NO drift for it and simply keeps its own value.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_string_lossy().into_owned();
        let session_dir = tmp.path().join("sessions").to_string_lossy().into_owned();

        let build = |slack: f64| {
            Session::build(
                SessionOpts {
                    root: Some(root.clone()),
                    session_dir: Some(session_dir.clone()),
                    eviction_slack: Some(slack),
                    ..Default::default()
                },
                &SessionConfig::test_defaults(),
            )
            .unwrap()
        };

        let logged = build(0.15);
        let mut log = Log::open(&logged).unwrap();
        log.append(Entry::UserText("go".into()));

        let resuming = build(0.25);
        let (_messages, drift) = resume(&log.path, &resuming).unwrap();

        assert!(!drift.iter().any(|d| d.key == "eviction_slack"));
        // The resuming Session keeps its own Setpoint; the logged 0.15 is gone.
        assert_eq!(resuming.eviction_slack, 0.25);
    }

    #[test]
    fn a_compaction_fold_discards_raw_entries_before_the_compacted_marker() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        log.append(Entry::UserText("turn 1".into()));
        log.append(Entry::assistant_blocks(vec![text("old response")]));
        log.append(Entry::UserText("turn 2".into()));
        log.append(Entry::assistant_blocks(vec![text("compacted response")]));
        log.append(Entry::ToolResult(tool_result("t1", "compacted result")));
        log.append(Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::EndTurn,
            reason: None,
        });
        log.append(Entry::Compacted {
            summary: "Summary of old turns".into(),
            skip_count: 5,
            tokens_before: 100,
            file_ops: FileOps::default(),
            original_task: Some("the original task".into()),
        });
        log.append(Entry::UserText("turn 3".into()));
        log.append(Entry::assistant_blocks(vec![text("new response")]));
        log.append(Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::EndTurn,
            reason: None,
        });

        let (messages, drift) = resume(&log.path, &session).unwrap();
        assert_eq!(drift, Vec::new());

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, Role::User);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text.contains("Summary of old turns"))
        );
        assert_eq!(messages[1].role, Role::User);
        assert_eq!(messages[2].role, Role::Assistant);
    }

    #[test]
    fn a_compaction_fold_reconstructs_the_mechanical_facts_task_and_file_ops() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        log.append(Entry::UserText("turn 1".into()));
        log.append(Entry::assistant_blocks(vec![text("old response")]));
        log.append(Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::EndTurn,
            reason: None,
        });
        log.append(Entry::Compacted {
            summary: "narrative from the model".into(),
            skip_count: 3,
            tokens_before: 100,
            file_ops: FileOps {
                read_files: vec!["lib/a.ex".into()],
                modified_files: vec!["lib/b.ex".into()],
            },
            original_task: Some("verbatim original task".into()),
        });
        log.append(Entry::UserText("turn 2".into()));
        log.append(Entry::assistant_blocks(vec![text("new response")]));
        log.append(Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::EndTurn,
            reason: None,
        });

        let (messages, _) = resume(&log.path, &session).unwrap();

        let summary_text: String = messages[0]
            .content
            .iter()
            .map(|b| match b {
                ContentBlock::Text { text } => text.clone(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(summary_text.contains("narrative from the model"));
        assert!(summary_text.contains("verbatim original task"));
        assert!(summary_text.contains("lib/a.ex"));
        assert!(summary_text.contains("lib/b.ex"));
    }

    // The Compaction<->Resume fidelity invariant (ADR-0012's "byte-identical
    // summary message", ADR-0021's test-as-spec): a LIVE compaction and the
    // fold of its logged `Compacted` entry must reconstruct byte-identical
    // Conversation messages. Both sides compose the summary through the single
    // shared `compose_summary` helper - this crosses the seam BETWEEN them
    // (each side is tested alone above; nothing exercised the round trip). The
    // test drives the same builder ops into the Conversation and the Session
    // Log in lockstep - exactly the "append every event as it happens" contract
    // of ADR-0010 - runs a real `Compaction::run` over the head, then logs the
    // `Compacted` entry via the production path (`session_log_entry`, converted
    // the way `agent.rs` does) followed by the surviving tail. If someone
    // changed one composition path without the other (e.g. `apply_compaction`
    // prepended a marker the fold did not), the summary message would diverge
    // and this assertion would fail.
    #[tokio::test]
    async fn a_live_compaction_and_its_logged_fold_reconstruct_byte_identical_messages() {
        use crate::compaction::Compaction;
        use crate::conversation::{Conversation, ConversationOpts};
        use crate::llm::model::{Api, Model};
        use crate::llm::response::{Response, StopReason as LlmStop};
        use crate::test_support::{Entry as ScriptEntry, FakeLlm};

        // Arrange: a Conversation of several Runs (user text + assistant text),
        // fat enough that the Compaction Keep leaves a real head to summarize.
        // The same ops feed the Session Log so the log mirrors the live events.
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        let opts = ConversationOpts::new(2000, 500).eviction_slack(0.0);
        let mut conv = Conversation::new("You are Baud.", opts);
        for i in (1..=5).rev() {
            let body = format!("{}: turn {i}", "line ".repeat(50));
            conv.add_user_text(body.clone());
            conv.add_assistant_blocks(vec![ContentBlock::text(body.clone())]);
            log.append(Entry::UserText(body.clone()));
            log.append(Entry::assistant_blocks(vec![text(&body)]));
        }
        // The Run that triggers Compaction settles first, like the real path.
        log.append(Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::EndTurn,
            reason: None,
        });

        // Act (live): a real compaction cycle with a scripted narrative.
        let narrative = "## Goal\nPin the compaction seam\n## Progress\n### Done\n- traced";
        let fake = FakeLlm::script(vec![ScriptEntry::just(Response {
            content: vec![ContentBlock::text(narrative)],
            stop_reason: LlmStop::EndTurn,
            usage: crate::content::Usage::default(),
            error: None,
        })]);
        let model = Model::new("local", "test-model", Api::AnthropicMessages, 64_000, 4000);
        let before = conv.clone();
        let (compacted, new_state) = Compaction::new()
            .run(&conv, &fake, &model, None)
            .await
            .unwrap();
        // Sanity: compaction actually folded something into one summary message.
        assert!(compacted.messages.len() < before.messages.len());

        // Act (log): append the `Compacted` entry through the production path -
        // `session_log_entry` then the exact usize/Option conversion agent.rs
        // performs - followed by the surviving tail as it would have been logged
        // by the Runs that ran after the Compaction.
        let skip = Compaction::skip_count(&before, &compacted);
        let entry = new_state.session_log_entry(skip, 0);
        log.append(Entry::Compacted {
            summary: entry.summary.unwrap_or_default(),
            skip_count: entry.skip_count as u64,
            tokens_before: entry.tokens_before,
            file_ops: entry.file_ops,
            original_task: entry.original_task,
        });
        // The live-compacted tail (everything after the summary message) is
        // what later Runs appended; replay each surviving message as its entry.
        for msg in &compacted.messages[1..] {
            match msg.role {
                Role::User => {
                    let text = match &msg.content[0] {
                        ContentBlock::Text { text } => text.clone(),
                        other => panic!("unexpected tail user block: {other:?}"),
                    };
                    log.append(Entry::UserText(text));
                }
                Role::Assistant => {
                    log.append(Entry::assistant_blocks(msg.content.clone()));
                }
            }
        }
        log.append(Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::EndTurn,
            reason: None,
        });

        // Assert: the folded Conversation equals the live-compacted one, message
        // for message, byte for byte - the summary message at index 0 in
        // particular (where the two composition paths meet).
        let (folded, drift) = resume(&log.path, &session).unwrap();
        assert_eq!(drift, Vec::new());
        assert_eq!(folded, compacted.messages);
    }

    // ---- Recovery Run entries (Continuation + Handoff) ----

    #[test]
    fn a_continuation_recovery_prompt_folds_as_a_fresh_user_message_after_the_marker() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        log.append(Entry::UserText("fix the tests".into()));
        log.append(Entry::assistant_blocks(vec![tool_use(
            "t1",
            "edit_file",
            json!({"path": "a.ex"}),
        )]));
        log.append(Entry::ToolResult(tool_result("t1", "edited")));
        log.append(Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::RunLimit,
            reason: None,
        });
        log.append(Entry::Recovery {
            shape: RecoveryShape::Continuation,
            reason: ReopenReason::UnverifiedWrites,
            text: "[recovery prompt]".into(),
        });
        log.append(Entry::assistant_blocks(vec![text("recovered")]));
        log.append(Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::EndTurn,
            reason: None,
        });

        let (messages, _) = resume(&log.path, &session).unwrap();

        assert_eq!(
            messages,
            vec![
                user_message(vec![text("fix the tests")]),
                Message::assistant(vec![tool_use("t1", "edit_file", json!({"path": "a.ex"}))]),
                user_message(vec![tool_result("t1", "edited")]),
                Message::assistant(vec![text("[turn limit reached - reply to continue]")]),
                user_message(vec![text("[recovery prompt]")]),
                Message::assistant(vec![text("recovered")]),
            ]
        );
    }

    #[test]
    fn a_handoff_fold_discards_history_and_recomposes_the_seed_with_the_prompt_merged() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        log.append(Entry::UserText("fix the tests".into()));
        log.append(Entry::assistant_blocks(vec![text("failing attempt")]));
        log.append(Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::RunLimit,
            reason: None,
        });
        log.append(Entry::Handoff {
            summary: Some("narrative of the dying turn".into()),
            file_ops: FileOps {
                read_files: vec!["lib/a.ex".into()],
                modified_files: vec!["lib/b.ex".into()],
            },
            original_task: Some("fix the tests".into()),
            verification: Some("exit 1\n2 tests failed".into()),
        });
        log.append(Entry::Recovery {
            shape: RecoveryShape::Handoff,
            reason: ReopenReason::DanglingFailure,
            text: "[recovery prompt]".into(),
        });
        log.append(Entry::assistant_blocks(vec![text("recovered")]));
        log.append(Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::EndTurn,
            reason: None,
        });

        let (messages, _) = resume(&log.path, &session).unwrap();

        // Everything before the handoff is gone; the seed message is the
        // composed handoff plus the prompt merged onto the same message -
        // byte-identical to the live seeding path.
        assert_eq!(messages.len(), 2);
        let composed = compose_handoff(
            Some("narrative of the dying turn"),
            Some("fix the tests"),
            &FileOps {
                read_files: vec!["lib/a.ex".into()],
                modified_files: vec!["lib/b.ex".into()],
            },
            Some("exit 1\n2 tests failed"),
        );
        assert_eq!(
            messages[0],
            user_message(vec![
                voice::summary_block(&composed),
                text("[recovery prompt]"),
            ])
        );
        assert_eq!(messages[1], Message::assistant(vec![text("recovered")]));

        // The composed seed carries every mechanical fact.
        let seed = match &messages[0].content[0] {
            ContentBlock::Text { text } => text.clone(),
            other => panic!("expected text, got {other:?}"),
        };
        assert!(seed.contains("fix the tests"));
        assert!(seed.contains("narrative of the dying turn"));
        assert!(seed.contains("lib/a.ex"));
        assert!(seed.contains("lib/b.ex"));
        assert!(seed.contains("exit 1\n2 tests failed"));
    }

    #[test]
    fn a_degraded_handoff_folds_without_a_narrative() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        log.append(Entry::UserText("go".into()));
        log.append(Entry::Handoff {
            summary: None,
            file_ops: FileOps::default(),
            original_task: Some("go".into()),
            verification: None,
        });
        log.append(Entry::Recovery {
            shape: RecoveryShape::Handoff,
            reason: ReopenReason::DanglingFailure,
            text: "[recovery prompt]".into(),
        });
        log.append(Entry::assistant_blocks(vec![text("done")]));
        log.append(Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::EndTurn,
            reason: None,
        });

        let (messages, _) = resume(&log.path, &session).unwrap();
        let seed = match &messages[0].content[0] {
            ContentBlock::Text { text } => text.clone(),
            other => panic!("expected text, got {other:?}"),
        };
        assert!(seed.contains(voice::handoff_no_narrative()));
        assert!(seed.contains("go"));
        assert!(seed.contains("- none was run"));
    }

    #[test]
    fn recovery_entries_round_trip_the_file_with_their_shape_and_reason() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        // Every (shape, reason) combination the mechanic produces: broken-state
        // reasons on either shape, and the Open Plan always on Continuation.
        let cases = [
            (RecoveryShape::Handoff, ReopenReason::DanglingFailure, "[df]"),
            (
                RecoveryShape::Handoff,
                ReopenReason::UnverifiedWrites,
                "[uw]",
            ),
            (
                RecoveryShape::Continuation,
                ReopenReason::OpenPlan,
                "[op]",
            ),
        ];
        for (shape, reason, text) in cases {
            log.append(Entry::Recovery {
                shape,
                reason,
                text: text.into(),
            });
        }

        let content = std::fs::read_to_string(&log.path).unwrap();
        let entries: Vec<Entry> = content
            .lines()
            .skip(1)
            .filter_map(|l| decode_line(l).and_then(|v| Entry::from_json(&v)))
            .collect();
        assert_eq!(
            entries,
            cases
                .iter()
                .map(|(shape, reason, text)| Entry::Recovery {
                    shape: *shape,
                    reason: *reason,
                    text: (*text).into(),
                })
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_pre_adr_0043_recovery_entry_without_a_reason_decodes_as_broken_state() {
        // Backward compatibility: a `recovery` line missing the `reason` key
        // (every recovery logged before ADR-0043 was broken-state) decodes as
        // UnverifiedWrites, never a torn line.
        let line = r#"{"e":"recovery","shape":"handoff","text":"[r]"}"#;
        let decoded = decode_line(line).and_then(|v| Entry::from_json(&v));
        assert_eq!(
            decoded,
            Some(Entry::Recovery {
                shape: RecoveryShape::Handoff,
                reason: ReopenReason::UnverifiedWrites,
                text: "[r]".into(),
            })
        );
    }

    // ---- retry entries (ADR-0030) ----

    #[test]
    fn retry_entries_round_trip_the_file_with_their_error_attempt_and_budget() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        log.append(Entry::Retry {
            error: "api_stream_error: Failed to generate a valid tool call".into(),
            attempt: 1,
            budget: 3,
        });
        log.append(Entry::Retry {
            error: "api_stream_error: Failed to generate a valid tool call".into(),
            attempt: 2,
            budget: 3,
        });

        let content = std::fs::read_to_string(&log.path).unwrap();
        let entries: Vec<Entry> = content
            .lines()
            .skip(1)
            .filter_map(|l| decode_line(l).and_then(|v| Entry::from_json(&v)))
            .collect();
        assert_eq!(
            entries,
            vec![
                Entry::Retry {
                    error: "api_stream_error: Failed to generate a valid tool call".into(),
                    attempt: 1,
                    budget: 3,
                },
                Entry::Retry {
                    error: "api_stream_error: Failed to generate a valid tool call".into(),
                    attempt: 2,
                    budget: 3,
                },
            ]
        );
    }

    #[test]
    fn a_retry_entry_is_silent_to_the_folded_conversation() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        log.append(Entry::UserText("go".into()));
        // A retryable draw failed and was re-drawn silently; the re-issued
        // request succeeded and the Run completed.
        log.append(Entry::Retry {
            error: "api_stream_error: Failed to generate a valid tool call".into(),
            attempt: 1,
            budget: 3,
        });
        log.append(Entry::assistant_blocks(vec![text("re-drawn answer")]));
        log.append(Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::EndTurn,
            reason: None,
        });

        let (messages, _) = resume(&log.path, &session).unwrap();

        // The retry never enters the Conversation: user prompt then the reply.
        assert_eq!(
            messages,
            vec![
                user_message(vec![text("go")]),
                Message::assistant(vec![text("re-drawn answer")]),
            ]
        );
    }

    // ---- recoveries_used/1 ----

    #[test]
    fn recoveries_used_counts_recovery_entries_since_the_last_user_prompt() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        assert_eq!(recoveries_used(&log.path), 0);

        log.append(Entry::UserText("first request".into()));
        log.append(Entry::Recovery {
            shape: RecoveryShape::Continuation,
            reason: ReopenReason::UnverifiedWrites,
            text: "[r1]".into(),
        });
        assert_eq!(recoveries_used(&log.path), 1);

        // A genuine user prompt starts a new request: the count resets.
        log.append(Entry::UserText("second request".into()));
        assert_eq!(recoveries_used(&log.path), 0);

        log.append(Entry::Recovery {
            shape: RecoveryShape::Handoff,
            reason: ReopenReason::DanglingFailure,
            text: "[r2]".into(),
        });
        log.append(Entry::Recovery {
            shape: RecoveryShape::Handoff,
            reason: ReopenReason::UnverifiedWrites,
            text: "[r3]".into(),
        });
        assert_eq!(recoveries_used(&log.path), 2);
    }

    #[test]
    fn advances_used_counts_only_open_plan_recovery_entries_since_the_last_prompt() {
        // ADR-0043: the two budgets are separate counters over the same
        // `recovery` entries, split by reason.
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        assert_eq!(advances_used(&log.path), 0);

        log.append(Entry::UserText("request".into()));
        log.append(Entry::Recovery {
            shape: RecoveryShape::Handoff,
            reason: ReopenReason::DanglingFailure,
            text: "[broken]".into(),
        });
        log.append(Entry::Recovery {
            shape: RecoveryShape::Continuation,
            reason: ReopenReason::OpenPlan,
            text: "[open1]".into(),
        });
        log.append(Entry::Recovery {
            shape: RecoveryShape::Continuation,
            reason: ReopenReason::OpenPlan,
            text: "[open2]".into(),
        });

        // The broken-state entry counts as a recovery, the two Open-Plan
        // entries as advances - neither budget bleeds into the other.
        assert_eq!(recoveries_used(&log.path), 1);
        assert_eq!(advances_used(&log.path), 2);

        // A genuine user prompt resets BOTH.
        log.append(Entry::UserText("next request".into()));
        assert_eq!(recoveries_used(&log.path), 0);
        assert_eq!(advances_used(&log.path), 0);
    }

    #[test]
    fn recoveries_used_is_zero_for_missing_or_foreign_files() {
        assert_eq!(recoveries_used("/definitely/not/here.jsonl"), 0);
    }

    // ---- plan/recoveries_used read under the fold's torn-line tolerance ----

    // A torn line is a truncated JSON object (a crash mid-write), the shape the
    // fold's `a_torn_last_line_is_dropped` uses. The header const and the
    // `write_log` raw-file helper both live lower in this module.
    const TORN_LINE: &str = r#"{"e": "plan", "tex"#;

    #[test]
    fn plan_is_none_for_a_missing_file() {
        assert_eq!(plan("/definitely/not/here.jsonl"), None);
    }

    #[test]
    fn plan_is_none_for_an_empty_file() {
        let tmp = TempDir::new().unwrap();
        let path = write_log(tmp.path(), "20260101-000000-1.jsonl", &[]);
        assert_eq!(plan(&path), None);
    }

    #[test]
    fn plan_is_none_for_a_header_only_log() {
        let tmp = TempDir::new().unwrap();
        let path = write_log(tmp.path(), "20260101-000000-1.jsonl", &[TEST_HEADER]);
        assert_eq!(plan(&path), None);
    }

    // A torn HEADER line: [`plan`] and [`recoveries_used`] both skip line 1
    // unconditionally (header validation is [`resume`]'s job, not theirs), so a
    // single torn header alone leaves nothing to scan - both read empty. The
    // point of the test is that the two agree, the consistency the fix
    // establishes; neither invents an entry from a log with no valid entries.
    #[test]
    fn plan_and_recoveries_used_agree_when_the_header_line_is_torn() {
        let tmp = TempDir::new().unwrap();
        let path = write_log(tmp.path(), "20260101-000000-1.jsonl", &[TORN_LINE]);
        assert_eq!(plan(&path), None);
        assert_eq!(recoveries_used(&path), 0);
    }

    #[test]
    fn plan_returns_the_last_plan_before_a_tear_never_one_after() {
        let tmp = TempDir::new().unwrap();
        let path = write_log(
            tmp.path(),
            "20260101-000000-1.jsonl",
            &[
                TEST_HEADER,
                r#"{"e":"user_text","text":"go"}"#,
                r#"{"e":"plan","text":"before the tear"}"#,
                TORN_LINE,
                r#"{"e":"plan","text":"after the tear"}"#,
            ],
        );
        // The tear stops the scan: the Plan after it is never observed, exactly
        // as the resumed Conversation would never see it.
        assert_eq!(plan(&path), Some("before the tear".to_string()));
    }

    #[test]
    fn recoveries_used_stops_at_the_first_torn_line() {
        let tmp = TempDir::new().unwrap();
        let path = write_log(
            tmp.path(),
            "20260101-000000-1.jsonl",
            &[
                TEST_HEADER,
                r#"{"e":"user_text","text":"go"}"#,
                r#"{"e":"recovery","shape":"continuation","text":"[r1]"}"#,
                TORN_LINE,
                r#"{"e":"recovery","shape":"continuation","text":"[r2]"}"#,
            ],
        );
        // Only the recovery before the tear is counted; the one after it is
        // dropped just as the fold drops everything past a torn line.
        assert_eq!(recoveries_used(&path), 1);
    }

    #[test]
    fn recoveries_used_and_plan_are_zero_none_for_a_header_only_log() {
        let tmp = TempDir::new().unwrap();
        let path = write_log(tmp.path(), "20260101-000000-1.jsonl", &[TEST_HEADER]);
        assert_eq!(recoveries_used(&path), 0);
        assert_eq!(plan(&path), None);
    }

    #[test]
    fn recoveries_used_is_zero_for_an_empty_file() {
        let tmp = TempDir::new().unwrap();
        let path = write_log(tmp.path(), "20260101-000000-1.jsonl", &[]);
        assert_eq!(recoveries_used(&path), 0);
    }

    // ---- resume_governed folds the governance facts once ----
    //
    // The single fold's `plan`/`recoveries` MUST equal the standalone
    // `plan`/`recoveries_used` queries on the same file: same last-Plan and
    // same recovery-since-last-user_text semantics, same torn-line tolerance.

    #[test]
    fn resume_governed_plan_and_recoveries_match_the_standalone_queries() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        log.append(Entry::UserText("go".into()));
        log.append(Entry::Plan("Goal: A. 1. read [x]".into()));
        log.append(Entry::Recovery {
            shape: RecoveryShape::Handoff,
            reason: ReopenReason::UnverifiedWrites,
            text: "[r1]".into(),
        });
        // A fresh prompt resets the recovery count (matching the live Agent's
        // reset on submit); the later Plan is the one that resumes.
        log.append(Entry::UserText("now do B".into()));
        log.append(Entry::Plan("Goal: B. 1. edit [ ]".into()));
        log.append(Entry::Recovery {
            shape: RecoveryShape::Handoff,
            reason: ReopenReason::UnverifiedWrites,
            text: "[r2]".into(),
        });
        // An Open-Plan continuation this request too: it feeds `advances`, not
        // `recoveries`, so the two governance counters stay separate (ADR-0043).
        log.append(Entry::Recovery {
            shape: RecoveryShape::Continuation,
            reason: ReopenReason::OpenPlan,
            text: "[r3]".into(),
        });

        let r = resume_governed(&log.path, &session).unwrap();

        assert_eq!(r.plan, plan(&log.path));
        assert_eq!(r.recoveries, recoveries_used(&log.path));
        assert_eq!(r.advances, advances_used(&log.path));
        assert_eq!(r.plan, Some("Goal: B. 1. edit [ ]".to_string()));
        assert_eq!(r.recoveries, 1);
        assert_eq!(r.advances, 1);
    }

    #[test]
    fn resume_governed_matches_the_standalone_queries_under_a_tear() {
        let tmp = TempDir::new().unwrap();
        // The header's root is `/r`; resume needs a Session rooted there.
        let session = Session::build(
            SessionOpts {
                root: Some("/r".into()),
                session_dir: Some(tmp.path().to_string_lossy().into_owned()),
                ..Default::default()
            },
            &SessionConfig::test_defaults(),
        )
        .unwrap();
        let path = write_log(
            tmp.path(),
            "20260101-000000-1.jsonl",
            &[
                TEST_HEADER,
                r#"{"e":"user_text","text":"go"}"#,
                r#"{"e":"plan","text":"before the tear"}"#,
                r#"{"e":"recovery","shape":"continuation","text":"[r1]"}"#,
                TORN_LINE,
                r#"{"e":"plan","text":"after the tear"}"#,
                r#"{"e":"recovery","shape":"continuation","text":"[r2]"}"#,
            ],
        );

        let r = resume_governed(&path, &session).unwrap();

        // The tear stops the scan for all three derivations identically: the fold
        // drops the post-tear messages, and `plan`/`recoveries` never observe the
        // Plan or Recovery after it.
        assert_eq!(r.plan, plan(&path));
        assert_eq!(r.recoveries, recoveries_used(&path));
        assert_eq!(r.plan, Some("before the tear".to_string()));
        assert_eq!(r.recoveries, 1);
    }

    #[test]
    fn seeded_message_entries_replay_verbatim() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        let seeded = Message::assistant(vec![text("from a previous life")]);
        log.append(Entry::Message(seeded.clone()));

        let (messages, drift) = resume(&log.path, &session).unwrap();
        assert_eq!(messages, vec![seeded]);
        assert_eq!(drift, Vec::new());
    }

    // ---- Provenance persistence (ADR-0037) ----

    #[test]
    fn assistant_provenance_round_trips_through_the_log_and_fold() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        let stamp = Provenance::new("anthropic", "claude-fable-5");
        log.append(Entry::UserText("go".into()));
        log.append(Entry::AssistantBlocks {
            blocks: vec![tool_use("t1", "grep", json!({}))],
            provenance: Some(stamp.clone()),
        });
        log.append(Entry::ToolResult(tool_result("t1", "hits")));
        log.append(Entry::AssistantBlocks {
            blocks: vec![text("done")],
            provenance: Some(stamp.clone()),
        });
        log.append(Entry::Settled {
            outcome: Settled::Completed,
            stop_reason: StopReason::EndTurn,
            reason: None,
        });

        let (messages, _) = resume(&log.path, &session).unwrap();

        assert_eq!(messages[1].provenance, Some(stamp.clone()));
        assert_eq!(messages[3].provenance, Some(stamp));
        assert_eq!(messages[0].provenance, None, "user messages carry none");
        assert_eq!(messages[2].provenance, None);
    }

    #[test]
    fn a_seeded_message_entry_keeps_its_provenance_across_log_generations() {
        // Resume seeds a fresh log with `message` entries; the stamp must
        // survive so a twice-resumed history still normalizes correctly.
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        let mut log = Log::open(&session).unwrap();

        let seeded = Message::assistant_from(
            vec![text("stamped reply")],
            Provenance::new("lmstudio", "qwen3.6-27b"),
        );
        log.append(Entry::Message(seeded.clone()));

        let (messages, _) = resume(&log.path, &session).unwrap();
        assert_eq!(messages, vec![seeded]);
    }

    // The documented decode choice: a logged assistant event MISSING the
    // provenance fields decodes as `None` (unknown Provenance, a transform
    // mismatch) rather than failing the fold - the same optional-field
    // tolerance the settled entry's `reason` takes, and strictly safer than
    // treating the line as torn (which would silently drop the rest of the
    // log). No backwards compatibility is intended; unknown is simply the
    // honest value for an unstamped line.
    #[test]
    fn a_line_missing_the_provenance_fields_decodes_as_unknown_provenance() {
        let raw = r#"{"e":"assistant_blocks","blocks":[{"type":"text","text":"old"}]}"#;
        let entry = Entry::from_json(&decode_line(raw).unwrap()).unwrap();
        assert_eq!(
            entry,
            Entry::AssistantBlocks {
                blocks: vec![text("old")],
                provenance: None,
            }
        );

        let raw = r#"{"e":"message","role":"assistant","content":[{"type":"text","text":"old"}]}"#;
        let entry = Entry::from_json(&decode_line(raw).unwrap()).unwrap();
        assert_eq!(entry, Entry::Message(Message::assistant(vec![text("old")])));
    }

    #[test]
    fn provenance_rides_the_wire_as_flat_provider_and_model_keys() {
        // The greppable-log thesis of ADR-0010: a human can read the stamp.
        let entry = Entry::AssistantBlocks {
            blocks: vec![text("hi")],
            provenance: Some(Provenance::new("anthropic", "claude-fable-5")),
        };
        let value = entry.to_json();
        assert_eq!(value["provider"], "anthropic");
        assert_eq!(value["model"], "claude-fable-5");
        assert_eq!(Entry::from_json(&value), Some(entry));
    }

    // ---- latest/1 ----

    #[test]
    fn returns_the_newest_log_by_filename_error_when_none() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());

        assert_eq!(latest(&session.session_dir), None);

        let first = Log::open(&session).unwrap();
        let second = Log::open(&session).unwrap();

        let got = latest(&session.session_dir).unwrap();
        assert!(got == first.path || got == second.path);

        let missing = std::path::Path::new(&session.session_dir)
            .join("missing")
            .to_string_lossy()
            .into_owned();
        assert_eq!(latest(&missing), None);
    }

    // ---- list/1 ----

    const TEST_HEADER: &str = r#"{"type":"session","version":1,"root":"/r","model":"m","context_budget":1,"turn_limit":1}"#;

    fn write_log(dir: &std::path::Path, name: &str, lines: &[&str]) -> String {
        let path = dir.join(name);
        std::fs::write(&path, lines.join("\n")).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn list_returns_newest_first_by_the_filename_stamp_latest_sorts_on() {
        let tmp = TempDir::new().unwrap();
        let older = write_log(
            tmp.path(),
            "20260101-090000-1.jsonl",
            &[TEST_HEADER, r#"{"e":"user_text","text":"older"}"#],
        );
        let newer = write_log(
            tmp.path(),
            "20260711-140205-1.jsonl",
            &[TEST_HEADER, r#"{"e":"user_text","text":"newer"}"#],
        );

        let entries = list(&tmp.path().to_string_lossy());

        assert_eq!(
            entries,
            vec![
                SessionEntry {
                    path: newer,
                    stamp: "2026-07-11 14:02".into(),
                    label: "newer".into(),
                },
                SessionEntry {
                    path: older,
                    stamp: "2026-01-01 09:00".into(),
                    label: "older".into(),
                },
            ]
        );
    }

    #[test]
    fn list_labels_with_the_first_user_text_first_line_only() {
        let tmp = TempDir::new().unwrap();
        write_log(
            tmp.path(),
            "20260101-000000-1.jsonl",
            &[
                TEST_HEADER,
                r#"{"e":"plan","text":"not a label"}"#,
                "{\"e\":\"user_text\",\"text\":\"fix the bug\\nwith much more detail below\"}",
                r#"{"e":"user_text","text":"a later prompt"}"#,
            ],
        );

        let entries = list(&tmp.path().to_string_lossy());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].label, "fix the bug");
    }

    #[test]
    fn list_char_truncates_long_labels_with_an_ellipsis() {
        let tmp = TempDir::new().unwrap();
        let long = "é".repeat(70);
        write_log(
            tmp.path(),
            "20260101-000000-1.jsonl",
            &[
                TEST_HEADER,
                &format!(r#"{{"e":"user_text","text":"{long}"}}"#),
            ],
        );

        let entries = list(&tmp.path().to_string_lossy());
        assert_eq!(entries[0].label.chars().count(), 61);
        assert!(entries[0].label.ends_with('…'));
        assert!(entries[0].label.starts_with(&"é".repeat(60)));
    }

    #[test]
    fn list_labels_a_log_with_no_user_text_as_an_empty_session() {
        let tmp = TempDir::new().unwrap();
        let session = session_in(tmp.path());
        Log::open(&session).unwrap();

        let entries = list(&session.session_dir);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].label, "(empty session)");
    }

    #[test]
    fn list_skips_torn_headers_and_foreign_files_without_panicking() {
        let tmp = TempDir::new().unwrap();
        write_log(
            tmp.path(),
            "20260101-000000-1.jsonl",
            &[r#"{"type": "sess"#],
        );
        write_log(tmp.path(), "20260102-000000-1.jsonl", &["not json at all"]);
        write_log(
            tmp.path(),
            "20260103-000000-1.jsonl",
            &[r#"{"type":"something_else"}"#],
        );
        write_log(tmp.path(), "notes.txt", &["a non-jsonl file"]);
        let good = write_log(
            tmp.path(),
            "20260104-000000-1.jsonl",
            &[TEST_HEADER, r#"{"e":"user_text","text":"survivor"}"#],
        );

        let entries = list(&tmp.path().to_string_lossy());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, good);
        assert_eq!(entries[0].label, "survivor");
    }

    #[test]
    fn list_of_an_empty_or_missing_dir_is_empty() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(list(&tmp.path().to_string_lossy()), Vec::new());

        let missing = tmp.path().join("missing").to_string_lossy().into_owned();
        assert_eq!(list(&missing), Vec::new());
    }
}
