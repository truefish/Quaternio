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
    let key = key.to_ascii_lowercase();
    if mods.ctrl && mods.shift && key == "z" {
        return Some(Command::Redo);
    }
    if mods.ctrl && mods.shift && key == "s" {
        return Some(Command::SaveAs);
    }
    if mods.ctrl && mods.shift && key == "i" {
        return Some(Command::ImportTimeline);
    }
    if mods.ctrl && !mods.shift {
        return match key.as_str() {
            "z" => Some(Command::Undo),
            "y" => Some(Command::Redo),
            "c" => Some(Command::Copy),
            "v" => Some(Command::Paste),
            "s" => Some(Command::Save),
            "o" => Some(Command::Open),
            "n" => Some(Command::New),
            "i" => Some(Command::Import),
            "q" => Some(Command::Quit),
            "a" => Some(Command::SelectAll),
            "k" => match profile {
                ShortcutProfile::Premiere => Some(Command::Split),
                ShortcutProfile::Resolve => None },
            "b" => match profile {
                ShortcutProfile::Premiere => None,
                ShortcutProfile::Resolve => Some(Command::Split) },
            "e" => Some(Command::ExportView),
            "=" | "+" => Some(Command::ZoomIn),
            "-" | "_" => Some(Command::ZoomOut),
            "0" => Some(Command::ZoomFit),
            _ => None };
    }
    if mods.ctrl {
        return None;
    }

    if mods.shift && key == "delete" {
        return Some(match profile {
            ShortcutProfile::Premiere => Command::RippleDelete,
            ShortcutProfile::Resolve => Command::RippleDelete });
    }
    if mods.shift && key == "i" {
        return Some(Command::GotoIn);
    }
    if mods.shift && key == "o" {
        return Some(Command::GotoOut);
    }
    if mods.shift && key == "left" {
        return Some(Command::StepBackSecond);
    }
    if mods.shift && key == "right" {
        return Some(Command::StepForwardSecond);
    }
    if mods.alt && key == "x" {
        return Some(Command::ClearInOut);
    }

    let plain = match key.as_str() {
        " " | "space" => Some(Command::PlayPause),
        "j" => Some(Command::ShuttleReverse),
        "k" => Some(Command::ShuttleStop),
        "l" => Some(Command::ShuttleForward),
        "i" => Some(Command::MarkIn),
        "o" => Some(Command::MarkOut),
        "q" => Some(Command::RipplePrev),
        "w" => Some(Command::RippleNext),
        "left" => Some(Command::StepBack),
        "right" => Some(Command::StepForward),
        "up" => Some(Command::PrevEdit),
        "down" => Some(Command::NextEdit),
        "home" => Some(Command::Home),
        "end" => Some(Command::End),
        "=" | "+" => Some(Command::ZoomIn),
        "-" | "_" => Some(Command::ZoomOut),
        "m" => Some(Command::MuteTrack),
        "escape" => Some(Command::Escape),
        "delete" => Some(match profile {
            ShortcutProfile::Premiere => Command::LiftDelete,
            ShortcutProfile::Resolve => Command::RippleDelete }),
        "backspace" => Some(match profile {
            ShortcutProfile::Premiere => Command::LiftDelete,
            ShortcutProfile::Resolve => Command::LiftDelete }),
        _ => None };
    if plain.is_some() {
        return plain;
    }

    match profile {
        ShortcutProfile::Premiere => match key.as_str() {
            "v" => Some(Command::SelectTool),
            "," => Some(Command::Insert),
            "." => Some(Command::Overwrite),
            "c" => Some(Command::RazorTool),
            "s" => Some(Command::Snap),
            _ => None },
        ShortcutProfile::Resolve => match key.as_str() {
            "f9" => Some(Command::Insert),
            "f10" => Some(Command::Overwrite),
            "b" => Some(Command::RazorTool),
            "a" => Some(Command::SelectTool),
            "n" => Some(Command::Snap),
            _ => None } }
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
            ("Shift+I / O", "Go to in / out"),
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
        ],
        ShortcutProfile::Resolve => &[
            ("I / O", "Mark in / out"),
            ("Shift+I / O", "Go to in / out"),
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

