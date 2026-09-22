//! End-to-end check of `#[derive(ooda::Choice)]` against the real macro (not
//! a hand-written `ChoiceSpace` impl, unlike the unit tests in
//! `src/choice.rs`) — this is the specific capability requested when this
//! crate was commissioned ("let them define an enum per step with serde
//! etc.") and deserves its own proof that the derive actually produces a
//! working `ChoiceSpace`.

use ooda::{Choice, ChoiceSpace, Decision, ScriptedClient, decide_choice, decide_choice_traced};
use serde::{Deserialize, Serialize};

/// A plain `serde`-derived enum, with no `ooda` attributes at all — the
/// minimal case: doc comments become descriptions, bare variant identifiers
/// become keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Choice)]
enum PlainChoice {
    /// Move the feature to clear the anchor.
    MoveIt,
    /// Open a relief pocket instead.
    OpenIt,
}

/// Exercises the `#[serde(rename_all = "...")]` + per-variant override path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Choice)]
#[serde(rename_all = "snake_case")]
enum RenamedChoice {
    /// Uses the enum's snake_case convention.
    MoveIt,
    /// Uses its own serde rename instead of the enum convention.
    #[serde(rename = "OPEN")]
    OpenIt,
    /// Uses its own serde rename, overriding the enum's rename_all.
    #[serde(rename = "abstain")]
    None,
}

#[test]
fn plain_variants_use_bare_identifiers_as_keys() {
    let options = PlainChoice::options();
    let keys: Vec<&str> = options.keys().collect();
    assert_eq!(keys, ["MoveIt", "OpenIt"]);
    assert_eq!(PlainChoice::from_key("MoveIt"), Some(PlainChoice::MoveIt));
    assert_eq!(PlainChoice::from_key("nope"), None);
}

#[test]
fn doc_comments_become_descriptions() {
    let request = ooda::Request::single(
        serde_json::json!({}),
        "edit_choice",
        ooda::Question::choice("pick one", PlainChoice::options()),
    );
    let wire = serde_json::to_value(&request.questions["edit_choice"]).unwrap();
    assert_eq!(
        wire["criteria"]["MoveIt"],
        "Move the feature to clear the anchor."
    );
}

#[test]
fn key_resolution_precedence_is_serde_rename_then_rename_all() {
    let options = RenamedChoice::options();
    let keys: Vec<&str> = options.keys().collect();
    assert_eq!(keys, ["move_it", "OPEN", "abstain"]);
    assert_eq!(
        RenamedChoice::from_key("move_it"),
        Some(RenamedChoice::MoveIt)
    );
    assert_eq!(RenamedChoice::from_key("OPEN"), Some(RenamedChoice::OpenIt));
    assert_eq!(
        RenamedChoice::from_key("abstain"),
        Some(RenamedChoice::None)
    );
}

#[test]
fn decide_choice_resolves_a_derived_enum_end_to_end() {
    let client = ScriptedClient::new([
        r#"{"answers":{"edit_choice":{"type":"choice","choice":"OpenIt",
            "probabilities":{"OpenIt":0.8},"confidence":0.8}}}"#
            .to_owned(),
    ]);
    let decision: Decision<PlainChoice> =
        decide_choice(&client, serde_json::json!({}), "edit_choice", "pick one").unwrap();
    assert_eq!(decision.answer, PlainChoice::OpenIt);
    assert_eq!(decision.confidence, 0.8);

    let mut trace = ooda::Trace::new();
    let client2 = ScriptedClient::new([
        r#"{"answers":{"edit_choice":{"type":"choice","choice":"MoveIt","confidence":0.99}}}"#
            .to_owned(),
    ]);
    let traced: Decision<PlainChoice> = decide_choice_traced(
        &client2,
        serde_json::json!({}),
        "edit_choice",
        "pick one",
        &mut trace,
    )
    .unwrap();
    assert_eq!(traced.answer, PlainChoice::MoveIt);
    assert!(trace.all_at_least(0.9));
}
