//! Bounded filesystem discovery and metadata workers. No UI handles cross threads.
use std::collections::{HashMap, HashSet};
use std::fs::{self, ReadDir};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{ImportEvent, Media};

pub const MAX_ROOTS: usize = 1024;
const MAX_FILES: usize = 10_000;
const MAX_ENTRIES: usize = 50_000;
const MAX_DEPTH: usize = 32;
pub const BATCH_SIZE: usize = 32;
pub const REFRESH_INTERVAL: Duration = Duration::from_millis(150);

/// Filesystem presence checks must never run inside bin row construction.
/// A single bounded worker refreshes the UI-owned cache, including loaded/offline
/// projects. Successful imports seed it immediately; unknown paths are optimistic.
pub struct Availability {
    known: HashMap<PathBuf, bool>,
    rx: Option<Receiver<(PathBuf, bool)>>,
    cancel: Arc<AtomicBool>,
    revision: u64,
    checked_at: Instant,
    refreshed_at: Instant,
    dirty: bool,
}

impl Default for Availability {
    fn default() -> Self {
        Self {
            known: HashMap::new(), rx: None, cancel: Arc::new(AtomicBool::new(false)),
            revision: u64::MAX, checked_at: Instant::now() - Duration::from_secs(30),
            refreshed_at: Instant::now(), dirty: false,
        }
    }
}

impl Drop for Availability {
    fn drop(&mut self) { self.cancel.store(true, Ordering::Release); }
}

impl Availability {
    pub fn known(&self, path: &Path) -> Option<bool> { self.known.get(path).copied() }

    pub fn mark_present(&mut self, path: &Path) { self.known.insert(path.into(), true); }

    pub fn poll(&mut self, revision: u64, media: &[Media]) -> bool {
        if self.revision != revision { self.cancel.store(true, Ordering::Release); }
        for _ in 0..64 {
            let Some(rx) = &self.rx else { break; };
            match rx.try_recv() {
                Ok((path, present)) if self.revision == revision => {
                    if self.known.insert(path, present) != Some(present) { self.dirty = true; }
                }
                Ok(_) => {}
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => { self.rx = None; break; }
            }
        }
        if self.rx.is_none() && self.checked_at.elapsed() >= Duration::from_secs(2)
            && (self.revision != revision || self.checked_at.elapsed() >= Duration::from_secs(30))
        {
            let paths: Vec<_> = media.iter().map(|media| media.path.clone()).collect();
            let keep: HashSet<_> = paths.iter().collect();
            self.known.retain(|path, _| keep.contains(path));
            let (tx, rx) = mpsc::sync_channel(128);
            self.cancel = Arc::new(AtomicBool::new(false));
            let cancel = self.cancel.clone();
            self.rx = Some(rx);
            self.revision = revision;
            self.checked_at = Instant::now();
            std::thread::spawn(move || {
                for path in paths {
                    if cancel.load(Ordering::Acquire) { break; }
                    let present = path.is_file();
                    if !send(&tx, (path, present), &cancel) { break; }
                }
            });
        }
        if self.dirty && self.refreshed_at.elapsed() >= REFRESH_INTERVAL {
            self.dirty = false;
            self.refreshed_at = Instant::now();
            true
        } else { false }
    }
}

#[derive(Default)]
pub struct Progress {
    pub discovered: AtomicUsize,
    pub skipped: AtomicUsize,
    pub scanning: AtomicBool,
    pub limited: AtomicBool,
}

pub struct Job {
    pub cancel: Arc<AtomicBool>,
    pub progress: Arc<Progress>,
    // Lexical keys are sufficient for cheap UI enqueue checks. Workers also
    // canonicalize aliases before probing, without doing filesystem IO on UI.
    pub paths: Arc<Mutex<HashSet<PathBuf>>>,
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

/// Normalize lexical aliases without hitting a disk/network share on the UI.
pub fn path_key(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir if result.file_name().is_some_and(|name| name != "..") => {
                result.pop();
            }
            _ => result.push(part.as_os_str()),
        }
    }
    #[cfg(windows)]
    {
        // Canonical paths may carry the extended-length prefix; compare them
        // to ordinary picker/drop paths using Windows' case-insensitive spelling.
        let text = result.to_string_lossy().replace('/', "\\").to_lowercase();
        let text = if let Some(unc) = text.strip_prefix("\\\\?\\unc\\") {
            format!("\\\\{unc}")
        } else {
            text.strip_prefix("\\\\?\\").unwrap_or(&text).to_string()
        };
        PathBuf::from(text)
    }
    #[cfg(not(windows))]
    result
}

