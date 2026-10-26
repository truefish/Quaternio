use std::collections::{HashMap, HashSet};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use crate::project::{FrameRate, Project};
use crate::transitions::audio_gains;

#[derive(Clone, Debug)]
pub struct Pcm {
    pub sample_rate: u32,
    pub samples: Vec<f32>,
}

pub(crate) const PREVIEW_WINDOW_SAMPLES: i64 = 96_000;

pub(crate) struct PcmWindow {
    pub start_sample: i64,
    pub pcm: Pcm,
}

pub(crate) type WindowBuffers = HashMap<u64, Vec<Arc<PcmWindow>>>;

enum Buffer<'a> {
    #[cfg(test)]
    Whole(&'a Pcm),
    Rendered(&'a Pcm, i64),
    Windows(&'a [Arc<PcmWindow>]),
}

impl Buffer<'_> {
    fn sample_rate(&self) -> u32 {
        match self {
            #[cfg(test)]
            Self::Whole(pcm) => pcm.sample_rate,
            Self::Rendered(pcm, _) => pcm.sample_rate,
            Self::Windows(_) => 48_000,
        }
    }

    fn sample(&self, position: f64) -> (f32, f32) {
        match self {
            #[cfg(test)]
            Self::Whole(pcm) => sample_stereo(pcm, position),
            Self::Rendered(pcm, start) => sample_stereo(pcm, position - *start as f64),
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

struct AudioWork<'a> {
    cancel: &'a AtomicBool,
    frames: usize,
    samples: usize,
    windows: usize,
    depth: usize,
}
impl<'a> AudioWork<'a> {
    fn new(cancel: &'a AtomicBool) -> Self {
        Self {
            cancel,
            frames: 2_000_000,
            samples: 32_000_000,
            windows: 256,
            depth: 0,
        }
    }
    fn check(&self) -> Result<(), String> {
        if self.cancel.load(Ordering::Acquire) {
            Err("Audio processing cancelled".into())
        } else {
            Ok(())
        }
    }
    fn frames(&mut self, count: usize) -> Result<(), String> {
        self.check()?;
        self.frames = self
            .frames
            .checked_sub(count)
            .ok_or("Nested audio is too complex; simplify or render a nested sequence first")?;
        Ok(())
    }
    fn samples(&mut self, count: usize) -> Result<(), String> {
        self.check()?;
        self.samples = self.samples.checked_sub(count).ok_or("Nested audio exceeds the working memory limit; simplify or render a nested sequence first")?;
        Ok(())
    }
}
fn nested_interval(
    project: &Project,
    clip: &crate::project::Clip,
    first: i64,
    end: i64,
    child: &Project,
) -> (i64, i64) {
    let rate = project.sequence.frame_rate;
    let a = clip.source_time_seconds(first - clip.timeline_start, rate);
    let b = clip.source_time_seconds(end - clip.timeline_start, rate);
    let fps = child.sequence.frame_rate.fps();
    let limit = child.sequence.duration_frames();
    let start = ((a.min(b) * fps).floor() as i64).clamp(0, limit);
    let end = ((a.max(b) * fps).ceil() as i64)
        .saturating_add(1)
        .clamp(0, limit);
    (start, end.max(start))
}

/// Resolve only samples used by voices in this interval, including transition
/// handles. Distant future clips do not delay the first playback buffer.
fn required_audio_windows_impl(
    project: &Project,
    start: i64,
    count: i64,
    work: &mut AudioWork<'_>,
) -> Result<HashSet<(u64, i64)>, String> {
    let (start, count) = if project.sequence.audio.is_identity() {
        (start, count)
    } else {
        let before = start
            .saturating_sub(if project.sequence.audio.ducking {
                project.sequence.frame_rate.seconds_to_frame(0.4) + 2
            } else {
                1
            })
            .max(0);
        (
            before,
            start.saturating_add(count).saturating_add(1) - before,
        )
    };
    let mut needed = HashSet::new();
    let mut nested = HashMap::<u64, (i64, i64)>::new();
    let rate = project.sequence.frame_rate;
    for (index, track) in project.sequence.tracks.iter().enumerate() {
        if !project.sequence.audio_track_enabled(index) || track.gain <= 0.0 {
            continue;
        }
        work.frames(
            usize::try_from(count.max(0))
                .unwrap_or(usize::MAX)
                .saturating_mul(1 + track.clips.len() + project.sequence.transitions.len()),
        )?;
        for frame in start..start.saturating_add(count.max(0)) {
            work.check()?;
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
                if clip.creative.sequence.is_some() {
                    if !clip.creative.freeze {
                        nested
                            .entry(clip.id)
                            .and_modify(|range| {
                                range.0 = range.0.min(frame);
                                range.1 = range.1.max(frame + 1);
                            })
                            .or_insert((frame, frame + 1));
                    }
                    continue;
                }
                if !project
                    .media(clip.media_id)
                    .is_some_and(|media| media.has_audio)
                {
                    continue;
                }
                let first = if clip.creative.speed == 1. {
                    (begin as f64 / 48_000.0 + source_offset(clip, rate)) * 48_000.0
                } else {
                    (clip.source_start_seconds(rate)
                        + (begin as f64 / 48_000.0 - rate.frame_to_seconds(clip.timeline_start))
                            * clip.creative.speed)
                        * 48_000.0
                };
                let last = first + (end - begin - 1) as f64 * clip.creative.speed;
                let first_index = (first.min(last).round() as i64).max(0);
                // Container duration is not a PCM boundary: AAC may decode a
                // real tail beyond it. The decoder determines actual EOF.
                let last_index = first.max(last).round() as i64;
                if last_index < first_index {
                    continue;
                }
                let begin_block =
                    first_index.saturating_sub(if clip.creative.dialogue { 1024 } else { 0 })
                        / PREVIEW_WINDOW_SAMPLES;
                let end_block = last_index / PREVIEW_WINDOW_SAMPLES;
                let blocks = end_block
                    .saturating_sub(begin_block)
                    .saturating_add(1)
                    .try_into()
                    .unwrap_or(usize::MAX);
                if blocks > 256 {
                    return Err("Audio source window limit exceeded".into());
                }
                for block in begin_block..=end_block {
                    if needed.insert((clip.media_id, block)) {
                        work.windows = work
                            .windows
                            .checked_sub(1)
                            .ok_or("Audio source window limit exceeded")?;
                    }
                }
            }
        }
    }
    for (id, (first, end)) in nested {
        work.check()?;
        let clip = project.clip(id).ok_or("Nested audio clip is missing")?;
        let mut child = project.clone();
        child.switch_sequence(clip.creative.sequence.as_deref().unwrap())?;
        let (a, b) = nested_interval(project, clip, first, end, &child);
        if b > a {
            if work.depth >= 8 {
                return Err("Nested audio exceeds eight levels".into());
            }
            work.depth += 1;
            let result = required_audio_windows_impl(&child, a, b - a, work);
            work.depth -= 1;
            needed.extend(result?);
        }
    }
    Ok(needed)
}

pub(crate) fn required_audio_windows_checked(
    project: &Project,
    start: i64,
    count: i64,
    cancel: &AtomicBool,
) -> Result<HashSet<(u64, i64)>, String> {
    required_audio_windows_impl(project, start, count, &mut AudioWork::new(cancel))
}

pub(crate) fn mix_window_range_checked(
    project: &Project,
    start: i64,
    count: i64,
    buffers: &WindowBuffers,
    cancel: &AtomicBool,
) -> Result<Vec<f32>, String> {
    mix_range_impl(
        project,
        start,
        count,
        48_000,
        None,
        &|id| buffers.get(&id).map(|windows| Buffer::Windows(windows)),
        &mut AudioWork::new(cancel),
    )
}

fn mix_range_impl<'a>(
    project: &Project,
    start_frame: i64,
    frame_count: i64,
    sample_rate: u32,
    peaks: Option<&mut StereoPeak>,
    get_buffer: &dyn Fn(u64) -> Option<Buffer<'a>>,
    work: &mut AudioWork<'_>,
) -> Result<Vec<f32>, String> {
    if project.sequence.audio.is_identity() {
        return mix_raw(
            project,
            start_frame,
            frame_count,
            sample_rate,
            peaks,
            get_buffer,
            true,
            work,
        );
    }
    let rate = project.sequence.frame_rate;
    let before = start_frame.saturating_sub(1).max(0);
    let end = start_frame.saturating_add(frame_count);
    let raw = mix_raw(
        project,
        before,
        end.saturating_add(1) - before,
        sample_rate,
        None,
        get_buffer,
        false,
        work,
    )?;
    let offset = (sample_boundary(start_frame, rate, sample_rate)
        - sample_boundary(before, rate, sample_rate))
    .max(0) as usize;
    let count = (sample_boundary(end, rate, sample_rate)
        - sample_boundary(start_frame, rate, sample_rate))
    .max(0) as usize;
    work.samples(count.saturating_mul(2))?;
    let mut out = project
        .sequence
        .audio
        .process(&raw, offset, count, sample_rate);
    for v in &mut out {
        if !v.is_finite() {
            *v = 0.;
        }
    }
    if let Some(p) = peaks {
        for pair in out.chunks_exact_mut(2) {
            for (c, v) in pair.iter_mut().enumerate() {
                p.amplitude[c] = p.amplitude[c].max(v.abs());
                p.clipped[c] |= v.abs() > 1.;
                *v = v.clamp(-1., 1.);
            }
        }
    } else {
        for v in &mut out {
            *v = v.clamp(-1., 1.);
        }
    }
    Ok(out)
}

fn mix_raw<'a>(
    project: &Project,
    start_frame: i64,
    frame_count: i64,
    sample_rate: u32,
    peaks: Option<&mut StereoPeak>,
    get_buffer: &dyn Fn(u64) -> Option<Buffer<'a>>,
    finish: bool,
    work: &mut AudioWork<'_>,
) -> Result<Vec<f32>, String> {
    let rate = project.sequence.frame_rate;
    let end_frame = start_frame.saturating_add(frame_count.max(0));
    let start_sample = sample_boundary(start_frame, rate, sample_rate);
    let end_sample = sample_boundary(end_frame, rate, sample_rate);
    let Ok(total) = usize::try_from(end_sample.saturating_sub(start_sample)) else {
        return Err("Audio sample range exceeds limits".into());
    };
    let Some(sample_count) = total.checked_mul(2) else {
        return Err("Audio sample range exceeds limits".into());
    };
    work.samples(sample_count)?;
    let mut out = vec![0f32; sample_count];
    if total == 0 || sample_rate == 0 {
        return Ok(out);
    }
    // Mix each nested clip once per chunk, rather than once per parent frame.
    let mut nested_ranges = HashMap::<u64, (i64, i64)>::new();
    for (index, nested_track) in project
        .sequence
        .tracks
        .iter()
        .enumerate()
        .filter(|(i, _)| project.sequence.audio_track_enabled(*i))
    {
        if !nested_track
            .clips
            .iter()
            .any(|c| c.creative.sequence.is_some())
        {
            continue;
        }
        work.frames(
            usize::try_from(frame_count.max(0))
                .unwrap_or(usize::MAX)
                .saturating_mul(1 + nested_track.clips.len() + project.sequence.transitions.len()),
        )?;
        for frame in start_frame..end_frame {
            work.check()?;
            for gain in audio_gains(&project.sequence, index, frame) {
                if gain.gain <= 0. {
                    continue;
                }
                if let Some(clip) = project
                    .clip(gain.clip_id)
                    .filter(|c| c.creative.sequence.is_some() && !c.creative.freeze)
                {
                    nested_ranges
                        .entry(clip.id)
                        .and_modify(|r| {
                            r.0 = r.0.min(frame);
                            r.1 = r.1.max(frame + 1);
                        })
                        .or_insert((frame, frame + 1));
                }
            }
        }
    }
    let mut rendered = HashMap::new();
    for (id, (first, end)) in nested_ranges {
        work.check()?;
        let clip = project.clip(id).ok_or("Nested audio clip is missing")?;
        let mut child = project.clone();
        child.switch_sequence(clip.creative.sequence.as_deref().unwrap())?;
        let (a, b) = nested_interval(project, clip, first, end, &child);
        let samples = if b > a {
            if work.depth >= 8 {
                return Err("Nested audio exceeds eight levels".into());
            }
            work.depth += 1;
            let result = mix_range_impl(&child, a, b - a, sample_rate, None, get_buffer, work);
            work.depth -= 1;
            result?
        } else {
            Vec::new()
        };
        rendered.insert(
            id,
            (
                Pcm {
                    sample_rate,
                    samples,
                },
                sample_boundary(a, child.sequence.frame_rate, sample_rate),
            ),
        );
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
        work.frames(
            usize::try_from(frame_count.max(0))
                .unwrap_or(usize::MAX)
                .saturating_mul(1 + track.clips.len() + project.sequence.transitions.len()),
        )?;
        for frame in start_frame..end_frame {
            work.check()?;
            let gains = audio_gains(&project.sequence, track_index, frame);
            if gains.is_empty() {
                continue;
            }
            let frame_start = sample_boundary(frame, rate, sample_rate);
            let frame_end = sample_boundary(frame.saturating_add(1), rate, sample_rate);
            // Resolve clips and PCM once per frame, rather than searching the project
            // again for every output sample.
            let duck = if project.sequence.audio.ducking
                && track.role == crate::project::audio::AudioRole::Music
            {
                duck_gain(project, frame, sample_rate, get_buffer)
            } else {
                1.
            };
            let duck_previous = if project.sequence.audio.ducking
                && track.role == crate::project::audio::AudioRole::Music
            {
                duck_gain(project, frame - 1, sample_rate, get_buffer)
            } else {
                1.
            };
            let mut voices: Vec<_> = gains
                .iter()
                .filter_map(|gain| {
                    let clip = project.clip(gain.clip_id)?;
                    let buffer = if clip.creative.sequence.is_some() {
                        if clip.creative.freeze {
                            return None;
                        }
                        let (pcm, start) = rendered.get(&clip.id)?;
                        Buffer::Rendered(pcm, *start)
                    } else {
                        get_buffer(clip.media_id)?
                    };
                    let buffer_rate = buffer.sample_rate();
                    if buffer_rate == 0 {
                        return None;
                    }
                    // Keep the historical zero-remainder arithmetic unchanged.
                    // Native source marks can lie between sequence frames; use
                    // their exact source origin without rounding the PCM phase.
                    let source_time = if clip.creative.speed == 1. {
                        frame_start as f64 / sample_rate as f64 + source_offset(clip, rate)
                    } else {
                        clip.source_start_seconds(rate)
                            + (frame_start as f64 / sample_rate as f64
                                - rate.frame_to_seconds(clip.timeline_start))
                                * clip.creative.speed
                    };
                    let first = source_time * buffer_rate as f64;
                    let step = buffer_rate as f64 / sample_rate as f64 * clip.creative.speed;
                    let alpha = 1. - (-2. * std::f32::consts::PI * 80. / sample_rate as f32).exp();
                    let mut low = (0., 0.);
                    let mut power = 0.;
                    if clip.creative.dialogue {
                        for n in (1..=1024).rev() {
                            let (l, r) = buffer.sample(first - n as f64 * step);
                            low.0 += alpha * (l - low.0);
                            low.1 += alpha * (r - low.1);
                            power += ((l * l + r * r) * 0.5 - power) * 0.004;
                        }
                    }
                    Some((
                        buffer,
                        gain.gain,
                        source_time * buffer_rate as f64,
                        buffer_rate as f64 / sample_rate as f64 * clip.creative.speed,
                        clip.creative.dialogue,
                        low,
                        power,
                        alpha,
                    ))
                })
                .collect();
            for output_sample in frame_start..frame_end {
                if (output_sample - frame_start) % 1024 == 0 {
                    work.check()?;
                }
                let mut left = 0.0f32;
                let mut right = 0.0f32;
                let along = (output_sample - frame_start) as f64;
                for (buffer, clip_gain, first, step, dialogue, low, power, alpha) in &mut voices {
                    let position = *first + along * *step;
                    let (mut sl, mut sr) = buffer.sample(position);
                    if *dialogue {
                        low.0 += *alpha * (sl - low.0);
                        low.1 += *alpha * (sr - low.1);
                        *power += ((sl * sl + sr * sr) * 0.5 - *power) * 0.004;
                        let level = power.sqrt();
                        let gate = ((level - 0.002) / 0.008).clamp(0., 1.);
                        let gate = 0.15 + 0.85 * gate * gate * (3. - 2. * gate);
                        sl = (sl - low.0) * gate;
                        sr = (sr - low.1) * gate;
                    }
                    left += sl * *clip_gain;
                    right += sr * *clip_gain;
                }
                let dest = (output_sample - start_sample) as usize * 2;
                let envelope = duck_previous
                    + (duck - duck_previous)
                        * (along as f32 / (frame_end - frame_start).max(1) as f32);
                out[dest] += left * left_gain * envelope;
                out[dest + 1] += right * right_gain * envelope;
            }
        }
    }
    if !finish {
        return Ok(out);
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
    Ok(out)
}

fn duck_gain<'a>(
    project: &Project,
    frame: i64,
    sample_rate: u32,
    get: &dyn Fn(u64) -> Option<Buffer<'a>>,
) -> f32 {
    let rate = project.sequence.frame_rate;
    let hold = rate.seconds_to_frame(0.4).clamp(1, 400);
    let mut voice = 0f32;
    for f in frame.saturating_sub(hold).max(0)..=frame.max(0) {
        let mut power = 0.;
        for (i, t) in project.sequence.tracks.iter().enumerate().filter(|(i, t)| {
            t.role == crate::project::audio::AudioRole::Dialogue
                && project.sequence.audio_track_enabled(*i)
        }) {
            for gain in audio_gains(&project.sequence, i, f) {
                if let Some(c) = project.clip(gain.clip_id) {
                    if let Some(pcm) = get(c.media_id) {
                        for n in 0..8 {
                            let time = rate.frame_to_seconds(f - c.timeline_start)
                                + n as f64 / rate.fps() / 8.;
                            let position = (c.source_start_seconds(rate) + time * c.creative.speed)
                                * sample_rate as f64;
                            let (l, r) = pcm.sample(position);
                            power += (l * l + r * r) * gain.gain.powi(2) * t.gain.powi(2) / 16.;
                        }
                    }
                }
            }
        }
        let activity = ((power.sqrt() - 0.008) / 0.04).clamp(0., 1.);
        let release = 1. - (frame - f).max(0) as f32 / hold as f32;
        voice = voice.max(activity * release);
    }
    1. - 0.75 * voice
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct StereoPeak {
    pub amplitude: [f32; 2],
    pub clipped: [bool; 2],
}

pub(super) fn mix_window_range_metered(
    project: &Project,
    start: i64,
    count: i64,
    buffers: &WindowBuffers,
    cancel: &AtomicBool,
) -> Result<(Vec<f32>, StereoPeak), String> {
    let mut peaks = StereoPeak::default();
    let samples = mix_range_impl(
        project,
        start,
        count,
        48_000,
        Some(&mut peaks),
        &|id| buffers.get(&id).map(|windows| Buffer::Windows(windows)),
        &mut AudioWork::new(cancel),
    )?;
    Ok((samples, peaks))
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
