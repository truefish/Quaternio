//! Disposable, verified CFR preview segments. Original media is never modified.
use super::decode::{find_ffmpeg, find_ffprobe, fitted_size};
use crate::project::Media;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PreviewCacheSettings {
    pub automatic: bool,
    pub paused: bool,
    pub limit_gb: u32 }
impl Default for PreviewCacheSettings {
    fn default() -> Self {
        Self {
            automatic: true,
            paused: false,
            limit_gb: 10 }
    }
}

#[derive(Clone)]
pub struct PreparedFrame {
    pub path: PathBuf,
    pub frame: i64,
    pub width: u32,
    pub height: u32 }

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Segment {
    version: u32,
    key: String,
    source: PathBuf,
    source_bytes: u64,
    source_modified: u128,
    first: i64,
    count: i64,
    fps: f64,
    width: u32,
    height: u32,
    bytes: u64,
    used: u64 }

struct Observation {
    previous: i64,
    samples: VecDeque<bool>,
    slow_seeks: usize,
    needed: bool }
struct Job {
    media: Media,
    first: i64,
    key: String,
    attempts: u8 }
#[derive(Clone)]
struct Identity {
    media: Media,
    bytes: u64,
    modified: u128 }
impl Identity {
    fn matches(&self, media: &Media) -> bool {
        self.media.path == media.path
            && self.media.width == media.width
            && self.media.height == media.height
            && self.media.source_fps == media.source_fps
            && self.media.duration_secs == media.duration_secs
    }
    fn key(&self, first: i64) -> String {
        make_key(&self.media, first, self.bytes, self.modified)
    }
}
struct State {
    settings: PreviewCacheSettings,
    entries: HashMap<String, Segment>,
    certified: HashSet<String>,
    certify_queue: VecDeque<String>,
    observations: HashMap<PathBuf, Observation>,
    queue: VecDeque<Job>,
    rejected: HashSet<String>,
    identities: HashMap<PathBuf, Identity>,
    pending_media: VecDeque<(Media, u64)>,
    registrations: HashMap<PathBuf, (u64, Media)>,
    next_registration: u64,
    invalid: HashSet<String>,
    busy_until: Instant,
    active: Option<Arc<AtomicBool>>,
    active_key: Option<String>,
    active_source: Option<PathBuf>,
    message: String,
    clear: bool,
    stopped: bool }
struct Shared {
    root: PathBuf,
    state: Mutex<State>,
    changed: Condvar,
    foreground_at: AtomicU64,
    interrupt: Arc<AtomicBool>,
    timing: Arc<super::native_preview::PreviewTimingService> }
pub struct OptimizedCache {
    shared: Arc<Shared>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>> }

fn stamp(path: &Path) -> Option<(u64, u128)> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((
        metadata.len(),
        metadata
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos(),
    ))
}
fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn key(media: &Media, first: i64) -> Option<String> {
    let (bytes, modified) = stamp(&media.path)?;
    Some(make_key(media, first, bytes, modified))
}
fn make_key(media: &Media, first: i64, bytes: u64, modified: u128) -> String {
    let source = format!(
        "{VERSION}|{}|{bytes}|{modified}|{}|{}|{}|{first}|mjpeg-q3-960x540",
        media.path.display(),
        media.source_fps,
        media.width,
        media.height
    );
    format!("{:x}", Sha256::digest(source.as_bytes()))
}

impl OptimizedCache {
    pub fn new(root: PathBuf, mut settings: PreviewCacheSettings) -> Arc<Self> {
        settings.limit_gb = settings.limit_gb.clamp(1, 100);
        let shared = Arc::new(Shared {
            root,
            state: Mutex::new(State {
                settings,
                entries: HashMap::new(),
                certified: HashSet::new(),
                certify_queue: VecDeque::new(),
                observations: HashMap::new(),
                queue: VecDeque::new(),
                rejected: HashSet::new(),
                busy_until: Instant::now(),
                identities: HashMap::new(),
                pending_media: VecDeque::new(),
                registrations: HashMap::new(),
                next_registration: 0,
                invalid: HashSet::new(),
                active: None,
                active_key: None,
                active_source: None,
                message: "Previews ready when needed".into(),
                clear: false,
                stopped: false }),
            changed: Condvar::new(),
            foreground_at: AtomicU64::new(0),
            interrupt: Arc::new(AtomicBool::new(false)),
            timing: super::native_preview::PreviewTimingService::shared() });
        let work = shared.clone();
        let worker = std::thread::spawn(move || cache_loop(work));
        Arc::new(Self {
            shared,
            worker: Mutex::new(Some(worker)) })
    }

