use super::*;

pub(super) fn note_audio(state: &Rc<std::cell::RefCell<AppState>>) {
    let mut state = state.borrow_mut();
    // UI/export state still changes; Program keeps its decoder and picture.
    state.edit_gen = state.edit_gen.wrapping_add(1);
    if state.playing && (state.speed - 1.0).abs() < 0.01 {
        state.audio.update_mix(state.editor.project.clone());
    }
}

fn audio_only_delta(before: &Project, after: &Project) -> bool {
    let normalize = |project: &Project| {
        let mut picture = project.clone();
        picture.next_id = 0;
        picture.sequence.playhead = 0;
        for track in &mut picture.sequence.tracks {
            track.gain = 1.0;
            track.pan = 0.0;
            track.solo = false;
            if track.kind == TrackKind::Audio { track.muted = false; }
            for clip in &mut track.clips { clip.volume = 1.0; }
        }
        picture.sequence.transitions.retain(|transition| !transition.kind.is_audio());
        picture
    };
    normalize(before) == normalize(after)
}

pub(super) fn history(state: &Rc<std::cell::RefCell<AppState>>, redo: bool) {
    let audio_only = apply_history(&mut state.borrow_mut().editor, redo);
    if audio_only { note_audio(state); } else { note(state); }
}

fn apply_history(editor: &mut timeline::Editor, redo: bool) -> bool {
    let before = editor.project.clone();
    if redo { editor.redo(); } else { editor.undo(); }
    let audio_only = audio_only_delta(&before, &editor.project);
    if audio_only {
        // Mixer history must not jump back to the position at which a
        // slider was first touched while the transport kept advancing.
        editor.project.sequence.playhead = before.sequence.playhead;
    }
    audio_only
}

pub(super) fn toggle_solo(editor: &mut timeline::Editor, index: usize) {
    let _ = editor.edit("track-solo", |project| {
        if let Some(track) = project.sequence.tracks.get_mut(index).filter(|track| track.kind == TrackKind::Audio) {
            track.solo = !track.solo;
        }
        Ok(())
    });
}

pub(super) fn sync_controls(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let current = state.borrow();
    let tracks: Vec<_> = current.editor.project.sequence.tracks.iter().enumerate()
        .filter(|(_, track)| track.kind == TrackKind::Audio)
        .map(|(index, track)| MixerTrack {
            index: index as i32, name: track.name.clone().into(), gain: track.gain, pan: track.pan,
            read_gain: format!("{}%", (track.gain * 100.0).round() as i32).into(),
            read_pan: if track.pan.abs() < 0.005 { "Center".into() } else {
                format!("{}{}", if track.pan < 0.0 { "L" } else { "R" }, (track.pan.abs() * 100.0).round() as i32).into()
            },
            muted: track.muted, solo: track.solo }).collect();
    let existing = ui.get_mixer_tracks();
    if let Some(rows) = existing.as_any().downcast_ref::<VecModel<MixerTrack>>()
        .filter(|rows| rows.row_count() == tracks.len()
            && tracks.iter().enumerate().all(|(index, track)| rows.row_data(index).is_some_and(|old| old.index == track.index))) {
        // Keep live slider instances and keyboard focus while values change.
        for (index, track) in tracks.into_iter().enumerate() { rows.set_row_data(index, track); }
    } else {
        ui.set_mixer_tracks(model(tracks));
    }
    let dirty = if current.editor.dirty { "*" } else { "" };
    ui.set_window_title(format!("Quaternio — {}{dirty}", current.editor.project.name).into());
    ui.set_can_undo(current.editor.can_undo());
    ui.set_can_redo(current.editor.can_redo());
    ui.set_save_label(if current.editor.path.is_some() && !current.editor.dirty { "Saved" } else { "Unsaved" }.into());
}

pub(super) fn sync_meters(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let snapshot = state.borrow().audio.meter_snapshot();
    let db = |amplitude: f32| {
        if amplitude > 0.001 { 20.0 * amplitude.log10() } else { -60.0 }
    };
    ui.set_meter_left(((db(snapshot.levels[0]) + 60.0) / 60.0).clamp(0.0, 1.0));
    ui.set_meter_right(((db(snapshot.levels[1]) + 60.0) / 60.0).clamp(0.0, 1.0));
    ui.set_meter_hold_left(((db(snapshot.held[0]) + 60.0) / 60.0).clamp(0.0, 1.0));
    ui.set_meter_hold_right(((db(snapshot.held[1]) + 60.0) / 60.0).clamp(0.0, 1.0));
    ui.set_meter_clip_left(snapshot.clipped[0]);
    ui.set_meter_clip_right(snapshot.clipped[1]);
    let read = |amplitude| if amplitude <= 0.001 { "−∞".into() } else { format!("{:.1}", db(amplitude)) };
    ui.set_meter_read_left(read(snapshot.held[0]).into());
    ui.set_meter_read_right(read(snapshot.held[1]).into());
}

