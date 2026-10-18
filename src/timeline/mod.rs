mod layout;
pub(crate) mod ops;

pub use layout::{frame_at, hit_rows, layout_rows, layout_rows_with_density, hit_layout, snap_targets, Edge, Hit, TrackRow};
pub use ops::{plan_placements, plan_placements_in_range, ClipCopy, Placement};

// Retain snap ordering/tie behavior while rejecting unrepresentable distances.
fn snap_delta(edges: &[i64], targets: &[i64], threshold: i64) -> i64 {
    if threshold < 0 { return 0; }
    let mut best: Option<i64> = None;
    for edge in edges {
        for target in targets {
            if let Some(delta) = target.checked_sub(*edge) {
                if delta.unsigned_abs() <= threshold as u64 && best.is_none_or(|old| delta.unsigned_abs() < old.unsigned_abs()) {
                    best = Some(delta);
                }
            }
        }
    }
    best.unwrap_or(0)
}
use ops::{
    add_transition, build_placements, build_placements_in_range, copy_selection,
    default_transition_duration, expand_links, insert_placements, lift_delete, link_clips,
    move_clips_to_track, overwrite_placements, paste_copies, remove_transition, ripple_delete,
    ripple_trim_next, ripple_trim_previous, set_transition_duration, split_at, trim_group_checked,
    unlink_clips, validate_unlocked };

use crate::project::{
    FrameRate, Project, ShortcutProfile, SourceRange, SourceTime, TrackKind, TransitionKind };

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Tool {
    Select,
    Razor }

/// Read-only plan shared by Razor feedback and the committed single-clip cut.
#[derive(Clone, Debug)]
pub struct RazorPlan {
    pub frame: i64,
    pub clips: Vec<u64>,
    pub reason: Option<String> }

#[derive(Clone, Debug)]
pub struct GestureFeedback {
    pub clip: u64,
    pub edge: Option<Edge>,
    pub delta: i64,
    pub duration: i64,
    pub snap: Option<i64>,
    pub reason: Option<String> }

#[derive(Clone, Debug)]
enum Gesture {
    Move {
        ids: Vec<u64>,
        anchor_id: u64,
        grab_frame: i64 },
    Trim {
        id: u64,
        ids: Vec<u64>,
        edge: Edge,
        ripple: bool } }

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct EditorSession {
    pub version: u32,
    pub project: Project,
    pub path: Option<std::path::PathBuf>,
    pub dirty: bool,
    undo: Vec<std::sync::Arc<Project>>,
    redo: Vec<std::sync::Arc<Project>>,
    selected_clips: Vec<u64>,
    selected_transition: Option<u64>,
    selected_media: Option<u64>,
    selected_track: Option<usize>,
    clipboard: Vec<ClipCopy>,
    tool: Tool,
    armed: Option<TransitionKind> }

impl EditorSession {
    /// Validate borrowed history without triggering Arc copy-on-write.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1
            || self.undo.len() > 100
            || self.redo.len() > 100
            || self.clipboard.len() > 100_000
            || self.selected_clips.len() > 100_000
        {
            return Err("Invalid or unsupported editor recovery session".into());
        }
        for project in std::iter::once(&self.project)
            .chain(self.undo.iter().map(std::sync::Arc::as_ref))
            .chain(self.redo.iter().map(std::sync::Arc::as_ref))
        {
            if !(1..=crate::project::PROJECT_VERSION).contains(&project.version) {
                return Err("Unsupported recovery project version".into());
            }
            if project.media.len() > 100_000
                || project.sequence.tracks.len() > 1024
                || project
                    .sequence
                    .tracks
                    .iter()
                    .map(|t| t.clips.len())
                    .sum::<usize>()
                    > 100_000
                || project.sequence.width == 0
                || project.sequence.height == 0
                || project.sequence.width > 16384
                || project.sequence.height > 16384
                || project.sequence.transitions.len() > 100_000
                || project.sequence.frame_rate.num == 0
                || project.sequence.frame_rate.den == 0
                || project.next_id == u64::MAX
            {
                return Err("Recovery project exceeds supported limits".into());
            }
            if project.media.iter().any(|m| m.id == u64::MAX)
                || project.sequence.tracks.iter().any(|t| {
                    t.id == u64::MAX
                        || t.clips
                            .iter()
                            .any(|c| c.id == u64::MAX || c.link == Some(u64::MAX))
                })
                || project
                    .sequence
                    .transitions
                    .iter()
                    .any(|t| t.id == u64::MAX)
            {
                return Err("Invalid recovery object identifier".into());
            }
        }
        Ok(())
    }
}

pub struct Editor {
    pub project: Project,
    pub path: Option<std::path::PathBuf>,
    pub dirty: bool,
    undo: Vec<std::sync::Arc<Project>>,
    redo: Vec<std::sync::Arc<Project>>,
    coalesce: Option<&'static str>,
    coalesce_at: Option<std::time::Instant>,
    adjustment_control: Option<(u64, String)>,
    gesture_before: Option<Project>,
    gesture: Option<Gesture>,
    pub gesture_feedback: Option<GestureFeedback>,
    pub selected_clips: Vec<u64>,
    pub selected_transition: Option<u64>,
    pub selected_media: Option<u64>,
    pub selected_track: Option<usize>,
    pub clipboard: Vec<ClipCopy>,
    pub tool: Tool,
    pub armed: Option<TransitionKind> }

