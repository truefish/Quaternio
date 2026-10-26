//! Bounded, acknowledged guest-to-host uploads with atomic publication.
use std::{collections::BTreeMap,path::{Path,PathBuf},io::{Read,Write,Seek,SeekFrom},time::{Instant,Duration}};
use serde::{Serialize,Deserialize};
use sha2::{Sha256,Digest};
use base64::{Engine,engine::general_purpose::STANDARD};
const CHUNK:usize=48*1024;
const MAX_FILE:u64=64*1024*1024*1024;
#[derive(Clone,Serialize,Deserialize)]
pub enum Packet{Begin{id:String,name:String,size:u64},Chunk{id:String,offset:u64,data:String},End{id:String,digest:String},Ack{id:String,offset:u64,complete:bool},Cancel{id:String,reason:String}}
impl Packet{pub fn id(&self)->&str{match self{Self::Begin{id,..}|Self::Chunk{id,..}|Self::End{id,..}|Self::Ack{id,..}|Self::Cancel{id,..}=>id}}}
struct Incoming{peer:String,path:PathBuf,destination:PathBuf,file:std::fs::File,size:u64,offset:u64,hash:Sha256,used:Instant,progress:u64}
impl Drop for Incoming{fn drop(&mut self){let _=std::fs::remove_file(&self.path);if let Some(parent)=self.destination.parent(){let _=std::fs::remove_dir(parent);}}}
struct Outgoing{file:std::fs::File,modified:Option<std::time::SystemTime>,size:u64,offset:u64,acked:u64,hash:Sha256,started:bool,begin_sent:bool,ending:bool,used:Instant,name:String,progress:u64,checkpoints:std::collections::VecDeque<(u64,Sha256)>}
struct Completed{peer:String,size:u64}
pub struct Transfers{root:PathBuf,incoming:BTreeMap<String,Incoming>,completed:BTreeMap<String,Completed>,outgoing:BTreeMap<String,Outgoing>,total:u64,errors:Vec<String>}
#[derive(Clone,Debug,PartialEq,Eq)]
pub enum Stage{Queued,Uploading,Receiving,Finishing}
#[derive(Clone,Debug,PartialEq,Eq)]
pub struct Progress{pub id:String,pub name:String,pub done:u64,pub total:u64,pub stage:Stage,pub cancellable:bool}
pub enum Update{None,Progress(String),Received(PathBuf),Complete(String)}
impl Transfers{
    pub fn new(epoch:&str)->Self{Self{root:crate::preferences::data_dir().join("session-media").join(epoch),incoming:BTreeMap::new(),completed:BTreeMap::new(),outgoing:BTreeMap::new(),total:0,errors:Vec::new()}}
    pub fn upload(&mut self,path:PathBuf)->Result<String,String>{
        if self.outgoing.len()>=8{return Err("Wait for queued uploads to finish".into());}
        let file=std::fs::File::open(&path).map_err(|_|"Could not open file for upload")?;let meta=file.metadata().map_err(|_|"Could not inspect upload")?;
        if !meta.is_file()||meta.len()==0||meta.len()>MAX_FILE{return Err("Upload must be a regular file between 1 byte and 64 GiB".into());}
        let id=uuid::Uuid::new_v4().simple().to_string();let name=path.file_name().unwrap_or_default().to_string_lossy().chars().take(180).collect();
        self.outgoing.insert(id.clone(),Outgoing{file,modified:meta.modified().ok(),size:meta.len(),offset:0,acked:0,hash:Sha256::new(),started:false,begin_sent:false,ending:false,used:Instant::now(),name,progress:u64::MAX,checkpoints:Default::default()});Ok(id)
    }
    pub fn receive(&mut self,peer:&str,packet:Packet,can_upload:bool)->Result<(Vec<Packet>,Update),String>{
        let id=packet.id().to_string();if id.len()!=32||!id.bytes().all(|b|b.is_ascii_hexdigit()){return Err("Invalid upload ID".into());}
        match packet{
            Packet::Begin{name,size,..}=>{
                if !can_upload{return Err("Only editors may upload media to the host".into());}
                if let Some(entry)=self.completed.get(&id){if entry.peer!=peer||entry.size!=size{return Err("Upload identity does not match".into());}return Ok((vec![Packet::Ack{id,offset:entry.size,complete:true}],Update::None));}
                if self.incoming.contains_key(&id){let entry=&self.incoming[&id];if entry.peer!=peer||entry.size!=size{return Err("Upload belongs to another peer".into());}return Ok((vec![Packet::Ack{id,offset:entry.offset,complete:false}],Update::None));}
                if size==0||size>MAX_FILE||self.incoming.len()>=14||self.incoming.values().filter(|v|v.peer==peer).count()>=2||self.total.saturating_add(size)>256*1024*1024*1024{return Err("Host upload size or queue limit reached".into());}
                if name.len()>720||name.contains(['/', '\\','\0'])||name=="."||name==".."{return Err("Invalid upload filename".into());}
                std::fs::create_dir_all(&self.root).map_err(|_|"Could not create host media folder")?;
                let reserved:u64=self.incoming.values().map(|entry|entry.size-entry.offset).sum();
                if fs2::available_space(&self.root).map_err(|_|"Could not check host storage")?<size.saturating_add(reserved).saturating_add(512*1024*1024){return Err("Not enough free storage on host".into());}
                let ext=Path::new(&name).extension().and_then(|v|v.to_str()).filter(|v|v.len()<=12&&v.bytes().all(|b|b.is_ascii_alphanumeric())).unwrap_or("media");
                let filename:String=name.chars().map(|ch|if ch.is_control()||"<>:\"|?*".contains(ch){'_'}else{ch}).take(160).collect();
                let filename=filename.trim_matches(['.',' ']);let filename=if filename.is_empty(){format!("media.{ext}")}else{format!("{filename}")};
                let destination_dir=self.root.join(&id);std::fs::create_dir(&destination_dir).map_err(|_|"Could not reserve host media folder")?;
                let path=self.root.join(format!("{id}.upload"));let destination=destination_dir.join(filename);
                let file=std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(|_|"Could not stage host upload")?;
                self.total+=size;self.incoming.insert(id.clone(),Incoming{peer:peer.into(),path,destination,file,size,offset:0,hash:Sha256::new(),used:Instant::now(),progress:u64::MAX});
                Ok((vec![Packet::Ack{id,offset:0,complete:false}],Update::Progress(format!("Receiving {name}…"))))
            },
            Packet::Chunk{offset,data,..}=>{
                if !can_upload{return Err("Upload permission was revoked".into());}
                let entry=self.incoming.get_mut(&id).ok_or("Upload is no longer active")?;if entry.peer!=peer{return Err("Upload belongs to another peer".into());}
                if data.len()>CHUNK*4/3+4{return Err("Upload chunk exceeds limit".into());}
                let bytes=STANDARD.decode(data).map_err(|_|"Invalid upload chunk")?;if bytes.is_empty()||bytes.len()>CHUNK||offset.saturating_add(bytes.len()as u64)>entry.size{return Err("Invalid upload interval".into());}
                if offset==entry.offset{entry.file.write_all(&bytes).map_err(|_|"Could not write upload; check host storage")?;entry.hash.update(&bytes);entry.offset+=bytes.len()as u64;}else if offset+bytes.len()as u64>entry.offset{return Err("Upload chunks arrived out of order".into());}
                entry.used=Instant::now();let percent=entry.offset*100/entry.size;let progress=if percent!=entry.progress{entry.progress=percent;Update::Progress(format!("Receiving media: {percent}%"))}else{Update::None};
                Ok((vec![Packet::Ack{id,offset:entry.offset,complete:false}],progress))
            },
            Packet::End{digest,..}=>{
                if !can_upload{return Err("Upload permission was revoked".into());}
                let entry=self.incoming.get(&id).ok_or("Upload is no longer active")?;if entry.peer!=peer||entry.offset!=entry.size||digest!=format!("{:x}",entry.hash.clone().finalize()){return Err("Upload size or checksum did not match".into());}
                entry.file.sync_all().map_err(|_|"Could not finish host upload")?;
                let entry=self.incoming.remove(&id).unwrap();self.total=self.total.saturating_sub(entry.size);let path=entry.destination.clone();
                std::fs::rename(&entry.path,&path).map_err(|_|"Could not publish host upload")?;
                if self.completed.len()>=128{if let Some(old)=self.completed.keys().next().cloned(){self.completed.remove(&old);}}
                self.completed.insert(id.clone(),Completed{peer:peer.into(),size:entry.size});
                Ok((vec![Packet::Ack{id,offset:entry.offset,complete:true}],Update::Received(path)))
            },
            Packet::Ack{offset,complete,..}=>{
                let entry=self.outgoing.get_mut(&id).ok_or("Unknown upload acknowledgement")?;
                if complete{
                    if offset!=entry.size||!(entry.ending||!entry.started&&entry.begin_sent){return Err("Premature upload completion".into());}
                    let name=entry.name.clone();self.outgoing.remove(&id);return Ok((Vec::new(),Update::Complete(format!("Uploaded {name}; the host is importing it."))));
                }
                if offset>entry.offset||entry.started&&offset<entry.acked{return Err("Invalid upload acknowledgement".into());}
                if !entry.started&&offset<entry.offset{
                    entry.hash=if offset==0{Sha256::new()}else{entry.checkpoints.iter().find(|(at,_)|*at==offset).map(|(_,hash)|hash.clone()).ok_or("Upload resume checkpoint is no longer available")?};
                    entry.file.seek(SeekFrom::Start(offset)).map_err(|_|"Could not resume source media")?;entry.offset=offset;entry.checkpoints.retain(|(at,_)|*at<=offset);
                }
                entry.started=true;entry.acked=offset;entry.used=Instant::now();
                let percent=offset*100/entry.size;let update=if percent!=entry.progress{entry.progress=percent;Update::Progress(format!("Uploading {}: {percent}%",entry.name))}else{Update::None};Ok((Vec::new(),update))
            },
            Packet::Cancel{reason,..}=>{if let Some(entry)=self.incoming.get(&id){if entry.peer!=peer{return Err("Upload belongs to another peer".into());}}
                self.cancel(&id);Ok((Vec::new(),Update::Progress(format!("Upload stopped: {}",reason.chars().take(160).collect::<String>()))))},
        }
    }
    pub fn pump(&mut self)->Vec<Packet>{
        let mut packets=Vec::new();let mut failed=Vec::new();
        // At most two active uploads, with a four-chunk send window each.
        for(id,entry)in self.outgoing.iter_mut().take(2){
            if !entry.started{if !entry.begin_sent||entry.used.elapsed()>Duration::from_secs(3){packets.push(Packet::Begin{id:id.clone(),name:entry.name.clone(),size:entry.size});entry.used=Instant::now();entry.begin_sent=true;}continue;}
            if entry.offset==entry.size&&entry.acked==entry.size&&!entry.ending{
                if entry.file.metadata().ok().is_none_or(|meta|meta.len()!=entry.size||meta.modified().ok()!=entry.modified){failed.push((id.clone(),"The source file changed during upload".to_string()));continue;}
                packets.push(Packet::End{id:id.clone(),digest:format!("{:x}",entry.hash.clone().finalize())});entry.ending=true;continue;}
            if entry.used.elapsed()>Duration::from_secs(30){continue;}
            for _ in 0..4{
                if entry.offset>=entry.size||entry.offset-entry.acked>=4*CHUNK as u64{break;}
                let size=(entry.size-entry.offset).min(CHUNK as u64)as usize;let mut bytes=vec![0;size];
                if entry.file.read_exact(&mut bytes).is_err(){failed.push((id.clone(),"Could not read source media during upload".to_string()));break;}entry.hash.update(&bytes);
                packets.push(Packet::Chunk{id:id.clone(),offset:entry.offset,data:STANDARD.encode(bytes)});entry.offset+=size as u64;entry.checkpoints.push_back((entry.offset,entry.hash.clone()));while entry.checkpoints.len()>8{entry.checkpoints.pop_front();}
            }
        }
        for(id,reason)in failed{self.outgoing.remove(&id);self.errors.push(reason.clone());packets.push(Packet::Cancel{id,reason});}
        packets
    }
    pub fn take_errors(&mut self)->Vec<String>{std::mem::take(&mut self.errors)}
    pub fn restart_uploads(&mut self){for entry in self.outgoing.values_mut(){entry.started=false;entry.begin_sent=false;entry.ending=false;entry.used=Instant::now();}}
    pub fn outgoing_ids(&self)->Vec<String>{self.outgoing.keys().cloned().collect()}
    pub fn progress(&self)->Vec<Progress>{
        self.outgoing.iter().map(|(id,v)|Progress{id:id.clone(),name:v.name.clone(),done:v.acked,total:v.size,stage:if v.ending{Stage::Finishing}else if v.started{Stage::Uploading}else{Stage::Queued},cancellable:true})
            .chain(self.incoming.iter().map(|(id,v)|Progress{id:id.clone(),name:v.destination.file_name().unwrap_or_default().to_string_lossy().into_owned(),done:v.offset,total:v.size,stage:Stage::Receiving,cancellable:false})).collect()
    }
    pub fn active(&self)->bool{!self.outgoing.is_empty()}
    pub fn cancel(&mut self,id:&str){if let Some(entry)=self.incoming.remove(id){self.total=self.total.saturating_sub(entry.size);}self.outgoing.remove(id);}
    pub fn cancel_peer_file(&mut self,peer:&str,id:&str){if self.incoming.get(id).is_some_and(|entry|entry.peer==peer){self.cancel(id);}}
    pub fn cancel_peer(&mut self,peer:&str){let ids:Vec<_>=self.incoming.iter().filter(|(_,v)|v.peer==peer).map(|(id,_)|id.clone()).collect();for id in ids{self.cancel(&id);}}
    pub fn expire(&mut self){let ids:Vec<_>=self.incoming.iter().filter(|(_,v)|v.used.elapsed()>Duration::from_secs(120)).map(|(id,_)|id.clone()).collect();for id in ids{self.cancel(&id);}}
}
