use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use crate::playback::{composite, find_ffmpeg, CachingSource, ImageBuf};
use crate::project::{Anchor, FrameRate, MediaKind, Project, TrackKind};
use crate::transitions::transition_window;

mod gpu;
mod audio;
mod progress;
pub mod diagnostics;
pub mod worker;
mod timeline;
use diagnostics::{ExportDiagnostics, ExportRoute, ExportStage};
pub use progress::format_remaining_time;

struct TemporaryAudioFile {
    path: PathBuf }

impl TemporaryAudioFile {
    fn create() -> Result<(Self, File), String> {
        static NEXT_FILE: AtomicU64 = AtomicU64::new(1);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|err| err.to_string())?
            .as_nanos();
        let serial = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "quaternio-audio-{}-{stamp}-{serial}.pcm",
            std::process::id()
        ));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|err| err.to_string())?;
        Ok((Self { path }, file))
    }
}

impl Drop for TemporaryAudioFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

struct EncoderProcess {
    child: Child,
    finished: bool }

impl EncoderProcess {
    fn stop(&mut self) {
        if !self.finished {
            let _ = self.child.kill();
            self.finished = self.child.wait().is_ok();
        }
    }
}

impl Drop for EncoderProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

fn take_encoder_stdin(
    child: &mut EncoderProcess,
    cancel: &AtomicBool,
) -> Result<std::process::ChildStdin, String> {
    if cancel.load(Ordering::Acquire) {
        child.stop();
        return Err("Export cancelled".into());
    }
    child.child.stdin.take().ok_or_else(|| {
        if cancel.load(Ordering::Acquire) {
            "Export cancelled".into()
        } else {
            "Could not write frames to FFmpeg".into()
        }
    })
}

fn final_export_result(result: Result<(), String>, cancel: &AtomicBool) -> Result<(), String> {
    match result {
        Err(_) if cancel.load(Ordering::Acquire) => Err("Export cancelled".into()),
        // A successful result has already published the staged file. A late
        // cancellation request must not misreport that completed publication.
        result => result }
}

fn export_audio_media(project: &Project, start: i64, end: i64) -> HashSet<u64> {
    let mut used = HashSet::new();
    for (track_index, track) in project.sequence.tracks.iter().enumerate() {
        if !project.sequence.audio_track_enabled(track_index) || track.gain <= 0.0 {
            continue;
        }
        for clip in &track.clips {
            if clip.end() > start && clip.timeline_start < end && clip.volume > 0.0 {
                used.insert(clip.media_id);
            }
        }
        for transition in &project.sequence.transitions {
            if !transition.kind.is_audio()
                || !transition_window(&project.sequence, transition).is_some_and(
                    |(window_start, window_end)| window_start < end && window_end > start,
                )
            {
                continue;
            }
            let Some(outgoing) = track
                .clips
                .iter()
                .find(|clip| clip.id == transition.anchor.clip_id())
            else {
                continue;
            };
            if outgoing.volume > 0.0 {
                used.insert(outgoing.media_id);
            }
            if matches!(transition.anchor, Anchor::Cut(_)) {
                if let Some(incoming) = track
                    .clips
                    .iter()
                    .filter(|clip| {
                        clip.id != outgoing.id && clip.timeline_start >= outgoing.end() - 1
                    })
                    .min_by_key(|clip| (clip.timeline_start, clip.id))
                {
                    if incoming.volume > 0.0 {
                        used.insert(incoming.media_id);
                    }
                }
            }
        }
    }
    used
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum EncoderPreference {
    Auto,
    Cpu,
    /// Require hardware encoding; picture composition may still use the CPU.
    Hardware }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum RenderSpeed {
    #[default]
    Balanced,
    Fast,
    Compact }

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ExportSettings {
    pub width: u32,
    pub height: u32,
    pub frame_rate: FrameRate,
    pub crf: u8,
    pub encoder: EncoderPreference,
    pub speed: RenderSpeed,
    pub force_composite: bool,
    pub use_in_out: bool,
    pub output: PathBuf }

#[derive(Clone, Debug)]
pub struct ExportPreparation {
    pub phase: &'static str,
    pub fraction: f32 }

#[derive(Clone, Debug)]
pub struct ExportProgress {
    pub fraction: f32,
    /// Preparation has its own phase fraction; encoding ETA starts afterwards.
    pub preparation: Option<ExportPreparation>,
    pub message: String,
    pub remaining_seconds: Option<u64>,
    pub done: bool,
    pub error: Option<String> }

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum VideoEncoder {
    Cpu,
    Nvenc,
    Qsv,
    Amf,
    VideoToolbox,
    Vaapi(PathBuf) }

impl VideoEncoder {
    fn label(&self) -> &'static str {
        match self {
            Self::Cpu => "CPU H.264",
            Self::Nvenc => "NVIDIA H.264",
            Self::Qsv => "Intel H.264",
            Self::Amf => "AMD H.264",
            Self::VideoToolbox => "Apple VideoToolbox H.264",
            Self::Vaapi(_) => "VAAPI H.264" }
    }

    fn description(&self) -> String {
        match self {
            Self::Vaapi(device) => format!("{} ({})", self.label(), device.display()),
            _ => self.label().into(),
        }
    }

    // Device initialization is global and must precede inputs and filter graphs.
    fn configure_device(&self, command: &mut Command) {
        if let Self::Vaapi(device) = self {
            command.arg("-vaapi_device").arg(device);
        }
    }

    fn software_frame_filter(&self) -> &'static str {
        match self {
            Self::Cpu => "format=yuv420p",
            Self::Vaapi(_) => "format=nv12,hwupload",
            _ => "format=nv12",
        }
    }

    fn configure_pixel_format(&self, command: &mut Command) {
        command.args(["-pix_fmt", match self {
            Self::Cpu => "yuv420p",
            Self::Vaapi(_) => "vaapi",
            _ => "nv12",
        }]);
    }

    fn configure(&self, command: &mut Command, settings: &ExportSettings) {
        let quality = settings.crf.clamp(1, 40).to_string();
        let speed = settings.speed;
        let preset = match (self, speed) {
            (Self::Cpu, RenderSpeed::Balanced) => "veryfast",
            (Self::Cpu, RenderSpeed::Fast) => "ultrafast",
            (Self::Cpu, RenderSpeed::Compact) => "medium",
            (Self::Nvenc, RenderSpeed::Balanced) => "p4",
            (Self::Nvenc, RenderSpeed::Fast) => "p2",
            (Self::Nvenc, RenderSpeed::Compact) => "p6",
            (Self::Qsv, RenderSpeed::Balanced) => "medium",
            (Self::Qsv, RenderSpeed::Fast) => "veryfast",
            (Self::Qsv, RenderSpeed::Compact) => "slow",
            (Self::Amf, RenderSpeed::Balanced) => "balanced",
            (Self::Amf, RenderSpeed::Fast) => "speed",
            (Self::Amf, RenderSpeed::Compact) => "quality",
            // These APIs have no portable equivalent of the above presets.
            (Self::VideoToolbox | Self::Vaapi(_), _) => "" };
        match self {
            Self::Cpu => {
                command.args(["-c:v", "libx264", "-preset", preset, "-crf", &quality]);
                let threads = std::thread::available_parallelism()
                    .map_or(2, |count| count.get().saturating_sub(2).clamp(1, 8));
                command.args(["-threads:v", &threads.to_string()]);
            }
            Self::Nvenc => {
                command.args([
                    "-c:v",
                    "h264_nvenc",
                    "-preset",
                    preset,
                    "-rc",
                    "vbr",
                    "-cq",
                    &quality,
                    "-b:v",
                    "0",
                ]);
            }
            Self::Qsv => {
                command.args([
                    "-c:v",
                    "h264_qsv",
                    "-preset",
                    preset,
                    "-global_quality",
                    &quality,
                ]);
            }
            Self::Amf => {
                command.args([
                    "-c:v", "h264_amf", "-quality", preset, "-rc", "cqp", "-qp_i", &quality,
                    "-qp_p", &quality, "-qp_b", &quality,
                ]);
            }
            Self::VideoToolbox => {
                // VT constant quality is not supported by Intel Macs. Average
                // bitrate works on both Intel and Apple Silicon; map the quality
                // control monotonically, without pretending this is x264 CRF.
                let pixels = (settings.width.max(16) & !1) as f64
                    * (settings.height.max(16) & !1) as f64;
                let bitrate = (pixels * settings.frame_rate.fps() * 0.12
                    * 2.0_f64.powf((21.0 - settings.crf.clamp(1, 40) as f64) / 6.0))
                    .clamp(100_000.0, 240_000_000.0).round() as u64;
                command.args([
                    "-c:v", "h264_videotoolbox", "-allow_sw", "0", "-b:v",
                    &bitrate.to_string(), "-realtime",
                    if speed == RenderSpeed::Fast { "1" } else { "0" },
                ]);
            }
            Self::Vaapi(_) => {
                // CQP avoids driver-dependent ICQ/QVBR support. No B frames
                // also accommodates older and low-end VAAPI H.264 devices.
                command.args([
                    "-c:v", "h264_vaapi", "-rc_mode", "CQP", "-qp", &quality,
                    "-bf", "0",
                ]);
            }
        }
    }
}

