//! Scout - the disposable read-only worker the model dispatches through the
//! explore Tool (CONTEXT.md). It searches the Project Root in its own fresh
//! Conversation against the same model connection, with a hard Pass cap and a
//! read-only Tool subset (read_file, list_files, grep), and returns a
//! structured findings report.
//!
//! ## An effect, not part of the Turn loop
//!
//! Like [`crate::compaction`], the Scout calls the [`Llm`] boundary directly -
//! it is an effect that lives outside the Turn loop (ADR-0011, ADR-0013). The
//! explore Tool reaches it through the `scout` capture on the Tool ctx, which
//! wires the Session's llm and connection. The Turn loop never sees the
//! Scout's Conversation; only the explore Tool Call and its Tool Result flow
//! through the normal path.
//!
//! ## No nesting
//!
//! The Scout's registry is [`crate::tools::scout_specs`], which excludes
//! explore, plan, and every mutating or command-running Tool. A Scout cannot
//! edit, run commands, or dispatch further Scouts (CONTEXT.md).
//!
//! ## The report Pass
//!
//! The last permitted Pass (`pass == pass_limit`) is a forced report Pass: the
//! request offers NO tools and the Conversation gains a Voice-owned user
//! message telling the Scout to write its findings report from what it has
//! seen (ADR-0014).
//!
//! ## No thinking
//!
//! With the `no_think` option the Scout's every request carries the
//! break-glass no-think field. Live A/B probing (2026-07-10, ADR-0014) showed
//! thinking is not load-bearing for search quality and is fatal on Qwen3.5.
//! Sessions default this on via `scout_no_think`.
//!
//! ## Never crashes the Turn
//!
//! [`Scout::run`] NEVER panics out to the dispatching Turn. An LLM error, empty
//! findings, or hitting the hard Pass cap all return a [`ScoutOutcome`] error
//! variant carrying whatever partial findings exist.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use crate::content::ContentBlock;
use crate::conversation::{Conversation, ConversationOpts};
use crate::llm::request;
use crate::llm::response::StopReason;
use crate::llm::Llm;
use crate::session::connection::Connection;
use crate::tool::ToolCtx;
use crate::tools;
use crate::tools::shaping;
use crate::voice;

/// The result of one Scout run.
///
/// A clean report is `Ok`. Every failure mode carries whatever partial
/// findings text was gathered (possibly empty), which the explore Tool rides
/// after a Voice-owned marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScoutOutcome {
    /// A clean findings report.
    Ok(String),
    /// The Scout's own model call failed; `partial` is what streamed before.
    LlmError { partial: String },
    /// The Scout stopped without reporting anything usable.
    Empty { partial: String },
    /// The Scout hit its hard Pass cap before reporting.
    PassCap { limit: u64, partial: String },
}

/// The `scout` capture on the Tool ctx: an effect wired to the Session that
/// dispatches a Scout for a `task` and yields its [`ScoutOutcome`]. Boxed and
/// pinned so it is object-safe and `Send`, mirroring the async run path.
pub type ScoutFn =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = ScoutOutcome> + Send>> + Send + Sync>;

// The disposable Scout Conversation is short by construction (a small Pass
// cap), so it does not need the main Session's budget. A generous default
// keeps Eviction dormant during a normal Scout run while still guarding
// against a runaway.
const DEFAULT_CONTEXT_BUDGET: u64 = 64_000;
const DEFAULT_PASS_LIMIT: u64 = 8;
const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 120_000;

/// Options for one Scout run; [`Default`] supplies the defaults (`pass_limit: 8`,
/// `context_budget: 64_000`, `no_think: false`, root = current dir).
#[derive(Debug, Clone)]
pub struct ScoutOpts {
    /// The Project Root the read-only tools resolve against (the ctx capture
    /// always supplies it).
    pub root: std::path::PathBuf,
    /// The hard Pass cap. The last permitted Pass is the forced report Pass.
    pub pass_limit: u64,
    /// The disposable Conversation's budget.
    pub context_budget: u64,
    /// Carry the no-think wire field on every Scout request (ADR-0014).
    pub no_think: bool,
    /// The read-only registry has no run_command, so this is never read; it
    /// still must honor the ctx type.
    pub command_timeout_ms: u64,
}

impl Default for ScoutOpts {
    fn default() -> Self {
        ScoutOpts {
            root: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            pass_limit: DEFAULT_PASS_LIMIT,
            context_budget: DEFAULT_CONTEXT_BUDGET,
            no_think: false,
            command_timeout_ms: DEFAULT_COMMAND_TIMEOUT_MS,
        }
    }
}

