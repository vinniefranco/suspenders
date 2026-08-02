use super::*;

#[test]
fn parse_callback_query_pulls_code_and_state() {
    let parsed = parse_callback_query("code=the%2Dcode&state=st8&extra=1");
    assert_eq!(parsed.code.as_deref(), Some("the-code"));
    assert_eq!(parsed.state.as_deref(), Some("st8"));
    assert!(parsed.error.is_none());
}

#[test]
fn parse_callback_query_pulls_an_error() {
    let parsed = parse_callback_query("error=access_denied");
    assert_eq!(parsed.error.as_deref(), Some("access_denied"));
    assert!(parsed.code.is_none());
}