fn ffmpeg_command(path: &Path) -> Command {
    let mut command = Command::new(path);
    #[cfg(windows)]
    command.creation_flags(0x08004000); // CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS
    command.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-xerror"]);
    command
}

/// Export can ship a driver-compatible FFmpeg without changing preview decoding.
fn export_ffmpeg() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("QUATERNIO_EXPORT_FFMPEG") {
        let path = PathBuf::from(path);
        if !path.is_absolute() || !path.is_file() {
            return Err("QUATERNIO_EXPORT_FFMPEG must name an existing absolute executable".into());
        }
        return Ok(path);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let bundled = parent.join("export-runtime").join(if cfg!(windows) {
                "ffmpeg.exe"
            } else {
                "ffmpeg"
            });
            if bundled.is_file() {
                return Ok(bundled);
            }
        }
    }
    find_ffmpeg()
}

fn send_progress(progress: &Sender<ExportProgress>, fraction: f32, message: String) {
    let _ = progress.send(ExportProgress {
        fraction,
        preparation: None,
        message,
        remaining_seconds: None,
        done: false,
        error: None });
}

struct StagedOutput {
    path: PathBuf }

impl StagedOutput {
    fn create(output: &Path) -> Result<Self, String> {
        static NEXT_FILE: AtomicU64 = AtomicU64::new(1);
        let parent = output
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let serial = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".quaternio-export-{}-{stamp}-{serial}.mp4",
            std::process::id()
        ));
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| error.to_string())?;
        Ok(Self { path })
    }

    fn commit(&self, output: &Path, cancel: &AtomicBool) -> Result<(), String> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .and_then(|file| file.sync_all())
            .map_err(|error| error.to_string())?;
        // Recheck after flushing, immediately before the publication step.
        if cancel.load(Ordering::Acquire) {
            return Err("Export cancelled".into());
        }
        std::fs::rename(&self.path, output)
            .map_err(|error| format!("Could not publish completed export: {error}"))
    }
}

