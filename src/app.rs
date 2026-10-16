slint::include_modules!();

mod live;
mod filmstrips;
mod viewer_status;
mod navigation;
mod audio_mix;
mod export_form;
mod media_selection;

use std::cell::Cell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::Instant;

use slint::{
    ComponentHandle, Model, ModelRc, Rgb8Pixel, SharedPixelBuffer, SharedString, Timer, TimerMode,
    VecModel };

use crate::color::Grade;
use crate::commands::{self, Scope, ACTIONS};
use crate::export::{
    export_mp4, export_range, EncoderPreference, ExportProgress, ExportSettings, RenderSpeed };
use crate::playback;
use crate::playback::{probe_path, AudioEngine, ImageBuf, PreviewEngine, PreviewIntent, ThumbnailEngine};
use crate::preferences::{Layout, Preferences};
use crate::project::{
    format_timecode, frame_rate_presets, sequence_presets, Clip, FrameRate, Media, MediaKind,
    Project, ShortcutProfile, SourceRange, SourceTime, TrackKind };
use crate::shortcuts::{self, Command, Mods};
use crate::source::SourceTransport;
use crate::timeline::{self, frame_at, hit_rows, layout_rows, Edge, Hit, Tool, TrackRow};
use crate::transitions::transition_window;

struct Pointer {
    down: bool,
    zone: i32,
    x: f32,
    y: f32,
    moved: bool,
    seeking: bool,
    origin_frame: i64,
    hit: Hit,
    ripple: bool,
    links: bool }

impl Default for Pointer {
    fn default() -> Self {
        Self {
            down: false,
            zone: 0,
            x: 0.0,
            y: 0.0,
            moved: false,
            seeking: false,
            origin_frame: 0,
            hit: Hit::Empty { frame: 0 },
            ripple: false,
            links: true }
    }
}

impl Pointer {
    fn finish(&mut self) -> (bool, bool) {
        let previous = std::mem::take(self);
        (previous.moved, previous.seeking)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TimelineViewKey {
    revision: u64,
    zoom: u64,
    scroll: u64,
    width: u32,
    height: u32 }

impl TimelineViewKey {
    fn capture(project: &Project, revision: u64, width: f32, height: f32) -> Self {
        Self {
            revision,
            zoom: project.sequence.zoom.to_bits(),
            scroll: project.sequence.scroll.to_bits(),
            width: width.to_bits(),
            height: height.to_bits() }
    }
}

fn playback_scroll(scroll: f64, play: f64, span: f64) -> f64 {
    if !scroll.is_finite() || !play.is_finite() || !span.is_finite() || span <= 0.0 {
        return scroll;
    }
    let margin = (span * 0.03).min(2.0);
    if play < scroll {
        (play - span * 0.8).max(0.0)
    } else if play > scroll + span - margin {
        (play - span * 0.2).max(0.0)
    } else {
        scroll
    }
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Hash)]
enum JobStatus {
    Pending,
    Rendering,
    Complete,
    Failed,
    Cancelled }

struct ExportJob {
    id: i32,
    project: Project,
    settings: ExportSettings,
    status: JobStatus,
    message: String }

enum ImportEvent {
    Media(Media),
    Relink(u64, Media),
    Error(String),
    Done }

#[derive(Clone, Copy)]
struct MediaDrag {
    id: i32,
    x: f32,
    y: f32,
    range: SourceRange }

#[derive(Clone, Copy)]
struct PendingInsert {
    media: u64,
    range: SourceRange,
    at: i64,
    overwrite: bool,
    track: Option<usize>,
    format: (u32, u32, FrameRate) }

struct AppState {
    optimized_cache: Arc<playback::optimized::OptimizedCache>,
    preview_cache_status_at: Instant,
    background_idle_after: Instant,
    edit_hover_key: Option<(u64, u64, u64, u32, u32, u32, u32, u32, bool, bool)>,
    edit_hover_selection: Vec<u64>,
    razor_hover: Option<(f32, f32)>,
    razor_hover_key: Option<(u64, i64, u64, u64, u32, u32, u32, u32, u32, bool)>,
    export_eta_at: Option<Instant>,
    program_request_pending: bool,
    source_request_pending: bool,
    program_feedback: viewer_status::ViewerStatus,
    source_feedback: viewer_status::ViewerStatus,
    program_picture_reset: bool,
    preview_telemetry: crate::playback::telemetry::PreviewTelemetry,
    editor: timeline::Editor,
    preview: PreviewEngine,
    source_preview: PreviewEngine,
    source_token: u64,
    source_min_token: u64,
    source_key: Option<(Media, FrameRate)>,
    source_project: Option<Project>,
    source_requested: Option<(i64, u32, u32)>,
    source_intent: PreviewIntent,
    program_intent: PreviewIntent,
    source_presented: Option<(u64, i64)>,
    source: SourceTransport,
    source_usable: bool,
    source_error: Option<String>,
    thumbnails: ThumbnailEngine,
    thumbnail_cache: VecDeque<(Media, Option<slint::Image>)>,
    thumbnail_pending: Vec<Media>,
    thumbnail_schedule_key: Option<ThumbnailScheduleKey>,
    waveform_engine: playback::timeline_waveforms::WaveformEngine,
    filmstrips: filmstrips::UiFilmstrips,
    waveform_results: VecDeque<(Media, Arc<playback::timeline_waveforms::WaveformPeaks>)>,
    waveform_view_key: Option<(u64, u64, u64, u32, u32, u32)>,
    waveform_scroll_y: f32,
    preferences: Preferences,
    palette_query: Option<String>,
    binding_query: Option<String>,
    audio: AudioEngine,
    playing: bool,
    waiting_audio: bool,
    waiting_picture: bool,
    speed: f64,
    anchor_frame: i64,
    anchor_at: Instant,
    view_w: f32,
    view_h: f32,
    timeline_w: f32,
    track_body: f32,
    preview_quality: i32,
    preview_size: (u32, u32),
    edit_gen: u64,
    picture_gen: u64,
    sent_gen: u64,
    shown_frame: i64,
    presented_picture: Option<(u64, i64)>,
    syncing: bool,
    status: String,
    pointer: Pointer,
    viewer_last: Option<(f32, f32)>,
    export_rx: Option<Receiver<ExportProgress>>,
    export_cancel: Arc<AtomicBool>,
    export_running: bool,
    export_jobs: Vec<ExportJob>,
    export_workers: Vec<std::thread::JoinHandle<()>>,
    export_job_id: Option<i32>,
    next_export_id: i32,
    queue_active: bool,
    preview_token: u64,
    preview_min_token: u64,
    timeline_view_key: Option<TimelineViewKey>,
    query: String,
    filter_kind: i32,
    inspector_seen: i32,
    import_rx: Option<Receiver<ImportEvent>>,
    import_count: usize,
    import_errors: usize,
    relinking: bool,
    pending_imports: VecDeque<PathBuf>,
    timeline_import_path: Option<PathBuf>,
    import_report_text: String,
    update_available: bool,
    update_tooltip: String,
    update_agree_tag: Option<String>,
    update_agree_url: Option<String>,
    update_agree_title: String,
    update_agree_body: String,
    version_rows: Vec<VersionRow>,
    versions_status: String,
    versions_busy: bool,
    media_drag_origin: Option<MediaDrag>,
    pending_insert: Option<PendingInsert>,
    sequence_match_offered: bool,
    current_bin: String,
    media_selection: media_selection::MediaSelection,
    media_sort: media_selection::MediaSort,
    native_events: Option<crate::platform_drag_drop::FileDropQueue>,
    live: Option<live::LiveState> }

thread_local! {
    static PREVIEW_UI_STATE: std::cell::RefCell<std::rc::Weak<std::cell::RefCell<AppState>>> =
        const { std::cell::RefCell::new(std::rc::Weak::new()) };
}

fn install_preview_wakes(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    PREVIEW_UI_STATE.with(|slot| *slot.borrow_mut() = Rc::downgrade(state));
    for source in [false, true] {
        // Slint's Weak is Send, but intentionally not Sync. The callback is
        // shared with a worker; serialize cloning and upgrade only on the UI.
        let weak_ui = Arc::new(std::sync::Mutex::new(ui.as_weak()));
        let scheduled = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(move || {
            if scheduled.swap(true, Ordering::AcqRel) { return; }
            let weak_ui = weak_ui.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let pending = scheduled.clone();
            if slint::invoke_from_event_loop(move || {
                pending.store(false, Ordering::Release);
                let Some(ui) = weak_ui.upgrade() else { return; };
                if ui.get_live_reloading() { return; }
                PREVIEW_UI_STATE.with(|slot| {
                    if let Some(state) = slot.borrow().upgrade() {
                        if source { pump_source_preview(&ui, &state); }
                        else { pump_preview(&ui, &state); }
                    }
                });
            }).is_err() { scheduled.store(false, Ordering::Release); }
        });
        if source { state.borrow().source_preview.set_wake_callback(wake); }
        else { state.borrow().preview.set_wake_callback(wake); }
    }
}

impl AppState {
    fn new() -> Self {
        let mut editor = timeline::Editor::new();
        let preferences = Preferences::load();
        editor.project.shortcut_profile = preferences.profile();
        let data = std::env::var_os("QUATERNIO_DATA_DIR").map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_exe().ok().and_then(|p| p.parent().map(|p| p.join("data")))
                .unwrap_or_else(|| PathBuf::from("data")));
        let optimized_cache = playback::optimized::OptimizedCache::new(data.join("preview-cache"), preferences.preview_cache.clone());
        let preview = PreviewEngine::new();
        let source_preview = PreviewEngine::new();
        preview.set_optimized_cache(Some(optimized_cache.clone()));
        source_preview.set_optimized_cache(Some(optimized_cache.clone()));
        Self {
            optimized_cache,
            preview_cache_status_at: Instant::now() - std::time::Duration::from_secs(2),
            background_idle_after: Instant::now(),
            edit_hover_key: None,
            edit_hover_selection: Vec::new(),
            razor_hover: None,
            razor_hover_key: None,
            export_eta_at: None,
            program_request_pending: false,
            source_request_pending: false,
            program_feedback: viewer_status::ViewerStatus::default(),
            source_feedback: viewer_status::ViewerStatus::default(),
            program_picture_reset: false,
            preview_telemetry: crate::playback::telemetry::PreviewTelemetry::new(),
            editor,
            preview,
            source_preview,
            source_token: 0,
            source_min_token: 0,
            source_key: None,
            source_project: None,
            source_requested: None,
            source_intent: PreviewIntent::FinalSeek,
            program_intent: PreviewIntent::FinalSeek,
            source_presented: None,
            source: SourceTransport::default(),
            source_usable: false,
            source_error: None,
            thumbnails: ThumbnailEngine::new(),
            thumbnail_cache: VecDeque::new(),
            thumbnail_pending: Vec::new(),
            thumbnail_schedule_key: None,
            waveform_engine: playback::timeline_waveforms::WaveformEngine::new(),
            filmstrips: filmstrips::UiFilmstrips::default(),
            waveform_results: VecDeque::new(),
            waveform_view_key: None,
            waveform_scroll_y: 0.0,
            preferences,
            palette_query: None,
            binding_query: None,
            audio: AudioEngine::new(),
            playing: false,
            waiting_audio: false,
            waiting_picture: false,
            speed: 0.0,
            anchor_frame: 0,
            anchor_at: Instant::now(),
            view_w: 640.0,
            view_h: 360.0,
            timeline_w: 800.0,
            track_body: 0.0,
            preview_quality: 0,
            preview_size: (0, 0),
            edit_gen: 1,
            picture_gen: 1,
            sent_gen: 0,
            shown_frame: -1,
            presented_picture: None,
            syncing: false,
            status: "Idle".into(),
            inspector_seen: -1,
            pointer: Pointer::default(),
            viewer_last: None,
            export_rx: None,
            export_cancel: Arc::new(AtomicBool::new(false)),
            export_running: false,
            export_jobs: Vec::new(),
            export_workers: Vec::new(),
            export_job_id: None,
            next_export_id: 1,
            queue_active: false,
            preview_token: 0,
            preview_min_token: 0,
            timeline_view_key: None,
            query: String::new(),
            filter_kind: 0,
            import_rx: None,
            import_count: 0,
            import_errors: 0,
            relinking: false,
            pending_imports: VecDeque::new(),
            timeline_import_path: None,
            import_report_text: String::new(),
            update_available: false,
            update_tooltip: String::new(),
            update_agree_tag: None,
            update_agree_url: None,
            update_agree_title: String::new(),
            update_agree_body: String::new(),
            version_rows: Vec::new(),
            versions_status: String::new(),
            versions_busy: false,
            media_drag_origin: None,
            pending_insert: None,
            sequence_match_offered: false,
            current_bin: String::new(),
            media_selection: media_selection::MediaSelection::default(),
            media_sort: media_selection::MediaSort::default(),
            native_events: None,
            live: None }
    }
}

pub fn run() -> anyhow::Result<()> {
    let ui = AppWindow::new()?;
    let state = Rc::new(std::cell::RefCell::new(AppState::new()));
    install_preview_wakes(&ui, &state);
    let file_drops = crate::platform_drag_drop::FileDropQueue::attach(ui.window());
    state.borrow_mut().native_events = Some(file_drops.clone());
    let recovered = live::initialize(&ui, &state).map_err(anyhow::Error::msg)?;
    if let Some(path) = std::env::args_os().nth(1).map(PathBuf::from) {
        if path.extension().and_then(|ext| ext.to_str()) == Some("quaternio") {
            match Project::load_from(&path) {
                Ok(project) => {
                    let mut current = state.borrow_mut();
                    current.editor = timeline::Editor::from_project(project);
                    current.editor.path = Some(path);
                    current.status = "Opened project".into();
                }
                Err(error) => state.borrow_mut().status = error }
        }
    }
    set_preview_default(&ui, &state);
    wire(&ui, &state);
    ui.set_app_version(SharedString::from(crate::update::CURRENT_VERSION));
    ui.set_version_rows(model(Vec::<VersionRow>::new()));
    start_background_update_check(&ui, &state);
    if let Some(layout) = state.borrow().preferences.current_layout.clone() {
        apply_layout(&ui, &layout);
    }
    full_sync(&ui, &state);
    if let Some(recovered) = recovered {
        live::restore_view(&ui, &state, recovered);
        full_sync(&ui, &state);
        live::restore_ui_after_sync(&ui, &state);
    }
    if std::env::args().any(|arg| arg == "--show-commands") {
        on_command(&ui, &state, "commands");
    } else if std::env::args().any(|arg| arg == "--show-preferences") {
        on_command(&ui, &state, "preferences");
    } else if std::env::args().any(|arg| arg == "--show-export") {
        on_command(&ui, &state, "export-view");
    } else if std::env::args().any(|arg| arg == "--show-source") {
        let first = state
            .borrow()
            .editor
            .project
            .media
            .first()
            .map(|media| media.id);
        state.borrow_mut().editor.selected_media = first;
        focus_panel(&ui, 0);
        full_sync(&ui, &state);
        pump_source_preview(&ui, &state);
    }
    if std::env::args().any(|arg| arg == "--show-sequence-match") {
        let media = state
            .borrow()
            .editor
            .project
            .media
            .first()
            .map(|media| media.id);
        state.borrow_mut().editor.selected_media = media;
        full_sync(&ui, &state);
        insert_selected_with_offer(&ui, &state, false);
    }
    let weak = ui.as_weak();
    let timer_state = state.clone();
    let fitted = Rc::new(Cell::new(false));
    let timer = Timer::default();
    timer.start(
        TimerMode::Repeated,
        std::time::Duration::from_millis(16),
        move || {
            if let Some(ui) = weak.upgrade() {
                if !fitted.replace(true) {
                    ui.window().set_maximized(true);
                }
                if ui.get_live_reloading() {
                    live::tick(&ui, &timer_state);
                    return;
                }
                ui.set_file_drop_hover(file_drops.is_hovering_files());
                let paths = file_drops.take_paths();
                if !paths.is_empty() {
                    handle_dropped_files(&ui, &timer_state, paths);
                }
                let overflow = file_drops.take_overflow_count();
                if overflow > 0 {
                    set_status(
                        &timer_state,
                        format!("File drop limit reached; {overflow} files skipped"),
                    );
                }
                tick(&ui, &timer_state);
                live::tick(&ui, &timer_state);
            }
        },
    );
    ui.run()?;
    Ok(())
}

fn sync_preview_cache_status(ui: &AppWindow, cache: &playback::optimized::OptimizedCache) {
    let status = cache.status();
    let label = if status.starts_with("Clearing") { "Clearing previews" }
        else if status.starts_with("Preparing ") { "Preparing previews" }
        else if status.starts_with("Preparation paused") { "Previews paused" }
        else if status.starts_with("Automatic previews off") { "Previews off" }
        else if status.starts_with("Optimized previews ready") { "Previews ready" }
        else if status.starts_with("Using originals") { "Using originals" }
        else if status.contains("unavailable") || status.contains("limit reached") { "Preview cache full / unavailable" }
        else { "Previews auto" };
    ui.set_preview_cache_label(label.into());
    ui.set_preview_cache_status(status.into());
}

fn wire(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let st = state.clone();
    let weak = ui.as_weak();
    ui.on_timecode_begin(move |source| {
        if let Some(ui) = weak.upgrade() { navigation::begin(&ui, &st, source); }
    });
    let st = state.clone();
    let weak = ui.as_weak();
    ui.on_timecode_seek(move |source, text| {
        weak.upgrade().is_some_and(|ui| navigation::seek(&ui, &st, source, &text))
    });
    let cache_ui = ui.as_weak();
    let st = state.clone();
    ui.on_preview_cache_settings(move || {
        let Some(ui) = cache_ui.upgrade() else { return; };
        let mut current = st.borrow_mut();
        let settings = playback::optimized::PreviewCacheSettings {
            automatic: ui.get_preview_cache_auto(), paused: ui.get_preview_cache_paused(),
            limit_gb: ui.get_preview_cache_limit_gb().clamp(1, 100) as u32 };
        current.optimized_cache.configure(settings.clone());
        current.preferences.preview_cache = settings;
        if let Err(error) = current.preferences.save() { current.status = format!("Preferences: {error}"); }
        sync_preview_cache_status(&ui, &current.optimized_cache);
    });
    let cache_ui = ui.as_weak();
    let st = state.clone();
    ui.on_preview_cache_clear(move || {
        let mut current = st.borrow_mut();
        current.optimized_cache.clear();
        // Process this control even when the viewer is idle, releasing helper
        // and CLI handles before Windows removes generated segments.
        current.preview.set_optimized_cache(Some(current.optimized_cache.clone()));
        current.source_preview.set_optimized_cache(Some(current.optimized_cache.clone()));
        current.shown_frame = -1;
        current.source_requested = None;
        if let Some(ui) = cache_ui.upgrade() { sync_preview_cache_status(&ui, &current.optimized_cache); }
    });
    let close_ui = ui.as_weak();
    let close_state = state.clone();
    ui.window().on_close_requested(move || {
        if let Some(ui) = close_ui.upgrade() {
            if close_state.borrow().live.is_some() {
                live::close(&ui, &close_state);
                return slint::CloseRequestResponse::KeepWindowShown;
            }
            if !confirm_unsaved(&ui, &close_state) {
                return slint::CloseRequestResponse::KeepWindowShown;
            }
            close_state.borrow().audio.stop();
            persist_current_layout(&ui, &close_state);
            close_state
                .borrow()
                .export_cancel
                .store(true, Ordering::Relaxed);
        }
        slint::CloseRequestResponse::HideWindow
    });
    let ui_cmd = ui.as_weak();
    let st = state.clone();
    ui.on_command(move |command| {
        if let Some(ui) = ui_cmd.upgrade() {
            on_command(&ui, &st, command.as_str());
        }
    });

    let st = state.clone();
    ui.on_shortcut_hint(move |action, _revision| binding_hint(&st, action.as_str()));

    let palette_ui = ui.as_weak();
    let st = state.clone();
    ui.on_palette_run(move |action| {
        if let Some(ui) = palette_ui.upgrade() {
            if action_enabled(&ui, &st.borrow(), action.as_str()) {
                ui.set_show_command_palette(false);
                ui.set_open_menu(0);
                on_command(&ui, &st, action.as_str());
            }
        }
    });

    let binding_ui = ui.as_weak();
    let st = state.clone();
    ui.on_binding_edit(move |action| {
        if let Some(ui) = binding_ui.upgrade() {
            if let Some(item) = ACTIONS.iter().find(|item| item.id == action.as_str()) {
                ui.set_binding_action(action);
                ui.set_binding_label(item.label.into());
                ui.set_binding_message(
                    "Press a shortcut. Esc cancels; Backspace removes it.".into(),
                );
                ui.set_capturing_binding(true);
            }
        }
    });
    let binding_ui = ui.as_weak();
    let st = state.clone();
    {
        let weak = ui.as_weak();
        let st = state.clone();
        ui.on_version_install(move |tag| {
            if let Some(ui) = weak.upgrade() {
                open_update_agree_for_tag(&ui, &st, tag.as_str());
            }
        });
    }
    {
        let st = state.clone();
        ui.on_version_open(move |url| {
            match crate::update::open_in_browser(url.as_str()) {
                Ok(()) => set_status(&st, "Opened download page".into()),
                Err(err) => set_status(&st, err),
            }
        });
    }
    ui.on_binding_reset(move |action| {
        if let Some(ui) = binding_ui.upgrade() {
            let conflict = {
                let current = st.borrow();
                let profile = current.editor.project.shortcut_profile;
                ACTIONS
                    .iter()
                    .find(|item| item.id == action.as_str())
                    .and_then(|reset| {
                        let binding = reset.default_binding(profile);
                        ACTIONS
                            .iter()
                            .find(|other| {
                                other.id != reset.id
                                    && !binding.is_empty()
                                    && current.preferences.binding(other, profile) == binding
                            })
                            .map(|other| other.label)
                    })
            };
            if let Some(conflict) = conflict {
                ui.set_binding_message(
                    format!("Reset would conflict with {conflict}. Reassign that shortcut first.")
                        .into(),
                );
                return;
            }
            st.borrow_mut()
                .preferences
                .keybindings
                .remove(action.as_str());
            save_preferences(&st);
            sync_keybindings(&ui, &st);
            full_sync(&ui, &st);
        }
    });

    let bin_ui = ui.as_weak();
    let st = state.clone();
    ui.on_create_bin(move |name| {
        if let Some(ui) = bin_ui.upgrade() {
            let mut created = None;
            let result = st.borrow_mut().editor.edit("create-bin", |project| {
                created = Some(project.create_bin(name.as_str())?);
                Ok(())
            });
            match result {
                Ok(()) => {
                    st.borrow_mut().current_bin = created.unwrap_or_default();
                    set_status(&st, "Created project bin".into());
                }
                Err(error) => set_status(&st, error) }
            full_sync(&ui, &st);
        }
    });
    let bin_ui = ui.as_weak();
    let st = state.clone();
    ui.on_choose_bin(move |bin| {
        if let Some(ui) = bin_ui.upgrade() {
            if bin.is_empty()
                || st
                    .borrow()
                    .editor
                    .project
                    .bins
                    .iter()
                    .any(|name| name == bin.as_str())
            {
                st.borrow_mut().current_bin = bin.to_string();
            }
            full_sync(&ui, &st);
        }
    });
    let bin_ui = ui.as_weak();
    let st = state.clone();
    ui.on_move_to_bin(move |bin| {
        if let Some(ui) = bin_ui.upgrade() {
            sync_media_selection(&mut st.borrow_mut());
            let ids = st.borrow().media_selection.ids.clone();
            let result = media_selection::move_to_bin(&mut st.borrow_mut().editor, &ids, bin.as_str());
            match result {
                Ok(count) => set_status(&st, format!("Moved {count} media item{} to {}", if count == 1 { "" } else { "s" }, if bin.is_empty() { "Unsorted" } else { bin.as_str() })),
                Err(error) => set_status(&st, error) }
            full_sync(&ui, &st);
        }
    });

    let media_ui = ui.as_weak();
    let st = state.clone();
    ui.on_select_media(move |id, ctrl, shift| {
        if let Some(ui) = media_ui.upgrade() { select_project_media(&ui, &st, id as u64, ctrl, shift); }
    });
    let media_ui = ui.as_weak();
    let st = state.clone();
    ui.on_sort_media(move |column| {
        st.borrow_mut().media_sort.choose(column);
        if let Some(ui) = media_ui.upgrade() { full_sync(&ui, &st); }
    });

    let ui_adj = ui.as_weak();
    let st = state.clone();
    ui.on_adjust(move |field, value| {
        if let Some(ui) = ui_adj.upgrade() {
            on_adjust(&ui, &st, field.as_str(), value);
        }
    });

    let ui_mix = ui.as_weak();
    let st = state.clone();
    ui.on_mix_track(move |index, field, value| {
        if let Some(ui) = ui_mix.upgrade() {
            let kind = if field.as_str() == "pan" {
                "mix-pan"
            } else {
                "mix-gain"
            };
            let target = st
                .borrow()
                .editor
                .project
                .sequence
                .tracks
                .get(index as usize)
                .map(|track| track.id);
            if let Some(target) = target {
                st.borrow_mut()
                    .editor
                    .begin_control_edit(target, field.as_str());
            }
            let _ = st.borrow_mut().editor.edit(kind, |project| {
                let Some(track) = project.sequence.tracks.get_mut(index as usize) else {
                    return Ok(());
                };
                match field.as_str() {
                    "gain" => track.gain = value.clamp(0.0, 2.0),
                    "pan" => track.pan = value.clamp(-1.0, 1.0),
                    _ => {}
                }
                Ok(())
            });
            audio_mix::note_audio(&st);
            audio_mix::sync_controls(&ui, &st);
        }
    });

    let ui_pick = ui.as_weak();
    let st = state.clone();
    ui.on_pick(move |kind, id| {
        if let Some(ui) = ui_pick.upgrade() {
            on_pick(&ui, &st, kind.as_str(), id);
        }
    });

    let hover_ui = ui.as_weak();
    let hover_state = state.clone();
    ui.on_razor_hover(move |inside, x, y| {
        hover_state.borrow_mut().razor_hover = inside.then_some((x, y));
        if let Some(ui) = hover_ui.upgrade() { sync_razor_hover(&ui, &hover_state); sync_edit_feedback(&ui, &hover_state); }
    });

    let ui_tl = ui.as_weak();
    let st = state.clone();
    ui.on_timeline(move |zone, phase, x, y, shift, _ctrl| {
        if let Some(ui) = ui_tl.upgrade() {
            on_timeline(&ui, &st, zone, phase, x, y, shift);
        }
    });

    let ui_wheel = ui.as_weak();
    let st = state.clone();
    ui.on_wheel(move |delta, x, ctrl| {
        if let Some(ui) = ui_wheel.upgrade() {
            on_wheel(&ui, &st, delta, x, ctrl);
        }
    });

    let ui_view = ui.as_weak();
    let st = state.clone();
    ui.on_viewer(move |phase, x, y| {
        if let Some(ui) = ui_view.upgrade() {
            on_viewer(&ui, &st, phase, x, y);
        }
    });

    let source_ui = ui.as_weak();
    let st = state.clone();
    ui.on_source_scrub(move |phase, position| {
        if let Some(ui) = source_ui.upgrade() {
            on_source_scrub(&ui, &st, phase, position);
        }
    });

    let match_ui = ui.as_weak();
    let st = state.clone();
    ui.on_sequence_match_choice(move |choice| {
        if let Some(ui) = match_ui.upgrade() {
            finish_sequence_match(&ui, &st, choice);
        }
    });

    let drag_ui = ui.as_weak();
    let st = state.clone();
    ui.on_media_drag(move |id, phase, x, y| {
        if let Some(ui) = drag_ui.upgrade() {
            on_media_drag(&ui, &st, id, phase, x, y);
        }
    });

    let ui_resize = ui.as_weak();
    let st = state.clone();
    ui.on_resized(move |vw, vh, tw, th| {
        let (body_changed, view_changed) = {
            let mut state = st.borrow_mut();
            let mut view_changed = false;
            if vw > 20.0 && ((vw - state.view_w).abs() > 1.0 || (vh - state.view_h).abs() > 1.0) {
                state.view_w = vw;
                state.view_h = vh;
                view_changed = true;
            }
            if tw > 20.0 {
                state.timeline_w = tw;
            }
            let changed = th > 8.0 && (th - state.track_body).abs() > 1.0;
            if th > 8.0 {
                state.track_body = th;
            }
            if view_changed {
                state.shown_frame = -1;
            }
            (changed, view_changed)
        };
        if body_changed {
            if let Some(ui) = ui_resize.upgrade() {
                refresh_tracks(&ui, &st);
            }
        }
    });

    let ui_key = ui.as_weak();
    let st = state.clone();
    ui.on_key_cmd(move |key, ctrl, shift, alt| {
        if let Some(ui) = ui_key.upgrade() {
            on_key(&ui, &st, key.as_str(), ctrl, shift, alt);
        }
    });
}

