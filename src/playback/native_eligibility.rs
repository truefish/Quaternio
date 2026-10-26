//! One shared, nonblocking packet timing certification worker for automatic native preview.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    io::{BufRead, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex, OnceLock, Weak },
    thread::JoinHandle,
    time::{Duration, Instant} };
const MAX_PACKETS: usize = 2_000_000;
const CERTIFICATE_VERSION: u32 = 2;
#[derive(Clone)]
struct Job {
    path: PathBuf,
    key: String,
    fps: f64,
    prepared: Option<PreparedJob> }
#[derive(Clone)]
struct PreparedJob {
    count: usize,
    cancel: Arc<AtomicBool>,
    done: std::sync::mpsc::SyncSender<Result<(), String>> }
#[derive(Clone, Serialize, Deserialize)]
struct Certificate {
    version: u32,
    key: String,
    eligible: bool,
    reason: String,
    #[serde(default)]
    prepared_count: Option<usize> }
#[derive(Default)]
struct State {
    jobs: VecDeque<Job>,
    records: HashMap<String, Option<Certificate>>,
    stopped: bool }
pub struct Eligibility {
    state: Arc<(Mutex<State>, Condvar)>,
    cancel: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>> }
static BACKGROUND_BUSY: AtomicBool = AtomicBool::new(false);
static SHARED: OnceLock<Mutex<Weak<Eligibility>>> = OnceLock::new();
pub fn set_busy(busy: bool) {
    BACKGROUND_BUSY.store(busy, Ordering::Release);
    if let Some(service) = SHARED.get().and_then(|s| s.lock().ok()?.upgrade()) {
        service.busy.store(busy, Ordering::Release);
        service.state.1.notify_one();
    }
}


