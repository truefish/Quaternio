//! Prepare saves off the UI thread; publish only while their owning project is active.
use super::*;
use std::{
    fs,
    path::Path,
    time::{Duration, SystemTime},
};

const INTERVAL: Duration = Duration::from_secs(30);
#[derive(Clone, PartialEq)]
struct Stamp {
    size: u64,
    modified: SystemTime,
}
impl Stamp {
    fn read(path: &Path) -> Result<Self, String> {
        let metadata = fs::metadata(path).map_err(|e| e.to_string())?;
        Ok(Self {
            size: metadata.len(),
            modified: metadata.modified().map_err(|e| e.to_string())?,
        })
    }
}
struct Prepared {
    temporary: PathBuf,
    destination: PathBuf,
    document: String,
    generation: u64,
    stamp: Stamp,
}
impl Drop for Prepared {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.temporary);
    }
}

pub(super) struct Autosave {
    observed: Option<(String, PathBuf)>,
    last: Instant,
    pending: Option<Receiver<Result<Prepared, String>>>,
    blocked: bool,
    expected: Option<Stamp>,
    pub(super) problem: Option<String>,
}
impl Default for Autosave {
    fn default() -> Self {
        Self {
            observed: None,
            last: Instant::now(),
            pending: None,
            blocked: false,
            expected: None,
            problem: None,
        }
    }
}
impl Autosave {
    pub(super) fn reset(&mut self) {
        self.pending = None;
        self.last = Instant::now();
        self.blocked = false;
        self.problem = None;
    }
    pub(super) fn observe(&mut self, editor: &timeline::Editor) {
        self.reset();
        self.observed = editor.path.as_ref().map(|p| (editor.project.document_id.clone(), p.clone()));
        self.expected = editor.path.as_ref().and_then(|p| Stamp::read(p).ok());
    }
    pub(super) fn check_disk(&mut self, editor: &timeline::Editor) -> Result<(), String> {
        let Some(path) = &editor.path else { return Ok(()); };
        if self.expected.as_ref().is_none_or(|expected| Stamp::read(path).ok().as_ref() != Some(expected)) {
            let message = "The project file changed outside Quaternio. Autosave paused; use Save As to keep your edits.".to_string();
            self.blocked = true;
            self.problem = Some(message.clone());
            return Err(message);
        }
        Ok(())
    }
    fn tick(
        &mut self,
        editor: &mut timeline::Editor,
        generation: u64,
        enabled: bool,
        busy: bool,
    ) -> Option<String> {
        let identity = editor
            .path
            .as_ref()
            .map(|p| (editor.project.document_id.clone(), p.clone()));
        if identity != self.observed {
            self.reset();
            self.observed = identity;
            self.expected = if editor.dirty { None } else { editor.path.as_ref().and_then(|p| Stamp::read(p).ok()) };
            if editor.dirty && editor.path.is_some() {
                self.blocked = true;
                self.problem = Some("Recovered edits need a manual Save As before autosave can replace the original project.".into());
                return self.problem.clone();
            }
        }
        if !enabled { self.pending = None; }
        if !enabled || busy {
            return None;
        }
        if let Some(rx) = &self.pending {
            let result = match rx.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(_) => Some(Err("Autosave stopped. Save the project manually.".into())),
            };
            if let Some(result) = result {
                self.pending = None;
                self.last = Instant::now();
                match result {
                    Err(error) => { let message = format!("Autosave: {error}"); self.problem = Some(message.clone()); return Some(message); },
                    Ok(prepared) => {
                        if editor.path.as_ref() != Some(&prepared.destination)
                            || editor.project.document_id != prepared.document
                        {
                            return None;
                        }
                        if Stamp::read(&prepared.destination).ok().as_ref() != Some(&prepared.stamp)
                        {
                            let message = "The project file changed outside Quaternio. Autosave paused; use Save As to keep your edits.".to_string();
                            self.blocked = true;
                            self.problem = Some(message.clone());
                            return Some(message);
                        }
                        if let Err(error) = fs::rename(&prepared.temporary, &prepared.destination) {
                            let message = format!("Autosave could not save the project: {error}");
                            self.problem = Some(message.clone());
                            return Some(message);
                        }
                        self.expected = Stamp::read(&prepared.destination).ok();
                        self.problem = None;
                        if generation == prepared.generation {
                            editor.mark_saved();
                        }
                        return Some("Autosaved".into());
                    }
                }
            }
        }
        if self.pending.is_some()
            || self.blocked
            || busy
            || !editor.dirty
            || self.last.elapsed() < INTERVAL
        {
            return None;
        }
        let Some(destination) = editor.path.clone() else {
            return None;
        };
        if let Err(message) = self.check_disk(editor) { return Some(message); }
        let stamp = self.expected.clone().expect("disk baseline checked");
        let project = editor.project.clone();
        let (tx, rx) = mpsc::channel();
        self.pending = Some(rx);
        self.last = Instant::now();
        std::thread::spawn(move || {
            let result = (|| {
                if Stamp::read(&destination)? != stamp { return Err("Project changed before autosave preparation; use Save As.".into()); }
                let temporary = destination.with_file_name(format!(
                    ".quaternio-autosave-{}.quaternio",
                    uuid::Uuid::new_v4()
                ));
                let prepared = Prepared {
                    temporary,
                    destination,
                    document: project.document_id.clone(),
                    generation,
                    stamp,
                };
                project.save_to(&prepared.temporary)?;
                Ok(prepared)
            })();
            let _ = tx.send(result);
        });
        None
    }
}
pub(super) fn pump(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let message = {
        let mut current = state.borrow_mut();
        let enabled =
            current.preferences.autosave && !collab::guest(&current) && !current.update_applying;
        let busy = current.pointer.down || current.import_rx.is_some() || ui.get_live_reloading();
        let generation = current.edit_gen;
        let mut autosave = std::mem::take(&mut current.autosave);
        let result = autosave.tick(&mut current.editor, generation, enabled, busy);
        current.autosave = autosave;
        result
    };
    if let Some(message) = message {
        if ui.get_show_project_manager() && message != "Autosaved" {
            ui.set_home_status(message.clone().into());
        }
        set_status(state, message);
        full_sync(ui, state);
    }
}
