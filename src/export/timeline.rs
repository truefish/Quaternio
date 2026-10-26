//! Experimental CPU-filtered timeline route; publication and opt-in belong to the parent.
use super::*;

const MAX_INPUTS: usize = 32;

#[derive(Debug)]
struct Segment {
    frames: i64,
    // Path, first frame, fitted width/height, original media width/height.
    source: Option<(PathBuf, i64, u32, u32, u32, u32)>,
}

#[derive(Debug)]
pub(super) struct TimelinePlan {
    pub(super) timeline_start: i64,
    pub(super) frame_count: i64,
    rate: FrameRate,
    width: u32,
    height: u32,
    segments: Vec<Segment>,
}

pub(super) fn plan(project: &Project, settings: &ExportSettings) -> Option<TimelinePlan> {
    if crate::playback::precision::needed(project){return None;}
    let rate = project.sequence.frame_rate;
    if settings.force_composite || settings.frame_rate != rate || rate.num == 0
        || rate.den == 0 || rate.fps() < 1.0 { return None; }
    let (start, end) = export_sequence_range(project, settings).ok()?;
    if start < 0 || end <= start { return None; }
    if project.sequence.transitions.iter().any(|t| !t.kind.is_audio()
        && transition_window(&project.sequence, t).map_or(true, |(a,b)| a < end && b > start)) {
        return None;
    }
    let mut clips = Vec::new();
    for track in project.sequence.tracks.iter().filter(|t| t.kind == TrackKind::Video && !t.muted) {
        for clip in &track.clips {
            let clip_end = clip.timeline_start.checked_add(clip.duration)?;
            if clip.duration <= 0 { return None; }
            if clip.timeline_start < end && clip_end > start {
                clips.push((clip, clip_end));
                if clips.len() > 4096 { return None; }
            }
        }
    }
    if clips.is_empty() { return None; }
    clips.sort_by_key(|(clip, _)| clip.timeline_start);
    let width = settings.width.max(16) & !1;
    let height = settings.height.max(16) & !1;
    let mut cursor = start;
    let mut segments = Vec::new();
    let mut inputs = 0;
    for (clip, clip_end) in clips {
        let a = clip.timeline_start.max(start);
        let b = clip_end.min(end);
        if a < cursor || clip.source_in < 0 || clip.source_time_offset != 0.0
            || clip.scale != 1.0 || clip.pos_x != 0.0 || clip.pos_y != 0.0
            || clip.opacity != 1.0 || !clip.grade.is_identity() || !clip.transform.is_identity() || !clip.creative.is_identity() { return None; }
        let media = project.media(clip.media_id)?;
        if media.kind != MediaKind::Video || !media.has_video || media.width < 2
            || media.height < 2 || !media.source_fps.is_finite()
            || (media.source_fps - rate.fps()).abs() > 0.000001 { return None; }
        let first = clip.source_in.checked_add(a.checked_sub(clip.timeline_start)?)?;
        let frames = b.checked_sub(a)?;
        if first.checked_add(frames)? > media.source_limit(rate)? { return None; }
        if a > cursor { segments.push(Segment { frames: a - cursor, source: None }); }
        let scale = (width as f32 / media.width as f32).min(height as f32 / media.height as f32);
        let w = (((media.width as f32 * scale).round() as u32).max(2) & !1).max(2);
        let h = (((media.height as f32 * scale).round() as u32).max(2) & !1).max(2);
        // A razor cut without a source discontinuity needs no second decoder.
        if let Some(previous) = segments.last_mut() {
            if previous.source.as_ref().is_some_and(|(path, start, pw, ph, sw, sh)|
                path == &media.path && *pw == w && *ph == h
                    && *sw == media.width && *sh == media.height
                    && start.checked_add(previous.frames) == Some(first)) {
                previous.frames = previous.frames.checked_add(frames)?;
                cursor = b;
                continue;
            }
        }
        inputs += 1;
        // Reserve one FFmpeg input for prepared PCM. Gaps use source filters.
        if inputs >= MAX_INPUTS { return None; }
        segments.push(Segment { frames, source: Some((media.path.clone(), first, w, h, media.width, media.height)) });
        cursor = b;
    }
    if cursor < end { segments.push(Segment { frames: end - cursor, source: None }); }
    Some(TimelinePlan { timeline_start: start, frame_count: end.checked_sub(start)?, rate, width, height, segments })
}

