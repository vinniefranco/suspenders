
use super::*;

#[test]
fn the_count_line_agrees_with_the_server_count() {
    assert_eq!(server_count_line(1), "1 server");
    assert_eq!(server_count_line(0), "0 servers");
    assert_eq!(server_count_line(3), "3 servers");
}

#[test]
fn the_tool_count_line_wraps_and_agrees() {
    assert_eq!(tool_count_line(1), "(1 tool)");
    assert_eq!(tool_count_line(0), "(0 tools)");
    assert_eq!(tool_count_line(2), "(2 tools)");
}
