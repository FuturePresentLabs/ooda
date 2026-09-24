//! The real client: blocking HTTPS to a System One-compatible endpoint.
//!
//! Synthesizes two independent, previously-separate production fixes found
//! while surveying this ecosystem's existing decision clients:
//! - confidence relocation: a `choice` answer sometimes reports its
//!   confidence out-of-band, under `providerMetadata.typesafe.confidence`,
//!   instead of inline — observed on a live gateway call routed through a
//!   different backend path than the one its own test suite covers.
//! - retry semantics: 429 honors a numeric `Retry-After` header when the
//!   endpoint sends one, 5xx and transport errors get exponential backoff, a
//!   real 4xx does not retry at all, and the endpoint's own nested
//!   `{"error":{"message":...}}` shape is read (a flat `{"error":"..."}`
//!   string is also accepted, since a caller has no way to know in advance
//!   which shape a given failure mode will use).
//!
//! Neither prior implementation had both fixes; this client has both.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::client::{Client, Observation, Outcome, Request};
use crate::error::{self, Error};
use crate::question::{Answer, Usage};

/// Default base URL: Future Present Labs' bifrost gateway, which routes to
/// Jev- and Laya-backed models alike. Override with [`BASE_URL_ENV`].
pub const DEFAULT_BASE_URL: &str = "https://ai.fpl.dev";

/// The decision route, appended to the base URL.
pub const DECIDE_PATH: &str = "/v1/systemone";

/// Default model alias. `fpl/decide` is bifrost's own routing alias, not a
/// universal identifier — a different gateway (or Jev/Laya hit directly)
/// will want its own model id via [`MODEL_ENV`] or
/// [`HttpClient::with_model`].
pub const DEFAULT_MODEL: &str = "fpl/decide";

/// Response header naming the backend model that actually answered.
pub const RESOLVED_MODEL_HEADER: &str = "x-fpl-resolved-model";

/// Environment variable holding the bearer token.
pub const API_KEY_ENV: &str = "OODA_API_KEY";

/// Environment variable overriding [`DEFAULT_BASE_URL`].
pub const BASE_URL_ENV: &str = "OODA_BASE_URL";

/// Environment variable overriding [`DEFAULT_MODEL`].
pub const MODEL_ENV: &str = "OODA_MODEL";

/// Attempts beyond this many give up rather than retry forever — covers a
/// genuinely down endpoint or a rate limit that isn't recovering. The
/// default for [`HttpClient::with_max_attempts`].
pub const MAX_ATTEMPTS: u32 = 5;

/// How long a single attempt may take before it is abandoned.
///
/// Sized for a bounded `decide()` as a batch step, where a slow answer
/// still beats no answer. Two real callers need their own value instead:
/// a long open-text `complete()` answer, which can run minutes at a
/// hundred-odd tokens a second, and a caller holding a live interaction
/// open (a voice turn), which wants far less than 30s. See
/// [`HttpClient::with_timeout`].
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Backoff when the endpoint doesn't say how long to wait (no
/// `Retry-After`): doubles each attempt, starting at 2s (2, 4, 8, 16s for
/// attempts 1-4).
fn backoff_secs(attempt: u32) -> u64 {
    2u64.saturating_pow(attempt)
}

/// `std::env::var(name)`, treating a present-but-blank value the same as
/// unset — see [`HttpClient::from_env`]'s docs for why.
fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn http_client(timeout: Duration) -> Result<reqwest::blocking::Client, Error> {
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| Error::Transport(e.to_string()))
}

/// A System One / Jev / Laya-compatible client over blocking HTTPS.
#[derive(Debug)]
pub struct HttpClient {
    base_url: String,
    api_key: String,
    model: String,
    max_attempts: u32,
    http: reqwest::blocking::Client,
}

