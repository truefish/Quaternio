use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use serde_json::Value;

use super::audio::Pcm;
use super::composite::{FrameSource, ImageBuf};
use super::native_preview::{NativeDecodeError, NativePreviewClient, NativePreviewStats};
use super::{optimized::OptimizedCache, PreviewIntent};
use crate::project::{Media, MediaKind};

static PREVIEW_HARDWARE: AtomicBool = AtomicBool::new(true);
pub fn set_preview_hardware(enabled: bool) { PREVIEW_HARDWARE.store(enabled, Ordering::Release); }

pub fn find_ffmpeg() -> Result<PathBuf, String> {
    crate::media_engine::discover().map(|engine| engine.ffmpeg)
}

pub fn find_ffprobe() -> Result<PathBuf, String> {
    crate::media_engine::discover().map(|engine| engine.ffprobe)
}

fn media_command(tool: &Path) -> Command {
    #[cfg(windows)]
    let mut command = Command::new(tool);
    #[cfg(not(windows))]
    let command = Command::new(tool);
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    command
}

fn command_output(tool: &Path, args: &[String]) -> Result<std::process::Output, String> {
    media_command(tool)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| err.to_string())
}

struct DecodeProcess {
    child: Child,
    reaped: bool }

impl Drop for DecodeProcess {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Drain both pipes while the owner polls cancellation and child completion.
/// The child is always reaped before returning, including cancellation/errors.
fn command_output_cancellable(
    tool: &Path,
    args: &[String],
    cancel: &AtomicBool,
) -> Result<std::process::Output, String> {
    command_output_with_cancels(tool, args, cancel, None)
}

fn command_output_with_cancels(
    tool: &Path,
    args: &[String],
    cancel: &AtomicBool,
    request: Option<&AtomicBool>,
) -> Result<std::process::Output, String> {
    command_output_with_deadline(tool, args, cancel, request, None)
}

fn command_output_with_deadline(
    tool: &Path,
    args: &[String],
    cancel: &AtomicBool,
    request: Option<&AtomicBool>,
    timeout: Option<Duration>,
) -> Result<std::process::Output, String> {
    let started = std::time::Instant::now();
    let cancelled = || {
        cancel.load(Ordering::Acquire) || request.is_some_and(|flag| flag.load(Ordering::Acquire))
    };
    if cancelled() {
        return Err("Media decode cancelled".into());
    }
    let child = media_command(tool)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let mut process = DecodeProcess {
        child,
        reaped: false };
    let mut stdout = process
        .child
        .stdout
        .take()
        .ok_or("Missing decoder stdout")?;
    let mut stderr = process
        .child
        .stderr
        .take()
        .ok_or("Missing decoder stderr")?;
    std::thread::scope(|scope| {
        let samples = scope.spawn(move || {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes).map(|_| bytes)
        });
        let errors = scope.spawn(move || {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).map(|_| bytes)
        });
        let status = loop {
            if cancelled() || timeout.is_some_and(|limit| started.elapsed() >= limit) {
                let _ = process.child.kill();
            }
            match process.child.try_wait() {
                Ok(Some(status)) => {
                    process.reaped = true;
                    break status;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(error) => {
                    let _ = process.child.kill();
                    process.reaped = process.child.wait().is_ok();
                    return Err(error.to_string());
                }
            }
        };
        let stdout_result = samples.join().map_err(|_| "Decoder sample reader failed")?;
        let stderr_result = errors.join().map_err(|_| "Decoder error reader failed")?;
        if cancelled() {
            return Err("Media decode cancelled".into());
        }
        if timeout.is_some_and(|limit| started.elapsed() >= limit) {
            return Err("Media metadata probe timed out".into());
        }
        let bytes = stdout_result.map_err(|error| error.to_string())?;
        let stderr = stderr_result.map_err(|error| error.to_string())?;
        Ok(std::process::Output {
            status,
            stdout: bytes,
            stderr })
    })
}

fn require_decode_success(
    output: &std::process::Output,
    kind: &str,
    path: &Path,
) -> Result<(), String> {
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr);
    let detail = detail.trim();
    if detail.is_empty() {
        Err(format!(
            "FFmpeg {kind} decode failed for {} ({})",
            path.display(),
            output.status
        ))
    } else {
        Err(format!(
            "FFmpeg {kind} decode failed for {}: {}",
            path.display(),
            detail.chars().take(2048).collect::<String>()
        ))
    }
}

pub fn parse_probe(json: &str, path: &Path) -> Result<Media, String> {
    let value: Value = serde_json::from_str(json).map_err(|err| err.to_string())?;
    let streams = value
        .get("streams")
        .and_then(|item| item.as_array())
        .cloned()
        .unwrap_or_default();
    let video = streams.iter().find(|stream| {
        stream.get("codec_type").and_then(|item| item.as_str()) == Some("video")
            && stream
                .get("disposition")
                .and_then(|item| item.get("attached_pic"))
                .and_then(|item| item.as_i64())
                != Some(1)
    });
    let audio = streams
        .iter()
        .find(|stream| stream.get("codec_type").and_then(|item| item.as_str()) == Some("audio"));
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let image_ext = matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "bmp" | "tif" | "tiff" | "webp" | "tga" | "gif" | "ppm"
    );
    let frames = video
        .and_then(|stream| stream.get("nb_frames"))
        .map(number_of)
        .unwrap_or(0.0);
    let duration = value
        .get("format")
        .and_then(|format| format.get("duration"))
        .map(number_of)
        .filter(|value| *value > 0.0)
        .or_else(|| {
            video
                .and_then(|stream| stream.get("duration"))
                .map(number_of)
        })
        .or_else(|| {
            audio
                .and_then(|stream| stream.get("duration"))
                .map(number_of)
        })
        .unwrap_or(0.0);
    let animated_gif = ext == "gif" && (frames > 1.0 || duration > 0.2);
    let kind = if image_ext && !animated_gif {
        MediaKind::Still
    } else if video.is_some() {
        MediaKind::Video
    } else if audio.is_some() {
        MediaKind::Audio
    } else {
        return Err(format!("{} has no picture or sound", path.display()));
    };
    let width = video
        .and_then(|stream| stream.get("width"))
        .map(number_of)
        .unwrap_or(0.0) as u32;
    let height = video
        .and_then(|stream| stream.get("height"))
        .map(number_of)
        .unwrap_or(0.0) as u32;
    let rate = video
        .and_then(|stream| {
            stream
                .get("r_frame_rate")
                .or_else(|| stream.get("avg_frame_rate"))
        })
        .and_then(|item| item.as_str())
        .and_then(parse_rate)
        .unwrap_or(if kind == MediaKind::Still { 0.0 } else { 24.0 });
    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("Media")
        .to_string();
    Ok(Media {
        color: crate::color::management::Metadata{primaries:video.and_then(|v|v["color_primaries"].as_str()).unwrap_or("").into(),transfer:video.and_then(|v|v["color_transfer"].as_str()).unwrap_or("").into(),matrix:video.and_then(|v|v["color_space"].as_str()).unwrap_or("").into(),range:video.and_then(|v|v["color_range"].as_str()).unwrap_or("").into(),bit_depth:video.and_then(|v|v["bits_per_raw_sample"].as_str()).and_then(|v|v.parse().ok()).unwrap_or(0)},
        id: 0,
        name,
        bin: String::new(),
        path: path.to_path_buf(),
        kind,
        duration_secs: if kind == MediaKind::Still {
            0.0
        } else {
            duration.max(0.0)
        },
        width,
        height,
        has_video: kind != MediaKind::Audio,
        has_audio: audio.is_some() || kind == MediaKind::Audio,
        source_fps: rate,
        source_in_mark: None,
        source_out_mark: None })
}

