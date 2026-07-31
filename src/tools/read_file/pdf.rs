//! PDF text extraction via the `pdftotext` subprocess - a port of qwen v0.16.0
//! `packages/core/src/utils/pdf.ts` (`parsePDFPageRange`, `extractPDFText`, the
//! `PDF_EXTRACTION_MAX_MB` and `MAX_PDF_TEXT_OUTPUT_CHARS` limits) reusing the
//! run_command spawn+timeout pattern. Every error string is copied verbatim so a
//! missing binary / password / corrupt / timeout reads exactly as qwen's.
//!
//! read_file.rs owns the native-PDF-vs-text decision (a PDF-capable Model with
//! no `pages` gets a Document block); this file only extracts text.

use std::process::Stdio;

/// The whole-extraction char budget (qwen `MAX_PDF_TEXT_OUTPUT_CHARS`).
const MAX_PDF_TEXT_OUTPUT_CHARS: usize = 100_000;
/// Upper bound on a page number forwarded to pdftotext (qwen `MAX_PDF_PAGE_NUMBER`).
const MAX_PDF_PAGE_NUMBER: u64 = 1_000_000;
/// Upper bound on the on-disk PDF size handed to the text-extraction path, in MB
/// (qwen `PDF_EXTRACTION_MAX_MB`).
pub const PDF_EXTRACTION_MAX_MB: f64 = 100.0;
/// The pdftotext timeout (qwen `extractPDFText` `timeout: 30000`).
const PDFTOTEXT_TIMEOUT_MS: u64 = 30_000;

/// A parsed 1-indexed inclusive page range (qwen's `{ firstPage, lastPage }`).
/// `last` is `None` for an open-ended `"3-"` range (qwen's `Infinity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRange {
    pub first: u64,
    pub last: Option<u64>,
}

/// Parse a page range string (qwen `parsePDFPageRange`): `"5"`, `"1-10"`, or the
/// open-ended `"3-"`. Optional whitespace around the hyphen is allowed. Returns
/// `None` on invalid input (non-numeric, zero, inverted, or past
/// `MAX_PDF_PAGE_NUMBER`).
pub fn parse_page_range(pages: &str) -> Option<PageRange> {
    let trimmed = pages.trim();
    if trimmed.is_empty() {
        return None;
    }
    let in_range = |n: u64| (1..=MAX_PDF_PAGE_NUMBER).contains(&n);

    // Open-ended: `<digits> -`.
    if let Some(rest) = trimmed.strip_suffix('-') {
        let first_str = rest.trim_end();
        if is_all_digits(first_str)
            && let Ok(first) = first_str.parse::<u64>()
            && in_range(first)
        {
            return Some(PageRange { first, last: None });
        }
        return None;
    }

    // Range: `<digits> - <digits>`.
    if let Some((a, b)) = split_hyphen(trimmed) {
        if is_all_digits(a)
            && is_all_digits(b)
            && let (Ok(first), Ok(last)) = (a.parse::<u64>(), b.parse::<u64>())
            && in_range(first)
            && in_range(last)
            && last >= first
        {
            return Some(PageRange {
                first,
                last: Some(last),
            });
        }
        return None;
    }

    // Single page.
    if is_all_digits(trimmed)
        && let Ok(page) = trimmed.parse::<u64>()
        && in_range(page)
    {
        return Some(PageRange {
            first: page,
            last: Some(page),
        });
    }
    None
}

fn is_all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

// Split on the single interior hyphen, allowing whitespace around it (qwen's
// `/^(\d+)\s*-\s*(\d+)$/`). Returns None when there is not exactly one hyphen.
fn split_hyphen(s: &str) -> Option<(&str, &str)> {
    let idx = s.find('-')?;
    if s[idx + 1..].contains('-') {
        return None;
    }
    Some((s[..idx].trim_end(), s[idx + 1..].trim_start()))
}

/// The outcome of a text extraction (qwen `PDFTextResult`).
pub enum PdfText {
    Ok(String),
    Failed(String),
}

