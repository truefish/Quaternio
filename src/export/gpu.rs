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
type SelectionKey = (Fingerprint, RenderSpeed, u32, u32);
static ENCODERS: OnceLock<Mutex<HashMap<SelectionKey, Selection>>> = OnceLock::new();

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

fn probe(mut command: Command, cancel: &AtomicBool) -> Result<Result<(), String>, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return Ok(Err(e.to_string())) };
    let mut child = EncoderProcess {
        child,
        finished: false };
    let stderr = child.child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        // Drain all diagnostic output but retain a bounded useful prefix.
        let mut message = String::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if message.len() < 4096 {
                message.extend(line.chars().take(4096 - message.len()));
                message.push('\n');
            }
        }
        message
    });
    let result = wait_for_encoder(&mut child, cancel, Some(Duration::from_secs(8)), None);
    if result.is_err() {
        child.stop();
    }
    let detail = reader.join().unwrap_or_default();
    if cancel.load(Ordering::Acquire) {
        return Err("Export cancelled".into());
    }
    Ok(match result {
        Ok(status) if status.success() => Ok(()),
        Ok(_) => Err(if detail.is_empty() {
            "Hardware encoder rejected the probe".into()
        } else {
            detail
        }),
        Err(error) => Err(error) })
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
    let key = fingerprint(ffmpeg).map(|stamp| {
        (
            stamp,
            settings.speed,
            settings.width.max(16) & !1,
            settings.height.max(16) & !1,
        )
    });
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
            reason: String::new(),
            at: Instant::now() };
        for encoder in [VideoEncoder::Nvenc, VideoEncoder::Qsv, VideoEncoder::Amf] {
            if cancel.load(Ordering::Acquire) {
                return Err("Export cancelled".into());
            }
            send_progress(
                progress,
                0.0,
                format!("Checking {} at export size", encoder.label()),
            );
            let mut command = ffmpeg_command(ffmpeg);
            command
                .args(["-f", "lavfi", "-i"])
                .arg(format!(
                    "color=c=black:s={}x{}:r=30:d=0.2",
                    settings.width.max(16) & !1,
                    settings.height.max(16) & !1
                ))
                .args(["-an", "-frames:v", "6", "-pix_fmt", "yuv420p"]);
            encoder.configure(&mut command, settings.crf, settings.speed);
            command.args(["-f", "null", "-"]);
            match probe(command, cancel)? {
                Ok(()) => {
                    selection.encoder = encoder;
                    break;
                }
                Err(reason) => {
                    selection.reason.push_str(&format!(
                        "{}: {} ",
                        encoder.label(),
                        reason.lines().take(2).collect::<Vec<_>>().join(" ")
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
                "Hardware encoding is unavailable. {}",
                selection.reason
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
    if let Some(stamp) = fingerprint(ffmpeg) {
        let mut cache = ENCODERS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        cache.insert(
            (
                stamp,
                settings.speed,
                settings.width.max(16) & !1,
                settings.height.max(16) & !1,
            ),
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
    encoder: VideoEncoder,
) -> bool {
    encoder == VideoEncoder::Nvenc
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
    encoder: VideoEncoder,
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
    encoder.configure(&mut command, settings.crf, settings.speed);
    command.args(["-f", "null", "-"]);
    let available = probe(command, cancel)?.is_ok();
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
