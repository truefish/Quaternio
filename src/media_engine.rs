//! Shared, verified media tools. Engine downloads are independent of app updates.
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;
use sha2::{Digest, Sha256};

const MAX_ARCHIVE: u64 = 1024 * 1024 * 1024;
const MAX_BINARY: u64 = 512 * 1024 * 1024;
const NOTICE: &str = include_str!("../packaging/FFmpeg-NOTICE.txt");
const MANIFEST: &str = include_str!("../packaging/ffmpeg-lock.json");

#[derive(Clone, Debug, serde::Serialize)]
pub struct Engine { pub ffmpeg: PathBuf, pub ffprobe: PathBuf, pub version: String }

pub fn run_if_requested() -> Result<bool, String> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let install_requested = args.iter().any(|a| a == "--media-engine-install");
    if !install_requested && !args.iter().any(|a| a == "--media-engine-check") { return Ok(false); }
    let engine = if install_requested {
        let archives = args.iter().position(|a| a == "--media-engine-archives")
            .map(|i| args.get(i+1).map(PathBuf::from).ok_or("--media-engine-archives requires a directory")).transpose()?;
        install(&AtomicBool::new(false), archives.as_deref(), |message| eprintln!("{message}"))?
    } else { discover()? };
    println!("{}", serde_json::to_string_pretty(&engine).map_err(|e| e.to_string())?);
    Ok(true)
}

type Stamp = (PathBuf, u64, Option<SystemTime>, u64, Option<SystemTime>);
static VALIDATED: OnceLock<Mutex<Vec<(Stamp, Engine)>>> = OnceLock::new();

fn executable(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

pub fn command(path: &Path) -> Command {
    let mut command = Command::new(path);
    #[cfg(windows)]
    { use std::os::windows::process::CommandExt; command.creation_flags(0x08000000); }
    command
}

type TimingStamp = (PathBuf, u64, Option<SystemTime>);
type TimingArgs = [&'static str; 2];
static TIMING_OPTIONS: OnceLock<Mutex<Vec<(TimingStamp, TimingArgs)>>> = OnceLock::new();

fn timing_from_help(help: &str) -> Result<TimingArgs, String> {
    let supports = |name: &str| help.lines().any(|line| line.split_whitespace().next().is_some_and(|option| option == name || option.strip_prefix(name).is_some_and(|suffix| suffix.starts_with("[:"))));
    if supports("-fps_mode") { Ok(["-fps_mode", "passthrough"]) }
    else if supports("-vsync") { Ok(["-vsync", "0"]) }
    else { Err("FFmpeg lacks frame-timing passthrough support; select another installation in Media engine setup".into()) }
}

/// Prefer the current output option, falling back only for older installations.
pub fn passthrough_args(ffmpeg: &Path) -> Result<TimingArgs, String> {
    let metadata = fs::metadata(ffmpeg).map_err(|e| e.to_string())?;
    let stamp = (ffmpeg.to_path_buf(), metadata.len(), metadata.modified().ok());
    let cache = TIMING_OPTIONS.get_or_init(Default::default);
    if let Some((_, args)) = cache.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(key, _)| key == &stamp) {
        return Ok(*args);
    }
    let args = timing_from_help(&output(ffmpeg, &["-hide_banner", "-h", "full"])?)?;
    let mut entries = cache.lock().unwrap_or_else(|e| e.into_inner());
    if entries.len() >= 32 { entries.clear(); }
    entries.push((stamp, args));
    Ok(args)
}

/// Bounded output and deadline, including broken third-party installations.
fn output(path: &Path, args: &[&str]) -> Result<String, String> {
    let mut child = command(path).args(args).stdin(Stdio::null())
        .stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(|e| e.to_string())?;
    let stdout = child.stdout.take().ok_or("Missing tool output")?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.take(4 * 1024 * 1024).read_to_end(&mut bytes).map(|_| bytes)
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if started.elapsed() < Duration::from_secs(8) => std::thread::sleep(Duration::from_millis(20)),
            other => {
                let _ = child.kill(); let _ = child.wait();
                break Err(match other { Err(e) => e.to_string(), _ => "Media tool check timed out".into() });
            }
        }
    };
    let bytes = reader.join().map_err(|_| "Media tool reader failed")?.map_err(|e| e.to_string())?;
    if !status?.success() { return Err(format!("{} failed its capability check", path.display())); }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn version(text: &str, tool: &str) -> Result<String, String> {
    text.lines().next().and_then(|line| line.strip_prefix(&format!("{tool} version ")))
        .and_then(|line| line.split_whitespace().next()).map(str::to_owned)
        .ok_or_else(|| format!("Not a recognized {tool} installation"))
}

