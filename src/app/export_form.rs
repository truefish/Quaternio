use super::*;
use crate::preferences::ExportFormat;

#[derive(Default)]
pub(super) struct State {
    initialized_destination: bool,
    default_destination: Option<(Option<PathBuf>, String, PathBuf)>,
    pending: Option<Pending>,
    cancelled_check: Option<Receiver<Result<Option<export_destination::ExistingOutput>, String>>>,
    frozen: Option<Fields>,
    approvals: std::collections::HashMap<i32, Option<export_destination::ExistingOutput>>,
    pub(super) result: Option<(PathBuf, JobStatus)>,
    last_completed: Option<PathBuf>,
    open_results: Vec<Receiver<Result<(), String>>>,
}

struct Fields {
    filename: String, folder: String, path: String, width: String, height: String,
    fps: i32, custom_rate: String, crf: f32, speed: i32, encoder: i32, inout: bool,
}

impl Fields {
    fn capture(ui: &AppWindow) -> Self {
        Self { filename: ui.get_export_filename().to_string(), folder: ui.get_export_folder().to_string(),
            path: ui.get_export_path().to_string(), width: ui.get_export_width().to_string(), height: ui.get_export_height().to_string(),
            fps: ui.get_export_fps(), custom_rate: ui.get_export_custom_rate().to_string(), crf: ui.get_export_crf(),
            speed: ui.get_export_speed(), encoder: ui.get_export_encoder(), inout: ui.get_export_inout() }
    }

    fn restore(&self, ui: &AppWindow) {
        ui.set_export_filename(self.filename.clone().into());
        ui.set_export_folder(self.folder.clone().into());
        ui.set_export_path(self.path.clone().into());
        ui.set_export_width(self.width.clone().into());
        ui.set_export_height(self.height.clone().into());
        ui.set_export_fps(self.fps);
        ui.set_export_custom_rate(self.custom_rate.clone().into());
        ui.set_export_crf(self.crf);
        ui.set_export_speed(self.speed);
        ui.set_export_encoder(self.encoder);
        ui.set_export_inout(self.inout);
        ui.set_read_crf(format!("{}", self.crf.round()).into());
    }
}

#[derive(Clone, Copy)]
enum Submit { Add { start: bool }, Launch(i32), Retry(i32) }

struct Pending {
    project: Project,
    settings: ExportSettings,
    action: Submit,
    result: Receiver<Result<Option<export_destination::ExistingOutput>, String>>,
}

pub(super) fn busy(state: &AppState) -> bool {
    state.export_running || state.export_form.pending.is_some()
}

pub(super) fn guard(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) -> bool {
    let current = state.borrow();
    if !busy(&current) { return false; }
    if let Some(fields) = &current.export_form.frozen { fields.restore(ui); }
    ui.set_export_ready(false);
    true
}

pub(super) fn error(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, message: String) {
    ui.set_export_form_error(message.clone().into());
    ui.set_export_message(message.clone().into());
    ui.set_status_text(message.clone().into());
    set_status(state, message);
}

fn range(project: &Project, inout: bool) -> Result<(i64, i64), String> {
    let seq = &project.sequence;
    let duration = seq.duration_frames();
    if duration <= 0 { return Err("Add clips to the sequence before exporting.".into()); }
    if seq.frame_rate.num == 0 || seq.frame_rate.den == 0 {
        return Err("The sequence frame rate is invalid. Check Sequence settings.".into());
    }
    if !inout { return Ok((0, duration)); }
    let (Some(start), Some(end)) = (seq.in_point, seq.out_point) else {
        return Err("Set both In and Out marks in the timeline, or choose Entire sequence.".into());
    };
    if start < 0 || end <= start { return Err("Set the Out mark after the In mark.".into()); }
    if end > duration { return Err("The In/Out range extends past the sequence. Move the Out mark or choose Entire sequence.".into()); }
    Ok((start, end))
}