/// Extract text from a PDF via `pdftotext -layout` (qwen `extractPDFText`). A
/// missing binary, a password/corrupt PDF, a timeout, and empty output each map
/// to the verbatim qwen error wording. Output past
/// `MAX_PDF_TEXT_OUTPUT_CHARS` is cut with the verbatim truncation marker.
pub async fn extract_text(abs: &std::path::Path, range: Option<PageRange>) -> PdfText {
    let mut args: Vec<String> = vec!["-layout".to_string()];
    if let Some(r) = range {
        args.push("-f".to_string());
        args.push(r.first.to_string());
        if let Some(last) = r.last {
            args.push("-l".to_string());
            args.push(last.to_string());
        }
    }
    // `--` ends options so a filename starting with `-` is not misread; `-`
    // writes extracted text to stdout.
    args.push("--".to_string());
    args.push(abs.to_string_lossy().into_owned());
    args.push("-".to_string());

    match run_pdftotext(&args).await {
        RunOutcome::Missing => PdfText::Failed(
            "pdftotext is not installed. Install poppler-utils to enable PDF text \
extraction (e.g. `apt-get install poppler-utils` or `brew install poppler`)."
                .to_string(),
        ),
        RunOutcome::TimedOut => PdfText::Failed(
            "pdftotext timed out after 30s. The PDF may be unusually large or \
complex; try the 'pages' parameter to narrow the range."
                .to_string(),
        ),
        RunOutcome::Ran {
            stdout,
            stderr,
            code,
        } => classify(stdout, &stderr, code),
    }
}

/// Turn a completed pdftotext run into a [`PdfText`] (qwen's post-run branches:
/// password/corrupt/failure detection, empty-output, then the char-cut).
fn classify(stdout: String, stderr: &str, code: i32) -> PdfText {
    if code != 0 {
        let lower = stderr.to_ascii_lowercase();
        if lower.contains("password") {
            return PdfText::Failed(
                "PDF is password-protected. Please provide an unprotected version.".to_string(),
            );
        }
        if lower.contains("damaged") || lower.contains("corrupt") || lower.contains("invalid") {
            return PdfText::Failed("PDF file is corrupted or invalid.".to_string());
        }
        let detail = if stderr.trim().is_empty() {
            "(no stderr)"
        } else {
            stderr.trim()
        };
        return PdfText::Failed(format!("pdftotext failed: {detail}"));
    }

    if stdout.trim().is_empty() {
        return PdfText::Failed(
            "pdftotext produced no text output. The PDF may contain only images.".to_string(),
        );
    }

    if stdout.chars().count() > MAX_PDF_TEXT_OUTPUT_CHARS {
        let head: String = stdout.chars().take(MAX_PDF_TEXT_OUTPUT_CHARS).collect();
        return PdfText::Ok(format!(
            "{head}\n\n... [text truncated at {MAX_PDF_TEXT_OUTPUT_CHARS} characters. \
Use the 'pages' parameter to read specific page ranges.]"
        ));
    }

    PdfText::Ok(stdout)
}

enum RunOutcome {
    Missing,
    TimedOut,
    Ran {
        stdout: String,
        stderr: String,
        code: i32,
    },
}

impl RunOutcome {
    /// A ran-but-failed outcome carrying the execution error as stderr and a
    /// non-zero exit (both the spawn-failure and the wait-failure arms yield
    /// this shape).
    fn exec_failure(err: std::io::Error) -> RunOutcome {
        RunOutcome::Ran {
            stdout: String::new(),
            stderr: format!("pdftotext execution failed: {err}"),
            code: 1,
        }
    }
}

/// Spawn `pdftotext` with the run_command timeout pattern. A spawn ENOENT means
/// the binary is not installed (qwen's `isPdftotextAvailable` false path).
async fn run_pdftotext(args: &[String]) -> RunOutcome {
    let mut cmd = tokio::process::Command::new("pdftotext");
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return RunOutcome::Missing,
        Err(err) => return RunOutcome::exec_failure(err),
    };

    let wait = child.wait_with_output();
    match tokio::time::timeout(std::time::Duration::from_millis(PDFTOTEXT_TIMEOUT_MS), wait).await {
        Ok(Ok(output)) => RunOutcome::Ran {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            code: output.status.code().unwrap_or(1),
        },
        Ok(Err(err)) => RunOutcome::exec_failure(err),
        Err(_elapsed) => RunOutcome::TimedOut,
    }
}

#[cfg(test)]
mod tests {
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
}
