use crate::project::{Anchor, Clip, Sequence, Track, Transition, TransitionKind};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VideoWeight {
    pub outgoing: f32,
    pub incoming: f32,
    pub black: f32 }

impl VideoWeight {
    pub fn sum(self) -> f32 {
        self.outgoing + self.incoming + self.black
    }
}

pub fn video_weight(kind: TransitionKind, progress: f32) -> VideoWeight {
    let p = progress.clamp(0.0, 1.0);
    match kind {
        TransitionKind::CrossDissolve | TransitionKind::AudioCrossfade => VideoWeight {
            outgoing: 1.0 - p,
            incoming: p,
            black: 0.0 },
        TransitionKind::DipToBlack => {
            if p < 0.5 {
                let out = 1.0 - p * 2.0;
                VideoWeight {
                    outgoing: out,
                    incoming: 0.0,
                    black: 1.0 - out }
            } else {
                let incoming = (p - 0.5) * 2.0;
                VideoWeight {
                    outgoing: 0.0,
                    incoming,
                    black: 1.0 - incoming }
            }
        }
        TransitionKind::FadeIn | TransitionKind::AudioFadeIn => VideoWeight {
            outgoing: 0.0,
            incoming: p,
            black: 1.0 - p },
        TransitionKind::FadeOut | TransitionKind::AudioFadeOut => VideoWeight {
            outgoing: 1.0 - p,
            incoming: 0.0,
            black: p } }
}

pub fn audio_gains_for_kind(kind: TransitionKind, progress: f32) -> (f32, f32) {
    let p = progress.clamp(0.0, 1.0);
    match kind {
        TransitionKind::AudioCrossfade | TransitionKind::CrossDissolve => {
            let angle = p * std::f32::consts::FRAC_PI_2;
            (angle.cos(), angle.sin())
        }
        TransitionKind::AudioFadeIn | TransitionKind::FadeIn => (0.0, p),
        TransitionKind::AudioFadeOut | TransitionKind::FadeOut => (1.0 - p, 0.0),
        TransitionKind::DipToBlack => {
            if p < 0.5 {
                (1.0 - p * 2.0, 0.0)
            } else {
                (0.0, (p - 0.5) * 2.0)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layer {
    pub clip_id: Option<u64>,
    pub weight: f32 }

pub fn clip_by_id<'a>(track: &'a Track, id: u64) -> Option<&'a Clip> {
    track.clips.iter().find(|clip| clip.id == id)
}

fn sequence_clip<'a>(sequence: &'a Sequence, id: u64) -> Option<&'a Clip> {
    sequence
        .tracks
        .iter()
        .flat_map(|track| track.clips.iter())
        .find(|clip| clip.id == id)
}

pub fn transition_window(sequence: &Sequence, transition: &Transition) -> Option<(i64, i64)> {
    let clip = sequence_clip(sequence, transition.anchor.clip_id())?;
    let duration = transition.duration.max(1);
    let (start, end) = match transition.anchor {
        Anchor::Head(_) => (clip.timeline_start, clip.timeline_start + duration),
        Anchor::Tail(_) => (clip.end() - duration, clip.end()),
        Anchor::Cut(_) => {
            let half = duration / 2;
            (clip.end() - half, clip.end() - half + duration)
        }
    };
    Some((start, end.max(start + 1)))
}

fn progress_at(frame: i64, start: i64, duration: i64) -> f32 {
    if duration <= 1 {
        0.0
    } else {
        (frame - start) as f32 / duration as f32
    }
}

fn next_clip<'a>(track: &'a Track, outgoing: &Clip) -> Option<&'a Clip> {
    track
        .clips
        .iter()
        .filter(|clip| clip.id != outgoing.id && clip.timeline_start >= outgoing.end() - 1)
        .min_by_key(|clip| (clip.timeline_start, clip.id))
}

