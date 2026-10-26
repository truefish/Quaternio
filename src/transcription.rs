//! Optional local speech recognition. Native inference is isolated from the editor.
use crate::project::{captions::Cue, Project};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};
const MODEL_REV: &str = "362722b3fdcd2300b58a8286933ead1c48619667";
const MODEL_NAME: &str = "ggml-base-q5_1.bin";
const MODEL_SIZE: u64 = 59_707_625;
const MODEL_SHA: &str = "422f1ae452ade6f30a004d7e5c6a43195e4433bc370bf23fac9cc591f01a8898";
pub fn model_path() -> PathBuf {
    crate::preferences::data_dir()
        .join("models")
        .join(MODEL_NAME)
}
pub fn installed() -> bool {
    fs::metadata(model_path()).is_ok_and(|m| m.is_file() && m.len() == MODEL_SIZE)
}
pub fn delete_model() -> Result<(), String> {
    match fs::remove_file(model_path()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}
#[derive(Serialize, Deserialize, Debug)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    pub text: String,
}
#[derive(Serialize, Deserialize)]
enum Event {
    Progress(String),
    Done(Result<Vec<Segment>, String>),
}
pub enum Output {
    Installed,
    Transcript(Vec<Segment>),
}
pub struct Job {
    pub cancel: Arc<AtomicBool>,
    pub status: Arc<Mutex<String>>,
    rx: mpsc::Receiver<Result<Output, String>>,
}
impl Drop for Job {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}
impl Job {
    fn spawn(
        work: impl FnOnce(Arc<AtomicBool>, Arc<Mutex<String>>) -> Result<Output, String>
            + Send
            + 'static,
    ) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new("Starting…".into()));
        let (tx, rx) = mpsc::channel();
        let c = cancel.clone();
        let s = status.clone();
        std::thread::spawn(move || {
            let _ = tx.send(work(c, s));
        });
        Self { cancel, status, rx }
    }
    pub fn poll(&self) -> Option<Result<Output, String>> {
        match self.rx.try_recv() {
            Ok(r) => Some(r),
            Err(mpsc::TryRecvError::Disconnected) => {
                Some(Err("Speech worker stopped unexpectedly".into()))
            }
            Err(_) => None,
        }
    }
    pub fn download() -> Self {
        Self::spawn(|cancel, status| {
            let destination = model_path();
            fs::create_dir_all(destination.parent().unwrap()).map_err(|e| e.to_string())?;
            let temporary = destination.with_extension(format!("{}.part", uuid::Uuid::new_v4()));
            let guard = Temporary(temporary.clone());
            let mut out = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .map_err(|e| e.to_string())?;
            let url = format!(
                "https://huggingface.co/ggerganov/whisper.cpp/resolve/{MODEL_REV}/{MODEL_NAME}"
            );
            let response = ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(15))
                .timeout_read(Duration::from_secs(15))
                .build()
                .get(&url)
                .call()
                .map_err(|e| format!("Model download failed: {e}"))?;
            let mut input = response.into_reader();
            let mut bytes = [0u8; 65536];
            let mut size = 0u64;
            let mut hash = Sha256::new();
            loop {
                if cancel.load(Ordering::Acquire) {
                    return Err("Model download cancelled".into());
                }
                let n = input.read(&mut bytes).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                size += n as u64;
                if size > MODEL_SIZE {
                    return Err("Model exceeded its expected size".into());
                }
                hash.update(&bytes[..n]);
                out.write_all(&bytes[..n])
                    .map_err(|e| format!("Could not save model: {e}"))?;
                *status.lock().unwrap() =
                    format!("Downloading speech model · {}%", size * 100 / MODEL_SIZE);
            }
            if size != MODEL_SIZE || format!("{:x}", hash.finalize()) != MODEL_SHA {
                return Err("Model verification failed; the download was discarded".into());
            }
            out.sync_all().map_err(|e| e.to_string())?;
            drop(out);
            if cancel.load(Ordering::Acquire) {
                return Err("Model download cancelled".into());
            }
            fs::rename(&temporary, destination).map_err(|e| e.to_string())?;
            drop(guard);
            Ok(Output::Installed)
        })
    }
    pub fn transcribe(project: Project) -> Self {
        Self::spawn(move |cancel, status| {
            let mut command = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
            command
                .arg("--speech-worker")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x08000000);
            }
            let mut child = command.spawn().map_err(|e| e.to_string())?;
            let mut control = child.stdin.take().unwrap();
            if let Err(e) = serde_json::to_writer(&mut control, &project)
                .and_then(|_| control.write_all(b"\n").map_err(serde_json::Error::io))
            {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e.to_string());
            }
            let output = child.stdout.take().unwrap();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(output);
                loop {
                    let line = read_line(&mut reader, 8 * 1024 * 1024);
                    let event = match line {
                        Ok(Some(line)) => {
                            serde_json::from_slice::<Event>(&line).map_err(|e| e.to_string())
                        }
                        Ok(None) => break,
                        Err(e) => Err(e),
                    };
                    let done = matches!(event, Ok(Event::Done(_))) || event.is_err();
                    if tx.send(event).is_err() || done {
                        break;
                    }
                }
            });
            let mut stopping = None;
            let result = loop {
                if cancel.load(Ordering::Acquire) && stopping.is_none() {
                    let _ = control.write_all(b"cancel\n");
                    let _ = control.flush();
                    stopping = Some(Instant::now());
                }
                if stopping.is_some_and(|t: Instant| t.elapsed() > Duration::from_secs(3)) {
                    let _ = child.kill();
                    break Err("Transcription cancelled".into());
                }
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(Ok(Event::Progress(message))) => *status.lock().unwrap() = message,
                    Ok(Ok(Event::Done(result))) => break result,
                    Ok(Err(e)) => break Err(e),
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        break Err("Speech process exited before completing".into())
                    }
                    Err(_) => {}
                }
            };
            drop(control);
            let _ = child.kill();
            let _ = child.wait();
            if cancel.load(Ordering::Acquire) {
                return Err("Transcription cancelled".into());
            }
            result.map(Output::Transcript)
        })
    }
}
struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn read_line(reader: &mut impl BufRead, limit: usize) -> Result<Option<Vec<u8>>, String> {
    let mut line = Vec::new();
    loop {
        let bytes = reader.fill_buf().map_err(|e| e.to_string())?;
        if bytes.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err("Truncated speech message".into())
            };
        }
        let n = bytes
            .iter()
            .position(|b| *b == b'\n')
            .map(|n| n + 1)
            .unwrap_or(bytes.len());
        if line.len() + n > limit {
            return Err("Speech message exceeds its size limit".into());
        }
        let done = bytes[n - 1] == b'\n';
        line.extend_from_slice(&bytes[..n]);
        reader.consume(n);
        if done {
            return Ok(Some(line));
        }
    }
}
fn emit(event: Event) {
    let mut out = std::io::stdout().lock();
    let _ = serde_json::to_writer(&mut out, &event);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}