fn number_of(value: &Value) -> f64 {
    match value {
        Value::Number(number) => number.as_f64().unwrap_or(0.0),
        Value::String(text) => text.parse().unwrap_or(0.0),
        _ => 0.0 }
}

fn parse_rate(text: &str) -> Option<f64> {
    let mut parts = text.split('/');
    let num: f64 = parts.next()?.parse().ok()?;
    let den: f64 = parts.next().unwrap_or("1").parse().ok()?;
    if den == 0.0 {
        None
    } else {
        Some(num / den)
    }
}

pub fn probe_path(path: &Path) -> Result<Media, String> {
    probe_path_cancellable(path, &AtomicBool::new(false))
}

/// Metadata probes share the decoder's kill/reap path, with a finite deadline.
/// Imports can cancel active ffprobe processes instead of waiting for a file.
pub fn probe_path_cancellable(path: &Path, cancel: &AtomicBool) -> Result<Media, String> {
    let before = origin_stamp(path);
    let tool = find_ffprobe()?;
    let uploaded = path.canonicalize().ok().zip(crate::preferences::data_dir().join("session-media").canonicalize().ok())
        .is_some_and(|(file, root)| file.starts_with(root));
    let mut arguments: Vec<String> = Vec::new();
    if uploaded {
        // Uploaded media must be self-contained: never follow playlists or network URLs.
        arguments.extend(["-protocol_whitelist", "file,pipe", "-format_whitelist",
            "mov,matroska,webm,avi,flv,mpeg,mpegts,ogg,mp3,aac,wav,flac,aiff,jpeg_pipe,png_pipe,bmp_pipe,webp_pipe,tiff_pipe,gif,asf,mxf,rm,ape,wv,ac3,eac3,dts,au,amr"].map(str::to_owned));
    }
    arguments.extend([
            "-v".into(),
            "error".into(),
            "-print_format".into(),
            "json".into(),
            "-show_entries".into(),
            "format=duration,start_time:stream=index,codec_type,width,height,r_frame_rate,avg_frame_rate,nb_frames,duration,start_time,color_primaries,color_transfer,color_space,color_range,bits_per_raw_sample:stream_disposition=attached_pic".into(),
            path.display().to_string(),
        ]);
    let output = command_output_with_deadline(
        &tool,
        &arguments,
        cancel,
        None,
        Some(Duration::from_secs(30)),
    )?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(if err.trim().is_empty() {
            format!("Could not read {}", path.display())
        } else {
            err.trim().chars().take(2048).collect()
        });
    }
    if cancel.load(Ordering::Acquire) {
        return Err("Media import cancelled".into());
    }
    if before != origin_stamp(path) {
        return Err(format!("Source changed during metadata probe: {}", path.display()));
    }
    let json = String::from_utf8_lossy(&output.stdout);
    if let Ok(origin) = probe_video_origin(&json) {
        if before == origin_stamp(path) {
            if let Some(stamp) = before {
                remember_video_origin(path, stamp, origin);
            }
        }
    }
    parse_probe(&json, path)
}

type OriginStamp = (u64, std::time::SystemTime);
#[path = "video_origin_cache.rs"]
mod video_origin_cache;
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
struct VideoOrigin {
    seconds: f64,
    stream: u64,
    #[serde(default)]
    frame_count: Option<u64>,
    #[serde(default)]
    nominal_fps: f64 }
impl VideoOrigin {
    fn bounded_frame(self, index: i64, fps: f64) -> i64 {
        // Nearest-frame sampling can round the final output sample to N for
        // an N-picture source. Hold N-1 only for that one boundary, using
        // matching nominal/average rate metadata; do not mask arbitrary EOFs,
        // damaged pictures, or timestamps far beyond the video stream.
        match self.frame_count.filter(|n| *n > 0 && *n <= i64::MAX as u64) {
            Some(count) if index == count as i64 && (fps-self.nominal_fps).abs() < 0.001 => index-1,
            _ => index,
        }
    }
}
static VIDEO_ORIGINS: OnceLock<Mutex<VecDeque<(PathBuf, OriginStamp, VideoOrigin)>>> =
    OnceLock::new();
