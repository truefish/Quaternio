//! Cooperative live updates: checkpoint first, then let the owned launcher restart.
use super::*;
use crate::live_preview::{LiveChannel, LiveSession, Manifest};
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};
use std::time::Duration;

const SAVE_INTERVAL: Duration = Duration::from_secs(5);
const UPDATE_INTERVAL: Duration = Duration::from_secs(2);
const QUIET_INTERVAL: Duration = Duration::from_secs(2);

enum WorkKind {
    Ready,
    Close,
    Reload }

struct Work {
    kind: WorkKind,
    result: Receiver<Result<(), String>> }

pub(super) struct LiveState {
    channel: Arc<LiveChannel>,
    check: Option<Receiver<Result<Option<Manifest>, String>>>,
    pending: Option<Manifest>,
    work: Option<Work>,
    serialization: Option<Receiver<Result<(), String>>>,
    ready: bool,
    close_requested: bool,
    last_check: Instant,
    last_sample: Instant,
    last_save: Instant,
    quiet_since: Instant,
    signature: Option<u64>,
    saved_signature: Option<u64>,
    restored_view: Option<View>,
    restored_track_scroll: Option<f32>,
    message: String }

pub(super) struct RecoveredView {
    layout: Layout,
    extras: Extras }

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct View {
    page: i32,
    active_panel: i32,
    grade_tab: i32,
    quality: i32,
    bin_filter: i32,
    bin: String,
    query: String,
    ripple: bool,
    linked: bool,
    basic_open: bool,
    wheels_open: bool,
    track_scroll: f32,
    export_path: String,
    export_width: String,
    export_height: String,
    export_fps: i32,
    export_initialized: bool,
    export_custom_rate: String,
    export_inout: bool,
    export_crf: f32,
    export_speed: i32,
    export_encoder: i32 }

impl Default for View {
    fn default() -> Self {
        Self {
            page: 0,
            active_panel: 1,
            grade_tab: 0,
            quality: 0,
            bin_filter: 0,
            bin: String::new(),
            query: String::new(),
            ripple: false,
            linked: true,
            basic_open: true,
            wheels_open: false,
            track_scroll: 0.0,
            export_path: String::new(),
            export_width: "1920".into(),
            export_height: "1080".into(),
            export_fps: 1,
            export_initialized: false,
            export_custom_rate: String::new(),
            export_inout: false,
            export_crf: 21.0,
            export_speed: 0,
            export_encoder: 0 }
    }
}

impl View {
    fn capture(ui: &AppWindow, state: &AppState) -> Self {
        Self {
            page: ui.get_page(),
            active_panel: ui.get_active_panel(),
            grade_tab: ui.get_grade_tab(),
            quality: ui.get_quality(),
            bin_filter: ui.get_bin_filter(),
            bin: state.current_bin.clone(),
            query: ui.get_query().to_string(),
            ripple: ui.get_ripple_on(),
            linked: ui.get_linked_on(),
            basic_open: ui.get_basic_open(),
            wheels_open: ui.get_wheels_open(),
            track_scroll: ui.get_timeline_scroll_y(),
            export_path: ui.get_export_path().to_string(),
            export_width: ui.get_export_width().to_string(),
            export_height: ui.get_export_height().to_string(),
            export_fps: ui.get_export_fps(),
            export_initialized: ui.get_export_initialized(),
            export_custom_rate: ui.get_export_custom_rate().to_string(),
            export_inout: ui.get_export_inout(),
            export_crf: ui.get_export_crf(),
            export_speed: ui.get_export_speed(),
            export_encoder: ui.get_export_encoder() }
    }

    fn restore(&self, ui: &AppWindow) {
        ui.set_page(self.page.clamp(0, 1));
        ui.set_active_panel(self.active_panel.clamp(0, 4));
        ui.set_grade_tab(self.grade_tab.clamp(0, 5));
        ui.set_quality(self.quality.clamp(0, 2));
        ui.set_bin_filter(self.bin_filter.clamp(0, 3));
        ui.set_query(self.query.clone().into());
        ui.set_ripple_on(self.ripple);
        ui.set_linked_on(self.linked);
        ui.set_basic_open(self.basic_open);
        ui.set_wheels_open(self.wheels_open);
        ui.invoke_scroll_tracks_to(self.track_scroll.max(0.0));
        ui.set_export_path(self.export_path.clone().into());
        ui.set_export_width(self.export_width.clone().into());
        ui.set_export_height(self.export_height.clone().into());
        ui.set_export_fps(self.export_fps.clamp(0, 8));
        ui.set_export_initialized(self.export_initialized || !self.export_path.is_empty());
        ui.set_export_custom_rate(self.export_custom_rate.clone().into());
        export_form::refresh_rates(ui);
        ui.set_export_inout(self.export_inout);
        ui.set_export_crf(self.export_crf.clamp(14.0, 28.0));
        ui.set_read_crf(format!("{}", ui.get_export_crf().round() as i32).into());
        ui.set_export_speed(self.export_speed.clamp(0, 2));
        ui.set_export_encoder(self.export_encoder.clamp(0, 2));
    }
}

