//! Exact export PCM stays on disk. Only the current mix chunk's source windows
//! are resident; decoding starts at the original stream origin, including AAC
//! decoder history, and stops once the last required source window is complete.
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};

use super::*;
use crate::playback::{
    mix_window_range_checked, required_audio_windows_checked, Pcm, PcmWindow, WindowBuffers, PREVIEW_WINDOW_SAMPLES };

const FRAME_BYTES: u64 = 8; // Two little-endian f32 samples.
const BLOCK_BYTES: u64 = PREVIEW_WINDOW_SAMPLES as u64 * FRAME_BYTES;
const COPY_BYTES: usize = 64 * 1024;

fn preparation(
    progress: &Sender<ExportProgress>,
    phase: &'static str,
    fraction: f32,
    message: String,
) {
    let _ = progress.send(ExportProgress {
        fraction: 0.0,
        preparation: Some(ExportPreparation {
            phase,
            fraction: fraction.clamp(0.0, 1.0) }),
        message,
        remaining_seconds: None,
        done: false,
        error: None });
}

struct DiskAudio {
    // Close the handle before TemporaryAudioFile removes its path on Windows.
    file: File,
    _temporary: TemporaryAudioFile,
    blocks: Vec<i64>,
    bytes: u64 }

impl DiskAudio {
    fn create(blocks: Vec<i64>) -> Result<Self, String> {
        let (temporary, file) = TemporaryAudioFile::create()?;
        Ok(Self {
            file,
            _temporary: temporary,
            blocks,
            bytes: 0 })
    }

    fn window(&mut self, block: i64) -> Result<Arc<PcmWindow>, String> {
        let index = self
            .blocks
            .binary_search(&block)
            .map_err(|_| "Audio mixer requested an unplanned source window")?;
        let offset = index as u64 * BLOCK_BYTES;
        let count = self.bytes.saturating_sub(offset).min(BLOCK_BYTES) as usize;
        let mut bytes = vec![0u8; count];
        self.file
            .seek(SeekFrom::Start(offset))
            .and_then(|_| self.file.read_exact(&mut bytes))
            .map_err(|error| format!("Could not read temporary audio: {error}"))?;
        let samples = bytes
            .chunks_exact(4)
            .map(|bytes| f32::from_le_bytes(bytes.try_into().unwrap()))
            .collect();
        Ok(Arc::new(PcmWindow {
            start_sample: block * PREVIEW_WINDOW_SAMPLES,
            pcm: Pcm {
                sample_rate: 48_000,
                samples } }))
    }
}

// Keep only this chunk's windows, reusing immutable PCM from the previous chunk.
// Evict first so retained PCM stays bounded by the current required windows.
fn retain_windows(windows: &mut WindowBuffers, needed: &HashSet<(u64, i64)>) {
    windows.retain(|id, buffers| {
        buffers.retain(|window| {
            needed.contains(&(*id, window.start_sample / PREVIEW_WINDOW_SAMPLES))
        });
        !buffers.is_empty()
    });
}

/// Copy only selected source blocks into a compact file. Discarding a prefix
/// happens after decoding, never through an input seek or a changed filter graph.
fn copy_blocks(
    input: &mut impl Read,
    output: &mut impl Write,
    blocks: &[i64],
    decoded: &AtomicU64,
) -> Result<(u64, u64), String> {
    let last = blocks
        .last()
        .copied()
        .ok_or("No source audio windows were planned")?;
    let limit = u64::try_from(last)
        .ok()
        .and_then(|block| block.checked_add(1))
        .and_then(|blocks| blocks.checked_mul(BLOCK_BYTES))
        .ok_or("The source audio sample range is invalid")?;
    let mut buffer = [0u8; COPY_BYTES];
    let mut position = 0u64;
    let mut written = 0u64;
    let mut selected = 0usize;
    while position < limit {
        let wanted = (limit - position).min(buffer.len() as u64) as usize;
        let count = input
            .read(&mut buffer[..wanted])
            .map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        let end = position + count as u64;
        while selected < blocks.len() {
            let begin_block = blocks[selected] as u64 * BLOCK_BYTES;
            let end_block = begin_block + BLOCK_BYTES;
            if begin_block >= end {
                break;
            }
            let begin = position.max(begin_block);
            let stop = end.min(end_block);
            if stop > begin {
                output
                    .write_all(&buffer[(begin - position) as usize..(stop - position) as usize])
                    .map_err(|error| format!("Could not write temporary audio: {error}"))?;
                written += stop - begin;
            }
            if end_block <= end {
                selected += 1;
            } else {
                break;
            }
        }
        position = end;
        decoded.store(position, Ordering::Release);
    }
    output.flush().map_err(|error| error.to_string())?;
    if position == 0 || position % FRAME_BYTES != 0 || written % FRAME_BYTES != 0 {
        return Err("FFmpeg returned incomplete stereo PCM".into());
    }
    Ok((position, written))
}