impl Drop for StagedOutput {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

struct DirectAudio {
    first_sample: i64,
    sample_count: i64,
    left_gain: f32,
    right_gain: f32 }

enum DirectAudioRoute {
    Silence,
    Source(DirectAudio),
    Mixed }

struct DirectExport {
    path: PathBuf,
    source_width: u32,
    source_height: u32,
    timeline_start: i64,
    first_frame: i64,
    frame_count: i64,
    fitted_width: u32,
    fitted_height: u32,
    audio: DirectAudioRoute,
    gpu_safe: bool }

fn rounded_sample(frame: i64, rate: FrameRate) -> Option<i64> {
    if rate.num == 0 || rate.den == 0 {
        return None;
    }
    let value = frame as i128 * 48_000 * rate.den as i128;
    let divisor = rate.num as i128;
    let rounded = if value >= 0 {
        (value + divisor / 2) / divisor
    } else {
        -((-value + divisor / 2) / divisor)
    };
    i64::try_from(rounded).ok()
}

fn prepare_mixed_audio(
    project: &Project,
    start: i64,
    end: i64,
    cancel: &AtomicBool,
    progress: &Sender<ExportProgress>,
) -> Result<TemporaryAudioFile, String> {
    audio::prepare(project, start, end, cancel, progress)
}

fn preflight_media(project: &Project, settings: &ExportSettings) -> Result<(), String> {
    let (start, end) = export_range(project, settings)?;
    let rate = project.sequence.frame_rate;
    let start = rate.seconds_to_frame(start);
    let end = rate.seconds_to_frame(end);
    let mut used = export_audio_media(project, start, end);
    for track in &project.sequence.tracks {
        if track.kind != TrackKind::Video || track.muted {
            continue;
        }
        for clip in &track.clips {
            if clip.timeline_start < end && clip.end() > start {
                used.insert(clip.media_id);
            }
        }
        for transition in &project.sequence.transitions {
            if transition.kind.is_audio()
                || !transition_window(&project.sequence, transition)
                    .is_some_and(|(a, b)| a < end && b > start)
            {
                continue;
            }
            if let Some(outgoing) = track
                .clips
                .iter()
                .find(|clip| clip.id == transition.anchor.clip_id())
            {
                used.insert(outgoing.media_id);
                if matches!(transition.anchor, Anchor::Cut(_)) {
                    if let Some(incoming) = track
                        .clips
                        .iter()
                        .filter(|clip| {
                            clip.id != outgoing.id && clip.timeline_start >= outgoing.end() - 1
                        })
                        .min_by_key(|clip| (clip.timeline_start, clip.id))
                    {
                        used.insert(incoming.media_id);
                    }
                }
            }
        }
    }
    let output = settings.output.canonicalize().ok();
    for id in used {
        let media = project
            .media(id)
            .ok_or("A timeline clip references missing project media")?;
        if !media.path.is_file() {
            return Err(format!(
                "Media is offline: {}. Relink {} before exporting",
                media.path.display(),
                media.name
            ));
        }
    }
    for track in &project.sequence.tracks {
        if track.kind != TrackKind::Video || track.muted {
            continue;
        }
        if track.clips.iter().any(|clip| {
            clip.timeline_start < end
                && clip.end() > start
                && !project
                    .media(clip.media_id)
                    .is_some_and(|media| media.has_video)
        }) {
            return Err(format!(
                "{} contains a clip without a video stream. Relink or remove it before exporting",
                track.name
            ));
        }
    }
    for media in &project.media {
        if output
            .as_ref()
            .zip(media.path.canonicalize().ok().as_ref())
            .is_some_and(|(output, source)| output == source)
            || settings.output == media.path
        {
            return Err(format!(
                "Choose another output file. Export cannot replace source media {}",
                media.name
            ));
        }
    }
    Ok(())
}

fn direct_export_plan(project: &Project, settings: &ExportSettings) -> Option<DirectExport> {
    let rate = project.sequence.frame_rate;
    if settings.force_composite || settings.frame_rate != rate || rate.fps() < 1.0 {
        return None;
    }
    let (start_seconds, end_seconds) = export_range(project, settings).ok()?;
    let start = rate.seconds_to_frame(start_seconds);
    let end = rate.seconds_to_frame(end_seconds);
    if start < 0 || end <= start {
        return None;
    }
    if project.sequence.transitions.iter().any(|transition| {
        !transition.kind.is_audio()
            && transition_window(&project.sequence, transition)
                .map_or(true, |(a, b)| a < end && b > start)
    }) {
        return None;
    }
    let videos: Vec<_> = project
        .sequence
        .tracks
        .iter()
        .filter(|track| track.kind == TrackKind::Video && !track.muted)
        .flat_map(|track| track.clips.iter())
        .filter(|clip| clip.timeline_start < end && clip.end() > start)
        .collect();
    if videos.len() != 1 {
        return None;
    }
    let video = videos[0];
    if video.timeline_start > start
        || video.end() < end
        || video.source_in < 0
        // A native source mark between sequence frames must keep its exact
        // picture/audio origin in the compositor and mixer. Frame trim cannot
        // represent that remainder without silently changing the source mark.
        || video.source_time_offset != 0.0
        || video.scale != 1.0
        || video.pos_x != 0.0
        || video.pos_y != 0.0
        || video.opacity != 1.0
        || !video.grade.is_identity()
    {
        return None;
    }
    let media = project.media(video.media_id)?;
    if media.kind != MediaKind::Video
        || !media.has_video
        || media.width < 2
        || media.height < 2
        || !media.source_fps.is_finite()
        || (media.source_fps - rate.fps()).abs() > 0.000001
    {
        return None;
    }
    let first_frame = video
        .source_in
        .checked_add(start.checked_sub(video.timeline_start)?)?;
    let frame_count = end.checked_sub(start)?;
    if first_frame.checked_add(frame_count)? > media.source_limit(rate)? {
        return None;
    }
    let width = settings.width.max(16) & !1;
    let height = settings.height.max(16) & !1;
    // Match CachingSource's fitted_size rounding and the compositor's centered bars.
    let scale = (width as f32 / media.width as f32).min(height as f32 / media.height as f32);
    let fitted_width = (((media.width as f32 * scale).round() as u32).max(2) & !1).max(2);
    let fitted_height = (((media.height as f32 * scale).round() as u32).max(2) & !1).max(2);
    let audio = direct_audio_route(project, video, start, end);
    Some(DirectExport {
        path: media.path.clone(),
        source_width: media.width,
        source_height: media.height,
        timeline_start: start,
        first_frame,
        frame_count,
        fitted_width,
        fitted_height,
        audio,
        gpu_safe: false })
}

fn direct_audio_route(
    project: &Project,
    video: &crate::project::Clip,
    start: i64,
    end: i64,
) -> DirectAudioRoute {
    // Audio edits do not require sending unchanged video through the Rust
    // compositor. Keep all envelope and overlap semantics in the exact mixer.
    if project.sequence.transitions.iter().any(|transition| {
        transition.kind.is_audio()
            && transition_window(&project.sequence, transition)
                .map_or(true, |(a, b)| a < end && b > start)
    }) {
        return DirectAudioRoute::Mixed;
    }
    let rate = project.sequence.frame_rate;
    let audios: Vec<_> = project
        .sequence
        .tracks
        .iter()
        .enumerate()
        .filter(|(index, track)| {
            project.sequence.audio_track_enabled(*index)
                && (track.gain > 0.0 || !track.gain.is_finite())
        })
        .flat_map(|(_, track)| track.clips.iter().map(move |clip| (track, clip)))
        .filter(|(_, clip)| clip.timeline_start < end && clip.end() > start && clip.volume > 0.0001)
        .filter(|(_, clip)| {
            project
                .media(clip.media_id)
                .is_some_and(|media| media.has_audio)
        })
        .collect();
    if audios.is_empty() {
        return DirectAudioRoute::Silence;
    }
    let source_audio = (|| {
        if audios.len() != 1 {
            return None;
        }
        let (track, clip) = audios[0];
        if clip.media_id != video.media_id
            || clip.timeline_start > start
            || clip.end() < end
            || !clip.volume.is_finite()
            || !track.gain.is_finite()
            || !track.pan.is_finite()
            || clip.source_time_offset != 0.0
        {
            return None;
        }
        let offset = clip.source_in.checked_sub(clip.timeline_start)?;
        if offset != video.source_in.checked_sub(video.timeline_start)? {
            return None;
        }
        let source_offset = offset as i128 * 48_000 * rate.den as i128;
        // Fractional PCM offsets use the mixer's source-sample rounding; keep
        // those in mix_range rather than approximate them with an FFmpeg trim.
        if source_offset % rate.num as i128 != 0 {
            return None;
        }
        let first_sample = rounded_sample(start, rate)?
            .checked_add(i64::try_from(source_offset / rate.num as i128).ok()?)?;
        if first_sample < 0 {
            return None;
        }
        let pan = track.pan.clamp(-1.0, 1.0);
        let gain = track.gain.max(0.0) * clip.volume.max(0.0);
        if !gain.is_finite() {
            return None;
        }
        Some(DirectAudio {
            first_sample,
            sample_count: rounded_sample(end, rate)?.checked_sub(rounded_sample(start, rate)?)?,
            left_gain: gain * (1.0 - pan.max(0.0)),
            right_gain: gain * (1.0 + pan.min(0.0)) })
    })();
    source_audio.map_or(DirectAudioRoute::Mixed, DirectAudioRoute::Source)
}

fn wait_for_encoder(
    child: &mut EncoderProcess,
    cancel: &AtomicBool,
    timeout: Option<Duration>,
    advancing_frames: Option<&AtomicU64>,
) -> Result<ExitStatus, String> {
    let started = Instant::now();
    let mut last_frame = 0;
    let mut last_advance = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            child.stop();
            return Err("Export cancelled".into());
        }
        if timeout.is_some_and(|limit| started.elapsed() >= limit) {
            child.stop();
            return Err("FFmpeg preparation timed out".into());
        }
        if let Some(frames) = advancing_frames {
            let frame = frames.load(Ordering::Relaxed);
            if frame != last_frame {
                last_frame = frame;
                last_advance = Instant::now();
            }
            if last_advance.elapsed() >= Duration::from_secs(300) {
                child.stop();
                return Err("FFmpeg produced no new frames for five minutes. Export stopped; the existing output is preserved".into());
            }
        }
        match child.child.try_wait().map_err(|error| error.to_string())? {
            Some(status) => {
                child.finished = true;
                return Ok(status);
            }
            None => std::thread::sleep(Duration::from_millis(20)) }
    }
}

