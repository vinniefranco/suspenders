use super::*;
use crate::voice::InteractionMode;

// ---- system_prompt: the qwen v0.21.4 base-template port ----

#[test]
fn system_prompt_tells_the_model_to_maintain_its_task_list() {
    let prompt = system_prompt(InteractionMode::Interactive);
    assert!(prompt.contains("todo_write"));
    assert!(prompt.contains("in_progress"));
}

#[test]
fn system_prompt_carries_the_faithful_reporting_rule() {
    let prompt = system_prompt(InteractionMode::Interactive);
    // qwen's "Report outcomes faithfully" bullet: never fake a green result.
    assert!(prompt.contains("Report outcomes faithfully"));
    assert!(prompt.contains("never suppress failing checks to manufacture a green result"));
}

#[test]
fn system_prompt_mandates_convention_and_library_verification() {
    let prompt = system_prompt(InteractionMode::Interactive);
    assert!(prompt.contains("# Core Mandates"));
    // Do not assume a library/framework is present; verify established usage.
    assert!(prompt.contains("NEVER assume a library/framework is available"));
    assert!(prompt.contains("Verify its established usage within the project"));
    // Comments: default to none, only for the hidden why, never narrating.
    assert!(prompt.contains("**Comments:** Default to none"));
    assert!(prompt.contains("*NEVER* talk to the user or describe your changes"));
}

#[test]
fn system_prompt_carries_the_software_engineering_workflow() {
    let prompt = system_prompt(InteractionMode::Interactive);
    assert!(prompt.contains("## Software Engineering Tasks"));
    assert!(prompt.contains("**Plan:**"));
    assert!(prompt.contains("**Verify (Tests):**"));
    assert!(prompt.contains("**Verify (Standards):**"));
    // Three-similar-lines over premature abstraction, ported verbatim.
    assert!(prompt.contains("Three similar lines of code is better than a premature abstraction"));
}

#[test]
fn system_prompt_prefers_dedicated_tools_over_the_shell() {
    let prompt = system_prompt(InteractionMode::Interactive);
    // The "Using Your Tools" dedicated-tool mapping, with suspenders wire names.
    assert!(prompt.contains("## Using Your Tools"));
    assert!(prompt.contains("To read files use 'read_file' instead of cat, head, tail, or sed"));
    assert!(prompt.contains("To search the content of files, use 'grep_search'"));
    assert!(prompt.contains("Reserve using the 'run_shell_command'"));
}

#[test]
fn system_prompt_sets_a_concise_no_chitchat_tone() {
    let prompt = system_prompt(InteractionMode::Interactive);
    assert!(prompt.contains("## Tone and Style (CLI Interaction)"));
    assert!(prompt.contains("**No Chitchat:**"));
}

#[test]
fn system_prompt_teaches_by_worked_example() {
    let prompt = system_prompt(InteractionMode::Interactive);
    assert!(prompt.contains("# Examples (Illustrating Tone and Workflow)"));
    // The terse knowledge answer and a bracketed tool-call stage direction.
    assert!(prompt.contains("1 + 2"));
    assert!(prompt.contains("[tool_call: run_shell_command"));
    assert!(prompt.contains("[tool_call: glob for pattern"));
}

#[test]
fn system_prompt_carries_task_management_todo_discipline() {
    let prompt = system_prompt(InteractionMode::Interactive);
    assert!(prompt.contains("# Task Management"));
    // The single-in_progress rule and the todo_id delegation guidance.
    assert!(prompt.contains("Keep at most one item in_progress"));
    assert!(prompt.contains("pass the matching Todo ID as `todo_id`"));
}

#[test]
fn system_prompt_ports_the_operational_and_care_sections() {
    let prompt = system_prompt(InteractionMode::Interactive);
    // The ported qwen sections that give the prompt its operational depth.
    assert!(prompt.contains("# Operational Guidelines"));
    assert!(prompt.contains("## Communicating With the User"));
    assert!(prompt.contains("## Security and Safety Rules"));
    assert!(prompt.contains("# Executing actions with care"));
    assert!(prompt.contains("# Final Reminder"));
    // The New Applications skill hand-off and Subagent Delegation guidance.
    assert!(prompt.contains("## New Applications"));
    assert!(prompt.contains("the 'skill' tool with skill=\"new-app\""));
    assert!(prompt.contains("**Subagent Delegation:**"));
    assert!(prompt.contains("subagent_type=Explore"));
}