fn on_key(
    ui: &AppWindow,
    state: &Rc<std::cell::RefCell<AppState>>,
    key: &str,
    ctrl: bool,
    shift: bool,
    alt: bool,
) {
    if ui.get_live_reloading() {
        return;
    }
    if ui.get_show_sequence_match() {
        if key == "escape" {
            finish_sequence_match(ui, state, -1);
        }
        return;
    }
    if ui.get_capturing_binding() {
        capture_binding(ui, state, key, Mods { ctrl, shift, alt });
        return;
    }
    if ui.get_show_command_palette() {
        sync_commands(ui, state);
        if key == "palette-enter" || key == "return" {
            let queued = state
                .borrow()
                .native_events
                .as_ref()
                .map(|queue| queue.take_palette_keys())
                .unwrap_or_default();
            for key in queued {
                palette_step(ui, if key == "palette-up" { -1 } else { 1 });
            }
        }
        match key {
            "escape" => ui.set_show_command_palette(false),
            "palette-up" | "up" => palette_step(ui, -1),
            "palette-down" | "down" => palette_step(ui, 1),
            "palette-enter" | "return" => {
                if let Some(item) = ui
                    .get_command_items()
                    .row_data(ui.get_command_selection().max(0) as usize)
                {
                    if item.enabled {
                        ui.set_show_command_palette(false);
                        on_command(ui, state, item.action.as_str());
                    }
                }
            }
            _ => {}
        }
        return;
    }
    if ui.get_show_dialog() || ui.get_show_prefs() || ui.get_show_about() {
        if key == "escape" {
            ui.set_show_dialog(false);
            ui.set_show_prefs(false);
            ui.set_show_about(false);
        }
        return;
    }
    // Explicit user assignments take precedence over contextual convenience
    // keys, including Project Enter and the optional Vim navigation layer.
    let chord = commands::chord(key, Mods { ctrl, shift, alt });
    let custom_action = {
        let current = state.borrow();
        ACTIONS
            .iter()
            .find(|action| {
                current
                    .preferences
                    .keybindings
                    .get(action.id)
                    .is_some_and(|binding| !binding.is_empty() && binding == &chord)
            })
            .map(|action| action.id)
    };
    if let Some(action) = custom_action {
        if action_enabled(ui, &state.borrow(), action) {
            on_command(ui, state, action);
        }
        return;
    }
    if ui.get_active_panel() == 2
        && !ctrl
        && !alt
        && !shift
        && matches!(key, "up" | "down" | "return")
    {
        if key == "return" {
            on_command(ui, state, "insert");
        } else {
            navigate_media(ui, state, if key == "up" { -1 } else { 1 });
        }
        return;
    }
    if ui.get_active_panel() == 0 && key == "return" && !ctrl && !alt {
        on_command(ui, state, "insert");
        return;
    }
    if state.borrow().preferences.vim_navigation && ui.get_active_panel() == 3 && !ctrl && !alt {
        let action = match key.to_ascii_lowercase().as_str() {
            "h" => Some(if shift {
                "step-back-second"
            } else {
                "step-back"
            }),
            "l" => Some(if shift { "step-fwd-second" } else { "step-fwd" }),
            "j" => Some("clip-next"),
            "k" => Some("clip-prev"),
            "g" => Some(if shift { "end" } else { "home" }),
            _ => None };
        if let Some(action) = action {
            on_command(ui, state, action);
            return;
        }
    }
    let action = {
        let current = state.borrow();
        let profile = current.editor.project.shortcut_profile;
        ACTIONS
            .iter()
            .find(|action| current.preferences.binding(action, profile) == chord)
            .map(|action| action.id)
    };
    if let Some(action) = action {
        if action_enabled(ui, &state.borrow(), action) {
            on_command(ui, state, action);
        }
        return;
    }
    let alias = match chord.as_str() {
        "ctrl+y" => Some("redo"),
        "ctrl+=" | "ctrl++" | "+" => Some("zoom-in"),
        "ctrl+-" => Some("zoom-out"),
        "backspace"
            if state.borrow().editor.project.shortcut_profile == ShortcutProfile::Premiere =>
        {
            Some("lift")
        }
        _ => None };
    if let Some(action) = alias {
        if !state.borrow().preferences.keybindings.contains_key(action) {
            on_command(ui, state, action);
        }
    }
}

fn viewer_action(id: &str) -> bool {
    matches!(
        id,
        "play-toggle"
            | "shuttle-j"
            | "shuttle-k"
            | "shuttle-l"
            | "step-back"
            | "step-fwd"
            | "step-back-second"
            | "step-fwd-second"
            | "mark-in"
            | "mark-out"
            | "clear-io"
            | "goto-in"
            | "goto-out"
            | "home"
            | "end"
    )
}

fn action_enabled(ui: &AppWindow, state: &AppState, id: &str) -> bool {
    match id {
        "undo" => return state.editor.can_undo(),
        "redo" => return state.editor.can_redo(),
        _ => {}
    }
    if viewer_action(id) && ui.get_active_panel() == 0 {
        return state
            .editor
            .selected_media
            .and_then(|id| state.editor.project.media(id))
            .is_some_and(|media| state.source.media_id == Some(media.id) && state.source_usable);
    }
    let Some(action) = ACTIONS.iter().find(|action| action.id == id) else {
        return true;
    };
    match action.scope {
        Scope::Always => true,
        Scope::Selection => !state.editor.selected_clips.is_empty(),
        Scope::Multiple => state.editor.selected_clips.len() >= 2,
        Scope::Media => state
            .editor
            .selected_media
            .and_then(|id| state.editor.project.media(id))
            .is_some(),
        Scope::Clips => state
            .editor
            .project
            .sequence
            .tracks
            .iter()
            .any(|track| !track.clips.is_empty()) }
}

fn sync_commands(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let query = ui.get_command_query().to_lowercase();
    let changed = state.borrow().palette_query.as_ref() != Some(&query);
    state.borrow_mut().palette_query = Some(query.clone());
    let rows = {
        let current = state.borrow();
        let source = ui.get_active_panel() == 0;
        let profile = current.editor.project.shortcut_profile;
        ACTIONS
            .iter()
            .filter(|action| {
                query.split_whitespace().all(|word| {
                    action_label(action, source).to_lowercase().contains(word)
                        || action.id.contains(word)
                })
            })
            .map(|action| {
                let binding = current.preferences.binding(action, profile);
                CommandItem {
                    action: action.id.into(),
                    label: action_label(action, source).into(),
                    hint: if binding.is_empty() {
                        "".into()
                    } else {
                        commands::display_binding(binding).into()
                    },
                    enabled: action_enabled(ui, &current, action.id) }
            })
            .collect::<Vec<_>>()
    };
    let selection = if changed {
        rows.iter().position(|row| row.enabled)
    } else {
        let selected = ui.get_command_selection().max(0) as usize;
        if rows.get(selected).is_some_and(|row| row.enabled) {
            Some(selected)
        } else {
            rows.iter().position(|row| row.enabled)
        }
    };
    ui.set_command_selection(selection.unwrap_or(0) as i32);
    ui.set_command_items(model(rows));
}

fn action_label(action: &commands::Action, source: bool) -> String {
    if source && viewer_action(action.id) {
        format!(
            "Source: {}",
            action
                .label
                .split_once(": ")
                .map_or(action.label, |(_, label)| label)
        )
    } else {
        action.label.to_string()
    }
}

fn palette_step(ui: &AppWindow, direction: i32) {
    let rows = ui.get_command_items();
    let count = rows.row_count() as i32;
    if count == 0 {
        return;
    }
    for offset in 1..=count {
        let next = (ui.get_command_selection() + direction * offset).rem_euclid(count);
        if rows.row_data(next as usize).is_some_and(|row| row.enabled) {
            ui.set_command_selection(next);
            break;
        }
    }
}

fn sync_keybindings(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let query = ui.get_keybinding_query().to_lowercase();
    state.borrow_mut().binding_query = Some(query.clone());
    let current = state.borrow();
    let profile = current.editor.project.shortcut_profile;
    ui.set_vim_navigation(current.preferences.vim_navigation);
    let rows = ACTIONS
        .iter()
        .filter(|action| {
            query.split_whitespace().all(|word| {
                action.label.to_lowercase().contains(word)
                    || commands::display_binding(current.preferences.binding(action, profile))
                        .to_lowercase()
                        .contains(word)
            })
        })
        .map(|action| KeyBindingRow {
            action: action.id.into(),
            label: action.label.into(),
            binding: commands::display_binding(current.preferences.binding(action, profile)).into(),
            modified: current.preferences.keybindings.contains_key(action.id) })
        .collect();
    ui.set_keybinding_items(model(rows));
}

fn capture_binding(
    ui: &AppWindow,
    state: &Rc<std::cell::RefCell<AppState>>,
    key: &str,
    mods: Mods,
) {
    if key == "escape" {
        ui.set_capturing_binding(false);
        return;
    }
    if key.is_empty()
        || key.chars().any(char::is_control)
        || matches!(
            key.to_lowercase().as_str(),
            "shift" | "control" | "ctrl" | "alt" | "meta"
        )
    {
        return;
    }
    let action_id = ui.get_binding_action().to_string();
    if !ACTIONS.iter().any(|action| action.id == action_id) {
        ui.set_capturing_binding(false);
        return;
    }
    let binding = if key == "backspace" && !mods.ctrl && !mods.shift && !mods.alt {
        String::new()
    } else {
        commands::chord(key, mods)
    };
    let conflict = {
        let current = state.borrow();
        let profile = current.editor.project.shortcut_profile;
        ACTIONS
            .iter()
            .find(|action| {
                action.id != action_id
                    && !binding.is_empty()
                    && current.preferences.binding(action, profile) == binding
            })
            .map(|action| action.label)
    };
    if let Some(conflict) = conflict {
        ui.set_binding_message(
            format!("Already assigned to {conflict}. Change that command first, or press Esc.")
                .into(),
        );
        return;
    }
    state
        .borrow_mut()
        .preferences
        .keybindings
        .insert(action_id, binding);
    save_preferences(state);
    ui.set_capturing_binding(false);
    sync_keybindings(ui, state);
    full_sync(ui, state);
}

fn save_preferences(state: &Rc<std::cell::RefCell<AppState>>) {
    let result = state.borrow().preferences.save();
    if let Err(error) = result {
        set_status(state, format!("Could not save preferences: {error}"));
    }
}

fn read_layout(ui: &AppWindow) -> Layout {
    Layout {
        upper_ratio: ui.get_upper_ratio(),
        source_width: ui.get_source_width(),
        properties_width: ui.get_properties_width(),
        project_width: ui.get_project_width(),
        workspace: ui.get_workspace(),
        project_top: ui.get_project_top() }
}

fn apply_layout(ui: &AppWindow, layout: &Layout) {
    ui.set_upper_ratio(layout.upper_ratio.clamp(0.2, 0.8));
    // The UI clamps displayed widths to the current viewport. Keep the stored
    // widths so a layout made on a wide screen survives save and restore.
    ui.set_source_width(if layout.source_width == 0.0 { 0.0 } else { layout.source_width.clamp(240.0, 10000.0) });
    ui.set_properties_width(layout.properties_width.clamp(260.0, 10000.0));
    ui.set_project_width(layout.project_width.clamp(240.0, 10000.0));
    ui.set_workspace(layout.workspace.clamp(0, 2));
    ui.set_project_top(layout.project_top);
    ui.set_page(0);
}

fn focus_panel(ui: &AppWindow, panel: i32) {
    ui.set_page(0);
    if (panel == 0 && ui.get_workspace() == 2)
        || ((panel == 1 || panel == 4) && ui.get_workspace() == 1)
    {
        ui.set_workspace(0);
    }
    ui.invoke_focus_pane(panel);
}

fn persist_current_layout(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    export_form::remember(ui, state);
    state.borrow_mut().preferences.current_layout = Some(read_layout(ui));
    save_preferences(state);
}

fn navigate_media(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, direction: i32) {
    let ids = bin_views(&state.borrow())
        .iter()
        .map(|item| item.id as u64)
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return;
    }
    let selected = state.borrow().editor.selected_media;
    let index = ids
        .iter()
        .position(|id| Some(*id) == selected)
        .map(|index| (index as i32 + direction).clamp(0, ids.len() as i32 - 1) as usize)
        .unwrap_or(0);
    select_project_media(ui, state, ids[index], false, false);
}

fn sync_media_selection(state: &mut AppState) {
    state.media_selection.reconcile(&state.editor.project, state.editor.selected_media);
    let visible = visible_media(state).iter().map(|media| media.id).collect::<Vec<_>>();
    state.media_selection.retain_visible(&visible);
}

fn select_project_media(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, id: u64, ctrl: bool, shift: bool) {
    {
        let mut current = state.borrow_mut();
        sync_media_selection(&mut current);
        let visible = visible_media(&current).iter().map(|media| media.id).collect::<Vec<_>>();
        current.editor.selected_media = current.media_selection.click(id, ctrl, shift, &visible);
        let count = current.media_selection.ids.len();
        current.status = match count {
            0 => "No Project media selected".into(),
            1 => "Project media selected".into(),
            _ => format!("{count} media items selected · Move to applies to the selection; Source shows the active item") };
    }
    full_sync(ui, state);
}

fn reveal_primary_clip(state: &Rc<std::cell::RefCell<AppState>>) {
    let mut current = state.borrow_mut();
    if let Some(clip) = current
        .editor
        .primary_clip()
        .and_then(|id| current.editor.project.clip(id))
        .cloned()
    {
        let span = current.timeline_w as f64 / current.editor.project.sequence.zoom.max(0.01);
        let scroll = current.editor.project.sequence.scroll;
        if (clip.timeline_start as f64) < scroll || clip.timeline_start as f64 > scroll + span {
            current.editor.project.sequence.scroll = (clip.timeline_start - 10).max(0) as f64;
        }
    }
}

fn reveal_track(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, explicit: Option<usize>) {
    let target = {
        let current = state.borrow();
        let track = explicit.or_else(|| {
            current
                .editor
                .primary_clip()
                .and_then(|id| current.editor.project.track_index_of_clip(id))
                .or(current.editor.selected_track)
        });
        track
            .and_then(|track| {
                layout_rows(&current.editor.project.sequence, current.track_body)
                    .into_iter()
                    .find(|row| row.track_index == track)
            })
            .map(|row| {
                let scroll = ui.get_timeline_scroll_y();
                if row.y < scroll {
                    row.y
                } else if row.y + row.h > scroll + current.track_body {
                    (row.y + row.h - current.track_body).max(0.0)
                } else {
                    scroll
                }
            })
    };
    if let Some(target) = target {
        ui.invoke_scroll_tracks_to(target);
    }
}

fn on_command(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, command: &str) {
    if matches!(command, "zoom-in" | "zoom-out" | "zoom-fit") {
        state.borrow().waveform_engine.set_busy(true);
        state.borrow().filmstrips.set_busy(true);
        state.borrow().optimized_cache.set_busy(true);
        state.borrow_mut().background_idle_after = Instant::now() + std::time::Duration::from_secs(2);
    }
    if ui.get_live_reloading() {
        return;
    }
    if ui.get_show_sequence_match() {
        if command == "deselect" {
            finish_sequence_match(ui, state, -1);
        }
        return;
    }
    if command == "deselect" && state.borrow().source.scrub_origin.is_some() {
        on_source_scrub(ui, state, 3, 0.0);
        return;
    }
    if let Some(action) = command.strip_prefix("source-") {
        on_source_command(ui, state, action);
        return;
    }
    if viewer_action(command) && ui.get_active_panel() == 0 {
        on_source_command(ui, state, command);
        return;
    }
    if command == "link" || command == "unlink" {
        on_command(
            ui,
            state,
            if command == "link" {
                "link-clips"
            } else {
                "unlink-clips"
            },
        );
        return;
    }
    if matches!(
        command,
        "link-clips"
            | "unlink-clips"
            | "clip-prev"
            | "clip-next"
            | "nudge-left"
            | "nudge-right"
            | "nudge-left-10"
            | "nudge-right-10"
            | "track-up"
            | "track-down"
    ) {
        stop_play(state);
        let links = ui.get_linked_on();
        act(state, |editor| match command {
            "link-clips" => editor.link_selected(),
            "unlink-clips" => editor.unlink_selected(),
            "clip-prev" => editor.select_adjacent_clip(-1, links),
            "clip-next" => editor.select_adjacent_clip(1, links),
            "nudge-left" => editor.nudge_selected(-1, 0, links),
            "nudge-right" => editor.nudge_selected(1, 0, links),
            "nudge-left-10" => editor.nudge_selected(-10, 0, links),
            "nudge-right-10" => editor.nudge_selected(10, 0, links),
            "track-up" => editor.nudge_selected(0, -1, links),
            "track-down" => editor.nudge_selected(0, 1, links),
            _ => Ok(()) });
        ui.set_active_panel(3);
        reveal_primary_clip(state);
        full_sync(ui, state);
        reveal_track(ui, state, None);
        return;
    }
    let mapped = match command {
        "focus-controls" => { ui.invoke_focus_controls(); return; }
        "focus-menu" => { ui.invoke_focus_menu(); return; }
        "commands" => {
            ui.set_open_menu(0);
            ui.set_command_query(SharedString::default());
            state.borrow_mut().palette_query = None;
            ui.set_show_command_palette(true);
            if let Some(queue) = &state.borrow().native_events {
                queue.set_palette_active(true);
            }
            sync_commands(ui, state);
            return;
        }
        "preferences" => {
            ui.set_show_prefs(true);
            sync_keybindings(ui, state);
            return;
        }
        "reset-keybindings" => {
            state.borrow_mut().preferences.keybindings.clear();
            save_preferences(state);
            sync_keybindings(ui, state);
            full_sync(ui, state);
            return;
        }
        "vim-toggle" => {
            let enabled = !state.borrow().preferences.vim_navigation;
            state.borrow_mut().preferences.vim_navigation = enabled;
            ui.set_vim_navigation(enabled);
            save_preferences(state);
            return;
        }
        "workspace-edit" | "workspace-media" | "workspace-color" | "workspace-audio" => {
            ui.set_page(0);
            ui.set_workspace(match command {
                "workspace-media" => 1,
                "workspace-color" => 2,
                _ => 0 });
            if command == "workspace-audio" {
                ui.set_grade_tab(1);
                focus_panel(ui, 4);
            } else {
                if command == "workspace-color" {
                    ui.set_grade_tab(2);
                    ui.set_basic_open(true);
                    ui.set_wheels_open(true);
                }
                ui.invoke_focus_pane(if command == "workspace-media" { 2 } else { 1 });
            }
            return;
        }
        "panel-next" | "panel-prev" => {
            let visible: &[i32] = match ui.get_workspace() {
                1 => &[0, 2, 3],
                2 => &[1, 2, 3, 4],
                _ => &[0, 1, 2, 3, 4] };
            let current = visible
                .iter()
                .position(|panel| *panel == ui.get_active_panel())
                .unwrap_or(0) as i32;
            let index = (current + if command == "panel-next" { 1 } else { -1 })
                .rem_euclid(visible.len() as i32);
            ui.invoke_focus_pane(visible[index as usize]);
            return;
        }
        "panel-source" | "panel-program" | "panel-project" | "panel-timeline"
        | "panel-properties" => {
            focus_panel(
                ui,
                match command {
                    "panel-source" => 0,
                    "panel-program" => 1,
                    "panel-project" => 2,
                    "panel-timeline" => 3,
                    _ => 4 },
            );
            return;
        }
        "layout-save" => {
            state.borrow_mut().preferences.saved_layout = Some(read_layout(ui));
            save_preferences(state);
            set_status(state, "Saved workspace layout".into());
            ui.set_status_text(state.borrow().status.clone().into());
            return;
        }
        "layout-restore" => {
            let layout = state.borrow().preferences.saved_layout.clone();
            if let Some(layout) = layout {
                apply_layout(ui, &layout);
                set_status(state, "Restored saved layout".into());
            } else {
                set_status(
                    state,
                    "No saved layout yet. Use Workspace: Save current layout.".into(),
                );
            }
            full_sync(ui, state);
            return;
        }
        "layout-reset" => {
            apply_layout(ui, &Layout::default());
            return;
        }
        "toggle-project-dock" => {
            ui.set_project_top(!ui.get_project_top());
            return;
        }
        "linked-selection" => {
            ui.set_linked_on(!ui.get_linked_on());
            return;
        }
        "ripple-toggle" => {
            ui.set_ripple_on(!ui.get_ripple_on());
            return;
        }
        "new" => {
            open_dialog(ui, state, true);
            return;
        }
        "sequence" => {
            open_dialog(ui, state, false);
            return;
        }
        "create-project" => {
            create_project(ui, state);
            return;
        }
        "apply-sequence" => {
            apply_sequence(ui, state);
            return;
        }
        "open" => {
            open_project(ui, state);
            return;
        }
        "save" => {
            save_project(ui, state, false);
            return;
        }
        "save-as" => {
            save_project(ui, state, true);
            return;
        }
        "import" => {
            import_media(ui, state);
            return;
        }
        "import-timeline" => {
            begin_timeline_import(ui, state);
            return;
        }
        "import-timeline-new" => {
            finish_timeline_import(ui, state, crate::import::ImportMode::NewProject);
            return;
        }
        "import-timeline-merge" => {
            finish_timeline_import(ui, state, crate::import::ImportMode::IntoCurrent);
            return;
        }
        "import-timeline-cancel" => {
            state.borrow_mut().timeline_import_path = None;
            ui.set_show_import_mode(false);
            return;
        }
        "import-report-close" => {
            ui.set_show_import_report(false);
            return;
        }
        "update-badge" => {
            open_update_agree_for_latest(ui, state);
            return;
        }
        "update-agree-cancel" => {
            clear_update_agree(state);
            ui.set_show_update_agree(false);
            return;
        }
        "update-agree-manual" => {
            let url = state.borrow().update_agree_url.clone();
            clear_update_agree(state);
            ui.set_show_update_agree(false);
            if let Some(url) = url {
                match crate::update::open_in_browser(&url) {
                    Ok(()) => set_status(state, "Opened download page".into()),
                    Err(err) => set_status(state, err),
                }
            }
            full_sync(ui, state);
            return;
        }
        "update-agree-auto" => {
            let tag = state.borrow().update_agree_tag.clone();
            clear_update_agree(state);
            ui.set_show_update_agree(false);
            if let Some(tag) = tag {
                apply_release_tag(ui, state, &tag);
            }
            return;
        }
        "versions-refresh" => {
            refresh_versions(ui, state, false);
            return;
        }
        "versions-refresh-force" => {
            refresh_versions(ui, state, true);
            return;
        }
        "relink-media" => {
            relink_media(ui, state);
            return;
        }
        "browse-export" => {
            browse_export(ui);
            return;
        }
        "render" => {
            start_export(ui, state);
            return;
        }
        "add-to-queue" => {
            enqueue_export(ui, state);
            return;
        }
        "render-queue" => {
            state.borrow_mut().queue_active = true;
            launch_next_export(ui, state);
            return;
        }
        "clear-completed" => {
            state
                .borrow_mut()
                .export_jobs
                .retain(|job| matches!(job.status, JobStatus::Pending | JobStatus::Rendering));
            sync_queue(ui, state);
            return;
        }
        "cancel-render" => {
            state.borrow_mut().queue_active = false;
            state.borrow().export_cancel.store(true, Ordering::Relaxed);
            if state.borrow().export_running {
                ui.set_export_cancelling(true);
                ui.set_export_eta("".into());
                state.borrow_mut().export_eta_at = None;
                ui.set_export_message("Cancelling…".into());
            }
            return;
        }
        "export-view" => {
            ui.set_page(1);
            prepare_export(ui, state);
            return;
        }
        "edit-view" => {
            export_form::remember(ui, state);
            save_preferences(state);
            ui.set_page(0);
            return;
        }
        "profile-premiere" => {
            set_profile(ui, state, ShortcutProfile::Premiere);
            return;
        }
        "profile-resolve" => {
            set_profile(ui, state, ShortcutProfile::Resolve);
            return;
        }
        "quit" => Command::Quit,
        "undo" => Command::Undo,
        "redo" => Command::Redo,
        "copy" => Command::Copy,
        "paste" => Command::Paste,
        "insert" => Command::Insert,
        "overwrite" => Command::Overwrite,
        "split" => Command::Split,
        "ripple-delete" => Command::RippleDelete,
        "lift" => Command::LiftDelete,
        "select-all" => Command::SelectAll,
        "mark-in" => Command::MarkIn,
        "mark-out" => Command::MarkOut,
        "clear-io" => Command::ClearInOut,
        "goto-in" => Command::GotoIn,
        "goto-out" => Command::GotoOut,
        "play-toggle" => Command::PlayPause,
        "shuttle-j" => Command::ShuttleReverse,
        "shuttle-k" => Command::ShuttleStop,
        "shuttle-l" => Command::ShuttleForward,
        "step-back" => Command::StepBack,
        "step-fwd" => Command::StepForward,
        "step-back-second" => Command::StepBackSecond,
        "step-fwd-second" => Command::StepForwardSecond,
        "ripple-prev" => Command::RipplePrev,
        "ripple-next" => Command::RippleNext,
        "mute-track" => Command::MuteTrack,
        "solo-track" => {
            let index = {
                let current = state.borrow();
                current.editor.selected_track.or_else(|| current.editor.primary_clip()
                    .and_then(|id| current.editor.project.track_index_of_clip(id)))
            };
            if let Some(index) = index {
                audio_mix::toggle_solo(&mut state.borrow_mut().editor, index);
                audio_mix::note_audio(state);
                full_sync(ui, state);
            }
            return;
        }
        "clear-meters" => {
            state.borrow().audio.clear_meters();
            audio_mix::sync_meters(ui, state);
            return;
        }
        "home" => Command::Home,
        "end" => Command::End,
        "prev-edit" => Command::PrevEdit,
        "next-edit" => Command::NextEdit,
        "zoom-in" => Command::ZoomIn,
        "zoom-out" => Command::ZoomOut,
        "zoom-fit" => Command::ZoomFit,
        "snap" => Command::Snap,
        "tool-select" => Command::SelectTool,
        "tool-razor" => Command::RazorTool,
        "add-video" => {
            state.borrow_mut().editor.add_track(TrackKind::Video);
            let index = state
                .borrow()
                .editor
                .project
                .sequence
                .tracks
                .len()
                .saturating_sub(1);
            state.borrow_mut().editor.selected_track = Some(index);
            note(state);
            focus_panel(ui, 3);
            full_sync(ui, state);
            reveal_track(ui, state, Some(index));
            return;
        }
        "add-audio" => {
            state.borrow_mut().editor.add_track(TrackKind::Audio);
            let index = state
                .borrow()
                .editor
                .project
                .sequence
                .tracks
                .len()
                .saturating_sub(1);
            state.borrow_mut().editor.selected_track = Some(index);
            note(state);
            focus_panel(ui, 3);
            full_sync(ui, state);
            reveal_track(ui, state, Some(index));
            return;
        }
        "filter" => {
            let query = ui.get_query().to_lowercase();
            let mut guard = state.borrow_mut();
            guard.query = query;
            guard.filter_kind = ui.get_bin_filter();
            drop(guard);
            full_sync(ui, state);
            return;
        }
        "preset-high" => {
            ui.set_export_crf(18.0);
            ui.set_export_speed(0);
            ui.set_read_crf(SharedString::from("18"));
            return;
        }
        "preset-medium" => {
            ui.set_export_crf(21.0);
            ui.set_export_speed(0);
            ui.set_read_crf(SharedString::from("21"));
            return;
        }
        "preset-fast" => {
            ui.set_export_crf(21.0);
            ui.set_export_speed(1);
            ui.set_read_crf(SharedString::from("21"));
            return;
        }
        "preset-compact" => {
            ui.set_export_crf(23.0);
            ui.set_export_speed(2);
            ui.set_read_crf(SharedString::from("23"));
            return;
        }
        "preset-match" => {
            export_form::match_sequence(ui, state);
            return;
        }
        "remove-transition" => {
            state.borrow_mut().editor.delete_selected_transition();
            note(state);
            full_sync(ui, state);
            return;
        }
        "deselect" => {
            run_command(ui, state, Command::Escape);
            return;
        }
        "reset-grade" => {
            {
                let mut guard = state.borrow_mut();
                if let Some(id) = guard.editor.primary_clip() {
                    let _ = guard.editor.edit("reset-grade", |project| {
                        if let Some(clip) = project.clip_mut(id) {
                            clip.grade = crate::color::Grade::default();
                        }
                        Ok::<(), String>(())
                    });
                }
            }
            note(state);
            full_sync(ui, state);
            return;
        }
        _ => return };
    run_command(ui, state, mapped);
}

