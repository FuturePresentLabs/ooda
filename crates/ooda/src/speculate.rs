//! Speculative branch pre-fetch: every follow-up asked alongside its root,
//! keeping only the branch that matches.
//!
//! Not a new wire capability — [`Request::with`] already batches any number
//! of independent named questions into one call. This is that batching used
//! one level deeper: a root [`Question::Choice`] plus one follow-up question
//! per possible root answer, all asked together. Once the root resolves,
//! `outcome.answer(&resolved_key)` is already the matching branch's answer —
//! no second round trip. The cost is real (every branch gets asked, whether
//! or not it ends up kept) but linear in the root's own option count, not
//! exponential with depth — which is exactly why [`crate::decide_staged`]'s
//! docs call this a shallow-depth, small-branching-factor trade, not a
//! blanket replacement for sequential staging.
//!
//! Every discarded branch's answer is real — the endpoint did answer it —
//! but it was never acted on, so [`decide_speculative`] never lets it reach
//! `trace`: [`crate::Trace::all_at_least`] folds every record it holds, and a
//! low-confidence answer to a branch nobody used must not be able to fail
//! that gate.

use std::collections::BTreeMap;

use crate::client::{Client, Observation, Outcome, Request};
use crate::error::Error;
use crate::question::{Answer, Question};
use crate::trace::{Record, Trace};

/// A root [`Question::Choice`] plus one follow-up question per possible root
/// answer, built with [`Speculation::branch`] and asked together by
/// [`decide_speculative`].
#[derive(Clone, Debug)]
pub struct Speculation {
    root_name: String,
    root: Question,
    branches: BTreeMap<String, Question>,
}

impl Speculation {
    /// Starts a speculation from a root question. `root` must be a
    /// [`Question::Choice`] — branching needs a finite, named answer key to
    /// key follow-ups by, which only `Choice` has. A non-`Choice` root is
    /// not rejected here; [`decide_speculative`] checks it before spending a
    /// real call.
    #[must_use]
    pub fn new(root_name: impl Into<String>, root: Question) -> Self {
        Speculation {
            root_name: root_name.into(),
            root,
            branches: BTreeMap::new(),
        }
    }

    /// Registers a follow-up question, asked in the same call as the root
    /// and every other branch, kept only if the root resolves to
    /// `root_answer_key`.
    #[must_use]
    pub fn branch(mut self, root_answer_key: impl Into<String>, question: Question) -> Self {
        self.branches.insert(root_answer_key.into(), question);
        self
    }
}

/// The root answer plus the one branch answer it resolved to.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolved {
    /// The root question's answer.
    pub root: Answer,
    /// The branch registered under the root's resolved key, if any —
    /// `None` for a root answer with no registered branch (a valid leaf,
    /// not an error: not every option needs a follow-up).
    pub branch: Option<Answer>,
}

/// Asks `speculation`'s root and every registered branch together, resolves
/// to the one branch matching the root's actual answer, and records only
/// that resolved path (root, then the matching branch if any) to `trace` —
/// see the module docs for why every discarded branch is deliberately kept
/// out of it.
///
/// # Errors
/// The underlying [`Client::decide`] error, or [`Error::WrongAnswerKind`] if
/// `speculation`'s root was not built as a [`Question::Choice`], or if the
/// endpoint answered it as something other than a choice.
pub fn decide_speculative(
    client: &impl Client,
    observation: impl Into<Observation>,
    speculation: Speculation,
    trace: &mut Trace,
) -> Result<Resolved, Error> {
    let Speculation {
        root_name,
        root,
        branches,
    } = speculation;
    if !matches!(root, Question::Choice { .. }) {
        return Err(Error::WrongAnswerKind {
            question: root_name,
            expected: "choice",
        });
    }

    let mut request = Request::new(observation).with(root_name.clone(), root);
    for (key, question) in branches {
        request = request.with(key, question);
    }

    let outcome = client.decide(&request)?;
    resolve(&outcome, &root_name, trace)
}

fn resolve(outcome: &Outcome, root_name: &str, trace: &mut Trace) -> Result<Resolved, Error> {
    let root = outcome.answer(root_name)?.clone();
    let resolved_key = root
        .choice()
        .ok_or_else(|| Error::WrongAnswerKind {
            question: root_name.to_owned(),
            expected: "choice",
        })?
        .to_owned();

    trace.push(Record::now(root_name, &root));

    let branch = outcome.answer(&resolved_key).ok().cloned();
    if let Some(branch) = &branch {
        trace.push(Record::now(resolved_key, branch));
    }

    Ok(Resolved { root, branch })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scripted::ScriptedClient;

    #[test]
    fn only_the_resolved_branch_is_kept_and_traced() {
        let client = ScriptedClient::new([r#"{"answers":{
            "category":{"type":"choice","choice":"pocket","confidence":0.9},
            "hole":{"type":"choice","choice":"countersink","confidence":0.4},
            "pocket":{"type":"choice","choice":"through","confidence":0.8}
        }}"#
        .to_owned()]);

        let speculation = Speculation::new(
            "category",
            Question::choice(
                "pick a category",
                [("hole", "add a hole"), ("pocket", "add a pocket")],
            ),
        )
        .branch(
            "hole",
            Question::choice(
                "entry treatment",
                [("countersink", "countersunk"), ("none", "plain")],
            ),
        )
        .branch(
            "pocket",
            Question::choice(
                "depth",
                [("through", "through pocket"), ("blind", "blind pocket")],
            ),
        );

        let mut trace = Trace::new();
        let resolved =
            decide_speculative(&client, serde_json::json!({}), speculation, &mut trace).unwrap();

        assert_eq!(resolved.root.choice(), Some("pocket"));
        assert_eq!(resolved.branch.as_ref().and_then(Answer::choice), Some("through"));

        // Only the resolved path landed in trace -- "hole"'s 0.4-confidence
        // branch, asked but discarded, must not be able to fail the gate.
        let keys: Vec<&str> = trace.records().iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, ["category", "pocket"]);
        assert!(trace.all_at_least(0.8));
    }

    #[test]
    fn a_resolved_key_with_no_registered_branch_is_a_valid_leaf_not_an_error() {
        let client = ScriptedClient::new([r#"{"answers":{
            "category":{"type":"choice","choice":"none","confidence":0.95}
        }}"#
        .to_owned()]);

        let speculation = Speculation::new(
            "category",
            Question::choice(
                "pick a category",
                [("hole", "add a hole"), ("none", "no change")],
            ),
        )
        .branch(
            "hole",
            Question::choice("entry treatment", [("countersink", "countersunk")]),
        );

        let mut trace = Trace::new();
        let resolved =
            decide_speculative(&client, serde_json::json!({}), speculation, &mut trace).unwrap();

        assert_eq!(resolved.root.choice(), Some("none"));
        assert_eq!(resolved.branch, None);
        assert_eq!(trace.records().len(), 1);
    }

    #[test]
    fn a_non_choice_root_is_rejected_before_any_call_is_made() {
        let client = ScriptedClient::new(Vec::<String>::new());
        let speculation = Speculation::new("confidence", Question::noul("is this correct?"));
        let mut trace = Trace::new();
        let result = decide_speculative(&client, serde_json::json!({}), speculation, &mut trace);
        assert!(matches!(result, Err(Error::WrongAnswerKind { .. })));
    }
}