/// The Scout: `run` dispatches one over `task` against the same model
/// `connection` and returns the structured findings report as a
/// [`ScoutOutcome`]. Never panics.
pub struct Scout;

// The Scout's own loop state, threaded across Passes.
struct LoopState<'a> {
    llm: &'a dyn Llm,
    connection: &'a Connection,
    ctx: ToolCtx,
    pass_limit: u64,
    pass: u64,
    last_text: String,
    no_think: bool,
}

impl Scout {
    /// Runs one Scout over `task` against the same model `connection`,
    /// returning the structured findings report.
    ///
    /// Returns [`ScoutOutcome::Ok`] with the report, or an error variant
    /// (`LlmError`, `PassCap`, `Empty`) carrying whatever findings text was
    /// gathered (possibly empty). Never panics.
    pub async fn run(
        task: &str,
        llm: &dyn Llm,
        connection: &Connection,
        opts: ScoutOpts,
    ) -> ScoutOutcome {
        let ctx = ToolCtx {
            root: opts.root,
            result_cap: shaping::cap_for(opts.context_budget, connection.max_tokens),
            command_timeout_ms: opts.command_timeout_ms,
            scout: None,
        };

        let mut conversation = Conversation::new(
            voice::scout_system_prompt(),
            ConversationOpts::new(opts.context_budget, connection.max_tokens),
        );
        conversation.add_user_text(task);

        let state = LoopState {
            llm,
            connection,
            ctx,
            pass_limit: opts.pass_limit,
            pass: 1,
            last_text: String::new(),
            no_think: opts.no_think,
        };

        Self::loop_(conversation, state).await
    }

    // One Scout Pass: call the model, act on the stop reason. The Pass cap is
    // a hard guard checked before every model call, so a model that keeps
    // asking for tools can never loop past it.
    async fn loop_(mut conversation: Conversation, mut state: LoopState<'_>) -> ScoutOutcome {
        loop {
            if state.pass > state.pass_limit {
                return ScoutOutcome::PassCap {
                    limit: state.pass_limit,
                    partial: state.last_text.clone(),
                };
            }

            let report_pass = state.pass == state.pass_limit;

            if report_pass {
                conversation.add_user_text(voice::scout_report_now());
            }

            let req = match conversation.for_request() {
                // A disposable Scout that outgrows its own budget stops with
                // what it has rather than failing the dispatching Turn.
                Err(_) => {
                    return ScoutOutcome::PassCap {
                        limit: state.pass_limit,
                        partial: state.last_text.clone(),
                    };
                }
                Ok(req) => req,
            };

            // The report Pass offers no tools: the only move left is the
            // report.
            let tools_specs = if report_pass {
                Vec::new()
            } else {
                tools::scout_specs()
            };

            let wire = request::build(
                &req.system,
                &req.messages,
                &tools_specs,
                state.connection,
                state.no_think,
            );

            let response = state
                .llm
                .complete(wire, state.connection, &mut |_ev| {})
                .await;

            match response.stop_reason {
                StopReason::Error => {
                    // The response carries whatever streamed before the
                    // failure. Prefer that partial text; fall back to the last
                    // full text.
                    let partial = match extract_text(&response.content) {
                        t if t.is_empty() => state.last_text.clone(),
                        t => t,
                    };
                    return ScoutOutcome::LlmError { partial };
                }
                StopReason::ToolUse if response.content.iter().any(is_tool_use) => {
                    // The Scout asked for read-only tools: run them
                    // (plugin-free, Shaped like any Tool Result), feed the
                    // results back, and loop for another Pass. The accumulated
                    // assistant text is remembered so a later failure can still
                    // return partial findings.
                    let text = extract_text(&response.content);
                    if !text.is_empty() {
                        state.last_text = text;
                    }

                    let results =
                        Self::run_tools(&response.content, &state.ctx).await;

                    conversation.add_assistant_blocks(response.content);
                    conversation.add_tool_results(results, Vec::new());

                    state.pass += 1;
                }
                // A tool_use stop with no tool_use blocks is a server quirk;
                // treat it like an ordinary completion. A truncated response
                // is the end of the road for a disposable worker. Any other
                // stop reason reports the accumulated text.
                _ => return report(&state, &response.content),
            }
        }
    }