#[test]
fn system_prompt_localizes_identity_and_agents_md_not_qwen() {
    let prompt = system_prompt(InteractionMode::Interactive);
    assert!(prompt.contains(
        "You are Suspenders, an interactive CLI agent, specializing in software engineering tasks."
    ));
    // AGENTS.md, not QWEN.md, in the Executing-actions section.
    assert!(prompt.contains("durable instructions like AGENTS.md files"));
    // No leakage of qwen identity.
    for banned in ["Qwen", "Alibaba", "QWEN.md"] {
        assert!(
            !prompt.contains(banned),
            "system prompt should not reference {banned:?}"
        );
    }
}

#[test]
fn system_prompt_headless_mode_sets_the_role_and_forbids_questions() {
    let prompt = system_prompt(InteractionMode::Headless);
    // Headless role in the identity sentence.
    assert!(prompt.contains("You are Suspenders, a non-interactive CLI agent,"));
    // The headless question guidance appears (twice - under Using Your Tools
    // and in the trailing reminder): never ask, report the blocker instead.
    assert!(prompt.contains("This is a non-interactive, single-turn run"));
    assert!(prompt.contains("report the blocker as the final result"));
    // The trailing interaction-mode reminder carries the same guidance.
    assert!(
        prompt
            .trim_end()
            .ends_with("report the blocker as the final result.")
    );
}

#[test]
fn system_prompt_interactive_mode_permits_asking_questions() {
    let prompt = system_prompt(InteractionMode::Interactive);
    assert!(prompt.contains(
        "Use 'ask_user_question' when you need clarification or want to validate assumptions."
    ));
    assert!(prompt.contains("Never include time estimates in options."));
    // The trailing interaction-mode reminder repeats the guidance.
    assert!(prompt.contains("Interaction mode reminder: Use 'ask_user_question'"));
}

#[test]
fn system_prompt_mandates_absolute_paths() {
    let prompt = system_prompt(InteractionMode::Interactive);
    // The ported file tools require absolute paths (qwen's contract).
    assert!(prompt.contains("Always use absolute paths when referring to files"));
    assert!(prompt.contains("Relative paths are not supported"));
}

#[test]
fn system_prompt_emits_the_git_section_inside_a_git_repo() {
    // The test harness runs from the suspenders repo root (a git repo), so the
    // cwd-gated Git Repository section is present.
    let prompt = system_prompt(InteractionMode::Interactive);
    assert!(prompt.contains("# Git Repository"));
    assert!(prompt.contains("## Git as Source of Truth"));
}

#[test]
fn system_prompt_carries_a_sandbox_status_section() {
    // Exactly one of the three sandbox branches is emitted, keyed on SANDBOX.
    let prompt = system_prompt(InteractionMode::Interactive);
    let has_sandbox = prompt.contains("# Outside of Sandbox")
        || prompt.contains("# Sandbox")
        || prompt.contains("# macOS Seatbelt");
    assert!(has_sandbox, "no sandbox status section emitted");
}

#[test]
fn system_prompt_has_no_em_or_en_dashes() {
    let prompt = system_prompt(InteractionMode::Interactive);
    assert!(!prompt.contains('\u{2014}'), "em-dash in system prompt");
    assert!(!prompt.contains('\u{2013}'), "en-dash in system prompt");
}

// ---- please_continue/0 ----

#[test]
fn please_continue_is_the_next_speaker_nudge() {
    // The unstamped user nudge injected on a `model` next-speaker verdict
    // (ADR-0043): short, plain, no bracketed-marker framing (it is an
    // ordinary user turn, not a Voice marker).
    let nudge = please_continue();
    assert_eq!(nudge, "Please continue.");
    assert!(!nudge.starts_with('['));
    assert!(!nudge.contains('\u{2014}')); // em-dash
    assert!(!nudge.contains('\u{2013}')); // en-dash
}

// ---- compaction_prompt/0 ----

#[test]
fn compaction_prompt_frames_analysis_then_state_snapshot() {
    let prompt = compaction_prompt();
    // qwen v0.21.4: an <analysis> scratchpad (stripped downstream) then the
    // EXACT <state_snapshot> XML envelope.
    assert!(prompt.starts_with(
        "You are the component that summarizes a conversation when its context window is about to overflow."
    ));
    assert!(prompt.contains("wrap your reasoning in an <analysis> block"));
    assert!(prompt.contains("produce the final summary as the EXACT XML structure below"));
    assert!(prompt.contains("<state_snapshot>"));
    assert!(prompt.trim_end().ends_with("</state_snapshot>"));
}