/// Caller must first validate the timeline's rate and safe source metadata.
/// A continuous razor-cut source can reuse the parent's GPU resident route.
pub(super) fn as_direct(plan: &TimelinePlan) -> Option<DirectExport> {
    let [segment] = plan.segments.as_slice() else { return None; };
    if segment.frames != plan.frame_count || plan.frame_count <= 0 { return None; }
    let (path, first_frame, fitted_width, fitted_height, source_width, source_height) = segment.source.as_ref()?;
    Some(DirectExport {
        path: path.clone(), timeline_start: plan.timeline_start,
        first_frame: *first_frame, frame_count: plan.frame_count,
        fitted_width: *fitted_width, fitted_height: *fitted_height,
        source_width: *source_width, source_height: *source_height,
        audio: DirectAudioRoute::Mixed, gpu_safe: true,
    })
}

/// Run before selecting this route so incompatible metadata can fall back safely.
pub(super) fn validate(ffmpeg: &Path, plan: &TimelinePlan, cancel: &AtomicBool) -> Result<bool, String> {
    let mut checked = HashSet::new();
    for segment in &plan.segments {
        if cancel.load(Ordering::Acquire) { return Err("Export cancelled".into()); }
        if let Some((path, _, _, _, source_width, source_height)) = &segment.source {
            if checked.insert((path, *source_width, *source_height)) {
                let (matches_rate, safe_metadata) =
                    source_matches_rate(path, ffmpeg, plan.rate, (*source_width, *source_height), cancel)?;
                // The shared safety flag also excludes rotation, non-square SAR,
                // HDR/full-range and unfamiliar pixel formats. Its conservative
                // codec restriction is intentional even though these are CPU filters.
                if !matches_rate || !safe_metadata {
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}

// Metadata is only a prerequisite: a complete packet scan proves the timestamp
// lattice before seeking. The route's rate/color checks still apply; incomplete metadata
// merely disables this optimization, not the frame-ordinal reference path.
fn seek_origin(ffmpeg: &Path, path: &Path, rate: FrameRate, cancel: &AtomicBool) -> Result<Option<f64>, String> {
    let probe = ffmpeg.with_file_name(if cfg!(windows) { "ffprobe.exe" } else { "ffprobe" });
    let mut command = Command::new(&probe);
    #[cfg(windows)]
    command.creation_flags(0x08004000);
    command.args(["-v", "error", "-select_streams", "V:0", "-show_entries",
        "stream=start_pts,time_base,duration_ts,nb_frames,avg_frame_rate,r_frame_rate", "-of", "json"])
        .arg(path).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let Ok(child) = command.spawn() else { return Ok(None); };
    let mut child = EncoderProcess { child, finished: false };
    let stdout = child.child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut stdout) = stdout { let _ = std::io::Read::read_to_string(&mut stdout, &mut text); }
        text
    });
    let status = wait_for_encoder(&mut child, cancel, Some(Duration::from_secs(8)), None);
    if status.is_err() { child.stop(); }
    let text = reader.join().unwrap_or_default();
    if cancel.load(Ordering::Acquire) { return Err("Export cancelled".into()); }
    if !status.is_ok_and(|s| s.success()) { return Ok(None); }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else { return Ok(None); };
    let stream = &value["streams"][0];
    let Some(origin) = timestamp_origin(stream, rate) else { return Ok(None); };
    let count = stream["nb_frames"].as_str().and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
    // Bound memory and work. Very large files or slow probes simply retain the
    // reference path. H.264/HEVC (required by validate) carry one picture/packet.
    if count == 0 || count > 2_000_000 { return Ok(None); }
    let start = stream["start_pts"].as_i64().unwrap();
    let ticks = stream["duration_ts"].as_i64().unwrap() / count as i64;
    let mut command = Command::new(&probe);
    #[cfg(windows)]
    command.creation_flags(0x08004000);
    command.args(["-v", "error", "-select_streams", "V:0", "-show_packets",
        "-show_entries", "packet=pts,duration,flags", "-of", "compact=p=0:nk=0"])
        .arg(path).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let Ok(child) = command.spawn() else { return Ok(None); };
    let mut child = EncoderProcess { child, finished: false };
    let stdout = child.child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || packet_lattice(BufReader::new(stdout), start, ticks, count));
    let status = wait_for_encoder(&mut child, cancel, Some(Duration::from_secs(8)), None);
    if status.is_err() { child.stop(); }
    let complete = reader.join().unwrap_or(false);
    if cancel.load(Ordering::Acquire) { return Err("Export cancelled".into()); }
    Ok((complete && status.is_ok_and(|s| s.success())).then_some(origin))
}

