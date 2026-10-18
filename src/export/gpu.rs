//! Executed capability checks, not encoder-list guesses. Every child is owned,
//! cancellable and reaped; negative results expire and executable changes invalidate.
use super::*;
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Fingerprint(PathBuf, u64, SystemTime);
fn fingerprint(path: &Path) -> Option<Fingerprint> {
    let m = std::fs::metadata(path).ok()?;
    Some(Fingerprint(path.to_path_buf(), m.len(), m.modified().ok()?))
}
#[derive(Clone)]
struct Selection {
    encoder: VideoEncoder,
    reason: String,
    at: Instant }
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SelectionKey {
    executable: Fingerprint,
    speed: RenderSpeed,
    width: u32,
    height: u32,
    rate: (u32, u32),
    quality: u8,
    candidates: Vec<VideoEncoder>,
}
static ENCODERS: OnceLock<Mutex<HashMap<SelectionKey, Selection>>> = OnceLock::new();

fn selection_key(ffmpeg: &Path, settings: &ExportSettings, candidates: Vec<VideoEncoder>) -> Option<SelectionKey> {
    Some(SelectionKey {
        executable: fingerprint(ffmpeg)?,
        speed: settings.speed,
        width: settings.width.max(16) & !1,
        height: settings.height.max(16) & !1,
        rate: (settings.frame_rate.num, settings.frame_rate.den),
        quality: settings.crf.clamp(1, 40),
        candidates,
    })
}

fn candidates() -> (Vec<VideoEncoder>, String) {
    if cfg!(target_os = "macos") {
        return (vec![VideoEncoder::VideoToolbox], String::new());
    }
    if cfg!(target_os = "linux") {
        let mut reason = String::new();
        let mut devices = if let Some(device) = std::env::var_os("QUATERNIO_VAAPI_DEVICE") {
            let device = PathBuf::from(device);
            if device.is_absolute() {
                // Let the executed probe diagnose permissions/driver problems.
                vec![device]
            } else {
                reason.push_str("VAAPI: QUATERNIO_VAAPI_DEVICE must be an absolute device path. ");
                Vec::new()
            }
        } else {
            match std::fs::read_dir("/dev/dri") {
                Ok(entries) => entries.filter_map(Result::ok).filter(|entry| {
                    entry.file_name().to_str().is_some_and(|name| {
                        name.strip_prefix("renderD").is_some_and(|number| {
                            !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
                        })
                    })
                }).map(|entry| entry.path()).collect(),
                Err(error) => {
                    reason.push_str(&format!("VAAPI: cannot discover /dev/dri render devices: {error}. "));
                    Vec::new()
                }
            }
        };
        devices.sort();
        if devices.is_empty() && reason.is_empty() {
            reason.push_str("VAAPI: no /dev/dri/renderD* devices found. ");
        }
        if devices.len() > 4 {
            reason.push_str("VAAPI: probing the first four render devices; set QUATERNIO_VAAPI_DEVICE to select another. ");
            devices.truncate(4);
        }
        // NVIDIA keeps its existing route. Prefer VAAPI to QSV/AMF on Linux:
        // it handles both Intel and AMD without their optional vendor runtimes.
        let mut encoders = vec![VideoEncoder::Nvenc];
        encoders.extend(devices.into_iter().map(VideoEncoder::Vaapi));
        encoders.extend([VideoEncoder::Qsv, VideoEncoder::Amf]);
        return (encoders, reason);
    }
    if cfg!(windows) {
        return (vec![VideoEncoder::Nvenc, VideoEncoder::Qsv, VideoEncoder::Amf], String::new());
    }
    (Vec::new(), "No hardware export backend is configured for this platform. ".into())
}

pub(super) fn safe_source_metadata(stream: &serde_json::Value) -> bool {
    let value = |key: &str| stream[key].as_str().unwrap_or("unknown");
    matches!(value("codec_name"), "h264" | "hevc")
        && matches!(value("pix_fmt"), "yuv420p" | "nv12")
        && matches!(value("sample_aspect_ratio"), "1:1" | "N/A" | "unknown")
        && matches!(value("color_range"), "tv" | "unknown")
        && matches!(
            value("color_space"),
            "bt709" | "smpte170m" | "bt470bg" | "unknown"
        )
        && matches!(
            value("color_transfer"),
            "bt709" | "smpte170m" | "bt470bg" | "unknown"
        )
        && matches!(
            value("color_primaries"),
            "bt709" | "smpte170m" | "bt470bg" | "unknown"
        )
        && stream["tags"]["rotate"].as_str().is_none_or(|r| r == "0")
        && stream["side_data_list"]
            .as_array()
            .is_none_or(|items| items.iter().all(|item| item.get("rotation").is_none()))
}

