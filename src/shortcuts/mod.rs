use crate::project::ShortcutProfile;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool }




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
