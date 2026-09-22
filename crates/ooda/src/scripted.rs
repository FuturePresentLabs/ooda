//! A client that answers from a canned queue of **raw JSON response
//! bodies**, for exercising a caller with no credentials and no network.
//!
//! Deliberately not a stub that returns pre-built Rust structs: it
//! serializes the request through the real [`serde::Serialize`] impls and
//! decodes the canned body through the real response-decoding path
//! ([`crate::http::decode_response`]), so a test that passes here is a test
//! about the wire format, not about a hand-rolled shortcut around it. A
//! recorded exchange can be replayed byte-for-byte against a real capture.

use std::sync::Mutex;

use crate::client::{Client, Outcome, Request};
use crate::error::Error;

/// A [`Client`] that answers from a queue of canned response bodies.
#[derive(Debug)]
pub struct ScriptedClient {
    bodies: Mutex<std::collections::VecDeque<String>>,
    requests: Mutex<Vec<serde_json::Value>>,
    resolved_model: Option<String>,
}

impl ScriptedClient {
    /// Queues response bodies, answered in order.
    #[must_use]
    pub fn new<S: Into<String>>(bodies: impl IntoIterator<Item = S>) -> Self {
        Self {
            bodies: Mutex::new(bodies.into_iter().map(Into::into).collect()),
            requests: Mutex::new(Vec::new()),
            resolved_model: None,
        }
    }

    /// Sets the value reported as [`Outcome::resolved_model`] on every
    /// answered call.
    #[must_use]
    pub fn with_resolved_model(mut self, model: impl Into<String>) -> Self {
        self.resolved_model = Some(model.into());
        self
    }

    /// Every request this client has been asked, as serialized JSON, in
    /// call order.
    ///
    /// # Panics
    /// If a previous call panicked while holding the lock.
    #[must_use]
    pub fn requests(&self) -> Vec<serde_json::Value> {
        self.requests.lock().expect("request log poisoned").clone()
    }
}

impl Client for ScriptedClient {
    fn decide(&self, request: &Request) -> Result<Outcome, Error> {
        let wire = serde_json::json!({
            "state": request.observation,
            "questions": request.questions,
        });
        self.requests
            .lock()
            .expect("request log poisoned")
            .push(wire);

        let body = self
            .bodies
            .lock()
            .expect("response queue poisoned")
            .pop_front()
            .ok_or_else(|| Error::Transport("scripted client ran out of responses".to_owned()))?;

        let value: serde_json::Value =
            serde_json::from_str(&body).map_err(|source| Error::Decode {
                source,
                body: crate::error::truncate(&body),
            })?;
        crate::http::decode_response(&value, self.resolved_model.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::question::Question;

    #[test]
    fn answers_in_queue_order_and_records_the_real_wire_request() {
        let client = ScriptedClient::new([
            r#"{"answers":{"q":{"type":"choice","choice":"a","confidence":0.9}}}"#.to_owned(),
        ])
        .with_resolved_model("fpl/decide-v3");
        let request = Request::single(
            serde_json::json!({"k": 1}),
            "q",
            Question::choice("pick", [("a", "desc")]),
        );
        let outcome = client.decide(&request).unwrap();
        assert_eq!(outcome.resolved_model.as_deref(), Some("fpl/decide-v3"));
        assert_eq!(outcome.answer("q").unwrap().choice(), Some("a"));
        assert_eq!(client.requests()[0]["state"]["k"], 1);
    }

    #[test]
    fn running_out_of_responses_is_an_error_not_a_panic() {
        let client = ScriptedClient::new(Vec::<String>::new());
        let request = Request::single(
            serde_json::json!({}),
            "q",
            Question::choice("pick", [("a", "desc")]),
        );
        assert!(client.decide(&request).is_err());
    }
}
