//! A running total of usage and latency across a run's decision calls.
//!
//! Deliberately separate from [`crate::Trace`]'s own serialization: `Trace`
//! stays a bare array of [`crate::Record`]s on the wire — the shape every
//! existing consumer's trace file format already depends on (legion-of-bom
//! writes it, cadbench and pcbbench parse it). Bolting a running total onto
//! `Trace` itself would silently break that. `Ledger` is opt-in instead: a
//! caller who wants cost/time tracking creates one alongside their `Trace`
//! and feeds it every [`crate::Outcome`] as it comes back.
//!
//! Dollar conversion is deliberately not this crate's job either — pricing
//! is a fact about whichever provider is actually answering (Jev direct,
//! Laya self-hosted and therefore free at the margin, a gateway with its
//! own rate card), not something `ooda` should hardcode and let go stale.
//! `Ledger` reports the raw facts (tokens, calls, wall-clock time); a
//! caller multiplies by whatever rate applies to them.

use std::time::Duration;

use crate::client::Outcome;
use crate::question::Usage;

/// Accumulated usage and latency across every [`Outcome`] fed to it.
///
/// `elapsed`/`mean_elapsed` are the *fair* latency numbers by construction:
/// they're built from [`Outcome::elapsed`], which already excludes retry
/// backoff (see its docs). `retries` is kept alongside as its own total —
/// real, worth knowing, just not something that inflates a benchmark's
/// reported time-per-result.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Ledger {
    /// How many `decide()` calls contributed to this total.
    pub calls: u32,
    /// How many of those calls reported a real [`Outcome::elapsed`] — kept
    /// separate from `calls` so [`Ledger::mean_elapsed`] never silently
    /// reports "0s" for "no call actually timed" (a scripted/mock run,
    /// say). "Unknown" and "instant" are different claims.
    timed_calls: u32,
    /// Summed token usage, across every call that reported any.
    pub usage: Usage,
    /// Summed wall-clock time of every *accepted* attempt, across every
    /// call that reported one ([`Outcome::elapsed`] — `None` for a
    /// scripted/mock call, which contributes nothing rather than a
    /// fabricated zero). Excludes retry backoff by construction.
    pub elapsed: Duration,
    /// Summed retries across every call that reported a count
    /// (`0` contributed per call that worked first try). Not folded into
    /// `elapsed` — see the module and [`Outcome::retries`] docs for why.
    pub retries: u32,
}

impl Ledger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Folds one [`Outcome`] into the running total.
    pub fn record(&mut self, outcome: &Outcome) {
        self.calls += 1;
        if let Some(usage) = outcome.usage {
            self.usage += usage;
        }
        if let Some(elapsed) = outcome.elapsed {
            self.timed_calls += 1;
            self.elapsed += elapsed;
        }
        if let Some(retries) = outcome.retries {
            self.retries += retries;
        }
    }

    /// Mean wall-clock time per timed call, excluding retry backoff.
    /// `None` if no call has reported a real [`Outcome::elapsed`] yet —
    /// never a fabricated zero for "no data" the way dividing by `calls`
    /// unconditionally would give.
    #[must_use]
    pub fn mean_elapsed(&self) -> Option<Duration> {
        if self.timed_calls == 0 {
            None
        } else {
            Some(self.elapsed / self.timed_calls)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scripted::ScriptedClient;
    use crate::{Client, Question, Request};

    #[test]
    fn records_usage_and_call_count_but_not_a_fabricated_elapsed() {
        let client = ScriptedClient::new([
            r#"{"answers":{"q":{"type":"choice","choice":"a","confidence":0.9}},
                "usage":{"input_tokens":100,"output_tokens":0}}"#
                .to_owned(),
            r#"{"answers":{"q":{"type":"choice","choice":"a","confidence":0.9}},
                "usage":{"input_tokens":50,"output_tokens":0}}"#
                .to_owned(),
        ]);
        let request = Request::single(
            serde_json::json!({}),
            "q",
            Question::choice("pick", [("a", "desc")]),
        );

        let mut ledger = Ledger::new();
        ledger.record(&client.decide(&request).unwrap());
        ledger.record(&client.decide(&request).unwrap());

        assert_eq!(ledger.calls, 2);
        assert_eq!(ledger.usage.input_tokens, 150);
        // ScriptedClient never sets elapsed -- no real time passed, and this
        // must read as "unknown," not a fabricated "instant."
        assert_eq!(ledger.elapsed, Duration::ZERO);
        assert_eq!(ledger.mean_elapsed(), None);
    }

    #[test]
    fn mean_elapsed_is_none_for_an_empty_ledger() {
        assert_eq!(Ledger::new().mean_elapsed(), None);
    }

    #[test]
    fn retries_are_totaled_but_never_folded_into_elapsed() {
        let quick_no_retry = Outcome::new(std::collections::BTreeMap::new(), None, None)
            .with_elapsed(Duration::from_millis(100))
            .with_retries(0);
        let slow_after_retries = Outcome::new(std::collections::BTreeMap::new(), None, None)
            .with_elapsed(Duration::from_millis(100))
            .with_retries(3);

        let mut ledger = Ledger::new();
        ledger.record(&quick_no_retry);
        ledger.record(&slow_after_retries);

        assert_eq!(ledger.retries, 3);
        // Both accepted attempts took 100ms -- the 3 retries before the
        // second one don't show up here, by design.
        assert_eq!(ledger.mean_elapsed(), Some(Duration::from_millis(100)));
    }

    #[test]
    fn mean_elapsed_only_averages_over_calls_that_actually_timed() {
        let timed = Outcome::new(std::collections::BTreeMap::new(), None, None)
            .with_elapsed(Duration::from_millis(200));
        let untimed = Outcome::new(std::collections::BTreeMap::new(), None, None);

        let mut ledger = Ledger::new();
        ledger.record(&timed);
        ledger.record(&untimed);
        ledger.record(&timed);

        assert_eq!(ledger.calls, 3);
        assert_eq!(ledger.mean_elapsed(), Some(Duration::from_millis(200)));
    }
}
