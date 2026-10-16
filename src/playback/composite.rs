use crate::color::PreparedByteGrade;
use crate::project::{Clip, Project, TrackKind};
use crate::transitions::{video_layers, Layer};
use rayon::prelude::*;
use std::sync::OnceLock;

const PARALLEL_PIXEL_THRESHOLD: usize = 128 * 1024;
const OCCLUSION_PREFETCH_BYTES: usize = 32 * 1024 * 1024;

/// One shared, bounded pool keeps preview and export from creating competing
/// sets of compositor workers. Two logical CPUs remain outside this pool.
fn render_pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: OnceLock<Option<rayon::ThreadPool>> = OnceLock::new();
    POOL.get_or_init(|| {
        let available = std::thread::available_parallelism().map_or(1, usize::from);
        let workers = available.saturating_sub(2).min(8);
        if workers <= 1 {
            return None;
        }
        rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .thread_name(|index| format!("quaternio-composite-{index}"))
            .build()
            .ok()
    })
    .as_ref()
}

fn with_rows_mut<T, F>(rows: &mut [T], stride: usize, pixels: usize, parallel: bool, process: F)
where
    T: Send,
    F: Fn(usize, &mut [T]) + Send + Sync,
{
    if parallel && pixels >= PARALLEL_PIXEL_THRESHOLD {
        if let Some(pool) = render_pool() {
            pool.install(|| {
                rows.par_chunks_mut(stride)
                    .enumerate()
                    .for_each(|(index, row)| process(index, row))
            });
            return;
        }
    }
    for (index, row) in rows.chunks_mut(stride).enumerate() {
        process(index, row);
    }
}

fn with_blend_rows_mut<F>(
    acc: &mut [f32],
    alpha: &mut [f32],
    width: usize,
    pixels: usize,
    parallel: bool,
    process: F,
) where
    F: Fn(usize, &mut [f32], &mut [f32]) + Send + Sync,
{
    if parallel && pixels >= PARALLEL_PIXEL_THRESHOLD {
        if let Some(pool) = render_pool() {
            pool.install(|| {
                acc.par_chunks_mut(width * 3)
                    .zip(alpha.par_chunks_mut(width))
                    .enumerate()
                    .for_each(|(index, (color, opacity))| process(index, color, opacity))
            });
            return;
        }
    }
    for (index, (color, opacity)) in acc
        .chunks_mut(width * 3)
        .zip(alpha.chunks_mut(width))
        .enumerate()
    {
        process(index, color, opacity);
    }
}

#[derive(Clone, Debug)]
pub struct ImageBuf {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8> }

impl ImageBuf {
    pub fn black(width: u32, height: u32) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        Self {
            width,
            height,
            rgb: vec![0; (width * height * 3) as usize] }
    }

    pub fn solid(width: u32, height: u32, color: [u8; 3]) -> Self {
        let mut image = Self::black(width, height);
        for pixel in image.rgb.chunks_mut(3) {
            pixel.copy_from_slice(&color);
        }
        image
    }
}

pub trait FrameSource {
    fn video(&mut self, media_id: u64, time_secs: f64, max_w: u32, max_h: u32) -> Option<ImageBuf>;