fn run_command(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, command: Command) {
    match command {
        Command::Quit => {
            if state.borrow().live.is_some() {
                live::close(ui, state);
                Some(())
            } else if confirm_unsaved(ui, state) {
                persist_current_layout(ui, state);
                state.borrow().audio.stop();
                state.borrow().export_cancel.store(true, Ordering::Relaxed);
                slint::quit_event_loop().ok()
            } else {
                Some(())
            }
        }
        Command::Undo => {
            if !state.borrow().editor.can_undo() { return; }
            audio_mix::history(state, false);
            Some(())
        }
        Command::Redo => {
            if !state.borrow().editor.can_redo() { return; }
            audio_mix::history(state, true);
            Some(())
        }
        Command::Copy => {
            state.borrow_mut().editor.copy();
            let n = state.borrow().editor.clipboard.len();
            set_status(
                state,
                if n == 0 {
                    "Nothing selected to copy".into()
                } else {
                    format!("Copied {n}")
                },
            );
            Some(())
        }
        Command::Paste => act(state, |editor| editor.paste()),
        Command::Insert => insert_selected_with_offer(ui, state, false),
        Command::Overwrite => insert_selected_with_offer(ui, state, true),
        Command::Split => act(state, |editor| editor.split_playhead()),
        Command::RippleDelete => act(state, |editor| editor.ripple_delete_selected()),
        Command::LiftDelete => act(state, |editor| editor.lift_delete_selected()),
        Command::SelectAll => {
            state.borrow_mut().editor.select_all();
            Some(())
        }
        Command::RipplePrev => act(state, |editor| editor.ripple_prev()),
        Command::RippleNext => act(state, |editor| editor.ripple_next()),
        Command::MarkIn => {
            state.borrow_mut().editor.mark_in();
            Some(())
        }
        Command::MarkOut => {
            state.borrow_mut().editor.mark_out();
            Some(())
        }
        Command::ClearInOut => {
            state.borrow_mut().editor.clear_in_out();
            Some(())
        }
        Command::GotoIn => {
            let mut state = state.borrow_mut();
            if let Some(frame) = state.editor.project.sequence.in_point {
                state.editor.project.sequence.playhead = frame;
            }
            Some(())
        }
        Command::GotoOut => {
            let mut state = state.borrow_mut();
            if let Some(frame) = state.editor.project.sequence.out_point {
                state.editor.project.sequence.playhead = frame;
            }
            Some(())
        }
        Command::PlayPause => {
            toggle_play(state);
            Some(())
        }
        Command::ShuttleReverse => {
            shuttle(state, -1.0);
            Some(())
        }
        Command::ShuttleStop => {
            stop_play(state);
            Some(())
        }
        Command::ShuttleForward => {
            shuttle(state, 1.0);
            Some(())
        }
        Command::StepBack => {
            stop_play(state);
            state.borrow_mut().editor.step(-1);
            Some(())
        }
        Command::StepForward => {
            stop_play(state);
            state.borrow_mut().editor.step(1);
            Some(())
        }
        Command::StepBackSecond => {
            stop_play(state);
            let frames = state.borrow().editor.project.sequence.frame_rate.nominal();
            state.borrow_mut().editor.step(-frames);
            Some(())
        }
        Command::StepForwardSecond => {
            stop_play(state);
            let frames = state.borrow().editor.project.sequence.frame_rate.nominal();
            state.borrow_mut().editor.step(frames);
            Some(())
        }
        Command::PrevEdit => {
            stop_play(state);
            state.borrow_mut().editor.jump_edit(false);
            Some(())
        }
        Command::NextEdit => {
            stop_play(state);
            state.borrow_mut().editor.jump_edit(true);
            Some(())
        }
        Command::Home => {
            stop_play(state);
            state.borrow_mut().editor.project.sequence.playhead = 0;
            Some(())
        }
        Command::End => {
            stop_play(state);
            let end = state.borrow().editor.project.sequence.duration_frames();
            state.borrow_mut().editor.project.sequence.playhead = end;
            Some(())
        }
        Command::ZoomIn => {
            state.borrow_mut().editor.zoom_by(1.25);
            Some(())
        }
        Command::ZoomOut => {
            state.borrow_mut().editor.zoom_by(0.8);
            Some(())
        }
        Command::ZoomFit => {
            let width = state.borrow().timeline_w;
            state.borrow_mut().editor.zoom_fit(width);
            Some(())
        }
        Command::Snap => {
            state.borrow_mut().editor.toggle_snap();
            Some(())
        }
        Command::MuteTrack => {
            let audio = {
                let current = state.borrow();
                current.editor.selected_track.or_else(|| current.editor.primary_clip()
                    .and_then(|id| current.editor.project.track_index_of_clip(id)))
                    .and_then(|index| current.editor.project.sequence.tracks.get(index))
                    .is_some_and(|track| track.kind == TrackKind::Audio)
            };
            state.borrow_mut().editor.mute_selected_track();
            if audio { audio_mix::note_audio(state); } else { note(state); }
            Some(())
        }
        Command::SelectTool => {
            let mut current = state.borrow_mut();
            current.editor.tool = Tool::Select;
            current.editor.armed = None;
            Some(())
        }
        Command::RazorTool => {
            let mut current = state.borrow_mut();
            current.editor.tool = Tool::Razor;
            current.editor.armed = None;
            Some(())
        }
        Command::Escape => {
            ui.set_show_dialog(false);
            ui.set_show_prefs(false);
            ui.set_show_import_mode(false);
            ui.set_show_import_report(false);
            ui.set_show_update_agree(false);
            ui.set_open_menu(0);
            ui.set_show_command_palette(false);
            ui.set_media_drag_active(false);
            state.borrow_mut().timeline_import_path = None;
            clear_update_agree(state);
            let mut state = state.borrow_mut();
            if state.pointer.down && state.pointer.seeking {
                state.editor.project.sequence.playhead = state.pointer.origin_frame;
            } else {
                state.editor.cancel_gesture();
            }
            state.pointer = Pointer::default();
            state.media_drag_origin = None;
            state.editor.armed = None;
            state.editor.tool = Tool::Select;
            state.editor.clear_selection();
            state.edit_gen = state.edit_gen.wrapping_add(1);
            state.picture_gen = state.picture_gen.wrapping_add(1);
            state.shown_frame = -1;
            Some(())
        }
        Command::New => return open_dialog(ui, state, true),
        Command::Open => return open_project(ui, state),
        Command::Save => return save_project(ui, state, false),
        Command::SaveAs => return save_project(ui, state, true),
        Command::Import => return import_media(ui, state),
        Command::ImportTimeline => return begin_timeline_import(ui, state),
        Command::ExportView => return on_command(ui, state, "export-view"),
        Command::EditView => {
            ui.set_page(0);
            None
        }
    };
    full_sync(ui, state);
}

fn act(
    state: &Rc<std::cell::RefCell<AppState>>,
    f: impl FnOnce(&mut timeline::Editor) -> Result<(), String>,
) -> Option<()> {
    let result = f(&mut state.borrow_mut().editor);
    match result {
        Ok(()) => note(state),
        Err(err) => set_status(state, err) }
    Some(())
}

fn sequence_match_format(project: &Project, media: &Media) -> Option<(u32, u32, FrameRate)> {
    if project
        .sequence
        .tracks
        .iter()
        .any(|track| !track.clips.is_empty())
        || media.kind != MediaKind::Video
        || !media.has_video
        || !media.source_fps.is_finite()
        || !(1.0..=240.0).contains(&media.source_fps)
        || !(16..=7680).contains(&media.width)
        || !(16..=7680).contains(&media.height)
    {
        return None;
    }
    let format = (media.width & !1, media.height & !1, media.source_rate());
    (format
        != (
            project.sequence.width,
            project.sequence.height,
            project.sequence.frame_rate,
        ))
        .then_some(format)
}

fn editing_availability(editor: &timeline::Editor) -> (bool, bool) {
    let project = &editor.project;
    let selected = timeline::ops::expand_links(project, &editor.selected_clips);
    let editable =
        !selected.is_empty() && timeline::ops::validate_unlocked(project, &selected).is_ok();
    let at = project.sequence.playhead;
    let targets = if selected.is_empty() {
        project
            .sequence
            .tracks
            .iter()
            .filter(|track| !track.locked)
            .flat_map(|track| track.clips.iter())
            .filter(|clip| clip.timeline_start < at && at < clip.end())
            .map(|clip| clip.id)
            .collect::<Vec<_>>()
    } else {
        selected
    };
    let targets = timeline::ops::expand_links(project, &targets);
    let can_split = timeline::ops::validate_unlocked(project, &targets).is_ok()
        && targets.iter().any(|id| {
            project
                .clip(*id)
                .is_some_and(|clip| clip.timeline_start < at && at < clip.end())
        });
    (editable, can_split)
}

fn offer_sequence_match(
    ui: &AppWindow,
    state: &Rc<std::cell::RefCell<AppState>>,
    media: u64,
    range: SourceRange,
    at: i64,
    overwrite: bool,
) -> bool {
    let pending = {
        let current = state.borrow();
        if current.sequence_match_offered {
            return false;
        }
        let Some(source) = current.editor.project.media(media) else {
            return false;
        };
        let Some(format) = sequence_match_format(&current.editor.project, source) else {
            return false;
        };
        let sequence = &current.editor.project.sequence;
        ui.set_sequence_match_description(format!(
            "{} is {}×{} at {} fps.\nThe sequence is {}×{} at {} fps.\nMatch the footage, or keep your sequence settings.",
            source.name, format.0, format.1, format.2.label(),
            sequence.width, sequence.height, sequence.frame_rate.label()
        ).into());
        PendingInsert {
            media,
            range,
            at,
            overwrite,
            track: current.editor.selected_track,
            format }
    };
    stop_play(state);
    state.borrow_mut().pending_insert = Some(pending);
    ui.set_open_menu(0);
    ui.set_show_dialog(false);
    ui.set_show_prefs(false);
    ui.set_show_command_palette(false);
    ui.set_show_about(false);
    ui.set_show_sequence_match(true);
    ui.invoke_focus_pane(ui.get_active_panel());
    true
}

fn insert_selected_with_offer(
    ui: &AppWindow,
    state: &Rc<std::cell::RefCell<AppState>>,
    overwrite: bool,
) -> Option<()> {
    let selected = {
        let current = state.borrow();
        current.editor.selected_media.and_then(|id| {
            current
                .editor
                .project
                .media(id)
                .and_then(|media| media.source_range().ok())
                .map(|range| (id, range, current.editor.project.sequence.playhead))
        })
    };
    if let Some((media, range, at)) = selected {
        if offer_sequence_match(ui, state, media, range, at, overwrite) {
            return Some(());
        }
    }
    act(state, |editor| {
        if overwrite {
            editor.overwrite_selected()
        } else {
            editor.insert_selected()
        }
    })
}

fn finish_sequence_match(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, choice: i32) {
    ui.set_show_sequence_match(false);
    let Some(pending) = state.borrow_mut().pending_insert.take() else {
        return;
    };
    if choice < 0 {
        return;
    }
    {
        let mut current = state.borrow_mut();
        current.editor.selected_media = Some(pending.media);
        current.editor.selected_track = pending.track;
    }
    let result = state
        .borrow_mut()
        .editor
        .insert_media_range_with_sequence_at(
            pending.media,
            pending.at,
            pending.overwrite,
            pending.range,
            (choice == 1).then_some(pending.format),
        );
    match result {
        Ok(()) => {
            state.borrow_mut().sequence_match_offered = true;
            note(state);
        }
        Err(error) => set_status(state, error) }
    set_preview_default(ui, state);
    ui.invoke_focus_pane(3);
    full_sync(ui, state);
}

fn note(state: &Rc<std::cell::RefCell<AppState>>) {
    let mut state = state.borrow_mut();
    state.edit_gen = state.edit_gen.wrapping_add(1);
    state.picture_gen = state.picture_gen.wrapping_add(1);
    state.shown_frame = -1;
    if state.playing && (state.speed - 1.0).abs() < 0.01 {
        state.audio.update_mix(state.editor.project.clone());
    }
}

fn set_status(state: &Rc<std::cell::RefCell<AppState>>, text: String) {
    state.borrow_mut().status = text;
}

fn toggle_play(state: &Rc<std::cell::RefCell<AppState>>) {
    let playing = {
        let state = state.borrow();
        state.playing && (state.speed - 1.0).abs() < 0.01
    };
    if playing {
        stop_play(state);
    } else {
        set_speed(state, 1.0);
    }
}

fn shuttle(state: &Rc<std::cell::RefCell<AppState>>, direction: f64) {
    let (playing, speed) = {
        let state = state.borrow();
        (state.playing, state.speed)
    };
    let next = if direction < 0.0 {
        if playing && speed < 0.0 {
            (speed * 2.0).max(-8.0)
        } else {
            -1.0
        }
    } else if playing && speed > 0.0 {
        (speed * 2.0).min(8.0)
    } else {
        1.0
    };
    set_speed(state, next);
}

fn stop_play(state: &Rc<std::cell::RefCell<AppState>>) {
    let mut state = state.borrow_mut();
    state.playing = false;
    state.waiting_audio = false;
    state.waiting_picture = false;
    state.speed = 0.0;
    state.source.stop();
    state.audio.stop();
}

fn set_speed(state: &Rc<std::cell::RefCell<AppState>>, speed: f64) {
    let project = {
        let mut state = state.borrow_mut();
        state.source.stop();
        state.audio.stop();
        state.anchor_frame = state.editor.project.sequence.playhead;
        state.anchor_at = Instant::now();
        state.speed = speed;
        state.playing = speed != 0.0;
        if state.playing {
            (state.waiting_picture, state.shown_frame) = prepare_picture_start(
                state.presented_picture,
                state.picture_gen,
                state.anchor_frame,
                state.shown_frame,
            );
        } else {
            state.waiting_picture = false;
        }
        state.preview_min_token = state.preview_token;
        state.waiting_audio = (speed - 1.0).abs() < 0.01;
        if state.waiting_picture {
            state.status = "Preparing playback…".into();
        } else if state.waiting_audio {
            state.status = "Preparing audio…".into();
        }
        state.editor.project.clone()
    };
    if (speed - 1.0).abs() < 0.01 && !state.borrow().waiting_picture {
        let frame = state.borrow().anchor_frame;
        state.borrow().audio.play(project, frame);
    } else {
        state.borrow().audio.stop();
    }
}

fn on_adjust(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, field: &str, value: f32) {
    if state.borrow().syncing {
        return;
    }
    if field == "crf" {
        ui.set_read_crf(SharedString::from(format!("{}", value.round() as i32)));
        return;
    }
    if field == "trans-dur" {
        let audio = {
            let current = state.borrow();
            current.editor.selected_transition.and_then(|id| current.editor.project.transition(id))
                .is_some_and(|transition| transition.kind.is_audio())
        };
        state
            .borrow_mut()
            .editor
            .set_selected_transition_duration(value.round() as i64);
        ui.set_read_trans(SharedString::from(format!("{}", value.round() as i32)));
        let fps = state
            .borrow()
            .editor
            .project
            .sequence
            .frame_rate
            .fps()
            .max(1.0);
        ui.set_read_seconds(SharedString::from(format!("{:.2}", value as f64 / fps)));
        if audio { audio_mix::note_audio(state); } else { note(state); }
        return;
    }
    state.borrow_mut().editor.adjust(field, value);
    match field {
        "pos-x" => ui.set_read_pos_x(SharedString::from(format!("{}", value.round() as i32))),
        "pos-y" => ui.set_read_pos_y(SharedString::from(format!("{}", value.round() as i32))),
        "scale" => ui.set_read_scale(SharedString::from(format!(
            "{}%",
            (value * 100.0).round() as i32
        ))),
        "opacity" => ui.set_read_opacity(SharedString::from(format!(
            "{}%",
            (value * 100.0).round() as i32
        ))),
        "volume" => ui.set_read_volume(SharedString::from(format!(
            "{}%",
            (value * 100.0).round() as i32
        ))),
        "exposure" => ui.set_read_exposure(SharedString::from(format!("{value:.2}"))),
        "contrast" => ui.set_read_contrast(SharedString::from(format!("{}", value.round() as i32))),
        "highlights" => {
            ui.set_read_highlights(SharedString::from(format!("{}", value.round() as i32)))
        }
        "shadows" => ui.set_read_shadows(SharedString::from(format!("{}", value.round() as i32))),
        "temperature" => {
            ui.set_read_temperature(SharedString::from(format!("{}", value.round() as i32)))
        }
        "tint" => ui.set_read_tint(SharedString::from(format!("{}", value.round() as i32))),
        "saturation" => {
            ui.set_read_saturation(SharedString::from(format!("{}", value.round() as i32)))
        }
        "lift-m" => ui.set_read_lift_m(SharedString::from(format!("{value:.2}"))),
        "lift-r" => ui.set_read_lift_r(SharedString::from(format!("{value:.2}"))),
        "lift-g" => ui.set_read_lift_g(SharedString::from(format!("{value:.2}"))),
        "lift-b" => ui.set_read_lift_b(SharedString::from(format!("{value:.2}"))),
        "gamma-m" => ui.set_read_gamma_m(SharedString::from(format!("{value:.2}"))),
        "gamma-r" => ui.set_read_gamma_r(SharedString::from(format!("{value:.2}"))),
        "gamma-g" => ui.set_read_gamma_g(SharedString::from(format!("{value:.2}"))),
        "gamma-b" => ui.set_read_gamma_b(SharedString::from(format!("{value:.2}"))),
        "gain-m" => ui.set_read_gain_m(SharedString::from(format!("{value:.2}"))),
        "gain-r" => ui.set_read_gain_r(SharedString::from(format!("{value:.2}"))),
        "gain-g" => ui.set_read_gain_g(SharedString::from(format!("{value:.2}"))),
        "gain-b" => ui.set_read_gain_b(SharedString::from(format!("{value:.2}"))),
        _ => {}
    }
    if field == "volume" {
        audio_mix::note_audio(state);
        audio_mix::sync_controls(ui, state);
    } else { note(state); }
}

fn on_pick(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, kind: &str, id: i32) {
    let audio_control = matches!(kind, "mute" | "solo") && state.borrow().editor.project.sequence.tracks
        .get(id as usize).is_some_and(|track| track.kind == TrackKind::Audio);
    match kind {
        "queue-remove" => {
            state
                .borrow_mut()
                .export_jobs
                .retain(|job| job.id != id || job.status == JobStatus::Rendering);
            sync_queue(ui, state);
            return;
        }
        "queue-retry" => {
            if let Some(job) = state.borrow_mut().export_jobs.iter_mut().find(|job| {
                job.id == id && matches!(job.status, JobStatus::Failed | JobStatus::Cancelled)
            }) {
                job.status = JobStatus::Pending;
                job.message = "Queued".into();
            }
            sync_queue(ui, state);
            return;
        }
        "media" => {
            select_project_media(ui, state, id as u64, false, false);
            return;
        }
        "track" => {
            let mut state = state.borrow_mut();
            state.editor.selected_track = Some(id as usize);
        }
        "mute" => state.borrow_mut().editor.toggle_mute(id as usize),
        "solo" => audio_mix::toggle_solo(&mut state.borrow_mut().editor, id as usize),
        "lock" => state.borrow_mut().editor.toggle_lock(id as usize),
        "arm" => {
            let kind = crate::project::TransitionKind::all()
                .get(id as usize)
                .copied();
            let mut guard = state.borrow_mut();
            guard.editor.armed = kind;
            let clip = guard.editor.primary_clip();
            if let (Some(kind), Some(clip_id)) = (kind, clip) {
                match guard.editor.apply_armed(clip_id) {
                    Ok(()) => guard.status = format!("{} added", kind.label()),
                    Err(err) => guard.status = err }
            } else if let Some(kind) = kind {
                guard.status = format!("{} — click a clip", kind.label());
            }
        }
        "preset" => {
            if let Some(preset) = sequence_presets().get(id as usize) {
                ui.set_dlg_width(SharedString::from(preset.width.to_string()));
                ui.set_dlg_height(SharedString::from(preset.height.to_string()));
                if let Some(index) = frame_rate_presets()
                    .iter()
                    .position(|rate| *rate == preset.rate)
                {
                    ui.set_dlg_fps(index as i32);
                }
            }
            return;
        }
        _ => {}
    }
    if audio_control { audio_mix::note_audio(state); } else { note(state); }
    full_sync(ui, state);
}

