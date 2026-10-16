//! Idle-only, real timeline pictures. UI calls never inspect the filesystem.
//! Integrate by planning visible clips, calling `request_visible` on geometry or
//! media changes, polling results, and holding `set_busy(true)` during playback,
//! gestures/export. Results carry a generation; UI image ownership needs its own
//! bound. This worker never borrows Program/Source decoders or prepares proxies.
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

use super::{CachingSource, FrameSource, ImageBuf};
use crate::project::{Clip, FrameRate, Media, MediaKind};

const LIMIT: usize = 64;
const CACHE_BYTES: usize = 16 * 1024 * 1024;
const MAX_DIMENSION: u32 = 256;

#[derive(Clone, Debug, PartialEq)]
pub struct FilmstripRequest {
    pub media: Media,
    pub ordinal: i64,
    pub width: u32,
    pub height: u32 }
impl FilmstripRequest {
    fn valid(&self) -> bool {
        (self.media.has_video || self.media.kind == MediaKind::Still)
            && self.ordinal >= 0
            && (2..=MAX_DIMENSION).contains(&self.width)
            && (2..=MAX_DIMENSION).contains(&self.height)
            && (self.media.kind == MediaKind::Still
                || (self.media.duration_secs.is_finite()
                    && self.media.duration_secs > 0.0
                    && self.media.source_fps.is_finite()
                    && self.media.source_fps > 0.0
                    && self.ordinal < self.media.source_frames()))
    }
    fn seconds(&self) -> f64 {
        self.ordinal as f64 / decode_fps(&self.media)
    }
}
fn decode_fps(media: &Media) -> f64 {
    if media.source_fps > 1.0 {
        media.source_fps
    } else {
        24.0
    }
}
fn ordinal_at(seconds: f64, fps: f64) -> i64 {
    super::decode::nearest_source_frame(seconds, fps)
}

#[derive(Clone, Debug)]
pub struct FilmstripSample {
    /// Cell left edge in absolute sequence frames; can precede visible_start.
    pub timeline_frame: i64,
    pub source_seconds: f64,
    pub request: FilmstripRequest }

/// Visible interval is half-open. Grid is anchored to clip start, so scrolling
/// at the same zoom reuses ordinals. Sample at each cell's left edge, preserving
/// the precise source offset of trimmed/rational clips before nearest-frame
/// conversion. Stills always use ordinal zero. At most64 cells per clip.
pub fn plan_visible(
    clip: &Clip,
    media: &Media,
    rate: FrameRate,
    visible_start: i64,
    visible_end: i64,
    frames_per_cell: i64,
    width: u32,
    height: u32,
) -> Vec<FilmstripSample> {
    if clip.media_id != media.id
        || clip.duration <= 0
        || frames_per_cell <= 0
        || rate.num == 0
        || rate.den == 0
        || !clip.source_time_offset.is_finite()
        || visible_end <= visible_start
    {
        return Vec::new();
    }
    if !(FilmstripRequest {
        media: media.clone(),
        ordinal: 0,
        width,
        height })
    .valid()
    {
        return Vec::new();
    }
    let end = clip.timeline_start.saturating_add(clip.duration);
    let start = visible_start.max(clip.timeline_start);
    let stop = visible_end.min(end);
    if start >= stop {
        return Vec::new();
    }
    let local = start.saturating_sub(clip.timeline_start);
    let mut cell = local / frames_per_cell * frames_per_cell;
    let mut samples = Vec::new();
    while samples.len() < LIMIT && cell < clip.duration {
        let frame = clip.timeline_start.saturating_add(cell);
        if frame >= stop {
            break;
        }
        let seconds = clip.source_time_seconds(cell, rate);
        if !seconds.is_finite() || seconds < 0.0 {
            break;
        }
        let ordinal = if media.kind == MediaKind::Still {
            0
        } else {
            ordinal_at(seconds, decode_fps(media))
                .min(media.source_frames().saturating_sub(1).max(0))
        };
        let request = FilmstripRequest {
            media: media.clone(),
            ordinal,
            width,
            height };
        if !request.valid() {
            break;
        }
        samples.push(FilmstripSample {
            timeline_frame: frame,
            source_seconds: seconds,
            request });
        let next = cell.saturating_add(frames_per_cell);
        if next <= cell {
            break;
        }
        cell = next;
    }
    samples
}