fn contains_name(text: &str, name: &str) -> bool {
    text.lines().any(|line| line.split_whitespace().nth(1) == Some(name))
}

pub fn validate(dir: &Path) -> Result<Engine, String> {
    let ffmpeg = dir.join(executable("ffmpeg"));
    let ffprobe = dir.join(executable("ffprobe"));
    let a = fs::metadata(&ffmpeg).map_err(|e| format!("{}: {e}", ffmpeg.display()))?;
    let b = fs::metadata(&ffprobe).map_err(|e| format!("{}: {e}", ffprobe.display()))?;
    let stamp = (dir.to_path_buf(), a.len(), a.modified().ok(), b.len(), b.modified().ok());
    {
        let cache = VALIDATED.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, engine)) = cache.iter().find(|(key, _)| key == &stamp) { return Ok(engine.clone()); }
    }
    // Capability checks are expensive with large static executables. Reuse a
    // successful check across launches until either tool changes. Hardware
    // probing remains independent and is never inferred from this cache.
    let mut disk: Vec<(Stamp, String)> = fs::metadata(root().join("validated-v1.json")).ok()
        .filter(|m| m.len() <= 64 * 1024).and_then(|_| fs::read(root().join("validated-v1.json")).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
    if let Some((_, version)) = disk.iter().find(|(key, _)| key == &stamp) {
        let engine = Engine { ffmpeg, ffprobe, version: version.clone() };
        let mut cache = VALIDATED.get().unwrap().lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() >= 16 { cache.clear(); } cache.push((stamp,engine.clone())); return Ok(engine);
    }
    let v = version(&output(&ffmpeg, &["-version"])?, "ffmpeg")?;
    let p = version(&output(&ffprobe, &["-version"])?, "ffprobe")?;
    if v != p { return Err(format!("FFmpeg/ffprobe versions differ ({v} / {p}); choose a matching pair")); }
    let encoders = output(&ffmpeg, &["-hide_banner", "-encoders"])?;
    for required in ["libx264", "aac", "pcm_s16le", "rawvideo", "mjpeg"] {
        if !contains_name(&encoders, required) { return Err(format!("FFmpeg lacks required encoder {required}")); }
    }
    let filters = output(&ffmpeg, &["-hide_banner", "-filters"])?;
    for required in ["scale", "format", "trim", "setpts", "aresample"] {
        if !contains_name(&filters, required) { return Err(format!("FFmpeg lacks required filter {required}")); }
    }
    let engine = Engine { ffmpeg, ffprobe, version: v };
    let mut cache = VALIDATED.get().unwrap().lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() >= 16 { cache.clear(); }
    disk.retain(|(key,_)| key.0 != stamp.0);
    if disk.len() >= 16 { disk.remove(0); }
    disk.push((stamp.clone(),engine.version.clone()));
    if let Ok(bytes) = serde_json::to_vec(&disk) {
        let temporary = root().join(format!("validated-{}.tmp",unique()));
        if fs::create_dir_all(root()).is_ok() && fs::write(&temporary,bytes).is_ok() {
            if fs::rename(&temporary,root().join("validated-v1.json")).is_err() { let _ = fs::remove_file(temporary); }
        }
    }
    cache.push((stamp, engine.clone()));
    Ok(engine)
}

fn root() -> PathBuf { crate::preferences::data_dir().join("media-engine") }

fn directories() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::var_os("QUATERNIO_FFMPEG_DIR") { dirs.push(dir.into()); }
    if let Ok(path) = fs::read(root().join("selected.json")) {
        if let Ok(dir) = serde_json::from_slice::<PathBuf>(&path) { dirs.push(dir); }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() { dirs.push(dir.into()); }
    }
    if let Some(path) = std::env::var_os("PATH") { dirs.extend(std::env::split_paths(&path)); }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|dir| seen.insert(dir.clone()));
    dirs
}

pub fn discover() -> Result<Engine, String> {
    let mut errors = Vec::new();
    for dir in directories() {
        if !dir.join(executable("ffmpeg")).is_file() { continue; }
        match validate(&dir) { Ok(engine) => return Ok(engine), Err(error) => errors.push(error) }
    }
    Err(if errors.is_empty() { "No compatible FFmpeg/ffprobe pair found. Open Media engine setup to install or select one.".into() }
        else { format!("Media engine unavailable: {}", errors.join("; ").chars().take(1800).collect::<String>()) })
}

