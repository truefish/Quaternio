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