// Packet order may differ from presentation order with B frames. Require a
// permutation of the entire frame lattice, not just endpoints or mean rates.
fn packet_lattice(reader: impl BufRead, start: i64, ticks: i64, count: usize) -> bool {
    let mut seen = vec![false; count];
    let mut found = 0;
    for line in reader.lines() {
        let Ok(line) = line else { return false; };
        if found >= count { return false; }
        let mut pts = None;
        let mut duration = None;
        let mut flags = None;
        for field in line.split('|') {
            if let Some(v) = field.strip_prefix("pts=") { pts = v.parse::<i64>().ok(); }
            if let Some(v) = field.strip_prefix("duration=") { duration = v.parse::<i64>().ok(); }
            if let Some(v) = field.strip_prefix("flags=") { flags = Some(v); }
        }
        if duration != Some(ticks) || flags.is_none_or(|v| v.contains('D') || v.contains('C')) { return false; }
        let Some(delta) = pts.and_then(|p| p.checked_sub(start)) else { return false; };
        if delta < 0 || ticks <= 0 || delta % ticks != 0 { return false; }
        let Ok(index) = usize::try_from(delta / ticks) else { return false; };
        if index >= count || seen[index] { return false; }
        seen[index] = true;
        found += 1;
    }
    found == count
}

fn timestamp_origin(stream: &serde_json::Value, rate: FrameRate) -> Option<f64> {
    fn ratio(value: &serde_json::Value) -> Option<(i128, i128)> {
        let (n, d) = value.as_str()?.split_once('/')?;
        let (n, d) = (n.parse::<i64>().ok()? as i128, d.parse::<i64>().ok()? as i128);
        (n > 0 && d > 0).then_some((n, d))
    }
    for field in ["avg_frame_rate", "r_frame_rate"] {
        let (n, d) = ratio(&stream[field])?;
        if n * rate.den as i128 != d * rate.num as i128 { return None; }
    }
    let (n, d) = ratio(&stream["time_base"])?;
    let numerator = d * rate.den as i128;
    let denominator = n * rate.num as i128;
    if denominator == 0 || numerator % denominator != 0 { return None; }
    let ticks = numerator / denominator;
    let count = stream["nb_frames"].as_str()?.parse::<i64>().ok()?;
    let duration = stream["duration_ts"].as_i64()?;
    let start = stream["start_pts"].as_i64()?;
    if ticks <= 0 || count <= 0 || duration as i128 != count as i128 * ticks { return None; }
    let origin = start as f64 * n as f64 / d as f64;
    // Keep microsecond seek rounding comfortably below half a frame. Negative
    // origins and extreme timestamps retain the conservative ordinal path.
    (origin.is_finite() && origin >= 0.0 && origin < 1e9 && rate.fps() <= 240.0).then_some(origin)
}

fn seek_window(first: i64, rate: FrameRate, origin: Option<f64>) -> (Option<f64>, i64) {
    let preroll = (rate.fps() * 2.0).ceil() as i64;
    if first <= preroll * 2 { return (None, first); }
    let Some(origin) = origin else { return (None, first); };
    let anchor = first - preroll;
    // Seek halfway between frames: FFmpeg's accurate input seek discards up to
    // anchor, without depending on rounding a fractional frame boundary to us.
    // It also decodes the preceding keyframe for B-frame/reference recovery.
    let seconds = origin + (anchor as f64 - 0.5) * rate.den as f64 / rate.num as f64;
    if !seconds.is_finite() || seconds >= 1e9 { return (None, first); }
    (Some(seconds), preroll)
}

fn video_graph(plan: &TimelinePlan, encoder: &VideoEncoder, first_frames: &[i64]) -> String {
    let mut filters = Vec::new();
    let mut input = 0;
    let rate = format!("{}/{}", plan.rate.num, plan.rate.den);
    for (index, segment) in plan.segments.iter().enumerate() {
        let head = if let Some((_, _, w, h, _, _)) = &segment.source {
            let first = first_frames[input];
            let head = format!("[{input}:V:0]trim=start_frame={first}:end_frame={},scale={w}:{h}:flags=bilinear,format=rgb24,pad={}:{}:(ow-iw)/2:(oh-ih)/2:color=black", first + segment.frames, plan.width, plan.height);
            input += 1;
            head
        } else {
            format!("color=c=black:s={}x{}:r={rate},trim=end_frame={},format=rgb24", plan.width, plan.height, segment.frames)
        };
        filters.push(format!("{head},setsar=1,settb=expr={}/{},setpts=N[s{index}]", plan.rate.den, plan.rate.num));
    }
    let labels: String = (0..plan.segments.len()).map(|i| format!("[s{i}]")).collect();
    // Rebuild timestamps from the concatenated frame ordinal: concat's microsecond
    // time base must not accumulate rounding error at NTSC cuts.
    filters.push(format!("{labels}concat=n={}:v=1:a=0,settb=expr={}/{},setpts=N,{}[v]", plan.segments.len(), plan.rate.den, plan.rate.num, encoder.software_frame_filter()));
    filters.join(";")
}