pub fn select(dir: &Path) -> Result<Engine, String> {
    let dir = dir.canonicalize().map_err(|e| e.to_string())?;
    let engine = validate(&dir)?;
    fs::create_dir_all(root()).map_err(|e| e.to_string())?;
    let temporary = root().join(format!("selected-{}.tmp", unique()));
    fs::write(&temporary, serde_json::to_vec(&dir).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    fs::rename(&temporary, root().join("selected.json")).map_err(|e| e.to_string())?;
    Ok(engine)
}

#[derive(Deserialize)]
struct Manifest { targets: std::collections::HashMap<String, Target> }
#[derive(Deserialize)]
struct Target { version: String, source: String, builder: String, assets: Vec<Asset> }
#[derive(Deserialize)]
struct Asset { file: String, url: String, sha256: String }

fn target() -> Result<Target, String> {
    let os = match std::env::consts::OS { "windows" => "windows", "linux" => "linux", "macos" => "macos", _ => "unsupported" };
    let arch = match std::env::consts::ARCH { "aarch64" => "arm64", other => other };
    let mut manifest: Manifest = serde_json::from_str(MANIFEST).map_err(|e| e.to_string())?;
    manifest.targets.remove(&format!("{os}-{arch}")).ok_or_else(|| "No managed engine for this platform. Select an existing installation instead.".into())
}

fn unique() -> String {
    format!("{}-{}", std::process::id(), SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_nanos())
}
fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) { Err("Media engine setup cancelled".into()) } else { Ok(()) }
}
struct Staging(PathBuf);
impl Drop for Staging { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }

