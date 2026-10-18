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
    match export::worker::run_if_requested() {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            eprintln!("Quaternio export worker: {error}");
            std::process::exit(1);
        }
    }
    match cli::run_if_requested() {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            eprintln!("Quaternio: {error}");
            std::process::exit(1);
        }
    }
    if let Err(error) = configure_linux_backend() {
        eprintln!("Quaternio: {error}");
        std::process::exit(1);
    }
    if let Err(err) = app::run() {
        eprintln!("Quaternio: {err:#}");
        std::process::exit(1);
    }
}

fn configure_linux_backend() -> Result<(), slint::PlatformError> {
    #[cfg(target_os = "linux")]
    {
        use i_slint_backend_winit::{winit, Backend};
        use winit::platform::wayland::EventLoopBuilderExtWayland;
        use winit::platform::x11::EventLoopBuilderExtX11;

        // winit 0.30 no longer reads WINIT_UNIX_BACKEND. Honor it explicitly;
        // prefer XWayland where available because native Wayland lacks file drops.
        let requested = std::env::var("WINIT_UNIX_BACKEND").unwrap_or_default();
        let display = std::env::var_os("DISPLAY").is_some_and(|s| !s.is_empty());
        let mut events = winit::event_loop::EventLoop::with_user_event();
        match requested.as_str() {
            "x11" => { events.with_x11(); }
            "wayland" => { events.with_wayland(); }
            "" if display => { events.with_x11(); }
            "" => {}
            other => return Err(format!("Unknown WINIT_UNIX_BACKEND '{other}'; use x11 or wayland").into()),
        }

        let renderer = std::env::var("SLINT_BACKEND").unwrap_or_else(|_| "winit-femtovg".into());
        let renderer = renderer.strip_prefix("winit-").unwrap_or(&renderer);
        let renderer = match renderer {
            "winit" | "gl" | "femtovg" => "femtovg",
            "sw" | "software" => "software",
            other => return Err(format!("Unsupported SLINT_BACKEND '{other}'; use winit-femtovg or winit-software").into()),
        };
        let backend = Backend::builder()
            .with_event_loop_builder(events)
            .with_renderer_name(renderer)
            .build()?;
        slint::platform::set_platform(Box::new(backend))
            .map_err(slint::PlatformError::SetPlatformError)?;
    }
    Ok(())
}
