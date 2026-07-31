use super::*;

fn ok(content: Vec<McpBlock>) -> McpCallResult {
    McpCallResult {
        content,
        is_error: false,
    }
}

#[test]
fn text_block_is_verbatim() {
    assert_eq!(
        render(&ok(vec![McpBlock::Text("hello".into())]), "read_file"),
        Ok("hello".to_string())
    );
}

#[test]
fn media_block_is_a_placeholder_line_naming_the_tool() {
    let result = ok(vec![McpBlock::Media {
        kind: "image".into(),
        mime: "image/png".into(),
    }]);
    assert_eq!(
        render(&result, "screenshot"),
        Ok(
            "[Tool 'screenshot' provided the following image data with mime-type: image/png]"
                .to_string()
        )
    );
}

#[test]
fn embedded_text_resource_yields_its_text() {
    let result = ok(vec![McpBlock::EmbeddedResource {
        text: Some("file body".into()),
        mime: Some("text/plain".into()),
    }]);
    assert_eq!(render(&result, "read_file"), Ok("file body".to_string()));
}

#[test]
fn embedded_blob_resource_is_a_placeholder_line_naming_the_tool() {
    let result = ok(vec![McpBlock::EmbeddedResource {
        text: None,
        mime: Some("application/pdf".into()),
    }]);
    assert_eq!(
        render(&result, "fetch"),
        Ok(
            "[Tool 'fetch' provided the following embedded resource with mime-type: application/pdf]"
                .to_string()
        )
    );
}

#[test]
fn resource_link_is_a_labelled_line() {
    let result = ok(vec![McpBlock::ResourceLink {
        label: "report".into(),
        uri: "file:///r.txt".into(),
    }]);
    assert_eq!(
        render(&result, "list"),
        Ok("Resource Link: report at file:///r.txt".to_string())
    );
}

#[test]
fn parts_join_on_newlines() {
    let result = ok(vec![
        McpBlock::Text("line one".into()),
        McpBlock::Text("line two".into()),
    ]);
    assert_eq!(
        render(&result, "tool"),
        Ok("line one\nline two".to_string())
    );
}

#[test]
fn empty_result_is_the_empty_string() {
    assert_eq!(render(&ok(vec![]), "tool"), Ok(String::new()));
}

#[test]
fn an_error_result_comes_back_err() {
    let result = McpCallResult {
        content: vec![McpBlock::Text("boom".into())],
        is_error: true,
    };
    assert_eq!(render(&result, "tool"), Err("boom".to_string()));
}

#[test]
fn an_empty_error_result_gets_a_non_empty_fallback() {
    let result = McpCallResult {
        content: vec![],
        is_error: true,
    };
    assert_eq!(
        render(&result, "tool"),
        Err("the MCP tool reported an error with no message".to_string())
    );
}
