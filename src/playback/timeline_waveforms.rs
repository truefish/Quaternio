//! Real source-audio peaks. One idle-only worker; no UI-thread decoding or disk writes.
//! Integration is intentionally separate from this module's implementation.
use std::collections::VecDeque;
use std::io::{Read, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use crate::project::Media;
use super::decode::{find_ffmpeg, find_ffprobe};

const QUEUE_LIMIT: usize = 16;
const CACHE_LIMIT: usize = 16;
// The worker retains <=1M cached pairs and <=500K in one active/result
// pyramid: below 2M total even conservatively counting shared Arc data twice.
// UI-held result Arcs are the integration owner's separate bounded budget.
const JOB_PEAK_LIMIT: usize = 500_000;
const CACHE_BYTES: usize = 1_000_000 * std::mem::size_of::<Peak>();

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Peak { pub min: f32, pub max: f32 }
impl Peak {
    fn merge(self, other: Self) -> Self {
        Self { min: self.min.min(other.min), max: self.max.max(other.max) }
    }
}

#[derive(Debug)]
pub struct PeakLevel {
    pub samples_per_bucket: u64,
    /// Interleaved channel peaks: bucket * channels + channel.
    pub peaks: Vec<Peak> }

#[derive(Debug)]
pub struct WaveformPeaks {
    pub sample_rate: u32,
    pub channels: usize,
    pub sample_frames: u64,
    pub levels: Vec<PeakLevel> }
impl WaveformPeaks {
    pub fn bytes(&self) -> usize {
        self.levels.iter().map(|l| l.peaks.len() * std::mem::size_of::<Peak>()).sum()
    }

    /// Source seconds, not video frame ordinals. Includes real leading silence.
    /// Each display column conservatively merges intersecting peak buckets.
    pub fn columns(&self, channel: usize, start: f64, end: f64, count: usize) -> Vec<Peak> {
        if channel >= self.channels || !start.is_finite() || !end.is_finite()
            || end <= start || count == 0 || self.levels.is_empty() { return Vec::new(); }
        let count = count.min(4096);
        let samples_per_column = (end - start) * self.sample_rate as f64 / count as f64;
        let level = self.levels.iter().rev()
            .find(|l| l.samples_per_bucket as f64 <= samples_per_column)
            .unwrap_or(&self.levels[0]);
        (0..count).map(|column| {
            let a = (start + (end-start) * column as f64/count as f64) * self.sample_rate as f64;
            let b = (start + (end-start) * (column+1) as f64/count as f64) * self.sample_rate as f64;
            if b <= 0.0 || a >= self.sample_frames as f64 { return Peak::default(); }
            let first = (a.max(0.0) / level.samples_per_bucket as f64).floor() as usize;
            let last = (b.min(self.sample_frames as f64) / level.samples_per_bucket as f64).ceil() as usize;
            let mut peak = Peak::default();
            for bucket in first..last.min(level.peaks.len()/self.channels) {
                peak = peak.merge(level.peaks[bucket*self.channels+channel]);
            }
            peak
        }).collect()
    }
}

#[derive(Clone)]
pub struct WaveformResult {
    pub media: Media,
    pub peaks: Result<Arc<WaveformPeaks>, String> }

#[derive(Clone, PartialEq, Eq)]
struct Stamp { path: PathBuf, bytes: u64, modified: SystemTime }
fn stamp(path: &Path) -> Result<Stamp, String> {
    let metadata = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_file() { return Err("Audio source is not a file".into()); }
    Ok(Stamp { path: path.to_path_buf(), bytes: metadata.len(), modified: metadata.modified().map_err(|e| e.to_string())? })
}
fn same_source(a: &Media, b: &Media) -> bool {
    a.id == b.id && a.path == b.path && a.has_audio == b.has_audio
        && a.duration_secs.to_bits() == b.duration_secs.to_bits()
}
struct Cached { stamp: Stamp, peaks: Arc<WaveformPeaks> }
struct State {
    queue: VecDeque<Media>, completed: VecDeque<WaveformResult>, cache: VecDeque<Cached>,
    active: Option<(Media, Arc<AtomicBool>)>, busy_until: Instant, stopped: bool }
struct Shared { state: Mutex<State>, ready: Condvar }
pub struct WaveformEngine {
    shared: Arc<Shared>, worker: Option<std::thread::JoinHandle<()>> }

impl WaveformEngine {
    pub fn new() -> Self {
        let shared = Arc::new(Shared { state: Mutex::new(State {
            queue: VecDeque::new(), completed: VecDeque::new(), cache: VecDeque::new(),
            active: None, busy_until: Instant::now(), stopped: false }), ready: Condvar::new() });
        let work = shared.clone();
        let worker = std::thread::spawn(move || {
            worker_loop(work)
        });
        Self { shared, worker: Some(worker)}
    }
    /// Call on visible-media/relink changes, not every paint. Metadata is checked
    /// by the worker even for a previously cached path, never by this method.
    pub fn request(&self, media: Media) {
        if !media.has_audio { return; }
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.stopped || state.queue.iter().any(|m| same_source(m, &media))
            || state.completed.iter().any(|r| same_source(&r.media, &media)) { return; }
        if let Some((active, cancel)) = &state.active {
            if same_source(active, &media) { return; }
            if active.id == media.id { cancel.store(true, Ordering::Release); }
        }
        state.queue.retain(|m| m.id != media.id);
        state.completed.retain(|r| r.media.id != media.id);
        if state.queue.len() == QUEUE_LIMIT { state.queue.pop_back(); }
        state.queue.push_front(media);
        self.shared.ready.notify_one();
    }
    /// Call true throughout playback, scrubbing, held gestures and export.
    pub fn set_busy(&self, busy: bool) {
        if !busy { return; }
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.busy_until = Instant::now() + Duration::from_secs(2);
        if let Some((_, cancel)) = &state.active { cancel.store(true, Ordering::Release); }
    }
    pub fn poll(&self) -> Option<WaveformResult> {
        self.shared.state.lock().unwrap_or_else(|e| e.into_inner()).completed.pop_front()
    }
}
impl Default for WaveformEngine { fn default() -> Self { Self::new() } }
impl Drop for WaveformEngine {
    fn drop(&mut self) {
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.stopped = true;
            if let Some((_, cancel)) = &state.active { cancel.store(true, Ordering::Release); }
            self.shared.ready.notify_one();
        }
        if let Some(worker) = self.worker.take() { let _ = worker.join(); }
    }
}

