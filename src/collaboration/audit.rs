//! Append-only, checksummed session log. A complete flushed record is the
//! acknowledgement boundary; a partial final record is recoverable after a crash.
use std::fs::{File,OpenOptions};
use std::io::{Read,Write,Seek,SeekFrom};
use std::path::{Path,PathBuf};
use serde::{Serialize,Deserialize};
use sha2::{Sha256,Digest};
use super::document::{Document,Member,Applied,Patch,Authority,MAX_BYTES};

#[derive(Serialize,Deserialize)]
pub enum Record {
    Start{version:u32,epoch:String,document:Document},
    Member{actor:String,member:Member},
    Upgrade{document:Document},
    Commit(Applied),Rejected{actor:String,patch:Patch,reason:String},
}
pub struct Audit {file:File,poisoned:bool}
impl Audit {
    pub fn create(path:PathBuf,authority:&Authority)->Result<Self,String>{
        if let Some(parent)=path.parent(){std::fs::create_dir_all(parent).map_err(|e|e.to_string())?;}
        let file=OpenOptions::new().create_new(true).read(true).write(true).open(&path).map_err(|e|e.to_string())?;
        fs2::FileExt::try_lock_exclusive(&file).map_err(|e|e.to_string())?;
        let mut audit=Self{file,poisoned:false};
        audit.append(&Record::Start{version:1,epoch:authority.epoch.clone(),document:authority.document.clone()})?;Ok(audit)
    }
    pub fn append(&mut self,record:&Record)->Result<(),String>{
        if self.poisoned{return Err("Session log needs recovery before further edits".into());}
        let bytes=serde_json::to_vec(record).map_err(|e|e.to_string())?;
        if bytes.len()>MAX_BYTES{return Err("Session record exceeds 32 MiB".into());}
        let start=self.file.seek(SeekFrom::End(0)).map_err(|e|e.to_string())?;
        if start+bytes.len() as u64+36>256*1024*1024{return Err("Session log reached 256 MiB; save and start a fresh session".into());}
        let result=self.file.write_all(&(bytes.len() as u32).to_be_bytes()).and_then(|()|self.file.write_all(&Sha256::digest(&bytes)))
            .and_then(|()|self.file.write_all(&bytes)).and_then(|()|self.file.sync_data());
        if let Err(error)=result {
            if self.file.set_len(start).and_then(|()|self.file.seek(SeekFrom::Start(start)).map(|_|())).is_err(){self.poisoned=true;}
            return Err(format!("Session edit could not be persisted: {error}"));
        }
        Ok(())
    }
    pub fn recover(path:&Path)->Result<(Self,Authority,Vec<(Patch,String)>),String>{
        let mut file=OpenOptions::new().read(true).write(true).open(path).map_err(|e|e.to_string())?;
        fs2::FileExt::try_lock_exclusive(&file).map_err(|_|"This session already has an active host".to_string())?;
        if file.metadata().map_err(|e|e.to_string())?.len()>256*1024*1024{return Err("Session log exceeds 256 MiB".into());}
        let mut records=Vec::new();let mut valid=0;
        loop {
            let mut header=[0u8;36];match file.read_exact(&mut header){Ok(())=>{},Err(e)if e.kind()==std::io::ErrorKind::UnexpectedEof=>break,Err(e)=>return Err(e.to_string())}
            let len=u32::from_be_bytes(header[..4].try_into().unwrap())as usize;if len>MAX_BYTES{return Err("Invalid session record length".into());}
            let mut bytes=vec![0;len];match file.read_exact(&mut bytes){Ok(())=>{},Err(e)if e.kind()==std::io::ErrorKind::UnexpectedEof=>break,Err(e)=>return Err(e.to_string())}
            if Sha256::digest(&bytes).as_slice()!=&header[4..]{return Err("Session log checksum mismatch; original log retained".into());}
            records.push(serde_json::from_slice::<Record>(&bytes).map_err(|e|e.to_string())?);valid=file.stream_position().map_err(|e|e.to_string())?;
        }
        let mut records=records.into_iter();let Some(Record::Start{version:1,epoch,document})=records.next()else{return Err("Missing session log header".into());};
        let project=super::document::materialize(&document)?;let mut authority=Authority::new(&project,epoch)?;authority.document=document;let mut rejected=Vec::new();
        for record in records {match record {
            Record::Member{actor,member}=>{if authority.members.len()>=64&&!authority.members.contains_key(&actor){return Err("Session participant limit exceeded".into());}authority.members.insert(actor,member);},
            Record::Commit(expected)=>{
                let actor=expected.patch.id.author.clone();
                let actual=if expected.undoes.is_some(){authority.undo(&actor,expected.patch.id.clone())?}else if expected.redid.is_some(){authority.redo(&actor,expected.patch.id.clone())?}else{authority.submit(&actor,expected.submitted.clone())?};
                if expected!=actual{return Err("Session replay diverged; original log retained".into());}
            },
            Record::Upgrade{document}=>{let expected=super::document::capture(&super::document::materialize(&authority.document)?);if document!=expected{return Err("Invalid session schema upgrade; original log retained".into());}authority.document=document;},
            Record::Rejected{patch,reason,..}=>{rejected.push((patch,reason));},
            _=>return Err("Unexpected session log header".into()),
        }}
        file.set_len(valid).and_then(|()|file.sync_data()).map_err(|e|e.to_string())?;file.seek(SeekFrom::End(0)).map_err(|e|e.to_string())?;
        let mut audit=Self{file,poisoned:false};
        if authority.document.get("document/version").and_then(|v|v.as_u64()).is_some_and(|v|v<crate::project::PROJECT_VERSION as u64){
            let document=super::document::capture(&super::document::materialize(&authority.document)?);
            audit.append(&Record::Upgrade{document:document.clone()})?;authority.document=document;
        }
        Ok((audit,authority,rejected))
    }
}
