//! `agent` - the ONE tool that launches a foreground subagent (P4/F4,
//! ADR-0061).
//!
//! Like the [`SkillTool`](crate::tools::skill::SkillTool), a single
//! [`AgentTool`] holds an `Arc` of the
//! [`SubagentRegistry`](crate::subagents::SubagentRegistry) and builds its
//! description AND schema dynamically: the description embeds the
//! available-subagents list (`- **{name}**: {description}`), and the
//! `subagent_type` param's `enum` is the registry's names. The tool's `run`
//! assembles a [`SubagentRequest`] and reaches the host through the
//! [`SubagentSpawner`](crate::tool::caps::SubagentSpawner) capability (F4,
//! ADR-0061), which drives a child Run to settlement and returns its result.
//!
//! The description scaffold is ported VERBATIM from qwen v0.16.0's
//! `tools/agent/agent.ts` `baseDescription`, TRIMMED of the background /
//! worktree / `run_in_background` usage notes and the `isolation` schema
//! property (all DEFERRED with 4c/4d, ADR-0061) - so the schema exposes exactly
//! `description`, `prompt`, and `subagent_type`.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::subagents::{DEFAULT_BUILTIN_SUBAGENT_TYPE, SubagentRegistry};
use crate::tool::caps::SubagentRequest;
use crate::tool::{Tool, ToolCtx, ToolSpec};

/// The `agent` tool: a stateful tool (like [`crate::tools::skill::SkillTool`])
/// holding the Session's subagent registry. Registered once in `init_agent` over
/// the `Arc<SubagentRegistry>` the Agent built.
pub struct AgentTool {
    registry: Arc<SubagentRegistry>,
}

impl AgentTool {
    /// Builds the tool over a shared [`SubagentRegistry`]. The Agent builds the
    /// registry once at launch and hands the `Arc` here.
    pub fn new(registry: Arc<SubagentRegistry>) -> Self {
        AgentTool { registry }
    }