fn probe(mut command: Command, cancel: &AtomicBool, timeout: Duration) -> Result<Result<(), String>, String> {
    if cancel.load(Ordering::Acquire) {
        return Err("Export cancelled".into());
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return Ok(Err(e.to_string())) };
    let mut child = EncoderProcess {
        child,
        finished: false };
    let reader = thread_stderr(child.child.stderr.take());
    let mut stdout = child.child.stdout.take().unwrap();
    let output = std::thread::spawn(move || {
        // A successful exit without any encoded packets is not capability.
        // Drain the stream with constant memory; never retain the video data.
        std::io::copy(&mut stdout, &mut std::io::sink()).unwrap_or(0)
    });
    let result = wait_for_encoder(&mut child, cancel, Some(timeout), None);
    if result.is_err() {
        child.stop();
    }
    let detail = reader.join().unwrap_or_default();
    let bytes = output.join().unwrap_or(0);
    if cancel.load(Ordering::Acquire) {
        return Err("Export cancelled".into());
    }
    Ok(match result {
        Ok(status) if status.success() && bytes > 0 => Ok(()),
        Ok(status) if status.success() => Err("FFmpeg produced no H.264 packets during the probe".into()),
        Ok(_) => Err(if detail.is_empty() {
            "Hardware encoder rejected the probe".into()
        } else {
            detail
        }),
        Err(error) => Err(format!("{error} (probe limit {:.1}s)", timeout.as_secs_f32())) })
}

pub(super) fn preferred(
    ffmpeg: &Path,
    settings: &ExportSettings,
    cancel: &AtomicBool,
    progress: &Sender<ExportProgress>,
) -> Result<VideoEncoder, String> {
    if cancel.load(Ordering::Acquire) {
        return Err("Export cancelled".into());
    }
    if matches!(settings.encoder, EncoderPreference::Cpu) {
        return Ok(VideoEncoder::Cpu);
    }
    let (candidates, discovery_reason) = candidates();
    let key = selection_key(ffmpeg, settings, candidates.clone());
    let cached = key
        .as_ref()
        .and_then(|key| {
            ENCODERS
                .get_or_init(Default::default)
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(key)
                .cloned()
        })
        .filter(|entry| {
            entry.at.elapsed()
                < Duration::from_secs(if entry.encoder == VideoEncoder::Cpu {
                    60
                } else {
                    600
                })
        });
    let selection = if let Some(cached) = cached {
        cached
    } else {
        let mut selection = Selection {
            encoder: VideoEncoder::Cpu,
            reason: discovery_reason,
            at: Instant::now() };
        let started = Instant::now();
        for encoder in candidates {
            if cancel.load(Ordering::Acquire) {
                return Err("Export cancelled".into());
            }
            let remaining = Duration::from_secs(30).saturating_sub(started.elapsed());
            if remaining.is_zero() {
                selection.reason.push_str("Hardware search reached its 30s limit; remaining candidates were not checked. ");
                break;
            }
            send_progress(
                progress,
                0.0,
                format!("Checking {} at export size and frame rate", encoder.description()),
            );
            let mut command = ffmpeg_command(ffmpeg);
            encoder.configure_device(&mut command);
            command
                .args(["-f", "lavfi", "-i"])
                .arg(format!(
                    "color=c=black:s={}x{}:r={}/{},format=rgb24",
                    settings.width.max(16) & !1,
                    settings.height.max(16) & !1,
                    settings.frame_rate.num, settings.frame_rate.den,
                ))
                .args(["-an", "-frames:v", "6", "-vf", encoder.software_frame_filter()]);
            encoder.configure(&mut command, settings);
            encoder.configure_pixel_format(&mut command);
            command.args(["-f", "h264", "pipe:1"]);
            match probe(command, cancel, remaining.min(Duration::from_secs(8)))? {
                Ok(()) => {
                    selection.encoder = encoder;
                    break;
                }
                Err(reason) => {
                    selection.reason.push_str(&format!(
                        "{}: {} ",
                        encoder.description(),
                        reason.lines().take(4).collect::<Vec<_>>().join(" ").chars().take(800).collect::<String>()
                    ));
                }
            }
        }
        if let Some(key) = key {
            let mut cache = ENCODERS
                .get_or_init(Default::default)
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if cache.len() >= 32 {
                cache.clear();
            }
            cache.insert(key, selection.clone());
        }
        selection
    };
    if selection.encoder == VideoEncoder::Cpu {
        if matches!(settings.encoder, EncoderPreference::Hardware) {
            return Err(format!(
                "Hardware encoding is unavailable at {}x{} {}/{} fps using {}. {}Check this FFmpeg build's H.264 encoders and the GPU driver/device access, or choose Auto/CPU.",
                settings.width.max(16) & !1, settings.height.max(16) & !1,
                settings.frame_rate.num, settings.frame_rate.den, ffmpeg.display(), selection.reason
            ));
        }
        send_progress(
            progress,
            0.0,
            format!(
                "Hardware encoding unavailable; using CPU · {}",
                selection.reason
            ),
        );
    }
    Ok(selection.encoder)
}

