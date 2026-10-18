use std::collections::HashMap;

use crate::color::Grade;
use crate::project::{
    Anchor, Clip, MediaKind, Project, SourceRange, TrackKind, Transition, TransitionKind };

use super::layout::{snap_delta, Edge};

#[derive(Clone, Debug)]
pub struct Placement {
    pub media_id: u64,
    pub track: usize,
    pub source_in: i64,
    pub source_time_offset: f64,
    pub duration: i64,
    pub name: String,
    pub link: Option<u64> }

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ClipCopy {
    pub track: usize,
    pub clip: Clip,
    pub transitions: Vec<Transition> }

pub fn cleanup(project: &mut Project) {
    let ids: Vec<u64> = project
        .sequence
        .tracks
        .iter()
        .flat_map(|track| track.clips.iter().map(|clip| clip.id))
        .collect();
    project.sequence.transitions.retain(|transition| {
        transition.duration >= 1 && ids.contains(&transition.anchor.clip_id())
    });
    let caps: Vec<(usize, i64)> = project
        .sequence
        .transitions
        .iter()
        .enumerate()
        .filter_map(|(index, transition)| {
            let clip = project.clip(transition.anchor.clip_id())?.clone();
            let cap = match transition.anchor {
                Anchor::Head(_) | Anchor::Tail(_) => clip.duration.max(1),
                Anchor::Cut(_) => {
                    let incoming = incoming_duration(project, &clip).unwrap_or(clip.duration);
                    (clip.duration.min(incoming) * 2).max(2)
                }
            };
            Some((index, transition.duration.clamp(2, cap.max(2))))
        })
        .collect();
    for (index, duration) in caps {
        project.sequence.transitions[index].duration = duration;
    }
    for track in &mut project.sequence.tracks {
        track
            .clips
            .retain(|clip| clip.duration >= 1 && clip.timeline_start >= 0);
        track
            .clips
            .sort_by_key(|clip| (clip.timeline_start, clip.id));
    }
}

fn incoming_duration(project: &Project, outgoing: &Clip) -> Option<i64> {
    let track_index = project.track_index_of_clip(outgoing.id)?;
    project.sequence.tracks[track_index]
        .clips
        .iter()
        .filter(|clip| clip.id != outgoing.id && clip.timeline_start >= outgoing.end() - 1)
        .min_by_key(|clip| clip.timeline_start)
        .map(|clip| clip.duration)
}

pub fn build_placements(
    project: &mut Project,
    media_id: u64,
    selected_track: Option<usize>,
) -> Result<Vec<Placement>, String> {
    let mut items = plan_placements(project, media_id, selected_track)?;
    assign_placement_link(project, &mut items);
    Ok(items)
}

pub fn build_placements_in_range(
    project: &mut Project,
    media_id: u64,
    selected_track: Option<usize>,
    range: SourceRange,
) -> Result<Vec<Placement>, String> {
    let mut items = plan_placements_in_range(project, media_id, selected_track, range)?;
    assign_placement_link(project, &mut items);
    Ok(items)
}

fn assign_placement_link(project: &mut Project, items: &mut [Placement]) {
    if items.len() > 1 {
        let link = project.alloc();
        for item in items {
            item.link = Some(link);
        }
    }
}

/// Preview all destination lanes without allocating IDs or changing history.
pub fn plan_placements(
    project: &Project,
    media_id: u64,
    selected_track: Option<usize>,
) -> Result<Vec<Placement>, String> {
    let range = project
        .media(media_id)
        .ok_or("Select a clip in the bin")?
        .source_range()?;
    plan_placements_in_range(project, media_id, selected_track, range)
}

/// Use a frozen Source range for a drag preview and its eventual edit.
pub fn plan_placements_in_range(
    project: &Project,
    media_id: u64,
    selected_track: Option<usize>,
    range: SourceRange,
) -> Result<Vec<Placement>, String> {
    let media = project.media(media_id).ok_or("Select a clip in the bin")?;
    media.validate_source_range(range)?;
    let source = range.placement(project.sequence.frame_rate)?;
    let mut items = Vec::new();
    if media.has_video || media.kind == MediaKind::Still {
        let track = target_track(project, TrackKind::Video, selected_track)?;
        items.push(Placement {
            media_id,
            track,
            source_in: source.source_in,
            source_time_offset: source.source_time_offset,
            duration: source.duration,
            name: media.name.clone(),
            link: None });
    }
    if media.has_audio {
        let track = target_track(project, TrackKind::Audio, selected_track)?;
        items.push(Placement {
            media_id,
            track,
            source_in: source.source_in,
            source_time_offset: source.source_time_offset,
            duration: source.duration,
            name: media.name.clone(),
            link: None });
    }
    if items.is_empty() {
        return Err("This media has no picture or sound".into());
    }
    Ok(items)
}