fn source_matches_rate(
    path: &Path,
    ffmpeg: &Path,
    rate: FrameRate,
    expected_size: (u32, u32),
    cancel: &AtomicBool,
) -> Result<(bool, bool), String> {
    type MetadataKey = (PathBuf, u64, SystemTime, PathBuf, u64, SystemTime, u32, u32, (u32, u32));
    type MetadataCache = std::collections::HashMap<MetadataKey, ((bool, bool), Instant)>;
    static CACHE: OnceLock<Mutex<MetadataCache>> = OnceLock::new();
    if cancel.load(Ordering::Acquire) { return Err("Export cancelled".into()); }
    let probe = ffmpeg.with_file_name(if cfg!(windows) { "ffprobe.exe" } else { "ffprobe" });
    let key = std::fs::metadata(path).ok().zip(std::fs::metadata(&probe).ok())
        .and_then(|(source, executable)| Some((
            std::fs::canonicalize(path).ok()?, source.len(), source.modified().ok()?,
            probe, executable.len(), executable.modified().ok()?, rate.num, rate.den, expected_size,
        )));
    if let Some(result) = key.as_ref().and_then(|key| CACHE.get_or_init(Default::default)
        .lock().unwrap_or_else(|e| e.into_inner()).get(key).copied())
        .filter(|(_, at)| at.elapsed() < Duration::from_secs(60)) {
        return Ok(result.0);
    }
    let result = source_matches_rate_uncached(path, ffmpeg, rate, expected_size, cancel)?;
    if let Some(key) = key {
        let mut cache = CACHE.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() >= 128 { cache.clear(); }
        cache.insert(key, (result, Instant::now()));
    }
    Ok(result)
}

fn source_matches_rate_uncached(
    path: &Path, ffmpeg: &Path, rate: FrameRate, expected_size: (u32, u32), cancel: &AtomicBool,
) -> Result<(bool, bool), String> {
    let ffprobe = ffmpeg.with_file_name(if cfg!(windows) {
        "ffprobe.exe"
    } else {
        "ffprobe"
    });
    let mut command = Command::new(ffprobe);
    #[cfg(windows)]
    command.creation_flags(0x08004000);
    command
        .args([
            "-v",
            "error",
            "-select_streams",
            "V:0",
            "-show_entries",
            "stream=width,height,avg_frame_rate,r_frame_rate,nb_frames,duration,codec_name,pix_fmt,color_range,color_space,color_transfer,color_primaries,sample_aspect_ratio:stream_side_data=rotation:stream_tags=rotate",
            "-of",
            "json",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let Ok(child) = command.spawn() else {
        return Ok((false, false));
    };
    let mut child = EncoderProcess {
        child,
        finished: false };
    let stdout = child.child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut stdout) = stdout {
            let _ = std::io::Read::read_to_string(&mut stdout, &mut text);
        }
        text
    });
    let status = wait_for_encoder(&mut child, cancel, Some(Duration::from_secs(8)), None);
    if status.is_err() {
        child.stop();
    }
    let text = reader.join().unwrap_or_default();
    if cancel.load(Ordering::Relaxed) {
        return Err("Export cancelled".into());
    }
    if !status.is_ok_and(|status| status.success()) {
        return Ok((false, false));
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Ok((false, false));
    };
    let Some(stream) = value["streams"]
        .as_array()
        .and_then(|streams| streams.first())
    else {
        return Ok((false, false));
    };
    let matches = |value: &serde_json::Value| {
        value
            .as_str()
            .and_then(|value| value.split_once('/'))
            .and_then(|(num, den)| Some((num.parse::<u64>().ok()?, den.parse::<u64>().ok()?)))
            .is_some_and(|(num, den)| {
                num > 0
                    && den > 0
                    && num as u128 * rate.den as u128 == den as u128 * rate.num as u128
            })
    };
    // Variable-rate or unspecified metadata stays on the compositor. The count
    // and duration check also rejects streams whose nominal rate is misleading.
    let count = stream["nb_frames"]
        .as_str()
        .and_then(|value| value.parse::<u64>().ok());
    let duration = stream["duration"]
        .as_str()
        .and_then(|value| value.parse::<f64>().ok());
    let gpu_safe = gpu::safe_source_metadata(stream)
        && stream["width"].as_u64() == Some(expected_size.0 as u64)
        && stream["height"].as_u64() == Some(expected_size.1 as u64);
    Ok((
        matches(&stream["r_frame_rate"])
            && matches(&stream["avg_frame_rate"])
            && count.zip(duration).is_some_and(|(count, duration)| {
                count > 0
                    && duration.is_finite()
                    && (count as f64 - duration * rate.fps()).abs() <= 0.5
            }),
        gpu_safe,
    ))
}

fn thread_progress(
    stdout: Option<std::process::ChildStdout>,
    progress: Sender<ExportProgress>,
    frame_count: i64,
    label: String,
    advancing_frames: Option<Arc<AtomicU64>>,
) -> std::thread::JoinHandle<()> {
    let collector = diagnostics::worker_collector();
    std::thread::spawn(move || {
        let Some(stdout) = stdout else { return };
        let started = Instant::now();
        let mut remaining = progress::RemainingEstimator::new(frame_count);
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(frame) = line
                .strip_prefix("frame=")
                .and_then(|frame| frame.trim().parse::<i64>().ok())
            {
                if let Some(frames) = &advancing_frames {
                    frames.store(frame.max(0) as u64, Ordering::Relaxed);
                }
                if let Some(collector) = &collector {
                    collector.observe_frame(frame.max(0) as u64);
                }
                let _ = progress.send(ExportProgress {
                    preparation: None,
                    fraction: (frame as f32 / frame_count.max(1) as f32).clamp(0.0, 0.995),
                    message: format!("{label} · {frame}/{frame_count}"),
                    remaining_seconds: remaining.observe(frame, started.elapsed()),
                    done: false,
                    error: None });
            }
        }
    })
}