fn sync_razor_hover(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let mut current = state.borrow_mut();
    let position = current.razor_hover.filter(|(x, y)| x.is_finite() && y.is_finite()
        && *x >= 0.0 && *x < current.timeline_w && *y >= 0.0 && *y < current.track_body);
    if !ui.get_razor_hover_allowed() || current.editor.tool != Tool::Razor || current.pointer.down || current.playing || position.is_none() {
        if current.razor_hover_key.take().is_some() { ui.set_razor_guides(model(Vec::<RazorGuide>::new())); ui.set_razor_text("".into()); }
        return;
    }
    let (x, y) = position.unwrap();
    let sequence = &current.editor.project.sequence;
    let scroll_y = ui.get_timeline_scroll_y();
    let key = (current.edit_gen, sequence.playhead, sequence.scroll.to_bits(), sequence.zoom.to_bits(),
        current.timeline_w.to_bits(), current.track_body.to_bits(), scroll_y.to_bits(), x.to_bits(), y.to_bits(), sequence.snap);
    if current.razor_hover_key == Some(key) { return; }
    let px = sequence.zoom as f32;
    let frame = frame_at(x, sequence.scroll as f32, px).max(0);
    let hit = hit_rows(sequence, x, y + scroll_y, px, sequence.scroll as f32, current.track_body);
    let mut guides = Vec::new();
    let (frame, reason, count) = if let Hit::Clip { id, .. } = hit {
        let plan = current.editor.razor_plan(id, frame, (6.0 / px.max(0.05)).round() as i64);
        let guide_x = ((plan.frame as f64 - sequence.scroll) * sequence.zoom) as f32;
        if guide_x >= 0.0 && guide_x < current.timeline_w {
            for row in rows_for(&current) {
                let track = &sequence.tracks[row.track_index];
                let applies = track.clips.iter().any(|clip| plan.clips.contains(&clip.id)
                    || plan.reason.is_some() && clip.id == id);
                let top = (row.y - scroll_y + 2.0).max(0.0);
                let bottom = (row.y + row.h - scroll_y - 2.0).min(current.track_body);
                if applies && bottom > top { guides.push(RazorGuide { x: guide_x, y: top, h: bottom-top }); }
            }
        }
        (plan.frame, plan.reason, plan.clips.len())
    } else { (frame, Some(if matches!(hit, Hit::Transition { .. }) { "Move above or below the transition to cut its clip" } else { "No clip here to cut" }.into()), 0) };
    ui.set_razor_valid(reason.is_none());
    let message = reason.unwrap_or_else(|| if count > 1 { format!("Cut {count} linked clips") } else { "Cut clip".into() });
    ui.set_razor_text(format!("{} · {message}", format_timecode(frame, sequence.frame_rate)).into());
    ui.set_razor_label_x(x);
    ui.set_razor_label_y(y);
    ui.set_razor_guides(model(guides));
    current.razor_hover_key = Some(key);
}

fn sync_edit_feedback(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let mut current = state.borrow_mut();
    let position = current.razor_hover.filter(|(x, y)| x.is_finite() && y.is_finite());
    if !ui.get_edit_feedback_allowed() || current.editor.tool != Tool::Select || current.playing || position.is_none() || current.pointer.seeking {
        if current.edit_hover_key.take().is_some() {
            ui.set_edit_guides(model(Vec::<RazorGuide>::new())); ui.set_edit_text("".into()); ui.set_edit_cursor(0);
        }
        return;
    }
    let (x, y) = position.unwrap();
    let seq = &current.editor.project.sequence;
    let scroll_y = ui.get_timeline_scroll_y();
    let key = (current.edit_gen, seq.scroll.to_bits(), seq.zoom.to_bits(), current.timeline_w.to_bits(),
        current.track_body.to_bits(), scroll_y.to_bits(), x.to_bits(), y.to_bits(), current.pointer.down, ui.get_linked_on());
    // Selection-only keyboard commands do not increment edit_gen. Keep the
    // stationary hover honest when a newly selected clip cannot be moved.
    if current.edit_hover_key == Some(key)
        && current.edit_hover_selection == current.editor.selected_clips { return; }
    let hit = hit_rows(seq, x, y + scroll_y, seq.zoom as f32, seq.scroll as f32, current.track_body);
    let feedback = current.pointer.down.then(|| current.editor.gesture_feedback.as_ref()).flatten();
    let mut guides = Vec::new();
    let mut text = String::new();
    let mut valid = true;
    let mut cursor = 0;
    let target = if let Some(feedback) = feedback {
        valid = feedback.reason.is_none();
        text = format!("{} {:+}f", if feedback.edge.is_some() { "Trim" } else { "Move" }, feedback.delta);
        if feedback.edge.is_some() { text.push_str(&format!(" · Duration {}", format_timecode(feedback.duration, seq.frame_rate))); }
        if let Some(reason) = &feedback.reason { text = format!("{reason} · {text}"); }
        if let Some(frame) = feedback.snap {
            let px = ((frame as f64 - seq.scroll) * seq.zoom) as f32;
            if px >= 0.0 && px < current.timeline_w { guides.push(RazorGuide { x: px, y: 0.0, h: current.track_body }); }
        }
        Some((feedback.clip, feedback.edge))
    } else if let Hit::Clip { id, edge } = hit {
        let reason = if edge.is_none() {
            current.editor.move_target_reason(id, ui.get_linked_on())
        } else {
            current.editor.edit_target_reason(id, ui.get_linked_on())
        };
        valid = reason.is_none();
        text = reason.unwrap_or_else(|| match edge { Some(Edge::In) => "Trim In · Drag to adjust the start".into(), Some(Edge::Out) => "Trim Out · Drag to adjust the end".into(), None => "Move clip · Drag to reposition".into() });
        Some((id, edge))
    } else { None };
    if let Some((id, edge)) = target {
        cursor = if valid { if edge.is_some() { 1 } else { 2 } } else { 0 };
        if let Some(edge) = edge {
            if let (Some(clip), Some(track)) = (current.editor.project.clip(id), current.editor.project.track_index_of_clip(id)) {
                if let Some(row) = rows_for(&current).into_iter().find(|row| row.track_index == track) {
                    let frame = if edge == Edge::In { clip.timeline_start } else { clip.end() };
                    let px = ((frame as f64-seq.scroll)*seq.zoom) as f32;
                    let top = (row.y-scroll_y+2.0).max(0.0);
                    let bottom = (row.y+row.h-scroll_y-2.0).min(current.track_body);
                    if px >= 0.0 && px < current.timeline_w && bottom > top { guides.push(RazorGuide { x: px, y: top, h: bottom-top }); }
                }
            }
        }
    }
    ui.set_edit_guides(model(guides)); ui.set_edit_text(text.into()); ui.set_edit_valid(valid); ui.set_edit_cursor(cursor);
    ui.set_edit_label_x(x); ui.set_edit_label_y(y);
    current.edit_hover_key = Some(key);
    current.edit_hover_selection = current.editor.selected_clips.clone();
}

fn on_timeline(
    ui: &AppWindow,
    state: &Rc<std::cell::RefCell<AppState>>,
    zone: i32,
    phase: i32,
    x: f32,
    y: f32,
    shift: bool,
) {
    if zone == 1 { state.borrow_mut().razor_hover = (phase != 3).then_some((x, y)); }
    if phase == 0 || state.borrow().pointer.down { state.borrow().filmstrips.set_busy(true); }
    // UI pointer coordinates are relative to the visible track viewport.
    let y = if zone == 1 {
        y + ui.get_timeline_scroll_y()
    } else {
        y
    };
    if phase == 3 {
        let mut current = state.borrow_mut();
        if current.pointer.down && current.pointer.seeking {
            current.editor.project.sequence.playhead = current.pointer.origin_frame;
        } else {
            current.editor.cancel_gesture();
            current.edit_gen = current.edit_gen.wrapping_add(1);
            current.picture_gen = current.picture_gen.wrapping_add(1);
        }
        current.pointer = Pointer::default();
        current.shown_frame = -1;
        drop(current);
        full_sync(ui, state);
        return;
    }
    if phase == 0 {
        stop_play(state);
        let (tool, armed, sequence, px, scroll, body) = {
            let state = state.borrow();
            (
                state.editor.tool,
                state.editor.armed,
                state.editor.project.sequence.clone(),
                state.editor.project.sequence.zoom as f32,
                state.editor.project.sequence.scroll as f32,
                state.track_body,
            )
        };
        let frame = frame_at(x, scroll, px).max(0);
        if zone == 0 {
            {
                let mut state = state.borrow_mut();
                state.pointer = Pointer {
                    down: true,
                    zone,
                    x,
                    y,
                    moved: false,
                    seeking: true,
                    origin_frame: sequence.playhead,
                    hit: Hit::Empty { frame },
                    ripple: false,
                    links: ui.get_linked_on() };
                state.editor.project.sequence.playhead = frame;
                state.playing = false;
                state.speed = 0.0;
                state.audio.stop();
                state.preview_min_token = state.preview_token.saturating_add(1);
                state.shown_frame = -1;
            }
            full_sync(ui, state);
            return;
        }
        let hit = hit_rows(&sequence, x, y, px, scroll, body);
        if tool == Tool::Razor {
            if let Hit::Clip { id, .. } = hit {
                let mut state = state.borrow_mut();
                let threshold = (6.0 / px.max(0.05)).round() as i64;
                let plan = state.editor.razor_plan(id, frame, threshold);
                let result = if let Some(reason) = plan.reason { Err(reason) }
                    else { state.editor.split_clip_at(id, plan.frame) };
                if let Err(err) = result {
                    state.status = err;
                }
                note_borrowed(&mut state);
            }
            full_sync(ui, state);
            return;
        }
        match hit {
            Hit::Transition { id } => {
                let mut state = state.borrow_mut();
                state.editor.selected_transition = Some(id);
                state.editor.selected_clips.clear();
                state.pointer = Pointer {
                    down: true,
                    zone,
                    x,
                    y,
                    moved: false,
                    seeking: false,
                    origin_frame: sequence.playhead,
                    hit,
                    ripple: false,
                    links: ui.get_linked_on() };
            }
            Hit::Clip { id, edge } => {
                if armed.is_some() && edge.is_none() {
                    let mut state = state.borrow_mut();
                    if let Err(err) = state.editor.apply_armed(id) {
                        state.status = err;
                    }
                    note_borrowed(&mut state);
                } else {
                    let mut state = state.borrow_mut();
                    if shift {
                        state.editor.toggle_select(id, ui.get_linked_on());
                    } else if !state.editor.selected_clips.contains(&id) {
                        state.editor.select_only(id, ui.get_linked_on());
                    }
                    if let Some(index) = state.editor.project.track_index_of_clip(id) {
                        state.editor.selected_track = Some(index);
                    }
                    if let Some(position) = state
                        .editor
                        .selected_clips
                        .iter()
                        .position(|selected| *selected == id)
                    {
                        state.editor.selected_clips.swap(0, position);
                    }
                    state.pointer = Pointer {
                        down: state.editor.selected_clips.contains(&id),
                        zone,
                        x,
                        y,
                        moved: false,
                        seeking: false,
                        origin_frame: sequence.playhead,
                        hit,
                        ripple: shift || ui.get_ripple_on(),
                        links: ui.get_linked_on() };
                }
            }
            Hit::Empty { frame } => {
                let mut state = state.borrow_mut();
                state.editor.selected_clips.clear();
                state.editor.selected_transition = None;
                state.editor.project.sequence.playhead = frame.max(0);
                state.playing = false;
                state.audio.stop();
                state.pointer = Pointer {
                    down: true,
                    zone,
                    x,
                    y,
                    moved: false,
                    seeking: true,
                    origin_frame: sequence.playhead,
                    hit: Hit::Empty { frame },
                    ripple: false,
                    links: ui.get_linked_on() };
                state.preview_min_token = state.preview_token.saturating_add(1);
                state.shown_frame = -1;
            }
        }
        full_sync(ui, state);
        return;
    }
    if phase == 1 {
        let start = {
            let state = state.borrow();
            if !state.pointer.down {
                return;
            }
            (
                state.pointer.x,
                state.pointer.y,
                state.pointer.seeking,
                state.pointer.moved,
            )
        };
        let dist = (x - start.0).abs() + (y - start.1).abs();
        if !start.3 && !start.2 && dist < 4.0 {
            return;
        }
        let seeking = start.2;
        {
            let mut state = state.borrow_mut();
            state.pointer.moved = true;
            if seeking || state.pointer.zone == 0 {
                let px = state.editor.project.sequence.zoom as f32;
                let scroll = state.editor.project.sequence.scroll as f32;
                state.editor.project.sequence.playhead = frame_at(x, scroll, px).max(0);
                state.playing = false;
                state.audio.stop();
            }
        }
        if !seeking {
            drag_edit(state, x, y);
            full_sync(ui, state);
        } else {
            // A seek moves the clock, not the media bin, inspector or clip models.
            light_sync(ui, state);
        }
        return;
    }
    if phase == 2 {
        let (moved, seeking) = state.borrow_mut().pointer.finish();
        if moved && !seeking {
            state.borrow_mut().editor.commit_gesture();
            note(state);
        }
        if seeking {
            light_sync(ui, state);
        } else {
            full_sync(ui, state);
        }
    }
}

fn drag_edit(state: &Rc<std::cell::RefCell<AppState>>, x: f32, y: f32) {
    let mut state = state.borrow_mut();
    let px = state.editor.project.sequence.zoom as f32;
    let scroll = state.editor.project.sequence.scroll as f32;
    let frame = frame_at(x, scroll, px);
    let threshold = (8.0 / px).ceil() as i64;
    if !state.editor.gesture_active() {
        match state.pointer.hit {
            Hit::Clip {
                id,
                edge: Some(edge) } => {
                let ripple = state.pointer.ripple;
                let links = state.pointer.links;
                state.editor.begin_trim(id, edge, ripple, links);
            }
            Hit::Clip { .. } => {
                let grab = frame_at(state.pointer.x, scroll, px);
                let links = state.pointer.links;
                if let Err(error) = state.editor.begin_move(grab, links) {
                    if let Hit::Clip { id, .. } = state.pointer.hit {
                        state.editor.gesture_feedback = Some(timeline::GestureFeedback { clip: id, edge: None, delta: 0,
                            duration: state.editor.project.clip(id).map_or(0, |clip| clip.duration), snap: None, reason: Some(error.clone()) });
                    }
                    state.status = error;
                    state.edit_gen = state.edit_gen.wrapping_add(1);
                    state.picture_gen = state.picture_gen.wrapping_add(1);
                    return;
                }
            }
            _ => return }
    }
    if matches!(state.pointer.hit, Hit::Clip { edge: Some(_), .. }) {
        state.editor.update_trim(frame, threshold);
    } else {
        let target = rows_for(&state)
            .iter()
            .find(|row| y >= row.y && y < row.y + row.h)
            .map(|row| row.track_index);
        if let Err(error) = state.editor.update_move(frame, threshold, target) {
            state.status = error;
        }
    }
    state.edit_gen = state.edit_gen.wrapping_add(1);
    state.picture_gen = state.picture_gen.wrapping_add(1);
    state.shown_frame = -1;
}

fn media_drop_target(
    ui: &AppWindow,
    state: &AppState,
    id: i32,
    x: f32,
    y: f32,
    range: SourceRange,
) -> Option<(usize, i64, Vec<timeline::Placement>)> {
    if ui.get_page() != 0 {
        return None;
    }
    let local_x = x - ui.get_timeline_origin_x();
    let viewport_y = y - ui.get_timeline_origin_y();
    if local_x < 0.0
        || local_x >= ui.get_timeline_px()
        || viewport_y < 0.0
        || viewport_y >= state.track_body
    {
        return None;
    }
    let local_y = viewport_y + ui.get_timeline_scroll_y();
    let media = state.editor.project.media(id as u64)?;
    let row = rows_for(state)
        .into_iter()
        .find(|row| local_y >= row.y && local_y < row.y + row.h)?;
    let sequence = &state.editor.project.sequence;
    let track = sequence.tracks.get(row.track_index)?;
    if track.locked {
        return None;
    }
    if match track.kind {
        TrackKind::Video => !media.has_video && media.kind != MediaKind::Still,
        TrackKind::Audio => !media.has_audio } {
        return None;
    }
    // Mixed sources create picture and sound together. Both need an available lane.
    if media.has_video
        && !sequence
            .tracks
            .iter()
            .any(|track| track.kind == TrackKind::Video && !track.locked)
    {
        return None;
    }
    if media.has_audio
        && !sequence
            .tracks
            .iter()
            .any(|track| track.kind == TrackKind::Audio && !track.locked)
    {
        return None;
    }
    let mut frame = frame_at(local_x, sequence.scroll as f32, sequence.zoom as f32).max(0);
    if sequence.snap {
        let threshold = (8.0 / sequence.zoom.max(0.01)).ceil() as i64;
        let mut points = vec![sequence.playhead, 0];
        if let Some(point) = sequence.in_point {
            points.push(point);
        }
        if let Some(point) = sequence.out_point {
            points.push(point);
        }
        for track in &sequence.tracks {
            for clip in &track.clips {
                points.push(clip.timeline_start);
                points.push(clip.end());
            }
        }
        if let Some(point) = points.into_iter().min_by_key(|point| (point - frame).abs()) {
            if (point - frame).abs() <= threshold {
                frame = point.max(0);
            }
        }
    }
    let placements = timeline::plan_placements_in_range(
        &state.editor.project,
        id as u64,
        Some(row.track_index),
        range,
    )
    .ok()?;
    Some((row.track_index, frame, placements))
}

fn on_media_drag(
    ui: &AppWindow,
    state: &Rc<std::cell::RefCell<AppState>>,
    id: i32,
    phase: i32,
    x: f32,
    y: f32,
) {
    if phase == 0 {
        let range = state
            .borrow()
            .editor
            .project
            .media(id as u64)
            .and_then(|media| media.source_range().ok());
        state.borrow_mut().media_drag_origin = range.map(|range| MediaDrag { id, x, y, range });
        return;
    }
    if phase == 3 {
        state.borrow_mut().media_drag_origin = None;
        ui.set_media_drag_active(false);
        return;
    }
    let Some(origin) = state.borrow().media_drag_origin else {
        return;
    };
    if origin.id != id {
        return;
    }
    let moved = (x - origin.x).abs() + (y - origin.y).abs() >= 4.0 || ui.get_media_drag_active();
    if !moved {
        if phase == 2 {
            state.borrow_mut().media_drag_origin = None;
        }
        return;
    }
    let target = media_drop_target(ui, &state.borrow(), id, x, y, origin.range);
    if phase == 1 {
        ui.set_media_drag_active(true);
        ui.set_media_drop_valid(target.is_some());
        ui.set_media_drag_x(x);
        ui.set_media_drag_y(y);
        let current = state.borrow();
        if let Some(media) = current.editor.project.media(id as u64) {
            ui.set_media_drag_label(
                format!(
                    "{} · {}",
                    media.name,
                    if target.is_some() {
                        "Insert"
                    } else {
                        "Drop on an unlocked matching track"
                    }
                )
                .into(),
            );
            if let Some((_, frame, placements)) = &target {
                let rows = rows_for(&current);
                let local_x = (*frame as f32 - current.editor.project.sequence.scroll as f32)
                    * current.editor.project.sequence.zoom as f32;
                let media_width = placements.first().map_or(1, |placement| placement.duration)
                    as f32
                    * current.editor.project.sequence.zoom as f32;
                let (visible_x, visible_width) =
                    visible_drop_span(local_x, media_width, ui.get_timeline_px());
                let ghosts = placements
                    .iter()
                    .filter_map(|placement| {
                        let row = rows.iter().find(|row| row.track_index == placement.track)?;
                        let top = (row.y - ui.get_timeline_scroll_y()).max(0.0);
                        let bottom =
                            (row.y + row.h - ui.get_timeline_scroll_y()).min(current.track_body);
                        if bottom <= top {
                            return None;
                        }
                        Some(DropGhost {
                            x: ui.get_timeline_origin_x() + visible_x,
                            y: ui.get_timeline_origin_y() + top,
                            w: visible_width,
                            h: bottom - top,
                            video: current.editor.project.sequence.tracks[placement.track].kind
                                == TrackKind::Video })
                    })
                    .collect();
                ui.set_media_drop_ghosts(model(ghosts));
                let names = placements
                    .iter()
                    .map(|placement| {
                        current.editor.project.sequence.tracks[placement.track]
                            .name
                            .as_str()
                    })
                    .collect::<Vec<_>>()
                    .join(" + ");
                ui.set_media_drop_hint(
                    format!(
                        "Insert · {names} · {}",
                        format_timecode(*frame, current.editor.project.sequence.frame_rate)
                    )
                    .into(),
                );
            } else {
                ui.set_media_drop_ghosts(model(Vec::<DropGhost>::new()));
                ui.set_media_drop_hint("Move to compatible, unlocked tracks".into());
            }
        }
    } else if phase == 2 {
        state.borrow_mut().media_drag_origin = None;
        ui.set_media_drag_active(false);
        if let Some((track, frame, _)) = target {
            stop_play(state);
            {
                let mut current = state.borrow_mut();
                current.editor.selected_track = Some(track);
                current.editor.selected_media = Some(id as u64);
            }
            if offer_sequence_match(ui, state, id as u64, origin.range, frame, false) {
                return;
            }
            act(state, |editor| {
                editor.insert_media_range_at(id as u64, frame, false, origin.range)
            });
            ui.invoke_focus_pane(3);
            full_sync(ui, state);
        }
    }
}

fn on_wheel(
    ui: &AppWindow,
    state: &Rc<std::cell::RefCell<AppState>>,
    delta: f32,
    x: f32,
    ctrl: bool,
) {
    if delta.abs() < 0.01 {
        return;
    }
    state.borrow().waveform_engine.set_busy(true);
        state.borrow().filmstrips.set_busy(true);
    state.borrow().optimized_cache.set_busy(true);
    state.borrow_mut().background_idle_after = Instant::now() + std::time::Duration::from_secs(2);
    {
        let mut state = state.borrow_mut();
        let px = state.editor.project.sequence.zoom as f32;
        if ctrl {
            let before = frame_at(x, state.editor.project.sequence.scroll as f32, px);
            let factor = if delta > 0.0 { 1.12 } else { 1.0 / 1.12 };
            state.editor.zoom_by(factor);
            let px = state.editor.project.sequence.zoom as f32;
            state.editor.project.sequence.scroll = (before as f32 - x / px).max(0.0) as f64;
        } else {
            state.editor.project.sequence.scroll =
                (state.editor.project.sequence.scroll - delta as f64 * 0.35).max(0.0);
        }
        clamp_scroll(&mut state);
    }
    full_sync(ui, state);
}

fn scroll_media_drag(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let origin = state.borrow().media_drag_origin;
    let Some(origin) = origin.filter(|_| ui.get_media_drag_active()) else {
        if ui.get_media_drag_active() {
            ui.set_media_drag_active(false);
        }
        return;
    };
    let x = ui.get_media_drag_x();
    let y = ui.get_media_drag_y();
    let local_x = x - ui.get_timeline_origin_x();
    let local_y = y - ui.get_timeline_origin_y();
    let width = ui.get_timeline_px();
    let height = state.borrow().track_body;
    if local_x < 0.0 || local_x > width || local_y < -18.0 || local_y > height + 18.0 {
        return;
    }
    let edge = 24.0;
    let horizontal = if local_x < edge {
        -(1.0 - local_x / edge)
    } else if local_x > width - edge {
        1.0 - (width - local_x) / edge
    } else {
        0.0
    };
    let vertical = if local_y < edge {
        -(1.0 - local_y.max(0.0) / edge)
    } else if local_y > height - edge {
        1.0 - (height - local_y).max(0.0) / edge
    } else {
        0.0
    };
    let old_y = ui.get_timeline_scroll_y();
    ui.invoke_scroll_tracks_to(old_y + vertical * 5.0);
    let horizontal_changed = {
        let mut current = state.borrow_mut();
        let previous = current.editor.project.sequence.scroll;
        let zoom = current.editor.project.sequence.zoom.max(0.01);
        current.editor.project.sequence.scroll =
            (previous + horizontal as f64 * 5.0 / zoom).max(0.0);
        clamp_scroll(&mut current);
        current.editor.project.sequence.scroll != previous
    };
    if old_y != ui.get_timeline_scroll_y() || horizontal_changed {
        refresh_tracks(ui, state);
        light_sync(ui, state);
        on_media_drag(ui, state, origin.id, 1, x, y);
    }
}

fn visible_drop_span(x: f32, width: f32, viewport_width: f32) -> (f32, f32) {
    let extent = viewport_width.max(0.0);
    let marker_width = extent.min(2.0);
    let left = x.clamp(0.0, (extent - marker_width).max(0.0));
    let right = (x + width).clamp(left + marker_width, extent);
    (left, right - left)
}

fn viewer_drag_delta(dx: f32, dy: f32, sequence: (f32, f32), viewport: (f32, f32)) -> (f32, f32) {
    let scale =
        (viewport.0.max(1.0) / sequence.0.max(1.0)).min(viewport.1.max(1.0) / sequence.1.max(1.0));
    // The preview fits the sequence uniformly, including portrait letterboxing.
    (dx / scale, dy / scale)
}

fn on_viewer(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, phase: i32, x: f32, y: f32) {
    if phase == 0 {
        state.borrow_mut().viewer_last = Some((x, y));
        return;
    }
    if phase == 2 || phase == 3 {
        state.borrow_mut().viewer_last = None;
        return;
    }
    let Some((lx, ly)) = state.borrow().viewer_last else {
        return;
    };
    let (seq_w, view_w, seq_h, view_h) = {
        let state = state.borrow();
        (
            state.editor.project.sequence.width as f32,
            state.view_w.max(1.0),
            state.editor.project.sequence.height as f32,
            state.view_h.max(1.0),
        )
    };
    let (dx, dy) = viewer_drag_delta(x - lx, y - ly, (seq_w, seq_h), (view_w, view_h));
    let current = {
        let state = state.borrow();
        state.editor.primary_clip().and_then(|id| {
            state
                .editor
                .project
                .clip(id)
                .map(|clip| (clip.pos_x, clip.pos_y))
        })
    };
    if let Some((px, py)) = current {
        on_adjust(ui, state, "pos-x", px + dx);
        on_adjust(ui, state, "pos-y", py + dy);
        state.borrow_mut().viewer_last = Some((x, y));
        full_sync(ui, state);
    }
}