fn canonical_key(path: &Path) -> PathBuf {
    path_key(&fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
}

fn send<T>(tx: &SyncSender<T>, mut value: T, cancel: &AtomicBool) -> bool {
    loop {
        if cancel.load(Ordering::Acquire) {
            return false;
        }
        match tx.try_send(value) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(pending)) => {
                value = pending;
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

fn supported_media(path: &Path) -> bool {
    matches!(path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase().as_str(),
        "mp4" | "mov" | "mkv" | "avi" | "webm" | "m4v" | "mts" | "m2ts" | "ts"
        | "mpg" | "mpeg" | "mxf" | "wmv" | "wav" | "mp3" | "aac" | "flac"
        | "aiff" | "aif" | "m4a" | "ogg" | "opus" | "wma" | "png" | "jpg"
        | "jpeg" | "tif" | "tiff" | "bmp" | "webp" | "gif" | "tga" | "ppm")
}

fn link_or_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    false
}

pub fn start(paths: Vec<PathBuf>, existing: Vec<PathBuf>, bin: String) -> (Job, Receiver<ImportEvent>) {
    let cancel = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(Progress::default());
    progress.scanning.store(true, Ordering::Release);
    let active = Arc::new(Mutex::new(paths.iter().map(|p| path_key(p)).collect()));
    let job = Job { cancel: cancel.clone(), progress: progress.clone(), paths: active.clone() };
    let (tx, rx) = mpsc::sync_channel(64);
    std::thread::spawn(move || {
        let (work_tx, work_rx) = mpsc::sync_channel::<PathBuf>(32);
        let work_rx = Mutex::new(work_rx);
        let workers = std::thread::available_parallelism().map(|n| n.get() / 2).unwrap_or(2).clamp(1, 4);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                let (tx, cancel, bin, work_rx) = (&tx, &cancel, &bin, &work_rx);
                scope.spawn(move || loop {
                    if cancel.load(Ordering::Acquire) { break; }
                    // Hold the queue mutex only while receiving, never probing.
                    let path = work_rx.lock().unwrap_or_else(|e| e.into_inner()).recv();
                    let Ok(path) = path else { break; };
                    if cancel.load(Ordering::Acquire) { break; }
                    let event = match crate::playback::probe_path_cancellable(&path, cancel) {
                        Ok(mut media) => {
                            media.bin = bin.clone();
                            ImportEvent::Media(media)
                        }
                        Err(error) => ImportEvent::Error(error),
                    };
                    if !send(tx, event, cancel) { break; }
                });
            }
            let mut seen = HashSet::new();
            for path in existing {
                if cancel.load(Ordering::Acquire) { break; }
                seen.insert(canonical_key(&path));
            }
            let mut directories = HashSet::new();
            let mut entries = 0;
            // Depth-first traversal keeps at most MAX_DEPTH open directories.
            let mut stack: Vec<ReadDir> = Vec::new();
            let mut roots = paths.into_iter();
            loop {
                if cancel.load(Ordering::Acquire) { break; }
                if entries >= MAX_ENTRIES || progress.discovered.load(Ordering::Relaxed) >= MAX_FILES {
                    progress.limited.store(true, Ordering::Release);
                    break;
                }
                let (path, nested) = if let Some(dir) = stack.last_mut() {
                    match dir.next() {
                        Some(Ok(entry)) => (entry.path(), true),
                        Some(Err(error)) => {
                            entries += 1;
                            if !send(&tx, ImportEvent::DiscoveryError(error.to_string()), &cancel) { break; }
                            continue;
                        }
                        None => { stack.pop(); continue; }
                    }
                } else if let Some(path) = roots.next() {
                    (path, false)
                } else { break; };
                entries += 1;
                let metadata = match fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        if !send(&tx, ImportEvent::DiscoveryError(format!("{}: {error}", path.display())), &cancel) { break; }
                        continue;
                    }
                };
                // Includes Windows junctions. Avoid following links at all, so
                // a cycle cannot defeat the depth/entry/canonical-key bounds.
                if link_or_reparse(&metadata) {
                    progress.skipped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                if metadata.is_dir() {
                    if stack.len() >= MAX_DEPTH {
                        progress.limited.store(true, Ordering::Release);
                        continue;
                    }
                    if !directories.insert(canonical_key(&path)) { continue; }
                    match fs::read_dir(&path) {
                        Ok(dir) => stack.push(dir),
                        Err(error) => {
                            if !send(&tx, ImportEvent::DiscoveryError(format!("{}: {error}", path.display())), &cancel) { break; }
                        }
                    }
                } else if metadata.is_file() && (!nested || supported_media(&path)) {
                    let key = canonical_key(&path);
                    if !seen.insert(key.clone()) {
                        progress.skipped.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    {
                        let mut active = active.lock().unwrap_or_else(|e| e.into_inner());
                        active.insert(path_key(&path));
                        active.insert(key);
                    }
                    progress.discovered.fetch_add(1, Ordering::Release);
                    if !send(&work_tx, path, &cancel) { break; }
                }
            }
            progress.scanning.store(false, Ordering::Release);
            drop(work_tx);
        });
        // Done follows every worker result, even after cancellation. Dropping
        // the receiver on project replacement unblocks this send immediately.
        let _ = tx.send(ImportEvent::Done);
    });
    (job, rx)
}
