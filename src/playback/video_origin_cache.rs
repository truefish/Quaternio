//! Disposable timestamp proofs, restricted to the configured workspace data root.
use super::{origin_stamp, OriginStamp, VideoOrigin};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex },
    time::UNIX_EPOCH };

const LIMIT: usize = 128;
const MAX_BYTES: u64 = 4096;
const STAGE_LIMIT: usize = 8;
const STALE_STAGE_SECS: u64 = 3600;
static WRITER: Mutex<()> = Mutex::new(());
static SERIAL: AtomicU64 = AtomicU64::new(0);
#[derive(Serialize, Deserialize)]
struct Proof {
    version: u32,
    key: String,
    clock: VideoOrigin,
    checksum: String }
fn checksum(key: &str, clock: VideoOrigin) -> String {
    let mut hash = Sha256::new();
    hash.update(key.as_bytes());
    hash.update(clock.seconds.to_bits().to_le_bytes());
    hash.update(clock.stream.to_le_bytes());
    format!("{:x}", hash.finalize())
}

fn plain(path: &Path) -> bool {
    let Ok(m) = std::fs::symlink_metadata(path) else {
        return false;
    };
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if m.file_attributes() & 0x400 != 0 {
            return false;
        }
    }
    !m.file_type().is_symlink()
}
fn directory(write: bool) -> Option<PathBuf> {
    let configured = PathBuf::from(std::env::var_os("QUATERNIO_DATA_DIR")?);
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .canonicalize()
        .ok()?;
    if !configured.is_absolute() || !plain(&configured) {
        return None;
    }
    let base = configured.canonicalize().ok()?;
    if !base.starts_with(&workspace) {
        return None;
    }
    for part in configured
        .ancestors()
        .take_while(|part| part.components().count() >= workspace.components().count())
    {
        if !plain(part) {
            return None;
        }
        if part == workspace {
            break;
        }
    }
    let path = base.join("video-origins");
    if write {
        std::fs::create_dir_all(&path).ok()?;
    }
    (plain(&path) && path.is_dir()).then_some(path)
}
fn key(path: &Path, stamp: OriginStamp) -> Option<String> {
    let mut hash = Sha256::new();
    hash.update(b"video-origin-v1");
    hash.update(path.canonicalize().ok()?.to_string_lossy().as_bytes());
    hash.update(stamp.0.to_le_bytes());
    hash.update(
        stamp
            .1
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos()
            .to_le_bytes(),
    );
    Some(format!("{:x}", hash.finalize()))
}
fn read(root: &Path, key: &str) -> Option<VideoOrigin> {
    use std::io::Read;
    let path = root.join(format!("{key}.json"));
    if !plain(&path) {
        return None;
    }
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_BYTES as usize {
        return None;
    }
    let proof: Proof = serde_json::from_slice(&bytes).ok()?;
    (proof.version == 1
        && proof.key == key
        && proof.checksum == checksum(key, proof.clock)
        && proof.clock.seconds.is_finite()
        && proof.clock.seconds.abs() <= 1e12
        && proof.clock.stream < 1024)
        .then_some(proof.clock)
}
pub(super) fn load(path: &Path, stamp: OriginStamp) -> Option<VideoOrigin> {
    let root = directory(false)?;
    let clock = read(&root, &key(path, stamp)?)?;
    (origin_stamp(path) == Some(stamp)).then_some(clock)
}
pub(super) fn save(path: &Path, stamp: OriginStamp, clock: VideoOrigin) {
    let _guard = WRITER.lock().unwrap_or_else(|e| e.into_inner());
    let Some(root) = directory(true) else {
        return;
    };
    save_at(&root, path, stamp, clock);
}
fn save_at(root: &Path, path: &Path, stamp: OriginStamp, clock: VideoOrigin) {
    // Serialize admission across viewers AND processes. Never wait on optional cache I/O.
    let lock_path = root.join("writer.lock");
    if lock_path.try_exists().ok() == Some(true) && !plain(&lock_path) {
        return;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0);
    }
    let Ok(_lock) = options.open(lock_path) else {
        return;
    };
    #[cfg(not(windows))]
    if _lock.try_lock().is_err() {
        return;
    }
    let Some(key) = key(path, stamp) else {
        return;
    };
    if origin_stamp(path) != Some(stamp) || read(&root, &key).is_some() {
        return;
    }
    let proof = Proof {
        version: 1,
        key: key.clone(),
        clock,
        checksum: checksum(&key, clock) };
    let Ok(bytes) = serde_json::to_vec(&proof) else {
        return;
    };
    if bytes.len() > MAX_BYTES as usize {
        return;
    }
    let final_path = root.join(format!("{key}.json"));
    if !admit(root, &final_path, std::time::SystemTime::now()) {
        return;
    }
    let stage = root.join(format!(
        "{key}.{}-{}.staged",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ));
    write_proof(&stage, &final_path, path, stamp, &bytes);
}

fn write_proof(stage: &Path, final_path: &Path, path: &Path, stamp: OriginStamp, bytes: &[u8]) {
    let mut created = false;
    let result = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stage)?;
        created = true;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        if origin_stamp(path) != Some(stamp) {
            return Err(std::io::Error::other("Source changed"));
        }
        std::fs::rename(&stage, &final_path)?;
        Ok(())
    })();
    if result.is_err() && created {
        let _ = std::fs::remove_file(&stage);
    }
}

fn owned_name(name: &str) -> Option<bool> {
    let (key, suffix) = name.split_once('.')?;
    if key.len() != 64
        || !key
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    if suffix == "json" {
        return Some(false);
    }
    let (pid, serial) = suffix.strip_suffix(".staged")?.split_once('-')?;
    let digits = |s: &str| !s.is_empty() && s.len() <= 20 && s.bytes().all(|b| b.is_ascii_digit());
    (digits(pid) && digits(serial)).then_some(true)
}

// Caller owns writer.lock. Failed cleanup denies admission rather than growing the cache.
fn admit(root: &Path, destination: &Path, now: std::time::SystemTime) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    let mut proofs = Vec::new();
    let mut stages = 0;
    for (index, entry) in entries.enumerate() {
        if index >= 1024 {
            return false;
        }
        let Ok(entry) = entry else {
            return false;
        };
        let Some(stage) = entry.file_name().to_str().and_then(owned_name) else {
            continue;
        };
        let path = entry.path();
        if !plain(&path) {
            return false;
        }
        let Ok(metadata) = entry.metadata() else {
            return false;
        };
        if !metadata.is_file() {
            return false;
        }
        let Ok(modified) = metadata.modified() else {
            return false;
        };
        if stage {
            let stale = now
                .duration_since(modified)
                .is_ok_and(|age| age.as_secs() >= STALE_STAGE_SECS);
            if !stale || std::fs::remove_file(&path).is_err() {
                stages += 1;
            }
        } else {
            proofs.push((modified, path));
        }
    }
    if stages >= STAGE_LIMIT {
        return false;
    }
    let replacing = proofs.iter().any(|(_, path)| path == destination);
    let target = LIMIT - usize::from(!replacing);
    proofs.sort_by_key(|(modified, _)| *modified);
    let mut excess = proofs.len().saturating_sub(target);
    for (_, path) in proofs {
        if excess == 0 {
            break;
        }
        if path != destination && std::fs::remove_file(path).is_ok() {
            excess -= 1;
        }
    }
    excess == 0
}