fn target_track(
    project: &Project,
    kind: TrackKind,
    selected: Option<usize>,
) -> Result<usize, String> {
    if let Some(index) = selected {
        if let Some(track) = project.sequence.tracks.get(index) {
            if track.kind == kind {
                if track.locked {
                    return Err(format!(
                        "{} is locked. Choose an unlocked destination",
                        track.name
                    ));
                }
                return Ok(index);
            }
        }
    }
    project
        .sequence
        .tracks
        .iter()
        .position(|track| track.kind == kind && !track.locked)
        .ok_or_else(|| match kind {
            TrackKind::Video => "No unlocked video track".to_string(),
            TrackKind::Audio => "No unlocked audio track".to_string() })
}

fn make_clip(project: &mut Project, item: &Placement, at: i64) -> Clip {
    Clip {
        transform: Default::default(),
        id: project.alloc(),
        media_id: item.media_id,
        name: item.name.clone(),
        timeline_start: at.max(0),
        duration: item.duration.max(1),
        source_in: item.source_in.max(0),
        source_time_offset: item.source_time_offset,
        link: item.link,
        volume: 1.0,
        pos_x: 0.0,
        pos_y: 0.0,
        scale: 1.0,
        opacity: 1.0,
        grade: Grade::default() }
}

pub fn insert_placements(
    project: &mut Project,
    at: i64,
    items: &[Placement],
) -> Result<(), String> {
    if items.is_empty() {
        return Err("Nothing to insert".into());
    }
    let at = at.max(0);
    for item in items {
        let track = project
            .sequence
            .tracks
            .get(item.track)
            .ok_or("Missing track")?;
        if track.locked {
            return Err(format!("{} is locked", track.name));
        }
    }
    let mut split_links = HashMap::new();
    for item in items {
        split_track_at(project, item.track, at, &mut split_links);
        let duration = item.duration.max(1);
        let clips = &mut project.sequence.tracks[item.track].clips;
        for clip in clips.iter_mut() {
            if clip.timeline_start >= at {
                clip.timeline_start += duration;
            }
        }
        let clip = make_clip(project, item, at);
        project.sequence.tracks[item.track].clips.push(clip);
    }
    cleanup(project);
    Ok(())
}

pub fn overwrite_placements(
    project: &mut Project,
    at: i64,
    items: &[Placement],
) -> Result<(), String> {
    if items.is_empty() {
        return Err("Nothing to overwrite".into());
    }
    let mut split_links = HashMap::new();
    for item in items {
        let clip = make_clip(project, item, at);
        overwrite_clip_with_links(project, item.track, clip, &mut split_links)?;
    }
    cleanup(project);
    Ok(())
}

pub fn overwrite_clip(
    project: &mut Project,
    track_index: usize,
    new_clip: Clip,
) -> Result<(), String> {
    overwrite_clip_with_links(project, track_index, new_clip, &mut HashMap::new())
}

fn overwrite_clip_with_links(
    project: &mut Project,
    track_index: usize,
    new_clip: Clip,
    split_links: &mut HashMap<(u64, i64), u64>,
) -> Result<(), String> {
    let locked = project
        .sequence
        .tracks
        .get(track_index)
        .ok_or("Missing track")?
        .locked;
    let name = project.sequence.tracks[track_index].name.clone();
    if locked {
        return Err(format!("{name} is locked"));
    }
    clear_range_with_links(
        project,
        track_index,
        new_clip.timeline_start,
        new_clip.end(),
        &[new_clip.id],
        split_links,
    );
    project.sequence.tracks[track_index].clips.push(new_clip);
    Ok(())
}

pub fn clear_range(project: &mut Project, track_index: usize, start: i64, end: i64, keep: &[u64]) {
    clear_range_with_links(project, track_index, start, end, keep, &mut HashMap::new());
}

