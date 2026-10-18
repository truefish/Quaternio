//! Packaged-launch crash recovery. Each instance owns an exclusive lease;
//! checkpoints reuse the editor's validated session format and atomic publication.
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use fs2::FileExt;
use crate::timeline::{Editor, EditorSession};

const MAX_CHECKPOINT: u64 = 128 * 1024 * 1024;
const QUIET: Duration = Duration::from_secs(5);
const PERIOD: Duration = Duration::from_secs(30);

pub struct Recovery {
    dir: PathBuf,
    lease: Option<File>,
    observed: Option<(u64, Option<PathBuf>, bool)>,
    saved: Option<(u64, Option<PathBuf>, bool)>,
    changed: Instant,
    written: Instant,
    work: Option<(JoinHandle<()>, Receiver<Result<(), String>>)>,
    cleaned: bool,
}

pub struct Candidate {
    pub path: PathBuf,
    pub session: EditorSession,
    lease: Option<File>,
    dir: PathBuf,
}
impl Candidate {
    pub fn dismiss(&self) -> Result<(), String> {
        fs::write(self.dir.join("clean"), b"dismissed").map_err(|e| e.to_string())
    }
}

fn root() -> PathBuf { crate::preferences::data_dir().join("recovery") }
fn identity() -> String {
    format!("{}-{}", std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos())
}
fn lease(dir: &Path) -> Result<File, String> {
    let file = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(dir.join("lease")).map_err(|e| e.to_string())?;
    file.try_lock_exclusive().map_err(|e| e.to_string())?;
    Ok(file)
}

pub fn candidates() -> Vec<Candidate> { candidates_in(&root()) }
fn candidates_in(root: &Path) -> Vec<Candidate> {
    let mut result = Vec::new();
    let Ok(entries) = fs::read_dir(root) else { return result; };
    for entry in entries.flatten().filter(|entry| !entry.path().join("clean").exists()).take(256) {
        let dir = entry.path();
        if !entry.file_type().is_ok_and(|t| t.is_dir()) || dir.join("clean").exists() { continue; }
        let Ok(lease) = lease(&dir) else { continue; }; // A live instance owns it.
        let mut snapshots: Vec<_> = fs::read_dir(&dir).into_iter().flatten().flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()) && e.path().extension().is_some_and(|e| e == "json"))
            .map(|e| e.path()).collect();
        snapshots.sort();
        for path in snapshots.into_iter().rev() {
            let Ok(session) = read(&path) else { continue; };
            result.push(Candidate { path, session, lease: Some(lease), dir });
            break;
        }
    }
    result.sort_by(|a,b| b.path.file_name().cmp(&a.path.file_name()));
    result
}

fn read(path: &Path) -> Result<EditorSession, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    if file.metadata().map_err(|e| e.to_string())?.len() > MAX_CHECKPOINT { return Err("Recovery checkpoint exceeds size limit".into()); }
    let mut bytes = Vec::new(); file.take(MAX_CHECKPOINT + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_CHECKPOINT { return Err("Recovery checkpoint exceeds size limit".into()); }
    let session: EditorSession = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    session.validate()?; Ok(session)
}

fn write(dir: &Path, session: &EditorSession) -> Result<(), String> {
    session.validate()?;
    let bytes = serde_json::to_vec(session).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_CHECKPOINT { return Err("Recovery checkpoint exceeds 128 MiB; save the project manually".into()); }
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let path = dir.join(format!("checkpoint-{stamp:030}.json"));
    let staged = path.with_extension("tmp");
    let mut file = OpenOptions::new().write(true).create_new(true).open(&staged).map_err(|e| e.to_string())?;
    let result = file.write_all(&bytes).and_then(|()| file.sync_all()); drop(file);
    if let Err(error) = result { let _ = fs::remove_file(staged); return Err(error.to_string()); }
    fs::rename(&staged, &path).map_err(|e| e.to_string())?;
    let mut paths: Vec<_> = fs::read_dir(dir).map_err(|e| e.to_string())?.flatten()
        .map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
    paths.sort();
    let excess = paths.len().saturating_sub(10);
    for path in paths.into_iter().take(excess) { let _ = fs::remove_file(path); }
    Ok(())
}