impl HttpClient {
    /// Builds a client against an explicit base URL, bearer token, and
    /// model.
    ///
    /// # Errors
    /// [`Error::MissingApiKey`] if `api_key` is blank; [`Error::Transport`]
    /// if the HTTP client cannot be constructed.
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, Error> {
        let api_key = api_key.into();
        if api_key.trim().is_empty() {
            return Err(Error::MissingApiKey(API_KEY_ENV));
        }
        let http = http_client(DEFAULT_TIMEOUT)?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key,
            model: model.into(),
            max_attempts: MAX_ATTEMPTS,
            http,
        })
    }

    /// Builds a client from [`API_KEY_ENV`], optional [`BASE_URL_ENV`]
    /// (defaulting to [`DEFAULT_BASE_URL`]), and optional [`MODEL_ENV`]
    /// (defaulting to [`DEFAULT_MODEL`]).
    ///
    /// A present-but-empty value (`OODA_MODEL=` with nothing after the `=`,
    /// as a `.env.example` template commonly ships) is treated the same as
    /// unset for [`BASE_URL_ENV`]/[`MODEL_ENV`] — otherwise the empty string
    /// itself would be sent as the model/base URL instead of falling back to
    /// the default, since `std::env::var` returns `Ok("")` for a blank
    /// value, not an absent one.
    ///
    /// # Errors
    /// [`Error::MissingApiKey`] when the token is absent or empty.
    pub fn from_env() -> Result<Self, Error> {
        let key = std::env::var(API_KEY_ENV).map_err(|_| Error::MissingApiKey(API_KEY_ENV))?;
        let base = non_empty_env(BASE_URL_ENV).unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
        let model = non_empty_env(MODEL_ENV).unwrap_or_else(|| DEFAULT_MODEL.to_owned());
        Self::new(base, key, model)
    }

    /// Returns a client that abandons a single attempt after `timeout`
    /// instead of [`DEFAULT_TIMEOUT`]. Two real reasons to call this, in
    /// opposite directions: a long open-text `complete()` answer (a model
    /// writing a whole document at a hundred-odd tokens a second needs
    /// minutes), or a caller holding a live interaction open (a voice turn
    /// that waits 30s for a decision has already failed the person waiting
    /// on it, and would rather fall back to its own deterministic path
    /// after a second or two).
    ///
    /// Pair with [`Self::with_max_attempts`] for the latter case: the
    /// timeout bounds one attempt, not the call, and the retry ladder
    /// multiplies it.
    ///
    /// # Errors
    /// [`Error::Transport`] if the HTTP client cannot be rebuilt.
    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self, Error> {
        self.http = http_client(timeout)?;
        Ok(self)
    }

    /// Returns a client that sends `model` instead of whatever it was built
    /// with.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Returns a client that gives up after `max_attempts` rather than
    /// [`MAX_ATTEMPTS`].
    ///
    /// `1` disables retrying entirely, which is what a latency-bound caller
    /// usually wants: the backoff ladder is seconds long by design, so for
    /// them a retry is indistinguishable from a hang. Values below `1` are
    /// raised to `1` — a client that makes no attempt at all would report a
    /// failure it never actually tried.
    #[must_use]
    pub fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts.max(1);
        self
    }

    /// The URL this client posts to for `decide()`.
    #[must_use]
    pub fn endpoint(&self) -> String {
        self.url(DECIDE_PATH)
    }

    /// `path`, resolved against this client's base URL — the same base
    /// every route (`decide()`'s System One path, `complete()`'s chat
    /// completions path) is served from, since both are Bifrost routes on
    /// one gateway.
    #[must_use]
    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// This client's own configured model -- [`crate::complete`] needs it
    /// to build a chat-completions request; auth and the base URL are
    /// already covered by [`HttpClient::post`]/[`HttpClient::url`].
    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    /// POSTs `body` to `url` (a full URL, typically built with
    /// [`HttpClient::url`]) with this client's bearer auth, retrying
    /// 429/5xx/transport failures the same way regardless of which Bifrost
    /// route is being called -- [`Client::decide`] and
    /// [`crate::complete::Complete::complete`] share this one retry
    /// implementation rather than each growing their own.
    ///
    /// Returns the decoded body, the resolved-model header, how long the
    /// *accepted* attempt's own round trip took, and how many retries
    /// preceded it (`0` = worked first try).
    ///
    /// Deliberately does not time backoff sleeps into the returned
    /// duration: a 429/5xx retry is the endpoint's current load, not the
    /// model's decision speed, and folding that into a latency number
    /// would make a benchmark's "time per result" measure server capacity
    /// on the day it happened to run rather than the thing it's supposed
    /// to measure. `retries` is reported separately so that information
    /// isn't lost, just not conflated with latency.
    pub(crate) fn post(
        &self,
        url: &str,
        body: &Value,
    ) -> Result<(Value, Option<String>, Duration, u32), Error> {
        let (response, resolved_model, call_started, retries) = self.send(url, body)?;
        let body_text = response
            .text()
            .map_err(|e| Error::Transport(e.to_string()))?;
        let value: Value = serde_json::from_str(&body_text).map_err(|source| Error::Decode {
            source,
            body: error::truncate(&body_text),
        })?;
        Ok((value, resolved_model, call_started.elapsed(), retries))
    }

    /// [`HttpClient::post`]'s retry loop, stopping at the first successful
    /// response and handing it back unread -- so a streamed body can be
    /// consumed as it arrives. Returns the response, the resolved-model
    /// header, when the accepted attempt started, and how many retries
    /// preceded it.
    pub(crate) fn send(
        &self,
        url: &str,
        body: &Value,
    ) -> Result<(reqwest::blocking::Response, Option<String>, std::time::Instant, u32), Error> {
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let call_started = std::time::Instant::now();
            let response = self
                .http
                .post(url)
                .bearer_auth(&self.api_key)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .json(body)
                .send();

            let response = match response {
                Ok(r) => r,
                Err(e) => {
                    if attempt >= self.max_attempts {
                        return Err(Error::Exhausted {
                            attempts: attempt,
                            message: e.to_string(),
                        });
                    }
                    std::thread::sleep(Duration::from_secs(backoff_secs(attempt)));
                    continue;
                }
            };

            let status = response.status();
            let resolved_model = response
                .headers()
                .get(RESOLVED_MODEL_HEADER)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());

            if status.is_success() {
                return Ok((response, resolved_model, call_started, attempt - 1));
            }
            let body_text = response
                .text()
                .map_err(|e| Error::Transport(e.to_string()))?;

            if status.as_u16() == 429 {
                if attempt >= self.max_attempts {
                    return Err(Error::Exhausted {
                        attempts: attempt,
                        message: format!("rate limited (429): {}", error::truncate(&body_text)),
                    });
                }
                std::thread::sleep(Duration::from_secs(
                    retry_after.unwrap_or_else(|| backoff_secs(attempt)),
                ));
                continue;
            }
            if status.is_server_error() {
                if attempt >= self.max_attempts {
                    return Err(Error::Exhausted {
                        attempts: attempt,
                        message: format!("HTTP {status}: {}", error::truncate(&body_text)),
                    });
                }
                std::thread::sleep(Duration::from_secs(backoff_secs(attempt)));
                continue;
            }
            // Not a success, not retryable: the endpoint's own words.
            let message = serde_json::from_str::<Value>(&body_text)
                .ok()
                .and_then(|v| extract_error_message(&v))
                .unwrap_or_else(|| error::truncate(&body_text));
            return Err(Error::Status {
                status: status.as_u16(),
                message,
            });
        }
    }
}