    // Runs the Scout's read-only tool calls and builds their Tool Result
    // blocks, plugin-free and Shaped, honoring the async Tool boundary.
    async fn run_tools(content: &[ContentBlock], ctx: &ToolCtx) -> Vec<ContentBlock> {
        let mut results = Vec::new();
        for block in content {
            if let ContentBlock::ToolUse { id, name, input } = block {
                let input = display_input(input);
                let result = tools::run(name, &input, ctx).await;
                results.push(ContentBlock::tool_result(id, result.content, result.is_error));
            }
        }
        results
    }
}

// A findings report is the Scout's final text. Empty text is a distinct
// failure (the model stopped without reporting anything usable); it still
// carries the last accumulated text as the partial findings.
fn report(state: &LoopState<'_>, content: &[ContentBlock]) -> ScoutOutcome {
    let text = extract_text(content);
    if text.is_empty() {
        ScoutOutcome::Empty {
            partial: state.last_text.clone(),
        }
    } else {
        ScoutOutcome::Ok(text)
    }
}

fn extract_text(content: &[ContentBlock]) -> String {
    let joined = content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    joined.trim().to_string()
}

// The LLM layer tags tool inputs whose JSON did not decode; never run those,
// and let the read-only tool's own validation reject an empty map.
fn display_input(input: &Value) -> Value {
    if let Value::Object(map) = input
        && map.contains_key(crate::llm::stream::MALFORMED_INPUT_SENTINEL)
    {
        return Value::Object(serde_json::Map::new());
    }
    input.clone()
}

