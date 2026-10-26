//! Floating-point SDR composition. Quantization occurs at the display/encoder boundary.
use super::composite::{FrameSource, ImageBuf};
use crate::{
    color::{
        management::{self, Interpretation},
        PreparedGrade,
    },
    project::{Clip, Project, TrackKind},
};
#[derive(Clone, Debug)]
pub struct FloatImage {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<f32>,
}
impl FloatImage {
    pub fn black(w: u32, h: u32) -> Self {
        Self {
            width: w,
            height: h,
            rgb: vec![0.; w as usize * h as usize * 3],
        }
    }
    pub fn from_byte(image: &ImageBuf) -> Self {
        Self {
            width: image.width,
            height: image.height,
            rgb: image.rgb.iter().map(|v| *v as f32 / 255.).collect(),
        }
    }
    pub fn to_byte(&self) -> ImageBuf {
        ImageBuf {
            width: self.width,
            height: self.height,
            rgb: self
                .rgb
                .iter()
                .map(|v| (v.clamp(0., 1.) * 255.).round() as u8)
                .collect(),
        }
    }
    fn sample(&self, u: f32, v: f32) -> [f32; 3] {
        let x = (u * self.width as f32 - 0.5).clamp(0., self.width.saturating_sub(1) as f32);
        let y = (v * self.height as f32 - 0.5).clamp(0., self.height.saturating_sub(1) as f32);
        let a = x.floor() as usize;
        let b = y.floor() as usize;
        let tx = x - a as f32;
        let ty = y - b as f32;
        let mut rgb = [0.; 3];
        for (py, wy) in [(b, 1. - ty), ((b + 1).min(self.height as usize - 1), ty)] {
            for (px, wx) in [(a, 1. - tx), ((a + 1).min(self.width as usize - 1), tx)] {
                for c in 0..3 {
                    rgb[c] += self.rgb[(py * self.width as usize + px) * 3 + c] * wx * wy;
                }
            }
        }
        rgb
    }
}
pub fn needed(project: &Project) -> bool {
    fn visit<'a>(
        p: &'a Project,
        id: &'a str,
        depth: u8,
        seen: &mut std::collections::BTreeSet<&'a str>,
    ) -> bool {
        if depth > 8 || !seen.insert(id) {
            return false;
        }
        let Some(s) = p.sequence_by_id(id) else {
            return false;
        };
        s.color.high_precision
            || s.color.gpu
            || s.color.lut.is_some()
            || s.tracks.iter().any(|t| {
                t.clips.iter().any(|c| c.creative.adjustment)
                    || (t.clips.iter().any(|c| c.creative.text.is_some())
                        && (t.clips.iter().any(|c| c.creative.text.is_none())
                            || s.transitions.iter().any(|transition| {
                                t.clips.iter().any(|c| c.id == transition.anchor.clip_id())
                            })))
            })
            || s.tracks
                .iter()
                .filter(|t| t.kind == TrackKind::Video && !t.muted)
                .flat_map(|t| &t.clips)
                .any(|c| {
                    c.creative.lut.is_some()
                        || c.creative.input_color != Interpretation::Tagged
                        || c.creative
                            .sequence
                            .as_deref()
                            .is_some_and(|id| visit(p, id, depth + 1, seen))
                        || std::iter::once(c.media_id)
                            .chain(
                                c.creative
                                    .multicam
                                    .iter()
                                    .flat_map(|m| m.angles.iter().map(|a| a.media_id)),
                            )
                            .any(|id| {
                                p.media(id).is_some_and(|m| {
                                    matches!(
                                        m.color.transfer.as_str(),
                                        "smpte2084" | "arib-std-b67"
                                    )
                                })
                            })
                })
    }
    visit(
        project,
        &project.active_sequence,
        0,
        &mut std::collections::BTreeSet::new(),
    )
}