fn export_direct(
    ffmpeg: &Path,
    plan: &DirectExport,
    settings: &ExportSettings,
    encoder: &VideoEncoder,
    mixed_audio: Option<&TemporaryAudioFile>,
    gpu_resident: bool,
    cancel: &AtomicBool,
    progress: Sender<ExportProgress>,
) -> Result<(), String> {
    diagnostics::record(|d| {
        d.route = Some(ExportRoute::Direct);
        d.picture_route = Some(if gpu_resident { "gpu_resident" } else { "software_frames" }.into());
        d.audio_route = Some(match plan.audio { DirectAudioRoute::Mixed => "mixed", DirectAudioRoute::Silence => "silence", DirectAudioRoute::Source(_) => "source" }.into());
    });
    let width = settings.width.max(16) & !1;
    let height = settings.height.max(16) & !1;
    let time_base = format!("{}/{}", settings.frame_rate.den, settings.frame_rate.num);
    let duration = settings.frame_rate.frame_to_seconds(plan.frame_count);
    let last_frame = plan
        .first_frame
        .checked_add(plan.frame_count)
        .ok_or("Export exceeds the source frame range")?;
    let mut command = ffmpeg_command(ffmpeg);
    command.args(["-y", "-threads", "2"]);
    if gpu_resident {
        gpu::configure_resident_input(&mut command, encoder, plan);
    } else {
        encoder.configure_device(&mut command);
        command.arg("-i").arg(&plan.path);
    }
    let video_filter = if gpu_resident {
        gpu::resident_filter(plan, settings, encoder)
    } else {
        format!(
        "[0:V:0]trim=start_frame={}:end_frame={last_frame},settb=expr={time_base},setpts=N,scale={}:{}:flags=bilinear,format=rgb24,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:color=black,{}[v]",
        plan.first_frame, plan.fitted_width, plan.fitted_height, encoder.software_frame_filter(),
    )
    };
    let audio_filter = match &plan.audio {
        DirectAudioRoute::Source(audio) => {
            let last_sample = audio
                .first_sample
                .checked_add(audio.sample_count)
                .ok_or("Export exceeds the source audio range")?;
            format!(
            "[0:a:0]aresample=48000,aformat=sample_fmts=flt:channel_layouts=stereo,atrim=start_sample={}:end_sample={last_sample},asetpts=PTS-STARTPTS,pan=stereo|c0={}*c0|c1={}*c1,aeval=exprs='clip(val(0),-1,1)|clip(val(1),-1,1)':channel_layout=stereo,apad=whole_len={},aformat=sample_fmts=fltp:sample_rates=48000:channel_layouts=stereo[a]",
            audio.first_sample, audio.left_gain, audio.right_gain, audio.sample_count,
        )
        }
        DirectAudioRoute::Silence => {
            format!("anullsrc=r=48000:cl=stereo,atrim=duration={duration:.12}[a]")
        }
        DirectAudioRoute::Mixed => {
            let audio = mixed_audio.ok_or("The mixed audio stream was not prepared")?;
            command
                .args(["-f", "f32le", "-ar", "48000", "-ac", "2"])
                .arg("-i")
                .arg(&audio.path);
            format!("[1:a:0]apad=whole_dur={:.12},aformat=sample_fmts=fltp:sample_rates=48000:channel_layouts=stereo[a]", duration + 1024.0 / 48_000.0)
        }
    };
    command
        .args(["-filter_complex_threads", "2", "-filter_complex"])
        .arg(format!("{video_filter};{audio_filter}"))
        .args(["-map", "[v]", "-map", "[a]", "-enc_time_base:v", &time_base,
            "-fps_mode", "passthrough", "-frames:v"])
        .arg(plan.frame_count.to_string());
    encoder.configure(&mut command, settings);
    if !gpu_resident {
        encoder.configure_pixel_format(&mut command);
    }
    command
        .args(["-c:a", "aac", "-b:a", "192k", "-ar", "48000", "-t"])
        .arg(format!("{duration:.12}"))
        .args(["-movflags", "+faststart", "-progress", "pipe:1", "-nostats"])
        .arg(&settings.output)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let label = match (&plan.audio, gpu_resident) {
        (DirectAudioRoute::Mixed, true) => {
            format!(
                "GPU decode + resize + encode · timeline audio · {}",
                encoder.label()
            )
        }
        (DirectAudioRoute::Mixed, false) => {
            format!("Direct FFmpeg video + timeline audio · {}", encoder.label())
        }
        (_, true) => format!("GPU decode + resize + encode · {}", encoder.label()),
        (_, false) => format!("Direct FFmpeg · {}", encoder.label()) };
    send_progress(&progress, 0.0, label.clone());
    let child = command.spawn().map_err(|error| error.to_string())?;
    let mut child = EncoderProcess {
        child,
        finished: false };
    let stdout = child.child.stdout.take();
    let stderr = child.child.stderr.take();
    let advancing_frames = Arc::new(AtomicU64::new(0));
    let progress_reader = thread_progress(
        stdout,
        progress,
        plan.frame_count,
        label,
        Some(advancing_frames.clone()),
    );
    let error_reader = thread_stderr(stderr);
    let status = wait_for_encoder(&mut child, cancel, None, Some(&advancing_frames));
    if status.is_err() {
        child.stop();
    }
    let _ = progress_reader.join();
    let error = error_reader.join().unwrap_or_default();
    let status = status?;
    if !status.success() {
        return Err(if error.trim().is_empty() {
            "FFmpeg direct export failed".into()
        } else {
            error.trim().to_string()
        });
    }
    let encoded = advancing_frames.load(Ordering::Relaxed);
    if encoded != plan.frame_count as u64 {
        return Err(format!("FFmpeg encoded {encoded} of {} planned frames. The source may have changed or ended early; the existing output is preserved", plan.frame_count));
    }
    Ok(())
}

fn export_sequence_range(
    project: &Project,
    settings: &ExportSettings,
) -> Result<(i64, i64), String> {
    let (start, end) = if settings.use_in_out {
        let inn = project.sequence.in_point.unwrap_or(0);
        let out = project
            .sequence
            .out_point
            .unwrap_or(project.sequence.duration_frames());
        if out <= inn {
            return Err("Out point is not after the in point".into());
        }
        (inn, out)
    } else {
        let end = project.sequence.duration_frames();
        if end <= 0 {
            return Err("The sequence is empty".into());
        }
        (0, end)
    };
    Ok((start, end))
}

