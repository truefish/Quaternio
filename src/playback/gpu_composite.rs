//! Optional compute composition on DX12, Vulkan or Metal; CPU is always available.
use super::{composite::FrameSource, precision::FloatImage};
use crate::{
    color::management::Interpretation,
    project::{Clip, Project, TrackKind},
};
use std::sync::{Mutex, OnceLock};
use wgpu::util::DeviceExt;
struct Engine {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    name: String,
}
static ENGINE: OnceLock<Mutex<Result<Engine, String>>> = OnceLock::new();
pub fn status() -> String {
    ENGINE
        .get()
        .map(|s| match &*s.lock().unwrap() {
            Ok(e) => format!("GPU: {}", e.name),
            Err(e) => format!("CPU fallback: {e}"),
        })
        .unwrap_or("GPU initializes on the next picture".into())
}
impl Engine {
    fn new() -> Result<Self, String> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            ..Default::default()
        }))
        .map_err(|e| e.to_string())?;
        let info = adapter.get_info();
        if info.device_type == wgpu::DeviceType::Cpu
            && std::env::var_os("QUATERNIO_GPU_ALLOW_SOFTWARE").is_none()
        {
            return Err("No hardware compute adapter".into());
        }
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("Quaternio compositor"),
            ..Default::default()
        }))
        .map_err(|e| e.to_string())?;
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Quaternio layers"),
            source: wgpu::ShaderSource::Wgsl(include_str!("gpu_composite.wgsl").into()),
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("Quaternio layers"),
            layout: None,
            module: &module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        if let Some(error) = pollster::block_on(scope.pop()) {
            return Err(error.to_string());
        }
        Ok(Self {
            device,
            queue,
            pipeline,
            name: info.name,
        })
    }
    fn dispatch(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        source: &wgpu::Buffer,
        layer: &wgpu::Buffer,
        output: &wgpu::Buffer,
        params: &[f32; 40],
        w: u32,
        h: u32,
    ) {
        let uniform = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("Layer parameters"),
                contents: bytemuck::cast_slice(params),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Layer"),
            layout: &self.pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: source.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: layer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: output.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: uniform.as_entire_binding(),
                },
            ],
        });
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("Compose layer"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.dispatch_workgroups(w.div_ceil(8), h.div_ceil(8), 1);
    }
    fn render(
        &self,
        project: &Project,
        frame: i64,
        w: u32,
        h: u32,
        source: &mut dyn FrameSource,
    ) -> Result<Option<FloatImage>, String> {
        let pixels = w as u64 * h as u64;
        let limit = self.device.limits().max_storage_buffer_binding_size as u64;
        if pixels * 16 > limit || pixels * 12 > limit {
            return Ok(None);
        }
        let mut plans = Vec::new();
        let mut input_bytes = 0usize;
        for (index, _) in project
            .sequence
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.kind == TrackKind::Video && !t.muted)
        {
            let layers = crate::transitions::video_layers(&project.sequence, index, frame);
            let mut track = Vec::new();
            for layer in layers {
                if source.is_superseded() {
                    return Ok(None);
                }
                if let Some(id) = layer.clip_id {
                    let Some(c) = project.clip(id) else {
                        continue;
                    };
                    let c = c.evaluated(frame).into_owned();
                    let time = c
                        .source_time_seconds(frame - c.timeline_start, project.sequence.frame_rate);
                    let image = if project.sequence.color.high_precision {
                        source.video_float(c.media_id, time, w, h)
                    } else {
                        source
                            .video(c.media_id, time, w, h)
                            .map(|image| std::sync::Arc::new(FloatImage::from_byte(&image)))
                    };
                    let Some(image) = image else {
                        continue;
                    };
                    input_bytes += image.rgb.len() * 4;
                    if input_bytes > 256 * 1024 * 1024 || image.rgb.len() as u64 * 4 > limit {
                        return Ok(None);
                    }
                    track.push((Some((c, image)), layer.weight));
                } else {
                    track.push((None, layer.weight));
                }
            }
            if !track.is_empty() {
                plans.push(track);
            }
        }
        let scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let make = |label, size, usage| {
            self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage,
                mapped_at_creation: false,
            })
        };
        let output = make(
            "Composited RGB",
            pixels * 12,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        );
        let layer = make(
            "Track accumulation",
            pixels * 16,
            wgpu::BufferUsages::STORAGE,
        );
        let staging = make(
            "Composited readback",
            pixels * 12,
            wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        );
        let dummy = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("Black"),
                contents: &[0; 12],
                usage: wgpu::BufferUsages::STORAGE,
            });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        for track in plans {
            let mut params = [0f32; 40];
            params[0] = w as f32;
            params[1] = h as f32;
            self.dispatch(&mut encoder, &dummy, &layer, &output, &params, w, h);
            for (picture, weight) in track {
                let data = if let Some((c, image)) = picture {
                    params = parameters(project, &c, &image, w, h, weight);
                    Some(
                        self.device
                            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                                label: Some("Source RGB"),
                                contents: bytemuck::cast_slice(&image.rgb),
                                usage: wgpu::BufferUsages::STORAGE,
                            }),
                    )
                } else {
                    params[18] = weight;
                    params[19] = 3.;
                    None
                };
                self.dispatch(
                    &mut encoder,
                    data.as_ref().unwrap_or(&dummy),
                    &layer,
                    &output,
                    &params,
                    w,
                    h,
                );
            }
            params[19] = 2.;
            self.dispatch(&mut encoder, &dummy, &layer, &output, &params, w, h);
        }
        encoder.copy_buffer_to_buffer(&output, 0, &staging, 0, pixels * 12);
        let submission = self.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        staging
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = tx.send(result);
            });
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(std::time::Duration::from_secs(3)),
            })
            .map_err(|e| e.to_string())?;
        rx.recv_timeout(std::time::Duration::from_secs(3))
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        if let Some(error) = pollster::block_on(scope.pop()) {
            return Err(error.to_string());
        }
        let mapped = staging
            .slice(..)
            .get_mapped_range()
            .map_err(|e| e.to_string())?;
        let rgb = bytemuck::cast_slice::<u8, f32>(&mapped).to_vec();
        drop(mapped);
        staging.unmap();
        Ok(Some(FloatImage {
            width: w,
            height: h,
            rgb,
        }))
    }
}
fn parameters(
    project: &Project,
    c: &Clip,
    image: &FloatImage,
    w: u32,
    h: u32,
    weight: f32,
) -> [f32; 40] {
    let t = c.transform;
    let g = c.grade;
    let fit = (w as f32 / image.width as f32).min(h as f32 / image.height as f32);
    let (sin, cos) = t.rotation.to_radians().sin_cos();
    [
        w as f32,
        h as f32,
        image.width as f32,
        image.height as f32,
        w as f32 * 0.5 + c.pos_x * w as f32 / project.sequence.width as f32,
        h as f32 * 0.5 + c.pos_y * h as f32 / project.sequence.height as f32,
        image.width as f32 * fit * c.scale * t.scale_x,
        image.height as f32 * fit * c.scale * t.scale_y,
        sin,
        cos,
        t.anchor_x,
        t.anchor_y,
        t.crop_left,
        t.crop_right,
        t.crop_top,
        t.crop_bottom,
        t.flip_x as u8 as f32,
        t.flip_y as u8 as f32,
        weight,
        1.,
        2f32.powf(g.exposure),
        1. + g.contrast / 100.,
        g.highlights / 100.,
        g.shadows / 100.,
        g.temperature / 100. * 0.15,
        g.tint / 100. * 0.15,
        (1. + g.saturation / 100.).max(0.),
        0.,
        g.lift.master + g.lift.r,
        g.lift.master + g.lift.g,
        g.lift.master + g.lift.b,
        0.,
        1. / (1. + g.gamma.master + g.gamma.r).max(0.05),
        1. / (1. + g.gamma.master + g.gamma.g).max(0.05),
        1. / (1. + g.gamma.master + g.gamma.b).max(0.05),
        0.,
        (1. + g.gain.master + g.gain.r).max(0.),
        (1. + g.gain.master + g.gain.g).max(0.),
        (1. + g.gain.master + g.gain.b).max(0.),
        0.,
    ]
}
pub fn render(
    project: &Project,
    frame: i64,
    w: u32,
    h: u32,
    source: &mut dyn FrameSource,
) -> Option<FloatImage> {
    if project.sequence.color.lut.is_some()
        || project
            .sequence
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.kind == TrackKind::Video && !t.muted)
            .flat_map(|(index, _)| {
                crate::transitions::video_layers(&project.sequence, index, frame)
            })
            .filter_map(|layer| layer.clip_id.and_then(|id| project.clip(id)))
            .any(|c| {
                c.creative.text.is_some()
                    || c.creative.adjustment
                    || c.creative.sequence.is_some()
                    || c.creative.multicam.is_some()
                    || c.creative.lut.is_some()
                    || !matches!(
                        c.creative.input_color,
                        Interpretation::Tagged | Interpretation::Rec709
                    )
                    || project.media(c.media_id).is_some_and(|m| {
                        matches!(m.color.transfer.as_str(), "smpte2084" | "arib-std-b67")
                    })
            })
    {
        return None;
    }
    let state = ENGINE.get_or_init(|| {
        Mutex::new(
            std::panic::catch_unwind(Engine::new)
                .unwrap_or_else(|_| Err("GPU initialization failed".into())),
        )
    });
    let mut state = state.lock().ok()?;
    let engine = state.as_ref().ok()?;
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        engine.render(project, frame, w.max(2) & !1, h.max(2) & !1, source)
    }))
    .unwrap_or_else(|_| Err("GPU rendering failed".into()))
    {
        Ok(result) => result,
        Err(error) => {
            *state = Err(error);
            None
        }
    }
}
