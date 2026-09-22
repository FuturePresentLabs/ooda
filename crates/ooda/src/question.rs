//! The bounded question/answer wire types.
//!
//! There is no free text on this wire in either direction. A request carries
//! a JSON *observation* and a set of named *questions*, each one a closed set
//! the caller wrote down; a response carries, per question, the chosen key
//! (or rating, or probability) plus a confidence. That is what makes a
//! decision call usable as a step inside a deterministic pipeline: the set of
//! things that can come back is known before the call is made, so the
//! caller's control flow is a `match`, not a parse-and-hope.
//!
//! # Provenance
//!
//! This is the wire contract typesafe.ai's Jev ("System One") models answer,
//! and that Laya (`github.com/receptron/laya`, MIT, its FOSS ONNX-runtime
//! twin) targets compatibility with. All three variants' wire shapes are now
//! confirmed against the live `typesafe-ai/jev` route (2026-09-21, via the
//! `fpl/decide` routing alias): `Choice`'s `criteria` is a `{key ->
//! description}` object; `Score`'s `criteria` is a plain `[key, ...]` array
//! (an object is rejected outright); `Noul`'s wire tag is `"boolean"`, not
//! `"noul"`, and its answer field is `"probability"`, not `"noul"` (both
//! rejected/absent otherwise) — see each variant's docs for the exact
//! findings.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// An ordered set of `key -> description` pairs, preserving the order the
/// caller declared them in.
///
/// A plain `BTreeMap<String, String>` would silently re-sort keys
/// alphabetically on serialization, which is wrong wherever the order itself
/// carries meaning (a [`Question::Score`] rubric is declared low to high;
/// re-sorting `"high" < "low" < "medium"` alphabetically breaks that).
/// `Criteria` keeps insertion order and serializes as a JSON object in that
/// order, satisfying every wire shape this ecosystem's four prior
/// implementations used ([`Question::Choice`]'s confirmed `key ->
/// description` map among them) without losing information any of them
/// captured.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Criteria(Vec<(String, String)>);

impl Criteria {
    /// An empty set. Rarely useful directly — build with [`FromIterator`]
    /// or [`Criteria::push`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends one `(key, description)` pair, keeping declaration order.
    pub fn push(&mut self, key: impl Into<String>, description: impl Into<String>) -> &mut Self {
        self.0.push((key.into(), description.into()));
        self
    }

    /// The keys, in declaration order — the answer alphabet this question
    /// permits.
    #[must_use]
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(k, _)| k.as_str())
    }

    /// Whether `key` is one of the declared options.
    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.0.iter().any(|(k, _)| k == key)
    }

    /// The number of declared options.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether no options were declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<K, V> FromIterator<(K, V)> for Criteria
where
    K: Into<String>,
    V: Into<String>,
{
    fn from_iter<T: IntoIterator<Item = (K, V)>>(iter: T) -> Self {
        Criteria(
            iter.into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
    }
}

impl<K, V, const N: usize> From<[(K, V); N]> for Criteria
where
    K: Into<String>,
    V: Into<String>,
{
    fn from(pairs: [(K, V); N]) -> Self {
        pairs.into_iter().collect()
    }
}

impl Serialize for Criteria {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_map(self.0.iter().map(|(k, v)| (k, v)))
    }
}

