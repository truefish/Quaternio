use super::*;
use crate::project::{
    captions,
    creative::{self, Creative, Interpolation, Keyframe, TextContent, TextStyle},
};
pub(super) fn sync(ui: &AppWindow, state: &AppState) {
    ui.set_precision_tool(match state.editor.tool {
        Tool::Slip => 1,
        Tool::Roll => 2,
        Tool::Slide => 3,
        _ => 0,
    });
    let id = state.editor.primary_clip();
    let clip = id.and_then(|id| state.editor.project.clip(id));
    ui.set_creator_readonly(collab::readonly(state));
    ui.set_creator_audio_busy(state.creative_job.is_some());
    ui.set_creator_has_clip(clip.is_some());
    ui.set_creator_is_text(clip.is_some_and(|c| c.creative.text.is_some()));
    let audio = id
        .and_then(|id| state.editor.linked_audio(id).or(Some(id)))
        .and_then(|id| state.editor.project.clip(id));
    let track = audio
        .and_then(|c| state.editor.project.track_index_of_clip(c.id))
        .and_then(|i| state.editor.project.sequence.tracks.get(i));
    ui.set_creator_audio_info(
        format!(
            "Ducking: {} · {} · cleanup: {}",
            if state.editor.project.sequence.audio.ducking {
                "on"
            } else {
                "off"
            },
            track
                .map(|t| format!("{}: {:?}", t.name, t.role))
                .unwrap_or("select an audio clip".into()),
            if audio.is_some_and(|c| c.creative.dialogue) {
                "on"
            } else {
                "off"
            }
        )
        .into(),
    );
    let token = id.unwrap_or(0) as i32;
    let model_token = format!(
        "{}:{}:{token}:{}",
        state.editor.project.document_id,
        state.editor.project.active_sequence,
        serde_json::to_string(&clip.and_then(|c| c.creative.text.as_ref())).unwrap()
    );
    if ui.get_creator_selected() == token && ui.get_creator_text_dirty() && ui.get_creator_model_token() != model_token {
        ui.set_creator_draft_notice("This title changed elsewhere. Your draft is kept; Apply text or Discard draft.".into());
    }
    if ui.get_creator_selected() != token
        || (!ui.get_live_input_active() && !ui.get_creator_text_dirty() && ui.get_creator_model_token() != model_token)
    {
        ui.set_creator_text_dirty(false);
        ui.set_creator_draft_notice("".into());
        ui.set_creator_selected(token);
        ui.set_creator_model_token(model_token.into());
        if let Some(t) = clip.and_then(|c| c.creative.text.as_ref()) {
            ui.set_creator_text_size(t.size);
            ui.set_creator_text_background(t.background);
        }
        ui.set_creator_text(
            clip.and_then(|c| c.creative.text.as_ref())
                .map(|t| t.text.clone())
                .unwrap_or_default()
                .into(),
        );
    }
    let field = field(ui);
    let key_clip = if field == "volume" {
        id.and_then(|id| state.editor.linked_audio(id))
            .and_then(|id| state.editor.project.clip(id))
            .or(clip)
    } else {
        clip
    };
    let frame = state.editor.project.sequence.playhead;
    let value = key_clip.and_then(|c| creative::value(&c.evaluated(frame), field)).unwrap_or(0.);
    let (low, high, scale) = match field {
        "pos-x" | "pos-y" => (-4096.,4096.,1.), "rotation" => (-360.,360.,1.),
        "scale" => (0.01,10.,100.), "opacity" => (0.,1.,100.), "volume" => (0.,4.,100.),
        "exposure" => (-5.,5.,1.), _ => (-100.,100.,1.),
    };
    ui.set_creator_key_field(field.into()); ui.set_creator_key_value(value);
    ui.set_creator_key_min(low); ui.set_creator_key_max(high); ui.set_creator_key_scale(scale);
    let message = key_clip
        .map(|c| {
            let local = frame - c.timeline_start + c.creative.offset;
            let keys = c.creative.animation.get(field);
            format!(
                "{} keyframes · clip frame {}",
                keys.map_or(0, Vec::len),
                local
            )
        })
        .unwrap_or("Select a timeline clip".into());
    ui.set_creator_key_info(message.into());
    ui.set_creator_speed_label(
        clip.map(|c| {
            if c.creative.freeze {
                "Freeze frame".into()
            } else {
                format!(
                    "{:.0}%{}",
                    c.creative.speed.abs() * 100.,
                    if c.creative.speed < 0. {
                        " · reversed"
                    } else {
                        ""
                    }
                )
            }
        })
        .unwrap_or_default()
        .into(),
    );
}
fn field(ui: &AppWindow) -> &'static str {
    creative::PROPERTIES
        .get(ui.get_creator_property().max(0) as usize)
        .copied()
        .unwrap_or("opacity")
}
fn interpolation(ui: &AppWindow) -> Interpolation {
    match ui.get_creator_interpolation() {
        1 => Interpolation::Hold,
        2 => Interpolation::Ease,
        _ => Interpolation::Linear,
    }
}
pub(super) fn text_clip(
    project: &mut Project,
    text: String,
    style: TextStyle,
    start: i64,
    duration: i64,
) -> u64 {
    let track = project.add_track(TrackKind::Video);
    project.sequence.tracks[track].name = match style {
        TextStyle::Title => "Titles",
        TextStyle::LowerThird => "Lower thirds",
        TextStyle::Caption => "Captions",
    }
    .into();
    let id = project.alloc();
    project.sequence.tracks[track]
        .clips
        .push(make_text(id, text, style, start, duration));
    id
}
pub(super) fn make_text(
    id: u64,
    text: String,
    style: TextStyle,
    start: i64,
    duration: i64,
) -> Clip {
    Clip {
        creative: Creative {
            text: Some(TextContent {
                text,
                style,
                size: if style == TextStyle::Caption {
                    44.
                } else {
                    64.
                },
                ..Default::default()
            }),
            ..Default::default()
        },
        id,
        media_id: 0,
        name: match style {
            TextStyle::Title => "Title",
            TextStyle::LowerThird => "Lower third",
            TextStyle::Caption => "Caption",
        }
        .into(),
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
    }
}
pub(super) fn handle(
    ui: &AppWindow,
    state: &Rc<std::cell::RefCell<AppState>>,
    command: &str,
) -> bool {
    if command == "creator" {
        ui.set_show_creator(!ui.get_show_creator());
        ui.set_show_session(false);
        ui.set_show_review(false);
        ui.set_show_activity(false);
        load_audio(ui, &state.borrow());
        sync(ui, &state.borrow());
        return true;
    }
    if !command.starts_with("creator-") {
        return false;
    }
    if command == "creator-text-reload" {
        ui.set_creator_text_dirty(false); ui.set_creator_draft_notice("".into()); ui.set_creator_model_token("".into());
        // Explicit discard may refresh even while the old input still has focus.
        ui.set_creator_selected(-1);
        sync(ui, &state.borrow()); return true;
    }
    if command == "creator-refresh" {
        sync(ui, &state.borrow());
        return true;
    }
    if command == "creator-precision-tool" {
        state.borrow_mut().editor.tool = match ui.get_precision_tool() {
            1 => Tool::Slip,
            2 => Tool::Roll,
            3 => Tool::Slide,
            _ => Tool::Select,
        };
        set_status(
            state,
            "Drag a clip to make the precision edit. V returns to Selection.".into(),
        );
        full_sync(ui, state);
        return true;
    }
    let navigation = matches!(
        command,
        "creator-key-prev"
            | "creator-key-next"
            | "creator-key-copy"
            | "creator-captions-export"
            | "creator-style-save"
    );
    if command == "creator-audio-cancel" {
        if let Some(j) = &state.borrow().creative_job {
            j.cancel.store(true, Ordering::Release);
        }
        return true;
    }
    if !navigation && collab::readonly(&state.borrow()) {
        set_status(
            state,
            "Only connected editors can change this project.".into(),
        );
        return true;
    }
    let result = (|| -> Result<(), String> {
        if matches!(
            command,
            "creator-dialogue" | "creator-music" | "creator-ducking" | "creator-cleanup"
        ) {
            let mut s = state.borrow_mut();
            let id = s
                .editor
                .primary_clip()
                .and_then(|id| s.editor.linked_audio(id).or(Some(id)));
            s.editor.edit("audio-assistance", |p| {
                if command == "creator-ducking" {
                    p.sequence.audio.ducking = !p.sequence.audio.ducking;
                    return Ok(());
                }
                let id = id.ok_or("Select a clip with audio")?;
                let index = p.track_index_of_clip(id).ok_or("Clip missing")?;
                if p.sequence.tracks[index].kind != TrackKind::Audio
                    || p.sequence.tracks[index].locked
                {
                    return Err("Select an unlocked audio clip".into());
                }
                if command == "creator-cleanup" {
                    let c = p.clip_mut(id).unwrap();
                    c.creative.dialogue = !c.creative.dialogue;
                } else {
                    p.sequence.tracks[index].role = if command == "creator-dialogue" {
                        crate::project::audio::AudioRole::Dialogue
                    } else {
                        crate::project::audio::AudioRole::Music
                    };
                }
                Ok(())
            })?;
            s.status = if command == "creator-ducking" {
                format!(
                    "Music ducking {}. Mark dialogue and music tracks below.",
                    if s.editor.project.sequence.audio.ducking {
                        "on"
                    } else {
                        "off"
                    }
                )
            } else {
                "Audio role or cleanup updated. Undo restores it.".into()
            };
        } else if command == "creator-style-save" {
            let mut s = state.borrow_mut();
            let mut style = s
                .editor
                .primary_clip()
                .and_then(|id| s.editor.project.clip(id))
                .and_then(|c| c.creative.text.clone())
                .ok_or("Select a title or caption first")?;
            style.text.clear();
            s.preferences.text_style = Some(style);
            drop(s);
            save_preferences(state);
            set_status(state, "Text style saved for reuse.".into());
        } else if command == "creator-style-apply" {
            let mut s = state.borrow_mut();
            let id = s.editor.primary_clip().ok_or("Select a title or caption")?;
            if let Some(reason) = s.editor.edit_target_reason(id, false) {
                return Err(reason);
            }
            let style = s
                .preferences
                .text_style
                .clone()
                .ok_or("Save a text style first")?;
            s.editor.edit("apply-text-style", |p| {
                let content = p
                    .clip_mut(id)
                    .and_then(|c| c.creative.text.as_mut())
                    .ok_or("Select a title or caption")?;
                let text = content.text.clone();
                let role = content.style;
                *content = style;
                if role == TextStyle::Caption || content.style == TextStyle::Caption {
                    content.style = role;
                }
                content.text = text;
                Ok(())
            })?;
            ui.set_creator_selected(0);
        } else if command == "creator-audio-apply" || command == "creator-audio-reset" {
            let settings = if command == "creator-audio-reset" {
                Default::default()
            } else {
                crate::project::audio::AudioFinish {
                    low_db: ui.get_creator_low_db(),
                    mid_db: ui.get_creator_mid_db(),
                    high_db: ui.get_creator_high_db(),
                    compressor: ui.get_creator_compressor(),
                    limiter: ui.get_creator_limiter(),
                    ducking: state.borrow().editor.project.sequence.audio.ducking,
                }
            };
            settings.validate()?;
            state.borrow_mut().editor.edit("audio-finish", |p| {
                p.sequence.audio = settings;
                Ok(())
            })?;
            load_audio(ui, &state.borrow());
        } else if command == "creator-normalize" {
            let mut s = state.borrow_mut();
            if s.creative_job.is_some() {
                return Err("An audio analysis is already running".into());
            }
            let id = s
                .editor
                .primary_clip()
                .ok_or("Select an audio clip or linked video first")?;
            let id = s.editor.linked_audio(id).unwrap_or(id);
            if let Some(reason) = s.editor.edit_target_reason(id, false) {
                return Err(reason);
            }
            let c = s.editor.project.clip(id).ok_or("Clip missing")?.clone();
            if c.creative.animation.contains_key("volume") {
                return Err("Reset volume animation before normalizing this clip".into());
            }
            let media = s
                .editor
                .project
                .media(c.media_id)
                .ok_or("Select a clip with audio")?;
            if !media.has_audio || !media.path.is_file() {
                return Err(
                    "Normalize this clip on the host where its source audio is stored".into(),
                );
            }
            let mut project = Project::new_default("Loudness analysis");
            project.media = vec![media.clone()];
            project.sequence.frame_rate = s.editor.project.sequence.frame_rate;
            let mut measured = c.clone();
            measured.timeline_start = 0;
            measured.volume = 1.;
            let track = project
                .sequence
                .tracks
                .iter_mut()
                .find(|t| t.kind == TrackKind::Audio)
                .unwrap();
            track.clips = vec![measured];
            let cancel = Arc::new(AtomicBool::new(false));
            let worker_cancel = cancel.clone();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(crate::export::normalization_gain(&project, &worker_cancel));
            });
            s.creative_job = Some(AudioJob {
                cancel,
                result: rx,
                clip: c,
                document: s.editor.project.document_id.clone(),
            });
            s.status = "Measuring clip loudness…".into();
        } else if command == "creator-captions-import" {
            let Some(path) = rfd::FileDialog::new()
                .add_filter("Captions", &["srt", "vtt"])
                .pick_file()
            else {
                return Ok(());
            };
            let data = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
            let mut s = state.borrow_mut();
            let cues = captions::parse(&data, s.editor.project.sequence.frame_rate)?;
            s.editor.edit("import-captions", |p| {
                let track = p.add_track(TrackKind::Video);
                p.sequence.tracks[track].name = "Captions".into();
                for cue in cues {
                    let id = p.alloc();
                    p.sequence.tracks[track].clips.push(make_text(
                        id,
                        cue.text,
                        TextStyle::Caption,
                        cue.start,
                        cue.end - cue.start,
                    ));
                }
                Ok(())
            })?;
        } else if command == "creator-captions-export" {
            let s = state.borrow();
            let mut cues: Vec<_> = s
                .editor
                .project
                .sequence
                .tracks
                .iter()
                .flat_map(|t| &t.clips)
                .filter_map(|c| {
                    c.creative
                        .text
                        .as_ref()
                        .filter(|t| t.style == TextStyle::Caption)
                        .map(|t| captions::Cue {
                            start: c.timeline_start,
                            end: c.end(),
                            text: t.text.clone(),
                        })
                })
                .collect();
            if cues.is_empty() {
                return Err("This project has no captions to export".into());
            }
            cues.sort_by_key(|c| c.start);
            let rate = s.editor.project.sequence.frame_rate;
            drop(s);
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("SubRip", &["srt"])
                .add_filter("WebVTT", &["vtt"])
                .set_file_name("captions.srt")
                .save_file()
            {
                let web = path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("vtt"));
                std::fs::write(path, captions::write(&cues, rate, web))
                    .map_err(|e| e.to_string())?;
            }
        } else if matches!(
            command,
            "creator-title" | "creator-lower-third" | "creator-caption"
        ) {
            let mut s = state.borrow_mut();
            let style = match command {
                "creator-caption" => TextStyle::Caption,
                "creator-lower-third" => TextStyle::LowerThird,
                _ => TextStyle::Title,
            };
            let text = if ui.get_creator_text().trim().is_empty() {
                "Your text".into()
            } else {
                ui.get_creator_text().to_string()
            };
            let id = s.editor.edit("add-text", |p| {
                Ok(text_clip(
                    p,
                    text,
                    style,
                    p.sequence.playhead,
                    p.sequence.frame_rate.seconds_to_frame(5.).max(1),
                ))
            })?;
            s.editor.select_only(id, false);
            ui.set_creator_selected(0);
        } else {
            let mut s = state.borrow_mut();
            let id = s
                .editor
                .primary_clip()
                .ok_or("Select a timeline clip first")?;
            let frame = s.editor.project.sequence.playhead;
            let property = field(ui).to_string();
            let id = if property == "volume" && command.starts_with("creator-key-") {
                s.editor.linked_audio(id).unwrap_or(id)
            } else {
                id
            };
            if command == "creator-key-copy" {
                s.creative_clipboard = s
                    .editor
                    .project
                    .clip(id)
                    .and_then(|c| c.creative.animation.get(&property))
                    .cloned();
                return Ok(());
            }
            if command == "creator-key-prev" || command == "creator-key-next" {
                let c = s.editor.project.clip(id).unwrap();
                let local = frame - c.timeline_start + c.creative.offset;
                let key = c.creative.animation.get(&property).and_then(|keys| {
                    if command == "creator-key-prev" {
                        keys.iter().rev().find(|k| k.frame < local)
                    } else {
                        keys.iter().find(|k| k.frame > local)
                    }
                });
                if let Some(k) = key {
                    s.editor.project.sequence.playhead =
                        (k.frame + c.timeline_start - c.creative.offset).max(0);
                }
                return Ok(());
            }
            let clipboard = s.creative_clipboard.clone();
            let rate = s.editor.project.sequence.frame_rate;
            if let Some(reason) = s.editor.edit_target_reason(id, false) {
                return Err(reason);
            }
            if let Some(action) = command.strip_prefix("creator-precision-") {
                let (mode, sign) = action.rsplit_once('-').ok_or("Invalid precision edit")?;
                let ids = timeline::ops::expand_links(&s.editor.project, &[id]);
                s.editor.edit("precision", |p| {
                    timeline::ops::precision(p, &ids, mode, if sign == "back" { -1 } else { 1 })
                })?;
            } else if matches!(
                command,
                "creator-speed" | "creator-reverse" | "creator-freeze"
            ) {
                let ids = timeline::ops::expand_links(&s.editor.project, &[id]);
                let speed = (ui.get_creator_speed() as f64 / 100.).clamp(0.05, 16.);
                s.editor.edit("retime", |p| {
                    for id in &ids {
                        let track = p.track_index_of_clip(*id).ok_or("Clip missing")?;
                        if p.sequence.tracks[track].locked {
                            return Err("A linked track is locked".into());
                        }
                        let mut c = p.clip(*id).unwrap().clone();
                        if c.creative.text.is_some() {
                            return Err("Text duration is edited by trimming its clip".into());
                        }
                        let origin = if command == "creator-reverse" {
                            c.source_time_seconds(c.duration - 1, rate)
                        } else if command == "creator-freeze" {
                            c.source_time_seconds(
                                (frame - c.timeline_start).clamp(0, c.duration - 1),
                                rate,
                            )
                        } else {
                            c.source_start_seconds(rate)
                        };
                        if command == "creator-speed" {
                            c.duration = ((c.duration as f64 * c.creative.speed.abs() / speed)
                                .round() as i64)
                                .max(1);
                            c.creative.speed = speed * c.creative.speed.signum();
                            c.creative.freeze = false;
                            c.creative.freeze_at = None;
                        } else if command == "creator-reverse" {
                            c.creative.speed = -c.creative.speed;
                            c.creative.freeze = false;
                            c.creative.freeze_at = None;
                        } else {
                            c.creative.freeze = !c.creative.freeze;
                            c.creative.freeze_at = if c.creative.freeze {
                                Some(origin)
                            } else {
                                None
                            };
                        }
                        if command != "creator-freeze" {
                            c.source_time_offset = origin - rate.frame_to_seconds(c.source_in);
                        }
                        if !p
                            .media(c.media_id)
                            .is_some_and(|m| m.clip_source_is_valid(&c, rate))
                        {
                            return Err(
                                "This speed would exceed the available source footage".into()
                            );
                        }
                        if p.sequence.tracks[track].clips.iter().any(|other| {
                            other.id != *id
                                && other.timeline_start < c.end()
                                && other.end() > c.timeline_start
                        }) {
                            return Err("Make room after this clip before slowing it down".into());
                        }
                        *p.clip_mut(*id).unwrap() = c;
                    }
                    Ok(())
                })?;
            } else {
                s.editor.edit("creator-edit", |p| {
                    let c = p.clip_mut(id).ok_or("Clip missing")?;
                    let local = frame - c.timeline_start + c.creative.offset;
                    match command {
                        "creator-text" => {
                            let t = c
                                .creative
                                .text
                                .as_mut()
                                .ok_or("Select a title or caption")?;
                            t.text = ui.get_creator_text().to_string();
                            t.size = ui.get_creator_text_size().clamp(8., 512.);
                            t.background = ui.get_creator_text_background();
                        }
                        "creator-key-add" => {
                            if c.creative.text.is_some() && property == "rotation" {
                                return Err(
                                    "Text rotation animation is not supported in this version"
                                        .into(),
                                );
                            }
                            let value = creative::value(&c.evaluated(frame), &property)
                                .ok_or("Unknown property")?;
                            let keys = c.creative.animation.entry(property.clone()).or_default();
                            let k = Keyframe {
                                frame: local,
                                value,
                                interpolation: interpolation(ui),
                            };
                            match keys.binary_search_by_key(&local, |k| k.frame) {
                                Ok(i) => keys[i] = k,
                                Err(i) => keys.insert(i, k),
                            }
                        }
                        "creator-key-remove" => {
                            if let Some(keys) = c.creative.animation.get_mut(&property) {
                                keys.retain(|k| k.frame != local);
                                if keys.is_empty() {
                                    c.creative.animation.remove(&property);
                                }
                            }
                        }
                        "creator-key-reset" => {
                            let value = creative::value(&c.evaluated(frame), &property);
                            c.creative.animation.remove(&property);
                            if let Some(value) = value {
                                creative::assign(c, &property, value);
                            }
                        }
                        "creator-key-paste" => {
                            let mut keys = clipboard.clone().ok_or("Copy keyframes first")?;
                            if let Some(first) = keys.first() {
                                let delta = local - first.frame;
                                for k in &mut keys {
                                    k.frame = k.frame.saturating_add(delta);
                                }
                            }
                            c.creative.animation.insert(property.clone(), keys);
                        }
                        _ => return Err("Unknown creator action".into()),
                    }
                    c.creative.validate()?;
                    Ok(())
                })?;
            }
        }
        Ok(())
    })();
    if let Err(e) = result {
        set_status(state, e);
    } else if !navigation {
        if command == "creator-text" { ui.set_creator_text_dirty(false); ui.set_creator_draft_notice("".into()); ui.set_creator_model_token("".into()); }
        if command.starts_with("creator-audio-") {
            audio_mix::note_audio(state);
        } else {
            note(state);
        }
    }
    full_sync(ui, state);
    true
}