pub(super) fn encoder_failed(ffmpeg: &Path, settings: &ExportSettings, reason: &str) {
    if let Some(key) = selection_key(ffmpeg, settings, candidates().0) {
        let mut cache = ENCODERS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        cache.insert(
            key,
            Selection {
                encoder: VideoEncoder::Cpu,
                reason: reason.chars().take(500).collect(),
                at: Instant::now() },
        );
    }
}

pub(super) fn resident_filter(plan: &DirectExport, settings: &ExportSettings) -> String {
    format!("[0:V:0]trim=start_frame={}:end_frame={},setpts=N/(({}/{})*TB),scale_cuda={}:{}:format=nv12:interp_algo=bilinear[v]",plan.first_frame,plan.first_frame+plan.frame_count,settings.frame_rate.num,settings.frame_rate.den,plan.fitted_width,plan.fitted_height)
}
pub(super) fn resident_eligible(
    plan: &DirectExport,
    settings: &ExportSettings,
    encoder: &VideoEncoder,
) -> bool {
    *encoder == VideoEncoder::Nvenc
        && plan.gpu_safe
        && plan.fitted_width == (settings.width.max(16) & !1)
        && plan.fitted_height == (settings.height.max(16) & !1)
}
type ResidentKey = (Fingerprint, Fingerprint, u32, u32, RenderSpeed);
static RESIDENT: OnceLock<Mutex<HashMap<ResidentKey, (bool, Instant)>>> = OnceLock::new();
pub(super) fn resident_available(
    ffmpeg: &Path,
    plan: &DirectExport,
    settings: &ExportSettings,
    encoder: &VideoEncoder,
    cancel: &AtomicBool,
    progress: &Sender<ExportProgress>,
) -> Result<bool, String> {
    if !resident_eligible(plan, settings, encoder) {
        return Ok(false);
    }
    let key = fingerprint(ffmpeg)
        .zip(fingerprint(&plan.path))
        .map(|(ff, source)| {
            (
                ff,
                source,
                plan.fitted_width,
                plan.fitted_height,
                settings.speed,
            )
        });
    if let Some(result) = key
        .as_ref()
        .and_then(|key| {
            RESIDENT
                .get_or_init(Default::default)
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(key)
                .copied()
        })
        .filter(|(_, at)| at.elapsed() < Duration::from_secs(60))
    {
        return Ok(result.0);
    }
    send_progress(
        progress,
        0.0,
        "Checking GPU decode, resize and encode path".into(),
    );
    let mut command = ffmpeg_command(ffmpeg);
    command.args(["-hwaccel","cuda","-hwaccel_output_format","cuda","-i"]).arg(&plan.path)
        .args(["-map","0:V:0","-an","-vf"]).arg(format!("trim=start_frame=0:end_frame=6,setpts=PTS-STARTPTS,scale_cuda={}:{}:format=nv12:interp_algo=bilinear",plan.fitted_width,plan.fitted_height))
        .args(["-frames:v","6"]);
    encoder.configure(&mut command, settings);
    command.args(["-f", "h264", "pipe:1"]);
    let available = probe(command, cancel, Duration::from_secs(8))?.is_ok();
    if let Some(key) = key {
        let mut cache = RESIDENT
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if cache.len() >= 32 {
            cache.clear();
        }
        cache.insert(key, (available, Instant::now()));
    }
    Ok(available)
}
pub(super) fn resident_failed(ffmpeg: &Path, plan: &DirectExport, settings: &ExportSettings) {
    if let Some((ff, source)) = fingerprint(ffmpeg).zip(fingerprint(&plan.path)) {
        RESIDENT
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                (
                    ff,
                    source,
                    plan.fitted_width,
                    plan.fitted_height,
                    settings.speed,
                ),
                (false, Instant::now()),
            );
    }
}
