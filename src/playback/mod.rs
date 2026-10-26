mod text;
mod audio;
pub(crate) mod sync_audio;
pub(crate) mod precision;
pub(crate) mod gpu_composite;
mod meters;
mod composite;
mod decode;
pub mod native_preview;
pub mod optimized;
pub mod timeline_waveforms;
pub mod filmstrips;
pub use native_preview::set_index_busy;
pub use decode::set_preview_hardware;
pub mod telemetry;
mod thumbnails;

pub use audio::Pcm;
pub use meters::MeterSnapshot;
pub use composite::{composite, FrameSource, ImageBuf};
pub(crate) use composite::picture_route;
pub use decode::{find_ffmpeg, find_ffprobe, probe_path, probe_path_cancellable, CachingSource};
pub(crate) use decode::decode_audio_window;
pub use thumbnails::ThumbnailEngine;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::project::{FrameRate, Project};
pub(crate) use audio::{
    mix_window_range_checked, required_audio_windows_checked, PcmWindow, WindowBuffers, PREVIEW_WINDOW_SAMPLES };

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct AudioWindowKey {
    path: PathBuf,
    stamp: Option<(u64, std::time::SystemTime)>,
    block: i64 }

#[derive(Default)]
struct AudioWindowState {
    entries: VecDeque<(AudioWindowKey, Arc<PcmWindow>)>,
    loading: HashSet<AudioWindowKey>,
    queue: VecDeque<(AudioWindowKey, Arc<AtomicBool>)>,
    stopped: bool }

impl AudioWindowState {
    fn store(&mut self, key: AudioWindowKey, window: Arc<PcmWindow>) {
        self.entries.retain(|(old, _)| old != &key);
        self.entries.push_back((key, window));
        while self.entries.len() > 64 {
            self.entries.pop_front();
        }
    }
}

struct AudioWindowCache {
    state: Mutex<AudioWindowState>,
    changed: Condvar,
    workers: Mutex<Vec<thread::JoinHandle<()>>> }

impl AudioWindowCache {
    fn new() -> Arc<Self> {
        let shared = Arc::new(Self {
            state: Mutex::new(AudioWindowState::default()),
            changed: Condvar::new(),
            workers: Mutex::new(Vec::new()) });
        for _ in 0..2 {
            let worker = shared.clone();
            let handle = thread::spawn(move || loop {
                let (key, stop) = {
                    let mut state = worker
                        .state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    while state.queue.is_empty() && !state.stopped {
                        state = worker
                            .changed
                            .wait(state)
                            .unwrap_or_else(|error| error.into_inner());
                    }
                    if state.stopped {
                        return;
                    }
                    let (key, stop) = state.queue.pop_front().unwrap();
                    if stop.load(Ordering::Acquire)
                        || state.entries.iter().any(|(cached, _)| cached == &key)
                        || !state.loading.insert(key.clone())
                    {
                        continue;
                    }
                    (key, stop)
                };
                // Prefetch errors are retried and reported only when audible.
                let _ = worker.decode(key, &stop);
            });
            shared
                .workers
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(handle);
        }
        shared
    }

    fn shutdown(&self) {
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.stopped = true;
            state.queue.clear();
            self.changed.notify_all();
        }
        let handles = std::mem::take(
            &mut *self
                .workers
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        for handle in handles {
            let _ = handle.join();
        }
    }

    fn decode(&self, key: AudioWindowKey, stop: &AtomicBool) -> Result<Arc<PcmWindow>, String> {
        let result = decode::decode_audio_window(&key.path, key.block, stop).map(|pcm| {
            Arc::new(PcmWindow {
                start_sample: key.block * PREVIEW_WINDOW_SAMPLES,
                pcm })
        });
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.loading.remove(&key);
        if !stop.load(Ordering::Acquire) {
            if let Ok(window) = &result {
                // 64 windows occupy at most48MiB stereo float PCM. In-flight
                // buffers retain Arcs, so eviction cannot interrupt mixing.
                state.store(key, window.clone());
            }
        }
        self.changed.notify_all();
        result
    }