#[derive(Serialize, Deserialize)]
struct SavedSettings {
    width: u32,
    height: u32,
    frame_rate: FrameRate,
    crf: u8,
    cpu: bool,
    #[serde(default)]
    hardware: bool,
    speed: i32,
    force_composite: bool,
    use_in_out: bool,
    output: PathBuf }

impl SavedSettings {
    fn capture(value: &ExportSettings) -> Self {
        Self {
            width: value.width,
            height: value.height,
            frame_rate: value.frame_rate,
            crf: value.crf,
            cpu: matches!(value.encoder, EncoderPreference::Cpu),
            hardware: matches!(value.encoder, EncoderPreference::Hardware),
            speed: match value.speed {
                RenderSpeed::Balanced => 0,
                RenderSpeed::Fast => 1,
                RenderSpeed::Compact => 2 },
            force_composite: value.force_composite,
            use_in_out: value.use_in_out,
            output: value.output.clone() }
    }

    fn restore(self) -> Result<ExportSettings, String> {
        if self.width == 0
            || self.height == 0
            || self.width > 16384
            || self.height > 16384
            || self.frame_rate.num == 0
            || self.frame_rate.den == 0
        {
            return Err("Invalid recovered export settings".into());
        }
        Ok(ExportSettings {
            width: self.width,
            height: self.height,
            frame_rate: self.frame_rate,
            crf: self.crf.clamp(14, 28),
            encoder: if self.hardware {
                EncoderPreference::Hardware
            } else if self.cpu {
                EncoderPreference::Cpu
            } else {
                EncoderPreference::Auto
            },
            speed: match self.speed {
                1 => RenderSpeed::Fast,
                2 => RenderSpeed::Compact,
                _ => RenderSpeed::Balanced },
            force_composite: self.force_composite,
            use_in_out: self.use_in_out,
            output: self.output })
    }
}

#[derive(Serialize, Deserialize)]
struct SavedJob {
    id: i32,
    project: Project,
    settings: SavedSettings,
    status: JobStatus,
    message: String }

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Extras {
    view: View,
    preferences: Preferences,
    jobs: Vec<SavedJob>,
    next_export_id: i32,
    sequence_match_offered: bool }

pub(super) fn initialize(
    ui: &AppWindow,
    state: &Rc<std::cell::RefCell<AppState>>,
) -> Result<Option<RecoveredView>, String> {
    let Some(channel) = LiveChannel::new_from_env()? else {
        return Ok(None);
    };
    let recovered = channel.recover()?;
    let recovered = if let Some(session) = recovered {
        let mut extras: Extras = serde_json::from_value(session.extras)
            .map_err(|e| format!("Invalid recovered workspace: {e}"))?;
        if extras.jobs.len() > 1000 {
            return Err("Recovery export queue exceeds supported size".into());
        }
        let mut ids = std::collections::HashSet::new();
        for job in &mut extras.jobs {
            if job.id <= 0 || job.id == i32::MAX || !ids.insert(job.id) {
                return Err("Invalid recovered export job identity".into());
            }
            let mut validator = timeline::Editor::new();
            validator.project = job.project.clone();
            job.project = timeline::Editor::from_session(validator.session_snapshot())?.project;
            // Validate without consuming the saved setting before any state changes.
            let serialized = serde_json::to_value(&job.settings).map_err(|e| e.to_string())?;
            serde_json::from_value::<SavedSettings>(serialized)
                .map_err(|e| e.to_string())?
                .restore()?;
        }
        if !extras.view.track_scroll.is_finite() || !extras.view.export_crf.is_finite() {
            return Err("Invalid recovered workspace controls".into());
        }
        let mut current = state.borrow_mut();
        current.editor = timeline::Editor::from_session(session.editor)?;
        current.source = SourceTransport::from_session(session.source, &current.editor.project)?;
        current.status = "Recovered live workspace".into();
        Some(RecoveredView {
            layout: session.layout,
            extras })
    } else {
        None
    };
    let now = Instant::now();
    state.borrow_mut().live = Some(LiveState {
        channel: Arc::new(channel),
        check: None,
        pending: None,
        work: None,
        serialization: None,
        ready: false,
        close_requested: false,
        last_check: now,
        last_sample: now - Duration::from_secs(1),
        last_save: now - SAVE_INTERVAL,
        quiet_since: now,
        signature: None,
        saved_signature: None,
        restored_view: None,
        restored_track_scroll: None,
        message: "Live · preparing recovery".into() });
    ui.set_live_preview(true);
    Ok(recovered)
}