impl Client for HttpClient {
    fn decide(&self, request: &Request) -> Result<Outcome, Error> {
        let body = serde_json::json!({
            "model": self.model,
            "state": request.observation,
            "questions": request.questions,
        });
        let (value, resolved_model, elapsed, retries) = self.post(&self.endpoint(), &body)?;
        decode_response(&value, resolved_model)
            .map(|outcome| outcome.with_elapsed(elapsed).with_retries(retries))
    }
}

/// Decodes a full System One response body into an [`Outcome`] — the shared
/// core of [`HttpClient::decide`] and [`crate::ScriptedClient::decide`], so
/// the two can never drift on how a response is read.
pub(crate) fn decode_response(
    value: &Value,
    resolved_model: Option<String>,
) -> Result<Outcome, Error> {
    let raw: RawResponse =
        serde_json::from_value(value.clone()).map_err(|source| Error::Decode {
            source,
            body: error::truncate(&value.to_string()),
        })?;
    let usage = raw_usage(value);
    let answers = raw.into_answers()?;
    Ok(Outcome::new(answers, usage, resolved_model))
}

pub(crate) fn raw_usage(value: &Value) -> Option<Usage> {
    value
        .get("usage")
        .and_then(|u| serde_json::from_value(u.clone()).ok())
}

/// Read an API-level error message out of a response body. Endpoints in this
/// ecosystem have been observed sending both a flat string
/// (`{"error": "..."}`) and a nested object
/// (`{"error": {"message": "...", "type": "..."}}`) — accept either so a
/// spec change in one direction doesn't silently stop being recognized as an
/// error.
pub(crate) fn extract_error_message(value: &Value) -> Option<String> {
    let err = value.get("error")?;
    if let Some(s) = err.as_str() {
        return Some(s.to_owned());
    }
    err.get("message")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// The wire shape, decoded just enough to relocate a `choice` or `score`
/// answer's confidence from `providerMetadata.typesafe.confidence.<name>`
/// before typing each [`Answer`] for real, when the answer omitted it
/// inline. Confirmed live (2026-09-21) that `score` relocates exactly like
/// `choice` — both were observed with `confidence` absent inline and present
/// under `providerMetadata.typesafe.confidence`; `boolean`/`Noul` carries no
/// separate confidence at all (the probability itself is the answer), so it
/// is not backfilled.
///
/// If confidence is genuinely absent from *both* places, building the typed
/// [`Answer`] still fails loud: this backfills a known alternate location,
/// it does not invent a value.
#[derive(Deserialize)]
struct RawResponse {
    #[serde(default)]
    answers: BTreeMap<String, Value>,
    #[serde(default, rename = "providerMetadata")]
    provider_metadata: Option<RawProviderMetadata>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Default, Deserialize)]
