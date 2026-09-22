//! Runs a single bounded decision end to end against a canned response — no
//! network, no credentials. Swap `ScriptedClient` for `HttpClient::from_env()`
//! to hit a real endpoint.
//!
//! ```bash
//! cargo run -p ooda --example edit_choice
//! ```

use ooda::{Choice, Decision, ScriptedClient, decide_choice};
use serde::{Deserialize, Serialize};

/// The bounded set of edits a caller might make to bring a design into
/// conformance — a shape modeled on a real CAD design-agent's decision step.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Choice)]
enum EditChoice {
    /// Move the feature to clear the anchor.
    MoveIt,
    /// Open a relief pocket instead.
    OpenIt,
    /// Neither edit applies; do not touch the design.
    #[serde(rename = "none")]
    Abstain,
}

fn main() {
    // In production this would be `HttpClient::from_env()?`, talking to
    // `ai.fpl.dev` or another System One-compatible endpoint. Here it's a
    // canned response so the example runs with no setup.
    let client = ScriptedClient::new([r#"{
        "answers": {
            "edit_choice": {
                "type": "choice",
                "choice": "move_it",
                "probabilities": { "move_it": 0.87, "open_it": 0.11, "none": 0.02 },
                "confidence": 0.87
            }
        }
    }"#]);

    let observation = serde_json::json!({
        "deviation_mm": 0.18,
        "tolerance_mm": 0.10,
        "feature": "boss_a4",
    });

    let decision: Decision<EditChoice> = decide_choice(
        &client,
        observation,
        "edit_choice",
        "The boss is out of tolerance. Which edit brings it into conformance?",
    )
    .expect("scripted client always answers");

    println!(
        "resolved: {:?} (confidence {:.2}, probabilities {:?})",
        decision.answer, decision.confidence, decision.probabilities
    );

    // The whole point of a bounded question: the match is total, not a
    // parse-and-hope over free text.
    match decision.answer {
        EditChoice::MoveIt => println!("-> translate the feature toward the anchor"),
        EditChoice::OpenIt => println!("-> add a relief pocket"),
        EditChoice::Abstain => println!("-> leave the design untouched"),
    }
}