pub(super) fn restore_view(
    ui: &AppWindow,
    state: &Rc<std::cell::RefCell<AppState>>,
    recovered: RecoveredView,
) {
    apply_layout(ui, &recovered.layout);
    let Extras {
        view,
        preferences,
        jobs,
        next_export_id,
        sequence_match_offered } = recovered.extras;
    let mut current = state.borrow_mut();
    current.preferences = preferences;
    current.current_bin = view.bin.clone();
    current.filter_kind = view.bin_filter.clamp(0, 3);
    current.query = view.query.to_lowercase();
    current.sequence_match_offered = sequence_match_offered;
    current.export_jobs = jobs
        .into_iter()
        .map(|job| {
            // Recovery never starts rendering or overwrites an output automatically.
            let project = job.project;
            let interrupted = job.status == JobStatus::Rendering;
            ExportJob {
                id: job.id,
                project,
                settings: job
                    .settings
                    .restore()
                    .expect("validated recovered settings"),
                status: if interrupted {
                    JobStatus::Failed
                } else {
                    job.status
                },
                message: if interrupted {
                    "Interrupted before recovery; retry to render".into()
                } else {
                    job.message
                } }
        })
        .collect();
    current.next_export_id = next_export_id
        .max(
            current
                .export_jobs
                .iter()
                .map(|j| j.id)
                .max()
                .unwrap_or(0)
                .saturating_add(1),
        )
        .max(1);
    current.live.as_mut().unwrap().restored_view = Some(view);
}

pub(super) fn restore_ui_after_sync(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let view = state
        .borrow_mut()
        .live
        .as_mut()
        .and_then(|live| live.restored_view.take());
    if let Some(view) = view {
        view.restore(ui);
        export_form::restore(ui, state);
        state
            .borrow_mut()
            .live
            .as_mut()
            .unwrap()
            .restored_track_scroll = Some(view.track_scroll);
        sync_keybindings(ui, state);
        sync_queue(ui, state);
    }
}

struct CapturedSession {
    editor: crate::timeline::EditorSession,
    source: crate::source::SourceSession,
    layout: Layout,
    extras: Extras }

impl CapturedSession {
    fn finish(self) -> Result<LiveSession, String> {
        if self.extras.jobs.len() > 1000 {
            return Err("Recovery export queue exceeds supported size".into());
        }
        Ok(LiveSession {
            version: 1,
            editor: self.editor,
            source: self.source,
            layout: self.layout,
            extras: serde_json::to_value(self.extras).map_err(|e| e.to_string())? })
    }
}

fn snapshot(ui: &AppWindow, state: &AppState) -> CapturedSession {
    let extras = Extras {
        view: View::capture(ui, state),
        preferences: state.preferences.clone(),
        jobs: state
            .export_jobs
            .iter()
            .map(|job| SavedJob {
                id: job.id,
                project: job.project.clone(),
                settings: SavedSettings::capture(&job.settings),
                status: job.status,
                message: job.message.clone() })
            .collect(),
        next_export_id: state.next_export_id,
        sequence_match_offered: state.sequence_match_offered };
    CapturedSession {
        editor: state.editor.session_snapshot(),
        source: state.source.session_snapshot(),
        layout: read_layout(ui),
        extras }
}