fn worker_loop(shared: Arc<Shared>) {
    loop {
        let (media, cancel) = {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if state.stopped { return; }
                if Instant::now() >= state.busy_until && state.completed.is_empty() {
                    if let Some(media) = state.queue.pop_front() {
                        let cancel = Arc::new(AtomicBool::new(false));
                        state.active = Some((media.clone(), cancel.clone()));
                        break (media, cancel);
                    }
                }
                state = shared.ready.wait_timeout(state, Duration::from_millis(100))
                    .unwrap_or_else(|e| e.into_inner()).0;
            }
        };
        let result = (|| {
            let before = stamp(&media.path)?;
            let cached = {
                let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                let found = state.cache.iter().position(|entry| entry.stamp == before);
                found.and_then(|index| state.cache.remove(index)).map(|entry| {
                    let peaks = entry.peaks.clone(); state.cache.push_front(entry); peaks
                })
            };
            let peaks = match cached { Some(peaks) => peaks, None => Arc::new(extract(&media.path, &cancel)?) };
            if stamp(&media.path)? != before { return Err("Audio source changed during waveform preparation".into()); }
            Ok((before, peaks))
        })();
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active = None;
        if state.stopped { return; }
        if cancel.load(Ordering::Acquire) {
            // A relink request wins over requeuing the obsolete media snapshot.
            if !state.queue.iter().any(|m| m.id == media.id) && state.queue.len() < QUEUE_LIMIT {
                state.queue.push_front(media);
            }
            continue;
        }
        let peaks = result.map(|(fingerprint, peaks)| {
            state.cache.retain(|entry| entry.stamp.path != fingerprint.path);
            while state.cache.len() >= CACHE_LIMIT || state.cache.iter().map(|e| e.peaks.bytes()).sum::<usize>() + peaks.bytes() > CACHE_BYTES {
                if state.cache.pop_back().is_none() { break; }
            }
            state.cache.push_front(Cached { stamp: fingerprint, peaks: peaks.clone() });
            peaks
        });
        state.completed.push_back(WaveformResult { media, peaks });
    }
}