pub fn export_range(project: &Project, settings: &ExportSettings) -> Result<(f64, f64), String> {
    let (start, end) = export_sequence_range(project, settings)?;
    let rate = project.sequence.frame_rate;
    Ok((rate.frame_to_seconds(start), rate.frame_to_seconds(end)))
}

/// Preserve nearest-sequence-frame sampling, but never sample the exclusive
/// boundary of the selected render range. Integer ratios also avoid NTSC ties
/// moving to another picture because of floating-point error.
struct OutputFramePolicy {
    start: i64,
    end: i64,
    step_num: i128,
    step_den: i128 }

impl OutputFramePolicy {
    fn new(sequence: FrameRate, output: FrameRate, start: i64, end: i64) -> Result<Self, String> {
        if sequence.num == 0 || sequence.den == 0 || output.num == 0 || output.den == 0 {
            return Err("Frame rate is invalid".into());
        }
        if end <= start {
            return Err("The render frame range is empty".into());
        }
        Ok(Self {
            start,
            end,
            step_num: sequence.num as i128 * output.den as i128,
            step_den: sequence.den as i128 * output.num as i128 })
    }

    fn frame(&self, output_index: i64) -> i64 {
        let numerator = output_index.max(0) as i128 * self.step_num;
        let nearest = numerator / self.step_den
            + i128::from((numerator % self.step_den) * 2 >= self.step_den);
        (self.start as i128 + nearest).clamp(self.start as i128, self.end as i128 - 1) as i64
    }
}

/// Only used inside one immutable export attempt with fixed canvas dimensions.
/// The image is replaced only after composition and decode validation succeed.
#[derive(Default)]
struct LastCompositeFrame {
    frame: Option<i64>,
    image: Option<Arc<ImageBuf>> }

impl LastCompositeFrame {
    fn get_or_render(
        &mut self,
        frame: i64,
        render: impl FnOnce() -> Result<ImageBuf, String>,
    ) -> Result<Arc<ImageBuf>, String> {
        if self.frame != Some(frame) {
            let image = render()?;
            self.image = Some(Arc::new(image));
            self.frame = Some(frame);
        }
        Ok(Arc::clone(self
            .image
            .as_ref()
            .expect("a cached frame has a successful image")))
    }
}

pub fn export_mp4(
    project: &Project,
    settings: &ExportSettings,
    cancel: &AtomicBool,
    progress: Sender<ExportProgress>,
) -> Result<(), String> {
    if std::env::var("QUATERNIO_EXPORT_WORKER").as_deref() == Ok("1")
        && !worker::is_worker_process()
    {
        return worker::run(project, settings, cancel, progress);
    }
    final_export_result(
        export_mp4_inner(project, settings, cancel, progress),
        cancel,
    )
}

pub fn export_mp4_with_diagnostics(
    project: &Project,
    settings: &ExportSettings,
    cancel: &AtomicBool,
    progress: Sender<ExportProgress>,
    diagnostics: &mut ExportDiagnostics,
) -> Result<(), String> {
    diagnostics::collect(diagnostics, || export_mp4(project, settings, cancel, progress))
}

fn record_encoder(encoder: &VideoEncoder) {
    diagnostics::record(|d| {
        d.backend = Some(match encoder {
            VideoEncoder::Cpu => "libx264",
            VideoEncoder::Nvenc => "h264_nvenc",
            VideoEncoder::Qsv => "h264_qsv",
            VideoEncoder::Amf => "h264_amf",
            VideoEncoder::VideoToolbox => "h264_videotoolbox",
            VideoEncoder::Vaapi(_) => "h264_vaapi",
        }.into());
        d.device = match encoder {
            VideoEncoder::Vaapi(path) => Some(path.to_string_lossy().into_owned()),
            _ => None,
        };
        d.frames_encoded = None;
    });
}