fn is_tool_use(block: &ContentBlock) -> bool {
    matches!(block, ContentBlock::ToolUse { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::{ContentBlock, Message, Role, Usage};
    use crate::llm::response::{Response, StopReason};
    use crate::test_support::{Entry, FakeLlm};
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    fn connection() -> Connection {
        Connection::new("http://localhost:0/v1", "", "m", 8)
    }

    // A tmp Project Root with a widget file, mirroring baud's setup.
    fn root() -> TempDir {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("widget.ex"),
            "defmodule Widget do\n  def go, do: :ok\nend\n",
        )
        .unwrap();
        tmp
    }

    fn opts(root: &TempDir) -> ScoutOpts {
        ScoutOpts {
            root: root.path().to_path_buf(),
            ..Default::default()
        }
    }

    fn text_response(s: &str) -> Response {
        Response {
            content: vec![ContentBlock::text(s)],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
            error: None,
        }
    }

    fn tool_use_response(id: &str, name: &str, input: Value) -> Response {
        Response {
            content: vec![ContentBlock::tool_use(id, name, input)],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
            error: None,
        }
    }

    // ---- display_input ----

    // A tool input tagged with the malformed-input sentinel is displayed as an
    // empty map, so the read-only tool's own validation rejects it instead of
    // acting on undecoded JSON. Routed through the one shared sentinel constant.
    #[test]
    fn display_input_empties_a_sentinel_tagged_input() {
        let tagged = json!({ crate::llm::stream::MALFORMED_INPUT_SENTINEL: "{\"path\": tru" });
        assert_eq!(display_input(&tagged), json!({}));

        // A well-formed input passes through untouched.
        let ok = json!({ "path": "lib/widget.ex" });
        assert_eq!(display_input(&ok), ok);
    }

    // ---- run/4 happy path ----

    #[tokio::test]
    async fn returns_the_models_findings_report_as_an_ordinary_string_result() {
        let root = root();
        let report = "Locations: widget.ex:2. How it works: Widget.go returns :ok.";
        let fake = FakeLlm::script(vec![Entry::just(text_response(report))]);

        let outcome = Scout::run("where does go live", &fake, &connection(), opts(&root)).await;
        assert_eq!(outcome, ScoutOutcome::Ok(report.to_string()));
    }

    #[tokio::test]
    async fn runs_multiple_passes_calling_read_only_tools_before_reporting() {
        let root = root();
        let fake = FakeLlm::script(vec![
            Entry::just(tool_use_response("t1", "list_files", json!({}))),
            Entry::just(tool_use_response(
                "t2",
                "read_file",
                json!({"path": "widget.ex"}),
            )),
            Entry::just(text_response("Found Widget.go at widget.ex:2.")),
        ]);

        let outcome = Scout::run("find go", &fake, &connection(), opts(&root)).await;
        assert_eq!(
            outcome,
            ScoutOutcome::Ok("Found Widget.go at widget.ex:2.".to_string())
        );
    }

    #[tokio::test]
    async fn uses_its_own_voice_owned_system_prompt_on_the_request() {
        let root = root();
        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let cap = Arc::clone(&captured);
        let fake = FakeLlm::script(vec![Entry::dynamic(vec![], move |req: &Value| {
            *cap.lock().unwrap() = Some(req["system"].as_str().unwrap_or("").to_string());
            text_response("done")
        })]);

        Scout::run("task", &fake, &connection(), opts(&root)).await;

        assert_eq!(
            captured.lock().unwrap().as_deref(),
            Some(voice::scout_system_prompt())
        );
    }

    // ---- read-only registry (no nesting) ----

    #[tokio::test]
    async fn the_request_only_carries_read_file_list_files_and_grep_specs() {
        let root = root();
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let cap = Arc::clone(&captured);
        let fake = FakeLlm::script(vec![Entry::dynamic(vec![], move |req: &Value| {
            let names: Vec<String> = req["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["name"].as_str().unwrap().to_string())
                .collect();
            *cap.lock().unwrap() = names;
            text_response("done")
        })]);

        Scout::run("task", &fake, &connection(), opts(&root)).await;

        let mut names = captured.lock().unwrap().clone();
        names.sort();
        assert_eq!(names, vec!["grep", "list_files", "read_file"]);
    }

    #[tokio::test]
    async fn excludes_explore_plan_edit_file_write_file_and_run_command() {
        let root = root();
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let cap = Arc::clone(&captured);
        let fake = FakeLlm::script(vec![Entry::dynamic(vec![], move |req: &Value| {
            let names: Vec<String> = req["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["name"].as_str().unwrap().to_string())
                .collect();
            *cap.lock().unwrap() = names;
            text_response("done")
        })]);

        Scout::run("task", &fake, &connection(), opts(&root)).await;

        let names = captured.lock().unwrap().clone();
        for excluded in ["explore", "plan", "edit_file", "write_file", "run_command"] {
            assert!(!names.contains(&excluded.to_string()), "leaked {excluded}");
        }
    }

    // ---- no-think (ADR-0014) ----

    #[tokio::test]
    async fn no_think_true_rides_on_every_request() {
        let root = root();
        let seen: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
        let s1 = Arc::clone(&seen);
        let s2 = Arc::clone(&seen);
        let fake = FakeLlm::script(vec![
            Entry::dynamic(vec![], move |req: &Value| {
                s1.lock()
                    .unwrap()
                    .push(has_no_think(req));
                tool_use_response("t1", "list_files", json!({}))
            }),
            Entry::dynamic(vec![], move |req: &Value| {
                s2.lock()
                    .unwrap()
                    .push(has_no_think(req));
                text_response("done")
            }),
        ]);

        let opts = ScoutOpts {
            no_think: true,
            ..opts(&root)
        };
        Scout::run("task", &fake, &connection(), opts).await;

        assert_eq!(seen.lock().unwrap().clone(), vec![true, true]);
    }

    #[tokio::test]
    async fn absent_by_default() {
        let root = root();
        let seen: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));
        let s = Arc::clone(&seen);
        let fake = FakeLlm::script(vec![Entry::dynamic(vec![], move |req: &Value| {
            *s.lock().unwrap() = Some(has_no_think(req));
            text_response("done")
        })]);

        Scout::run("task", &fake, &connection(), opts(&root)).await;

        assert_eq!(*seen.lock().unwrap(), Some(false));
    }

    // Mirrors baud's `Map.has_key?(request, :no_think)` - the wire field is
    // the no-think key `chat_template_kwargs.enable_thinking == false`.
    fn has_no_think(req: &Value) -> bool {
        req.get("chat_template_kwargs")
            .and_then(|k| k.get("enable_thinking"))
            .and_then(|v| v.as_bool())
            .map(|b| !b)
            .unwrap_or(false)
    }

    // ---- the report Pass ----

    #[tokio::test]
    async fn offers_no_tools_and_injects_the_voice_report_now_message() {
        let root = root();
        let captured: Arc<Mutex<Option<(usize, Message)>>> = Arc::new(Mutex::new(None));
        let cap = Arc::clone(&captured);
        let fake = FakeLlm::script(vec![
            Entry::just(tool_use_response("t1", "list_files", json!({}))),
            Entry::dynamic(vec![], move |req: &Value| {
                let tool_count = req["tools"].as_array().map(|a| a.len()).unwrap_or(0);
                let messages: Vec<Message> =
                    serde_json::from_value(req["messages"].clone()).unwrap();
                let last = messages.last().cloned().unwrap();
                *cap.lock().unwrap() = Some((tool_count, last));
                text_response("Findings: widget.ex holds Widget.")
            }),
        ]);

        let scout_opts = ScoutOpts {
            pass_limit: 2,
            ..opts(&root)
        };
        let outcome = Scout::run("task", &fake, &connection(), scout_opts).await;
        assert_eq!(
            outcome,
            ScoutOutcome::Ok("Findings: widget.ex holds Widget.".to_string())
        );

        let (tool_count, last) = captured.lock().unwrap().clone().unwrap();
        // No tools are offered on the report Pass (the `tools` key is omitted).
        assert_eq!(tool_count, 0);
        assert_eq!(last.role, Role::User);
        // The last user message carries the Voice report-now text.
        assert_eq!(
            last.content,
            vec![ContentBlock::text(voice::scout_report_now())]
        );
    }

    #[tokio::test]
    async fn an_empty_report_pass_still_fails_as_empty_with_the_partial() {
        let root = root();
        let fake = FakeLlm::script(vec![
            Entry::just(Response {
                content: vec![
                    ContentBlock::text("So far: widget.ex."),
                    ContentBlock::tool_use("t1", "list_files", json!({})),
                ],
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
                error: None,
            }),
            Entry::just(text_response("   ")),
        ]);

        let scout_opts = ScoutOpts {
            pass_limit: 2,
            ..opts(&root)
        };
        let outcome = Scout::run("task", &fake, &connection(), scout_opts).await;
        assert_eq!(
            outcome,
            ScoutOutcome::Empty {
                partial: "So far: widget.ex.".to_string()
            }
        );
    }

    // ---- Pass cap (hard scout_pass_limit) ----

    #[tokio::test]
    async fn stops_after_the_cap_and_returns_an_error_carrying_partial_findings() {
        let root = root();
        // The Scout keeps asking for tools forever; the cap must break it.
        let forever: Vec<Entry> = (0..20)
            .map(|_| Entry::just(tool_use_response("t", "list_files", json!({}))))
            .collect();
        let fake = FakeLlm::script(forever);

        let scout_opts = ScoutOpts {
            pass_limit: 3,
            ..opts(&root)
        };
        let outcome = Scout::run("loop forever", &fake, &connection(), scout_opts).await;
        match outcome {
            ScoutOutcome::PassCap { limit, partial: _ } => assert_eq!(limit, 3),
            other => panic!("expected PassCap, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn never_runs_more_passes_than_the_cap() {
        let root = root();
        let count: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
        let forever: Vec<Entry> = (0..20)
            .map(|_| {
                let c = Arc::clone(&count);
                Entry::dynamic(vec![], move |_req: &Value| {
                    *c.lock().unwrap() += 1;
                    tool_use_response("t", "list_files", json!({}))
                })
            })
            .collect();
        let fake = FakeLlm::script(forever);

        let scout_opts = ScoutOpts {
            pass_limit: 4,
            ..opts(&root)
        };
        Scout::run("loop", &fake, &connection(), scout_opts).await;

        assert_eq!(*count.lock().unwrap(), 4);
    }

    // ---- failure modes (the Scout NEVER raises) ----

    #[tokio::test]
    async fn an_llm_error_returns_llm_error_with_partial() {
        let root = root();
        let fake = FakeLlm::script(vec![Entry::error("connection_refused")]);

        let outcome = Scout::run("task", &fake, &connection(), opts(&root)).await;
        match outcome {
            ScoutOutcome::LlmError { partial: _ } => {}
            other => panic!("expected LlmError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn keeps_text_streamed_before_an_llm_error_as_the_partial_findings() {
        let root = root();
        let partial_response = Response {
            content: vec![ContentBlock::text("Partway: found widget.ex.")],
            stop_reason: StopReason::Error,
            usage: Usage::default(),
            error: Some("midstream".to_string()),
        };
        let fake = FakeLlm::script(vec![Entry::just(partial_response)]);

        let outcome = Scout::run("task", &fake, &connection(), opts(&root)).await;
        assert_eq!(
            outcome,
            ScoutOutcome::LlmError {
                partial: "Partway: found widget.ex.".to_string()
            }
        );
    }

    #[tokio::test]
    async fn an_empty_report_returns_empty() {
        let root = root();
        let fake = FakeLlm::script(vec![Entry::just(text_response("   "))]);

        let outcome = Scout::run("task", &fake, &connection(), opts(&root)).await;
        match outcome {
            ScoutOutcome::Empty { partial: _ } => {}
            other => panic!("expected Empty, got {other:?}"),
        }
    }
}
