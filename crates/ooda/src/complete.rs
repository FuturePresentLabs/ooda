//! `ooda`'s other capability against Bifrost: genuinely open text, not a
//! bounded decision.
//!
//! Adapted from `surf`'s own already-production `OpenAiText` -- built there
//! independently because this crate only offered `decide()`, and free-text
//! form-value generation is a real, different need (typing into a web
//! form) from a bounded `Choice`/`Score`/`Noul` question. Kept as a
//! clearly separate capability, never folded into [`crate::Question`]/
//! [`crate::Answer`]: `decide()` should keep meaning what it says --
//! bounded in, typed out -- and a caller reading `complete()` should know
//! at a glance there is no closed answer set here, no confidence, nothing
//! to `match` exhaustively.
//!
//! `HttpClient::post`'s retry/backoff is shared with [`Client::decide`]:
//! both are Bifrost routes on the same gateway, so a 429 means the same
//! thing on either one. This closes a real gap `surf`'s own `OpenAiText`
//! had -- zero retry logic at all -- for free.
//!
//! [`Client::decide`]: crate::Client::decide

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::http::HttpClient;

/// Bifrost's OpenAI-compatible chat-completions route, appended to a
/// client's base URL -- the same base [`crate::HttpClient::endpoint`]
/// resolves against, since both are routes on one gateway.
pub const CHAT_PATH: &str = "/v1/chat/completions";

/// A free-text completion request: system instructions plus a user
/// message. Unlike [`crate::Question`], there is no closed answer set --
/// bounded only by `max_tokens`.
#[derive(Clone, Debug, PartialEq)]
pub struct Prompt {
    pub system: String,
    pub user: String,
    pub max_tokens: u16,
    pub temperature: f32,
}

impl Prompt {
    /// Defaults to 512 max tokens and 0.2 temperature -- low but not
    /// deterministic-zero, since this is meant to fit a range of callers
    /// (a terse form value, a longer planning note), not one caller's own
    /// tuning. Override with [`Prompt::with_max_tokens`]/
    /// [`Prompt::with_temperature`] for a specific need.
    #[must_use]
    pub fn new(system: impl Into<String>, user: impl Into<String>) -> Self {
        Prompt {
            system: system.into(),
            user: user.into(),
            max_tokens: 512,
            temperature: 0.2,
        }
    }

    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: u16) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    #[must_use]
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = temperature;
        self
    }
}

/// Anything that can answer a free-text [`Prompt`].
///
/// A separate trait from [`crate::Client`], not an extra method on it: the
/// two have unrelated wire shapes and unrelated response types (a bounded
/// [`crate::Outcome`] vs. a plain `String`), so a caller that only ever
/// makes bounded decisions should never need to know this trait exists.
pub trait Complete {
    /// # Errors
    /// See [`Error`].
    fn complete(&self, prompt: &Prompt) -> Result<String, Error>;
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: [ChatMessage<'a>; 2],
    max_tokens: u16,
    temperature: f32,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatAnswer,
}

#[derive(Deserialize)]
struct ChatAnswer {
    content: String,
}

impl Complete for HttpClient {
    fn complete(&self, prompt: &Prompt) -> Result<String, Error> {
        let body = serde_json::to_value(ChatRequest {
            model: self.model(),
            messages: [
                ChatMessage {
                    role: "system",
                    content: &prompt.system,
                },
                ChatMessage {
                    role: "user",
                    content: &prompt.user,
                },
            ],
            max_tokens: prompt.max_tokens,
            temperature: prompt.temperature,
        })
        .map_err(|source| Error::Decode {
            source,
            body: "<prompt serialization failed>".to_owned(),
        })?;
        let (value, ..) = self.post(&self.url(CHAT_PATH), &body)?;
        let response: ChatResponse =
            serde_json::from_value(value.clone()).map_err(|source| Error::Decode {
                source,
                body: crate::error::truncate(&value.to_string()),
            })?;
        response
            .choices
            .into_iter()
            .next()
            .map(|choice| choice.message.content.trim().to_owned())
            .filter(|content| !content.is_empty())
            .ok_or(Error::EmptyCompletion)
    }
}

/// Answers [`Complete::complete`] from a queue of canned strings, in order
/// -- [`crate::ScriptedClient`]'s counterpart for [`Complete`], kept
/// separate rather than folded into it: a completion call and a `decide()`
/// call have unrelated wire shapes, so mixing their canned data into one
/// queue would make a test harder to read, not easier.
#[derive(Debug, Default)]
pub struct ScriptedComplete {
    responses: std::sync::Mutex<std::collections::VecDeque<String>>,
}

impl ScriptedComplete {
    /// Queues responses, answered in order.
    #[must_use]
    pub fn new<S: Into<String>>(responses: impl IntoIterator<Item = S>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses.into_iter().map(Into::into).collect()),
        }
    }
}

impl Complete for ScriptedComplete {
    fn complete(&self, _prompt: &Prompt) -> Result<String, Error> {
        self.responses
            .lock()
            .expect("response queue poisoned")
            .pop_front()
            .ok_or_else(|| Error::Transport("ScriptedComplete ran out of responses".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prompt_defaults_to_a_low_but_not_zero_temperature() {
        let prompt = Prompt::new("system", "user");
        assert_eq!(prompt.max_tokens, 512);
        assert!((prompt.temperature - 0.2).abs() < f32::EPSILON);
    }

    #[test]
    fn builder_methods_override_the_defaults() {
        let prompt = Prompt::new("system", "user")
            .with_max_tokens(128)
            .with_temperature(0.0);
        assert_eq!(prompt.max_tokens, 128);
        assert_eq!(prompt.temperature, 0.0);
    }

    #[test]
    fn scripted_complete_answers_in_queue_order() {
        let client = ScriptedComplete::new(["first", "second"]);
        let prompt = Prompt::new("system", "user");
        assert_eq!(client.complete(&prompt).unwrap(), "first");
        assert_eq!(client.complete(&prompt).unwrap(), "second");
    }

    #[test]
    fn scripted_complete_running_out_is_an_error_not_a_panic() {
        let client = ScriptedComplete::new(Vec::<String>::new());
        assert!(client.complete(&Prompt::new("s", "u")).is_err());
    }
}
