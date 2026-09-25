//! **ooda** — a Rust client and type system for typed, calibrated decision
//! models: Jev/Laya-compatible "System One" APIs.
//!
//! # Why OODA
//!
//! Every consumer of a decision endpoint in this ecosystem follows the same
//! shape: **O**bserve some state, **O**rient it against a bounded question
//! (a closed set of options, a rubric, a yes/no), **D**ecide by putting that
//! question to a fast, calibrated model instead of an autoregressive LLM,
//! and **A**ct on the typed, confidence-scored answer that comes back. This
//! crate is the Observe/Orient/Decide plumbing — Act is always the caller's
//! own concern, since what "acting" means is different in a browser
//! controller, a PCB layout loop, and a CAD edit agent.
//!
//! # What this is not
//!
//! There is no free text anywhere on the wire, in either direction. A
//! request carries a JSON observation and a set of named, bounded
//! [`Question`]s; a response carries a typed [`Answer`] plus a confidence
//! for each. A caller's control flow is a `match`, never a parse-and-hope —
//! see [`Question`] and [`choice::ChoiceSpace`] for how a whole answer
//! alphabet is expressed as a closed Rust type instead of a stringly-typed
//! map.
//!
//! # Quick start
//!
//! ```
//! use ooda::{Client, Criteria, Question, Request};
//!
//! # fn run(client: &impl Client) -> Result<(), ooda::Error> {
//! let request = Request::single(
//!     serde_json::json!({ "deviation_mm": 0.18 }),
//!     "edit_choice",
//!     Question::choice(
//!         "pick one",
//!         Criteria::from([("move_it", "move it"), ("open_it", "open it")]),
//!     ),
//! );
//! let outcome = client.decide(&request)?;
//! let answer = outcome.answer("edit_choice")?;
//! match answer.choice() {
//!     Some("move_it") => { /* ... */ }
//!     Some("open_it") => { /* ... */ }
//!     _ => unreachable!("bounded to the offered set"),
//! }
//! # Ok(())
//! # }
//! ```
//!
//! For a decision scoped to a plain Rust enum instead of string keys, see
//! [`choice::ChoiceSpace`] and [`choice::decide_choice`].

mod best;
#[cfg(feature = "capture")]
mod capture;
mod choice;
mod client;
mod complete;
mod cost;
mod error;
mod gpc1;
mod http;
mod numeric;
mod question;
mod scripted;
mod speculate;
mod stage;
mod trace;

pub use best::RunningBest;
#[cfg(feature = "capture")]
pub use capture::{Capture, CapturedDecision, CapturingClient, MAX_CAPTURE_LOG_BYTES};
pub use choice::{ChoiceSpace, Decision, decide_choice, decide_choice_traced};
pub use client::{Client, Observation, Outcome, Request};
pub use complete::{CHAT_PATH, Complete, Prompt, ScriptedComplete};
pub use cost::Ledger;
pub use error::Error;
pub use gpc1::Gpc1Client;
pub use http::{
    API_KEY_ENV, BASE_URL_ENV, DECIDE_PATH, DEFAULT_BASE_URL, DEFAULT_MODEL, DEFAULT_TIMEOUT,
    HttpClient, IntoObservation, MAX_ATTEMPTS, MODEL_ENV, RESOLVED_MODEL_HEADER,
};
pub use numeric::{
    BoundedError, BoundedPredictor, JointOutcome, JointRequest, NumericEstimate, NumericField,
    NumericOutcome, NumericRequest,
};
pub use question::{Answer, Criteria, Question, Usage};
pub use scripted::ScriptedClient;
pub use speculate::{Resolved, Speculation, decide_speculative};
pub use stage::{MAX_STAGES, decide_staged};
pub use trace::{Kind, Record, Trace};

#[cfg(feature = "derive")]
pub use choice::Choice;