fn clamp_scroll(state: &mut AppState) {
    let px = state.editor.project.sequence.zoom as f32;
    let content = state.editor.project.sequence.duration_frames().max(1) as f32 * px + 240.0;
    let max_scroll = ((content - state.timeline_w) / px).max(0.0) as f64;
    if state.editor.project.sequence.scroll > max_scroll {
        state.editor.project.sequence.scroll = max_scroll;
    }
    if state.editor.project.sequence.scroll < 0.0 {
        state.editor.project.sequence.scroll = 0.0;
    }
}

fn note_borrowed(state: &mut AppState) {
    state.edit_gen = state.edit_gen.wrapping_add(1);
    state.picture_gen = state.picture_gen.wrapping_add(1);
    state.shown_frame = -1;
}

fn sync_track_body(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let body = ui.get_body_px();
    let changed = {
        let mut state = state.borrow_mut();
        if body > 8.0 && (body - state.track_body).abs() > 1.0 {
            state.track_body = body;
            true
        } else {
            false
        }
    };
    if changed {
        refresh_tracks(ui, state);
    }
}

fn export_terminal_status(error: Option<&str>) -> JobStatus {
    // The worker normalizes cancellation before sending the terminal result.
    // A later button press must not relabel either a committed file or a failure.
    match error {
        None => JobStatus::Complete,
        Some("Export cancelled") => JobStatus::Cancelled,
        Some(_) => JobStatus::Failed }
}