    pub fn configure(&self, mut settings: PreviewCacheSettings) {
        settings.limit_gb = settings.limit_gb.clamp(1, 100);
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if settings.paused || !settings.automatic {
            if let Some(active) = &state.active {
                active.store(true, Ordering::Release);
            }
        }
        state.settings = settings;
        self.shared.changed.notify_one();
    }

    /// A short idle grace prevents the preparation worker competing with gestures.
    pub fn set_busy(&self, busy: bool) {
        if !busy {
            return;
        }
        self.shared
            .foreground_at
            .store(now_seconds(), Ordering::Release);
        self.shared.interrupt.store(true, Ordering::Release);
    }

    pub fn clear(&self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.clear = true;
        state.queue.clear();
        state.certify_queue.clear();
        state.certified.clear();
        state.observations.clear();
        if let Some(active) = &state.active {
            active.store(true, Ordering::Release);
        }
        self.shared.changed.notify_one();
    }

    pub fn status(&self) -> String {
        let Ok(state) = self.shared.state.try_lock() else {
            return "Preview cache working…".into();
        };
        let bytes: u64 = state.entries.values().map(|s| s.bytes).sum();
        let mode = if state.clear {
            "Clearing previews · waiting for files in use"
        } else if state.settings.paused {
            "Preparation paused"
        } else if !state.settings.automatic {
            "Automatic previews off"
        } else {
            &state.message
        };
        format!(
            "{mode} · {:.1} / {} GB",
            bytes as f64 / 1e9,
            state.settings.limit_gb
        )
    }

    /// Caller has measured the original stamp; this check performs no filesystem I/O.
    pub fn lookup_verified(
        &self,
        media: &Media,
        ordinal: i64,
        expected_bytes: u64,
        expected_modified: u128,
    ) -> Option<PreparedFrame> {
        {
            let Ok(mut state) = self.shared.state.try_lock() else {
                return None;
            };
            let identity = state.identities.get(&media.path)?;
            if !identity.matches(media)
                || (identity.bytes, identity.modified) != (expected_bytes, expected_modified)
            {
                queue_registration(&mut state, media.clone(), true);
                let keys: Vec<_> = state
                    .entries
                    .values()
                    .filter(|s| s.source == media.path)
                    .map(|s| s.key.clone())
                    .collect();
                state.invalid.extend(keys);
                state.observations.remove(&media.path);
                self.shared.changed.notify_one();
                return None;
            }
        }
        self.lookup_inner(media, ordinal, Some((expected_bytes, expected_modified)))
    }

    pub fn lookup(&self, media: &Media, ordinal: i64) -> Option<PreparedFrame> {
        self.lookup_inner(media, ordinal, None)
    }
    fn lookup_inner(
        &self,
        media: &Media,
        ordinal: i64,
        expected: Option<(u64, u128)>,
    ) -> Option<PreparedFrame> {
        if !media.source_fps.is_finite() || media.source_fps <= 1.0 {
            return None;
        }
        let span = (media.source_fps * 10.0).ceil() as i64;
        let first = ordinal.max(0) / span * span;
        let Ok(mut state) = self.shared.state.try_lock() else {
            return None;
        };
        let identity = state
            .identities
            .get(&media.path)
            .filter(|id| id.matches(media))?;
        if expected.is_some_and(|stamp| stamp != (identity.bytes, identity.modified)) {
            return None;
        }
        let key = identity.key(first);
        if state.invalid.contains(&key) || state.clear {
            return None;
        }
        if !state.certified.contains(&key) {
            if state.entries.contains_key(&key)
                && !state.rejected.contains(&key)
                && !state.certify_queue.contains(&key)
                && state.certify_queue.len() < 4
            {
                state.certify_queue.push_back(key);
                self.shared.changed.notify_one();
            }
            return None;
        }
        let segment = state.entries.get_mut(&key)?;
        if ordinal < segment.first || ordinal >= segment.first + segment.count {
            return None;
        }
        segment.used = now_seconds();
        Some(PreparedFrame {
            path: self.shared.root.join(format!("{key}.mkv")),
            frame: ordinal - segment.first,
            width: segment.width,
            height: segment.height })
    }

    pub fn invalidate(&self, path: &Path) {
        let Some(key) = path.file_stem().and_then(|k| k.to_str()) else {
            return;
        };
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.invalid.insert(key.to_string());
        state.rejected.insert(key.to_string());
        self.shared.changed.notify_one();
    }

