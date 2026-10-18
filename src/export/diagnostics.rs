//! Opt-in export measurements. Missing values mean unmeasured, not zero.
//!
//! Stage durations accumulate across attempts and may overlap (especially worker
//! measurements); they must not be summed to obtain wall time. Backend and route
//! fields describe the latest actual attempt, not the requested preferences.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Serialize, Deserialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportRoute {
    Direct,
    Composite,
    Timeline,
}

impl ExportRoute {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Composite => "composite",
            Self::Timeline => "timeline",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExportTimings {
    pub preflight_seconds: Option<f64>,
    pub encoder_probe_seconds: Option<f64>,
    pub route_probe_seconds: Option<f64>,
    pub audio_preparation_seconds: Option<f64>,
    /// Wall time inside composite(), including source decode and cache access.
    /// This is not isolated compositor time; retries accumulate.
    pub decode_and_composition_seconds: Option<f64>,
    pub pipe_write_seconds: Option<f64>,
    /// Export collection start to first observed positive encoded frame count.
    /// Includes preflight/preparation and progress reporting latency. Set once
    /// across all attempts; it is not isolated encoder initialization time.
    pub startup_seconds: Option<f64>,
    /// Isolated decode time only when directly measured.
    pub decode_seconds: Option<f64>,
    /// Elapsed encoding attempt time, not isolated encoder CPU/GPU time.
    pub encode_seconds: Option<f64>,
    pub finalization_seconds: Option<f64>,
    pub total_seconds: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExportDiagnostics {
    /// Actual encoder backend identifier, e.g. libx264 or h264_nvenc.
    pub backend: Option<String>,
    /// Device identifier only when known; never inferred from backend branding.
    pub device: Option<String>,
    pub route: Option<ExportRoute>,
    pub picture_route: Option<String>,
    pub audio_route: Option<String>,
    pub fallback_reasons: Vec<String>,
    pub timings: ExportTimings,
    /// Observed encoded frames for the final attempt, not the planned count.
    pub frames_encoded: Option<u64>,
    /// Observed encoded frames divided by the entire export wall time.
    pub throughput_fps: Option<f64>,
    /// Measured export process-tree peak resident bytes, if available.
    pub peak_memory_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub enum ExportStage {
    Preflight,
    EncoderProbe,
    RouteProbe,
    AudioPreparation,
    Composition,
    PipeWrite,
    Startup,
    Decode,
    Encode,
    Finalization,
}

impl ExportDiagnostics {
    pub fn add_timing(&mut self, stage: ExportStage, elapsed: Duration) {
        let slot = match stage {
            ExportStage::Preflight => &mut self.timings.preflight_seconds,
            ExportStage::EncoderProbe => &mut self.timings.encoder_probe_seconds,
            ExportStage::RouteProbe => &mut self.timings.route_probe_seconds,
            ExportStage::AudioPreparation => &mut self.timings.audio_preparation_seconds,
            ExportStage::Composition => &mut self.timings.decode_and_composition_seconds,
            ExportStage::PipeWrite => &mut self.timings.pipe_write_seconds,
            ExportStage::Startup => &mut self.timings.startup_seconds,
            ExportStage::Decode => &mut self.timings.decode_seconds,
            ExportStage::Encode => &mut self.timings.encode_seconds,
            ExportStage::Finalization => &mut self.timings.finalization_seconds,
        };
        *slot = Some(slot.unwrap_or(0.0) + elapsed.as_secs_f64());
    }
}

/// Explicitly pass this handle to workers; thread-local state is not inherited.
/// Join all such workers before `collect` returns so their data is included.
#[derive(Clone)]
pub struct WorkerCollector {
    diagnostics: Arc<Mutex<ExportDiagnostics>>,
    started: Instant,
}

impl WorkerCollector {
    /// Elapsed wall time since this export collection began, shared by clones.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Record an observed FFmpeg frame count for the current attempt. Counts
    /// may decrease when a retry begins; callers should reset the count at the
    /// attempt boundary. Zero does not establish startup. The first positive
    /// observation across attempts establishes startup exactly once.
    pub fn observe_frame(&self, frame: u64) {
        let seconds = self.elapsed().as_secs_f64();
        self.record(|diagnostics| {
            diagnostics.frames_encoded = Some(frame);
            if frame > 0 {
                diagnostics.timings.startup_seconds.get_or_insert(seconds);
            }
        });
    }

    /// Keep the callback short; do not recursively call collector functions.
    pub fn record(&self, update: impl FnOnce(&mut ExportDiagnostics)) {
        update(&mut self.diagnostics.lock().unwrap_or_else(|error| error.into_inner()));
    }

    pub fn add_timing(&self, stage: ExportStage, elapsed: Duration) {
        self.record(|diagnostics| diagnostics.add_timing(stage, elapsed));
    }
}

thread_local! {
    static ACTIVE: RefCell<Option<WorkerCollector>> = const { RefCell::new(None) };
}

pub fn worker_collector() -> Option<WorkerCollector> {
    ACTIVE.with(|active| active.borrow().clone())
}

/// Attach an existing export collection to a scoped preparation worker.
pub(super) fn with_collector<T>(collector: Option<WorkerCollector>, operation: impl FnOnce() -> T) -> T {
    struct Restore(Option<WorkerCollector>);
    impl Drop for Restore {
        fn drop(&mut self) {
            ACTIVE.with(|active| *active.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(ACTIVE.with(|active| active.replace(collector)));
    operation()
}

/// Does not invoke the callback when diagnostics are disabled.
pub fn record(update: impl FnOnce(&mut ExportDiagnostics)) {
    if let Some(collector) = worker_collector() {
        collector.record(update);
    }
}

/// Measure a synchronous operation, including its error return, when enabled.
pub fn measure<T>(stage: ExportStage, operation: impl FnOnce() -> T) -> T {
    let Some(collector) = worker_collector() else {
        return operation();
    };
    let started = Instant::now();
    let result = operation();
    collector.add_timing(stage, started.elapsed());
    result
}

/// Start a fresh collection; nested collections are isolated and restore their
/// predecessor even on unwind. The supplied output is replaced on scope exit.
pub fn collect<T>(output: &mut ExportDiagnostics, operation: impl FnOnce() -> T) -> T {
    struct Scope<'a> {
        output: &'a mut ExportDiagnostics,
        collector: WorkerCollector,
        previous: Option<WorkerCollector>,
    }
    impl Drop for Scope<'_> {
        fn drop(&mut self) {
            ACTIVE.with(|active| *active.borrow_mut() = self.previous.take());
            let mut diagnostics = self.collector.diagnostics.lock().unwrap_or_else(|error| error.into_inner());
            let seconds = self.collector.elapsed().as_secs_f64();
            diagnostics.timings.total_seconds = Some(seconds);
            diagnostics.throughput_fps = diagnostics.frames_encoded.and_then(|frames| {
                (seconds > 0.0).then_some(frames as f64 / seconds)
            });
            *self.output = diagnostics.clone();
        }
    }
    let collector = WorkerCollector {
        diagnostics: Arc::new(Mutex::new(ExportDiagnostics::default())),
        started: Instant::now(),
    };
    let previous = ACTIVE.with(|active| active.replace(Some(collector.clone())));
    let _scope = Scope { output, collector, previous };
    operation()
}