fn origin_stamp(path: &Path) -> Option<OriginStamp> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.len(), metadata.modified().ok()?))
}
fn probe_video_origin(json: &str) -> Result<VideoOrigin, String> {
    let document: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let stream = document["streams"]
        .as_array()
        .and_then(|streams| {
            streams.iter().find(|s| {
                s["codec_type"] == "video" && s["disposition"]["attached_pic"].as_i64() != Some(1)
            })
        })
        .ok_or("No video stream timestamp source")?;
    let value = stream
        .get("start_time")
        .filter(|v| v.as_str() != Some("N/A"))
        .or_else(|| document["format"].get("start_time"));
    let origin = value.map(number_of).unwrap_or(0.0);
    let nominal = stream["r_frame_rate"].as_str().and_then(parse_rate).unwrap_or(0.0);
    let average = stream["avg_frame_rate"].as_str().and_then(parse_rate).unwrap_or(0.0);
    let frame_count = if nominal > 0.0 && (nominal-average).abs() < 0.001 {
        stream["nb_frames"].as_str().and_then(|n| n.parse::<u64>().ok()).or_else(|| stream["nb_frames"].as_u64())
    } else { None };
    if origin.is_finite() {
        Ok(VideoOrigin {
            seconds: origin,
            frame_count,
            nominal_fps: nominal,
            stream: stream["index"]
                .as_u64()
                .ok_or("Missing video stream index")? })
    } else {
        Err("Invalid video timestamp origin".into())
    }
}
fn remember_video_origin(path: &Path, stamp: OriginStamp, origin: VideoOrigin) {
    let mut cache = VIDEO_ORIGINS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache.retain(|(old, _, _)| old != path);
    while cache.len() >= 128 {
        cache.pop_front();
    }
    cache.push_back((path.into(), stamp, origin));
    drop(cache);
    video_origin_cache::save(path, stamp, origin);
}
fn video_origin(
    path: &Path,
    cancel: Option<&AtomicBool>,
    request: Option<&AtomicBool>,
) -> Result<VideoOrigin, String> {
    let cancelled = || {
        cancel.is_some_and(|f| f.load(Ordering::Acquire))
            || request.is_some_and(|f| f.load(Ordering::Acquire))
    };
    if cancelled() {
        return Err("Media decode cancelled".into());
    }
    let stamp = origin_stamp(path);
    if let Some(stamp) = stamp {
        let cache = VIDEO_ORIGINS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some((_, _, origin)) = cache.iter().find(|(old, s, _)| old == path && *s == stamp) {
            return Ok(*origin);
        }
    }
    if let Some(stamp) = stamp {
        if let Some(origin) = video_origin_cache::load(path, stamp) {
            if cancelled() {
                return Err("Media decode cancelled".into());
            }
            if origin_stamp(path) == Some(stamp) {
                remember_video_origin(path, stamp, origin);
                return Ok(origin);
            }
        }
    }
    let args = [
        "-v".into(),
        "error".into(),
        "-show_streams".into(),
        "-show_format".into(),
        "-of".into(),
        "json".into(),
        path.display().to_string(),
    ];
    let none = AtomicBool::new(false);
    let output =
        command_output_with_cancels(&find_ffprobe()?, &args, cancel.unwrap_or(&none), request)?;
    require_decode_success(&output, "timestamp probe", path)?;
    let origin = probe_video_origin(&String::from_utf8_lossy(&output.stdout))?;
    if origin_stamp(path) != stamp {
        return Err("Source changed during timestamp probe".into());
    }
    if cancelled() {
        return Err("Media decode cancelled".into());
    }
    if let Some(stamp) = stamp {
        remember_video_origin(path, stamp, origin);
    }
    Ok(origin)
}

fn frame_timestamp_boundary(index: i64, fps: f64, origin: f64) -> f64 {
    origin + (index.max(0) as f64 - 0.5) / fps
}

struct CliFrameStart {
    seek: Option<f64>,
    stream: String,
    select: String }
fn cli_frame_start(
    path: &Path,
    index: i64,
    fps: f64,
    cancel: Option<&AtomicBool>,
    request: Option<&AtomicBool>,
) -> Result<CliFrameStart, String> {
    if cancel.is_some_and(|f| f.load(Ordering::Acquire))
        || request.is_some_and(|f| f.load(Ordering::Acquire))
    {
        return Err("Media decode cancelled".into());
    }
    if index <= 2 {
        // A bounded prefix needs no timestamp origin, demux seek or probe.
        // Uppercase V excludes cover art, matching probe_video_origin.
        return Ok(CliFrameStart {
            seek: None,
            stream: "0:V:0".into(),
            select: format!("select=gte(n\\,{})", index.max(0)) });
    }
    let clock = video_origin(path, cancel, request)?;
    let index = clock.bounded_frame(index, fps);
    let (_, time) = seek_window(index, fps);
    Ok(CliFrameStart {
        seek: Some(clock.seconds + time),
        stream: format!("0:{}", clock.stream),
        select: format!(
            "select=gte(t\\,{:.12})",
            frame_timestamp_boundary(index, fps, clock.seconds)
        ) })
}

pub fn fitted_size(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let max_w = max_w.max(2);
    let max_h = max_h.max(2);
    if src_w == 0 || src_h == 0 {
        return (max_w & !1, max_h & !1);
    }
    let scale = (max_w as f32 / src_w as f32).min(max_h as f32 / src_h as f32);
    let w = ((src_w as f32 * scale).round() as u32).max(2) & !1;
    let h = ((src_h as f32 * scale).round() as u32).max(2) & !1;
    (w.max(2), h.max(2))
}

struct SeqDec {
    hardware: bool,
    media_id: u64,
    w: u32,
    h: u32,
    next: i64,
    child: Arc<Mutex<Child>>,
    stdout: std::io::BufReader<std::process::ChildStdout> }