    pub fn register_media(&self, media: &[Media]) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        for media in media.iter().filter(|m| m.has_video) {
            if state
                .identities
                .get(&media.path)
                .is_some_and(|id| id.matches(media))
            {
                continue;
            }
            queue_registration(&mut state, media.clone(), false);
        }
        self.shared.changed.notify_one();
    }

    pub fn invalidate_source(&self, path: &Path) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let media = state
            .identities
            .remove(path)
            .map(|id| id.media)
            .or_else(|| {
                state
                    .registrations
                    .get(path)
                    .map(|(_, media)| media.clone())
            });
        cancel_source_work(&mut state, path);
        if let Some(media) = media {
            queue_registration(&mut state, media, true);
        }
        let keys: Vec<_> = state
            .entries
            .values()
            .filter(|s| s.source == path)
            .map(|s| s.key.clone())
            .collect();
        state.invalid.extend(keys);
        state.observations.remove(path);
        self.shared.changed.notify_one();
    }

    pub fn observe(&self, media: &Media, ordinal: i64, elapsed: Duration) {
        if !media.has_video
            || !media.source_fps.is_finite()
            || media.source_fps <= 1.0
            || media.source_fps > 120.0
        {
            return;
        }
        let Ok(mut state) = self.shared.state.try_lock() else {
            return;
        };
        if !state.settings.automatic || state.settings.paused || state.clear {
            return;
        }
        let Some(observed) = state.observations.get_mut(&media.path) else {
            if state.observations.len() >= 256 {
                state.observations.clear();
            }
            state.observations.insert(
                media.path.clone(),
                Observation {
                    previous: ordinal,
                    samples: VecDeque::new(),
                    slow_seeks: 0,
                    needed: false },
            );
            return; // Initial decoder startup must not trigger preparation.
        };
        let distance = ordinal.saturating_sub(observed.previous);
        if distance > 0 && distance <= 2 {
            observed
                .samples
                .push_back(elapsed.as_secs_f64() > 1.0 / media.source_fps);
            while observed.samples.len() > 60 {
                observed.samples.pop_front();
            }
        } else if distance != 0 && elapsed > Duration::from_millis(200) {
            observed.slow_seeks += 1;
        }
        observed.previous = ordinal;
        observed.needed |= observed.slow_seeks >= 3
            || observed.samples.len() >= 30
                && observed.samples.iter().filter(|miss| **miss).count() * 10
                    > observed.samples.len();
        if !observed.needed {
            return;
        }
        let span = (media.source_fps * 10.0).ceil() as i64;
        let first = ordinal.max(0) / span * span;
        let Some(identity) = state
            .identities
            .get(&media.path)
            .filter(|id| id.matches(media))
            .cloned()
        else {
            return;
        };
        // Nearby timeline footage wins; the queue itself cannot grow without bound.
        for first in [first.saturating_sub(span), first + span, first] {
            if first < 0 || first as f64 >= media.duration_secs * media.source_fps {
                continue;
            }
            let key = identity.key(first);
            if state.entries.contains_key(&key)
                || state.rejected.contains(&key)
                || state.active_key.as_ref() == Some(&key)
                || state.queue.iter().any(|j| j.key == key)
            {
                continue;
            }
            if state.queue.len() >= 12 {
                state.queue.pop_back();
            }
            state.queue.push_front(Job {
                media: media.clone(),
                first,
                key,
                attempts: 0 });
        }
        self.shared.changed.notify_one();
    }
}

impl Drop for OptimizedCache {
    fn drop(&mut self) {
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.stopped = true;
            if let Some(active) = &state.active {
                active.store(true, Ordering::Release);
            }
            self.shared.changed.notify_one();
        }
        if let Some(worker) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = worker.join();
        }
    }
}

