//! Encrypted, ordered WebRTC channels with bounded application framing.
//! Session settings select optional public/private coordination and relay routes.
use std::sync::{Arc,Mutex,mpsc};
use std::sync::atomic::{AtomicBool,AtomicUsize,Ordering};
use std::collections::BTreeMap;
use std::time::{Duration,Instant};
use bytes::BytesMut;
use tokio::sync::mpsc as channel;
use webrtc::data_channel::{DataChannel,DataChannelEvent};
use webrtc::peer_connection::{PeerConnection,PeerConnectionBuilder,PeerConnectionEventHandler,RTCIceGatheringState,RTCIceConnectionState,RTCPeerConnectionState,RTCConfigurationBuilder,RTCIceTransportPolicy,RTCIceServer,SettingEngineBuilder};
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RelaySettings {
    #[serde(default = "public_default")]
    pub use_public:bool,
    pub service_url:String,
    #[serde(skip)]
    pub access_key:String,
    pub url:String,
    pub username:String,
    #[serde(skip)]
    pub password:String,
    pub relay_only:bool,
    #[serde(skip)]
    pub tunnel_url:String,
    #[serde(skip)]
    pub tunnel_token:String,
}
fn public_default()->bool{true}
impl RelaySettings {
    pub fn uses_public(&self)->bool{self.use_public&&self.service_url.is_empty()&&self.url.trim().is_empty()&&std::env::var_os("QUATERNIO_ICE_SERVERS").is_none()}
    pub fn validate(&self)->Result<(),String>{
        if self.uses_public(){return Ok(());}
        if !self.service_url.is_empty(){super::signaling::validate_url(&self.service_url)?;if self.access_key.is_empty()||self.access_key.len()>512{return Err("Enter the private relay service access key".into());}return Ok(());}
        self.configuration().map(|_|())
    }
    fn configuration(&self)->Result<(Vec<RTCIceServer>,bool),String>{
        if self.url.len()>2048||self.username.len()>512||self.password.len()>4096{return Err("Relay settings exceed supported lengths".into());}
        let mut servers=if self.url.trim().is_empty(){
            match std::env::var("QUATERNIO_ICE_SERVERS") {Ok(text)=>serde_json::from_str::<Vec<RTCIceServer>>(&text).map_err(|_|"Invalid ICE server configuration")?,Err(_)=>Vec::new()}
        }else{
            let urls:Vec<String>=self.url.split(',').map(|url|url.trim().to_string()).collect();
            if urls.iter().any(|url|!url.starts_with("turn:")&&!url.starts_with("turns:")){return Err("Use a TURN server address beginning with turn: or turns:".into());}
            vec![RTCIceServer{urls,username:self.username.clone(),credential:self.password.clone()}]
        };
        if servers.len()>8{return Err("At most eight ICE servers are supported".into());}
        for server in &servers{server.urls().map_err(|_|"Check the relay address, username and password")?;}
        let has_turn=servers.iter().flat_map(|s|&s.urls).any(|url|url.starts_with("turn:")||url.starts_with("turns:"));
        let has_udp_turn=servers.iter().flat_map(|s|&s.urls).any(|url|url.starts_with("turn:")&&!url.contains("transport=tcp"));
        if has_turn&&!has_udp_turn{return Err("This version requires TURN over UDP. TCP and TLS relays are not supported yet".into());}
        let only=self.relay_only||std::env::var("QUATERNIO_RELAY_ONLY").as_deref()==Ok("1");
        if only&&!servers.iter().flat_map(|s|&s.urls).any(|url|url.starts_with("turn:")||url.starts_with("turns:")){return Err("Relay-only connections need a TURN server".into());}
        if !only {
            let stun:Vec<String>=servers.iter().flat_map(|server|&server.urls).filter_map(|url|{
                let address=url.strip_prefix("turn:")?;
                if address.contains("transport=tcp"){return None;}
                Some(format!("stun:{}",address.split('?').next().unwrap_or(address)))
            }).collect();
            if !stun.is_empty(){servers.push(RTCIceServer{urls:stun,username:String::new(),credential:String::new()});}
        }
        Ok((servers,only))
    }
}

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
    Opened(String),Message(String,Payload),AuxOpened(String,u8),AuxMessage(String,u8,Payload),Closed(String),Error(String,String),State(String,String),Route(String,bool),
}
enum Command {
    Offer(String),Answer(String,RTCSessionDescription),Accept(String,RTCSessionDescription),Send(String,u8,Payload),Configure(RelaySettings),Close(String),Stop,
}
pub struct Transport {budget:Arc<AtomicUsize>,tx:Option<channel::Sender<Command>>,pub rx:mpsc::Receiver<Event>,stop:Arc<AtomicBool>,thread:Option<std::thread::JoinHandle<()>>}
impl Transport {
    pub fn new(relay:RelaySettings)->Result<Self,String>{
        relay.validate()?;
        let (tx,rx)=channel::channel(128);let (events,receive)=mpsc::sync_channel(256);
        let stop=Arc::new(AtomicBool::new(false));let stopping=stop.clone();let budget=Arc::new(AtomicUsize::new(0));let thread_budget=budget.clone();
        let thread=std::thread::Builder::new().name("quaternio-peers".into()).spawn(move||{
            let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build();
            match runtime {Ok(r)=>r.block_on(run(rx,events.clone(),stopping,thread_budget,relay)),Err(e)=>{let _=events.send(Event::Error(String::new(),e.to_string()));}}
        }).map_err(|e|e.to_string())?;
        Ok(Self{budget,tx:Some(tx),rx:receive,stop,thread:Some(thread)})
    }
    fn command(&self,c:Command)->Result<(),String>{self.tx.as_ref().ok_or("Peer transport stopped")?.try_send(c).map_err(|_|"Peer queue is full or stopped; your local edits are retained".into())}
    pub fn offer(&self,peer:String)->Result<(),String>{self.command(Command::Offer(peer))}
    pub fn answer(&self,peer:String,description:RTCSessionDescription)->Result<(),String>{self.command(Command::Answer(peer,description))}
    pub fn accept(&self,peer:String,description:RTCSessionDescription)->Result<(),String>{self.command(Command::Accept(peer,description))}
    pub fn send(&self,peer:String,bytes:Vec<u8>)->Result<(),String>{if bytes.len()>MAX_BYTES{return Err("Peer message exceeds 32 MiB".into());}self.command(Command::Send(peer,0,Payload::reserve(bytes,self.budget.clone())?))}
    pub fn send_aux(&self,peer:String,lane:u8,bytes:Vec<u8>)->Result<(),String>{
        if !(1..=2).contains(&lane)||bytes.len()>2*1024*1024{return Err("Invalid media packet".into());}
        self.command(Command::Send(peer,lane,Payload::reserve(bytes,self.budget.clone())?))
    }
    pub fn configure(&self,relay:RelaySettings)->Result<(),String>{relay.validate()?;self.command(Command::Configure(relay))}
    pub fn close(&self,peer:String){let _=self.command(Command::Close(peer));}
}
impl Drop for Transport {fn drop(&mut self){
    self.stop.store(true,Ordering::Release);
    if let Some(tx)=self.tx.take(){let _=tx.try_send(Command::Stop);drop(tx);}
    if let Some(thread)=self.thread.take(){let _=thread.join();}
}}