fn clear_range_with_links(
    project: &mut Project,
    track_index: usize,
    start: i64,
    end: i64,
    keep: &[u64],
    split_links: &mut HashMap<(u64, i64), u64>,
) {
    if end <= start {
        return;
    }
    let clips = std::mem::take(&mut project.sequence.tracks[track_index].clips);
    let mut kept = Vec::new();
    for clip in clips {
        if keep.contains(&clip.id) || clip.end() <= start || clip.timeline_start >= end {
            kept.push(clip);
            continue;
        }
        let old_id = clip.id;
        if clip.timeline_start < start && clip.end() > end {
            let mut left = clip.clone();
            left.duration = start - clip.timeline_start;
            let mut right = clip.clone();
            let trimmed = end - clip.timeline_start;
            right.id = project.alloc();
            right.link = split_right_link(project, right.link, end, split_links);
            right.source_in += trimmed;
            right.duration -= trimmed;
            right.timeline_start = end;
            retarget_split(project, old_id, left.id, right.id);
            if left.duration >= 1 {
                kept.push(left);
            }
            if right.duration >= 1 && source_ok(project, &right) {
                kept.push(right);
            }
            continue;
        }
        if clip.timeline_start < start {
            let mut left = clip;
            left.duration = start - left.timeline_start;
            if left.duration >= 1 {
                kept.push(left);
            } else {
                drop_transitions(project, old_id);
            }
            continue;
        }
        if clip.end() > end {
            let mut right = clip;
            let trimmed = end - right.timeline_start;
            right.source_in += trimmed;
            right.duration -= trimmed;
            right.timeline_start = end;
            drop_head_transitions(project, old_id);
            if right.duration >= 1 && source_ok(project, &right) {
                kept.push(right);
            } else {
                drop_transitions(project, old_id);
            }
            continue;
        }
        drop_transitions(project, old_id);
    }
    project.sequence.tracks[track_index].clips = kept;
}

fn source_ok(project: &Project, clip: &Clip) -> bool {
    let Some(media) = project.media(clip.media_id) else {
        return clip.duration >= 1;
    };
    media.clip_source_is_valid(clip, project.sequence.frame_rate)
}

fn drop_transitions(project: &mut Project, clip_id: u64) {
    project
        .sequence
        .transitions
        .retain(|transition| transition.anchor.clip_id() != clip_id);
}

fn drop_head_transitions(project: &mut Project, clip_id: u64) {
    project
        .sequence
        .transitions
        .retain(|transition| !matches!(transition.anchor, Anchor::Head(id) if id == clip_id));
}

fn retarget_split(project: &mut Project, old_id: u64, left_id: u64, right_id: u64) {
    for transition in &mut project.sequence.transitions {
        match &mut transition.anchor {
            Anchor::Head(id) if *id == old_id => *id = left_id,
            Anchor::Tail(id) | Anchor::Cut(id) if *id == old_id => *id = right_id,
            _ => {}
        }
    }
}

fn split_right_link(
    project: &mut Project,
    link: Option<u64>,
    boundary: i64,
    split_links: &mut HashMap<(u64, i64), u64>,
) -> Option<u64> {
    link.map(|original| {
        *split_links
            .entry((original, boundary))
            .or_insert_with(|| project.alloc())
    })
}

fn split_track_at(
    project: &mut Project,
    track_index: usize,
    frame: i64,
    split_links: &mut HashMap<(u64, i64), u64>,
) {
    let clips = std::mem::take(&mut project.sequence.tracks[track_index].clips);
    let mut next = Vec::new();
    for clip in clips {
        if frame > clip.timeline_start && frame < clip.end() {
            let old = clip.id;
            let left_dur = frame - clip.timeline_start;
            let mut left = clip.clone();
            left.duration = left_dur;
            let mut right = clip;
            right.id = project.alloc();
            right.link = split_right_link(project, right.link, frame, split_links);
            right.timeline_start = frame;
            right.source_in += left_dur;
            right.duration -= left_dur;
            retarget_split(project, old, left.id, right.id);
            next.push(left);
            next.push(right);
        } else {
            next.push(clip);
        }
    }
    project.sequence.tracks[track_index].clips = next;
}

pub fn split_at(project: &mut Project, ids: &[u64], frame: i64) -> Result<(), String> {
    if ids.is_empty() {
        let all = unlocked_clips_at(project, frame);
        if all.is_empty() {
            return Err("Nothing to split at the playhead".into());
        }
        return split_at(project, &all, frame);
    }
    let ids = expand_links(project, ids);
    validate_unlocked(project, &ids)?;
    let mut split_links = HashMap::new();
    let mut did = false;
    for id in ids {
        let Some(track_index) = project.track_index_of_clip(id) else {
            continue;
        };
        if project.sequence.tracks[track_index].locked {
            continue;
        }
        let Some(clip) = project.clip(id).cloned() else {
            continue;
        };
        if frame <= clip.timeline_start || frame >= clip.end() {
            continue;
        }
        split_track_at(project, track_index, frame, &mut split_links);
        did = true;
    }
    cleanup(project);
    if did {
        Ok(())
    } else {
        Err("Playhead is not inside a selected clip".into())
    }
}

fn unlocked_clips_at(project: &Project, frame: i64) -> Vec<u64> {
    project
        .sequence
        .tracks
        .iter()
        .filter(|track| !track.locked)
        .flat_map(|track| track.clips.iter())
        .filter(|clip| clip.contains(frame))
        .map(|clip| clip.id)
        .collect()
}