fn export_mp4_inner(
    project: &Project,
    settings: &ExportSettings,
    cancel: &AtomicBool,
    progress: Sender<ExportProgress>,
) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("Export cancelled".into());
    }
    send_progress(&progress, 0.0, "Checking export settings and source media".into());
    export_range(project, settings)?;
    if !settings.frame_rate.fps().is_finite() || settings.frame_rate.fps() <= 0.0 {
        return Err("Frame rate is invalid".into());
    }
    diagnostics::measure(ExportStage::Preflight, || preflight_media(project, settings))?;
    let ffmpeg = export_ffmpeg()?;
    // Device probing and source inspection are independent subprocesses.
    let (encoder, direct, timeline) = std::thread::scope(|scope| -> Result<_, String> {
        let collector = diagnostics::worker_collector();
        let ffmpeg_ref = &ffmpeg;
        let progress_ref = &progress;
        let probe = scope.spawn(move || diagnostics::with_collector(collector, || {
            diagnostics::measure(ExportStage::EncoderProbe, ||
                gpu::preferred(ffmpeg_ref, settings, cancel, progress_ref))
        }));
        let plans = (|| -> Result<_, String> {
            let direct = if let Some(mut plan) = direct_export_plan(project, settings) {
                send_progress(
                    &progress,
                    0.0,
                    "Checking direct export compatibility".into(),
                );
                let (matches, gpu_safe) =
                    diagnostics::measure(ExportStage::RouteProbe, || source_matches_rate(&plan.path, &ffmpeg, settings.frame_rate,
                        (plan.source_width, plan.source_height), cancel))?;
                plan.gpu_safe = gpu_safe;
                matches.then_some(plan)
            } else {
                None
            };
            let timeline = if direct.is_none()
                && std::env::var("QUATERNIO_EXPERIMENTAL_TIMELINE").as_deref() == Ok("1")
            {
                match timeline::plan(project, settings) {
                    Some(plan) if diagnostics::measure(ExportStage::RouteProbe, || timeline::validate(&ffmpeg, &plan, cancel))? => Some(plan),
                    _ => None,
                }
            } else { None };
            Ok((direct, timeline))
        })();
        // Reap the probe even when source validation failed or was cancelled.
        let encoder = probe.join().map_err(|_| "Encoder probe worker failed".to_string())?;
        let (mut direct, mut timeline) = plans?;
        if direct.is_none()
            && std::env::var("QUATERNIO_EXPERIMENTAL_GPU").as_deref() == Ok("1") {
            // Continuous razor cuts can reuse the existing native picture path.
            // Keep long trims on the timeline route's bounded seek implementation.
            direct = timeline.as_ref().and_then(timeline::as_direct)
                .filter(|plan| plan.first_frame == 0);
            if direct.is_some() { timeline = None; }
        }
        Ok((encoder?, direct, timeline))
    })?;
    if cancel.load(Ordering::Relaxed) {
        return Err("Export cancelled".into());
    }
    let staged = StagedOutput::create(&settings.output)?;
    let mut working_settings = settings.clone();
    working_settings.output = staged.path.clone();
    let mixed_audio = diagnostics::measure(ExportStage::AudioPreparation, || -> Result<_, String> { Ok(if let Some(plan) = &direct {
        if matches!(plan.audio, DirectAudioRoute::Mixed) {
            Some(prepare_mixed_audio(
                project,
                plan.timeline_start,
                plan.timeline_start + plan.frame_count,
                cancel,
                &progress,
            )?)
        } else {
            None
        }
    } else {
        // A low output rate can make one encoded picture longer than the
        // selected timeline range. Keep mixing inside the original marks;
        // export_composited pads the encoded tail with silence as needed.
        let (start, end) = export_sequence_range(project, settings)?;
        Some(prepare_mixed_audio(project, start, end, cancel, &progress)?)
    }) })?;
    let timeline_failed = std::cell::Cell::new(false);
    let render = |encoder: &VideoEncoder| {
        record_encoder(encoder);
        if let Some(plan) = &direct {
            let try_hwaccel = gpu::resident_available(
                &ffmpeg,
                plan,
                &working_settings,
                encoder,
                cancel,
                &progress,
            )?;
            match export_direct(
                &ffmpeg,
                plan,
                &working_settings,
                encoder,
                mixed_audio.as_ref(),
                try_hwaccel,
                cancel,
                progress.clone(),
            ) {
                Ok(()) => Ok(()),
                Err(error) if cancel.load(Ordering::Acquire) => Err(error),
                Err(error) if try_hwaccel => {
                    diagnostics::record(|d| d.fallback_reasons.push(error.clone()));
                    record_encoder(encoder);
                    gpu::resident_failed(&ffmpeg, plan, &working_settings);
                    let detail: String = error.chars().take(180).collect();
                    send_progress(
                        &progress,
                        0.0,
                        format!("GPU picture path failed; retrying software pictures · {detail}"),
                    );
                    export_direct(
                        &ffmpeg,
                        plan,
                        &working_settings,
                        encoder,
                        mixed_audio.as_ref(),
                        false,
                        cancel,
                        progress.clone(),
                    )
                }
                Err(error) => Err(error) }
        } else if let Some(plan) = &timeline {
            let audio = mixed_audio.as_ref().ok_or("The mixed audio stream was not prepared")?;
            if !timeline_failed.get() {
                diagnostics::record(|d| {
                    d.route = Some(ExportRoute::Timeline);
                    d.picture_route = Some("cpu_decode_filtergraph".into());
                    d.audio_route = Some("mixed".into());
                });
                match timeline::render(&ffmpeg, plan, &working_settings, encoder, audio, cancel, progress.clone()) {
                    Ok(()) => return Ok(()),
                    Err(error) if cancel.load(Ordering::Acquire) => return Err(error),
                    Err(error) => {
                        timeline_failed.set(true);
                        diagnostics::record(|d| d.fallback_reasons.push(format!("Timeline picture path: {error}")));
                        send_progress(&progress, 0.0, "Timeline picture path failed; retrying compositor".into());
                        record_encoder(encoder);
                    }
                }
            }
            export_composited(project, &working_settings, encoder, audio, cancel, progress.clone())
        } else {
            send_progress(&progress, 0.0, format!("Compositor · {}", encoder.label()));
            export_composited(
                project,
                &working_settings,
                encoder,
                mixed_audio
                    .as_ref()
                    .ok_or("The mixed audio stream was not prepared")?,
                cancel,
                progress.clone(),
            )
        }
    };
    let mut completed_encoder = encoder.clone();
    if let Err(error) = diagnostics::measure(ExportStage::Encode, || render(&encoder)) {
        diagnostics::record(|d| d.fallback_reasons.push(error.clone()));
        if cancel.load(Ordering::Acquire) {
            return Err("Export cancelled".into());
        }
        if encoder == VideoEncoder::Cpu {
            return Err(error);
        }
        gpu::encoder_failed(&ffmpeg, settings, &error);
        if matches!(settings.encoder, EncoderPreference::Hardware) {
            return Err(format!(
                "Required hardware encoding failed ({}); output was not replaced. {error}",
                encoder.description()
            ));
        }
        let detail: String = error.chars().take(180).collect();
        send_progress(
            &progress,
            0.0,
            format!("{} failed; retrying CPU · {detail}", encoder.label()),
        );
        diagnostics::measure(ExportStage::Encode, || render(&VideoEncoder::Cpu)).map_err(|fallback| {
            format!("Hardware encoding failed: {detail}. CPU fallback: {fallback}")
        })?;
        completed_encoder = VideoEncoder::Cpu;
    }
    if cancel.load(Ordering::Relaxed) {
        return Err("Export cancelled".into());
    }
    send_progress(&progress, 0.995, "Finalizing output".into());
    diagnostics::measure(ExportStage::Finalization, || staged.commit(&settings.output, cancel))?;
    let _ = progress.send(ExportProgress {
        preparation: None,
        fraction: 1.0,
        message: format!("Wrote {} · {}", settings.output.display(), completed_encoder.description()),
        remaining_seconds: None,
        done: true,
        error: None });
    Ok(())
}

