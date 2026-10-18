use crate::project::ShortcutProfile;
use crate::shortcuts::Mods;

#[derive(Clone, Copy)]
pub enum Scope {
    Always,
    Selection,
    Media,
    Clips,
    Multiple }

pub struct Action {
    pub id: &'static str,
    pub label: &'static str,
    pub premiere: &'static str,
    pub resolve: &'static str,
    pub scope: Scope }

macro_rules! actions {
    ($(($id:literal, $label:literal, $p:literal, $r:literal, $scope:ident)),* $(,)?) => {
        pub static ACTIONS: &[Action] = &[$(Action { id:$id, label:$label, premiere:$p, resolve:$r, scope:Scope::$scope }),*];
    }
}

actions![
    ("session", "Collaboration: Live peer session", "", "", Always),
    ("track-density:0", "Timeline: Automatic track height", "", "", Always),
    ("track-density:1", "Timeline: Compact tracks", "", "", Always),
    ("track-density:2", "Timeline: Medium tracks", "", "", Always),
    ("track-density:3", "Timeline: Large tracks", "", "", Always),
    ("activity", "Window: Background activity", "", "", Always),
    ("review", "Review: Show timecoded notes", "", "", Always),
    ("relink-missing", "Project: Batch relink missing media", "", "", Always),
    ("engine-setup", "Settings: Media engine setup", "", "", Always),
    (
        "relink-media",
        "Project: Relink selected media",
        "",
        "",
        Media
    ),
    ("new", "File: New project", "ctrl+n", "ctrl+n", Always),
    ("open", "File: Open project", "ctrl+o", "ctrl+o", Always),
    ("save", "File: Save project", "ctrl+s", "ctrl+s", Always),
    (
        "save-as",
        "File: Save project as",
        "ctrl+shift+s",
        "ctrl+shift+s",
        Always
    ),
    (
        "import",
        "Project: Import media",
        "ctrl+i",
        "ctrl+i",
        Always
    ),
    (
        "import-timeline",
        "File: Import Premiere / Resolve timeline…",
        "ctrl+shift+i",
        "ctrl+shift+i",
        Always
    ),
    (
        "export-view",
        "File: Export sequence",
        "ctrl+m",
        "ctrl+e",
        Clips
    ),
    ("quit", "File: Quit", "ctrl+q", "ctrl+q", Always),
    ("preset-medium", "Export: Balanced preset", "", "", Always),
    ("preset-high", "Export: High quality preset", "", "", Always),
    ("preset-fast", "Export: Speed first preset", "", "", Always),
    (
        "preset-compact",
        "Export: Smaller file preset",
        "",
        "",
        Always
    ),
    ("undo", "Edit: Undo", "ctrl+z", "ctrl+z", Always),
    ("redo", "Edit: Redo", "ctrl+shift+z", "ctrl+shift+z", Always),
    (
        "copy",
        "Edit: Copy selected clips",
        "ctrl+c",
        "ctrl+c",
        Selection
    ),
    (
        "paste",
        "Edit: Paste clips at playhead",
        "ctrl+v",
        "ctrl+v",
        Always
    ),
    (
        "select-all",
        "Timeline: Select all clips",
        "ctrl+a",
        "ctrl+a",
        Clips
    ),
    (
        "deselect",
        "Timeline: Deselect / cancel tool",
        "escape",
        "escape",
        Always
    ),
    (
        "split",
        "Timeline: Split at playhead",
        "ctrl+k",
        "ctrl+b",
        Clips
    ),
    ("tool-select", "Tool: Selection", "v", "a", Always),
    ("tool-razor", "Tool: Razor", "c", "b", Always),
    ("insert", "Source: Insert selected media", ",", "f9", Media),
    (
        "overwrite",
        "Source: Overwrite selected media",
        ".",
        "f10",
        Media
    ),
    (
        "ripple-delete",
        "Timeline: Ripple delete selected clips",
        "shift+delete",
        "delete",
        Selection
    ),
    (
        "lift",
        "Timeline: Lift selected clips",
        "delete",
        "backspace",
        Selection
    ),
    (
        "ripple-prev",
        "Timeline: Ripple trim previous edit",
        "q",
        "q",
        Clips
    ),
    (
        "ripple-next",
        "Timeline: Ripple trim next edit",
        "w",
        "w",
        Clips
    ),
    (
        "link-clips",
        "Clip: Link selected clips",
        "ctrl+l",
        "ctrl+l",
        Multiple
    ),
    (
        "unlink-clips",
        "Clip: Unlink selected clips",
        "ctrl+shift+l",
        "ctrl+shift+l",
        Selection
    ),
    (
        "linked-selection",
        "Timeline: Toggle linked selection",
        "",
        "",
        Always
    ),
    (
        "ripple-toggle",
        "Timeline: Toggle ripple trimming",
        "",
        "",
        Always
    ),
    (
        "clip-prev",
        "Timeline: Select previous clip",
        "[",
        "[",
        Clips
    ),
    ("clip-next", "Timeline: Select next clip", "]", "]", Clips),
    (
        "nudge-left",
        "Clip: Nudge left one frame",
        "alt+left",
        "alt+left",
        Selection
    ),
    (
        "nudge-right",
        "Clip: Nudge right one frame",
        "alt+right",
        "alt+right",
        Selection
    ),
    (
        "nudge-left-10",
        "Clip: Nudge left ten frames",
        "alt+shift+left",
        "alt+shift+left",
        Selection
    ),
    (
        "nudge-right-10",
        "Clip: Nudge right ten frames",
        "alt+shift+right",
        "alt+shift+right",
        Selection
    ),
    (
        "track-up",
        "Clip: Move to track above",
        "alt+up",
        "alt+up",
        Selection
    ),
    (
        "track-down",
        "Clip: Move to track below",
        "alt+down",
        "alt+down",
        Selection
    ),
    (
        "add-video",
        "Sequence: Add video track",
        "ctrl+alt+v",
        "ctrl+alt+v",
        Always
    ),
    (
        "add-audio",
        "Sequence: Add audio track",
        "ctrl+alt+a",
        "ctrl+alt+a",
        Always
    ),
    ("mute-track", "Audio: Mute selected track", "m", "m", Always),
    ("solo-track", "Audio: Solo selected track", "", "", Always),
    (
        "play-toggle",
        "Playback: Play / pause",
        "space",
        "space",
        Clips
    ),
    ("shuttle-j", "Playback: Shuttle reverse", "j", "j", Clips),
    ("shuttle-k", "Playback: Stop shuttle", "k", "k", Always),
    ("shuttle-l", "Playback: Shuttle forward", "l", "l", Clips),
    ("mark-in", "Sequence: Mark in", "i", "i", Always),
    ("mark-out", "Sequence: Mark out", "o", "o", Always),
    (
        "goto-in",
        "Sequence: Go to in point",
        "shift+i",
        "shift+i",
        Always
    ),
    (
        "goto-out",
        "Sequence: Go to out point",
        "shift+o",
        "shift+o",
        Always
    ),
    (
        "clear-io",
        "Sequence: Clear in and out",
        "ctrl+shift+x",
        "alt+x",
        Always
    ),
    (
        "step-back",
        "Playback: Previous frame",
        "left",
        "left",
        Always
    ),
    ("step-fwd", "Playback: Next frame", "right", "right", Always),
    (
        "step-back-second",
        "Playback: Back one second",
        "shift+left",
        "shift+left",
        Always
    ),
    (
        "step-fwd-second",
        "Playback: Forward one second",
        "shift+right",
        "shift+right",
        Always
    ),
    ("prev-edit", "Timeline: Previous edit", "up", "up", Clips),
    ("next-edit", "Timeline: Next edit", "down", "down", Clips),
    ("home", "Timeline: Go to start", "home", "home", Always),
    ("end", "Timeline: Go to end", "end", "end", Clips),
    ("zoom-in", "Timeline: Zoom in", "=", "=", Always),
    ("zoom-out", "Timeline: Zoom out", "-", "-", Always),
    (
        "zoom-fit",
        "Timeline: Fit sequence",
        "\\",
        "ctrl+0",
        Always
    ),
    ("snap", "Timeline: Toggle snapping", "s", "n", Always),
    ("sequence", "Sequence: Settings and format", "", "", Always),
    (
        "workspace-edit",
        "Workspace: Editing",
        "ctrl+1",
        "ctrl+1",
        Always
    ),
    (
        "workspace-media",
        "Workspace: Media",
        "ctrl+2",
        "ctrl+2",
        Always
    ),
    (
        "workspace-color",
        "Workspace: Color",
        "ctrl+3",
        "ctrl+3",
        Always
    ),
    (
        "workspace-audio",
        "Workspace: Audio mixing",
        "ctrl+4",
        "ctrl+4",
        Always
    ),
    ("focus-controls", "Panel: Navigate compact controls", "f6", "f6", Always),
    ("focus-menu", "Menu: Focus menu bar", "f10", "alt+m", Always),
    ("panel-next", "Panel: Focus next", "tab", "tab", Always),
    (
        "panel-prev",
        "Panel: Focus previous",
        "shift+tab",
        "shift+tab",
        Always
    ),
    (
        "panel-source",
        "Panel: Focus Source",
        "shift+2",
        "shift+1",
        Always
    ),
    (
        "panel-program",
        "Panel: Focus Program",
        "shift+4",
        "shift+2",
        Always
    ),
    (
        "panel-project",
        "Panel: Focus Project",
        "shift+1",
        "shift+3",
        Always
    ),
    (
        "panel-timeline",
        "Panel: Focus Timeline",
        "shift+3",
        "shift+4",
        Always
    ),
    (
        "panel-properties",
        "Panel: Focus Properties",
        "shift+5",
        "shift+5",
        Always
    ),
    (
        "toggle-panel-expand",
        "Panel: Expand Program / Timeline",
        "`",
        "",
        Always
    ),
    (
        "layout-save",
        "Workspace: Save current layout",
        "",
        "",
        Always
    ),
    (
        "layout-restore",
        "Workspace: Restore saved layout",
        "",
        "",
        Always
    ),
    ("layout-reset", "Workspace: Reset layout", "", "", Always),
    (
        "toggle-project-dock",
        "Panel: Swap Source and Project docks",
        "",
        "",
        Always
    ),
    (
        "commands",
        "Help: Search commands",
        "ctrl+shift+p",
        "ctrl+shift+p",
        Always
    ),
    (
        "preferences",
        "Settings: Keyboard shortcuts",
        "ctrl+alt+k",
        "ctrl+alt+k",
        Always
    ),
    (
        "reset-grade",
        "Color: Reset selected clip grade",
        "",
        "",
        Selection
    ),
    (
        "reset-motion",
        "Clip: Reset selected clip motion",
        "",
        "",
        Selection
    ),
    (
        "reset-opacity",
        "Clip: Reset selected clip opacity",
        "",
        "",
        Selection
    ),
];