fn range_summary(ui: &AppWindow, project: &Project) {
    ui.set_export_range_available(range(project, true).is_ok());
    let label = match range(project, ui.get_export_inout()) {
        Ok((start, end)) => {
            let rate = project.sequence.frame_rate;
            let duration = format_timecode(end - start, rate);
            if ui.get_export_inout() {
                format!("In/Out · {duration} ({:.3} s) · {} to {} (Out excluded)",
                    rate.frame_to_seconds(end - start), format_timecode(start, rate), format_timecode(end, rate))
            } else { format!("Entire sequence · {duration} ({:.3} s)", rate.frame_to_seconds(end - start)) }
        },
        Err(message) => message,
    };
    ui.set_export_range_label(label.into());
}

pub(super) fn settings(ui: &AppWindow, project: &Project) -> Result<ExportSettings, String> {
    let output = export_destination::resolve(&ui.get_export_folder(), &ui.get_export_filename())?;
    let (width, height) = parse_export_dimensions(&ui.get_export_width(), &ui.get_export_height())?;
    let frame_rate = rate(ui)?;
    if !ui.get_export_crf().is_finite() || !(14.0..=28.0).contains(&ui.get_export_crf()) {
        return Err("Choose a quality from 14 to 28.".into());
    }
    let speed = match ui.get_export_speed() {
        0 => RenderSpeed::Balanced, 1 => RenderSpeed::Fast, 2 => RenderSpeed::Compact,
        _ => return Err("Choose an export speed.".into()),
    };
    let encoder = match ui.get_export_encoder() {
        0 => EncoderPreference::Auto, 1 => EncoderPreference::Cpu, 2 => EncoderPreference::Hardware,
        _ => return Err("Choose an encoding method.".into()),
    };
    range(project, ui.get_export_inout())?;
    Ok(ExportSettings { output, width, height, frame_rate, crf: ui.get_export_crf().round() as u8,
        speed, encoder, use_in_out: ui.get_export_inout(), force_composite: false })
}

pub(super) fn changed(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    if guard(ui, state) { return; }
    let mut current = state.borrow_mut();
    range_summary(ui, &current.editor.project);
    // Editing never touches the filesystem, including when a UNC folder is entered.
    let output = export_destination::resolve(&ui.get_export_folder(), &ui.get_export_filename());
    if current.export_form.default_destination.as_ref().is_some_and(|(_, _, path)| output.as_ref().ok() != Some(path)) {
        current.export_form.default_destination = None;
    }
    ui.set_export_path(output.as_ref().map(|path| path.display().to_string()).unwrap_or_default().into());
    let validation = settings(ui, &current.editor.project).and_then(|settings| {
        if current.export_jobs.iter().any(|job| matches!(job.status, JobStatus::Pending | JobStatus::Rendering)
            && export_destination::same_lexical_path(&job.settings.output, &settings.output)) {
            return Err("A queued or running export already uses this file. Choose a different filename or folder.".into());
        }
        if current.editor.project.media.iter().any(|media| export_destination::same_lexical_path(&media.path, &settings.output))
            || current.editor.path.as_ref().is_some_and(|path| export_destination::same_lexical_path(path, &settings.output)) {
            return Err("This output would overwrite project or source media. Choose a different filename or folder.".into());
        }
        Ok(())
    });
    ui.set_export_ready(validation.is_ok());
    ui.set_export_form_error(validation.err().unwrap_or_default().into());
    ui.set_read_crf(format!("{}", ui.get_export_crf().round() as i32).into());
}

pub(super) fn set_path(ui: &AppWindow, path: &std::path::Path) {
    ui.set_export_filename(path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default().into());
    ui.set_export_folder(path.parent().map(|parent| parent.display().to_string()).unwrap_or_default().into());
    ui.set_export_path(path.display().to_string().into());
}

pub(super) fn browse(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, folder_only: bool) {
    if guard(ui, state) { return; }
    let mut dialog = rfd::FileDialog::new();
    if let Ok(folder) = export_destination::folder(&ui.get_export_folder()) { dialog = dialog.set_directory(folder); }
    if folder_only {
        if let Some(path) = dialog.set_title("Choose export folder").pick_folder() {
            ui.set_export_folder(path.display().to_string().into());
        }
    } else if let Some(path) = dialog.set_title("Export MP4").set_file_name(ui.get_export_filename().as_str())
        .add_filter("MP4 video", &["mp4"]).save_file() {
        set_path(ui, &ensure_extension(path, "mp4"));
    }
    changed(ui, state);
}

