//! Quiet GitHub release checks and portable self-update / version switch.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::preferences;

pub const REPO: &str = "truefish/Quaternio";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const USER_AGENT: &str = concat!("Quaternio/", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub name: String,
    pub html_url: String,
    pub published_at: String,
    pub prerelease: bool,
    pub draft: bool,
    pub assets: Vec<Asset>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    fetched_at: u64,
    releases: Vec<Release>,
}

impl Release {
    pub fn display_tag(&self) -> &str {
        self.tag_name.trim_start_matches('v')
    }

    pub fn version(&self) -> Option<semver::Version> {
        parse_version(&self.tag_name)
    }

    pub fn is_newer_than_current(&self) -> bool {
        match (self.version(), parse_version(CURRENT_VERSION)) {
            (Some(remote), Some(current)) => remote > current,
            _ => false,
        }
    }

    pub fn matches_current(&self) -> bool {
        match (self.version(), parse_version(CURRENT_VERSION)) {
            (Some(remote), Some(current)) => remote == current,
            _ => self.display_tag() == CURRENT_VERSION
                || self.tag_name == CURRENT_VERSION
                || self.tag_name == format!("v{CURRENT_VERSION}"),
        }
    }

    pub fn published_label(&self) -> String {
        // 2026-04-01T12:00:00Z → 2026-04-01
        self.published_at
            .split('T')
            .next()
            .unwrap_or(&self.published_at)
            .to_string()
    }
}

fn parse_version(raw: &str) -> Option<semver::Version> {
    let trimmed = raw.trim().trim_start_matches('v');
    semver::Version::parse(trimmed).ok()
}

fn cache_path() -> PathBuf {
    preferences::data_dir().join("update-cache.json")
}

fn updates_dir() -> PathBuf {
    preferences::data_dir().join("updates")
}

/// List non-draft releases, using a 24h disk cache unless `force` is set.
pub fn list_releases(force: bool) -> Result<Vec<Release>, String> {
    if !force {
        if let Some(cached) = read_cache() {
            let age = now_secs().saturating_sub(cached.fetched_at);
            if age < CACHE_TTL.as_secs() {
                return Ok(filter_drafts(cached.releases));
            }
        }
    }
    let releases = fetch_releases()?;
    write_cache(&releases)?;
    Ok(filter_drafts(releases))
}

fn filter_drafts(releases: Vec<Release>) -> Vec<Release> {
    releases.into_iter().filter(|r| !r.draft).collect()
}

fn read_cache() -> Option<CacheFile> {
    let text = fs::read_to_string(cache_path()).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_cache(releases: &[Release]) -> Result<(), String> {
    let dir = preferences::data_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let cache = CacheFile {
        fetched_at: now_secs(),
        releases: releases.to_vec(),
    };
    let bytes = serde_json::to_vec_pretty(&cache).map_err(|e| e.to_string())?;
    let temporary = cache_path().with_extension("json.tmp");
    fs::write(&temporary, bytes).map_err(|e| e.to_string())?;
    fs::rename(&temporary, cache_path()).map_err(|e| e.to_string())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn fetch_releases() -> Result<Vec<Release>, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=30");
    let response = ureq::get(&url)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("Could not reach GitHub releases: {e}"))?;
    if !(200..300).contains(&response.status()) {
        return Err(format!(
            "GitHub releases returned HTTP {}",
            response.status()
        ));
    }
    response
        .into_json::<Vec<Release>>()
        .map_err(|e| format!("Could not parse GitHub releases: {e}"))
}

pub fn latest_update(releases: &[Release]) -> Option<&Release> {
    releases.iter().find(|r| r.is_newer_than_current())
}

pub fn find_release<'a>(releases: &'a [Release], tag: &str) -> Option<&'a Release> {
    let needle = tag.trim().trim_start_matches('v');
    releases.iter().find(|r| {
        r.tag_name == tag
            || r.display_tag() == needle
            || r.tag_name == format!("v{needle}")
    })
}

