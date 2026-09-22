//! The `decide()` seam: one request, any number of named questions, one
//! typed outcome.
//!
//! Earlier implementations in this ecosystem exposed this as separate
//! `ask_choice`/`ask_score`/`ask_noul`/`ask_many` methods. This crate
//! collapses that to one [`Request`] builder and one [`Client::decide`]
//! method: a single question is just a batch of one, so there is exactly one
//! code path to get right (retry, trace recording, error handling) instead
//! of four.

use std::collections::BTreeMap;

use crate::error::Error;
use crate::question::{Answer, Question};
use crate::trace::Record;

/// An observation: arbitrary JSON describing the situation being decided
/// about.
pub type Observation = serde_json::Value;

/// A decide-call request: one observation, one or more named questions.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// The situation being decided about.
    pub observation: Observation,
    /// Question name -> question, asked in one call.
    pub questions: BTreeMap<String, Question>,
}

impl Request {
    /// Starts a request with no questions yet — add them with
    /// [`Request::with`].
    #[must_use]
    pub fn new(observation: impl Into<Observation>) -> Self {
        Request {
            observation: observation.into(),
            questions: BTreeMap::new(),
        }
    }

    /// A request carrying exactly one named question.
    #[must_use]
    pub fn single(
        observation: impl Into<Observation>,
        name: impl Into<String>,
        question: Question,
    ) -> Self {
        Request::new(observation).with(name, question)
    }

    /// Adds a named question, batching it into the same call as any others
    /// already added.
    ///
    /// Batching independent questions is a pure win over one call per
    /// question: same round trip, same observation payload, strictly less
    /// latency. For a genuinely *dependent* pair (a second question whose
    /// real content differs by the first's answer), including every branch
    /// variant in one [`Request`] and keeping only the [`Answer`] matching
    /// the resolved first answer trades wasted compute for one round trip
    /// instead of two — worth it at shallow depth / small branching factor,
    /// since cost grows as `branching_factor^depth`.
    #[must_use]
    pub fn with(mut self, name: impl Into<String>, question: Question) -> Self {
        self.questions.insert(name.into(), question);
        self
    }
}

/// A response plus the transport metadata worth keeping.
#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    /// Question name -> answer.
    answers: BTreeMap<String, Answer>,
    /// Token accounting, when the endpoint reported it.
    pub usage: Option<crate::question::Usage>,
    /// The backend model that actually answered, when the endpoint reports
    /// it (useful behind a routing alias like `fpl/decide`, where the
    /// nominal request name and the model that resolved it can differ).
    pub resolved_model: Option<String>,
}

impl Outcome {
    #[must_use]
    pub(crate) fn new(
        answers: BTreeMap<String, Answer>,
        usage: Option<crate::question::Usage>,
        resolved_model: Option<String>,
    ) -> Self {
        Outcome {
            answers,
            usage,
            resolved_model,
        }
    }

    /// The answer to a named question.
    ///
    /// # Errors
    /// [`Error::MissingAnswer`] when the endpoint omitted it.
    pub fn answer(&self, name: &str) -> Result<&Answer, Error> {
        self.answers
            .get(name)
            .ok_or_else(|| Error::MissingAnswer(name.to_owned()))
    }

    /// Every answer, keyed by question name.
    #[must_use]
    pub fn answers(&self) -> &BTreeMap<String, Answer> {
        &self.answers
    }

    /// [`Outcome::answer`], additionally appending a [`Record`] of it to
    /// `trace`. Every sugar method on [`crate::Client`] goes through this so
    /// tracing can never be forgotten on one code path and not another.
    ///
    /// # Errors
    /// [`Error::MissingAnswer`] when the endpoint omitted it.
    pub fn recorded_answer(
        &self,
        name: &str,
        trace: &mut crate::trace::Trace,
    ) -> Result<&Answer, Error> {
        let answer = self.answer(name)?;
        trace.push(Record::now(name, answer));
        Ok(answer)
    }
}

/// Anything that can answer a batch of bounded questions about one
/// observation.
///
/// The seam exists so a caller can be exercised with no credentials and no
/// network ([`crate::ScriptedClient`]), and so a recorded exchange can be
/// replayed byte-for-byte.
pub trait Client {
    /// Puts `request`'s questions to the decision endpoint in one call.
    ///
    /// # Errors
    /// See [`Error`].
    fn decide(&self, request: &Request) -> Result<Outcome, Error>;
}
