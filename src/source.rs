use std::collections::HashMap;
use std::time::Instant;

use crate::project::FrameRate;

/// Source transport is independent of sequence history and its playhead.
/// Marks are project data; cursors and playback clocks are session state.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct SourceSession {
    pub version: u32,
    pub media_id: Option<u64>,
    pub cursors: HashMap<u64, i64> }

pub struct SourceTransport {
    pub media_id: Option<u64>,
    pub rate: FrameRate,
    pub total_frames: i64,
    pub frame: i64,
    pub playing: bool,
    pub waiting_audio: bool,
    pub waiting_picture: bool,
    pub speed: f64,
    anchor_frame: i64,
    anchor_at: Instant,
    cursors: HashMap<u64, i64>,
    pub scrub_origin: Option<i64> }

impl Default for SourceTransport {
    fn default() -> Self {
        Self {
            media_id: None,
            rate: FrameRate::default(),
            total_frames: 1,
            frame: 0,
            playing: false,
            waiting_audio: false,
            waiting_picture: false,
            speed: 0.0,
            anchor_frame: 0,
            anchor_at: Instant::now(),
            cursors: HashMap::new(),
            scrub_origin: None }
    }
}

impl SourceTransport {
    pub fn remote_picture(&mut self, frame: i64) {
        self.frame = frame.clamp(0, self.total_frames.max(1) - 1);
        self.anchor_frame = self.frame;
        self.anchor_at = Instant::now();
        self.waiting_picture = false;
        self.waiting_audio = false;
    }
    pub fn session_snapshot(&self) -> SourceSession {
        let mut cursors = self.cursors.clone();
        if let Some(id) = self.media_id {
            cursors.insert(id, self.scrub_origin.unwrap_or(self.frame));
        }
        SourceSession {
            version: 1,
            media_id: self.media_id,
            cursors }
    }

    pub fn from_session(
        session: SourceSession,
        project: &crate::project::Project,
    ) -> Result<Self, String> {
        if session.version != 1 || session.cursors.len() > 100_000 {
            return Err("Invalid or unsupported source recovery session".into());
        }
        let mut source = Self::default();
        for (id, frame) in session.cursors {
            if let Some(media) = project.media(id) {
                source
                    .cursors
                    .insert(id, frame.clamp(0, media.source_frames().max(1) - 1));
            }
        }
        if let Some(media) = session.media_id.and_then(|id| project.media(id)) {
            source.select(Some(media.id), media.source_rate(), media.source_frames());
        }
        Ok(source)
    }

    pub fn select(&mut self, id: Option<u64>, rate: FrameRate, frames: i64) -> bool {
        let frames = frames.max(1);
        if self.media_id == id && self.rate == rate && self.total_frames == frames {
            return false;
        }
        if let Some(previous) = self.media_id {
            self.cursors.insert(previous, self.frame);
        }
        self.stop();
        self.scrub_origin = None;
        self.media_id = id;
        self.rate = rate;
        self.total_frames = frames;
        self.frame = id
            .and_then(|id| self.cursors.get(&id).copied())
            .unwrap_or(0)
            .clamp(0, frames - 1);
        true
    }

    pub fn seek(&mut self, frame: i64) {
        self.stop();
        self.frame = frame.clamp(0, self.total_frames - 1);
    }

    pub fn step(&mut self, delta: i64) {
        self.seek(self.frame.saturating_add(delta));
    }

    pub fn stop(&mut self) {
        self.playing = false;
        self.waiting_audio = false;
        self.waiting_picture = false;
        self.speed = 0.0;
    }

    pub fn set_speed(&mut self, speed: f64) {
        let speed = if speed.is_finite() {
            speed.clamp(-8.0, 8.0)
        } else {
            0.0
        };
        self.anchor_frame = self.frame;
        self.anchor_at = Instant::now();
        self.speed = speed;
        self.playing = speed != 0.0 && self.media_id.is_some();
        self.waiting_audio = self.playing && (speed - 1.0).abs() < 0.01;
        self.waiting_picture = false;
    }

    pub fn picture_ready(&mut self) {
        self.waiting_picture = false;
        self.anchor_at = Instant::now();
    }

    pub fn audio_ready(&mut self) {
        self.waiting_audio = false;
        self.anchor_at = Instant::now();
    }

    pub fn next_shuttle_speed(&self, direction: f64) -> f64 {
        if self.playing && self.speed.signum() == direction.signum() {
            (self.speed * 2.0).clamp(-8.0, 8.0)
        } else {
            direction.signum()
        }
    }

    fn frame_after(&self, seconds: f64) -> (i64, bool) {
        let frames = seconds.max(0.0) * self.rate.fps() * self.speed;
        let delta = (frames + frames.signum() * 1e-9).trunc() as i64;
        let frame = self.anchor_frame.saturating_add(delta);
        let end = self.total_frames - 1;
        let stopped =
            (self.speed > 0.0 && frame >= self.total_frames) || (self.speed < 0.0 && frame < 0);
        (frame.clamp(0, end), stopped)
    }

    pub fn advance(&mut self) -> bool {
        if !self.playing || self.waiting_audio || self.waiting_picture {
            return false;
        }
        let (frame, stop) = self.frame_after(self.anchor_at.elapsed().as_secs_f64());
        let changed = self.frame != frame;
        self.frame = frame;
        if stop {
            self.stop();
        }
        changed || stop
    }
}