/// A question put to the decision model. The variant tag serializes as
/// `type`, matching the wire format.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    /// Pick exactly one of up to 255 named options. **Confirmed** wire
    /// shape (see module docs): `criteria` as a `key -> description` object.
    Choice {
        /// What the model is being asked to decide.
        instructions: String,
        /// Option key -> description. The keys are the answer alphabet.
        criteria: Criteria,
    },
    /// Rate the observation against an ordered rubric, low to high.
    /// **Confirmed** wire shape as of 2026-09-21 against the live
    /// `typesafe-ai/jev` route: unlike [`Question::Choice`], `criteria`
    /// serializes as a plain JSON *array* of level keys (descriptions are
    /// not sent) — confirmed by the endpoint rejecting an object-shaped
    /// `criteria` for this variant with `"expected array, received
    /// object"`. [`Criteria`]'s own descriptions are still kept in the type
    /// for callers who want them, just not put on this wire.
    Score {
        /// What the model is being asked to rate.
        instructions: String,
        /// Level key -> description, ascending; serialized as just the
        /// ordered keys (see variant docs).
        #[serde(serialize_with = "criteria_as_key_array")]
        criteria: Criteria,
    },
    /// A calibrated yes/no probability — the probability itself is the
    /// answer, not a threshold decision on top of it. **Confirmed** wire
    /// tag as of 2026-09-21 against the live `typesafe-ai/jev` route: the
    /// tag is `"boolean"`, not `"noul"` (the endpoint rejects `"noul"` with
    /// `"Invalid discriminator value. Expected 'choice' | 'score' |
    /// 'boolean'"`). Kept as `Noul` in this crate's own vocabulary — the
    /// ecosystem term this crate is named after — since `boolean` is purely
    /// this one wire tag, not a naming choice worth propagating through the
    /// Rust API.
    #[serde(rename = "boolean")]
    Noul {
        /// The proposition whose probability is wanted.
        instructions: String,
        /// Optional `true`/`false` framing hints. Omitted entirely when
        /// `None` (a live call with no criteria at all has been confirmed to
        /// work); include them when they add real information beyond the
        /// instructions themselves.
        #[serde(skip_serializing_if = "Option::is_none")]
        criteria: Option<Criteria>,
    },
}

/// Serializes a [`Criteria`] as a plain array of its keys, dropping
/// descriptions — the wire shape [`Question::Score`] needs, distinct from
/// [`Criteria`]'s own `key -> description` object shape that
/// [`Question::Choice`] uses.
fn criteria_as_key_array<S: serde::Serializer>(
    criteria: &Criteria,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(criteria.keys())
}

impl Question {
    /// Builds a [`Question::Choice`] from `(key, description)` pairs, in
    /// declaration order.
    #[must_use]
    pub fn choice(instructions: impl Into<String>, criteria: impl Into<Criteria>) -> Self {
        Question::Choice {
            instructions: instructions.into(),
            criteria: criteria.into(),
        }
    }

    /// Builds a [`Question::Score`] from `(level_key, description)` pairs,
    /// ascending.
    #[must_use]
    pub fn score(instructions: impl Into<String>, criteria: impl Into<Criteria>) -> Self {
        Question::Score {
            instructions: instructions.into(),
            criteria: criteria.into(),
        }
    }

    /// Builds a [`Question::Noul`] with no framing hints.
    #[must_use]
    pub fn noul(instructions: impl Into<String>) -> Self {
        Question::Noul {
            instructions: instructions.into(),
            criteria: None,
        }
    }

    /// Builds a [`Question::Noul`] with `true`/`false` framing hints.
    #[must_use]
    pub fn noul_with_context(
        instructions: impl Into<String>,
        true_desc: impl Into<String>,
        false_desc: impl Into<String>,
    ) -> Self {
        Question::Noul {
            instructions: instructions.into(),
            criteria: Some(Criteria::from([
                ("true", true_desc.into()),
                ("false", false_desc.into()),
            ])),
        }
    }

    /// The option keys this question permits as an answer, for a `Choice`.
    #[must_use]
    pub fn option_keys(&self) -> Option<Vec<&str>> {
        match self {
            Question::Choice { criteria, .. } => Some(criteria.keys().collect()),
            Question::Score { .. } | Question::Noul { .. } => None,
        }
    }
}