impl Editor {
    pub fn clear_shared_history(&mut self) {
        self.undo.clear(); self.redo.clear(); self.coalesce=None; self.coalesce_at=None;
        self.gesture=None; self.gesture_before=None; self.gesture_feedback=None;
        self.adjustment_control=None;
    }

    /// Replace only shared document state; transport, selection, paths and source
    /// marks belong to this editor. Collaborative undo is handled by compensation.
    pub fn apply_shared_project(&mut self, mut project: Project) {
        let same=self.project.document_id==project.document_id;
        let seconds=self.project.sequence.frame_rate.frame_to_seconds(self.project.sequence.playhead);
        project.shortcut_profile=self.project.shortcut_profile;
        project.sequence.playhead=project.sequence.frame_rate.seconds_to_frame(seconds).max(0);
        project.sequence.zoom=self.project.sequence.zoom; project.sequence.scroll=self.project.sequence.scroll;
        project.sequence.snap=self.project.sequence.snap;
        project.sequence.in_point=self.project.sequence.in_point;
        project.sequence.out_point=self.project.sequence.out_point;
        if same {
            for media in &mut project.media {
                if let Some(local)=self.project.media(media.id) {
                    media.path=local.path.clone();media.source_in_mark=local.source_in_mark;media.source_out_mark=local.source_out_mark;
                    if media.source_range().is_err(){media.clear_source_marks();}
                }
            }
        }
        self.project=project;self.clear_shared_history();self.dirty=true;self.sanitize_selection();
    }

    pub fn session_snapshot(&self) -> EditorSession {
        EditorSession {
            version: 1,
            project: self
                .gesture_before
                .as_ref()
                .unwrap_or(&self.project)
                .clone(),
            path: self.path.clone(),
            dirty: self.dirty,
            undo: self.undo.clone(),
            redo: self.redo.clone(),
            selected_clips: self.selected_clips.clone(),
            selected_transition: self.selected_transition,
            selected_media: self.selected_media,
            selected_track: self.selected_track,
            clipboard: self.clipboard.clone(),
            tool: self.tool,
            armed: self.armed }
    }

    pub fn from_session(mut session: EditorSession) -> Result<Self, String> {
        session.validate()?;
        let document_id = session.project.document_id.clone();
        for project in std::iter::once(&mut session.project)
            .chain(session.undo.iter_mut().map(std::sync::Arc::make_mut))
            .chain(session.redo.iter_mut().map(std::sync::Arc::make_mut))
        {
            project.migrate()?;
            project.document_id = document_id.clone();
            project.ensure_integrity();
        }
        let mut editor = Self::from_project(session.project);
        editor.path = session.path;
        editor.dirty = session.dirty;
        editor.undo = session.undo;
        editor.redo = session.redo;
        editor.selected_clips = session.selected_clips;
        editor.selected_transition = session.selected_transition;
        editor.selected_media = session.selected_media;
        editor.selected_track = session.selected_track;
        editor.clipboard = session.clipboard;
        editor.tool = session.tool;
        editor.armed = session.armed;
        editor.sanitize_selection();
        Ok(editor)
    }

    pub fn new() -> Self {
        Self::from_project(Project::new_default("Untitled"))
    }

    pub fn from_project(project: Project) -> Self {
        Self {
            project,
            path: None,
            dirty: false,
            undo: Vec::new(),
            redo: Vec::new(),
            coalesce: None,
            coalesce_at: None,
            adjustment_control: None,
            gesture_before: None,
            gesture: None,
            gesture_feedback: None,
            selected_clips: Vec::new(),
            selected_transition: None,
            selected_media: None,
            selected_track: None,
            clipboard: Vec::new(),
            tool: Tool::Select,
            armed: None }
    }