fn decode(
    path: &Path,
    blocks: Vec<i64>,
    cancel: &AtomicBool,
    update: impl FnMut(u64, u64, bool),
) -> Result<DiskAudio, String> {
    if cancel.load(Ordering::Acquire) {
        return Err("Export cancelled".into());
    }
    decode_into(path, DiskAudio::create(blocks)?, cancel, update)
}

fn decode_into(
    path: &Path,
    mut spool: DiskAudio,
    cancel: &AtomicBool,
    mut update: impl FnMut(u64, u64, bool),
) -> Result<DiskAudio, String> {
    if cancel.load(Ordering::Acquire) {
        return Err("Export cancelled".into());
    }
    let limit = spool
        .blocks
        .last()
        .copied()
        .and_then(|last| u64::try_from(last).ok())
        .and_then(|last| last.checked_add(1))
        .and_then(|blocks| blocks.checked_mul(BLOCK_BYTES))
        .ok_or("The source audio sample range is invalid")?;
    // Match decode_audio_inner exactly: no seek, trim filter, timestamp reset,
    // different resampler, or input codec options can alter original PCM.
    let ffmpeg = find_ffmpeg()?;
    let child = ffmpeg_command(&ffmpeg)
        .arg("-i")
        .arg(path)
        .args(["-vn", "-ac", "2", "-ar", "48000", "-f", "f32le", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let mut child = EncoderProcess {
        child,
        finished: false };
    let mut stdout = child
        .child
        .stdout
        .take()
        .ok_or("Missing audio decoder output")?;
    let errors = thread_stderr(child.child.stderr.take());
    let decoded = AtomicU64::new(0);
    let reached = AtomicBool::new(false);
    let failed = AtomicBool::new(false);
    update(0, limit, false);
    let outcome: Result<(Option<ExitStatus>, (u64, u64)), String> = std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let result = copy_blocks(&mut stdout, &mut spool.file, &spool.blocks, &decoded);
            if let Ok((count, _)) = &result {
                reached.store(*count == limit, Ordering::Release);
            } else {
                failed.store(true, Ordering::Release);
            }
            result
        });
        let mut last_update = Instant::now();
        let status = loop {
            if cancel.load(Ordering::Acquire) {
                child.stop();
                break Err("Export cancelled".to_string());
            }
            match child.child.try_wait() {
                Ok(Some(status)) => {
                    child.finished = true;
                    break Ok(Some(status));
                }
                Err(error) => {
                    child.stop();
                    break Err(error.to_string());
                }
                Ok(None) => {}
            }
            if failed.load(Ordering::Acquire) {
                child.stop();
                break Ok(None);
            }
            if reached.load(Ordering::Acquire) {
                // This is successful bounded completion, distinct from user
                // cancellation. Reap FFmpeg before deleting any spool on error.
                child.stop();
                break Ok(None);
            }
            if last_update.elapsed() >= Duration::from_millis(200) {
                update(decoded.load(Ordering::Acquire), limit, false);
                last_update = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let copied = reader
            .join()
            .map_err(|_| "Audio spool reader stopped unexpectedly")?;
        let status = status?;
        let copied = copied?;
        if cancel.load(Ordering::Acquire) {
            return Err("Export cancelled".into());
        }
        Ok((status, copied))
    });
    let detail = errors.join().unwrap_or_default();
    let (status, (count, bytes)) = outcome?;
    if status.is_some_and(|status| !status.success()) {
        return Err(if detail.trim().is_empty() {
            "FFmpeg failed to decode audio".into()
        } else {
            detail.trim().to_owned()
        });
    }
    update(count, limit, true);
    if cancel.load(Ordering::Acquire) {
        return Err("Export cancelled".into());
    }
    spool.bytes = bytes;
    Ok(spool)
}

