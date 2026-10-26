use super::*;
use crate::color::management::{Cube, Interpretation};
pub(super) fn sync(ui: &AppWindow, s: &AppState) {
    ui.set_picture_precision(s.editor.project.sequence.color.high_precision);
    ui.set_picture_gpu(s.editor.project.sequence.color.gpu);
    let clip = s
        .editor
        .primary_clip()
        .and_then(|id| s.editor.project.clip(id));
    let media = clip.and_then(|c| s.editor.project.media(c.media_id));
    ui.set_picture_input(clip.map_or(0, |c| match c.creative.input_color {
        Interpretation::Tagged => 0,
        Interpretation::Rec709 => 1,
        Interpretation::Pq => 2,
        Interpretation::Hlg => 3,
        Interpretation::LogLut => 4,
    }));
    let detail = media
        .map(|m| {
            format!(
                "Source: {} · {} · {} bits",
                if m.color.primaries.is_empty() {
                    "unspecified primaries"
                } else {
                    &m.color.primaries
                },
                if m.color.transfer.is_empty() {
                    "unspecified transfer (SDR)"
                } else {
                    &m.color.transfer
                },
                m.color.bit_depth.max(8)
            )
        })
        .unwrap_or("Select a clip to set its input color or conversion LUT.".into());
    ui.set_picture_info(
        if s.editor.project.sequence.color.gpu {
            format!("{detail}\n{}", playback::gpu_composite::status())
        } else {
            detail
        }
        .into(),
    );
}
pub(super) fn scopes(ui: &AppWindow, image: &ImageBuf) {
    if ui.get_show_creator() && ui.get_creator_section() == 7 {
        ui.set_picture_scope(to_image(&crate::color::scopes::waveform(
            image,
            ui.get_picture_parade(),
        )));
    }
}
pub(super) fn handle(
    ui: &AppWindow,
    state: &Rc<std::cell::RefCell<AppState>>,
    command: &str,
) -> bool {
    if !command.starts_with("picture-") {
        return false;
    }
    let result = (|| -> Result<(), String> {
        if collab::readonly(&state.borrow()) {
            return Err("Editing permission is required".into());
        }
        let lut = if command == "picture-lut" || command == "picture-output-lut" {
            let Some(path) = rfd::FileDialog::new()
                .add_filter("3D Cube LUT", &["cube"])
                .pick_file()
            else {
                return Ok(());
            };
            let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
            Some(Cube::parse(&text)?)
        } else {
            None
        };
        let mut s = state.borrow_mut();
        let id = s.editor.primary_clip();
        if let Some(id) = id {
            if let Some(reason) = s.editor.edit_target_reason(id, false) {
                if !matches!(
                    command,
                    "picture-precision"
                        | "picture-gpu"
                        | "picture-output-lut"
                        | "picture-output-reset"
                ) {
                    return Err(reason);
                }
            }
        }
        s.editor.edit("picture-pipeline", |p| {
            match command {
                "picture-precision" => p.sequence.color.high_precision = ui.get_picture_precision(),
                "picture-gpu" => p.sequence.color.gpu = ui.get_picture_gpu(),
                "picture-output-lut" => p.sequence.color.lut = lut,
                "picture-output-reset" => p.sequence.color.lut = None,
                _ => {
                    let c = p
                        .clip_mut(id.ok_or("Select a clip")?)
                        .ok_or("Clip missing")?;
                    match command {
                        "picture-lut" => c.creative.lut = lut,
                        "picture-lut-reset" => c.creative.lut = None,
                        "picture-input" => {
                            c.creative.input_color = match ui.get_picture_input() {
                                1 => Interpretation::Rec709,
                                2 => Interpretation::Pq,
                                3 => Interpretation::Hlg,
                                4 => Interpretation::LogLut,
                                _ => Interpretation::Tagged,
                            };
                            if c.creative.input_color == Interpretation::LogLut
                                && c.creative.lut.is_none()
                            {
                                return Err("Load a log-to-Rec.709 conversion LUT first".into());
                            }
                        }
                        _ => return Err("Unknown color action".into()),
                    }
                    c.creative.validate()?;
                }
            }
            Ok(())
        })?;
        s.status = "Color pipeline updated · SDR Rec.709 output".into();
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
