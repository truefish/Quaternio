//! Shared, deterministic clip animation and source-time mapping.
use super::{Clip, FrameRate};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum Interpolation {
    #[default]
    Linear,
    Hold,
    Ease,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CameraAngle {
    pub media_id: u64,
    pub offset: f64,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AngleCut {
    pub frame: i64,
    pub angle: usize,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Multicam {
    pub angles: Vec<CameraAngle>,
    pub cuts: Vec<AngleCut>,
}
impl Multicam {
    pub fn angle(&self, frame: i64) -> Option<&CameraAngle> {
        let index = self.cuts.partition_point(|c| c.frame <= frame);
        let angle = if index == 0 {
            0
        } else {
            self.cuts[index - 1].angle
        };
        self.angles.get(angle)
    }
    pub fn validate(&self) -> Result<(), String> {
        if !(2..=16).contains(&self.angles.len())
            || self
                .angles
                .iter()
                .any(|a| a.media_id == 0 || !a.offset.is_finite() || a.offset.abs() > 86400.)
            || self.cuts.len() > 100_000
            || self.cuts.windows(2).any(|w| w[0].frame >= w[1].frame)
            || self
                .cuts
                .iter()
                .any(|c| c.frame < 0 || c.angle >= self.angles.len())
        {
            Err("Invalid multicam angles or cuts".into())
        } else {
            Ok(())
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Keyframe {
    pub frame: i64,
    pub value: f32,
    pub interpolation: Interpolation,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum TextStyle {
    #[default]
    Title,
    LowerThird,
    Caption,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TextContent {
    pub text: String,
    pub style: TextStyle,
    pub size: f32,
    pub color: [u8; 3],
    pub background: bool,
}
impl Default for TextContent {
    fn default() -> Self {
        Self {
            text: "Your title".into(),
            style: TextStyle::Title,
            size: 64.,
            color: [255; 3],
            background: false,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Creative {
    pub animation: BTreeMap<String, Vec<Keyframe>>,
    pub offset: i64,
    pub speed: f64,
    pub freeze: bool,
    pub freeze_at: Option<f64>,
    pub text: Option<TextContent>,
    pub sequence: Option<String>,
    pub adjustment: bool,
    pub dialogue: bool,
    pub multicam: Option<Multicam>,
    pub input_color: crate::color::management::Interpretation,
    pub lut: Option<crate::color::management::Cube>,
}
impl Default for Creative {
    fn default() -> Self {
        Self {
            animation: BTreeMap::new(),
            offset: 0,
            speed: 1.,
            freeze: false,
            freeze_at: None,
            text: None,
            sequence: None,
            adjustment: false,
            dialogue: false,
            multicam: None,
            input_color: Default::default(),
            lut: None,
        }
    }
}
pub const PROPERTIES: &[&str] = &[
    "pos-x",
    "pos-y",
    "scale",
    "rotation",
    "opacity",
    "volume",
    "exposure",
    "contrast",
    "highlights",
    "shadows",
    "temperature",
    "tint",
    "saturation",
];
pub fn value(clip: &Clip, name: &str) -> Option<f32> {
    Some(match name {
        "pos-x" => clip.pos_x,
        "pos-y" => clip.pos_y,
        "scale" => clip.scale,
        "rotation" => clip.transform.rotation,
        "opacity" => clip.opacity,
        "volume" => clip.volume,
        "exposure" => clip.grade.exposure,
        "contrast" => clip.grade.contrast,
        "highlights" => clip.grade.highlights,
        "shadows" => clip.grade.shadows,
        "temperature" => clip.grade.temperature,
        "tint" => clip.grade.tint,
        "saturation" => clip.grade.saturation,
        _ => return None,
    })
}
pub fn assign(clip: &mut Clip, name: &str, v: f32) {
    match name {
        "pos-x" => clip.pos_x = v,
        "pos-y" => clip.pos_y = v,
        "scale" => clip.scale = v.clamp(0.01, 10.),
        "rotation" => clip.transform.rotation = v,
        "opacity" => clip.opacity = v.clamp(0., 1.),
        "volume" => clip.volume = v.clamp(0., 4.),
        "exposure" => clip.grade.exposure = v,
        "contrast" => clip.grade.contrast = v,
        "highlights" => clip.grade.highlights = v,
        "shadows" => clip.grade.shadows = v,
        "temperature" => clip.grade.temperature = v,
        "tint" => clip.grade.tint = v,
        "saturation" => clip.grade.saturation = v,
        _ => {}
    }
}
pub fn evaluate(keys: &[Keyframe], frame: i64) -> Option<f32> {
    let first = keys.first()?;
    if frame <= first.frame {
        return Some(first.value);
    }
    let i = keys.partition_point(|k| k.frame <= frame);
    let a = &keys[i - 1];
    let Some(b) = keys.get(i) else {
        return Some(a.value);
    };
    let t = ((frame - a.frame) as f64 / (b.frame - a.frame).max(1) as f64).clamp(0., 1.) as f32;
    let t = match a.interpolation {
        Interpolation::Hold => 0.,
        Interpolation::Linear => t,
        Interpolation::Ease => t * t * (3. - 2. * t),
    };
    Some(a.value + (b.value - a.value) * t)
}
impl Creative {
    pub fn is_identity(&self) -> bool {
        self.animation.is_empty()
            && self.speed == 1.
            && !self.freeze
            && self.text.is_none()
            && self.sequence.is_none()
            && !self.adjustment
            && !self.dialogue
            && self.multicam.is_none()
            && self.input_color == Default::default()
            && self.lut.is_none()
    }
    pub fn validate(&self) -> Result<(), String> {
        if let Some(lut) = &self.lut {
            lut.validate()?;
        }
        if let Some(m) = &self.multicam {
            m.validate()?;
            if self.sequence.is_some() || self.text.is_some() || self.adjustment {
                return Err("Multicam requires media sources".into());
            }
        }
        if self.text.is_some() as u8 + self.sequence.is_some() as u8 + self.adjustment as u8 > 1 {
            return Err("A clip can have only one generated source".into());
        }
        if !self.speed.is_finite()
            || self.speed.abs() < 0.05
            || self.speed.abs() > 16.
            || self.offset.unsigned_abs() > 2_000_000_000
        {
            return Err("Invalid clip timing".into());
        }
        if self.freeze_at.is_some_and(|v| !v.is_finite() || v < 0.) {
            return Err("Invalid freeze frame time".into());
        }
        if self.animation.len() > PROPERTIES.len() {
            return Err("Too many animated properties".into());
        }
        for (name, keys) in &self.animation {
            if !PROPERTIES.contains(&name.as_str())
                || keys.len() > 10_000
                || keys.windows(2).any(|w| w[0].frame >= w[1].frame)
                || keys.iter().any(|k| {
                    k.frame.unsigned_abs() > 2_000_000_000
                        || !k.value.is_finite()
                        || k.value.abs() > 1_000_000.
                })
            {
                return Err("Invalid clip keyframes".into());
            }
        }
        if let Some(t) = &self.text {
            if t.text.len() > 16384 || !t.size.is_finite() || !(8.0..=512.).contains(&t.size) {
                return Err("Invalid title or caption".into());
            }
        }
        Ok(())
    }
}
impl Clip {
    pub fn evaluated(&self, frame: i64) -> std::borrow::Cow<'_, Self> {
        if self.creative.animation.is_empty() {
            return std::borrow::Cow::Borrowed(self);
        }
        let mut c = self.clone();
        let local = frame
            .saturating_sub(self.timeline_start)
            .saturating_add(self.creative.offset);
        for (name, keys) in &self.creative.animation {
            if let Some(v) = evaluate(keys, local) {
                assign(&mut c, name, v);
            }
        }
        std::borrow::Cow::Owned(c)
    }
    pub fn advance_source(&mut self, frames: i64, rate: FrameRate) {
        if self.creative.text.is_some() || self.creative.adjustment {
            // Generated content has no source handle; its animation origin still moves.
        } else if self.creative.speed == 1. && !self.creative.freeze {
            self.source_in = self.source_in.saturating_add(frames);
        } else if !self.creative.freeze {
            self.source_time_offset += rate.frame_to_seconds(frames) * self.creative.speed;
        }
        self.creative.offset = self.creative.offset.saturating_add(frames);
    }
}
