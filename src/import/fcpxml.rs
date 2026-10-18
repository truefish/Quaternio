//! Final Cut Pro XML (FCPXML) → Quaternio [`Project`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use roxmltree::{Document, Node};

use crate::color::Grade;
use crate::playback::probe_path;
use crate::project::{
    Clip, FrameRate, Media, MediaKind, Project, TrackKind, PROJECT_VERSION,
};

use super::{resolve_media_path, ImportReport};

struct AssetInfo {
    name: String,
    path: PathBuf,
    duration_secs: f64,
    has_video: bool,
    has_audio: bool,
    width: u32,
    height: u32,
    source_fps: f64,
}

struct PlacedClip {
    asset_id: String,
    name: String,
    timeline_start: f64,
    duration: f64,
    source_start: f64,
    lane: i32,
    wants_video: bool,
    wants_audio: bool,
}

pub fn import_fcpxml(path: &Path) -> Result<(Project, ImportReport), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("Could not read {}: {e}", path.display()))?;
    let xml_dir = path.parent().unwrap_or_else(|| Path::new("."));
    parse_fcpxml(&text, xml_dir, path)
}

fn parse_fcpxml(text: &str, xml_dir: &Path, source_path: &Path) -> Result<(Project, ImportReport), String> {
    let doc = Document::parse(text).map_err(|e| format!("Invalid FCPXML: {e}"))?;
    let root = doc.root_element();
    if !root.has_tag_name("fcpxml") {
        return Err("Not an FCPXML document (missing <fcpxml> root)".into());
    }

    let mut report = ImportReport::default();
    let assets = collect_assets(root, xml_dir, &mut report);
    if assets.is_empty() {
        return Err("FCPXML has no media assets to import".into());
    }

    let (seq_name, format_id, sequence_node) = find_sequence(root)
        .ok_or("FCPXML has no <sequence> / <project> timeline to import")?;

    let (width, height, rate) = resolve_format(root, format_id.as_deref())
        .unwrap_or((1920, 1080, FrameRate::rational(24, 1)));

    let mut placed = Vec::new();
    if let Some(spine) = sequence_node
        .children()
        .find(|n| n.is_element() && n.has_tag_name("spine"))
    {
        walk_spine(spine, 0.0, 0, &mut placed, &mut report);
    } else {
        // Some exporters nest spine deeper.
        for spine in sequence_node.descendants().filter(|n| n.has_tag_name("spine")) {
            walk_spine(spine, 0.0, 0, &mut placed, &mut report);
            break;
        }
    }

    if placed.is_empty() {
        return Err("FCPXML sequence has no clips on the spine".into());
    }

    let mut project = Project::new_default(&seq_name);
    project.version = PROJECT_VERSION;
    project.sequence.name = seq_name.clone();
    project.set_resolution(width.max(16), height.max(16));
    project.set_frame_rate(rate);
    project.sequence.tracks.clear();

    // Probe / stub media
    let mut asset_to_media: HashMap<String, u64> = HashMap::new();
    for (id, asset) in &assets {
        let media = materialize_media(asset, &mut report);
        if !media.path.is_file() {
            report.offline_count += 1;
        }
        let media_id = project.add_media(media);
        asset_to_media.insert(id.clone(), media_id);
        report.media_count += 1;
    }

    // Partition lanes → tracks (video lanes + audio)
    let mut video_lanes: Vec<i32> = placed
        .iter()
        .filter(|p| p.wants_video)
        .map(|p| p.lane)
        .collect();
    video_lanes.sort_unstable();
    video_lanes.dedup();
    if video_lanes.is_empty() {
        video_lanes.push(0);
    }

    let video_track_for_lane: HashMap<i32, usize> = video_lanes
        .iter()
        .map(|&lane| {
            let idx = project.add_track(TrackKind::Video);
            (lane, idx)
        })
        .collect();

    let needs_audio = placed.iter().any(|p| p.wants_audio);
    let audio_track = if needs_audio {
        Some(project.add_track(TrackKind::Audio))
    } else {
        // Keep at least one audio track like new_default.
        project.add_track(TrackKind::Audio);
        None
    };
    // Ensure a couple of spare tracks similar to defaults for editing comfort.
    while project
        .sequence
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Video)
        .count()
        < 3
    {
        project.add_track(TrackKind::Video);
    }
    while project
        .sequence
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Audio)
        .count()
        < 2
    {
        project.add_track(TrackKind::Audio);
    }

    let mut audio_start_used: HashMap<(u64, i64), ()> = HashMap::new();

    for item in &placed {
        let Some(&media_id) = asset_to_media.get(&item.asset_id) else {
            report
                .skipped
                .push(format!("Clip '{}' references unknown asset", item.name));
            continue;
        };
        let timeline_start = rate.seconds_to_frame(item.timeline_start).max(0);
        let timeline_end = rate
            .seconds_to_frame(item.timeline_start + item.duration)
            .max(timeline_start + 1);
        let duration = (timeline_end - timeline_start).max(1);
        let source_in = rate.seconds_to_frame(item.source_start).max(0);
        let source_time_offset =
            item.source_start - rate.frame_to_seconds(source_in);

        if item.wants_video {
            if let Some(&track_index) = video_track_for_lane.get(&item.lane) {
                let id = project.alloc();
                project.sequence.tracks[track_index].clips.push(Clip {
                    transform: Default::default(),
                    id,
                    media_id,
                    name: item.name.clone(),
                    timeline_start,
                    duration,
                    source_in,
                    source_time_offset,
                    link: None,
                    volume: 1.0,
                    pos_x: 0.0,
                    pos_y: 0.0,
                    scale: 1.0,
                    opacity: 1.0,
                    grade: Grade::default(),
                });
                report.clip_count += 1;
            }
        }

        if item.wants_audio {
            if let Some(track_index) = audio_track {
                let key = (media_id, timeline_start);
                if audio_start_used.insert(key, ()).is_none() {
                    let id = project.alloc();
                    project.sequence.tracks[track_index].clips.push(Clip {
                        transform: Default::default(),
                        id,
                        media_id,
                        name: item.name.clone(),
                        timeline_start,
                        duration,
                        source_in,
                        source_time_offset,
                        link: None,
                        volume: 1.0,
                        pos_x: 0.0,
                        pos_y: 0.0,
                        scale: 1.0,
                        opacity: 1.0,
                        grade: Grade::default(),
                    });
                    report.clip_count += 1;
                }
            }
        }
    }

    project.sequence.playhead = 0;
    project.ensure_integrity();

    if report.clip_count == 0 {
        return Err(format!(
            "Imported {} but no timeline clips could be mapped",
            source_path.display()
        ));
    }

    Ok((project, report))
}