    /// Preview cancellation is distinct from a missing or corrupt source frame.
    fn is_superseded(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Superseded;

pub fn composite_cancellable(
    project: &Project,
    frame: i64,
    canvas_w: u32,
    canvas_h: u32,
    source: &mut dyn FrameSource,
) -> Result<ImageBuf, Superseded> {
    if source.is_superseded() {
        return Err(Superseded);
    }
    let image = composite(project, frame, canvas_w, canvas_h, source);
    if source.is_superseded() { Err(Superseded) } else { Ok(image) }
}

pub fn composite(
    project: &Project,
    frame: i64,
    canvas_w: u32,
    canvas_h: u32,
    source: &mut dyn FrameSource,
) -> ImageBuf {
    composite_impl(project, frame, canvas_w, canvas_h, source, true, true)
}

fn composite_impl(
    project: &Project,
    frame: i64,
    canvas_w: u32,
    canvas_h: u32,
    source: &mut dyn FrameSource,
    parallel: bool,
    aligned_grading: bool,
) -> ImageBuf {
    composite_planned(
        project,
        frame,
        canvas_w,
        canvas_h,
        source,
        parallel,
        aligned_grading,
        true,
    )
}

struct TrackPlan {
    layers: Vec<Layer>,
    // None means not requested; Some(None) remembers a failed request.
    decoded: Option<Option<ImageBuf>> }

fn composite_planned(
    project: &Project,
    frame: i64,
    canvas_w: u32,
    canvas_h: u32,
    source: &mut dyn FrameSource,
    parallel: bool,
    aligned_grading: bool,
    occlusion: bool,
) -> ImageBuf {
    let canvas_w = canvas_w.max(2) & !1;
    let canvas_h = canvas_h.max(2) & !1;
    let mut dst = ImageBuf::black(canvas_w, canvas_h);
    let sx = canvas_w as f32 / project.sequence.width.max(1) as f32;
    let sy = canvas_h as f32 / project.sequence.height.max(1) as f32;
    let mut acc = Vec::new();
    let mut alpha = Vec::new();
    let mut plans: Vec<TrackPlan> = project
        .sequence
        .tracks
        .iter()
        .enumerate()
        .filter(|(_, track)| track.kind == TrackKind::Video && !track.muted)
        .map(|(index, _)| TrackPlan {
            layers: video_layers(&project.sequence, index, frame),
            decoded: None })
        .filter(|plan| !plan.layers.is_empty())
        .collect();
    let first = if occlusion {
        find_occlusion_start(
            project,
            frame,
            canvas_w,
            canvas_h,
            sx,
            sy,
            source,
            &mut plans,
            OCCLUSION_PREFETCH_BYTES,
        )
    } else {
        0
    };
    for plan in plans.iter_mut().skip(first) {
        if source.is_superseded() {
            return dst;
        }
        let layers = &plan.layers;
        if layers.len() == 1 && layers[0].weight == 1.0 {
            if let Some(id) = layers[0].clip_id {
                if let Some(clip) = project.clip(id) {
                    let image = plan.decoded.take().unwrap_or_else(|| {
                        decode_clip_image(project, clip, frame, canvas_w, canvas_h, source)
                    });
                    if let Some(image) = image {
                        blit_opaque_image(
                            clip,
                            &image,
                            sx,
                            sy,
                            &mut dst,
                            parallel,
                            aligned_grading,
                        );
                    }
                }
            } else {
                dst.rgb.fill(0);
            }
            continue;
        }
        acc.resize((canvas_w * canvas_h * 3) as usize, 0f32);
        alpha.resize((canvas_w * canvas_h) as usize, 0f32);
        acc.fill(0f32);
        alpha.fill(0f32);
        for layer in layers {
            if source.is_superseded() {
                return dst;
            }
            match layer.clip_id {
                None => {
                    for value in &mut alpha {
                        *value += layer.weight;
                    }
                }
                Some(id) => {
                    if let Some(clip) = project.clip(id) {
                        blit_clip(
                            project,
                            clip,
                            frame,
                            layer.weight,
                            canvas_w,
                            canvas_h,
                            sx,
                            sy,
                            source,
                            &mut acc,
                            &mut alpha,
                            parallel,
                            aligned_grading,
                        );
                    }
                }
            }
        }
        let width = canvas_w as usize;
        with_rows_mut(
            &mut dst.rgb,
            width * 3,
            width * canvas_h as usize,
            parallel,
            |py, row| {
                let offset = py * width;
                for px in 0..width {
                    let a = alpha[offset + px].clamp(0.0, 1.0);
                    if a <= 0.0 {
                        continue;
                    }
                    let local_base = px * 3;
                    let base = (offset + px) * 3;
                    for channel in 0..3 {
                        let src = acc[base + channel];
                        let bg = row[local_base + channel] as f32;
                        row[local_base + channel] =
                            (src + bg * (1.0 - a)).round().clamp(0.0, 255.0) as u8;
                    }
                }
            },
        );
    }
    dst
}

fn decode_clip_image(
    project: &Project,
    clip: &Clip,
    frame: i64,
    canvas_w: u32,
    canvas_h: u32,
    source: &mut dyn FrameSource,
) -> Option<ImageBuf> {
    let local = frame.saturating_sub(clip.timeline_start);
    let time = clip.source_time_seconds(local, project.sequence.frame_rate);
    source
        .video(clip.media_id, time.max(0.0), canvas_w, canvas_h)
        .filter(valid_image)
}

fn valid_image(image: &ImageBuf) -> bool {
    image.width > 0
        && image.height > 0
        && (image.width as usize)
            .checked_mul(image.height as usize)
            .and_then(|pixels| pixels.checked_mul(3))
            == Some(image.rgb.len())
}

fn find_occlusion_start(
    project: &Project,
    frame: i64,
    canvas_w: u32,
    canvas_h: u32,
    sx: f32,
    sy: f32,
    source: &mut dyn FrameSource,
    plans: &mut [TrackPlan],
    budget_bytes: usize,
) -> usize {
    let mut retained_bytes = 0usize;
    for (index, plan) in plans.iter_mut().enumerate().rev() {
        if source.is_superseded() {
            break;
        }
        if plan.layers.len() != 1 || plan.layers[0].weight != 1.0 {
            continue;
        }
        let Some(id) = plan.layers[0].clip_id else {
            // An opaque transition black layer covers the canvas without decoding.
            return index;
        };
        let Some(clip) = project.clip(id) else {
            continue;
        };
        // Fitting cannot fill both canvas axes at less than 100% scale. A
        // translated fitted picture at 100% also cannot cover the whole canvas.
        // These checks only reject candidates; decoded dimensions prove coverage.
        if !clip.scale.is_finite()
            || !clip.pos_x.is_finite()
            || !clip.pos_y.is_finite()
            || clip.scale < 1.0
            || (clip.scale == 1.0 && (clip.pos_x != 0.0 || clip.pos_y != 0.0))
        {
            continue;
        }
        if retained_bytes >= budget_bytes {
            // Keep at most the budget plus one decoded frame. Unsearched layers
            // use ordinary composition, so this limit never changes output.
            break;
        }
        let image = decode_clip_image(project, clip, frame, canvas_w, canvas_h, source);
        let covers = image
            .as_ref()
            .is_some_and(|image| image_covers_canvas(clip, image, canvas_w, canvas_h, sx, sy));
        retained_bytes = retained_bytes.saturating_add(image.as_ref().map_or(0, |i| i.rgb.len()));
        plan.decoded = Some(image);
        if covers {
            return index;
        }
    }
    0
}

fn image_covers_canvas(
    clip: &Clip,
    image: &ImageBuf,
    canvas_w: u32,
    canvas_h: u32,
    sx: f32,
    sy: f32,
) -> bool {
    if !valid_image(image) {
        return false;
    }
    let (x, y, dw, dh) = dest_rect(
        canvas_w,
        canvas_h,
        image.width,
        image.height,
        clip.scale.max(0.01),
        clip.pos_x,
        clip.pos_y,
        sx,
        sy,
    );
    x.is_finite()
        && y.is_finite()
        && dw.is_finite()
        && dh.is_finite()
        && dw >= 1.0
        && dh >= 1.0
        && x <= 0.0
        && y <= 0.0
        && x + dw >= canvas_w as f32
        && y + dh >= canvas_h as f32
}

fn blit_opaque_image(
    clip: &Clip,
    image: &ImageBuf,
    sx: f32,
    sy: f32,
    dst: &mut ImageBuf,
    parallel: bool,
    aligned_grading: bool,
) {
    let (x, y, dw, dh) = dest_rect(
        dst.width,
        dst.height,
        image.width,
        image.height,
        clip.scale.max(0.01),
        clip.pos_x,
        clip.pos_y,
        sx,
        sy,
    );
    if dw < 1.0 || dh < 1.0 {
        return;
    }
    let x0 = x.floor().max(0.0) as i32;
    let y0 = y.floor().max(0.0) as i32;
    let x1 = (x + dw).ceil().min(dst.width as f32) as i32;
    let y1 = (y + dh).ceil().min(dst.height as f32) as i32;
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let grade = PreparedByteGrade::new(clip.grade);
    let identity_grade = grade.is_identity();
    if dw == image.width as f32 && dh == image.height as f32 && x.fract() == 0.0 && y.fract() == 0.0
    {
        let bytes = (x1 - x0) as usize * 3;
        if identity_grade {
            // FFmpeg already scaled the usual fitted picture to preview size.
            for py in y0..y1 {
                let src = ((py - y as i32) as usize * image.width as usize
                    + (x0 - x as i32) as usize)
                    * 3;
                let target = (py as usize * dst.width as usize + x0 as usize) * 3;
                dst.rgb[target..target + bytes].copy_from_slice(&image.rgb[src..src + bytes]);
            }
            return;
        }
        if aligned_grading
            && aligned_sample_axis_matches(x0, x1, x, dw)
            && aligned_sample_axis_matches(y0, y1, y, dh)
        {
            let stride = dst.width as usize * 3;
            let pixels = (x1 - x0) as usize * (y1 - y0) as usize;
            let rows = &mut dst.rgb[y0 as usize * stride..y1 as usize * stride];
            with_rows_mut(rows, stride, pixels, parallel, |local_y, row| {
                let py = y0 as usize + local_y;
                // Signed translation is needed when a picture is cropped at the top.
                let src = ((py as i32 - y as i32) as usize * image.width as usize
                    + (x0 - x as i32) as usize)
                    * 3;
                let target = &mut row[x0 as usize * 3..x0 as usize * 3 + bytes];
                for (input, output) in image.rgb[src..src + bytes]
                    .chunks_exact(3)
                    .zip(target.chunks_exact_mut(3))
                {
                    output.copy_from_slice(&grade.grade_u8([input[0], input[1], input[2]]));
                }
            });
            return;
        }
    }
    let columns = prepare_columns(&image, x0, x1, x, dw);
    let stride = dst.width as usize * 3;
    let pixels = (x1 - x0) as usize * (y1 - y0) as usize;
    let rows = &mut dst.rgb[y0 as usize * stride..y1 as usize * stride];
    with_rows_mut(rows, stride, pixels, parallel, |local_y, row| {
        let py = y0 as usize + local_y;
        let sample_row = SampleRow::new(&image, ((py as f32 + 0.5 - y) / dh).clamp(0.0, 0.9999));
        for (local_x, column) in columns.iter().enumerate() {
            let sample = sample_prepared(&image, column, &sample_row);
            let rgb = if identity_grade {
                sample
            } else {
                grade.grade_u8(sample)
            };
            let base = (x0 as usize + local_x) * 3;
            row[base..base + 3].copy_from_slice(&rgb);
        }
    });
}

fn aligned_sample_axis_matches(start: i32, end: i32, origin: f32, extent: f32) -> bool {
    // Bilinear sampling first rounds to bytes. At this coordinate error on both
    // axes, its maximum deviation is under 0.25 of a byte (plus <0.001 floating
    // evaluation error), so every RGB input rounds to the corresponding pixel.
    // Check the existing .9999 clamp too: very large images may need the sampler.
    const MAX_ERROR: f32 = 1.0 / 2048.0;
    if extent > 8_388_608.0 || origin.abs() > 8_388_608.0 || end > 8_388_608 {
        return false;
    }
    (start..end).all(|pixel| {
        let expected = pixel as f32 - origin;
        let normalized = ((pixel as f32 + 0.5 - origin) / extent).clamp(0.0, 0.9999);
        let sampled = normalized * extent - 0.5;
        (sampled - expected).abs() <= MAX_ERROR
    })
}

fn blit_clip(
    project: &Project,
    clip: &Clip,
    frame: i64,
    weight: f32,
    canvas_w: u32,
    canvas_h: u32,
    sx: f32,
    sy: f32,
    source: &mut dyn FrameSource,
    acc: &mut [f32],
    alpha: &mut [f32],
    parallel: bool,
    aligned_sampling: bool,
) {
    if weight <= 0.0 {
        return;
    }
    let Some(image) = decode_clip_image(project, clip, frame, canvas_w, canvas_h, source) else {
        return;
    };
    let (x, y, dw, dh) = dest_rect(
        canvas_w,
        canvas_h,
        image.width,
        image.height,
        clip.scale.max(0.01),
        clip.pos_x,
        clip.pos_y,
        sx,
        sy,
    );
    if dw < 1.0 || dh < 1.0 {
        return;
    }
    let x0 = x.floor().max(0.0) as i32;
    let y0 = y.floor().max(0.0) as i32;
    let x1 = (x + dw).ceil().min(canvas_w as f32) as i32;
    let y1 = (y + dh).ceil().min(canvas_h as f32) as i32;
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let grade = PreparedByteGrade::new(clip.grade);
    let identity_grade = grade.is_identity();
    // Decoding already fits ordinary pictures to the canvas. At an integer
    // placement the sampler returns those same bytes, including during a
    // dissolve, so avoid four pixel reads and interpolation per channel.
    let aligned = aligned_sampling
        && dw == image.width as f32
        && dh == image.height as f32
        && x.fract() == 0.0
        && y.fract() == 0.0
        && aligned_sample_axis_matches(x0, x1, x, dw)
        && aligned_sample_axis_matches(y0, y1, y, dh);
    let columns = if aligned {
        Vec::new()
    } else {
        prepare_columns(&image, x0, x1, x, dw)
    };
    let width = canvas_w as usize;
    let pixels = (x1 - x0) as usize * (y1 - y0) as usize;
    let color_rows = &mut acc[y0 as usize * width * 3..y1 as usize * width * 3];
    let alpha_rows = &mut alpha[y0 as usize * width..y1 as usize * width];
    with_blend_rows_mut(
        color_rows,
        alpha_rows,
        width,
        pixels,
        parallel,
        |local_y, color, opacity| {
            let py = y0 as usize + local_y;
            let sample_row =
                SampleRow::new(&image, ((py as f32 + 0.5 - y) / dh).clamp(0.0, 0.9999));
            let source_start = if aligned {
                ((py as i32 - y as i32) as usize * image.width as usize
                    + (x0 - x as i32) as usize) * 3
            } else {
                0
            };
            for local_x in 0..(x1 - x0) as usize {
                let sample = if aligned {
                    let offset = source_start + local_x * 3;
                    [image.rgb[offset], image.rgb[offset + 1], image.rgb[offset + 2]]
                } else {
                    sample_prepared(&image, &columns[local_x], &sample_row)
                };
                let graded = if identity_grade {
                    [sample[0] as f32, sample[1] as f32, sample[2] as f32]
                } else {
                    let normalized = grade.grade_rgb(sample);
                    [
                        normalized[0] * 255.0,
                        normalized[1] * 255.0,
                        normalized[2] * 255.0,
                    ]
                };
                let offset = x0 as usize + local_x;
                let base = offset * 3;
                color[base] += graded[0] * weight;
                color[base + 1] += graded[1] * weight;
                color[base + 2] += graded[2] * weight;
                opacity[offset] += weight;
            }
        },
    );
}

pub fn dest_rect(
    canvas_w: u32,
    canvas_h: u32,
    src_w: u32,
    src_h: u32,
    scale: f32,
    pos_x: f32,
    pos_y: f32,
    sx: f32,
    sy: f32,
) -> (f32, f32, f32, f32) {
    let fit = (canvas_w as f32 / src_w.max(1) as f32).min(canvas_h as f32 / src_h.max(1) as f32);
    let dw = src_w as f32 * fit * scale;
    let dh = src_h as f32 * fit * scale;
    let x = (canvas_w as f32 - dw) * 0.5 + pos_x * sx;
    let y = (canvas_h as f32 - dh) * 0.5 + pos_y * sy;
    (x, y, dw, dh)
}

#[derive(Clone, Copy)]
struct SampleColumn {
    left: usize,
    right: usize,
    left_weight: f32,
    right_weight: f32 }

impl SampleColumn {
    fn new(image: &ImageBuf, u: f32) -> Self {
        let x = u * image.width as f32 - 0.5;
        let x0 = x.floor().max(0.0) as u32;
        let x1 = (x0 + 1).min(image.width - 1);
        let tx = (x - x0 as f32).clamp(0.0, 1.0);
        Self {
            left: x0 as usize * 3,
            right: x1 as usize * 3,
            left_weight: 1.0 - tx,
            right_weight: tx }
    }
}

struct SampleRow {
    top: usize,
    bottom: usize,
    top_weight: f32,
    bottom_weight: f32 }

impl SampleRow {
    fn new(image: &ImageBuf, v: f32) -> Self {
        let y = v * image.height as f32 - 0.5;
        let y0 = y.floor().max(0.0) as u32;
        let y1 = (y0 + 1).min(image.height - 1);
        let ty = (y - y0 as f32).clamp(0.0, 1.0);
        Self {
            top: y0 as usize * image.width as usize * 3,
            bottom: y1 as usize * image.width as usize * 3,
            top_weight: 1.0 - ty,
            bottom_weight: ty }
    }
}

fn prepare_columns(image: &ImageBuf, start: i32, end: i32, x: f32, dw: f32) -> Vec<SampleColumn> {
    (start..end)
        .map(|px| {
            let u = ((px as f32 + 0.5 - x) / dw).clamp(0.0, 0.9999);
            SampleColumn::new(image, u)
        })
        .collect()
}

#[inline]
fn sample_prepared(image: &ImageBuf, column: &SampleColumn, row: &SampleRow) -> [u8; 3] {
    let mut out = [0u8; 3];
    for channel in 0..3 {
        let top = image.rgb[row.top + column.left + channel] as f32 * column.left_weight
            + image.rgb[row.top + column.right + channel] as f32 * column.right_weight;
        let bottom = image.rgb[row.bottom + column.left + channel] as f32 * column.left_weight
            + image.rgb[row.bottom + column.right + channel] as f32 * column.right_weight;
        out[channel] = (top * row.top_weight + bottom * row.bottom_weight)
            .round()
            .clamp(0.0, 255.0) as u8;
    }
    out
}