#[test]
fn compaction_prompt_carries_all_nine_state_snapshot_sections() {
    let prompt = compaction_prompt();
    for section in [
        "<primary_request_and_intent>",
        "<key_technical_concepts>",
        "<files_and_code_sections>",
        "<errors_and_fixes>",
        "<problem_solving>",
        "<all_user_messages>",
        "<pending_tasks>",
        "<current_work>",
        "<next_step>",
    ] {
        assert!(
            prompt.contains(section),
            "compaction prompt is missing the {section:?} section"
        );
    }
}

// ---- compaction_facts/2 ----

#[test]
fn compaction_facts_carries_the_verbatim_original_task() {
    let facts = compaction_facts(
        Some("Fix the flaky test in user_test.exs"),
        &FileOps::default(),
    );
    assert!(facts.contains("Fix the flaky test in user_test.exs"));
}

#[test]
fn compaction_facts_lists_accumulated_read_and_modified_files() {
    let facts = compaction_facts(
        Some("original task"),
        &FileOps {
            read_files: vec!["lib/a.ex".into(), "lib/b.ex".into()],
            modified_files: vec!["lib/c.ex".into()],
        },
    );
    assert!(facts.contains("lib/a.ex"));
    assert!(facts.contains("lib/b.ex"));
    assert!(facts.contains("lib/c.ex"));
}

#[test]
fn compaction_facts_handles_absent_task_and_empty_ops() {
    let facts = compaction_facts(None, &FileOps::default());
    assert!(!facts.is_empty());
}

// ---- Marker::ToolError ----

#[test]
fn tool_error_marker_is_a_short_bracketed_marker() {
    let marker = Marker::ToolError.text();
    assert!(marker.starts_with('['));
    assert!(marker.ends_with(']'));
    assert!(marker.chars().count() < 40);
    assert!(marker.contains("error"));
    assert!(!marker.contains('\u{2014}')); // em-dash
    assert!(!marker.contains('\u{2013}')); // en-dash
}

// ---- Marker::OrphanedCall ----

#[test]
fn orphaned_call_answer_is_a_bracketed_error_telling_the_model_to_reissue() {
    let answer = Marker::OrphanedCall.text();
    assert!(answer.starts_with('['));
    assert!(answer.ends_with(']'));
    assert!(answer.contains("model switch"));
    assert!(answer.contains("re-issue"));
    assert!(!answer.contains('\u{2014}')); // em-dash
    assert!(!answer.contains('\u{2013}')); // en-dash
}

// ---- Marker::text: every fixed marker is bracketed and dash-free ----

#[test]
fn every_fixed_marker_is_bracketed_and_dash_free() {
    for marker in [
        Marker::RunLimit,
        Marker::LoopStall,
        Marker::RunStopped,
        Marker::Truncation,
        Marker::EmptyResponse,
        Marker::RunCancelled,
        Marker::RunFailed,
        Marker::ToolError,
        Marker::OrphanedCall,
        Marker::TruncatedCallReissue,
        Marker::CommandDenied,
    ] {
        let text = marker.text();
        assert!(text.starts_with('['), "{marker:?} not bracketed: {text:?}");
        assert!(text.ends_with(']'), "{marker:?} not bracketed: {text:?}");
        assert!(!text.contains('\u{2014}'), "{marker:?} has an em-dash");
        assert!(!text.contains('\u{2013}'), "{marker:?} has an en-dash");
    }
}

// ---- Marker::completing: the run-close classifier ----

#[test]
fn completing_maps_each_stop_reason_to_its_close_marker() {
    use crate::stop_reason::StopReason;
    // The Run Limit and loop-stall stops name themselves.
    assert_eq!(Marker::completing(&StopReason::RunLimit), Marker::RunLimit);
    assert_eq!(
        Marker::completing(&StopReason::RunLimitStuck),
        Marker::LoopStall
    );
    // Every other completion closes as an after-Pass stop.
    assert_eq!(Marker::completing(&StopReason::EndTurn), Marker::RunStopped);
    assert_eq!(
        Marker::completing(&StopReason::MaxTokens),
        Marker::RunStopped
    );
}

// ---- Marker::is_run_close ----

#[test]
fn is_run_close_recognizes_the_run_close_markers_only() {
    assert!(Marker::is_run_close(Marker::RunLimit.text()));
    assert!(Marker::is_run_close(Marker::LoopStall.text()));
    assert!(Marker::is_run_close(Marker::EmptyResponse.text()));
    // Answers (not run-close markers) and plain text are not close markers.
    assert!(!Marker::is_run_close(Marker::OrphanedCall.text()));
    assert!(!Marker::is_run_close(Marker::CommandDenied.text()));
    assert!(!Marker::is_run_close("some model reply"));
}