    /// The available-subagents block: one `- **{name}**: {description}` line per
    /// def (qwen's `subagentDescriptions`), or the empty-catalog wording. Built
    /// fresh for every `spec()` call off the registry's current defs.
    fn subagent_descriptions(&self) -> String {
        let defs = self.registry.list();
        if defs.is_empty() {
            // qwen's full wording is "No subagents are currently configured. You
            // can create subagents using the /agents command." The second
            // sentence is INTENTIONALLY omitted: there is no `/agents` command
            // ported yet (user/project subagent files are DEFERRED, ADR-0061), so
            // pointing the model at a command that does not exist would mislead
            // it. This branch is itself unreachable in production - `builtins()`
            // always ships two defs - and survives only for the empty-registry
            // spec test.
            return "No subagents are currently configured.".to_string();
        }
        defs.iter()
            .map(|def| format!("- **{}**: {}", def.name, def.description))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[async_trait::async_trait]
impl Tool for AgentTool {
    fn spec(&self) -> ToolSpec {
        // The description scaffold, VERBATIM from qwen's `tools/agent/agent.ts`
        // `baseDescription`, with the live available-subagents block spliced in
        // and the background/worktree/run_in_background notes TRIMMED (deferred).
        // qwen's `${ToolNames.*}` interpolations resolve to Suspenders' own tool
        // names.
        let description = format!(
            "Launch a new agent to handle complex, multi-step tasks autonomously.
The Agent tool launches specialized agents (subprocesses) that autonomously handle complex tasks. Each agent type has specific capabilities and tools available to it.

Available agent types and the tools they have access to:
{}

When using the Agent tool, specify a subagent_type parameter to select which agent type to use.

When NOT to use the Agent tool:
- If you want to read a specific file path, use the read_file tool or the glob tool instead of the agent tool, to find the match more quickly
- If you are searching for a specific class definition like \"class Foo\", use the grep tool instead, to find the match more quickly
- If you are searching for code within a specific file or set of 2-3 files, use the read_file tool instead of the agent tool, to find the match more quickly
- Other tasks that are not related to the agent descriptions above


Usage notes:
- Always include a short description (3-5 words) summarizing what the agent will do
- Launch multiple agents concurrently whenever possible, to maximize performance; to do that, use a single message with multiple tool uses
- When the agent is done, it will return a single message back to you. The result returned by the agent is not visible to the user. To show the user the result, you should send a text message back to the user with a concise summary of the result.
- Provide clear, detailed prompts so the agent can work autonomously and return exactly the information you need.
- The agent's outputs should generally be trusted
- Clearly tell the agent whether you expect it to write code or just to do research (search, file reads, web fetches, etc.), since it is not aware of the user's intent
- If the agent description mentions that it should be used proactively, then you should try your best to use it without the user having to ask for it first. Use your judgement.
- If the user specifies that they want you to run agents \"in parallel\", you MUST send a single message with multiple Agent tool use content blocks. For example, if you need to launch both a build-validator agent and a test-runner agent in parallel, send a single message with both tool calls.

Example usage:

<example_agent_descriptions>
\"test-runner\": use this agent after you are done writing code to run tests
\"greeting-responder\": use this agent to respond to user greetings with a friendly joke
</example_agent_descriptions>

<example>
user: \"Please write a function that checks if a number is prime\"
assistant: I'm going to use the Write tool to write the following code:
<code>
function isPrime(n) {{
  if (n <= 1) return false
  for (let i = 2; i * i <= n; i++) {{
    if (n % i === 0) return false
  }}
  return true
}}
</code>
<commentary>
Since a significant piece of code was written and the task was completed, now use the test-runner agent to run the tests
</commentary>
assistant: Uses the agent tool to launch the test-runner agent
</example>

<example>
user: \"Hello\"
<commentary>
Since the user is greeting, use the greeting-responder agent to respond with a friendly joke
</commentary>
assistant: \"I'm going to use the agent tool to launch the greeting-responder agent\"
</example>
",
            self.subagent_descriptions()
        );

        // The dynamic schema: `description`/`prompt`/`subagent_type` (qwen's
        // initial schema TRIMMED of `run_in_background` and `isolation`). The
        // `subagent_type` enum is the registry's names when non-empty.
        let mut subagent_type = json!({
            "type": "string",
            "description": "The type of specialized agent to use for this task",
        });
        let names = self.registry.names();
        if !names.is_empty() {
            subagent_type["enum"] = json!(names);
        }

        ToolSpec {
            name: "agent".to_string(),
            description,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "description": {
                        "type": "string",
                        "description": "A short (3-5 word) description of the task",
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The task for the agent to perform",
                    },
                    "subagent_type": subagent_type,
                },
                "required": ["description", "prompt"],
                "additionalProperties": false,
            }),
        }
    }

    /// On the base wire list: `agent` is a first-class delegation tool, not a
    /// deferred one, so the model sees it (and its available-subagents catalog)
    /// at Run start.
    fn should_defer(&self) -> bool {
        false
    }

    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<String, String> {
        // Validate the params qwen's `validateToolParams` way: `description` and
        // `prompt` non-empty strings, `subagent_type` (when present) a known
        // non-empty name. (Suspenders' schema validation guarantees presence +
        // string type; the non-empty + existence checks are agent-specific.)
        let description = input
            .get("description")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "Parameter \"description\" must be a non-empty string.".to_string())?;
        let _ = description; // qwen carries it for the display; the child Run does not need it.

        let prompt = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "Parameter \"prompt\" must be a non-empty string.".to_string())?
            .to_string();

        // `subagent_type` defaults to the built-in default when the model omits
        // it (qwen's fork path is DEFERRED, so an omitted type routes to the
        // general-purpose agent rather than a context-sharing fork).
        let subagent_type = match input.get("subagent_type") {
            Some(v) => {
                let s = v
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        "Parameter \"subagent_type\" must be a non-empty string.".to_string()
                    })?;
                if self.registry.get(s).is_none() {
                    return Err(format!(
                        "Subagent \"{s}\" not found. Available subagents: {}",
                        self.registry.names().join(", ")
                    ));
                }
                s.to_string()
            }
            None => DEFAULT_BUILTIN_SUBAGENT_TYPE.to_string(),
        };

        // Reach the host through the SubagentSpawner capability (F4, ADR-0061).
        // `model: None` defers the per-subagent Model to the def's own choice.
        let request = SubagentRequest {
            subagent_type,
            prompt,
            model: None,
        };
        match ctx.caps.subagents.spawn(request).await {
            Ok(result) => Ok(format_result(result)),
            // A launch failure (unknown type, unresolvable Model, degraded host):
            // fold the verbatim string into an error result.
            Err(reason) => Err(reason),
        }
    }
}

