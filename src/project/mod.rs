pub mod review;
pub mod creative;
pub mod captions;
pub mod audio;
pub mod sequences;
pub mod portability;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const PROJECT_VERSION: u32 = 7;
pub const STILL_DURATION_SECS: f64 = 5.0;

/// An exclusively created sibling of the destination, never the last good save.
struct StagedProjectSave {
    path: PathBuf,
    published: bool }

impl StagedProjectSave {
    fn create(destination: &Path) -> Result<(Self, fs::File), String> {
        static NEXT_SAVE: AtomicU64 = AtomicU64::new(1);
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        for _ in 0..32 {
            let serial = NEXT_SAVE.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(
                ".quaternio-save-{}-{stamp}-{serial}.tmp",
                std::process::id()
            ));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => {
                    return Ok((
                        Self {
                            path,
                            published: false },
                        file,
                    ))
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("Could not stage project save: {error}")) }
        }
        Err("Could not create a unique project save file".into())
    }
}

impl Drop for StagedProjectSave {
    fn drop(&mut self) {
        // Only this save's exclusively created file may be cleaned up.
        if !self.published {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Publish a fully flushed sibling file, preserving the previous destination on
/// write failure. Shared by local project and collaboration checkpoints.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let (mut staged, mut file) = StagedProjectSave::create(path)?;
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    result.map_err(|e| e.to_string())?;
    fs::rename(&staged.path, path).map_err(|e| e.to_string())?;
    staged.published = true;
    Ok(())
}

fn normalize_bin_name(name: &str) -> Result<String, String> {
    if name.chars().any(char::is_control) {
        return Err("Bin names cannot contain control characters".into());
    }
    let name = name.replace('\\', "/");
    let parts: Vec<_> = name
        .split('/')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.iter().any(|part| matches!(*part, "." | "..")) {
        return Err("Use a name for each part of the bin path".into());
    }
    let normalized = parts.join("/");
    if normalized.chars().count() > 160 {
        return Err("Bin names must be 160 characters or fewer".into());
    }
    Ok(normalized)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ShortcutProfile {
    #[default]
    Premiere,
    Resolve }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaKind {
    Video,
    Audio,
    Still }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrackKind {
    Video,
    Audio }

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrameRate {
    pub num: u32,
    pub den: u32 }

impl Default for FrameRate {
    fn default() -> Self {
        Self { num: 24, den: 1 }
    }
}

impl FrameRate {
    pub const fn rational(num: u32, den: u32) -> Self {
        Self { num, den }
    }

    pub fn fps(self) -> f64 {
        if self.den == 0 {
            0.0
        } else {
            self.num as f64 / self.den as f64
        }
    }

    pub fn nominal(self) -> i64 {
        self.fps().round().max(1.0) as i64
    }

    pub fn is_drop(self) -> bool {
        self.den == 1001 && (self.num == 30000 || self.num == 60000)
    }

    pub fn frame_to_seconds(self, frame: i64) -> f64 {
        if self.num == 0 {
            0.0
        } else {
            frame as f64 * self.den as f64 / self.num as f64
        }
    }

    pub fn seconds_to_frame(self, seconds: f64) -> i64 {
        if self.num == 0 {
            0
        } else {
            (seconds * self.num as f64 / self.den as f64).round() as i64
        }
    }

    pub fn label(self) -> String {
        match (self.num, self.den) {
            (24000, 1001) => "23.976".into(),
            (24, 1) => "24".into(),
            (25, 1) => "25".into(),
            (30000, 1001) => "29.97".into(),
            (30, 1) => "30".into(),
            (50, 1) => "50".into(),
            (60000, 1001) => "59.94".into(),
            (60, 1) => "60".into(),
            (n, 1) => n.to_string(),
            (n, d) => format!("{n}/{d}") }
    }
}

/// A source boundary in its own rational timebase, independent of the sequence.
/// Source ranges are half-open: In is included and Out is excluded.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceTime {
    pub frame: i64,
    pub rate: FrameRate }

impl SourceTime {
    pub const fn new(frame: i64, rate: FrameRate) -> Self {
        Self { frame, rate }
    }

    pub fn seconds(self) -> f64 {
        self.rate.frame_to_seconds(self.frame)
    }

    pub fn is_valid(self) -> bool {
        self.frame >= 0 && self.rate.num > 0 && self.rate.den > 0
    }

    /// Convert a display position to another timebase without changing the mark.
    pub fn frame_at_rate(self, rate: FrameRate) -> i64 {
        if self.rate.num == 0 || rate.den == 0 {
            return 0;
        }
        let numerator = self.frame as i128 * self.rate.den as i128 * rate.num as i128;
        let denominator = self.rate.num as i128 * rate.den as i128;
        let rounded = if numerator >= 0 {
            (numerator + denominator / 2) / denominator
        } else {
            -((-numerator + denominator / 2) / denominator)
        };
        rounded.clamp(i64::MIN as i128, i64::MAX as i128) as i64
    }

    pub fn frame_ceil_at_rate(self, rate: FrameRate) -> i64 {
        if !self.is_valid() || rate.den == 0 {
            return 0;
        }
        let numerator = self.frame as i128 * self.rate.den as i128 * rate.num as i128;
        let denominator = self.rate.num as i128 * rate.den as i128;
        ((numerator + denominator - 1) / denominator).min(i64::MAX as i128) as i64
    }

}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SourceRange {
    pub start: SourceTime,
    pub end: SourceTime }

impl SourceRange {
    pub fn duration_seconds(self) -> f64 {
        self.end.seconds() - self.start.seconds()
    }

    pub fn is_valid(self) -> bool {
        self.start.is_valid() && self.end.is_valid() && self.end.seconds() > self.start.seconds()
    }

    pub fn first_frame(self, rate: FrameRate) -> i64 {
        self.start.frame_at_rate(rate)
    }

    /// Convert exclusive Out to the final included display-frame index.
    pub fn last_frame(self, rate: FrameRate) -> i64 {
        self.end.frame_ceil_at_rate(rate).saturating_sub(1)
    }

    pub fn placement(self, rate: FrameRate) -> Result<SourcePlacementRange, String> {
        if rate.num == 0 || rate.den == 0 || !self.is_valid() {
            return Err("The source range or sequence frame rate is invalid".into());
        }
        let start = self.start.seconds();
        let source_in = self.start.frame_at_rate(rate).max(0);
        let source_time_offset = start - rate.frame_to_seconds(source_in);
        let duration = self.duration_frames(rate).max(1);
        let last = start + rate.frame_to_seconds(duration - 1);
        if last >= self.end.seconds() {
            return Err("The source range has no playable picture at this sequence rate".into());
        }
        Ok(SourcePlacementRange {
            source_in,
            source_time_offset,
            duration })
    }

    /// Nearest sequence-frame duration, using rational boundaries when the
    /// intermediate products fit. Half-frame ties round toward a longer range.
    pub fn duration_frames(self, rate: FrameRate) -> i64 {
        let rational = (|| {
            if !self.is_valid() || rate.num == 0 || rate.den == 0 {
                return None;
            }
            let end = (self.end.frame as i128)
                .checked_mul(self.end.rate.den as i128)?
                .checked_mul(self.start.rate.num as i128)?;
            let start = (self.start.frame as i128)
                .checked_mul(self.start.rate.den as i128)?
                .checked_mul(self.end.rate.num as i128)?;
            let numerator = end.checked_sub(start)?.checked_mul(rate.num as i128)?;
            let denominator = (self.start.rate.num as i128)
                .checked_mul(self.end.rate.num as i128)?
                .checked_mul(rate.den as i128)?;
            Some(
                (numerator.checked_add(denominator / 2)? / denominator).min(i64::MAX as i128)
                    as i64,
            )
        })();
        rational.unwrap_or_else(|| rate.seconds_to_frame(self.duration_seconds()))
    }
}

/// Sequence-frame placement with a precise source origin. The duration alone
/// is quantized to sequence frames; the selected native first picture is not.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SourcePlacementRange {
    pub source_in: i64,
    pub source_time_offset: f64,
    pub duration: i64 }



const FRAME_RATE_PRESETS: &[FrameRate] = &[
    FrameRate::rational(24000, 1001),
    FrameRate::rational(24, 1),
    FrameRate::rational(25, 1),
    FrameRate::rational(30000, 1001),
    FrameRate::rational(30, 1),
    FrameRate::rational(50, 1),
    FrameRate::rational(60000, 1001),
    FrameRate::rational(60, 1),
];

pub fn frame_rate_presets() -> &'static [FrameRate] {
    FRAME_RATE_PRESETS
}