// UI-sized data only: do not serialize projects/history every timer tick.
fn signature(ui: &AppWindow, state: &AppState) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_vec(&View::capture(ui, state))
        .unwrap_or_default()
        .hash(&mut hash);
    serde_json::to_vec(&read_layout(ui))
        .unwrap_or_default()
        .hash(&mut hash);
    serde_json::to_vec(&state.preferences)
        .unwrap_or_default()
        .hash(&mut hash);
    state.edit_gen.hash(&mut hash);
    state.editor.path.hash(&mut hash);
    state.editor.dirty.hash(&mut hash);
    state.editor.selected_clips.hash(&mut hash);
    state.editor.selected_transition.hash(&mut hash);
    state.editor.selected_media.hash(&mut hash);
    state.editor.selected_track.hash(&mut hash);
    std::mem::discriminant(&state.editor.tool).hash(&mut hash);
    state.editor.clipboard.len().hash(&mut hash);
    for copy in &state.editor.clipboard {
        copy.track.hash(&mut hash);
        copy.clip.id.hash(&mut hash);
        copy.clip.media_id.hash(&mut hash);
        copy.clip.source_in.hash(&mut hash);
        copy.clip.duration.hash(&mut hash);
    }
    state.editor.armed.is_some().hash(&mut hash);
    if let Some(armed) = state.editor.armed {
        std::mem::discriminant(&armed).hash(&mut hash);
    }
    state.editor.project.sequence.playhead.hash(&mut hash);
    state
        .editor
        .project
        .sequence
        .scroll
        .to_bits()
        .hash(&mut hash);
    state.editor.project.sequence.zoom.to_bits().hash(&mut hash);
    state.source.media_id.hash(&mut hash);
    state.source.frame.hash(&mut hash);
    state.next_export_id.hash(&mut hash);
    state.sequence_match_offered.hash(&mut hash);
    for job in &state.export_jobs {
        job.id.hash(&mut hash);
        job.status.hash(&mut hash);
    }
    hash.finish()
}

fn busy(ui: &AppWindow, state: &AppState) -> bool {
    state.playing
        || state.source.playing
        || state.waiting_audio
        || state.source.waiting_audio
        || state.export_running
        || state.queue_active
        || state
            .export_workers
            .iter()
            .any(|worker| !worker.is_finished())
        || state.pointer.down
        || state.viewer_last.is_some()
        || state.media_drag_origin.is_some()
        || state.editor.gesture_active()
        || state.source.scrub_origin.is_some()
        || state.import_rx.is_some()
        || !state.pending_imports.is_empty()
        || state.pending_insert.is_some()
        || ui.get_live_input_active()
        || ui.get_show_dialog()
        || ui.get_show_prefs()
        || ui.get_show_about()
        || ui.get_show_command_palette()
        || ui.get_show_sequence_match()
        || ui.get_capturing_binding()
        || ui.get_open_menu() != 0
}

fn start_work(live: &mut LiveState, kind: WorkKind, session: CapturedSession) {
    let channel = live.channel.clone();
    let manifest = live.pending.clone();
    let (tx, result) = mpsc::channel();
    let is_ready = matches!(kind, WorkKind::Ready);
    let is_reload = matches!(kind, WorkKind::Reload);
    std::thread::spawn(move || {
        let outcome = session.finish().and_then(|session| {
            if is_reload {
                channel.checkpoint_for_reload(session, &manifest.expect("pending validated build"))
            } else {
                channel.flush(session).and_then(|_| {
                    if is_ready {
                        channel.mark_ready()
                    } else {
                        Ok(())
                    }
                })
            }
        });
        let _ = tx.send(outcome);
    });
    live.work = Some(Work { kind, result });
}

pub(super) fn close(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let mut current = state.borrow_mut();
    if current.live.as_ref().is_some_and(|live| {
        live.work
            .as_ref()
            .is_some_and(|w| !matches!(w.kind, WorkKind::Ready))
    }) {
        return;
    }
    current.live.as_mut().unwrap().close_requested = true;
    current.audio.stop();
    current.playing = false;
    current.source.stop();
    current.export_cancel.store(true, Ordering::Relaxed);
    ui.set_live_reloading(true);
    ui.set_live_status("Saving recovery checkpoint…".into());
}

