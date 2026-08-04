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
//!   * `user_text` - submit and Rollover alike, for a pure-text prompt
//!   * `user_content{blocks}` - a submitted prompt carrying media (ADR-0068);
//!     a pure-text prompt still logs as `user_text`, byte-identical to before
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

use std::io::Write;

use serde::{Deserialize, Serialize};

use crate::content::{ContentBlock, Message, Provenance};
use crate::session::Session;
use crate::voice::FileOps;

mod codec;
mod fold;
mod picker;
mod resume;
// The reconstructed-summary composition lives with the fold that reuses it; the
// Compaction module reaches it as `crate::session::log::compose_summary`.
pub use fold::compose_summary;
// The read-back path lives in two submodules - the `--resume` picker (`picker`)
// and the Resume fold entry (`resume`); the public surface is re-exported so
// callers still reach it as `crate::session::log::…` (the split is invisible).
pub use picker::{SessionEntry, latest, list};
pub(crate) use resume::resume_governed;
pub use resume::{Drift, ResumeError, plan, resume};

// ------------------------------------------------------------------
// Terminal stop reason + settled outcome (shared with Run Settlement).
// `Settled`/`SettledEntry` are BUILT ON here; `StopReason` is the leaf
// run-lifecycle vocabulary [`crate::stop_reason`], re-exported so the
// Session Log's historical `session::log::StopReason` path keeps resolving
// without importing it into any cycle (Voice reads the leaf directly).
// ------------------------------------------------------------------

pub(crate) use crate::stop_reason::StopReason;

/// How a settled Run resolved (baud's `:completed | :failed | :cancelled`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settled {
    Completed,
    Failed,
    Cancelled,
}

impl Settled {
    pub(super) fn as_str(&self) -> &'static str {
        match self {
            Settled::Completed => "completed",
            Settled::Failed => "failed",
            Settled::Cancelled => "cancelled",
        }
    }

    pub(super) fn from_str(s: &str) -> Option<Settled> {
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

// ------------------------------------------------------------------
// The entry enum
// ------------------------------------------------------------------

/// One Session Log entry. The fold consumes these; [`append`] serializes them.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    UserText(String),
    /// A submitted user prompt's full content-block list (ADR-0068): the
    /// media-capable sibling of [`Entry::UserText`], written when the prompt
    /// carries more than a single Text block (an At Expansion image/PDF). A
    /// pure-text prompt still persists as the byte-identical `UserText`, so an
    /// old log and a text-only new log read identically. The fold folds this
    /// into one user [`Message`] over the blocks.
    UserContent(Vec<ContentBlock>),
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
    /// An `assistant_blocks` entry with unknown Provenance; the live Agent
    /// appends stamped entries via the struct form.
    #[cfg(test)]
    pub fn assistant_blocks(blocks: Vec<ContentBlock>) -> Entry {
        Entry::AssistantBlocks {
            blocks,
            provenance: None,
        }
    }
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

#[cfg(test)]
#[path = "../../tests/session/log.rs"]
mod tests;
