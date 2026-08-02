use super::*;

// ---- system_prompt/0 ----

#[test]
fn system_prompt_tells_the_model_to_maintain_its_task_list() {
    assert!(system_prompt().contains("todo_write"));
    assert!(system_prompt().contains("in_progress"));
}

#[test]
fn system_prompt_tells_the_model_to_fix_the_code_under_test() {
    assert!(system_prompt().contains("Fix the code under test, not the tests"));
}

#[test]
fn system_prompt_bans_invented_line_numbers_but_allows_quoting_tool_output() {
    let prompt = system_prompt();
    assert!(prompt.contains("name the file and the function - never a line number"));
    assert!(prompt.contains("Quoting a line number printed by a compiler or test error is fine"));
}

#[test]
fn system_prompt_sequences_new_builds_without_capping_scope() {
    let prompt = system_prompt();
    // Sequencing: verified increments, compile errors fixed first.
    assert!(prompt.contains("smallest slice that compiles and passes at least one test"));
    assert!(prompt.contains("a tree that will not build makes every other step blind"));
    // Guard against the cycle-002 over-read ("do less"): the rule must
    // explicitly demand full spec coverage.
    assert!(prompt.contains("until every behavior in the spec is covered"));
}

#[test]
fn system_prompt_steers_off_piping_command_output_through_head() {
    let prompt = system_prompt();
    assert!(prompt.contains("Run commands whole"));
    assert!(prompt.contains("never pipe their output through head, tail, or wc"));
    // Both reasons: the harness truncates, and pipefail runs an
    // early-closing consumer into a spurious failure.
    assert!(prompt.contains("truncates long output while keeping the exit code"));
    assert!(prompt.contains("pipefail"));
    assert!(prompt.contains("make a passing run report failure"));
}

#[test]
fn system_prompt_steers_toward_quiet_flags_for_builds_and_tests() {
    let prompt = system_prompt();
    // The sanctioned way to shorten output (piping is the forbidden way):
    // the runner's own quiet flags, with a concrete example, and the
    // reassurance that failures are not silenced by them.
    assert!(prompt.contains("quiet flags"));
    assert!(prompt.contains("--status-level fail"));
    assert!(prompt.contains("failures still print in full"));
}

#[test]
fn system_prompt_mandates_convention_and_library_verification() {
    let prompt = system_prompt();
    assert!(prompt.contains("Core Mandates"));
    // Do not assume a library is present; verify it is already used.
    assert!(prompt.contains("NEVER assume a library is available"));
    assert!(prompt.contains("Verify it is already used in the project"));
    // Comments: sparingly, the why not the what, never narrating changes.
    assert!(prompt.contains("Add code comments sparingly"));
    assert!(prompt.contains("*NEVER* talk to the user or describe your changes"));
}

#[test]
fn system_prompt_carries_the_understand_verify_workflow() {
    let prompt = system_prompt();
    assert!(prompt.contains("Understand"));
    assert!(prompt.contains("Verify"));
    // The Understand step points the model at inline exploration - glob,
    // grep_search, list_directory, read_file - not a delegated Scout.
    assert!(prompt.contains("Use 'glob' to find files"));
    assert!(prompt.contains("'grep_search' to search for symbols"));
    assert!(!prompt.contains("explore tool"));
    // Faithful reporting: never claim a green suite that is not green.
    assert!(prompt.contains("never claim green when it is not"));
}

#[test]
fn system_prompt_sets_a_concise_no_chitchat_tone() {
    let prompt = system_prompt();
    assert!(prompt.contains("Tone and Style (CLI Interaction)"));
    assert!(prompt.contains("No chitchat"));
}

#[test]
fn system_prompt_teaches_by_worked_example() {
    let prompt = system_prompt();
    assert!(prompt.contains("# Examples (Illustrating Tone and Workflow)"));
    // The terse knowledge answer and a bracketed tool-call stage direction.
    assert!(prompt.contains("1 + 2"));
    assert!(prompt.contains("[tool_call: grep_search"));
}