impl Recovery {
    pub fn adopt(&mut self, session: &EditorSession) -> Result<(), String> { write(&self.dir, session) }
    pub fn new() -> Result<Self, String> { Self::in_root(&root()) }
    fn in_root(root: &Path) -> Result<Self, String> {
        let dir = root.join(identity()); fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let lease = lease(&dir)?;
        Ok(Self { dir, lease: Some(lease), observed: None, saved: None, changed: Instant::now(), written: Instant::now(), work: None, cleaned: false })
    }

    /// `generation` changes on document edits, not playback/navigation ticks.
    pub fn tick(&mut self, editor: &Editor, generation: u64, gesture: bool) -> Option<String> {
        if let Some((_, rx)) = &self.work {
            match rx.try_recv() {
                Ok(result) => {
                    if let Some((thread, _)) = self.work.take() { let _ = thread.join(); }
                    if let Err(error) = result { self.saved = None; self.written = Instant::now(); self.changed = Instant::now(); return Some(format!("Recovery: {error}")); }
                },
                Err(mpsc::TryRecvError::Disconnected) => { self.work = None; self.saved = None; return Some("Recovery writer stopped; save the project manually".into()); },
                Err(mpsc::TryRecvError::Empty) => {},
            }
        }
        let key = (generation, editor.path.clone(), editor.dirty);
        if self.observed.as_ref() != Some(&key) { self.observed = Some(key.clone()); self.changed = Instant::now(); }
        if self.work.is_some() || self.cleaned || gesture || self.saved.as_ref() == Some(&key) { return None; }
        if editor.dirty && self.changed.elapsed() < QUIET && self.written.elapsed() < PERIOD { return None; }
        let session = editor.dirty.then(|| editor.session_snapshot());
        let dir = self.dir.clone(); let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let result = if let Some(session) = session { write(&dir, &session) }
            else {
                // Saving/discarding a project invalidates its dirty checkpoints.
                (|| { for entry in fs::read_dir(&dir).map_err(|e| e.to_string())?.flatten() {
                    if entry.path().extension().is_some_and(|e| e == "json") { fs::remove_file(entry.path()).map_err(|e| e.to_string())?; }
                } Ok(()) })()
            };
            let _ = tx.send(result);
        });
        self.work = Some((thread, rx)); self.saved = Some(key); self.written = Instant::now(); None
    }

    pub fn clean(&mut self) {
        if let Some((thread, _)) = self.work.take() { let _ = thread.join(); }
        // Keep checkpoints if marking clean fails; a recovery offer is safer.
        if fs::write(self.dir.join("clean"), b"closed normally").is_ok() {
            self.cleaned = true;
            if let Ok(entries) = fs::read_dir(&self.dir) { for entry in entries.flatten() {
                if entry.path().extension().is_some_and(|e| e == "json") { let _ = fs::remove_file(entry.path()); }
            } }
        }
    }
}
impl Drop for Recovery {
    fn drop(&mut self) {
        if let Some((thread, _)) = self.work.take() { let _ = thread.join(); }
        drop(self.lease.take());
        if self.cleaned { cleanup_closed(&self.dir); }
    }
}
impl Drop for Candidate {
    fn drop(&mut self) {
        drop(self.lease.take());
        if self.dir.join("clean").is_file() { cleanup_closed(&self.dir); }
    }
}
fn cleanup_closed(dir: &Path) {
    // Remove only our known files, never recursively remove unexpected content.
    if let Ok(entries) = fs::read_dir(dir) { for entry in entries.flatten() {
        let name=entry.file_name().to_string_lossy().into_owned();
        if name=="lease" || name=="clean" || (name.starts_with("checkpoint-") && (name.ends_with(".json") || name.ends_with(".tmp"))) {
            let _=fs::remove_file(entry.path());
        }
    } }
    let _=fs::remove_dir(dir);
}
