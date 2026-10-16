//! Viewport-bounded timeline pictures. Planning and painting never decode or
//! inspect media files on the UI thread; the idle worker owns that work.
use super::*;
use crate::playback::filmstrips::{plan_visible, FilmstripEngine, FilmstripRequest};
use std::cell::RefCell;

const IMAGE_LIMIT: usize = 64;
const IMAGE_BYTES: usize = 16 * 1024 * 1024;
const RASTER_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
struct ViewKey {
    timeline: TimelineViewKey,
    vertical: u32,
    visible: bool }

#[derive(Clone)]
struct CellPicture {
    clip_id: u64,
    frame: i64,
    span: i64,
    request: FilmstripRequest }

#[derive(Clone, PartialEq, Eq)]
struct RasterKey {
    clip_id: u64,
    version: u64,
    x: u32,
    width: u32,
    height: u32,
    viewport: u32,
    zoom: u64,
    scroll: u64 }

struct Raster {
    key: RasterKey,
    image: slint::Image,
    offset: f32,
    width: f32,
    bytes: usize }

pub(super) struct UiFilmstrips {
    engine: FilmstripEngine,
    generation: u64,
    key: Option<ViewKey>,
    cells: Vec<CellPicture>,
    pictures: VecDeque<(FilmstripRequest, Arc<ImageBuf>)>,
    version: u64,
    rasters: RefCell<VecDeque<Raster>> }

impl Default for UiFilmstrips {
    fn default() -> Self {
        Self {
            engine: FilmstripEngine::new(),
            generation: 0,
            key: None,
            cells: Vec::new(),
            pictures: VecDeque::new(),
            version: 0,
            rasters: RefCell::new(VecDeque::new()) }
    }
}

impl UiFilmstrips {
    pub(super) fn set_busy(&self, busy: bool) {
        self.engine.set_busy(busy);
    }

    pub(super) fn reset(&mut self) {
        self.engine.request_visible(Vec::new());
        self.generation = self.engine.invalidate();
        self.key = None;
        self.cells.clear();
        self.pictures.clear();
        self.changed();
    }

    fn changed(&mut self) {
        self.version = self.version.wrapping_add(1);
        self.rasters.get_mut().clear();
    }

    fn replace(&mut self, key: ViewKey, cells: Vec<CellPicture>) {
        // Old pictures may be reused at another scroll/zoom, but never across
        // a media snapshot change. File stamps remain the worker's concern.
        self.pictures
            .retain(|(request, _)| cells.iter().any(|cell| cell.request.media == request.media));
        self.generation = self
            .engine
            .request_visible(cells.iter().map(|cell| cell.request.clone()).collect());
        self.cells = cells;
        self.key = Some(key);
        self.changed();
    }

    fn collect(&mut self) -> bool {
        let mut changed = false;
        while let Some(result) = self.engine.poll() {
            if result.generation != self.generation
                || !self.cells.iter().any(|cell| cell.request == result.request)
            {
                continue;
            }
            self.pictures
                .retain(|(request, _)| request != &result.request);
            if let Ok(image) = result.image {
                let bytes = image.rgb.len();
                if bytes <= IMAGE_BYTES {
                    while self.pictures.len() >= IMAGE_LIMIT
                        || self
                            .pictures
                            .iter()
                            .map(|(_, image)| image.rgb.len())
                            .sum::<usize>()
                            + bytes
                            > IMAGE_BYTES
                    {
                        if self.pictures.pop_back().is_none() {
                            break;
                        }
                    }
                    self.pictures.push_front((result.request, image));
                }
            }
            changed = true;
        }
        if changed {
            self.changed();
        }
        changed
    }
}

fn view_key(state: &AppState, vertical: f32, visible: bool) -> ViewKey {
    ViewKey {
        timeline: TimelineViewKey::capture(
            &state.editor.project,
            state.edit_gen,
            state.timeline_w,
            state.track_body,
        ),
        vertical: vertical.to_bits(),
        visible }
}

