use crate::commands;
use crate::project::ShortcutProfile;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    New,
    Open,
    Save,
    SaveAs,
    Import,
    ImportTimeline,
    Quit,
    Undo,
    Redo,
    Copy,
    Paste,
    SelectAll,
    Split,
    RazorTool,
    SelectTool,
    Insert,
    Overwrite,
    RippleDelete,
    LiftDelete,
    RipplePrev,
    RippleNext,
    PlayPause,
    ShuttleReverse,
    ShuttleStop,
    ShuttleForward,
    MarkIn,
    MarkOut,
    GotoIn,
    GotoOut,
    ClearInOut,
    StepBack,
    StepForward,
    StepBackSecond,
    StepForwardSecond,
    PrevEdit,
    NextEdit,
    Home,
    End,
    ZoomIn,
    ZoomOut,
    ZoomFit,
    Snap,
    MuteTrack,
    Escape,
    ExportView,
    EditView }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool }

impl Mods {
    pub fn none() -> Self {
        Self {
            ctrl: false,
            shift: false,
            alt: false }
    }
}

pub fn lookup(profile: ShortcutProfile, key: &str, mods: Mods) -> Option<Command> {
    // The catalog is the source of truth for defaults, including exact
    // modifiers. Alt+Left must not fall through to plain Left, for example.
    // The app resolves user overrides before dispatching by action id.
    let chord = commands::chord(key, mods);
    let action = commands::ACTIONS
        .iter()
        .find(|action| {
            let binding = action.default_binding(profile);
            !binding.is_empty() && binding == chord
        })
        .map(|action| action.id)
        // Match the app's convenience aliases only when no default owns the key.
        .or_else(|| match chord.as_str() {
            "ctrl+y" => Some("redo"),
            "ctrl+=" | "ctrl++" | "+" => Some("zoom-in"),
            "ctrl+-" => Some("zoom-out"),
            "backspace" if profile == ShortcutProfile::Premiere => Some("lift"),
            _ => None })?;

    Some(match action {
        "new" => Command::New,
        "open" => Command::Open,
        "save" => Command::Save,
        "save-as" => Command::SaveAs,
        "import" => Command::Import,
        "import-timeline" => Command::ImportTimeline,
        "quit" => Command::Quit,
        "undo" => Command::Undo,
        "redo" => Command::Redo,
        "copy" => Command::Copy,
        "paste" => Command::Paste,
        "select-all" => Command::SelectAll,
        "split" => Command::Split,
        "tool-razor" => Command::RazorTool,
        "tool-select" => Command::SelectTool,
        "insert" => Command::Insert,
        "overwrite" => Command::Overwrite,
        "ripple-delete" => Command::RippleDelete,
        "lift" => Command::LiftDelete,
        "ripple-prev" => Command::RipplePrev,
        "ripple-next" => Command::RippleNext,
        "play-toggle" => Command::PlayPause,
        "shuttle-j" => Command::ShuttleReverse,
        "shuttle-k" => Command::ShuttleStop,
        "shuttle-l" => Command::ShuttleForward,
        "mark-in" => Command::MarkIn,
        "mark-out" => Command::MarkOut,
        "goto-in" => Command::GotoIn,
        "goto-out" => Command::GotoOut,
        "clear-io" => Command::ClearInOut,
        "step-back" => Command::StepBack,
        "step-fwd" => Command::StepForward,
        "step-back-second" => Command::StepBackSecond,
        "step-fwd-second" => Command::StepForwardSecond,
        "prev-edit" => Command::PrevEdit,
        "next-edit" => Command::NextEdit,
        "home" => Command::Home,
        "end" => Command::End,
        "zoom-in" => Command::ZoomIn,
        "zoom-out" => Command::ZoomOut,
        "zoom-fit" => Command::ZoomFit,
        "snap" => Command::Snap,
        "mute-track" => Command::MuteTrack,
        "deselect" => Command::Escape,
        "export-view" => Command::ExportView,
        _ => return None })
}

#[derive(Clone, Copy, Debug)]
pub struct Hints {
    pub insert: &'static str,
    pub overwrite: &'static str,
    pub razor: &'static str,
    pub split: &'static str,
    pub ripple_delete: &'static str,
    pub lift_delete: &'static str,
    pub snap: &'static str }

pub fn cheat_sheet(profile: ShortcutProfile) -> &'static [(&'static str, &'static str)] {
    match profile {
        ShortcutProfile::Premiere => &[
            ("I / O", "Mark in / out"),
            ("Shift+I / Shift+O", "Go to in / out"),
            ("Ctrl+Shift+X", "Clear in / out"),
            ("Q / W", "Ripple trim previous / next"),
            ("V", "Selection"),
            (",", "Insert"),
            (".", "Overwrite"),
            ("C", "Razor"),
            ("Ctrl+K", "Split at playhead"),
            ("Shift+Del", "Ripple delete"),
            ("Del", "Lift"),
            ("J K L", "Shuttle"),
            ("Space", "Play / pause"),
            ("S", "Snap"),
            ("← →", "Step frame"),
            ("Ctrl+Z", "Undo"),
            ("Backslash", "Fit sequence"),
            ("Backtick", "Expand Program / Timeline"),
            ("Shift+1 / Shift+2", "Project / Source panel"),
            ("Shift+3 / Shift+4", "Timeline / Program panel"),
            ("Ctrl+M", "Export sequence"),
        ],
        ShortcutProfile::Resolve => &[
            ("I / O", "Mark in / out"),
            ("Shift+I / Shift+O", "Go to in / out"),
            ("Q / W", "Ripple trim previous / next"),
            ("F9", "Insert"),
            ("F10", "Overwrite"),
            ("B", "Blade"),
            ("Ctrl+B", "Split at playhead"),
            ("Del", "Ripple delete"),
            ("Backspace", "Lift"),
            ("A", "Selection"),
            ("J K L", "Shuttle"),
            ("Space", "Play / pause"),
            ("N", "Snap"),
            ("Ctrl+Z", "Undo"),
        ] }
}

pub fn hints(profile: ShortcutProfile) -> Hints {
    match profile {
        ShortcutProfile::Premiere => Hints {
            insert: ",",
            overwrite: ".",
            razor: "C",
            split: "Ctrl+K",
            ripple_delete: "Shift+Del",
            lift_delete: "Del",
            snap: "S" },
        ShortcutProfile::Resolve => Hints {
            insert: "F9",
            overwrite: "F10",
            razor: "B",
            split: "Ctrl+B",
            ripple_delete: "Del",
            lift_delete: "Backspace",
            snap: "N" } }
}

