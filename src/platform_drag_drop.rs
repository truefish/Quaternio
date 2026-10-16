use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;

use i_slint_backend_winit::winit::event::{ElementState, WindowEvent};
use i_slint_backend_winit::winit::keyboard::{Key, NamedKey};
use i_slint_backend_winit::{WinitWindowAccessor, WinitWindowEventResult};

const MAX_PENDING_FILES: usize = 1024;
const MAX_PENDING_PALETTE_KEYS: usize = 256;

#[derive(Default)]
struct DropMailbox {
    paths: VecDeque<PathBuf>,
    overflow_count: usize,
    hover_count: usize,
    palette_active: bool,
    palette_keys: VecDeque<&'static str>,
}

/// Receives native file drops and palette navigation for one Slint window.
///
/// Note: winit 0.30 emits `DroppedFile` / `HoveredFile` on Windows, macOS, and
/// X11. Native Wayland does not deliver those events yet — use Import or run
/// with `WINIT_UNIX_BACKEND=x11` for drag-and-drop on Wayland desktops.
#[derive(Clone)]
pub struct FileDropQueue {
    mailbox: Rc<RefCell<DropMailbox>>,
}

impl FileDropQueue {
    /// Attach on the UI thread after constructing the Quaternio window.
    pub fn attach(window: &slint::Window) -> Self {
        let mailbox = Rc::new(RefCell::new(DropMailbox::default()));
        let receiver = Rc::downgrade(&mailbox);
        window.on_winit_window_event(move |_window, event| {
            if let Some(mailbox) = receiver.upgrade() {
                let mut mailbox = mailbox.borrow_mut();
                match event {
                    WindowEvent::HoveredFile(_) => {
                        mailbox.hover_count = mailbox.hover_count.saturating_add(1);
                    }
                    WindowEvent::HoveredFileCancelled => {
                        mailbox.hover_count = 0;
                    }
                    WindowEvent::DroppedFile(path) => {
                        mailbox.hover_count = 0;
                        if !mailbox.paths.iter().any(|pending| pending == path) {
                            if mailbox.paths.len() < MAX_PENDING_FILES {
                                mailbox.paths.push_back(path.clone());
                            } else {
                                mailbox.overflow_count =
                                    mailbox.overflow_count.saturating_add(1);
                            }
                        }
                    }
                    WindowEvent::KeyboardInput {
                        event,
                        is_synthetic: false,
                        ..
                    } if mailbox.palette_active => {
                        if event.state == ElementState::Pressed {
                            let command = match &event.logical_key {
                                Key::Named(NamedKey::ArrowUp) => Some("palette-up"),
                                Key::Named(NamedKey::ArrowDown) => Some("palette-down"),
                                _ => None,
                            };
                            if let Some(command) = command {
                                if mailbox.palette_keys.len() < MAX_PENDING_PALETTE_KEYS {
                                    mailbox.palette_keys.push_back(command);
                                }
                                return WinitWindowEventResult::PreventDefault;
                            }
                        }
                    }
                    _ => {}
                }
            }
            WinitWindowEventResult::Propagate
        });
        Self { mailbox }
    }

    pub fn take_paths(&self) -> Vec<PathBuf> {
        self.mailbox.borrow_mut().paths.drain(..).collect()
    }

    pub fn take_overflow_count(&self) -> usize {
        std::mem::take(&mut self.mailbox.borrow_mut().overflow_count)
    }

    pub fn is_hovering_files(&self) -> bool {
        self.mailbox.borrow().hover_count > 0
    }

    /// Set immediately when the palette opens or closes, before keyboard dispatch.
    pub fn set_palette_active(&self, active: bool) {
        let mut mailbox = self.mailbox.borrow_mut();
        mailbox.palette_active = active;
        if !active {
            mailbox.palette_keys.clear();
        }
    }

    pub fn take_palette_keys(&self) -> Vec<&'static str> {
        self.mailbox.borrow_mut().palette_keys.drain(..).collect()
    }
}
