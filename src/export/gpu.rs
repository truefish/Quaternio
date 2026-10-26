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
    delivery:Delivery,
    backend:Option<String>,
    candidates: Vec<VideoEncoder>,
    runtime: Vec<Option<std::ffi::OsString>>,
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
        delivery:settings.delivery,backend:settings.backend.clone(),
        candidates,
        runtime: runtime_identity(),
    })
}

fn runtime_identity() -> Vec<Option<std::ffi::OsString>> {
    let mut identity: Vec<_> = ["CUDA_VISIBLE_DEVICES", "CUDA_DEVICE_ORDER", "NVIDIA_VISIBLE_DEVICES",
        "LIBVA_DRIVER_NAME", "LIBVA_DRIVERS_PATH", "LD_LIBRARY_PATH", "PATH",
        "QUATERNIO_VAAPI_DEVICE"].iter().map(std::env::var_os).collect();
    #[cfg(windows)]
    if let Some(system) = std::env::var_os("SystemRoot") {
        let system = PathBuf::from(system).join("System32");
        for name in ["nvEncodeAPI64.dll", "nvcuda.dll", "amfrt64.dll", "libmfxhw64.dll", "libvpl.dll"] {
            identity.push(fingerprint(&system.join(name)).map(|f| format!("{f:?}").into()));
        }
    }
    #[cfg(target_os = "linux")]
    for path in ["/proc/driver/nvidia/version", "/sys/module/i915/version", "/sys/module/xe/version", "/sys/module/amdgpu/version"] {
        use std::io::Read;
        let mut text = String::new();
        identity.push(File::open(path).ok().and_then(|file|
            file.take(4096).read_to_string(&mut text).ok().map(|_| text.into())));
    }
    identity
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
        encoders.extend([VideoEncoder::Qsv, VideoEncoder::Amf(None)]);
        return (encoders, reason);
    }
    if cfg!(windows) {
        return (vec![VideoEncoder::Nvenc, VideoEncoder::Qsv, VideoEncoder::Amf(None)], String::new());
    }
    (Vec::new(), "No hardware export backend is configured for this platform. ".into())
}

fn is_amd_vaapi(encoder: &VideoEncoder) -> bool {
    let VideoEncoder::Vaapi(device) = encoder else { return false; };
    let Some(name) = device.file_name() else { return false; };
    let vendor = PathBuf::from("/sys/class/drm").join(name).join("device/vendor");
    std::fs::read_to_string(vendor).is_ok_and(|value| value.trim().eq_ignore_ascii_case("0x1002"))
}