#[derive(Clone)]
struct Link{tx:channel::Sender<Payload>,dc:Arc<dyn DataChannel>,queued:Arc<AtomicUsize>}
type Senders=Arc<Mutex<BTreeMap<(String,u8),Link>>>;
struct Handler {live:Arc<AtomicBool>,peer:String,events:mpsc::SyncSender<Event>,senders:Senders,budget:Arc<AtomicUsize>,gathered:Arc<AtomicBool>}
#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self,state:RTCIceGatheringState){if state==RTCIceGatheringState::Complete{self.gathered.store(true,Ordering::Release);}}
    async fn on_ice_connection_state_change(&self,state:RTCIceConnectionState){if !self.live.load(Ordering::Acquire){return;}let _=self.events.try_send(Event::State(self.peer.clone(),format!("ICE {state:?}")));}
    async fn on_connection_state_change(&self,state:RTCPeerConnectionState){
        if !self.live.load(Ordering::Acquire){return;}
        let _=self.events.try_send(Event::State(self.peer.clone(),format!("Peer {state:?}")));
        if matches!(state,RTCPeerConnectionState::Failed|RTCPeerConnectionState::Closed){let _=self.events.try_send(Event::Closed(self.peer.clone()));}
    }
    async fn on_data_channel(&self,dc:Arc<dyn DataChannel>){
        let peer=self.peer.clone();let live=self.live.clone();let events=self.events.clone();let senders=self.senders.clone();let budget=self.budget.clone();
        tokio::spawn(async move{
            let lane=match dc.label().await.ok().as_deref(){Some("quaternio-edit-v1")=>0,Some("quaternio-preview-v1")=>1,Some("quaternio-upload-v1")=>2,_=>{let _=dc.close().await;return;}};
            attach(peer,lane,live,dc,events,senders,budget);
        });
    }
}

