//! Opt-in same-executable export isolation. Uses the existing FFmpeg executable.
//! Protocol v1: one job line, then optional cancel lines; progress and one result.
//! The caller must gate delegation with !is_worker_process().
use crate::export::{self, diagnostics, ExportPreparation, ExportProgress, ExportSettings};
use crate::project::Project;
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{atomic::{AtomicBool, Ordering}, mpsc::{self, Sender}, Arc};
use std::time::{Duration, Instant};
#[cfg(windows)]
use std::os::windows::process::CommandExt;

const VERSION: u32 = 1;
const MAX_JOB: usize = 32 * 1024 * 1024;
const MAX_EVENT: usize = 4 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(25);
const CANCEL_GRACE: Duration = Duration::from_secs(10);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Job { version: u32, project: Project, settings: ExportSettings }
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Control { version: u32, cancel: bool }
#[derive(Serialize, Deserialize)]
struct WireProgress {
    fraction: f32,
    preparation: Option<(String, f32)>,
    message: String,
    remaining_seconds: Option<u64>,
    done: bool,
    error: Option<String>,
}
impl From<ExportProgress> for WireProgress {
    fn from(p: ExportProgress) -> Self {
        Self { fraction: p.fraction, preparation: p.preparation.map(|p| (p.phase.into(), p.fraction)),
            message: p.message, remaining_seconds: p.remaining_seconds, done: p.done, error: p.error }
    }
}
impl WireProgress {
    fn into_progress(self) -> ExportProgress {
        ExportProgress { fraction: self.fraction, preparation: self.preparation.map(|(phase, fraction)| {
            // Never leak an arbitrary wire string to satisfy the UI's static phase type.
            let phase = match phase.as_str() {
                "Analyzing audio" => "Analyzing audio", "Decoding audio" => "Decoding audio",
                "Mixing audio" => "Mixing audio", _ => "Preparing export",
            };
            ExportPreparation { phase, fraction }
        }), message: self.message, remaining_seconds: self.remaining_seconds, done: self.done, error: self.error }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Event {
    Progress { version: u32, progress: WireProgress },
    Result { version: u32, result: Result<(), String>, diagnostics: diagnostics::ExportDiagnostics },
}

/// Read one newline-terminated message without allocating beyond the limit.
fn line(reader: &mut impl BufRead, limit: usize) -> Result<Option<Vec<u8>>, String> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().map_err(|e| e.to_string())?;
        if available.is_empty() {
            return if bytes.is_empty() { Ok(None) } else { Err("Truncated worker protocol line".into()) };
        }
        let newline = available.iter().position(|b| *b == b'\n');
        let count = newline.map_or(available.len(), |n| n + 1);
        if bytes.len() + count > limit { return Err("Worker protocol line exceeds size limit".into()); }
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if newline.is_some() { return Ok(Some(bytes)); }
    }
}
fn emit(event: &Event) -> Result<(), String> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, event).map_err(|e| e.to_string())?;
    out.write_all(b"\n").and_then(|_| out.flush()).map_err(|e| e.to_string())
}

pub fn is_worker_process() -> bool {
    std::env::args_os().nth(1).is_some_and(|arg| arg == "--export-worker")
}

