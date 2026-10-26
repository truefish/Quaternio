//! Deterministic stereo finishing shared by preview and export.
use serde::{Deserialize, Serialize};
#[derive(Clone,Copy,Debug,Default,PartialEq,Eq,Serialize,Deserialize)]
pub enum AudioRole{#[default]Normal,Dialogue,Music}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioFinish {
    pub low_db: f32,
    pub mid_db: f32,
    pub high_db: f32,
    pub compressor: bool,
    pub limiter: bool,
    pub ducking: bool,
}
impl Default for AudioFinish {
    fn default() -> Self {
        Self {
            low_db: 0.,
            mid_db: 0.,
            high_db: 0.,
            compressor: false,
            limiter: false,
            ducking: false,
        }
    }
}
impl AudioFinish {
    pub fn is_identity(&self) -> bool {
        *self == Self::default()
    }
    pub fn validate(&self) -> Result<(), String> {
        if [self.low_db, self.mid_db, self.high_db]
            .iter()
            .any(|v| !v.is_finite() || v.abs() > 12.)
        {
            Err("Audio EQ must be between -12 and +12 dB".into())
        } else {
            Ok(())
        }
    }
    /// A compensated 65-tap linear-phase three-band EQ. Prefix/suffix samples
    /// come from the timeline, so seeking and export chunks use identical history.
    pub fn process(&self, input: &[f32], offset: usize, count: usize, rate: u32) -> Vec<f32> {
        let mut low = [0f64; 65];
        let mut upper = [0f64; 65];
        for (k, cut) in [(&mut low, 250.), (&mut upper, 4000.)] {
            let f = (cut / rate as f64).min(0.45);
            for (i, v) in k.iter_mut().enumerate() {
                let x = i as f64 - 32.;
                *v = if x == 0. {
                    2. * f
                } else {
                    (2. * std::f64::consts::PI * f * x).sin() / (std::f64::consts::PI * x)
                } * (0.54 - 0.46 * (2. * std::f64::consts::PI * i as f64 / 64.).cos());
            }
            let sum: f64 = k.iter().sum();
            for v in k {
                *v /= sum;
            }
        }
        let gains = [self.low_db, self.mid_db, self.high_db].map(|db| 10f64.powf(db as f64 / 20.));
        let mut kernel = [0f64; 65];
        for i in 0..65 {
            kernel[i] = low[i] * gains[0]
                + (upper[i] - low[i]) * gains[1]
                + ((if i == 32 { 1. } else { 0. }) - upper[i]) * gains[2];
        }
        let sample = |n: i64, c: usize| -> f64 {
            if n < 0 {
                0.
            } else {
                input
                    .get(n as usize * 2 + c)
                    .copied()
                    .filter(|v| v.is_finite())
                    .unwrap_or(0.) as f64
            }
        };
        let mut out = vec![0.; count * 2];
        for n in 0..count {
            let center = (offset + n) as i64;
            let gain = if self.compressor {
                let mut power = 0.;
                for k in -32..=32 {
                    let a = sample(center + k, 0);
                    let b = sample(center + k, 1);
                    power += (a * a + b * b) / 130.;
                }
                let level = 10. * power.max(1e-12).log10();
                let reduction = if level > -18. {
                    (-18. + (level + 18.) / 3.) - level
                } else {
                    0.
                };
                10f64.powf(reduction / 20.)
            } else {
                1.
            };
            for c in 0..2 {
                let mut v = 0.;
                for (i, k) in kernel.iter().enumerate() {
                    v += sample(center + i as i64 - 32, c) * k;
                }
                v *= gain;
                if self.limiter {
                    v = v.clamp(-0.89125094, 0.89125094);
                }
                out[n * 2 + c] = v as f32;
            }
        }
        out
    }
}
