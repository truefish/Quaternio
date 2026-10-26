//! A local recent-project library. Removing an entry never deletes a project.
use super::*;
use crate::preferences::RecentProject;
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    sync::atomic::AtomicU64,
    time::{SystemTime, UNIX_EPOCH},
};

const LIMIT: usize = 50;
#[derive(Default)]
pub(super) struct Home {
    query: Option<(String, i32, bool)>,
    posters: Arc<AtomicU64>,
    poster_seen: u64,
}
fn key(path: &Path) -> String {
    let absolute;
    let path = if path.is_absolute() {
        path
    } else {
        absolute = std::env::current_dir().unwrap_or_default().join(path);
        &absolute
    };
    format!(
        "{:x}",
        Sha256::digest(media_import::path_key(path).as_os_str().as_encoded_bytes())
    )
}
fn poster(path: &Path) -> PathBuf {
    crate::preferences::data_dir()
        .join("project-posters")
        .join(format!("{}.png", key(path)))
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn age(opened: u64) -> String {
    match now().saturating_sub(opened) / 86400 {
        0 => "Opened today".into(),
        1 => "Opened yesterday".into(),
        n => format!("Opened {n} days ago"),
    }
}
fn remember_entry(entries: &mut Vec<RecentProject>, path: PathBuf, project: &Project) {
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let identity = key(&path);
    let pinned = entries
        .iter()
        .find(|e| key(&e.path) == identity)
        .is_some_and(|e| e.pinned);
    entries.retain(|e| key(&e.path) != identity);
    entries.insert(
        0,
        RecentProject {
            path,
            name: project.name.clone(),
            opened: now(),
            pinned,
            details: format!(
                "{} × {} · {} fps · {} sequence{}",
                project.sequence.width,
                project.sequence.height,
                project.sequence.frame_rate.label(),
                project.sequences.len() + 1,
                if project.sequences.is_empty() {
                    ""
                } else {
                    "s"
                }
            ),
        },
    );
    entries.sort_by(|a, b| b.pinned.cmp(&a.pinned).then(b.opened.cmp(&a.opened)));
    entries.truncate(LIMIT);
}
pub(super) fn remember(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>, capture: bool) {
    let destination = {
        let mut s = state.borrow_mut();
        let Some(path) = s.editor.path.clone() else {
            return;
        };
        if collab::guest(&s) {
            return;
        }
        let AppState {
            editor,
            preferences,
            ..
        } = &mut *s;
        remember_entry(
            &mut preferences.recent_projects,
            path.clone(),
            &editor.project,
        );
        s.project_home.query = None;
        (
            poster(&path),
            capture
                && s.presented_picture
                    .is_some_and(|(generation, _)| generation == s.picture_gen),
            s.project_home.posters.clone(),
        )
    };
    save_preferences(state);
    if destination.1 {
        if let Some(pixels) = ui.get_viewer_image().to_rgb8() {
            let image = image::RgbImage::from_raw(
                pixels.width(),
                pixels.height(),
                pixels.as_bytes().to_vec(),
            );
            std::thread::spawn(move || {
                if let Some(image) = image {
                    let image = image::imageops::thumbnail(&image, 384, 216);
                    if let Some(parent) = destination.0.parent() {
                        if std::fs::create_dir_all(parent).is_err() {
                            return;
                        }
                    }
                    let temporary = destination
                        .0
                        .with_extension(format!("{}.png", uuid::Uuid::new_v4()));
                    if image
                        .save_with_format(&temporary, image::ImageFormat::Png)
                        .is_ok()
                    {
                        if std::fs::rename(&temporary, &destination.0).is_ok() {
                            destination.2.fetch_add(1, Ordering::Release);
                        }
                    }
                    let _ = std::fs::remove_file(temporary);
                }
            });
        }
    }
}
pub(super) fn sync(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    let query = ui.get_home_query().to_lowercase();
    let sort = ui.get_home_sort();
    let mut s = state.borrow_mut();
    let list = s.preferences.project_list_view;
    s.project_home.query = Some((query.clone(), sort, list));
    s.project_home.poster_seen = s.project_home.posters.load(Ordering::Acquire);
    ui.set_home_list_view(list);
    ui.set_home_can_resume(
        s.editor.path.is_some() || s.editor.dirty || !s.editor.project.media.is_empty(),
    );
    let mut entries: Vec<_> = s
        .preferences
        .recent_projects
        .iter()
        .take(LIMIT)
        .filter(|e| {
            query.split_whitespace().all(|word| {
                e.name.to_lowercase().contains(word)
                    || e.path.to_string_lossy().to_lowercase().contains(word)
            })
        })
        .cloned()
        .collect();
    entries.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then_with(|| {
                if sort == 1 {
                    a.name.to_lowercase().cmp(&b.name.to_lowercase())
                } else {
                    b.opened.cmp(&a.opened)
                }
            })
            .then(a.path.cmp(&b.path))
    });
    let rows: Vec<_> = entries
        .iter()
        .map(|e| RecentProjectRow {
            key: key(&e.path).into(),
            name: e.name.clone().into(),
            path: e.path.to_string_lossy().into_owned().into(),
            details: e.details.clone().into(),
            age: age(e.opened).into(),
            pinned: e.pinned,
            cover: {
                let path = poster(&e.path);
                if path.is_file() {
                    slint::Image::load_from_path(&path).unwrap_or_default()
                } else {
                    slint::Image::default()
                }
            },
        })
        .collect();
    let selected = ui.get_home_selection().key;
    ui.set_home_selection(
        rows.iter()
            .find(|row| row.key == selected)
            .or(rows.first())
            .cloned()
            .unwrap_or_default(),
    );
    ui.set_home_projects(model(rows));
}
pub(super) fn initialize(
    ui: &AppWindow,
    state: &Rc<std::cell::RefCell<AppState>>,
    resumed: bool,
    explicit_view: bool,
) {
    let show = {
        let s = state.borrow();
        !resumed
            && !explicit_view
            && s.editor.path.is_none()
            && !s.editor.dirty
            && s.editor.project.media.is_empty()
    };
    ui.set_show_project_manager(show);
    if show {
        let status = state.borrow().status.clone();
        ui.set_home_status(if status == "Idle" {
            "".into()
        } else {
            status.into()
        });
        sync(ui, state);
    } else if state.borrow().editor.path.is_some() {
        remember(ui, state, false);
    }
}

