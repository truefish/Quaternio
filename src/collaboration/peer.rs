//! Encrypted, ordered WebRTC channels with bounded application framing. SDP is
//! exchanged explicitly; no public signaling/STUN/TURN service is contacted by default.
use std::sync::{Arc,Mutex,mpsc};
use std::sync::atomic::{AtomicBool,AtomicUsize,Ordering};
use std::collections::BTreeMap;
use std::time::{Duration,Instant};
use bytes::BytesMut;
use tokio::sync::mpsc as channel;
use webrtc::data_channel::{DataChannel,DataChannelEvent};
use webrtc::peer_connection::{PeerConnection,PeerConnectionBuilder,PeerConnectionEventHandler,RTCIceGatheringState,RTCIceConnectionState,RTCPeerConnectionState,RTCConfigurationBuilder,RTCIceServer,SettingEngineBuilder};
pub use webrtc::peer_connection::RTCSessionDescription;
use super::document::MAX_BYTES;

pub struct Payload{pub bytes:Vec<u8>,budget:Arc<AtomicUsize>}
impl Payload{
    fn reserve(bytes:Vec<u8>,budget:Arc<AtomicUsize>)->Result<Self,String>{
        let len=bytes.len();let mut used=budget.load(Ordering::Acquire);
        loop{if used+len>128*1024*1024{return Err("Peer byte queue reached 128 MiB; local work is retained".into());}
            match budget.compare_exchange_weak(used,used+len,Ordering::AcqRel,Ordering::Acquire){Ok(_)=>return Ok(Self{bytes,budget}),Err(actual)=>used=actual}}
    }
}
impl Drop for Payload{fn drop(&mut self){self.budget.fetch_sub(self.bytes.len(),Ordering::AcqRel);}}
pub enum Event {
    Description{peer:String,description:RTCSessionDescription,offer:bool},
    Opened(String),Message(String,Payload),Closed(String),Error(String,String),State(String,String),
}
enum Command {
    Offer(String),Answer(String,RTCSessionDescription),Accept(String,RTCSessionDescription),Send(String,Payload),Close(String),Stop,
}
pub struct Transport {budget:Arc<AtomicUsize>,tx:Option<channel::Sender<Command>>,pub rx:mpsc::Receiver<Event>,stop:Arc<AtomicBool>,thread:Option<std::thread::JoinHandle<()>>}
impl Transport {
    pub fn new()->Result<Self,String>{
        let (tx,rx)=channel::channel(128);let (events,receive)=mpsc::sync_channel(256);
        let stop=Arc::new(AtomicBool::new(false));let stopping=stop.clone();let budget=Arc::new(AtomicUsize::new(0));let thread_budget=budget.clone();
        let thread=std::thread::Builder::new().name("quaternio-peers".into()).spawn(move||{
            let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build();
            match runtime {Ok(r)=>r.block_on(run(rx,events.clone(),stopping,thread_budget)),Err(e)=>{let _=events.send(Event::Error(String::new(),e.to_string()));}}
        }).map_err(|e|e.to_string())?;
        Ok(Self{budget,tx:Some(tx),rx:receive,stop,thread:Some(thread)})
    }
    fn command(&self,c:Command)->Result<(),String>{self.tx.as_ref().ok_or("Peer transport stopped")?.try_send(c).map_err(|_|"Peer queue is full or stopped; your local edits are retained".into())}
    pub fn offer(&self,peer:String)->Result<(),String>{self.command(Command::Offer(peer))}
    pub fn answer(&self,peer:String,description:RTCSessionDescription)->Result<(),String>{self.command(Command::Answer(peer,description))}
    pub fn accept(&self,peer:String,description:RTCSessionDescription)->Result<(),String>{self.command(Command::Accept(peer,description))}
    pub fn send(&self,peer:String,bytes:Vec<u8>)->Result<(),String>{if bytes.len()>MAX_BYTES{return Err("Peer message exceeds 32 MiB".into());}self.command(Command::Send(peer,Payload::reserve(bytes,self.budget.clone())?))}
    pub fn close(&self,peer:String){let _=self.command(Command::Close(peer));}
}
impl Drop for Transport {fn drop(&mut self){
    self.stop.store(true,Ordering::Release);
    if let Some(tx)=self.tx.take(){let _=tx.try_send(Command::Stop);drop(tx);}
    if let Some(thread)=self.thread.take(){let _=thread.join();}
}}

