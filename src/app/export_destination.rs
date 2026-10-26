use std::path::{Component, Path, PathBuf};

fn reserved_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || ["COM", "LPT"].iter().any(|prefix| stem.strip_prefix(prefix)
            .is_some_and(|tail| matches!(tail, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")))
}

pub(super) fn default_filename(project: &str) -> String {
    let cleaned: String = project.chars().map(|c| {
        if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c }
    }).collect();
    let mut name = cleaned.trim().trim_end_matches('.').to_string();
    while name.len() > 180 { name.pop(); }
    if name.is_empty() { name = "Sequence".into(); }
    if reserved_name(&name) { name.insert(0, '_'); }
    if !name.to_ascii_lowercase().ends_with(".mp4") { name.push_str(".mp4"); }
    name
}

pub(super) fn default_folder(project: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(parent) = project.and_then(|path| absolute(path).ok())
        .and_then(|path| path.parent().map(Path::to_path_buf)) {
        return Ok(parent);
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from).filter(|path| path.is_absolute());
    if let Some(home) = home {
        let videos = home.join(if cfg!(target_os = "macos") { "Movies" } else { "Videos" });
        return Ok(if videos.is_dir() { videos } else { home });
    }
    std::env::current_dir().map_err(|_| "Choose an absolute export folder.".into())
}

pub(super) fn absolute(path: &Path) -> Result<PathBuf, String> {
    std::path::absolute(path).map_err(|error| format!("Cannot resolve this path: {error}"))
}

pub(super) fn folder(text: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(text.trim());
    if !path.is_absolute() {
        return Err("Choose a folder or enter its full absolute path.".into());
    }
    if text.chars().any(char::is_control) {
        return Err("The export folder contains an invalid character.".into());
    }
    #[cfg(windows)]
    if text.starts_with(r"\\.\") || text.starts_with(r"\\?\")
        || path.components().any(|part| matches!(part, Component::Normal(name)
            if name.to_string_lossy().chars().any(|c| "<>:\"|?*".contains(c))
                || name.to_string_lossy().ends_with(['.', ' ']) || reserved_name(&name.to_string_lossy()))) {
        return Err("Choose a normal drive or network folder without reserved characters.".into());
    }
    Ok(path)
}

pub(super) fn resolve(folder_text: &str, filename: &str) -> Result<PathBuf, String> {
    let folder = folder(folder_text)?;
    if filename.trim().is_empty() {
        return Err("Enter a filename for the exported video.".into());
    }
    if filename.chars().any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
        || filename.ends_with(['.', ' ']) || filename.starts_with(' ')
        || reserved_name(filename) || matches!(filename, "." | "..") {
        return Err("Use a filename without slashes, reserved names, or special characters: < > : \" | ? *.".into());
    }
    let mut name = filename.to_string();
    if Path::new(filename).extension().is_none() { name.push_str(".mp4"); }
    if !name.to_ascii_lowercase().ends_with(".mp4") && !name.to_ascii_lowercase().ends_with(".wav") {
        return Err("Use .mp4 for video or .wav for audio.".into());
    }
    if name.len() > 240 || name.encode_utf16().count() > 240 {
        return Err("Use a shorter filename (up to 240 bytes; some characters take more than one byte).".into());
    }
    Ok(folder.join(name))
}

fn path_key(path: &Path) -> String {
    let mut normalized = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {},
            Component::ParentDir => { normalized.pop(); },
            other => normalized.push(other.as_os_str()),
        }
    }
    let key = normalized.to_string_lossy().into_owned();
    #[cfg(windows)]
    let key = key.replace('/', "\\").trim_start_matches(r"\\?\").to_lowercase();
    key
}

pub(super) fn same_lexical_path(left: &Path, right: &Path) -> bool {
    match (absolute(left), absolute(right)) {
        (Ok(left), Ok(right)) => path_key(&left) == path_key(&right),
        _ => false,
    }
}

fn canonical_destination(path: &Path) -> Result<PathBuf, String> {
    match std::fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or("Choose an export folder.")?;
            let parent = std::fs::canonicalize(parent)
                .map_err(|error| format!("Cannot access export folder {}: {error}", parent.display()))?;
            Ok(parent.join(path.file_name().ok_or("Enter an export filename.")?))
        },
        Err(error) => Err(format!("Cannot access {}: {error}", path.display())),
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct ExistingOutput {
    path: String,
    size: u64,
    modified: Option<std::time::SystemTime>,
}

// Run on the validation worker; canonicalizing a network path can block.
pub(super) fn inspect(output: &Path, sources: &[PathBuf], occupied: &[PathBuf])
    -> Result<Option<ExistingOutput>, String> {
    if !output.is_absolute() { return Err("The export needs an absolute output path. Choose a folder and try again.".into()); }
    let parent = output.parent().ok_or("Choose an export folder.")?;
    let parent_meta = std::fs::metadata(parent)
        .map_err(|error| format!("Cannot access export folder {}: {error}", parent.display()))?;
    if !parent_meta.is_dir() { return Err("The export folder is a file. Choose a folder instead.".into()); }
    if parent_meta.permissions().readonly() && !cfg!(windows) {
        return Err("The export folder is read-only. Choose a writable folder.".into());
    }
    let canonical = canonical_destination(output)?;
    for source in sources {
        let source_canonical = std::fs::canonicalize(source).ok();
        if same_lexical_path(output, source)
            || source_canonical.as_ref().is_some_and(|source| path_key(source) == path_key(&canonical))
            || same_file(output, source) {
            return Err("This output would overwrite project or source media. Choose a different filename or folder.".into());
        }
    }
    for other in occupied {
        let other_canonical = canonical_destination(other).ok();
        if same_lexical_path(output, other)
            || other_canonical.as_ref().is_some_and(|other| path_key(other) == path_key(&canonical))
            || same_file(output, other) {
            return Err("A queued or running export already uses this file. Choose a different filename or folder.".into());
        }
    }
    match std::fs::metadata(output) {
        Ok(meta) if !meta.is_file() => Err("The output path is a folder. Choose a different filename.".into()),
        Ok(meta) if meta.permissions().readonly() => Err("The output file is read-only. Choose another filename.".into()),
        Ok(meta) => Ok(Some(ExistingOutput { path: path_key(&canonical), size: meta.len(), modified: meta.modified().ok() })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Cannot access the output file: {error}")),
    }
}

#[cfg(unix)]
fn same_file(left: &Path, right: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(left), std::fs::metadata(right)) {
        (Ok(left), Ok(right)) => left.dev() == right.dev() && left.ino() == right.ino(),
        _ => false,
    }
}

#[cfg(windows)]
fn same_file(left: &Path, right: &Path) -> bool {
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    #[derive(Default)]
    struct Info { attributes: u32, creation: [u32; 2], access: [u32; 2], write: [u32; 2], volume: u32,
        size_high: u32, size_low: u32, links: u32, index_high: u32, index_low: u32 }
    #[link(name = "kernel32")]
    extern "system" { fn GetFileInformationByHandle(file: *mut std::ffi::c_void, info: *mut Info) -> i32; }
    let identity = |path: &Path| {
        let file = std::fs::File::open(path).ok()?;
        let mut info = Info::default();
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 { return None; }
        Some((info.volume, info.index_high, info.index_low))
    };
    identity(left).zip(identity(right)).is_some_and(|(a, b)| a == b)
}

#[cfg(not(any(unix, windows)))]
fn same_file(_left: &Path, _right: &Path) -> bool { false }

#[derive(Clone, Copy)]
pub(super) enum OpenAction { File, Reveal, Folder }

pub(super) fn open_output(path: &Path, action: OpenAction) -> Result<(), String> {
    if !path.is_absolute() { return Err("The saved export path is not absolute. Export again to a chosen folder.".into()); }
    let meta = std::fs::metadata(path)
        .map_err(|error| format!("Cannot find or access {}. It may have been moved or deleted. {error}", path.display()))?;
    if matches!(action, OpenAction::Folder) && !meta.is_dir() { return Err("The selected export folder no longer exists.".into()); }
    if !matches!(action, OpenAction::Folder) && !meta.is_file() { return Err("The completed export file no longer exists at this path.".into()); }
    let result = if matches!(action, OpenAction::Reveal) { reveal(path) } else { open::that(path) };
    result.map_err(|error| format!("Could not open {}: {error}", path.display()))
}

#[cfg(windows)]
fn reveal(path: &Path) -> std::io::Result<()> {
    use std::{ffi::c_void, os::windows::ffi::OsStrExt, ptr};
    #[link(name = "ole32")]
    extern "system" {
        fn CoInitializeEx(reserved: *mut c_void, mode: u32) -> i32;
        fn CoUninitialize();
    }
    #[link(name = "shell32")]
    extern "system" {
        fn ILCreateFromPathW(path: *const u16) -> *mut c_void;
        fn ILFree(item: *mut c_void);
        fn SHOpenFolderAndSelectItems(item: *const c_void, count: u32, children: *const *const c_void, flags: u32) -> i32;
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let initialized = unsafe { CoInitializeEx(ptr::null_mut(), 2) };
    if initialized < 0 { return Err(std::io::Error::other(format!("Cannot initialize Explorer (0x{:08X})", initialized as u32))); }
    let item = unsafe { ILCreateFromPathW(wide.as_ptr()) };
    let result = if item.is_null() {
        Err(std::io::Error::other("Explorer could not locate the exported file"))
    } else {
        // A zero selection count reveals this absolute item in its parent folder.
        let status = unsafe { SHOpenFolderAndSelectItems(item, 0, ptr::null(), 0) };
        unsafe { ILFree(item); }
        if status < 0 { Err(std::io::Error::other(format!("Explorer could not open the folder (0x{:08X})", status as u32))) }
        else { Ok(()) }
    };
    unsafe { CoUninitialize(); }
    result
}

#[cfg(not(windows))]
fn reveal(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg("-R").arg(path);
        command
    };
    #[cfg(target_os = "macos")]
    {
        let status = command.status()?;
        if !status.success() { return Err(std::io::Error::other(format!("file manager exited with {status}"))); }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    open::that(path.parent().ok_or_else(|| std::io::Error::other("The file has no parent folder"))?)
}