fn materialize_media(asset: &AssetInfo, report: &mut ImportReport) -> Media {
    if asset.path.is_file() {
        match probe_path(&asset.path) {
            Ok(mut media) => {
                if media.name.is_empty() {
                    media.name = asset.name.clone();
                }
                return media;
            }
            Err(error) => {
                report
                    .skipped
                    .push(format!("Probe failed for {}: {error}", asset.path.display()));
            }
        }
    }
    let kind = if asset.has_video {
        MediaKind::Video
    } else if asset.has_audio {
        MediaKind::Audio
    } else {
        MediaKind::Video
    };
    Media {
        id: 0,
        name: asset.name.clone(),
        bin: String::new(),
        path: asset.path.clone(),
        kind,
        duration_secs: asset.duration_secs.max(0.0),
        width: asset.width,
        height: asset.height,
        has_video: asset.has_video || kind == MediaKind::Video,
        has_audio: asset.has_audio || kind == MediaKind::Audio,
        source_fps: if asset.source_fps > 0.0 {
            asset.source_fps
        } else {
            24.0
        },
        source_in_mark: None,
        source_out_mark: None,
    }
}

fn collect_assets(
    root: Node<'_, '_>,
    xml_dir: &Path,
    report: &mut ImportReport,
) -> HashMap<String, AssetInfo> {
    let mut assets = HashMap::new();
    for node in root.descendants().filter(|n| n.has_tag_name("asset")) {
        let id = match node.attribute("id") {
            Some(id) => id.to_string(),
            None => continue,
        };
        let name = node
            .attribute("name")
            .unwrap_or("Media")
            .to_string();
        let src = node
            .attribute("src")
            .map(str::to_string)
            .or_else(|| {
                node.children()
                    .find(|c| c.has_tag_name("media-rep"))
                    .and_then(|c| c.attribute("src").map(str::to_string))
            });
        let Some(src) = src else {
            report
                .skipped
                .push(format!("Asset '{name}' has no media path"));
            continue;
        };
        let path = resolve_media_path(&src, xml_dir);
        let duration_secs = node
            .attribute("duration")
            .and_then(parse_time)
            .unwrap_or(0.0);
        let has_video = attr_bool(node, "hasVideo", true);
        let has_audio = attr_bool(node, "hasAudio", false);
        assets.insert(
            id,
            AssetInfo {
                name,
                path,
                duration_secs,
                has_video,
                has_audio,
                width: attr_u32(node, "width", 0),
                height: attr_u32(node, "height", 0),
                source_fps: 0.0,
            },
        );
    }
    assets
}