pub(super) fn pump(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    if ui.get_show_project_manager() {
        let current = (
            ui.get_home_query().to_lowercase(),
            ui.get_home_sort(),
            state.borrow().preferences.project_list_view,
        );
        let posters_changed = {
            let s = state.borrow();
            s.project_home.poster_seen != s.project_home.posters.load(Ordering::Acquire)
        };
        if state.borrow().project_home.query.as_ref() != Some(&current) || posters_changed {
            sync(ui, state);
        }
    }
}
pub(super) fn handle(
    ui: &AppWindow,
    state: &Rc<std::cell::RefCell<AppState>>,
    command: &str,
) -> bool {
    match command {
        "project-manager" => {
            remember(ui, state, true);
            stop_play(state);
            ui.set_show_creator(false);
            ui.set_show_session(false);
            ui.set_show_activity(false);
            ui.set_show_review(false);
            ui.set_open_menu(0);
            ui.set_show_project_manager(true);
            ui.set_home_status("".into());
            sync(ui, state);
            return true;
        }
        "project-resume" => {
            ui.set_show_project_manager(false);
            return true;
        }
        "project-grid" | "project-list" => {
            state.borrow_mut().preferences.project_list_view = command == "project-list";
            save_preferences(state);
            sync(ui, state);
            return true;
        }
        _ => {}
    }
    for prefix in [
        "project-open:",
        "project-pin:",
        "project-forget:",
        "project-locate:",
    ] {
        let Some(id) = command.strip_prefix(prefix) else {
            continue;
        };
        let entry = state
            .borrow()
            .preferences
            .recent_projects
            .iter()
            .find(|e| key(&e.path) == id)
            .cloned();
        let Some(entry) = entry else {
            return true;
        };
        match prefix {
            "project-open:" => {
                open_project_path(ui, state, entry.path);
                if ui.get_show_project_manager() {
                    ui.set_home_status(state.borrow().status.clone().into());
                }
            }
            "project-pin:" => {
                let mut s = state.borrow_mut();
                for e in &mut s.preferences.recent_projects {
                    if key(&e.path) == id {
                        e.pinned = !e.pinned;
                    }
                }
                drop(s);
                save_preferences(state);
                sync(ui, state);
            }
            "project-forget:" => {
                state
                    .borrow_mut()
                    .preferences
                    .recent_projects
                    .retain(|e| key(&e.path) != id);
                save_preferences(state);
                sync(ui, state);
            }
            "project-locate:" => {
                if let Some(path) = rfd::FileDialog::new()
                    .set_title("Locate project")
                    .add_filter("Quaternio", &["quaternio"])
                    .pick_file()
                {
                    open_project_path(ui, state, path.clone());
                    if state.borrow().editor.path.as_ref() == Some(&path) && key(&path) != id {
                        state
                            .borrow_mut()
                            .preferences
                            .recent_projects
                            .retain(|e| key(&e.path) != id);
                        save_preferences(state);
                    }
                    if ui.get_show_project_manager() {
                        ui.set_home_status(state.borrow().status.clone().into());
                    }
                }
            }
            _ => {}
        }
        return true;
    }
    false
}