pub fn expand_links(project: &Project, ids: &[u64]) -> Vec<u64> {
    let mut out = ids.to_vec();
    let links: Vec<u64> = ids
        .iter()
        .filter_map(|id| project.clip(*id).and_then(|clip| clip.link))
        .collect();
    for track in &project.sequence.tracks {
        for clip in &track.clips {
            if let Some(link) = clip.link {
                if links.contains(&link) && !out.contains(&clip.id) {
                    out.push(clip.id);
                }
            }
        }
    }
    out
}

pub fn validate_unlocked(project: &Project, ids: &[u64]) -> Result<(), String> {
    if ids.is_empty() {
        return Err("Select clips to edit".into());
    }
    for id in ids {
        let index = project
            .track_index_of_clip(*id)
            .ok_or("A selected clip is missing")?;
        let track = &project.sequence.tracks[index];
        if track.locked {
            return Err(format!(
                "{} is locked. Unlock it before editing these clips",
                track.name
            ));
        }
    }
    Ok(())
}

pub fn unlink_clips(project: &mut Project, ids: &[u64]) -> Result<(), String> {
    let group = expand_links(project, ids);
    validate_unlocked(project, &group)?;
    if !group
        .iter()
        .any(|id| project.clip(*id).is_some_and(|clip| clip.link.is_some()))
    {
        return Err("Selected clips are already unlinked".into());
    }
    for id in group {
        if let Some(clip) = project.clip_mut(id) {
            clip.link = None;
        }
    }
    Ok(())
}

pub fn link_clips(project: &mut Project, ids: &[u64]) -> Result<(), String> {
    let mut selected = ids.to_vec();
    selected.sort_unstable();
    selected.dedup();
    if selected.len() < 2 {
        return Err("Select at least two clips to link, using Shift to add clips".into());
    }
    let previous_groups = expand_links(project, &selected);
    validate_unlocked(project, &previous_groups)?;
    let existing = project.clip(selected[0]).and_then(|clip| clip.link);
    if existing.is_some()
        && previous_groups.len() == selected.len()
        && selected
            .iter()
            .all(|id| project.clip(*id).is_some_and(|clip| clip.link == existing))
    {
        return Err("Selected clips are already linked".into());
    }
    // Replacing a relationship dissolves its old groups together, so a linked
    // audio or video partner cannot be left with a stale partial relationship.
    for id in previous_groups {
        if let Some(clip) = project.clip_mut(id) {
            clip.link = None;
        }
    }
    let link = project.alloc();
    for id in selected {
        if let Some(clip) = project.clip_mut(id) {
            clip.link = Some(link);
        }
    }
    Ok(())
}

pub fn move_clips_to_track(
    project: &mut Project,
    ids: &[u64],
    anchor_id: u64,
    delta: i64,
    target_track: Option<usize>,
) -> Result<(), String> {
    validate_unlocked(project, ids)?;
    if !ids.contains(&anchor_id) {
        return Err("The grabbed clip is not selected".into());
    }
    let anchor_track = project
        .track_index_of_clip(anchor_id)
        .ok_or("The grabbed clip is missing")?;
    let anchor_kind = project.sequence.tracks[anchor_track].kind;
    let destination = target_track.unwrap_or(anchor_track);
    let destination_track = project
        .sequence
        .tracks
        .get(destination)
        .ok_or("Destination track is missing")?;
    if destination_track.kind != anchor_kind {
        return Err(match anchor_kind {
            TrackKind::Video => "Video clips can only move to video tracks".into(),
            TrackKind::Audio => "Audio clips can only move to audio tracks".into() });
    }
    let anchor_lanes = tracks_of_kind(project, anchor_kind);
    let old_lane = anchor_lanes
        .iter()
        .position(|track| *track == anchor_track)
        .ok_or("Original track is missing")?;
    let new_lane = anchor_lanes
        .iter()
        .position(|track| *track == destination)
        .ok_or("Destination track is missing")?;
    let lane_delta = new_lane as isize - old_lane as isize;
    let min_start = ids
        .iter()
        .filter_map(|id| project.clip(*id).map(|clip| clip.timeline_start))
        .min()
        .unwrap_or(0);
    let delta = delta.max(-min_start);
    let mut moved = Vec::new();
    let mut affected = Vec::new();
    for id in ids {
        if moved.iter().any(|(_, clip): &(usize, Clip)| clip.id == *id) {
            continue;
        }
        let origin = project
            .track_index_of_clip(*id)
            .ok_or("A selected clip is missing")?;
        let kind = project.sequence.tracks[origin].kind;
        let lanes = tracks_of_kind(project, kind);
        let rank = lanes
            .iter()
            .position(|track| *track == origin)
            .ok_or("Original track is missing")?;
        let rank = rank as isize + lane_delta;
        let Some(&target) = usize::try_from(rank).ok().and_then(|rank| lanes.get(rank)) else {
            let label = match kind {
                TrackKind::Video => "video",
                TrackKind::Audio => "audio" };
            return Err(format!(
                "This group needs another {label} track. Add a track or move fewer lanes"
            ));
        };
        let track = &project.sequence.tracks[target];
        if track.locked {
            return Err(format!(
                "{} is locked. Choose an unlocked destination",
                track.name
            ));
        }
        let mut clip = project
            .clip(*id)
            .cloned()
            .ok_or("A selected clip is missing")?;
        clip.timeline_start = clip
            .timeline_start
            .checked_add(delta)
            .ok_or("Move exceeds the timeline range")?;
        if clip.timeline_start < 0 || clip.timeline_start.checked_add(clip.duration).is_none() {
            return Err("Move exceeds the timeline range".into());
        }
        moved.push((target, clip));
        for index in [origin, target] {
            if !affected.contains(&index) {
                affected.push(index);
            }
        }
    }

    let mut tracks = Vec::new();
    for index in affected {
        let mut clips: Vec<Clip> = project.sequence.tracks[index]
            .clips
            .iter()
            .filter(|clip| !ids.contains(&clip.id))
            .cloned()
            .collect();
        clips.extend(
            moved
                .iter()
                .filter(|(target, _)| *target == index)
                .map(|(_, clip)| clip.clone()),
        );
        if !track_ranges_clear(project, index, &clips) {
            return Err(format!(
                "Move would overlap another clip on {}",
                project.sequence.tracks[index].name
            ));
        }
        tracks.push((index, clips));
    }
    for (index, clips) in tracks {
        project.sequence.tracks[index].clips = clips;
    }
    cleanup(project);
    Ok(())
}

