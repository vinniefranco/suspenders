use super::*;

#[test]
fn parse_single_page() {
    assert_eq!(
        parse_page_range("5"),
        Some(PageRange {
            first: 5,
            last: Some(5)
        })
    );
}

#[test]
fn parse_closed_range_with_optional_whitespace() {
    assert_eq!(
        parse_page_range("1-10"),
        Some(PageRange {
            first: 1,
            last: Some(10)
        })
    );
    assert_eq!(
        parse_page_range(" 1 - 5 "),
        Some(PageRange {
            first: 1,
            last: Some(5)
        })
    );
}

#[test]
fn parse_open_ended_range() {
    assert_eq!(
        parse_page_range("3-"),
        Some(PageRange {
            first: 3,
            last: None
        })
    );
}

#[test]
fn parse_rejects_garbage_zero_and_inverted() {
    assert_eq!(parse_page_range(""), None);
    assert_eq!(parse_page_range("5abc"), None);
    assert_eq!(parse_page_range("1-2-3"), None);
    assert_eq!(parse_page_range("1.5"), None);
    assert_eq!(parse_page_range("0"), None);
    assert_eq!(parse_page_range("5-1"), None);
    assert_eq!(parse_page_range("2000000"), None);
}

#[test]
fn classify_password_corrupt_and_generic_failures() {
    assert!(matches!(
        classify(String::new(), "Command Line Error: Incorrect password", 1),
        PdfText::Failed(m) if m.contains("password-protected")
    ));
    assert!(matches!(
        classify(String::new(), "Error: PDF file is damaged", 1),
        PdfText::Failed(m) if m == "PDF file is corrupted or invalid."
    ));
    assert!(matches!(
        classify(String::new(), "some other error", 1),
        PdfText::Failed(m) if m == "pdftotext failed: some other error"
    ));
    assert!(matches!(
        classify(String::new(), "   ", 1),
        PdfText::Failed(m) if m == "pdftotext failed: (no stderr)"
    ));
}

#[test]
fn classify_empty_output_is_an_images_only_failure() {
    assert!(matches!(
        classify("   \n".to_string(), "", 0),
        PdfText::Failed(m) if m.contains("only images")
    ));
}

#[test]
fn classify_truncates_past_the_char_budget_with_the_verbatim_marker() {
    let big = "a".repeat(MAX_PDF_TEXT_OUTPUT_CHARS + 100);
    match classify(big, "", 0) {
        PdfText::Ok(text) => {
            assert!(text.contains(
                "... [text truncated at 100000 characters. Use the 'pages' parameter to read specific page ranges.]"
            ));
        }
        PdfText::Failed(_) => panic!("expected Ok"),
    }
}

#[test]
fn classify_passes_short_output_through() {
    assert!(matches!(
        classify("hello world".to_string(), "", 0),
        PdfText::Ok(t) if t == "hello world"
    ));
}

#[tokio::test]
async fn extract_reports_missing_binary_verbatim() {
    // pdftotext may or may not be on PATH; if absent, we get the verbatim
    // "not installed" wording. If present on a nonexistent file, we still
    // get a Failed (nonzero exit), so only assert the Missing wording when
    // the binary is genuinely absent.
    let missing = tokio::process::Command::new("pdftotext")
        .arg("-v")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_err();
    if missing {
        let out = extract_text(std::path::Path::new("/nope.pdf"), None).await;
        assert!(matches!(out, PdfText::Failed(m) if m.contains("poppler-utils")));
    }
}
