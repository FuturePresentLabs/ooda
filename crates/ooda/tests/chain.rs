//! The decision → consequence → reward chain, end to end.
//!
//! A captured record holds a request and the answer to it. That is everything
//! knowable when `decide` returns, and it is deliberately not everything a
//! caller eventually wants: the *consequence* of acting on the answer cannot
//! be in that record, because the next state is caused by the action that has
//! not been taken yet. Reward is a function of (state, action, next_state), so
//! it is necessarily computed later, by the caller, somewhere else.
//!
//! That makes the join the load-bearing part. Without a shared key, a corpus
//! of decisions and a log of outcomes are two piles of JSON that can never be
//! put back together, and the decisions are trainable only by behaviour
//! cloning — never evaluated, never rewarded.
//!
//! This test is the executable statement of that contract: it plays the
//! caller's role, keeps its own consequence log exactly as a caller would, and
//! rejoins the two afterwards.

#![cfg(feature = "capture")]

use std::collections::BTreeMap;

use ooda::{
    Answer, Capture, CapturingClient, Client, Criteria, Question, Request, ScriptedClient,
};

/// What a caller records once it knows what happened — the half `ooda` cannot
/// hold. Keyed by the same correlation it put on the request.
struct Consequence {
    correlation: String,
    /// The action actually taken. Not always the answer: a caller that
    /// samples from the posterior executes something the model only ranked.
    executed: String,
    score_before: i64,
    score_after: i64,
}

#[test]
fn a_captured_decision_rejoins_the_consequence_of_acting_on_it() {
    let dir = std::env::temp_dir().join(format!("ooda-chain-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let reply = r#"{"answers":{"action":{"type":"choice","choice":"accelerate","confidence":0.6,"probabilities":{"accelerate":0.7,"brake":0.3}}}}"#;
    let capture = Capture::at(&dir)
        .expect("open a capture log")
        .labelled([("game", "smk"), ("policy", "rollout")]);
    let run_id = capture.run_id().to_owned();
    let client = CapturingClient::new(
        ScriptedClient::new([reply.to_owned(), reply.to_owned()]),
        capture,
    );

    // The caller drives its own episode and keeps its own log, as it must.
    let mut consequences = Vec::new();
    let mut score = 100_i64;
    for step in 0..2 {
        let correlation = format!("smk-session-{step}");
        let mut offered = Criteria::new();
        offered.push("accelerate", "Hold the throttle");
        offered.push("brake", "Slow for the corner");
        let request = Request::single(
            serde_json::json!({"speed": 180 + step, "score": score}),
            "action",
            Question::choice("Drive the kart", offered),
        )
        .correlated(&correlation);

        let outcome = client.decide(&request).expect("a scripted decision");
        let Answer::Choice { choice, .. } = outcome.answer("action").expect("an action") else {
            panic!("a choice question must yield a choice answer");
        };

        // Acting is what produces the next state -- which is precisely why it
        // could not have been in the captured record.
        let before = score;
        score += if choice == "accelerate" { 10 } else { -2 };
        consequences.push(Consequence {
            correlation,
            executed: choice.clone(),
            score_before: before,
            score_after: score,
        });
    }

    // --- Later, offline: rejoin the two logs. -----------------------------
    let log = std::fs::read_to_string(dir.join("decisions.jsonl")).expect("the log exists");
    let captured: Vec<serde_json::Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).expect("one record per line"))
        .collect();
    assert_eq!(captured.len(), 2);

    let by_correlation: BTreeMap<String, &serde_json::Value> = captured
        .iter()
        .map(|row| {
            (
                row["correlation"]
                    .as_str()
                    .expect("every record carries the caller's id")
                    .to_owned(),
                row,
            )
        })
        .collect();

    for consequence in &consequences {
        let decision = by_correlation
            .get(&consequence.correlation)
            .unwrap_or_else(|| panic!("no captured decision for {}", consequence.correlation));

        // The join yields a complete RL transition: the state that was
        // observed, the action taken, and the reward that followed.
        let observed_score = decision
            .pointer("/request/observation/score")
            .and_then(serde_json::Value::as_i64)
            .expect("the observation is the state half of the transition");
        assert_eq!(
            observed_score, consequence.score_before,
            "the state in the capture must be the state the caller acted from"
        );

        let answered = decision
            .pointer("/outcome/answers/action/choice")
            .and_then(serde_json::Value::as_str)
            .expect("the answer is the action half");
        assert_eq!(answered, consequence.executed);

        let reward = consequence.score_after - consequence.score_before;
        assert_eq!(reward, 10, "reward is computable only after the join");

        // Provenance survives too, so a mixed corpus stays filterable.
        assert_eq!(decision["labels"]["game"], "smk");
        assert_eq!(decision["run_id"], run_id.as_str());
    }

    // The run is addressable as a unit, which is what a train/test split has
    // to divide on -- consecutive decisions within one run are correlated, so
    // splitting by row leaks the test set into training.
    assert!(
        captured
            .iter()
            .all(|row| row["run_id"] == run_id.as_str()),
        "one run's records must share one run id"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The teacher half of the same chain: a deterministic policy answering the
/// identical typed question, captured through the identical path. Without
/// `Outcome::answered` this could not be written at all, and demonstrations
/// would need a second format that has to be reconciled by hand forever.
#[test]
fn a_deterministic_teacher_chains_the_same_way() {
    struct Baseline;
    impl Client for Baseline {
        fn decide(&self, request: &Request) -> Result<ooda::Outcome, ooda::Error> {
            // Plays from the same observation a model would get.
            let speed = request.observation["speed"].as_i64().unwrap_or(0);
            let choice = if speed > 190 { "brake" } else { "accelerate" };
            Ok(ooda::Outcome::answered([(
                "action",
                Answer::Choice {
                    choice: choice.to_owned(),
                    probabilities: BTreeMap::new(),
                    confidence: 1.0,
                },
            )]))
        }
    }

    let dir = std::env::temp_dir().join(format!("ooda-chain-teacher-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let capture = Capture::at(&dir)
        .expect("open a capture log")
        .labelled([("game", "smk"), ("policy", "teacher")]);
    let client = CapturingClient::new(Baseline, capture);

    let mut offered = Criteria::new();
    offered.push("accelerate", "Hold the throttle");
    offered.push("brake", "Slow for the corner");
    client
        .decide(
            &Request::single(
                serde_json::json!({"speed": 195}),
                "action",
                Question::choice("Drive the kart", offered),
            )
            .correlated("smk-teacher-0"),
        )
        .expect("a local policy decides");

    let log = std::fs::read_to_string(dir.join("decisions.jsonl")).expect("the log exists");
    let row: serde_json::Value =
        serde_json::from_str(log.lines().next().expect("one record")).expect("valid JSON");

    assert_eq!(row["correlation"], "smk-teacher-0");
    assert_eq!(row["labels"]["policy"], "teacher");
    assert_eq!(row.pointer("/outcome/answers/action/choice").unwrap(), "brake");
    assert_eq!(
        row.pointer("/request/observation/speed").unwrap(),
        195,
        "a demonstration is trainable only with the observation it answered"
    );
    assert!(
        row.pointer("/outcome/resolved_model")
            .is_none_or(serde_json::Value::is_null),
        "no endpoint answered, so no model may be claimed"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
