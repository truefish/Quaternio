use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::project::{FrameRate, Project, TrackKind};
use crate::transitions::audio_gains;

#[derive(Clone, Debug)]
pub struct Pcm {
    pub sample_rate: u32,
    pub samples: Vec<f32> }

pub(crate) const PREVIEW_WINDOW_SAMPLES: i64 = 96_000;

pub(crate) struct PcmWindow {
    pub start_sample: i64,
    pub pcm: Pcm }

pub(crate) type WindowBuffers = HashMap<u64, Vec<Arc<PcmWindow>>>;

enum Buffer<'a> {
    Whole(&'a Pcm),
    Windows(&'a [Arc<PcmWindow>]) }

impl Buffer<'_> {
    fn sample_rate(&self) -> u32 {
        match self {
            Self::Whole(pcm) => pcm.sample_rate,
            Self::Windows(_) => 48_000 }
    }

    fn sample(&self, position: f64) -> (f32, f32) {
        match self {
            Self::Whole(pcm) => sample_stereo(pcm, position),
            Self::Windows(windows) => {
                // Round the absolute source position before subtracting the
                // integer window origin, preserving the export mixer's phase.
                let index = position.round() as i64;
                windows
                    .iter()
                    .find(|window| {
                        index >= window.start_sample
                            && index < window.start_sample + PREVIEW_WINDOW_SAMPLES
                    })
                    .map_or((0.0, 0.0), |window| {
                        sample_stereo(&window.pcm, (index - window.start_sample) as f64)
                    })
            }
        }
    }
}

fn source_offset(clip: &crate::project::Clip, rate: FrameRate) -> f64 {
    if clip.source_time_offset == 0.0 {
        (clip.source_in as f64 - clip.timeline_start as f64) / rate.fps().max(1.0)
    } else {
        clip.source_start_seconds(rate) - rate.frame_to_seconds(clip.timeline_start)
    }
}

/// Resolve only samples used by voices in this interval, including transition
/// handles. Distant future clips do not delay the first playback buffer.
pub(crate) fn required_audio_windows(
    project: &Project,
    start: i64,
    count: i64,
) -> HashSet<(u64, i64)> {
    let mut needed = HashSet::new();
    let rate = project.sequence.frame_rate;
    for (index, track) in project.sequence.tracks.iter().enumerate() {
        if !project.sequence.audio_track_enabled(index) || track.gain <= 0.0 {
            continue;
        }
        for frame in start..start.saturating_add(count.max(0)) {
            let begin = sample_boundary(frame, rate, 48_000);
            let end = sample_boundary(frame.saturating_add(1), rate, 48_000);
            if end <= begin {
                continue;
            }
            for gain in audio_gains(&project.sequence, index, frame) {
                if gain.gain <= 0.0 {
                    continue;
                }
                let Some(clip) = project.clip(gain.clip_id) else {
                    continue;
                };
                if !project
                    .media(clip.media_id)
                    .is_some_and(|media| media.has_audio)
                {
                    continue;
                }
                let first = (begin as f64 / 48_000.0 + source_offset(clip, rate)) * 48_000.0;
                let first_index = (first.round() as i64).max(0);
                // Container duration is not a PCM boundary: AAC may decode a
                // real tail beyond it. The decoder determines actual EOF.
                let last_index = (first + (end - begin - 1) as f64).round() as i64;
                if last_index < first_index {
                    continue;
                }
                for block in
                    first_index / PREVIEW_WINDOW_SAMPLES..=last_index / PREVIEW_WINDOW_SAMPLES
                {
                    needed.insert((clip.media_id, block));
                }
            }
        }
    }
    needed
}

pub(crate) fn mix_window_range(
    project: &Project,
    start: i64,
    count: i64,
    buffers: &WindowBuffers,
) -> Vec<f32> {
    mix_range_impl(project, start, count, 48_000, None, |id| {
        buffers.get(&id).map(|windows| Buffer::Windows(windows))
    })
}

pub fn mix_range(
    project: &Project,
    start_frame: i64,
    frame_count: i64,
    sample_rate: u32,
    buffers: &HashMap<u64, Pcm>,
) -> Vec<f32> {
    mix_range_impl(project, start_frame, frame_count, sample_rate, None, |id| {
        buffers.get(&id).map(Buffer::Whole)
    })
}

fn mix_range_impl<'a>(
    project: &Project,
    start_frame: i64,
    frame_count: i64,
    sample_rate: u32,
    peaks: Option<&mut StereoPeak>,
    get_buffer: impl Fn(u64) -> Option<Buffer<'a>>,
) -> Vec<f32> {
    let rate = project.sequence.frame_rate;
    let end_frame = start_frame.saturating_add(frame_count.max(0));
    let start_sample = sample_boundary(start_frame, rate, sample_rate);
    let end_sample = sample_boundary(end_frame, rate, sample_rate);
    let Ok(total) = usize::try_from(end_sample.saturating_sub(start_sample)) else {
        return Vec::new();
    };
    let Some(sample_count) = total.checked_mul(2) else {
        return Vec::new();
    };
    let mut out = vec![0f32; sample_count];
    if total == 0 || sample_rate == 0 {
        return out;
    }
    for (track_index, track) in project.sequence.tracks.iter().enumerate() {
        if !project.sequence.audio_track_enabled(track_index) {
            continue;
        }
        let gain = if track.gain.is_finite() {
            track.gain.max(0.0)
        } else {
            1.0
        };
        let pan = if track.pan.is_finite() {
            track.pan.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        let left_gain = gain * (1.0 - pan.max(0.0));
        let right_gain = gain * (1.0 + pan.min(0.0));
        if gain == 0.0 {
            continue;
        }
        for frame in start_frame..end_frame {
            let gains = audio_gains(&project.sequence, track_index, frame);
            if gains.is_empty() {
                continue;
            }
            let frame_start = sample_boundary(frame, rate, sample_rate);
            let frame_end = sample_boundary(frame.saturating_add(1), rate, sample_rate);
            // Resolve clips and PCM once per frame, rather than searching the project
            // again for every output sample.
            let voices: Vec<_> = gains
                .iter()
                .filter_map(|gain| {
                    let clip = project.clip(gain.clip_id)?;
                    let buffer = get_buffer(clip.media_id)?;
                    let buffer_rate = buffer.sample_rate();
                    if buffer_rate == 0 {
                        return None;
                    }
                    // Keep the historical zero-remainder arithmetic unchanged.
                    // Native source marks can lie between sequence frames; use
                    // their exact source origin without rounding the PCM phase.
                    let source_time =
                        frame_start as f64 / sample_rate as f64 + source_offset(clip, rate);
                    Some((
                        buffer,
                        gain.gain,
                        source_time * buffer_rate as f64,
                        buffer_rate as f64 / sample_rate as f64,
                    ))
                })
                .collect();
            for output_sample in frame_start..frame_end {
                let mut left = 0.0f32;
                let mut right = 0.0f32;
                let along = (output_sample - frame_start) as f64;
                for (buffer, clip_gain, first, step) in &voices {
                    let (sl, sr) = buffer.sample(first + along * step);
                    left += sl * clip_gain;
                    right += sr * clip_gain;
                }
                let dest = (output_sample - start_sample) as usize * 2;
                out[dest] += left * left_gain;
                out[dest + 1] += right * right_gain;
            }
        }
    }
    if let Some(peaks) = peaks {
        for samples in out.chunks_exact_mut(2) {
            for (channel, sample) in samples.iter_mut().enumerate() {
                let amplitude = sample.abs();
                if amplitude.is_finite() {
                    peaks.amplitude[channel] = peaks.amplitude[channel].max(amplitude);
                    peaks.clipped[channel] |= amplitude > 1.0;
                }
                *sample = sample.clamp(-1.0, 1.0);
            }
        }
    } else {
        for sample in &mut out {
            *sample = sample.clamp(-1.0, 1.0);
        }
    }
    out
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct StereoPeak {
    pub amplitude: [f32; 2],
    pub clipped: [bool; 2] }

pub(super) fn mix_window_range_metered(
    project: &Project, start: i64, count: i64, buffers: &WindowBuffers,
) -> (Vec<f32>, StereoPeak) {
    let mut peaks = StereoPeak::default();
    let samples = mix_range_impl(project, start, count, 48_000, Some(&mut peaks), |id| {
        buffers.get(&id).map(|windows| Buffer::Windows(windows))
    });
    (samples, peaks)
}

fn sample_boundary(frame: i64, rate: FrameRate, sample_rate: u32) -> i64 {
    // Use the rational sequence rate to make adjacent chunk boundaries identical.
    // Independently rounding samples per frame adds drift at 29.97 and 59.94 fps.
    let (numerator, denominator) = if rate.den > 0 && rate.num >= rate.den {
        (
            frame as i128 * sample_rate as i128 * rate.den as i128,
            rate.num as i128,
        )
    } else {
        (frame as i128 * sample_rate as i128, 1)
    };
    let rounded = if numerator >= 0 {
        (numerator + denominator / 2) / denominator
    } else {
        -((-numerator + denominator / 2) / denominator)
    };
    rounded.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn sample_stereo(buffer: &Pcm, source_position: f64) -> (f32, f32) {
    let index = source_position.round() as i64;
    if index < 0 {
        return (0.0, 0.0);
    }
    let Some(offset) = (index as usize).checked_mul(2) else {
        return (0.0, 0.0);
    };
    if offset + 1 >= buffer.samples.len() {
        return (0.0, 0.0);
    }
    (buffer.samples[offset], buffer.samples[offset + 1])
}