fn tick(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    audio_mix::sync_meters(ui, state);
    {
        let mut current = state.borrow_mut();
        let busy = current.playing || current.source.playing || current.pointer.down
            || current.source.scrub_origin.is_some() || Instant::now() < current.background_idle_after
            || current.program_request_pending || current.source_request_pending
            || current.export_running || current.import_rx.is_some() || ui.get_live_input_active();
        crate::playback::set_index_busy(busy);
        current.optimized_cache.set_busy(busy);
        if current.preview_cache_status_at.elapsed() >= std::time::Duration::from_secs(1) {
            sync_preview_cache_status(&ui, &current.optimized_cache);
            current.preview_cache_status_at = Instant::now();
        }
    }
    let palette_keys = {
        let current = state.borrow();
        if let Some(queue) = &current.native_events {
            queue.set_palette_active(ui.get_show_command_palette());
            queue.take_palette_keys()
        } else {
            Vec::new()
        }
    };
    for key in palette_keys {
        on_key(ui, state, key, false, false, false);
    }
    if ui.get_show_command_palette()
        && state.borrow().palette_query.as_ref() != Some(&ui.get_command_query().to_lowercase())
    {
        sync_commands(ui, state);
    }
    if ui.get_show_prefs()
        && state.borrow().binding_query.as_ref() != Some(&ui.get_keybinding_query().to_lowercase())
    {
        sync_keybindings(ui, state);
    }
    let timeline_resized = {
        let mut current = state.borrow_mut();
        let w = ui.get_preview_px_width();
        let h = ui.get_preview_px_height();
        if (w - current.view_w).abs() > 1.0 || (h - current.view_h).abs() > 1.0 {
            current.view_w = w;
            current.view_h = h;
            current.shown_frame = -1;
        }
        let width = ui.get_timeline_px();
        let changed = current.timeline_w.to_bits() != width.to_bits();
        current.timeline_w = width;
        changed
    };
    let previous_body = state.borrow().track_body;
    sync_track_body(ui, state);
    if timeline_resized || state.borrow().track_body != previous_body {
        light_sync(ui, state);
    }
    scroll_media_drag(ui, state);
    pump_import(ui, state);
    let quality = ui.get_quality();
    {
        let mut state = state.borrow_mut();
        if state.preview_quality != quality {
            state.preview_quality = quality;
            state.shown_frame = -1;
        }
    }
    let source_changed = {
        let mut current = state.borrow_mut();
        let was_playing = current.source.playing;
        let ready = current.source.waiting_audio && !current.source.waiting_picture
            && current.audio.is_ready();
        if ready {
            current.source.audio_ready();
            current.status = "Source playing".into();
        }
        let changed = current.source.advance();
        if was_playing && !current.source.playing {
            current.audio.stop();
        }
        ready || changed
    };
    if source_changed {
        sync_source_controls(ui, &state.borrow());
    }
    {
        let mut current = state.borrow_mut();
        if current.waiting_audio && !current.waiting_picture && current.audio.is_ready() {
            current.waiting_audio = false;
            current.anchor_at = Instant::now();
            current.status = "Playing".into();
            ui.set_status_text(current.status.clone().into());
        }
    }
    let playing = {
        let current = state.borrow();
        playback_clock_ready(
            current.playing,
            current.waiting_picture,
            current.waiting_audio,
        )
    };
    if playing {
        let (frame, stop) = {
            let state = state.borrow();
            let fps = state.editor.project.sequence.frame_rate.fps().max(1.0);
            let elapsed = state.anchor_at.elapsed().as_secs_f64();
            let mut frame = state.anchor_frame + (elapsed * fps * state.speed).round() as i64;
            let end = state.editor.project.sequence.duration_frames();
            let mut stop = false;
            if state.speed > 0.0 && frame >= end {
                frame = end;
                stop = true;
            }
            if state.speed < 0.0 && frame <= 0 {
                frame = 0;
                stop = true;
            }
            (frame.max(0), stop)
        };
        let clock_changed = {
            let mut state = state.borrow_mut();
            let previous_frame = state.editor.project.sequence.playhead;
            let previous_scroll = state.editor.project.sequence.scroll;
            state.editor.project.sequence.playhead = frame;
            if stop {
                state.playing = false;
                state.speed = 0.0;
                state.audio.stop();
            } else {
                let px = state.editor.project.sequence.zoom as f32;
                let span = (state.timeline_w / px).max(1.0) as f64;
                let play = frame as f64;
                state.editor.project.sequence.scroll =
                    playback_scroll(state.editor.project.sequence.scroll, play, span);
            }
            stop || previous_frame != frame
                || previous_scroll != state.editor.project.sequence.scroll
        };
        if clock_changed {
            light_sync(ui, state);
        }
    }
    pump_preview(ui, state);
    let query = ui.get_query().to_lowercase();
    let query_changed = state.borrow().query != query;
    if query_changed {
        state.borrow_mut().query = query;
        full_sync(ui, state);
    }
    let audio_status = state.borrow().audio.take_status();
    if let Some(message) = audio_status {
        let mut current = state.borrow_mut();
        if current.source.playing || current.source.waiting_audio {
            current.source_error = Some(message.clone());
            let picture = current
                .editor
                .selected_media
                .and_then(|id| current.editor.project.media(id))
                .is_some_and(|media| media.has_video || media.kind == MediaKind::Still);
            if !picture {
                current.source.stop();
                current.audio.stop();
            }
            sync_source_controls(ui, &current);
        }
        current.status = message;
        ui.set_status_text(SharedString::from(current.status.clone()));
    }
    let progress = state.borrow_mut().export_rx.as_ref().and_then(|rx| {
        let mut last = None;
        while let Ok(item) = rx.try_recv() {
            last = Some(item);
        }
        last
    });
    if let Some(progress) = progress {
        ui.set_export_progress(progress.fraction);
        ui.set_export_preparing(progress.preparation.is_some() && !progress.done);
        ui.set_export_preparation_label(progress.preparation.as_ref().map_or("Preparing", |prep| prep.phase.trim_end_matches(" audio")).into());
        ui.set_export_preparation_progress(progress.preparation.as_ref().map_or(0.0, |prep| prep.fraction));
        let cancelling = !progress.done && state.borrow().export_cancel.load(Ordering::Relaxed);
        let eta = if cancelling || progress.done || progress.preparation.is_some() { String::new() } else { crate::export::format_remaining_time(progress.remaining_seconds) };
        state.borrow_mut().export_eta_at = (!eta.is_empty()).then(Instant::now);
        ui.set_export_eta(eta.into());
        ui.set_export_cancelling(cancelling);
        ui.set_export_message(if cancelling { "Cancelling…".into() } else { SharedString::from(progress.message.clone()) });
        {
            let mut current = state.borrow_mut();
            let job_id = current.export_job_id;
            let cancelled = current.export_cancel.load(Ordering::Relaxed);
            if let Some(job) = current
                .export_jobs
                .iter_mut()
                .find(|job| Some(job.id) == job_id)
            {
                job.message = progress.message.clone();
                if progress.done {
                    job.status = export_terminal_status(progress.error.as_deref());
                }
            }
            if progress.done {
                current.export_running = false;
                current.export_job_id = None;
                current.export_rx = None;
                current.status = progress
                    .error
                    .clone()
                    .unwrap_or_else(|| progress.message.clone());
                if cancelled || progress.error.is_some() {
                    current.queue_active = false;
                }
            }
        }
        sync_queue(ui, state);
        if progress.done {
            full_sync(ui, state);
            launch_next_export(ui, state);
        }
    }
    pump_source_preview(ui, state);
    pump_thumbnails(ui, state);
    pump_waveforms(ui, state);
    filmstrips::pump(ui, state);
    sync_razor_hover(ui, state);
    sync_edit_feedback(ui, state);
    if state.borrow().export_eta_at.is_some_and(|at| at.elapsed() > std::time::Duration::from_secs(3)) {
        ui.set_export_eta("".into());
        state.borrow_mut().export_eta_at = None;
    }
    let mut current = state.borrow_mut();
    let mut index = 0;
    while index < current.export_workers.len() {
        if current.export_workers[index].is_finished() {
            let _ = current.export_workers.swap_remove(index).join();
        } else {
            index += 1;
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ThumbnailScheduleKey {
    revision: u64,
    bin: String,
    query: String }

impl ThumbnailScheduleKey {
    fn matches(&self, revision: u64, bin: &str, query: &str) -> bool {
        self.revision == revision && self.bin == bin && self.query == query
    }
}

fn thumbnail_schedule_needed(
    previous: Option<&ThumbnailScheduleKey>,
    revision: u64,
    bin: &str,
    query: &str,
    completed: bool,
) -> bool {
    completed || previous.is_none_or(|key| !key.matches(revision, bin, query))
}

// Waveforms never decode on this thread. This retained result budget is
// separate from the worker's bounded cache; old UI Arcs are released here.
fn same_waveform_source(a: &Media, b: &Media) -> bool {
    a.id == b.id && a.path == b.path && a.has_audio == b.has_audio
        && a.duration_secs.to_bits() == b.duration_secs.to_bits()
}
fn pump_waveforms(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let mut current = state.borrow_mut();
    current.waveform_engine.set_busy(current.playing || current.source.playing
        || current.program_request_pending || current.source_request_pending
        || current.export_running || current.pointer.down || ui.get_live_input_active());
    let mut changed = false;
    while let Some(result) = current.waveform_engine.poll() {
        let valid = current.editor.project.media(result.media.id)
            .is_some_and(|m| same_waveform_source(m, &result.media));
        if !valid { continue; }
        if let Ok(peaks) = result.peaks {
            current.waveform_results.retain(|(m, _)| m.id != result.media.id);
            while current.waveform_results.len() >= 16
                || current.waveform_results.iter().map(|(_, p)| p.bytes()).sum::<usize>() + peaks.bytes() > 8_000_000 {
                if current.waveform_results.pop_back().is_none() { break; }
            }
            current.waveform_results.push_front((result.media, peaks));
            changed = true;
        }
    }
    // Slint owns vertical scrolling; retain it for raster filtering without
    // changing the complete clip geometry used by editing and hit testing.
    current.waveform_scroll_y = ui.get_timeline_scroll_y();
    let seq = &current.editor.project.sequence;
    let key = (current.edit_gen, seq.scroll.to_bits(), seq.zoom.to_bits(), current.timeline_w.to_bits(),
        current.waveform_scroll_y.to_bits(), current.track_body.to_bits());
    if current.waveform_view_key != Some(key) {
        // Queue only visible audio media, de-duplicated. Existing cached peaks
        // remain usable while playback prevents background preparation.
        let mut requested = Vec::new();
        for row in rows_for(&current) {
            if !waveform_row_visible(row.y, row.h, current.waveform_scroll_y, current.track_body) { continue; }
            let track = &seq.tracks[row.track_index];
            if track.kind != TrackKind::Audio { continue; }
            for clip in &track.clips {
                let x = (clip.timeline_start as f64 - seq.scroll) as f32 * seq.zoom as f32;
                let width = clip.duration as f32 * seq.zoom as f32;
                if !timeline_span_visible(x, width, current.timeline_w) || requested.contains(&clip.media_id) { continue; }
                if let Some(media) = current.editor.project.media(clip.media_id) {
                    current.waveform_engine.request(media.clone());
                    requested.push(clip.media_id);
                }
                if requested.len() >= 16 { break; }
            }
            if requested.len() >= 16 { break; }
        }
        current.waveform_view_key = Some(key);
        // A vertical-only move must install cached peaks for newly visible rows
        // and release raster images belonging to rows that just left the view.
        changed = true;
    }
    drop(current);
    if changed { ui.set_clips(model(clip_views(&state.borrow()))); }
}

fn waveform_row_visible(y: f32, height: f32, scroll: f32, viewport: f32) -> bool {
    y.is_finite() && height.is_finite() && scroll.is_finite() && viewport.is_finite()
        && height > 0.0 && viewport > 0.0 && y < scroll + viewport && y + height > scroll
}

fn waveform_picture(state: &AppState, clip: &Clip, x: f32, width: f32, height: f32) -> (slint::Image, f32, f32) {
    let Some(media) = state.editor.project.media(clip.media_id) else { return (slint::Image::default(), 0.0, 0.0); };
    let Some((_, peaks)) = state.waveform_results.iter().find(|(m, _)| same_waveform_source(m, media)) else { return (slint::Image::default(), 0.0, 0.0); };
    let left = x.max(0.0);
    let right = (x + width).min(state.timeline_w);
    let visible = right - left;
    if visible < 1.0 || !visible.is_finite() || height < 24.0 { return (slint::Image::default(), 0.0, 0.0); }
    let count = (visible.ceil() as usize).clamp(1, 1024);
    let image_height = ((height - 24.0).floor() as u32).clamp(4, 64);
    let rate = state.editor.project.sequence.frame_rate;
    let seconds_per_pixel = 1.0 / (state.editor.project.sequence.zoom * rate.fps());
    let start = clip.source_start_seconds(rate) + (left-x) as f64 * seconds_per_pixel;
    let end = start + visible as f64 * seconds_per_pixel;
    // Display a combined peak envelope (not a downmix).
    let mut columns = vec![playback::timeline_waveforms::Peak::default(); count];
    for channel in 0..peaks.channels {
        for (dst, source) in columns.iter_mut().zip(peaks.columns(channel, start, end, count)) {
            dst.min = dst.min.min(source.min); dst.max = dst.max.max(source.max);
        }
    }
    let mut buffer = SharedPixelBuffer::<slint::Rgba8Pixel>::new(count as u32, image_height);
    let pixels = buffer.make_mut_slice();
    for (column, peak) in columns.iter().enumerate() {
        let center = (image_height - 1) as f32 / 2.0;
        let top = (center - peak.max.clamp(-1.0, 1.0) * center).floor().max(0.0) as usize;
        let bottom = (center - peak.min.clamp(-1.0, 1.0) * center).ceil().min((image_height-1) as f32) as usize;
        for row in top..=bottom { pixels[row*count+column] = slint::Rgba8Pixel { r: 207, g: 236, b: 214, a: 190 }; }
    }
    (slint::Image::from_rgba8(buffer), left-x, visible)
}

fn pump_thumbnails(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let mut changed = false;
    let mut completed = false;
    let mut current = state.borrow_mut();
    while let Some(result) = current.thumbnails.poll() {
        completed = true;
        current
            .thumbnail_pending
            .retain(|pending| !same_picture(pending, &result.media));
        if let Some(media) = current
            .editor
            .project
            .media(result.media.id)
            .filter(|media| same_picture(media, &result.media))
            .cloned()
        {
            current
                .thumbnail_cache
                .retain(|(media, _)| media.id != result.media.id);
            current
                .thumbnail_cache
                .push_back((media, result.image.as_ref().map(to_image)));
            while current.thumbnail_cache.len() > 128 {
                current.thumbnail_cache.pop_front();
            }
            changed = true;
        }
    }
    // Playback ticks only poll completions and compare this small key. Project
    // mutations (including import/relink/undo) advance edit_gen; changing the
    // bin/query invalidates independently. Even stale completions free a slot.
    if !thumbnail_schedule_needed(
        current.thumbnail_schedule_key.as_ref(),
        current.edit_gen,
        &current.current_bin,
        &current.query,
        completed,
    ) {
        return;
    }
    current.thumbnail_schedule_key = Some(ThumbnailScheduleKey {
        revision: current.edit_gen,
        bin: current.current_bin.clone(),
        query: current.query.clone() });
    let candidates = current
        .editor
        .project
        .media
        .iter()
        .filter(|media| media.has_video || media.kind == MediaKind::Still)
        .filter(|media| current.current_bin.is_empty() || media.bin == current.current_bin)
        .filter(|media| {
            current.query.is_empty() || media.name.to_lowercase().contains(&current.query)
        })
        .take(128)
        .cloned()
        .collect::<Vec<_>>();
    // The worker's completion mailbox holds 16 terminal results. Keeping at most
    // 16 requests outstanding also survives native dialogs that pause UI polling.
    for media in candidates {
        if current.thumbnail_pending.len() >= 16 {
            break;
        }
        if current
            .thumbnail_cache
            .iter()
            .any(|(cached, _)| same_picture(cached, &media))
            || current
                .thumbnail_pending
                .iter()
                .any(|pending| same_picture(pending, &media))
        {
            continue;
        }
        current
            .thumbnail_cache
            .retain(|(cached, _)| cached.id != media.id);
        current.thumbnails.request(media.clone());
        current.thumbnail_pending.push(media);
    }
    drop(current);
    if changed {
        ui.set_bin_items(model(bin_views(&state.borrow())));
    }
}

fn same_picture(a: &Media, b: &Media) -> bool {
    a.id == b.id
        && a.path == b.path
        && a.kind == b.kind
        && a.width == b.width
        && a.height == b.height
        && a.source_fps == b.source_fps
        && a.duration_secs == b.duration_secs
        && a.has_video == b.has_video
}

fn pump_preview(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    if state.borrow().program_picture_reset {
        ui.set_show_picture(false);
        state.borrow_mut().program_picture_reset = false;
    }
    let (gen, frame, w, h, project) = {
        let state = state.borrow();
        let (mut dst_w, mut dst_h) = display_pixels(ui, &state);
        if state.pointer.down && state.pointer.seeking {
            // Keep active scrubs light. Releasing the pointer requests the chosen
            // preview quality again; exported resolution and quality are unchanged.
            dst_w = (dst_w / 2).max(2);
            dst_h = (dst_h / 2).max(2);
        }
        let (w, h) = preview_canvas(
            state.editor.project.sequence.width,
            state.editor.project.sequence.height,
            dst_w,
            dst_h,
        );
        (
            state.picture_gen,
            state.editor.project.sequence.playhead,
            w,
            h,
            if state.sent_gen != state.picture_gen {
                Some(state.editor.project.clone())
            } else {
                None
            },
        )
    };
    if let Some(project) = project {
        state.borrow().preview.set_project(project);
        state.borrow_mut().sent_gen = gen;
        state.borrow_mut().shown_frame = -1;
        let mut current = state.borrow_mut();
        current.preview_min_token = current.preview_token.saturating_add(1);
    }
    let intent = {
        let current = state.borrow();
        preview_request_intent(current.playing, current.waiting_picture, current.waiting_audio,
            current.speed, current.pointer.down && current.pointer.seeking)
    };
    let need = {
        let state = state.borrow();
        state.shown_frame != frame || state.preview_size != (w, h) || state.program_intent != intent
    };
    if need {
        let token = {
            let current = state.borrow();
            match intent {
                PreviewIntent::Playback => current.preview.request_playback(frame, w, h),
                PreviewIntent::Scrub => current.preview.request_scrub(frame, w, h),
                PreviewIntent::FinalSeek => current.preview.request(frame, w, h) }
        };
        let mut current = state.borrow_mut();
        current.preview_token = token;
        current.program_request_pending = true;
        current.program_feedback.request(Instant::now());
        current.optimized_cache.set_busy(true);
        current.waveform_engine.set_busy(true);
        current.filmstrips.set_busy(true);
        playback::set_index_busy(true);
        current.shown_frame = frame;
        current.preview_size = (w, h);
        current.program_intent = intent;
        if !(current.playing || current.pointer.down && current.pointer.seeking) {
            current.preview_min_token = token;
        }
    }
    let mut latest = None;
    while let Some(image) = state.borrow().preview.poll() {
        latest = Some(image);
    }
    if let Some(image) = latest {
        let mut current = state.borrow_mut();
        if accepts_preview_reply(
            image.token,
            current.preview_token,
            current.preview_min_token,
            current.playing,
            current.pointer.down && current.pointer.seeking,
            image.error.is_some(),
        ) {
            let is_latest = image.token == current.preview_token;
            if is_latest { current.program_request_pending = false; }
            if let Some(error) = &image.error {
                current.program_feedback.fail(error.clone());
                current.presented_picture = None;
                current.status = format!("Preview: {error}");
                if current.playing || current.waiting_audio { current.audio.stop(); }
                current.playing = false;
                current.waiting_picture = false;
                current.waiting_audio = false;
                current.speed = 0.0;
                current.program_intent = preview_request_intent(false, false, false, 0.0,
                    current.pointer.down && current.pointer.seeking);
                ui.set_status_text(current.status.clone().into());
                push_clock(ui, &current);
                sync_viewer_status(ui, &current);
                return;
            }
            let before_upload = Instant::now();
            ui.set_viewer_image(to_image(&image.image));
            ui.set_show_picture(true);
            current.program_feedback.picture(is_latest, Instant::now());
            current.preview_telemetry.record("Program", &image, before_upload);
            current.presented_picture = (image.error.is_none() && !image.proxy_used).then_some((gen, image.frame));
            if current.waiting_picture
                && !image.proxy_used
                && image.frame == current.anchor_frame
                && gen == current.picture_gen
            {
                current.waiting_picture = false;
                if current.waiting_audio {
                    // Starting audio before the first picture would let sound
                    // run ahead while a cold decoder prepares the playback origin.
                    current
                        .audio
                        .play(current.editor.project.clone(), current.anchor_frame);
                    current.status = "Preparing audio…".into();
                } else {
                    current.anchor_at = Instant::now();
                    current.status = "Playing".into();
                }
                ui.set_status_text(current.status.clone().into());
            }
        }
    }
    sync_viewer_status(ui, &state.borrow());
}

fn sync_viewer_status(ui: &AppWindow, state: &AppState) {
    let now = Instant::now();
    ui.set_program_preparing(state.program_feedback.preparing(now));
    ui.set_program_problem(state.program_feedback.problem.clone().into());
    ui.set_source_preparing(state.source_feedback.preparing(now));
    ui.set_source_problem(state.source_feedback.problem.clone().into());
}

fn accepts_preview_image(
    token: u64,
    latest: u64,
    minimum: u64,
    playing: bool,
    scrubbing: bool,
) -> bool {
    token == latest || (playing || scrubbing) && token >= minimum && token <= latest
}

fn accepts_preview_reply(token: u64, latest: u64, minimum: u64, playing: bool,
    scrubbing: bool, failed: bool) -> bool {
    // Old successful pictures are useful while moving. An old failure must not
    // stop newer decoding, clear its pending state, or replace a valid picture.
    accepts_preview_image(token, latest, minimum, playing, scrubbing)
        && (!failed || token == latest)
}

fn preview_request_intent(playing: bool, waiting_picture: bool, waiting_audio: bool,
    speed: f64, scrubbing: bool) -> PreviewIntent {
    if waiting_picture || waiting_audio { PreviewIntent::FinalSeek }
    else if scrubbing || playing && (speed - 1.0).abs() >= 0.01 { PreviewIntent::Scrub }
    else if playing { PreviewIntent::Playback }
    else { PreviewIntent::FinalSeek }
}

fn prepare_picture_start(
    presented: Option<(u64, i64)>,
    generation: u64,
    frame: i64,
    requested: i64,
) -> (bool, i64) {
    if presented == Some((generation, frame)) {
        (false, requested)
    } else {
        // A failed reply has already consumed the previous request. Retry the
        // origin even when the requested frame has not changed since that failure.
        (true, -1)
    }
}

fn playback_clock_ready(playing: bool, waiting_picture: bool, waiting_audio: bool) -> bool {
    playing && !waiting_picture && !waiting_audio
}

fn light_sync(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    {
        let guard = state.borrow();
        push_clock(ui, &guard);
    }
    let key = {
        let guard = state.borrow();
        TimelineViewKey::capture(
            &guard.editor.project,
            guard.edit_gen,
            guard.timeline_w,
            guard.track_body,
        )
    };
    if state.borrow().timeline_view_key != Some(key) {
        let (clips, transitions, ticks) = {
            let guard = state.borrow();
            (clip_views(&guard), trans_views(&guard), tick_views(&guard))
        };
        ui.set_clips(model(clips));
        ui.set_trans(model(transitions));
        ui.set_ticks(model(ticks));
        state.borrow_mut().timeline_view_key = Some(key);
    }
    ui.set_can_split(editing_availability(&state.borrow().editor).1);
}

fn glyphs(tc: &str) -> Vec<Glyph> {
    tc.chars()
        .map(|ch| Glyph {
            ch: ch.to_string().into(),
            sep: ch == ':' || ch == ';' })
        .collect()
}

fn update_glyph_model(current: ModelRc<Glyph>, text: &str) -> ModelRc<Glyph> {
    if let Some(rows) = current.as_any().downcast_ref::<VecModel<Glyph>>() {
        if rows.row_count() == text.chars().count() {
            for (index, ch) in text.chars().enumerate() {
                let sep = ch == ':' || ch == ';';
                let value = ch.to_string();
                if rows
                    .row_data(index)
                    .is_none_or(|row| row.ch.as_str() != value || row.sep != sep)
                {
                    rows.set_row_data(
                        index,
                        Glyph {
                            ch: value.into(),
                            sep },
                    );
                }
            }
            return current;
        }
    }
    model(glyphs(text))
}

fn push_clock(ui: &AppWindow, state: &AppState) {
    let seq = &state.editor.project.sequence;
    let rate = seq.frame_rate;
    let px = seq.zoom as f32;
    let scroll = seq.scroll as f32;
    let play = format_timecode(seq.playhead, rate);
    let dur = format_timecode(seq.duration_frames(), rate);
    let (shuttle, silent, playback) = shuttle_parts(state.playing, state.speed);
    ui.set_timecode(SharedString::from(play.clone()));
    ui.set_program_tc(update_glyph_model(ui.get_program_tc(), &play));
    ui.set_duration_tc(update_glyph_model(ui.get_duration_tc(), &dur));
    ui.set_bar_tc(update_glyph_model(ui.get_bar_tc(), &play));
    sync_source_controls(ui, state);
    ui.set_shuttle(SharedString::from(shuttle));
    ui.set_shuttle_silent(silent);
    ui.set_playback_label(SharedString::from(playback));
    ui.set_playhead_x((seq.playhead as f32 - scroll) * px);
    ui.set_playhead_frame(seq.playhead as f32);
    ui.set_seq_frames(seq.duration_frames().max(1) as f32);
    ui.set_in_frame(seq.in_point.unwrap_or(0) as f32);
    ui.set_out_frame(seq.out_point.unwrap_or(0) as f32);
    ui.set_scroll_frames(scroll);
    ui.set_px(px);
    ui.set_in_on(seq.in_point.is_some());
    ui.set_out_on(seq.out_point.is_some());
    if let Some(frame) = seq.in_point {
        ui.set_in_x((frame as f32 - scroll) * px);
    }
    if let Some(frame) = seq.out_point {
        ui.set_out_x((frame as f32 - scroll) * px);
    }
}

fn source_clock(state: &AppState) -> (String, String) {
    let rate = state.source.rate;
    let zero = format_timecode(0, rate);
    let Some(id) = state.editor.selected_media else {
        return (zero.clone(), zero);
    };
    let Some(media) = state.editor.project.media(id) else {
        return (zero.clone(), zero);
    };
    (
        format_timecode(state.source.frame, rate),
        format_timecode(media.source_frames(), rate),
    )
}

fn sync_source_controls(ui: &AppWindow, state: &AppState) {
    let (cursor, duration) = source_clock(state);
    ui.set_source_timecode(cursor.clone().into());
    ui.set_source_tc(update_glyph_model(ui.get_source_tc(), &cursor));
    ui.set_source_dur_tc(update_glyph_model(ui.get_source_dur_tc(), &duration));
    ui.set_source_playing(state.source.playing);
    ui.set_source_position(state.source.frame as f32 / state.source.total_frames.max(1) as f32);
    let media = state
        .editor
        .selected_media
        .and_then(|id| state.editor.project.media(id));
    ui.set_source_usable(media.is_some() && state.source_usable);
    ui.set_source_range_on(
        media
            .is_some_and(|media| media.source_in_mark.is_some() || media.source_out_mark.is_some()),
    );
    if let Some(media) = media {
        let rate = media.source_rate();
        if let Ok(range) = media.source_range() {
            let total = media.source_duration_seconds().max(1e-9);
            ui.set_source_in_position((range.start.seconds() / total) as f32);
            ui.set_source_out_position((range.end.seconds() / total) as f32);
            let native_duration = rate.seconds_to_frame(range.duration_seconds()).max(1);
            let placement = media.source_placement_range(state.editor.project.sequence.frame_rate);
            let label = if media.source_in_mark.is_some() || media.source_out_mark.is_some() {
                match placement {
                    Ok(placement) => format!(
                        "{} → {} · {} source / {} sequence frames",
                        format_timecode(range.first_frame(rate), rate),
                        format_timecode(range.last_frame(rate), rate),
                        native_duration,
                        placement.duration
                    ),
                    Err(error) => error }
            } else {
                format!(
                    "Entire source · {}",
                    if media.kind == MediaKind::Audio {
                        "millisecond ticks".to_string()
                    } else {
                        format!("{} fps", rate.label())
                    }
                )
            };
            ui.set_source_range_label(label.into());
        }
        ui.set_source_placeholder(if let Some(error) = &state.source_error {
            error.clone().into()
        } else if !state.source_usable {
            "Media is offline. Relink it in Project.".into()
        } else if !media.has_video && media.kind != MediaKind::Still {
            "Audio source · use playback and In/Out to choose a range.".into()
        } else {
            "Loading source picture…".into()
        });
    } else {
        ui.set_source_in_position(0.0);
        ui.set_source_out_position(1.0);
        ui.set_source_range_label("Select media to preview and mark a range.".into());
        ui.set_source_placeholder(
            "Select media in the Project bin to preview, scrub, and mark In/Out.".into(),
        );
    }
}

fn set_source_speed(state: &Rc<std::cell::RefCell<AppState>>, speed: f64) {
    let mut current = state.borrow_mut();
    current.playing = false;
    current.waiting_audio = false;
    current.waiting_picture = false;
    current.speed = 0.0;
    current.audio.stop();
    if speed != 0.0 {
        current.source_error = None;
    }
    current.source_requested = None;
    current.source_min_token = current.source_token.saturating_add(1);
    if speed > 0.0 && current.source.frame >= current.source.total_frames - 1 {
        current.source.seek(0);
    }
    current.source.set_speed(speed);
    let picture = current.source_project.as_ref().is_some_and(|project| {
        project.media.iter().any(|media| media.has_video || media.kind == MediaKind::Still)
    });
    current.source.waiting_picture = current.source.playing && picture
        && current.source_presented != current.source.media_id.map(|id| (id, current.source.frame));
    if current.source.waiting_picture {
        current.status = "Preparing source picture…".into();
    } else {
        start_source_audio(&mut current);
    }
}

fn start_source_audio(current: &mut AppState) {
    if current.source.waiting_audio {
        if let Some(project) = current.source_project.clone() {
            if project.media.iter().any(|media| media.has_audio) {
                current.audio.play(project, current.source.frame);
                current.status = "Preparing source audio…".into();
            } else {
                current.source.audio_ready();
                current.status = "Source playing".into();
            }
        } else {
            current.source.stop();
        }
    } else {
        current.status = if current.source.playing {
            "Source shuttle · audio at 1× forward"
        } else {
            "Source stopped"
        }
        .into();
    }
}

fn on_source_command(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, action: &str) {
    pump_source_preview(ui, state);
    let selected = {
        let current = state.borrow();
        current
            .editor
            .selected_media
            .and_then(|id| current.editor.project.media(id))
            .cloned()
    };
    let Some(media) = selected.filter(|_| state.borrow().source_usable) else {
        return;
    };
    match action {
        "play-toggle" => {
            let pause = {
                let current = state.borrow();
                current.source.playing && (current.source.speed - 1.0).abs() < 0.01
            };
            if pause {
                stop_play(state);
            } else {
                set_source_speed(state, 1.0);
            }
        }
        "shuttle-j" | "shuttle-l" => {
            let speed = state
                .borrow()
                .source
                .next_shuttle_speed(if action == "shuttle-j" { -1.0 } else { 1.0 });
            set_source_speed(state, speed);
        }
        "shuttle-k" => stop_play(state),
        "step-back" | "step-fwd" | "step-back-second" | "step-fwd-second" | "home" | "end"
        | "goto-in" | "goto-out" => {
            stop_play(state);
            let mut current = state.borrow_mut();
            let frame = current.source.frame;
            let second = current.source.rate.nominal();
            let range = media.source_range().ok();
            let target = match action {
                "step-back" => frame.saturating_sub(1),
                "step-fwd" => frame.saturating_add(1),
                "step-back-second" => frame.saturating_sub(second),
                "step-fwd-second" => frame.saturating_add(second),
                "home" => 0,
                "end" => current.source.total_frames - 1,
                "goto-in" => range.map_or(0, |range| range.first_frame(current.source.rate)),
                _ => range.map_or(current.source.total_frames - 1, |range| {
                    range.last_frame(current.source.rate)
                }) };
            current.source.seek(target);
            current.source_requested = None;
            current.source_min_token = current.source_token.saturating_add(1);
            current.status = "Source paused".into();
        }
        "mark-in" | "mark-out" | "clear-io" => {
            let point = {
                let current = state.borrow();
                SourceTime::new(current.source.frame, current.source.rate)
            };
            let out = media.source_out_after_frame(point.frame);
            act(state, |editor| {
                editor.edit("source-marks", |project| {
                    let media = project
                        .media
                        .iter_mut()
                        .find(|item| item.id == media.id)
                        .ok_or("Source media is missing")?;
                    match action {
                        "mark-in" => media.mark_source_in(point),
                        "mark-out" => media.mark_source_out(out),
                        _ => {
                            media.clear_source_marks();
                            Ok(())
                        }
                    }
                })
            });
            full_sync(ui, state);
        }
        _ => return }
    {
        let mut current = state.borrow_mut();
        current.source_requested = None;
        current.source_min_token = current.source_token.saturating_add(1);
    }
    sync_source_controls(ui, &state.borrow());
    pump_source_preview(ui, state);
}

fn on_source_scrub(
    ui: &AppWindow,
    state: &Rc<std::cell::RefCell<AppState>>,
    phase: i32,
    position: f32,
) {
    pump_source_preview(ui, state);
    if state.borrow().source.media_id.is_none() {
        return;
    }
    if phase == 0 {
        let frame = state.borrow().source.frame;
        stop_play(state);
        state.borrow_mut().source.scrub_origin = Some(frame);
    }
    if phase == 3 {
        let mut current = state.borrow_mut();
        if let Some(frame) = current.source.scrub_origin.take() {
            current.source.seek(frame);
        }
    } else if state.borrow().source.scrub_origin.is_some() {
        let mut current = state.borrow_mut();
        let frame =
            (position.clamp(0.0, 1.0) as f64 * current.source.total_frames as f64).round() as i64;
        current.source.seek(frame);
        if phase == 2 {
            current.source.scrub_origin = None;
        }
    }
    if phase != 1 {
        let mut current = state.borrow_mut();
        current.source_requested = None;
        current.source_min_token = current.source_token.saturating_add(1);
    }
    sync_source_controls(ui, &state.borrow());
    pump_source_preview(ui, state);
}

fn shuttle_parts(playing: bool, speed: f64) -> (String, bool, String) {
    if !playing || speed.abs() < 0.001 {
        return ("STOP".into(), false, "Stopped".into());
    }
    let mag = speed.abs();
    let digits = if (mag - mag.round()).abs() < 0.01 {
        format!("{}", mag.round() as i32)
    } else {
        format!("{mag:.1}")
    };
    let shuttle = if speed < 0.0 {
        format!("REV {digits}×")
    } else {
        format!("FWD {digits}×")
    };
    let silent = (speed - 1.0).abs() > 0.01;
    let playback = if silent {
        shuttle.clone()
    } else {
        "Playing 1×".into()
    };
    (shuttle, silent, playback)
}

fn zoom_percent(zoom: f64) -> String {
    format!("{}%", (zoom / 8.0 * 100.0).round() as i32)
}

fn wheel_text(value: f32) -> SharedString {
    SharedString::from(format!("{value:.2}"))
}

fn full_sync(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    sync_media_selection(&mut state.borrow_mut());
    sync_update_ui(ui, state);
    {
        let current = state.borrow();
        ui.set_project_selected_count(current.media_selection.ids.len() as i32);
        ui.set_media_sort_column(current.media_sort.column);
        ui.set_media_sort_descending(current.media_sort.descending);
    }
    {
        let current = state.borrow();
        let settings = &current.preferences.preview_cache;
        current.optimized_cache.configure(settings.clone());
        ui.set_preview_cache_auto(settings.automatic);
        ui.set_preview_cache_paused(settings.paused);
        ui.set_preview_cache_limit_gb(settings.limit_gb as i32);
        sync_preview_cache_status(&ui, &current.optimized_cache);
    }
    state.borrow_mut().syncing = true;
    {
        let mut current = state.borrow_mut();
        if !current.current_bin.is_empty()
            && !current.editor.project.bins.contains(&current.current_bin)
        {
            current.current_bin.clear();
        }
        ui.set_current_bin(current.current_bin.clone().into());
        ui.set_bins(model(
            current
                .editor
                .project
                .bins
                .iter()
                .map(|bin| SharedString::from(bin.as_str()))
                .collect(),
        ));
    }
    let snap = {
        let state = state.borrow();
        SyncSnap::capture(&state)
    };
    // Callback bindings depend on this revision, after the state borrow ends.
    ui.set_shortcut_revision(ui.get_shortcut_revision().wrapping_add(1));
    ui.set_window_title(SharedString::from(snap.title));
    ui.set_profile_name(SharedString::from(snap.profile));
    ui.set_timecode(SharedString::from(snap.timecode));
    ui.set_status_text(SharedString::from(snap.status));
    ui.set_sequence_label(SharedString::from(snap.sequence_label.clone()));
    ui.set_snap_on(snap.snap);
    ui.set_tool_razor(snap.razor);
    ui.set_armed_label(SharedString::from(snap.armed));
    ui.set_shuttle(SharedString::from(snap.shuttle));
    ui.set_zoom_label(SharedString::from(snap.zoom));
    ui.set_hint_insert(binding_hint(state, "insert"));
    ui.set_hint_overwrite(binding_hint(state, "overwrite"));
    ui.set_hint_razor(binding_hint(state, "tool-razor"));
    ui.set_hint_split(binding_hint(state, "split"));
    ui.set_hint_ripple(binding_hint(state, "ripple-delete"));
    ui.set_hint_lift(binding_hint(state, "lift"));
    ui.set_hint_snap(binding_hint(state, "snap"));
    ui.set_bin_items(model(snap.bin.clone()));
    let video: Vec<_> = snap
        .bin
        .iter()
        .filter(|item| item.kind.as_str() != "A")
        .cloned()
        .collect();
    let audio: Vec<_> = snap
        .bin
        .iter()
        .filter(|item| item.kind.as_str() == "A")
        .cloned()
        .collect();
    ui.set_video_items(model(video));
    ui.set_audio_items(model(audio));
    ui.set_lanes(model(snap.heads.clone()));
    ui.set_heads(model(snap.heads));
    ui.set_has_media(!snap.bin.is_empty());
    {
        let current = state.borrow();
        ui.set_has_selected_media(
            current
                .editor
                .selected_media
                .and_then(|id| current.editor.project.media(id))
                .is_some(),
        );
        ui.set_sequence_aspect(
            current.editor.project.sequence.width as f32
                / current.editor.project.sequence.height.max(1) as f32,
        );
    }
    audio_mix::sync_controls(ui, state);
    ui.set_has_clips(
        state
            .borrow()
            .editor
            .project
            .sequence
            .tracks
            .iter()
            .any(|track| !track.clips.is_empty()),
    );
    let (editable, can_split) = editing_availability(&state.borrow().editor);
    ui.set_can_undo(state.borrow().editor.can_undo());
    ui.set_can_redo(state.borrow().editor.can_redo());
    ui.set_can_edit_selection(editable);
    ui.set_can_split(can_split);
    ui.set_clips(model(snap.clips));
    ui.set_trans(model(snap.trans));
    ui.set_ticks(model(snap.ticks));
    ui.set_playhead_x(snap.playhead_x);
    ui.set_in_on(snap.in_on);
    ui.set_out_on(snap.out_on);
    ui.set_in_x(snap.in_x);
    ui.set_out_x(snap.out_x);
    ui.set_tracks_h(snap.tracks_h);
    ui.set_inspector_mode(snap.inspector_mode);
    ui.set_inspector_title(SharedString::from(snap.inspector_title));
    ui.set_pos_x(snap.pos_x);
    ui.set_pos_y(snap.pos_y);
    ui.set_scale_v(snap.scale);
    ui.set_opacity_v(snap.opacity);
    ui.set_volume_v(snap.volume);
    ui.set_exposure(snap.exposure);
    ui.set_contrast(snap.contrast);
    ui.set_highlights(snap.highlights);
    ui.set_shadows(snap.shadows);
    ui.set_temperature(snap.temperature);
    ui.set_tint(snap.tint);
    ui.set_saturation(snap.saturation);
    ui.set_lift_m(snap.lift[0]);
    ui.set_lift_r(snap.lift[1]);
    ui.set_lift_g(snap.lift[2]);
    ui.set_lift_b(snap.lift[3]);
    ui.set_gamma_m(snap.gamma[0]);
    ui.set_gamma_r(snap.gamma[1]);
    ui.set_gamma_g(snap.gamma[2]);
    ui.set_gamma_b(snap.gamma[3]);
    ui.set_gain_m(snap.gain[0]);
    ui.set_gain_r(snap.gain[1]);
    ui.set_gain_g(snap.gain[2]);
    ui.set_gain_b(snap.gain[3]);
    ui.set_read_pos_x(SharedString::from(snap.read_pos_x));
    ui.set_read_pos_y(SharedString::from(snap.read_pos_y));
    ui.set_read_scale(SharedString::from(snap.read_scale));
    ui.set_read_opacity(SharedString::from(snap.read_opacity));
    ui.set_read_volume(SharedString::from(snap.read_volume));
    ui.set_read_exposure(SharedString::from(snap.read_exposure));
    ui.set_read_contrast(SharedString::from(snap.read_contrast));
    ui.set_read_highlights(SharedString::from(snap.read_highlights));
    ui.set_read_shadows(SharedString::from(snap.read_shadows));
    ui.set_read_temperature(SharedString::from(snap.read_temperature));
    ui.set_read_tint(SharedString::from(snap.read_tint));
    ui.set_read_saturation(SharedString::from(snap.read_saturation));
    ui.set_trans_name(SharedString::from(snap.trans_name));
    ui.set_trans_dur(snap.trans_dur);
    ui.set_read_trans(SharedString::from(snap.read_trans));
    ui.set_rates(model(snap.rates));
    ui.set_presets(model(snap.presets));
    ui.set_shortcuts(model(snap.shortcuts));
    sync_commands(ui, state);
    sync_keybindings(ui, state);
    ui.set_export_busy(snap.export_busy);
    sync_queue(ui, state);
    let (project_name, source_name, seq_name, seq_fps, scratch) = {
        let state = state.borrow();
        let source = state
            .editor
            .selected_media
            .and_then(|id| state.editor.project.media(id))
            .map(|media| media.name.clone())
            .unwrap_or_else(|| "No clip".into());
        let scratch = state
            .editor
            .path
            .as_ref()
            .and_then(|path| path.parent())
            .map(|dir| dir.display().to_string())
            .unwrap_or_else(|| "Local".into());
        (
            state.editor.project.name.clone(),
            source,
            state.editor.project.sequence.name.clone(),
            state.editor.project.sequence.frame_rate.label(),
            scratch,
        )
    };
    ui.set_project_name(SharedString::from(project_name));
    ui.set_source_name(SharedString::from(source_name));
    ui.set_seq_name(SharedString::from(seq_name));
    ui.set_seq_fps(SharedString::from(seq_fps));
    ui.set_footer_scratch(SharedString::from(scratch));
    ui.set_read_lift_m(wheel_text(snap.lift[0]));
    ui.set_read_lift_r(wheel_text(snap.lift[1]));
    ui.set_read_lift_g(wheel_text(snap.lift[2]));
    ui.set_read_lift_b(wheel_text(snap.lift[3]));
    ui.set_read_gamma_m(wheel_text(snap.gamma[0]));
    ui.set_read_gamma_r(wheel_text(snap.gamma[1]));
    ui.set_read_gamma_g(wheel_text(snap.gamma[2]));
    ui.set_read_gamma_b(wheel_text(snap.gamma[3]));
    ui.set_read_gain_m(wheel_text(snap.gain[0]));
    ui.set_read_gain_r(wheel_text(snap.gain[1]));
    ui.set_read_gain_g(wheel_text(snap.gain[2]));
    ui.set_read_gain_b(wheel_text(snap.gain[3]));
    {
        let guard = state.borrow();
        let seq = &guard.editor.project.sequence;
        let vids = seq
            .tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Video)
            .count();
        let auds = seq
            .tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Audio)
            .count();
        let save = if guard.editor.path.is_some() && !guard.editor.dirty {
            "Saved"
        } else {
            "Unsaved"
        };
        let fps = seq.frame_rate.fps().max(1.0);
        ui.set_res_label(SharedString::from(format!("{}×{}", seq.width, seq.height)));
        ui.set_duration_label(SharedString::from(format_timecode(
            seq.duration_frames(),
            seq.frame_rate,
        )));
        ui.set_vid_count(SharedString::from(vids.to_string()));
        ui.set_aud_count(SharedString::from(auds.to_string()));
        ui.set_save_label(SharedString::from(save));
        ui.set_read_seconds(SharedString::from(format!(
            "{:.2}",
            snap.trans_dur as f64 / fps
        )));
        let anchor = guard
            .editor
            .selected_transition
            .and_then(|id| guard.editor.project.transition(id))
            .map(|transition| match transition.anchor {
                crate::project::Anchor::Head(_) => "Head",
                crate::project::Anchor::Tail(_) => "Tail",
                crate::project::Anchor::Cut(_) => "Cut" })
            .unwrap_or("Cut");
        ui.set_trans_anchor(SharedString::from(anchor));
        let armed = guard
            .editor
            .armed
            .and_then(|kind| {
                crate::project::TransitionKind::all()
                    .iter()
                    .position(|item| *item == kind)
            })
            .map(|index| index as i32)
            .unwrap_or(-1);
        ui.set_armed_kind(armed);
        push_clock(ui, &guard);
    }
    let mode = snap.inspector_mode;
    if state.borrow().inspector_seen != mode {
        state.borrow_mut().inspector_seen = mode;
        match mode {
            1 => ui.set_grade_tab(0),
            2 => ui.set_grade_tab(1),
            3 => ui.set_grade_tab(4),
            _ => {}
        }
    }
    let mut current = state.borrow_mut();
    current.timeline_view_key = Some(TimelineViewKey::capture(
        &current.editor.project,
        current.edit_gen,
        current.timeline_w,
        current.track_body,
    ));
    current.syncing = false;
}

struct SyncSnap {
    title: String,
    profile: String,
    timecode: String,
    status: String,
    sequence_label: String,
    snap: bool,
    razor: bool,
    armed: String,
    shuttle: String,
    zoom: String,
    hints: shortcuts::Hints,
    bin: Vec<BinItem>,
    heads: Vec<TrackHead>,
    clips: Vec<ClipView>,
    trans: Vec<TransView>,
    ticks: Vec<Tick>,
    playhead_x: f32,
    in_on: bool,
    out_on: bool,
    in_x: f32,
    out_x: f32,
    tracks_h: f32,
    inspector_mode: i32,
    inspector_title: String,
    pos_x: f32,
    pos_y: f32,
    scale: f32,
    opacity: f32,
    volume: f32,
    exposure: f32,
    contrast: f32,
    highlights: f32,
    shadows: f32,
    temperature: f32,
    tint: f32,
    saturation: f32,
    lift: [f32; 4],
    gamma: [f32; 4],
    gain: [f32; 4],
    read_pos_x: String,
    read_pos_y: String,
    read_scale: String,
    read_opacity: String,
    read_volume: String,
    read_exposure: String,
    read_contrast: String,
    read_highlights: String,
    read_shadows: String,
    read_temperature: String,
    read_tint: String,
    read_saturation: String,
    trans_name: String,
    trans_dur: f32,
    read_trans: String,
    rates: Vec<LabelItem>,
    presets: Vec<LabelItem>,
    shortcuts: Vec<ShortcutRow>,
    export_busy: bool }

impl SyncSnap {
    fn capture(state: &AppState) -> Self {
        let editor = &state.editor;
        let seq = &editor.project.sequence;
        let rate = seq.frame_rate;
        let px = seq.zoom as f32;
        let scroll = seq.scroll as f32;
        let profile = match editor.project.shortcut_profile {
            ShortcutProfile::Premiere => "Premiere",
            ShortcutProfile::Resolve => "Resolve" };
        let dirty = if editor.dirty { "*" } else { "" };
        let title = format!("Quaternio — {}{dirty}", editor.project.name);
        let (inspector_mode, inspector_title, clip_vals) = inspector(editor);
        let hints = shortcuts::hints(editor.project.shortcut_profile);
        Self {
            title,
            profile: profile.into(),
            timecode: format_timecode(seq.playhead, rate),
            status: state.status.clone(),
            sequence_label: format!(
                "{}×{}  {}  {}",
                seq.width,
                seq.height,
                rate.label(),
                seq.name
            ),
            snap: seq.snap,
            razor: editor.tool == Tool::Razor,
            armed: editor
                .armed
                .map(|kind| format!("{} armed", kind.label()))
                .unwrap_or_default(),
            shuttle: shuttle_parts(state.playing, state.speed).0,
            zoom: zoom_percent(seq.zoom),
            hints,
            bin: bin_views(state),
            heads: head_views(state),
            clips: clip_views(state),
            trans: trans_views(state),
            ticks: tick_views(state),
            playhead_x: (seq.playhead as f32 - scroll) * px,
            in_on: seq.in_point.is_some(),
            out_on: seq.out_point.is_some(),
            in_x: seq
                .in_point
                .map(|frame| (frame as f32 - scroll) * px)
                .unwrap_or(0.0),
            out_x: seq
                .out_point
                .map(|frame| (frame as f32 - scroll) * px)
                .unwrap_or(0.0),
            tracks_h: rows_for(state).iter().map(|row| row.h).sum(),
            inspector_mode,
            inspector_title,
            pos_x: clip_vals.pos_x,
            pos_y: clip_vals.pos_y,
            scale: clip_vals.scale,
            opacity: clip_vals.opacity,
            volume: clip_vals.volume,
            exposure: clip_vals.exposure,
            contrast: clip_vals.contrast,
            highlights: clip_vals.highlights,
            shadows: clip_vals.shadows,
            temperature: clip_vals.temperature,
            tint: clip_vals.tint,
            saturation: clip_vals.saturation,
            lift: clip_vals.lift,
            gamma: clip_vals.gamma,
            gain: clip_vals.gain,
            read_pos_x: format!("{}", clip_vals.pos_x.round() as i32),
            read_pos_y: format!("{}", clip_vals.pos_y.round() as i32),
            read_scale: format!("{}%", (clip_vals.scale * 100.0).round() as i32),
            read_opacity: format!("{}%", (clip_vals.opacity * 100.0).round() as i32),
            read_volume: format!("{}%", (clip_vals.volume * 100.0).round() as i32),
            read_exposure: format!("{:.2}", clip_vals.exposure),
            read_contrast: format!("{}", clip_vals.contrast.round() as i32),
            read_highlights: format!("{}", clip_vals.highlights.round() as i32),
            read_shadows: format!("{}", clip_vals.shadows.round() as i32),
            read_temperature: format!("{}", clip_vals.temperature.round() as i32),
            read_tint: format!("{}", clip_vals.tint.round() as i32),
            read_saturation: format!("{}", clip_vals.saturation.round() as i32),
            trans_name: clip_vals.trans_name,
            trans_dur: clip_vals.trans_dur,
            read_trans: format!("{}", clip_vals.trans_dur.round() as i32),
            rates: frame_rate_presets()
                .iter()
                .map(|rate| LabelItem {
                    text: rate.label().into() })
                .collect(),
            presets: sequence_presets()
                .iter()
                .map(|preset| LabelItem {
                    text: preset.label.into() })
                .collect(),
            shortcuts: shortcuts::cheat_sheet(editor.project.shortcut_profile)
                .iter()
                .map(|(keys, action)| ShortcutRow {
                    keys: (*keys).into(),
                    action: (*action).into() })
                .collect(),
            export_busy: state.export_running }
    }
}

struct ClipVals {
    pos_x: f32,
    pos_y: f32,
    scale: f32,
    opacity: f32,
    volume: f32,
    exposure: f32,
    contrast: f32,
    highlights: f32,
    shadows: f32,
    temperature: f32,
    tint: f32,
    saturation: f32,
    lift: [f32; 4],
    gamma: [f32; 4],
    gain: [f32; 4],
    trans_name: String,
    trans_dur: f32 }

fn inspector(editor: &timeline::Editor) -> (i32, String, ClipVals) {
    let mut vals = ClipVals {
        pos_x: 0.0,
        pos_y: 0.0,
        scale: 1.0,
        opacity: 1.0,
        volume: 1.0,
        exposure: 0.0,
        contrast: 0.0,
        highlights: 0.0,
        shadows: 0.0,
        temperature: 0.0,
        tint: 0.0,
        saturation: 0.0,
        lift: [0.0; 4],
        gamma: [0.0; 4],
        gain: [0.0; 4],
        trans_name: String::new(),
        trans_dur: editor.default_transition_frames() as f32 };
    if let Some(id) = editor.selected_transition {
        if let Some(transition) = editor.project.transition(id) {
            vals.trans_name = transition.kind.label().into();
            vals.trans_dur = transition.duration as f32;
            return (3, transition.kind.label().into(), vals);
        }
    }
    let Some(id) = editor.primary_clip() else {
        return (0, "No selection".into(), vals);
    };
    let Some(clip) = editor.project.clip(id) else {
        return (0, "No selection".into(), vals);
    };
    vals.pos_x = clip.pos_x;
    vals.pos_y = clip.pos_y;
    vals.scale = clip.scale;
    vals.opacity = clip.opacity;
    vals.volume = clip.volume;
    if let Some(audio) = editor.linked_audio(id) {
        if let Some(audio_clip) = editor.project.clip(audio) {
            vals.volume = audio_clip.volume;
        }
    }
    vals.exposure = clip.grade.exposure;
    vals.contrast = clip.grade.contrast;
    vals.highlights = clip.grade.highlights;
    vals.shadows = clip.grade.shadows;
    vals.temperature = clip.grade.temperature;
    vals.tint = clip.grade.tint;
    vals.saturation = clip.grade.saturation;
    vals.lift = [
        clip.grade.lift.master,
        clip.grade.lift.r,
        clip.grade.lift.g,
        clip.grade.lift.b,
    ];
    vals.gamma = [
        clip.grade.gamma.master,
        clip.grade.gamma.r,
        clip.grade.gamma.g,
        clip.grade.gamma.b,
    ];
    vals.gain = [
        clip.grade.gain.master,
        clip.grade.gain.r,
        clip.grade.gain.g,
        clip.grade.gain.b,
    ];
    let video = editor
        .project
        .track_index_of_clip(id)
        .and_then(|index| editor.project.sequence.tracks.get(index))
        .map(|track| track.kind == TrackKind::Video)
        .unwrap_or(false);
    let mode = if video { 1 } else { 2 };
    (mode, clip.name.clone(), vals)
}

fn visible_media(state: &AppState) -> Vec<&Media> {
    let mut media = state
        .editor
        .project
        .media
        .iter()
        .filter(|media| state.current_bin.is_empty() || media.bin == state.current_bin)
        .filter(|media| match state.filter_kind {
            1 => media.kind == MediaKind::Video,
            2 => media.kind == MediaKind::Audio,
            3 => media.kind == MediaKind::Still,
            _ => true })
        .filter(|media| state.query.is_empty() || media.name.to_lowercase().contains(&state.query))
        .collect::<Vec<_>>();
    state.media_sort.sort(&mut media);
    media
}

fn bin_views(state: &AppState) -> Vec<BinItem> {
    let rate = state.editor.project.sequence.frame_rate;
    let selected: std::collections::HashSet<_> = state.media_selection.ids.iter().copied().collect();
    visible_media(state).into_iter()
        .map(|media| {
            let kind = match media.kind {
                MediaKind::Video => "V",
                MediaKind::Audio => "A",
                MediaKind::Still => "S" };
            let frames = media.timeline_frames(rate).max(1);
            let tc_in = format_timecode(0, rate);
            let tc_out = format_timecode(frames.saturating_sub(1), rate);
            let duration = format_timecode(frames, rate);
            let info = if media.kind == MediaKind::Audio || media.width == 0 {
                "—".to_string()
            } else {
                format!("{}×{}", media.width, media.height)
            };
            let fps = if media.source_fps > 1.0 {
                let rounded = (media.source_fps * 100.0).round() / 100.0;
                if (rounded - rounded.round()).abs() < 0.001 {
                    format!("{}", rounded.round() as i32)
                } else {
                    format!("{rounded:.2}")
                }
            } else {
                rate.label()
            };
            BinItem {
                thumbnail: state
                    .thumbnail_cache
                    .iter()
                    .find(|(cached, _)| same_picture(cached, media))
                    .and_then(|(_, image)| image.clone())
                    .unwrap_or_default(),
                id: media.id as i32,
                name: media.name.clone().into(),
                kind: kind.into(),
                info: info.into(),
                selected: selected.contains(&media.id),
                tc_in: tc_in.into(),
                tc_out: tc_out.into(),
                duration: duration.into(),
                fps: fps.into(),
                offline: !media.path.exists() }
        })
        .collect()
}

fn rows_for(state: &AppState) -> Vec<TrackRow> {
    layout_rows(&state.editor.project.sequence, state.track_body)
}

fn refresh_tracks(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let (heads, clips, trans, tracks_h) = {
        let state = state.borrow();
        let heads = head_views(&state);
        let clips = clip_views(&state);
        let trans = trans_views(&state);
        let tracks_h = heads.iter().map(|head| head.h).sum();
        (heads, clips, trans, tracks_h)
    };
    ui.set_lanes(model(heads.clone()));
    ui.set_heads(model(heads));
    ui.set_clips(model(clips));
    ui.set_trans(model(trans));
    ui.set_tracks_h(tracks_h);
}

fn head_views(state: &AppState) -> Vec<TrackHead> {
    rows_for(state)
        .into_iter()
        .map(|row| {
            let track = &state.editor.project.sequence.tracks[row.track_index];
            TrackHead {
                index: row.track_index as i32,
                y: row.y,
                h: row.h,
                name: track.name.clone().into(),
                muted: track.muted,
                solo: track.solo,
                locked: track.locked,
                selected: state.editor.selected_track == Some(row.track_index),
                video: track.kind == TrackKind::Video }
        })
        .collect()
}

fn clip_views(state: &AppState) -> Vec<ClipView> {
    let seq = &state.editor.project.sequence;
    let px = seq.zoom as f32;
    let scroll = seq.scroll as f32;
    let mut views = Vec::new();
    let mut offline_by_media = std::collections::HashMap::new();
    for row in rows_for(state) {
        let track = &seq.tracks[row.track_index];
        for clip in &track.clips {
            let x = (clip.timeline_start as f32 - scroll) * px;
            let w = (clip.duration as f32 * px).max(2.0);
            if !timeline_span_visible(x, w, state.timeline_w) {
                continue;
            }
            let media = state.editor.project.media(clip.media_id);
            let still = media
                .map(|item| item.kind == MediaKind::Still)
                .unwrap_or(false);
            let offline = *offline_by_media
                .entry(clip.media_id)
                .or_insert_with(|| media.map(|item| !item.path.exists()).unwrap_or(true));
            let (waveform, waveform_x, waveform_width) = if track.kind == TrackKind::Audio
                && waveform_row_visible(row.y, row.h, state.waveform_scroll_y, state.track_body) {
                waveform_picture(state, clip, x, w, row.h)
            } else { (slint::Image::default(), 0.0, 0.0) };
            let (filmstrip, filmstrip_x, filmstrip_width) = if track.kind == TrackKind::Video
                && waveform_row_visible(row.y, row.h, state.waveform_scroll_y, state.track_body) {
                filmstrips::picture(state, clip, x, w, row.h)
            } else { (slint::Image::default(), 0.0, 0.0) };
            views.push(ClipView {
                waveform, waveform_x, waveform_width, filmstrip, filmstrip_x, filmstrip_width,
                id: clip.id as i32,
                x,
                y: row.y,
                w,
                h: row.h,
                name: clip.name.clone().into(),
                selected: state.editor.selected_clips.contains(&clip.id),
                video: track.kind == TrackKind::Video,
                dim: if track.kind == TrackKind::Audio {
                    !seq.audio_track_enabled(row.track_index)
                } else {
                    track.muted
                },
                still,
                offline });
        }
    }
    views
}

fn timeline_span_visible(x: f32, width: f32, viewport: f32) -> bool {
    x <= viewport.max(0.0) + 4.0 && x + width >= -4.0
}

fn trans_views(state: &AppState) -> Vec<TransView> {
    let seq = &state.editor.project.sequence;
    let px = seq.zoom as f32;
    let scroll = seq.scroll as f32;
    let rows = rows_for(state);
    let mut views = Vec::new();
    for transition in &seq.transitions {
        let Some((start, end)) = transition_window(seq, transition) else {
            continue;
        };
        let x = (start as f32 - scroll) * px;
        let w = ((end - start) as f32 * px).max(4.0);
        if !timeline_span_visible(x, w, state.timeline_w) {
            continue;
        }
        let Some(track_index) = state
            .editor
            .project
            .track_index_of_clip(transition.anchor.clip_id())
        else {
            continue;
        };
        let Some(row) = rows.iter().find(|row| row.track_index == track_index) else {
            continue;
        };
        let band = 16.0_f32.min(row.h);
        views.push(TransView {
            id: transition.id as i32,
            x,
            y: row.y + (row.h - band).max(0.0) * 0.5,
            w,
            h: band,
            selected: state.editor.selected_transition == Some(transition.id),
            audio: transition.kind.is_audio(),
            label: transition.kind.label().into() });
    }
    views
}

fn tick_views(state: &AppState) -> Vec<Tick> {
    let seq = &state.editor.project.sequence;
    let px = seq.zoom.max(0.25) as f32;
    let scroll = seq.scroll as f32;
    let span = (state.timeline_w / px).ceil() as i64 + 2;
    let start = scroll.floor() as i64;
    let mut step = 1i64;
    for candidate in [
        1, 2, 5, 10, 12, 15, 24, 30, 48, 60, 96, 120, 240, 480, 960, 1920,
    ] {
        if candidate as f32 * px >= 80.0 {
            step = candidate;
            break;
        }
    }
    let mut ticks = Vec::new();
    let mut frame = (start / step) * step;
    if frame < 0 {
        frame = 0;
    }
    while frame < start + span + step {
        let major = frame % (step * 2) == 0 || step >= 24;
        ticks.push(Tick {
            x: (frame as f32 - scroll) * px,
            label: if major && (frame as f32 - scroll) * px > -2.0 {
                format_timecode(frame, seq.frame_rate).into()
            } else {
                SharedString::default()
            },
            major });
        frame += step;
        if ticks.len() > 400 {
            break;
        }
    }
    ticks
}

fn model<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(items))
}