fn attach(peer:String,lane:u8,live:Arc<AtomicBool>,dc:Arc<dyn DataChannel>,events:mpsc::SyncSender<Event>,senders:Senders,budget:Arc<AtomicUsize>){
    let (tx,mut rx)=channel::channel::<Payload>(8);let queued=Arc::new(AtomicUsize::new(0));
    let link=Link{tx,dc:dc.clone(),queued:queued.clone()};
    let duplicate={let mut map=senders.lock().unwrap();if map.contains_key(&(peer.clone(),lane)){true}else{map.insert((peer.clone(),lane),link);false}};
    if duplicate{
        let events=events.clone();tokio::spawn(async move{let _=dc.close().await;let _=events.try_send(Event::Error(peer,"Duplicate data channel".into()));});return;
    }
    tokio::spawn(async move{
        let (open_tx,open_rx)=tokio::sync::oneshot::channel();let mut open_tx=Some(open_tx);
        let writer_live=live.clone();let writer_dc=dc.clone();let writer_peer=peer.clone();let writer_events=events.clone();
        let writer=tokio::spawn(async move{
            if open_rx.await.is_err(){return;}
            while let Some(packet)=rx.recv().await{
                let bytes=&packet.bytes;
                let result=tokio::time::timeout(Duration::from_secs(30),async {
                    writer_dc.send(BytesMut::from(&(bytes.len() as u32).to_be_bytes()[..])).await?;
                    for chunk in bytes.chunks(1024){writer_dc.send(BytesMut::from(chunk)).await?;}Ok::<_,webrtc::error::Error>(())
                }).await;
                queued.fetch_sub(1,Ordering::AcqRel);
                if !matches!(result,Ok(Ok(()))){
                    let reason=match result{Ok(Err(error))=>format!("Could not send peer data: {error}"),_=>"Peer transfer stalled for 30 seconds".into()};
                    if writer_live.load(Ordering::Acquire){let _=writer_events.try_send(Event::Error(writer_peer.clone(),reason));}let _=writer_dc.close().await;break;
                }
            }
        });
        // Poll channel events while the writer waits for send-buffer capacity.
        // Otherwise backpressure can prevent consumption of progress events.
        let mut read=Vec::new();
        'read: loop {
            if !live.load(Ordering::Acquire){break;}
            match dc.poll().await {
                Some(DataChannelEvent::OnOpen)=>{
                    if let Some(open)=open_tx.take(){let _=open.send(());if events.try_send(if lane==0{Event::Opened(peer.clone())}else{Event::AuxOpened(peer.clone(),lane)}).is_err(){break;}}
                },
                Some(DataChannelEvent::OnMessage(message))=>{
                    if message.data.len()>16*1024||read.len()+message.data.len()>if lane==0{MAX_BYTES+4}else{2*1024*1024+4} {break;}
                    read.extend_from_slice(&message.data);
                    loop {
                        if read.len()<4{break;}
                        let len=u32::from_be_bytes(read[..4].try_into().unwrap()) as usize;
                        if len>if lane==0{MAX_BYTES}else{2*1024*1024}||len==0 {let _=events.try_send(Event::Error(peer.clone(),"Invalid peer message length".into()));break 'read;}
                        if read.len()<len+4{break;}
                        read.drain(..4);let tail=read.split_off(len);let bytes=std::mem::replace(&mut read,tail);
                        let packet=match Payload::reserve(bytes,budget.clone()){Ok(packet)=>packet,Err(reason)=>{let _=events.try_send(Event::Error(peer.clone(),reason));break 'read;}};
                        if events.try_send(if lane==0{Event::Message(peer.clone(),packet)}else{Event::AuxMessage(peer.clone(),lane,packet)}).is_err(){break 'read;}
                    }
                },
                Some(DataChannelEvent::OnClose|DataChannelEvent::OnError)|None=>break,
                _=>{},
            }
        }
        writer.abort();let _=writer.await;
        {let mut map=senders.lock().unwrap();if map.get(&(peer.clone(),lane)).is_some_and(|link|Arc::ptr_eq(&link.dc,&dc)){map.remove(&(peer.clone(),lane));}}let _=dc.close().await;if lane==0&&live.load(Ordering::Acquire){let _=events.try_send(Event::Closed(peer));}
    });
}
async fn connection(peer:String,live:Arc<AtomicBool>,events:mpsc::SyncSender<Event>,senders:Senders,budget:Arc<AtomicUsize>,relay:&RelaySettings)->Result<(Arc<dyn PeerConnection>,Arc<AtomicBool>,Option<super::tunnel::Tunnel>),String>{
    let gathered=Arc::new(AtomicBool::new(false));
    let handler=Handler{live,peer,events,senders,budget,gathered:gathered.clone()};
    let loopback=std::env::var_os("QUATERNIO_PEER_LOOPBACK").is_some();
    let (mut servers,only)=relay.configuration()?;
    let tunnel=if relay.tunnel_url.is_empty(){None}else{
        let url=relay.tunnel_url.clone();let token=relay.tunnel_token.clone();
        match tokio::task::spawn_blocking(move||super::tunnel::Tunnel::start(&url,&token)).await.map_err(|e|e.to_string())? {
            Ok(tunnel)=>{servers.push(RTCIceServer{urls:vec![format!("turn:{}?transport=udp",tunnel.address)],username:relay.username.clone(),credential:relay.password.clone()});Some(tunnel)},
            Err(error) if std::env::var_os("QUATERNIO_HTTPS_RELAY_ONLY").is_some()=>return Err(error),
            Err(_)=>None,
        }
    };
    if std::env::var_os("QUATERNIO_HTTPS_RELAY_ONLY").is_some(){
        let address=tunnel.as_ref().ok_or("HTTPS relay is not configured")?.address.to_string();
        servers.retain(|server|server.urls.iter().any(|url|url.contains(&address)));
    }
    // The HTTPS bridge listens only on loopback. Keep a dedicated local socket
    // alongside ordinary interfaces so restrictive networks can use that route.
    let mut bindings=vec![if loopback{"127.0.0.1:0"}else{"0.0.0.0:0"}];
    if tunnel.is_some()&&!loopback{bindings.push("127.0.0.1:0");}
    let pc=PeerConnectionBuilder::new().with_configuration(RTCConfigurationBuilder::default().with_ice_servers(servers)
        .with_ice_transport_policy(if only{RTCIceTransportPolicy::Relay}else{RTCIceTransportPolicy::All}).build())
        .with_handler(Arc::new(handler)).with_setting_engine(SettingEngineBuilder::default().with_include_loopback_candidate(loopback||tunnel.is_some()).build()).with_udp_addrs(bindings).with_data_channel_send_buffer_limit(256*1024)
        .build().await.map_err(|e|e.to_string())?;
    Ok((Arc::new(pc),gathered,tunnel))
}
async fn description(pc:&Arc<dyn PeerConnection>,gathered:&AtomicBool,stop:&AtomicBool)->Result<RTCSessionDescription,String>{
    // Unreachable UDP servers must not discard usable direct or HTTPS candidates.
    let start=Instant::now();while !gathered.load(Ordering::Acquire){if stop.load(Ordering::Acquire){return Err("Peer setup cancelled".into());}if start.elapsed()>Duration::from_secs(15){break;}tokio::time::sleep(Duration::from_millis(20)).await;}
    let description=pc.local_description().await.ok_or("Missing local peer description".to_string())?;
    if !description.sdp.lines().any(|line|line.starts_with("a=candidate:")){return Err("No connection route was found. Check the TURN credentials, UDP port and relay firewall".into());}
    Ok(description)
}
type Created=(String,Arc<dyn PeerConnection>,Option<super::tunnel::Tunnel>,Arc<AtomicBool>);
async fn prepare_connection(peer:String,offer:Option<RTCSessionDescription>,live:Arc<AtomicBool>,events:mpsc::SyncSender<Event>,senders:Senders,budget:Arc<AtomicUsize>,relay:RelaySettings,stop:Arc<AtomicBool>,created:channel::Sender<Created>){
    let result=async{
        let (pc,gathered,tunnel)=connection(peer.clone(),live.clone(),events.clone(),senders.clone(),budget.clone(),&relay).await?;
        if !live.load(Ordering::Acquire)||stop.load(Ordering::Acquire){let _=pc.close().await;return Ok(());}
        created.send((peer.clone(),pc.clone(),tunnel,live.clone())).await.map_err(|_|"Peer setup was cancelled")?;
        let is_offer=offer.is_none();
        if let Some(offer)=offer{
            pc.set_remote_description(offer).await.map_err(|e|e.to_string())?;
            let answer=pc.create_answer(None).await.map_err(|e|e.to_string())?;pc.set_local_description(answer).await.map_err(|e|e.to_string())?;
        }else{
            for(lane,label)in[(0,"quaternio-edit-v1"),(1,"quaternio-preview-v1"),(2,"quaternio-upload-v1")]{let dc=pc.create_data_channel(label,None).await.map_err(|e|e.to_string())?;attach(peer.clone(),lane,live.clone(),dc,events.clone(),senders.clone(),budget.clone());}
            let offer=pc.create_offer(None).await.map_err(|e|e.to_string())?;pc.set_local_description(offer).await.map_err(|e|e.to_string())?;
        }
        let local=description(&pc,&gathered,&stop).await?;
        if live.load(Ordering::Acquire)&&!stop.load(Ordering::Acquire){events.try_send(Event::Description{peer:peer.clone(),description:local,offer:is_offer}).map_err(|_|"Peer event queue is full")?;}
        Ok::<_,String>(())
    }.await;
    if let Err(error)=result{if live.load(Ordering::Acquire)&&!stop.load(Ordering::Acquire){let _=events.try_send(Event::Error(peer,error));}}
}
async fn run(mut commands:channel::Receiver<Command>,events:mpsc::SyncSender<Event>,stop:Arc<AtomicBool>,budget:Arc<AtomicUsize>,mut relay:RelaySettings){
    let senders:Senders=Arc::new(Mutex::new(BTreeMap::new()));let mut connections:BTreeMap<String,Arc<dyn PeerConnection>>=BTreeMap::new();
    let mut tunnels:BTreeMap<String,super::tunnel::Tunnel>=BTreeMap::new();
    let(mut tasks,mut cleanup)=(tokio::task::JoinSet::new(),tokio::task::JoinSet::new());
    let(created_tx,mut created_rx)=channel::channel::<Created>(8);
    let mut lives:BTreeMap<String,Arc<AtomicBool>>=BTreeMap::new();let mut routes=BTreeMap::new();let mut route_tick=tokio::time::interval(Duration::from_secs(1));
    loop {
        let command=tokio::select!{
            biased;
            ready=created_rx.recv()=>{
                if let Some((peer,pc,tunnel,live))=ready{
                    if lives.get(&peer).is_some_and(|current|Arc::ptr_eq(current,&live)&&live.load(Ordering::Acquire)){
                        connections.insert(peer.clone(),pc);if let Some(tunnel)=tunnel{tunnels.insert(peer,tunnel);}
                    }else{let _=pc.close().await;if let Some(tunnel)=tunnel{cleanup.spawn_blocking(move||drop(tunnel));}}
                }continue;
            },
            _=tasks.join_next(),if !tasks.is_empty()=>continue,
            _=cleanup.join_next(),if !cleanup.is_empty()=>continue,
            command=commands.recv()=>{let Some(command)=command else{break};command},
            _=route_tick.tick()=>{
                for (peer,pc) in &connections {
                    if let Some(sctp)=pc.sctp().await {
                        if let Ok(Some(pair))=sctp.transport().ice_transport().get_selected_candidate_pair().await {
                            let relayed=pair.local().typ.to_string()=="relay"||pair.remote().typ.to_string()=="relay";
                            if routes.get(peer)!=Some(&relayed){routes.insert(peer.clone(),relayed);let _=events.try_send(Event::Route(peer.clone(),relayed));}
                        }
                    }
                }
                continue;
            },
        };
        let (peer,result)=match command {
            Command::Offer(peer)=>{
                if lives.len()>=7||lives.contains_key(&peer){(peer,Err("Session peer limit reached".into()))}else{
                    let live=Arc::new(AtomicBool::new(true));lives.insert(peer.clone(),live.clone());
                    tasks.spawn(prepare_connection(peer,None,live,events.clone(),senders.clone(),budget.clone(),relay.clone(),stop.clone(),created_tx.clone()));continue;
                }
            },
            Command::Answer(peer,offer)=>{
                if !lives.is_empty(){(peer,Err("Already connected to a session".into()))}else{
                    let live=Arc::new(AtomicBool::new(true));lives.insert(peer.clone(),live.clone());
                    tasks.spawn(prepare_connection(peer,Some(offer),live,events.clone(),senders.clone(),budget.clone(),relay.clone(),stop.clone(),created_tx.clone()));continue;
                }
            },
            Command::Accept(peer,answer)=>{let result=if let Some(pc)=connections.get(&peer){pc.set_remote_description(answer).await.map_err(|e|e.to_string())}else{Err("Invitation is no longer active".into())};(peer,result)},
            Command::Send(peer,lane,bytes)=>{let sender=senders.lock().unwrap().get(&(peer.clone(),lane)).cloned();let result=sender.ok_or("Peer channel is not ready".to_string()).and_then(|s|{
                s.queued.fetch_add(1,Ordering::AcqRel);if s.tx.try_send(bytes).is_err(){s.queued.fetch_sub(1,Ordering::AcqRel);Err("Peer is too slow; outbound queue is full".into())}else{Ok(())}
            });if lane!=0{if let Err(error)=result{let _=events.try_send(Event::State(peer,error));}continue;}(peer,result)},
            Command::Configure(config)=>{relay=config;continue;},
            Command::Close(peer)=>{if let Some(live)=lives.remove(&peer){live.store(false,Ordering::Release);}routes.remove(&peer);if let Some(pc)=connections.remove(&peer){let _=pc.close().await;}if let Some(tunnel)=tunnels.remove(&peer){cleanup.spawn_blocking(move||drop(tunnel));}senders.lock().unwrap().retain(|(id,_),_|id!=&peer);continue;},
            Command::Stop=>break,
        };
        if let Err(error)=result {if let Some(live)=lives.remove(&peer){live.store(false,Ordering::Release);}routes.remove(&peer);if let Some(pc)=connections.remove(&peer){let _=pc.close().await;}if let Some(tunnel)=tunnels.remove(&peer){cleanup.spawn_blocking(move||drop(tunnel));}senders.lock().unwrap().retain(|(id,_),_|id!=&peer);let _=events.try_send(Event::Error(peer,error));}
    }
    // Let queued edits and the final goodbye reach reliable-channel acknowledgement
    // before closing sockets. The deadline bounds shutdown on a lost peer.
    let deadline=Instant::now()+Duration::from_secs(3);let mut empty_once=false;
    while Instant::now()<deadline{
        let links:Vec<_>=senders.lock().unwrap().values().cloned().collect();let mut busy=false;
        for link in links{if link.queued.load(Ordering::Acquire)>0||link.dc.outstanding_bytes().await.unwrap_or(0)>0{busy=true;}}
        if !busy&&empty_once{break;}empty_once=!busy;tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for (_,live)in lives{live.store(false,Ordering::Release);}tasks.abort_all();while tasks.join_next().await.is_some(){}
    for (_,pc) in connections {let _=pc.close().await;}
    for(_,tunnel)in tunnels{cleanup.spawn_blocking(move||drop(tunnel));}
    while cleanup.join_next().await.is_some(){}
}
