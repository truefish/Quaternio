//! One-way import of foreign NLE timelines into a Quaternio [`Project`].

mod fcpxml;

use std::path::{Path, PathBuf};

use crate::project::Project;

pub use fcpxml::import_fcpxml;

#[derive(Clone, Debug, Default)]
pub struct ImportReport {
    pub media_count: usize,
    pub clip_count: usize,
    pub offline_count: usize,
    pub skipped: Vec<String>,
}

impl ImportReport {
    pub fn summary_line(&self) -> String {
        let mut parts = vec![
            format!("{} media", self.media_count),
            format!("{} clips", self.clip_count),
        ];
        if self.offline_count > 0 {
            parts.push(format!("{} offline", self.offline_count));
        }
        if !self.skipped.is_empty() {
            parts.push(format!("{} skipped", self.skipped.len()));
        }
        parts.join(", ")
    }

    pub fn detail_text(&self) -> String {
        let mut lines = vec![
            format!("Media items: {}", self.media_count),
            format!("Timeline clips: {}", self.clip_count),
            format!("Offline media: {}", self.offline_count),
        ];
        if self.skipped.is_empty() {
            lines.push("Skipped items: none".into());
        } else {
            lines.push(format!("Skipped items ({}):", self.skipped.len()));
            for item in self.skipped.iter().take(40) {
                lines.push(format!("  • {item}"));
            }
            if self.skipped.len() > 40 {
                lines.push(format!("  …and {} more", self.skipped.len() - 40));
            }
        }
        lines.join("\n")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportMode {
    /// Replace the open project with the imported timeline.
    NewProject,
    /// Merge media and place clips into the current sequence at the playhead.
    IntoCurrent,
}

/// Detect format from path/extension and import.
pub fn import_timeline(path: &Path) -> Result<(Project, ImportReport), String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "fcpxml" | "xml" => import_fcpxml(path),
        "prproj" => Err(
            "Premiere .prproj is not imported directly yet. In Premiere: File → Export → Final Cut Pro XML, then import that .fcpxml / .xml here."
                .into(),
        ),
        "drp" => Err(
            "DaVinci Resolve .drp packages are not imported directly yet. In Resolve: File → Export → Timeline → Final Cut Pro XML, then import that file here."
                .into(),
        ),
        _ => Err(format!(
            "Unsupported timeline format ({}). Use an FCPXML export from Premiere or DaVinci Resolve.",
            path.display()
        )),
    }
}