// Old catalogue entries remain on disk but unavailable until requested and
// certified. Never scan the entire cache or delete it because playback is busy.
fn certify_loaded(shared: &Shared) -> bool {
    let segment = {
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.stopped
            || state.clear
            || state.settings.paused
            || Instant::now() < state.busy_until
            || now_seconds().saturating_sub(shared.foreground_at.load(Ordering::Acquire)) < 2
        {
            return false;
        }
        let Some(key) = state.certify_queue.pop_front() else {
            return false;
        };
        let Some(segment) = state.entries.get(&key).cloned() else {
            return true;
        };
        if state.invalid.contains(&key) {
            return true;
        }
        shared.interrupt.store(false, Ordering::Release);
        if now_seconds().saturating_sub(shared.foreground_at.load(Ordering::Acquire)) < 2 {
            shared.interrupt.store(true, Ordering::Release);
        }
        state.active = Some(shared.interrupt.clone());
        state.active_key = Some(key);
        state.active_source = Some(segment.source.clone());
        segment
    };
    let path = shared.root.join(format!("{}.mkv", segment.key));
    let result = shared.timing.certify_prepared(
        &path,
        segment.fps,
        segment.count as usize,
        &shared.interrupt,
    );
    let identity = stamp(&segment.source) == Some((segment.source_bytes, segment.source_modified));
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    state.active = None;
    state.active_key = None;
    state.active_source = None;
    if state.stopped || state.clear || state.invalid.contains(&segment.key) {
        return true;
    }
    if result.is_ok() && identity && !shared.interrupt.load(Ordering::Acquire) {
        state.certified.insert(segment.key);
    } else if shared.interrupt.load(Ordering::Acquire)
        || result
            .as_ref()
            .err()
            .is_some_and(|e| e.contains("interrupted"))
    {
        state.certify_queue.push_back(segment.key);
        state.busy_until = Instant::now() + Duration::from_secs(2);
    } else {
        state.rejected.insert(segment.key);
    }
    true
}

fn cache_loop(shared: Arc<Shared>) {
    if let Err(error) = std::fs::create_dir_all(&shared.root) {
        shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .message = format!("Preview cache unavailable: {error}");
        return;
    }
    load_entries(&shared);
    loop {
        if certify_loaded(&shared) {
            continue;
        }
        let pending = shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending_media
            .pop_front();
        if let Some((media, generation)) = pending {
            // Source metadata can involve slow storage. Never hold the shared
            // lock while checking it, and never do it in the preview hot path.
            let fingerprint = stamp(&media.path);
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            install_identity(&mut state, media, generation, fingerprint);
            continue;
        }
        let (job, cancel) = {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if state.stopped {
                    return;
                }
                if !state.certify_queue.is_empty()
                    && !state.clear
                    && !state.settings.paused
                    && Instant::now() >= state.busy_until
                    && now_seconds().saturating_sub(shared.foreground_at.load(Ordering::Acquire))
                        >= 2
                {
                    break (None, Arc::new(AtomicBool::new(false)));
                }
                if !state.pending_media.is_empty() {
                    break (None, Arc::new(AtomicBool::new(false)));
                }
                process_deletions(&shared.root, &mut state);
                evict(&shared.root, &mut state, 0);
                if !state.clear
                    && state.settings.automatic
                    && !state.settings.paused
                    && Instant::now() >= state.busy_until
                    && now_seconds().saturating_sub(shared.foreground_at.load(Ordering::Acquire))
                        >= 2
                {
                    if let Some(job) = state.queue.pop_front() {
                        let cancel = shared.interrupt.clone();
                        cancel.store(false, Ordering::Release);
                        // Do not miss a foreground gesture racing the reset.
                        if now_seconds()
                            .saturating_sub(shared.foreground_at.load(Ordering::Acquire))
                            < 2
                        {
                            cancel.store(true, Ordering::Release);
                        }
                        state.active = Some(cancel.clone());
                        state.active_key = Some(job.key.clone());
                        state.active_source = Some(job.media.path.clone());
                        state.message = format!("Preparing {}", job.media.name);
                        break (Some(job), cancel);
                    }
                }
                state = shared
                    .changed
                    .wait_timeout(state, Duration::from_millis(250))
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        };
        let Some(mut job) = job else {
            continue;
        };
        let allowance = {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            let (w, h) = fitted_size(job.media.width, job.media.height, 960, 540);
            let reserve = (w as u64 * h as u64 * 3 + 4096)
                .saturating_mul((job.media.source_fps * 10.0).ceil() as u64);
            evict(&shared.root, &mut state, reserve);
            (state.settings.limit_gb as u64 * 1_000_000_000)
                .saturating_sub(managed_disk_bytes(&shared.root))
        };
        let result = if allowance < 1_000_000 {
            Err("Preview cache limit reached".into())
        } else {
            prepare(&shared.root, &job, &cancel, allowance)
        };
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active = None;
        state.active_key = None;
        state.active_source = None;
        if cancel.load(Ordering::Acquire) {
            let superseded = state.invalid.contains(&job.key);
            if !remove_segment(&shared.root, &job.key) {
                state.invalid.insert(job.key.clone());
            }
            if !state.clear && !state.stopped && !superseded {
                state.queue.push_front(job);
            }
            state.message = "Preparation waiting for idle".into();
        } else {
            match result {
                Ok(segment) => {
                    evict(&shared.root, &mut state, segment.bytes);
                    let bytes: u64 = state.entries.values().map(|s| s.bytes).sum();
                    if bytes.saturating_add(segment.bytes)
                        <= state.settings.limit_gb as u64 * 1_000_000_000
                    {
                        state.certified.insert(segment.key.clone());
                        state.entries.insert(segment.key.clone(), segment);
                        state.message = "Optimized previews ready".into();
                    } else {
                        remove_segment(&shared.root, &segment.key);
                        state.message = "Preview cache limit reached".into();
                    }
                }
                Err(error) => {
                    let unsupported = error.starts_with("Unsupported:");
                    if unsupported || job.attempts >= 2 {
                        if state.rejected.len() >= 1024 {
                            state.rejected.clear();
                        }
                        state.rejected.insert(job.key);
                    } else {
                        job.attempts += 1;
                        state.queue.push_back(job);
                        state.busy_until = Instant::now() + Duration::from_secs(15);
                    }
                    state.message = format!("Using originals · {error}");
                }
            }
        }
        drop(state);
    }
}

