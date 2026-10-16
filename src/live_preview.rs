//! Workspace-contained recovery checkpoints and cooperative immutable-build reloads.
use crate::{preferences::Layout, source::SourceSession, timeline::EditorSession};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Condvar, Mutex},
    thread::JoinHandle };
const MAX_SESSION: u64 = 128 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
pub struct LiveSession {
    pub version: u32,
    pub editor: EditorSession,
    pub source: SourceSession,
    pub layout: Layout,
    pub extras: serde_json::Value }
impl LiveSession {
    fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("Unsupported live session version".into());
        }
        self.editor.validate()?;
        if self.source.version != 1 || self.source.cursors.len() > 100_000 {
            return Err("Invalid or unsupported source recovery session".into());
        }
        if !self.layout.upper_ratio.is_finite()
            || !self.layout.source_width.is_finite()
            || !self.layout.properties_width.is_finite()
            || !self.layout.project_width.is_finite()
        {
            return Err("Invalid recovery layout".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub build_id: String,
    pub relative_executable: String,
    pub sha256: String }
#[derive(Default)]
struct Pending {
    snapshot: Option<LiveSession>,
    stop: bool }
pub struct LiveChannel {
    root: PathBuf,
    data: PathBuf,
    build_id: String,
    session_id: String,
    rejected: Option<String>,
    _lock: File,
    pending: Arc<(Mutex<Pending>, Condvar)>,
    publication: Arc<Mutex<()>>,
    results: Mutex<mpsc::Receiver<Result<(), String>>>,
    worker: Option<JoinHandle<()>> }
impl LiveChannel {
    pub fn new_from_env() -> Result<Option<Self>, String> {
        let Some(root) = std::env::var_os("QUATERNIO_LIVE_DIR") else {
            return Ok(None);
        };
        let root = PathBuf::from(root);
        reject_reparse(&root)?;
        let root = fs::canonicalize(root).map_err(|e| e.to_string())?;
        let data = root.join("data");
        reject_reparse(&data)?;
        fs::create_dir_all(&data).map_err(|e| e.to_string())?;
        if !fs::canonicalize(&data)
            .map_err(|e| e.to_string())?
            .starts_with(&root)
        {
            return Err("Live data directory escapes channel".into());
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0);
        }
        reject_reparse(&data.join("session.lock"))?;
        let lock = options
            .open(data.join("session.lock"))
            .map_err(|e| format!("Live session is already open or unavailable: {e}"))?;
        #[cfg(not(windows))]
        lock.try_lock().map_err(|e| e.to_string())?;
        let build_id =
            std::env::var("QUATERNIO_LIVE_BUILD_ID").map_err(|_| "Missing live build identity")?;
        let session_id =
            std::env::var("QUATERNIO_LIVE_SESSION").map_err(|_| "Missing live session identity")?;
        if build_id.is_empty() || session_id.is_empty() {
            return Err("Empty live identity".into());
        }
        let pending = Arc::new((Mutex::new(Pending::default()), Condvar::new()));
        let publication = Arc::new(Mutex::new(()));
        let (tx, rx) = mpsc::channel();
        let queue = pending.clone();
        let gate = publication.clone();
        let path = data.join("live-session.json");
        let worker = std::thread::spawn(move || loop {
            let (mutex, wake) = &*queue;
            let mut state = mutex.lock().unwrap();
            while state.snapshot.is_none() && !state.stop {
                state = wake.wait(state).unwrap();
            }
            if state.stop {
                break;
            }
            // Take the publication gate before dequeuing: a final synchronous save
            // clears pending work under this same order, so stale work cannot overwrite it.
            drop(state);
            let _guard = gate.lock().unwrap();
            let mut state = mutex.lock().unwrap();
            let snapshot = state.snapshot.take();
            drop(state);
            if let Some(snapshot) = snapshot {
                let _ = tx.send(save_session(&path, &snapshot));
            }
        });
        Ok(Some(Self {
            root,
            data,
            build_id,
            session_id,
            rejected: std::env::var("QUATERNIO_LIVE_REJECTED_BUILD_ID").ok(),
            _lock: lock,
            pending,
            publication,
            results: Mutex::new(rx),
            worker: Some(worker) }))
    }
    pub fn recover(&self) -> Result<Option<LiveSession>, String> {
        let path = self.data.join("live-session.json");
        reject_reparse(&path)?;
        if !path.exists() {
            return Ok(None);
        }
        let bytes = read_bounded(&path, MAX_SESSION)?;
        let session: LiveSession = serde_json::from_slice(&bytes)
            .map_err(|e| format!("Recovery checkpoint is invalid: {e}"))?;
        session.validate()?;
        Ok(Some(session))
    }
    pub fn submit(&self, snapshot: LiveSession) {
        let (state, wake) = &*self.pending;
        state.lock().unwrap().snapshot = Some(snapshot);
        wake.notify_one();
    }
    /// Synchronously publish the most recent state before a normal close.
    pub fn flush(&self, snapshot: LiveSession) -> Result<(), String> {
        let _guard = self.publication.lock().unwrap();
        self.pending.0.lock().unwrap().snapshot = None;
        save_session(&self.data.join("live-session.json"), &snapshot)
    }
    pub fn poll_save(&self) -> Option<Result<(), String>> {
        self.results.lock().unwrap().try_recv().ok()
    }
    pub fn mark_ready(&self) -> Result<(), String> {
        if self.recover()?.is_none() {
            return Err("No durable recovery checkpoint is available".into());
        }
        atomic_json(
            &self.data.join("live-ready.json"),
            &serde_json::json!({"schema_version":1,"build_id":self.build_id,"session_id":self.session_id}),
        )
    }
    pub fn check_update(&self) -> Result<Option<Manifest>, String> {
        let manifest: Manifest = serde_json::from_slice(&read_bounded(
            &self.root.join("active-build.json"),
            64 * 1024,
        )?)
        .map_err(|e| e.to_string())?;
        validate_manifest_schema(&manifest)?;
        if manifest.build_id == self.build_id || self.rejected.as_ref() == Some(&manifest.build_id)
        {
            return Ok(None);
        }
        self.validate_manifest(&manifest)?;
        Ok(Some(manifest))
    }
    fn validate_manifest(&self, manifest: &Manifest) -> Result<(), String> {
        validate_manifest_schema(manifest)?;
        let relative = Path::new(&manifest.relative_executable);
        if relative
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err("Invalid live executable path".into());
        }
        reject_reparse(&self.root.join("builds"))?;
        reject_reparse(&self.root.join(relative))?;
        let builds = fs::canonicalize(self.root.join("builds")).map_err(|e| e.to_string())?;
        let executable = fs::canonicalize(self.root.join(relative)).map_err(|e| e.to_string())?;
        if !builds.starts_with(&self.root)
            || !executable.starts_with(&builds)
            || !executable.is_file()
        {
            return Err("Live executable escapes immutable builds".into());
        }
        if !hash_file(&executable)?.eq_ignore_ascii_case(&manifest.sha256) {
            return Err("Live executable checksum mismatch".into());
        }
        Ok(())
    }
    pub fn checkpoint_for_reload(
        &self,
        snapshot: LiveSession,
        manifest: &Manifest,
    ) -> Result<(), String> {
        self.validate_manifest(manifest)?;
        let active: Manifest = serde_json::from_slice(&read_bounded(
            &self.root.join("active-build.json"),
            64 * 1024,
        )?)
        .map_err(|e| e.to_string())?;
        if active != *manifest {
            return Err("Active build changed; reload will be retried".into());
        }
        let _guard = self.publication.lock().unwrap();
        self.pending.0.lock().unwrap().snapshot = None;
        let path = self.data.join("live-session.json");
        save_session(
            &self.data.join("live-session.before-reload.json"),
            &snapshot,
        )?;
        save_session(&path, &snapshot)?;
        atomic_json(
            &self.data.join("reload-request.json"),
            &serde_json::json!({"schema_version":1,"build_id":manifest.build_id,"session_id":self.session_id,"checkpoint_sha256":hash_file(&path)?}),
        )
    }
}
impl Drop for LiveChannel {
    fn drop(&mut self) {
        {
            let (state, wake) = &*self.pending;
            state.lock().unwrap().stop = true;
            wake.notify_one();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn validate_manifest_schema(manifest: &Manifest) -> Result<(), String> {
    if manifest.schema_version != 1
        || manifest.build_id.is_empty()
        || manifest.sha256.len() != 64
        || !manifest.sha256.bytes().all(|c| c.is_ascii_hexdigit())
    {
        return Err("Invalid live build manifest".into());
    }
    Ok(())
}
/// Check the lexical path before canonicalization, including in-channel junctions.
/// Missing leaf files are allowed for creation; other metadata failures fail closed.
fn reject_reparse(path: &Path) -> Result<(), String> {
    for ancestor in path.ancestors() {
        if ancestor.as_os_str().is_empty() {
            continue;
        }
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                #[cfg(windows)]
                let reparse = {
                    use std::os::windows::fs::MetadataExt;
                    metadata.file_attributes() & 0x400 != 0
                };
                #[cfg(not(windows))]
                let reparse = metadata.file_type().is_symlink();
                if reparse {
                    return Err(format!(
                        "Live channel paths cannot contain reparse points: {}",
                        ancestor.display()
                    ));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()) }
    }
    Ok(())
}
fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    reject_reparse(path)?;
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > limit {
        return Err("Live session file exceeds supported size".into());
    }
    Ok(bytes)
}
fn save_session(path: &Path, session: &LiveSession) -> Result<(), String> {
    session.validate()?;
    atomic_json(path, session)
}
fn atomic_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_SESSION {
        return Err("Recovery checkpoint exceeds supported size".into());
    }
    reject_reparse(path)?;
    let staged = path.with_extension("json.staged");
    reject_reparse(&staged)?;
    // A process may have crashed before rename. Discard only an ordinary staged
    // file; never follow a link or remove a directory, and never truncate it.
    if fs::symlink_metadata(&staged).is_ok_and(|m| m.is_file()) {
        fs::remove_file(&staged).map_err(|e| e.to_string())?;
    }
    let mut created = false;
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)
            .map_err(|e| e.to_string())?;
        created = true;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| e.to_string())?;
        drop(file);
        reject_reparse(path)?;
        reject_reparse(&staged)?;
        fs::rename(&staged, path).map_err(|e| e.to_string())
    })();
    if result.is_err() && created && reject_reparse(&staged).is_ok() {
        let _ = fs::remove_file(&staged);
    }
    result
}
fn hash_file(path: &Path) -> Result<String, String> {
    reject_reparse(path)?;
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut hash = Sha256::new();
    let mut buf = [0; 65536];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