/// Streaming reducer: at most 500K pairs across the complete pyramid.
/// Long sources become coarser, never truncated or represented with fake bars.
struct Reducer {
    rate: u32, channels: usize, frames: u64, bucket_samples: u64,
    pending_frames: u64, pending: Vec<Peak>, base: Vec<Peak>, bucket_limit: usize }
impl Reducer {
    fn new(rate: u32, channels: usize) -> Result<Self, String> {
        if rate == 0 || rate > 768_000 || channels == 0 || channels > 32 { return Err("Unsupported audio format for waveform".into()); }
        let maximum = JOB_PEAK_LIMIT / 4 / channels;
        let bucket_limit = 1usize << (usize::BITS - 1 - maximum.leading_zeros());
        Ok(Self { rate, channels, frames: 0, bucket_samples: (rate as u64).div_ceil(100),
            pending_frames: 0, pending: vec![Peak::default(); channels], base: Vec::new(), bucket_limit })
    }
    fn frame(&mut self, samples: &[f32]) -> Result<(), String> {
        for (channel, &sample) in samples.iter().enumerate() {
            if !sample.is_finite() { return Err("Non-finite audio sample".into()); }
            self.pending[channel] = self.pending[channel].merge(Peak { min: sample, max: sample });
        }
        self.frames += 1; self.pending_frames += 1;
        if self.pending_frames == self.bucket_samples { self.flush(); }
        Ok(())
    }
    fn flush(&mut self) {
        if self.pending_frames == 0 { return; }
        self.base.extend_from_slice(&self.pending);
        self.pending.fill(Peak::default()); self.pending_frames = 0;
        if self.base.len()/self.channels >= self.bucket_limit {
            self.base = coarsen(&self.base, self.channels);
            self.bucket_samples *= 2;
        }
    }
    fn finish(mut self) -> WaveformPeaks {
        self.flush();
        let mut levels = vec![PeakLevel { samples_per_bucket: self.bucket_samples, peaks: self.base }];
        while levels.last().unwrap().peaks.len() > self.channels {
            let previous = levels.last().unwrap();
            levels.push(PeakLevel { samples_per_bucket: previous.samples_per_bucket.saturating_mul(2), peaks: coarsen(&previous.peaks, self.channels) });
        }
        WaveformPeaks { sample_rate: self.rate, channels: self.channels, sample_frames: self.frames, levels }
    }
}
fn coarsen(peaks: &[Peak], channels: usize) -> Vec<Peak> {
    let mut result = Vec::with_capacity((peaks.len()/channels).div_ceil(2)*channels);
    for pair in peaks.chunks(channels*2) {
        for channel in 0..channels {
            result.push(pair[channel].merge(pair.get(channel+channels).copied().unwrap_or_default()));
        }
    }
    result
}

fn read_pcm(reader: impl Read, rate: u32, channels: usize) -> Result<WaveformPeaks, String> {
    let mut reducer = Reducer::new(rate, channels)?;
    let mut reader = BufReader::with_capacity(64*1024, reader);
    let mut frame = vec![0u8; channels*4];
    let mut samples = vec![0.0; channels];
    loop {
        let mut read = 0;
        while read < frame.len() {
            let count = reader.read(&mut frame[read..]).map_err(|e| e.to_string())?;
            if count == 0 {
                return if read == 0 { Ok(reducer.finish()) } else { Err("Truncated waveform PCM frame".into()) };
            }
            read += count;
        }
        for (sample, bytes) in samples.iter_mut().zip(frame.chunks_exact(4)) { *sample = f32::from_le_bytes(bytes.try_into().unwrap()); }
        reducer.frame(&samples)?;
    }
}