fn same_registration(a: &Media, b: &Media) -> bool {
    a.path == b.path
        && a.width == b.width
        && a.height == b.height
        && a.source_fps.to_bits() == b.source_fps.to_bits()
        && a.duration_secs.to_bits() == b.duration_secs.to_bits()
        && a.has_video == b.has_video
}
fn cancel_source_work(state: &mut State, path: &Path) {
    state.queue.retain(|job| job.media.path != path);
    if state.active_source.as_deref() == Some(path) {
        if let Some(cancel) = &state.active {
            cancel.store(true, Ordering::Release);
        }
        if let Some(key) = &state.active_key {
            state.invalid.insert(key.clone());
        }
    }
}
fn queue_registration(state: &mut State, media: Media, force: bool) {
    if !force
        && state
            .registrations
            .get(&media.path)
            .is_some_and(|(_, old)| same_registration(old, &media))
    {
        return;
    }
    cancel_source_work(state, &media.path);
    state.identities.remove(&media.path);
    state
        .pending_media
        .retain(|(old, _)| old.path != media.path);
    state.next_registration = state.next_registration.wrapping_add(1);
    let generation = state.next_registration;
    if state.pending_media.len() >= 256 {
        if let Some((old, old_generation)) = state.pending_media.pop_front() {
            if state
                .registrations
                .get(&old.path)
                .is_some_and(|(current, _)| *current == old_generation)
            {
                state.registrations.remove(&old.path);
            }
        }
    }
    state
        .registrations
        .insert(media.path.clone(), (generation, media.clone()));
    state.pending_media.push_back((media, generation));
}
fn install_identity(
    state: &mut State,
    media: Media,
    generation: u64,
    fingerprint: Option<(u64, u128)>,
) -> bool {
    if state.stopped
        || !state
            .registrations
            .get(&media.path)
            .is_some_and(|(current, _)| *current == generation)
    {
        return false;
    }
    state.registrations.remove(&media.path);
    let Some((bytes, modified)) = fingerprint else {
        return false;
    };
    if state.identities.len() >= 256 {
        state.identities.clear();
    }
    state.identities.insert(
        media.path.clone(),
        Identity {
            media,
            bytes,
            modified },
    );
    true
}
fn process_deletions(root: &Path, state: &mut State) {
    if state.clear {
        state.invalid.extend(state.entries.keys().cloned());
    }
    let invalid: Vec<_> = state.invalid.iter().cloned().collect();
    for key in invalid {
        if remove_segment(root, &key) {
            state.entries.remove(&key);
            state.invalid.remove(&key);
        }
    }
    state
        .certified
        .retain(|key| state.entries.contains_key(key));
    state
        .certify_queue
        .retain(|key| state.entries.contains_key(key));
    if state.clear {
        if state.entries.is_empty() && state.invalid.is_empty() {
            state.rejected.clear();
            state.clear = false;
            state.message = "Preview cache cleared".into();
        } else {
            state.message = "Clearing previews · waiting for files in use".into();
        }
    }
}
fn verify_publication_source(
    source: &Path,
    expected: (u64, u128),
    cancel: &AtomicBool,
) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        return Err("Preparation interrupted".into());
    }
    if stamp(source) != Some(expected) {
        return Err("Source changed during preparation".into());
    }
    // Cancellation may have arrived during a slow metadata read.
    if cancel.load(Ordering::Acquire) {
        return Err("Preparation interrupted".into());
    }
    Ok(())
}

