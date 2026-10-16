use super::*;
use crate::preferences::ExportFormat;

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
    if ui.get_export_busy() { return; }
    let current = state.borrow();
    let seq = &current.editor.project.sequence;
    ui.set_export_width(seq.width.to_string().into());
    ui.set_export_height(seq.height.to_string().into());
    set_rate(ui, seq.frame_rate);
    ui.set_export_initialized(true);
}

pub(super) fn prepare(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    if ui.get_export_path().is_empty() {
        ui.set_export_path(format!("{}.mp4", state.borrow().editor.project.name).into());
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
}