pub fn video_layers(sequence: &Sequence, track_index: usize, frame: i64) -> Vec<Layer> {
    let Some(track) = sequence.tracks.get(track_index) else {
        return Vec::new();
    };
    if track.muted {
        return Vec::new();
    }
    let mut claimed = Vec::new();
    let mut layers = Vec::new();

    for transition in &sequence.transitions {
        if transition.kind.is_audio() {
            continue;
        }
        let (Anchor::Head(id) | Anchor::Tail(id) | Anchor::Cut(id)) = transition.anchor;
        if track.clips.iter().all(|clip| clip.id != id) {
            continue;
        }
        let Some((start, end)) = transition_window(sequence, transition) else {
            continue;
        };
        if frame < start || frame >= end {
            continue;
        }
        let p = progress_at(frame, start, transition.duration.max(1));
        let weight = video_weight(transition.kind, p);
        match transition.anchor {
            Anchor::Head(id) => {
                claimed.push(id);
                let opacity = clip_by_id(track, id)
                    .map(|clip| clip.opacity)
                    .unwrap_or(1.0);
                push_weighted(&mut layers, Some(id), weight.incoming * opacity);
                push_weighted(&mut layers, None, weight.black);
            }
            Anchor::Tail(id) => {
                claimed.push(id);
                let opacity = clip_by_id(track, id)
                    .map(|clip| clip.opacity)
                    .unwrap_or(1.0);
                push_weighted(&mut layers, Some(id), weight.outgoing * opacity);
                push_weighted(&mut layers, None, weight.black);
            }
            Anchor::Cut(id) => {
                claimed.push(id);
                let outgoing_opacity = clip_by_id(track, id)
                    .map(|clip| clip.opacity)
                    .unwrap_or(1.0);
                push_weighted(&mut layers, Some(id), weight.outgoing * outgoing_opacity);
                if let Some(outgoing) = clip_by_id(track, id) {
                    if let Some(incoming) = next_clip(track, outgoing) {
                        claimed.push(incoming.id);
                        push_weighted(
                            &mut layers,
                            Some(incoming.id),
                            weight.incoming * incoming.opacity,
                        );
                    }
                }
                push_weighted(&mut layers, None, weight.black);
            }
        }
    }

    let mut active: Vec<&Clip> = track
        .clips
        .iter()
        .filter(|clip| clip.contains(frame) && !claimed.contains(&clip.id))
        .collect();
    active.sort_by_key(|clip| (clip.timeline_start, clip.id));
    if let Some(clip) = active.last() {
        let mut opacity = clip.opacity;
        opacity *= edge_fade(sequence, clip, frame, true);
        opacity *= edge_fade(sequence, clip, frame, false);
        push_weighted(&mut layers, Some(clip.id), opacity);
    }
    layers
}

fn edge_fade(sequence: &Sequence, clip: &Clip, frame: i64, head: bool) -> f32 {
    for transition in &sequence.transitions {
        let matches = if head {
            matches!(transition.anchor, Anchor::Head(id) if id == clip.id)
        } else {
            matches!(transition.anchor, Anchor::Tail(id) if id == clip.id)
        };
        if !matches || transition.kind.is_audio() {
            continue;
        }
        if let Some((start, end)) = transition_window(sequence, transition) {
            if frame >= start && frame < end {
                let p = progress_at(frame, start, transition.duration.max(1));
                let weight = video_weight(transition.kind, p);
                return if head {
                    weight.incoming
                } else {
                    weight.outgoing
                };
            }
        }
    }
    1.0
}

fn push_weighted(layers: &mut Vec<Layer>, clip_id: Option<u64>, weight: f32) {
    if weight <= 0.001 {
        return;
    }
    layers.push(Layer {
        clip_id,
        weight: weight.clamp(0.0, 1.0) });
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioGain {
    pub clip_id: u64,
    pub gain: f32 }

pub fn audio_gains(sequence: &Sequence, track_index: usize, frame: i64) -> Vec<AudioGain> {
    let Some(track) = sequence.tracks.get(track_index) else {
        return Vec::new();
    };
    if track.muted {
        return Vec::new();
    }
    let mut claimed = Vec::new();
    let mut gains = Vec::new();

    for transition in &sequence.transitions {
        let audio_kind = match transition.kind {
            TransitionKind::AudioCrossfade => TransitionKind::AudioCrossfade,
            TransitionKind::AudioFadeIn => TransitionKind::AudioFadeIn,
            TransitionKind::AudioFadeOut => TransitionKind::AudioFadeOut,
            _ => continue };
        let id = transition.anchor.clip_id();
        if track.clips.iter().all(|clip| clip.id != id) {
            continue;
        }
        let Some((start, end)) = transition_window(sequence, transition) else {
            continue;
        };
        if frame < start || frame >= end {
            continue;
        }
        let p = progress_at(frame, start, transition.duration.max(1));
        let (outgoing, incoming) = audio_gains_for_kind(audio_kind, p);
        match transition.anchor {
            Anchor::Head(id) => {
                claimed.push(id);
                push_gain(&mut gains, track, id, incoming);
            }
            Anchor::Tail(id) => {
                claimed.push(id);
                push_gain(&mut gains, track, id, outgoing);
            }
            Anchor::Cut(id) => {
                claimed.push(id);
                push_gain(&mut gains, track, id, outgoing);
                if let Some(outgoing_clip) = clip_by_id(track, id) {
                    if let Some(incoming_clip) = next_clip(track, outgoing_clip) {
                        claimed.push(incoming_clip.id);
                        push_gain(&mut gains, track, incoming_clip.id, incoming);
                    }
                }
            }
        }
    }

    for clip in &track.clips {
        if clip.contains(frame) && !claimed.contains(&clip.id) {
            push_gain(&mut gains, track, clip.id, 1.0);
        }
    }
    gains
}

fn push_gain(gains: &mut Vec<AudioGain>, track: &Track, id: u64, envelope: f32) {
    let Some(clip) = clip_by_id(track, id) else {
        return;
    };
    let gain = clip.volume.max(0.0) * envelope.max(0.0);
    if gain <= 0.0001 {
        return;
    }
    gains.push(AudioGain { clip_id: id, gain });
}

