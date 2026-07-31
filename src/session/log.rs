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
use crate::session::Session;
use crate::voice::{self, FileOps};

// ------------------------------------------------------------------
// Terminal stop reason + settled outcome (shared with Run Settlement).
// These types were introduced by the Settlement phase and are BUILT ON
// here, not redefined.
// ------------------------------------------------------------------

/// A Run's terminal stop reason as it enters the Session Log and the
/// settlement event. Spans the LLM-reported reasons that ride through a
/// completed Run (`end_turn`, `max_tokens`, ...) and the Run-Limit reasons
/// the loop mints (`turn_limit` at the max-turns bound, `turn_limit_stuck`
/// when the loop-detector trips). `Error`/`Unknown` are the synthetic reasons
/// Settlement writes for failed/cancelled Runs.
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

    // Delegates to `as_str` to keep the string mapping in one place.
    fn from_str(s: &str) -> Option<Settled> {
        [Settled::Completed, Settled::Failed, Settled::Cancelled]
            .into_iter()
            .find(|v| v.as_str() == s)
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
    fn to_json(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            Entry::UserText(text) => json!({"e": "user_text", "text": text}),
            Entry::Steering(text) => json!({"e": "steering", "text": text}),
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
            "plan" => Some(Entry::Plan(string_field(m, "text")?)),
            "assistant_blocks" => parse_assistant_blocks(m),
            "tool_result" => parse_tool_result(m),
            "message" => parse_message(m),
            "settled" => parse_settled(m),
            "compacted" => parse_compacted(m),
            "retry" => parse_retry(m),
            _ => None,
        }
    }
}

// Per-kind entry parsers. Each returns `None` on a shape mismatch - the same
// torn-line tolerance `from_json` carries to the fold.

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
        // The header is a fixed-shape struct; serialization cannot fail.
        let header_line = serde_json::to_string(&header).map_err(std::io::Error::other)?;
        writeln!(io, "{header_line}")?;
        io.flush()?;

        Ok(Log { path, io })
    }

    /// Appends one entry as one line, flushed through immediately.
    pub fn append(&mut self, entry: Entry) -> &mut Self {
        // `entry.to_json()` returns a `Value`; `to_string` of a `Value` is
        // infallible in serde_json - skip writing on the vanishingly unlikely
        // serializer error rather than panicking.
        if let Ok(line) = serde_json::to_string(&entry.to_json()) {
            let _ = writeln!(self.io, "{line}");
            let _ = self.io.flush();
        }
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
/// `compaction_slack` and `compaction_keep` are user-tunable (ADR-0031) and simply
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
    names.sort();
    let newest = names.last()?;
    Some(
        std::path::Path::new(dir)
            .join(newest)
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

// `20260711-140205-3.jsonl` -> `2026-07-11 14:02` (the [`utc_stamp`] shape,
// seconds and the uniquifier dropped). A name that doesn't carry that shape
// falls back to its bare stem.

/// Minimum bytes in the stamp prefix (YYYYMMDD-HHMMss = 15 chars).
const STAMP_PREFIX_LEN: usize = 15;
/// Byte offset of the date/time separator dash in the stamp.
const STAMP_DASH_POS: usize = 8;

fn human_stamp(name: &str) -> String {
    let stem = name.strip_suffix(".jsonl").unwrap_or(name);
    let raw = stem.as_bytes();
    let stamped = raw.len() >= STAMP_PREFIX_LEN
        && raw[STAMP_DASH_POS] == b'-'
        && raw[..STAMP_PREFIX_LEN]
            .iter()
            .enumerate()
            .all(|(i, b)| i == STAMP_DASH_POS || b.is_ascii_digit());
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

// ------------------------------------------------------------------
// Resume: fold a log file into Conversation messages
// ------------------------------------------------------------------

/// A resumed log, folded once. Carries the Conversation `messages` and header
/// `drift` (the Transcript-facing facts), plus the last logged `plan` derived
/// from the SAME entry stream the fold walked. Because all three come from one
/// pass that stops at the first torn line, `plan` here matches [`plan`] on the
/// same file exactly, and stays consistent with the Conversation the fold
/// produced. Private: the Agent unpacks it and keeps `plan` outside the
/// Transcript-facing `ResumeInfo`.
pub(crate) struct Resumed {
    pub(crate) messages: Vec<Message>,
    pub(crate) drift: Vec<Drift>,
    pub(crate) plan: Option<String>,
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

/// Like [`resume`], but also returns the last logged Plan ([`Resumed::plan`])
/// computed in the SAME single fold - so the Agent resumes the Plan without
/// re-reading the file. It is derived from `entries`, which the loop below stops
/// populating at the first torn line, so it inherits the fold's tolerance and
/// matches the standalone [`plan`] query line for line.
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

    let plan = last_plan(&entries);

    Ok(Resumed {
        messages: fold(&entries),
        drift: drift(&header, session),
        plan,
    })
}

// The last Plan seen in the tolerated entry stream, mirroring [`plan`] exactly.
fn last_plan(entries: &[Entry]) -> Option<String> {
    let mut plan: Option<String> = None;
    for entry in entries {
        if let Entry::Plan(text) = entry {
            plan = Some(text.clone());
        }
    }
    plan
}

fn check_root(header: &serde_json::Value, session: &Session) -> Result<(), ResumeError> {
    if header.get("root").and_then(|r| r.as_str()) == Some(session.root.as_str()) {
        Ok(())
    } else {
        Err(ResumeError::RootMismatch)
    }
}

fn push_drift(out: &mut Vec<Drift>, key: &'static str, logged: String, current: String) {
    if logged != current {
        out.push(Drift {
            key,
            logged,
            current,
        });
    }
}

fn drift(header: &serde_json::Value, session: &Session) -> Vec<Drift> {
    let mut out = Vec::new();

    let logged_model = header.get("model").and_then(|v| v.as_str()).unwrap_or("");
    push_drift(
        &mut out,
        "model",
        logged_model.to_string(),
        session.model.scoped_id(),
    );

    let current_budget = session.context_budget_for(&session.model);
    let logged_budget = header.get("context_budget").and_then(|v| v.as_u64());
    push_drift(
        &mut out,
        "context_budget",
        opt_num(logged_budget),
        current_budget.to_string(),
    );

    let logged_limit = header.get("turn_limit").and_then(|v| v.as_u64());
    push_drift(
        &mut out,
        "turn_limit",
        opt_num(logged_limit),
        session.run_limit.to_string(),
    );

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
                let marker = match stop_reason {
                    StopReason::RunLimit => voice::run_limit_marker(),
                    StopReason::RunLimitStuck => voice::loop_stall_marker(),
                    _ => voice::run_stopped_marker(),
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
#[path = "../../tests/unit/session/log.rs"]
mod tests;
