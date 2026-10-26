use super::*;
use crate::project::creative::{AngleCut, CameraAngle, Multicam};
pub(super) struct Job {
    cancel: Arc<AtomicBool>,
    rx: Receiver<Result<Vec<f64>, String>>,
    clip: Clip,
    sequence: String,
    document: String,
}
impl Drop for Job {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}
pub(super) fn sync(ui: &AppWindow, s: &AppState) {
    ui.set_multicam_busy(s.multicam.is_some());
    let clip = s
        .editor
        .primary_clip()
        .and_then(|id| s.editor.project.clip(id));
    let camera = clip.and_then(|c| c.creative.multicam.as_ref());
    ui.set_multicam_options(model(
        camera
            .map(|m| {
                m.angles
                    .iter()
                    .enumerate()
                    .map(|(i, a)| LabelItem {
                        text: format!(
                            "{} · {}",
                            i + 1,
                            s.editor
                                .project
                                .media(a.media_id)
                                .map_or("Camera", |m| m.name.as_str())
                        )
                        .into(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    ));
    ui.set_multicam_has_clip(camera.is_some());
    let angle = (ui.get_multicam_angle().max(0) as usize)
        .min(camera.map_or(0, |m| m.angles.len().saturating_sub(1)));
    ui.set_multicam_angle(angle as i32);
    let token = format!("{}:{angle}", clip.map_or(0, |c| c.id));
    if ui.get_multicam_token() != token {
        ui.set_multicam_token(token.into());
        ui.set_multicam_offset(
            camera
                .and_then(|m| m.angles.get(angle))
                .map_or(0., |a| a.offset) as f32,
        );
    }
}
pub(super) fn handle(
    ui: &AppWindow,
    state: &Rc<std::cell::RefCell<AppState>>,
    command: &str,
) -> bool {
    if !command.starts_with("multicam-") {
        return false;
    }
    if command == "multicam-refresh" {
        sync(ui, &state.borrow());
        return true;
    }
    let result = (|| -> Result<(), String> {
        let mut s = state.borrow_mut();
        if command == "multicam-cancel" {
            if let Some(j) = &s.multicam {
                j.cancel.store(true, Ordering::Release);
            }
            return Ok(());
        }
        if collab::readonly(&s) {
            return Err("Editing permission is required".into());
        }
        if command == "multicam-create" {
            let media = s.media_selection.ids.clone();
            if !(2..=16).contains(&media.len()) {
                return Err("Select 2–16 camera recordings in the Project panel".into());
            }
            s.editor.edit("create-multicam", |p| {
                let mut angles = Vec::new();
                let mut duration = f64::INFINITY;
                for id in media {
                    let m = p.media(id).ok_or("Camera missing")?;
                    if !m.has_video {
                        return Err("Select video recordings for every angle".into());
                    }
                    let offset = m.source_range()?.start.seconds();
                    duration = duration.min(m.source_duration_seconds() - offset);
                    angles.push(CameraAngle {
                        media_id: id,
                        offset,
                    });
                }
                let baseline = angles[0].offset;
                for a in &mut angles {
                    a.offset -= baseline;
                }
                let duration = p.sequence.frame_rate.seconds_to_frame(duration).max(1);
                let start = p.sequence.playhead;
                let source_in = p.sequence.frame_rate.seconds_to_frame(baseline);
                let remainder = baseline - p.sequence.frame_rate.frame_to_seconds(source_in);
                let link = p.alloc();
                let media_id = angles[0].media_id;
                for kind in [TrackKind::Video, TrackKind::Audio] {
                    let t = p.add_track(kind);
                    p.sequence.tracks[t].name = if kind == TrackKind::Video {
                        "Multicam"
                    } else {
                        "Camera 1 audio"
                    }
                    .into();
                    let id = p.alloc();
                    let creative = crate::project::creative::Creative {
                        multicam: if kind == TrackKind::Video {
                            Some(Multicam {
                                angles: angles.clone(),
                                cuts: Vec::new(),
                            })
                        } else {
                            None
                        },
                        ..Default::default()
                    };
                    p.sequence.tracks[t].clips.push(Clip {
                        creative,
                        id,
                        media_id,
                        name: "Multicam".into(),
                        timeline_start: start,
                        duration,
                        source_in,
                        source_time_offset: remainder,
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
                Ok(())
            })?;
            s.status="Multicam created. Select its video clip and choose angles at the playhead. Source In marks align the cameras; camera 1 supplies audio.".into();
        } else {
            let id = s
                .editor
                .primary_clip()
                .ok_or("Select a multicam video clip")?;
            if let Some(reason) = s.editor.edit_target_reason(id, false) {
                return Err(reason);
            }
            let clip = s.editor.project.clip(id).ok_or("Clip missing")?.clone();
            let camera = clip
                .creative
                .multicam
                .as_ref()
                .ok_or("Select a multicam video clip")?;
            let angle = ui.get_multicam_angle().max(0) as usize;
            if angle >= camera.angles.len() {
                return Err("This recording has fewer camera angles".into());
            }
            if command == "multicam-sync" {
                if s.multicam.is_some() {
                    return Err("Synchronization is already running".into());
                }
                if collab::guest(&s) {
                    return Err("Run waveform synchronization on the host".into());
                }
                let paths=camera.angles.iter().map(|a|s.editor.project.media(a.media_id).filter(|m|m.has_audio).map(|m|m.path.clone()).ok_or("Every camera needs shared audible sound; otherwise use manual offsets")).collect::<Result<Vec<_>,_>>()?;
                let cancel = Arc::new(AtomicBool::new(false));
                let c = cancel.clone();
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || {
                    let result = (|| {
                        let reference = playback::sync_audio::envelope(&paths[0], &c)?;
                        let mut offsets = vec![0.];
                        for path in &paths[1..] {
                            let audio = playback::sync_audio::envelope(path, &c)?;
                            offsets.push(playback::sync_audio::offset(&reference, &audio, &c)?);
                        }
                        Ok(offsets)
                    })();
                    let _ = tx.send(result);
                });
                s.multicam = Some(Job {
                    cancel,
                    rx,
                    clip,
                    sequence: s.editor.project.active_sequence.clone(),
                    document: s.editor.project.document_id.clone(),
                });
                s.status = "Matching the first 30 seconds of camera sound…".into();
            } else {
                let frame = s.editor.project.sequence.playhead;
                let offset = ui.get_multicam_offset() as f64;
                s.editor.edit("multicam-angle", |p| {
                    let c = p.clip_mut(id).ok_or("Clip missing")?;
                    let local = frame - c.timeline_start + c.creative.offset;
                    let contains = c.contains(frame);
                    let m = c.creative.multicam.as_mut().unwrap();
                    if command == "multicam-offset" {
                        m.angles[angle].offset = offset;
                    } else if command == "multicam-cut" {
                        if !contains {
                            return Err("Move the playhead inside the multicam clip".into());
                        }
                        m.cuts.retain(|k| k.frame != local);
                        m.cuts.push(AngleCut {
                            frame: local,
                            angle,
                        });
                        m.cuts.sort_by_key(|k| k.frame);
                    } else {
                        return Err("Unknown multicam action".into());
                    }
                    m.validate()
                })?;
                s.status = "Camera edit applied. Undo restores it.".into();
            }
        }
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
pub(super) fn pump(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let result = state
        .borrow()
        .multicam
        .as_ref()
        .and_then(|j| match j.rx.try_recv() {
            Ok(r) => Some(r),
            Err(mpsc::TryRecvError::Disconnected) => {
                Some(Err("Synchronization worker stopped".into()))
            }
            Err(_) => None,
        });
    if let Some(result) = result {
        let mut s = state.borrow_mut();
        let job = s.multicam.take().unwrap();
        let result = result.and_then(|offsets| {
            if job.cancel.load(Ordering::Acquire) {
                return Err("Synchronization cancelled".into());
            }
            if collab::readonly(&s)
                || s.editor.project.document_id != job.document
                || s.editor.project.active_sequence != job.sequence
                || s.editor.project.clip(job.clip.id) != Some(&job.clip)
            {
                return Err(
                    "The multicam clip changed during analysis. Run synchronization again.".into(),
                );
            }
            s.editor.edit("sync-cameras", |p| {
                let m = p
                    .clip_mut(job.clip.id)
                    .unwrap()
                    .creative
                    .multicam
                    .as_mut()
                    .unwrap();
                for (angle, offset) in m.angles.iter_mut().zip(offsets) {
                    angle.offset = offset;
                }
                m.validate()
            })?;
            Ok("Cameras synchronized. Review the alignment before cutting.".into())
        });
        s.status = result.unwrap_or_else(|e| e);
        drop(s);
        note(state);
        full_sync(ui, state);
    }
}