#[derive(Clone)]
struct Link{tx:channel::Sender<Payload>,dc:Arc<dyn DataChannel>,queued:Arc<AtomicUsize>}
type Senders=Arc<Mutex<BTreeMap<String,Link>>>;
struct Handler {peer:String,events:mpsc::SyncSender<Event>,senders:Senders,budget:Arc<AtomicUsize>,gathered:Arc<AtomicBool>}
#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self,state:RTCIceGatheringState){if state==RTCIceGatheringState::Complete{self.gathered.store(true,Ordering::Release);}}
    async fn on_ice_connection_state_change(&self,state:RTCIceConnectionState){let _=self.events.try_send(Event::State(self.peer.clone(),format!("ICE {state:?}")));}
    async fn on_connection_state_change(&self,state:RTCPeerConnectionState){
        let _=self.events.try_send(Event::State(self.peer.clone(),format!("Peer {state:?}")));
        if matches!(state,RTCPeerConnectionState::Failed|RTCPeerConnectionState::Closed){let _=self.events.try_send(Event::Closed(self.peer.clone()));}
    }
    async fn on_data_channel(&self,dc:Arc<dyn DataChannel>){
        let peer=self.peer.clone();let events=self.events.clone();let senders=self.senders.clone();let budget=self.budget.clone();
        tokio::spawn(async move{
            if dc.label().await.ok().as_deref()!=Some("quaternio-edit-v1"){let _=dc.close().await;return;}
            attach(peer,dc,events,senders,budget);
        });
    }
}