/// Formats a settled [`SubagentResult`](crate::tool::caps::SubagentResult) into
/// the tool's content string (qwen's FOREGROUND result shaping,
/// `tools/agent/agent.ts` ~2276-2306, the path this ports - NOT the background
/// `registry.fail` string). qwen's foreground shaping is: on `ERROR` with an
/// empty finalText, the verbatim `"Subagent execution failed."`; otherwise (a
/// non-empty finalText on ERROR, or any other terminate mode) the bare finalText,
/// which may itself be empty. So GOAL and every non-ERROR mode surface
/// `result.result` verbatim, and only an empty ERROR gets the synthetic message.
/// (qwen's `CANCELLED` prefix path is DEFERRED with the background path.)
fn format_result(result: crate::tool::caps::SubagentResult) -> String {
    if result.terminate_reason == "ERROR" && result.result.is_empty() {
        "Subagent execution failed.".to_string()
    } else {
        result.result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagents::builtins;
    use crate::tool::caps::{
        Capabilities, SubagentRequest, SubagentResult, SubagentSpawner,
    };
    use std::sync::Mutex;

    fn tool() -> AgentTool {
        AgentTool::new(Arc::new(SubagentRegistry::new(builtins())))
    }

    #[test]
    fn spec_schema_enum_is_the_registry_names() {
        let spec = tool().spec();
        assert_eq!(spec.name, "agent");
        let enum_names = spec.input_schema["properties"]["subagent_type"]["enum"]
            .as_array()
            .expect("enum present with defs")
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(enum_names, vec!["general-purpose", "Explore"]);
        // The required set is description + prompt (subagent_type optional).
        let required = spec.input_schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 2);
    }

    #[test]
    fn spec_description_lists_the_subagents_and_is_not_deferred() {
        let t = tool();
        let spec = t.spec();
        assert!(spec.description.contains("- **general-purpose**:"));
        assert!(spec.description.contains("- **Explore**:"));
        // Deferred usage notes are trimmed.
        assert!(!spec.description.contains("run_in_background"));
        assert!(!spec.description.contains("worktree"));
        assert!(!t.should_defer());
    }

    #[test]
    fn empty_registry_omits_the_enum_and_shows_the_empty_wording() {
        let t = AgentTool::new(Arc::new(SubagentRegistry::default()));
        let spec = t.spec();
        assert!(spec.input_schema["properties"]["subagent_type"]
            .get("enum")
            .is_none());
        assert!(spec.description.contains("No subagents are currently configured."));
    }

    // A spawner that records the request it received and returns a fixed result.
    struct RecordingSpawner {
        seen: Arc<Mutex<Vec<(String, String)>>>,
        result: SubagentResult,
    }

    #[async_trait::async_trait]
    impl SubagentSpawner for RecordingSpawner {
        async fn spawn(&self, request: SubagentRequest) -> Result<SubagentResult, String> {
            self.seen
                .lock()
                .unwrap()
                .push((request.subagent_type, request.prompt));
            Ok(SubagentResult {
                terminate_reason: self.result.terminate_reason.clone(),
                result: self.result.result.clone(),
            })
        }
    }

    fn ctx_with(spawner: Arc<dyn SubagentSpawner>) -> ToolCtx {
        ToolCtx {
            caps: Capabilities::for_test_with_subagents(spawner),
            ..ToolCtx::for_test(std::env::temp_dir(), 100_000)
        }
    }

    #[tokio::test]
    async fn run_spawns_the_resolved_request_and_returns_the_goal_result() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let spawner = Arc::new(RecordingSpawner {
            seen: Arc::clone(&seen),
            result: SubagentResult {
                terminate_reason: "GOAL".into(),
                result: "the findings".into(),
            },
        });
        let out = tool()
            .run(
                &json!({
                    "description": "find the bug",
                    "prompt": "investigate the panic",
                    "subagent_type": "general-purpose",
                }),
                &ctx_with(spawner),
            )
            .await
            .unwrap();
        assert_eq!(out, "the findings");
        // The spawner received the resolved request.
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0],
            ("general-purpose".to_string(), "investigate the panic".to_string())
        );
    }

    #[tokio::test]
    async fn run_defaults_the_subagent_type_to_general_purpose() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let spawner = Arc::new(RecordingSpawner {
            seen: Arc::clone(&seen),
            result: SubagentResult {
                terminate_reason: "GOAL".into(),
                result: "ok".into(),
            },
        });
        tool()
            .run(
                &json!({"description": "do it", "prompt": "the task"}),
                &ctx_with(spawner),
            )
            .await
            .unwrap();
        assert_eq!(seen.lock().unwrap()[0].0, "general-purpose");
    }

    #[tokio::test]
    async fn run_error_empty_result_is_the_verbatim_execution_failed_message() {
        // qwen foreground: ERROR + empty finalText -> "Subagent execution failed."
        let spawner = Arc::new(RecordingSpawner {
            seen: Arc::new(Mutex::new(Vec::new())),
            result: SubagentResult {
                terminate_reason: "ERROR".into(),
                result: String::new(),
            },
        });
        let out = tool()
            .run(
                &json!({"description": "do it", "prompt": "the task"}),
                &ctx_with(spawner),
            )
            .await
            .unwrap();
        assert_eq!(out, "Subagent execution failed.");
    }

    #[tokio::test]
    async fn run_error_non_empty_result_surfaces_the_bare_partial_text() {
        // qwen foreground: ERROR + non-empty finalText -> the raw finalText, no
        // synthetic message.
        let spawner = Arc::new(RecordingSpawner {
            seen: Arc::new(Mutex::new(Vec::new())),
            result: SubagentResult {
                terminate_reason: "ERROR".into(),
                result: "got this far".into(),
            },
        });
        let out = tool()
            .run(
                &json!({"description": "do it", "prompt": "the task"}),
                &ctx_with(spawner),
            )
            .await
            .unwrap();
        assert_eq!(out, "got this far");
    }

    #[tokio::test]
    async fn run_max_turns_empty_result_is_the_bare_empty_string() {
        // qwen foreground: a non-ERROR terminate (MAX_TURNS) with an empty
        // finalText surfaces the bare (empty) text, NOT the background
        // `Agent terminated with mode: ...` string.
        let spawner = Arc::new(RecordingSpawner {
            seen: Arc::new(Mutex::new(Vec::new())),
            result: SubagentResult {
                terminate_reason: "MAX_TURNS".into(),
                result: String::new(),
            },
        });
        let out = tool()
            .run(
                &json!({"description": "do it", "prompt": "the task"}),
                &ctx_with(spawner),
            )
            .await
            .unwrap();
        assert_eq!(out, "");
    }

    #[tokio::test]
    async fn run_unknown_subagent_type_is_the_verbatim_not_found_error() {
        let err = tool()
            .run(
                &json!({"description": "d", "prompt": "p", "subagent_type": "nope"}),
                &ctx_with(Arc::new(RecordingSpawner {
                    seen: Arc::new(Mutex::new(Vec::new())),
                    result: SubagentResult {
                        terminate_reason: "GOAL".into(),
                        result: String::new(),
                    },
                })),
            )
            .await
            .unwrap_err();
        assert_eq!(
            err,
            "Subagent \"nope\" not found. Available subagents: general-purpose, Explore"
        );
    }

    #[tokio::test]
    async fn run_degraded_spawner_folds_the_verbatim_unavailable_string() {
        // The default for_test ctx carries an UnavailableSubagentSpawner.
        let err = tool()
            .run(
                &json!({"description": "d", "prompt": "p", "subagent_type": "general-purpose"}),
                &ToolCtx::for_test(std::env::temp_dir(), 100_000),
            )
            .await
            .unwrap_err();
        assert_eq!(err, "subagents are unavailable in this environment");
    }
}