/// `settings.output` must be the parent's staged output. Audio starts at timeline_start.
pub(super) fn render(
    ffmpeg: &Path, plan: &TimelinePlan, settings: &ExportSettings,
    encoder: &VideoEncoder, audio: &TemporaryAudioFile, cancel: &AtomicBool,
    progress: Sender<ExportProgress>,
) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) { return Err("Export cancelled".into()); }
    if settings.frame_rate != plan.rate || settings.width.max(16) & !1 != plan.width
        || settings.height.max(16) & !1 != plan.height {
        return Err("Timeline plan no longer matches export settings".into());
    }
    let mut command = ffmpeg_command(ffmpeg);
    command.arg("-y");
    encoder.configure_device(&mut command);
    let mut inputs = 0;
    let mut first_frames = Vec::new();
    let mut origins = std::collections::HashMap::new();
    for segment in &plan.segments {
        if let Some((path, first, _, _, _, _)) = &segment.source {
            if cancel.load(Ordering::Acquire) { return Err("Export cancelled".into()); }
            let origin = if *first > (plan.rate.fps() * 2.0).ceil() as i64 * 2 {
                if !origins.contains_key(path) {
                    origins.insert(path.clone(), seek_origin(ffmpeg, path, plan.rate, cancel)?);
                }
                origins[path]
            } else { None };
            let (seek, residual) = seek_window(*first, plan.rate, origin);
            first_frames.push(residual);
            command.args(["-threads", "2"]);
            if let Some(seconds) = seek {
                // Absolute stream timestamps handle video/container start offsets.
                command.args(["-accurate_seek", "-seek_timestamp", "1", "-ss"])
                    .arg(format!("{seconds:.12}"));
            }
            command.arg("-i").arg(path);
            inputs += 1;
        }
    }
    command.args(["-f", "f32le", "-ar", "48000", "-ac", "2"]).arg("-i").arg(&audio.path);
    let duration = plan.rate.frame_to_seconds(plan.frame_count);
    let graph = format!("{};[{inputs}:a:0]apad=whole_dur={:.12},aformat=sample_fmts=fltp:sample_rates=48000:channel_layouts=stereo[a]", video_graph(plan, encoder, &first_frames), duration + 1024.0 / 48_000.0);
    command.args(["-filter_complex_threads", "2", "-filter_complex"]).arg(graph)
        .args(["-map", "[v]", "-map", "[a]", "-enc_time_base:v"])
        .arg(format!("{}/{}", plan.rate.den, plan.rate.num))
        // Passthrough prevents an early source EOF being hidden by duplicated frames.
        .args(crate::media_engine::passthrough_args(ffmpeg)?)
        .arg("-frames:v").arg(plan.frame_count.to_string());
    encoder.configure(&mut command, settings);
    encoder.configure_pixel_format(&mut command);
    command.args(["-c:a", "aac", "-b:a", "192k", "-ar", "48000", "-t"])
        .arg(format!("{duration:.12}"))
        .args(["-movflags", "+faststart", "-progress", "pipe:1", "-nostats"])
        .arg(&settings.output).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let label = format!("Timeline · CPU decode/filter + {} · mixed audio", encoder.label());
    send_progress(&progress, 0.0, label.clone());
    let mut child = EncoderProcess { child: command.spawn().map_err(|e| e.to_string())?, finished: false };
    let frames = Arc::new(AtomicU64::new(0));
    let reader = thread_progress(child.child.stdout.take(), progress, plan.frame_count, label, Some(frames.clone()));
    let errors = thread_stderr(child.child.stderr.take());
    let result = wait_for_encoder(&mut child, cancel, None, Some(&frames));
    if result.is_err() { child.stop(); }
    let _ = reader.join();
    let error = errors.join().unwrap_or_default();
    if !result?.success() { return Err(if error.trim().is_empty() { "FFmpeg timeline export failed".into() } else { error.trim().into() }); }
    let encoded = frames.load(Ordering::Relaxed);
    if encoded != plan.frame_count as u64 {
        return Err(format!("FFmpeg encoded {encoded} of {} planned frames; output was not published", plan.frame_count));
    }
    Ok(())
}