fn plan(state: &AppState, vertical: f32) -> Vec<CellPicture> {
    let seq = &state.editor.project.sequence;
    if !seq.zoom.is_finite()
        || seq.zoom <= 0.0
        || !seq.scroll.is_finite()
        || !state.timeline_w.is_finite()
        || state.timeline_w <= 0.0
    {
        return Vec::new();
    }
    let first = seq.scroll.floor().max(0.0) as i64;
    let last = (seq.scroll + state.timeline_w as f64 / seq.zoom)
        .ceil()
        .max(0.0) as i64;
    let mut visible = Vec::new();
    for row in rows_for(state) {
        if row.h < 32.0 || !waveform_row_visible(row.y, row.h, vertical, state.track_body) {
            continue;
        }
        let track = &seq.tracks[row.track_index];
        if track.kind != TrackKind::Video {
            continue;
        }
        for clip in &track.clips {
            if clip.timeline_start >= last
                || clip.timeline_start.saturating_add(clip.duration) <= first
            {
                continue;
            }
            let Some(media) = state.editor.project.media(clip.media_id) else {
                continue;
            };
            if !media.has_video && media.kind != MediaKind::Still {
                continue;
            }
            visible.push((clip, media, row.h));
            if visible.len() == IMAGE_LIMIT {
                break;
            }
        }
        if visible.len() == IMAGE_LIMIT {
            break;
        }
    }
    let per_clip = (IMAGE_LIMIT / visible.len().max(1)).clamp(1, 32);
    let spacing = (state.timeline_w as f64 / per_clip.saturating_sub(1).max(1) as f64).max(56.0);
    let span = (spacing / seq.zoom).ceil().max(1.0) as i64;
    let mut cells = Vec::new();
    for (clip, media, height) in visible {
        let image_height = (height - 24.0).floor().clamp(8.0, 40.0) as u32;
        for sample in plan_visible(
            clip,
            media,
            seq.frame_rate,
            first,
            last,
            span,
            96,
            image_height,
        )
        .into_iter()
        .take(per_clip)
        {
            cells.push(CellPicture {
                clip_id: clip.id,
                frame: sample.timeline_frame,
                span,
                request: sample.request });
        }
    }
    cells
}

pub(super) fn pump(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let mut current = state.borrow_mut();
    let visible = ui.get_page() == 0 && !ui.get_live_reloading();
    let busy = current.playing
        || current.source.playing
        || current.pointer.down
        || current.source.scrub_origin.is_some()
        || Instant::now() < current.background_idle_after
        || current.program_request_pending
        || current.source_request_pending
        || current.export_running
        || current.import_rx.is_some()
        || ui.get_live_input_active()
        || !visible;
    current.filmstrips.engine.set_busy(busy);
    let key = view_key(&current, ui.get_timeline_scroll_y(), visible);
    let mut changed = false;
    if current.filmstrips.key != Some(key) {
        let cells = if visible {
            plan(&current, ui.get_timeline_scroll_y())
        } else {
            Vec::new()
        };
        current.filmstrips.replace(key, cells);
        changed = true;
    }
    changed |= current.filmstrips.collect();
    drop(current);
    if changed {
        ui.set_clips(model(clip_views(&state.borrow())));
    }
}

fn strip_bounds(x: f32, width: f32, height: f32, viewport: f32) -> Option<(f32, f32, u32, u32)> {
    if ![x, width, height, viewport]
        .iter()
        .all(|value| value.is_finite())
        || height < 32.0
    {
        return None;
    }
    let left = x.max(0.0);
    let visible = (x + width).min(viewport) - left;
    if visible < 2.0 {
        return None;
    }
    Some((
        left,
        visible,
        visible.ceil().clamp(2.0, 2048.0) as u32,
        (height - 24.0).floor().clamp(8.0, 40.0) as u32,
    ))
}

