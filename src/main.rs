#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod cli;
mod color;
mod commands;
mod export;
mod import;
mod live_preview;
mod platform_drag_drop;
mod playback;
mod preferences;
mod project;
mod shortcuts;
mod source;
mod timeline;
mod transitions;
mod update;

mod app;

fn main() {
    prefer_responsive_linux_backend();
    match cli::run_if_requested() {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            eprintln!("Quaternio: {error}");
            std::process::exit(1);
        }
    }
    if let Err(err) = app::run() {
        eprintln!("Quaternio: {err:#}");
        std::process::exit(1);
    }
}

/// Wayland + software renderer can stop repainting (Slint #7663): clicks land but
/// button chrome never redraws. Prefer FemtoVG unless the user already chose a backend.
fn prefer_responsive_linux_backend() {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("SLINT_BACKEND").is_none() {
            // SAFETY: called once before the Slint/winit event loop starts.
            unsafe {
                std::env::set_var("SLINT_BACKEND", "winit-femtovg");
            }
        }
    }
}