#[derive(Clone, Copy, Debug)]
pub struct SequencePreset {
    pub label: &'static str,
    pub width: u32,
    pub height: u32,
    pub rate: FrameRate }

const SEQUENCE_PRESETS: &[SequencePreset] = &[
    SequencePreset {
        label: "1080p · 1920×1080 23.976",
        width: 1920,
        height: 1080,
        rate: FrameRate::rational(24000, 1001) },
    SequencePreset {
        label: "1080p · 1920×1080 24",
        width: 1920,
        height: 1080,
        rate: FrameRate::rational(24, 1) },
    SequencePreset {
        label: "1080p · 1920×1080 25",
        width: 1920,
        height: 1080,
        rate: FrameRate::rational(25, 1) },
    SequencePreset {
        label: "1080p · 1920×1080 29.97",
        width: 1920,
        height: 1080,
        rate: FrameRate::rational(30000, 1001) },
    SequencePreset {
        label: "1080p · 1920×1080 30",
        width: 1920,
        height: 1080,
        rate: FrameRate::rational(30, 1) },
    SequencePreset {
        label: "720p · 1280×720 24",
        width: 1280,
        height: 720,
        rate: FrameRate::rational(24, 1) },
    SequencePreset {
        label: "720p · 1280×720 30",
        width: 1280,
        height: 720,
        rate: FrameRate::rational(30, 1) },
    SequencePreset {
        label: "720p · 1280×720 60",
        width: 1280,
        height: 720,
        rate: FrameRate::rational(60, 1) },
    SequencePreset {
        label: "4K · 3840×2160 24",
        width: 3840,
        height: 2160,
        rate: FrameRate::rational(24, 1) },
    SequencePreset {
        label: "4K · 3840×2160 30",
        width: 3840,
        height: 2160,
        rate: FrameRate::rational(30, 1) },
    SequencePreset {
        label: "4K · 3840×2160 60",
        width: 3840,
        height: 2160,
        rate: FrameRate::rational(60, 1) },
    SequencePreset {
        label: "Portrait · 1080×1920 30",
        width: 1080,
        height: 1920,
        rate: FrameRate::rational(30, 1) },
    SequencePreset {
        label: "Portrait · 1080×1920 60",
        width: 1080,
        height: 1920,
        rate: FrameRate::rational(60, 1) },
    SequencePreset {
        label: "Square · 1080×1080 30",
        width: 1080,
        height: 1080,
        rate: FrameRate::rational(30, 1) },
];