struct RawProviderMetadata {
    #[serde(default)]
    typesafe: Option<RawTypesafeMetadata>,
}

#[derive(Default, Deserialize)]
struct RawTypesafeMetadata {
    #[serde(default)]
    confidence: BTreeMap<String, f64>,
}

impl RawResponse {
    fn into_answers(self) -> Result<BTreeMap<String, Answer>, Error> {
        if let Some(err) = self.error.as_ref().and_then(extract_error_message) {
            return Err(Error::Api(err));
        }
        let out_of_band_confidence = self
            .provider_metadata
            .and_then(|meta| meta.typesafe)
            .map(|t| t.confidence)
            .unwrap_or_default();

        let mut answers = BTreeMap::new();
        for (name, mut value) in self.answers {
            if let Some(object) = value.as_object_mut() {
                let type_tag = object.get("type").and_then(Value::as_str);
                let relocatable = matches!(type_tag, Some("choice") | Some("score"));
                let has_confidence = object.get("confidence").is_some_and(|c| !c.is_null());
                if relocatable && !has_confidence {
                    if let Some(&confidence) = out_of_band_confidence.get(&name) {
                        object.insert("confidence".to_owned(), serde_json::json!(confidence));
                    }
                }
            }
            let answer: Answer =
                serde_json::from_value(value.clone()).map_err(|source| Error::Decode {
                    source,
                    body: error::truncate(&value.to_string()),
                })?;
            answers.insert(name, answer);
        }
        Ok(answers)
    }
}

/// Trivial extension so [`Observation`] can be built from any `Serialize`
/// value without every caller writing `serde_json::to_value(..).unwrap()`.
pub trait IntoObservation {
    /// Converts into an [`Observation`], panicking only if `self` cannot be
    /// represented as JSON at all (a map key that isn't a string, or a
    /// `NaN`/`Infinity` float) — the same cases `serde_json::to_value` itself
    /// rejects.
    fn into_observation(self) -> Observation;
}