fn tracks_of_kind(project: &Project, kind: TrackKind) -> Vec<usize> {
    project
        .sequence
        .tracks
        .iter()
        .enumerate()
        .filter_map(|(index, track)| (track.kind == kind).then_some(index))
        .collect()
}

pub fn lift_delete(project: &mut Project, ids: &[u64]) -> Result<(), String> {
    let ids = expand_links(project, ids);
    validate_unlocked(project, &ids)?;
    for id in &ids {
        drop_transitions(project, *id);
    }
    for track in &mut project.sequence.tracks {
        if track.locked {
            continue;
        }
        track.clips.retain(|clip| !ids.contains(&clip.id));
    }
    cleanup(project);
    Ok(())
}

pub fn ripple_delete(project: &mut Project, ids: &[u64]) -> Result<(), String> {
    let ids = expand_links(project, ids);
    validate_unlocked(project, &ids)?;
    for track_index in 0..project.sequence.tracks.len() {
        if project.sequence.tracks[track_index].locked {
            continue;
        }
        let removed: Vec<Clip> = project.sequence.tracks[track_index]
            .clips
            .iter()
            .filter(|clip| ids.contains(&clip.id))
            .cloned()
            .collect();
        if removed.is_empty() {
            continue;
        }
        for clip in &removed {
            drop_transitions(project, clip.id);
        }
        project.sequence.tracks[track_index]
            .clips
            .retain(|clip| !ids.contains(&clip.id));
        for clip in &mut project.sequence.tracks[track_index].clips {
            let shift: i64 = removed
                .iter()
                .filter(|gone| gone.end() <= clip.timeline_start)
                .map(|gone| gone.duration)
                .sum();
            clip.timeline_start -= shift;
        }
    }
    cleanup(project);
    Ok(())
}

pub fn resolve_moved(project: &mut Project, ids: &[u64]) {
    let moved = expand_links(project, ids);
    let ranges: Vec<(usize, i64, i64)> = moved
        .iter()
        .filter_map(|id| {
            let clip = project.clip(*id)?;
            let track = project.track_index_of_clip(*id)?;
            Some((track, clip.timeline_start, clip.end()))
        })
        .collect();
    let mut split_links = HashMap::new();
    for (track, start, end) in ranges {
        if project
            .sequence
            .tracks
            .get(track)
            .map(|track| track.locked)
            .unwrap_or(true)
        {
            continue;
        }
        clear_range_with_links(project, track, start, end, &moved, &mut split_links);
    }
    cleanup(project);
}

pub fn shift_to(project: &mut Project, originals: &[(u64, i64)], delta: i64) -> i64 {
    let min_start = originals.iter().map(|(_, start)| *start).min().unwrap_or(0);
    let delta = delta.max(-min_start);
    for (id, start) in originals {
        if let Some(clip) = project.clip_mut(*id) {
            clip.timeline_start = *start + delta;
        }
    }
    delta
}

pub fn try_trim_in(project: &mut Project, id: u64, new_start: i64, ripple: bool) -> bool {
    let Some(delta) = project
        .clip(id)
        .and_then(|clip| new_start.max(0).checked_sub(clip.timeline_start))
    else {
        return false;
    };
    try_trim_group(project, &[id], Edge::In, delta, ripple)
}