/// Merge an imported project into `target` at `playhead` (sequence frames).
pub fn merge_into_current(
    target: &mut Project,
    imported: Project,
    playhead: i64,
    report: &mut ImportReport,
) -> Result<(), String> {
    let rate = target.sequence.frame_rate;
    let mut id_map = std::collections::HashMap::<u64, u64>::new();

    for media in imported.media {
        let old_id = media.id;
        if let Some(existing) = target
            .media
            .iter()
            .find(|item| same_media_path(&item.path, &media.path))
        {
            id_map.insert(old_id, existing.id);
            continue;
        }
        let mut media = media;
        media.id = 0;
        let new_id = target.add_media(media);
        id_map.insert(old_id, new_id);
    }

    ensure_track_capacity(target, &imported.sequence.tracks);

    let video_targets: Vec<usize> = target
        .sequence
        .tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| t.kind == crate::project::TrackKind::Video)
        .map(|(i, _)| i)
        .collect();
    let audio_targets: Vec<usize> = target
        .sequence
        .tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| t.kind == crate::project::TrackKind::Audio)
        .map(|(i, _)| i)
        .collect();

    let mut v_src = 0usize;
    let mut a_src = 0usize;
    for track in &imported.sequence.tracks {
        let dest_index = match track.kind {
            crate::project::TrackKind::Video => {
                let idx = video_targets.get(v_src).copied().ok_or("Missing video track")?;
                v_src += 1;
                idx
            }
            crate::project::TrackKind::Audio => {
                let idx = audio_targets.get(a_src).copied().ok_or("Missing audio track")?;
                a_src += 1;
                idx
            }
        };
        if target.sequence.tracks[dest_index].locked {
            report
                .skipped
                .push(format!("Track {} is locked; clips not merged", track.name));
            continue;
        }
        for clip in &track.clips {
            let Some(&media_id) = id_map.get(&clip.media_id) else {
                report
                    .skipped
                    .push(format!("Clip '{}' missing media mapping", clip.name));
                continue;
            };
            let mut clip = clip.clone();
            clip.id = target.alloc();
            clip.media_id = media_id;
            clip.link = None;
            let start = playhead.saturating_add(
                rate
                    .seconds_to_frame(
                        imported
                            .sequence
                            .frame_rate
                            .frame_to_seconds(clip.timeline_start),
                    )
                    .max(0),
            );
            let end = playhead.saturating_add(
                rate
                    .seconds_to_frame(
                        imported
                            .sequence
                            .frame_rate
                            .frame_to_seconds(clip.end()),
                    )
                    .max(0),
            );
            clip.timeline_start = start;
            clip.duration = (end - start).max(1);
            // Remap source_in into target sequence rate when rates differ.
            if imported.sequence.frame_rate != rate {
                let source_start = clip.source_start_seconds(imported.sequence.frame_rate);
                clip.source_in = rate.seconds_to_frame(source_start).max(0);
                clip.source_time_offset =
                    source_start - rate.frame_to_seconds(clip.source_in);
            }
            target.sequence.tracks[dest_index].clips.push(clip);
            report.clip_count = report.clip_count.saturating_add(1);
        }
    }
    target.sequence.playhead = playhead.max(0);
    target.ensure_integrity();
    Ok(())
}

fn same_media_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn ensure_track_capacity(target: &mut Project, source_tracks: &[crate::project::Track]) {
    let need_v = source_tracks
        .iter()
        .filter(|t| t.kind == crate::project::TrackKind::Video)
        .count();
    let need_a = source_tracks
        .iter()
        .filter(|t| t.kind == crate::project::TrackKind::Audio)
        .count();
    let have_v = target
        .sequence
        .tracks
        .iter()
        .filter(|t| t.kind == crate::project::TrackKind::Video)
        .count();
    let have_a = target
        .sequence
        .tracks
        .iter()
        .filter(|t| t.kind == crate::project::TrackKind::Audio)
        .count();
    for _ in have_v..need_v {
        target.add_track(crate::project::TrackKind::Video);
    }
    for _ in have_a..need_a {
        target.add_track(crate::project::TrackKind::Audio);
    }
}

pub fn path_looks_like_timeline(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("fcpxml" | "xml" | "prproj" | "drp")
    )
}

pub fn suggested_filters() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        (
            "Premiere / Resolve / FCPXML",
            vec!["prproj", "drp", "fcpxml", "xml"],
        ),
        ("Premiere project", vec!["prproj"]),
        ("DaVinci Resolve project", vec!["drp"]),
        ("FCPXML / XML", vec!["fcpxml", "xml"]),
    ]
}

pub fn resolve_media_path(raw: &str, xml_dir: &Path) -> PathBuf {
    let trimmed = raw.trim();
    if let Some(path) = file_url_to_path(trimmed) {
        return path;
    }
    let candidate = PathBuf::from(trimmed);
    if candidate.is_absolute() {
        return candidate;
    }
    xml_dir.join(candidate)
}

fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    // file:///C:/x or file://localhost/C:/x or file:///Users/...
    let path = rest
        .strip_prefix("localhost")
        .unwrap_or(rest)
        .trim_start_matches('/');
    #[cfg(windows)]
    {
        let decoded = percent_decode(path);
        // /C:/... or C:/...
        let decoded = decoded.trim_start_matches('/');
        if decoded.len() >= 2 && decoded.as_bytes()[1] == b':' {
            return Some(PathBuf::from(decoded));
        }
        Some(PathBuf::from(format!("/{decoded}")))
    }
    #[cfg(not(windows))]
    {
        Some(PathBuf::from(format!("/{}", percent_decode(path))))
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
