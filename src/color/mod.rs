use serde::{Deserialize, Serialize};

/// One grade. Basic correction follows Lumetri order, then lift / gamma / gain.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Wheel {
    pub master: f32,
    pub r: f32,
    pub g: f32,
    pub b: f32 }

impl Default for Wheel {
    fn default() -> Self {
        Self {
            master: 0.0,
            r: 0.0,
            g: 0.0,
            b: 0.0 }
    }
}

impl Wheel {
    fn channel(self, index: usize) -> f32 {
        match index {
            0 => self.r,
            1 => self.g,
            _ => self.b }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Grade {
    pub exposure: f32,
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub temperature: f32,
    pub tint: f32,
    pub saturation: f32,
    pub lift: Wheel,
    pub gamma: Wheel,
    pub gain: Wheel }

impl Default for Grade {
    fn default() -> Self {
        Self {
            exposure: 0.0,
            contrast: 0.0,
            highlights: 0.0,
            shadows: 0.0,
            temperature: 0.0,
            tint: 0.0,
            saturation: 0.0,
            lift: Wheel::default(),
            gamma: Wheel::default(),
            gain: Wheel::default() }
    }
}

impl Grade {
    pub fn is_identity(self) -> bool {
        self == Self::default()
    }
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Coefficients shared by every pixel of one clip grade.
///
/// Construction performs the grade-dependent powers and divisions once. Pixel
/// evaluation keeps the original operation order and clamping, without a LUT.
#[derive(Clone, Copy, Debug)]
pub struct PreparedGrade {
    identity: bool,
    exposure: f32,
    contrast: f32,
    highlights: f32,
    shadows: f32,
    temperature: f32,
    tint: f32,
    saturation: f32,
    lift: [f32; 3],
    inverse_gamma: [f32; 3],
    gain: [f32; 3] }

impl PreparedGrade {
    pub fn new(grade: Grade) -> Self {
        let mut lift = [0.0; 3];
        let mut inverse_gamma = [0.0; 3];
        let mut gain = [0.0; 3];
        for index in 0..3 {
            lift[index] = grade.lift.master + grade.lift.channel(index);
            let gamma = (1.0 + grade.gamma.master + grade.gamma.channel(index)).max(0.05);
            inverse_gamma[index] = 1.0 / gamma;
            gain[index] = (1.0 + grade.gain.master + grade.gain.channel(index)).max(0.0);
        }
        Self {
            identity: grade.is_identity(),
            exposure: 2f32.powf(grade.exposure),
            contrast: 1.0 + grade.contrast / 100.0,
            highlights: grade.highlights / 100.0,
            shadows: grade.shadows / 100.0,
            temperature: grade.temperature / 100.0 * 0.15,
            tint: grade.tint / 100.0 * 0.15,
            saturation: (1.0 + grade.saturation / 100.0).max(0.0),
            lift,
            inverse_gamma,
            gain }
    }

    #[inline]
    pub fn is_identity(&self) -> bool {
        self.identity
    }

    #[inline]
    pub fn grade_rgb(&self, rgb: [f32; 3]) -> [f32; 3] {
        if self.identity {
            return rgb;
        }
        self.finish_rgb(self.basic_rgb(rgb))
    }

    #[inline]
    fn basic_rgb(&self, rgb: [f32; 3]) -> [f32; 3] {
        let mut c = rgb;
        for channel in &mut c {
            *channel *= self.exposure;
        }
        for channel in &mut c {
            *channel = (*channel - 0.5) * self.contrast + 0.5;
        }
        if self.highlights != 0.0 {
            for channel in &mut c {
                let weight = smoothstep(0.5, 1.0, *channel);
                if self.highlights >= 0.0 {
                    *channel += self.highlights * weight * (1.0 - *channel);
                } else {
                    *channel += self.highlights * weight * *channel;
                }
            }
        }
        if self.shadows != 0.0 {
            for channel in &mut c {
                let weight = 1.0 - smoothstep(0.0, 0.5, *channel);
                if self.shadows >= 0.0 {
                    *channel += self.shadows * weight * 0.5 * (1.0 - *channel);
                } else {
                    *channel += self.shadows * weight * 0.5 * *channel;
                }
            }
        }
        c[0] += self.temperature;
        c[2] -= self.temperature;
        c[1] -= self.tint;
        c
    }

    #[inline]
    fn finish_rgb(&self, mut c: [f32; 3]) -> [f32; 3] {
        let luma = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
        for channel in &mut c {
            *channel = luma + (*channel - luma) * self.saturation;
        }
        for index in 0..3 {
            let mut x = c[index] + self.lift[index] * (1.0 - c[index]);
            x *= self.gain[index];
            let clamped = x.clamp(0.0, 1.0);
            // powf(x, 1) is x, including signed zero and the clamped endpoints.
            c[index] = if self.inverse_gamma[index] == 1.0 {
                clamped
            } else {
                clamped.powf(self.inverse_gamma[index])
            };
        }
        [
            c[0].clamp(0.0, 1.0),
            c[1].clamp(0.0, 1.0),
            c[2].clamp(0.0, 1.0),
        ]
    }

    #[inline]
    pub fn grade_u8(&self, rgb: [u8; 3]) -> [u8; 3] {
        if self.identity {
            return rgb;
        }
        let linear = self.grade_rgb([
            rgb[0] as f32 / 255.0,
            rgb[1] as f32 / 255.0,
            rgb[2] as f32 / 255.0,
        ]);
        [
            (linear[0] * 255.0).round() as u8,
            (linear[1] * 255.0).round() as u8,
            (linear[2] * 255.0).round() as u8,
        ]
    }
}

/// Exact basic correction for byte-decoded pictures. These operations depend
/// on only one input channel, so all 256 possibilities fit in a 3 KiB table.
/// Saturation and wheels still use the original float expressions; no color
/// quantization or interpolated color cube changes the resulting pixels.
pub struct PreparedByteGrade {
    grade: PreparedGrade,
    basic: [[f32; 256]; 3] }

impl PreparedByteGrade {
    pub fn new(grade: Grade) -> Self {
        let grade = PreparedGrade::new(grade);
        let mut basic = [[0.0; 256]; 3];
        if !grade.is_identity() {
            for value in 0..256 {
                let corrected = grade.basic_rgb([value as f32 / 255.0; 3]);
                for channel in 0..3 {
                    basic[channel][value] = corrected[channel];
                }
            }
        }
        Self { grade, basic }
    }

    #[inline]
    pub fn is_identity(&self) -> bool {
        self.grade.is_identity()
    }

    #[inline]
    pub fn grade_rgb(&self, rgb: [u8; 3]) -> [f32; 3] {
        if self.is_identity() {
            return rgb.map(|channel| channel as f32 / 255.0);
        }
        self.grade.finish_rgb([
            self.basic[0][rgb[0] as usize],
            self.basic[1][rgb[1] as usize],
            self.basic[2][rgb[2] as usize],
        ])
    }

    #[inline]
    pub fn grade_u8(&self, rgb: [u8; 3]) -> [u8; 3] {
        if self.is_identity() {
            return rgb;
        }
        self.grade_rgb(rgb).map(|channel| (channel * 255.0).round() as u8)
    }
}

pub fn grade_rgb(rgb: [f32; 3], grade: Grade) -> [f32; 3] {
    if grade.is_identity() {
        return rgb;
    }
    PreparedGrade::new(grade).grade_rgb(rgb)
}

pub fn grade_u8(rgb: [u8; 3], grade: Grade) -> [u8; 3] {
    if grade.is_identity() {
        return rgb;
    }
    PreparedGrade::new(grade).grade_u8(rgb)
}

