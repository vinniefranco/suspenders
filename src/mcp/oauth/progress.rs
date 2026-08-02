//! The progress sink the AUTHENTICATE dialog step (Phase E) consumes, mirroring
//! qwen's `OauthDisplayMessage` / `OauthAuthUrl` events.

/// A progress signal from an in-flight [`authenticate`](super::McpOAuthProvider::authenticate),
/// for the AUTHENTICATE dialog step (Phase E) to render - the Suspenders port of
/// qwen's `OAUTH_DISPLAY_MESSAGE_EVENT` / `OAUTH_AUTH_URL_EVENT`. The provider
/// pushes these through a caller-supplied sink so the flow stays UI-free; the
/// Agent (Phase D wire-in) forwards each as an operator-visible line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OAuthProgress {
    /// A user-facing status line (qwen's display-message event), e.g. the
    /// copy-the-URL hint or the "exchanging code" note.
    Message(String),
    /// The authorization URL the operator opens (qwen's auth-url event) - surfaced
    /// separately so the dialog can render it as a copyable/clickable link rather
    /// than hard-wrapping it inside a message line.
    AuthUrl(String),
}

/// The sink [`authenticate`](super::McpOAuthProvider::authenticate) pushes
/// [`OAuthProgress`] through: a boxed `FnMut` the caller supplies. `Sync` is not
/// required (the flow runs on one task); the caller (the Agent) forwards each
/// signal onto its `events` broadcast.
pub type ProgressSink<'a> = Box<dyn FnMut(OAuthProgress) + Send + 'a>;