pub fn try_trim_out(project: &mut Project, id: u64, new_end: i64, ripple: bool) -> bool {
    let Some(delta) = project
        .clip(id)
        .and_then(|clip| new_end.checked_sub(clip.end()))
    else {
        return false;
    };
    try_trim_group(project, &[id], Edge::Out, delta, ripple)
}

/// Apply one edge delta to a linked set after validating every source and track.
/// No clip changes when one member lacks handles or belongs to a locked track.
pub fn try_trim_group(project: &mut Project, ids: &[u64], edge: Edge, delta: i64, ripple: bool) -> bool {
    trim_group_checked(project, ids, edge, delta, ripple).is_ok()
}

pub fn trim_group_checked(
    project: &mut Project,
    ids: &[u64],
    edge: Edge,
    delta: i64,
    ripple: bool,
) -> Result<(), String> {
    struct TrimEdit {
        track: usize,
        original: Clip,
        trimmed: Clip }

    if ids.is_empty() { return Err("Select clips to trim".into()); }
    let mut edits: Vec<TrimEdit> = Vec::new();
    for id in ids {
        if edits.iter().any(|edit| edit.original.id == *id) {
            continue;
        }
        let Some(original) = project.clip(*id).cloned() else {
            return Err("A trimmed clip is missing".into());
        };
        let Some(track) = project.track_index_of_clip(*id) else {
            return Err("The trimmed track is missing".into());
        };
        if project.sequence.tracks[track].locked {
            return Err(format!("{} is locked. Unlock it before trimming", project.sequence.tracks[track].name));
        }
        let mut trimmed = original.clone();
        match edge {
            Edge::In => {
                let Some(source_in) = original.source_in.checked_add(delta) else {
                    return Err("Trim exceeds the timeline range".into());
                };
                let Some(duration) = original.duration.checked_sub(delta) else {
                    return Err("Trim exceeds the timeline range".into());
                };
                trimmed.source_in = source_in;
                trimmed.duration = duration;
                if !ripple {
                    let Some(start) = original.timeline_start.checked_add(delta) else {
                        return Err("Trim exceeds the timeline range".into());
                    };
                    trimmed.timeline_start = start;
                }
                // A ripple head trim changes the source head and duration while
                // keeping this clip at its original timeline start.
            }
            Edge::Out => {
                let Some(duration) = original.duration.checked_add(delta) else {
                    return Err("Trim exceeds the timeline range".into());
                };
                trimmed.duration = duration;
            }
        }
        if trimmed.timeline_start < 0 { return Err("Trim reaches before the timeline start".into()); }
        if trimmed.duration < 1 { return Err("A clip must retain at least one frame".into()); }
        if trimmed.timeline_start.checked_add(trimmed.duration).is_none() { return Err("Trim exceeds the timeline range".into()); }
        if trimmed.source_in < 0 || !source_ok(project, &trimmed) { return Err("Trim reaches the available source limit".into()); }
        edits.push(TrimEdit {
            track,
            original,
            trimmed });
    }

    if delta == 0 {
        return Ok(());
    }

    let mut tracks = Vec::new();
    for edit in &edits {
        if tracks.iter().any(|(track, _)| *track == edit.track) {
            continue;
        }
        let mut clips = Vec::new();
        for original in &project.sequence.tracks[edit.track].clips {
            let mut next = edits
                .iter()
                .find(|edit| edit.original.id == original.id)
                .map(|edit| edit.trimmed.clone())
                .unwrap_or_else(|| original.clone());
            if ripple {
                let mut shift = 0i64;
                for preceding in edits.iter().filter(|candidate| {
                    candidate.track == edit.track
                        && candidate.original.id != original.id
                        && candidate.original.end() <= original.timeline_start
                }) {
                    let duration_delta = preceding.trimmed.duration - preceding.original.duration;
                    let Some(total) = shift.checked_add(duration_delta) else {
                        return Err("Trim exceeds the timeline range".into());
                    };
                    shift = total;
                }
                let Some(start) = next.timeline_start.checked_add(shift) else {
                    return Err("Trim exceeds the timeline range".into());
                };
                if start < 0 {
                    return Err("Trim exceeds the timeline range".into());
                }
                next.timeline_start = start;
            }
            if next.timeline_start.checked_add(next.duration).is_none() {
                return Err("Trim exceeds the timeline range".into());
            }
            clips.push(next);
        }
        if !track_ranges_clear(project, edit.track, &clips) {
            return Err("Trim would overlap another clip".into());
        }
        tracks.push((edit.track, clips));
    }

    for (track, clips) in tracks {
        project.sequence.tracks[track].clips = clips;
    }
    cleanup(project);
    Ok(())
}

