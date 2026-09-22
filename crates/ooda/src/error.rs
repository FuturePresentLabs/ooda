//! Why a decision call did not produce a usable answer.

/// Errors from a [`crate::Client::decide`] call.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No credential was configured for the endpoint.
    ///
    /// Deliberately an error rather than an anonymous call: a request with no
    /// credential fails later, further from the caller, and less legibly.
    #[error("{0} is not set; a decide() call needs a bearer token")]
    MissingApiKey(&'static str),

    /// The request never completed (DNS, connect, TLS, timeout, ...).
    #[error("transport failed: {0}")]
    Transport(String),

    /// The endpoint answered with a status that is not worth retrying: a
    /// real 4xx (bad request, invalid key, malformed question). Retrying
    /// would just spend more of the rate-limit budget on the same failure.
    #[error("HTTP {status}: {message}")]
    Status {
        /// HTTP status code.
        status: u16,
        /// The endpoint's own error message, when it sent one.
        message: String,
    },

    /// The endpoint answered with HTTP success but an API-level `error`
    /// field in the body (observed shape: either a flat string or a nested
    /// `{"message": ...}` object — both are read).
    #[error("decision endpoint reported an error: {0}")]
    Api(String),

    /// Retries against a transient condition (429 / 5xx / transport error)
    /// were exhausted. Distinct from [`Error::Status`]: this is an infra
    /// condition, not a verdict on the request's content, so a caller
    /// folding results into a benchmark or a pipeline should treat it as a
    /// delay, not a design failure.
    #[error("gave up after {attempts} attempt(s): {message}")]
    Exhausted {
        /// How many attempts were made before giving up.
        attempts: u32,
        /// The last error observed.
        message: String,
    },

    /// The body was not a decodable response.
    #[error("response did not decode: {source}; body was: {body}")]
    Decode {
        /// The JSON error.
        source: serde_json::Error,
        /// The body that failed to decode, truncated for legibility.
        body: String,
    },

    /// A question the caller asked went unanswered.
    #[error("no answer for question {0:?}")]
    MissingAnswer(String),

    /// A question the caller asked as one kind (e.g. [`crate::Question::Choice`])
    /// came back answered as a different kind — a protocol violation, not a
    /// missing answer, so it gets its own message rather than being folded
    /// into [`Error::MissingAnswer`].
    #[error("question {question:?} expected a {expected} answer but got a different kind")]
    WrongAnswerKind {
        /// The question's name.
        question: String,
        /// The kind that was expected (`"choice"`, `"score"`, or `"noul"`).
        expected: &'static str,
    },

    /// A [`crate::decide_staged`] chain's continuation kept asking for
    /// another stage past [`crate::stage::MAX_STAGES`].
    ///
    /// A bug in the continuation (one that never returns `None`) would
    /// otherwise turn into an unbounded sequence of real, billed decision
    /// calls — this is the backstop, not a limit anyone should expect to
    /// hit with a real curated decision tree.
    #[error("staged decision chain exceeded {0} stages without stopping")]
    TooManyStages(u32),

    /// The `capture` feature's durable decision log couldn't be written.
    ///
    /// Deliberately a hard error, not a silently dropped record: a caller
    /// capturing decisions for fine-tuning needs to know when a decision
    /// went un-logged, the same way `speedy`'s own event sink (this
    /// feature's model) treats a write failure as fatal rather than best-
    /// effort.
    #[error("capture log {path}: {source}")]
    Capture {
        /// The log file's path.
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// A `Choice` question was answered with a key outside the offered set.
    ///
    /// The endpoint is supposed to be architecturally incapable of this
    /// (that is the entire premise of a bounded question) but a caller must
    /// not silently accept it if the endpoint gets it wrong anyway — fail
    /// loud, not a guessed fallback.
    #[error("question {question:?} answered with unknown choice {chosen:?}")]
    UnknownChoice {
        /// The question's name.
        question: String,
        /// The key the endpoint returned.
        chosen: String,
    },
}

/// Longest body kept in an error message. Enough to see what went wrong,
/// short enough that a gateway HTML error page does not bury the log.
pub(crate) const MAX_ERROR_BODY: usize = 512;

/// Shortens a body for an error message, on a char boundary.
pub(crate) fn truncate(body: &str) -> String {
    if body.len() <= MAX_ERROR_BODY {
        return body.to_owned();
    }
    let mut cut = MAX_ERROR_BODY;
    while cut > 0 && !body.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}... ({} bytes total)", &body[..cut], body.len())
}