/// A typed answer to one question.
///
/// Unknown fields are ignored, so a backend that grows the payload does not
/// break this client.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    /// One of the option keys the caller offered.
    Choice {
        /// The chosen option key.
        choice: String,
        /// Posterior over the offered option keys.
        #[serde(default)]
        probabilities: BTreeMap<String, f64>,
        /// How much to trust the choice, in `0.0..=1.0`.
        confidence: f64,
    },
    /// A rating against ordered levels.
    ///
    /// `score` is deliberately untyped: the wire shape is not confirmed
    /// (see module docs), and assuming `f64` would turn a documentation gap
    /// into a decode failure at the worst moment. Use
    /// [`Answer::score_as_f64`].
    Score {
        /// Raw score payload as returned.
        score: serde_json::Value,
        /// Posterior over levels, when present.
        #[serde(default)]
        probabilities: BTreeMap<String, f64>,
        /// Confidence, when present.
        #[serde(default)]
        confidence: Option<f64>,
    },
    /// A yes/no probability in `0.0..=1.0`. Documented to carry no
    /// confidence: the probability *is* the answer.
    ///
    /// Wire tag `"boolean"` and field `"probability"` — confirmed against
    /// the live `typesafe-ai/jev` route as of 2026-09-21 (see
    /// [`Question::Noul`]'s docs for the matching outgoing-side finding).
    #[serde(rename = "boolean")]
    Noul {
        /// Probability that the proposition holds.
        #[serde(rename = "probability")]
        noul: f64,
    },
}

impl Answer {
    /// The chosen option key, for a `Choice` answer.
    #[must_use]
    pub fn choice(&self) -> Option<&str> {
        match self {
            Answer::Choice { choice, .. } => Some(choice.as_str()),
            _ => None,
        }
    }

    /// The reported confidence, where the primitive carries one.
    ///
    /// `Noul` answers carry none: there is nothing separate to be confident
    /// about when the probability itself is the answer.
    #[must_use]
    pub fn confidence(&self) -> Option<f64> {
        match self {
            Answer::Choice { confidence, .. } => Some(*confidence),
            Answer::Score { confidence, .. } => *confidence,
            Answer::Noul { .. } => None,
        }
    }

    /// A `Score` answer's value, when it came back as a plain number.
    #[must_use]
    pub fn score_as_f64(&self) -> Option<f64> {
        match self {
            Answer::Score { score, .. } => score.as_f64(),
            _ => None,
        }
    }

    /// A `Noul` answer's probability.
    #[must_use]
    pub fn noul(&self) -> Option<f64> {
        match self {
            Answer::Noul { noul } => Some(*noul),
            _ => None,
        }
    }

    /// A short tag naming this answer's kind, for trace recording.
    #[must_use]
    pub(crate) fn kind(&self) -> crate::trace::Kind {
        match self {
            Answer::Choice { .. } => crate::trace::Kind::Choice,
            Answer::Score { .. } => crate::trace::Kind::Score,
            Answer::Noul { .. } => crate::trace::Kind::Noul,
        }
    }

    /// The value worth recording in a [`crate::trace::Record`] — the chosen
    /// key for a choice, else the numeric value formatted plainly.
    #[must_use]
    pub(crate) fn recorded_value(&self) -> String {
        match self {
            Answer::Choice { choice, .. } => choice.clone(),
            Answer::Score { score, .. } => score.to_string(),
            Answer::Noul { noul } => format!("{noul:.3}"),
        }
    }

    /// The confidence worth recording — [`Answer::confidence`], falling back
    /// to the `Noul` probability itself (its own calibration signal) so
    /// every answer kind has *something* a [`crate::trace::Trace`] can fold.
    #[must_use]
    pub(crate) fn recorded_confidence(&self) -> f64 {
        self.confidence().unwrap_or_else(|| match self {
            Answer::Noul { noul } => *noul,
            _ => 0.0,
        })
    }
}