pub fn pick_asset(release: &Release) -> Option<&Asset> {
    let names: Vec<&str> = release.assets.iter().map(|a| a.name.as_str()).collect();
    let preferred = preferred_asset_name(&names)?;
    release.assets.iter().find(|a| a.name == preferred)
}

fn preferred_asset_name(names: &[&str]) -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        names
            .iter()
            .find(|n| n.starts_with("Quaternio-Windows-Portable-") && n.ends_with(".zip"))
            .map(|s| (*s).to_string())
    }
    #[cfg(target_os = "linux")]
    {
        names
            .iter()
            .find(|n| n.starts_with("Quaternio-Linux-x86_64-") && n.ends_with(".tar.gz"))
            .map(|s| (*s).to_string())
    }
    #[cfg(target_os = "macos")]
    {
        let arch = if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "x86_64"
        };
        let prefix = format!("Quaternio-macOS-{arch}-");
        names
            .iter()
            .find(|n| n.starts_with(&prefix) && n.ends_with(".tar.gz"))
            .map(|s| (*s).to_string())
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        let _ = names;
        None
    }
}

pub fn open_in_browser(url: &str) -> Result<(), String> {
    open::that(url).map_err(|e| format!("Could not open browser: {e}"))
}

/// Download the platform asset and replace the running install, then exit via helper.
pub fn download_and_apply(release: &Release) -> Result<(), String> {
    let asset = pick_asset(release).ok_or_else(|| {
        format!(
            "No downloadable build for this platform in {}",
            release.tag_name
        )
    })?;
    let install_dir = install_dir()?;
    let staging_root = updates_dir();
    fs::create_dir_all(&staging_root).map_err(|e| e.to_string())?;
    let archive_path = staging_root.join(&asset.name);
    let extract_dir = staging_root.join("staging");
    if extract_dir.exists() {
        let _ = fs::remove_dir_all(&extract_dir);
    }
    fs::create_dir_all(&extract_dir).map_err(|e| e.to_string())?;

    download_file(&asset.browser_download_url, &archive_path)?;
    extract_archive(&archive_path, &extract_dir)?;
    let payload = find_payload_dir(&extract_dir)?;
    spawn_replace_and_exit(&install_dir, &payload)?;
    Ok(())
}

fn install_dir() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "Could not resolve install directory".into())
}

fn download_file(url: &str, dest: &Path) -> Result<(), String> {
    let response = ureq::get(url)
        .set("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| format!("Download failed: {e}"))?;
    if !(200..300).contains(&response.status()) {
        return Err(format!("Download failed: HTTP {}", response.status()));
    }
    let temporary = dest.with_extension("partial");
    let mut file = File::create(&temporary).map_err(|e| e.to_string())?;
    let mut reader = response.into_reader();
    io::copy(&mut reader, &mut file).map_err(|e| e.to_string())?;
    file.flush().map_err(|e| e.to_string())?;
    drop(file);
    fs::rename(&temporary, dest).map_err(|e| e.to_string())
}

fn extract_archive(archive: &Path, dest: &Path) -> Result<(), String> {
    let name = archive
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if name.ends_with(".zip") {
        extract_zip(archive, dest)
    } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        extract_tar_gz(archive, dest)
    } else {
        Err(format!("Unsupported archive: {}", archive.display()))
    }
}

fn extract_zip(archive: &Path, dest: &Path) -> Result<(), String> {
    let file = File::open(archive).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        let Some(rel) = entry.enclosed_name() else {
            continue;
        };
        let out = dest.join(rel);
        if entry.is_dir() {
            fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut outfile = File::create(&out).map_err(|e| e.to_string())?;
        io::copy(&mut entry, &mut outfile).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = entry.unix_mode() {
                let _ = fs::set_permissions(&out, fs::Permissions::from_mode(mode));
            }
        }
    }
    Ok(())
}

fn extract_tar_gz(archive: &Path, dest: &Path) -> Result<(), String> {
    let file = File::open(archive).map_err(|e| e.to_string())?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(dest).map_err(|e| e.to_string())
}

