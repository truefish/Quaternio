//! Background engine discovery/setup; the editor remains usable without tools.
use super::*;
use crate::media_engine::{self, Engine};

enum Event { Progress(String), Done(Result<Engine, String>) }
#[derive(Default)]
pub(super) struct Setup {
    pub deferred_paths: Vec<PathBuf>,
    ready: bool,
    rx: Option<Receiver<Event>>,
    cancel: Arc<AtomicBool>,
}
impl Drop for Setup {
    fn drop(&mut self) { self.cancel.store(true, Ordering::Release); }
}

pub(super) fn check(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let hardware = state.borrow().preferences.preview_hardware;
    playback::set_preview_hardware(hardware); ui.set_preview_hardware(hardware);
    start(ui, state, |_, _| media_engine::discover());
}

fn start(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>,
    work: impl FnOnce(Arc<AtomicBool>, mpsc::Sender<Event>) -> Result<Engine, String> + Send + 'static) {
    if state.borrow().engine_setup.rx.is_some() { return; }
    let (tx, rx) = mpsc::channel(); let cancel = Arc::new(AtomicBool::new(false));
    state.borrow_mut().engine_setup.rx = Some(rx);
    state.borrow_mut().engine_setup.cancel = cancel.clone();
    ui.set_engine_busy(true);
    ui.set_engine_details("".into());
    ui.set_engine_status("Checking media engine…".into());
    std::thread::spawn(move || { let result = work(cancel, tx.clone()); let _ = tx.send(Event::Done(result)); });
}

const PREFERENCES_TAB: i32 = 5;

pub(super) fn open(ui: &AppWindow) {
    ui.set_preferences_tab(PREFERENCES_TAB);
    ui.set_open_menu(0);
    ui.set_show_command_palette(false);
    ui.set_show_prefs(true);
}

fn close(ui: &AppWindow) {
    if ui.get_preferences_tab() == PREFERENCES_TAB {
        ui.set_show_prefs(false);
        ui.invoke_focus_pane(focused_panel(ui));
    }
}

pub(super) fn require(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) -> bool {
    if state.borrow().engine_setup.ready { return true; }
    open(ui);
    false
}

pub(super) fn handle(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, command: &str) -> bool {
    match command {
        "preview-hardware-toggle" => {
            let enabled = !state.borrow().preferences.preview_hardware;
            playback::set_preview_hardware(enabled); ui.set_preview_hardware(enabled);
            let mut current = state.borrow_mut(); current.preferences.preview_hardware = enabled;
            if let Err(error) = current.preferences.save() { current.status = format!("Preferences: {error}"); }
        },
        "engine-setup" => open(ui),
        "engine-close" => close(ui),
        "engine-cancel" => { state.borrow().engine_setup.cancel.store(true, Ordering::Release); ui.set_engine_status("Cancelling setup…".into()); },
        "engine-check" => check(ui, state),
        "engine-install" => start(ui, state, |cancel, tx| media_engine::install(&cancel, None, |message| { let _ = tx.send(Event::Progress(message)); })),
        "engine-select" => {
            if state.borrow().engine_setup.rx.is_some() { return true; }
            if let Some(dir) = rfd::FileDialog::new().set_title("Select folder containing ffmpeg and ffprobe").pick_folder() {
                start(ui, state, move |_, _| media_engine::select(&dir));
            }
        },
        "engine-offline" => {
            if state.borrow().engine_setup.rx.is_some() { return true; }
            if let Some(dir) = rfd::FileDialog::new().set_title("Select folder containing pinned FFmpeg archives (see packaging/ffmpeg-lock.json)").pick_folder() {
                start(ui, state, move |cancel, tx| media_engine::install(&cancel, Some(&dir), |message| { let _ = tx.send(Event::Progress(message)); }));
            }
        },
        _ => return false,
    }
    true
}

pub(super) fn pump(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    loop {
        let event = state.borrow().engine_setup.rx.as_ref().map(|rx| rx.try_recv());
        match event {
            Some(Ok(Event::Progress(message))) => ui.set_engine_status(message.into()),
            Some(Ok(Event::Done(result))) => {
                state.borrow_mut().engine_setup.rx = None; ui.set_engine_busy(false);
                match result {
                    Ok(engine) => {
                        state.borrow_mut().engine_setup.ready = true;
                        ui.set_engine_ready(true);
                        ui.set_engine_status("Media engine ready".into());
                        ui.set_engine_details(format!("FFmpeg {}\n{}", engine.version, engine.ffmpeg.display()).into());
                        // Invalidate failed requests made while tools were absent.
                        state.borrow_mut().source_requested = None;
                        state.borrow_mut().shown_frame = -1;
                        let paths = std::mem::take(&mut state.borrow_mut().engine_setup.deferred_paths);
                        if !paths.is_empty() { close(ui); import_paths(ui, state, paths); }
                    },
                    Err(error) => {
                        state.borrow_mut().engine_setup.ready = false;
                        ui.set_engine_ready(false);
                        ui.set_engine_status(error.into());
                    },
                }
            },
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                state.borrow_mut().engine_setup.rx = None; ui.set_engine_busy(false);
                ui.set_engine_status("Media engine worker stopped. Try checking again.".into());
            },
            _ => break,
        }
    }
}