/// Dispatch before CLI/GUI initialization. A worker handles exactly one snapshot.
pub fn run_if_requested() -> Result<bool, String> {
    if !is_worker_process() { return Ok(false); }
    let result = serve();
    if let Err(error) = &result {
        let _ = emit(&Event::Result { version: VERSION, result: Err(error.clone()), diagnostics: Default::default() });
    }
    result.map(|_| true)
}
fn serve() -> Result<(), String> {
    let mut input = BufReader::new(io::stdin());
    let bytes = line(&mut input, MAX_JOB)?.ok_or("Missing export worker job")?;
    let job: Job = serde_json::from_slice(&bytes).map_err(|e| format!("Invalid export job: {e}"))?;
    if job.version != VERSION { return Err("Unsupported export worker protocol version".into()); }
    let cancel = Arc::new(AtomicBool::new(false));
    let input_cancel = cancel.clone();
    // Intentionally detached: stdin may remain open after completion. Process exit
    // terminates this reader; it owns no export resources and cannot delay result.
    std::thread::Builder::new().name("export-control".into()).spawn(move || {
        loop {
            match line(&mut input, 1024).and_then(|bytes| {
                bytes.map(|bytes| serde_json::from_slice::<Control>(&bytes).map_err(|e| e.to_string())).transpose()
            }) {
                Ok(Some(control)) if control.version == VERSION && !control.cancel => continue,
                _ => { input_cancel.store(true, Ordering::Release); break; }
            }
        }
    }).map_err(|e| e.to_string())?;
    let (tx, rx) = mpsc::channel::<ExportProgress>();
    let output_cancel = cancel.clone();
    let forward = std::thread::Builder::new().name("export-progress".into()).spawn(move || {
        let mut error = None;
        for progress in rx {
            if error.is_none() {
                if let Err(e) = emit(&Event::Progress { version: VERSION, progress: progress.into() }) {
                    output_cancel.store(true, Ordering::Release);
                    error = Some(e);
                }
            }
        }
        error
    }).map_err(|e| e.to_string())?;
    let mut collected = diagnostics::ExportDiagnostics::default();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        export::export_mp4_with_diagnostics(&job.project, &job.settings, &cancel, tx, &mut collected)
    })).unwrap_or_else(|_| Err("Export worker panicked".into()));
    // Export has returned and joined its render/encoder workers. Drain and join
    // progress before publishing the authoritative result and diagnostics.
    if let Some(error) = forward.join().map_err(|_| "Progress forwarding panicked")? { return Err(error); }
    emit(&Event::Result { version: VERSION, result, diagnostics: collected })
}

// A job is assigned before the request is sent, so the child cannot start
// FFmpeg before containment. Closing it terminates all remaining descendants.
#[cfg(windows)]
mod containment {
    use super::*;
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    type Handle = *mut c_void;
    #[repr(C)]
    #[derive(Default)]
    struct BasicLimits {
        process_time: i64, job_time: i64, flags: u32,
        minimum_working_set: usize, maximum_working_set: usize,
        active_processes: u32, affinity: usize, priority: u32, scheduling: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct ExtendedLimits {
        basic: BasicLimits, io_counters: [u64; 6],
        process_memory: usize, job_memory: usize,
        peak_process_memory: usize, peak_job_memory: usize,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> Handle;
        fn SetInformationJobObject(job: Handle, class: i32, info: *const c_void, length: u32) -> i32;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
    }
    pub struct Job(Handle);
    impl Job {
        pub fn assign(child: &Child) -> Result<Self, String> {
            // SAFETY: null optional parameters, valid owned process handle,
            // and ABI-compatible, initialized JOBOBJECT_EXTENDED_LIMIT_INFORMATION.
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() { return Err(io::Error::last_os_error().to_string()); }
                let job = Self(handle);
                let mut limits = ExtendedLimits::default();
                limits.basic.flags = 0x2000; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                if SetInformationJobObject(handle, 9, &limits as *const _ as *const c_void,
                    std::mem::size_of::<ExtendedLimits>() as u32) == 0
                    || AssignProcessToJobObject(handle, child.as_raw_handle()) == 0 {
                    return Err(io::Error::last_os_error().to_string());
                }
                Ok(job)
            }
        }
    }
    impl Drop for Job {
        fn drop(&mut self) { unsafe { CloseHandle(self.0); } }
    }
}