fn remove_segment(root: &Path, key: &str) -> bool {
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    // Remove the catalogue first so an interrupted invalidation cannot revive it.
    for ext in ["json", "mkv"] {
        if let Err(error) = std::fs::remove_file(root.join(format!("{key}.{ext}"))) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return false;
            }
        }
    }
    true
}
fn evict(root: &Path, state: &mut State, incoming: u64) {
    let limit = state.settings.limit_gb as u64 * 1_000_000_000;
    let mut bytes: u64 = state.entries.values().map(|s| s.bytes).sum();
    let mut pinned = HashSet::new();
    while bytes.saturating_add(incoming) > limit {
        let oldest = state
            .entries
            .values()
            .filter(|s| now_seconds().saturating_sub(s.used) > 2 && !pinned.contains(&s.key))
            .min_by_key(|s| s.used)
            .map(|s| s.key.clone());
        let Some(key) = oldest else {
            break;
        };
        if remove_segment(root, &key) {
            if let Some(segment) = state.entries.remove(&key) {
                bytes = bytes.saturating_sub(segment.bytes);
            }
        } else {
            pinned.insert(key);
        }
    }
}
fn load_entries(shared: &Shared) {
    let Ok(files) = std::fs::read_dir(&shared.root) else {
        return;
    };
    let mut entries = HashMap::new();
    for entry in files.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(segment) = serde_json::from_slice::<Segment>(&bytes) else {
            continue;
        };
        if segment.version == VERSION
            && segment.key.len() == 64
            && segment.key.bytes().all(|b| b.is_ascii_hexdigit())
            && stamp(&segment.source) == Some((segment.source_bytes, segment.source_modified))
            && std::fs::metadata(shared.root.join(format!("{}.mkv", segment.key)))
                .is_ok_and(|m| m.len() == segment.bytes)
        {
            entries.insert(segment.key.clone(), segment);
        } else {
            remove_segment(&shared.root, &segment.key);
        }
    }
    // Remove only this cache's named incomplete/orphan outputs after a crash.
    if let Ok(files) = std::fs::read_dir(&shared.root) {
        for entry in files.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some((key, suffix)) = name.split_once('.') else {
                continue;
            };
            if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            if matches!(suffix, "partial.mkv" | "partial.json")
                || suffix == "mkv" && !entries.contains_key(key)
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    shared
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entries = entries;
}

fn managed_disk_bytes(root: &Path) -> u64 {
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

fn run(tool: &Path, args: &[String], cancel: &AtomicBool) -> Result<Vec<u8>, String> {
    if cancel.load(Ordering::Acquire) {
        return Err("Preparation interrupted".into());
    }
    let mut command = Command::new(tool);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000 | 0x00004000); // hidden, below normal
    }
    let mut child = command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let stdout = child.stdout.take().ok_or("Missing preparation output")?;
    let stderr = child
        .stderr
        .take()
        .ok_or("Missing preparation diagnostics")?;
    std::thread::scope(|scope| {
        let out = scope.spawn(move || {
            let mut b = Vec::new();
            stdout.take(8 * 1024 * 1024).read_to_end(&mut b).map(|_| b)
        });
        let err = scope.spawn(move || {
            let mut b = Vec::new();
            stderr.take(64 * 1024).read_to_end(&mut b).map(|_| b)
        });
        let started = Instant::now();
        let status = loop {
            if cancel.load(Ordering::Acquire) || started.elapsed() > Duration::from_secs(90) {
                let _ = child.kill();
            }
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(error.to_string());
                }
            }
        };
        let output = out
            .join()
            .map_err(|_| "Preparation reader failed")?
            .map_err(|e| e.to_string())?;
        let errors = err
            .join()
            .map_err(|_| "Preparation reader failed")?
            .map_err(|e| e.to_string())?;
        if cancel.load(Ordering::Acquire) {
            return Err("Preparation interrupted".into());
        }
        if !status?.success() {
            return Err(String::from_utf8_lossy(&errors).chars().take(180).collect());
        }
        Ok(output)
    })
}

fn number(value: &serde_json::Value) -> Option<f64> {
    value.as_f64().or_else(|| value.as_str()?.parse().ok())
}
fn rate(value: &serde_json::Value) -> Option<f64> {
    let (n, d) = value.as_str()?.split_once('/')?;
    let result = n.parse::<f64>().ok()? / d.parse::<f64>().ok()?;
    result.is_finite().then_some(result)
}

