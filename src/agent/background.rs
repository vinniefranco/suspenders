//! Background subagent task registry (P4b/4c, ADR-0061, ADR-0063).
//!
//! A background subagent is a detached child Run: the parent launches it and
//! carries on, and a `<task-notification>` reaches the parent's next Run when
//! the child settles. The Agent (the single owner of mutable Session state,
//! ADR-0017) owns the registry - a `HashMap<String, BackgroundTask>` keyed by
//! the minted task id - so a completing child hands its result back through the
//! same mpsc every Run event travels (a `RunMsg::BackgroundDone`), and the
//! single owner serializes the registry mutation with everything else.
//!
//! This module holds the plain data (the [`BackgroundTask`] entry and its
//! [`BackgroundStatus`]) plus two pure helpers: [`mint_task_id`] (qwen's
//! `<subagentName>-<suffix>` id shape) and [`task_notification`] (the VERBATIM
//! qwen `<task-notification>` envelope, TRIMMED to the fields Suspenders carries
//! today). No actor, no channel - the Agent drives all of that; this is the
//! vocabulary it drives over.

use std::sync::Arc;

use crate::agent::{AgentState, Msg, RunMsg};
use crate::event::Event;
use crate::session::log::Entry as LogEntry;
use crate::skills::escape_xml;
use crate::tool::caps::SubagentResult;

/// One background subagent the Agent is tracking (ADR-0063). The `abort` handle
/// cancels the detached `tokio::spawn` at its next `.await` (`task_stop`, or the
/// Agent's actor-loop-exit abortAll); `status` is where it is in its lifecycle;
/// `description` is carried for the notification envelope (the model's 3-5 word
/// summary, escaped into `<summary>`).
pub struct BackgroundTask {
    /// The detached child Run's abort handle: cancels it at its next `.await`.
    /// A cancel sets the status to [`BackgroundStatus::Stopped`], so the later
    /// `BackgroundDone` (if the abort does not fire first) is dropped.
    pub abort: tokio::task::AbortHandle,
    /// Where the task is in its lifecycle.
    pub status: BackgroundStatus,
    /// The model's 3-5 word summary, escaped into the notification `<summary>`.
    pub description: String,
}

/// A background subagent's lifecycle (ADR-0063). `Running` is the live state a
/// `task_stop` or a settling child transitions out of; `Done`/`Failed` are the
/// settled terminal states the `not-running` wording reports; `Stopped` is the
/// cancelled state a `task_stop` sets synchronously (so a racing
/// `BackgroundDone` is dropped rather than double-notifying). The terminal
/// states are UNIT variants today - `background_done` assembles the notification
/// from the settling result inline and never reads it back off the status; the
/// deferred payload (for a future `send_message`/resume) is noted in ADR-0063.
pub enum BackgroundStatus {
    /// The child Run is in flight.
    Running,
    /// The child settled successfully.
    Done,
    /// A `task_stop` cancelled the child (the abort was requested).
    Stopped,
    /// The child settled as a failure.
    Failed,
}

/// Mints a background task id (qwen's `<subagentName>-<suffix>`, ADR-0063):
/// `{subagent_type}-{n}`. The `n` is the Agent's monotonic per-Session counter,
/// so ids never collide within a Session. Kept a pure fn so the id shape is
/// testable and the Agent's handler stays about registry mechanics.
pub fn mint_task_id(subagent_type: &str, n: u64) -> String {
    format!("{subagent_type}-{n}")
}