pub(super) fn submit(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, start: bool) {
    if guard(ui, state) { return; }
    changed(ui, state);
    let project = state.borrow().editor.project.clone();
    match settings(ui, &project) {
        Ok(settings) => {
            set_path(ui, &settings.output);
            remember(ui, state);
            save_preferences(state);
            check(ui, state, project, settings, Submit::Add { start });
        },
        Err(message) => error(ui, state, message),
    }
}

pub(super) fn submit_job(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, id: i32, retry: bool) {
    if guard(ui, state) { return; }
    let job = state.borrow().export_jobs.iter().find(|job| job.id == id
        && if retry { matches!(job.status, JobStatus::Failed | JobStatus::Cancelled) } else { job.status == JobStatus::Pending })
        .map(|job| (job.project.clone(), job.settings.clone()));
    if let Some((project, settings)) = job {
        check(ui, state, project, settings, if retry { Submit::Retry(id) } else { Submit::Launch(id) });
    }
}

fn check(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, project: Project, settings: ExportSettings, action: Submit) {
    if state.borrow().export_form.cancelled_check.is_some() {
        state.borrow_mut().queue_active = false;
        error(ui, state, "The previous destination check is still finishing. You can edit settings and try again shortly.".into());
        return;
    }
    stop_play(state);
    sync_source_controls(ui, &state.borrow());
    let valid = parse_export_dimensions(&settings.width.to_string(), &settings.height.to_string())
        .and_then(|_| range(&project, settings.use_in_out))
        .and_then(|_| {
            if settings.frame_rate.num == 0 || settings.frame_rate.den == 0 || settings.frame_rate.fps() > 1000.0
                || !(14..=28).contains(&settings.crf) {
                Err("This queued job has invalid encoding settings. Remove it and add a new export.".into())
            } else { export_range(&project, &settings).map(|_| ()) }
        });
    let (sources, occupied) = {
        let current = state.borrow();
        let mut sources: Vec<PathBuf> = current.editor.project.media.iter().chain(project.media.iter())
            .map(|media| media.path.clone()).collect();
        sources.extend(current.editor.path.clone());
        for job in &current.export_jobs {
            if matches!(job.status, JobStatus::Pending | JobStatus::Rendering) {
                sources.extend(job.project.media.iter().map(|media| media.path.clone()));
            }
        }
        let own = match action { Submit::Launch(id) | Submit::Retry(id) => Some(id), _ => None };
        let occupied: Vec<PathBuf> = current.export_jobs.iter().filter(|job| Some(job.id) != own
            && matches!(job.status, JobStatus::Pending | JobStatus::Rendering))
            .map(|job| job.settings.output.clone()).collect();
        sources.sort();
        sources.dedup();
        (sources, occupied)
    };
    let (tx, rx) = mpsc::channel();
    let output = settings.output.clone();
    let checked_output = output.clone();
    std::thread::spawn(move || { let _ = tx.send(valid.and_then(|_| export_destination::inspect(&checked_output, &sources, &occupied))); });
    let mut current = state.borrow_mut();
    current.export_form.frozen = Some(Fields::capture(ui));
    current.export_form.pending = Some(Pending { project, settings, action, result: rx });
    ui.set_export_busy(true);
    ui.set_export_checking(true);
    ui.set_export_active_path(output.display().to_string().into());
    ui.set_export_progress(0.0);
    ui.set_export_preparing(false);
    ui.set_export_preparation_progress(0.0);
    ui.set_export_eta("".into());
    current.export_eta_at = None;
    ui.set_export_ready(false);
    ui.set_export_cancelling(false);
    ui.set_export_message("Checking export destination…".into());
}