fn track_ranges_clear(project: &Project, track: usize, clips: &[Clip]) -> bool {
    let originals: HashMap<u64, (usize, &Clip)> = project
        .sequence
        .tracks
        .iter()
        .enumerate()
        .flat_map(|(index, track)| track.clips.iter().map(move |clip| (clip.id, (index, clip))))
        .collect();
    let changed: Vec<bool> = clips
        .iter()
        .map(|after| {
            originals
                .get(&after.id)
                .map_or(true, |(before_track, before)| {
                    *before_track != track
                        || before.timeline_start != after.timeline_start
                        || before.duration != after.duration
                })
        })
        .collect();
    let mut ordered: Vec<usize> = (0..clips.len()).collect();
    ordered.sort_unstable_by_key(|index| (clips[*index].timeline_start, clips[*index].id));
    let mut active: Vec<usize> = Vec::new();
    for index in ordered {
        let clip = &clips[index];
        active.retain(|other| clips[*other].end() > clip.timeline_start);
        for other in &active {
            if !changed[index] && !changed[*other] {
                continue;
            }
            let previous = &clips[*other];
            let overlap = previous.end().min(clip.end()) - clip.timeline_start;
            let prior_overlap = originals
                .get(&clip.id)
                .zip(originals.get(&previous.id))
                .map_or(0, |((_, before), (_, before_previous))| {
                    before
                        .end()
                        .min(before_previous.end())
                        .saturating_sub(before.timeline_start.max(before_previous.timeline_start))
                        .max(0)
                });
            // Preserve an imported overlap when the edit leaves it unchanged or
            // reduces it, while rejecting every new or enlarged collision.
            if overlap > prior_overlap {
                return false;
            }
        }
        active.push(index);
    }
    true
}

pub fn ripple_trim_previous(project: &mut Project, playhead: i64) -> Result<(), String> {
    ripple_trim_at(project, playhead, true)
}

pub fn ripple_trim_next(project: &mut Project, playhead: i64) -> Result<(), String> {
    ripple_trim_at(project, playhead, false)
}

fn ripple_trim_at(project: &mut Project, playhead: i64, previous: bool) -> Result<(), String> {
    let mut edits = Vec::new();
    let mut visited = Vec::new();
    for id in trim_targets(project, playhead) {
        if visited.contains(&id) {
            continue;
        }
        let Some(clip) = project.clip(id) else {
            continue;
        };
        let edge = if clip.contains(playhead) {
            if previous { Edge::In } else { Edge::Out }
        } else if previous && clip.end() <= playhead {
            Edge::Out
        } else if !previous && clip.timeline_start >= playhead {
            Edge::In
        } else {
            continue;
        };
        let boundary = match edge {
            Edge::In => clip.timeline_start,
            Edge::Out => clip.end() };
        let delta = playhead.checked_sub(boundary).ok_or("Trim exceeds the timeline range")?;
        let group = expand_links(project, &[id]);
        visited.extend(group.iter().copied());
        if delta != 0 {
            edits.push((group, edge, delta));
        }
    }

    // Q/W must use the same source, lock, and overlap checks as dragging an
    // edge. Stage every group together so a rejected partner cannot leave an
    // earlier lane trimmed or move linked picture and sound out of sync.
    let mut trimmed = project.clone();
    for (group, edge, delta) in edits {
        trim_group_checked(&mut trimmed, &group, edge, delta, true)?;
    }
    *project = trimmed;
    Ok(())
}

fn trim_targets(project: &Project, playhead: i64) -> Vec<u64> {
    let mut ids = Vec::new();
    for track in &project.sequence.tracks {
        if track.locked {
            continue;
        }
        if let Some(clip) = track.clips.iter().find(|clip| clip.contains(playhead)) {
            ids.push(clip.id);
            continue;
        }
        if let Some(clip) = track
            .clips
            .iter()
            .filter(|clip| clip.end() <= playhead)
            .max_by_key(|clip| clip.end())
        {
            ids.push(clip.id);
        }
        if let Some(clip) = track
            .clips
            .iter()
            .filter(|clip| clip.timeline_start >= playhead)
            .min_by_key(|clip| clip.timeline_start)
        {
            ids.push(clip.id);
        }
    }
    ids.sort_unstable();
    ids.dedup();
    ids
}

pub fn copy_selection(project: &Project, ids: &[u64]) -> Vec<ClipCopy> {
    let ids = expand_links(project, ids);
    let mut copies = Vec::new();
    for id in ids {
        let Some(track) = project.track_index_of_clip(id) else {
            continue;
        };
        let Some(clip) = project.clip(id).cloned() else {
            continue;
        };
        let transitions = project
            .sequence
            .transitions
            .iter()
            .filter(|transition| transition.anchor.clip_id() == id)
            .cloned()
            .collect();
        copies.push(ClipCopy {
            track,
            clip,
            transitions });
    }
    copies.sort_by_key(|copy| (copy.clip.timeline_start, copy.clip.id));
    copies
}

