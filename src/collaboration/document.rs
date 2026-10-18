//! ID-addressed document transactions shared by all live peers. Array indexes,
//! local paths, transport position and workspace preferences never enter a patch.
use std::collections::{BTreeMap, BTreeSet};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use crate::project::{Project, PROJECT_VERSION};
use super::OperationId;

pub type Document = BTreeMap<String, Value>;
pub const ID_BLOCK: u64 = 1_000_000;
pub const MAX_BYTES: usize = 32 * 1024 * 1024;

fn flatten(prefix: &str, value: Value, out: &mut Document) {
    if let Value::Object(fields) = value {
        for (key, value) in fields { flatten(&format!("{prefix}/{key}"), value, out); }
    } else { out.insert(prefix.into(), value); }
}
fn insert_object(out: &mut Document, prefix: &str, value: impl Serialize) {
    flatten(prefix, serde_json::to_value(value).expect("serializable document"), out);
}
pub fn capture(project: &Project) -> Document {
    let mut out = Document::new();
    insert_object(&mut out,"document",json!({"version":PROJECT_VERSION,"document_id":project.document_id,"name":project.name,
        "sequence_name":project.sequence.name,"width":project.sequence.width,"height":project.sequence.height,"frame_rate":project.sequence.frame_rate,"bins":project.bins}));
    out.insert("order".into(),json!(project.sequence.tracks.iter().map(|t|t.id).collect::<Vec<_>>()));
    for track in &project.sequence.tracks {
        let mut value = serde_json::to_value(track).unwrap(); value.as_object_mut().unwrap().remove("clips");
        flatten(&format!("track/{}",track.id),value,&mut out);
        for clip in &track.clips {
            let mut value = serde_json::to_value(clip).unwrap(); value["track"] = json!(track.id);
            flatten(&format!("clip/{}",clip.id),value,&mut out);
        }
    }
    for media in &project.media {
        let mut media = media.clone(); media.path.clear(); media.clear_source_marks();
        insert_object(&mut out,&format!("media/{}",media.id),media);
    }
    for transition in &project.sequence.transitions { insert_object(&mut out,&format!("transition/{}",transition.id),transition); }
    for note in &project.review_notes { insert_object(&mut out,&format!("note/{}",note.id),note); }
    out
}
fn unflatten(doc: &Document) -> Result<Value,String> {
    let mut root = json!({});
    for (path,value) in doc {
        if path.len()>256 || path.split('/').count()>8 { return Err("Invalid document property path".into()); }
        let mut target = &mut root;
        let parts:Vec<_> = path.split('/').collect();
        for (i,part) in parts.iter().enumerate() {
            let object = target.as_object_mut().ok_or("Conflicting document paths")?;
            if i+1==parts.len() { object.insert((*part).into(),value.clone()); break; }
            target = object.entry(*part).or_insert_with(||json!({}));
        }
    }
    Ok(root)
}
pub fn materialize(doc: &Document) -> Result<Project,String> {
    if doc.len()>2_000_000 { return Err("Document exceeds supported property count".into()); }
    let mut value = unflatten(doc)?;
    let metadata = value["document"].clone();
    let order:Vec<u64> = serde_json::from_value(value["order"].clone()).map_err(|e|e.to_string())?;
    if order.len()>1024 || order.iter().collect::<BTreeSet<_>>().len()!=order.len() { return Err("Invalid track order".into()); }
    let mut tracks = Vec::new();
    let mut clips:Vec<Value> = value["clip"].as_object().map(|v|v.values().cloned().collect()).unwrap_or_default();
    for id in order {
        let mut track = value["track"][id.to_string()].take();
        if track.is_null() { return Err("A track is missing from the document".into()); }
        let mut placed = Vec::new();
        clips.retain(|clip| {
            if clip["track"].as_u64()==Some(id) { let mut c=clip.clone(); c.as_object_mut().unwrap().remove("track"); placed.push(c); false } else { true }
        });
        track["clips"] = json!(placed); tracks.push(track);
    }
    if !clips.is_empty() || value["track"].as_object().is_some_and(|t|t.values().any(|v|!v.is_null())) { return Err("An object refers to a missing track".into()); }
    let array = |key:&str| value[key].as_object().map(|v|v.values().cloned().collect::<Vec<_>>()).unwrap_or_default();
    let mut project:Project = serde_json::from_value(json!({"version":metadata["version"],"document_id":metadata["document_id"],"name":metadata["name"],
        "sequence":{"name":metadata["sequence_name"],"width":metadata["width"],"height":metadata["height"],"frame_rate":metadata["frame_rate"],"tracks":tracks,"transitions":array("transition"),"playhead":0,"in_point":null,"out_point":null},
        "media":array("media"),"review_notes":array("note"),"bins":metadata["bins"],"next_id":1})).map_err(|e|format!("Invalid shared document: {e}"))?;
    project.migrate()?;
    validate(&project)?;
    project.ensure_integrity();
    // Canonical round trip rejects unknown fields and inconsistent entity keys.
    let canonical=capture(&project);
    if canonical!=*doc {
        let mismatch=canonical.keys().chain(doc.keys()).find(|k|canonical.get(*k)!=doc.get(*k)).map(|k|k.as_str()).unwrap_or("unknown");
        return Err(format!("Shared document has an invalid property at {mismatch}"));
    }
    Ok(project)
}
fn validate(p:&Project)->Result<(),String> {
    if p.name.chars().count()>1024 || p.sequence.name.chars().count()>1024 || p.media.len()>100_000 || p.review_notes.len()>10_000 || p.sequence.width==0 || p.sequence.height==0 || p.sequence.width>16384 || p.sequence.height>16384
        || p.sequence.frame_rate.num==0 || p.sequence.frame_rate.den==0 || !(0.1..=1000.0).contains(&p.sequence.frame_rate.fps()) { return Err("Invalid shared document bounds".into()); }
    let mut ids=BTreeSet::new();
    for id in p.media.iter().map(|m|m.id).chain(p.sequence.tracks.iter().map(|t|t.id)).chain(p.sequence.tracks.iter().flat_map(|t|t.clips.iter().map(|c|c.id)))
        .chain(p.sequence.transitions.iter().map(|t|t.id)).chain(p.review_notes.iter().map(|n|n.id)) {
        if id==0 || id>=i32::MAX as u64 || !ids.insert(id) { return Err("Duplicate or unsupported object identity".into()); }
    }
    for track in &p.sequence.tracks {
        if track.name.chars().count()>1024 { return Err("Track name exceeds 1,024 characters".into()); }
        let mut clips:Vec<_>=track.clips.iter().collect(); clips.sort_by_key(|c|c.timeline_start);
        let mut end=0;
        for c in clips {
            if c.name.chars().count()>1024 || c.timeline_start<end || c.timeline_start>1_000_000_000 || c.duration>1_000_000_000 || c.duration<=0 || c.timeline_start.checked_add(c.duration).is_none()
                || c.source_in<0 || !c.source_time_offset.is_finite() || p.media(c.media_id).is_none() {
                return Err("Edit would create an invalid or overlapping clip range".into());
            }
            end=c.end();
            let mut sanitized=c.transform; sanitized.sanitize();
            if sanitized!=c.transform || !c.volume.is_finite() || !(0.0..=4.0).contains(&c.volume)
                || !c.scale.is_finite() || !(0.01..=10.0).contains(&c.scale) || !c.opacity.is_finite() || !(0.0..=1.0).contains(&c.opacity)
                || !c.pos_x.is_finite() || !c.pos_y.is_finite() { return Err("Invalid clip property".into()); }
        }
    }
    Ok(())
}

