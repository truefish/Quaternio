//! Review stays available beside the timeline; adding notes uses normal undo.
use super::*;

pub(super) fn sync_markers(ui: &AppWindow, state: &AppState) {
    let seq = &state.editor.project.sequence;
    let mut buckets = std::collections::BTreeSet::new();
    let mut notes: Vec<_> = state.editor.project.review_notes.iter().collect();
    notes.sort_by_key(|n| (n.resolved, n.frame, n.id));
    ui.set_review_markers(model(notes.into_iter().filter_map(|n| {
        let x = ((n.frame as f64 - seq.scroll) * seq.zoom) as f32;
        if x < 0.0 || x > state.timeline_w || !buckets.insert((x / 6.0).floor() as i32) { return None; }
        Some(ReviewMarker { id:n.id.to_string().into(), x, resolved:n.resolved })
    }).collect()));
}

pub(super) fn sync(ui: &AppWindow, state: &AppState) {
    sync_markers(ui, state);
    let show_resolved = ui.get_review_show_resolved();
    let query = ui.get_review_query().to_lowercase();
    let mut notes: Vec<_> = state.editor.project.review_notes.iter().filter(|n| (show_resolved || !n.resolved) && (query.is_empty() || n.text.to_lowercase().contains(&query) || n.author.to_lowercase().contains(&query))).collect();
    notes.sort_by_key(|n| (n.frame, n.id));
    let pages = notes.len().div_ceil(100).max(1) as i32;
    let page = ui.get_review_page().clamp(0,pages - 1);
    ui.set_review_page(page); ui.set_review_pages(pages);
    let rate = state.editor.project.sequence.frame_rate;
    ui.set_review_notes(model(notes.into_iter().skip(page as usize * 100).take(100).map(|n| ReviewItem {
        id: n.id.to_string().into(), timecode: format_timecode(n.frame, rate).into(),
        author: n.author.clone().into(), text: n.text.clone().into(), resolved: n.resolved,
    }).collect()));
    let open = state.editor.project.review_notes.iter().filter(|n| !n.resolved).count();
    ui.set_review_summary(format!("{open} open · {} total", state.editor.project.review_notes.len()).into());
}

pub(super) fn handle(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, command: &str) -> bool {
    if command == "review" {
        ui.set_show_review(!ui.get_show_review());
        ui.set_show_activity(false); ui.set_show_session(false);
        if ui.get_review_author().is_empty() {
            ui.set_review_author(state.borrow().preferences.reviewer_name.clone().into());
        }
        sync(ui, &state.borrow());
        return true;
    }
    if !command.starts_with("review-") { return false; }
    match command {
        "review-export" => {
            if state.borrow().editor.dirty || state.borrow().editor.path.is_none() {
                ui.set_review_error("Save the project first, then share that project and its review file.".into());
                return true;
            }
            if let Some(path) = rfd::FileDialog::new().set_title("Export review notes").add_filter("Quaternio review", &["qreview"]).set_file_name("review.qreview").save_file() {
                let result = state.borrow().editor.project.review_bundle().and_then(|bytes| {
                    crate::project::write_atomic(&path, &bytes)
                });
                ui.set_review_error(result.err().unwrap_or_else(|| "Review file saved. It contains notes only; share it with your saved project.".into()).into());
            }
        }
        "review-import" => {
            if let Some(path) = rfd::FileDialog::new().set_title("Import review notes").add_filter("Quaternio review", &["qreview"]).pick_file() {
                use std::io::Read;
                let result = std::fs::File::open(path).map_err(|e| e.to_string()).and_then(|file| {
                    let mut bytes = Vec::new(); file.take(64 * 1024 * 1024 + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
                    state.borrow_mut().editor.edit("review-import", |p| p.import_review_bundle(&bytes))
                });
                match result {
                    Ok(count) => { if count > 0 { let mut current = state.borrow_mut(); current.edit_gen = current.edit_gen.wrapping_add(1); } ui.set_review_error(format!("Imported {count} new notes. Existing notes retain their local resolution.").into()); },
                    Err(error) => ui.set_review_error(error.into()),
                }
            }
        }
        "review-search" => { ui.set_review_page(0); }
        "review-prev" => { ui.set_review_page(ui.get_review_page() - 1); }
        "review-next" => { ui.set_review_page(ui.get_review_page() + 1); }
        "review-filter" => { ui.set_review_show_resolved(!ui.get_review_show_resolved()); ui.set_review_page(0); }
        "review-add" => {
            let author = ui.get_review_author().to_string();
            let text = ui.get_review_draft().to_string();
            let frame = state.borrow().editor.project.sequence.playhead;
            let result = state.borrow_mut().editor.edit("review-add", |p| p.add_review_note(frame, &author, &text));
            match result {
                Ok(_) => {
                    ui.set_review_draft("".into()); ui.set_review_error("".into());
                    let mut current = state.borrow_mut();
                    current.preferences.reviewer_name = author.trim().to_string();
                    if let Err(error) = current.preferences.save() { current.status = format!("Reviewer name could not be saved: {error}"); }
                    current.edit_gen = current.edit_gen.wrapping_add(1);
                },
                Err(error) => ui.set_review_error(error.into()),
            }
        }
        _ => {
            let (action, id) = if let Some(id) = command.strip_prefix("review-jump:") { (0, id) }
                else if let Some(id) = command.strip_prefix("review-resolve:") { (1, id) }
                else { return false; };
            let Ok(id) = id.parse::<u64>() else { return true; };
            if action == 0 {
                ui.set_show_review(true); ui.set_show_activity(false); ui.set_show_session(false);
                if ui.get_review_author().is_empty() { ui.set_review_author(state.borrow().preferences.reviewer_name.clone().into()); }
                stop_play(state);
                let frame = state.borrow().editor.project.review_notes.iter().find(|n| n.id == id).map(|n| n.frame);
                if let Some(frame) = frame { state.borrow_mut().editor.project.sequence.playhead = frame; }
            } else {
                act(state, |editor| editor.edit("review-resolve", |p| p.resolve_review_note(id)));
            }
        }
    }
    full_sync(ui, state);
    true
}