#[test]
fn system_prompt_carries_task_management_todo_discipline() {
    let prompt = system_prompt();
    assert!(prompt.contains("# Task Management"));
    // todo_write with the {content,status} shape and the single-in_progress
    // rule ported from qwen's discipline.
    assert!(prompt.contains("'pending', 'in_progress', or 'completed'"));
    assert!(prompt.contains("exactly one todo 'in_progress' at a time"));
    assert!(prompt.contains("mark todos as completed as soon as you are done"));
}

#[test]
fn system_prompt_ports_the_operational_and_care_sections() {
    let prompt = system_prompt();
    // The ported qwen sections that give the prompt its operational depth.
    assert!(prompt.contains("# Operational Guidelines"));
    assert!(prompt.contains("## Security and Safety Rules"));
    assert!(prompt.contains("# Executing actions with care"));
    assert!(prompt.contains("# Git Repository"));
    assert!(prompt.contains("## Git as Source of Truth"));
    assert!(prompt.contains("# Final Reminder"));
    // The New Applications workflow and Subagent Delegation guidance, restored
    // to full qwen fidelity now that the 'agent' tool exists.
    assert!(prompt.contains("## New Applications"));
    assert!(prompt.contains("**Subagent Delegation:**"));
    assert!(prompt.contains("subagent_type=Explore"));
}

#[test]
fn system_prompt_keeps_suspenders_identity_not_qwen() {
    let prompt = system_prompt();
    assert!(prompt.contains("You are Suspenders, an interactive CLI coding agent"));
    // No leakage of qwen-code infrastructure suspenders lacks.
    for banned in [
        "Qwen",
        "Alibaba",
        "QWEN.md",
        "tool_search",
        "save_memory",
        "auto memory",
        "sandbox",
    ] {
        assert!(
            !prompt.contains(banned),
            "system prompt should not reference {banned:?}"
        );
    }
}

#[test]
fn system_prompt_mandates_absolute_paths() {
    let prompt = system_prompt();
    // The ported file tools require absolute paths (qwen's contract), so the
    // prompt must instruct the model to construct them, not relative paths.
    assert!(prompt.contains("you must construct the full absolute path"));
    assert!(prompt.contains("Relative paths are not supported"));
    assert!(!prompt.contains("Absolute paths are not used"));
}

#[test]
fn system_prompt_has_no_em_or_en_dashes() {
    let prompt = system_prompt();
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
fn compaction_prompt_demands_all_six_fixed_sections() {
    let prompt = compaction_prompt();
    for section in [
        "Task",
        "Completed",
        "In progress",
        "Decisions made",
        "Key identifiers",
        "Next step",
    ] {
        assert!(
            prompt.contains(section),
            "compaction prompt is missing the {section:?} section"
        );
    }
}

#[test]
fn compaction_prompt_shows_the_markdown_skeleton() {
    let prompt = compaction_prompt();
    for heading in [
        "## Task",
        "## Completed",
        "## In progress",
        "## Decisions made",
        "## Key identifiers",
        "## Next step",
    ] {
        assert!(
            prompt.contains(heading),
            "compaction prompt is missing the heading {heading:?}"
        );
    }
}

#[test]
fn compaction_prompt_names_the_mechanical_identifiers() {
    let prompt = compaction_prompt();
    assert!(prompt.contains("file path"));
    assert!(prompt.contains("function name"));
    assert!(prompt.contains("error message"));
    assert!(prompt.contains("command"));
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
    use crate::session::log::StopReason;
    // The Run Limit and loop-stall stops name themselves.
    assert_eq!(Marker::completing(StopReason::RunLimit), Marker::RunLimit);
    assert_eq!(
        Marker::completing(StopReason::RunLimitStuck),
        Marker::LoopStall
    );
    // Every other completion closes as an after-Pass stop.
    assert_eq!(Marker::completing(StopReason::EndTurn), Marker::RunStopped);
    assert_eq!(
        Marker::completing(StopReason::MaxTokens),
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