/// Returns a transparent, sparse filmstrip; unavailable cells remain the real
/// clip colour. Images preserve aspect ratio and are cropped at viewport edges.
pub(super) fn picture(
    state: &AppState,
    clip: &Clip,
    x: f32,
    width: f32,
    height: f32,
) -> (slint::Image, f32, f32) {
    let empty = || (slint::Image::default(), 0.0, 0.0);
    let Some((left, visible, pixels_w, pixels_h)) =
        strip_bounds(x, width, height, state.timeline_w)
    else {
        return empty();
    };
    let Some(view) = state.filmstrips.key else {
        return empty();
    };
    if !view.visible
        || view.timeline
            != TimelineViewKey::capture(
                &state.editor.project,
                state.edit_gen,
                state.timeline_w,
                state.track_body,
            )
    {
        return empty();
    }
    if !state.filmstrips.cells.iter().any(|cell| {
        cell.clip_id == clip.id
            && state
                .filmstrips
                .pictures
                .iter()
                .any(|(request, _)| request == &cell.request)
    }) {
        return empty();
    }
    let key = RasterKey {
        clip_id: clip.id,
        version: state.filmstrips.version,
        x: x.to_bits(),
        width: width.to_bits(),
        height: height.to_bits(),
        viewport: state.timeline_w.to_bits(),
        zoom: state.editor.project.sequence.zoom.to_bits(),
        scroll: state.editor.project.sequence.scroll.to_bits() };
    if let Some(raster) = state
        .filmstrips
        .rasters
        .borrow()
        .iter()
        .find(|raster| raster.key == key)
    {
        return (raster.image.clone(), raster.offset, raster.width);
    }
    let mut buffer = SharedPixelBuffer::<slint::Rgba8Pixel>::new(pixels_w, pixels_h);
    let pixels = buffer.make_mut_slice();
    let seq = &state.editor.project.sequence;
    let scale = pixels_w as f64 / visible as f64;
    let mut painted = false;
    for cell in state
        .filmstrips
        .cells
        .iter()
        .filter(|cell| cell.clip_id == clip.id)
    {
        let Some((_, image)) = state
            .filmstrips
            .pictures
            .iter()
            .find(|(request, _)| request == &cell.request)
        else {
            continue;
        };
        if image.width == 0
            || image.height == 0
            || image.rgb.len() != image.width as usize * image.height as usize * 3
        {
            continue;
        }
        let cell_x = (cell.frame as f64 - seq.scroll) * seq.zoom;
        let cell_width = cell.span as f64 * seq.zoom;
        let draw_height = pixels_h as f64;
        let draw_width = (draw_height * image.width as f64 / image.height as f64).min(cell_width);
        let fitted_height =
            (draw_width * image.height as f64 / image.width as f64).min(draw_height);
        if draw_width < 1.0 || fitted_height < 1.0 {
            continue;
        }
        let pixel_start = ((cell_x - left as f64) * scale).floor().max(0.0) as u32;
        let pixel_end = ((cell_x + draw_width - left as f64) * scale)
            .ceil()
            .clamp(0.0, pixels_w as f64) as u32;
        for px in pixel_start..pixel_end {
            let logical_x = left as f64 + (px as f64 + 0.5) / scale;
            let sx = (((logical_x - cell_x) / draw_width) * image.width as f64)
                .floor()
                .clamp(0.0, image.width.saturating_sub(1) as f64) as usize;
            for py in 0..(fitted_height.floor() as u32).min(pixels_h) {
                let sy = ((py as f64 + 0.5) / fitted_height * image.height as f64)
                    .floor()
                    .clamp(0.0, image.height.saturating_sub(1) as f64)
                    as usize;
                let rgb = &image.rgb[(sy * image.width as usize + sx) * 3..][..3];
                pixels[(py * pixels_w + px) as usize] = slint::Rgba8Pixel {
                    r: rgb[0],
                    g: rgb[1],
                    b: rgb[2],
                    a: 255 };
                painted = true;
            }
        }
    }
    if !painted {
        return empty();
    }
    let image = slint::Image::from_rgba8(buffer);
    let bytes = pixels_w as usize * pixels_h as usize * 4;
    let mut cache = state.filmstrips.rasters.borrow_mut();
    while cache.len() >= IMAGE_LIMIT
        || cache.iter().map(|raster| raster.bytes).sum::<usize>() + bytes > RASTER_BYTES
    {
        if cache.pop_back().is_none() {
            break;
        }
    }
    cache.push_front(Raster {
        key,
        image: image.clone(),
        offset: left - x,
        width: visible,
        bytes });
    (image, left - x, visible)
}

