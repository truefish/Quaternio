//! Explicit SDR delivery with bounded, embedded LUTs and HDR source conversion.
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Metadata {
    pub primaries: String,
    pub transfer: String,
    pub matrix: String,
    pub range: String,
    pub bit_depth: u8,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum Interpretation {
    #[default]
    Tagged,
    Rec709,
    Pq,
    Hlg,
    LogLut,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pipeline {
    pub working_space: ColorSpace,
    pub output_space: ColorSpace,
    pub high_precision: bool,
    pub gpu: bool,
    pub lut: Option<Cube>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorSpace {
    #[default]
    Rec709,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cube {
    pub size: usize,
    pub min: [f32; 3],
    pub max: [f32; 3],
    pub values: Vec<[f32; 3]>,
}
impl Cube {
    pub fn validate(&self) -> Result<(), String> {
        if !(2..=33).contains(&self.size)
            || self.values.len() != self.size.pow(3)
            || self
                .min
                .iter()
                .chain(&self.max)
                .chain(self.values.iter().flatten())
                .any(|v| !v.is_finite() || v.abs() > 65536.)
            || (0..3).any(|i| self.max[i] <= self.min[i])
        {
            Err("Invalid 3D LUT (supported size: 2–33)".into())
        } else {
            Ok(())
        }
    }
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > 4 * 1024 * 1024 {
            return Err("LUT exceeds 4 MiB".into());
        }
        let mut lut = Self {
            size: 0,
            min: [0.; 3],
            max: [1.; 3],
            values: Vec::new(),
        };
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let fields: Vec<_> = line.split_whitespace().collect();
            match fields[0] {
                "TITLE" => {}
                "LUT_3D_SIZE" => {
                    if fields.len() != 2 || lut.size != 0 {
                        return Err("Invalid LUT size declaration".into());
                    }
                    lut.size = fields[1].parse().map_err(|_| "Invalid LUT size")?;
                    if !(2..=33).contains(&lut.size) {
                        return Err("Choose a 3D LUT of size 2–33".into());
                    }
                }
                "DOMAIN_MIN" | "DOMAIN_MAX" => {
                    let values = triple(&fields[1..])?;
                    if fields[0] == "DOMAIN_MIN" {
                        lut.min = values;
                    } else {
                        lut.max = values;
                    }
                }
                "LUT_1D_SIZE" => {
                    return Err("Choose a 3D .cube LUT; 1D LUTs are not supported".into())
                }
                _ => {
                    lut.values.push(triple(&fields)?);
                    if lut.values.len() > 33usize.pow(3) {
                        return Err("LUT has too many entries".into());
                    }
                }
            }
        }
        lut.validate()?;
        Ok(lut)
    }
    pub fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        let p = std::array::from_fn::<_, 3, _>(|i| {
            ((rgb[i] - self.min[i]) / (self.max[i] - self.min[i])).clamp(0., 1.)
                * (self.size - 1) as f32
        });
        let lo = p.map(|x| x.floor() as usize);
        let hi = lo.map(|x| (x + 1).min(self.size - 1));
        let t = std::array::from_fn::<_, 3, _>(|i| p[i] - lo[i] as f32);
        let mut out = [0.; 3];
        for z in 0..2 {
            for y in 0..2 {
                for x in 0..2 {
                    let index = (if z == 0 { lo[2] } else { hi[2] }) * self.size * self.size
                        + (if y == 0 { lo[1] } else { hi[1] }) * self.size
                        + if x == 0 { lo[0] } else { hi[0] };
                    let weight = (if x == 0 { 1. - t[0] } else { t[0] })
                        * (if y == 0 { 1. - t[1] } else { t[1] })
                        * (if z == 0 { 1. - t[2] } else { t[2] });
                    for c in 0..3 {
                        out[c] += self.values[index][c] * weight;
                    }
                }
            }
        }
        out
    }
}
fn triple(fields: &[&str]) -> Result<[f32; 3], String> {
    if fields.len() != 3 {
        return Err("Expected three LUT values".into());
    }
    let mut result = [0.; 3];
    for (out, input) in result.iter_mut().zip(fields) {
        *out = input.parse().map_err(|_| "Invalid LUT number")?;
    }
    Ok(result)
}
pub fn source_to_sdr(
    rgb: [f32; 3],
    metadata: &Metadata,
    interpretation: Interpretation,
) -> [f32; 3] {
    let pq = interpretation == Interpretation::Pq
        || interpretation == Interpretation::Tagged && metadata.transfer == "smpte2084";
    let hlg = interpretation == Interpretation::Hlg
        || interpretation == Interpretation::Tagged && metadata.transfer == "arib-std-b67";
    if !pq && !hlg {
        return rgb;
    }
    let mut linear = rgb.map(|v| {
        if pq {
            let p = v.max(0.).powf(1. / 78.84375);
            ((p - 0.8359375).max(0.) / (18.8515625 - 18.6875 * p).max(1e-6)).powf(1. / 0.15930176)
                * 10000.
                / 203.
        } else {
            let scene = if v <= 0.5 {
                v * v / 3.
            } else {
                (((v - 0.5599107) / 0.17883277).exp() + 0.28466892) / 12.
            };
            scene.powf(1.2) * 1000. / 203.
        }
    });
    linear = [
        1.660491 * linear[0] - 0.587641 * linear[1] - 0.07285 * linear[2],
        -0.12455 * linear[0] + 1.1329 * linear[1] - 0.008349 * linear[2],
        -0.018151 * linear[0] - 0.100579 * linear[1] + 1.11873 * linear[2],
    ];
    let luminance = (0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2]).max(0.);
    let tone = 1. / (1. + luminance);
    linear.map(|v| {
        let v = (v * tone).max(0.);
        if v < 0.018 {
            4.5 * v
        } else {
            1.099 * v.powf(0.45) - 0.099
        }
    })
}
