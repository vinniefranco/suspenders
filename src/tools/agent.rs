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
//! `tools/agent/agent.ts` `baseDescription`. P4b (4c) restores the ONE
//! `run_in_background` usage bullet and the boolean `run_in_background` schema
//! property; the `isolation`/`worktree` note and schema property stay TRIMMED
//! (git-worktree isolation is DEFERRED, ADR-0061/ADR-0063) - so the schema
//! exposes exactly `description`, `prompt`, `subagent_type`, and
//! `run_in_background`.

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
    // Agent (qwen agent/agent.ts:885 `Kind.Agent`): BLOCKED in plan mode. Agent
    // is neither a mutator nor read-only; spawning a subagent could mutate, so
    // plan mode blocks it.
    fn kind(&self) -> crate::approvals::Kind {
        crate::approvals::Kind::Agent
    }

    fn spec(&self) -> ToolSpec {
        // The description scaffold, VERBATIM from qwen's `tools/agent/agent.ts`
        // `baseDescription`, with the live available-subagents block spliced in.
        // P4b restores the ONE `run_in_background` usage bullet; the
        // worktree/isolation note stays TRIMMED (deferred). qwen's
        // `${ToolNames.*}` interpolations resolve to Suspenders' own tool names.
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
- You can optionally set `run_in_background: true` to run the agent in the background. You will be notified when it completes. Use this when you have genuinely independent work to do in parallel and don't need the agent's results before you can proceed.

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

        // The dynamic schema: `description`/`prompt`/`subagent_type`/
        // `run_in_background` (qwen's initial schema, P4b restoring the boolean
        // `run_in_background`; the `isolation`/`worktree` property stays TRIMMED,
        // deferred). The `subagent_type` enum is the registry's names when
        // non-empty.
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
                    "run_in_background": {
                        "type": "boolean",
                        "description": "Set to true to run this agent in the background. You will be notified when it completes.",
                    },
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
            .ok_or_else(|| "Parameter \"description\" must be a non-empty string.".to_string())?
            .to_string();

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

        // Whether the model asked to background this agent (P4b/4c, ADR-0063).
        // Schema guarantees a boolean when present; absent defaults to false (the
        // foreground path). Only `true` takes the background branch.
        let run_in_background = input
            .get("run_in_background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Reach the host through the SubagentSpawner capability (F4, ADR-0061).
        // `model: None` defers the per-subagent Model to the def's own choice.
        let request = SubagentRequest {
            subagent_type,
            prompt,
            model: None,
        };

        if run_in_background {
            // Background (4c): launch a detached child Run and return immediately
            // with the minted task id. The parent does NOT park - it carries on,
            // and a `<task-notification>` reaches its next Run when the child
            // settles. A launch failure folds the verbatim string as an error.
            return match ctx
                .caps
                .subagents
                .spawn_background(request, description)
                .await
            {
                Ok(task_id) => Ok(background_launch_string(&task_id)),
                Err(reason) => Err(reason),
            };
        }

        match ctx.caps.subagents.spawn(request).await {
            Ok(result) => Ok(format_result(result)),
            // A launch failure (unknown type, unresolvable Model, degraded host):
            // fold the verbatim string into an error result.
            Err(reason) => Err(reason),
        }
    }
}

/// The background-launch content string (qwen's `tools/agent/agent.ts` ~2049,
/// the `Background agent launched successfully.` block), TRIMMED of the deferred
/// lines: the `output_file:` line and the `read_file`/`shell tail`-on-the-output
/// tail (there is no live-output file yet, ADR-0063), plus the `send_message`
/// clause of the agentId line (send_message is not ported). The `task_stop`
/// reference stays - that tool IS ported (4d). qwen's em-dashes are hyphenated to
/// the house style. `task_id` is the minted `{subagent_type}-{n}`.
fn background_launch_string(task_id: &str) -> String {
    format!(
        "Background agent launched successfully.\n\
         agentId: {task_id} (internal ID - do not mention to the user. Use task_stop to cancel.)\n\
         The agent is working in the background. You will be notified automatically when it completes.\n\
         Do not duplicate this agent's work - avoid working with the same files or topics it is using. Work on non-overlapping tasks, or briefly tell the user what you launched and end your response."
    )
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
#[path = "../../tests/tools/agent.rs"]
mod tests;
