use crate::project::{Media, Project};
use crate::timeline::Editor;
use std::collections::HashSet;

#[derive(Default)]
pub(super) struct MediaSelection {
    pub ids: Vec<u64>,
    anchor: Option<u64>,
    active_seen: Option<u64> }

impl MediaSelection {
    pub fn reconcile(&mut self, project: &Project, active: Option<u64>) {
        // Import, recovery and keyboard navigation still choose one Source asset.
        if active != self.active_seen {
            self.ids = active.into_iter().collect();
            self.anchor = active;
            self.active_seen = active;
        }
        let existing: HashSet<_> = project.media.iter().map(|media| media.id).collect();
        self.ids.retain(|id| existing.contains(id));
        if self.anchor.is_some_and(|id| !existing.contains(&id)) {
            self.anchor = None;
        }
    }

    pub fn retain_visible(&mut self, visible: &[u64]) {
        let visible: HashSet<_> = visible.iter().copied().collect();
        self.ids.retain(|id| visible.contains(id));
        if self.anchor.is_some_and(|id| !visible.contains(&id)) {
            self.anchor = None;
        }
    }

    pub fn click(&mut self, id: u64, ctrl: bool, shift: bool, visible: &[u64]) -> Option<u64> {
        let Some(index) = visible.iter().position(|item| *item == id) else {
            return self.active_seen;
        };
        self.retain_visible(visible);
        if shift {
            let anchor = self.anchor.and_then(|id| visible.iter().position(|item| *item == id)).unwrap_or(index);
            if !ctrl { self.ids.clear(); }
            self.ids.extend_from_slice(&visible[anchor.min(index)..=anchor.max(index)]);
            if self.anchor.is_none() { self.anchor = Some(id); }
        } else if ctrl {
            if self.ids.contains(&id) { self.ids.retain(|item| *item != id); }
            else { self.ids.push(id); }
            self.anchor = Some(id);
        } else {
            self.ids = vec![id];
            self.anchor = Some(id);
        }
        // Walk the visible order once: deterministic range order without a
        // linear position lookup for every comparison in a large selection.
        let selected: HashSet<_> = self.ids.iter().copied().collect();
        self.ids = visible.iter().copied().filter(|id| selected.contains(id)).collect();
        self.active_seen = if selected.contains(&id) { Some(id) } else { self.ids.last().copied() };
        self.active_seen
    }
}

#[derive(Default)]
pub(super) struct MediaSort {
    pub column: i32,
    pub descending: bool }

impl MediaSort {
    pub fn choose(&mut self, column: i32) {
        if !matches!(column, 0 | 1) { return; }
        if self.column == column { self.descending = !self.descending; }
        else { self.column = column; self.descending = false; }
    }

    pub fn sort(&self, media: &mut Vec<&Media>) {
        media.sort_by(|a, b| {
            let order = if self.column == 1 {
                a.source_duration_seconds().total_cmp(&b.source_duration_seconds())
            } else { a.name.to_lowercase().cmp(&b.name.to_lowercase()) };
            let order = if self.descending { order.reverse() } else { order };
            order.then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())).then(a.id.cmp(&b.id))
        });
    }
}

pub(super) fn move_to_bin(editor: &mut Editor, ids: &[u64], bin: &str) -> Result<usize, String> {
    if ids.is_empty() { return Err("Select media in Project first".into()); }
    let unique = ids.iter().copied().collect::<std::collections::BTreeSet<_>>();
    editor.edit("move-media-to-bin", |project| {
        for id in &unique { project.move_media_to_bin(*id, bin)?; }
        Ok(unique.len())
    })
}