fn prepare(root: &Path, job: &Job, cancel: &AtomicBool, allowance: u64) -> Result<Segment, String> {
    let media = &job.media;
    let (source_bytes, source_modified) = stamp(&media.path).ok_or("Source is unavailable")?;
    if key(media, job.first).as_deref() != Some(&job.key) {
        return Err("Source changed".into());
    }
    let fps = media.source_fps;
    let span = (fps * 10.0).ceil() as i64;
    let count = span.min((media.duration_secs * fps - 1e-7).ceil() as i64 - job.first);
    if count <= 0 {
        return Err("No pictures in preview range".into());
    }
    let probe = find_ffprobe()?;
    // Inspect original timestamps around this range. VFR, missing timestamps,
    // rotation, interlacing or HDR remain on originals rather than guessed maps.
    let interval = format!(
        "{:.9}%+{:.9}",
        job.first as f64 / fps,
        count as f64 / fps + 2.0
    );
    let json = run(&probe, &[
        "-v".into(), "error".into(), "-select_streams".into(), "v:0".into(),
        "-read_intervals".into(), interval, "-show_frames".into(), "-show_streams".into(),
        "-show_entries".into(), "stream=start_time,r_frame_rate,avg_frame_rate,color_transfer:stream_side_data_list:frame=best_effort_timestamp_time,interlaced_frame".into(),
        "-of".into(), "json".into(), media.path.display().to_string(),
    ], cancel)?;
    let document: serde_json::Value = serde_json::from_slice(&json).map_err(|e| e.to_string())?;
    let stream = document["streams"]
        .as_array()
        .and_then(|s| s.first())
        .ok_or("No video stream")?;
    if rate(&stream["r_frame_rate"]).is_none_or(|r| (r - fps).abs() > 1e-5)
        || rate(&stream["avg_frame_rate"]).is_none_or(|r| (r - fps).abs() > 1e-5)
        || matches!(
            stream["color_transfer"].as_str(),
            Some("smpte2084" | "arib-std-b67")
        )
        || stream["side_data_list"].as_array().is_some_and(|rows| {
            rows.iter()
                .any(|r| number(&r["rotation"]).is_some_and(|r| r.abs() > 0.01))
        })
    {
        return Err("Unsupported: this source needs original decoding".into());
    }
    let origin = number(&stream["start_time"]).unwrap_or(0.0);
    let frames = document["frames"]
        .as_array()
        .ok_or("Missing original frame timestamps")?;
    let mut ordinals = Vec::new();
    for frame in frames {
        if frame["interlaced_frame"].as_i64().unwrap_or(0) != 0 {
            return Err("Unsupported: interlaced source uses originals".into());
        }
        let position = (number(&frame["best_effort_timestamp_time"])
            .ok_or("Missing frame timestamp")?
            - origin)
            * fps;
        let ordinal = position.round() as i64;
        if ordinal >= job.first && ordinal < job.first + count {
            if (position - ordinal as f64).abs() > 0.02 {
                return Err("Unsupported: variable frame timing uses originals".into());
            }
            ordinals.push(ordinal);
        }
    }
    if ordinals.len() != count as usize
        || ordinals
            .iter()
            .enumerate()
            .any(|(i, n)| *n != job.first + i as i64)
    {
        return Err("Unsupported: preview frame mapping could not be verified".into());
    }
    let (width, height) = fitted_size(
        media.width,
        media.height,
        media.width.min(960),
        media.height.min(540),
    );
    let first_decode = job.first.saturating_sub(2).max(0);
    let seek = if first_decode == 0 {
        0.0
    } else {
        (first_decode as f64 - 0.5) / fps
    };
    let discard = job.first - first_decode;
    let partial = root.join(format!("{}.partial.mkv", job.key));
    let final_path = root.join(format!("{}.mkv", job.key));
    let result = (|| {
        let ffmpeg = find_ffmpeg()?;
        let timing = crate::media_engine::passthrough_args(&ffmpeg)?;
        run(&ffmpeg, &[
            "-y".into(), "-nostdin".into(), "-v".into(), "error".into(), "-xerror".into(),
            "-filter_threads".into(), "1".into(), "-ss".into(), format!("{seek:.12}"),
            "-threads".into(), "2".into(), "-i".into(),media.path.display().to_string(),
            "-map".into(),"0:v:0".into(),"-an".into(),"-sn".into(),"-dn".into(),
            "-vf".into(),format!("trim=start_frame={discard}:end_frame={},setpts=PTS-STARTPTS,scale={width}:{height}:flags=bilinear:out_color_matrix=bt709:out_range=full",discard+count),
            "-c:v".into(),"mjpeg".into(),"-q:v".into(),"3".into(),"-threads".into(),"2".into(),
            "-pix_fmt".into(),"yuvj444p".into(),"-color_range".into(),"pc".into(),"-colorspace".into(),"bt709".into(),
            "-fs".into(),allowance.to_string(),
            timing[0].into(),timing[1].into(),"-f".into(),"matroska".into(),partial.display().to_string()
        ],cancel)?;
        let check = run(
            &probe,
            &[
                "-v".into(),
                "error".into(),
                "-select_streams".into(),
                "v:0".into(),
                "-count_frames".into(),
                "-show_entries".into(),
                "stream=nb_read_frames,width,height,codec_name".into(),
                "-of".into(),
                "json".into(),
                partial.display().to_string(),
            ],
            cancel,
        )?;
        let check: serde_json::Value = serde_json::from_slice(&check).map_err(|e| e.to_string())?;
        let stream = &check["streams"][0];
        if number(&stream["nb_read_frames"]) != Some(count as f64)
            || stream["width"].as_u64() != Some(width as u64)
            || stream["height"].as_u64() != Some(height as u64)
            || stream["codec_name"] != "mjpeg"
        {
            return Err("Prepared segment validation failed".into());
        }
        if cancel.load(Ordering::Acquire)
            || stamp(&media.path) != Some((source_bytes, source_modified))
        {
            return Err("Source changed during preparation".into());
        }
        let bytes = std::fs::metadata(&partial)
            .map_err(|e| e.to_string())?
            .len();
        if bytes > allowance {
            return Err("Preview cache limit reached".into());
        }
        for offset in [0, count - 1] {
            let original = picture(&media.path, job.first + offset, fps, width, height, cancel)?;
            let prepared = picture(&partial, offset, fps, width, height, cancel)?;
            if original.len() != prepared.len() || original.is_empty() {
                return Err("Preview picture verification failed".into());
            }
            let difference: u64 = original
                .iter()
                .zip(&prepared)
                .map(|(a, b)| a.abs_diff(*b) as u64)
                .sum();
            if difference as f64 / original.len() as f64 > 12.0 {
                return Err("Unsupported: prepared picture differs from original".into());
            }
        }
        let segment = Segment {
            version: VERSION,
            key: job.key.clone(),
            source: media.path.clone(),
            source_bytes,
            source_modified,
            first: job.first,
            count,
            fps,
            width,
            height,
            bytes,
            used: now_seconds() };
        verify_publication_source(&media.path, (source_bytes, source_modified), cancel)?;
        std::fs::rename(&partial, &final_path).map_err(|e| e.to_string())?;
        super::native_preview::PreviewTimingService::shared().certify_prepared(
            &final_path,
            fps,
            count as usize,
            cancel,
        )?;
        let meta = root.join(format!("{}.json", job.key));
        let stage = root.join(format!("{}.partial.json", job.key));
        std::fs::write(
            &stage,
            serde_json::to_vec(&segment).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        verify_publication_source(&media.path, (source_bytes, source_modified), cancel)?;
        std::fs::rename(stage, meta).map_err(|e| e.to_string())?;
        Ok(segment)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&partial);
        let _ = std::fs::remove_file(root.join(format!("{}.partial.json", job.key)));
        remove_segment(root, &job.key);
    }
    result
}

fn picture(
    path: &Path,
    ordinal: i64,
    fps: f64,
    width: u32,
    height: u32,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, String> {
    let first = ordinal.saturating_sub(2).max(0);
    let seek = if first == 0 {
        0.0
    } else {
        (first as f64 - 0.5) / fps
    };
    run(
        &find_ffmpeg()?,
        &[
            "-nostdin".into(),
            "-v".into(),
            "error".into(),
            "-filter_threads".into(),
            "1".into(),
            "-ss".into(),
            format!("{seek:.12}"),
            "-threads".into(),
            "2".into(),
            "-i".into(),
            path.display().to_string(),
            "-an".into(),
            "-sn".into(),
            "-vf".into(),
            format!(
                "select=eq(n\\,{}),scale={width}:{height}:flags=bilinear",
                ordinal - first
            ),
            "-frames:v".into(),
            "1".into(),
            "-pix_fmt".into(),
            "rgb24".into(),
            "-threads".into(),
            "1".into(),
            "-f".into(),
            "rawvideo".into(),
            "pipe:1".into(),
        ],
        cancel,
    )
}

