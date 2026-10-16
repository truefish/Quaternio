//! Explicit clock entry is view navigation, never a project edit.
use super::*;

pub(super) fn begin(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, source: bool) {
    stop_play(state);
    ui.set_active_panel(if source { 0 } else { 1 });
    push_clock(ui, &state.borrow());
}

fn destination(text: &str, rate: FrameRate, last: i64) -> Result<i64, String> {
    let frame = crate::project::parse_timecode(text, rate)
        .filter(|frame| *frame >= 0)
        .ok_or_else(|| "Enter a nonnegative frame number or valid HH:MM:SS:FF timecode.".to_string())?;
    if frame > last {
        return Err(format!("The last available position is {} (frame {last}).", format_timecode(last, rate)));
    }
    Ok(frame)
}

pub(super) fn seek(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, source: bool, text: &str) -> bool {
    let target = {
        let current = state.borrow();
        if source {
            if !current.source_usable { return false; }
            destination(text, current.source.rate, current.source.total_frames.saturating_sub(1))
        } else {
            let seq = &current.editor.project.sequence;
            destination(text, seq.frame_rate, seq.duration_frames())
        }
    };
    let frame = match target {
        Ok(frame) => frame,
        Err(message) => {
            ui.set_status_text(message.clone().into());
            set_status(state, message);
            return false;
        }
    };
    stop_play(state);
    {
        let mut current = state.borrow_mut();
        if source {
            current.source.seek(frame);
            current.source_requested = None;
            current.source_min_token = current.source_token.saturating_add(1);
            current.source_intent = PreviewIntent::FinalSeek;
        } else {
            current.editor.project.sequence.playhead = frame;
            let span = current.timeline_w as f64 / current.editor.project.sequence.zoom.max(0.01);
            let seq = &mut current.editor.project.sequence;
            if (frame as f64) < seq.scroll || (frame as f64) > seq.scroll + span {
                seq.scroll = (frame as f64 - span * 0.2).max(0.0);
            }
            current.shown_frame = -1;
            current.preview_min_token = current.preview_token.saturating_add(1);
        }
        current.status = format!("{} paused at frame {frame}", if source { "Source" } else { "Sequence" });
    }
    full_sync(ui, state);
    if source { pump_source_preview(ui, state); } else { pump_preview(ui, state); }
    true
}