fn display_pixels(ui: &AppWindow, state: &AppState) -> (u32, u32) {
    let (width, height) =
        if ui.get_page() == 1 || ui.get_workspace() == 1 || ui.get_workspace() == 2 {
            (960.0, 540.0)
        } else {
            let (width, height) = viewer_pixels(state.view_w, state.view_h);
            (width as f32, height as f32)
        };
    let scale = match ui.get_quality() {
        1 => 0.5,
        2 => 0.25,
        _ => 1.0 };
    (
        ((width * scale).round() as u32).max(2),
        ((height * scale).round() as u32).max(2),
    )
}

fn viewer_pixels(view_w: f32, view_h: f32) -> (u32, u32) {
    let w = (view_w.round() as u32).clamp(2, 1920);
    let h = (view_h.round() as u32).clamp(2, 1440);
    (w.max(2), h.max(2))
}

fn preview_canvas(seq_w: u32, seq_h: u32, view_w: u32, view_h: u32) -> (u32, u32) {
    let scale = (view_w as f32 / seq_w.max(1) as f32).min(view_h as f32 / seq_h.max(1) as f32);
    let w = ((seq_w as f32 * scale).round() as u32).max(2) & !1;
    let h = ((seq_h as f32 * scale).round() as u32).max(2) & !1;
    (w, h)
}

fn source_project(media: &Media) -> Project {
    let mut project = Project::new_default("Source");
    project.sequence.width = media.width.max(16);
    project.sequence.height = media.height.max(16);
    project.sequence.frame_rate = media.source_rate();
    project.sequence.tracks.clear();
    let media_id = project.add_media(media.clone());
    for kind in [TrackKind::Video, TrackKind::Audio] {
        let included = match kind {
            TrackKind::Video => media.has_video || media.kind == MediaKind::Still,
            TrackKind::Audio => media.has_audio };
        if !included {
            continue;
        }
        let track = project.add_track(kind);
        let id = project.alloc();
        project.sequence.tracks[track].clips.push(Clip {
            id,
            media_id,
            name: media.name.clone(),
            timeline_start: 0,
            duration: media.source_frames(),
            source_in: 0,
            source_time_offset: 0.0,
            link: None,
            volume: 1.0,
            pos_x: 0.0,
            pos_y: 0.0,
            scale: 1.0,
            opacity: 1.0,
            grade: Grade::default() });
    }
    project
}

fn pump_source_preview(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let media = {
        let guard = state.borrow();
        guard
            .editor
            .selected_media
            .and_then(|id| guard.editor.project.media(id))
            .cloned()
    };
    let Some(media) = media else {
        let mut current = state.borrow_mut();
        current.source_request_pending = false;
        current.source_feedback.clear();
        if current.source.media_id.is_some() {
            if current.source.playing {
                current.audio.stop();
            }
            current.source.select(None, FrameRate::default(), 1);
            current.source_key = None;
            current.source_request_pending = false;
            current.source_presented = None;
            current.source_project = None;
            current.source_requested = None;
            current.source_usable = false;
            current.source_error = None;
            current
                .source_preview
                .set_project(Project::new_default("Source"));
            current.source_token = current.source_preview.request(0, 2, 2);
            sync_source_controls(ui, &current);
        }
        ui.set_show_source_picture(false);
        sync_viewer_status(ui, &current);
        return;
    };
    let rate = media.source_rate();
    let changed = state
        .borrow()
        .source_key
        .as_ref()
        .is_none_or(|(previous, previous_rate)| {
            !same_picture(previous, &media)
                || previous.has_audio != media.has_audio
                || *previous_rate != rate
        });
    if changed {
        let project = source_project(&media);
        let mut current = state.borrow_mut();
        if current.source.playing {
            current.audio.stop();
        }
        current.source.stop();
        current
            .source
            .select(Some(media.id), rate, media.source_frames());
        current.source_project = Some(project.clone());
        current.source_preview.set_project(project);
        current.source_key = Some((media.clone(), rate));
        current.source_request_pending = false;
        current.source_feedback.clear();
        current.source_presented = None;
        current.source_requested = None;
        current.source_min_token = current.source_token.saturating_add(1);
        current.source_error = None;
        // Probe once on media identity/metadata changes, rather than statting
        // paths every UI frame or while a long network source is playing.
        current.source_usable = media.path.is_file();
        if !current.source_usable {
            current.source_feedback.fail("Media is offline. Relink it in Project.".into());
        }
        ui.set_show_source_picture(false);
        sync_source_controls(ui, &current);
    }
    let picture = media.has_video || media.kind == MediaKind::Still;
    if !picture {
        let mut current = state.borrow_mut();
        current.source_request_pending = false;
        if current.source_usable { current.source_feedback.clear(); }
        ui.set_show_source_picture(false);
        sync_viewer_status(ui, &current);
        return;
    }
    let quality = match ui.get_quality() {
        1 => 0.5,
        2 => 0.25,
        _ => 1.0 } * if state.borrow().source.scrub_origin.is_some() {
        0.5
    } else {
        1.0
    };
    let (width, height) = viewer_pixels(
        ui.get_source_px_width() * quality,
        ui.get_source_px_height() * quality,
    );
    let (width, height) = preview_canvas(media.width.max(16), media.height.max(16), width, height);
    {
        let mut current = state.borrow_mut();
        let requested = (current.source.frame, width, height);
        let intent = preview_request_intent(current.source.playing, current.source.waiting_picture,
            current.source.waiting_audio, current.source.speed, current.source.scrub_origin.is_some());
        if current.source_usable && (current.source_requested != Some(requested) || current.source_intent != intent) {
            current.source_token = match intent {
                PreviewIntent::Playback => current.source_preview.request_playback(requested.0, width, height),
                PreviewIntent::Scrub => current.source_preview.request_scrub(requested.0, width, height),
                PreviewIntent::FinalSeek => current.source_preview.request(requested.0, width, height) };
            current.source_requested = Some(requested);
            current.source_request_pending = true;
            current.source_feedback.request(Instant::now());
            current.source_error = None;
            current.optimized_cache.set_busy(true);
            current.waveform_engine.set_busy(true);
        current.filmstrips.set_busy(true);
            playback::set_index_busy(true);
            current.source_intent = intent;
        }
    }
    let mut latest = None;
    while let Some(image) = state.borrow().source_preview.poll() {
        latest = Some(image);
    }
    if let Some(image) = latest {
        let mut current = state.borrow_mut();
        if current.source_usable
            && accepts_preview_reply(
                image.token,
                current.source_token,
                current.source_min_token,
                current.source.playing,
                current.source.scrub_origin.is_some(),
                image.error.is_some(),
            )
        {
            let is_latest = image.token == current.source_token;
            if is_latest { current.source_request_pending = false; }
            if let Some(error) = &image.error {
                current.source_request_pending = false;
                current.source_error = Some(error.clone());
                current.source_feedback.fail(error.clone());
                current.source_presented = None;
                current.status = format!("Source: {error}");
                if current.source.playing || current.source.waiting_audio {
                    current.audio.stop();
                }
                current.source.stop();
                // Retain the last valid picture and permit an explicit seek/play retry.
                current.source_intent = preview_request_intent(false, false, false, 0.0,
                    current.source.scrub_origin.is_some());
                sync_source_controls(ui, &current);
                ui.set_status_text(current.status.clone().into());
                sync_viewer_status(ui, &current);
                return;
            }
            let before_upload = Instant::now();
            ui.set_source_image(to_image(&image.image));
            ui.set_show_source_picture(true);
            current.source_feedback.picture(is_latest, Instant::now());
            current.source_error = None;
            current.preview_telemetry.record("Source", &image, before_upload);
            current.source_presented = (!image.proxy_used).then_some((media.id, image.frame));
            if current.source.waiting_picture && !image.proxy_used && image.frame == current.source.frame {
                current.source.picture_ready();
                start_source_audio(&mut current);
                ui.set_status_text(current.status.clone().into());
            }
        }
    }
    sync_viewer_status(ui, &state.borrow());
}

fn to_image(image: &ImageBuf) -> slint::Image {
    let mut buffer = SharedPixelBuffer::<Rgb8Pixel>::new(image.width.max(1), image.height.max(1));
    let dst = buffer.make_mut_bytes();
    if dst.len() == image.rgb.len() {
        dst.copy_from_slice(&image.rgb);
    }
    slint::Image::from_rgb8(buffer)
}

fn open_dialog(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, is_new: bool) {
    let (name, w, h, rate) = {
        let state = state.borrow();
        (
            state.editor.project.name.clone(),
            state.editor.project.sequence.width,
            state.editor.project.sequence.height,
            state.editor.project.sequence.frame_rate,
        )
    };
    ui.set_dialog_is_new(is_new);
    ui.set_dialog_title(SharedString::from(if is_new {
        "New project"
    } else {
        "Sequence settings"
    }));
    ui.set_dlg_name(SharedString::from(if is_new {
        "Untitled"
    } else {
        name.as_str()
    }));
    ui.set_dlg_width(SharedString::from(w.to_string()));
    ui.set_dlg_height(SharedString::from(h.to_string()));
    let index = frame_rate_presets()
        .iter()
        .position(|item| *item == rate)
        .unwrap_or(1) as i32;
    ui.set_dlg_fps(index);
    ui.set_dialog_error(SharedString::default());
    ui.set_show_dialog(true);
    ui.set_open_menu(0);
}

fn create_project(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let Some((width, height)) = read_dialog_dimensions(ui) else {
        return;
    };
    if !confirm_unsaved(ui, state) {
        return;
    }
    let name = ui.get_dlg_name().to_string();
    let rate = rate_from_index(ui.get_dlg_fps());
    let profile = state.borrow().editor.project.shortcut_profile;
    stop_play(state);
    let mut project = Project::new_default(if name.trim().is_empty() {
        "Untitled"
    } else {
        name.trim()
    });
    project.set_resolution(width, height);
    project.set_frame_rate(rate);
    project.shortcut_profile = profile;
    let mut state_mut = state.borrow_mut();
    state_mut.import_rx = None;
    reset_media_workers(&mut state_mut);
    state_mut.editor = timeline::Editor::from_project(project);
    state_mut.editor.project.shortcut_profile = profile;
    state_mut.status = "New project".into();
    state_mut.source_key = None;
    state_mut.edit_gen = state_mut.edit_gen.wrapping_add(1);
    state_mut.picture_gen = state_mut.picture_gen.wrapping_add(1);
    drop(state_mut);
    ui.set_show_dialog(false);
    set_preview_default(ui, state);
    full_sync(ui, state);
}

fn apply_sequence(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let Some((width, height)) = read_dialog_dimensions(ui) else {
        return;
    };
    let rate = rate_from_index(ui.get_dlg_fps());
    {
        let mut state = state.borrow_mut();
        let _ = state.editor.edit("sequence", move |project| {
            project.set_resolution(width, height);
            project.set_frame_rate(rate);
            Ok(())
        });
        state.status = "Sequence settings updated".into();
        state.edit_gen = state.edit_gen.wrapping_add(1);
        state.picture_gen = state.picture_gen.wrapping_add(1);
    }
    ui.set_show_dialog(false);
    set_preview_default(ui, state);
    full_sync(ui, state);
}

fn parse_dimensions(width: &str, height: &str) -> Result<(u32, u32), String> {
    let parse = |text: &str, name: &str| {
        text.trim()
            .parse::<u32>()
            .ok()
            .filter(|value| (16..=7680).contains(value))
            .ok_or_else(|| format!("{name} must be a whole number from 16 to 7680 pixels."))
    };
    Ok((parse(width, "Width")?, parse(height, "Height")?))
}

fn parse_export_dimensions(width: &str, height: &str) -> Result<(u32, u32), String> {
    let (width, height) = parse_dimensions(width, height)?;
    if width % 2 != 0 || height % 2 != 0 {
        return Err("H.264 export requires an even width and height. Adjust the dimensions before rendering.".into());
    }
    Ok((width, height))
}

fn read_dialog_dimensions(ui: &AppWindow) -> Option<(u32, u32)> {
    match parse_dimensions(&ui.get_dlg_width(), &ui.get_dlg_height()) {
        Ok(dimensions) => {
            ui.set_dialog_error(SharedString::default());
            Some(dimensions)
        }
        Err(error) => {
            ui.set_dialog_error(error.into());
            None
        }
    }
}

fn rate_from_index(index: i32) -> FrameRate {
    frame_rate_presets()
        .get(index as usize)
        .copied()
        .unwrap_or(FrameRate::rational(24, 1))
}

fn open_project(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let Some(path) = rfd::FileDialog::new()
        .set_title("Open Project")
        .add_filter("Quaternio", &["quaternio"])
        .pick_file()
    else {
        return;
    };
    match Project::load_from(&path) {
        Ok(project) => {
            if !confirm_unsaved(ui, state) {
                return;
            }
            stop_play(state);
            let mut state_mut = state.borrow_mut();
            state_mut.import_rx = None;
            reset_media_workers(&mut state_mut);
            state_mut.editor = timeline::Editor::from_project(project);
            state_mut.editor.path = Some(path);
            state_mut.editor.dirty = false;
            state_mut.status = "Opened project".into();
            state_mut.edit_gen = state_mut.edit_gen.wrapping_add(1);
            state_mut.picture_gen = state_mut.picture_gen.wrapping_add(1);
            drop(state_mut);
            set_preview_default(ui, state);
            let profile = state.borrow().editor.project.shortcut_profile;
            state.borrow_mut().preferences.profile = if profile == ShortcutProfile::Resolve {
                "resolve".into()
            } else {
                "premiere".into()
            };
            save_preferences(state);
            full_sync(ui, state);
        }
        Err(err) => {
            set_status(state, err);
            full_sync(ui, state);
        }
    }
}

