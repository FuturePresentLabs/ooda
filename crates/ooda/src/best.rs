//! A running-best-with-patience accumulator for a bounded search loop.
//!
//! Generalizes the shape of legion-of-bom's layout hill-climb: try a
//! candidate, keep it only if it scores better than the best seen so far,
//! and stop once too many consecutive attempts failed to improve. No
//! decision-call dependency — this is a plain optimizer-loop utility, usable
//! whether or not any step in the loop calls a decision endpoint at all.

/// Keeps the best-scored candidate seen so far, with patience-based early
/// stop. Higher score wins.
#[derive(Debug)]
pub struct RunningBest<T> {
    best: Option<(T, f64)>,
    patience: u32,
    stale: u32,
}

impl<T> RunningBest<T> {
    /// Starts empty. `patience` is how many consecutive non-improving
    /// attempts [`RunningBest::should_stop`] tolerates before returning
    /// `true`.
    #[must_use]
    pub fn new(patience: u32) -> Self {
        RunningBest {
            best: None,
            patience,
            stale: 0,
        }
    }

    /// Considers one attempt. Keeps it as the new best if it strictly beats
    /// the current best (or if there is no current best yet). Returns
    /// whether it improved.
    pub fn consider(&mut self, candidate: T, score: f64) -> bool {
        let improved = match &self.best {
            Some((_, best_score)) => score > *best_score,
            None => true,
        };
        if improved {
            self.best = Some((candidate, score));
            self.stale = 0;
        } else {
            self.stale += 1;
        }
        improved
    }

    /// Whether `patience` consecutive attempts have failed to improve — the
    /// loop should stop trying.
    #[must_use]
    pub fn should_stop(&self) -> bool {
        self.stale >= self.patience
    }

    /// The best score seen so far, if any attempt was considered.
    #[must_use]
    pub fn best_score(&self) -> Option<f64> {
        self.best.as_ref().map(|(_, s)| *s)
    }

    /// A reference to the best candidate so far, if any.
    #[must_use]
    pub fn best(&self) -> Option<&T> {
        self.best.as_ref().map(|(t, _)| t)
    }

    /// Consumes the accumulator, returning the best candidate, if any.
    #[must_use]
    pub fn into_best(self) -> Option<T> {
        self.best.map(|(t, _)| t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_higher_scoring_candidate() {
        let mut best = RunningBest::new(2);
        assert!(best.consider("a", 1.0));
        assert!(!best.consider("b", 0.5));
        assert!(best.consider("c", 2.0));
        assert_eq!(best.best(), Some(&"c"));
        assert_eq!(best.best_score(), Some(2.0));
    }

    #[test]
    fn stops_after_patience_consecutive_non_improvements() {
        let mut best = RunningBest::new(2);
        best.consider("a", 1.0);
        assert!(!best.should_stop());
        best.consider("b", 0.5);
        assert!(!best.should_stop());
        best.consider("c", 0.5);
        assert!(best.should_stop());
    }

    #[test]
    fn an_improvement_resets_patience() {
        let mut best = RunningBest::new(2);
        best.consider("a", 1.0);
        best.consider("b", 0.5);
        best.consider("c", 2.0);
        assert!(!best.should_stop());
    }
}