/// Reader helper threads only drain this worker's single owned child. No extra
/// preparation job or child is started while one is active.
fn run<T: Send>(tool: &Path, args: &[String], cancel: &AtomicBool,
    consume: impl FnOnce(std::process::ChildStdout) -> Result<T, String> + Send) -> Result<T, String> {
    if cancel.load(Ordering::Acquire) { return Err("Waveform preparation interrupted".into()); }
    let mut command = Command::new(tool);
    #[cfg(windows)] {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000 | 0x00004000); // hidden, below normal
    }
    let mut child = command.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    let stdout = child.stdout.take().ok_or("Missing waveform output")?;
    let mut stderr = child.stderr.take().ok_or("Missing waveform diagnostics")?;
    std::thread::scope(|scope| {
        let output = scope.spawn(move || consume(stdout));
        let errors = scope.spawn(move || {
            let mut saved = Vec::new(); let mut chunk = [0u8; 4096];
            while let Ok(count) = stderr.read(&mut chunk) {
                if count == 0 { break; }
                let keep = count.min((64usize*1024).saturating_sub(saved.len()));
                saved.extend_from_slice(&chunk[..keep]);
            }
            saved
        });
        let start = Instant::now();
        let mut timed_out = false;
        let status = loop {
            timed_out |= start.elapsed() > Duration::from_secs(300);
            if cancel.load(Ordering::Acquire) || timed_out { let _ = child.kill(); }
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(error) => { let _ = child.kill(); let _ = child.wait(); break Err(error.to_string()); }
            }
        };
        let result = output.join().map_err(|_| "Waveform reader failed")?;
        let diagnostics = errors.join().map_err(|_| "Waveform diagnostics failed")?;
        if cancel.load(Ordering::Acquire) { return Err("Waveform preparation interrupted".into()); }
        if timed_out { return Err("Waveform preparation timed out".into()); }
        if !status?.success() { return Err(format!("Waveform decode failed: {}", String::from_utf8_lossy(&diagnostics).chars().take(240).collect::<String>())); }
        result
    })
}
fn extract(path: &Path, cancel: &AtomicBool) -> Result<WaveformPeaks, String> {
    let probe = run(&find_ffprobe()?, &[
        "-v".into(), "error".into(), "-select_streams".into(), "a:0".into(),
        "-show_entries".into(), "stream=sample_rate,channels".into(), "-of".into(), "json".into(), path.display().to_string(),
    ], cancel, |output| { let mut data = Vec::new(); output.take(64*1024).read_to_end(&mut data).map_err(|e|e.to_string())?; Ok(data) })?;
    let metadata: serde_json::Value = serde_json::from_slice(&probe).map_err(|e|e.to_string())?;
    let stream = metadata["streams"].as_array().and_then(|s|s.first()).ok_or("No audio stream")?;
    let rate = stream["sample_rate"].as_str().and_then(|s|s.parse::<u32>().ok()).ok_or("Missing audio sample rate")?;
    let channels = stream["channels"].as_u64().ok_or("Missing audio channels")? as usize;
    Reducer::new(rate, channels)?;
    // Match original preview decoding: no -ss0 (AAC priming), first_pts=0
    // inserts genuine leading silence for delayed audio. Preserve native rate
    // and channels; do not mix to mono or discard peaks through downsampling.
    run(&find_ffmpeg()?, &[
        "-nostdin".into(), "-v".into(), "error".into(), "-xerror".into(), "-threads".into(), "1".into(),
        "-i".into(), path.display().to_string(), "-map".into(), "0:a:0".into(), "-vn".into(), "-sn".into(), "-dn".into(),
        "-filter_threads".into(), "1".into(), "-af".into(), format!("aresample={rate}:first_pts=0,aformat=sample_fmts=flt"),
        "-ac".into(), channels.to_string(), "-ar".into(), rate.to_string(), "-f".into(), "f32le".into(), "pipe:1".into(),
    ], cancel, move |output| read_pcm(output, rate, channels))
}

