//! Reading a Session Log back (ADR-0010): the `--resume` picker (which logs
//! exist, their labels) and the Resume fold entry point (fold one log into a
//! Conversation, report header drift, recover the last Plan). Carved out of
//! [`super`] so the parent keeps the entry vocabulary + wire codec + file
//! lifecycle and this owns the read-back path.
//!
//! Everything here reads a log file with the fold's torn-line tolerance: a torn
//! line stops the scan (a crash mid-write is the expected failure of an
//! append-only file). The public picker/resume items are re-exported from
//! [`super`], so callers still reach them as `crate::session::log::…`.

use crate::content::Message;
use crate::session::Session;

use super::{Entry, codec, fold};

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
        match codec::decode_line(line).and_then(|v| Entry::from_json(&v)) {
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
    let header: serde_json::Value =
        codec::decode_line(header_line).ok_or(ResumeError::MalformedLog)?;

    check_root(&header, session)?;

    // A torn last line (crash mid-write) decodes as an error; stop at the first
    // undecodable line and drop everything after.
    let mut entries: Vec<Entry> = Vec::new();
    for line in lines {
        match codec::decode_line(line).and_then(|v| Entry::from_json(&v)) {
            Some(entry) => entries.push(entry),
            None => break,
        }
    }

    let plan = last_plan(&entries);

    Ok(Resumed {
        messages: fold::fold(&entries),
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
