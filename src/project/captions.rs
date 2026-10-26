//! Bounded SRT/WebVTT interchange. Cue ends are exclusive sequence boundaries.
use super::FrameRate;
#[derive(Clone, Debug, PartialEq)]
pub struct Cue {
    pub start: i64,
    pub end: i64,
    pub text: String,
}
fn seconds(s: &str) -> Option<f64> {
    let s = s.trim().replace(',', ".");
    let parts: Vec<_> = s.split(':').collect();
    let (h, m, t) = match parts.as_slice() {
        [m, t] => (0., m.parse::<f64>().ok()?, t.parse::<f64>().ok()?),
        [h, m, t] => (
            h.parse::<f64>().ok()?,
            m.parse::<f64>().ok()?,
            t.parse::<f64>().ok()?,
        ),
        _ => return None,
    };
    if !h.is_finite() || h < 0. || h > 24. * 365. || !(0.0..60.).contains(&m) || !(0.0..60.).contains(&t) {
        return None;
    }
    Some(h * 3600. + m * 60. + t)
}
pub fn parse(input: &str, rate: FrameRate) -> Result<Vec<Cue>, String> {
    if input.len() > 8 * 1024 * 1024 {
        return Err("Caption file exceeds 8 MiB".into());
    }
    let input = input
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let mut cues = Vec::new();
    for block in input.split("\n\n") {
        let lines: Vec<_> = block.lines().collect();
        let Some(i) = lines.iter().position(|s| s.contains("-->")) else {
            continue;
        };
        let (a, b) = lines[i].split_once("-->").unwrap();
        let start = seconds(a).ok_or("Invalid caption start time")?;
        let end =
            seconds(b.split_whitespace().next().unwrap_or("")).ok_or("Invalid caption end time")?;
        if end <= start {
            return Err("Caption end must follow its start".into());
        }
        let text = lines[i + 1..].join("\n");
        if text.len() > 16384 {
            return Err("Caption cue is too long".into());
        }
        if text.is_empty() {
            continue;
        }
        let start = rate.seconds_to_frame(start);
        let minimum_end = start.checked_add(1).ok_or("Caption time exceeds the supported range")?;
        let end = rate.seconds_to_frame(end).max(minimum_end);
        if end <= start { return Err("Invalid caption range".into()); }
        cues.push(Cue { start, end, text });
        if cues.len() > 10_000 {
            return Err("Caption file exceeds 10,000 cues".into());
        }
    }
    if cues.is_empty() {
        return Err("No SRT or WebVTT cues found".into());
    }
    cues.sort_by_key(|c| c.start);
    if cues.windows(2).any(|w| w[0].end > w[1].start) {
        return Err(
            "Overlapping captions need separate tracks. Adjust their times before importing."
                .into(),
        );
    }
    Ok(cues)
}
fn stamp(frame: i64, rate: FrameRate, web: bool) -> String {
    let ms = (rate.frame_to_seconds(frame.max(0)) * 1000.).round() as u64;
    format!(
        "{:02}:{:02}:{:02}{}{:03}",
        ms / 3600000,
        ms / 60000 % 60,
        ms / 1000 % 60,
        if web { '.' } else { ',' },
        ms % 1000
    )
}
pub fn write(cues: &[Cue], rate: FrameRate, web: bool) -> String {
    let mut out = if web {
        "WEBVTT\n\n".into()
    } else {
        String::new()
    };
    for (i, c) in cues.iter().enumerate() {
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            i + 1,
            stamp(c.start, rate, web),
            stamp(c.end, rate, web),
            c.text
        ));
    }
    out
}
