#[path = "native_eligibility.rs"]
mod native_eligibility;
pub use native_eligibility::Eligibility as PreviewTimingService;
pub use native_eligibility::set_busy as set_index_busy;
// Isolated preview decoder. Never used by the export source by default.
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc },
    thread::JoinHandle,
    time::{Duration, Instant} };
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Cli,
    Native,
    Auto }
#[derive(Debug)]
pub enum NativeDecodeError {
    Superseded,
    Failed(String) }
#[derive(Deserialize)]
struct Header {
    version: u32,
    id: u64,
    status: String,
    width: u32,
    height: u32,
    bytes: usize,
    ordinal: i64,
    error: Option<String> }
struct Reply {
    header: Header,
    bytes: Vec<u8> }
struct Worker {
    child: Child,
    input: ChildStdin,
    replies: mpsc::Receiver<Result<Reply, String>>,
    reader: Option<JoinHandle<()>> }
impl Worker {
    fn spawn(path: &Path) -> Result<Self, String> {
        let mut command = Command::new(path);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("Preview helper launch failed: {e}"))?;
        let input = child.stdin.take().ok_or("Missing preview control pipe")?;
        let output = child.stdout.take().ok_or("Missing preview image pipe")?;
        let (tx, replies) = mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            let mut output = std::io::BufReader::new(output);
            loop {
                let reply = (|| {
                    let mut line = Vec::new();
                    let n = (&mut output)
                        .take(65537)
                        .read_until(b'\n', &mut line)
                        .map_err(|e| e.to_string())?;
                    if n == 0 || n > 65536 {
                        return Err("Preview helper reply header invalid".into());
                    }
                    let header: Header =
                        serde_json::from_slice(&line).map_err(|e| e.to_string())?;
                    if header.bytes > 4096 * 4096 * 3 {
                        return Err("Preview helper reply exceeds buffer limit".into());
                    }
                    let mut bytes = vec![0; header.bytes];
                    output.read_exact(&mut bytes).map_err(|e| e.to_string())?;
                    Ok(Reply { header, bytes })
                })();
                let failed = reply.is_err();
                if tx.try_send(reply).is_err() || failed {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            input,
            replies,
            reader: Some(reader) })
    }
    fn send(&mut self, message: serde_json::Value) -> Result<(), String> {
        serde_json::to_writer(&mut self.input, &message).map_err(|e| e.to_string())?;
        self.input
            .write_all(b"\n")
            .and_then(|_| self.input.flush())
            .map_err(|e| e.to_string())
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[derive(Clone, Default, Serialize)]
pub struct NativePreviewStats {
    pub policy: String,
    pub native_frames: u64,
    pub cli_fallbacks: u64,
    pub eligibility_waits: u64,
    pub helper_starts: u64,
    pub transport_warmups: u64,
    pub cancellations: u64,
    pub failures: u64,
    pub last_error: Option<String> }

pub struct NativePreviewClient {
    stats: NativePreviewStats,
    mode: Mode,
    eligibility: std::sync::Arc<native_eligibility::Eligibility>,
    path: PathBuf,
    worker: Option<Worker>,
    warmup_attempted: bool,
    next_id: u64,
    failures: u32,
    retry_after: Option<Instant>,
    unsupported: HashSet<(PathBuf, u64, u128)>,
    fingerprints: HashMap<PathBuf, (u64, u128)> }
impl NativePreviewClient {
    pub fn reset(&mut self) {
        self.worker = None;
    }

    pub fn stats(&self) -> NativePreviewStats {
        self.stats.clone()
    }
    /// Keep an already warm CLI stream during forward playback. An intentional
    /// seek remains free to adopt a newly certified native decoder.
    pub fn retain_warm_cli(&mut self, playback: bool, reusable: bool) -> bool {
        let retain = self.mode == Mode::Auto && playback && reusable;
        if retain {
            self.stats.cli_fallbacks += 1;
        }
        retain
    }
    pub fn from_env() -> Self {
        let mode = match std::env::var("QUATERNIO_PREVIEW_BACKEND").as_deref() {
            Ok("native") => Mode::Native,
            Ok("auto") => Mode::Auto,
            Ok("cli") => Mode::Cli,
            _ => Mode::Auto };
        let path = std::env::var_os("QUATERNIO_PREVIEW_HELPER")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::current_exe()
                    .unwrap_or_default()
                    .parent()
                    .unwrap_or(Path::new("."))
                    .join("preview-decoder")
                    .join(if cfg!(windows) {
                        "quaternio-preview-helper.exe"
                    } else {
                        "quaternio-preview-helper"
                    })
            });
        Self {
            stats: NativePreviewStats {
                policy: match mode {
                    Mode::Cli => "cli",
                    Mode::Native => "native",
                    Mode::Auto => "auto" }
                .into(),
                ..Default::default()
            },
            mode,
            eligibility: native_eligibility::Eligibility::shared(),
            path,
            worker: None,
            warmup_attempted: false,
            next_id: 1,
            failures: 0,
            retry_after: None,
            unsupported: HashSet::new(),
            fingerprints: HashMap::new() }
    }
    /// None requests the unchanged CLI route. Supersession must never fallback.
    pub fn decode(
        &mut self,
        path: &Path,
        index: i64,
        fps: f64,
        width: u32,
        height: u32,
        lifetime: Option<&AtomicBool>,
        request: Option<&AtomicBool>,
    ) -> Result<Option<Vec<u8>>, NativeDecodeError> {
        if self.mode == Mode::Cli {
            self.stats.cli_fallbacks += 1;
            return Ok(None);
        }
        let cancelled = || {
            lifetime.is_some_and(|f| f.load(Ordering::Acquire))
                || request.is_some_and(|f| f.load(Ordering::Acquire))
        };
        if cancelled() {
            return Err(NativeDecodeError::Superseded);
        }
        let fingerprint = std::fs::metadata(path).ok().map(|m| {
            (
                path.to_path_buf(),
                m.len(),
                m.modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |t| t.as_nanos()),
            )
        });
        if let Some((path, length, modified)) = &fingerprint {
            let identity = (*length, *modified);
            if self
                .fingerprints
                .get(path)
                .is_some_and(|old| *old != identity)
            {
                self.worker = None;
                self.unsupported.retain(|entry| &entry.0 != path);
            }
            if self.fingerprints.len() >= 1024 {
                self.fingerprints.clear();
                self.worker = None;
            }
            self.fingerprints.insert(path.clone(), identity);
        }
        if self.mode == Mode::Auto
            && (self.retry_after.is_some_and(|at| Instant::now() < at)
                || fingerprint
                    .as_ref()
                    .is_some_and(|key| self.unsupported.contains(key)))
        {
            self.stats.cli_fallbacks += 1;
            return Ok(None);
        }
        if self.mode == Mode::Auto && !self.eligibility.eligible(path, fps) {
            // Loading the helper executable/DLLs overlaps the original CLI
            // decode. No media is opened and no certification is bypassed.
            self.warm_transport(&cancelled);
            if cancelled() {
                return Err(NativeDecodeError::Superseded);
            }
            self.stats.eligibility_waits += 1;
            self.stats.cli_fallbacks += 1;
            return Ok(None);
        }
        let result = self.decode_inner(path, index, fps, width, height, &cancelled);
        self.finish_decode(result, fingerprint)
    }
    fn warm_transport(&mut self, cancelled: &dyn Fn() -> bool) {
        if self.mode != Mode::Auto
            || self.warmup_attempted
            || self.worker.is_some()
            || cancelled()
            || self.retry_after.is_some_and(|at| Instant::now() < at)
        {
            return;
        }
        self.warmup_attempted = true;
        self.stats.transport_warmups += 1;
        self.stats.helper_starts += 1;
        let result = if self.path.is_absolute() {
            Worker::spawn(&self.path)
        } else {
            Err("Preview helper path must be absolute".into())
        };
        match result {
            Ok(worker) if !cancelled() => self.worker = Some(worker),
            Ok(worker) => drop(worker),
            Err(error) => {
                self.stats.failures += 1;
                self.stats.last_error = Some(error);
                self.failures = self.failures.saturating_add(1);
                if self.failures >= 3 {
                    self.retry_after = Some(Instant::now() + Duration::from_secs(30));
                }
            }
        }
    }
    fn finish_decode(
        &mut self,
        result: Result<Vec<u8>, NativeDecodeError>,
        fingerprint: Option<(PathBuf, u64, u128)>,
    ) -> Result<Option<Vec<u8>>, NativeDecodeError> {
        match result {
            Ok(rgb) => {
                self.stats.native_frames += 1;
                self.failures = 0;
                self.retry_after = None;
                Ok(Some(rgb))
            }
            Err(NativeDecodeError::Superseded) => {
                self.stats.cancellations += 1;
                Err(NativeDecodeError::Superseded)
            }
            Err(NativeDecodeError::Failed(error)) => {
                self.stats.failures += 1;
                self.stats.last_error = Some(error.clone());
                self.worker = None;
                self.failures = self.failures.saturating_add(1);
                if error.contains("unsupported") || error.contains("not certified") {
                    if let Some(key) = fingerprint {
                        if self.unsupported.len() >= 1024 {
                            self.unsupported.clear();
                        }
                        self.unsupported.insert(key);
                    }
                }
                if self.failures >= 3 {
                    self.retry_after = Some(Instant::now() + Duration::from_secs(30));
                }
                if self.mode == Mode::Auto {
                    self.stats.cli_fallbacks += 1;
                    Ok(None)
                } else {
                    Err(NativeDecodeError::Failed(error))
                }
            }
        }
    }
    fn decode_inner(
        &mut self,
        path: &Path,
        index: i64,
        fps: f64,
        width: u32,
        height: u32,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<u8>, NativeDecodeError> {
        let failed = NativeDecodeError::Failed;
        if !self.path.is_absolute() {
            return Err(failed("Preview helper path must be absolute".into()));
        }
        if !(2..=4096).contains(&width)
            || !(2..=4096).contains(&height)
            || index < 0
            || !fps.is_finite()
            || fps <= 0.0
        {
            return Err(failed("Native preview unsupported request".into()));
        }
        if self.worker.is_none() {
            self.stats.helper_starts += 1;
            self.worker = Some(Worker::spawn(&self.path).map_err(failed)?);
        }
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| failed("Preview request identity exhausted".into()))?;
        let worker = self.worker.as_mut().unwrap();
        worker.send(serde_json::json!({"op":"decode","version":1,"id":id,"path":path,"index":index,"fps":fps,"width":width,"height":height})).map_err(failed)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut cancellation_at = None;
        loop {
            if cancelled() && cancellation_at.is_none() {
                cancellation_at = Some(Instant::now());
                if worker
                    .send(serde_json::json!({"op":"cancel","id":id}))
                    .is_err()
                {
                    self.worker = None;
                    return Err(NativeDecodeError::Superseded);
                }
            }
            match worker.replies.recv_timeout(Duration::from_millis(5)) {
                Ok(Ok(reply)) => {
                    let h = reply.header;
                    if cancelled() || h.status == "cancelled" {
                        return Err(NativeDecodeError::Superseded);
                    }
                    if h.version != 1
                        || h.id != id
                        || h.width != width
                        || h.height != height
                        || h.ordinal != index
                    {
                        return Err(failed("Preview helper reply identity mismatch".into()));
                    }
                    if h.status != "ok" {
                        return Err(failed(
                            h.error
                                .unwrap_or_else(|| "Preview helper decode failed".into()),
                        ));
                    }
                    if h.bytes != width as usize * height as usize * 3
                        || reply.bytes.len() != h.bytes
                    {
                        return Err(failed("Preview helper RGB shape mismatch".into()));
                    }
                    return Ok(reply.bytes);
                }
                Ok(Err(error)) => {
                    return Err(if cancelled() {
                        NativeDecodeError::Superseded
                    } else {
                        failed(error)
                    })
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(if cancelled() {
                        NativeDecodeError::Superseded
                    } else {
                        failed("Preview helper reply pipe closed".into())
                    })
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if cancellation_at.is_some_and(|at| at.elapsed() >= Duration::from_millis(150)) {
                self.worker = None;
                return Err(NativeDecodeError::Superseded);
            }
            if Instant::now() >= deadline {
                return Err(failed("Preview helper request timed out".into()));
            }
        }
    }
}