fn find_payload_dir(extract_dir: &Path) -> Result<PathBuf, String> {
    let binary = binary_name();
    if extract_dir.join(binary).is_file() {
        return Ok(extract_dir.to_path_buf());
    }
    let mut found = None;
    let entries = fs::read_dir(extract_dir).map_err(|e| e.to_string())?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && path.join(binary).is_file() {
            if found.is_some() {
                return Err("Archive contains multiple Quaternio builds".into());
            }
            found = Some(path);
        }
    }
    found.ok_or_else(|| format!("Archive is missing {binary}"))
}

fn binary_name() -> &'static str {
    if cfg!(windows) {
        "quaternio.exe"
    } else {
        "quaternio"
    }
}

fn companion_names() -> &'static [&'static str] {
    if cfg!(windows) {
        &["ffmpeg.exe", "ffprobe.exe"]
    } else {
        &["ffmpeg", "ffprobe"]
    }
}

fn spawn_replace_and_exit(install_dir: &Path, payload: &Path) -> Result<(), String> {
    let pid = std::process::id();
    let helper = updates_dir().join(if cfg!(windows) {
        "apply-update.bat"
    } else {
        "apply-update.sh"
    });
    let binary = binary_name();
    let mut files = vec![binary.to_string()];
    for name in companion_names() {
        if payload.join(name).is_file() {
            files.push((*name).to_string());
        }
    }
    let launch = install_dir.join(binary);
    write_helper(&helper, install_dir, payload, &files, &launch, pid)?;

    #[cfg(windows)]
    {
        Command::new("cmd")
            .args(["/C", &helper.display().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("Could not start updater: {e}"))?;
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&helper).map_err(|e| e.to_string())?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&helper, perms).map_err(|e| e.to_string())?;
        Command::new(&helper)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("Could not start updater: {e}"))?;
    }

    std::process::exit(0);
}

fn write_helper(
    helper: &Path,
    install_dir: &Path,
    payload: &Path,
    files: &[String],
    launch: &Path,
    pid: u32,
) -> Result<(), String> {
    if let Some(parent) = helper.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    #[cfg(windows)]
    {
        let mut script = String::from("@echo off\r\nsetlocal\r\n");
        script.push_str(&format!(":wait\r\ntasklist /FI \"PID eq {pid}\" 2>NUL | find \"{pid}\" >NUL\r\nif not errorlevel 1 (\r\n  timeout /t 1 /nobreak >NUL\r\n  goto wait\r\n)\r\n"));
        for name in files {
            let src = payload.join(name);
            let dst = install_dir.join(name);
            script.push_str(&format!(
                "copy /Y \"{}\" \"{}\" >NUL\r\n",
                src.display(),
                dst.display()
            ));
        }
        script.push_str(&format!("start \"\" \"{}\"\r\n", launch.display()));
        script.push_str(&format!("del \"%~f0\"\r\n"));
        fs::write(helper, script).map_err(|e| e.to_string())?;
    }
    #[cfg(not(windows))]
    {
        let mut script = String::from("#!/bin/sh\nset -eu\n");
        script.push_str(&format!(
            "while kill -0 {pid} 2>/dev/null; do sleep 0.2; done\n"
        ));
        for name in files {
            let src = payload.join(name);
            let dst = install_dir.join(name);
            script.push_str(&format!(
                "cp -f '{}' '{}'\n",
                src.display(),
                dst.display()
            ));
            if name == binary_name() || name == "ffmpeg" || name == "ffprobe" {
                script.push_str(&format!("chmod +x '{}'\n", dst.display()));
            }
        }
        script.push_str(&format!("'{}' &\n", launch.display()));
        script.push_str("rm -f -- \"$0\"\n");
        fs::write(helper, script).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_beta_tags() {
        assert!(parse_version("v0.1.0-beta.1").is_some());
        assert!(parse_version("0.1.0-beta.1").is_some());
    }
}
