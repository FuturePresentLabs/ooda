//! Dependent decision chains: a later [`Request`] built from an earlier
//! [`Outcome`].
//!
//! [`Request::with`] already covers *independent* batching — any number of
//! named questions in one call, because none of them needs another's
//! answer first. This module is the other half: a genuinely dependent
//! chain, where stage 2's real content (which options to offer, what to
//! even ask) depends on how stage 1 resolved. A curated CAD move catalog
//! deciding a category first ("hole" vs "pocket" vs "finish") and only then
//! asking that category's own follow-ups is the motivating case; the same
//! shape shows up anywhere a decision tree has a branch worth asking about
//! rather than pre-fetching.
//!
//! This is deliberately *not* the speculative-pre-fetch idea — see
//! [`crate::decide_speculative`] for that one (every branch of Q2 asked
//! alongside Q1, keep only the one that matches; trades wasted compute for
//! one round trip, worth it at shallow depth/small branching factor).
//! Sequential staging here is the plain version: each stage costs a real
//! call, in exchange for asking only the questions that turn out to matter.

use crate::client::{Client, Outcome, Request};
use crate::error::Error;
use crate::trace::Trace;

/// Stages beyond this many without the continuation returning `None` is
/// refused as [`Error::TooManyStages`] rather than run forever. A bug in the
/// continuation would otherwise turn into an unbounded sequence of real,
/// billed decision calls; no real curated decision tree in this ecosystem
/// is anywhere near this deep.
pub const MAX_STAGES: u32 = 16;

/// Asks a chain of dependent [`Request`]s.
///
/// `first` is asked, then `next` inspects its [`Outcome`] and either builds
/// the next stage's `Request` (`Some`) or ends the chain (`None`). Every
/// stage's full answer set is appended to `trace` in order
/// ([`Outcome::record_all`]) — the whole chain's audit trail, not just the
/// fields `next` happened to read.
///
/// Returns every stage's [`Outcome`], in order, so a caller can read
/// per-stage detail (probabilities, usage) that the flattened [`Trace`]
/// doesn't carry.
///
/// # Errors
/// The underlying [`Client::decide`] error from whichever stage failed — a
/// failure mid-chain still leaves every earlier stage's answers in `trace`
/// and every earlier stage's [`Outcome`] is not returned (the call itself
/// returns `Err`, not a partial `Vec`). [`Error::TooManyStages`] if `next`
/// keeps asking past [`MAX_STAGES`].
pub fn decide_staged(
    client: &impl Client,
    first: Request,
    trace: &mut Trace,
    mut next: impl FnMut(&Outcome) -> Option<Request>,
) -> Result<Vec<Outcome>, Error> {
    let mut outcomes = Vec::new();
    let mut request = first;
    loop {
        if outcomes.len() as u32 >= MAX_STAGES {
            return Err(Error::TooManyStages(MAX_STAGES));
        }
        let outcome = client.decide(&request)?;
        outcome.record_all(trace);
        let continuation = next(&outcome);
        outcomes.push(outcome);
        match continuation {
            Some(next_request) => request = next_request,
            None => break,
        }
    }
    Ok(outcomes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::question::Question;
    use crate::scripted::ScriptedClient;

    #[test]
    fn a_single_stage_chain_stops_when_next_returns_none() {
        let client = ScriptedClient::new([
            r#"{"answers":{"category":{"type":"choice","choice":"hole","confidence":0.9}}}"#
                .to_owned(),
        ]);
        let first = Request::single(
            serde_json::json!({}),
            "category",
            Question::choice("pick a move category", [("hole", "add a hole")]),
        );
        let mut trace = Trace::new();
        let outcomes = decide_staged(&client, first, &mut trace, |_outcome| None).unwrap();

        assert_eq!(outcomes.len(), 1);
        assert_eq!(trace.records().len(), 1);
        assert_eq!(trace.records()[0].key, "category");
    }

    #[test]
    fn stage_two_is_built_from_stage_ones_answer_and_both_are_traced() {
        let client = ScriptedClient::new([
            r#"{"answers":{"category":{"type":"choice","choice":"hole","confidence":0.9}}}"#
                .to_owned(),
            r#"{"answers":{
                "entry":{"type":"choice","choice":"countersink","confidence":0.8},
                "threaded":{"type":"boolean","probability":0.1}
            }}"#
            .to_owned(),
        ]);
        let first = Request::single(
            serde_json::json!({}),
            "category",
            Question::choice(
                "pick a move category",
                [("hole", "add a hole"), ("pocket", "add a pocket")],
            ),
        );
        let mut trace = Trace::new();
        let mut asked_category: Option<String> = None;
        let outcomes = decide_staged(&client, first, &mut trace, |outcome| {
            let category = outcome.answer("category").ok()?.choice()?.to_owned();
            asked_category = Some(category.clone());
            if category != "hole" {
                return None;
            }
            Some(
                Request::new(serde_json::json!({}))
                    .with(
                        "entry",
                        Question::choice(
                            "entry treatment",
                            [("none", "no chamfer"), ("countersink", "countersunk entry")],
                        ),
                    )
                    .with("threaded", Question::noul("is this hole threaded?")),
            )
        })
        .unwrap();

        assert_eq!(asked_category.as_deref(), Some("hole"));
        assert_eq!(outcomes.len(), 2);
        assert_eq!(
            outcomes[1].answer("entry").unwrap().choice(),
            Some("countersink")
        );

        // Every answer from both stages landed in the trace, not just the
        // ones the continuation happened to read.
        let keys: Vec<&str> = trace.records().iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, ["category", "entry", "threaded"]);
    }

    #[test]
    fn a_continuation_that_never_stops_is_refused_not_run_forever() {
        let bodies = (0..MAX_STAGES + 5).map(|_| {
            r#"{"answers":{"q":{"type":"choice","choice":"a","confidence":0.9}}}"#.to_owned()
        });
        let client = ScriptedClient::new(bodies);
        let first = Request::single(
            serde_json::json!({}),
            "q",
            Question::choice("pick", [("a", "desc")]),
        );
        let mut trace = Trace::new();
        let result = decide_staged(&client, first, &mut trace, |_outcome| {
            Some(Request::single(
                serde_json::json!({}),
                "q",
                Question::choice("pick", [("a", "desc")]),
            ))
        });
        assert!(matches!(result, Err(Error::TooManyStages(n)) if n == MAX_STAGES));
    }
}