pub fn sequence_presets() -> &'static [SequencePreset] {
    SEQUENCE_PRESETS
}

pub fn format_timecode(frame: i64, rate: FrameRate) -> String {
    if frame < 0 {
        return format!("-{}", format_timecode(-frame, rate));
    }
    let fps = rate.nominal().max(1);
    let shown = if rate.is_drop() {
        drop_display_frames(frame, fps)
    } else {
        frame
    };
    let sep = if rate.is_drop() { ';' } else { ':' };
    let ff = shown % fps;
    let total_secs = shown / fps;
    let s = total_secs % 60;
    let total_mins = total_secs / 60;
    let m = total_mins % 60;
    let h = total_mins / 60;
    format!("{h:02}:{m:02}:{s:02}{sep}{ff:02}")
}

fn drop_display_frames(frame: i64, fps: i64) -> i64 {
    let drop = if fps >= 60 { 4 } else { 2 };
    let frames_per_min = fps * 60 - drop;
    let frames_per_10 = fps * 60 * 10 - drop * 9;
    let d = frame / frames_per_10;
    let m = frame % frames_per_10;
    let mut shown = frame + drop * 9 * d;
    if m > drop {
        shown += drop * ((m - drop) / frames_per_min);
    }
    shown
}

pub fn parse_timecode(text: &str, rate: FrameRate) -> Option<i64> {
    let text = text.trim();
    if text.is_empty() || rate.num == 0 || rate.den == 0 {
        return None;
    }
    let negative = text.starts_with('-');
    let text = text.strip_prefix('-').unwrap_or(text);
    let parts: Vec<&str> = text.split([':', ';']).collect();
    if parts.iter().any(|part| part.is_empty() || !part.bytes().all(|ch| ch.is_ascii_digit())) {
        return None;
    }
    let frame = if parts.len() == 1 {
        parts[0].parse::<i128>().ok()?
    } else if parts.len() == 4 {
        let h: i128 = parts[0].parse().ok()?;
        let m: i128 = parts[1].parse().ok()?;
        let s: i128 = parts[2].parse().ok()?;
        let f: i128 = parts[3].parse().ok()?;
        let fps = i128::from(rate.nominal().max(1));
        if m >= 60 || s >= 60 || f >= fps { return None; }
        let total_minutes = h.checked_mul(60)?.checked_add(m)?;
        let nominal = total_minutes.checked_mul(60)?.checked_add(s)?.checked_mul(fps)?.checked_add(f)?;
        if rate.is_drop() {
            let drop = if fps >= 60 { 4 } else { 2 };
            // These labels do not exist in drop-frame timecode.
            if m % 10 != 0 && s == 0 && f < drop { return None; }
            nominal.checked_sub(drop * (total_minutes - total_minutes / 10))?
        } else {
            nominal
        }
    } else {
        return None;
    };
    i64::try_from(if negative { frame.checked_neg()? } else { frame }).ok()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Media {
    #[serde(default)] pub color: crate::color::management::Metadata,
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub bin: String,
    pub path: PathBuf,
    pub kind: MediaKind,
    pub duration_secs: f64,
    pub width: u32,
    pub height: u32,
    pub has_video: bool,
    pub has_audio: bool,
    pub source_fps: f64,
    #[serde(default)]
    pub source_in_mark: Option<SourceTime>,
    #[serde(default)]
    pub source_out_mark: Option<SourceTime> }

impl Media {
    /// Video uses its native frame grid, stills use 24 fps, and audio uses
    /// millisecond ticks. Saved boundaries retain their own timebase.
    pub fn source_rate(&self) -> FrameRate {
        if self.kind == MediaKind::Still {
            return FrameRate::default();
        }
        if self.has_video && self.source_fps.is_finite() && self.source_fps > 0.0 {
            for rate in frame_rate_presets() {
                if (rate.fps() - self.source_fps).abs() < 0.001 {
                    return *rate;
                }
            }
            let num = (self.source_fps * 1_000_000.0).round();
            if num >= 1.0 && num <= u32::MAX as f64 {
                let mut a = num as u32;
                let mut b = 1_000_000u32;
                while b != 0 {
                    let next = a % b;
                    a = b;
                    b = next;
                }
                return FrameRate::rational(num as u32 / a, 1_000_000 / a);
            }
        }
        if self.kind == MediaKind::Audio || !self.has_video && self.has_audio {
            FrameRate::rational(1000, 1)
        } else {
            FrameRate::default()
        }
    }

    pub fn source_frames(&self) -> i64 {
        self.source_end_boundary()
            .frame_at_rate(self.source_rate())
            .max(1)
    }

    pub fn source_duration_seconds(&self) -> f64 {
        if self.kind == MediaKind::Still {
            STILL_DURATION_SECS
        } else if self.duration_secs.is_finite() && self.duration_secs > 0.0 {
            self.duration_secs
        } else {
            self.source_rate().frame_to_seconds(1)
        }
    }

    /// Exact native EOFs keep their frame boundary. Other probe durations use
    /// nanosecond ticks instead of rounding away a partial-frame media tail.
    pub fn source_end_boundary(&self) -> SourceTime {
        let duration = self.source_duration_seconds();
        let native = self.source_rate();
        let boundary = SourceTime::new(native.seconds_to_frame(duration).max(1), native);
        if (boundary.seconds() - duration).abs() <= 1e-9 {
            return boundary;
        }
        let rate = FrameRate::rational(1_000_000_000, 1);
        SourceTime::new(rate.seconds_to_frame(duration), rate)
    }

    /// Inclusive displayed-picture Out marking, clamped to actual media EOF.
    pub fn source_out_after_frame(&self, frame: i64) -> SourceTime {
        let following = SourceTime::new(
            frame.clamp(0, self.source_frames() - 1).saturating_add(1),
            self.source_rate(),
        );
        if following.seconds() > self.source_duration_seconds() {
            self.source_end_boundary()
        } else {
            following
        }
    }

    pub fn source_range(&self) -> Result<SourceRange, String> {
        let range = SourceRange {
            start: self
                .source_in_mark
                .unwrap_or(SourceTime::new(0, self.source_rate())),
            end: self.source_out_mark.unwrap_or(self.source_end_boundary()) };
        self.validate_source_range(range)?;
        Ok(range)
    }

    pub fn validate_source_range(&self, range: SourceRange) -> Result<(), String> {
        if !range.is_valid() {
            return Err("Source Out must follow Source In".into());
        }
        if range.end.seconds() > self.source_duration_seconds() + 1e-9 {
            return Err("Source range extends past the end of the media".into());
        }
        if range.start.seconds() >= self.source_duration_seconds() {
            return Err("Source In must precede the end of the media".into());
        }
        Ok(())
    }

    /// Quantize playback duration to the nearest sequence frame, with one
    /// picture minimum. Every sampled picture starts before Out; the last
    /// output frame may finish less than one sequence frame beyond that mark.
    pub fn source_placement_range(&self, rate: FrameRate) -> Result<SourcePlacementRange, String> {
        self.source_range()?.placement(rate)
    }

    /// Strict setter: invalid boundaries leave the previous marks unchanged.
    pub fn set_source_marks(
        &mut self,
        source_in: Option<SourceTime>,
        source_out: Option<SourceTime>,
    ) -> Result<(), String> {
        let mut candidate = self.clone();
        candidate.source_in_mark = source_in;
        candidate.source_out_mark = source_out;
        candidate.source_range()?;
        self.source_in_mark = source_in;
        self.source_out_mark = source_out;
        Ok(())
    }

    /// Moving In beyond a previous Out clears Out rather than storing an
    /// inverted range. The new In itself must still be before media EOF.
    pub fn mark_source_in(&mut self, boundary: SourceTime) -> Result<(), String> {
        let source_out = self
            .source_out_mark
            .filter(|out| out.seconds() > boundary.seconds());
        self.set_source_marks(Some(boundary), source_out)
    }

    /// Accepts the exclusive boundary, not the displayed frame. Call
    /// the exclusive boundary following the displayed picture when marking Out.
    pub fn mark_source_out(&mut self, boundary: SourceTime) -> Result<(), String> {
        let source_in = self
            .source_in_mark
            .filter(|input| input.seconds() < boundary.seconds());
        self.set_source_marks(source_in, Some(boundary))
    }

    pub fn clear_source_marks(&mut self) {
        self.source_in_mark = None;
        self.source_out_mark = None;
    }

    pub fn clip_source_is_valid(&self, clip: &Clip, rate: FrameRate) -> bool {
        if clip.source_in < 0 || clip.duration < 1 || !clip.source_time_offset.is_finite() {
            return false;
        }
        let start = clip.source_start_seconds(rate);
        if !start.is_finite() || start < -1e-9 {
            return false;
        }
        if self.kind == MediaKind::Still {
            return true;
        }
        // Duration is a sequence-frame count. Validate the final sample start,
        // allowing only the final frame's unsampled tail to cross media EOF.
        let last = clip.source_time_seconds(clip.duration - 1, rate);
        start < self.source_duration_seconds() && last.is_finite() && last >= -1e-9 && last < self.source_duration_seconds()
    }

    pub fn timeline_frames(&self, rate: FrameRate) -> i64 {
        if self.kind == MediaKind::Still {
            rate.seconds_to_frame(STILL_DURATION_SECS).max(1)
        } else {
            rate.seconds_to_frame(self.duration_secs).max(1)
        }
    }

    pub fn source_limit(&self, rate: FrameRate) -> Option<i64> {
        if self.kind == MediaKind::Still {
            None
        } else {
            Some(rate.seconds_to_frame(self.duration_secs.max(0.0)).max(1))
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    #[serde(default)]
    pub creative: creative::Creative,
    pub id: u64,
    pub media_id: u64,
    pub name: String,
    pub timeline_start: i64,
    pub duration: i64,
    pub source_in: i64,
    /// Precise source seconds relative to `source_in` in sequence frames.
    #[serde(default)]
    pub source_time_offset: f64,
    pub link: Option<u64>,
    pub volume: f32,
    pub pos_x: f32,
    pub pos_y: f32,
    pub scale: f32,
    pub opacity: f32,
    #[serde(default)]
    pub transform: Transform,
    #[serde(default)]
    pub grade: crate::color::Grade }

impl Clip {
    pub fn source_start_seconds(&self, rate: FrameRate) -> f64 {
        rate.frame_to_seconds(self.source_in) + self.source_time_offset
    }

    pub fn source_time_seconds(&self, local_frames: i64, rate: FrameRate) -> f64 {
        if self.creative.freeze { return self.creative.freeze_at.unwrap_or_else(||self.source_start_seconds(rate)); }
        if self.creative.speed != 1. { return self.source_start_seconds(rate)+rate.frame_to_seconds(local_frames)*self.creative.speed; }
        match self.source_in.checked_add(local_frames) {
            Some(frame) => rate.frame_to_seconds(frame) + self.source_time_offset,
            None => self.source_start_seconds(rate) + rate.frame_to_seconds(local_frames) }
    }

    pub fn end(&self) -> i64 {
        self.timeline_start + self.duration
    }

    pub fn contains(&self, frame: i64) -> bool {
        frame >= self.timeline_start && frame < self.end()
    }
}

/// Additional motion controls; crop and anchor use source-normalized axes.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Transform {
    pub rotation: f32,
    pub scale_x: f32,
    pub scale_y: f32,
    pub anchor_x: f32,
    pub anchor_y: f32,
    pub crop_left: f32,
    pub crop_right: f32,
    pub crop_top: f32,
    pub crop_bottom: f32,
    pub flip_x: bool,
    pub flip_y: bool,
}
impl Default for Transform {
    fn default() -> Self {
        Self { rotation: 0.0, scale_x: 1.0, scale_y: 1.0, anchor_x: 0.5, anchor_y: 0.5,
            crop_left: 0.0, crop_right: 0.0, crop_top: 0.0, crop_bottom: 0.0, flip_x: false, flip_y: false }
    }
}
impl Transform {
    pub fn is_identity(&self) -> bool { *self == Self::default() }
    pub fn sanitize(&mut self) {
        fn finite(v: &mut f32, default: f32, min: f32, max: f32) { *v = if v.is_finite() { v.clamp(min,max) } else { default }; }
        finite(&mut self.rotation,0.0,-36000.0,36000.0);
        finite(&mut self.scale_x,1.0,0.01,10.0); finite(&mut self.scale_y,1.0,0.01,10.0);
        finite(&mut self.anchor_x,0.5,0.0,1.0); finite(&mut self.anchor_y,0.5,0.0,1.0);
        for crop in [&mut self.crop_left,&mut self.crop_right,&mut self.crop_top,&mut self.crop_bottom] { finite(crop,0.0,0.0,1.0); }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransitionKind {
    CrossDissolve,
    DipToBlack,
    FadeIn,
    FadeOut,
    AudioCrossfade,
    AudioFadeIn,
    AudioFadeOut }

impl TransitionKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::CrossDissolve => "Cross Dissolve",
            Self::DipToBlack => "Dip to Black",
            Self::FadeIn => "Fade In",
            Self::FadeOut => "Fade Out",
            Self::AudioCrossfade => "Crossfade",
            Self::AudioFadeIn => "Audio Fade In",
            Self::AudioFadeOut => "Audio Fade Out" }
    }

    pub fn is_audio(self) -> bool {
        matches!(
            self,
            Self::AudioCrossfade | Self::AudioFadeIn | Self::AudioFadeOut
        )
    }

    pub fn is_cut(self) -> bool {
        matches!(
            self,
            Self::CrossDissolve | Self::DipToBlack | Self::AudioCrossfade
        )
    }

    pub fn all() -> &'static [TransitionKind] {
        &[
            Self::CrossDissolve,
            Self::DipToBlack,
            Self::FadeIn,
            Self::FadeOut,
            Self::AudioCrossfade,
            Self::AudioFadeIn,
            Self::AudioFadeOut,
        ]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Anchor {
    Head(u64),
    Tail(u64),
    Cut(u64) }

impl Anchor {
    pub fn clip_id(self) -> u64 {
        match self {
            Self::Head(id) | Self::Tail(id) | Self::Cut(id) => id }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transition {
    pub id: u64,
    pub kind: TransitionKind,
    pub duration: i64,
    pub anchor: Anchor }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Track {
    #[serde(default)] pub role: audio::AudioRole,
    #[serde(default)] pub sync_lock: bool,
    pub id: u64,
    pub kind: TrackKind,
    pub name: String,
    pub muted: bool,
    #[serde(default)]
    pub solo: bool,
    pub locked: bool,
    #[serde(default = "default_track_gain")]
    pub gain: f32,
    #[serde(default)]
    pub pan: f32,
    pub clips: Vec<Clip> }

fn default_track_gain() -> f32 {
    1.0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sequence {
    #[serde(default)] pub color: crate::color::management::Pipeline,
    #[serde(default)] pub targeting: sequences::Targeting,
    #[serde(default)]pub audio:audio::AudioFinish,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub frame_rate: FrameRate,
    pub tracks: Vec<Track>,
    #[serde(default)]
    pub transitions: Vec<Transition>,
    pub playhead: i64,
    pub in_point: Option<i64>,
    pub out_point: Option<i64>,
    #[serde(default = "default_zoom")]
    pub zoom: f64,
    #[serde(default)]
    pub scroll: f64,
    #[serde(default = "default_true")]
    pub snap: bool }

fn default_zoom() -> f64 {
    8.0
}

fn default_true() -> bool {
    true
}

impl Sequence {
    /// Mute wins over Solo; any soloed audio track suppresses other audio lanes.
    /// Video mute/visibility and Source monitoring are independent of this mix.
    pub fn audio_track_enabled(&self, index: usize) -> bool {
        self.tracks.get(index).is_some_and(|track| {
            track.kind == TrackKind::Audio && !track.muted
                && (track.solo || !self.tracks.iter().any(|track| track.kind == TrackKind::Audio && track.solo))
        })
    }

    pub fn duration_frames(&self) -> i64 {
        self.tracks
            .iter()
            .flat_map(|track| track.clips.iter())
            .map(|clip| clip.end())
            .max()
            .unwrap_or(0)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    #[serde(default = "new_document_id")]
    pub document_id: String,
    #[serde(default)]
    pub review_notes: Vec<review::ReviewNote>,
    #[serde(default = "legacy_project_version")]
    pub version: u32,
    pub name: String,
    #[serde(default)]
    pub shortcut_profile: ShortcutProfile,
    pub sequence: Sequence,
    #[serde(default = "sequences::main_id")]
    pub active_sequence: String,
    #[serde(default)]
    pub sequences: std::collections::BTreeMap<String, sequences::StoredSequence>,
    pub media: Vec<Media>,
    #[serde(default)]
    pub bins: Vec<String>,
    pub next_id: u64 }

fn new_document_id() -> String { uuid::Uuid::new_v4().to_string() }

fn legacy_project_version() -> u32 {
    1
}

#[derive(Deserialize)]
struct ProjectHeader {
    #[serde(default = "legacy_project_version")]
    version: u32 }

impl Project {
    pub fn new_default(name: &str) -> Self {
        let mut project = Self {
            version: PROJECT_VERSION,
            document_id: new_document_id(),
            review_notes: Vec::new(),
            name: name.to_string(),
            shortcut_profile: ShortcutProfile::Premiere,
            active_sequence: sequences::main_id(),
            sequences: Default::default(),
            sequence: Sequence {color:Default::default(),targeting:Default::default(),audio:Default::default(),
                name: "Sequence 01".into(),
                width: 1920,
                height: 1080,
                frame_rate: FrameRate::rational(24, 1),
                tracks: Vec::new(),
                transitions: Vec::new(),
                playhead: 0,
                in_point: None,
                out_point: None,
                zoom: default_zoom(),
                scroll: 0.0,
                snap: true },
            media: Vec::new(),
            bins: Vec::new(),
            next_id: 1 };
        project.add_track(TrackKind::Video);
        project.add_track(TrackKind::Video);
        project.add_track(TrackKind::Video);
        project.add_track(TrackKind::Audio);
        project.add_track(TrackKind::Audio);
        project
    }

    pub fn alloc(&mut self) -> u64 {
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        id
    }

    pub fn add_track(&mut self, kind: TrackKind) -> usize {
        let count = self
            .sequence
            .tracks
            .iter()
            .filter(|track| track.kind == kind)
            .count()
            + 1;
        let name = match kind {
            TrackKind::Video => format!("V{count}"),
            TrackKind::Audio => format!("A{count}") };
        let track = Track {
            role:Default::default(),
            sync_lock:false,
            id: self.alloc(),
            kind,
            name,
            muted: false,
            solo: false,
            locked: false,
            gain: 1.0,
            pan: 0.0,
            clips: Vec::new() };
        self.sequence.tracks.push(track);
        self.sequence.tracks.len() - 1
    }

    pub fn add_media(&mut self, mut media: Media) -> u64 {
        if media.id == 0 {
            media.id = self.alloc();
        } else {
            self.next_id = self.next_id.max(media.id + 1);
        }
        let id = media.id;
        self.media.push(media);
        id
    }

    pub fn media(&self, id: u64) -> Option<&Media> {
        self.media.iter().find(|media| media.id == id)
    }

    pub fn relink_media(&mut self, id: u64, mut replacement: Media) -> Result<(), String> {
        let original = self
            .media(id)
            .cloned()
            .ok_or("Select media in the Project panel")?;
        for (_, sequence) in self.all_sequences() {
            for track in &sequence.tracks {
                for clip in &track.clips {
                    if clip.creative.text.is_some() || clip.creative.sequence.is_some() || clip.creative.adjustment { continue; }
                    let mut spans = Vec::new();
                    if let Some(multicam) = &clip.creative.multicam {
                        let mut boundaries = vec![0, clip.duration];
                        boundaries.extend(multicam.cuts.iter().map(|c| c.frame.saturating_sub(clip.creative.offset)).filter(|f| *f > 0 && *f < clip.duration));
                        boundaries.sort_unstable();
                        boundaries.dedup();
                        for span in boundaries.windows(2) {
                            if let Some(angle) = multicam.angle(span[0].saturating_add(clip.creative.offset)).filter(|a| a.media_id == id) {
                                spans.push((span[0], span[1] - 1, angle.offset));
                            }
                        }
                    } else if clip.media_id == id {
                        if !replacement.clip_source_is_valid(clip, sequence.frame_rate) {
                            return Err("Replacement is shorter than an existing clip's source range".into());
                        }
                        spans.push((0, clip.duration - 1, 0.));
                    }
                    if spans.is_empty() { continue; }
                    if track.kind == TrackKind::Video && !replacement.has_video && replacement.kind != MediaKind::Still {
                        return Err("Replacement has no picture for the existing video clips".into());
                    }
                    if track.kind == TrackKind::Audio && !replacement.has_audio {
                        return Err("Replacement has no sound for the existing audio clips".into());
                    }
                    if replacement.kind != MediaKind::Still {
                        for (first, last, offset) in spans {
                            let a = clip.source_time_seconds(first, sequence.frame_rate) + offset;
                            let b = clip.source_time_seconds(last, sequence.frame_rate) + offset;
                            // Existing camera gaps remain silent/black. Preserve every available
                            // source interval used by an angle, including inactive sequences.
                            let low = a.min(b).max(0.);
                            let high = a.max(b).min(original.source_duration_seconds() - 1e-9);
                            if high >= low && high >= replacement.source_duration_seconds() {
                                return Err("Replacement is shorter than an existing camera angle's source range".into());
                            }
                        }
                    }
                }
            }
        }
        replacement.id = id;
        replacement.name = original.name;
        replacement.bin = original.bin;
        replacement.source_in_mark = original.source_in_mark;
        replacement.source_out_mark = original.source_out_mark;
        replacement.source_range()?;
        let target = self
            .media
            .iter_mut()
            .find(|media| media.id == id)
            .ok_or("Media is missing")?;
        *target = replacement;
        Ok(())
    }

    pub fn create_bin(&mut self, name: &str) -> Result<String, String> {
        let name = normalize_bin_name(name)?;
        if name.is_empty() {
            return Err("Enter a name for the bin".into());
        }
        if self
            .bins
            .iter()
            .any(|bin| bin.to_lowercase() == name.to_lowercase())
        {
            return Err("A bin with this name already exists".into());
        }
        self.bins.push(name.clone());
        self.bins.sort_by_key(|bin| bin.to_lowercase());
        Ok(name)
    }

    pub fn move_media_to_bin(&mut self, media_id: u64, bin: &str) -> Result<(), String> {
        let requested = normalize_bin_name(bin)?;
        let target = if requested.is_empty() {
            String::new()
        } else {
            self.bins
                .iter()
                .find(|bin| bin.to_lowercase() == requested.to_lowercase())
                .cloned()
                .ok_or("The destination bin is missing")?
        };
        let media = self
            .media
            .iter_mut()
            .find(|media| media.id == media_id)
            .ok_or("Select media in the project bin")?;
        media.bin = target;
        Ok(())
    }

    pub fn clip(&self, id: u64) -> Option<&Clip> {
        self.sequence
            .tracks
            .iter()
            .flat_map(|track| track.clips.iter())
            .find(|clip| clip.id == id)
    }

    pub fn clip_mut(&mut self, id: u64) -> Option<&mut Clip> {
        self.sequence
            .tracks
            .iter_mut()
            .flat_map(|track| track.clips.iter_mut())
            .find(|clip| clip.id == id)
    }

    pub fn track_index_of_clip(&self, id: u64) -> Option<usize> {
        self.sequence
            .tracks
            .iter()
            .position(|track| track.clips.iter().any(|clip| clip.id == id))
    }

    pub fn transition(&self, id: u64) -> Option<&Transition> {
        self.sequence.transitions.iter().find(|item| item.id == id)
    }

    pub fn set_resolution(&mut self, width: u32, height: u32) {
        self.sequence.width = width.max(16);
        self.sequence.height = height.max(16);
    }

    pub fn set_frame_rate(&mut self, rate: FrameRate) {
        if rate.num == 0 || rate.den == 0 || self.sequence.frame_rate == rate {
            return;
        }
        let old = self.sequence.frame_rate;
        let map = |frame: i64| rate.seconds_to_frame(old.frame_to_seconds(frame));
        for track in &mut self.sequence.tracks {
            for clip in &mut track.clips {
                let end = map(clip.end());
                let source_start = clip.source_start_seconds(old);
                clip.timeline_start = map(clip.timeline_start).max(0);
                clip.source_in = rate.seconds_to_frame(source_start).max(0);
                clip.source_time_offset = source_start - rate.frame_to_seconds(clip.source_in);
                clip.duration = (end - clip.timeline_start).max(1);
                clip.creative.offset=map(clip.creative.offset);
                for keys in clip.creative.animation.values_mut(){for key in keys.iter_mut(){key.frame=map(key.frame);}keys.dedup_by(|a,b|a.frame==b.frame);}
                if let Some(multicam) = &mut clip.creative.multicam {
                    for cut in &mut multicam.cuts { cut.frame = map(cut.frame); }
                    multicam.cuts.dedup_by(|a, b| { if a.frame == b.frame { b.angle = a.angle; true } else { false } });
                }
                if let Some(media) = self.media.iter().find(|media| media.id == clip.media_id) {
                    if media.kind != MediaKind::Still && !clip.creative.freeze {
                        let remaining = media.source_duration_seconds() - source_start;
                        let capacity = if clip.creative.speed>0.{(remaining / clip.creative.speed * rate.fps() - 1e-9).ceil().max(1.)as i64}else{(source_start / clip.creative.speed.abs() * rate.fps()).floor()as i64+1};
                        clip.duration = clip.duration.min(capacity);
                    }
                }
            }
        }
        for note in &mut self.review_notes { note.frame = map(note.frame).max(0); }
        self.sequence.playhead = map(self.sequence.playhead).max(0);
        self.sequence.in_point = self.sequence.in_point.map(|frame| map(frame).max(0));
        self.sequence.out_point = self.sequence.out_point.map(|frame| map(frame).max(0));
        for transition in &mut self.sequence.transitions {
            transition.duration = map(transition.duration).max(2);
        }
        self.sequence.frame_rate = rate;
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        let json = self.to_json()?;
        let (mut staged, mut file) = StagedProjectSave::create(path)?;
        let written = file
            .write_all(json.as_bytes())
            .and_then(|()| file.flush())
            .and_then(|()| file.sync_all());
        // Close before publication or cleanup, including a failed write on Windows.
        drop(file);
        written.map_err(|error| format!("Could not write project save: {error}"))?;
        // Same-directory rename replaces the destination without first removing it.
        // This also follows the completed-export publication path on Windows.
        fs::rename(&staged.path, path)
            .map_err(|error| format!("Could not publish project save: {error}"))?;
        staged.published = true;
        Ok(())
    }

    pub fn load_from(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path).map_err(|err| err.to_string())?;
        let mut project=Self::from_json(&text)?;
        if let Some(parent)=path.parent(){for m in &mut project.media{if m.path.is_relative(){m.path=parent.join(&m.path);}}}
        Ok(project)
    }

    /// Check the version before decoding the body, including unknown future
    /// schemas whose contents do not match the current Project structure.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let header: ProjectHeader = serde_json::from_str(text).map_err(|err| err.to_string())?;
        Self::validate_version(header.version)?;
        let mut project: Project = serde_json::from_str(text).map_err(|err| err.to_string())?;
        project.migrate()?;
        project.ensure_integrity();
        Ok(project)
    }

    /// Saved copies always carry the current schema. Serializing a legacy
    /// in-memory project does not mutate the user's edit/undo snapshot.
    pub fn to_json(&self) -> Result<String, String> {
        let mut saved = self.clone();
        saved.migrate()?;
        serde_json::to_string_pretty(&saved).map_err(|err| err.to_string())
    }

    fn validate_version(version: u32) -> Result<(), String> {
        match version {
            1..=PROJECT_VERSION => Ok(()),
            0 => Err("Unsupported project version 0".into()),
            _ => Err(format!("This project uses version {version}; this editor supports up to version {PROJECT_VERSION}. Open it with a newer Quaternio version.")) }
    }

    pub fn migrate(&mut self) -> Result<(), String> {
        Self::validate_version(self.version)?;
        review::validate_notes(&self.review_notes)?;
        self.validate_sequences()?;
        self.sequence.audio.validate()?;
        if uuid::Uuid::parse_str(&self.document_id).is_err() { return Err("Invalid project document identity".into()); }
        // v1 lacked Source marks and precise offsets; serde defaults retain its
        // full-source insertion and integer sequence-frame source origins.
        // v1/v2 lacked track Solo; serde defaults keep every audio track audible.
        // v1-v3 lack extended motion; identity defaults retain their pictures.
        for clip in self.sequence.tracks.iter().flat_map(|t|&t.clips){clip.creative.validate()?;}
        self.version = PROJECT_VERSION;
        Ok(())
    }

    pub fn ensure_integrity(&mut self) {
        let mut max_id = 0u64;
        for media in &self.media {
            max_id = max_id.max(media.id);
        }
        for media in &mut self.media {
            if media.source_range().is_err() {
                media.clear_source_marks();
            }
        }
        for track in &self.sequence.tracks {
            max_id = max_id.max(track.id);
            for clip in &track.clips {
                max_id = max_id.max(clip.id);
                max_id = max_id.max(clip.link.unwrap_or(0));
            }
        }
        for transition in &self.sequence.transitions {
            max_id = max_id.max(transition.id);
        }
        for note in &self.review_notes { max_id = max_id.max(note.id); }
        for slot in self.sequences.values() {
            for t in &slot.sequence.tracks { max_id=max_id.max(t.id); for c in &t.clips {max_id=max_id.max(c.id).max(c.link.unwrap_or(0));} }
            for t in &slot.sequence.transitions {max_id=max_id.max(t.id);}
            for n in &slot.notes {max_id=max_id.max(n.id);}
        }
        self.next_id = self.next_id.max(max_id.saturating_add(1)).max(1);
        if !self
            .sequence
            .tracks
            .iter()
            .any(|track| track.kind == TrackKind::Video)
        {
            self.add_track(TrackKind::Video);
        }
        if !self
            .sequence
            .tracks
            .iter()
            .any(|track| track.kind == TrackKind::Audio)
        {
            self.add_track(TrackKind::Audio);
        }
        for track in &mut self.sequence.tracks {
            if track.kind != TrackKind::Audio {
                track.solo = false;
            }
            for clip in &mut track.clips {
                clip.transform.sanitize();
                if !clip.source_time_offset.is_finite() {
                    clip.source_time_offset = 0.0;
                }
            }
            track.clips.retain(|clip| clip.duration >= 1);
            track
                .clips
                .sort_by_key(|clip| (clip.timeline_start, clip.id));
        }
        let live: Vec<u64> = self
            .sequence
            .tracks
            .iter()
            .flat_map(|track| track.clips.iter().map(|clip| clip.id))
            .collect();
        self.sequence.transitions.retain(|transition| {
            live.contains(&transition.anchor.clip_id()) && transition.duration >= 1
        });
        if self.sequence.zoom < 0.25 {
            self.sequence.zoom = 0.25;
        }
        if self.sequence.width < 16 {
            self.sequence.width = 16;
        }
        if self.sequence.height < 16 {
            self.sequence.height = 16;
        }
    }
}

