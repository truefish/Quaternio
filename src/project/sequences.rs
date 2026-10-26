//! Sequence views are local; source references and sequence contents are shared.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

pub fn main_id() -> String {
    "main".into()
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Targeting {
    pub video: Option<u64>,
    pub audio: Option<u64>,
    pub patch_video: bool,
    pub patch_audio: bool,
}
impl Default for Targeting {
    fn default() -> Self {
        Self {
            video: None,
            audio: None,
            patch_video: true,
            patch_audio: true,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredSequence {
    pub sequence: Sequence,
    #[serde(default)]
    pub notes: Vec<review::ReviewNote>,
}
impl Project {
    pub fn sequence_ids(&self) -> Vec<String> {
        let mut ids: Vec<_> = self
            .sequences
            .keys()
            .cloned()
            .chain(std::iter::once(self.active_sequence.clone()))
            .collect();
        ids.sort_by_key(|id| {
            (
                id != "main",
                self.sequence_by_id(id)
                    .map(|s| s.name.to_lowercase())
                    .unwrap_or_default(),
                id.clone(),
            )
        });
        ids
    }
    pub fn sequence_by_id(&self, id: &str) -> Option<&Sequence> {
        if id == self.active_sequence {
            Some(&self.sequence)
        } else {
            self.sequences.get(id).map(|s| &s.sequence)
        }
    }
    pub fn switch_sequence(&mut self, id: &str) -> Result<(), String> {
        if id == self.active_sequence {
            return Ok(());
        }
        let slot = self
            .sequences
            .remove(id)
            .ok_or("This sequence no longer exists")?;
        let old = StoredSequence {
            sequence: std::mem::replace(&mut self.sequence, slot.sequence),
            notes: std::mem::replace(&mut self.review_notes, slot.notes),
        };
        self.sequences
            .insert(std::mem::replace(&mut self.active_sequence, id.into()), old);
        Ok(())
    }
    pub fn new_sequence(&mut self, duplicate: bool) -> Result<String, String> {
        if self.sequences.len() >= 127 {
            return Err("A project supports up to 128 sequences".into());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let mut sequence = self.sequence.clone();
        sequence.name = if duplicate {
            format!("Copy of {}", self.sequence.name)
        } else {
            format!("Sequence {:02}", self.sequences.len() + 2)
        };
        sequence.playhead = 0;
        sequence.in_point = None;
        sequence.out_point = None;
        sequence.targeting = Default::default();
        if duplicate {
            let mut ids = BTreeMap::new();
            for t in &mut sequence.tracks {
                t.id = self.alloc();
                for c in &mut t.clips {
                    let old = c.id;
                    c.id = self.alloc();
                    ids.insert(old, c.id);
                }
            }
            let mut links = BTreeMap::new();
            for t in &mut sequence.tracks {
                for c in &mut t.clips {
                    if let Some(link) = c.link {
                        let fresh = *links.entry(link).or_insert_with(|| self.alloc());
                        c.link = Some(fresh);
                    }
                }
            }
            for t in &mut sequence.transitions {
                t.id = self.alloc();
                let id = ids[&t.anchor.clip_id()];
                t.anchor = match t.anchor {
                    Anchor::Head(_) => Anchor::Head(id),
                    Anchor::Tail(_) => Anchor::Tail(id),
                    Anchor::Cut(_) => Anchor::Cut(id),
                };
            }
        } else {
            for t in &mut sequence.tracks {
                t.id = self.alloc();
                t.clips.clear();
            }
            sequence.transitions.clear();
            sequence.audio = Default::default();
        }
        self.sequences.insert(
            id.clone(),
            StoredSequence {
                sequence,
                notes: Vec::new(),
            },
        );
        self.switch_sequence(&id)?;
        Ok(id)
    }
    pub fn delete_sequence(&mut self, id: &str) -> Result<(), String> {
        if id == "main" {
            return Err("The original sequence cannot be deleted".into());
        }
        if self.all_sequences().any(|(_, s)| {
            s.tracks
                .iter()
                .flat_map(|t| &t.clips)
                .any(|c| c.creative.sequence.as_deref() == Some(id))
        }) {
            return Err("Remove this sequence's nested clips before deleting it".into());
        }
        if id == self.active_sequence {
            self.switch_sequence("main")?;
        }
        self.sequences
            .remove(id)
            .ok_or("This sequence no longer exists")?;
        Ok(())
    }
    pub fn all_sequences(&self) -> impl Iterator<Item = (&str, &Sequence)> {
        std::iter::once((self.active_sequence.as_str(), &self.sequence)).chain(
            self.sequences
                .iter()
                .map(|(id, s)| (id.as_str(), &s.sequence)),
        )
    }
    pub fn all_object_ids(&self) -> BTreeSet<u64> {
        self.media
            .iter()
            .map(|m| m.id)
            .chain(self.all_sequences().flat_map(|(_, s)| {
                s.tracks
                    .iter()
                    .flat_map(|t| std::iter::once(t.id).chain(t.clips.iter().map(|c| c.id)))
                    .chain(s.transitions.iter().map(|t| t.id))
            }))
            .chain(self.review_notes.iter().map(|n| n.id))
            .chain(
                self.sequences
                    .values()
                    .flat_map(|s| s.notes.iter().map(|n| n.id)),
            )
            .collect()
    }
    pub fn validate_sequences(&self) -> Result<(), String> {
        if self.sequences.len() > 127
            || self.sequences.contains_key(&self.active_sequence)
            || (self.active_sequence != "main" && !self.sequences.contains_key("main"))
        {
            return Err("Invalid sequence collection".into());
        }
        let mut ids = BTreeSet::new();
        for m in &self.media {
            if !ids.insert(m.id) {
                return Err("Duplicate media identity".into());
            }
        }
        for (id, s) in self.all_sequences() {
            if id != "main" && uuid::Uuid::parse_str(id).is_err() {
                return Err("Invalid sequence identity".into());
            }
            if s.name.trim().is_empty()
                || s.name.chars().count() > 1024
                || s.width < 16
                || s.width > 16384
                || s.height < 16
                || s.height > 16384
                || s.frame_rate.num == 0
                || s.frame_rate.den == 0
                || !(0.1..=1000.).contains(&s.frame_rate.fps())
                || s.tracks.len() > 1024
            {
                return Err("Invalid sequence settings".into());
            }
            s.audio.validate()?;
            if let Some(lut) = &s.color.lut {
                lut.validate()?;
            }
            for t in &s.tracks {
                if !ids.insert(t.id) {
                    return Err("Duplicate track identity".into());
                }
                let mut end = 0;
                let mut clips: Vec<_> = t.clips.iter().collect();
                clips.sort_by_key(|c| c.timeline_start);
                for c in clips {
                    if !ids.insert(c.id)
                        || c.duration <= 0
                        || c.duration > 1_000_000_000
                        || c.timeline_start < end
                        || c.timeline_start.checked_add(c.duration).is_none()
                    {
                        return Err("Invalid sequence clip range or identity".into());
                    }
                    c.creative.validate()?;
                    end = c.end();
                    if c.creative
                        .multicam
                        .as_ref()
                        .is_some_and(|m| m.angles.iter().any(|a| self.media(a.media_id).is_none()))
                    {
                        return Err("A multicam source is missing".into());
                    }
                    if let Some(target) = &c.creative.sequence {
                        if self.sequence_by_id(target).is_none() {
                            return Err("A nested sequence is missing".into());
                        }
                    }
                }
            }
            for t in &s.transitions {
                if !ids.insert(t.id) {
                    return Err("Duplicate transition identity".into());
                }
            }
        }
        fn visit<'a>(
            p: &'a Project,
            id: &'a str,
            path: &mut BTreeSet<&'a str>,
            heights: &mut BTreeMap<&'a str, usize>,
        ) -> Result<usize, String> {
            if let Some(height) = heights.get(id) {
                return Ok(*height);
            }
            if !path.insert(id) {
                return Err("Nested sequences cannot form a cycle or exceed eight levels".into());
            }
            let mut height = 0;
            if let Some(s) = p.sequence_by_id(id) {
                for c in s.tracks.iter().flat_map(|t| &t.clips) {
                    if let Some(next) = c.creative.sequence.as_deref() {
                        height = height.max(visit(p, next, path, heights)? + 1);
                        if height > 8 {
                            return Err(
                                "Nested sequences cannot form a cycle or exceed eight levels"
                                    .into(),
                            );
                        }
                    }
                }
            }
            path.remove(id);
            heights.insert(id, height);
            Ok(height)
        }
        let mut heights = BTreeMap::new();
        for (id, _) in self.all_sequences() {
            visit(self, id, &mut BTreeSet::new(), &mut heights)?;
        }
        for slot in self.sequences.values() {
            review::validate_notes(&slot.notes)?;
        }
        Ok(())
    }
}