pub(super) fn prepare(
    project: &Project,
    start: i64,
    end: i64,
    cancel: &AtomicBool,
    progress: &Sender<ExportProgress>,
) -> Result<TemporaryAudioFile, String> {
    if end <= start {
        return Err("The audio export range is empty".into());
    }
    for id in export_audio_media(project, start, end) {
        if project.media(id).is_none() {
            return Err("An audio clip references missing project media".into());
        }
    }
    let rate = project.sequence.frame_rate;
    let chunk_frames = rate.seconds_to_frame(0.5).max(1);
    let mut planned: HashMap<u64, BTreeSet<i64>> = HashMap::new();
    let mut frame = start;
    let mut last_update = Instant::now();
    preparation(
        progress,
        "Analyzing audio",
        0.0,
        "Analyzing selected audio range".into(),
    );
    while frame < end {
        if cancel.load(Ordering::Acquire) {
            return Err("Export cancelled".into());
        }
        let next = frame.saturating_add(chunk_frames).min(end);
        for (id, block) in required_audio_windows_checked(project, frame, next - frame, cancel)? {
            planned.entry(id).or_default().insert(block);
        }
        frame = next;
        if last_update.elapsed() >= Duration::from_millis(200) || frame == end {
            let fraction = (frame - start) as f32 / (end - start) as f32;
            preparation(
                progress,
                "Analyzing audio",
                fraction,
                format!("Analyzing selected audio · {:.0}%", fraction * 100.0),
            );
            last_update = Instant::now();
        }
    }
    let total = planned.len();
    let mut spools = HashMap::new();
    for media in &project.media {
        let Some(blocks) = planned.remove(&media.id) else {
            continue;
        };
        let index = spools.len();
        let audio = decode(&media.path, blocks.into_iter().collect(), cancel, |done, count, finished| {
            let portion = if finished { 1.0 } else { done as f64 / count as f64 };
            let fraction = (index as f64 + portion) / total.max(1) as f64;
            preparation(progress, "Decoding audio", fraction as f32,
                if finished { format!("Prepared audio {}/{} · {}", index + 1, total, media.name) }
                else { format!("Preparing audio {}/{} · {} · {:.1}/{:.1}s", index + 1, total,
                    media.name, done as f64 / 384_000.0, count as f64 / 384_000.0) });
        }).map_err(|detail| if cancel.load(Ordering::Acquire) { "Export cancelled".into() } else {
            format!("Could not decode audio for {}. Check or relink the media before exporting: {detail}", media.name)
        })?;
        spools.insert(media.id, audio);
    }
    if !planned.is_empty() {
        return Err("An audio clip references missing project media".into());
    }
    let (temporary, mut file) = TemporaryAudioFile::create()?;
    preparation(
        progress,
        "Mixing audio",
        0.0,
        "Mixing timeline audio".into(),
    );
    frame = start;
    last_update = Instant::now();
    let mut windows = WindowBuffers::new();
    while frame < end {
        if cancel.load(Ordering::Acquire) {
            return Err("Export cancelled".into());
        }
        let next = frame.saturating_add(chunk_frames).min(end);
        let needed = required_audio_windows_checked(project, frame, next - frame, cancel)?;
        retain_windows(&mut windows, &needed);
        for (id, block) in needed {
            if cancel.load(Ordering::Acquire) {
                return Err("Export cancelled".into());
            }
            if windows.get(&id).is_some_and(|buffers| {
                buffers.iter().any(|window| {
                    window.start_sample == block * PREVIEW_WINDOW_SAMPLES
                })
            }) {
                continue;
            }
            let spool = spools.get_mut(&id).ok_or("Audio source was not prepared")?;
            windows.entry(id).or_default().push(spool.window(block)?);
        }
        let samples = mix_window_range_checked(project, frame, next - frame, &windows, cancel)?;
        let expected = rounded_sample(next, rate)
            .and_then(|last| last.checked_sub(rounded_sample(frame, rate)?))
            .and_then(|frames| usize::try_from(frames).ok())
            .and_then(|frames| frames.checked_mul(2))
            .ok_or("The sequence frame rate or audio sample range is invalid")?;
        if samples.len() != expected {
            return Err(format!(
                "Audio mixer returned {} samples; expected {expected}",
                samples.len()
            ));
        }
        write_f32(&mut file, &samples)?;
        frame = next;
        if last_update.elapsed() >= Duration::from_millis(200) || frame == end {
            let fraction = (frame - start) as f32 / (end - start) as f32;
            preparation(
                progress,
                "Mixing audio",
                fraction,
                format!("Mixing timeline audio · {:.0}%", fraction * 100.0),
            );
            last_update = Instant::now();
        }
    }
    drop(file);
    if cancel.load(Ordering::Acquire) {
        return Err("Export cancelled".into());
    }
    Ok(temporary)
}

