//! Fixture-backed subset of Final Cut Pro 7 XML (Premiere's XML export dialect).
use std::collections::HashMap;
use std::path::Path;
use roxmltree::{Document, Node};
use crate::color::Grade;
use crate::project::{Clip, FrameRate, Media, MediaKind, Project, TrackKind};
use super::{ImportReport, resolve_media_path};

fn child<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> { node.children().find(|n| n.has_tag_name(name)) }
fn text<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Option<&'a str> { child(node, name)?.text() }
fn integer(node: Node<'_, '_>, name: &str) -> Option<i64> { text(node, name)?.parse().ok() }
fn rate(node: Node<'_, '_>, fallback: FrameRate) -> FrameRate {
    let Some(rate) = child(node, "rate") else { return fallback; };
    let base = integer(rate, "timebase").filter(|n| (1..=240).contains(n));
    match base {
        Some(base) if text(rate, "ntsc").is_some_and(|v| v.eq_ignore_ascii_case("TRUE")) => FrameRate::rational(base as u32 * 1000, 1001),
        Some(base) => FrameRate::rational(base as u32, 1),
        None => fallback,
    }
}

pub(super) fn parse(xml: &str, xml_dir: &Path) -> Result<(Project, ImportReport), String> {
    let doc = Document::parse_with_options(xml, roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() })
        .map_err(|e| format!("Invalid Final Cut Pro XML: {e}"))?;
    if !doc.root_element().has_tag_name("xmeml") { return Err("Expected <xmeml> Final Cut Pro 7 XML".into()); }
    let sequences: Vec<_> = doc.descendants().filter(|n| n.has_tag_name("sequence") && !n.ancestors().any(|p| p.has_tag_name("clipitem"))).collect();
    let seq = *sequences.first().ok_or("XML contains no sequence")?;
    let mut report = ImportReport::default();
    if sequences.len() > 1 { report.skipped.push("Multiple sequences: imported the first; import others separately".into()); }
    let seq_rate = rate(seq, FrameRate::rational(24, 1));
    let mut project = Project::new_default(text_of(seq, "name", "Imported timeline"));
    project.set_frame_rate(seq_rate);
    let media = child(seq, "media").ok_or("Sequence has no media tracks")?;
    if let Some(format) = child(media, "video").and_then(|n| child(n, "format")).and_then(|n| child(n, "samplecharacteristics")) {
        let width = integer(format, "width").unwrap_or(1920).clamp(16, 16384) as u32;
        let height = integer(format, "height").unwrap_or(1080).clamp(16, 16384) as u32;
        project.set_resolution(width, height);
    }
    project.sequence.tracks.clear();
    // Full definitions may occur after references; resolve them in a first pass.
    let files: HashMap<_, _> = doc.descendants().filter(|n| n.has_tag_name("file") && child(*n, "pathurl").is_some())
        .filter_map(|n| n.attribute("id").map(|id| (id.to_owned(), n))).collect();
    let mut media_ids = HashMap::new();
    let mut clip_ids = HashMap::new();
    let mut links = Vec::new();
    for (tag, kind) in [("video", TrackKind::Video), ("audio", TrackKind::Audio)] {
        let Some(section) = child(media, tag) else { continue; };
        for track in section.children().filter(|n| n.has_tag_name("track")) {
            let index = project.add_track(kind);
            project.sequence.tracks[index].muted = text(track, "enabled").is_some_and(|v| v.eq_ignore_ascii_case("FALSE"));
            for item in track.children().filter(|n| n.is_element()) {
                if item.has_tag_name("transitionitem") { report.skipped.push(format!("{} transition requires recreation; adjacent transition-bound clips are not guessed", project.sequence.tracks[index].name)); continue; }
                if !item.has_tag_name("clipitem") { continue; }
                let name = text_of(item, "name", "Clip").to_owned();
                let (Some(start), Some(end), Some(source_in)) = (integer(item, "start"), integer(item, "end"), integer(item, "in")) else {
                    report.skipped.push(format!("'{name}': missing explicit clip boundaries")); continue;
                };
                if start < 0 || end <= start || source_in < 0 || end > 1_000_000_000_000 {
                    report.skipped.push(format!("'{name}': unsupported transition-bound or invalid range")); continue;
                }
                if item.descendants().any(|n| n.has_tag_name("effectid") && n.text().is_some_and(|t| matches!(t, "timeremap" | "Time Remap"))) {
                    report.skipped.push(format!("'{name}': speed remapping requires baking")); continue;
                }
                let Some(file) = child(item, "file").and_then(|n| if child(n, "pathurl").is_some() { Some(n) } else { n.attribute("id").and_then(|id| files.get(id).copied()) }) else {
                    report.skipped.push(format!("'{name}': nested sequence, generator, or missing file reference")); continue;
                };
                let path = resolve_media_path(text(file, "pathurl").unwrap_or(""), xml_dir);
                let source_rate = rate(item, rate(file, seq_rate));
                let source_seconds = source_rate.frame_to_seconds(source_in);
                let id = if let Some(id) = media_ids.get(&path) { *id } else {
                    let video = child(file, "media").and_then(|n| child(n, "video"));
                    let sample = video.and_then(|n| child(n, "samplecharacteristics"));
                    let duration = rate(file, source_rate).frame_to_seconds(integer(file, "duration").unwrap_or(0));
                    let offline = !path.is_file();
                    let probed = (!offline).then(|| crate::playback::probe_path(&path)).transpose();
                    let mut asset = match probed {
                        Ok(Some(asset)) => asset,
                        result => {
                            if let Err(error) = result { report.skipped.push(format!("'{name}': probe failed; retained metadata: {error}")); }
                            Media { id: 0, name: text_of(file, "name", &name).to_owned(), bin: String::new(), path: path.clone(),
                                kind: if video.is_some() || kind == TrackKind::Video { MediaKind::Video } else { MediaKind::Audio },
                                duration_secs: duration.max(source_seconds + seq_rate.frame_to_seconds(end - start)),
                                width: sample.and_then(|n| integer(n, "width")).unwrap_or(1920).clamp(0, 16384) as u32,
                                height: sample.and_then(|n| integer(n, "height")).unwrap_or(1080).clamp(0, 16384) as u32,
                                has_video: video.is_some() || kind == TrackKind::Video,
                                has_audio: kind == TrackKind::Audio || child(file, "media").and_then(|n| child(n, "audio")).is_some(),
                                source_fps: source_rate.fps(), source_in_mark: None, source_out_mark: None }
                        }
                    };
                    asset.id = 0;
                    let id = project.add_media(asset); media_ids.insert(path, id);
                    report.media_count += 1; if offline { report.offline_count += 1; } id
                };
                if let Some(asset) = project.media.iter_mut().find(|m| m.id == id) {
                    if kind == TrackKind::Audio { asset.has_audio = true; } else { asset.has_video = true; }
                    if !asset.path.is_file() {
                        asset.duration_secs = asset.duration_secs.max(source_seconds + seq_rate.frame_to_seconds(end - start));
                    }
                }
                if text(item, "enabled").is_some_and(|v| v.eq_ignore_ascii_case("FALSE")) {
                    report.skipped.push(format!("'{name}': disabled clip omitted")); continue;
                }
                let clip_id = project.alloc();
                if let Some(xml_id) = item.attribute("id") {
                    clip_ids.insert(xml_id.to_owned(), clip_id);
                    for link in item.children().filter(|n| n.has_tag_name("link")) {
                        if let Some(other) = text(link, "linkclipref") { links.push((xml_id.to_owned(), other.to_owned())); }
                    }
                }
                let source_frame = seq_rate.seconds_to_frame(source_seconds);
                project.sequence.tracks[index].clips.push(Clip { id: clip_id, media_id: id, name: name.clone(), timeline_start: start,
                    transform: Default::default(),
                    duration: end - start, source_in: source_frame, source_time_offset: source_seconds - seq_rate.frame_to_seconds(source_frame),
                    link: None, volume: 1.0, pos_x: 0.0, pos_y: 0.0, scale: 1.0, opacity: 1.0, grade: Grade::default() });
                report.clip_count += 1;
                for filter in item.children().filter(|n| n.has_tag_name("filter")) {
                    report.skipped.push(format!("'{name}': effect '{}' requires recreation", filter.descendants().find(|n| n.has_tag_name("name")).and_then(|n| n.text()).unwrap_or("unknown")));
                }
                if kind == TrackKind::Audio && child(item, "sourcetrack").and_then(|n| integer(n, "trackindex")).is_some_and(|n| n > 1) {
                    report.skipped.push(format!("'{name}': source channel mapping not supported; verify audio before export"));
                }
            }
        }
    }
    // Union link components; a link is a shared group identity, not another clip ID.
    let mut groups: HashMap<u64, u64> = clip_ids.values().map(|id| (*id, *id)).collect();
    for (a,b) in links {
        if let (Some(a), Some(b)) = (clip_ids.get(&a), clip_ids.get(&b)) {
            let ga = group_root(&mut groups, *a); let gb = group_root(&mut groups, *b);
            if ga != gb { groups.insert(gb,ga); }
        }
    }
    let mut link_ids = HashMap::new();
    let mut counts = HashMap::new();
    for id in groups.keys().copied().collect::<Vec<_>>() {
        let root = group_root(&mut groups,id); groups.insert(id,root);
        *counts.entry(root).or_insert(0usize) += 1;
    }
    for (group,count) in counts {
        if count > 1 { link_ids.insert(group, project.alloc()); }
    }
    for track in &mut project.sequence.tracks { for clip in &mut track.clips { clip.link = groups.get(&clip.id).and_then(|g| link_ids.get(g)).copied(); } }
    if report.clip_count == 0 { return Err(format!("XML contains no supported clips. {}", report.detail_text())); }
    project.ensure_integrity(); Ok((project, report))
}

fn text_of<'a>(node: Node<'a, '_>, name: &str, fallback: &'a str) -> &'a str { text(node, name).unwrap_or(fallback) }
fn group_root(groups: &mut HashMap<u64,u64>, id: u64) -> u64 {
    let mut root = id; while groups[&root] != root { root = groups[&root]; }
    let mut item = id; while groups[&item] != root { let next = groups[&item]; groups.insert(item,root); item = next; }
    root
}