impl Drop for SeqDec {
    fn drop(&mut self) {
        let mut child = self.child.lock().unwrap_or_else(|error| error.into_inner());
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[derive(Default)]
struct PreviewDecodeRequest {
    flag: Option<Arc<AtomicBool>>,
    active: Option<Weak<Mutex<Child>>> }

struct ActivePreviewDecode(Arc<Mutex<PreviewDecodeRequest>>);
impl Drop for ActivePreviewDecode {
    fn drop(&mut self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).active = None;
    }
}

struct DecodeCancellation {
    request: Arc<Mutex<PreviewDecodeRequest>>,
    flag: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    children: Arc<Mutex<Vec<Weak<Mutex<Child>>>>>,
    watcher: Option<std::thread::JoinHandle<()>> }

impl DecodeCancellation {
    fn new(flag: Arc<AtomicBool>) -> Self {
        let alive = Arc::new(AtomicBool::new(true));
        let children = Arc::new(Mutex::new(Vec::<Weak<Mutex<Child>>>::new()));
        let request = Arc::new(Mutex::new(PreviewDecodeRequest::default()));
        let worker_request = request.clone();
        let worker_flag = flag.clone();
        let worker_alive = alive.clone();
        let worker_children = children.clone();
        let watcher = std::thread::spawn(move || {
            while worker_alive.load(Ordering::Acquire) {
                if worker_flag.load(Ordering::Acquire) {
                    let active: Vec<_> = worker_children
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .iter()
                        .filter_map(Weak::upgrade)
                        .collect();
                    for child in active {
                        let mut child = child.lock().unwrap_or_else(|error| error.into_inner());
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    // Keep watching until the source is dropped: a spawn that
                    // raced cancellation must also be interrupted and reaped.
                }
                // Hold the registration lock through kill/reap. Disarming cannot
                // race a watcher snapshot and let an old request kill a reused stream.
                let request = worker_request.lock().unwrap_or_else(|e| e.into_inner());
                if request
                    .flag
                    .as_ref()
                    .is_some_and(|flag| flag.load(Ordering::Acquire))
                {
                    if let Some(child) = request.active.as_ref().and_then(Weak::upgrade) {
                        let mut child = child.lock().unwrap_or_else(|e| e.into_inner());
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                }
                drop(request);
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        Self {
            request,
            flag,
            alive,
            children,
            watcher: Some(watcher) }
    }
}

impl Drop for DecodeCancellation {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct PreparedPictureKey {
    media_id: u64,
    source: PathBuf,
    source_bytes: u64,
    source_modified: u128,
    segment: PathBuf,
    ordinal: i64,
    dimensions: (u32, u32),
    segment_dimensions: (u32, u32) }

pub struct CachingSource {
    precise: VecDeque<((u64,i64,u32,u32),Arc<super::precision::FloatImage>)>,
    hardware_failures: HashMap<u64, std::time::Instant>,
    media: HashMap<u64, Media>,
    cache: HashMap<(u64, i64, u32, u32), ImageBuf>,
    order: VecDeque<(u64, i64, u32, u32)>,
    cache_bytes: usize,
    cache_budget: usize,
    prepared_pictures: HashMap<PreparedPictureKey, ImageBuf>,
    prepared_order: VecDeque<PreparedPictureKey>,
    prepared_bytes: usize,
    sequences: VecDeque<SeqDec>,
    pub audio: HashMap<u64, Pcm>,
    decode_error: Option<String>,
    cancellation: Option<DecodeCancellation>,
    preview_request: Option<Arc<AtomicBool>>,
    preview_mode: bool,
    preview_intent: PreviewIntent,
    optimized_cache: Option<Arc<OptimizedCache>>,
    proxy_used: bool,
    proxy_sequences: VecDeque<SeqDec>,
    proxy_paths: HashMap<u64, PathBuf>,
    fingerprint_checked: HashMap<u64, std::time::Instant>,
    native_preview: Option<NativePreviewClient>,
    preview_fingerprints: HashMap<u64, Option<(u64, std::time::SystemTime)>> }

impl CachingSource {
    pub fn new() -> Self {
        Self {
            precise:Default::default(),
            hardware_failures: HashMap::new(),
            media: HashMap::new(),
            cache: HashMap::new(),
            order: VecDeque::new(),
            cache_bytes: 0,
            cache_budget: 96 * 1024 * 1024,
            prepared_pictures: HashMap::new(),
            prepared_order: VecDeque::new(),
            prepared_bytes: 0,
            sequences: VecDeque::new(),
            audio: HashMap::new(),
            decode_error: None,
            cancellation: None,
            preview_request: None,
            preview_mode: false,
            preview_intent: PreviewIntent::FinalSeek,
            optimized_cache: None,
            proxy_used: false,
            proxy_sequences: VecDeque::new(),
            proxy_paths: HashMap::new(),
            fingerprint_checked: HashMap::new(),
            native_preview: None,
            preview_fingerprints: HashMap::new() }
    }

    pub fn with_cancel(cancel: Arc<AtomicBool>) -> Self {
        let mut source = Self::new();
        source.cancellation = Some(DecodeCancellation::new(cancel));
        source
    }

    /// Configure only preview requests; exports keep the original decode/cache path.
    pub fn set_preview_request(&mut self, cancel: Option<Arc<AtomicBool>>) {
        self.preview_mode = true;
        self.proxy_used = false;
        self.native_preview
            .get_or_insert_with(NativePreviewClient::from_env);
        self.preview_request = cancel.clone();
        let guard = self
            .cancellation
            .get_or_insert_with(|| DecodeCancellation::new(Arc::new(AtomicBool::new(false))));
        let mut request = guard.request.lock().unwrap_or_else(|e| e.into_inner());
        request.active = None;
        request.flag = cancel;
        self.decode_error = None;
    }

    pub fn set_optimized_cache(&mut self, cache: Option<Arc<OptimizedCache>>) {
        // Also used after Clear: release open prepared files before disk eviction.
        self.proxy_sequences.clear();
        self.proxy_paths.clear();
        self.prepared_pictures.clear();
        self.prepared_order.clear();
        self.prepared_bytes = 0;
        if let Some(client) = self.native_preview.as_mut() {
            client.reset();
        }
        if let Some(cache) = &cache {
            cache.register_media(&self.media.values().cloned().collect::<Vec<_>>());
        }
        self.optimized_cache = cache;
    }

    pub fn set_preview_intent(&mut self, intent: PreviewIntent) {
        self.preview_intent = intent;
    }
    pub fn take_proxy_used(&mut self) -> bool {
        std::mem::take(&mut self.proxy_used)
    }

    fn may_use_prepared(&self) -> bool {
        self.preview_mode
            && matches!(
                self.preview_intent,
                PreviewIntent::Playback | PreviewIntent::Scrub
            )
    }

    fn prepared_picture(
        &mut self,
        media: &Media,
        ordinal: i64,
        w: u32,
        h: u32,
        fps: f64,
    ) -> Option<ImageBuf> {
        if !self.may_use_prepared() {
            return None;
        }
        let cache = self.optimized_cache.clone()?;
        let (source_bytes, source_modified) = self
            .preview_fingerprints
            .get(&media.id)
            .copied()
            .flatten()?;
        let source_modified = source_modified
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos();
        let prepared = cache.lookup_verified(media, ordinal, source_bytes, source_modified)?;
        if prepared.frame < 0 || prepared.width < 2 || prepared.height < 2 {
            cache.invalidate(&prepared.path);
            return None;
        }
        // Verify the live segment registry before consulting RGB: Clear and
        // invalidated segments must never be resurrected by retained pixels.
        let key = PreparedPictureKey {
            media_id: media.id,
            source: media.path.clone(),
            source_bytes,
            source_modified,
            segment: prepared.path.clone(),
            ordinal: prepared.frame,
            dimensions: (w, h),
            segment_dimensions: (prepared.width, prepared.height) };
        if let Some(image) = self.prepared_hit(&key) {
            self.proxy_used = true;
            return Some(image);
        }
        let native = self.native_preview.as_mut().and_then(|client| {
            client
                .decode(
                    &prepared.path,
                    prepared.frame,
                    fps,
                    w,
                    h,
                    self.cancellation.as_ref().map(|guard| guard.flag.as_ref()),
                    self.preview_request.as_deref(),
                )
                .ok()
                .flatten()
        });
        if self.cancelled() {
            return None;
        }
        let decoded = native.or_else(|| {
            if self
                .proxy_paths
                .insert(media.id, prepared.path.clone())
                .is_some_and(|old| old != prepared.path)
            {
                self.proxy_sequences.retain(|seq| seq.media_id != media.id);
            }
            let mut proxy_media = media.clone();
            proxy_media.path = prepared.path.clone();
            proxy_media.width = prepared.width;
            proxy_media.height = prepared.height;
            // Separate stream namespace; never put approximate pixels in original RGB cache.
            std::mem::swap(&mut self.sequences, &mut self.proxy_sequences);
            let rgb = self.read_seq_mode(media.id, prepared.frame, w, h, &proxy_media, fps, false);
            std::mem::swap(&mut self.sequences, &mut self.proxy_sequences);
            rgb
        });
        if self.cancelled() {
            return None;
        }
        match decoded {
            Some(rgb) if rgb.len() == w as usize * h as usize * 3 => {
                self.proxy_used = true;
                let image = ImageBuf {
                    width: w,
                    height: h,
                    rgb };
                self.store_prepared(key, image.clone());
                Some(image)
            }
            _ => {
                cache.invalidate(&prepared.path);
                self.prepared_pictures
                    .retain(|key, _| key.segment != prepared.path);
                self.recount_prepared();
                None
            }
        }
    }

    pub fn native_preview_stats(&self) -> Option<NativePreviewStats> {
        self.native_preview.as_ref().map(NativePreviewClient::stats)
    }

    fn check_preview_fingerprint(&mut self, media_id: u64, path: &Path) {
        if self.preview_intent != PreviewIntent::FinalSeek
            && self
                .fingerprint_checked
                .get(&media_id)
                .is_some_and(|checked| checked.elapsed() < Duration::from_millis(250))
        {
            return;
        }
        self.fingerprint_checked
            .insert(media_id, std::time::Instant::now());
        let fingerprint = std::fs::metadata(path)
            .ok()
            .and_then(|m| Some((m.len(), m.modified().ok()?)));
        if self
            .preview_fingerprints
            .insert(media_id, fingerprint)
            .is_some_and(|old| old != fingerprint)
        {
            self.cache.retain(|key, _| key.0 != media_id);
            self.precise.retain(|(key,_)|key.0!=media_id);
            self.order.retain(|key| self.cache.contains_key(key));
            self.cache_bytes = self.cache.values().map(|image| image.rgb.len()).sum();
            self.sequences.retain(|seq| seq.media_id != media_id);
            self.proxy_sequences.retain(|seq| seq.media_id != media_id);
            self.proxy_paths.remove(&media_id);
            self.prepared_pictures
                .retain(|key, _| key.media_id != media_id);
            self.recount_prepared();
            if let Some(cache) = &self.optimized_cache {
                cache.invalidate_source(path);
            }
        }
    }

    fn request_superseded(&self) -> bool {
        self.preview_request
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
    }

    fn cancelled(&self) -> bool {
        self.request_superseded()
            || self
                .cancellation
                .as_ref()
                .is_some_and(|guard| guard.flag.load(Ordering::Acquire))
    }

    pub fn set_project_media(&mut self, media: &[Media]) {
        self.decode_error = None;
        let next: HashMap<u64, Media> = media.iter().cloned().map(|item| (item.id, item)).collect();
        let unchanged = |id: u64| {
            self.media
                .get(&id)
                .zip(next.get(&id))
                .is_some_and(|(old, new)| {
                    old.path == new.path
                        && old.kind == new.kind
                        && old.width == new.width
                        && old.height == new.height
                        && old.source_fps == new.source_fps
                        && old.duration_secs == new.duration_secs
                        && old.has_video == new.has_video
                        && old.has_audio == new.has_audio
                })
        };
        // IDs can be reused by a newly opened project or by relinking media.
        self.cache.retain(|key, _| unchanged(key.0));
        self.precise.retain(|(key,_)|unchanged(key.0));
        self.order.retain(|key| self.cache.contains_key(key));
        self.cache_bytes = self.cache.values().map(|image| image.rgb.len()).sum();
        self.sequences.retain(|seq| unchanged(seq.media_id));
        self.hardware_failures.retain(|id, _| unchanged(*id));
        self.audio.retain(|id, _| unchanged(*id));
        self.preview_fingerprints.retain(|id, _| unchanged(*id));
        self.fingerprint_checked.retain(|id, _| unchanged(*id));
        self.proxy_sequences.retain(|seq| unchanged(seq.media_id));
        self.proxy_paths.retain(|id, _| unchanged(*id));
        self.prepared_pictures
            .retain(|key, _| unchanged(key.media_id));
        self.recount_prepared();
        if let Some(cache) = &self.optimized_cache {
            cache.register_media(media);
        }
        self.media = next;
    }


    /// Preview may display a missing frame, but export must surface the decoder
    /// failure instead of silently writing black picture or missing audio.
    pub fn take_decode_error(&mut self) -> Option<String> {
        self.decode_error.take()
    }
}

impl FrameSource for CachingSource {
    fn video_float(&mut self,id:u64,time:f64,max_w:u32,max_h:u32)->Option<Arc<super::precision::FloatImage>>{
        if self.cancelled(){return None;}
        let media=self.media.get(&id)?.clone();if self.preview_mode{self.check_preview_fingerprint(id,&media.path);}let(w,h)=fitted_size(media.width,media.height,max_w,max_h);let fps=media.source_fps.max(1.);let mut index=if media.kind==MediaKind::Still{0}else{nearest_source_frame(time,fps)};
        if media.kind!=MediaKind::Still{if let Ok(clock)=video_origin(&media.path,self.cancellation.as_ref().map(|g|g.flag.as_ref()),self.preview_request.as_deref()){index=clock.bounded_frame(index,fps);}}
        let key=(id,index,w,h);if let Some((_,image))=self.precise.iter().find(|(k,_)|*k==key){return Some(image.clone());}
        let bytes=match decode_frame_format(&media.path,index,fps,w,h,self.cancellation.as_ref().map(|g|g.flag.as_ref()),self.preview_request.as_deref(),true){Ok(v)=>v,Err(e)=>{self.decode_error.get_or_insert(e);return None;}};
        let image=Arc::new(super::precision::FloatImage{width:w,height:h,rgb:bytes.chunks_exact(2).map(|v|u16::from_le_bytes([v[0],v[1]])as f32/65535.).collect()});
        while self.precise.iter().map(|(_,v)|v.rgb.len()*4).sum::<usize>()+image.rgb.len()*4>96*1024*1024{if self.precise.pop_back().is_none(){break;}}
        self.precise.push_front((key,image.clone()));Some(image)
    }

    fn is_superseded(&self) -> bool {
        self.request_superseded()
    }
    fn video(&mut self, media_id: u64, time_secs: f64, max_w: u32, max_h: u32) -> Option<ImageBuf> {
        if self.cancelled() {
            return None;
        }
        let Some(media) = self.media.get(&media_id).cloned() else {
            self.decode_error
                .get_or_insert_with(|| format!("Media {media_id} is missing from the project"));
            return None;
        };
        if !media.has_video && media.kind != MediaKind::Still {
            return None;
        }
        if self.preview_mode {
            self.check_preview_fingerprint(media_id, &media.path);
        }
        let (w, h) = fitted_size(media.width, media.height, max_w, max_h);
        let fps = if media.source_fps > 1.0 {
            media.source_fps
        } else {
            24.0
        };
        let mut index = if media.kind == MediaKind::Still {
            0
        } else {
            nearest_source_frame(time_secs, fps)
        };
        if media.kind != MediaKind::Still && (index > 2 || time_secs >= media.source_duration_seconds()-0.5/fps) {
            if let Ok(clock) = video_origin(&media.path,
                self.cancellation.as_ref().map(|guard| guard.flag.as_ref()),self.preview_request.as_deref()) {
                index = clock.bounded_frame(index,fps);
            }
        }
        let key = (media_id, index, w, h);
        if let Some(hit) = self.cache.get(&key).cloned() {
            self.touch(key);
            return Some(hit);
        }
        if media.kind != MediaKind::Still {
            if let Some(prepared) = self.prepared_picture(&media, index, w, h, fps) {
                return Some(prepared);
            }
            if self.cancelled() {
                return None;
            }
        }
        let original_started = std::time::Instant::now();
        let reusable_cli = self.sequences.iter().any(|seq| {
            seq.media_id == media_id
                && seq.w == w
                && seq.h == h
                && can_drain_forward(seq.next, index, fps)
        });
        let native = if media.kind != MediaKind::Still {
            if let Some(client) = self.native_preview.as_mut() {
                if client
                    .retain_warm_cli(self.preview_intent == PreviewIntent::Playback, reusable_cli)
                {
                    None
                } else {
                    match client.decode(
                        &media.path,
                        index,
                        fps,
                        w,
                        h,
                        self.cancellation.as_ref().map(|guard| guard.flag.as_ref()),
                        self.preview_request.as_deref(),
                    ) {
                        Ok(rgb) => rgb,
                        Err(NativeDecodeError::Superseded) => return None,
                        Err(NativeDecodeError::Failed(error)) => {
                            self.decode_error = Some(error);
                            return None;
                        }
                    }
                }
            } else {
                None
            }
        } else {
            None
        };
        let decoded = if let Some(rgb) = native {
            Ok(rgb)
        } else if media.kind == MediaKind::Still {
            decode_frame_with_request(
                &media.path,
                0,
                24.0,
                w,
                h,
                self.cancellation.as_ref().map(|guard| guard.flag.as_ref()),
                self.preview_request.as_deref(),
            )
        } else if let Some(frame) = self.read_seq(media_id, index, w, h, &media, fps) {
            Ok(frame)
        } else if self.cancelled() {
            return None;
        } else {
            decode_frame_with_request(
                &media.path,
                index,
                fps,
                w,
                h,
                self.cancellation.as_ref().map(|guard| guard.flag.as_ref()),
                self.preview_request.as_deref(),
            )
        };
        if self.cancelled() {
            return None;
        }
        let rgb = match decoded {
            Ok(rgb) => rgb,
            Err(error) => {
                self.decode_error.get_or_insert_with(|| {
                    format!(
                        "Could not decode {} at {time_secs:.3}s: {error}",
                        media.name
                    )
                });
                return None;
            }
        };
        if rgb.len() != (w * h * 3) as usize {
            self.decode_error.get_or_insert_with(|| {
                format!("Incomplete picture in {} at {time_secs:.3}s", media.name)
            });
            return None;
        }
        if self.preview_mode && media.kind != MediaKind::Still {
            if let Some(cache) = &self.optimized_cache {
                cache.observe(&media, index, original_started.elapsed());
            }
        }
        let image = ImageBuf {
            width: w,
            height: h,
            rgb };
        if image.rgb.len() <= self.cache_budget {
            self.store(key, image.clone());
        }
        Some(image)
    }
}

impl CachingSource {
    fn recount_prepared(&mut self) {
        self.prepared_order
            .retain(|key| self.prepared_pictures.contains_key(key));
        self.prepared_bytes = self
            .prepared_pictures
            .values()
            .map(|image| image.rgb.len())
            .sum();
    }

    fn evict_prepared(&mut self) -> bool {
        while let Some(key) = self.prepared_order.pop_front() {
            if let Some(image) = self.prepared_pictures.remove(&key) {
                self.prepared_bytes -= image.rgb.len();
                return true;
            }
        }
        false
    }

    fn prepared_hit(&mut self, key: &PreparedPictureKey) -> Option<ImageBuf> {
        if !self.may_use_prepared() {
            return None;
        }
        let image = self.prepared_pictures.get(key)?.clone();
        self.prepared_order.retain(|old| old != key);
        self.prepared_order.push_back(key.clone());
        Some(image)
    }

    fn store_prepared(&mut self, key: PreparedPictureKey, image: ImageBuf) {
        if !self.may_use_prepared() {
            return;
        }
        let bytes = image.rgb.len();
        // Prepared frames never evict originals; originals may reclaim their
        // memory immediately. The two maps share the original total budget.
        let budget = (32 * 1024 * 1024).min(self.cache_budget.saturating_sub(self.cache_bytes));
        if bytes > budget {
            return;
        }
        if let Some(old) = self.prepared_pictures.remove(&key) {
            self.prepared_bytes -= old.rgb.len();
            self.prepared_order.retain(|old| old != &key);
        }
        while self.prepared_bytes + bytes > budget
            || self.cache.len() + self.prepared_pictures.len() >= 128
        {
            if !self.evict_prepared() {
                return;
            }
        }
        self.prepared_bytes += bytes;
        self.prepared_order.push_back(key.clone());
        self.prepared_pictures.insert(key, image);
    }

    fn touch(&mut self, key: (u64, i64, u32, u32)) {
        self.order.retain(|old| *old != key);
        self.order.push_back(key);
    }

    fn store(&mut self, key: (u64, i64, u32, u32), image: ImageBuf) {
        let bytes = image.rgb.len();
        if let Some(previous) = self.cache.remove(&key) {
            self.cache_bytes -= previous.rgb.len();
            self.order.retain(|old| *old != key);
        }
        // A single oversized frame can be displayed without consuming the whole cache.
        if bytes > self.cache_budget {
            return;
        }
        while self.cache_bytes + self.prepared_bytes + bytes > self.cache_budget
            || self.cache.len() + self.prepared_pictures.len() >= 128
        {
            if !self.evict_prepared() {
                break;
            }
        }
        while self.cache_bytes + bytes > self.cache_budget || self.cache.len() >= 128 {
            if let Some(old) = self.order.pop_front() {
                if let Some(evicted) = self.cache.remove(&old) {
                    self.cache_bytes -= evicted.rgb.len();
                }
            } else {
                break;
            }
        }
        self.cache_bytes += bytes;
        self.cache.insert(key, image);
        self.touch(key);
    }

    fn read_seq(
        &mut self,
        media_id: u64,
        index: i64,
        w: u32,
        h: u32,
        media: &Media,
        fps: f64,
    ) -> Option<Vec<u8>> {
        self.read_seq_mode(media_id, index, w, h, media, fps, true)
    }

    fn read_seq_mode(
        &mut self,
        media_id: u64,
        index: i64,
        w: u32,
        h: u32,
        media: &Media,
        fps: f64,
        retain_intermediate: bool,
    ) -> Option<Vec<u8>> {
        let hardware = self.preview_mode && PREVIEW_HARDWARE.load(Ordering::Acquire)
            && self.hardware_failures.get(&media_id).is_none_or(|at| at.elapsed() >= Duration::from_secs(60));
        let reuse = self.sequences.iter().position(|seq| {
            seq.media_id == media_id
                && seq.hardware == hardware
                && seq.w == w
                && seq.h == h
                && can_drain_forward(seq.next, index, fps)
        });
        let mut seq = if let Some(position) = reuse {
            self.sequences.remove(position)?
        } else {
            self.sequences
                .retain(|seq| seq.media_id != media_id || seq.w != w || seq.h != h);
            if self.sequences.len() + self.proxy_sequences.len() >= 4 {
                if self.proxy_sequences.is_empty() {
                    self.sequences.pop_front();
                } else {
                    self.proxy_sequences.pop_front();
                }
            }
            SeqDec::start(
                &media.path,
                media_id,
                index,
                fps,
                w,
                h,
                self.cancellation.as_ref(),
                hardware,
            )?
        };
        let active_request = self.cancellation.as_ref().map(|guard| {
            let mut request = guard.request.lock().unwrap_or_else(|e| e.into_inner());
            request.active = Some(Arc::downgrade(&seq.child));
            ActivePreviewDecode(guard.request.clone())
        });
        let mut rgb = vec![0u8; w as usize * h as usize * 3];
        while seq.next <= index {
            if self.cancelled() {
                return None;
            }
            let ordinal = seq.next;
            if seq.read_frame_into(&mut rgb).is_none() {
                if hardware && !self.cancelled() { self.hardware_failures.insert(media_id, std::time::Instant::now()); }
                return None; // Existing single-frame software path handles this request.
            }
            if self.cancelled() {
                return None;
            }
            if self.preview_mode
                && retain_intermediate
                && ordinal < index
                && rgb.len() <= self.cache_budget
            {
                self.store(
                    (media_id, ordinal, w, h),
                    ImageBuf {
                        width: w,
                        height: h,
                        rgb: rgb.clone() },
                );
            }
        }
        drop(active_request);
        if self.cancelled() {
            return None;
        }
        // Keep a few streams warm so layered clips and dissolves do not respawn FFmpeg
        // for each source on every frame. Recently used decoders stay at the back.
        self.sequences.push_back(seq);
        Some(rgb)
    }
}

/// Permit roughly one second of forward catch-up after decoder startup. A fixed
/// eight-frame limit caused paced playback to restart repeatedly while the clock
/// advanced during each process startup. Keep both CPU work and cancellation
/// latency bounded: at most 120 skipped pictures, with a check before each read.
fn can_drain_forward(next: i64, requested: i64, fps: f64) -> bool {
    let budget = if fps.is_finite() && fps > 0.0 {
        fps.ceil().clamp(8.0, 120.0) as i64
    } else {
        8
    };
    requested >= next && requested <= next.saturating_add(budget)
}

/// Preserve half-up frame selection when a stored source offset and timeline
/// time reconstruct a mathematical half-frame a few floating-point ULPs low.
pub(super) fn nearest_source_frame(time_secs: f64, fps: f64) -> i64 {
    let position = time_secs * fps;
    let half = position.floor() + 0.5;
    let tolerance = (4.0 * f64::EPSILON * position.abs().max(1.0)).min(1e-7);
    let position = if (position - half).abs() <= tolerance {
        half
    } else {
        position
    };
    position.round().max(0.0) as i64
}

/// Bound input seeking to two pictures of preroll. Demuxers may round this
/// earlier; preserved timestamps, not a guessed number of decoded pictures,
/// select the requested CFR frame afterward.
fn seek_window(index: i64, fps: f64) -> (i64, f64) {
    let first = index.saturating_sub(2).max(0);
    let time = if first == 0 {
        0.0
    } else {
        (first as f64 - 0.5) / fps
    };
    (first, time)
}

impl SeqDec {
    fn start(
        path: &Path,
        media_id: u64,
        index: i64,
        fps: f64,
        w: u32,
        h: u32,
        cancellation: Option<&DecodeCancellation>,
        hardware: bool,
    ) -> Option<Self> {
        if cancellation.is_some_and(|guard| guard.flag.load(Ordering::Acquire)) {
            return None;
        }
        let request = cancellation.and_then(|guard| guard.request.lock().ok()?.flag.clone());
        let start = cli_frame_start(
            path,
            index,
            fps,
            cancellation.map(|guard| guard.flag.as_ref()),
            request.as_deref(),
        )
        .ok()?;
        let ffmpeg = find_ffmpeg().ok()?;
        let mut command = media_command(&ffmpeg);
        // No hardware output format: FFmpeg returns software frames for the
        // existing CPU compositor. Auto supports Radeon, Intel and NVIDIA;
        // a failed stream retries through the existing software frame path.
        if hardware { command.args(["-hwaccel", "auto"]); }
        command
            .arg("-nostdin")
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-xerror")
            .arg("-filter_threads")
            .arg("1")
            .arg("-copyts");
        if let Some(time) = start.seek {
            command
                .args(["-seek_timestamp", "1", "-ss"])
                .arg(format!("{time:.12}"));
        }
        let mut child = command
            .arg("-threads")
            .arg("2")
            .arg("-i")
            .arg(path)
            .arg("-map")
            .arg(&start.stream)
            .arg("-an")
            .arg("-sn")
            .arg("-dn")
            .arg("-vf")
            .arg(format!("{},scale={w}:{h}:flags=bilinear", start.select))
            .args(crate::media_engine::passthrough_args(&ffmpeg).ok()?)
            .arg("-f")
            .arg("rawvideo")
            .arg("-pix_fmt")
            .arg("rgb24")
            .arg("-threads")
            .arg("1")
            .arg("pipe:1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdout = std::io::BufReader::new(child.stdout.take()?);
        let child = Arc::new(Mutex::new(child));
        if let Some(guard) = cancellation {
            let mut children = guard
                .children
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            children.retain(|child| child.strong_count() > 0);
            children.push(Arc::downgrade(&child));
        }
        Some(Self {
            hardware,
            media_id,
            w,
            h,
            next: index.max(0),
            child,
            stdout })
    }

    fn read_frame_into(&mut self, buf: &mut [u8]) -> Option<()> {
        self.stdout.read_exact(buf).ok()?;
        if self
            .child
            .lock()
            .ok()?
            .try_wait()
            .ok()?
            .is_some_and(|status| !status.success())
        {
            return None;
        }
        self.next += 1;
        Some(())
    }
}


fn decode_frame_with_request(
    path: &Path,
    index: i64,
    fps: f64,
    w: u32,
    h: u32,
    cancel: Option<&AtomicBool>,
    request: Option<&AtomicBool>,
) -> Result<Vec<u8>, String> {
    decode_frame_format(path,index,fps,w,h,cancel,request,false)
}
fn decode_frame_format(path:&Path,index:i64,fps:f64,w:u32,h:u32,cancel:Option<&AtomicBool>,request:Option<&AtomicBool>,precise:bool)->Result<Vec<u8>,String>{
    let ffmpeg = find_ffmpeg()?;
    let timing = crate::media_engine::passthrough_args(&ffmpeg)?;
    let start = cli_frame_start(path, index, fps, cancel, request)?;
    let mut args = vec![
        "-nostdin".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-xerror".into(),
        "-filter_threads".into(),
        "1".into(),
        "-copyts".into(),
    ];
    if let Some(time) = start.seek {
        args.extend([
            "-seek_timestamp".into(),
            "1".into(),
            "-ss".into(),
            format!("{time:.12}"),
        ]);
    }
    args.extend([
        "-threads".into(),
        "2".into(),
        "-i".into(),
        path.display().to_string(),
        "-map".into(),
        start.stream,
        "-an".into(),
        "-sn".into(),
        "-dn".into(),
        "-frames:v".into(),
        "1".into(),
        "-vf".into(),
        format!("{},scale={w}:{h}:flags=bilinear", start.select),
        timing[0].into(),
        timing[1].into(),
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        if precise{"rgb48le".into()}else{"rgb24".into()},
        "-threads".into(),
        "1".into(),
        "pipe:1".into(),
    ]);
    let output = match cancel {
        Some(cancel) => command_output_with_cancels(&ffmpeg, &args, cancel, request),
        None => match request {
            Some(flag) => command_output_cancellable(&ffmpeg, &args, flag),
            None => command_output(&ffmpeg, &args) } }?;
    require_decode_success(&output, "video", path)?;
    let expected = (w as usize)
        .checked_mul(h as usize)
        .and_then(|pixels| pixels.checked_mul(if precise{6}else{3}))
        .ok_or("Requested frame dimensions exceed the supported buffer size")?;
    if output.stdout.len() != expected {
        return Err(format!(
            "FFmpeg returned {} bytes for a {expected}-byte frame from {}",
            output.stdout.len(),
            path.display()
        ));
    }
    Ok(output.stdout)
}


/// Preview-only bounded PCM. AAC input seeks can begin at the next packet,
/// so align the decoder's timestamp origin before resampling/trimming. A full
/// second of preroll keeps codec/resampler startup outside the audible window.
/// AAC perceptual-noise synthesis may differ from an origin decode after a
/// seek; sample phase/count remain aligned. Export still uses origin decoding.
pub(crate) fn decode_audio_window(
    path: &Path,
    block: i64,
    cancel: &AtomicBool,
) -> Result<Pcm, String> {
    let start = block.max(0).saturating_mul(2);
    let seek = start.saturating_sub(1).max(0);
    let discard = (start - seek) * 48_000;
    let mut args = vec![
        "-nostdin".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-xerror".into(),
    ];
    // Even -ss0 changes AAC priming behavior. Preserve normal origin decoding.
    if seek > 0 {
        args.extend(["-ss".into(), seek.to_string()]);
    }
    args.extend([
        "-i".into(), path.display().to_string(), "-vn".into(), "-af".into(),
        format!("aresample=48000:first_pts=0,aformat=sample_fmts=flt:channel_layouts=stereo,atrim=start_sample={discard}:end_sample={},asetpts=PTS-STARTPTS", discard + super::audio::PREVIEW_WINDOW_SAMPLES),
        "-t".into(), "2".into(), "-ac".into(), "2".into(), "-ar".into(), "48000".into(),
        "-f".into(), "f32le".into(), "pipe:1".into(),
    ]);
    let output = command_output_cancellable(&find_ffmpeg()?, &args, cancel)?;
    require_decode_success(&output, "preview audio", path)?;
    if (output.stdout.is_empty() && block <= 0)
        || output.stdout.len() % 8 != 0
        || output.stdout.len() > super::audio::PREVIEW_WINDOW_SAMPLES as usize * 8
    {
        return Err(format!(
            "Invalid bounded preview PCM from {}",
            path.display()
        ));
    }
    if cancel.load(Ordering::Acquire) {
        return Err("Media decode cancelled".into());
    }
    let samples = output
        .stdout
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()))
        .collect();
    Ok(Pcm {
        sample_rate: 48_000,
        samples })
}