struct Process {
    child: Child,
    cleaned: bool,
    #[cfg(windows)]
    job: Option<containment::Job>,
}
impl Process {
    fn new(child: Child) -> Result<Self, String> {
        let mut process = Self { child, cleaned: false, #[cfg(windows)] job: None };
        #[cfg(windows)]
        { process.job = Some(containment::Job::assign(&process.child)
            .map_err(|e| format!("Could not contain export worker: {e}"))?); }
        Ok(process)
    }
    fn stop(&mut self) {
        if self.cleaned { return; }
        self.cleaned = true;
        #[cfg(windows)]
        drop(self.job.take());
        #[cfg(unix)]
        {
            extern "C" { fn kill(pid: i32, signal: i32) -> i32; }
            // Child starts a private group inherited by FFmpeg. Signal even
            // after the leader exits: descendants may still own pipe handles.
            if let Ok(pid) = i32::try_from(self.child.id()) {
                if pid > 0 { unsafe { kill(-pid, 9); } }
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Drop for Process { fn drop(&mut self) { self.stop(); } }

/// Only a successfully, exclusively created directory is ever owned/deleted.
/// Its absolute parent keeps cleanup independent of relative working paths.
struct JobDirectory { path: PathBuf }
impl JobDirectory {
    fn create(output: &Path) -> Result<Self, String> {
        use std::sync::atomic::AtomicU64;
        use std::time::{SystemTime, UNIX_EPOCH};
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        let parent = output.parent().filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let parent = std::path::absolute(parent).map_err(|e| e.to_string())?;
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?.as_nanos();
        for _ in 0..100 {
            let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(".quaternio-export-job-{}-{stamp}-{serial}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("Could not create export staging directory: {e}")),
            }
        }
        Err("Could not allocate a unique export staging directory".into())
    }
}
impl Drop for JobDirectory {
    fn drop(&mut self) {
        // Never enumerate or remove siblings. remove_dir_all does not follow
        // symlinks; refuse a replaced root as an additional containment check.
        if !self.path.is_absolute() { return; }
        let Ok(metadata) = std::fs::symlink_metadata(&self.path) else { return; };
        if !metadata.is_dir() || metadata.file_type().is_symlink() { return; }
        // Job-object termination is asynchronous. Allow remaining OS handles
        // to close after the process and pipe threads have been joined.
        for attempt in 0..80 {
            match std::fs::remove_dir_all(&self.path) {
                Ok(()) => return,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return,
                Err(e) if attempt == 79 => {
                    eprintln!("Could not clean export staging directory {}: {e}", self.path.display());
                    return;
                }
                Err(_) => std::thread::sleep(POLL),
            }
        }
    }
}

/// Blocking API for a background export thread. Diagnostics are copied into the
/// caller's active diagnostics::collect scope, including on export failure.
pub fn run(project: &Project, settings: &ExportSettings, cancel: &AtomicBool, progress: Sender<ExportProgress>) -> Result<(), String> {
    if is_worker_process() { return Err("Recursive export worker invocation refused".into()); }
    if cancel.load(Ordering::Acquire) { return Err("Export cancelled".into()); }
    // Check the real destination before substituting the private staging path;
    // otherwise the child cannot detect overwriting original project media.
    super::preflight_media(project, settings)?;
    let job_directory = JobDirectory::create(&settings.output)?;
    let mut child_settings = settings.clone();
    child_settings.output = job_directory.path.join("movie.mp4");
    // Serialize borrowed immutable state before spawning; limit allocations too.
    #[derive(Serialize)]
    struct Request<'a> { version: u32, project: &'a Project, settings: &'a ExportSettings }
    struct Bounded(Vec<u8>);
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.0.len().saturating_add(bytes.len()) >= MAX_JOB {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "Export snapshot exceeds 32 MiB"));
            }
            self.0.extend_from_slice(bytes); Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }
    let mut payload = Bounded(Vec::new());
    serde_json::to_writer(&mut payload, &Request { version: VERSION, project, settings: &child_settings }).map_err(|e| e.to_string())?;
    payload.0.push(b'\n');
    let mut command = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
    command.arg("--export-worker").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for variable in ["TMP", "TEMP", "TMPDIR"] {
        command.env(variable, &job_directory.path);
    }
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    #[cfg(unix)]
    { use std::os::unix::process::CommandExt; command.process_group(0); }
    let mut child = Process::new(command.spawn().map_err(|e| format!("Could not start export worker: {e}"))?)?;
    let mut stdin = child.child.stdin.take().ok_or("Missing worker stdin")?;
    let stdout = child.child.stdout.take().ok_or("Missing worker stdout")?;
    let mut stderr = child.child.stderr.take().ok_or("Missing worker stderr")?;
    let finished = AtomicBool::new(false);
    let outcome = std::thread::scope(|scope| {
        let finished_ref = &finished;
        let writer = scope.spawn(move || -> Result<(), String> {
            stdin.write_all(&payload.0).and_then(|_| stdin.flush()).map_err(|e| e.to_string())?;
            while !finished_ref.load(Ordering::Acquire) {
                if cancel.load(Ordering::Acquire) {
                    serde_json::to_writer(&mut stdin, &Control { version: VERSION, cancel: true }).map_err(|e| e.to_string())?;
                    stdin.write_all(b"\n").and_then(|_| stdin.flush()).map_err(|e| e.to_string())?;
                    break;
                }
                std::thread::sleep(POLL);
            }
            Ok(())
        });
        let (events, receive) = mpsc::sync_channel(16);
        let reader = scope.spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let event = line(&mut reader, MAX_EVENT).and_then(|b| b.map(|b| serde_json::from_slice::<Event>(&b).map_err(|e| e.to_string())).transpose());
                let last = !matches!(event, Ok(Some(_)));
                if events.send(event).is_err() || last { break; }
            }
        });
        let errors = scope.spawn(move || {
            let mut tail = Vec::new(); let mut buffer = [0; 4096];
            while let Ok(n) = stderr.read(&mut buffer) {
                if n == 0 { break; } tail.extend_from_slice(&buffer[..n]);
                if tail.len() > 65536 { tail.drain(..tail.len() - 65536); }
            }
            String::from_utf8_lossy(&tail).into_owned()
        });
        let mut completion = None;
        let mut terminal = None;
        let mut result_at: Option<Instant> = None;
        let mut exit_at: Option<Instant> = None;
        let mut cancelled_at = None;
        let protocol = (|| -> Result<(), String> {
            loop {
                if exit_at.is_none() && child.child.try_wait().map_err(|e| e.to_string())?.is_some() {
                    exit_at = Some(Instant::now());
                    child.stop(); // descendants must not retain protocol pipes
                }
                if exit_at.is_some_and(|at| at.elapsed() >= CANCEL_GRACE) {
                    return Err("Export worker pipes remained open after exit".into());
                }
                if result_at.is_some_and(|at| at.elapsed() >= CANCEL_GRACE) {
                    return Err("Export worker failed to exit after result".into());
                }
                if cancel.load(Ordering::Acquire) {
                    if cancelled_at.get_or_insert_with(Instant::now).elapsed() >= CANCEL_GRACE {
                        return Err("Export worker cancellation timed out".into());
                    }
                }
                match receive.recv_timeout(POLL) {
                    Ok(Ok(Some(Event::Progress { version, progress: p }))) if version == VERSION && terminal.is_none() => {
                        if p.done { completion = Some(p.into_progress()); } else { let _ = progress.send(p.into_progress()); }
                    }
                    Ok(Ok(Some(Event::Result { version, result, diagnostics: collected }))) if version == VERSION && terminal.is_none() => {
                        diagnostics::record(|d| *d = collected);
                        terminal = Some(result);
                        result_at = Some(Instant::now());
                        finished.store(true, Ordering::Release);
                    }
                    Ok(Ok(Some(_))) => return Err("Unexpected export worker event or protocol version".into()),
                    Ok(Ok(None)) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Ok(Err(e)) => return Err(format!("Invalid export worker response: {e}")),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
            terminal.as_ref().ok_or("Export worker exited without a result")?;
            // stdout EOF alone is not success: require process termination too.
            let deadline = Instant::now() + CANCEL_GRACE;
            loop {
                if let Some(status) = child.child.try_wait().map_err(|e| e.to_string())? {
                    return if status.success() { Ok(()) } else { Err(format!("Export worker exited with {status}")) };
                }
                if Instant::now() >= deadline { return Err("Export worker failed to exit after result".into()); }
                std::thread::sleep(POLL);
            }
        })();
        finished.store(true, Ordering::Release);
        drop(receive); // unblock bounded reader before joining it
        child.stop();
        let sent = writer.join().map_err(|_| "Export input writer panicked".to_string()).and_then(|r| r);
        let _ = reader.join();
        let detail = errors.join().unwrap_or_default();
        protocol.map_err(|e| if detail.trim().is_empty() { e } else { format!("{e}: {}", detail.trim()) })?;
        sent?;
        terminal.ok_or("Missing export result")??;
        Ok::<_, String>(completion)
    });
    // Close containment and every pipe before deleting the owned directory,
    // including on protocol errors, abnormal exit, and forced cancellation.
    drop(child);
    let completion = outcome?;
    let staged = super::StagedOutput { path: child_settings.output.clone() };
    diagnostics::measure(diagnostics::ExportStage::Finalization, || {
        staged.commit(&settings.output, cancel)
    })?;
    drop(staged);
    drop(job_directory);
    let mut completion = completion.unwrap_or(ExportProgress {
        fraction: 1.0, preparation: None, message: String::new(),
        remaining_seconds: None, done: true, error: None,
    });
    completion.message = format!("Wrote {}", settings.output.display());
    let _ = progress.send(completion);
    Ok(())
}
