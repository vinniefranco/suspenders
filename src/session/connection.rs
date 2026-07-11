//! The Session's model connection: everything the LLM boundary needs to reach
//! the server and shape a request. Part of the Session's fixed facts, resolved
//! once at launch.
//!
//! `max_tokens` lives here and nowhere else: the Conversation's Eviction
//! reserve is set from this field by the composition root, so "Eviction
//! reserves what the reply may consume" is one value read twice.
//!
//! `temperature` is the sampling temperature sent with every request; `None`
//! leaves sampling to the server's own defaults.

/// The Session's model connection.
#[derive(Debug, Clone, PartialEq)]
pub struct Connection {
    pub base_url: String,
    pub token: String,
    pub model: String,
    pub max_tokens: u64,
    pub temperature: Option<f64>,
}

impl Connection {
    /// A connection with `temperature` defaulting to `None`.
    pub fn new(
        base_url: impl Into<String>,
        token: impl Into<String>,
        model: impl Into<String>,
        max_tokens: u64,
    ) -> Self {
        Connection {
            base_url: base_url.into(),
            token: token.into(),
            model: model.into(),
            max_tokens,
            temperature: None,
        }
    }

    pub fn with_temperature(mut self, temperature: Option<f64>) -> Self {
        self.temperature = temperature;
        self
    }
}