impl Action {
    pub fn default_binding(&self, profile: ShortcutProfile) -> &str {
        match profile {
            ShortcutProfile::Premiere => self.premiere,
            ShortcutProfile::Resolve => self.resolve }
    }
}

pub fn chord(key: &str, mods: Mods) -> String {
    let key = match key {
        " " => "space",
        "\t" => "tab",
        _ => key }
    .to_lowercase();
    // Slint/winit reports logical characters. On a US keyboard Shift+1 is
    // "!", for example; store the underlying key consistently with the modifier.
    let key = if mods.shift {
        match key.as_str() {
            "!" => "1",
            "@" => "2",
            "#" => "3",
            "$" => "4",
            "%" => "5",
            "^" => "6",
            "&" => "7",
            "*" => "8",
            "(" => "9",
            ")" => "0",
            "+" => "=",
            "_" => "-",
            "{" => "[",
            "}" => "]",
            ":" => ";",
            "\"" => "'",
            "<" => ",",
            ">" => ".",
            "?" => "/",
            "|" => "\\",
            "~" => "`",
            _ => &key }
        .to_string()
    } else {
        key
    };
    let mut parts = Vec::new();
    if mods.ctrl {
        parts.push("ctrl");
    }
    if mods.alt {
        parts.push("alt");
    }
    if mods.shift {
        parts.push("shift");
    }
    parts.push(&key);
    parts.join("+")
}

