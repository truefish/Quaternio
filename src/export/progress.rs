use std::time::Duration;

/// Estimates the remaining encoding phase from advancing encoded-frame counts.
/// Preparation, encoder retries and final publication have their own lifecycle
/// and deliberately do not contribute to this estimate.
pub(super) struct RemainingEstimator {
    total: i64,
    first_at: Option<f64>,
    previous: Option<(i64, f64)>,
    seconds_per_frame: Option<f64>,
    advances: u32 }

impl RemainingEstimator {
    pub(super) fn new(total: i64) -> Self {
        Self {
            total,
            first_at: None,
            previous: None,
            seconds_per_frame: None,
            advances: 0 }
    }

    pub(super) fn observe(&mut self, frame: i64, elapsed: Duration) -> Option<u64> {
        let now = elapsed.as_secs_f64();
        if self.total <= 0 || frame <= 0 || frame >= self.total {
            *self = Self::new(self.total);
            return None;
        }
        let Some((previous_frame, previous_at)) = self.previous else {
            self.previous = Some((frame, now));
            self.first_at = Some(now);
            return None;
        };
        // Duplicate progress is not a fresh estimate. The UI also expires an
        // estimate if the encoder stops emitting progress entirely.
        if frame == previous_frame {
            return None;
        }
        if frame < previous_frame || now <= previous_at {
            *self = Self::new(self.total);
            self.previous = Some((frame, now));
            self.first_at = Some(now);
            return None;
        }
        let interval = (now - previous_at) / (frame - previous_frame) as f64;
        let smoothed = self
            .seconds_per_frame
            .map_or(interval, |previous| previous * 0.75 + interval * 0.25);
        self.seconds_per_frame = Some(smoothed);
        self.previous = Some((frame, now));
        self.advances = self.advances.saturating_add(1);
        if self.advances < 3 || now - self.first_at.unwrap_or(now) < 1.5 {
            return None;
        }
        let remaining = (smoothed * (self.total - frame) as f64).ceil();
        if !remaining.is_finite() || remaining > u64::MAX as f64 {
            return None;
        }
        Some((remaining as u64).max(1))
    }
}

/// Empty means there is not enough current evidence for a useful estimate.
pub fn format_remaining_time(seconds: Option<u64>) -> String {
    match seconds {
        None => String::new(),
        Some(seconds) if seconds >= 3600 => {
            format!("About {}h {:02}m left", seconds / 3600, seconds % 3600 / 60)
        }
        Some(seconds) if seconds >= 60 => {
            format!("About {}m {:02}s left", seconds / 60, seconds % 60)
        }
        Some(seconds) => format!("About {}s left", seconds.max(1)) }
}

