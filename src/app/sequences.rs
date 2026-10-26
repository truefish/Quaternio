use super::*;
pub(super) fn sync(ui: &AppWindow, s: &AppState) {
    let ids = s.editor.project.sequence_ids();
    ui.set_sequence_index(
        ids.iter()
            .position(|id| id == &s.editor.project.active_sequence)
            .unwrap_or(0) as i32,
    );
    ui.set_sequence_options(model(
        ids.iter()
            .map(|id| LabelItem {
                text: s
                    .editor
                    .project
                    .sequence_by_id(id)
                    .unwrap()
                    .name
                    .clone()
                    .into(),
            })
            .collect(),
    ));
    let targeting = &s.editor.project.sequence.targeting;
    for (kind, target) in [
        (TrackKind::Video, targeting.video),
        (TrackKind::Audio, targeting.audio),
    ] {
        let tracks: Vec<_> = s
            .editor
            .project
            .sequence
            .tracks
            .iter()
            .filter(|t| t.kind == kind)
            .collect();
        let mut options = vec![LabelItem {
            text: "Automatic destination".into(),
        }];
        options.extend(tracks.iter().map(|t| LabelItem {
            text: t.name.clone().into(),
        }));
        let index = target
            .and_then(|id| tracks.iter().position(|t| t.id == id))
            .map_or(0, |i| i + 1) as i32;
        if kind == TrackKind::Video {
            ui.set_target_video_options(model(options));
            ui.set_target_video(index);
        } else {
            ui.set_target_audio_options(model(options));
            ui.set_target_audio(index);
        }
    }
    ui.set_patch_video(targeting.patch_video);
    ui.set_patch_audio(targeting.patch_audio);
}
pub(super) fn handle(
    ui: &AppWindow,
    state: &Rc<std::cell::RefCell<AppState>>,
    command: &str,
) -> bool {
    if !command.starts_with("sequence-") {
        return false;
    }
    stop_play(state);
    let result = (|| -> Result<(), String> {
        let mut s = state.borrow_mut();
        if command != "sequence-switch"
            && !command.starts_with("sequence-target")
            && !command.starts_with("sequence-patch")
            && collab::readonly(&s)
        {
            return Err("Editing permission is required".into());
        }
        s.playing = false;
        match command {
            "sequence-switch" => {
                let ids = s.editor.project.sequence_ids();
                let id = ids
                    .get(ui.get_sequence_index().max(0) as usize)
                    .ok_or("Sequence missing")?;
                s.editor.project.switch_sequence(id)?;
                s.editor.selected_clips.clear();
                s.editor.selected_transition = None;
                s.editor.selected_track = None;
            }
            "sequence-target-video" | "sequence-target-audio" => {
                let video = command == "sequence-target-video";
                let kind = if video {
                    TrackKind::Video
                } else {
                    TrackKind::Audio
                };
                let index = if video {
                    ui.get_target_video()
                } else {
                    ui.get_target_audio()
                };
                let target = if index == 0 {
                    None
                } else {
                    Some(
                        s.editor
                            .project
                            .sequence
                            .tracks
                            .iter()
                            .filter(|t| t.kind == kind)
                            .nth(index.saturating_sub(1) as usize)
                            .ok_or("Destination missing")?
                            .id,
                    )
                };
                if video {
                    s.editor.project.sequence.targeting.video = target;
                } else {
                    s.editor.project.sequence.targeting.audio = target;
                }
            }
            "sequence-patch" => {
                s.editor.project.sequence.targeting.patch_video = ui.get_patch_video();
                s.editor.project.sequence.targeting.patch_audio = ui.get_patch_audio();
            }
            "sequence-sync-lock" => {
                let index = s.editor.selected_track.ok_or("Select a track header")?;
                s.editor.edit("sync-lock", |p| {
                    let t = p.sequence.tracks.get_mut(index).ok_or("Track missing")?;
                    t.sync_lock = !t.sync_lock;
                    Ok(())
                })?;
            }
            "sequence-new" | "sequence-copy" => {
                s.editor.edit("new-sequence", |p| {
                    p.new_sequence(command == "sequence-copy")
                })?;
            }
            "sequence-delete" => {
                s.editor.edit("delete-sequence", |p| {
                    p.delete_sequence(&p.active_sequence.clone())
                })?;
            }
            "sequence-nest" => {
                let ids = s.editor.project.sequence_ids();
                let target = ids
                    .get(ui.get_sequence_source().max(0) as usize)
                    .ok_or("Choose a sequence")?
                    .clone();
                let start = s.editor.project.sequence.playhead;
                s.editor.edit("nest-sequence", |p| {
                    if target == p.active_sequence {
                        return Err("Choose another sequence to nest".into());
                    }
                    let source = p.sequence_by_id(&target).ok_or("Sequence missing")?;
                    let name = source.name.clone();
                    let duration = p
                        .sequence
                        .frame_rate
                        .seconds_to_frame(
                            source.frame_rate.frame_to_seconds(source.duration_frames()),
                        )
                        .max(1);
                    let link = p.alloc();
                    for kind in [TrackKind::Video, TrackKind::Audio] {
                        let t = p.add_track(kind);
                        let id = p.alloc();
                        p.sequence.tracks[t].clips.push(Clip {
                            creative: crate::project::creative::Creative {
                                sequence: Some(target.clone()),
                                ..Default::default()
                            },
                            id,
                            media_id: 0,
                            name: name.clone(),
                            timeline_start: start,
                            duration,
                            source_in: 0,
                            source_time_offset: 0.,
                            link: Some(link),
                            volume: 1.,
                            pos_x: 0.,
                            pos_y: 0.,
                            scale: 1.,
                            opacity: 1.,
                            transform: Default::default(),
                            grade: Default::default(),
                        });
                    }
                    p.validate_sequences()
                })?;
            }
            "sequence-adjustment" => {
                let start = s.editor.project.sequence.playhead;
                let duration = (s.editor.project.sequence.duration_frames() - start)
                    .max(s.editor.project.sequence.frame_rate.seconds_to_frame(5.));
                s.editor.edit("adjustment", |p| {
                    let t = p.add_track(TrackKind::Video);
                    let id = p.alloc();
                    p.sequence.tracks[t].name = "Adjustments".into();
                    p.sequence.tracks[t].clips.push(Clip {
                        creative: crate::project::creative::Creative {
                            adjustment: true,
                            ..Default::default()
                        },
                        id,
                        media_id: 0,
                        name: "Adjustment".into(),
                        timeline_start: start,
                        duration,
                        source_in: 0,
                        source_time_offset: 0.,
                        link: None,
                        volume: 1.,
                        pos_x: 0.,
                        pos_y: 0.,
                        scale: 1.,
                        opacity: 1.,
                        transform: Default::default(),
                        grade: Default::default(),
                    });
                    Ok(())
                })?;
            }
            _ => return Err("Unknown sequence action".into()),
        }
        s.editor.project.validate_sequences()?;
        s.status = "Sequence ready".into();
        Ok(())
    })();
    if let Err(e) = result {
        set_status(state, e);
    } else {
        note(state);
    }
    full_sync(ui, state);
    true
}