pub struct FilmstripResult {
    pub generation: u64,
    pub request: FilmstripRequest,
    /// Actual fitted RGB dimensions; missing/offline/error never becomes a fake picture.
    pub image: Result<Arc<ImageBuf>, String> }
struct State {
    visible: Vec<FilmstripRequest>,
    pending: VecDeque<FilmstripRequest>,
    completed: VecDeque<FilmstripResult>,
    active: Option<(FilmstripRequest, Arc<AtomicBool>)>,
    generation: u64,
    retry_epoch: u64,
    busy_until: Instant,
    stopped: bool }
struct Shared {
    state: Mutex<State>,
    ready: Condvar }
pub struct FilmstripEngine {
    shared: Arc<Shared>,
    worker: Option<std::thread::JoinHandle<()>> }

impl FilmstripEngine {
    pub fn new() -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                visible: Vec::new(),
                pending: VecDeque::new(),
                completed: VecDeque::new(),
                active: None,
                generation: 0,
                retry_epoch: 0,
                busy_until: Instant::now(),
                stopped: false }),
            ready: Condvar::new() });
        let work = shared.clone();
        Self {
            shared,
            worker: Some(std::thread::spawn(move || worker(work))) }
    }
    /// Replace latest visible set (first64 unique valid requests), no file I/O.
    /// Caller should retain generation and ignore older polled results.
    pub fn request_visible(&self, requests: Vec<FilmstripRequest>) -> u64 {
        let mut visible = Vec::new();
        for request in requests {
            if request.valid() && !visible.contains(&request) {
                visible.push(request);
            }
            if visible.len() == LIMIT {
                break;
            }
        }
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.visible == visible {
            return state.generation;
        }
        state.generation = state.generation.wrapping_add(1);
        if let Some((request, cancel)) = &state.active {
            if !visible.contains(request) {
                cancel.store(true, Ordering::Release);
            }
        }
        state.pending = visible
            .iter()
            .filter(|r| state.active.as_ref().is_none_or(|(a, _)| a != *r))
            .cloned()
            .collect();
        state.visible = visible;
        state.completed.clear();
        self.shared.ready.notify_one();
        state.generation
    }
    /// Explicit retry/revalidate for same-path disk changes or project replacement.
    /// Metadata is still checked exclusively on the worker.
    pub fn invalidate(&self) -> u64 {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.generation = state.generation.wrapping_add(1);
        state.retry_epoch = state.retry_epoch.wrapping_add(1);
        if let Some((_, cancel)) = &state.active {
            cancel.store(true, Ordering::Release);
        }
        state.pending = state.visible.iter().cloned().collect();
        state.completed.clear();
        self.shared.ready.notify_one();
        state.generation
    }
    pub fn set_busy(&self, busy: bool) {
        if !busy {
            return;
        }
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.busy_until = Instant::now() + Duration::from_secs(2);
        if let Some((_, cancel)) = &state.active {
            cancel.store(true, Ordering::Release);
        }
        self.shared.ready.notify_one();
    }
    pub fn poll(&self) -> Option<FilmstripResult> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .completed
            .pop_front()
    }
}
impl Default for FilmstripEngine {
    fn default() -> Self {
        Self::new()
    }
}
impl Drop for FilmstripEngine {
    fn drop(&mut self) {
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.stopped = true;
            if let Some((_, cancel)) = &state.active {
                cancel.store(true, Ordering::Release);
            }
            self.shared.ready.notify_one();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    path: PathBuf,
    bytes: u64,
    modified: SystemTime }
fn stamp(path: &PathBuf) -> Result<Stamp, String> {
    let m = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !m.is_file() {
        return Err("Filmstrip source is not a file".into());
    }
    Ok(Stamp {
        path: path.clone(),
        bytes: m.len(),
        modified: m.modified().map_err(|e| e.to_string())? })
}
struct Cached {
    request: FilmstripRequest,
    stamp: Stamp,
    image: Arc<ImageBuf> }
fn extract(request: &FilmstripRequest, cancel: Arc<AtomicBool>) -> Result<Arc<ImageBuf>, String> {
    // Per-job decoder lifetime also bounds decoder/cache memory and guarantees
    // no stale internally cached pixels survive file replacement or relinking.
    let mut source = CachingSource::with_cancel(cancel);
    source.set_project_media(std::slice::from_ref(&request.media));
    let image = source.video(
        request.media.id,
        request.seconds(),
        request.width,
        request.height,
    );
    image.map(Arc::new).ok_or_else(|| {
        source
            .take_decode_error()
            .unwrap_or_else(|| "Filmstrip decode returned no image".into())
    })
}
fn worker(shared: Arc<Shared>) {
    let mut cache: VecDeque<Cached> = VecDeque::new();
    // Stable failures do not repeatedly spawn decoders as the viewport moves.
    // Metadata changes/relinks or explicit invalidate permit a retry.
    let mut errors: VecDeque<(FilmstripRequest, Option<Stamp>, String)> = VecDeque::new();
    let mut retry_epoch = 0;
    loop {
        let (request, cancel) = {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if state.stopped {
                    return;
                }
                if retry_epoch != state.retry_epoch {
                    retry_epoch = state.retry_epoch;
                    errors.clear();
                    cache.clear();
                }
                if Instant::now() >= state.busy_until && state.completed.len() < LIMIT {
                    if let Some(request) = state.pending.pop_front() {
                        let cancel = Arc::new(AtomicBool::new(false));
                        state.active = Some((request.clone(), cancel.clone()));
                        break (request, cancel);
                    }
                }
                state = shared
                    .ready
                    .wait_timeout(state, Duration::from_millis(50))
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        };
        let fingerprint = stamp(&request.media.path);
        let identity = fingerprint.as_ref().ok().cloned();
        let prior_error = errors
            .iter()
            .find(|(r, s, _)| r == &request && s == &identity)
            .map(|(_, _, e)| e.clone());
        let result = if let Some(error) = prior_error {
            Err(error)
        } else {
            (|| {
                let before = fingerprint?;
                cache.retain(|c| c.stamp.path != before.path || c.stamp == before);
                let cached = cache
                    .iter()
                    .position(|c| c.request == request && c.stamp == before)
                    .and_then(|i| cache.remove(i));
                let image = if let Some(cached) = cached {
                    let image = cached.image.clone();
                    cache.push_front(cached);
                    image
                } else {
                    extract(&request, cancel.clone())?
                };
                if stamp(&request.media.path)? != before {
                    return Err("Filmstrip source changed during decoding".into());
                }
                if !cancel.load(Ordering::Acquire)
                    && !cache
                        .iter()
                        .any(|c| c.request == request && c.stamp == before)
                {
                    while cache.len() >= LIMIT
                        || cache.iter().map(|c| c.image.rgb.len()).sum::<usize>() + image.rgb.len()
                            > CACHE_BYTES
                    {
                        if cache.pop_back().is_none() {
                            break;
                        }
                    }
                    cache.push_front(Cached {
                        request: request.clone(),
                        stamp: before,
                        image: image.clone() });
                }
                Ok(image)
            })()
        };
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active = None;
        if state.stopped {
            return;
        }
        if !state.visible.contains(&request) {
            continue;
        }
        if cancel.load(Ordering::Acquire) {
            if !state.pending.contains(&request) {
                state.pending.push_front(request);
            }
            continue;
        }
        if let Err(error) = &result {
            errors.retain(|(r, _, _)| r != &request);
            if errors.len() == LIMIT {
                errors.pop_back();
            }
            errors.push_front((request.clone(), identity, error.clone()));
        }
        let generation = state.generation;
        state.completed.push_back(FilmstripResult {
            generation,
            request,
            image: result });
    }
}