pub fn paste_copies(project: &mut Project, copies: &[ClipCopy], at: i64) -> Result<(), String> {
    if copies.is_empty() {
        return Err("Clipboard is empty".into());
    }
    let min_start = copies
        .iter()
        .map(|copy| copy.clip.timeline_start)
        .min()
        .unwrap_or(0);
    let mut link_map: HashMap<u64, u64> = HashMap::new();
    let mut id_map: HashMap<u64, u64> = HashMap::new();
    let mut split_links = HashMap::new();
    for copy in copies {
        let track = remap_track(project, copy.track, &copy.clip)?;
        if project.sequence.tracks[track].locked {
            return Err(format!("{} is locked", project.sequence.tracks[track].name));
        }
        let mut clip = copy.clip.clone();
        let old_id = clip.id;
        clip.id = project.alloc();
        clip.timeline_start = at + (copy.clip.timeline_start - min_start);
        if clip.timeline_start < 0 {
            clip.timeline_start = 0;
        }
        clip.link = clip
            .link
            .map(|link| *link_map.entry(link).or_insert_with(|| project.alloc()));
        id_map.insert(old_id, clip.id);
        overwrite_clip_with_links(project, track, clip, &mut split_links)?;
    }
    for copy in copies {
        let Some(&new_id) = id_map.get(&copy.clip.id) else {
            continue;
        };
        for transition in &copy.transitions {
            let mut next = transition.clone();
            next.id = project.alloc();
            next.anchor = match transition.anchor {
                Anchor::Head(_) => Anchor::Head(new_id),
                Anchor::Tail(_) => Anchor::Tail(new_id),
                Anchor::Cut(_) => Anchor::Cut(new_id) };
            project.sequence.transitions.push(next);
        }
    }
    cleanup(project);
    Ok(())
}

fn remap_track(project: &Project, index: usize, clip: &Clip) -> Result<usize, String> {
    if let Some(track) = project.sequence.tracks.get(index) {
        let media_kind = project.media(clip.media_id).map(|media| {
            if media.has_video && track.kind == TrackKind::Video {
                return track.kind;
            }
            track.kind
        });
        let _ = media_kind;
        return Ok(index);
    }
    let kind = project
        .sequence
        .tracks
        .get(index)
        .map(|track| track.kind)
        .unwrap_or(TrackKind::Video);
    target_track(project, kind, None)
}

pub fn add_transition(
    project: &mut Project,
    kind: TransitionKind,
    clip_id: u64,
) -> Result<u64, String> {
    let clip = project.clip(clip_id).cloned().ok_or("Select a clip")?;
    let track_index = project
        .track_index_of_clip(clip_id)
        .ok_or("Missing track")?;
    let track = &project.sequence.tracks[track_index];
    if track.locked {
        return Err(format!("{} is locked", track.name));
    }
    let video_track = track.kind == TrackKind::Video;
    if kind.is_audio() && video_track {
        return Err("Audio transitions go on an audio clip".into());
    }
    if !kind.is_audio() && !video_track {
        return Err("Video transitions go on a video clip".into());
    }
    let anchor = if kind.is_cut() {
        Anchor::Cut(clip_id)
    } else if matches!(kind, TransitionKind::FadeIn | TransitionKind::AudioFadeIn) {
        Anchor::Head(clip_id)
    } else {
        Anchor::Tail(clip_id)
    };
    project.sequence.transitions.retain(|transition| {
        transition.anchor != anchor || transition.kind.is_audio() != kind.is_audio()
    });
    project
        .sequence
        .transitions
        .retain(|transition| transition.anchor != anchor);
    let duration = default_transition_duration(project);
    let id = project.alloc();
    project.sequence.transitions.push(Transition {
        id,
        kind,
        duration,
        anchor });
    cleanup(project);
    Ok(id)
}

pub fn default_transition_duration(project: &Project) -> i64 {
    project.sequence.frame_rate.nominal().clamp(2, 48)
}

pub fn set_transition_duration(project: &mut Project, id: u64, duration: i64) {
    if let Some(transition) = project
        .sequence
        .transitions
        .iter_mut()
        .find(|transition| transition.id == id)
    {
        transition.duration = duration.max(2);
    }
    cleanup(project);
}

pub fn remove_transition(project: &mut Project, id: u64) {
    project
        .sequence
        .transitions
        .retain(|transition| transition.id != id);
}

pub fn apply_snap(sequence_edges: &[i64], targets: &[i64], threshold_frames: i64) -> i64 {
    snap_delta(sequence_edges, targets, threshold_frames)
}

