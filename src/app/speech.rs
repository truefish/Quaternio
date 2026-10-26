use super::*;
use crate::{
    project::{captions::Cue, creative::TextStyle},
    transcription::{self, Job, Output},
};
#[derive(Default)]
pub(super) struct State {
    job: Option<Job>,
    document: String,
    sequence: String,
    cues: Vec<Cue>,
    rate: FrameRate,
    snapshot: crate::collaboration::document::Document,
    history: VecDeque<(String, Arc<Vec<Cue>>, usize)>,
}
fn signature(document: &crate::collaboration::document::Document) -> String {
    use sha2::Digest;
    format!(
        "{:x}",
        sha2::Sha256::digest(serde_json::to_vec(document).unwrap())
    )
}
fn remember(s: &mut State) {
    let key = signature(&s.snapshot);
    let bytes = s
        .cues
        .iter()
        .map(|c| c.text.len() + std::mem::size_of::<Cue>())
        .sum();
    s.history.retain(|(old, _, _)| old != &key);
    s.history.push_back((key, Arc::new(s.cues.clone()), bytes));
    while s.history.len() > 100
        || s.history.iter().map(|(_, _, bytes)| bytes).sum::<usize>() > 16 * 1024 * 1024
    {
        if s.history.pop_front().is_none() {
            break;
        }
    }
}
pub(super) fn sync(ui: &AppWindow, s: &mut AppState) {
    let same = s.speech.document == s.editor.project.document_id
        && s.speech.sequence == s.editor.project.active_sequence;
    if same && !s.speech.history.is_empty() {
        let current = crate::collaboration::document::capture(&s.editor.project);
        let key = signature(&current);
        if let Some(cues) = s
            .speech
            .history
            .iter()
            .find(|(old, _, _)| old == &key)
            .map(|(_, cues, _)| cues.clone())
        {
            s.speech.cues = (*cues).clone();
            s.speech.snapshot = current;
        }
    }
    ui.set_speech_installed(transcription::installed());
    ui.set_speech_busy(s.speech.job.is_some());
    ui.set_speech_status(
        s.speech
            .job
            .as_ref()
            .map(|j| j.status.lock().unwrap().clone())
            .unwrap_or_else(|| {
                if !same && !s.speech.cues.is_empty() {
                    "This transcript belongs to another sequence. Switch back to review it.".into()
                } else if s.speech.cues.is_empty() {
                    "Transcribe on the host. Audio stays on this computer.".into()
                } else {
                    format!(
                        "{} passages ready. Review before adding captions or cutting.",
                        s.speech.cues.len()
                    )
                }
            })
            .into(),
    );
    ui.set_speech_segments(model(
        s.speech
            .cues
            .iter()
            .filter(|_| same)
            .map(|c| LabelItem {
                text: format!("{}  {}", format_timecode(c.start, s.speech.rate), c.text).into(),
            })
            .collect(),
    ));
}
pub(super) fn handle(
    ui: &AppWindow,
    state: &Rc<std::cell::RefCell<AppState>>,
    command: &str,
) -> bool {
    if !command.starts_with("speech-") {
        return false;
    }
    let result = (|| -> Result<(), String> {
        let mut s = state.borrow_mut();
        if command == "speech-cancel" {
            if let Some(j) = &s.speech.job {
                j.cancel.store(true, Ordering::Release);
            }
            return Ok(());
        }
        if command == "speech-delete" {
            if s.speech.job.is_some() {
                return Err("Stop the speech job before deleting its model".into());
            }
            transcription::delete_model()?;
            s.status = "Speech model deleted".into();
            return Ok(());
        }
        if s.speech.job.is_some() {
            return Err("A speech job is already running".into());
        }
        if command == "speech-download" {
            s.speech.job = Some(Job::download());
            return Ok(());
        }
        if command == "speech-start" {
            if collab::guest(&s) {
                return Err(
                    "The host can transcribe the sequence without sending source files to guests"
                        .into(),
                );
            }
            if !transcription::installed() {
                return Err("Download the speech model first (57 MiB)".into());
            }
            s.speech.document = s.editor.project.document_id.clone();
            s.speech.sequence = s.editor.project.active_sequence.clone();
            s.speech.rate = s.editor.project.sequence.frame_rate;
            s.speech.snapshot = crate::collaboration::document::capture(&s.editor.project);
            s.speech.cues.clear();
            s.speech.history.clear();
            s.speech.job = Some(Job::transcribe(s.editor.project.clone()));
            return Ok(());
        }
        if s.speech.document != s.editor.project.document_id
            || s.speech.sequence != s.editor.project.active_sequence
            || s.speech.rate != s.editor.project.sequence.frame_rate
        {
            return Err("Open the sequence used for this transcript".into());
        }
        let index = ui.get_speech_selection().max(0) as usize;
        if s.speech.snapshot != crate::collaboration::document::capture(&s.editor.project) {
            return Err("The timeline changed since transcription. Transcribe again before using its timing.".into());
        }
        if command == "speech-seek" {
            let cue = s.speech.cues.get(index).ok_or("Choose a passage")?;
            s.editor.project.sequence.playhead = cue.start;
            return Ok(());
        }
        if collab::readonly(&s) {
            return Err("Editing permission is required".into());
        }
        if s.speech.snapshot != crate::collaboration::document::capture(&s.editor.project) {
            return Err("The timeline changed since transcription. Transcribe again before applying timed edits.".into());
        }
        remember(&mut s.speech);
        if command == "speech-captions" {
            let cues = s.speech.cues.clone();
            if cues.is_empty() {
                return Err("No speech was recognized".into());
            }
            s.editor.edit("transcript-captions", |p| {
                let track = p.add_track(TrackKind::Video);
                p.sequence.tracks[track].name = "Captions".into();
                for c in cues {
                    let id = p.alloc();
                    p.sequence.tracks[track].clips.push(creative::make_text(
                        id,
                        c.text,
                        TextStyle::Caption,
                        c.start,
                        c.end - c.start,
                    ));
                }
                Ok(())
            })?;
            s.status = "Captions added. Select a caption to edit its text.".into();
        } else if command == "speech-cut" {
            let cue = s.speech.cues.get(index).ok_or("Choose a passage")?.clone();
            // Review the range in the timeline before committing the destructive cut.
            if s.editor.project.sequence.in_point != Some(cue.start)
                || s.editor.project.sequence.out_point != Some(cue.end - 1)
            {
                s.editor.project.sequence.in_point = Some(cue.start);
                s.editor.project.sequence.out_point = Some(cue.end - 1);
                s.editor.project.sequence.playhead = cue.start;
                s.status="Passage marked. Press Remove passage again to cut it across all tracks (Undo restores it).".into();
                return Ok(());
            }
            s.editor.edit("transcript-cut", |p| {
                crate::timeline::ops::extract_range(p, cue.start, cue.end)
            })?;
            s.speech.cues.remove(index);
            for c in &mut s.speech.cues {
                if c.start >= cue.end {
                    c.start -= cue.end - cue.start;
                    c.end -= cue.end - cue.start;
                }
            }
            s.status = "Passage removed across all tracks. Undo restores it.".into();
        } else {
            return Err("Unknown speech action".into());
        }
        s.speech.snapshot = crate::collaboration::document::capture(&s.editor.project);
        remember(&mut s.speech);
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
    let result = state.borrow().speech.job.as_ref().and_then(Job::poll);
    if let Some(result) = result {
        let mut s = state.borrow_mut();
        s.speech.job.take();
        match result {
            Ok(Output::Installed) => {
                s.status = "Speech model ready.".into()
            }
            Ok(Output::Transcript(segments)) => {
                s.speech.cues = transcription::cues(segments, s.speech.rate);
                remember(&mut s.speech);
                s.status = "Transcript ready for review".into();
            }
            Err(e) => s.status = e,
        }
        drop(s);
        full_sync(ui, state);
    } else if state.borrow().speech.job.is_some() {
        sync(ui, &mut state.borrow_mut());
    }
}
