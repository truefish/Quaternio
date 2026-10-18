//! Participant-hosted session service. Network and journal I/O live off the UI.
use std::collections::BTreeMap;
use std::sync::mpsc::{self,SyncSender,Receiver};
use std::time::{Duration,Instant};
use serde::{Serialize,Deserialize};
use base64::{Engine,engine::general_purpose::URL_SAFE_NO_PAD};
use super::{OperationId,document::{self,Authority,Document,Member,Patch,Applied,ID_BLOCK},audit::{Audit,Record},peer::{Transport,Event as PeerEvent,RTCSessionDescription}};
use crate::project::Project;

#[derive(Clone,Serialize,Deserialize)]
pub struct Participant{pub id:String,pub name:String,pub editor:bool,pub state:String}
#[derive(Clone,Serialize,Deserialize)]
pub struct Invitation{version:u32,pub peer:String,pub epoch:String,secret:String,pub member:Member,description:RTCSessionDescription}
#[derive(Serialize,Deserialize)]
enum Signal{Offer(Invitation),Answer{version:u32,peer:String,epoch:String,description:RTCSessionDescription}}
fn encode(signal:&Signal)->Result<String,String>{serde_json::to_vec(signal).map(|v|format!("quaternio-peer-v1:{}",URL_SAFE_NO_PAD.encode(v))).map_err(|e|e.to_string())}
fn decode(code:&str)->Result<Signal,String>{
    if code.len()>512*1024{return Err("Connection code exceeds 512 KiB".into());}
    let body=code.trim().strip_prefix("quaternio-peer-v1:").ok_or("This is not a Quaternio connection code")?;
    let bytes=URL_SAFE_NO_PAD.decode(body).map_err(|_|"Invalid connection code".to_string())?;
    serde_json::from_slice(&bytes).map_err(|e|e.to_string())
}
pub enum Event{
    Ready{epoch:String,actor:String,member:Member,document:Document,revision:u64,next_sequence:u64,host:bool},
    Code(String),Conflict(String),Commit(Applied),Rejected{ id:OperationId,reason:String },Participants(Vec<Participant>),Status(String),Ended(String),
}
enum Command{Invite(String,bool),Signal(String),Propose(Patch,Document),Undo(OperationId),Redo(OperationId),Revoke(String),Stop}
pub struct Session{tx:Option<SyncSender<Command>>,pub rx:Receiver<Event>,thread:Option<std::thread::JoinHandle<()>>}
impl Session{
    fn spawn(initial:Initial)->Result<Self,String>{
        let(tx,commands)=mpsc::sync_channel(128);let(events,rx)=mpsc::sync_channel(256);
        let thread=std::thread::Builder::new().name("quaternio-session".into()).spawn(move||{
            if let Err(error)=run(initial,commands,events.clone()){let _=events.try_send(Event::Ended(error));}
        }).map_err(|e|e.to_string())?;Ok(Self{tx:Some(tx),rx,thread:Some(thread)})
    }
    pub fn host(project:Project,name:String)->Result<Self,String>{if name.trim().is_empty()||name.chars().count()>80{return Err("Enter a host name between 1 and 80 characters".into());}Self::spawn(Initial::Host(project,name))}
    pub fn join(code:&str)->Result<Self,String>{
        let Signal::Offer(invite)=decode(code)? else{return Err("Join requires an invitation, not an answer".into());};
        if invite.version!=1||uuid::Uuid::parse_str(&invite.peer).is_err()||uuid::Uuid::parse_str(&invite.epoch).is_err()||invite.secret.len()!=64
            ||invite.member.limit.checked_sub(invite.member.first)!=Some(ID_BLOCK)||invite.member.limit>=i32::MAX as u64||invite.member.name.chars().count()>80 {
            return Err("Invalid or unsupported session invitation".into());
        }
        Self::spawn(Initial::Join(invite))
    }
    fn command(&self,c:Command)->Result<(),String>{self.tx.as_ref().ok_or("Session stopped")?.try_send(c).map_err(|_|"Session is stopped or busy; local edits are retained".into())}
    pub fn invite(&self,name:String,editor:bool)->Result<(),String>{self.command(Command::Invite(name,editor))}
    pub fn signal(&self,code:String)->Result<(),String>{self.command(Command::Signal(code))}
    pub fn propose(&self,patch:Patch,branch:Document)->Result<(),String>{self.command(Command::Propose(patch,branch))}
    pub fn undo(&self,id:OperationId)->Result<(),String>{self.command(Command::Undo(id))}
    pub fn redo(&self,id:OperationId)->Result<(),String>{self.command(Command::Redo(id))}
    pub fn revoke(&self,id:String)->Result<(),String>{self.command(Command::Revoke(id))}
}
impl Drop for Session{fn drop(&mut self){
    if let Some(tx)=self.tx.take(){let _=tx.try_send(Command::Stop);drop(tx);}
    if let Some(thread)=self.thread.take(){let _=thread.join();}
}}
enum Initial{Host(Project,String),Join(Invitation)}
#[derive(Serialize,Deserialize)]
enum Wire{
    Hello{version:u32,epoch:String,secret:String},
    Snapshot{epoch:String,actor:String,member:Member,document:Document,revision:u64,next_sequence:u64,participants:Vec<Participant>},
    Propose(Patch),Undo(OperationId),Redo(OperationId),Commit(Applied),Rejected{id:OperationId,reason:String},Participants(Vec<Participant>),Goodbye(String),
}
struct Peer{secret:String,member:Member,authenticated:bool,opened:Option<Instant>,revoked:bool}
struct Worker{
    transport:Transport,events:SyncSender<Event>,authority:Option<Authority>,audit:Option<Audit>,actor:String,
    outbox:Option<super::outbox::Outbox>,invite:Option<Invitation>,peers:BTreeMap<String,Peer>,finished:bool,
}
impl Worker{
    fn emit(&mut self,event:Event)->Result<(),String>{self.events.try_send(event).map_err(|_|"Editor session queue is full; reopen the saved branch".into())}
    fn send(&self,peer:&str,message:&Wire)->Result<(),String>{self.transport.send(peer.into(),serde_json::to_vec(message).map_err(|e|e.to_string())?)}
    fn participants(&self)->Vec<Participant>{
        if let Some(authority)=&self.authority {
            authority.members.iter().map(|(id,m)|Participant{id:id.clone(),name:m.name.clone(),editor:m.editor,state:if id==&self.actor{"Host".into()}else if let Some(p)=self.peers.get(id){if p.revoked{"Revoked"}else if p.authenticated{"Connected"}else{"Invited"}.into()}else{"Disconnected".into()}}).collect()
        }else{Vec::new()}
    }
    fn publish_participants(&mut self)->Result<(),String>{let participants=self.participants();self.broadcast(&Wire::Participants(participants.clone()));self.emit(Event::Participants(participants))}
    fn broadcast(&mut self,message:&Wire){
        let peers:Vec<_>=self.peers.iter().filter(|(_,p)|p.authenticated&&!p.revoked).map(|(id,_)|id.clone()).collect();
        for peer in peers{if let Err(error)=self.send(&peer,message){self.transport.close(peer.clone());if let Some(p)=self.peers.get_mut(&peer){p.authenticated=false;}let _=self.events.try_send(Event::Status(error));}}
    }
    fn invite(&mut self,name:String,editor:bool)->Result<(),String>{
        let name=name.trim();if name.is_empty()||name.chars().count()>80{return Err("Enter a participant name between 1 and 80 characters".into());}
        if self.peers.values().filter(|p|!p.revoked).count()>=7{return Err("Eight participants are supported, including the host. Revoke an unused invitation first.".into());}
        let authority=self.authority.as_mut().ok_or("Only the host can invite participants")?;
        if authority.members.len()>=64{return Err("This session has used 64 invitations; save and start a new session".into());}
        let first=authority.members.get(&self.actor).unwrap().first+authority.members.len()as u64*ID_BLOCK;
        if first+ID_BLOCK>=i32::MAX as u64{return Err("Session object-ID space exhausted; save and start a new session".into());}
        let peer=uuid::Uuid::new_v4().to_string();let member=Member{name:name.into(),editor,first,limit:first+ID_BLOCK};
        self.audit.as_mut().unwrap().append(&Record::Member{actor:peer.clone(),member:member.clone()})?;authority.members.insert(peer.clone(),member.clone());
        let secret=format!("{}{}",uuid::Uuid::new_v4().simple(),uuid::Uuid::new_v4().simple());
        self.peers.insert(peer.clone(),Peer{secret,member,authenticated:false,opened:None,revoked:false});self.transport.offer(peer)?;
        self.emit(Event::Status("Creating your invitation…".into()))?;self.publish_participants()
    }
    fn edit(&mut self,actor:&str,kind:EditKind)->Result<(),String>{
        let authority=self.authority.as_ref().ok_or("The host is unavailable")?;let mut next=authority.clone();
        let (id,patch)=match &kind{EditKind::Propose(p)=>(p.id.clone(),Some(p.clone())),EditKind::Undo(id)|EditKind::Redo(id)=>(id.clone(),None)};
        let result=match kind{EditKind::Propose(p)=>next.submit(actor,p),EditKind::Undo(id)=>next.undo(actor,id),EditKind::Redo(id)=>next.redo(actor,id)};
        match result{
            Ok(commit)=>{
                if commit.revision<=authority.revision {
                    if actor==self.actor{self.emit(Event::Commit(commit))?;}else{self.send(actor,&Wire::Commit(commit))?;}return Ok(());
                }
                self.audit.as_mut().unwrap().append(&Record::Commit(commit.clone()))?;self.authority=Some(next);
                self.broadcast(&Wire::Commit(commit.clone()));self.emit(Event::Commit(commit))
            },
            Err(reason)=>{
                if let Some(patch)=patch{self.audit.as_mut().unwrap().append(&Record::Rejected{actor:actor.into(),patch,reason:reason.clone()})?;}
                if actor==self.actor{self.emit(Event::Rejected{id,reason})}else{
                    self.emit(Event::Conflict(format!("{}: {}",self.authority.as_ref().and_then(|a|a.members.get(actor)).map(|m|m.name.as_str()).unwrap_or("Participant"),reason)))?;
                    self.send(actor,&Wire::Rejected{id,reason})
                }
            }
        }
    }
    fn message(&mut self,peer:&str,bytes:&[u8])->Result<(),String>{
        let message:Wire=serde_json::from_slice(bytes).map_err(|_|"Peer sent an invalid protocol message".to_string())?;
        if self.authority.is_some(){
            let p=self.peers.get(peer).ok_or("Peer has no invitation")?;
            if p.revoked{return Err("Invitation was revoked".into());}
            if !p.authenticated {
                let Wire::Hello{version:1,epoch,secret}=message else{return Err("Peer must authenticate before editing".into());};
                use sha2::{Sha256,Digest};
                use subtle::ConstantTimeEq;
                if epoch!=self.authority.as_ref().unwrap().epoch||secret.len()!=64||!bool::from(Sha256::digest(secret.as_bytes()).as_slice().ct_eq(Sha256::digest(p.secret.as_bytes()).as_slice())){return Err("Invalid invitation credentials".into());}
                let member=p.member.clone();self.peers.get_mut(peer).unwrap().authenticated=true;
                let authority=self.authority.as_ref().unwrap();
                let next_sequence=authority.log.iter().filter(|c|c.patch.id.author==peer).map(|c|c.patch.id.sequence).max().unwrap_or(0)+1;
                self.send(peer,&Wire::Snapshot{epoch:authority.epoch.clone(),actor:peer.into(),member,document:authority.document.clone(),revision:authority.revision,next_sequence,participants:self.participants()})?;
                self.publish_participants()?;return Ok(());
            }
            match message{Wire::Propose(p)=>self.edit(peer,EditKind::Propose(p)),Wire::Undo(id)=>self.edit(peer,EditKind::Undo(id)),Wire::Redo(id)=>self.edit(peer,EditKind::Redo(id)),_=>Err("Unexpected client protocol message".into())}
        }else{
            let invite=self.invite.as_ref().ok_or("No active invitation")?;
            if peer!=invite.peer{return Err("Unknown host connection".into());}
            match message{
                Wire::Snapshot{epoch,actor,member,document,revision,next_sequence,participants}=>{
                    if epoch!=invite.epoch||actor!=invite.peer||member.first!=invite.member.first||member.limit!=invite.member.limit||member.editor!=invite.member.editor{return Err("Host snapshot does not match the invitation".into());}
                    document::materialize(&document)?;
                    self.outbox=Some(super::outbox::Outbox::new(epoch.clone(),actor.clone(),document.clone(),revision)?);
                    self.emit(Event::Ready{epoch,actor,member,document,revision,next_sequence,host:false})?;self.emit(Event::Participants(participants))
                },
                Wire::Commit(commit)=>{self.outbox.as_mut().ok_or("No client snapshot")?.accept(&commit)?;self.emit(Event::Commit(commit))},Wire::Rejected{id,reason}=>{self.outbox.as_mut().ok_or("No client snapshot")?.reject(&id,&reason)?;self.emit(Event::Rejected{id,reason})},
                Wire::Participants(peers)=>self.emit(Event::Participants(peers)),Wire::Goodbye(reason)=>{self.finished=true;self.emit(Event::Ended(reason))},
                _=>Err("Unexpected host protocol message".into()),
            }
        }
    }
    fn peer_event(&mut self,event:PeerEvent)->Result<(),String>{match event{
        PeerEvent::Description{peer,description,offer}=>{
            let signal=if offer{let p=self.peers.get(&peer).ok_or("Invitation expired")?;Signal::Offer(Invitation{version:1,peer,epoch:self.authority.as_ref().unwrap().epoch.clone(),secret:p.secret.clone(),member:p.member.clone(),description})}
                else{let invite=self.invite.as_ref().unwrap();Signal::Answer{version:1,peer,epoch:invite.epoch.clone(),description}};
            self.emit(Event::Code(encode(&signal)?))
        },
        PeerEvent::Opened(peer)=>{
            if self.authority.is_some(){if let Some(p)=self.peers.get_mut(&peer){p.opened=Some(Instant::now());}}
            else{let i=self.invite.as_ref().unwrap();self.send(&peer,&Wire::Hello{version:1,epoch:i.epoch.clone(),secret:i.secret.clone()})?;}
            self.emit(Event::Status("Securing your connection…".into()))
        },
        PeerEvent::Message(peer,bytes)=>{
            if let Err(error)=self.message(&peer,&bytes.bytes){self.transport.close(peer.clone());if self.authority.is_none(){self.finished=true;self.emit(Event::Ended(error))}else{if let Some(p)=self.peers.get_mut(&peer){p.authenticated=false;}self.emit(Event::Status(error))}}else{Ok(())}
        },
        PeerEvent::State(_,state)=>self.emit(Event::Status(state)),
        PeerEvent::Error(peer,reason)=>{
            self.transport.close(peer.clone());
            if self.authority.is_none(){self.finished=true;self.emit(Event::Ended(format!("Peer connection failed: {reason}")))}
            else{if let Some(p)=self.peers.get_mut(&peer){p.authenticated=false;p.revoked=true;}self.emit(Event::Status(format!("Peer connection failed: {reason}")))?;self.publish_participants()}
        },
        PeerEvent::Closed(peer)=>{
            self.transport.close(peer.clone());
            if self.authority.is_some(){if let Some(p)=self.peers.get_mut(&peer){p.authenticated=false;p.revoked=true;}self.publish_participants()}
            else{self.finished=true;self.emit(Event::Ended("Host connection closed. Acknowledged edits and local pending work remain on this machine.".into()))}
        },
    }}
    fn command(&mut self,command:Command)->Result<(),String>{match command{
        Command::Invite(name,editor)=>self.invite(name,editor),
        Command::Signal(code)=>{
            let Signal::Answer{version:1,peer,epoch,description}=decode(&code)? else{return Err("The host needs the participant's answer code".into());};
            if self.authority.as_ref().is_none_or(|a|a.epoch!=epoch)||!self.peers.contains_key(&peer){return Err("Answer does not belong to this active session".into());}
            self.transport.accept(peer,description)
        },
        Command::Propose(p,branch)=>if self.authority.is_some(){let actor=self.actor.clone();self.edit(&actor,EditKind::Propose(p))}else{self.outbox.as_mut().ok_or("Wait for the host snapshot")?.enqueue(p.clone(),branch)?;self.send(&self.invite.as_ref().unwrap().peer,&Wire::Propose(p))},
        Command::Undo(id)=>if self.authority.is_some(){let actor=self.actor.clone();self.edit(&actor,EditKind::Undo(id))}else{self.send(&self.invite.as_ref().unwrap().peer,&Wire::Undo(id))},
        Command::Redo(id)=>if self.authority.is_some(){let actor=self.actor.clone();self.edit(&actor,EditKind::Redo(id))}else{self.send(&self.invite.as_ref().unwrap().peer,&Wire::Redo(id))},
        Command::Revoke(id)=>{
            let authority=self.authority.as_mut().ok_or("Only the host can revoke invitations")?;
            if id==self.actor{return Err("The host cannot revoke itself".into());}
            let mut member=authority.members.get(&id).cloned().ok_or("Unknown participant")?;member.editor=false;
            self.audit.as_mut().unwrap().append(&Record::Member{actor:id.clone(),member:member.clone()})?;
            authority.members.insert(id.clone(),member);
            let _=self.send(&id,&Wire::Goodbye("The host revoked this invitation. Your local work is retained.".into()));self.transport.close(id.clone());
            if let Some(p)=self.peers.get_mut(&id){p.revoked=true;p.authenticated=false;}self.publish_participants()
        },
        Command::Stop=>{self.broadcast(&Wire::Goodbye("The host ended the session. Your document remains available locally.".into()));self.finished=true;Ok(())},
    }}
}
enum EditKind{Propose(Patch),Undo(OperationId),Redo(OperationId)}
fn run(initial:Initial,commands:Receiver<Command>,events:SyncSender<Event>)->Result<(),String>{
    let mut worker=Worker{transport:Transport::new()?,events,authority:None,audit:None,actor:String::new(),outbox:None,invite:None,peers:BTreeMap::new(),finished:false};
    match initial{
        Initial::Host(project,name)=>{
            let epoch=uuid::Uuid::new_v4().to_string();let actor=uuid::Uuid::new_v4().to_string();let first=(project.next_id/ID_BLOCK+1)*ID_BLOCK;
            if first+64*ID_BLOCK>=i32::MAX as u64{return Err("This project has exhausted the ID space needed for a live session".into());}
            let mut authority=Authority::new(&project,epoch.clone())?;
            let member=Member{name:name.trim().chars().take(80).collect(),editor:true,first,limit:first+ID_BLOCK};
            let mut audit=Audit::create(crate::preferences::data_dir().join("sessions").join(format!("{epoch}.qsession")),&authority)?;
            audit.append(&Record::Member{actor:actor.clone(),member:member.clone()})?;authority.members.insert(actor.clone(),member.clone());
            worker.actor=actor.clone();worker.emit(Event::Ready{epoch,actor,member,document:authority.document.clone(),revision:0,next_sequence:1,host:true})?;
            worker.authority=Some(authority);worker.audit=Some(audit);worker.publish_participants()?;
        },
        Initial::Join(invite)=>{worker.actor=invite.peer.clone();worker.transport.answer(invite.peer.clone(),invite.description.clone())?;worker.invite=Some(invite);worker.emit(Event::Status("Preparing your reply…".into()))?;},
    }
    while !worker.finished{
        for _ in 0..32{match commands.try_recv(){Ok(c)=>{
            let id=match &c{Command::Propose(p,_)=>Some(p.id.clone()),Command::Undo(id)|Command::Redo(id)=>Some(id.clone()),_=>None};
            if let Err(error)=worker.command(c){if let Some(id)=id{
                if let Some(outbox)=&mut worker.outbox{let _=outbox.reject(&id,&error);}
                worker.emit(Event::Rejected{id,reason:error})?;
            }else{worker.emit(Event::Status(error))?;}}
        },Err(mpsc::TryRecvError::Disconnected)=>{worker.finished=true;break;},Err(_)=>break}}
        for _ in 0..32{match worker.transport.rx.try_recv(){Ok(e)=>worker.peer_event(e)?,Err(mpsc::TryRecvError::Disconnected)=>return Err("Peer transport stopped".into()),Err(_)=>break}}
        let expired:Vec<_>=worker.peers.iter().filter(|(_,p)|!p.authenticated&&p.opened.is_some_and(|at|at.elapsed()>Duration::from_secs(15))).map(|(id,_)|id.clone()).collect();
        for id in expired{worker.transport.close(id);}
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}