pub(super) fn tick(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let mut current = state.borrow_mut();
    let Some(mut live) = current.live.take() else {
        return;
    };
    let now = Instant::now();
    if current.track_body > 0.0 {
        if let Some(scroll) = live.restored_track_scroll.take() {
            ui.invoke_scroll_tracks_to(scroll.max(0.0));
        }
    }
    if let Some(rx) = &live.serialization {
        let result = match rx.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Disconnected) => Some(Err("Recovery worker stopped".into())),
            Err(mpsc::TryRecvError::Empty) => None };
        if let Some(result) = result {
            if let Err(error) = result {
                live.saved_signature = None;
                live.message = format!("Recovery failed: {error}");
            }
            live.serialization = None;
        }
    }
    if let Some(work) = &live.work {
        let outcome = match work.result.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Disconnected) => Some(Err("Recovery worker stopped".into())),
            Err(mpsc::TryRecvError::Empty) => None };
        if let Some(outcome) = outcome {
            let kind = &work.kind;
            match outcome {
                Ok(()) if matches!(kind, WorkKind::Ready) => {
                    live.ready = true;
                    live.message = "Live · recovery saved".into();
                    live.last_save = now;
                }
                Ok(()) => {
                    // The exact durable checkpoint has already been signed for the supervisor.
                    // Do not save again from a close callback after publishing its hash.
                    current.live = Some(live);
                    drop(current);
                    slint::quit_event_loop().ok();
                    return;
                }
                Err(error) => {
                    live.message = format!("Recovery failed: {error}");
                    live.close_requested = false;
                    live.last_save = now;
                    live.quiet_since = now;
                    if matches!(kind, WorkKind::Reload) {
                        live.pending = None;
                    }
                    ui.set_live_reloading(false);
                    ui.set_live_update_blocked(true);
                }
            }
            live.work = None;
        }
    }
    if live.work.is_some() {
        current.live = Some(live);
        return;
    }
    if !live.ready || live.close_requested {
        if live.serialization.is_some()
            || (live.close_requested && current.export_workers.iter().any(|w| !w.is_finished()))
        {
            ui.set_live_status("Finishing background work before recovery…".into());
            current.live = Some(live);
            return;
        }
        while let Some(worker) = current.export_workers.pop() {
            if worker.is_finished() {
                let _ = worker.join();
            } else {
                current.export_workers.push(worker);
                break;
            }
        }
        if !live.close_requested && now.duration_since(live.last_save) < SAVE_INTERVAL {
            ui.set_live_status(live.message.clone().into());
            current.live = Some(live);
            return;
        }
        let session = snapshot(ui, &current);
        let kind = if live.close_requested {
            WorkKind::Close
        } else {
            WorkKind::Ready
        };
        live.saved_signature = Some(signature(ui, &current));
        start_work(&mut live, kind, session);
        current.live = Some(live);
        return;
    }
    while let Some(result) = live.channel.poll_save() {
        live.message = match result {
            Ok(()) => "Live · recovery saved".into(),
            Err(error) => {
                live.saved_signature = None;
                format!("Recovery failed: {error}")
            }
        };
    }
    if now.duration_since(live.last_sample) >= Duration::from_millis(250) {
        let new_signature = signature(ui, &current);
        if live.signature != Some(new_signature) {
            live.signature = Some(new_signature);
            live.quiet_since = now;
        }
        live.last_sample = now;
    }
    let blocked = busy(ui, &current);
    if blocked {
        live.quiet_since = now;
    }
    if live.pending.is_none()
        && live.check.is_none()
        && now.duration_since(live.last_check) >= UPDATE_INTERVAL
    {
        let channel = live.channel.clone();
        let (tx, result) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(channel.check_update());
        });
        live.check = Some(result);
        live.last_check = now;
    }
    if let Some(check) = &live.check {
        let result = match check.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Disconnected) => {
                Some(Err("Update validation worker stopped".into()))
            }
            Err(mpsc::TryRecvError::Empty) => None };
        if let Some(result) = result {
            match result {
                Ok(pending) => {
                    live.pending = pending;
                }
                Err(error) => {
                    live.message = format!("Update rejected: {error}");
                }
            }
            live.check = None;
        }
    }
    let pending = live.pending.is_some();
    ui.set_live_update_pending(pending);
    ui.set_live_update_blocked(pending && blocked);
    if pending
        && !blocked
        && live.serialization.is_none()
        && now.duration_since(live.quiet_since) >= QUIET_INTERVAL
    {
        let session = snapshot(ui, &current);
        ui.set_live_reloading(true);
        ui.set_live_status("Saving recovery checkpoint…".into());
        start_work(&mut live, WorkKind::Reload, session);
    } else {
        if live.serialization.is_none()
            && live.signature != live.saved_signature
            && now.duration_since(live.last_save) >= SAVE_INTERVAL
            && !current.pointer.down
            && !current.editor.gesture_active()
            && current.source.scrub_origin.is_none()
        {
            let captured = snapshot(ui, &current);
            let channel = live.channel.clone();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let result = captured.finish().map(|session| channel.submit(session));
                let _ = tx.send(result);
            });
            live.serialization = Some(rx);
            live.saved_signature = live.signature;
            live.last_save = now;
        }
        let message = if pending {
            "Update ready · waiting for an editing pause"
        } else {
            &live.message
        };
        ui.set_live_status(message.into());
    }
    current.live = Some(live);
}

