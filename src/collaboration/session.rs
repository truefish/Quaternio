//! Participant-hosted session service. Network and journal I/O live off the UI.
use std::collections::BTreeMap;
use std::sync::mpsc::{self,SyncSender,Receiver};
use std::time::{Duration,Instant};
use serde::{Serialize,Deserialize};
use base64::{Engine,engine::general_purpose::URL_SAFE_NO_PAD};
use super::{OperationId,document::{self,Authority,Document,Member,Patch,Applied,ID_BLOCK},audit::{Audit,Record},peer::{Transport,Event as PeerEvent,RTCSessionDescription}};
use crate::project::Project;

#[derive(Clone,Serialize,Deserialize)]
pub struct Participant{pub id:String,pub name:String,pub editor:bool,pub state:String,#[serde(default)]pub route:String}
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
fn validate_invitation(invite:&Invitation)->Result<(),String>{
        if invite.version!=1||uuid::Uuid::parse_str(&invite.peer).is_err()||uuid::Uuid::parse_str(&invite.epoch).is_err()||invite.secret.len()!=64
            ||invite.member.limit.checked_sub(invite.member.first)!=Some(ID_BLOCK)||invite.member.limit>=i32::MAX as u64||invite.member.name.chars().count()>80 {
            return Err("Invalid or unsupported session invitation".into());
        }
    Ok(())
}
pub enum Event{
    Ready{epoch:String,actor:String,member:Member,document:Document,revision:u64,next_sequence:u64,host:bool},
    OperationFailed(String),Transfers(Vec<super::transfers::Progress>),ExportRequested(String,String),MediaChanged,Waveform(super::media::WavePacket),UploadState(bool),Reconnecting,Resumed{document:Document,revision:u64,next_sequence:u64},Preview(super::media::Packet),Uploaded(std::path::PathBuf),MediaAvailable(bool),Code(String),Conflict(String),Commit(Applied),Rejected{ id:OperationId,reason:String },Participants(Vec<Participant>),Status(String),Ended(String),
}
enum Command{RequestExport(String),Waveform(super::media::WaveRequest),#[cfg(test)] Reconnect,Sources(Project),Preview(super::media::Request),Upload(std::path::PathBuf),CancelUpload(String),Invite(String,bool),Signal(String),Propose(Patch,Document),Undo(OperationId),Redo(OperationId),Revoke(String),Stop}
pub struct Session{tx:Option<SyncSender<Command>>,pub rx:Receiver<Event>,thread:Option<std::thread::JoinHandle<()>>}
impl Session{
    fn spawn(initial:Initial,relay:super::peer::RelaySettings)->Result<Self,String>{
        let(tx,commands)=mpsc::sync_channel(128);let(events,rx)=mpsc::sync_channel(256);
        let thread=std::thread::Builder::new().name("quaternio-session".into()).spawn(move||{
            if let Err(error)=run(initial,commands,events.clone(),relay){let _=events.try_send(Event::Ended(error));}
        }).map_err(|e|e.to_string())?;Ok(Self{tx:Some(tx),rx,thread:Some(thread)})
    }
    #[cfg(test)]
    pub fn host(project:Project,name:String)->Result<Self,String>{Self::host_with_relay(project,name,Default::default())}
    pub fn host_with_relay(project:Project,name:String,relay:super::peer::RelaySettings)->Result<Self,String>{relay.validate()?;if name.trim().is_empty()||name.chars().count()>80{return Err("Enter a host name between 1 and 80 characters".into());}Self::spawn(Initial::Host(project,name),relay)}
    #[cfg(test)]
    pub fn join(code:&str)->Result<Self,String>{Self::join_with_relay(code,Default::default())}
    pub fn join_with_relay(code:&str,relay:super::peer::RelaySettings)->Result<Self,String>{
        if let Some(invite)=super::signaling::parse(code)?{return Self::spawn(Initial::JoinRoom(invite),relay);}
        relay.validate()?;
        let Signal::Offer(invite)=decode(code)? else{return Err("Join requires an invitation, not an answer".into());};
        validate_invitation(&invite)?;
        Self::spawn(Initial::Join(invite),relay)
    }
    fn command(&self,c:Command)->Result<(),String>{self.tx.as_ref().ok_or("Session stopped")?.try_send(c).map_err(|_|"Session is stopped or busy; local edits are retained".into())}
    pub fn request_export(&self,sequence:String)->Result<(),String>{self.command(Command::RequestExport(sequence))}
    pub fn waveform(&self,request:super::media::WaveRequest)->Result<(),String>{self.command(Command::Waveform(request))}
    #[cfg(test)]
    pub fn reconnect(&self)->Result<(),String>{self.command(Command::Reconnect)}
    pub fn update_sources(&self,project:Project)->Result<(),String>{self.command(Command::Sources(project))}
    pub fn preview(&self,request:super::media::Request)->Result<(),String>{self.command(Command::Preview(request))}
    pub fn upload(&self,path:std::path::PathBuf)->Result<(),String>{self.command(Command::Upload(path))}
    pub fn cancel_upload(&self,id:String)->Result<(),String>{self.command(Command::CancelUpload(id))}
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
enum Initial{Host(Project,String),Join(Invitation),JoinRoom(super::signaling::Invite)}
#[derive(Serialize,Deserialize)]
enum Wire{
    Hello{version:u32,epoch:String,secret:String,#[serde(default)]media:bool},
    Snapshot{epoch:String,actor:String,member:Member,document:Document,revision:u64,next_sequence:u64,participants:Vec<Participant>,#[serde(default)]media:bool,#[serde(default)]resume:Option<super::signaling::Invite>},
    PreviewAck{view:u8,id:u64,frame:i64},RequestExport(String),MediaChanged,WaveformRequest(super::media::WaveRequest),Leave,Ping,Pong,PreviewRequest(super::media::Request),Propose(Patch),Undo(OperationId),Redo(OperationId),Commit(Applied),Rejected{id:OperationId,reason:String},Participants(Vec<Participant>),Goodbye(String),
}
struct Peer{secret:String,member:Member,authenticated:bool,opened:Option<Instant>,revoked:bool,route:String}
struct Worker{
    preview_latency:BTreeMap<(String,u8),Duration>,preview_inflight:BTreeMap<(String,u8),(u64,i64,Instant)>,preview_pending:BTreeMap<(String,u8),super::media::Packet>,
    last_transfer_update:Instant,last_transfers:Vec<super::transfers::Progress>,
    waveforms:Option<super::media::Waveforms>,upload_active:bool,reconnecting:bool,last_seen:Instant,last_ping:Instant,links:BTreeMap<String,super::signaling::HostLink>,
    sources:BTreeMap<u64,std::path::PathBuf>,renderers:BTreeMap<(String,u8),super::media::Renderer>,render_project:Option<(u64,std::sync::Arc<Project>)>,
    aux:std::collections::BTreeSet<(String,u8)>,transfers:super::transfers::Transfers,media_available:bool,
    signaling:super::signaling::Tasks,service:Option<super::signaling::Service>,guest_room:Option<super::signaling::Invite>,
    transport:Transport,events:SyncSender<Event>,authority:Option<Authority>,audit:Option<Audit>,actor:String,
    outbox:Option<super::outbox::Outbox>,invite:Option<Invitation>,peers:BTreeMap<String,Peer>,finished:bool,
}
impl Worker{
    fn emit(&mut self,event:Event)->Result<(),String>{self.events.try_send(event).map_err(|_|"Editor session queue is full; reopen the saved branch".into())}
    fn send(&self,peer:&str,message:&Wire)->Result<(),String>{self.transport.send(peer.into(),serde_json::to_vec(message).map_err(|e|e.to_string())?)}
    fn reconnect(&mut self)->Result<(),String>{
        if self.authority.is_some()||self.reconnecting{return Ok(());}
        let Some(room)=self.guest_room.clone()else{return Err("Manual connections need a fresh invitation after interruption".into());};
        let peer=self.invite.as_ref().ok_or("Missing host invitation")?.peer.clone();
        self.reconnecting=true;self.aux.clear();self.transport.close(peer);self.transfers.restart_uploads();self.emit(Event::Reconnecting)?;self.signaling.reconnect(room)
    }
    fn host_project(&mut self)->Result<std::sync::Arc<Project>,String>{
        let authority=self.authority.as_ref().ok_or("Only the host renders shared media")?;
        if self.render_project.as_ref().is_none_or(|(revision,_)|*revision!=authority.revision){
            let mut project=document::materialize(&authority.document)?;
            for media in &mut project.media{media.path=self.sources.get(&media.id).cloned().unwrap_or_default();}
            self.render_project=Some((authority.revision,std::sync::Arc::new(project)));
        }
        Ok(self.render_project.as_ref().unwrap().1.clone())
    }
    fn preview_request(&mut self,peer:&str,mut request:super::media::Request)->Result<(),String>{
        if self.authority.as_ref().is_none_or(|authority|authority.revision!=request.revision){return Ok(());}
        let project=self.host_project()?;request.validate(&project)?;
        if request.view<2 {
            let latency=self.preview_latency.get(&(peer.into(),request.view)).copied().unwrap_or_default();
            let width=if latency>Duration::from_millis(1500){160}else if latency>Duration::from_millis(400){320}else{640};
            if request.width>width{request.height=((request.height as u64*width as u64/request.width as u64)as u32).max(2);request.width=width;}
        }
        self.renderers.entry((peer.into(),request.view)).or_insert_with(super::media::Renderer::new).submit(project,request);Ok(())
    }
    fn auxiliary(&mut self,peer:&str,lane:u8,bytes:&[u8])->Result<(),String>{
        if self.authority.is_some(){if !self.peers.get(peer).is_some_and(|p|p.authenticated&&!p.revoked){return Err("Media channel is not authenticated".into());}}
        else if self.invite.as_ref().is_none_or(|invite|invite.peer!=peer)||self.outbox.is_none(){return Err("Unknown media host".into());}
        if lane==1{
            if self.authority.is_some(){return Err("Guests cannot submit rendered previews".into());}
            let message:super::media::Message=serde_json::from_slice(bytes).map_err(|_|"Invalid media packet")?;
            match message{
                super::media::Message::Picture(packet)=>{
                    if packet.request.width>1280||packet.request.height>720||packet.request.width<2||packet.request.height<2{return Err("Invalid remote preview size".into());}
                    self.send(peer,&Wire::PreviewAck{view:packet.request.view,id:packet.request.id,frame:packet.request.frame})?;
                    let _=self.events.try_send(Event::Preview(packet));
                },
                super::media::Message::Waveform(packet)=>{
                    if packet.peaks.len()>1024||packet.peaks.len()!=packet.request.columns as usize||packet.peaks.iter().flatten().any(|v|!v.is_finite()||v.abs()>1.){return Err("Invalid waveform data".into());}
                    let _=self.events.try_send(Event::Waveform(packet));
                }
            }Ok(())
        }else{
            let packet:super::transfers::Packet=serde_json::from_slice(bytes).map_err(|_|"Invalid upload packet")?;let id=packet.id().to_string();
            let can_upload=self.peers.get(peer).is_some_and(|p|p.authenticated&&!p.revoked&&p.member.editor)&&self.authority.is_some();
            if self.authority.is_some()&&matches!(packet,super::transfers::Packet::Ack{..}){return Err("Unexpected upload acknowledgement".into());}
            if self.authority.is_none()&&!matches!(packet,super::transfers::Packet::Ack{..}|super::transfers::Packet::Cancel{..}){return Err("The host cannot download media to a guest".into());}
            match self.transfers.receive(peer,packet,can_upload){
                Ok((responses,update))=>{
                    for packet in responses{self.transport.send_aux(peer.into(),2,serde_json::to_vec(&packet).map_err(|e|e.to_string())?)?;}
                    match update{super::transfers::Update::None=>Ok(()),super::transfers::Update::Received(path)=>self.emit(Event::Uploaded(path)),super::transfers::Update::Progress(text)|super::transfers::Update::Complete(text)=>self.emit(Event::Status(text))}
                },
                Err(reason)=>{if self.authority.is_some(){self.transfers.cancel_peer_file(peer,&id);}else{self.transfers.cancel(&id);}let packet=super::transfers::Packet::Cancel{id,reason};self.transport.send_aux(peer.into(),2,serde_json::to_vec(&packet).map_err(|e|e.to_string())?)}
            }
        }
    }
    fn pump_media(&mut self)->Result<(),String>{
        if let Some(waveforms)=&mut self.waveforms{for(peer,packet)in waveforms.poll(){if self.aux.contains(&(peer.clone(),1)){let _=self.transport.send_aux(peer,1,serde_json::to_vec(&super::media::Message::Waveform(packet)).map_err(|e|e.to_string())?);}}}
        self.transfers.expire();for error in self.transfers.take_errors(){self.emit(Event::Status(error))?;}
        if self.last_transfer_update.elapsed()>=Duration::from_millis(200){
            self.last_transfer_update=Instant::now();let progress=self.transfers.progress();
            if progress!=self.last_transfers{self.last_transfers=progress.clone();self.emit(Event::Transfers(progress))?;}
        }
        let uploading=self.transfers.active();if uploading!=self.upload_active{self.upload_active=uploading;self.emit(Event::UploadState(uploading))?;}
        if self.authority.is_some(){
            self.preview_inflight.retain(|_,(_,_,sent)|sent.elapsed()<Duration::from_secs(30));
            for(key,renderer)in &self.renderers{while let Ok(packet)=renderer.rx.try_recv(){self.preview_pending.insert(key.clone(),packet);}}
            let ready:Vec<_>=self.preview_pending.keys().filter(|key|self.aux.contains(&(key.0.clone(),1))&&!self.preview_inflight.contains_key(*key)).cloned().collect();
            for key in ready{if let Some(packet)=self.preview_pending.remove(&key){let stamp=(packet.request.id,packet.request.frame,Instant::now());
                if self.transport.send_aux(key.0.clone(),1,serde_json::to_vec(&super::media::Message::Picture(packet)).map_err(|e|e.to_string())?).is_ok(){self.preview_inflight.insert(key,stamp);}
            }}
        }
        else if self.outbox.is_some()&&self.media_available{if let Some(invite)=&self.invite{if self.aux.contains(&(invite.peer.clone(),2)){for packet in self.transfers.pump(){self.transport.send_aux(invite.peer.clone(),2,serde_json::to_vec(&packet).map_err(|e|e.to_string())?)?;}}}}
        Ok(())
    }
    fn participants(&self)->Vec<Participant>{
        if let Some(authority)=&self.authority {
            authority.members.iter().map(|(id,m)|Participant{id:id.clone(),name:m.name.clone(),editor:m.editor,state:if id==&self.actor{"Host".into()}else if let Some(p)=self.peers.get(id){if p.revoked{"Revoked"}else if p.authenticated{"Connected"}else if p.opened.is_some(){"Disconnected"}else{"Invited"}.into()}else{"Disconnected".into()},route:self.peers.get(id).map(|p|p.route.clone()).unwrap_or_default()}).collect()
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
        self.peers.insert(peer.clone(),Peer{secret,member,authenticated:false,opened:None,revoked:false,route:String::new()});self.transport.offer(peer)?;
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
                let Wire::Hello{version:3,epoch,secret,media}=message else{return Err("This collaboration session requires the same sequence-capable Quaternio version".into());};
                use sha2::{Sha256,Digest};
                use subtle::ConstantTimeEq;
                if epoch!=self.authority.as_ref().unwrap().epoch||secret.len()!=64||!bool::from(Sha256::digest(secret.as_bytes()).as_slice().ct_eq(Sha256::digest(p.secret.as_bytes()).as_slice())){return Err("Invalid invitation credentials".into());}
                let member=p.member.clone();self.peers.get_mut(peer).unwrap().authenticated=true;
                let authority=self.authority.as_ref().unwrap();
                let next_sequence=authority.log.iter().filter(|c|c.patch.id.author==peer).map(|c|c.patch.id.sequence).max().unwrap_or(0)+1;
                let resume=self.links.get(peer).filter(|link|!link.resume.is_empty()).map(|link|super::signaling::Invite{url:link.invite.url.clone(),id:link.invite.id.clone(),token:link.resume.clone(),generation:0});
                self.send(peer,&Wire::Snapshot{epoch:authority.epoch.clone(),actor:peer.into(),member,document:authority.document.clone(),revision:authority.revision,next_sequence,participants:self.participants(),media,resume})?;
                if let Some(link)=self.links.get(peer).cloned(){self.signaling.activate(link)?;}
                self.publish_participants()?;return Ok(());
            }
            match message{Wire::PreviewAck{view,id,frame}=>{let key=(peer.to_owned(),view);if self.preview_inflight.get(&key).is_some_and(|(sent,at,_)|*sent==id&&*at==frame){if let Some((_,_,sent))=self.preview_inflight.remove(&key){self.preview_latency.insert(key,sent.elapsed());}}Ok(())},Wire::RequestExport(sequence)=>{if !self.peers.get(peer).is_some_and(|p|p.member.editor){return Err("Viewers cannot request exports".into());}let name=self.peers.get(peer).unwrap().member.name.clone();if self.host_project()?.sequence_by_id(&sequence).is_none(){return Err("Unknown export sequence".into());}self.emit(Event::ExportRequested(name,sequence))},Wire::WaveformRequest(request)=>{if self.authority.as_ref().is_some_and(|a|a.revision==request.revision){let project=self.host_project()?;self.waveforms.get_or_insert_with(super::media::Waveforms::new).submit(peer,&project,request)?;}Ok(())},Wire::Leave=>self.command(Command::Revoke(peer.into())),Wire::Ping=>self.send(peer,&Wire::Pong),Wire::PreviewRequest(request)=>self.preview_request(peer,request),Wire::Propose(p)=>self.edit(peer,EditKind::Propose(p)),Wire::Undo(id)=>self.edit(peer,EditKind::Undo(id)),Wire::Redo(id)=>self.edit(peer,EditKind::Redo(id)),_=>Err("Unexpected client protocol message".into())}
        }else{
            let invite=self.invite.as_ref().ok_or("No active invitation")?;
            if peer!=invite.peer{return Err("Unknown host connection".into());}
            self.last_seen=Instant::now();
            match message{
                Wire::MediaChanged=>self.emit(Event::MediaChanged),
                Wire::Pong=>Ok(()),
                Wire::Snapshot{epoch,actor,member,document,revision,next_sequence,participants,media,resume}=>{
                    if epoch!=invite.epoch||actor!=invite.peer||member.first!=invite.member.first||member.limit!=invite.member.limit||member.editor!=invite.member.editor{return Err("Host snapshot does not match the invitation".into());}
                    document::materialize(&document)?;
                    if let Some(mut resume)=resume{super::signaling::validate_url(&resume.url)?;if let Some(current)=&self.guest_room{if resume.id!=current.id||resume.url!=current.url||resume.token.len()<32||resume.token.len()>128{return Err("Invalid reconnect capability".into());}resume.generation=current.generation;self.guest_room=Some(resume);}}
                    self.media_available=media;
                    if let Some(outbox)=&mut self.outbox{
                        let pending=outbox.resnapshot(document.clone(),revision,next_sequence)?;
                        self.reconnecting=false;self.emit(Event::Resumed{document,revision,next_sequence})?;
                        for patch in pending{self.send(peer,&Wire::Propose(patch))?;}
                    }else{
                        self.outbox=Some(super::outbox::Outbox::new(epoch.clone(),actor.clone(),document.clone(),revision)?);
                        self.emit(Event::Ready{epoch,actor,member,document,revision,next_sequence,host:false})?;
                    }
                    self.emit(Event::MediaAvailable(media))?;self.emit(Event::Participants(participants))
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
            let code=encode(&signal)?;
            if offer {let Signal::Offer(invite)=&signal else{unreachable!()};if let Some(link)=self.links.get(&invite.peer).cloned(){return self.signaling.renew(link,code);}if let Some(service)=self.service.clone(){return self.signaling.publish(service,invite.peer.clone(),code);}}
            else if let Some(room)=&self.guest_room{room.answer(code)?;return self.emit(Event::Status("Invite accepted. Connecting automatically…".into()));}
            self.emit(Event::Code(code))
        },
        PeerEvent::Opened(peer)=>{
            if self.authority.is_some(){if let Some(p)=self.peers.get_mut(&peer){p.opened=Some(Instant::now());}}
            else{let i=self.invite.as_ref().unwrap();self.send(&peer,&Wire::Hello{version:3,epoch:i.epoch.clone(),secret:i.secret.clone(),media:true})?;}
            self.emit(Event::Status("Securing your connection…".into()))
        },
        PeerEvent::AuxOpened(peer,lane)=>{self.aux.insert((peer,lane));Ok(())},
        PeerEvent::AuxMessage(peer,lane,bytes)=>{if let Err(error)=self.auxiliary(&peer,lane,&bytes.bytes){self.emit(Event::Status(error))?;}Ok(())},
        PeerEvent::Message(peer,bytes)=>{
            if let Err(error)=self.message(&peer,&bytes.bytes){self.transport.close(peer.clone());if self.authority.is_none(){self.finished=true;self.emit(Event::Ended(error))}else{if let Some(p)=self.peers.get_mut(&peer){p.authenticated=false;}self.emit(Event::Status(error))}}else{Ok(())}
        },
        PeerEvent::State(state)=>self.emit(Event::Status(state)),
        PeerEvent::Route(peer,relayed)=>{
            let route=if relayed{"Relayed"}else{"Direct"};
            if let Some(p)=self.peers.get_mut(&peer){p.route=route.into();self.publish_participants()?;}
            self.emit(Event::Status(format!("Connected · {route}")))
        },
        PeerEvent::Error(peer,reason)=>{
            self.transport.close(peer.clone());
            if self.authority.is_none(){if self.guest_room.is_some(){self.reconnect()}else{self.finished=true;self.emit(Event::Ended(format!("Peer connection failed: {reason}")))}}
            else{if let Some(p)=self.peers.get_mut(&peer){p.authenticated=false;}self.emit(Event::OperationFailed(format!("Could not connect: {reason}")))?;self.publish_participants()}
        },
        PeerEvent::Closed(peer)=>{
            self.aux.retain(|(id,_)|id!=&peer);self.renderers.retain(|(id,_),_|id!=&peer);self.preview_latency.retain(|(id,_),_|id!=&peer);self.preview_pending.retain(|(id,_),_|id!=&peer);self.preview_inflight.retain(|(id,_),_|id!=&peer);if !self.links.contains_key(&peer){self.transfers.cancel_peer(&peer);}if let Some(waveforms)=&mut self.waveforms{waveforms.cancel_peer(&peer);}
            self.transport.close(peer.clone());
            if self.authority.is_some(){if let Some(p)=self.peers.get_mut(&peer){p.authenticated=false;}self.publish_participants()}
            else{if self.guest_room.is_some(){self.reconnect()}else{self.finished=true;self.emit(Event::Ended("Host connection closed. Acknowledged edits and local pending work remain on this machine.".into()))}}
        },
    }}
    fn command(&mut self,command:Command)->Result<(),String>{match command{
        Command::RequestExport(sequence)=>{if self.authority.is_some()||self.reconnecting||!self.invite.as_ref().is_some_and(|invite|invite.member.editor){return Err("Only connected editors can request a host export".into());}self.send(&self.invite.as_ref().unwrap().peer,&Wire::RequestExport(sequence))},
        Command::Waveform(request)=>{if self.media_available&&self.outbox.is_some()&&!self.reconnecting{self.send(&self.invite.as_ref().unwrap().peer,&Wire::WaveformRequest(request))}else{Ok(())}},
        #[cfg(test)]
        Command::Reconnect=>self.reconnect(),
        Command::Sources(project)=>{
            if let Some(authority)=&self.authority{if project.document_id!=authority.document.get("document/document_id").and_then(|v|v.as_str()).unwrap_or(""){return Err("Media registry belongs to another project".into());}
                let paths=project.media.iter().filter(|m|!m.path.as_os_str().is_empty()).map(|m|(m.id,m.path.clone())).collect();
                if self.sources!=paths{self.sources=paths;self.render_project=None;self.renderers.clear();self.preview_pending.clear();self.preview_inflight.clear();self.waveforms=None;self.broadcast(&Wire::MediaChanged);}
            }Ok(())
        },
        Command::Preview(request)=>{if self.media_available&&self.outbox.is_some(){self.send(&self.invite.as_ref().unwrap().peer,&Wire::PreviewRequest(request))}else{Ok(())}},
        Command::Upload(path)=>{if self.authority.is_some(){return Err("Import files directly on the host".into());}if !self.media_available||!self.invite.as_ref().is_some_and(|i|i.member.editor){return Err("Only connected editors can upload media".into());}self.transfers.upload(path)?;self.emit(Event::Status("Upload queued for the host…".into()))},
        Command::CancelUpload(id)=>{let ids=if id.is_empty(){self.transfers.outgoing_ids()}else{vec![id]};for id in ids{self.transfers.cancel(&id);if let Some(invite)=&self.invite{let packet=super::transfers::Packet::Cancel{id,reason:"Cancelled by contributor".into()};self.transport.send_aux(invite.peer.clone(),2,serde_json::to_vec(&packet).map_err(|e|e.to_string())?)?;}}self.emit(Event::Status("Uploads cancelled.".into()))},
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
            if let Some(p)=self.peers.get_mut(&id){p.revoked=true;p.authenticated=false;}
            self.aux.retain(|(peer,_)|peer!=&id);self.renderers.retain(|(peer,_),_|peer!=&id);self.preview_latency.retain(|(peer,_),_|peer!=&id);self.preview_pending.retain(|(peer,_),_|peer!=&id);self.preview_inflight.retain(|(peer,_),_|peer!=&id);self.transfers.cancel_peer(&id);
            if let Some(link)=self.links.remove(&id){link.cancel.store(true,std::sync::atomic::Ordering::Release);}self.publish_participants()
        },
        Command::Stop=>{if self.authority.is_some(){self.broadcast(&Wire::Goodbye("The host ended the session. Shared media is no longer available.".into()));}else if let Some(invite)=&self.invite{let _=self.send(&invite.peer,&Wire::Leave);}self.finished=true;Ok(())},
    }}
}
enum EditKind{Propose(Patch),Undo(OperationId),Redo(OperationId)}
fn run(initial:Initial,commands:Receiver<Command>,events:SyncSender<Event>,relay:super::peer::RelaySettings)->Result<(),String>{
    let mut guest_room=None;
    let mut public_offline=false;
    let (initial,relay,service)=match initial{
        Initial::JoinRoom(mut room)=>{let(code,mut config,generation)=room.fetch_state()?;room.generation=generation;let Signal::Offer(invite)=decode(&code)? else{return Err("Relay returned an invalid invitation".into())};validate_invitation(&invite)?;config.relay_only|=relay.relay_only;guest_room=Some(room);(Initial::Join(invite),config,None)},
        Initial::Join(invite) if relay.uses_public()=>(Initial::Join(invite),relay,None),
        initial=>{match super::signaling::Service::configure(&relay){
            Ok(Some((service,config)))=>(initial,config,Some(service)),Ok(None)=>(initial,relay,None),
            Err(_) if relay.uses_public()&&!relay.relay_only=>{public_offline=true;(initial,super::peer::RelaySettings::default(),None)},
            Err(error)=>return Err(error),
        }}
    };
    let mut worker=Worker{last_transfer_update:Instant::now(),last_transfers:Vec::new(),preview_latency:BTreeMap::new(),preview_pending:BTreeMap::new(),preview_inflight:BTreeMap::new(),waveforms:None,upload_active:false,reconnecting:false,last_seen:Instant::now(),last_ping:Instant::now(),links:BTreeMap::new(),sources:BTreeMap::new(),renderers:BTreeMap::new(),render_project:None,aux:Default::default(),transfers:super::transfers::Transfers::new(&uuid::Uuid::new_v4().to_string()),media_available:false,signaling:super::signaling::Tasks::new(),service,guest_room,transport:Transport::new(relay)?,events,authority:None,audit:None,actor:String::new(),outbox:None,invite:None,peers:BTreeMap::new(),finished:false};
    match initial{
        Initial::Host(project,name)=>{
            worker.sources=project.media.iter().filter(|m|!m.path.as_os_str().is_empty()).map(|m|(m.id,m.path.clone())).collect();
            let epoch=uuid::Uuid::new_v4().to_string();let actor=uuid::Uuid::new_v4().to_string();let first=(project.next_id/ID_BLOCK+1)*ID_BLOCK;
            if first+64*ID_BLOCK>=i32::MAX as u64{return Err("This project has exhausted the ID space needed for a live session".into());}
            let mut authority=Authority::new(&project,epoch.clone())?;
            let member=Member{name:name.trim().chars().take(80).collect(),editor:true,first,limit:first+ID_BLOCK};
            let mut audit=Audit::create(crate::preferences::data_dir().join("sessions").join(format!("{epoch}.qsession")),&authority)?;
            audit.append(&Record::Member{actor:actor.clone(),member:member.clone()})?;authority.members.insert(actor.clone(),member.clone());
            worker.actor=actor.clone();worker.emit(Event::Ready{epoch,actor,member,document:authority.document.clone(),revision:0,next_sequence:1,host:true})?;
            worker.authority=Some(authority);worker.audit=Some(audit);worker.publish_participants()?;
        },
        Initial::JoinRoom(_)=>unreachable!("Room invitation was resolved before creating the transport"),
        Initial::Join(invite)=>{worker.actor=invite.peer.clone();worker.transport.answer(invite.peer.clone(),invite.description.clone())?;worker.invite=Some(invite);worker.emit(Event::Status(if worker.guest_room.is_some(){"Connecting to the host…"}else{"Preparing your reply…"}.into()))?;},
    }
    if public_offline{worker.emit(Event::Status("Public service unavailable. Direct/LAN invitations still work using invitation and reply files.".into()))?;}
    while !worker.finished{
        for _ in 0..32{match commands.try_recv(){Ok(c)=>{
            let id=match &c{Command::Propose(p,_)=>Some(p.id.clone()),Command::Undo(id)|Command::Redo(id)=>Some(id.clone()),_=>None};
            if let Err(error)=worker.command(c){if let Some(id)=id{
                if let Some(outbox)=&mut worker.outbox{let _=outbox.reject(&id,&error);}
                worker.emit(Event::Rejected{id,reason:error})?;
            }else{worker.emit(Event::Status(error))?;}}
        },Err(mpsc::TryRecvError::Disconnected)=>{worker.finished=true;break;},Err(_)=>break}}
        for _ in 0..16{match worker.signaling.rx.try_recv(){
            Ok(super::signaling::Event::Registered(peer,link))=>{worker.links.insert(peer,link);},
            Ok(super::signaling::Event::Reconnect(peer,config))=>{
                if worker.peers.get(&peer).is_some_and(|p|!p.revoked){
                    worker.transport.close(peer.clone());worker.aux.retain(|(id,_)|id!=&peer);worker.renderers.retain(|(id,_),_|id!=&peer);worker.preview_latency.retain(|(id,_),_|id!=&peer);worker.preview_pending.retain(|(id,_),_|id!=&peer);worker.preview_inflight.retain(|(id,_),_|id!=&peer);if let Some(waveforms)=&mut worker.waveforms{waveforms.cancel_peer(&peer);}
                    if let Some(p)=worker.peers.get_mut(&peer){p.authenticated=false;p.opened=None;p.route.clear();}
                    worker.transport.configure(config)?;worker.transport.offer(peer)?;
                }
            },
            Ok(super::signaling::Event::Resume(room,code,config))=>{
                let Signal::Offer(invite)=decode(&code)?else{return Err("Invalid reconnect invitation".into());};validate_invitation(&invite)?;
                let original=worker.invite.as_ref().ok_or("Missing original invitation")?;
                if invite.peer!=original.peer||invite.epoch!=original.epoch||invite.secret!=original.secret{return Err("Reconnect identity changed".into());}
                worker.transport.configure(config)?;worker.guest_room=Some(room);worker.transport.answer(invite.peer.clone(),invite.description.clone())?;worker.invite=Some(invite);
            },
            Ok(super::signaling::Event::ResumeFailed(reason))=>{worker.finished=true;worker.emit(Event::Ended(reason))?;},
            Ok(super::signaling::Event::Code(code))=>worker.emit(Event::Code(code))?,
            Ok(super::signaling::Event::Answer(code))=>{if let Err(error)=worker.command(Command::Signal(code)){worker.emit(Event::Status(error))?;}},
            Ok(super::signaling::Event::Error(error))=>worker.emit(Event::OperationFailed(error))?,
            Err(_)=>break,
        }}
        for _ in 0..32{match worker.transport.rx.try_recv(){Ok(e)=>worker.peer_event(e)?,Err(mpsc::TryRecvError::Disconnected)=>return Err("Peer transport stopped".into()),Err(_)=>break}}
        worker.pump_media()?;
        if worker.authority.is_none()&&worker.outbox.is_some()&&!worker.reconnecting{
            if worker.last_seen.elapsed()>Duration::from_secs(15){if let Err(error)=worker.reconnect(){worker.finished=true;worker.emit(Event::Ended(error))?;}}
            else if worker.last_ping.elapsed()>Duration::from_secs(2){worker.last_ping=Instant::now();let peer=worker.invite.as_ref().unwrap().peer.clone();let _=worker.send(&peer,&Wire::Ping);}
        }
        let expired:Vec<_>=worker.peers.iter().filter(|(_,p)|!p.authenticated&&p.opened.is_some_and(|at|at.elapsed()>Duration::from_secs(15))).map(|(id,_)|id.clone()).collect();
        for id in expired{worker.transport.close(id);}
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}