/// Builds the VERBATIM qwen `<task-notification>` envelope (ADR-0063,
/// background-tasks.ts `emitNotification`), TRIMMED to the fields Suspenders
/// carries today: `<task-id>`, `<status>`, `<summary>`, and a CONDITIONAL
/// `<result>`. The deferred `<tool-use-id>`/`<output-file>`/`<usage>` lines are
/// omitted (no live-output file, no per-task usage roll-up yet). Every
/// interpolated value is [`escape_xml`]-escaped, so a result or description
/// carrying `<`/`&`/`"` cannot close the envelope early and forge sibling tags.
///
/// `status` is the raw lifecycle word the `<status>` tag carries and that drives
/// the `statusText` in the `<summary>`: `"completed"`, `"failed"`, or
/// `"cancelled"` (which renders as "was cancelled" in the summary sentence, qwen
/// verbatim). `result` is the child's answer text (or, for a failure, the
/// `Error: {error}` line qwen writes) - already assembled by the caller.
///
/// The `<result>` line is CONDITIONAL, matching qwen's `if (entry.result)`
/// guard (background-tasks.ts): an empty `result` omits the line entirely, so a
/// cancelled notification (which carries no result) emits NO `<result>` tag
/// rather than an empty `<result></result>`. A failure always carries a
/// non-empty `Error: {error}` line, so its result tag is always present.
pub fn task_notification(id: &str, status: &str, description: &str, result: &str) -> String {
    // qwen's statusText: completed | failed | was cancelled. The `<status>` tag
    // carries the raw word; the `<summary>` sentence carries the phrase.
    let status_text = match status {
        "completed" => "completed",
        "failed" => "failed",
        _ => "was cancelled",
    };
    // The `<result>` line only appears when there IS a result (qwen's
    // `if (entry.result)`): an empty result omits the whole line, so a cancelled
    // notification has no `<result>` tag.
    let result_line = if result.is_empty() {
        String::new()
    } else {
        format!("<result>{}</result>\n", escape_xml(result))
    };
    format!(
        "<task-notification>\n\
         <task-id>{id}</task-id>\n\
         <status>{status}</status>\n\
         <summary>Agent \"{description}\" {status_text}.</summary>\n\
         {result_line}\
         </task-notification>",
        id = escape_xml(id),
        status = escape_xml(status),
        description = escape_xml(description),
        status_text = status_text,
        result_line = result_line,
    )
}

/// The background-subagent lifecycle handlers the Agent's actor loop delegates to
/// (P4b/4c/4d, ADR-0063). They live here beside the registry vocabulary they
/// drive over (a launch, a settlement, a stop, and the actor-loop-exit abortAll),
/// so the actor file keeps the `RunMsg` dispatch and this module owns the
/// registry mechanics. Each takes `&mut self` (the Agent's single-owner state),
/// so the map never leaves the owning task (ADR-0017).
impl AgentState {
    /// A background-subagent launch (P4b/4c, ADR-0063): mint the id, build the
    /// child request through the SHARED DirectSubagentSpawner resolution (so
    /// foreground and background never drift), spawn a DETACHED child Run,
    /// register the entry, and return the id. The `agent` tool does NOT park - a
    /// background launch returns immediately with the id and the parent carries
    /// on. A resolution Err (unknown type, unresolvable Model) surfaces as the id
    /// string carrying the Err so the tool folds it into its own result - the
    /// same launch-failure shape spawn takes.
    pub(super) fn spawn_background(
        &mut self,
        request: crate::tool::caps::SubagentRequest,
        description: String,
    ) -> String {
        // Build the shared spawner (the same handles `capture()` builds it from)
        // so the background resolution matches the foreground one exactly.
        let spawner = crate::run::subagent::DirectSubagentSpawner {
            llm: Arc::clone(&self.llm),
            parent_model: self.model.clone(),
            temperature: self.session.temperature,
            thinking_budget: self.session.thinking_budget,
            tool_call_style: self.session.tool_call_style,
            session: self.session.clone(),
            registry: Arc::clone(&self.subagents),
            subagent_run_limit: self.session.run_limit as usize,
        };

        // Mint the id BEFORE resolving, so it reads `{subagent_type}-{n}` even on
        // a resolution failure the caller surfaces. The counter is per-Session.
        self.background_counter += 1;
        let id = mint_task_id(&request.subagent_type, self.background_counter);

        // Resolve into the child request (the shared path). `sink: None` - the
        // live background feed is DEFERRED (ADR-0063), so a background child is as
        // invisible mid-run as a foreground one; only its settlement notification
        // crosses back.
        let child = match spawner.build_child_request(request, None) {
            Ok(child) => child,
            // A launch failure: return the Err string as the id so the tool folds
            // it into its own error result (mirrors `spawn`'s Err propagation).
            Err(reason) => return reason,
        };

        // Spawn the DETACHED child Run: a plain `tokio::spawn` that drives the
        // child to settlement, then posts a `BackgroundDone` back over the SAME
        // mpsc every Run event travels (so the single owner serializes the
        // registry mutation). The Agent holds only the AbortHandle -
        // `task_stop`/abortAll cancel through it, and the detached task's own
        // JoinHandle is dropped (fire-and-forget).
        let tx = self.self_tx.clone();
        let done_id = id.clone();
        let handle = tokio::spawn(async move {
            let result = crate::run::run_child(child).await;
            let _ = tx.send(Msg::Run(RunMsg::BackgroundDone {
                id: done_id,
                result,
            }));
        });

        self.background.insert(
            id.clone(),
            BackgroundTask {
                abort: handle.abort_handle(),
                status: BackgroundStatus::Running,
                description,
            },
        );

        id
    }

