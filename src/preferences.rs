use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::commands::Action;
use crate::project::ShortcutProfile;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Layout {
    pub upper_ratio: f32,
    pub source_width: f32,
    pub properties_width: f32,
    pub project_width: f32,
    pub workspace: i32,
    pub project_top: bool }

impl Default for Layout {
    fn default() -> Self {
        Self {
            upper_ratio: 0.55,
            source_width: 0.0,
            properties_width: 300.0,
            project_width: 320.0,
            workspace: 0,
            project_top: false }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub export_format: Option<ExportFormat>,
    pub profile: String,
    pub keybindings: BTreeMap<String, String>,
    pub saved_layout: Option<Layout>,
    pub current_layout: Option<Layout>,
    pub vim_navigation: bool,
    pub preview_hardware: bool,
    pub reviewer_name: String,
    pub track_density: i32,
    pub preview_cache: crate::playback::optimized::PreviewCacheSettings }

impl Default for Preferences {
    fn default() -> Self {
        Self {
            export_format: None,
            profile: "premiere".into(),
            keybindings: BTreeMap::new(),
            saved_layout: None,
            current_layout: None,
            vim_navigation: false,
            preview_hardware: true,
            reviewer_name: "Editor".into(),
            track_density: 0,
            preview_cache: Default::default() }
    }
}

/// Reusable encoding choices; destinations remain specific to each editing session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExportFormat {
    pub width: u32,
    pub height: u32,
    pub frame_rate: crate::project::FrameRate,
    pub crf: f32,
    pub speed: i32,
    pub encoder: i32,
    #[serde(default)]
    pub amd_adapter: Option<u32> }

impl Preferences {
    pub fn load() -> Self {
        std::fs::read_to_string(config_path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        // Finish the new document before replacing the previous preferences.
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, bytes).map_err(|e| e.to_string())?;
        std::fs::rename(&temporary, &path).map_err(|e| e.to_string())
    }

    pub fn profile(&self) -> ShortcutProfile {
        if self.profile == "resolve" {
            ShortcutProfile::Resolve
        } else {
            ShortcutProfile::Premiere
        }
    }

    pub fn binding<'a>(&'a self, action: &'a Action, profile: ShortcutProfile) -> &'a str {
        self.keybindings
            .get(action.id)
            .map(String::as_str)
            .unwrap_or_else(|| action.default_binding(profile))
    }
}

/// App data directory (preferences, update cache, staging).
pub fn data_dir() -> PathBuf {
    if let Some(directory) = std::env::var_os("QUATERNIO_DATA_DIR") {
        return PathBuf::from(directory);
    }
    if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA").unwrap_or_else(|| ".".into())).join("Quaternio")
    } else {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into()))
            .join(".config")
            .join("quaternio")
    }
}

fn config_path() -> PathBuf {
    data_dir().join("preferences.json")
}