/// Token accounting returned alongside the answers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Usage {
    /// Tokens consumed by the observation and questions.
    ///
    /// `inputTokens` is accepted alongside `input_tokens`: a gateway's
    /// different backend paths have been observed to report camelCase on one
    /// and snake_case on the other for the same logical field.
    #[serde(default, alias = "inputTokens")]
    pub input_tokens: u64,
    /// Tokens generated. Typically `0`: nothing is written, only decided.
    #[serde(default, alias = "outputTokens")]
    pub output_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choice_criteria_preserves_declaration_order_over_alphabetical() {
        let criteria = Criteria::from([("high", "top"), ("low", "bottom"), ("medium", "middle")]);
        let wire = serde_json::to_value(&Question::choice("pick one", criteria)).unwrap();
        let keys: Vec<&str> = wire["criteria"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["high", "low", "medium"]);
    }

    #[test]
    fn a_choice_question_serializes_to_the_confirmed_shape() {
        let question = Question::choice("pick one", [("a", "move it"), ("b", "open it")]);
        let wire = serde_json::to_value(&question).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "type": "choice",
                "instructions": "pick one",
                "criteria": { "a": "move it", "b": "open it" }
            })
        );
    }

    /// Confirmed against the live `typesafe-ai/jev` route: an object-shaped
    /// `criteria` is rejected outright for `score` (`"expected array,
    /// received object"`) — only the keys go on the wire, in declaration
    /// order, descriptions dropped.
    #[test]
    fn a_score_question_serializes_criteria_as_a_key_array_not_an_object() {
        let question = Question::score(
            "rate it",
            Criteria::from([("high", "top"), ("low", "bottom"), ("medium", "middle")]),
        );
        let wire = serde_json::to_value(&question).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "type": "score",
                "instructions": "rate it",
                "criteria": ["high", "low", "medium"]
            })
        );
    }

    /// Confirmed against the live `typesafe-ai/jev` route: the wire tag is
    /// `"boolean"`, not `"noul"` (the endpoint rejects `"noul"` with
    /// `"Invalid discriminator value. Expected 'choice' | 'score' |
    /// 'boolean'"`).
    #[test]
    fn a_noul_question_serializes_with_the_boolean_wire_tag() {
        let question = Question::noul("is this net critical?");
        let wire = serde_json::to_value(&question).unwrap();
        assert_eq!(wire["type"], "boolean");
        assert!(wire.get("criteria").is_none());
    }

    #[test]
    fn a_noul_question_can_carry_framing_hints() {
        let question = Question::noul_with_context("is this net critical?", "yes", "no");
        let wire = serde_json::to_value(&question).unwrap();
        assert_eq!(wire["criteria"]["true"], "yes");
        assert_eq!(wire["criteria"]["false"], "no");
    }

    #[test]
    fn a_choice_answer_decodes() {
        let raw =
            r#"{"type":"choice","choice":"a","probabilities":{"a":0.9,"b":0.1},"confidence":0.9}"#;
        let answer: Answer = serde_json::from_str(raw).unwrap();
        assert_eq!(answer.choice(), Some("a"));
        assert_eq!(answer.confidence(), Some(0.9));
    }

    #[test]
    fn a_noul_answer_carries_no_confidence_but_records_its_own_probability() {
        // Real captured shape from the live `typesafe-ai/jev` route
        // (2026-09-21): tag `"boolean"`, field `"probability"` — not
        // `"noul"`/`"noul"`.
        let raw = r#"{"type":"boolean","probability":0.42}"#;
        let answer: Answer = serde_json::from_str(raw).unwrap();
        assert_eq!(answer.noul(), Some(0.42));
        assert_eq!(answer.confidence(), None);
        assert_eq!(answer.recorded_confidence(), 0.42);
    }

    #[test]
    fn a_score_answer_decodes_whether_numeric_or_leveled() {
        let numeric: Answer =
            serde_json::from_str(r#"{"type":"score","score":3,"confidence":0.8}"#).unwrap();
        assert_eq!(numeric.score_as_f64(), Some(3.0));

        let leveled: Answer = serde_json::from_str(r#"{"type":"score","score":"high"}"#).unwrap();
        assert_eq!(leveled.score_as_f64(), None);
    }
}
