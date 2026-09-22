//! The decision history of a run — the "aggregate state" side of OODA.
//!
//! Every prior implementation surveyed in this ecosystem wanted roughly the
//! same thing here: a `{key, kind, chosen, confidence, timestamp}` record per
//! call, and a fold over the whole run's records that fails on an *empty*
//! trace rather than treating "nothing was decided" as vacuously passing.
//! This module is that record type and that fold, built once.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Which [`crate::Question`] variant produced a [`Record`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// From a [`crate::Question::Choice`].
    Choice,
    /// From a [`crate::Question::Score`].
    Score,
    /// From a [`crate::Question::Noul`].
    Noul,
}

/// One recorded decision, kept for audit and downstream eval scoring (a
/// benchmark harness reading a serialized trace to score decision confidence
/// as an explicit rubric criterion, not just a pass/fail outcome).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// The question's name, as given to [`crate::Request::with`].
    pub key: String,
    /// Which question kind produced this record.
    pub kind: Kind,
    /// The chosen key (`Choice`), or the value formatted plainly
    /// (`Score`/`Noul`).
    pub chosen: String,
    /// The confidence folded over by [`Trace::all_at_least`]. For a `Noul`
    /// answer (which carries no separate confidence field) this is the
    /// probability itself.
    pub confidence: f64,
    /// Unix seconds when the record was appended.
    pub timestamp_unix: u64,
}

impl Record {
    pub(crate) fn now(key: impl Into<String>, answer: &crate::Answer) -> Self {
        let timestamp_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Record {
            key: key.into(),
            kind: answer.kind(),
            chosen: answer.recorded_value(),
            confidence: answer.recorded_confidence(),
            timestamp_unix,
        }
    }
}

/// A run's decision history, foldable into a confidence gate.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Trace(Vec<Record>);

impl Trace {
    /// An empty trace.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a record.
    pub fn push(&mut self, record: Record) {
        self.0.push(record);
    }

    /// Every record, in the order they were appended.
    #[must_use]
    pub fn records(&self) -> &[Record] {
        &self.0
    }

    /// Whether every recorded decision cleared `min_confidence`, **and** at
    /// least one decision was recorded.
    ///
    /// An empty trace deliberately does not pass: "nothing was decided" is
    /// not the same claim as "everything decided was confident," and
    /// treating them the same turns a caller that never wired up any
    /// decisions into a silent, permanent pass.
    #[must_use]
    pub fn all_at_least(&self, min_confidence: f64) -> bool {
        !self.0.is_empty() && self.0.iter().all(|r| r.confidence >= min_confidence)
    }

    /// The lowest confidence recorded, if any decisions were made.
    #[must_use]
    pub fn min_confidence(&self) -> Option<f64> {
        self.0
            .iter()
            .map(|r| r.confidence)
            .fold(None, |acc, c| Some(acc.map_or(c, |a: f64| a.min(c))))
    }
}

impl IntoIterator for Trace {
    type Item = Record;
    type IntoIter = std::vec::IntoIter<Record>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(confidence: f64) -> Record {
        Record {
            key: "k".into(),
            kind: Kind::Choice,
            chosen: "a".into(),
            confidence,
            timestamp_unix: 0,
        }
    }

    #[test]
    fn an_empty_trace_does_not_vacuously_pass() {
        assert!(!Trace::new().all_at_least(0.0));
    }

    #[test]
    fn passes_only_when_every_record_clears_the_bar() {
        let mut trace = Trace::new();
        trace.push(record(0.9));
        trace.push(record(0.95));
        assert!(trace.all_at_least(0.8));
        trace.push(record(0.5));
        assert!(!trace.all_at_least(0.8));
    }

    #[test]
    fn min_confidence_tracks_the_lowest_record() {
        let mut trace = Trace::new();
        assert_eq!(trace.min_confidence(), None);
        trace.push(record(0.9));
        trace.push(record(0.4));
        trace.push(record(0.7));
        assert_eq!(trace.min_confidence(), Some(0.4));
    }
}
