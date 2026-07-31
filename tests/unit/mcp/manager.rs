use super::*;

#[tokio::test]
async fn an_empty_map_yields_an_empty_manager_and_no_tools() {
    let (manager, tools) = McpManager::connect(&BTreeMap::new()).await;
    assert_eq!(manager.conn_count(), 0);
    assert!(manager.failures().is_empty());
    assert!(tools.is_empty());
}

/// A tiny [`McpConn`] the assembly test can hand a real (if inert) conn, so
/// two `Ok` servers each contribute a conn + a tool without a live server.
struct StubConn;

#[async_trait::async_trait]
impl McpConn for StubConn {
    async fn call_tool(&self, _tool: &str, _arguments: Value) -> Result<McpCallResult, McpError> {
        Ok(McpCallResult {
            content: vec![],
            is_error: false,
        })
    }
}

fn ok_server(server: &str, tool: &str) -> Result<ServerAttach, String> {
    let conn: Arc<dyn McpConn> = Arc::new(StubConn);
    let mcp_tool = crate::mcp::adapter::McpTool::new(
        crate::mcp::adapter::McpToolInfo::new(
            server,
            tool,
            String::new(),
            Value::Object(Default::default()),
        ),
        Arc::clone(&conn),
        None,
    );
    Ok((conn, vec![Box::new(mcp_tool)]))
}

#[test]
fn assemble_lets_two_ok_servers_both_contribute_a_conn_and_tools() {
    let attached = vec![
        ("alpha", ok_server("alpha", "one")),
        ("beta", ok_server("beta", "two")),
    ];
    let (conns, tools, failures) = assemble(attached);
    assert_eq!(conns.len(), 2);
    assert_eq!(tools.len(), 2);
    assert!(failures.is_empty());
    // Deterministic assembly: the tools land in the input (server-name-sorted)
    // order, NOT completion order.
    assert_eq!(tools[0].spec().name, "mcp__alpha__one");
    assert_eq!(tools[1].spec().name, "mcp__beta__two");
}

#[test]
fn assemble_records_an_err_server_as_a_failure_and_keeps_the_ok_one() {
    // Input order is server-name-sorted; the failure list preserves it, so an
    // Err between two Oks lands deterministically keyed by name.
    let attached = vec![
        ("alpha", ok_server("alpha", "one")),
        ("beta", Err("boom".to_string())),
        ("gamma", ok_server("gamma", "three")),
    ];
    let (conns, tools, failures) = assemble(attached);
    assert_eq!(conns.len(), 2);
    assert_eq!(tools.len(), 2);
    assert_eq!(failures, vec![("beta".to_string(), "boom".to_string())]);
}
