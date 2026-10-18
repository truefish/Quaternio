//! Portable, timecoded review notes. Local view state stays outside the document.
use serde::{Deserialize, Serialize};
use super::Project;

pub const MAX_NOTES: usize = 10_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReviewNote {
    #[serde(default = "note_key")]
    pub key: String,
    pub id: u64,
    pub frame: i64,
    pub author: String,
    pub text: String,
    pub resolved: bool,
}

fn note_key() -> String { uuid::Uuid::new_v4().to_string() }

#[derive(Serialize, Deserialize)]
pub struct ReviewBundle {
    version: u32,
    document_id: String,
    frame_rate: super::FrameRate,
    notes: Vec<ReviewNote>,
}

pub fn validate_notes(notes: &[ReviewNote]) -> Result<(), String> {
    if notes.len() > MAX_NOTES { return Err("Review file exceeds 10,000 notes".into()); }
    let mut keys = std::collections::BTreeSet::new();
    let mut ids = std::collections::BTreeSet::new();
    for n in notes {
        if uuid::Uuid::parse_str(&n.key).is_err() || !keys.insert(&n.key) || (n.id == u64::MAX || n.id == 0 || !ids.insert(n.id))
            || n.frame < 0 || n.frame > 1_000_000_000 || n.author.trim().is_empty()
            || n.author.chars().count() > 80 || n.text.trim().is_empty() || n.text.chars().count() > 4000 {
            return Err("Review notes contain invalid identifiers, timing or text".into());
        }
    }
    Ok(())
}

impl Project {
    pub fn review_bundle(&self) -> Result<Vec<u8>, String> {
        validate_notes(&self.review_notes)?;
        serde_json::to_vec_pretty(&ReviewBundle { version: 1, document_id: self.document_id.clone(),
            frame_rate: self.sequence.frame_rate, notes: self.review_notes.clone() }).map_err(|e| e.to_string())
    }

    /// Import adds unseen notes, retaining local resolution of existing notes.
    /// No media paths or timeline edits are included in this exchange format.
    pub fn import_review_bundle(&mut self, bytes: &[u8]) -> Result<usize, String> {
        if bytes.len() > 64 * 1024 * 1024 { return Err("Review file exceeds 64 MiB".into()); }
        let bundle: ReviewBundle = serde_json::from_slice(bytes).map_err(|e| format!("Invalid review file: {e}"))?;
        if bundle.version != 1 || bundle.document_id != self.document_id {
            return Err("These notes belong to a different project. Share a saved copy of the same project before reviewing.".into());
        }
        if bundle.frame_rate.num == 0 || bundle.frame_rate.den == 0 { return Err("Invalid review timebase".into()); }
        validate_notes(&bundle.notes)?;
        let mut additions = Vec::new();
        for mut note in bundle.notes {
            note.frame = self.sequence.frame_rate.seconds_to_frame(bundle.frame_rate.frame_to_seconds(note.frame));
            if !(0..=1_000_000_000).contains(&note.frame) { return Err("Converted review timing exceeds the supported range".into()); }
            if let Some(existing) = self.review_notes.iter().find(|n| n.key == note.key) {
                if existing.text != note.text || existing.author != note.author || existing.frame != note.frame {
                    return Err("An existing note has a conflicting version; no notes were imported".into());
                }
            } else { additions.push(note); }
        }
        if self.review_notes.len() + additions.len() > MAX_NOTES { return Err("Import would exceed 10,000 review notes".into()); }
        let count = additions.len();
        for mut note in additions { note.id = self.alloc(); self.review_notes.push(note); }
        Ok(count)
    }

    pub fn add_review_note(&mut self, frame: i64, author: &str, text: &str) -> Result<u64, String> {
        if !(0..=1_000_000_000).contains(&frame) { return Err("Note time is outside the supported range".into()); }
        let text = text.trim();
        let author = author.trim();
        if text.is_empty() || text.chars().count() > 4000 {
            return Err("Enter a note between 1 and 4,000 characters".into());
        }
        if author.is_empty() || author.chars().count() > 80 {
            return Err("Enter a reviewer name between 1 and 80 characters".into());
        }
        if self.review_notes.len() >= MAX_NOTES { return Err("This project has reached 10,000 review notes".into()); }
        let id = self.alloc();
        self.review_notes.push(ReviewNote { key: note_key(), id, frame: frame.max(0), author: author.into(), text: text.into(), resolved: false });
        Ok(id)
    }

    pub fn resolve_review_note(&mut self, id: u64) -> Result<(), String> {
        let note = self.review_notes.iter_mut().find(|n| n.id == id).ok_or("This review note no longer exists")?;
        note.resolved = !note.resolved;
        Ok(())
    }
}
