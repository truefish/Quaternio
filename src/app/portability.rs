use super::*;
pub(super) struct Job {
    cancel: Arc<AtomicBool>,
    status: Arc<std::sync::Mutex<String>>,
    rx: Receiver<Result<PathBuf, String>>,
}
impl Drop for Job {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}
pub(super) fn handle(
    ui: &AppWindow,
    state: &Rc<std::cell::RefCell<AppState>>,
    command: &str,
) -> bool {
    if !command.starts_with("portable-") {
        return false;
    }
    if command == "portable-reveal" {
        let path = PathBuf::from(ui.get_collection_output().as_str());
        if !path.as_os_str().is_empty() { let target = if path.is_dir() { path } else { path.parent().unwrap_or(&path).to_path_buf() }; if let Err(error)=open::that(target){set_status(state,error.to_string());} }
        return true;
    }
    let result = (|| -> Result<(), String> {
        if command == "portable-cancel" {
            if let Some(j) = &state.borrow().collection {
                j.cancel.store(true, Ordering::Release);
            }
            return Ok(());
        }
        if collab::guest(&state.borrow()) {
            return Err(
                "Collect and export timelines on the host, where source media is stored".into(),
            );
        }
        if command == "portable-xml" {
            let Some(path) = rfd::FileDialog::new()
                .add_filter("Final Cut Pro 7 XML", &["xml"])
                .set_file_name("Timeline.xml")
                .save_file()
            else {
                return Ok(());
            };
            let (xml, report) =
                crate::project::portability::timeline_xml(&state.borrow().editor.project)?;
            let summary = if report.is_empty() {
                "Cuts and source timing exported without omissions.".into()
            } else {
                format!(
                    "{} compatibility notes. See the report beside the XML.\n{}",
                    report.len(),
                    report.join("\n")
                )
            };
            // Stage the report first so exported omissions always have an explanation.
            crate::project::write_atomic(
                &path.with_extension("compatibility.txt"),
                summary.as_bytes(),
            )?;
            crate::project::write_atomic(&path, xml.as_bytes())?;
            set_status(
                state,
                format!(
                    "Timeline XML exported · {} compatibility notes",
                    report.len()
                ),
            );
        } else if command == "portable-collect" || command == "portable-archive" {
            if state.borrow().collection.is_some() {
                return Err("A collection is already running".into());
            }
            let Some(parent) = rfd::FileDialog::new()
                .set_title("Choose where to put the new collection")
                .pick_folder()
            else {
                return Ok(());
            };
            let destination = parent.join(format!("Quaternio-{}", uuid::Uuid::new_v4()));
            let project = state.borrow().editor.project.clone();
            let cancel = Arc::new(AtomicBool::new(false));
            let status = Arc::new(std::sync::Mutex::new("Collecting project…".into()));
            let c = cancel.clone();
            let s = status.clone();
            let (tx, rx) = mpsc::channel();
            let archive = command == "portable-archive";
            std::thread::spawn(move || {
                let result =
                    crate::project::portability::collect(&project, &destination, &c, |message| {
                        *s.lock().unwrap() = message
                    })
                    .and_then(|path| {
                        if archive {
                            crate::project::portability::archive(&destination, &c, |message| {
                                *s.lock().unwrap() = message
                            })
                        } else {
                            Ok(path)
                        }
                    });
                let _ = tx.send(result);
            });
            ui.set_collection_output("".into());
            ui.set_collection_busy(true);
            ui.set_collection_status("Collecting project…".into());
            state.borrow_mut().collection = Some(Job { cancel, status, rx });
        } else {
            return Err("Unknown portability action".into());
        }
        Ok(())
    })();
    if let Err(e) = result {
        set_status(state, e);
    }
    full_sync(ui, state);
    true
}
pub(super) fn pump(ui: &AppWindow, state: &Rc<std::cell::RefCell<AppState>>) {
    ui.set_collection_busy(state.borrow().collection.is_some());
    let result = state
        .borrow()
        .collection
        .as_ref()
        .and_then(|j| match j.rx.try_recv() {
            Ok(r) => Some(r),
            Err(mpsc::TryRecvError::Disconnected) => {
                Some(Err("Collection worker stopped unexpectedly".into()))
            }
            Err(_) => None,
        });
    if let Some(result) = result {
        let mut s = state.borrow_mut();
        s.collection.take();
        s.status = match result {
            Ok(path) => { ui.set_collection_output(path.to_string_lossy().into_owned().into()); format!("Saved portable project: {}", path.display()) },
            Err(e) => e,
        };
        ui.set_collection_busy(false);
        ui.set_collection_status(s.status.clone().into());
        drop(s);
        full_sync(ui, state);
    } else if let Some(job) = &state.borrow().collection {
        let status = job.status.lock().unwrap().clone();
        ui.set_collection_status(status.clone().into());
        ui.set_status_text(status.into());
    }
}
