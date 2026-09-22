# ooda

[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![tests](https://img.shields.io/badge/tests-42%20passing-brightgreen.svg)](#status)
[![built on](https://img.shields.io/badge/built%20on-typesafe.ai%20Jev-6b46c1.svg)](https://typesafe.ai)
[![status](https://img.shields.io/badge/status-in%20production-success.svg)](#status)

*(Badge numbers are generated — run `scripts/update-badges.sh` after a test count changes; don't hand-edit them.)*

**Observe. Orient. Decide. Act.** That's all it takes to build an RLCD
harness. This crate is the Observe/Orient/Decide plumbing — strongly typed
over [typesafe.ai](https://typesafe.ai)'s Jev, and
[Laya](https://github.com/receptron/laya) (its MIT-licensed FOSS twin).

Bounded `Choice`/`Score`/`Noul` questions in. Typed, confidence-scored
answers out. No free text, either direction. Act is always the caller's own
concern — this crate stops at the decision.

Already running in production across EDA/PCB design and CAD/CAM workflows.
Copy our homework.

## 5 lines

```rust
use ooda::{Choice, HttpClient, decide_choice};

#[derive(serde::Serialize, serde::Deserialize, Choice)]
enum Edit { /// move it clear of the anchor
            MoveIt, /// open a relief pocket instead
            OpenIt }

let client = HttpClient::from_env()?; // -> typesafe.ai's Jev, by default
let decision = decide_choice(&client, state, "edit", "pick one")?;
```

`decision.answer` is an `Edit`, not a string. The match is total — not a
parse-and-hope.

## Why this exists

Sibling Rust projects across this ecosystem — `surf`, `speedy`,
`legion-of-bom`, and others — each reimplemented the same Jev/Laya wire
client independently. Each was missing a fix another one had already found:

- **Retry semantics.** 429 honors `Retry-After`. 5xx/transport errors back
  off. A real 4xx doesn't retry. Both the flat and nested `error` shapes a
  gateway can send are read.
- **Confidence relocation.** A live call against `ai.fpl.dev` showed a
  `choice` answer's confidence sometimes arrives out-of-band, under
  `providerMetadata.typesafe.confidence.<name>`, instead of inline.

`ooda::HttpClient` ships with both fixes — plus a `ChoiceSpace` derive and a
`Trace` type generalizing patterns every one of those clients had also
rebuilt from scratch. One client, maintained once.

## What's confirmed

All three wire shapes, confirmed against the live `typesafe-ai/jev` route:

- `Choice` — `criteria` is a `{key -> description}` object.
- `Score` — `criteria` is a plain `[key, ...]` array; an object is rejected
  outright.
- `Noul` — wire tag `"boolean"`, answer field `"probability"`. (Not
  `"noul"` on either — that's purely this crate's own vocabulary.)

Pinned tests: `crates/ooda/src/question.rs`.

## Status

- **Compiled, tested, dogfooded.** `cargo test --workspace` — 42 tests
  green.
- **`legion-of-bom` migrated for real.** Its own `DecisionClient` is
  deleted; it calls `ooda` directly now, and its DRC checks pass end to end
  through the new path.
- **`surf` migrated too.** The bounded-choice decision path (its dual
  wire-format support was the architectural stress test) now goes through
  `ooda::HttpClient`; the one protocol with no `ooda` equivalent (a
  generic structured-output contract) stays Surf's own, on purpose.
- **`speedy` migrated too.** Its hand-rolled client — a fixed retry ladder
  that ignored `Retry-After` — is gone; `DecisionClient` is now a thin
  wrapper over `ooda::HttpClient`, reqwest dropped entirely. Reading
  `ooda`'s source surfaced two real bugs speedy had been carrying: it read
  a choice answer's `confidence` as the chosen action's own probability
  (it isn't — `ooda`'s `probabilities` field is), and it timed retry
  backoff into its reported latency, which is exactly the fairness gap
  `Ledger`/`Outcome::elapsed` exist to close.
- **`transmog` is integrating** its own decision step onto `ooda` next.

### Scoping a decision to an enum

```rust
use ooda::Choice;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Choice)]
enum EditChoice {
    /// Move the feature to clear the anchor.
    MoveIt,
    /// Open a relief pocket instead.
    OpenIt,
    /// Neither applies.
    #[serde(rename = "none")]
    Abstain,
}

let decision: ooda::Decision<EditChoice> =
    ooda::decide_choice(&client, observation, "edit_choice", "pick one")?;
```

- **Key**: the variant name, via `#[serde(rename_all)]`/`#[serde(rename)]`
  if present, else the bare identifier.
- **Description**: the variant's doc comment — required. An undescribed
  option is a compile error, not a silent gap.

No separate `ooda`-specific override attribute — `#[serde(rename)]` already
covers it. (An early attempt at one hit a reproducible parse error under
rustc 1.98.1; not worth chasing with a real consumer blocked on this crate
compiling.)

### Testing without a live endpoint

`ScriptedClient` answers from a queue of canned response bodies, decoded
through the exact same path `HttpClient` uses. A test against it is a test
about the wire format, not a hand-rolled shortcut around it.

## What's in this crate

- `Question` / `Answer` — the bounded wire types.
- `Client` / `Request` / `Outcome` — one `decide()` seam for any number of
  batched named questions.
- `HttpClient` — blocking HTTPS, both retry fixes above.
- `ScriptedClient` — canned-response mock for tests.
- `ChoiceSpace` / `#[derive(Choice)]` — scope a `Choice` question to a plain
  Rust enum.
- `Trace` / `Record` — a run's decision history, foldable into a confidence
  gate that fails on an *empty* trace instead of passing it vacuously.
- `RunningBest` — a patience-based running-best accumulator for a bounded
  search loop.
- `decide_staged` — a dependent decision chain: stage 2's `Request` built
  from stage 1's `Outcome`, every stage's full answer set folded into one
  `Trace`. The counterpart to `Request::with`'s *independent* batching —
  use that when every question can be asked in the same call, this when a
  later question's real content depends on an earlier answer. Capped at
  `MAX_STAGES` (16) so a continuation that never stops fails loud instead
  of turning into an unbounded sequence of billed calls.
- `Ledger` — a running total of usage and latency across a run's calls.
  `Outcome::elapsed` times only the *accepted* attempt, excluding retry
  backoff by design: a 429/5xx retry is the endpoint's current load, not
  the model's decision speed, and a benchmark's time-per-result shouldn't
  conflate the two. `Outcome::retries` keeps that information around
  separately rather than dropping it. No dollar conversion here — pricing
  is a fact about whichever provider actually answered (Jev direct, Laya
  self-hosted and free at the margin, a gateway with its own rate card),
  not something this crate should hardcode and let go stale.

## Out of scope for v1

- **Speculative branch pre-fetch** for a dependent question chain — ask
  every possible follow-up variant alongside the first question, keep only
  the one that matches. `decide_staged` covers the plain sequential case
  (one real call per stage); this trades wasted compute for fewer round
  trips and only pays off at shallow depth / small branching factor.
  Designed (`Request::with`'s docs), not built — nothing in this ecosystem
  has a decision tree deep enough yet to need it.
- **A shared bench-harness crate** for CADBench/PCBBench/DFMBench's
  near-identical rubric shape — different concern from making one
  decision, so not bundled here, but it exists now:
  [`eval`](https://github.com/FuturePresentLabs/eval).
- **A generic episode-loop runner.** Real duplication exists elsewhere in
  this ecosystem, but it's internal to one project.
- **Async.** Every consumer makes a decision call as a synchronous gate
  inside a deterministic pipeline step.

## License

MIT OR Apache-2.0 — deliberately permissive. Every consumer in this
ecosystem can depend on `ooda` regardless of its own license: FOSS
(AGPL-3.0-or-later or Apache-2.0) or proprietary alike.