    fn get(&self, key: AudioWindowKey, stop: &AtomicBool) -> Result<Arc<PcmWindow>, String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        loop {
            if stop.load(Ordering::Acquire) || state.stopped {
                return Err("Media decode cancelled".into());
            }
            if let Some(index) = state.entries.iter().position(|(cached, _)| cached == &key) {
                let entry = state.entries.remove(index).unwrap();
                let window = entry.1.clone();
                state.entries.push_back(entry);
                return Ok(window);
            }
            if !state.loading.contains(&key) {
                state.queue.retain(|(queued, _)| queued != &key);
                state.loading.insert(key.clone());
                drop(state);
                return self.decode(key, stop);
            }
            state = self
                .changed
                .wait_timeout(state, Duration::from_millis(10))
                .unwrap_or_else(|error| error.into_inner())
                .0;
        }
    }

    fn prefetch(&self, key: AudioWindowKey, stop: Arc<AtomicBool>) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state
            .queue
            .retain(|(_, cancelled)| !cancelled.load(Ordering::Acquire));
        if state.stopped
            || state.queue.len() >= 32
            || state.loading.contains(&key)
            || state.entries.iter().any(|(cached, _)| cached == &key)
            || state.queue.iter().any(|(queued, _)| queued == &key)
        {
            return;
        }
        state.queue.push_back((key, stop));
        self.changed.notify_one();
    }
}

struct PlaybackAudioSource {
    paths: HashMap<u64, PathBuf>,
    keys: HashMap<u64, AudioWindowKey>,
    cache: Arc<AudioWindowCache> }

impl PlaybackAudioSource {
    fn new(project: &Project, cache: Arc<AudioWindowCache>) -> Self {
        let paths = project
            .media
            .iter()
            .filter(|media| media.has_audio)
            .map(|media| (media.id, media.path.clone()))
            .collect();
        Self { paths, keys: HashMap::new(), cache }
    }

    fn window_key(&mut self, media_id: u64, block: i64) -> Option<AudioWindowKey> {
        let path = self.paths.get(&media_id)?;
        // Only inspect files reached by the audible window or bounded lookahead.
        // Starting playback and moving a mixer control must not stat an entire
        // bin, especially when unused assets live on slow or offline storage.
        let base = self.keys.entry(media_id).or_insert_with(|| {
            let stamp = std::fs::metadata(path)
                .ok()
                .and_then(|meta| meta.modified().ok().map(|time| (meta.len(), time)));
            AudioWindowKey { path: path.clone(), stamp, block: 0 }
        });
        let mut key = base.clone();
        key.block = block;
        Some(key)
    }

    fn windows(
        &mut self,
        project: &Project,
        cursor: i64,
        frames: i64,
        stop: &AtomicBool,
    ) -> Result<WindowBuffers, String> {
        let mut buffers = WindowBuffers::new();
        for (id, block) in required_audio_windows_checked(project, cursor, frames, stop)? {
            if stop.load(Ordering::Acquire) {
                return Err("Media decode cancelled".into());
            }
            let Some(key) = self.window_key(id, block) else {
                continue;
            };
            buffers
                .entry(id)
                .or_default()
                .push(self.cache.get(key, stop)?);
        }
        Ok(buffers)
    }