pub(super) struct AudioJob {
    cancel: Arc<AtomicBool>,
    result: Receiver<Result<f32, String>>,
    clip: Clip,
    document: String,
}
pub(super) fn pump(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let result = state
        .borrow()
        .creative_job
        .as_ref()
        .and_then(|j| match j.result.try_recv() {
            Ok(v) => Some(v),
            Err(mpsc::TryRecvError::Disconnected) => {
                Some(Err("Loudness analysis stopped unexpectedly".into()))
            }
            Err(mpsc::TryRecvError::Empty) => None,
        });
    let Some(result) = result else { return };
    let mut s = state.borrow_mut();
    let job = s.creative_job.take().unwrap();
    let result = result.and_then(|gain| {
        if job.cancel.load(Ordering::Acquire) {
            return Err("Loudness analysis cancelled".into());
        }
        if collab::readonly(&s)
            || s.editor.project.document_id != job.document
            || s.editor.project.clip(job.clip.id) != Some(&job.clip)
        {
            return Err(
                "The clip changed during analysis. Its volume was kept; run normalization again."
                    .into(),
            );
        }
        let value = gain.min(4.);
        s.editor.edit("normalize-audio", |p| {
            p.clip_mut(job.clip.id).ok_or("Clip missing")?.volume = value;
            Ok(())
        })?;
        Ok(if gain > 4. {
            "Normalization reached the clip gain limit (+12 dB)."
        } else {
            "Clip normalized toward -16 LUFS with a -1 dBTP ceiling."
        }
        .to_string())
    });
    let changed = result.is_ok();
    s.status = result.unwrap_or_else(|e| e);
    drop(s);
    if changed {
        audio_mix::note_audio(state);
    }
    full_sync(ui, state);
}
fn load_audio(ui: &AppWindow, state: &AppState) {
    let a = state.editor.project.sequence.audio;
    ui.set_creator_low_db(a.low_db);
    ui.set_creator_mid_db(a.mid_db);
    ui.set_creator_high_db(a.high_db);
    ui.set_creator_compressor(a.compressor);
    ui.set_creator_limiter(a.limiter);
}

impl Drop for AudioJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}