pub fn run_if_requested() -> Result<bool, String> {
    if std::env::args_os()
        .nth(1)
        .is_none_or(|a| a != "--speech-worker")
    {
        return Ok(false);
    }
    let result = serve();
    let success = result.is_ok();
    emit(Event::Done(result));
    if success {
        Ok(true)
    } else {
        Err("Speech worker could not complete".into())
    }
}
fn serve() -> Result<Vec<Segment>, String> {
    let mut input = BufReader::new(std::io::stdin());
    let bytes = read_line(&mut input, 32 * 1024 * 1024)?.ok_or("Missing speech job")?;
    let mut project: Project = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    project.migrate()?;
    if project.sequence.duration_frames() <= 0 {
        return Err("This sequence is empty".into());
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let watch = cancel.clone();
    std::thread::spawn(move || {
        let _ = read_line(&mut input, 1024);
        watch.store(true, Ordering::Release);
    });
    emit(Event::Progress("Preparing sequence audio…".into()));
    let path = model_path();
    let mut file = fs::File::open(&path).map_err(|_| "Download the speech model first")?;
    let mut hash = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err("Transcription cancelled".into());
        }
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    if format!("{:x}", hash.finalize()) != MODEL_SHA {
        return Err("Speech model is damaged; delete it and download it again".into());
    }
    crate::export::with_speech_audio(&project, &cancel, |pcm| infer(pcm, &path, cancel.clone()))
}
fn infer(pcm: &Path, model: &Path, cancel: Arc<AtomicBool>) -> Result<Vec<Segment>, String> {
    use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};
    let mut config = WhisperContextParameters::default();
    config.use_gpu(false);
    let context =
        WhisperContext::new_with_params(model.to_str().ok_or("Model path is not UTF-8")?, config)
            .map_err(|e| e.to_string())?;
    let mut input = fs::File::open(pcm).map_err(|e| e.to_string())?;
    let total = input.metadata().map_err(|e| e.to_string())?.len() / 8;
    let mut result = Vec::new();
    let chunk = 30 * 48_000u64;
    for begin in (0..total).step_by(chunk as usize) {
        if cancel.load(Ordering::Acquire) {
            return Err("Transcription cancelled".into());
        }
        let count = (total - begin).min(chunk);
        let mut bytes = vec![0; count as usize * 8];
        input
            .seek(SeekFrom::Start(begin * 8))
            .and_then(|_| input.read_exact(&mut bytes))
            .map_err(|e| e.to_string())?;
        let audio: Vec<f32> = bytes
            .chunks_exact(24)
            .map(|frame| {
                let mut sum = 0.;
                for sample in frame.chunks_exact(4) {
                    sum += f32::from_le_bytes(sample.try_into().unwrap());
                }
                sum / 6.
            })
            .collect();
        if audio.iter().all(|v| v.abs() < 0.00001) {
            continue;
        }
        let mut state = context.create_state().map_err(|e| e.to_string())?;
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_n_threads(
            std::thread::available_parallelism()
                .map_or(2, usize::from)
                .saturating_sub(2)
                .clamp(1, 8) as i32,
        );
        params.set_language(None);
        params.set_no_context(true);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_print_special(false);
        // whisper-rs 0.16's safe abort wrapper passes a boxed trait object to a
        // concrete-closure trampoline. Keep our AtomicBool alive through full().
        unsafe extern "C" fn abort(pointer: *mut std::ffi::c_void) -> bool {
            unsafe { pointer.cast::<AtomicBool>().as_ref() }
                .is_none_or(|flag| flag.load(Ordering::Acquire))
        }
        unsafe {
            params.set_abort_callback(Some(abort));
            params.set_abort_callback_user_data(Arc::as_ptr(&cancel) as *mut std::ffi::c_void);
        }
        params.set_progress_callback_safe(move |percent| {
            emit(Event::Progress(format!(
                "Transcribing locally · {}%",
                ((begin as f64 + count as f64 * percent as f64 / 100.) / total.max(1) as f64 * 100.)
                    .round() as u32
            )))
        });
        state.full(params, &audio).map_err(|e| e.to_string())?;
        for segment in state.as_iter() {
            let start = begin as f64 / 48_000. + segment.start_timestamp() as f64 / 100.;
            let end = (begin as f64 / 48_000. + segment.end_timestamp() as f64 / 100.)
                .min(total as f64 / 48_000.);
            let text = segment
                .to_str_lossy()
                .map_err(|e| e.to_string())?
                .trim()
                .to_string();
            if end > start && !text.is_empty() {
                if result.len() >= 10_000 {
                    return Err("Transcript exceeds 10,000 segments".into());
                }
                result.push(Segment { start, end, text });
            }
        }
    }
    Ok(result)
}
pub fn cues(segments: Vec<Segment>, rate: crate::project::FrameRate) -> Vec<Cue> {
    let mut end = 0;
    segments
        .into_iter()
        .filter_map(|s| {
            let start = rate.seconds_to_frame(s.start).max(end);
            let next = rate.seconds_to_frame(s.end);
            if next <= start {
                return None;
            }
            end = next;
            Some(Cue {
                start,
                end: next,
                text: s.text,
            })
        })
        .collect()
}
