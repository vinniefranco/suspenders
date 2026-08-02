//! The AUTHENTICATE step's render + its folded state (ADR-0065 Phase D/E, qwen's
//! `AuthenticateStep`): the streamed OAuth progress log ([`AuthLine`]), the OSC52
//! copy-URL feedback ([`CopyState`]), and the pure rows that render them. The
//! progress lines and the copy state are folded by [`super::McpDialog`]; this
//! module owns their shape and rendering (ADR-0001/0019).

use crate::mcp::McpServerView;

use super::row::{McpDialogView, McpRow, McpSpan, McpStyle, go_back_footer};

/// One AUTHENTICATE progress line, folded from an
/// [`Event::McpAuthProgress`](crate::event::Event::McpAuthProgress) (qwen's
/// `OauthDisplayMessage` / `OauthAuthUrl`): a plain status `Message` or the
/// authorization `Url` (rendered accented, and the copy-hint follows it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AuthLine {
    /// A status line (starting/discovering/exchanging/complete).
    Message(String),
    /// The authorization URL to open (qwen's `OauthAuthUrl`).
    Url(String),
}

/// The OSC52 copy-to-clipboard feedback for the AUTHENTICATE step (qwen's
/// `copyState`): `Idle` shows the "Press c to copy" hint, `Copied` /
/// `Unsupported` show the post-copy feedback. Folded from the `c` key
/// ([`super::McpDialog::fold_key`] -> [`super::McpFold::CopyUrl`]) once the
/// adapter reports whether the OSC52 write reached a TTY. qwen resets this to
/// `Idle` after a 2s timer; suspenders leaves the feedback up until the step is
/// left or a fresh URL streams in (no display-side timer in the pure core,
/// ADR-0019).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CopyState {
    /// No copy attempted yet: the "Press c to copy" hint shows.
    Idle,
    /// The OSC52 sequence was written to a TTY (qwen's `copied`).
    Copied,
    /// No TTY to write the OSC52 sequence to (qwen's `unsupported`).
    Unsupported,
}

/// The AUTHENTICATE step (qwen `AuthenticateStep`): the "OAuth Authentication"
/// header, the "Server: {name}" line, the streamed progress log (with the auth
/// URL + copy hint), and the "Esc to go back" footer.
pub(super) fn authenticate_view(
    server: &McpServerView,
    progress: &[AuthLine],
    copy: CopyState,
) -> McpDialogView {
    let header = vec![McpRow::bold_styled(
        McpStyle::Accent,
        "OAuth Authentication",
    )];
    let mut content = vec![McpRow::new(vec![
        McpSpan::new(McpStyle::Secondary, "Server: "),
        McpSpan::new(McpStyle::Secondary, server.name.clone()),
    ])];
    content.extend(auth_rows(progress, copy));
    McpDialogView {
        header,
        content,
        footer: go_back_footer(),
    }
}

/// The most recent authorization URL in an AUTHENTICATE progress log, if any
/// (qwen's `authUrl` state - the copy affordance keys off it). The last
/// [`AuthLine::Url`] wins, so a re-issued URL replaces an earlier one.
pub(super) fn auth_url(progress: &[AuthLine]) -> Option<&str> {
    progress.iter().rev().find_map(|line| match line {
        AuthLine::Url(url) => Some(url.as_str()),
        AuthLine::Message(_) => None,
    })
}

/// The AUTHENTICATE progress rows (qwen's message log + auth URL + copy hint):
/// each message a secondary line, the auth URL an accented line. When a URL is on
/// screen the copy hint follows the whole log (qwen renders it once, keyed off
/// `authUrl`), reading the OSC52 `copy`-feedback state: the idle "Press c to
/// copy" hint (bold, accent, like qwen's `bold` idle line), or the copied /
/// unsupported feedback in its status colour.
fn auth_rows(progress: &[AuthLine], copy: CopyState) -> Vec<McpRow> {
    let mut rows = Vec::new();
    for line in progress {
        match line {
            AuthLine::Message(text) => rows.push(McpRow::styled(McpStyle::Secondary, text.clone())),
            AuthLine::Url(url) => rows.push(McpRow::styled(McpStyle::Accent, url.clone())),
        }
    }
    if auth_url(progress).is_some() {
        rows.push(copy_hint_row(copy));
    }
    rows
}

/// The OSC52 copy-hint row (qwen's `copyState` `<Text>`): the idle "Press c to
/// copy" prompt (bold accent, qwen bolds the idle line), the "Copy request sent"
/// feedback (success green), or the "Cannot write to terminal" feedback (warning
/// yellow). qwen's `—` em-dash in the unsupported line is rendered as ` - ` here
/// (house style).
fn copy_hint_row(copy: CopyState) -> McpRow {
    match copy {
        CopyState::Idle => McpRow::bold_styled(
            McpStyle::Accent,
            "Press c to copy the authorization URL to your clipboard.",
        ),
        CopyState::Copied => McpRow::styled(
            McpStyle::Success,
            "Copy request sent to your terminal. If paste is empty, copy the URL above manually.",
        ),
        CopyState::Unsupported => McpRow::styled(
            McpStyle::Warning,
            "Cannot write to terminal - copy the URL above manually.",
        ),
    }
}
