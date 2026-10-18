use super::*;
pub(super) fn sync(ui: &AppWindow, editor: &timeline::Editor) {
    let t = editor.primary_clip().and_then(|id| editor.project.clip(id)).map(|c| c.transform).unwrap_or_default();
    ui.set_motion_rotation(t.rotation); ui.set_motion_scale_x(t.scale_x); ui.set_motion_scale_y(t.scale_y);
    ui.set_motion_anchor_x(t.anchor_x); ui.set_motion_anchor_y(t.anchor_y);
    ui.set_motion_crop_left(t.crop_left); ui.set_motion_crop_right(t.crop_right);
    ui.set_motion_crop_top(t.crop_top); ui.set_motion_crop_bottom(t.crop_bottom);
    ui.set_motion_flip_x(t.flip_x); ui.set_motion_flip_y(t.flip_y);
}
