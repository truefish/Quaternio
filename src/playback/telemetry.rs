//! Bounded UI handoff measurements. These are not monitor presentation or audio
//! hardware timestamps. Disk writes run on one worker outside the UI thread.
use std::collections::VecDeque;
use std::sync::mpsc::{self, SyncSender};
use std::time::{Duration, Instant};

use serde::Serialize;
use super::PreviewImage;

#[derive(Clone, Serialize)]
struct Sample {
    viewer: &'static str,
    frame: i64,
    token: u64,
    request_to_ready_ms: f64,
    decode_ms: f64,
    ready_to_ui_ms: f64,
    image_upload_ms: f64,
    request_to_ui_ms: f64,
    optimized_picture: bool }

pub struct PreviewTelemetry {
    samples: VecDeque<Sample>,
    last_write: Instant,
    writer: Option<SyncSender<Vec<Sample>>> }

impl PreviewTelemetry {
    pub fn new() -> Self {
        // Portable/live launches choose their own data directory. Do not write
        // an implicit fallback location for library tests or headless benchmarks.
        let writer = std::env::var_os("QUATERNIO_DATA_DIR").map(|directory| {
            let path = std::path::PathBuf::from(directory).join("playback-metrics.json");
            let (tx, rx) = mpsc::sync_channel::<Vec<Sample>>(1);
            std::thread::spawn(move || {
                while let Ok(samples) = rx.recv() {
                    let mut reports = Vec::new();
                    for viewer in ["Program", "Source"] {
                        let mut latency: Vec<_> = samples.iter().filter(|s| s.viewer == viewer)
                            .map(|s| s.request_to_ui_ms).collect();
                        latency.sort_by(f64::total_cmp);
                        if !latency.is_empty() {
                            reports.push(serde_json::json!({ "viewer": viewer, "count": latency.len(),
                                "median_ms": latency[(latency.len()-1)/2],
                                "p95_ms": latency[(latency.len()*95).div_ceil(100)-1] }));
                        }
                    }
                    let report = serde_json::json!({ "schema_version": 1,
                        "scope": "Recent accepted preview frames through Slint image upload; excludes OS display presentation and audio hardware. Mixed interaction modes, not a controlled benchmark.",
                        "viewers": reports, "samples": samples });
                    if let Ok(bytes) = serde_json::to_vec_pretty(&report) {
                        let tmp = path.with_extension("json.tmp");
                        if std::fs::write(&tmp, bytes).is_ok() { let _ = std::fs::rename(&tmp, &path); }
                    }
                }
            });
            tx
        });
        Self { samples: VecDeque::new(), last_write: Instant::now() - Duration::from_secs(3), writer }
    }

    pub fn record(&mut self, viewer: &'static str, image: &PreviewImage, before_upload: Instant) {
        let now = Instant::now();
        self.samples.push_back(Sample { viewer, frame: image.frame, token: image.token,
            optimized_picture: image.proxy_used,
            request_to_ready_ms: image.ready_at.duration_since(image.requested_at).as_secs_f64()*1000.0,
            decode_ms: image.decode_ms,
            ready_to_ui_ms: before_upload.saturating_duration_since(image.ready_at).as_secs_f64()*1000.0,
            image_upload_ms: now.duration_since(before_upload).as_secs_f64()*1000.0,
            request_to_ui_ms: now.duration_since(image.requested_at).as_secs_f64()*1000.0 });
        while self.samples.len() > 300 { self.samples.pop_front(); }
        if self.last_write.elapsed() >= Duration::from_secs(2) {
            if let Some(writer) = &self.writer {
                if writer.try_send(self.samples.iter().cloned().collect()).is_ok() {
                    self.last_write = now;
                }
            }
        }
    }
}