impl Eligibility {
    /// Run on the preparation worker only. The sole timing worker performs the
    /// proof; no segment may become visible until this returns successfully.
    pub fn certify_prepared(
        &self,
        path: &Path,
        fps: f64,
        count: usize,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        if cancel.load(Ordering::Acquire) {
            return Err("Preparation interrupted during timing certification".into());
        }
        if !(2..=10_000).contains(&count) || !fps.is_finite() || fps <= 0.0 {
            return Err("Unsupported: prepared timing bounds".into());
        }
        let key = fingerprint(path, fps).ok_or("Prepared segment unavailable")?;
        let (done, result) = std::sync::mpsc::sync_channel(1);
        let request_cancel = Arc::new(AtomicBool::new(false));
        {
            let mut state = self.state.0.lock().unwrap();
            if state.stopped || state.jobs.len() >= 8 {
                return Err("Timing worker unavailable".into());
            }
            state.jobs.push_front(Job {
                path: path.into(),
                key,
                fps,
                prepared: Some(PreparedJob {
                    count,
                    cancel: request_cancel.clone(),
                    done }) });
            self.state.1.notify_one();
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
                request_cancel.store(true, Ordering::Release);
                self.state.1.notify_one();
                return Err("Preparation interrupted during timing certification".into());
            }
            match result.recv_timeout(Duration::from_millis(10)) {
                Ok(result) => return result,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => (),
                Err(_) => return Err("Timing worker stopped".into()) }
        }
    }
    pub fn shared() -> Arc<Self> {
        let mut shared = SHARED
            .get_or_init(|| Mutex::new(Weak::new()))
            .lock()
            .unwrap();
        if let Some(service) = shared.upgrade() {
            return service;
        }
        let state = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let cancel = Arc::new(AtomicBool::new(false));
        let busy = Arc::new(AtomicBool::new(BACKGROUND_BUSY.load(Ordering::Acquire)));
        let work = state.clone();
        let stop = cancel.clone();
        let paused = busy.clone();
        let worker = std::thread::spawn(move || loop {
            let job = {
                let (lock, wake) = &*work;
                let mut state = lock.lock().unwrap();
                while !state.stopped && state.jobs.is_empty() {
                    state = wake
                        .wait_timeout(state, Duration::from_millis(100))
                        .unwrap()
                        .0;
                }
                if state.stopped {
                    return;
                }
                state.jobs.pop_front().unwrap()
            };
            if let Some(prepared) = &job.prepared {
                let result = (|| {
                    if prepared.cancel.load(Ordering::Acquire) || paused.load(Ordering::Acquire) {
                        return Err("Preparation interrupted during timing certification".into());
                    }
                    let saved = load(&job.key).is_some_and(|certificate| {
                        certificate.eligible && certificate.prepared_count == Some(prepared.count)
                    });
                    if !saved {
                        scan_limited(
                            &job.path,
                            job.fps,
                            &stop,
                            &paused,
                            Some(&prepared.cancel),
                            Some(prepared.count),
                        )?;
                    }
                    if prepared.cancel.load(Ordering::Acquire)
                        || stop.load(Ordering::Acquire)
                        || paused.load(Ordering::Acquire)
                        || fingerprint(&job.path, job.fps).as_deref() != Some(job.key.as_str())
                    {
                        return Err("Prepared segment changed or certification interrupted".into());
                    }
                    let certificate = Certificate {
                        version: CERTIFICATE_VERSION,
                        key: job.key.clone(),
                        eligible: true,
                        reason: String::new(),
                        prepared_count: Some(prepared.count) };
                    if !save(&certificate) {
                        return Err("Could not save prepared timing certificate".into());
                    }
                    let mut state = work.0.lock().unwrap();
                    if state.records.len() >= 256 && !state.records.contains_key(&job.key) {
                        if let Some(old) = state
                            .records
                            .iter()
                            .find(|(_, c)| c.is_some())
                            .map(|(key, _)| key.clone())
                        {
                            state.records.remove(&old);
                        } else {
                            return Err("Timing record limit reached".into());
                        }
                    }
                    state.records.insert(job.key.clone(), Some(certificate));
                    Ok(())
                })();
                let _ = prepared.done.send(result);
                continue;
            }
            let persisted = load(&job.key);
            if persisted.is_none() && paused.load(Ordering::Acquire) {
                let mut state = work.0.lock().unwrap();
                state.jobs.push_back(job);
                drop(work.1.wait_timeout(state, Duration::from_millis(100)));
                continue;
            }
            let certificate = persisted.unwrap_or_else(|| {
                let result = scan(&job.path, job.fps, &stop, &paused);
                Certificate {
                    version: CERTIFICATE_VERSION,
                    key: job.key.clone(),
                    eligible: result.is_ok(),
                    reason: result.err().unwrap_or_default(),
                    prepared_count: None }
            });
            if stop.load(Ordering::Acquire) {
                return;
            }
            if certificate.reason == "interrupted" {
                work.0.lock().unwrap().jobs.push_front(job);
                continue;
            }
            // Metadata and durable publication occur outside the foreground state lock.
            let valid = fingerprint(&job.path, job.fps).is_some_and(|key| key == job.key);
            if valid {
                save(&certificate);
            }
            let mut state = work.0.lock().unwrap();
            if valid {
                state.records.insert(job.key, Some(certificate));
            } else {
                state.records.remove(&job.key);
            }
        });
        let service = Arc::new(Self {
            state,
            cancel,
            busy,
            worker: Some(worker) });
        *shared = Arc::downgrade(&service);
        service
    }
    pub fn eligible(&self, path: &Path, fps: f64) -> bool {
        let Some(key) = fingerprint(path, fps) else {
            return false;
        };
        let mut state = self.state.0.lock().unwrap();
        if let Some(record) = state.records.get(&key) {
            return record.as_ref().is_some_and(|c| c.eligible);
        }
        if state.jobs.len() >= 4 {
            return false;
        }
        if state.records.len() >= 256 {
            if let Some(old) = state
                .records
                .iter()
                .find(|(_, c)| c.is_some())
                .map(|(k, _)| k.clone())
            {
                state.records.remove(&old);
            } else {
                return false;
            }
        }
        state.records.insert(key.clone(), None);
        state.jobs.push_back(Job {
            path: path.to_path_buf(),
            key,
            fps,
            prepared: None });
        self.state.1.notify_one();
        false
    }
}
impl Drop for Eligibility {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.state.0.lock().unwrap().stopped = true;
        self.state.1.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn fingerprint(path: &Path, fps: f64) -> Option<String> {
    let canonical = path.canonicalize().ok()?;
    let metadata = canonical.metadata().ok()?;
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let mut digest = Sha256::new();
    digest.update(b"packet-cfr-v2-first-noncover");
    digest.update(canonical.to_string_lossy().as_bytes());
    digest.update(metadata.len().to_le_bytes());
    digest.update(modified.to_le_bytes());
    digest.update(fps.to_bits().to_le_bytes());
    Some(format!("{:x}", digest.finalize()))
}
fn sidecar(key: &str) -> Option<PathBuf> {
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let root = std::env::var_os("QUATERNIO_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()?
                .parent()
                .map(|p| p.join("data"))
        })?;
    if !root.is_absolute() {
        return None;
    }
    let directory = root.join("preview-timing");
    for p in directory.ancestors() {
        if let Ok(m) = std::fs::symlink_metadata(p) {
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if m.file_attributes() & 0x400 != 0 {
                    return None;
                }
            }
            if m.file_type().is_symlink() {
                return None;
            }
        }
    }
    std::fs::create_dir_all(&directory).ok()?;
    Some(directory.join(format!("{key}.json")))
}
fn load(key: &str) -> Option<Certificate> {
    let path = sidecar(key)?;
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if !metadata.is_file() || metadata.len() > 4096 || metadata.file_type().is_symlink() {
        return None;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return None;
        }
    }
    let certificate: Certificate = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    current_certificate(certificate, key)
}
fn current_certificate(certificate: Certificate, key: &str) -> Option<Certificate> {
    (certificate.version == CERTIFICATE_VERSION && certificate.key == key).then_some(certificate)
}
fn save(certificate: &Certificate) -> bool {
    if load(&certificate.key).is_some_and(|old| {
        old.eligible == certificate.eligible
            && old.reason == certificate.reason
            && old.prepared_count == certificate.prepared_count
    }) {
        return true;
    }
    let Some(path) = sidecar(&certificate.key) else {
        return false;
    };
    let stage = path.with_extension(format!("{}.staged", std::process::id()));
    let result = (|| {
        let bytes = serde_json::to_vec(certificate).ok()?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stage)
            .ok()?;
        file.write_all(&bytes).and_then(|_| file.sync_all()).ok()?;
        drop(file);
        // Timing certificates are disposable; replace an older schema proof.
        // Only this worker writes this key. No viewer entry is published yet.
        if path.exists() {
            std::fs::remove_file(&path).ok()?;
        }
        std::fs::rename(&stage, &path).ok()
    })();
    if result.is_none() {
        let _ = std::fs::remove_file(stage);
    }
    result.is_some()
}
fn rational(value: &str) -> Result<(i128, i128), String> {
    let (a, b) = value.split_once('/').ok_or("missing rational")?;
    let a = a.parse::<u32>().map_err(|_| "bad numerator")?;
    let b = b.parse::<u32>().map_err(|_| "bad denominator")?;
    if a == 0 || b == 0 {
        return Err("zero timing grid".into());
    }
    Ok((a as i128, b as i128))
}
fn validate_grid(
    mut pts: Vec<i64>,
    origin: i64,
    tb: (i128, i128),
    rate: (i128, i128),
) -> Result<(), String> {
    if pts.len() < 2 || pts.len() > MAX_PACKETS {
        return Err("unsupported packet count".into());
    }
    pts.sort_unstable();
    if pts.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err("duplicate PTS".into());
    }
    if pts[0] != origin {
        return Err("packet origin differs from stream origin".into());
    }
    for (i, pts) in pts.iter().enumerate() {
        if i > 0 && *pts <= origin {
            return Err("duplicate PTS".into());
        }
        let delta = *pts as i128 - origin as i128;
        let position = delta * tb.0 * rate.0;
        let expected = i as i128 * tb.1 * rate.1;
        // The demuxer timebase can quantize a constant-rate picture by half a tick.
        if (position - expected).abs() * 2 > tb.0 * rate.0 + 2 {
            return Err("nonuniform or duplicate presentation timestamps".into());
        }
    }
    Ok(())
}
fn scan(path: &Path, fps: f64, cancel: &AtomicBool, busy: &AtomicBool) -> Result<(), String> {
    scan_limited(path, fps, cancel, busy, None, None)
}
fn scan_limited(
    path: &Path,
    fps: f64,
    cancel: &AtomicBool,
    busy: &AtomicBool,
    request_cancel: Option<&AtomicBool>,
    expected_count: Option<usize>,
) -> Result<(), String> {
    let tool = std::env::var_os("QUATERNIO_PREVIEW_FFPROBE")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|e| {
                    e.parent().map(|p| {
                        p.join(if cfg!(windows) {
                            "ffprobe.exe"
                        } else {
                            "ffprobe"
                        })
                    })
                })
        })
        .ok_or("ffprobe unavailable")?;
    let mut command = Command::new(tool);
    command.args(["-v","error","-select_streams","V:0","-show_packets","-show_streams","-show_format","-show_entries","packet=pts:stream=codec_name,avg_frame_rate,r_frame_rate,time_base,start_pts,nb_frames:format=format_name","-of","compact=p=1:nk=0"]).arg(path).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000 | 0x00004000);
    }
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let output = child.stdout.take().ok_or("missing scan pipe")?;
    let invalid = Arc::new(AtomicBool::new(false));
    let read_invalid = invalid.clone();
    let reader = std::thread::spawn(
        move || -> Result<(Vec<i64>, HashMap<String, String>, String), String> {
            let mut reader = std::io::BufReader::new(output);
            let mut pts = Vec::new();
            let mut stream = HashMap::new();
            let mut format = String::new();
            loop {
                let mut line = Vec::new();
                let n = (&mut reader)
                    .take(8193)
                    .read_until(b'\n', &mut line)
                    .map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                if n > 8192 {
                    read_invalid.store(true, Ordering::Release);
                    return Err("scan line exceeds bound".into());
                }
                let text = std::str::from_utf8(&line)
                    .map_err(|_| "invalid probe text")?
                    .trim();
                let mut fields = text.split('|');
                let kind = fields.next().unwrap_or_default();
                let values: HashMap<_, _> = fields
                    .filter_map(|v| v.split_once('='))
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
                match kind {
                    "packet" => {
                        if pts.len() >= expected_count.unwrap_or(MAX_PACKETS) {
                            read_invalid.store(true, Ordering::Release);
                            return Err("packet index exceeds bound".into());
                        }
                        let value = values
                            .get("pts")
                            .and_then(|s| s.parse().ok())
                            .ok_or("packet timestamp missing")?;
                        pts.push(value);
                    }
                    "stream" => stream = values,
                    "format" => format = values.get("format_name").cloned().unwrap_or_default(),
                    _ => {}
                }
            }
            Ok((pts, stream, format))
        },
    );
    let deadline =
        Instant::now() + Duration::from_secs(if expected_count.is_some() { 15 } else { 90 });
    let result = loop {
        if cancel.load(Ordering::Acquire)
            || request_cancel.is_some_and(|flag| flag.load(Ordering::Acquire))
            || busy.load(Ordering::Acquire)
            || invalid.load(Ordering::Acquire)
            || Instant::now() >= deadline
        {
            let interrupted = cancel.load(Ordering::Acquire) || busy.load(Ordering::Acquire);
            let _ = child.kill();
            let _ = child.wait();
            break Err(if interrupted {
                "interrupted"
            } else {
                "scan limit exceeded"
            }
            .to_string());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                break if status.success() {
                    Ok(())
                } else {
                    Err("packet scan failed".into())
                }
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(e.to_string());
            }
        }
    };
    let decoded = reader.join().map_err(|_| "packet reader failed")?;
    result?;
    let (pts, stream, format) = decoded?;
    if expected_count.is_some_and(|count| count != pts.len()) {
        return Err("Prepared packet count differs from verified pictures".into());
    }
    let codec = stream.get("codec_name").ok_or("missing codec")?;
    if !matches!(codec.as_str(), "h264" | "hevc" | "ffv1" | "mjpeg")
        || !format
            .split(',')
            .any(|name| matches!(name, "mov" | "mp4" | "matroska" | "avi"))
    {
        return Err("codec/container packet mapping unsupported".into());
    }
    let get = |name: &str| {
        stream
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| format!("missing {name}"))
    };
    let avg = rational(get("avg_frame_rate")?)?;
    let nominal = rational(get("r_frame_rate")?)?;
    let tb = rational(get("time_base")?)?;
    if avg.0 * nominal.1 != nominal.0 * avg.1 || (avg.0 as f64 / avg.1 as f64 - fps).abs() > 0.001 {
        return Err("frame-rate agreement failed".into());
    }
    if let Some(count) = stream
        .get("nb_frames")
        .and_then(|value| value.parse::<usize>().ok())
    {
        if count != pts.len() {
            return Err("sample/frame count differs".into());
        }
    }
    let origin = get("start_pts")?
        .parse::<i64>()
        .map_err(|_| "missing stream origin")?;
    validate_grid(pts, origin, tb, avg)
}
