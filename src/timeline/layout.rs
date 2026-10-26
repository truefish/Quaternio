use crate::project::{Sequence, TrackKind};
use crate::transitions::transition_window;

pub const TRACK_H: f32 = 28.0;
const MAX_TRACK_H: f32 = 50.0;
pub const EDGE_PX: f32 = 6.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackRow {
    pub track_index: usize,
    pub y: f32,
    pub h: f32 }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    In,
    Out }

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hit {
    Empty { frame: i64 },
    Clip { id: u64, edge: Option<Edge> },
    Transition { id: u64 } }

fn visual_order(sequence: &Sequence) -> Vec<usize> {
    let mut videos: Vec<usize> = sequence
        .tracks
        .iter()
        .enumerate()
        .filter(|(_, track)| track.kind == TrackKind::Video)
        .map(|(index, _)| index)
        .collect();
    videos.reverse();
    let mut order = videos;
    for (index, track) in sequence.tracks.iter().enumerate() {
        if track.kind == TrackKind::Audio {
            order.push(index);
        }
    }
    order
}


/// Explicit lane sizes are local view preferences, never document edits.
pub fn layout_rows_with_density(sequence: &Sequence, body: f32, density: i32) -> Vec<TrackRow> {
    let height = match density { 1 => 28.0, 2 => 48.0, 3 => 76.0, _ => return layout_rows(sequence, body) };
    visual_order(sequence).into_iter().enumerate().map(|(index, track_index)| TrackRow {
        track_index, y: index as f32 * height, h: height,
    }).collect()
}

/// Fit compact rows to the available body without stretching a short timeline
/// into oversized lanes. Many tracks retain their minimum height and scroll.
pub fn layout_rows(sequence: &Sequence, body: f32) -> Vec<TrackRow> {
    let order = visual_order(sequence);
    let count = order.len();
    if count == 0 {
        return Vec::new();
    }
    let min_total = TRACK_H * count as f32;
    let body = if body < 8.0 {
        min_total
    } else {
        body.max(min_total).min(MAX_TRACK_H * count as f32)
    };
    let base = (body / count as f32).floor();
    let mut rows = Vec::with_capacity(count);
    let mut y = 0.0;
    let mut used = 0.0;
    for (index, track_index) in order.into_iter().enumerate() {
        let h = if index + 1 == count {
            body - used
        } else {
            base
        };
        rows.push(TrackRow { track_index, y, h });
        y += h;
        used += h;
    }
    rows
}

pub fn frame_at(x: f32, scroll: f32, px: f32) -> i64 {
    let px = px.max(0.05);
    (scroll + x / px).floor() as i64
}


pub fn hit_layout(sequence: &Sequence, x: f32, y: f32, px: f32, scroll: f32, rows: &[TrackRow]) -> Hit {
    let px = px.max(0.05);
    let frame = frame_at(x, scroll, px);
    let Some(row) = rows.iter().find(|row| y >= row.y && y < row.y + row.h) else {
        return Hit::Empty { frame };
    };
    let track = &sequence.tracks[row.track_index];
    let band_h = 16.0_f32.min(row.h);
    let band_y = row.y + (row.h - band_h).max(0.0) * 0.5;

    for transition in &sequence.transitions {
        let id = transition.anchor.clip_id();
        if track.clips.iter().all(|clip| clip.id != id) {
            continue;
        }
        let Some((start, end)) = transition_window(sequence, transition) else {
            continue;
        };
        let x0 = (start as f32 - scroll) * px;
        let x1 = (end as f32 - scroll) * px;
        if x >= x0 && x <= x1 && y >= band_y && y < band_y + band_h {
            return Hit::Transition { id: transition.id };
        }
    }

    for clip in track.clips.iter().rev() {
        let x0 = (clip.timeline_start as f32 - scroll) * px;
        let x1 = (clip.end() as f32 - scroll) * px;
        if x >= x0 && x < x1 {
            let edge = if x - x0 <= EDGE_PX {
                Some(Edge::In)
            } else if x1 - x <= EDGE_PX {
                Some(Edge::Out)
            } else {
                None
            };
            return Hit::Clip { id: clip.id, edge };
        }
    }
    Hit::Empty { frame }
}




pub fn snap_targets(sequence: &Sequence, ignore: &[u64]) -> Vec<i64> {
    let mut targets = vec![0, sequence.playhead];
    if let Some(frame) = sequence.in_point {
        targets.push(frame);
    }
    if let Some(frame) = sequence.out_point {
        targets.push(frame);
    }
    for track in &sequence.tracks {
        for clip in &track.clips {
            if ignore.contains(&clip.id) {
                continue;
            }
            targets.push(clip.timeline_start);
            targets.push(clip.end());
        }
    }
    targets.sort_unstable();
    targets.dedup();
    targets
}