pub fn display_binding(binding: &str) -> String {
    if binding.is_empty() {
        return "Unassigned".into();
    }
    // Only split modifier prefixes: the key itself can be "+", including
    // saved custom chords such as "ctrl++". Keep the stored chord unchanged.
    let mut parts = Vec::new();
    let mut key = binding;
    while let Some((modifier, rest)) = key.split_once('+') {
        let label = match modifier {
            "ctrl" => "Ctrl",
            "alt" => "Alt",
            "shift" => "Shift",
            _ => break };
        parts.push(label.to_string());
        key = rest;
    }
    parts.push(match key {
        "left" => "←".into(),
        "right" => "→".into(),
        "up" => "↑".into(),
        "down" => "↓".into(),
        "space" => "Space".into(),
        "delete" => "Delete".into(),
        "backspace" => "Backspace".into(),
        "escape" => "Esc".into(),
        "tab" => "Tab".into(),
        "return" => "Enter".into(),
        "home" => "Home".into(),
        "end" => "End".into(),
        "pageup" => "Page Up".into(),
        "pagedown" => "Page Down".into(),
        "insert" => "Insert".into(),
        "+" => "Plus".into(),
        "\\" => "Backslash".into(),
        "`" => "Backtick".into(),
        other => other.to_uppercase() });
    parts.join("+")
}