    fn prefetch(&mut self, project: &Project, cursor: i64, frames: i64, stop: &Arc<AtomicBool>) {
        let Ok(needed) = required_audio_windows_checked(project, cursor, frames, stop) else { return; };
        for (id, block) in needed {
            if stop.load(Ordering::Acquire) {
                return;
            }
            if let Some(key) = self.window_key(id, block) {
                self.cache.prefetch(key, stop.clone());
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewIntent {
    Playback,
    Scrub,
    FinalSeek }

#[derive(Clone)]
struct PreviewRequest {
    frame: i64,
    w: u32,
    h: u32,
    token: u64,
    intent: PreviewIntent,
    submitted: Instant }

struct ActivePreview {
    request: PreviewRequest,
    cancel: Arc<AtomicBool>,
    started: Instant,
    speculative: bool }

impl ActivePreview {
    fn should_supersede(&self, next: &PreviewRequest, fps: f64) -> bool {
        if next.intent == PreviewIntent::FinalSeek {
            return true;
        }
        if self.speculative {
            return self.request.frame != next.frame || self.request.w != next.w || self.request.h != next.h;
        }
        let compatible = self.request.w == next.w && self.request.h == next.h
            && next.frame >= self.request.frame
            && next.frame.saturating_sub(self.request.frame) <= fps.ceil().max(8.0) as i64;
        // Let useful forward work finish, including its first decoder startup.
        // Incompatible dragging coalesces for a short window instead of restarting
        // the decoder on each pointer event. Release is always immediate.
        !compatible && self.started.elapsed() >= Duration::from_millis(120)
    }
}

struct PrefetchedImage {
    generation: u64,
    frame: i64,
    width: u32,
    height: u32,
    image: ImageBuf,
    error: Option<String>,
    proxy_used: bool }

impl PrefetchedImage {
    fn matches(&self, generation: u64, frame: i64, width: u32, height: u32) -> bool {
        self.generation == generation
            && self.frame == frame
            && self.width == width
            && self.height == height
    }
}

#[derive(Default)]
struct PreviewMailbox {
    project: Option<Project>,
    request: Option<PreviewRequest>,
    image: Option<PreviewImage>,
    generation: u64,
    stopped: bool,
    active: Option<ActivePreview>,
    frame_rate: f64,
    last_soft_cancel: Option<Instant>,
    wake_ui: Option<Arc<dyn Fn() + Send + Sync>>,
    optimized_update: Option<Option<Arc<optimized::OptimizedCache>>>,
    native_stats: Option<native_preview::NativePreviewStats> }

pub struct PreviewImage {
    pub token: u64,
    pub frame: i64,
    pub image: ImageBuf,
    pub error: Option<String>,
    pub requested_at: Instant,
    pub ready_at: Instant,
    pub decode_ms: f64,
    pub proxy_used: bool }

pub struct PreviewEngine {
    mailbox: Arc<(Mutex<PreviewMailbox>, Condvar)>,
    token: AtomicU64,
    cancel: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>> }

impl PreviewEngine {

    pub fn set_optimized_cache(&self, cache: Option<Arc<optimized::OptimizedCache>>) {
        let (lock, ready) = &*self.mailbox;
        let mut mailbox = lock.lock().unwrap_or_else(|e| e.into_inner());
        mailbox.optimized_update = Some(cache);
        if let Some(active) = &mailbox.active { active.cancel.store(true, Ordering::Release); }
        ready.notify_one();
    }

    pub fn set_wake_callback(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        self.mailbox.0.lock().unwrap_or_else(|e| e.into_inner()).wake_ui = Some(wake);
    }

    pub fn new() -> Self {
        Self::new_cancellable(Arc::new(AtomicBool::new(false)))
    }

    pub fn new_cancellable(cancel: Arc<AtomicBool>) -> Self {
        let mailbox = Arc::new((Mutex::new(PreviewMailbox::default()), Condvar::new()));
        let worker_mailbox = mailbox.clone();
        let worker_cancel = cancel.clone();
        let worker = thread::spawn(move || preview_loop(worker_mailbox, worker_cancel));
        Self {
            mailbox,
            token: AtomicU64::new(1),
            cancel,
            worker: Some(worker) }
    }

    pub fn set_project(&self, project: Project) {
        let (lock, ready) = &*self.mailbox;
        let mut mailbox = lock.lock().unwrap_or_else(|err| err.into_inner());
        mailbox.frame_rate = project.sequence.frame_rate.fps();
        mailbox.project = Some(project);
        mailbox.generation = mailbox.generation.wrapping_add(1);
        mailbox.image = None;
        mailbox.request = None;
        if let Some(active) = &mailbox.active {
            active.cancel.store(true, Ordering::Release);
        }
        ready.notify_one();
    }

    pub fn request(&self, frame: i64, w: u32, h: u32) -> u64 {
        self.request_mode(frame, w, h, PreviewIntent::FinalSeek)
    }

    pub fn request_scrub(&self, frame: i64, w: u32, h: u32) -> u64 {
        self.request_mode(frame, w, h, PreviewIntent::Scrub)
    }

    /// Forward playback may warm one following picture while the UI consumes
    /// this one. Scrubs and seeks retain latest-request-only scheduling.
    pub fn request_playback(&self, frame: i64, w: u32, h: u32) -> u64 {
        self.request_mode(frame, w, h, PreviewIntent::Playback)
    }

    fn request_mode(&self, frame: i64, w: u32, h: u32, intent: PreviewIntent) -> u64 {
        let token = self.token.fetch_add(1, Ordering::Relaxed) + 1;
        let (lock, ready) = &*self.mailbox;
        let mut mailbox = lock.lock().unwrap_or_else(|err| err.into_inner());
        // Scrubbing replaces work that has not started, keeping memory and latency bounded.
        let request = PreviewRequest {
            frame,
            w: w.max(2),
            h: h.max(2),
            token,
            intent,
            submitted: Instant::now() };
        if let Some(active) = &mailbox.active {
            let immediate = intent == PreviewIntent::FinalSeek || active.speculative;
            let may_interrupt = immediate || mailbox.last_soft_cancel
                .is_none_or(|last| last.elapsed() >= Duration::from_secs(1));
            if may_interrupt && active.should_supersede(&request, mailbox.frame_rate)
                && !active.cancel.load(Ordering::Acquire) {
                active.cancel.store(true, Ordering::Release);
                if !immediate { mailbox.last_soft_cancel = Some(Instant::now()); }
            }
        }
        mailbox.request = Some(request);
        ready.notify_one();
        token
    }

    pub fn poll(&self) -> Option<PreviewImage> {
        self.mailbox.0.lock().ok()?.image.take()
    }
}

impl Drop for PreviewEngine {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        let (lock, ready) = &*self.mailbox;
        if let Ok(mut mailbox) = lock.lock() {
            mailbox.stopped = true;
            ready.notify_one();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn preview_loop(shared: Arc<(Mutex<PreviewMailbox>, Condvar)>, cancel: Arc<AtomicBool>) {
    let mut project: Option<Project> = None;
    let mut source = CachingSource::with_cancel(cancel.clone());
    let mut prefetched: Option<PrefetchedImage> = None;
    loop {
        let (lock, ready) = &*shared;
        let (next_project, request, generation, request_cancel, optimized_update) = {
            let mut mailbox = lock.lock().unwrap_or_else(|err| err.into_inner());
            while mailbox.request.is_none() && mailbox.optimized_update.is_none()
                && !mailbox.stopped && !cancel.load(Ordering::Acquire) {
                mailbox = ready
                    .wait_timeout(mailbox, Duration::from_millis(20))
                    .unwrap_or_else(|err| err.into_inner())
                    .0;
            }
            if mailbox.stopped || cancel.load(Ordering::Acquire) {
                return;
            }
            let request = mailbox.request.take();
            let request_cancel = Arc::new(AtomicBool::new(false));
            mailbox.active = request.as_ref().map(|request| ActivePreview {
                request: request.clone(), cancel: request_cancel.clone(),
                started: Instant::now(), speculative: false });
            (
                mailbox.project.take(),
                request,
                mailbox.generation,
                request_cancel,
                mailbox.optimized_update.take(),
            )
        };
        if let Some(cache) = optimized_update {
            source.set_optimized_cache(cache);
            prefetched = None;
        }
        if let Some(next) = next_project {
            source.set_project_media(&next.media);
            project = Some(next);
        }
        let Some(PreviewRequest {
            frame,
            w,
            h,
            token,
            intent,
            submitted }) = request
        else {
            continue;
        };
        source.set_preview_intent(intent);
        source.set_preview_request(Some(request_cancel.clone()));
        let decode_started = Instant::now();
        let cached = prefetched
            .take()
            .filter(|image| intent != PreviewIntent::FinalSeek && image.matches(generation, frame, w, h));
        let (image, error, proxy_used) = if let Some(cached) = cached {
            (cached.image, cached.error, cached.proxy_used)
        } else if let Some(project) = &project {
            let image = composite::composite_cancellable(project, frame, w, h, &mut source);
            let error = source.take_decode_error();
            match image {
                Ok(image) => (image, error, source.take_proxy_used()),
                Err(_) => continue }
        } else {
            (ImageBuf::black(w, h), None, false)
        };
        // Drain errors even when this project's generation has been replaced,
        // so an older request cannot leak its failure into the next picture.
        let mut mailbox = lock.lock().unwrap_or_else(|err| err.into_inner());
        if mailbox.stopped || cancel.load(Ordering::Acquire) {
            return;
        }
        // A completed playback frame can still be useful after the playhead advances.
        // A frame from an older project revision must never replace the current picture.
        let published = mailbox.generation == generation && !request_cancel.load(Ordering::Acquire);
        mailbox.native_stats = source.native_preview_stats();
        if published {
            mailbox.image = Some(PreviewImage {
                token,
                frame,
                image,
                error,
                requested_at: submitted,
                ready_at: Instant::now(),
                decode_ms: decode_started.elapsed().as_secs_f64() * 1000.0,
                proxy_used });
        }
        mailbox.active = None;
        let warm_next = intent == PreviewIntent::Playback && mailbox.request.is_none()
            && mailbox.generation == generation && !request_cancel.load(Ordering::Acquire);
        let prefetch_cancel = Arc::new(AtomicBool::new(false));
        if warm_next {
            mailbox.active = Some(ActivePreview {
                request: PreviewRequest { frame: frame.saturating_add(1), w, h, token,
                    intent, submitted: Instant::now() },
                cancel: prefetch_cancel.clone(), started: Instant::now(), speculative: true });
        }
        let wake_ui = published.then(|| mailbox.wake_ui.clone()).flatten();
        drop(mailbox);
        if let Some(wake) = wake_ui { wake(); }
        if warm_next {
            if let Some(project) = &project {
                let next = frame.saturating_add(1);
                if next < project.sequence.duration_frames() {
                    source.set_preview_intent(PreviewIntent::Playback);
                    source.set_preview_request(Some(prefetch_cancel.clone()));
                    let image = composite::composite_cancellable(project, next, w, h, &mut source);
                    let error = source.take_decode_error();
                    if let Ok(image) = image {
                        prefetched = Some(PrefetchedImage {
                            generation, frame: next, width: w, height: h, image, error,
                            proxy_used: source.take_proxy_used() });
                    }
                }
            }
        }
    }
}

struct AudioPlayRequest {
    project: Project,
    start_frame: i64,
    stop: Arc<AtomicBool> }

#[derive(Default)]
struct AudioPlayMailbox {
    request: Option<AudioPlayRequest>,
    stopped: bool }

pub struct AudioEngine {
    meters: Arc<Mutex<meters::PlaybackMeters>>,
    stop: Arc<Mutex<Arc<AtomicBool>>>,
    status: Arc<Mutex<Option<String>>>,
    ready: Arc<AtomicBool>,
    pending_project: Arc<Mutex<Option<Project>>>,
    cache: Arc<AudioWindowCache>,
    mailbox: Arc<(Mutex<AudioPlayMailbox>, Condvar)>,
    worker: Option<thread::JoinHandle<()>> }

impl AudioEngine {
    pub fn new() -> Self {
        let mut engine = Self {
            meters: Arc::new(Mutex::new(meters::PlaybackMeters::default())),
            stop: Arc::new(Mutex::new(Arc::new(AtomicBool::new(true)))),
            status: Arc::new(Mutex::new(None)),
            ready: Arc::new(AtomicBool::new(false)),
            pending_project: Arc::new(Mutex::new(None)),
            cache: AudioWindowCache::new(),
            mailbox: Arc::new((Mutex::new(AudioPlayMailbox::default()), Condvar::new())),
            worker: None };
        let mailbox = engine.mailbox.clone();
        let status = engine.status.clone();
        let ready = engine.ready.clone();
        let active_stop = engine.stop.clone();
        let pending_project = engine.pending_project.clone();
        let cache = engine.cache.clone();
        let meters = engine.meters.clone();
        engine.worker = Some(thread::spawn(move || loop {
            let request = {
                let (lock, changed) = &*mailbox;
                let mut pending = lock.lock().unwrap_or_else(|error| error.into_inner());
                while pending.request.is_none() && !pending.stopped {
                    pending = changed
                        .wait(pending)
                        .unwrap_or_else(|error| error.into_inner());
                }
                if pending.stopped {
                    return;
                }
                pending.request.take().unwrap()
            };
            // One persistent output worker serializes cancellation/cleanup and
            // device creation. Rapid Play/seek requests replace one queued item.
            if let Err(error) = play_from(
                request.project,
                request.start_frame,
                &request.stop,
                &active_stop,
                &ready,
                &pending_project,
                cache.clone(),
                meters.clone(),
            ) {
                let active = active_stop
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if Arc::ptr_eq(&active, &request.stop) && !request.stop.load(Ordering::Acquire) {
                    ready.store(true, Ordering::Release);
                    if let Ok(mut slot) = status.lock() {
                        *slot = Some(error);
                    }
                }
            }
        }));
        engine
    }

    pub fn stop(&self) {
        let flag = self.stop.lock().unwrap_or_else(|err| err.into_inner());
        flag.store(true, Ordering::Relaxed);
        self.meters.lock().unwrap_or_else(|error| error.into_inner()).reset(None);
        self.ready.store(false, Ordering::Release);
        if let Ok(mut pending) = self.pending_project.lock() {
            *pending = None;
        }
        if let Ok(mut mailbox) = self.mailbox.0.lock() {
            mailbox.request = None;
        }
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    pub fn meter_snapshot(&self) -> MeterSnapshot {
        self.meters.lock().unwrap_or_else(|error| error.into_inner()).snapshot(Instant::now())
    }

    pub fn clear_meters(&self) {
        self.meters.lock().unwrap_or_else(|error| error.into_inner()).clear();
    }

    pub fn update_mix(&self, project: Project) {
        let flag = self.stop.lock().unwrap_or_else(|err| err.into_inner());
        if !flag.load(Ordering::Relaxed) {
            if let Ok(mut pending) = self.pending_project.lock() {
                *pending = Some(project);
            }
        }
    }

    pub fn take_status(&self) -> Option<String> {
        self.status.lock().ok().and_then(|mut slot| slot.take())
    }

    pub fn play(&self, project: Project, start_frame: i64) {
        let stop = Arc::new(AtomicBool::new(false));
        let mut flag = self.stop.lock().unwrap_or_else(|err| err.into_inner());
        flag.store(true, Ordering::Relaxed);
        *flag = stop.clone();
        self.meters.lock().unwrap_or_else(|error| error.into_inner()).reset(Some(&stop));
        self.ready.store(false, Ordering::Release);
        if let Ok(mut pending) = self.pending_project.lock() {
            *pending = None;
        }
        if let Ok(mut status) = self.status.lock() {
            *status = None;
        }
        let (lock, changed) = &*self.mailbox;
        let mut mailbox = lock.lock().unwrap_or_else(|error| error.into_inner());
        mailbox.request = Some(AudioPlayRequest {
            project,
            start_frame,
            stop });
        changed.notify_one();
    }
}


fn mark_audio_ready(
    stop: &Arc<AtomicBool>,
    active_stop: &Mutex<Arc<AtomicBool>>,
    ready: &AtomicBool,
) -> bool {
    let active = active_stop.lock().unwrap_or_else(|err| err.into_inner());
    if Arc::ptr_eq(&active, stop) && !stop.load(Ordering::Relaxed) {
        ready.store(true, Ordering::Release);
        true
    } else {
        false
    }
}

fn take_mix_update(
    stop: &Arc<AtomicBool>,
    active_stop: &Mutex<Arc<AtomicBool>>,
    pending_project: &Mutex<Option<Project>>,
) -> Option<Project> {
    let active = active_stop.lock().unwrap_or_else(|err| err.into_inner());
    if Arc::ptr_eq(&active, stop) && !stop.load(Ordering::Relaxed) {
        pending_project.lock().ok()?.take()
    } else {
        None
    }
}

fn playback_chunk_frames(rate: FrameRate, remaining: i64) -> i64 {
    if remaining <= 0 {
        return 0;
    }
    // Keep the existing two-frame chunks at ordinary video rates. Audio-only
    // Source media has a 1000 Hz cursor grid, so two ticks would queue only 2 ms
    // per buffer against the worker's 4 ms sleep. Use at least 20 ms there and
    // at other high tick rates, keeping live mixer updates and memory bounded.
    let denominator = u64::from(rate.den) * 50;
    let minimum = if denominator == 0 {
        2
    } else {
        (u64::from(rate.num).div_ceil(denominator) as i64).max(2)
    };
    remaining.min(minimum)
}

fn play_from(
    mut project: Project,
    start_frame: i64,
    stop: &Arc<AtomicBool>,
    active_stop: &Mutex<Arc<AtomicBool>>,
    ready: &AtomicBool,
    pending_project: &Mutex<Option<Project>>,
    cache: Arc<AudioWindowCache>,
    meters: Arc<Mutex<meters::PlaybackMeters>>,
) -> Result<(), String> {
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    let (stream, handle) =
        rodio::OutputStream::try_default().map_err(|_| "Audio device unavailable".to_string())?;
    let sink = rodio::Sink::try_new(&handle).map_err(|err| err.to_string())?;
    let mut source = PlaybackAudioSource::new(&project, cache);
    let mut cursor = start_frame.max(0);
    let mut prefetch_at = cursor;
    let mut sent_ready = false;
    while !stop.load(Ordering::Relaxed) {
        // Keep only a short amount of sound queued so live faders and mute changes
        // reach the output promptly without restarting transport.
        while sink.len() >= 2 && !stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(4));
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if let Some(next) = take_mix_update(stop, active_stop, pending_project) {
            project = next;
            source = PlaybackAudioSource::new(&project, source.cache.clone());
            prefetch_at = cursor;
        }
        let end = project.sequence.duration_frames();
        if cursor >= end {
            if !sent_ready {
                mark_audio_ready(stop, active_stop, ready);
            }
            break;
        }
        let frames = playback_chunk_frames(project.sequence.frame_rate, end - cursor);
        let buffers = match source.windows(&project, cursor, frames, stop) {
            Ok(buffers) => buffers,
            Err(_) if stop.load(Ordering::Acquire) => break,
            Err(error) => return Err(error) };
        let (mixed, peaks) = audio::mix_window_range_metered(&project, cursor, frames, &buffers, stop)?;
        if !mixed.is_empty() {
            if !sent_ready {
                if !mark_audio_ready(stop, active_stop, ready) {
                    break;
                }
                sent_ready = true;
            }
            sink.append(meters::MeteredChunk::new(mixed, peaks, meters.clone(), stop.clone()));
        }
        // Decode upcoming windows off the output worker. Refresh twice per
        // second, looking two seconds ahead, while keeping startup synchronous
        // work limited to the first audible chunk.
        if cursor >= prefetch_at {
            let rate = project.sequence.frame_rate;
            let ahead = rate
                .seconds_to_frame(2.0)
                .max(1)
                .min(end.saturating_sub(cursor));
            source.prefetch(&project, cursor, ahead, stop);
            prefetch_at = cursor.saturating_add(rate.seconds_to_frame(0.5).max(1));
        }
        cursor += frames;
    }
    while !stop.load(Ordering::Relaxed) && !sink.empty() {
        thread::sleep(Duration::from_millis(20));
    }
    sink.stop();
    drop(stream);
    Ok(())
}

impl Default for PreviewEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Default for AudioEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.stop();
        {
            let (lock, changed) = &*self.mailbox;
            let mut mailbox = lock.lock().unwrap_or_else(|error| error.into_inner());
            mailbox.stopped = true;
            mailbox.request = None;
            changed.notify_one();
        }
        self.cache.shutdown();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

