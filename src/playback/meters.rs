use std::sync::{Arc, Mutex, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rodio::Source;
use super::audio::StereoPeak;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MeterSnapshot {
    pub levels: [f32; 2],
    pub held: [f32; 2],
    pub clipped: [bool; 2] }

#[derive(Default)]
pub(super) struct PlaybackMeters {
    active: Option<Weak<AtomicBool>>,
    level: [f32; 2],
    held: [f32; 2],
    updated: Option<Instant>,
    hold_until: [Option<Instant>; 2],
    clip_until: [Option<Instant>; 2] }

impl PlaybackMeters {
    pub fn reset(&mut self, active: Option<&Arc<AtomicBool>>) {
        *self = Self { active: active.map(Arc::downgrade), ..Self::default() };
    }

    pub fn clear(&mut self) {
        let active = self.active.clone();
        *self = Self { active, ..Self::default() };
    }

    fn record(&mut self, active: &Arc<AtomicBool>, peaks: StereoPeak, now: Instant) {
        if active.load(Ordering::Acquire)
            || !self.active.as_ref().is_some_and(|current| current.ptr_eq(&Arc::downgrade(active))) {
            return;
        }
        let previous = self.snapshot(now);
        for channel in 0..2 {
            let amplitude = peaks.amplitude[channel].min(1.0);
            self.level[channel] = previous.levels[channel].max(amplitude);
            if amplitude >= previous.held[channel] {
                self.held[channel] = amplitude;
                self.hold_until[channel] = Some(now + Duration::from_secs(1));
            } else {
                self.held[channel] = previous.held[channel];
                // Preserve the original hold deadline; a quiet chunk must not
                // keep extending a loud peak indefinitely.
            }
            if peaks.clipped[channel] {
                self.clip_until[channel] = Some(now + Duration::from_secs(2));
            }
        }
        self.updated = Some(now);
    }

    pub fn snapshot(&self, now: Instant) -> MeterSnapshot {
        let Some(updated) = self.updated else { return MeterSnapshot::default(); };
        let release = |seconds: f32| 10f32.powf(-24.0 * seconds / 20.0);
        let mut snapshot = MeterSnapshot::default();
        for channel in 0..2 {
            snapshot.levels[channel] = self.level[channel] * release(now.saturating_duration_since(updated).as_secs_f32());
            let from = self.hold_until[channel].unwrap_or(updated).max(updated);
            snapshot.held[channel] = self.held[channel] * release(now.saturating_duration_since(from).as_secs_f32());
            snapshot.clipped[channel] = self.clip_until[channel].is_some_and(|until| now < until);
        }
        snapshot
    }
}

/// Meter a buffer when the output stream begins consuming it, not when it is
/// decoded ahead of time. Superseded transport cannot publish into new meters.
pub(super) struct MeteredChunk {
    inner: rodio::buffer::SamplesBuffer<f32>,
    peaks: Option<StereoPeak>,
    meters: Arc<Mutex<PlaybackMeters>>,
    active: Arc<AtomicBool> }

impl MeteredChunk {
    pub fn new(samples: Vec<f32>, peaks: StereoPeak, meters: Arc<Mutex<PlaybackMeters>>, active: Arc<AtomicBool>) -> Self {
        Self { inner: rodio::buffer::SamplesBuffer::new(2, 48_000, samples), peaks: Some(peaks), meters, active }
    }
}

impl Iterator for MeteredChunk {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        let sample = self.inner.next()?;
        if let Some(peaks) = self.peaks.take() {
            self.meters.lock().unwrap_or_else(|error| error.into_inner()).record(&self.active, peaks, Instant::now());
        }
        Some(sample)
    }
    fn size_hint(&self) -> (usize, Option<usize>) { self.inner.size_hint() }
}

impl Source for MeteredChunk {
    fn current_frame_len(&self) -> Option<usize> { self.inner.current_frame_len() }
    fn channels(&self) -> u16 { self.inner.channels() }
    fn sample_rate(&self) -> u32 { self.inner.sample_rate() }
    fn total_duration(&self) -> Option<Duration> { self.inner.total_duration() }
}