fn image(
    project: &Project,
    c: &Clip,
    frame: i64,
    w: u32,
    h: u32,
    source: &mut dyn FrameSource,
) -> Option<std::sync::Arc<FloatImage>> {
    let mut time = c.source_time_seconds(frame - c.timeline_start, project.sequence.frame_rate);
    let mut id = c.media_id;
    if let Some(sequence) = c.creative.sequence.as_deref() {
        let mut nested = project.clone();
        nested.switch_sequence(sequence).ok()?;
        let child_frame = nested.sequence.frame_rate.seconds_to_frame(time);
        let (w, h) =
            super::decode::fitted_size(nested.sequence.width, nested.sequence.height, w, h);
        return Some(std::sync::Arc::new(render(
            &nested,
            child_frame,
            w,
            h,
            source,
        )));
    }
    if let Some(angle) = c
        .creative
        .multicam
        .as_ref()
        .and_then(|m| m.angle(frame - c.timeline_start + c.creative.offset))
    {
        id = angle.media_id;
        time += angle.offset;
    }
    if time < 0. {
        return None;
    }
    let hdr = project
        .media(id)
        .is_some_and(|m| matches!(m.color.transfer.as_str(), "smpte2084" | "arib-std-b67"));
    if project.sequence.color.high_precision
        || hdr
        || matches!(
            c.creative.input_color,
            Interpretation::Pq | Interpretation::Hlg
        )
    {
        source.video_float(id, time, w, h)
    } else {
        source
            .video(id, time, w, h)
            .map(|image| std::sync::Arc::new(FloatImage::from_byte(&image)))
    }
}
fn grade(
    metadata: &management::Metadata,
    c: &Clip,
    rgb: [f32; 3],
    prepared: &PreparedGrade,
) -> [f32; 3] {
    let mut rgb = management::source_to_sdr(rgb, metadata, c.creative.input_color);
    if let Some(lut) = &c.creative.lut {
        rgb = lut.apply(rgb);
    }
    prepared.grade_rgb(rgb)
}
pub fn render(
    project: &Project,
    frame: i64,
    w: u32,
    h: u32,
    source: &mut dyn FrameSource,
) -> FloatImage {
    if project.sequence.color.gpu {
        if let Some(image) = super::gpu_composite::render(project, frame, w, h, source) {
            super::composite::record_compositor(true);
            return image;
        }
    }
    super::composite::record_compositor(false);
    let (w, h) = (w.max(2) & !1, h.max(2) & !1);
    let mut output = FloatImage::black(w, h);
    let sx = w as f32 / project.sequence.width as f32;
    let sy = h as f32 / project.sequence.height as f32;
    for (index, _) in project
        .sequence
        .tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| t.kind == TrackKind::Video && !t.muted)
    {
        if source.is_superseded() {
            return output;
        }
        let layers = crate::transitions::video_layers(&project.sequence, index, frame);
        if layers.is_empty() {
            continue;
        }
        let mut color = vec![0.; output.rgb.len()];
        let mut alpha = vec![0f32; w as usize * h as usize];
        for layer in layers {
            let Some(id) = layer.clip_id else {
                for a in &mut alpha {
                    *a += layer.weight;
                }
                continue;
            };
            let Some(c) = project.clip(id) else {
                continue;
            };
            let c = c.evaluated(frame);
            let opacity = layer.weight.clamp(0., 1.);
            if opacity == 0. {
                continue;
            }
            let prepared = PreparedGrade::new(c.grade);
            let media_id = c
                .creative
                .multicam
                .as_ref()
                .and_then(|m| m.angle(frame - c.timeline_start + c.creative.offset))
                .map_or(c.media_id, |a| a.media_id);
            let metadata = project
                .media(media_id)
                .map(|m| m.color.clone())
                .unwrap_or_default();
            if c.creative.adjustment {
                for (i, pixel) in output.rgb.chunks_exact(3).enumerate() {
                    let original = [pixel[0], pixel[1], pixel[2]];
                    let adjusted = grade(&metadata, &c, original, &prepared);
                    for channel in 0..3 {
                        color[i * 3 + channel] += adjusted[channel] * opacity;
                    }
                    alpha[i] += opacity;
                }
                continue;
            }
            if c.creative.text.is_some() {
                let mut black = ImageBuf::black(w, h);
                let mut white = ImageBuf::solid(w, h, [255; 3]);
                super::text::draw(&mut black, &c, project.sequence.width, layer.weight);
                super::text::draw(&mut white, &c, project.sequence.width, layer.weight);
                for (i, (a, b)) in black
                    .rgb
                    .chunks_exact(3)
                    .zip(white.rgb.chunks_exact(3))
                    .enumerate()
                {
                    let coverage = 1. - (b[0] as f32 - a[0] as f32) / 255.;
                    for channel in 0..3 {
                        color[i * 3 + channel] += a[channel] as f32 / 255.;
                    }
                    alpha[i] += coverage;
                }
                continue;
            }
            let Some(image) = image(project, &c, frame, w, h, source) else {
                continue;
            };
            let t = c.transform;
            let fit =
                (w as f32 / image.width.max(1) as f32).min(h as f32 / image.height.max(1) as f32);
            let dw = image.width as f32 * fit * c.scale * t.scale_x;
            let dh = image.height as f32 * fit * c.scale * t.scale_y;
            if dw <= 0. || dh <= 0. {
                continue;
            }
            let (sin, cos) = t.rotation.to_radians().sin_cos();
            let cx = w as f32 * 0.5 + c.pos_x * sx;
            let cy = h as f32 * 0.5 + c.pos_y * sy;
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let dx = x as f32 + 0.5 - cx;
                    let dy = y as f32 + 0.5 - cy;
                    let mut u = (dx * cos + dy * sin) / dw + t.anchor_x;
                    let mut v = (-dx * sin + dy * cos) / dh + t.anchor_y;
                    if t.flip_x {
                        u = 1. - u;
                    }
                    if t.flip_y {
                        v = 1. - v;
                    }
                    if u < t.crop_left
                        || u >= 1. - t.crop_right
                        || v < t.crop_top
                        || v >= 1. - t.crop_bottom
                    {
                        continue;
                    }
                    let sample = grade(&metadata, &c, image.sample(u, v), &prepared);
                    let pixel = y * w as usize + x;
                    for channel in 0..3 {
                        color[pixel * 3 + channel] += sample[channel] * opacity;
                    }
                    alpha[pixel] += opacity;
                }
            }
        }
        for (i, pixel) in output.rgb.chunks_exact_mut(3).enumerate() {
            let coverage = alpha[i].clamp(0., 1.);
            for c in 0..3 {
                pixel[c] = color[i * 3 + c] + pixel[c] * (1. - coverage);
            }
        }
    }
    if let Some(lut) = &project.sequence.color.lut {
        for pixel in output.rgb.chunks_exact_mut(3) {
            let mapped = lut.apply([pixel[0], pixel[1], pixel[2]]);
            pixel.copy_from_slice(&mapped);
        }
    }
    output
}
