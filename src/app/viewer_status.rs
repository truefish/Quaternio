//! Presentation feedback follows requests and replies, never picture brightness.
use std::time::{Duration, Instant};

const PREPARING_DELAY: Duration = Duration::from_millis(150);

#[derive(Default)]
pub(super) struct ViewerStatus {
    pending_since: Option<Instant>,
    pub(super) problem: String }

impl ViewerStatus {
    pub(super) fn request(&mut self, now: Instant) {
        // Replaced scrubs share a wait period until a useful picture arrives.
        self.pending_since.get_or_insert(now);
        self.problem.clear();
    }

    pub(super) fn picture(&mut self, latest: bool, now: Instant) {
        self.pending_since = (!latest).then_some(now);
        self.problem.clear();
    }

    pub(super) fn fail(&mut self, problem: String) {
        self.pending_since = None;
        self.problem = problem;
    }

    pub(super) fn preparing(&self, now: Instant) -> bool {
        self.pending_since.is_some_and(|since| now.saturating_duration_since(since) >= PREPARING_DELAY)
    }

    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }
}