fn selected_candidates(settings: &ExportSettings) -> (Vec<VideoEncoder>, String) {
    let (mut candidates, reason) = candidates();
    if let EncoderPreference::Amd(adapter) = settings.encoder {
        if cfg!(windows) { candidates = vec![VideoEncoder::Amf(adapter)]; }
        candidates.retain(|encoder| if cfg!(windows) { matches!(encoder, VideoEncoder::Amf(_)) }
            else if cfg!(target_os = "linux") { is_amd_vaapi(encoder) } else { false });
    }
    (candidates, reason)
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
    if settings.backend.as_deref().is_some_and(|b|b=="libx264"||b=="libx265") || settings.backend.is_none()&&matches!(settings.encoder, EncoderPreference::Cpu) {
        return Ok(VideoEncoder::Cpu);
    }
    let (mut candidates, discovery_reason) = selected_candidates(settings);
    if let Some(name)=&settings.backend{candidates.retain(|c|match c{VideoEncoder::Nvenc=>name.ends_with("_nvenc"),VideoEncoder::Qsv=>name.ends_with("_qsv"),VideoEncoder::Amf(_)=>name.ends_with("_amf"),VideoEncoder::VideoToolbox=>name.ends_with("_videotoolbox"),VideoEncoder::Vaapi(_)=>name.ends_with("_vaapi"),_=>false});if candidates.is_empty(){return Err("The selected encoder is not available on this system".into());}}
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
            command.args(["-f", settings.delivery.stream_format(), "pipe:1"]);
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
    if !selection.reason.is_empty() {
        diagnostics::record(|d| d.fallback_reasons.push(selection.reason.clone()));
    }
    if selection.encoder == VideoEncoder::Cpu {
        if settings.backend.is_some() || matches!(settings.encoder, EncoderPreference::Hardware | EncoderPreference::Amd(_)) {
            return Err(format!(
                "Hardware encoding is unavailable at {}x{} {}/{} fps using {}. {}Check this FFmpeg build's selected codec encoders and the GPU driver/device access, or choose Auto/CPU.",
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

pub(super) fn encoder_failed(ffmpeg: &Path, settings: &ExportSettings) {
    if let Some(key) = selection_key(ffmpeg, settings, selected_candidates(settings).0) {
        let mut cache = ENCODERS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // An export can fail because of media decoding, disk IO or muxing.
        // Only an executed encoder probe may certify hardware unavailable.
        // Invalidate the old success so the next job performs that probe.
        cache.remove(&key);
    }
}

// Native hardware frame filters are mandatory: never insert hwupload/hwdownload
// to make a "resident" probe succeed. This establishes the FFmpeg frame route,
// not a guarantee about copies inside a driver. VAAPI options are documented in
// https://ffmpeg.org/doxygen/8.1/vf__scale__vaapi_8c.html.
// Experimental backends must pass the complete decode/filter/encode probe.
// AMF uses native AMF surfaces and vpp_amf; installations without this route
// fail the complete probe and retain the software-frame AMF encoder.
// https://github.com/GPUOpen-LibrariesAndSDKs/AMF/wiki/FFmpeg-and-AMF-HW-Acceleration
fn resident_scale(encoder: &VideoEncoder, width: u32, height: u32) -> Option<String> {
    match encoder {
        VideoEncoder::Nvenc => Some(format!("scale_cuda={width}:{height}:format=nv12:interp_algo=bilinear")),
        VideoEncoder::Vaapi(_) => Some(format!("scale_vaapi=w={width}:h={height}:format=nv12")),
        VideoEncoder::Qsv => Some(format!("scale_qsv=w={width}:h={height}:format=nv12")),
        VideoEncoder::Amf(_) => Some(format!("vpp_amf=w={width}:h={height}:format=nv12")),
        VideoEncoder::VideoToolbox => Some(format!("scale_vt=w={width}:h={height}")),
        _ => None,
    }
}

/// Owns device setup AND the first input. Call instead of configure_device/-i
/// for a resident attempt, so probing and actual export use the same device.
pub(super) fn configure_resident_input(command: &mut Command, encoder: &VideoEncoder, plan: &DirectExport) {
    match encoder {
        VideoEncoder::Nvenc => { command.args(["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"]); }
        VideoEncoder::Amf(_) => {
            encoder.configure_device(command);
            command.args(["-hwaccel", "amf", "-hwaccel_output_format", "amf"]);
            if matches!(encoder, VideoEncoder::Amf(Some(_))) { command.args(["-hwaccel_device", "quaternio_amf"]); }
        }
        VideoEncoder::Vaapi(device) => {
            command.args(["-hwaccel", "vaapi", "-hwaccel_device"]).arg(device)
                .args(["-hwaccel_output_format", "vaapi"]);
        }
        VideoEncoder::Qsv => {
            command.args(["-hwaccel", "qsv", "-hwaccel_output_format", "qsv"]);
        }
        VideoEncoder::VideoToolbox => {
            command.args(["-hwaccel", "videotoolbox", "-hwaccel_output_format", "videotoolbox_vld"]);
        }
        _ => unreachable!("resident input requires an eligible backend"),
    }
    command.arg("-i").arg(&plan.path);
}

pub(super) fn resident_filter(plan: &DirectExport, settings: &ExportSettings, encoder: &VideoEncoder) -> String {
    let scale = if plan.source_width == plan.fitted_width && plan.source_height == plan.fitted_height {
        "null".into()
    } else {
        resident_scale(encoder, plan.fitted_width, plan.fitted_height)
            .expect("resident filter requires an eligible backend")
    };
    let end = plan.first_frame.checked_add(plan.frame_count).expect("validated resident frame range");
    format!("[0:V:0]trim=start_frame={}:end_frame={end},settb=expr={}/{},setpts=N,{scale}[v]",
        plan.first_frame, settings.frame_rate.den, settings.frame_rate.num)
}

fn enabled(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| value == "1")
}

pub(super) fn resident_eligible(plan: &DirectExport, settings: &ExportSettings, encoder: &VideoEncoder) -> bool {
    !enabled("QUATERNIO_DISABLE_GPU_RESIDENT")
        && (*encoder == VideoEncoder::Nvenc
            || (cfg!(windows) && matches!(encoder, VideoEncoder::Amf(_)))
            || (enabled("QUATERNIO_EXPERIMENTAL_GPU")
                && ((cfg!(target_os = "linux") && matches!(encoder, VideoEncoder::Vaapi(_)))
                    || ((cfg!(windows) || cfg!(target_os = "linux")) && *encoder == VideoEncoder::Qsv)
                    || (cfg!(target_os = "macos") && *encoder == VideoEncoder::VideoToolbox))))
        && plan.gpu_safe
        && plan.first_frame >= 0 && plan.frame_count > 0
        && plan.first_frame.checked_add(plan.frame_count).is_some()
        && settings.frame_rate.num > 0 && settings.frame_rate.den > 0
        && plan.fitted_width == (settings.width.max(16) & !1)
        && plan.fitted_height == (settings.height.max(16) & !1)
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ResidentKey {
    executable: Fingerprint,
    source: Fingerprint,
    width: u32,
    height: u32,
    source_size: (u32, u32),
    first_frame: i64,
    frame_count: i64,
    speed: RenderSpeed,
    rate: (u32, u32),
    quality: u8,
    delivery:Delivery,
    backend:Option<String>,
    encoder: VideoEncoder,
    device: Option<Fingerprint>,
    // Environment identity is cheap and bounded; no driver discovery subprocess.
    // Driver replacement not reflected here is covered by the short cache TTL.
    runtime: Vec<Option<std::ffi::OsString>>,
}
fn resident_key(ffmpeg: &Path, plan: &DirectExport, settings: &ExportSettings, encoder: &VideoEncoder) -> Option<ResidentKey> {
    Some(ResidentKey {
        executable: fingerprint(ffmpeg)?, source: fingerprint(&plan.path)?,
        width: plan.fitted_width, height: plan.fitted_height,
        source_size: (plan.source_width, plan.source_height),
        first_frame: plan.first_frame, frame_count: plan.frame_count,
        speed: settings.speed, rate: (settings.frame_rate.num, settings.frame_rate.den),
        quality: settings.crf.clamp(1, 40),
        delivery:settings.delivery,backend:settings.backend.clone(), encoder: encoder.clone(),
        device: match encoder { VideoEncoder::Vaapi(path) => fingerprint(path), _ => None },
        runtime: runtime_identity(),
    })
}
#[derive(Clone)]
struct ResidentResult { failure: Option<String>, at: Instant }
static RESIDENT: OnceLock<Mutex<HashMap<ResidentKey, ResidentResult>>> = OnceLock::new();
fn cache_resident(key: ResidentKey, failure: Option<String>) {
    let mut cache = RESIDENT.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() >= 32 { cache.clear(); }
    cache.insert(key, ResidentResult { failure, at: Instant::now() });
}
fn record_resident_failure(reason: &str) {
    diagnostics::record(|d| d.fallback_reasons.push(format!("GPU resident route: {reason}")));
}
pub(super) fn resident_available(
    ffmpeg: &Path, plan: &DirectExport, settings: &ExportSettings,
    encoder: &VideoEncoder, cancel: &AtomicBool, progress: &Sender<ExportProgress>,
) -> Result<bool, String> {
    if cancel.load(Ordering::Acquire) { return Err("Export cancelled".into()); }
    if !resident_eligible(plan, settings, encoder) {
        if enabled("QUATERNIO_DISABLE_GPU_RESIDENT") {
            record_resident_failure("disabled by QUATERNIO_DISABLE_GPU_RESIDENT=1");
        }
        return Ok(false);
    }
    let key = resident_key(ffmpeg, plan, settings, encoder);
    let cached = key.as_ref().and_then(|key| RESIDENT.get_or_init(Default::default)
        .lock().unwrap_or_else(|e| e.into_inner()).get(key).cloned())
        .filter(|entry| entry.at.elapsed() < Duration::from_secs(60));
    if let Some(entry) = cached {
        if let Some(reason) = &entry.failure { record_resident_failure(reason); }
        return Ok(entry.failure.is_none());
    }
    send_progress(progress, 0.0, "Checking GPU decode, resize and encode path".into());
    let mut command = ffmpeg_command(ffmpeg);
    configure_resident_input(&mut command, encoder, plan);
    command.args(["-an", "-filter_complex"]).arg(resident_filter(plan, settings, encoder))
        .args(["-map", "[v]", "-frames:v"]).arg(plan.frame_count.min(6).to_string())
        .arg("-enc_time_base:v").arg(format!("{}/{}", settings.frame_rate.den, settings.frame_rate.num))
        .args(crate::media_engine::passthrough_args(ffmpeg)?);
    encoder.configure(&mut command, settings);
    command.args(["-f", settings.delivery.stream_format(), "pipe:1"]);
    // Long source trims may time out. Conservatively use the ordinary route;
    // never replace frame-accurate trimming with an approximate input seek.
    let result = diagnostics::measure(diagnostics::ExportStage::RouteProbe,
        || probe(command, cancel, Duration::from_secs(8)))?;
    let failure = result.err().map(|reason| reason.chars().take(1600).collect::<String>());
    if let Some(reason) = &failure { record_resident_failure(reason); }
    let available = failure.is_none();
    if let Some(key) = key { cache_resident(key, failure); }
    Ok(available)
}

// Preserve the parent API: invalidate every backend for this source/settings,
// retaining the precise encoder/runtime keys instead of guessing the encoder.
pub(super) fn resident_failed(ffmpeg: &Path, plan: &DirectExport, settings: &ExportSettings) {
    let Some((ff, source)) = fingerprint(ffmpeg).zip(fingerprint(&plan.path)) else { return; };
    let mut cache = RESIDENT.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    for (key, entry) in cache.iter_mut() {
        if key.executable == ff && key.source == source
            && key.width == plan.fitted_width && key.height == plan.fitted_height
            && key.rate == (settings.frame_rate.num, settings.frame_rate.den)
            && key.quality == settings.crf.clamp(1, 40) && key.speed == settings.speed {
            *entry = ResidentResult { failure: Some("previous resident export failed".into()), at: Instant::now() };
        }
    }
}