/// Optional archive directory permits verified offline installation with the same pins.
pub fn install(cancel: &AtomicBool, archives: Option<&Path>, progress: impl Fn(String)) -> Result<Engine, String> {
    let target = target()?;
    fs::create_dir_all(root()).map_err(|e| e.to_string())?;
    let id = format!("{}-{:x}", target.version, Sha256::digest(serde_json::to_vec(&target.assets.iter().map(|a| &a.sha256).collect::<Vec<_>>()).unwrap()));
    let mut destination = root().join(&id);
    if let Ok(engine) = validate(&destination) { return select(engine.ffmpeg.parent().unwrap()); }
    if destination.exists() { destination = root().join(format!("{id}-repair-{}", unique())); }
    let stage = Staging(root().join(format!("staging-{}", unique())));
    fs::create_dir(&stage.0).map_err(|e| e.to_string())?;
    let mut found = std::collections::HashSet::new();
    for asset in &target.assets {
        check_cancel(cancel)?;
        progress(format!("{} media engine {}…", if archives.is_some() { "Reading offline" } else { "Downloading" }, target.version));
        let archive = stage.0.join(&asset.file);
        let reader: Box<dyn Read> = if let Some(dir) = archives {
            Box::new(File::open(dir.join(&asset.file)).map_err(|e| format!("Offline archive {}: {e}", asset.file))?)
        } else {
            Box::new(ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(15)).timeout_read(Duration::from_secs(5))
                .build().get(&asset.url).set("User-Agent", "Quaternio media-engine setup").call().map_err(|e| e.to_string())?.into_reader())
        };
        copy_archive(reader, &archive, &asset.sha256, cancel, &progress)?;
        progress("Verifying and extracting media tools…".into());
        if asset.url.ends_with(".zip") {
            let mut zip = zip::ZipArchive::new(File::open(&archive).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            for index in 0..zip.len() {
                check_cancel(cancel)?;
                let mut entry = zip.by_index(index).map_err(|e| e.to_string())?;
                let path = entry.enclosed_name().ok_or("Unsafe archive path")?;
                if entry.unix_mode().is_some_and(|mode| mode & 0o170000 == 0o120000) { return Err("Archive contains a symbolic link".into()); }
                if entry.is_file() { extract(&path, &mut entry, &stage.0, &mut found, cancel)?; }
            }
        } else if asset.url.ends_with(".tar.xz") {
            #[cfg(target_os = "linux")]
            {
            let decoder = xz2::read::XzDecoder::new(File::open(&archive).map_err(|e| e.to_string())?);
            for entry in tar::Archive::new(decoder).entries().map_err(|e| e.to_string())? {
                check_cancel(cancel)?;
                let mut entry = entry.map_err(|e| e.to_string())?;
                let path = entry.path().map_err(|e| e.to_string())?.into_owned();
                if path.is_absolute() || path.components().any(|c| matches!(c, std::path::Component::ParentDir)) { return Err("Unsafe archive path".into()); }
                if entry.header().entry_type().is_file() { extract(&path, &mut entry, &stage.0, &mut found, cancel)?; }
            }
            }
            #[cfg(not(target_os = "linux"))]
            return Err("XZ engine archives are only used on Linux".into());
        } else { return Err("Unsupported engine archive".into()); }
        fs::remove_file(&archive).map_err(|e| e.to_string())?;
    }
    check_cancel(cancel)?;
    let engine = validate(&stage.0)?;
    fs::write(stage.0.join("FFmpeg-NOTICE.txt"), format!("{NOTICE}\nVersion: {}\nSource: {}\nBuilder: {}\n", target.version, target.source, target.builder)).map_err(|e| e.to_string())?;
    fs::write(stage.0.join("COPYING.GPLv3"), include_bytes!("../packaging/licenses/COPYING.GPLv3")).map_err(|e| e.to_string())?;
    fs::write(stage.0.join("COPYING.GPLv2"), include_bytes!("../packaging/licenses/COPYING.GPLv2")).map_err(|e| e.to_string())?;
    fs::write(stage.0.join("engine-lock.json"), MANIFEST).map_err(|e| e.to_string())?;
    let mut hashes = std::collections::BTreeMap::new();
    for (name, tool) in [("ffmpeg", engine.ffmpeg), ("ffprobe", engine.ffprobe)] {
        let mut binary = File::open(&tool).map_err(|e| e.to_string())?;
        let mut hash = Sha256::new(); let mut buffer = [0u8; 128 * 1024];
        loop { check_cancel(cancel)?; let n = binary.read(&mut buffer).map_err(|e| e.to_string())?; if n == 0 { break; } hash.update(&buffer[..n]); }
        hashes.insert(name, format!("{:x}", hash.finalize()));
        for (label, args) in [("version", vec!["-version"]), ("buildconf", vec!["-buildconf"]), ("license", vec!["-L"])] {
            fs::write(stage.0.join(format!("{name}-{label}.txt")), output(&tool, &args)?).map_err(|e| e.to_string())?;
        }
    }
    fs::write(stage.0.join("binary-sha256.json"), serde_json::to_vec_pretty(&hashes).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    check_cancel(cancel)?;
    // Never replace an engine another running application may still be using.
    match fs::rename(&stage.0, &destination) {
        Ok(()) => {},
        Err(_) if validate(&destination).is_ok() => {},
        Err(e) => return Err(format!("Could not publish media engine: {e}")),
    }
    select(&destination)
}

fn copy_archive(mut reader: Box<dyn Read>, path: &Path, expected: &str, cancel: &AtomicBool, progress: &impl Fn(String)) -> Result<(), String> {
    let mut file = File::create(path).map_err(|e| e.to_string())?;
    let mut hash = Sha256::new(); let mut count = 0u64; let mut last = Instant::now();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        check_cancel(cancel)?;
        let n = reader.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 { break; }
        count += n as u64;
        if count > MAX_ARCHIVE { return Err("Media archive exceeds size limit".into()); }
        hash.update(&buffer[..n]); file.write_all(&buffer[..n]).map_err(|e| e.to_string())?;
        if last.elapsed() >= Duration::from_millis(250) { progress(format!("Receiving media archive: {} MiB", count / 1024 / 1024)); last = Instant::now(); }
    }
    file.sync_all().map_err(|e| e.to_string())?;
    if format!("{:x}", hash.finalize()) != expected { return Err("Media archive SHA-256 mismatch; installation rejected".into()); }
    Ok(())
}

fn extract(path: &Path, reader: &mut impl Read, stage: &Path, found: &mut std::collections::HashSet<String>, cancel: &AtomicBool) -> Result<(), String> {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else { return Ok(()); };
    let binary = name == executable("ffmpeg") || name == executable("ffprobe");
    if !binary && name != "LICENSE.txt" { return Ok(()); }
    if !found.insert(name.to_owned()) {
        if binary { return Err(format!("Duplicate media executable {name}")); }
        return Ok(());
    }
    let mut file = File::create(stage.join(name)).map_err(|e| e.to_string())?;
    let mut bytes = 0u64; let mut buffer = [0u8; 128 * 1024];
    loop {
        check_cancel(cancel)?;
        let n = reader.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 { break; }
        bytes += n as u64;
        if bytes > MAX_BINARY { return Err("Media executable exceeds size limit".into()); }
        file.write_all(&buffer[..n]).map_err(|e| e.to_string())?;
    }
    file.sync_all().map_err(|e| e.to_string())?;
    #[cfg(unix)]
    { if binary { use std::os::unix::fs::PermissionsExt; fs::set_permissions(stage.join(name), fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?; } }
    Ok(())
}
