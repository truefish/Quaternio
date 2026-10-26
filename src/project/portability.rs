//! Portable collection and a deliberately reported FCP 7 XML interchange subset.
use super::*;
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    sync::atomic::{AtomicBool, Ordering},
};

pub fn collect(
    project: &Project,
    destination: &Path,
    cancel: &AtomicBool,
    progress: impl Fn(String),
) -> Result<PathBuf, String> {
    fs::create_dir(destination).map_err(|e| format!("Choose a new collection folder: {e}"))?;
    // Keep failed collections for inspection; only our partial individual files are discarded.
    let mut copy = project.clone();
    let media_dir = destination.join("Media");
    fs::create_dir(&media_dir).map_err(|e| e.to_string())?;
    let mut manifest = Vec::new();
    let total = copy.media.len();
    for (index, media) in copy.media.iter_mut().enumerate() {
        if cancel.load(Ordering::Acquire) {
            return Err("Collection cancelled; the incomplete folder was kept".into());
        }
        let source = media.path.clone();
        let name = source
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("media");
        let name = format!("{}-{name}", media.id);
        let relative = PathBuf::from("Media").join(name);
        let output = destination.join(&relative);
        let temporary = output.with_extension(format!("{}.part", uuid::Uuid::new_v4()));
        let result = (|| -> Result<String, String> {
            let mut input = fs::File::open(&source)
                .map_err(|e| format!("Could not collect {}: {e}", media.name))?;
            let before = input.metadata().map_err(|e| e.to_string())?;
            let mut out = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .map_err(|e| e.to_string())?;
            let mut buf = [0u8; 262144];
            let mut hash = Sha256::new();
            let mut bytes = 0;
            loop {
                if cancel.load(Ordering::Acquire) {
                    return Err("Collection cancelled".into());
                }
                let n = input.read(&mut buf).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])
                    .map_err(|e| format!("Collection write failed: {e}"))?;
                hash.update(&buf[..n]);
                bytes += n as u64;
                progress(format!(
                    "Collecting {} of {total}: {} · {} MiB",
                    index + 1,
                    media.name,
                    bytes / 1048576
                ));
            }
            let after = input.metadata().map_err(|e| e.to_string())?;
            if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
                return Err("Source changed during collection; try again".into());
            }
            out.sync_all().map_err(|e| e.to_string())?;
            drop(out);
            fs::rename(&temporary, &output).map_err(|e| e.to_string())?;
            Ok(format!("{:x}", hash.finalize()))
        })();
        let _ = fs::remove_file(&temporary);
        let checksum = result?;
        manifest.push(serde_json::json!({"file":relative,"sha256":checksum}));
        media.path = relative;
    }
    let project_path = destination.join("Project.quaternio");
    copy.save_to(&project_path)?;
    write_atomic(
        &destination.join("checksums.json"),
        &serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?,
    )?;
    progress("Collection complete".into());
    Ok(project_path)
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
pub fn archive(
    directory: &Path,
    cancel: &AtomicBool,
    progress: impl Fn(String),
) -> Result<PathBuf, String> {
    let output = directory.with_extension("zip");
    let temporary = output.with_extension(format!("{}.part", uuid::Uuid::new_v4()));
    let result = (|| -> Result<(), String> {
        let file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .large_file(true);
        let mut paths = vec![
            directory.join("Project.quaternio"),
            directory.join("checksums.json"),
        ];
        for entry in fs::read_dir(directory.join("Media")).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.is_file() {
                paths.push(path);
            }
        }
        for path in paths {
            if cancel.load(Ordering::Acquire) {
                return Err("Archive cancelled; the collection folder was kept".into());
            }
            let name = path
                .strip_prefix(directory)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            progress(format!("Archiving {name}"));
            zip.start_file(name, options).map_err(|e| e.to_string())?;
            let mut input = fs::File::open(&path).map_err(|e| e.to_string())?;
            let mut buf = [0u8; 262144];
            loop {
                if cancel.load(Ordering::Acquire) {
                    return Err("Archive cancelled; the collection folder was kept".into());
                }
                let n = input.read(&mut buf).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                zip.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            }
        }
        let file = zip.finish().map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        fs::rename(&temporary, &output).map_err(|e| e.to_string())?;
        Ok(())
    })();
    let _ = fs::remove_file(temporary);
    result?;
    Ok(output)
}
fn url(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut encoded = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-_.~:".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    if encoded.starts_with('/') {
        format!("file://localhost{encoded}")
    } else {
        format!("file://localhost/{encoded}")
    }
}
fn rate_xml(rate: FrameRate) -> String {
    format!(
        "<rate><timebase>{}</timebase><ntsc>{}</ntsc></rate>",
        rate.nominal(),
        if rate.den == 1001 { "TRUE" } else { "FALSE" }
    )
}
pub fn timeline_xml(project: &Project) -> Result<(String, Vec<String>), String> {
    let s = &project.sequence;
    let mut report = Vec::new();
    if s.frame_rate.den != 1 && s.frame_rate.den != 1001 {
        return Err("XML export supports whole and NTSC frame rates".into());
    }
    let mut xml=format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<xmeml version=\"5\"><sequence id=\"sequence-1\"><name>{}</name>{}<duration>{}</duration><media>",escape(&s.name),rate_xml(s.frame_rate),s.duration_frames());
    for (kind, tag) in [(TrackKind::Video, "video"), (TrackKind::Audio, "audio")] {
        xml.push_str(&format!("<{tag}>"));
        if kind == TrackKind::Video {
            xml.push_str(&format!("<format><samplecharacteristics><width>{}</width><height>{}</height>{}</samplecharacteristics></format>",s.width,s.height,rate_xml(s.frame_rate)));
        }
        for t in s.tracks.iter().filter(|t| t.kind == kind) {
            xml.push_str("<track>");
            xml.push_str(&format!(
                "<enabled>{}</enabled>",
                if t.muted { "FALSE" } else { "TRUE" }
            ));
            if t.gain != 1. || t.pan != 0. || t.role != audio::AudioRole::Normal {
                report.push(format!(
                    "{}: track gain, pan and audio role are not transferred",
                    t.name
                ));
            }
            for c in &t.clips {
                if c.creative.text.is_some()
                    || c.creative.sequence.is_some()
                    || c.creative.adjustment
                    || c.creative.multicam.is_some()
                {
                    report.push(format!(
                        "{}: generated, nested or multicam content omitted; render it before interchange",
                        c.name
                    ));
                    continue;
                }
                let Some(m) = project.media(c.media_id) else {
                    report.push(format!("{}: missing media omitted", c.name));
                    continue;
                };
                if c.creative.speed != 1. || c.creative.freeze {
                    report.push(format!(
                        "{}: retiming omitted; render it before interchange",
                        c.name
                    ));
                    continue;
                }
                if !c.creative.animation.is_empty()
                    || !c.grade.is_identity()
                    || !c.transform.is_identity()
                    || c.opacity != 1.
                    || c.volume != 1.
                    || c.scale != 1.
                    || c.pos_x != 0.
                    || c.pos_y != 0.
                    || c.creative.dialogue
                    || c.creative.lut.is_some()
                    || c.creative.input_color != crate::color::management::Interpretation::Tagged
                    || matches!(m.color.transfer.as_str(), "smpte2084" | "arib-std-b67")
                {
                    report.push(format!(
                        "{}: picture/audio effects are not transferred",
                        c.name
                    ));
                }
                let rate = if m.kind == MediaKind::Audio {
                    s.frame_rate
                } else {
                    m.source_rate()
                };
                let source = rate.seconds_to_frame(c.source_start_seconds(s.frame_rate));
                let duration = rate.seconds_to_frame(s.frame_rate.frame_to_seconds(c.duration));
                if (rate.frame_to_seconds(source) - c.source_start_seconds(s.frame_rate)).abs()
                    > 1e-6
                {
                    report.push(format!(
                        "{}: subframe source timing is rounded to the XML frame grid",
                        c.name
                    ));
                }
                xml.push_str(&format!("<clipitem id=\"clip-{}\"><name>{}</name>{}<start>{}</start><end>{}</end><in>{}</in><out>{}</out><file id=\"file-{}\"><name>{}</name><pathurl>{}</pathurl>{}<duration>{}</duration><media>",c.id,escape(&c.name),rate_xml(rate),c.timeline_start,c.end(),source,source+duration,m.id,escape(&m.name),escape(&url(&m.path)),rate_xml(rate),rate.seconds_to_frame(m.source_duration_seconds())));
                if m.has_video {
                    xml.push_str(&format!("<video><samplecharacteristics><width>{}</width><height>{}</height></samplecharacteristics></video>",m.width,m.height));
                }
                if m.has_audio {
                    xml.push_str("<audio><channelcount>2</channelcount></audio>");
                }
                xml.push_str("</media></file></clipitem>");
            }
            xml.push_str("</track>");
        }
        xml.push_str(&format!("</{tag}>"));
    }
    if !s.transitions.is_empty() {
        report.push("Transitions are omitted; cuts and clip timing are preserved".into());
    }
    if !s.audio.is_identity() {
        report.push("Mix finishing effects are not transferred".into());
    }
    if s.color.lut.is_some() {
        report.push("Sequence color LUT is not transferred".into());
    }
    if !project.sequences.is_empty() {
        report.push("Only the open sequence is exported".into());
    }
    xml.push_str("</media></sequence></xmeml>\n");
    Ok((xml, report))
}