fn find_sequence<'a, 'input>(
    root: Node<'a, 'input>,
) -> Option<(String, Option<String>, Node<'a, 'input>)> {
    if let Some(project) = root.descendants().find(|n| n.has_tag_name("project")) {
        let name = project
            .attribute("name")
            .unwrap_or("Imported Timeline")
            .to_string();
        if let Some(sequence) = project
            .children()
            .find(|n| n.is_element() && n.has_tag_name("sequence"))
            .or_else(|| {
                project
                    .descendants()
                    .find(|n| n.has_tag_name("sequence"))
            })
        {
            let format = sequence.attribute("format").map(str::to_string);
            return Some((name, format, sequence));
        }
    }
    root.descendants()
        .find(|n| n.has_tag_name("sequence"))
        .map(|sequence| {
            let name = sequence
                .attribute("name")
                .unwrap_or("Imported Timeline")
                .to_string();
            let format = sequence.attribute("format").map(str::to_string);
            (name, format, sequence)
        })
}

fn resolve_format(
    root: Node<'_, '_>,
    format_id: Option<&str>,
) -> Option<(u32, u32, FrameRate)> {
    let format = root.descendants().find(|n| {
        n.has_tag_name("format")
            && format_id
                .map(|id| n.attribute("id") == Some(id))
                .unwrap_or(true)
    })?;
    let width = attr_u32(format, "width", 1920).max(16);
    let height = attr_u32(format, "height", 1080).max(16);
    let rate = format
        .attribute("frameDuration")
        .and_then(frame_duration_to_rate)
        .unwrap_or_else(FrameRate::default);
    Some((width, height, rate))
}

fn frame_duration_to_rate(text: &str) -> Option<FrameRate> {
    let seconds = parse_time(text)?;
    if seconds <= 0.0 {
        return None;
    }
    let fps = 1.0 / seconds;
    for preset in crate::project::frame_rate_presets() {
        if (preset.fps() - fps).abs() < 0.02 {
            return Some(*preset);
        }
    }
    let num = (fps * 1001.0).round();
    if (num / 1001.0 - fps).abs() < 0.01 && num >= 1.0 {
        return Some(FrameRate::rational(num as u32, 1001));
    }
    let num = (fps * 1000.0).round().max(1.0) as u32;
    Some(FrameRate::rational(num, 1000))
}