    /// A background child settled (P4b/4c, ADR-0063): if the entry is still
    /// Running, record its terminal status, queue the `<task-notification>` for
    /// the next Run to drain, log it as a durable user-role entry, and broadcast
    /// the finished Event. If the entry is already Stopped (a `task_stop`
    /// cancelled it), drop the result - the `was cancelled` notification was
    /// queued synchronously at stop.
    pub(super) fn background_done(&mut self, id: String, result: SubagentResult) {
        let Some(entry) = self.background.get_mut(&id) else {
            return; // Unknown/pruned id: nothing to settle.
        };
        if !matches!(entry.status, BackgroundStatus::Running) {
            return; // Already Stopped/settled: drop the racing result.
        }

        // GOAL vs a non-GOAL terminate maps to completed vs failed (qwen's
        // registry complete/fail split). The `<result>` is the child's answer
        // text, or the `Error: {finalText}` line for a failure (empty finalText ->
        // bare `Error:`). The terminal status is a UNIT marker (ADR-0063): the
        // notification is assembled from `result` inline here, never read back off
        // the status.
        let (status_word, result_text) = if result.terminate_reason == "GOAL" {
            entry.status = BackgroundStatus::Done;
            ("completed", result.result.clone())
        } else {
            entry.status = BackgroundStatus::Failed;
            ("failed", format!("Error: {}", result.result))
        };
        let description = entry.description.clone();

        let notification = task_notification(&id, status_word, &description, &result_text);
        self.notifications.push(notification.clone());
        // Durable + operator-visible: the notification is a user-role log entry
        // (so a resumed Session carries it) and a broadcast Event (so the UI sees
        // it now).
        super::log_entry(self, LogEntry::UserText(notification.clone()));
        super::broadcast(self, Event::background_notification(notification));
        super::broadcast(self, Event::background_task_finished(id, status_word));
    }

    /// A background-subagent stop request (P4b/4d, ADR-0063): abort the child, set
    /// the entry Stopped, queue the `was cancelled` notification SYNCHRONOUSLY
    /// (the terminal notification the parent still receives), and return the
    /// VERBATIM qwen `task_stop` wording. The two HIT legs are VERBATIM:
    /// found+running (stop confirmation) and found+not-running (the not-running
    /// error). Returns `None` when no SUBAGENT owns the id, so the dual-registry
    /// handler (Phase 9, ADR-0063) can fall through to the shell registry and then
    /// synthesize the verbatim not-found ONCE (NO string-sniffing).
    pub(super) fn stop_background(&mut self, id: String) -> Option<String> {
        let entry = self.background.get_mut(&id)?;
        if !matches!(entry.status, BackgroundStatus::Running) {
            let status = background_status_word(&entry.status);
            return Some(format!(
                "Error: Background agent \"{id}\" is not running (status: {status})."
            ));
        }

        // Abort the detached child at its next `.await`, then mark it Stopped so
        // the racing `BackgroundDone` (if the abort loses the race) is dropped.
        entry.abort.abort();
        entry.status = BackgroundStatus::Stopped;
        let description = entry.description.clone();

        // Queue the terminal `was cancelled` notification synchronously - qwen's
        // own handler emits the terminal notification via the registry, and here
        // the abort means no `BackgroundDone` will arrive to carry the child's
        // partial result, so the cancelled notification is queued now.
        let notification = task_notification(&id, "cancelled", &description, "");
        self.notifications.push(notification.clone());
        super::log_entry(self, LogEntry::UserText(notification.clone()));
        super::broadcast(self, Event::background_notification(notification));
        super::broadcast(
            self,
            Event::background_task_finished(id.clone(), "cancelled"),
        );

        Some(format!(
            "Cancellation requested for background agent \"{id}\". A final \
             task-notification carrying the agent's last result will follow.\n\
             Description: {description}"
        ))
    }