// [] is absent; [null] is an explicit null. Plain Option<Value> would decode
// both states as None and break creation/deletion of nullable clip fields.
mod optional_value{
    use super::*;
    pub fn serialize<S:serde::Serializer>(value:&Option<Value>,serializer:S)->Result<S::Ok,S::Error>{
        value.iter().collect::<Vec<_>>().serialize(serializer)
    }
    pub fn deserialize<'de,D:serde::Deserializer<'de>>(deserializer:D)->Result<Option<Value>,D::Error>{
        let mut values=Vec::<Value>::deserialize(deserializer)?;
        match values.len(){0=>Ok(None),1=>Ok(values.pop()),_=>Err(serde::de::Error::custom("Expected an absent or single property value"))}
    }
}
#[derive(Clone,Debug,PartialEq,Serialize,Deserialize)]
pub struct Edit { pub key:String, #[serde(with="optional_value")] pub before:Option<Value>, #[serde(with="optional_value")] pub after:Option<Value> }
#[derive(Clone,Debug,PartialEq,Serialize,Deserialize)]
pub struct Patch { pub epoch:String,pub id:OperationId,pub base:u64,pub edits:Vec<Edit>, #[serde(default)] pub parents:BTreeMap<u64,u64>, #[serde(default)] pub inherit:BTreeSet<String> }
impl Patch {
    pub fn between(epoch:String,id:OperationId,base:u64,before:&Document,after:&Document)->Self {
        let keys:BTreeSet<_>=before.keys().chain(after.keys()).collect();
        let edits:Vec<Edit>=keys.into_iter().filter(|k|before.get(*k)!=after.get(*k)).map(|k|Edit{key:k.clone(),before:before.get(k).cloned(),after:after.get(k).cloned()}).collect();
        let mut parents=BTreeMap::new();
        // Recognize a normal split so a concurrently updated grade/motion is
        // inherited by the new right half rather than copied from an old frame.
        let may_split=edits.iter().any(|e|e.key.starts_with("clip/")&&e.key.ends_with("/id")&&e.before.is_none())
            && edits.iter().any(|e|e.key.starts_with("clip/")&&e.key.ends_with("/duration")&&e.before.is_some());
        if may_split { if let (Ok(a),Ok(b))=(materialize(before),materialize(after)) {
            for new in b.sequence.tracks.iter().flat_map(|t|&t.clips).filter(|c|a.clip(c.id).is_none()) {
                if let Some(parent)=a.sequence.tracks.iter().flat_map(|t|&t.clips).find(|old|old.media_id==new.media_id && old.end()==new.end()
                    && b.clip(old.id).is_some_and(|left|left.end()==new.timeline_start && left.duration<old.duration)
                    && (old.source_time_seconds(new.timeline_start-old.timeline_start,a.sequence.frame_rate)-new.source_start_seconds(a.sequence.frame_rate)).abs()<1e-6) { parents.insert(new.id,parent.id); }
            }
        }
        }
        let mut inherit=BTreeSet::new();
        for (child,parent) in &parents {
            let prefix=format!("clip/{parent}/");
            for (key,value) in before.iter().filter(|(k,_)|k.starts_with(&prefix)&&!structural(k)){
                let target=format!("clip/{child}/{}",&key[prefix.len()..]);if after.get(&target)==Some(value){inherit.insert(target);}
            }
        }
        Self{epoch,id,base,edits,parents,inherit}
    }
}
fn structural(key:&str)->bool {
    if key.starts_with("clip/") {
        let field=key.split('/').nth(2).unwrap_or("");
        return !matches!(field,"name"|"volume"|"pos_x"|"pos_y"|"scale"|"opacity"|"transform"|"grade");
    }
    !key.starts_with("note/") && !(key.starts_with("track/") && matches!(key.split('/').nth(2),Some("name"|"gain"|"pan"|"muted"|"solo"|"locked")))
}
#[derive(Clone,Debug,PartialEq,Serialize,Deserialize)]
pub struct Alternative {pub key:String,pub previous:Option<Value>,pub incoming:Option<Value>,pub previous_author:Option<OperationId>,pub incoming_author:OperationId}
#[derive(Clone,Debug,PartialEq,Serialize,Deserialize)]
pub struct Applied {pub submitted:Patch,pub redid:Option<OperationId>,pub revision:u64,pub patch:Patch,pub inverse:Vec<Edit>,pub previous_sources:Vec<u64>,pub alternatives:Vec<Alternative>,pub undoes:Option<OperationId>}
#[derive(Clone,Serialize,Deserialize)]
pub struct Member {pub name:String,pub editor:bool,pub first:u64,pub limit:u64}
#[derive(Clone)]
pub struct Authority {
    pub document:Document,pub epoch:String,pub revision:u64,pub members:BTreeMap<String,Member>,
    pub log:Vec<Applied>,versions:BTreeMap<String,(u64,OperationId)>,sources:BTreeMap<String,u64>,undone:BTreeSet<OperationId>,redone:BTreeSet<OperationId>,
}
impl Authority {
    pub fn new(p:&Project,epoch:String)->Result<Self,String> {
        let document=capture(p); materialize(&document)?;
        Ok(Self{document,epoch,revision:0,members:BTreeMap::new(),log:Vec::new(),versions:BTreeMap::new(),sources:BTreeMap::new(),undone:BTreeSet::new(),redone:BTreeSet::new()})
    }
    pub fn submit(&mut self,actor:&str,patch:Patch)->Result<Applied,String>{
        let revision=self.revision;let commit=self.apply(actor,patch,None)?;
        if commit.revision>revision{for undo in self.log.iter().filter(|c|c.patch.id.author==actor&&c.undoes.is_some()){self.redone.insert(undo.patch.id.clone());}}
        Ok(commit)
    }
    fn apply(&mut self,actor:&str,mut patch:Patch,undoes:Option<OperationId>)->Result<Applied,String> {
        let member=self.members.get(actor).filter(|m|m.editor).ok_or("Editing permission is required")?;
        if actor!=patch.id.author || (patch.id.sequence==0||patch.id.sequence>1_000_000_000_000) || patch.epoch!=self.epoch || patch.base>self.revision {return Err("Invalid operation identity, epoch or revision".into());}
        if let Some(prior)=self.log.iter().find(|c|c.patch.id==patch.id) {
            return if prior.submitted==patch && prior.undoes==undoes {Ok(prior.clone())}else{Err("Operation identity reused".into())};
        }
        if patch.edits.is_empty() || patch.edits.len()>100_000 || self.log.len()>=10_000 {return Err("Transaction/session size limit reached".into());}
        let submitted=patch.clone();
        let mut keys=BTreeSet::new();let mut candidate=self.document.clone();let mut inverse=Vec::new();let mut previous_sources=Vec::new();let mut alternatives=Vec::new();
        let project=materialize(&self.document)?;
        let existing_entities:BTreeSet<_>=self.document.keys().filter_map(|key|{let mut parts=key.split('/');Some(format!("{}/{}",parts.next()?,parts.next()?))}).collect();
        let locked_clips:BTreeSet<_>=project.sequence.tracks.iter().filter(|t|t.locked).flat_map(|t|t.clips.iter().map(|c|c.id)).collect();
        for edit in &patch.edits {
            if !keys.insert(edit.key.clone()) || edit.key.len()>256 {return Err("Duplicate or invalid transaction property".into());}
            let current=self.document.get(&edit.key).cloned();
            let entity=edit.key.split('/').take(2).collect::<Vec<_>>().join("/");
            let creation=edit.before.is_none() && !existing_entities.contains(&entity);
            let object_id=edit.key.split('/').nth(1).and_then(|id|id.parse::<u64>().ok());
            // Every newly allocated entity belongs to the authenticated peer's range.
            if undoes.is_none() && creation && object_id.is_some_and(|id|id<member.first||id>=member.limit) {return Err("New object is outside this participant's ID range".into());}
            if let Some(id)=object_id.filter(|_|edit.key.starts_with("clip/")) {
                if locked_clips.contains(&id){return Err("The target track is locked".into());}
            }
            let changes_existence=edit.before.is_none()||edit.after.is_none();
            if current!=edit.before {
                if structural(&edit.key)||changes_existence {return Err(format!("Concurrent structural change at {}. Your edit is retained for review.",edit.key));}
                if current!=edit.after {alternatives.push(Alternative{key:edit.key.clone(),previous:current.clone(),incoming:edit.after.clone(),previous_author:self.versions.get(&edit.key).map(|(_,a)|a.clone()),incoming_author:patch.id.clone()});}
            }
            inverse.push(Edit{key:edit.key.clone(),before:edit.after.clone(),after:current});
            previous_sources.push(self.sources.get(&edit.key).copied().unwrap_or(0));
            match &edit.after {Some(v)=>{candidate.insert(edit.key.clone(),v.clone());},None=>{candidate.remove(&edit.key);}}
        }
        // A split right half inherits current independent properties of its parent.
        for (child,parent) in &patch.parents {
            let prefix=format!("clip/{parent}/");let child_prefix=format!("clip/{child}/");
            for (key,value) in self.document.iter().filter(|(k,_)|k.starts_with(&prefix)&&!structural(k)) {
                let target=format!("{}{}",child_prefix,&key[prefix.len()..]);
                if !patch.inherit.contains(&target){continue;}
                if let Some(edit)=patch.edits.iter_mut().find(|e|e.key==target&&e.before.is_none()) {
                    candidate.insert(target,value.clone());edit.after=Some(value.clone());
                }
            }
        }
        let candidate_project=materialize(&candidate)?;
        // Locking a destination track also protects new inserts and cross-track moves.
        for track in &project.sequence.tracks {
            if track.locked && candidate_project.sequence.tracks.iter().find(|t|t.id==track.id).map(|t|&t.clips)!=Some(&track.clips) {return Err("Edit would change a locked track".into());}
        }
        let revision=self.revision+1;
        for key in keys {self.versions.insert(key.clone(),(revision,patch.id.clone()));self.sources.insert(key.clone(),revision);}
        // Adjust inverse preconditions after split inheritance.
        for edit in &mut inverse {edit.before=candidate.get(&edit.key).cloned();}
        let applied=Applied{submitted,redid:None,revision,patch,inverse,previous_sources,alternatives,undoes};
        self.document=candidate;self.revision=revision;self.log.push(applied.clone());Ok(applied)
    }
    pub fn redo(&mut self,actor:&str,id:OperationId)->Result<Applied,String>{
        if let Some(prior)=self.log.iter().find(|c|c.patch.id==id) {return if prior.redid.is_some()&&id.author==actor {Ok(prior.clone())}else{Err("Operation identity reused".into())};}
        let target=self.log.iter().rev().find(|c|c.patch.id.author==actor&&c.undoes.is_some()&&!self.redone.contains(&c.patch.id)).cloned().ok_or("No edits by this author to redo")?;
        if target.patch.edits.iter().any(|e|self.versions.get(&e.key).map(|(r,_)|*r)!=Some(target.revision)){return Err("A later edit changed an affected property; redo requires conflict review".into());}
        let patch=Patch{epoch:self.epoch.clone(),id,base:self.revision,edits:target.inverse.clone(),parents:BTreeMap::new(),inherit:BTreeSet::new()};
        let mut applied=self.apply(actor,patch,None)?;applied.redid=Some(target.patch.id.clone());
        self.log.last_mut().unwrap().redid=applied.redid.clone();self.redone.insert(target.patch.id);Ok(applied)
    }
    pub fn undo(&mut self,actor:&str,id:OperationId)->Result<Applied,String>{
        if let Some(prior)=self.log.iter().find(|c|c.patch.id==id) {return if prior.undoes.is_some()&&id.author==actor {Ok(prior.clone())}else{Err("Operation identity reused".into())};}
        let target=self.log.iter().rev().find(|c|c.patch.id.author==actor&&c.undoes.is_none()&&!self.undone.contains(&c.patch.id)).cloned().ok_or("No edits by this author to undo")?;
        if target.patch.edits.iter().any(|e|self.sources.get(&e.key).copied()!=Some(target.revision)){return Err("A later edit changed an affected property. Review the conflict before undoing.".into());}
        let patch=Patch{epoch:self.epoch.clone(),id,base:self.revision,edits:target.inverse.clone(),parents:BTreeMap::new(),inherit:BTreeSet::new()};
        let applied=self.apply(actor,patch,Some(target.patch.id.clone()))?;
        for (edit,source) in target.patch.edits.iter().zip(target.previous_sources){self.sources.insert(edit.key.clone(),source);}
        self.undone.insert(target.patch.id);Ok(applied)
    }
}

/// Apply an authoritative commit in revision order, including journal replay.
pub fn apply_commit(document:&mut Document,revision:&mut u64,commit:&Applied)->Result<(),String>{
    if commit.revision<=*revision{return Ok(());}
    if commit.revision!=*revision+1{return Err("Session revision gap; reconnect for a fresh snapshot".into());}
    let mut next=document.clone();for edit in &commit.patch.edits {match &edit.after {Some(v)=>{next.insert(edit.key.clone(),v.clone());},None=>{next.remove(&edit.key);}}}
    materialize(&next)?;*document=next;*revision=commit.revision;Ok(())
}