pub(super) fn cancel_check(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) -> bool {
    let pending = state.borrow_mut().export_form.pending.take();
    if let Some(pending) = pending {
        let mut current = state.borrow_mut();
        current.queue_active = false;
        current.export_form.cancelled_check = Some(pending.result);
        current.export_form.frozen = None;
        drop(current);
        ui.set_export_busy(false);
        ui.set_export_checking(false);
        ui.set_export_active_path("".into());
        changed(ui, state);
        ui.set_export_message("Export preparation cancelled. No file was changed.".into());
        return true;
    }
    false
}

pub(super) fn pump(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let retired = state.borrow().export_form.cancelled_check.as_ref()
        .is_some_and(|rx| !matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    if retired { state.borrow_mut().export_form.cancelled_check = None; }
    let result = state.borrow().export_form.pending.as_ref().and_then(|pending| match pending.result.try_recv() {
        Ok(result) => Some(result),
        Err(mpsc::TryRecvError::Empty) => None,
        Err(mpsc::TryRecvError::Disconnected) => Some(Err("Export destination check stopped. Please try again.".into())),
    });
    if let Some(result) = result {
        let pending = state.borrow_mut().export_form.pending.take().unwrap();
        let mut failure = None;
        let mut launch = None;
        match result {
            Ok(existing) => {
                let approved = match pending.action {
                    Submit::Launch(id) | Submit::Retry(id) => state.borrow().export_form.approvals.get(&id) == Some(&existing),
                    _ => false,
                };
                let overwrite = existing.is_none() || approved || rfd::MessageDialog::new()
                    .set_title("Replace existing export?")
                    .set_description(format!("{} already exists. Replace it after this export finishes successfully?", pending.settings.output.display()))
                    .set_buttons(rfd::MessageButtons::YesNo).set_level(rfd::MessageLevel::Warning).show() == rfd::MessageDialogResult::Yes;
                if overwrite {
                    let mut current = state.borrow_mut();
                    let id = match pending.action {
                        Submit::Add { start } => {
                            let id = current.next_export_id;
                            current.next_export_id += 1;
                            current.export_jobs.push(ExportJob { id, project: pending.project, settings: pending.settings,
                                status: JobStatus::Pending, message: "Queued".into() });
                            current.queue_active |= start;
                            id
                        },
                        Submit::Retry(id) => {
                            if let Some(job) = current.export_jobs.iter_mut().find(|job| job.id == id) {
                                job.status = JobStatus::Pending;
                                job.message = "Queued".into();
                            }
                            id
                        },
                        Submit::Launch(id) => { launch = Some(id); id },
                    };
                    current.export_form.approvals.insert(id, existing);
                    ui.set_export_message("Export added to queue".into());
                } else {
                    state.borrow_mut().queue_active = false;
                    ui.set_export_message("Export not started. The existing file was kept.".into());
                }
            },
            Err(message) => {
                let mut current = state.borrow_mut();
                if let Submit::Launch(id) | Submit::Retry(id) = pending.action {
                    let failed_path = current.export_jobs.iter_mut().find(|job| job.id == id).map(|job| {
                        job.status = JobStatus::Failed;
                        job.message = message.clone();
                        job.settings.output.clone()
                    });
                    if let Some(path) = failed_path { current.export_form.result = Some((path, JobStatus::Failed)); }
                }
                current.queue_active = false;
                failure = Some(message);
            },
        }
        ui.set_export_busy(false);
        ui.set_export_checking(false);
        ui.set_export_active_path("".into());
        state.borrow_mut().export_form.frozen = None;
        changed(ui, state);
        sync_queue(ui, state);
        sync_result(ui, state);
        if let Some(message) = failure { error(ui, state, message); ui.set_export_ready(false); }
        if let Some(id) = launch { launch_export_job(ui, state, id); }
        else {
            let start_next = state.borrow().queue_active;
            if start_next { launch_next_export(ui, state); }
        }
    }
    let mut messages = Vec::new();
    state.borrow_mut().export_form.open_results.retain(|rx| match rx.try_recv() {
        Ok(result) => { messages.push(result); false },
        Err(mpsc::TryRecvError::Empty) => true,
        Err(mpsc::TryRecvError::Disconnected) => { messages.push(Err("The file manager could not be started. Please try again.".into())); false },
    });
    for result in messages {
        if let Err(message) = result { error(ui, state, message); }
    }
}

pub(super) fn freeze(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    state.borrow_mut().export_form.frozen = Some(Fields::capture(ui));
    ui.set_export_ready(false);
}

pub(super) fn completed(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, path: PathBuf, status: JobStatus) {
    let mut current = state.borrow_mut();
    if status == JobStatus::Complete { current.export_form.last_completed = Some(path.clone()); }
    current.export_form.result = Some((path, status));
    current.export_form.frozen = None;
    drop(current);
    ui.set_export_active_path("".into());
    sync_result(ui, state);
    changed(ui, state);
}

fn sync_result(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let current = state.borrow();
    if let Some((path, status)) = &current.export_form.result {
        ui.set_export_result_path(path.display().to_string().into());
        ui.set_export_result_state(match status { JobStatus::Complete => 1, JobStatus::Failed => 2, JobStatus::Cancelled => 3, _ => 0 });
    } else { ui.set_export_result_path("".into()); ui.set_export_result_state(0); }
}

pub(super) fn open(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, action: export_destination::OpenAction, id: Option<i32>) {
    use export_destination::OpenAction;
    let path = if matches!(action, OpenAction::Folder) {
        export_destination::folder(&ui.get_export_folder())
    } else if let Some(id) = id {
        state.borrow().export_jobs.iter().find(|job| job.id == id && job.status == JobStatus::Complete)
            .map(|job| job.settings.output.clone()).ok_or_else(|| "This job has no successfully completed export to open.".into())
    } else {
        state.borrow().export_form.last_completed.clone().ok_or_else(|| "There is no successfully completed export to open yet.".into())
    };
    match path {
        Ok(path) => {
            let (tx, rx) = mpsc::channel();
            state.borrow_mut().export_form.open_results.push(rx);
            std::thread::spawn(move || { let _ = tx.send(export_destination::open_output(&path, action)); });
        },
        Err(message) => error(ui, state, message),
    }
}

fn custom_rate(text: &str) -> Option<FrameRate> {
    let (num, den) = text.split_once('/')?;
    let rate = FrameRate::rational(num.parse().ok()?, den.parse().ok()?);
    (rate.num > 0 && rate.den > 0 && rate.fps() <= 1000.0).then_some(rate)
}

pub(super) fn refresh_rates(ui: &AppWindow) {
    let mut items: Vec<LabelItem> = frame_rate_presets().iter()
        .map(|rate| LabelItem { text: rate.label().into() }).collect();
    if let Some(rate) = custom_rate(&ui.get_export_custom_rate()) {
        items.push(LabelItem { text: format!("{} · matched", rate.label()).into() });
    }
    ui.set_export_rates(model(items));
}

pub(super) fn rate(ui: &AppWindow) -> Result<FrameRate, String> {
    frame_rate_presets().get(ui.get_export_fps() as usize).copied()
        .or_else(|| (ui.get_export_fps() == frame_rate_presets().len() as i32)
            .then(|| custom_rate(&ui.get_export_custom_rate())).flatten())
        .ok_or_else(|| "Choose a valid export frame rate or use Match sequence.".into())
}

fn set_rate(ui: &AppWindow, rate: FrameRate) {
    if let Some(index) = frame_rate_presets().iter().position(|preset| *preset == rate) {
        ui.set_export_custom_rate("".into());
        ui.set_export_fps(index as i32);
    } else {
        ui.set_export_custom_rate(format!("{}/{}", rate.num, rate.den).into());
        ui.set_export_fps(frame_rate_presets().len() as i32);
    }
    refresh_rates(ui);
}

fn valid(format: &ExportFormat) -> bool {
    parse_export_dimensions(&format.width.to_string(), &format.height.to_string()).is_ok()
        && format.frame_rate.num > 0 && format.frame_rate.den > 0
        && format.frame_rate.fps() <= 1000.0 && format.crf.is_finite()
        && (14.0..=28.0).contains(&format.crf)
        && (0..=2).contains(&format.speed) && (0..=2).contains(&format.encoder)
}

pub(super) fn remember(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    if !ui.get_export_initialized() { return; }
    let Ok((width, height)) = parse_export_dimensions(&ui.get_export_width(), &ui.get_export_height()) else { return; };
    let Ok(frame_rate) = rate(ui) else { return; };
    let format = ExportFormat { width, height, frame_rate, crf: ui.get_export_crf(), speed: ui.get_export_speed(), encoder: ui.get_export_encoder() };
    if valid(&format) { state.borrow_mut().preferences.export_format = Some(format); }
}

pub(super) fn match_sequence(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    if guard(ui, state) { return; }
    let current = state.borrow();
    let seq = &current.editor.project.sequence;
    ui.set_export_width(seq.width.to_string().into());
    ui.set_export_height(seq.height.to_string().into());
    set_rate(ui, seq.frame_rate);
    ui.set_export_initialized(true);
    drop(current);
    changed(ui, state);
}

pub(super) fn prepare(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    if guard(ui, state) { return; }
    let refresh_default = {
        let current = state.borrow();
        current.export_form.default_destination.as_ref().is_some_and(|(project_path, name, output)| {
            (project_path != &current.editor.path || name != &current.editor.project.name)
                && export_destination::resolve(&ui.get_export_folder(), &ui.get_export_filename()).as_ref().ok() == Some(output)
        })
    };
    if refresh_default {
        state.borrow_mut().export_form.initialized_destination = false;
        ui.set_export_path("".into());
    }
    if !state.borrow().export_form.initialized_destination {
        let current = state.borrow();
        let folder = export_destination::default_folder(current.editor.path.as_deref());
        let mut default = None;
        match folder {
            Ok(folder) => {
                let saved = PathBuf::from(ui.get_export_path().as_str());
                let path = if saved.as_os_str().is_empty() {
                    let path = folder.join(export_destination::default_filename(&current.editor.project.name));
                    default = Some((current.editor.path.clone(), current.editor.project.name.clone(), path.clone()));
                    path
                } else if saved.is_absolute() { saved } else { folder.join(saved) };
                set_path(ui, &path);
            },
            Err(message) => ui.set_export_form_error(message.into()),
        }
        drop(current);
        let mut current = state.borrow_mut();
        current.export_form.initialized_destination = true;
        current.export_form.default_destination = default;
    }
    if !ui.get_export_initialized() {
        let saved = state.borrow().preferences.export_format.clone().filter(valid);
        if let Some(format) = saved {
            ui.set_export_width(format.width.to_string().into());
            ui.set_export_height(format.height.to_string().into());
            set_rate(ui, format.frame_rate);
            ui.set_export_crf(format.crf);
            ui.set_export_speed(format.speed);
            ui.set_export_encoder(format.encoder);
            ui.set_export_initialized(true);
        } else {
            match_sequence(ui, state);
        }
    }
    refresh_rates(ui);
    ui.set_read_crf(format!("{}", ui.get_export_crf().round() as i32).into());
    changed(ui, state);
    sync_result(ui, state);
}

pub(super) fn restore(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    state.borrow_mut().export_form.initialized_destination = false;
    prepare(ui, state);
    let mut current = state.borrow_mut();
    let fallback = export_destination::default_folder(current.editor.path.as_deref()).ok();
    for job in &mut current.export_jobs {
        if !job.settings.output.is_absolute() {
            if let Some(folder) = &fallback { job.settings.output = folder.join(&job.settings.output); }
            job.status = JobStatus::Failed;
            job.message = "Recovered a legacy relative destination. Check the full output path before retrying.".into();
        }
    }
    current.export_form.last_completed = current.export_jobs.iter().rev().find(|job| job.status == JobStatus::Complete)
        .map(|job| job.settings.output.clone());
    current.export_form.result = current.export_jobs.iter().rev()
        .find(|job| matches!(job.status, JobStatus::Complete | JobStatus::Failed | JobStatus::Cancelled))
        .map(|job| (job.settings.output.clone(), job.status));
    drop(current);
    sync_result(ui, state);
    changed(ui, state);
}