    pub fn edit<T>(
        &mut self,
        kind: &'static str,
        f: impl FnOnce(&mut Project) -> Result<T, String>,
    ) -> Result<T, String> {
        self.commit_gesture();
        let snapshot = self.project.clone();
        // Continuous slider updates share one undo step. Discrete edits such as
        // insert, split and delete always retain their own history entry.
        let continuous = matches!(
            kind,
            "adjust" | "transition-duration" | "mix-gain" | "mix-pan"
        );
        let recent = self
            .coalesce_at
            .is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(500));
        let push = !continuous || !recent || self.coalesce != Some(kind);
        match f(&mut self.project) {
            Ok(value) => {
                if self.project == snapshot {
                    return Ok(value);
                }
                if push {
                    self.undo.push(std::sync::Arc::new(snapshot));
                    if self.undo.len() > 100 {
                        self.undo.remove(0);
                    }
                }
                self.redo.clear();
                self.coalesce = continuous.then_some(kind);
                self.coalesce_at = continuous.then(std::time::Instant::now);
                self.dirty = true;
                self.sanitize_selection();
                Ok(value)
            }
            Err(err) => {
                self.project = snapshot;
                Err(err)
            }
        }
    }

    pub fn mark_saved(&mut self) {
        self.dirty = false;
        self.coalesce = None;
        self.coalesce_at = None;
        self.adjustment_control = None;
    }

    pub fn begin_control_edit(&mut self, target: u64, field: &str) {
        let control = (target, field.to_string());
        if self.adjustment_control.as_ref() != Some(&control) {
            self.coalesce = None;
        }
        self.adjustment_control = Some(control);
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty() || self.gesture_before.is_some()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo(&mut self) {
        self.cancel_gesture();
        self.coalesce = None;
        let Some(previous) = self.undo.pop() else {
            return;
        };
        let playhead_time = self
            .project
            .sequence
            .frame_rate
            .frame_to_seconds(self.project.sequence.playhead);
        let zoom = self.project.sequence.zoom;
        let scroll = self.project.sequence.scroll;
        self.redo.push(std::sync::Arc::new(self.project.clone()));
        self.project = std::sync::Arc::unwrap_or_clone(previous);
        self.project.sequence.playhead = self
            .project
            .sequence
            .frame_rate
            .seconds_to_frame(playhead_time);
        self.project.sequence.zoom = zoom;
        self.project.sequence.scroll = scroll;
        self.dirty = true;
        self.sanitize_selection();
    }

    pub fn redo(&mut self) {
        self.cancel_gesture();
        self.coalesce = None;
        let Some(next) = self.redo.pop() else {
            return;
        };
        let playhead_time = self
            .project
            .sequence
            .frame_rate
            .frame_to_seconds(self.project.sequence.playhead);
        let zoom = self.project.sequence.zoom;
        let scroll = self.project.sequence.scroll;
        self.undo.push(std::sync::Arc::new(self.project.clone()));
        self.project = std::sync::Arc::unwrap_or_clone(next);
        self.project.sequence.playhead = self
            .project
            .sequence
            .frame_rate
            .seconds_to_frame(playhead_time);
        self.project.sequence.zoom = zoom;
        self.project.sequence.scroll = scroll;
        self.dirty = true;
        self.sanitize_selection();
    }

    fn sanitize_selection(&mut self) {
        self.selected_clips
            .retain(|id| self.project.clip(*id).is_some());
        if self
            .selected_transition
            .is_some_and(|id| self.project.transition(id).is_none())
        {
            self.selected_transition = None;
        }
        if self
            .selected_media
            .is_some_and(|id| self.project.media(id).is_none())
        {
            self.selected_media = None;
        }
        if self
            .selected_track
            .is_some_and(|index| index >= self.project.sequence.tracks.len())
        {
            self.selected_track = None;
        }
    }

    pub fn insert_selected(&mut self) -> Result<(), String> {
        let media = self.selected_media.ok_or("Select a clip in the bin")?;
        let at = self.project.sequence.playhead;
        let track = self.selected_track;
        self.edit("insert", move |project| {
            let items = build_placements(project, media, track)?;
            insert_placements(project, at, &items)
        })
    }

    pub fn overwrite_selected(&mut self) -> Result<(), String> {
        let media = self.selected_media.ok_or("Select a clip in the bin")?;
        let at = self.project.sequence.playhead;
        let track = self.selected_track;
        self.edit("overwrite", move |project| {
            let items = build_placements(project, media, track)?;
            overwrite_placements(project, at, &items)
        })
    }

    pub fn insert_media_at(&mut self, media: u64, at: i64, overwrite: bool) -> Result<(), String> {
        let range = self
            .project
            .media(media)
            .ok_or("Select a clip in the bin")?
            .source_range()?;
        self.insert_media_range_at(media, at, overwrite, range)
    }

    /// Commit exactly the range previewed at drag start, even if Source focus
    /// or its live marks changed before release.
    pub fn insert_media_range_at(
        &mut self,
        media: u64,
        at: i64,
        overwrite: bool,
        range: SourceRange,
    ) -> Result<(), String> {
        self.insert_media_range_with_sequence_at(media, at, overwrite, range, None)
    }

    /// Matching the first clip's format and inserting its marked range form
    /// one transaction, so failed placement and Undo also restore the format.
    pub fn insert_media_range_with_sequence_at(
        &mut self,
        media: u64,
        at: i64,
        overwrite: bool,
        range: SourceRange,
        format: Option<(u32, u32, FrameRate)>,
    ) -> Result<(), String> {
        let track = self.selected_track;
        let at = at.max(0);
        self.edit(
            if overwrite { "overwrite" } else { "insert" },
            move |project| {
                let old_rate = project.sequence.frame_rate;
                if let Some((width, height, rate)) = format {
                    project.set_resolution(width, height);
                    project.set_frame_rate(rate);
                }
                let at = if old_rate == project.sequence.frame_rate {
                    at
                } else {
                    project
                        .sequence
                        .frame_rate
                        .seconds_to_frame(old_rate.frame_to_seconds(at))
                };
                let items = build_placements_in_range(project, media, track, range)?;
                if overwrite {
                    overwrite_placements(project, at, &items)?;
                } else {
                    insert_placements(project, at, &items)?;
                }
                project.sequence.playhead = at;
                Ok(())
            },
        )
    }

    pub fn set_source_marks(
        &mut self,
        media: u64,
        source_in: Option<SourceTime>,
        source_out: Option<SourceTime>,
    ) -> Result<(), String> {
        self.edit("source-mark", move |project| {
            project
                .media
                .iter_mut()
                .find(|item| item.id == media)
                .ok_or("Select media in the Project panel")?
                .set_source_marks(source_in, source_out)
        })
    }

    pub fn mark_source_in(&mut self, media: u64, boundary: SourceTime) -> Result<(), String> {
        self.edit("source-in", move |project| {
            project
                .media
                .iter_mut()
                .find(|item| item.id == media)
                .ok_or("Select media in the Project panel")?
                .mark_source_in(boundary)
        })
    }

    /// `boundary` is exclusive; use Media::source_out_after_frame(cursor).
    pub fn mark_source_out(&mut self, media: u64, boundary: SourceTime) -> Result<(), String> {
        self.edit("source-out", move |project| {
            project
                .media
                .iter_mut()
                .find(|item| item.id == media)
                .ok_or("Select media in the Project panel")?
                .mark_source_out(boundary)
        })
    }

    pub fn clear_source_marks(&mut self, media: u64) -> Result<(), String> {
        self.edit("source-clear", move |project| {
            project
                .media
                .iter_mut()
                .find(|item| item.id == media)
                .ok_or("Select media in the Project panel")?
                .clear_source_marks();
            Ok(())
        })
    }

    pub fn split_playhead(&mut self) -> Result<(), String> {
        let frame = self.project.sequence.playhead;
        let ids = if self.selected_clips.is_empty() {
            Vec::new()
        } else {
            expand_links(&self.project, &self.selected_clips)
        };
        self.edit("split", move |project| split_at(project, &ids, frame))
    }

    pub fn razor_plan(&self, id: u64, frame: i64, threshold: i64) -> RazorPlan {
        let frame = if self.project.sequence.snap && threshold >= 0 {
            frame.saturating_add(snap_delta(&[frame], &snap_targets(&self.project.sequence, &[]), threshold))
        } else { frame };
        let ids = expand_links(&self.project, &[id]);
        let clips: Vec<_> = ids.iter().copied().filter(|id| self.project.clip(*id)
            .is_some_and(|clip| frame > clip.timeline_start && frame < clip.end())).collect();
        let reason = validate_unlocked(&self.project, &ids).err().or_else(|| {
            if clips.is_empty() { Some("Cut must be inside a clip, not at its boundary".into()) } else { None }
        });
        RazorPlan { frame, clips, reason }
    }

    pub fn split_clip_at(&mut self, id: u64, frame: i64) -> Result<(), String> {
        // A negative threshold preserves callers' exact frame while sharing
        // every eligibility check with the hover plan.
        let plan = self.razor_plan(id, frame, -1);
        if let Some(reason) = plan.reason { return Err(reason); }
        let ids = expand_links(&self.project, &[id]);
        self.edit("split", move |project| split_at(project, &ids, frame))
    }

    pub fn ripple_delete_selected(&mut self) -> Result<(), String> {
        if self.selected_clips.is_empty() {
            return Err("Select a clip to ripple delete".into());
        }
        let ids = self.selected_clips.clone();
        self.edit("delete", move |project| ripple_delete(project, &ids))
    }

    pub fn lift_delete_selected(&mut self) -> Result<(), String> {
        if self.selected_clips.is_empty() {
            return Err("Select a clip to lift".into());
        }
        let ids = self.selected_clips.clone();
        self.edit("delete", move |project| lift_delete(project, &ids))
    }

    pub fn copy(&mut self) {
        if self.selected_clips.is_empty() {
            return;
        }
        self.clipboard = copy_selection(&self.project, &self.selected_clips);
    }

    pub fn paste(&mut self) -> Result<(), String> {
        let copies = self.clipboard.clone();
        let at = self.project.sequence.playhead;
        self.edit("paste", move |project| paste_copies(project, &copies, at))
    }

    pub fn ripple_prev(&mut self) -> Result<(), String> {
        let frame = self.project.sequence.playhead;
        self.edit("trim", move |project| ripple_trim_previous(project, frame))
    }

    pub fn ripple_next(&mut self) -> Result<(), String> {
        let frame = self.project.sequence.playhead;
        self.edit("trim", move |project| ripple_trim_next(project, frame))
    }

    pub fn apply_armed(&mut self, clip_id: u64) -> Result<(), String> {
        let Some(kind) = self.armed else {
            return Ok(());
        };
        self.edit("transition", move |project| {
            let id = add_transition(project, kind, clip_id)?;
            Ok(id)
        })
        .map(|id| {
            self.selected_transition = Some(id);
            self.armed = None;
        })
    }

    pub fn apply_kind_to_selection(&mut self, kind: TransitionKind) -> Result<(), String> {
        let clip_id = self
            .selected_clips
            .first()
            .copied()
            .ok_or("Select a clip")?;
        self.edit("transition", move |project| {
            add_transition(project, kind, clip_id).map(|_| ())
        })
    }

    pub fn set_selected_transition_duration(&mut self, frames: i64) {
        let Some(id) = self.selected_transition else {
            return;
        };
        self.begin_control_edit(id, "transition-duration");
        let _ = self.edit("transition-duration", move |project| {
            set_transition_duration(project, id, frames);
            Ok(())
        });
    }

    pub fn delete_selected_transition(&mut self) {
        let Some(id) = self.selected_transition else {
            return;
        };
        let _ = self.edit("transition", move |project| {
            remove_transition(project, id);
            Ok(())
        });
        self.selected_transition = None;
    }

    pub fn select_only(&mut self, id: u64, links: bool) {
        self.selected_transition = None;
        self.selected_clips = if links {
            expand_links(&self.project, &[id])
        } else {
            vec![id]
        };
    }

    pub fn toggle_select(&mut self, id: u64, links: bool) {
        let ids = if links {
            expand_links(&self.project, &[id])
        } else {
            vec![id]
        };
        if ids.iter().any(|id| self.selected_clips.contains(id)) {
            self.selected_clips.retain(|have| !ids.contains(have));
        } else {
            self.selected_clips.extend(ids);
        }
    }

    pub fn select_all(&mut self) {
        self.selected_clips = self
            .project
            .sequence
            .tracks
            .iter()
            .filter(|track| !track.locked)
            .flat_map(|track| track.clips.iter().map(|clip| clip.id))
            .collect();
    }

    pub fn clear_selection(&mut self) {
        self.selected_clips.clear();
        self.selected_transition = None;
        self.armed = None;
    }

    pub fn primary_clip(&self) -> Option<u64> {
        self.selected_clips.first().copied()
    }

    pub fn unlink_selected(&mut self) -> Result<(), String> {
        let ids = self.selected_clips.clone();
        self.coalesce = None;
        self.edit("unlink", move |project| unlink_clips(project, &ids))
    }

    pub fn link_selected(&mut self) -> Result<(), String> {
        let ids = self.selected_clips.clone();
        self.coalesce = None;
        self.edit("link", move |project| link_clips(project, &ids))
    }

    pub fn nudge_selected(
        &mut self,
        delta_frames: i64,
        track_delta: i32,
        links: bool,
    ) -> Result<(), String> {
        if delta_frames == 0 && track_delta == 0 {
            return Ok(());
        }
        let ids = if links {
            expand_links(&self.project, &self.selected_clips)
        } else {
            self.selected_clips.clone()
        };
        let anchor_id = ids.first().copied().ok_or("Select clips to move")?;
        let anchor_track = self
            .project
            .track_index_of_clip(anchor_id)
            .ok_or("A selected clip is missing")?;
        let kind = self.project.sequence.tracks[anchor_track].kind;
        let lanes: Vec<usize> = self
            .project
            .sequence
            .tracks
            .iter()
            .enumerate()
            .filter_map(|(index, track)| (track.kind == kind).then_some(index))
            .collect();
        let rank = lanes
            .iter()
            .position(|track| *track == anchor_track)
            .ok_or("Original track is missing")?;
        let lane_delta = match kind {
            TrackKind::Video => -(track_delta as i64),
            TrackKind::Audio => track_delta as i64 };
        let rank = rank as i64 + lane_delta;
        let target = usize::try_from(rank)
            .ok()
            .and_then(|rank| lanes.get(rank))
            .copied()
            .ok_or("No compatible track in that direction. Add a track first")?;
        self.coalesce = None;
        self.edit("nudge", move |project| {
            move_clips_to_track(project, &ids, anchor_id, delta_frames, Some(target))
        })
    }

    pub fn select_adjacent_clip(&mut self, direction: i32, links: bool) -> Result<(), String> {
        if direction == 0 {
            return Ok(());
        }
        let primary = self.primary_clip();
        let track_index = primary
            .and_then(|id| self.project.track_index_of_clip(id))
            .or(self.selected_track)
            .or_else(|| {
                self.project
                    .sequence
                    .tracks
                    .iter()
                    .position(|track| !track.clips.is_empty())
            })
            .ok_or("The timeline has no clips")?;
        let track = self
            .project
            .sequence
            .tracks
            .get(track_index)
            .ok_or("Selected track is missing")?;
        let mut clips: Vec<_> = track.clips.iter().collect();
        clips.sort_unstable_by_key(|clip| (clip.timeline_start, clip.id));
        let candidate = if let Some(position) =
            primary.and_then(|id| clips.iter().position(|clip| clip.id == id))
        {
            let ignore = if links {
                expand_links(&self.project, &[clips[position].id])
            } else {
                vec![clips[position].id]
            };
            if direction > 0 {
                clips
                    .iter()
                    .skip(position + 1)
                    .find(|clip| !ignore.contains(&clip.id))
            } else {
                clips[..position]
                    .iter()
                    .rev()
                    .find(|clip| !ignore.contains(&clip.id))
            }
        } else if direction > 0 {
            clips
                .iter()
                .find(|clip| clip.end() > self.project.sequence.playhead)
                .or_else(|| clips.last())
        } else {
            clips
                .iter()
                .rev()
                .find(|clip| clip.timeline_start < self.project.sequence.playhead)
                .or_else(|| clips.first())
        };
        let id = candidate.map(|clip| clip.id).ok_or(if direction > 0 {
            "No next clip on this track"
        } else {
            "No previous clip on this track"
        })?;
        self.select_only(id, links);
        self.selected_track = Some(track_index);
        Ok(())
    }

    pub fn edit_target_reason(&self, id: u64, links: bool) -> Option<String> {
        let ids = if links { expand_links(&self.project, &[id]) } else { vec![id] };
        validate_unlocked(&self.project, &ids).err()
    }

    pub fn move_target_reason(&self, id: u64, links: bool) -> Option<String> {
        let selected = if self.selected_clips.contains(&id) { self.selected_clips.clone() } else { vec![id] };
        let ids = if links { expand_links(&self.project, &selected) } else { selected };
        validate_unlocked(&self.project, &ids).err()
    }

    pub fn begin_move(&mut self, grab_frame: i64, links: bool) -> Result<(), String> {
        self.commit_gesture();
        let mut ids = if links {
            expand_links(&self.project, &self.selected_clips)
        } else {
            self.selected_clips.clone()
        };
        let mut seen = std::collections::HashSet::new();
        ids.retain(|id| seen.insert(*id));
        let anchor_id = ids.first().copied().ok_or("Select clips to move")?;
        validate_unlocked(&self.project, &ids)?;
        self.selected_clips = ids.clone();
        self.gesture_before = Some(self.project.clone());
        self.gesture = Some(Gesture::Move {
            ids,
            anchor_id,
            grab_frame });
        self.coalesce = None;
        Ok(())
    }

    pub fn update_move(
        &mut self,
        frame: i64,
        threshold: i64,
        target_track: Option<usize>,
    ) -> Result<(), String> {
        let Some(Gesture::Move {
            ids,
            anchor_id,
            grab_frame }) = self.gesture.clone()
        else {
            return Err("No clip move is active".into());
        };
        let before = self
            .gesture_before
            .as_ref()
            .ok_or("Missing move snapshot")?;
        let actual = self.project.clip(anchor_id).ok_or("The moved clip is missing")?;
        let original = before.clip(anchor_id).ok_or("The grabbed clip is missing")?;
        let overflow_feedback = GestureFeedback { clip: anchor_id, edge: None,
            delta: actual.timeline_start - original.timeline_start, duration: actual.duration,
            snap: None, reason: Some("Move exceeds the timeline range".into()) };
        let Some(delta) = frame.checked_sub(grab_frame) else {
            self.gesture_feedback = Some(overflow_feedback);
            return Err("Move exceeds the timeline range".into());
        };
        let mut next = before.clone();
        next.sequence.playhead = self.project.sequence.playhead;
        next.sequence.zoom = self.project.sequence.zoom;
        next.sequence.scroll = self.project.sequence.scroll;
        next.sequence.snap = self.project.sequence.snap;
        let mut edges = Vec::new();
        for id in &ids {
            let clip = before.clip(*id).ok_or("A selected clip is missing")?;
            edges.push(clip.timeline_start.saturating_add(delta));
            edges.push(clip.end().saturating_add(delta));
        }
        let targets = snap_targets(&next.sequence, &ids);
        let snap = if next.sequence.snap {
            snap_delta(&edges, &targets, threshold)
        } else {
            0
        };
        let Some(delta) = delta.checked_add(snap) else {
            self.gesture_feedback = Some(overflow_feedback);
            return Err("Move exceeds the timeline range".into());
        };
        if let Err(reason) = move_clips_to_track(&mut next, &ids, anchor_id, delta, target_track) {
            let actual = self.project.clip(anchor_id).unwrap();
            self.gesture_feedback = Some(GestureFeedback { clip: anchor_id, edge: None,
                delta: actual.timeline_start - before.clip(anchor_id).unwrap().timeline_start,
                duration: actual.duration, snap: None, reason: Some(reason.clone()) });
            return Err(reason);
        }
        let actual = next.clip(anchor_id).ok_or("The moved clip is missing")?;
        let applied_delta = actual.timeline_start - before.clip(anchor_id).unwrap().timeline_start;
        let applied_snap = if next.sequence.snap && applied_delta == delta {
            edges.iter().map(|edge| edge.saturating_add(snap)).find(|edge| targets.contains(edge))
        } else { None };
        self.gesture_feedback = Some(GestureFeedback { clip: anchor_id, edge: None, delta: applied_delta,
            duration: actual.duration, snap: applied_snap,
            reason: (applied_delta != delta).then(|| "Move limited by timeline start".into()) });
        self.project = next;
        Ok(())
    }

    pub fn begin_trim(&mut self, id: u64, edge: Edge, ripple: bool, links: bool) {
        self.commit_gesture();
        let ids = if links {
            expand_links(&self.project, &[id])
        } else {
            vec![id]
        };
        if let Err(reason) = validate_unlocked(&self.project, &ids) {
            self.gesture_feedback = Some(GestureFeedback { clip: id, edge: Some(edge), delta: 0,
                duration: self.project.clip(id).map_or(0, |clip| clip.duration), snap: None, reason: Some(reason) });
            return;
        }
        self.gesture_before = Some(self.project.clone());
        self.gesture = Some(Gesture::Trim {
            id,
            ids,
            edge,
            ripple });
        self.coalesce = None;
    }

    pub fn update_trim(&mut self, frame: i64, threshold: i64) {
        let Some(Gesture::Trim {
            id,
            ids,
            edge,
            ripple }) = self.gesture.clone()
        else {
            return;
        };
        let Some(before) = self.gesture_before.as_ref() else {
            return;
        };
        let Some(clip) = before.clip(id) else {
            return;
        };
        let edge_frame = match edge {
            Edge::In => clip.timeline_start,
            Edge::Out => clip.end() };
        // Rebuild every pointer update from the press snapshot. Repeated ripple
        // updates must not keep moving the following clips or consume handles.
        let mut next = before.clone();
        next.sequence.playhead = self.project.sequence.playhead;
        next.sequence.zoom = self.project.sequence.zoom;
        next.sequence.scroll = self.project.sequence.scroll;
        next.sequence.snap = self.project.sequence.snap;
        let targets = snap_targets(&next.sequence, &ids);
        let snap = if next.sequence.snap {
            snap_delta(&[frame], &targets, threshold)
        } else {
            0
        };
        let result = frame.checked_add(snap).and_then(|snapped| snapped.checked_sub(edge_frame).map(|delta| (snapped, delta)))
            .ok_or_else(|| "Trim exceeds the timeline range".to_string())
            .and_then(|(snapped, delta)| trim_group_checked(&mut next, &ids, edge, delta, ripple).map(|_| (snapped, delta)));
        match result {
            Ok((snapped, delta)) => {
                let actual = next.clip(id).unwrap();
                let actual_edge = match edge { Edge::In => actual.timeline_start, Edge::Out => actual.end() };
                self.gesture_feedback = Some(GestureFeedback { clip: id, edge: Some(edge), delta, duration: actual.duration,
                    snap: (next.sequence.snap && actual_edge == snapped && targets.contains(&snapped)).then_some(snapped), reason: None });
                self.project = next;
            }
            Err(reason) => {
                let actual = self.project.clip(id).unwrap();
                let delta = match edge { Edge::In => actual.source_in - clip.source_in, Edge::Out => actual.duration - clip.duration };
                self.gesture_feedback = Some(GestureFeedback { clip: id, edge: Some(edge), delta,
                    duration: actual.duration, snap: None, reason: Some(reason) });
            }
        }
    }

    pub fn commit_gesture(&mut self) {
        self.gesture_feedback = None;
        self.gesture = None;
        if let Some(mut before) = self.gesture_before.take() {
            before.sequence.playhead = self.project.sequence.playhead;
            before.sequence.zoom = self.project.sequence.zoom;
            before.sequence.scroll = self.project.sequence.scroll;
            before.sequence.snap = self.project.sequence.snap;
            if before != self.project {
                self.undo.push(std::sync::Arc::new(before));
                if self.undo.len() > 100 {
                    self.undo.remove(0);
                }
                self.redo.clear();
                self.dirty = true;
                self.coalesce = None;
            }
        }
        self.sanitize_selection();
    }

    pub fn cancel_gesture(&mut self) {
        self.gesture_feedback = None;
        if let Some(mut before) = self.gesture_before.take() {
            before.sequence.playhead = self.project.sequence.playhead;
            before.sequence.zoom = self.project.sequence.zoom;
            before.sequence.scroll = self.project.sequence.scroll;
            before.sequence.snap = self.project.sequence.snap;
            self.project = before;
        }
        self.gesture = None;
    }

    pub fn gesture_active(&self) -> bool {
        self.gesture.is_some()
    }

    pub fn set_profile(&mut self, profile: ShortcutProfile) {
        self.project.shortcut_profile = profile;
        self.dirty = true;
    }

    pub fn mark_in(&mut self) {
        self.project.sequence.in_point = Some(self.project.sequence.playhead.max(0));
        self.dirty = true;
    }

    pub fn mark_out(&mut self) {
        self.project.sequence.out_point = Some(self.project.sequence.playhead.max(0));
        self.dirty = true;
    }

    pub fn clear_in_out(&mut self) {
        self.project.sequence.in_point = None;
        self.project.sequence.out_point = None;
        self.dirty = true;
    }

    pub fn toggle_snap(&mut self) {
        self.project.sequence.snap = !self.project.sequence.snap;
    }

    pub fn toggle_mute(&mut self, index: usize) {
        let _ = self.edit("track", move |project| {
            if let Some(track) = project.sequence.tracks.get_mut(index) {
                track.muted = !track.muted;
            }
            Ok(())
        });
    }

    pub fn toggle_lock(&mut self, index: usize) {
        let _ = self.edit("track", move |project| {
            if let Some(track) = project.sequence.tracks.get_mut(index) {
                track.locked = !track.locked;
            }
            Ok(())
        });
    }

    pub fn mute_selected_track(&mut self) {
        let index = self.selected_track.or_else(|| {
            self.primary_clip()
                .and_then(|id| self.project.track_index_of_clip(id))
        });
        if let Some(index) = index {
            self.toggle_mute(index);
        }
    }

    pub fn add_track(&mut self, kind: TrackKind) {
        let _ = self.edit("track", move |project| {
            project.add_track(kind);
            Ok(())
        });
    }

    pub fn adjust(&mut self, field: &str, value: f32) {
        if !value.is_finite() { return; }
        let Some(id) = self.primary_clip() else {
            return;
        };
        let audio = self.linked_audio(id);
        let field = field.to_string();
        self.begin_control_edit(
            if field == "volume" {
                audio.unwrap_or(id)
            } else {
                id
            },
            &field,
        );
        let _ = self.edit("adjust", move |project| {
            let target = if field == "volume" {
                audio.unwrap_or(id)
            } else {
                id
            };
            let Some(clip) = project.clip_mut(target) else {
                return Ok(());
            };
            match field.as_str() {
                "pos-x" => clip.pos_x = value,
                "pos-y" => clip.pos_y = value,
                "scale" => clip.scale = value.max(0.01),
                "rotation" => clip.transform.rotation = value.clamp(-36000.0,36000.0),
                "scale-x" => clip.transform.scale_x = value.clamp(0.01,10.0),
                "scale-y" => clip.transform.scale_y = value.clamp(0.01,10.0),
                "anchor-x" => clip.transform.anchor_x = value.clamp(0.0,1.0),
                "anchor-y" => clip.transform.anchor_y = value.clamp(0.0,1.0),
                "crop-left" => clip.transform.crop_left = value.clamp(0.0,1.0),
                "crop-right" => clip.transform.crop_right = value.clamp(0.0,1.0),
                "crop-top" => clip.transform.crop_top = value.clamp(0.0,1.0),
                "crop-bottom" => clip.transform.crop_bottom = value.clamp(0.0,1.0),
                "flip-x" => clip.transform.flip_x = value >= 0.5,
                "flip-y" => clip.transform.flip_y = value >= 0.5,
                "opacity" => clip.opacity = value.clamp(0.0, 1.0),
                "volume" => clip.volume = value.clamp(0.0, 2.0),
                "exposure" => clip.grade.exposure = value,
                "contrast" => clip.grade.contrast = value,
                "highlights" => clip.grade.highlights = value,
                "shadows" => clip.grade.shadows = value,
                "temperature" => clip.grade.temperature = value,
                "tint" => clip.grade.tint = value,
                "saturation" => clip.grade.saturation = value,
                "lift-m" => clip.grade.lift.master = value,
                "lift-r" => clip.grade.lift.r = value,
                "lift-g" => clip.grade.lift.g = value,
                "lift-b" => clip.grade.lift.b = value,
                "gamma-m" => clip.grade.gamma.master = value,
                "gamma-r" => clip.grade.gamma.r = value,
                "gamma-g" => clip.grade.gamma.g = value,
                "gamma-b" => clip.grade.gamma.b = value,
                "gain-m" => clip.grade.gain.master = value,
                "gain-r" => clip.grade.gain.r = value,
                "gain-g" => clip.grade.gain.g = value,
                "gain-b" => clip.grade.gain.b = value,
                _ => {}
            }
            Ok(())
        });
    }

    pub fn linked_audio(&self, id: u64) -> Option<u64> {
        let link = self.project.clip(id)?.link?;
        self.project
            .sequence
            .tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Audio)
            .flat_map(|track| track.clips.iter())
            .find(|clip| clip.link == Some(link) && clip.id != id)
            .map(|clip| clip.id)
    }

    pub fn default_transition_frames(&self) -> i64 {
        default_transition_duration(&self.project)
    }

    pub fn step(&mut self, frames: i64) {
        let next = (self.project.sequence.playhead + frames).max(0);
        self.project.sequence.playhead = next;
    }

    pub fn jump_edit(&mut self, forward: bool) {
        let mut points = vec![0, self.project.sequence.duration_frames()];
        for track in &self.project.sequence.tracks {
            for clip in &track.clips {
                points.push(clip.timeline_start);
                points.push(clip.end());
            }
        }
        points.sort_unstable();
        points.dedup();
        let playhead = self.project.sequence.playhead;
        let next = if forward {
            points.into_iter().find(|frame| *frame > playhead)
        } else {
            points.into_iter().rev().find(|frame| *frame < playhead)
        };
        if let Some(frame) = next {
            self.project.sequence.playhead = frame.max(0);
        }
    }

    pub fn zoom_by(&mut self, factor: f64) {
        self.project.sequence.zoom = (self.project.sequence.zoom * factor).clamp(0.25, 64.0);
    }

    pub fn zoom_fit(&mut self, view_px: f32) {
        let frames = self.project.sequence.duration_frames().max(24) as f64;
        let px = (view_px as f64 / frames).clamp(0.25, 64.0);
        self.project.sequence.zoom = px;
        self.project.sequence.scroll = 0.0;
    }
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

