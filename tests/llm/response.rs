use super::*;

#[test]
fn error_constructor_sets_error_stop_reason() {
    let r = Response::error("boom");
    assert_eq!(r.stop_reason, StopReason::Error);
    assert_eq!(r.error.as_deref(), Some("boom"));
    assert!(r.content.is_empty());
}

#[test]
fn error_with_keeps_partial_content() {
    let content = vec![ContentBlock::text("partial")];
    let r = Response::error_with("boom", content.clone(), Usage::default());
    assert_eq!(r.content, content);
    assert_eq!(r.stop_reason, StopReason::Error);
}

// ---- retryable classifier (ADR-0030) ----

#[test]
fn only_the_malformed_tool_call_class_is_retryable() {
    // The server's constrained-decoding miss, as stream.rs wraps it.
    assert!(is_retryable_error(
        "api_stream_error: Failed to generate a valid tool call"
    ));
}

#[test]
fn context_exceeded_transport_errors_and_empty_are_not_retryable() {
    assert!(!is_retryable_error(
        "api_stream_error: Context size has been exceeded"
    ));
    assert!(!is_retryable_error("request_failed: connection refused"));
    assert!(!is_retryable_error(""));
}

#[test]
fn is_retryable_reads_the_response_error_and_defaults_to_false_when_absent() {
    let malformed = Response::error("api_stream_error: Failed to generate a valid tool call");
    assert!(malformed.is_retryable());

    let context = Response::error("api_stream_error: Context size has been exceeded");
    assert!(!context.is_retryable());

    // No error string set: fail loud by default.
    assert!(!Response::default().is_retryable());
}

#[test]
fn stop_reason_display_parity() {
    assert_eq!(StopReason::EndTurn.to_string(), "end_turn");
    assert_eq!(StopReason::ToolUse.to_string(), "tool_use");
    assert_eq!(StopReason::MaxTokens.to_string(), "max_tokens");
    assert_eq!(StopReason::StopSequence.to_string(), "stop_sequence");
    assert_eq!(StopReason::Error.to_string(), "error");
    assert_eq!(StopReason::Unknown.to_string(), "unknown");
}

#[test]
fn stop_reason_serde_parity() {
    assert_eq!(
        serde_json::to_string(&StopReason::EndTurn).unwrap(),
        "\"end_turn\""
    );
    let r: StopReason = serde_json::from_str("\"tool_use\"").unwrap();
    assert_eq!(r, StopReason::ToolUse);
    // Unknown wire value folds into Unknown.
    let r: StopReason = serde_json::from_str("\"something_new\"").unwrap();
    assert_eq!(r, StopReason::Unknown);
}

#[test]
fn every_wire_stop_reason_embeds_into_the_canonical_vocabulary_name_for_name() {
    // The ONE wire-to-canonical seam (ADR-0069): total over the wire enum,
    // and name-preserving, so no reason changes spelling crossing it.
    let wire = [
        StopReason::EndTurn,
        StopReason::ToolUse,
        StopReason::MaxTokens,
        StopReason::StopSequence,
        StopReason::Error,
        StopReason::Unknown,
    ];
    for w in wire {
        let name = w.to_string();
        let canonical: crate::stop_reason::StopReason = w.into();
        assert_eq!(canonical.as_str(), name);
    }
}