impl<T: serde::Serialize> IntoObservation for T {
    fn into_observation(self) -> Observation {
        serde_json::to_value(self).expect("value is representable as JSON")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_api_key_is_refused_before_any_request_is_made() {
        assert!(matches!(
            HttpClient::new(DEFAULT_BASE_URL, "   ", DEFAULT_MODEL),
            Err(Error::MissingApiKey(_))
        ));
    }

    /// A `.env.example`-style blank value (`OODA_MODEL=` with nothing after
    /// the `=`) must fall back to the default, not send the empty string as
    /// the model — reproduces a real failure (`HTTP 404: no backend serves
    /// model ''`) hit against the live endpoint.
    #[test]
    fn a_blank_env_value_is_treated_as_unset() {
        // SAFETY: test-only; single-threaded within this process's test run
        // for this var name, and restored before returning.
        unsafe {
            std::env::set_var("OODA_TEST_BLANK_VAR", "");
        }
        assert_eq!(non_empty_env("OODA_TEST_BLANK_VAR"), None);
        unsafe {
            std::env::remove_var("OODA_TEST_BLANK_VAR");
        }
    }

    #[test]
    fn the_endpoint_is_built_without_a_double_slash() {
        let client = HttpClient::new("https://ai.fpl.dev/", "k", "m").unwrap();
        assert_eq!(client.endpoint(), "https://ai.fpl.dev/v1/systemone");
    }

    #[test]
    fn backoff_doubles_each_attempt() {
        assert_eq!(backoff_secs(1), 2);
        assert_eq!(backoff_secs(2), 4);
        assert_eq!(backoff_secs(3), 8);
        assert_eq!(backoff_secs(4), 16);
    }

    #[test]
    fn extracts_flat_and_nested_error_shapes() {
        let flat: Value = serde_json::from_str(r#"{"error":"invalid api key"}"#).unwrap();
        assert_eq!(extract_error_message(&flat), Some("invalid api key".into()));

        let nested: Value =
            serde_json::from_str(r#"{"error":{"message":"invalid api key","type":"x"}}"#).unwrap();
        assert_eq!(
            extract_error_message(&nested),
            Some("invalid api key".into())
        );
    }

    #[test]
    fn confidence_is_backfilled_from_provider_metadata_when_absent_inline() {
        let raw: RawResponse = serde_json::from_str(
            r#"{
                "answers": {
                    "edit_choice": { "type": "choice", "choice": "E1",
                        "probabilities": {"E1": 0.67} }
                },
                "providerMetadata": {
                    "typesafe": { "confidence": {"edit_choice": 0.56} }
                }
            }"#,
        )
        .unwrap();
        let answers = raw.into_answers().unwrap();
        assert_eq!(answers["edit_choice"].confidence(), Some(0.56));
    }

    /// Real captured shape from the live `typesafe-ai/jev` route
    /// (2026-09-21): a `score` answer with `confidence` absent inline and
    /// present under `providerMetadata.typesafe.confidence`, same as
    /// `choice`.
    #[test]
    fn score_confidence_is_also_backfilled_from_provider_metadata() {
        let raw: RawResponse = serde_json::from_str(
            r#"{
                "answers": {
                    "gain_character": { "type": "score", "score": 3,
                        "probabilities": {"3": 1.0} }
                },
                "providerMetadata": {
                    "typesafe": { "confidence": {"gain_character": 1.0} }
                }
            }"#,
        )
        .unwrap();
        let answers = raw.into_answers().unwrap();
        assert_eq!(answers["gain_character"].confidence(), Some(1.0));
    }

    #[test]
    fn an_inline_confidence_is_never_overridden_by_provider_metadata() {
        let raw: RawResponse = serde_json::from_str(
            r#"{
                "answers": {
                    "q": { "type": "choice", "choice": "a",
                        "probabilities": {"a": 1.0}, "confidence": 0.42 }
                },
                "providerMetadata": { "typesafe": { "confidence": { "q": 0.99 } } }
            }"#,
        )
        .unwrap();
        let answers = raw.into_answers().unwrap();
        assert_eq!(answers["q"].confidence(), Some(0.42));
    }

    #[test]
    fn a_choice_answer_missing_confidence_in_both_places_still_fails_loud() {
        let raw: RawResponse =
            serde_json::from_str(r#"{"answers":{"q":{"type":"choice","choice":"a"}}}"#).unwrap();
        assert!(raw.into_answers().is_err());
    }

    #[test]
    fn unknown_response_fields_do_not_break_decoding() {
        let raw: RawResponse = serde_json::from_str(
            r#"{
                "answers": { "q": { "type": "choice", "choice": "a",
                    "probabilities": {"a": 1.0}, "confidence": 1.0, "latency_ms": 12 } },
                "trace_id": "abc"
            }"#,
        )
        .unwrap();
        let answers = raw.into_answers().unwrap();
        assert_eq!(answers["q"].choice(), Some("a"));
    }

    #[test]
    fn a_client_defaults_to_the_full_retry_ladder() {
        let client = HttpClient::new("https://example.invalid", "k", "m").unwrap();
        assert_eq!(client.max_attempts, MAX_ATTEMPTS);
    }

    #[test]
    fn one_attempt_means_no_retry_at_all() {
        // What a latency-bound caller wants. The backoff ladder is seconds
        // long by design, so to a voice turn a retry is indistinguishable
        // from a hang -- it would rather fail fast and use its own
        // deterministic fallback.
        let client = HttpClient::new("https://example.invalid", "k", "m")
            .unwrap()
            .with_max_attempts(1);
        assert_eq!(client.max_attempts, 1);
    }

    #[test]
    fn an_attempt_budget_below_one_is_raised_rather_than_honored() {
        // Zero attempts would report a failure the client never actually
        // tried, which is a lie about the endpoint.
        let client = HttpClient::new("https://example.invalid", "k", "m")
            .unwrap()
            .with_max_attempts(0);
        assert_eq!(client.max_attempts, 1);
    }

    #[test]
    fn a_timeout_can_be_tightened_for_an_interactive_caller() {
        let client = HttpClient::new("https://example.invalid", "k", "m")
            .unwrap()
            .with_timeout(Duration::from_millis(1500))
            .unwrap()
            .with_max_attempts(1);
        // The builders compose and keep the rest of the configuration.
        assert_eq!(client.max_attempts, 1);
        assert_eq!(client.model, "m");
        assert_eq!(client.base_url, "https://example.invalid");
    }
}
