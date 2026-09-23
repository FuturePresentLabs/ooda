# ooda

[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![tests](https://img.shields.io/badge/tests-55%20passing-brightgreen.svg)](#status)
[![built on](https://img.shields.io/badge/built%20on-typesafe.ai%20Jev-6b46c1.svg)](https://typesafe.ai)
[![status](https://img.shields.io/badge/status-in%20production-success.svg)](#status)

*(Badge numbers are generated — run `scripts/update-badges.sh` after a test count changes; don't hand-edit them.)*

**Observe. Orient. Decide. Act.** This crate is the Rust client for
[Bifrost](https://ai.fpl.dev), Future Present Labs' inference gateway — one
credential, one retry story, behind two clearly separate capabilities:

- **`decide()`** — bounded `Choice`/`Score`/`Noul` questions in, typed,
  confidence-scored answers out. No free text, either direction, ever.
  Strongly typed over [typesafe.ai](https://typesafe.ai)'s Jev, and
  [Laya](https://github.com/receptron/laya) (its MIT-licensed FOSS twin).
- **`complete()`** — genuinely open free text, for the real cases that
  actually need it (typing a value into a web form, drafting a planning
  note) — never used to widen what `decide()` itself accepts, and never
  used to let a model choose what to ask next inside a hand-written
  decision chain (see `complete()`'s entry below for why that specific
  line matters).

Act is always the caller's own concern — this crate stops at the decision
(or the completion).

Already running in production across EDA/PCB design, CAD/CAM, and browser
agent workflows. Copy our homework.

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

- **Compiled, tested, dogfooded.** `cargo test --workspace` — 55 tests
  green (66 with `--features capture`).
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
- **`complete()` lifted from `surf`'s own `OpenAiText`**, the same move
  `capture` already was for `speedy`'s `RlcdEventSink`: generalize a
  sibling's already-proven code instead of inventing fresh. Closes a real
  gap in the original — `surf`'s hand-rolled version had zero retry logic;
  `complete()` shares `decide()`'s own 429/5xx/transport retry path, since
  both are Bifrost routes on the same gateway. It also streams now: a
  document-length answer at a hundred-odd tokens/s took over 125s sent
  whole, long enough for a gateway in front of Bifrost to cut it with HTTP
  524. `complete()` asks for a stream and collects it as it arrives instead.
- **`jarvis` is the first consumer pulling the other direction.** A voice
  turn budgets ~2s for a decision and would rather fail fast than wait out
  even one retry — `with_timeout`/`with_max_attempts` exist because the
  alternative was `jarvis` keeping its own hand-rolled client, exactly the
  duplication this crate exists to end.
- **Captured decisions are rejoinable to their consequences.** A record
  originally held just a request and its answer — enough for behavior
  cloning, nothing else. `Request::correlated(id)`, `run_id`, `labels`, and
  a now-public `Outcome::answered(..)` (so a deterministic policy's
  demonstrations can be captured through the same path a model's decisions
  are) close the gap: a caller can now rejoin a captured decision to
  whatever it separately records about acting on it, which is what makes a
  corpus evaluable and rewardable, not just clonable.

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
`ScriptedComplete` is the same idea for `complete()`.

### Free text with `complete()`

```rust
use ooda::{Complete, HttpClient, Prompt};

let client = HttpClient::from_env()?;
let value = client.complete(
    &Prompt::new(
        "Return only the short text to enter into the selected browser field.",
        "goal: ship the order; field: shipping_city; current value: (empty)",
    )
    .with_max_tokens(128)
    .with_temperature(0.0),
)?;
```

Same client type as `decide()` — `HttpClient` implements both `Client` and
`Complete`. A caller wanting a different model for completions than for
decisions (a smaller, faster one is usually the right call) constructs a
second `HttpClient` via `.with_model(..)`; `Prompt` itself carries no model
override, so there's exactly one place a model is ever configured.

Always streamed under the hood (`choices[0].delta.content`, collected as it
arrives) — a long answer sent whole risks a front-Bifrost gateway cutting an
idle connection with HTTP 524 before the first byte comes back. For a
reasoning model, `Prompt::with_reasoning_effort("low"|"medium"|"high")` sets
the gateway's `reasoning.effort`; left unset, the model's own default
applies. Reasoning spends `max_tokens` too — a model left to think freely
can burn its whole budget reasoning and answer with nothing, which
`complete()` reports as `Error::ReasoningOnly` (lower the effort or raise
the budget) rather than silently returning an empty string.

### Tuning timeout and retries

```rust
use std::time::Duration;
use ooda::HttpClient;

// A voice turn: fail fast, no retry ladder -- a retry is indistinguishable
// from a hang to someone waiting on an answer.
let client = HttpClient::from_env()?
    .with_timeout(Duration::from_secs(2))?
    .with_max_attempts(1);

// A long complete() answer: the default 30s suits a bounded decide(), not
// a model writing a whole document at a hundred-odd tokens/s.
let patient = HttpClient::from_env()?.with_timeout(Duration::from_secs(180))?;
```

`DEFAULT_TIMEOUT` (30s) and `MAX_ATTEMPTS` (5) are sized for a `decide()`
call inside a batch pipeline, where a slow answer still beats no answer —
both are public so a caller can read what it's overriding instead of
guessing. `with_max_attempts(1)` disables retrying outright; a budget below
`1` is raised to `1` rather than honored, since a client that never actually
tried would otherwise report a failure it didn't earn.

## What's in this crate

- `Question` / `Answer` — the bounded wire types.
- `Client` / `Request` / `Outcome` — one `decide()` seam for any number of
  batched named questions.
- `HttpClient` — blocking HTTPS, both retry fixes above.
  `with_timeout`/`with_max_attempts` override `DEFAULT_TIMEOUT`/
  `MAX_ATTEMPTS` per client, in either direction — a latency-bound voice
  turn wanting less of both, a long `complete()` answer wanting more time
  (see [Tuning timeout and retries](#tuning-timeout-and-retries)).
- `ScriptedClient` — canned-response mock for tests.
- `Complete` / `Prompt` / `ScriptedComplete` — `ooda`'s other capability:
  genuinely open free text (`HttpClient` implements this too, against
  Bifrost's OpenAI-compatible `/v1/chat/completions` route), for the real
  cases that need it — typing a value into a web form, drafting a planning
  note — kept as a clearly separate trait from `Client`, never a second way
  to answer a `Question`. The line that matters: `complete()` must never
  become how a caller decides *what bounded question to ask next* inside a
  `decide_staged`/`decide_speculative` chain — that's an LLM improvising
  control flow at runtime, exactly what this ecosystem's own
  deterministic-orchestration principle exists to prevent. Use `complete()`
  to draft a candidate question for a human (or a validation layer) to
  review, never to silently reshape a pipeline's next step. Always
  streamed internally, so a long answer is never idle long enough for a
  gateway to cut it; `Prompt::with_reasoning_effort` tunes a reasoning
  model's `reasoning.effort`, and a stream that reasons but never answers
  fails loud as `Error::ReasoningOnly` rather than returning an empty
  string.
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
- `decide_speculative` — speculative branch pre-fetch: a root `Choice` plus
  one follow-up question per possible root answer, all asked in the *same*
  call. Once the root resolves, the matching branch's answer is already in
  hand — one round trip instead of two. Not a new wire capability, just
  `Request::with`'s existing batching used one level deeper; the real work
  is resolving to the one branch that matches and keeping every discarded
  branch's answer — real, but never acted on — out of `Trace`, so a
  low-confidence answer nobody used can't fail `Trace::all_at_least`'s
  gate. Worth it at shallow depth / small branching factor, same as
  `decide_staged`'s docs already say — this is that trade, built.
- `Ledger` — a running total of usage and latency across a run's calls.
  `Outcome::elapsed` times only the *accepted* attempt, excluding retry
  backoff by design: a 429/5xx retry is the endpoint's current load, not
  the model's decision speed, and a benchmark's time-per-result shouldn't
  conflate the two. `Outcome::retries` keeps that information around
  separately rather than dropping it. No dollar conversion here — pricing
  is a fact about whichever provider actually answered (Jev direct, Laya
  self-hosted and free at the margin, a gateway with its own rate card),
  not something this crate should hardcode and let go stale.
- `CapturingClient` (the `capture` feature, off by default) — wraps any
  `Client`, durably appending every `decide()` call to an append-only,
  rollover-capped JSONL log at `.ooda/<binary-name>/decisions.jsonl` (the
  binary name is inferred from `std::env::current_exe`, never
  caller-supplied). Adapted from `speedy`'s own already-production
  `rlcd.decision.v1` event log — built there independently because this
  crate had no equivalent yet, exactly the kind of duplication `ooda`
  exists to end. For fine-tuning on real decision traffic later: request
  and outcome (or error) in one record, not `speedy`'s split
  requested/completed/failed phases, since a training example needs a
  request paired with its real answer. A capture-write failure is a hard
  error when the underlying decision succeeded — a caller must never be
  left thinking a decision was durably logged when it wasn't. Every record
  carries a `run_id` (so restarting a supervised agent doesn't collide
  sequences from a previous life in a log it shares) and optional
  `labels` (untyped, caller-supplied context this crate has no business
  knowing the shape of — which game, which board revision, which tenant).
  `Request::correlated(id)` tags a request with the caller's own id for
  the call, written to the capture log and **never transmitted**, so a
  captured decision can be rejoined to whatever the caller separately
  records about the consequence of acting on it — the seam a corpus needs
  to be evaluable and rewardable, not just clonable.
  `Outcome::answered(..)` is public for the same reason from the other
  side: a deterministic policy (a scripted baseline, a rules engine, a
  human) answering the same typed question a model would can now be
  captured through the identical path, instead of a second format that
  has to be reconciled by hand.

### Capturing decisions for fine-tuning

```rust
use ooda::{Capture, CapturingClient, HttpClient, Request};

let client = CapturingClient::new(
    HttpClient::from_env()?,
    Capture::for_current_binary()?.labelled([("game", "smk")]),
);
let request = Request::single(observation, "action", question).correlated("turn-482");
// use `client` exactly like `HttpClient` -- every decide() call is now
// also appended to .ooda/<this binary's name>/decisions.jsonl
```

Needs the `capture` feature (`ooda = { ..., features = ["capture"] }`) —
off by default, so a consumer who doesn't want a local write on every
decision pays nothing for it.

## Out of scope for v1

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
