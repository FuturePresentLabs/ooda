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

use serde::Serialize;

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
    /// How hard a reasoning model thinks before answering: `"low"`,
    /// `"medium"` or `"high"`, sent as the gateway's `reasoning.effort`.
    /// `None` leaves the model's default. Reasoning spends `max_tokens` too:
    /// a long answer from a model left to think freely can use its whole
    /// budget on reasoning and come back empty.
    pub reasoning_effort: Option<String>,
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
            reasoning_effort: None,
        }
    }

    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self
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
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Reasoning<'a>>,
    /// Always streamed: a long answer sent whole arrives only when it is
    /// finished, and a gateway in front of Bifrost cuts a response that
    /// says nothing for ~125 s (HTTP 524). Streamed, bytes flow the whole time.
    stream: bool,
}

#[derive(Serialize)]
struct Reasoning<'a> {
    effort: &'a str,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
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
            reasoning: prompt.reasoning_effort.as_deref().map(|effort| Reasoning { effort }),
            stream: true,
        })
        .map_err(|source| Error::Decode {
            source,
            body: "<prompt serialization failed>".to_owned(),
        })?;
        let (response, ..) = self.send(&self.url(CHAT_PATH), &body)?;
        collect_stream(std::io::BufReader::new(response))
    }
}

/// The answer text of a chat-completions event stream: every `data:` event's
/// `choices[0].delta.content`, in order, until `data: [DONE]` or the end.
/// Comments (keep-alives) and events without content (a role, reasoning,
/// usage) are skipped; an `error` event fails the call.
///
/// # Errors
/// [`Error::Transport`] if the stream breaks, [`Error::Status`] for an error
/// event, [`Error::Decode`] for an event that is not JSON,
/// [`Error::EmptyCompletion`] if no content arrived.
pub(crate) fn collect_stream(reader: impl std::io::BufRead) -> Result<String, Error> {
    let mut answer = String::new();
    for line in reader.lines() {
        let line = line.map_err(|e| Error::Transport(e.to_string()))?;
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data == "[DONE]" {
            break;
        }
        if data.is_empty() {
            continue;
        }
        let event: serde_json::Value = serde_json::from_str(data).map_err(|source| Error::Decode {
            source,
            body: crate::error::truncate(data),
        })?;
        if let Some(error) = event.get("error") {
            return Err(Error::Status {
                status: error.get("code").and_then(serde_json::Value::as_u64).unwrap_or(500) as u16,
                message: error
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("error event in the stream")
                    .to_owned(),
            });
        }
        if let Some(piece) = event
            .pointer("/choices/0/delta/content")
            .and_then(serde_json::Value::as_str)
        {
            answer.push_str(piece);
        }
    }
    let answer = answer.trim().to_owned();
    if answer.is_empty() {
        Err(Error::EmptyCompletion)
    } else {
        Ok(answer)
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
    fn a_stream_is_its_content_deltas_in_order() {
        let stream = ": keep-alive\n\
data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"reasoning\":\"thinking...\"}}]}\n\
data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"a\\\": \"}}]}\n\
data: {\"choices\":[{\"delta\":{\"content\":\"1}\"}}]}\n\
data: {\"usage\":{\"total_tokens\":3},\"choices\":[]}\n\
data: [DONE]\n\
data: {\"choices\":[{\"delta\":{\"content\":\"after done\"}}]}\n";
        assert_eq!(collect_stream(stream.as_bytes()).unwrap(), "{\"a\": 1}");
    }

    #[test]
    fn an_error_event_or_an_empty_stream_fails() {
        let error = "data: {\"error\":{\"code\":429,\"message\":\"slow down\"}}\n";
        assert!(matches!(
            collect_stream(error.as_bytes()),
            Err(Error::Status { status: 429, .. })
        ));
        assert!(matches!(
            collect_stream("data: [DONE]\n".as_bytes()),
            Err(Error::EmptyCompletion)
        ));
    }

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
