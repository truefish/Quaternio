//! Bounded envelope correlation for manually chosen camera recordings.
use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};
pub fn envelope(path: &Path, cancel: &AtomicBool) -> Result<Vec<f32>, String> {
    let mut out = Vec::new();
    for block in 0..15 {
        if cancel.load(Ordering::Acquire) {
            return Err("Audio synchronization cancelled".into());
        }
        let pcm = super::decode::decode_audio_window(path, block, cancel)?;
        if pcm.samples.is_empty() {
            break;
        }
        for frame in pcm.samples.chunks(1920) {
            let energy =
                frame.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / frame.len().max(1) as f64;
            out.push((energy.sqrt() + 1e-6).ln() as f32);
        }
        if pcm.samples.len() < 192_000 {
            break;
        }
    }
    if out.len() < 150 {
        return Err("Waveform sync needs at least three seconds of shared audible sound".into());
    }
    Ok(out)
}
pub fn offset(reference: &[f32], angle: &[f32], cancel: &AtomicBool) -> Result<f64, String> {
    let mut scores = Vec::new();
    for lag in -500i64..=500 {
        if cancel.load(Ordering::Acquire) {
            return Err("Audio synchronization cancelled".into());
        }
        let start = 0.max(-lag);
        let end = (reference.len() as i64).min(angle.len() as i64 - lag);
        let n = end - start;
        if n < 150 {
            continue;
        }
        let mut sums = [0f64; 5];
        for i in start..end {
            let a = reference[i as usize] as f64;
            let b = angle[(i + lag) as usize] as f64;
            sums[0] += a;
            sums[1] += b;
            sums[2] += a * a;
            sums[3] += b * b;
            sums[4] += a * b;
        }
        let n = n as f64;
        let denominator = ((sums[2] - sums[0] * sums[0] / n) * (sums[3] - sums[1] * sums[1] / n))
            .max(0.)
            .sqrt();
        let score = if denominator > 1e-9 {
            (sums[4] - sums[0] * sums[1] / n) / denominator
        } else {
            0.
        };
        scores.push((lag, score));
    }
    let &(lag, score) = scores
        .iter()
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .ok_or("Not enough overlapping sound")?;
    let runner = scores
        .iter()
        .filter(|(other, _)| (other - lag).abs() > 5)
        .map(|(_, score)| *score)
        .fold(-1., f64::max);
    if score < 0.4 || score - runner < 0.025 {
        return Err(
            "Waveform match is ambiguous. Align the cameras manually using source In marks.".into(),
        );
    }
    Ok(lag as f64 * 0.02)
}