fn confirm_unsaved(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) -> bool {
    if !state.borrow().editor.dirty {
        return true;
    }
    let name = state.borrow().editor.project.name.clone();
    match rfd::MessageDialog::new()
        .set_title("Unsaved changes")
        .set_description(format!("Save changes to {name} before continuing?"))
        .set_buttons(rfd::MessageButtons::YesNoCancel)
        .show()
    {
        rfd::MessageDialogResult::Yes => try_save_project(ui, state, false),
        rfd::MessageDialogResult::No => true,
        _ => false }
}

fn save_project(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, save_as: bool) {
    let _ = try_save_project(ui, state, save_as);
}

fn try_save_project(
    ui: &AppWindow,
    state: &Rc<std::cell::RefCell<AppState>>,
    save_as: bool,
) -> bool {
    let existing = state.borrow().editor.path.clone();
    let path = if save_as || existing.is_none() {
        let name = format!("{}.quaternio", state.borrow().editor.project.name);
        let Some(path) = rfd::FileDialog::new()
            .set_title("Save Project")
            .set_file_name(&name)
            .add_filter("Quaternio", &["quaternio"])
            .save_file()
        else {
            return false;
        };
        ensure_extension(path, "quaternio")
    } else {
        existing.unwrap()
    };
    let result = {
        let state = state.borrow();
        state.editor.project.save_to(&path)
    };
    let saved = result.is_ok();
    match result {
        Ok(()) => {
            let mut state = state.borrow_mut();
            state.editor.path = Some(path);
            state.editor.mark_saved();
            state.status = "Saved".into();
        }
        Err(err) => set_status(state, err) }
    full_sync(ui, state);
    saved
}

fn ensure_extension(path: PathBuf, ext: &str) -> PathBuf {
    if path.extension().and_then(|ext| ext.to_str()) == Some(ext) {
        path
    } else {
        let mut name = path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        name.push(".");
        name.push(ext);
        path.with_file_name(name)
    }
}

fn relink_media(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    if state.borrow().import_rx.is_some() {
        set_status(
            state,
            "Wait for media import to finish before relinking".into(),
        );
        full_sync(ui, state);
        return;
    }
    let Some(id) = state.borrow().editor.selected_media else {
        return;
    };
    let Some(path) = rfd::FileDialog::new()
        .set_title("Relink selected media")
        .pick_file()
    else {
        return;
    };
    let (tx, rx) = mpsc::sync_channel(2);
    {
        let mut current = state.borrow_mut();
        current.import_rx = Some(rx);
        current.import_count = 0;
        current.import_errors = 0;
        current.relinking = true;
        current.status = "Reading replacement media…".into();
    }
    full_sync(ui, state);
    std::thread::spawn(move || {
        let event = match probe_path(&path) {
            Ok(media) => ImportEvent::Relink(id, media),
            Err(error) => ImportEvent::Error(error) };
        if tx.send(event).is_ok() {
            let _ = tx.send(ImportEvent::Done);
        }
    });
}

fn import_media(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let Some(paths) = rfd::FileDialog::new()
        .set_title("Import Media")
        .add_filter(
            "Media",
            &[
                "mp4", "mov", "mkv", "avi", "webm", "wav", "mp3", "aac", "flac", "aiff", "png",
                "jpg", "jpeg", "tif", "tiff", "bmp", "webp", "gif",
            ],
        )
        .pick_files()
    else {
        return;
    };
    import_paths(ui, state, paths);
}

fn sync_update_ui(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let current = state.borrow();
    ui.set_app_version(SharedString::from(crate::update::CURRENT_VERSION));
    ui.set_update_available(current.update_available);
    ui.set_update_tooltip(SharedString::from(current.update_tooltip.clone()));
    ui.set_update_agree_title(SharedString::from(current.update_agree_title.clone()));
    ui.set_update_agree_body(SharedString::from(current.update_agree_body.clone()));
    ui.set_versions_status(SharedString::from(current.versions_status.clone()));
    ui.set_versions_busy(current.versions_busy);
    ui.set_version_rows(model(current.version_rows.clone()));
}

fn clear_update_agree(state: &Rc<std::cell::RefCell<AppState>>) {
    let mut current = state.borrow_mut();
    current.update_agree_tag = None;
    current.update_agree_url = None;
    current.update_agree_title.clear();
    current.update_agree_body.clear();
}

fn open_update_agree_for_latest(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let tag = crate::update::list_releases(false)
        .ok()
        .and_then(|list| crate::update::latest_update(&list).map(|r| r.tag_name.clone()));
    if let Some(tag) = tag {
        open_update_agree_for_tag(ui, state, &tag);
    } else {
        set_status(state, "No newer release found".into());
        full_sync(ui, state);
    }
}

fn open_update_agree_for_tag(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, tag: &str) {
    let release = match crate::update::list_releases(false)
        .ok()
        .and_then(|list| crate::update::find_release(&list, tag).cloned())
    {
        Some(release) => release,
        None => {
            set_status(state, format!("Release {tag} not found"));
            full_sync(ui, state);
            return;
        }
    };
    let current = crate::update::CURRENT_VERSION;
    let target = release.display_tag().to_string();
    let action = if release.matches_current() {
        "Reinstall"
    } else if release.is_newer_than_current() {
        "Update"
    } else {
        "Switch"
    };
    {
        let mut current_state = state.borrow_mut();
        current_state.update_agree_tag = Some(release.tag_name.clone());
        current_state.update_agree_url = Some(release.html_url.clone());
        current_state.update_agree_title = format!("{action} to {target}?");
        current_state.update_agree_body = format!(
            "Running {current}. Choose automatic replace for this install, or open the GitHub download page."
        );
    }
    sync_update_ui(ui, state);
    ui.set_show_update_agree(true);
}

fn apply_release_tag(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, tag: &str) {
    set_status(state, format!("Downloading {tag}…"));
    full_sync(ui, state);
    let release = match crate::update::list_releases(false)
        .ok()
        .and_then(|list| crate::update::find_release(&list, tag).cloned())
    {
        Some(release) => release,
        None => {
            set_status(state, format!("Release {tag} not found"));
            full_sync(ui, state);
            return;
        }
    };
    if crate::update::pick_asset(&release).is_none() {
        set_status(
            state,
            format!("No portable build for this platform in {tag}; open the download page instead"),
        );
        full_sync(ui, state);
        return;
    }
    match crate::update::download_and_apply(&release) {
        Ok(()) => {}
        Err(err) => {
            set_status(state, err);
            full_sync(ui, state);
        }
    }
}

fn refresh_versions(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, force: bool) {
    state.borrow_mut().versions_busy = true;
    state.borrow_mut().versions_status = if force {
        "Refreshing releases…".into()
    } else {
        "Loading releases…".into()
    };
    sync_update_ui(ui, state);
    match crate::update::list_releases(force) {
        Ok(releases) => {
            let rows: Vec<VersionRow> = releases
                .iter()
                .map(|release| VersionRow {
                    tag: SharedString::from(release.tag_name.as_str()),
                    date: SharedString::from(release.published_label()),
                    prerelease: release.prerelease,
                    current: release.matches_current(),
                    html_url: SharedString::from(release.html_url.as_str()),
                })
                .collect();
            let latest = crate::update::latest_update(&releases).cloned();
            let mut current = state.borrow_mut();
            current.version_rows = rows;
            current.versions_busy = false;
            current.versions_status = if current.version_rows.is_empty() {
                "No published releases yet.".into()
            } else {
                format!("{} releases", current.version_rows.len())
            };
            if let Some(latest) = latest {
                current.update_available = true;
                current.update_tooltip = format!(
                    "Update available: {} → {}",
                    crate::update::CURRENT_VERSION,
                    latest.display_tag()
                );
            } else {
                current.update_available = false;
                current.update_tooltip.clear();
            }
        }
        Err(err) => {
            {
                let mut current = state.borrow_mut();
                current.versions_busy = false;
                current.versions_status = err.clone();
            }
            set_status(state, err);
        }
    }
    full_sync(ui, state);
}

fn start_background_update_check(ui: &AppWindow, _state: &Rc<std::cell::RefCell<AppState>>) {
    // Slint Weak is Send; AppState lives in a thread-local (same as preview wakes).
    let weak_ui = Arc::new(std::sync::Mutex::new(ui.as_weak()));
    std::thread::spawn(move || {
        let result = crate::update::list_releases(false);
        let weak_ui = weak_ui.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak_ui.upgrade() else {
                return;
            };
            PREVIEW_UI_STATE.with(|slot| {
                let Some(state) = slot.borrow().upgrade() else {
                    return;
                };
                match result {
                    Ok(releases) => {
                        let rows: Vec<VersionRow> = releases
                            .iter()
                            .map(|release| VersionRow {
                                tag: SharedString::from(release.tag_name.as_str()),
                                date: SharedString::from(release.published_label()),
                                prerelease: release.prerelease,
                                current: release.matches_current(),
                                html_url: SharedString::from(release.html_url.as_str()),
                            })
                            .collect();
                        let latest = crate::update::latest_update(&releases).cloned();
                        {
                            let mut current = state.borrow_mut();
                            current.version_rows = rows;
                            if let Some(latest) = latest {
                                current.update_available = true;
                                current.update_tooltip = format!(
                                    "Update available: {} → {}",
                                    crate::update::CURRENT_VERSION,
                                    latest.display_tag()
                                );
                            }
                        }
                        sync_update_ui(&ui, &state);
                    }
                    Err(_) => {
                        // Quiet on startup — Versions tab can surface errors on Refresh.
                    }
                }
            });
        });
    });
}

fn begin_timeline_import(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let mut dialog = rfd::FileDialog::new().set_title("Import Premiere / Resolve timeline");
    dialog = dialog
        .add_filter("Premiere / Resolve / FCPXML", &["prproj", "drp", "fcpxml", "xml"])
        .add_filter("Premiere project", &["prproj"])
        .add_filter("DaVinci Resolve project", &["drp"])
        .add_filter("FCPXML / XML", &["fcpxml", "xml"]);
    let Some(path) = dialog.pick_file() else {
        return;
    };
    state.borrow_mut().timeline_import_path = Some(path);
    ui.set_show_import_mode(true);
    ui.set_open_menu(0);
}

fn finish_timeline_import(
    ui: &AppWindow,
    state: &Rc<std::cell::RefCell<AppState>>,
    mode: crate::import::ImportMode,
) {
    ui.set_show_import_mode(false);
    let Some(path) = state.borrow_mut().timeline_import_path.take() else {
        set_status(state, "No timeline file selected".into());
        full_sync(ui, state);
        return;
    };
    let imported = crate::import::import_timeline(&path);
    match imported {
        Ok((project, mut report)) => {
            match mode {
                crate::import::ImportMode::NewProject => {
                    if !confirm_unsaved(ui, state) {
                        return;
                    }
                    stop_play(state);
                    let mut current = state.borrow_mut();
                    current.import_rx = None;
                    reset_media_workers(&mut current);
                    current.editor = timeline::Editor::from_project(project);
                    current.editor.path = None;
                    current.editor.dirty = true;
                    current.edit_gen = current.edit_gen.wrapping_add(1);
                    current.picture_gen = current.picture_gen.wrapping_add(1);
                    current.status = format!("Imported timeline ({})", report.summary_line());
                    current.import_report_text = report.detail_text();
                }
                crate::import::ImportMode::IntoCurrent => {
                    stop_play(state);
                    let playhead = state.borrow().editor.project.sequence.playhead;
                    let result = {
                        let mut current = state.borrow_mut();
                        crate::import::merge_into_current(
                            &mut current.editor.project,
                            project,
                            playhead,
                            &mut report,
                        )
                    };
                    match result {
                        Ok(()) => {
                            let mut current = state.borrow_mut();
                            current.editor.dirty = true;
                            current.edit_gen = current.edit_gen.wrapping_add(1);
                            current.picture_gen = current.picture_gen.wrapping_add(1);
                            current.status =
                                format!("Merged timeline at playhead ({})", report.summary_line());
                            current.import_report_text = report.detail_text();
                        }
                        Err(error) => {
                            set_status(state, error);
                            full_sync(ui, state);
                            return;
                        }
                    }
                }
            }
            let detail = state.borrow().import_report_text.clone();
            ui.set_import_report_text(detail.into());
            ui.set_show_import_report(true);
            set_preview_default(ui, state);
            full_sync(ui, state);
        }
        Err(error) => {
            set_status(state, error);
            full_sync(ui, state);
        }
    }
}

fn handle_dropped_files(
    ui: &AppWindow,
    state: &Rc<std::cell::RefCell<AppState>>,
    paths: Vec<PathBuf>,
) {
    ui.set_file_drop_hover(false);
    if paths.is_empty() {
        return;
    }
    let mut media = Vec::new();
    let mut timelines = Vec::new();
    for path in paths {
        if crate::import::path_looks_like_timeline(&path) {
            timelines.push(path);
        } else {
            media.push(path);
        }
    }
    if !media.is_empty() {
        import_paths(ui, state, media);
    }
    if let Some(path) = timelines.first().cloned() {
        let extra = timelines.len().saturating_sub(1);
        if extra > 0 {
            set_status(
                state,
                format!("Importing first timeline; {extra} additional timeline files skipped"),
            );
        }
        state.borrow_mut().timeline_import_path = Some(path);
        ui.set_show_import_mode(true);
        ui.set_open_menu(0);
    }
}

fn import_paths(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    if state.borrow().import_rx.is_some() {
        let mut current = state.borrow_mut();
        let mut skipped = 0;
        for path in paths {
            if current.pending_imports.contains(&path) {
                continue;
            }
            if current.pending_imports.len() >= 1024 {
                skipped += 1;
            } else {
                current.pending_imports.push_back(path);
            }
        }
        current.status = if skipped == 0 {
            format!("Importing; {} files queued", current.pending_imports.len())
        } else {
            format!(
                "Importing; {} files queued, {skipped} skipped at queue limit",
                current.pending_imports.len()
            )
        };
        ui.set_status_text(current.status.clone().into());
        return;
    }
    let (tx, rx) = mpsc::sync_channel(16);
    {
        let mut current = state.borrow_mut();
        current.import_rx = Some(rx);
        current.import_count = 0;
        current.import_errors = 0;
        current.relinking = false;
        current.status = format!("Importing {} files…", paths.len());
    }
    full_sync(ui, state);
    let import_bin = state.borrow().current_bin.clone();
    std::thread::spawn(move || {
        for path in paths {
            let event = match probe_path(&path) {
                Ok(mut media) => {
                    media.bin = import_bin.clone();
                    ImportEvent::Media(media)
                }
                Err(error) => ImportEvent::Error(error) };
            if tx.send(event).is_err() {
                return;
            }
        }
        let _ = tx.send(ImportEvent::Done);
    });
}

fn add_imported_media(editor: &mut timeline::Editor, media: Vec<Media>) -> Result<usize, String> {
    if media.is_empty() {
        return Ok(0);
    }
    let count = media.len();
    let selected = editor.edit("import-media", |project| {
        let mut selected = None;
        for mut asset in media {
            if !project.bins.contains(&asset.bin) {
                asset.bin.clear();
            }
            selected = Some(project.add_media(asset));
        }
        Ok(selected)
    })?;
    editor.selected_media = selected;
    Ok(count)
}

fn flush_import_batch(current: &mut AppState, media: &mut Vec<Media>) {
    if media.is_empty() {
        return;
    }
    let count = media.len();
    match add_imported_media(&mut current.editor, std::mem::take(media)) {
        Ok(imported) => {
            current.import_count += imported;
            current.status = format!("Imported {} files…", current.import_count);
            note_borrowed(current);
        }
        Err(error) => {
            current.import_errors += count;
            current.status = error;
        }
    }
}

fn pump_import(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let mut changed = false;
    // One history snapshot for adjacent results available on this UI turn.
    // Later arrivals stay separate so intervening user edits keep their order.
    let mut media_batch = Vec::new();
    for _ in 0..16 {
        let event = state
            .borrow()
            .import_rx
            .as_ref()
            .and_then(|rx| rx.try_recv().ok());
        let Some(event) = event else {
            break;
        };
        let mut current = state.borrow_mut();
        match event {
            ImportEvent::Media(media) => media_batch.push(media),
            ImportEvent::Relink(id, media) => {
                flush_import_batch(&mut current, &mut media_batch);
                match current
                    .editor
                    .edit("relink-media", |project| project.relink_media(id, media))
                {
                    Ok(()) => {
                        current.import_count += 1;
                        current.source_key = None;
                        // Relinking may replace bytes at the same path with
                        // unchanged probe metadata. Revalidate worker stamps.
                        current.filmstrips.reset();
                        note_borrowed(&mut current);
                    }
                    Err(error) => {
                        current.import_errors += 1;
                        current.status = error;
                    }
                }
            }
            ImportEvent::Error(error) => {
                flush_import_batch(&mut current, &mut media_batch);
                current.import_errors += 1;
                current.status = error;
            }
            ImportEvent::Done => {
                flush_import_batch(&mut current, &mut media_batch);
                current.status = if current.relinking && current.import_errors == 0 {
                    "Media relinked".into()
                } else if current.relinking {
                    current.status.clone()
                } else if current.import_errors == 0 {
                    format!("Imported {} files", current.import_count)
                } else {
                    format!(
                        "Imported {} files; {} could not be read",
                        current.import_count, current.import_errors
                    )
                };
                current.import_rx = None;
                current.relinking = false;
            }
        }
        changed = true;
    }
    flush_import_batch(&mut state.borrow_mut(), &mut media_batch);
    if changed {
        full_sync(ui, state);
    }
    let queued = {
        let mut current = state.borrow_mut();
        if current.import_rx.is_none() {
            current.pending_imports.drain(..).collect::<Vec<_>>()
        } else {
            Vec::new()
        }
    };
    if !queued.is_empty() {
        import_paths(ui, state, queued);
    }
}

fn browse_export(ui: &AppWindow) {
    let name = if ui.get_export_path().is_empty() {
        "sequence.mp4".into()
    } else {
        ui.get_export_path().to_string()
    };
    let Some(path) = rfd::FileDialog::new()
        .set_title("Export MP4")
        .set_file_name(&name)
        .add_filter("MP4", &["mp4"])
        .save_file()
    else {
        return;
    };
    let path = ensure_extension(path, "mp4");
    ui.set_export_path(SharedString::from(path.display().to_string()));
}

fn prepare_export(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    export_form::prepare(ui, state);
}

fn set_preview_default(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let quality = {
        let current = state.borrow();
        if u64::from(current.editor.project.sequence.width)
            * u64::from(current.editor.project.sequence.height)
            >= 3840 * 2160
        {
            1
        } else {
            0
        }
    };
    ui.set_quality(quality);
    state.borrow_mut().preview_quality = quality;
    state.borrow_mut().shown_frame = -1;
}

fn sync_queue(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let current = state.borrow();
    ui.set_queue_items(model(
        current
            .export_jobs
            .iter()
            .map(|job| QueueItem {
                id: job.id,
                name: SharedString::from(job.project.name.clone()),
                output: SharedString::from(job.settings.output.display().to_string()),
                status: SharedString::from(job.message.clone()),
                removable: job.status != JobStatus::Rendering,
                retryable: matches!(job.status, JobStatus::Failed | JobStatus::Cancelled),
                complete: job.status == JobStatus::Complete })
            .collect(),
    ));
    ui.set_queue_pending(
        current
            .export_jobs
            .iter()
            .any(|job| job.status == JobStatus::Pending),
    );
    ui.set_queue_has_finished(current.export_jobs.iter().any(|job| {
        matches!(
            job.status,
            JobStatus::Complete | JobStatus::Failed | JobStatus::Cancelled
        )
    }));
}

fn enqueue_export(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) -> bool {
    if ui.get_export_path().trim().is_empty() {
        ui.set_export_message(SharedString::from("Choose an output file"));
        return false;
    }
    let (width, height) = match parse_export_dimensions(
        &ui.get_export_width(),
        &ui.get_export_height(),
    ) {
        Ok(dimensions) => dimensions,
        Err(error) => {
            set_status(state, error.clone());
            ui.set_export_message(error.into());
            full_sync(ui, state);
            return false;
        }
    };
    let frame_rate = match export_form::rate(ui) {
        Ok(rate) => rate,
        Err(error) => { ui.set_export_message(error.into()); return false; }
    };
    export_form::remember(ui, state);
    save_preferences(state);
    let settings = ExportSettings {
        force_composite: false,
        speed: match ui.get_export_speed() {
            1 => RenderSpeed::Fast,
            2 => RenderSpeed::Compact,
            _ => RenderSpeed::Balanced },
        encoder: match ui.get_export_encoder() {
            1 => EncoderPreference::Cpu,
            2 => EncoderPreference::Hardware,
            _ => EncoderPreference::Auto },
        width,
        height,
        frame_rate,
        crf: ui.get_export_crf().round().clamp(14.0, 28.0) as u8,
        use_in_out: ui.get_export_inout(),
        output: ensure_extension(PathBuf::from(ui.get_export_path().as_str()), "mp4") };
    let project = state.borrow().editor.project.clone();
    if let Err(error) = export_range(&project, &settings) {
        set_status(state, error.clone());
        ui.set_export_message(SharedString::from(error));
        full_sync(ui, state);
        return false;
    }
    {
        let mut current = state.borrow_mut();
        if current.export_jobs.iter().any(|job| {
            matches!(job.status, JobStatus::Pending | JobStatus::Rendering)
                && job.settings.output == settings.output
        }) {
            current.status =
                "An active job already uses this output file. Choose a different name.".into();
            ui.set_export_message(SharedString::from(current.status.clone()));
            return false;
        }
        let id = current.next_export_id;
        current.next_export_id += 1;
        current.export_jobs.push(ExportJob {
            id,
            project,
            settings,
            status: JobStatus::Pending,
            message: "Queued".into() });
        current.status = "Export added to queue".into();
    }
    full_sync(ui, state);
    true
}

fn start_export(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    if state.borrow().export_running {
        return;
    }
    if enqueue_export(ui, state) {
        state.borrow_mut().queue_active = true;
        launch_next_export(ui, state);
    }
}

fn launch_next_export(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let job = {
        let mut current = state.borrow_mut();
        if current.export_running || !current.queue_active {
            return;
        }
        let Some(job) = current
            .export_jobs
            .iter_mut()
            .find(|job| job.status == JobStatus::Pending)
        else {
            current.queue_active = false;
            return;
        };
        job.status = JobStatus::Rendering;
        job.message = "Starting".into();
        (job.id, job.project.clone(), job.settings.clone())
    };
    let (id, project, settings) = job;
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    {
        let mut current = state.borrow_mut();
        current.export_cancel = cancel.clone();
        current.export_rx = Some(rx);
        current.export_running = true;
        current.export_job_id = Some(id);
        current.status = format!("Rendering {}", project.name);
    }
    ui.set_export_busy(true);
    ui.set_export_cancelling(false);
    state.borrow_mut().export_eta_at = None;
    ui.set_export_eta("".into());
    ui.set_export_progress(0.0);
    ui.set_export_preparing(false);
    ui.set_export_preparation_progress(0.0);
    ui.set_export_message(SharedString::from("Starting"));
    sync_queue(ui, state);
    let worker = std::thread::spawn(move || {
        if let Err(err) = export_mp4(&project, &settings, &cancel, tx.clone()) {
            let _ = tx.send(ExportProgress {
                preparation: None,
                remaining_seconds: None,
                fraction: 0.0,
                message: err.clone(),
                done: true,
                error: Some(err) });
        }
    });
    state.borrow_mut().export_workers.push(worker);
}

fn set_profile(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, profile: ShortcutProfile) {
    let conflict = {
        let current = state.borrow();
        let mut seen = std::collections::BTreeMap::new();
        ACTIONS.iter().find_map(|action| {
            let binding = current.preferences.binding(action, profile);
            if binding.is_empty() { return None; }
            seen.insert(binding, action.label).map(|previous| format!("Profile switch would share {} between {previous} and {}. Reassign or reset custom shortcuts first.", commands::display_binding(binding), action.label))
        })
    };
    if let Some(message) = conflict {
        ui.set_binding_message(message.clone().into());
        set_status(state, message);
        return;
    }
    state.borrow_mut().editor.set_profile(profile);
    state.borrow_mut().preferences.profile = if profile == ShortcutProfile::Resolve {
        "resolve".into()
    } else {
        "premiere".into()
    };
    save_preferences(state);
    set_status(
        state,
        format!(
            "Shortcut profile: {}",
            match profile {
                ShortcutProfile::Premiere => "Premiere",
                ShortcutProfile::Resolve => "Resolve" }
        ),
    );
    full_sync(ui, state);
}

fn binding_hint(state: &Rc<std::cell::RefCell<AppState>>, id: &str) -> SharedString {
    let current = state.borrow();
    let Some(action) = ACTIONS.iter().find(|action| action.id == id) else {
        return SharedString::default();
    };
    let binding = current
        .preferences
        .binding(action, current.editor.project.shortcut_profile);
    commands::display_binding(binding).into()
}

fn reset_media_workers(state: &mut AppState) {
    state.media_selection = media_selection::MediaSelection::default();
    state.program_feedback.clear();
    state.source_feedback.clear();
    state.program_request_pending = false;
    state.program_picture_reset = true;
    state.presented_picture = None;
    state.filmstrips.reset();
    state.razor_hover = None;
    state.pending_insert = None;
    state.sequence_match_offered = false;
    state.pending_imports.clear();
    state.thumbnails = ThumbnailEngine::new();
    state.thumbnail_pending.clear();
    state.thumbnail_cache.clear();
    state.thumbnail_schedule_key = None;
    state.waveform_engine = playback::timeline_waveforms::WaveformEngine::new();
    state.waveform_results.clear();
    state.waveform_view_key = None;
    state.waveform_scroll_y = 0.0;
    state.source_key = None;
    state.source_request_pending = false;
    state.source_project = None;
    state.source_requested = None;
    state.source_usable = false;
    state.source_error = None;
    state.source = SourceTransport::default();
    state
        .source_preview
        .set_project(Project::new_default("Source"));
    state.source_token = state.source_preview.request(0, 2, 2);
    state.media_drag_origin = None;
    state.current_bin.clear();
}