fn attach(peer:String,dc:Arc<dyn DataChannel>,events:mpsc::SyncSender<Event>,senders:Senders,budget:Arc<AtomicUsize>){
    let (tx,mut rx)=channel::channel::<Payload>(8);let queued=Arc::new(AtomicUsize::new(0));
    let link=Link{tx,dc:dc.clone(),queued:queued.clone()};
    let duplicate={let mut map=senders.lock().unwrap();if map.contains_key(&peer){true}else{map.insert(peer.clone(),link);false}};
    if duplicate{
        let events=events.clone();tokio::spawn(async move{let _=dc.close().await;let _=events.try_send(Event::Error(peer,"Duplicate data channel".into()));});return;
    }
    tokio::spawn(async move{
        let mut read=Vec::new();let mut opened=false;
        loop {
            tokio::select! {
                event=dc.poll()=>match event {
                    Some(DataChannelEvent::OnOpen)=>{opened=true;if events.try_send(Event::Opened(peer.clone())).is_err(){break;}},
                    Some(DataChannelEvent::OnMessage(message))=>{
                        if message.data.len()>16*1024||read.len()+message.data.len()>MAX_BYTES+4 {break;}
                        read.extend_from_slice(&message.data);
                        loop {
                            if read.len()<4{break;}
                            let len=u32::from_be_bytes(read[..4].try_into().unwrap()) as usize;
                            if len>MAX_BYTES||len==0 {let _=events.try_send(Event::Error(peer.clone(),"Invalid peer message length".into()));let _=dc.close().await;return;}
                            if read.len()<len+4{break;}
                            read.drain(..4);let tail=read.split_off(len);let bytes=std::mem::replace(&mut read,tail);
                            let packet=match Payload::reserve(bytes,budget.clone()){Ok(packet)=>packet,Err(reason)=>{let _=events.try_send(Event::Error(peer.clone(),reason));let _=dc.close().await;return;}};
                            if events.try_send(Event::Message(peer.clone(),packet)).is_err(){let _=dc.close().await;return;}
                        }
                    },
                    Some(DataChannelEvent::OnClose|DataChannelEvent::OnError)|None=>break,
                    _=>{},
                },
                bytes=rx.recv(),if opened=>{
                    let Some(packet)=bytes else {break;};let bytes=&packet.bytes;
                    let result=tokio::time::timeout(Duration::from_secs(30),async {
                        dc.send(BytesMut::from(&(bytes.len() as u32).to_be_bytes()[..])).await?;
                        for chunk in bytes.chunks(12*1024){dc.send(BytesMut::from(chunk)).await?;}Ok::<_,webrtc::error::Error>(())
                    }).await;
                    queued.fetch_sub(1,Ordering::AcqRel);
                    if !matches!(result,Ok(Ok(()))){break;}
                }
            }
        }
        senders.lock().unwrap().remove(&peer);let _=dc.close().await;let _=events.try_send(Event::Closed(peer));
    });
}
async fn connection(peer:String,events:mpsc::SyncSender<Event>,senders:Senders,budget:Arc<AtomicUsize>)->Result<(Arc<dyn PeerConnection>,Arc<AtomicBool>),String>{
    let gathered=Arc::new(AtomicBool::new(false));
    let handler=Handler{peer,events,senders,budget,gathered:gathered.clone()};
    // Advanced deployments can supply private STUN/TURN configuration. Empty
    // defaults preserve direct offline/LAN operation and avoid third-party services.
    let loopback=std::env::var_os("QUATERNIO_PEER_LOOPBACK").is_some();
    let servers:Vec<RTCIceServer>=if loopback{Vec::new()}else{match std::env::var("QUATERNIO_ICE_SERVERS") {
        Ok(text)=>serde_json::from_str(&text).map_err(|e|format!("Invalid ICE server configuration: {e}"))?,Err(_)=>Vec::new(),
    }};
    let pc=PeerConnectionBuilder::new().with_configuration(RTCConfigurationBuilder::default().with_ice_servers(servers).build())
        .with_handler(Arc::new(handler)).with_setting_engine(SettingEngineBuilder::default().with_include_loopback_candidate(loopback).build()).with_udp_addrs(vec![if loopback{"127.0.0.1:0"}else{"0.0.0.0:0"}]).with_data_channel_send_buffer_limit(256*1024)
        .build().await.map_err(|e|e.to_string())?;
    Ok((Arc::new(pc),gathered))
}
async fn description(pc:&Arc<dyn PeerConnection>,gathered:&AtomicBool,stop:&AtomicBool)->Result<RTCSessionDescription,String>{
    let start=Instant::now();while !gathered.load(Ordering::Acquire){if stop.load(Ordering::Acquire){return Err("Peer setup cancelled".into());}if start.elapsed()>Duration::from_secs(20){return Err("ICE gathering timed out; check the configured STUN/TURN service".into());}tokio::time::sleep(Duration::from_millis(20)).await;}
    pc.local_description().await.ok_or("Missing local peer description".into())
}
async fn run(mut commands:channel::Receiver<Command>,events:mpsc::SyncSender<Event>,stop:Arc<AtomicBool>,budget:Arc<AtomicUsize>){
    let senders:Senders=Arc::new(Mutex::new(BTreeMap::new()));let mut connections:BTreeMap<String,Arc<dyn PeerConnection>>=BTreeMap::new();
    while let Some(command)=commands.recv().await {
        let (peer,result)=match command {
            Command::Offer(peer)=>{
                let result=async {
                    if connections.len()>=7||connections.contains_key(&peer){return Err("Session peer limit reached".into());}
                    let (pc,gathered)=connection(peer.clone(),events.clone(),senders.clone(),budget.clone()).await?;
                    connections.insert(peer.clone(),pc.clone());
                    let dc=pc.create_data_channel("quaternio-edit-v1",None).await.map_err(|e|e.to_string())?;
                    attach(peer.clone(),dc,events.clone(),senders.clone(),budget.clone());
                    let offer=pc.create_offer(None).await.map_err(|e|e.to_string())?;pc.set_local_description(offer).await.map_err(|e|e.to_string())?;
                    let local=description(&pc,&gathered,&stop).await?;connections.insert(peer.clone(),pc);
                    events.try_send(Event::Description{peer:peer.clone(),description:local,offer:true}).map_err(|_|"Peer event queue is full".to_string())?;Ok(())
                }.await;(peer,result)
            },
            Command::Answer(peer,offer)=>{
                let result=async {
                    if !connections.is_empty(){return Err("Already connected to a session".into());}
                    let (pc,gathered)=connection(peer.clone(),events.clone(),senders.clone(),budget.clone()).await?;
                    connections.insert(peer.clone(),pc.clone());
                    pc.set_remote_description(offer).await.map_err(|e|e.to_string())?;
                    let answer=pc.create_answer(None).await.map_err(|e|e.to_string())?;pc.set_local_description(answer).await.map_err(|e|e.to_string())?;
                    let local=description(&pc,&gathered,&stop).await?;connections.insert(peer.clone(),pc);
                    events.try_send(Event::Description{peer:peer.clone(),description:local,offer:false}).map_err(|_|"Peer event queue is full".to_string())?;Ok(())
                }.await;(peer,result)
            },
            Command::Accept(peer,answer)=>{let result=if let Some(pc)=connections.get(&peer){pc.set_remote_description(answer).await.map_err(|e|e.to_string())}else{Err("Invitation is no longer active".into())};(peer,result)},
            Command::Send(peer,bytes)=>{let sender=senders.lock().unwrap().get(&peer).cloned();let result=sender.ok_or("Peer channel is not ready".to_string()).and_then(|s|{
                s.queued.fetch_add(1,Ordering::AcqRel);if s.tx.try_send(bytes).is_err(){s.queued.fetch_sub(1,Ordering::AcqRel);Err("Peer is too slow; outbound queue is full".into())}else{Ok(())}
            });(peer,result)},
            Command::Close(peer)=>{if let Some(pc)=connections.remove(&peer){let _=pc.close().await;}senders.lock().unwrap().remove(&peer);continue;},
            Command::Stop=>break,
        };
        if let Err(error)=result {if let Some(pc)=connections.remove(&peer){let _=pc.close().await;}senders.lock().unwrap().remove(&peer);let _=events.try_send(Event::Error(peer,error));}
    }
    // Let queued edits and the final goodbye reach reliable-channel acknowledgement
    // before closing sockets. The deadline bounds shutdown on a lost peer.
    let deadline=Instant::now()+Duration::from_secs(3);let mut empty_once=false;
    while Instant::now()<deadline{
        let links:Vec<_>=senders.lock().unwrap().values().cloned().collect();let mut busy=false;
        for link in links{if link.queued.load(Ordering::Acquire)>0||link.dc.outstanding_bytes().await.unwrap_or(0)>0{busy=true;}}
        if !busy&&empty_once{break;}empty_once=!busy;tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for (_,pc) in connections {let _=pc.close().await;}
}