fn export_composited(
    project: &Project,
    settings: &ExportSettings,
    encoder: &VideoEncoder,
    audio_temp: &TemporaryAudioFile,
    cancel: &AtomicBool,
    progress: Sender<ExportProgress>,
) -> Result<(), String> {
    diagnostics::record(|d| {
        d.route = Some(ExportRoute::Composite);
        d.picture_route = Some("cpu_composite_bounded_pipe".into());
        d.audio_route = Some("mixed".into());
    });
    if cancel.load(Ordering::Relaxed) {
        return Err("Export cancelled".into());
    }
    let ffmpeg = export_ffmpeg()?;
    let (start_sec, end_sec) = export_range(project, settings)?;
    let out_fps = settings.frame_rate.fps();
    if out_fps <= 0.0 {
        return Err("Frame rate is invalid".into());
    }
    let duration = (end_sec - start_sec).max(1.0 / out_fps);
    let frame_count = (duration * out_fps).round().max(1.0) as i64;
    let encoded_duration = settings.frame_rate.frame_to_seconds(frame_count);
    let width = (settings.width.max(16)) & !1;
    let height = (settings.height.max(16)) & !1;

    let mut source = CachingSource::new();
    source.set_project_media(&project.media);
    let seq_rate = project.sequence.frame_rate;
    let (sequence_start, sequence_end) = export_sequence_range(project, settings)?;
    let frame_policy =
        OutputFramePolicy::new(seq_rate, settings.frame_rate, sequence_start, sequence_end)?;
    let mut last_composite = LastCompositeFrame::default();

    if cancel.load(Ordering::Relaxed) {
        return Err("Export cancelled".into());
    }

    if let Some(parent) = settings.output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
    }

    let rate = format!("{}/{}", settings.frame_rate.num, settings.frame_rate.den);
    let mut command = ffmpeg_command(&ffmpeg);
    encoder.configure_device(&mut command);
    command
        .arg("-y")
        .arg("-nostdin")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg("rgb24")
        .arg("-s")
        .arg(format!("{width}x{height}"))
        .arg("-r")
        .arg(&rate)
        .arg("-i")
        .arg("pipe:0")
        .arg("-f")
        .arg("f32le")
        .arg("-ar")
        .arg("48000")
        .arg("-ac")
        .arg("2")
        .arg("-i")
        .arg(&audio_temp.path);
    command.args(["-vf", encoder.software_frame_filter()]);
    encoder.configure(&mut command, settings);
    encoder.configure_pixel_format(&mut command);
    let child = command
        .arg("-c:a")
        .arg("aac")
        .arg("-b:a")
        .arg("192k")
        // Rational sample boundaries can put the PCM end less than one sample
        // before the video end. -shortest would drop a complete final picture.
        // Video's planned count is authoritative; pad only the audio tail and
        // limit both streams to that exact encoded duration instead.
        .arg("-af")
        .arg(format!("apad=whole_dur={:.12},aformat=sample_fmts=fltp:sample_rates=48000:channel_layouts=stereo", encoded_duration + 1024.0 / 48_000.0))
        .arg("-frames:v")
        .arg(frame_count.to_string())
        .arg("-t")
        .arg(format!("{encoded_duration:.12}"))
        .arg("-movflags")
        .arg("+faststart")
        .args(["-progress", "pipe:1", "-nostats"])
        .arg(&settings.output)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| err.to_string())?;
    let mut child = EncoderProcess {
        child,
        finished: false };

    let mut stdin = take_encoder_stdin(&mut child, cancel)?;
    let stderr = child.child.stderr.take();
    let err_thread = thread_stderr(stderr);
    let stdout = child.child.stdout.take();
    let advancing_frames = Arc::new(AtomicU64::new(0));
    let progress_thread = thread_progress(
        stdout,
        progress,
        frame_count,
        format!("Compositor · {}", encoder.label()),
        Some(advancing_frames.clone()),
    );
    let child = Mutex::new(child);
    let monitor_done = AtomicBool::new(false);
    let stalled = AtomicBool::new(false);
    struct MonitorFinished<'a>(&'a AtomicBool);
    impl Drop for MonitorFinished<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let outcome = std::thread::scope(|scope| {
        // Killing FFmpeg from a separate watcher unblocks a write to a stalled
        // encoder's full input pipe when the user cancels.
        scope.spawn(|| {
            let mut last_frame = 0;
            let mut last_advance = Instant::now();
            while !monitor_done.load(Ordering::Acquire) {
                let frame = advancing_frames.load(Ordering::Relaxed);
                if frame != last_frame {
                    last_frame = frame;
                    last_advance = Instant::now();
                }
                let timed_out = last_advance.elapsed() >= Duration::from_secs(300);
                if cancel.load(Ordering::Acquire) || timed_out {
                    stalled.store(timed_out, Ordering::Release);
                    child
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .stop();
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let _finished = MonitorFinished(&monitor_done);
        let outcome = (|| {
            // Bound queued pictures to 64 MiB (at most two frames). The writer
            // owns the pipe so composition can overlap encoder backpressure.
            let frame_bytes = width as usize * height as usize * 3;
            let capacity = (64 * 1024 * 1024 / frame_bytes.max(1)).min(2);
            let (pictures, pending) = std::sync::mpsc::sync_channel::<Arc<ImageBuf>>(capacity);
            let collector = diagnostics::worker_collector();
            let writer = scope.spawn(move || -> Result<(), String> {
                for image in pending {
                    if cancel.load(Ordering::Acquire) {
                        return Err("Export cancelled".into());
                    }
                    let started = Instant::now();
                    let result = stdin.write_all(&image.rgb);
                    if let Some(collector) = &collector {
                        collector.add_timing(ExportStage::PipeWrite, started.elapsed());
                    }
                    result.map_err(|_| "FFmpeg closed the video pipe".to_string())?;
                }
                Ok(())
            });
            let produced = (|| -> Result<(), String> {
              for index in 0..frame_count {
                if cancel.load(Ordering::Relaxed) {
                    return Err("Export cancelled".into());
                }
                let frame = frame_policy.frame(index);
                let image = last_composite.get_or_render(frame, || {
                    let image = diagnostics::measure(ExportStage::Composition, || composite(project, frame, width, height, &mut source));
                    if let Some(error) = source.take_decode_error() {
                        return Err(format!("Could not render frame {frame}: {error}"));
                    }
                    Ok(image)
                })?;
                pictures.send(image)
                    .map_err(|_| "FFmpeg closed the video pipe".to_string())?;
              }
              Ok(())
            })();
            drop(pictures);
            if produced.is_err() {
                child.lock().unwrap_or_else(|error| error.into_inner()).stop();
            }
            let written = writer.join().map_err(|_| "Export pipe worker stopped unexpectedly")?;
            produced?;
            written?;
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err("Export cancelled".into());
                }
                {
                    let mut child = child.lock().unwrap_or_else(|error| error.into_inner());
                    if let Some(status) =
                        child.child.try_wait().map_err(|error| error.to_string())?
                    {
                        child.finished = true;
                        return Ok(status);
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        })();
        if outcome.is_err() {
            child
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .stop();
        }
        outcome
    });
    let _ = progress_thread.join();
    let err_text = err_thread.join().unwrap_or_default();
    let status = outcome.map_err(|error| {
        if stalled.load(Ordering::Acquire) {
            "Export produced no new encoded frames for five minutes; the existing output is preserved".into()
        } else if error == "FFmpeg closed the video pipe" && !err_text.trim().is_empty() {
            err_text.trim().to_string()
        } else {
            error
        }
    })?;
    if !status.success() {
        return Err(if err_text.trim().is_empty() {
            "FFmpeg failed to encode the movie".into()
        } else {
            err_text.trim().to_string()
        });
    }
    let encoded = advancing_frames.load(Ordering::Relaxed);
    if encoded != frame_count as u64 {
        return Err(format!("FFmpeg encoded {encoded} of {frame_count} planned frames. Export stopped; the existing output is preserved"));
    }
    Ok(())
}

fn write_f32(file: &mut File, samples: &[f32]) -> Result<(), String> {
    let mut bytes = Vec::with_capacity(samples.len() * 4);
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    file.write_all(&bytes).map_err(|err| err.to_string())
}

fn thread_stderr(stderr: Option<std::process::ChildStderr>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let Some(mut stderr) = stderr else {
            return String::new();
        };
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let Ok(count) = std::io::Read::read(&mut stderr, &mut buffer) else {
                break;
            };
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
            if bytes.len() > 65_536 {
                bytes.drain(..bytes.len() - 65_536);
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

