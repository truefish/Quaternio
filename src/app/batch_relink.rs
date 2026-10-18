//! Bounded missing-media search. Ambiguous names require individual relinking.
use super::*;
use std::collections::HashMap;
use std::path::Path;

#[derive(Default)]
pub(super) struct Search {
    rx: Option<Receiver<Result<Matches, String>>>,
    cancel: Arc<AtomicBool>,
    generation: u64,
}
impl Drop for Search { fn drop(&mut self) { self.cancel.store(true, Ordering::Release); } }
struct Matches { paths: Vec<(u64, PathBuf)>, ambiguous: usize, missing: usize, limited: bool }

fn key(path: &Path) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
    if cfg!(windows) { name.to_lowercase() } else { name }
}

fn find(root: PathBuf, media: Vec<Media>, cancel: &AtomicBool) -> Result<Matches, String> {
    let missing: Vec<_> = media.into_iter().filter(|m| !m.path.is_file()).collect();
    let names: HashSet<_> = missing.iter().map(|m| key(&m.path)).collect();
    let mut files: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let mut queue = VecDeque::from([(root, 0usize)]); let mut visited = 0usize; let mut limited = false;
    while let Some((dir, depth)) = queue.pop_front() {
        if cancel.load(Ordering::Acquire) { return Err("Batch relink cancelled".into()); }
        let entries = match std::fs::read_dir(&dir) { Ok(entries) => entries, Err(e) if depth == 0 => return Err(e.to_string()), Err(_) => continue };
        for entry in entries.flatten() {
            if cancel.load(Ordering::Acquire) { return Err("Batch relink cancelled".into()); }
            visited += 1; if visited > 50_000 { limited = true; break; }
            let Ok(kind) = entry.file_type() else { continue; };
            if kind.is_dir() && depth < 32 { queue.push_back((entry.path(), depth + 1)); }
            else if kind.is_file() {
                let name = key(&entry.path()); if names.contains(&name) {
                    // Two matches are enough to prove ambiguity; bound memory.
                    let matches = files.entry(name).or_default(); if matches.len() < 2 { matches.push(entry.path()); }
                }
            }
        }
        if limited { break; }
    }
    let mut result = Matches { paths: Vec::new(), ambiguous: 0, missing: 0, limited };
    for media in missing {
        match files.get(&key(&media.path)).map(Vec::as_slice) {
            Some([path]) => result.paths.push((media.id, path.clone())),
            Some(_) => result.ambiguous += 1,
            None => result.missing += 1,
        }
    }
    // A truncated search cannot establish that an apparent single match is unique.
    if limited { result.ambiguous += result.paths.len(); result.paths.clear(); }
    Ok(result)
}

pub(super) fn start(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    if !engine_setup::require(ui, state) { return; }
    if state.borrow().import_rx.is_some() || state.borrow().batch_relink.rx.is_some() {
        set_status(state, "Finish the current import or relink search first".into()); return;
    }
    let Some(root) = rfd::FileDialog::new().set_title("Search this folder for missing media").pick_folder() else { return; };
    let (tx, rx) = mpsc::channel(); let cancel = Arc::new(AtomicBool::new(false));
    let media = state.borrow().editor.project.media.clone();
    let mut current = state.borrow_mut();
    current.batch_relink = Search { rx: Some(rx), cancel: cancel.clone(), generation: current.edit_gen };
    current.status = "Searching for missing media… Escape cancels.".into(); ui.set_status_text(current.status.clone().into());
    std::thread::spawn(move || { let _ = tx.send(find(root, media, &cancel)); });
}

pub(super) fn cancel(state: &Rc<std::cell::RefCell<AppState>>) { state.borrow().batch_relink.cancel.store(true, Ordering::Release); }

pub(super) fn pump(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    if state.borrow().pointer.down || ui.get_modal_open() { return; }
    let result = state.borrow().batch_relink.rx.as_ref().and_then(|rx| match rx.try_recv() {
        Ok(result) => Some(result), Err(mpsc::TryRecvError::Disconnected) => Some(Err("Media search stopped unexpectedly".into())), _ => None,
    });
    let Some(result) = result else { return; };
    state.borrow_mut().batch_relink.rx = None;
    let matches = match result { Ok(matches) => matches, Err(error) => { set_status(state,error); return; } };
    if state.borrow().edit_gen != state.borrow().batch_relink.generation || state.borrow().import_rx.is_some() {
        set_status(state, "Project changed during search. Run batch relink again.".into()); return;
    }
    let description = format!("{} unique filename matches\n{} ambiguous (use individual relink)\n{} not found{}\n\n{}\n\nRelink these matches? Each replacement is probed and checked against existing clip ranges. Same filenames do not prove identical content.",
        matches.paths.len(), matches.ambiguous, matches.missing, if matches.limited { "\nSearch limit reached; no replacements proposed. Choose a smaller folder." } else { "" },
        matches.paths.iter().take(12).map(|(_,p)| p.display().to_string()).collect::<Vec<_>>().join("\n"));
    if matches.paths.is_empty() {
        rfd::MessageDialog::new().set_title("Batch relink results").set_description(description).show(); return;
    }
    if rfd::MessageDialog::new().set_title("Confirm batch relink").set_description(description).set_buttons(rfd::MessageButtons::YesNo).show() != rfd::MessageDialogResult::Yes { return; }
    let (tx, rx) = mpsc::sync_channel(32); let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut current = state.borrow_mut(); current.import_rx = Some(rx); current.import_job = None;
        current.import_cancel = cancel.clone(); current.import_batch.clear(); current.import_processed = 0; current.import_count = 0;
        current.import_errors = 0; current.import_last_error.clear(); current.relinking = true; update_import_status(&mut current, false);
    }
    full_sync(ui, state);
    std::thread::spawn(move || {
        for (id,path) in matches.paths {
            if cancel.load(Ordering::Acquire) { break; }
            let event = match playback::probe_path_cancellable(&path,&cancel) {
                Ok(media) => ImportEvent::Relink(id,media), Err(error) => ImportEvent::Error(format!("{}: {error}",path.display())),
            };
            if cancel.load(Ordering::Acquire) || tx.send(event).is_err() { return; }
        }
        let _ = tx.send(ImportEvent::Done);
    });
}
