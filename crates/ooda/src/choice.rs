//! Scoping a decision to a plain Rust enum.
//!
//! Every prior implementation surveyed for this crate bounded a `Choice`
//! question with a stringly-typed map (`BTreeMap<&str, &str>` or similar) at
//! the call site, then matched the returned key back against the same
//! strings by hand. [`ChoiceSpace`] replaces that with a closed Rust type:
//! implement it once (or derive it, with the `derive` feature) and a
//! `Choice` question's option set and its answer's parse-back both come from
//! the same enum, so they cannot drift apart.

use crate::client::{Client, Observation, Outcome, Request};
use crate::error::Error;
use crate::question::{Criteria, Question};
use crate::trace::Trace;

#[cfg(feature = "derive")]
pub use ooda_derive::Choice;

/// A closed, named set of options a [`Question::Choice`] can resolve to.
///
/// Implement by hand, or `#[derive(ooda::Choice)]` (the `derive` feature,
/// on by default) on a unit-variant-only enum — the derive reads each
/// variant's doc comment as its description (required) and the variant's
/// name, cased per any `#[serde(rename_all = "...")]`/`#[serde(rename =
/// "...")]` already on the enum, as its wire key.
pub trait ChoiceSpace: Sized {
    /// Every option this type can resolve to, key -> description, in
    /// declaration order.
    fn options() -> Criteria;

    /// Parses a wire key back into a variant.
    ///
    /// `None` for a key outside the declared set — a caller must treat that
    /// as [`Error::UnknownChoice`], never a silent default, since the whole
    /// point of a bounded question is that this should not be reachable.
    fn from_key(key: &str) -> Option<Self>;
}

/// A typed, confidence-scored decision: [`ChoiceSpace::from_key`] applied to
/// a resolved [`crate::Answer::Choice`].
#[derive(Clone, Debug, PartialEq)]
pub struct Decision<T> {
    /// The resolved variant.
    pub answer: T,
    /// How much to trust it, in `0.0..=1.0`.
    pub confidence: f64,
    /// Posterior over the offered option keys.
    pub probabilities: std::collections::BTreeMap<String, f64>,
}

/// [`Client::decide`] sugar for a single [`ChoiceSpace`]-typed question.
///
/// A free function rather than a trait method so [`Client`] stays a plain,
/// object-safe seam — this is a convenience layered on top, not part of the
/// contract every implementor must satisfy.
///
/// # Errors
/// The underlying [`Client::decide`] error, or [`Error::UnknownChoice`] if
/// the endpoint answered outside `T`'s declared option set.
pub fn decide_choice<T: ChoiceSpace>(
    client: &impl Client,
    observation: impl Into<Observation>,
    name: &str,
    instructions: impl Into<String>,
) -> Result<Decision<T>, Error> {
    let request = Request::single(
        observation,
        name,
        Question::choice(instructions, T::options()),
    );
    let outcome = client.decide(&request)?;
    decision_from_outcome(&outcome, name)
}

/// [`decide_choice`], additionally appending the resolved decision to
/// `trace` as a [`crate::trace::Record`].
///
/// # Errors
/// See [`decide_choice`].
pub fn decide_choice_traced<T: ChoiceSpace>(
    client: &impl Client,
    observation: impl Into<Observation>,
    name: &str,
    instructions: impl Into<String>,
    trace: &mut Trace,
) -> Result<Decision<T>, Error> {
    let request = Request::single(
        observation,
        name,
        Question::choice(instructions, T::options()),
    );
    let outcome = client.decide(&request)?;
    outcome.recorded_answer(name, trace)?;
    decision_from_outcome(&outcome, name)
}

fn decision_from_outcome<T: ChoiceSpace>(
    outcome: &Outcome,
    name: &str,
) -> Result<Decision<T>, Error> {
    let answer = outcome.answer(name)?;
    let crate::question::Answer::Choice {
        choice,
        probabilities,
        confidence,
    } = answer
    else {
        return Err(Error::WrongAnswerKind {
            question: name.to_owned(),
            expected: "choice",
        });
    };
    let resolved = T::from_key(choice).ok_or_else(|| Error::UnknownChoice {
        question: name.to_owned(),
        chosen: choice.clone(),
    })?;
    Ok(Decision {
        answer: resolved,
        confidence: *confidence,
        probabilities: probabilities.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScriptedClient;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum EditChoice {
        MoveIt,
        OpenIt,
        None,
    }

    impl ChoiceSpace for EditChoice {
        fn options() -> Criteria {
            Criteria::from([
                ("move_it", "Move the feature to clear the anchor"),
                ("open_it", "Open a relief pocket instead"),
                ("none", "Neither applies; abstain"),
            ])
        }

        fn from_key(key: &str) -> Option<Self> {
            match key {
                "move_it" => Some(EditChoice::MoveIt),
                "open_it" => Some(EditChoice::OpenIt),
                "none" => Some(EditChoice::None),
                _ => None,
            }
        }
    }

    #[test]
    fn resolves_a_declared_key_into_its_variant() {
        let client = ScriptedClient::new([
            r#"{"answers":{"edit_choice":{"type":"choice","choice":"move_it",
                "probabilities":{"move_it":0.9},"confidence":0.9}}}"#
                .to_owned(),
        ]);
        let decision: Decision<EditChoice> =
            decide_choice(&client, serde_json::json!({}), "edit_choice", "pick one").unwrap();
        assert_eq!(decision.answer, EditChoice::MoveIt);
        assert_eq!(decision.confidence, 0.9);
    }

    #[test]
    fn a_key_outside_the_declared_set_fails_loud() {
        let client = ScriptedClient::new([
            r#"{"answers":{"edit_choice":{"type":"choice","choice":"delete_it",
                "confidence":0.9}}}"#
                .to_owned(),
        ]);
        let result: Result<Decision<EditChoice>, Error> =
            decide_choice(&client, serde_json::json!({}), "edit_choice", "pick one");
        assert!(matches!(result, Err(Error::UnknownChoice { .. })));
    }

    #[test]
    fn traced_variant_appends_a_record() {
        let client = ScriptedClient::new([
            r#"{"answers":{"edit_choice":{"type":"choice","choice":"none",
                "confidence":0.7}}}"#
                .to_owned(),
        ]);
        let mut trace = Trace::new();
        let decision: Decision<EditChoice> = decide_choice_traced(
            &client,
            serde_json::json!({}),
            "edit_choice",
            "pick one",
            &mut trace,
        )
        .unwrap();
        assert_eq!(decision.answer, EditChoice::None);
        assert_eq!(trace.records().len(), 1);
        assert_eq!(trace.records()[0].chosen, "none");
    }
}