fn walk_spine(
    spine: Node<'_, '_>,
    mut cursor: f64,
    parent_lane: i32,
    out: &mut Vec<PlacedClip>,
    report: &mut ImportReport,
) {
    for child in spine.children().filter(|n| n.is_element()) {
        match child.tag_name().name() {
            "gap" => {
                let duration = child.attribute("duration").and_then(parse_time).unwrap_or(0.0);
                let offset = child
                    .attribute("offset")
                    .and_then(parse_time)
                    .unwrap_or(cursor);
                cursor = offset + duration;
            }
            "asset-clip" | "video" | "audio" => {
                let lane = child
                    .attribute("lane")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(parent_lane);
                if let Some(placed) = read_clip(child, cursor, lane, report) {
                    cursor = placed.timeline_start + placed.duration;
                    // Nested connected clips on other lanes.
                    for nested in child.children().filter(|n| n.is_element()) {
                        let tag = nested.tag_name().name();
                        if matches!(tag, "asset-clip" | "video" | "audio" | "spine") {
                            if tag == "spine" {
                                walk_spine(nested, placed.timeline_start, lane, out, report);
                            } else {
                                let nested_lane = nested
                                    .attribute("lane")
                                    .and_then(|v| v.parse().ok())
                                    .unwrap_or(lane);
                                if let Some(item) =
                                    read_clip(nested, placed.timeline_start, nested_lane, report)
                                {
                                    out.push(item);
                                }
                            }
                        } else if matches!(
                            tag,
                            "filter-video" | "filter-audio" | "title" | "caption"
                        ) {
                            report.skipped.push(format!(
                                "Skipped {} on '{}'",
                                tag,
                                placed.name
                            ));
                        }
                    }
                    out.push(placed);
                }
            }
            "ref-clip" | "sync-clip" | "mc-clip" | "audition-clip" => {
                let name = child.attribute("name").unwrap_or(child.tag_name().name());
                report
                    .skipped
                    .push(format!("Unsupported clip type <{}> ('{name}')", child.tag_name().name()));
                let duration = child.attribute("duration").and_then(parse_time).unwrap_or(0.0);
                let offset = child
                    .attribute("offset")
                    .and_then(parse_time)
                    .unwrap_or(cursor);
                cursor = offset + duration;
            }
            "transition" => {
                report.skipped.push("Skipped FCPXML transition".into());
                let duration = child.attribute("duration").and_then(parse_time).unwrap_or(0.0);
                cursor += duration * 0.5;
            }
            other => {
                if !matches!(other, "marker" | "keyword" | "note" | "rating") {
                    report.skipped.push(format!("Skipped <{other}> on spine"));
                }
            }
        }
    }
}

fn read_clip(
    node: Node<'_, '_>,
    default_offset: f64,
    lane: i32,
    report: &mut ImportReport,
) -> Option<PlacedClip> {
    let asset_id = node.attribute("ref")?.to_string();
    let name = node
        .attribute("name")
        .unwrap_or("Clip")
        .to_string();
    let timeline_start = node
        .attribute("offset")
        .and_then(parse_time)
        .unwrap_or(default_offset);
    let duration = node.attribute("duration").and_then(parse_time)?;
    if duration <= 0.0 {
        report
            .skipped
            .push(format!("Clip '{name}' has zero duration"));
        return None;
    }
    let source_start = node
        .attribute("start")
        .and_then(parse_time)
        .unwrap_or(0.0);
    let tag = node.tag_name().name();
    let (wants_video, wants_audio) = match tag {
        "audio" => (false, true),
        "video" => (true, false),
        _ => {
            // asset-clip usually carries picture + sound together.
            let role = node.attribute("audioRole").or_else(|| node.attribute("role"));
            let role_is_audio = role.is_some_and(|r| {
                let r = r.to_ascii_lowercase();
                r.contains("dialogue") || r.contains("music") || r.contains("effects") || r.starts_with("aud")
            });
            let has_audio_child = node.children().any(|c| c.has_tag_name("audio"));
            (true, role_is_audio || has_audio_child || role.is_none())
        }
    };
    Some(PlacedClip {
        asset_id,
        name,
        timeline_start,
        duration,
        source_start,
        lane,
        wants_video,
        wants_audio,
    })
}

fn parse_time(text: &str) -> Option<f64> {
    let text = text.trim().strip_suffix('s').unwrap_or(text.trim());
    if let Some((num, den)) = text.split_once('/') {
        let num: f64 = num.parse().ok()?;
        let den: f64 = den.parse().ok()?;
        if den == 0.0 {
            None
        } else {
            Some(num / den)
        }
    } else {
        text.parse().ok()
    }
}

fn attr_bool(node: Node<'_, '_>, name: &str, default: bool) -> bool {
    match node.attribute(name) {
        Some("1") | Some("true") | Some("TRUE") | Some("yes") => true,
        Some("0") | Some("false") | Some("FALSE") | Some("no") => false,
        _ => default,
    }
}

fn attr_u32(node: Node<'_, '_>, name: &str, default: u32) -> u32 {
    node.attribute(name)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