    /// Abort every tracked background child at actor-loop exit (P4b, ADR-0063):
    /// the Session is ending, so the detached child Runs must not outlive it.
    /// `abort()` cancels each at its next `.await`; the entries are dropped with
    /// the state.
    pub(super) fn abort_all_background(&mut self) {
        for entry in self.background.values() {
            entry.abort.abort();
        }
        self.background.clear();
    }
}

/// The lifecycle word qwen's `not-running` error shows (status: {status}).
fn background_status_word(status: &BackgroundStatus) -> &'static str {
    match status {
        BackgroundStatus::Running => "running",
        BackgroundStatus::Done => "completed",
        BackgroundStatus::Stopped => "cancelled",
        BackgroundStatus::Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_task_id_is_type_dash_n() {
        assert_eq!(mint_task_id("general-purpose", 1), "general-purpose-1");
        assert_eq!(mint_task_id("Explore", 7), "Explore-7");
    }

    #[test]
    fn a_completed_notification_is_the_verbatim_envelope() {
        let out = task_notification(
            "general-purpose-1",
            "completed",
            "find the bug",
            "the findings",
        );
        assert_eq!(
            out,
            "<task-notification>\n\
             <task-id>general-purpose-1</task-id>\n\
             <status>completed</status>\n\
             <summary>Agent \"find the bug\" completed.</summary>\n\
             <result>the findings</result>\n\
             </task-notification>"
        );
    }

    #[test]
    fn a_failed_notification_says_failed() {
        let out = task_notification("scout-2", "failed", "explore api", "Error: boom");
        assert!(out.contains("<status>failed</status>"));
        assert!(out.contains("<summary>Agent \"explore api\" failed.</summary>"));
        assert!(out.contains("<result>Error: boom</result>"));
    }

    #[test]
    fn a_cancelled_notification_says_was_cancelled() {
        let out = task_notification("scout-3", "cancelled", "explore api", "partial work");
        assert!(out.contains("<status>cancelled</status>"));
        assert!(out.contains("<summary>Agent \"explore api\" was cancelled.</summary>"));
    }

    #[test]
    fn an_empty_result_omits_the_result_tag() {
        // qwen's `if (entry.result)`: a cancelled notification carries no
        // result, so the `<result>` line is omitted ENTIRELY - no empty
        // `<result></result>` where qwen writes nothing.
        let out = task_notification("scout-4", "cancelled", "explore api", "");
        assert!(
            !out.contains("<result>"),
            "no result tag on an empty result: {out}"
        );
        assert!(!out.contains("</result>"));
        // The rest of the envelope is intact, and the summary line is the last
        // line before the close tag.
        assert_eq!(
            out,
            "<task-notification>\n\
             <task-id>scout-4</task-id>\n\
             <status>cancelled</status>\n\
             <summary>Agent \"explore api\" was cancelled.</summary>\n\
             </task-notification>"
        );
    }

    #[test]
    fn a_non_empty_result_includes_the_result_tag() {
        // The symmetric case: a non-empty result keeps the `<result>` line.
        let out = task_notification("scout-5", "completed", "explore api", "the findings");
        assert!(out.contains("<result>the findings</result>"), "{out}");
    }

    #[test]
    fn every_interpolated_value_is_xml_escaped() {
        // A result/description/id carrying the five metacharacters must not be
        // able to close the envelope early and forge sibling tags.
        let out = task_notification(
            "a<b&c-1",
            "completed",
            "read <config> & \"x\"",
            "</result><forged>evil</forged>",
        );
        assert!(out.contains("<task-id>a&lt;b&amp;c-1</task-id>"));
        assert!(out.contains("Agent \"read &lt;config&gt; &amp; &quot;x&quot;\""));
        assert!(
            out.contains("<result>&lt;/result&gt;&lt;forged&gt;evil&lt;/forged&gt;</result>"),
            "a result cannot forge a sibling tag: {out}"
        );
        // No unescaped forged tag survives.
        assert!(!out.contains("<forged>"));
    }
}
