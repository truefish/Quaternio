//! Durable client outbox and an explicit local branch for rejected/offline work.
use std::path::{Path,PathBuf};
use std::io::Read;
use serde::{Serialize,Deserialize};
use super::document::{self,Document,Patch,Applied,MAX_BYTES};
use crate::project::Project;

#[derive(Clone,Serialize,Deserialize)]
struct Data{version:u32,epoch:String,actor:String,document:Document,revision:u64,pending:Vec<Patch>,branch:Document,rejected:Vec<(Patch,String)>}
pub struct Outbox{data:Data,path:PathBuf,_lease:std::fs::File}
impl Outbox{
    pub fn new(epoch:String,actor:String,document:Document,revision:u64)->Result<Self,String>{
        let path=crate::preferences::data_dir().join("sessions").join(format!("{epoch}-{actor}.qoutbox"));
        Self::create(path,Data{version:1,epoch,actor,branch:document.clone(),document,revision,pending:Vec::new(),rejected:Vec::new()})
    }
    fn create(path:PathBuf,data:Data)->Result<Self,String>{
        std::fs::create_dir_all(path.parent().unwrap()).map_err(|e|e.to_string())?;
        let lease=std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(path.with_extension("outbox-lock")).map_err(|e|e.to_string())?;
        fs2::FileExt::try_lock_exclusive(&lease).map_err(|_|"This invitation already has an active local client".to_string())?;
        let result=Self{data,path,_lease:lease};
        // Never overwrite a previous client's unacknowledged branch.
        if result.path.exists(){return Err("This client has a retained outbox. Recover it as a branch before reusing this invitation.".into());}
        result.persist(&result.data)?;Ok(result)
    }
    fn persist(&self,data:&Data)->Result<(),String>{let bytes=serde_json::to_vec(data).map_err(|e|e.to_string())?;if bytes.len()>MAX_BYTES*4{return Err("Client outbox reached its size limit; save a local branch".into());}crate::project::write_atomic(&self.path,&bytes)}
    pub fn resnapshot(&mut self,document:Document,revision:u64,next_sequence:u64)->Result<Vec<Patch>,String>{
        document::materialize(&document)?;let mut next=self.data.clone();next.document=document;next.revision=revision;
        next.pending.retain(|patch|patch.id.sequence>=next_sequence);if next.pending.is_empty()&&next.rejected.is_empty(){next.branch=next.document.clone();}
        self.persist(&next)?;self.data=next;Ok(self.data.pending.clone())
    }
    pub fn enqueue(&mut self,patch:Patch,branch:Document)->Result<(),String>{
        if self.data.pending.len()>=16{return Err("Client outbox is full; local branch retained".into());}
        document::materialize(&branch)?;
        let mut next=self.data.clone();next.pending.push(patch);next.branch=branch;self.persist(&next)?;self.data=next;Ok(())
    }
    pub fn accept(&mut self,commit:&Applied)->Result<(),String>{
        let mut next=self.data.clone();document::apply_commit(&mut next.document,&mut next.revision,commit)?;next.pending.retain(|p|p.id!=commit.patch.id);
        if next.pending.is_empty()&&next.rejected.is_empty(){next.branch=next.document.clone();}
        self.persist(&next)?;self.data=next;Ok(())
    }
    pub fn reject(&mut self,id:&super::OperationId,reason:&str)->Result<(),String>{
        let mut next=self.data.clone();if let Some(patch)=next.pending.iter().find(|p|&p.id==id).cloned(){next.rejected.push((patch,reason.into()));}next.pending.retain(|p|&p.id!=id);
        if next.rejected.len()>64{return Err("Retained conflict limit reached; save a branch before continuing".into());}self.persist(&next)?;self.data=next;Ok(())
    }
}
pub fn recover_branch(path:&Path)->Result<Project,String>{
    let mut bytes=Vec::new();std::fs::File::open(path).map_err(|e|e.to_string())?.take((MAX_BYTES*4+1)as u64).read_to_end(&mut bytes).map_err(|e|e.to_string())?;
    if bytes.len()>MAX_BYTES*4{return Err("Client outbox exceeds its size limit".into());}
    let data:Data=serde_json::from_slice(&bytes).map_err(|e|e.to_string())?;
    if data.version!=1||data.pending.len()>16||data.rejected.len()>64{return Err("Unsupported client outbox".into());}
    document::materialize(&data.branch)
}
