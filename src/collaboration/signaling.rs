//! Private signaling exchanges invitations; document traffic stays on WebRTC.
use std::io::Read;
use std::sync::{Arc,atomic::{AtomicBool,Ordering},mpsc};
use std::time::{Duration,Instant};
use serde::{Serialize,Deserialize};
use serde_json::{Value,json};
use base64::{Engine,engine::general_purpose::URL_SAFE_NO_PAD};
use super::peer::RelaySettings;

const PREFIX:&str="quaternio-join-v1:";
pub const PUBLIC_SERVICE:&str="https://connect.quaternio.app";
#[derive(Clone,Serialize,Deserialize)]
pub struct Invite {pub url:String,pub id:String,pub token:String,#[serde(default)]pub generation:u64}

pub fn validate_url(url:&str)->Result<(),String>{
    if url.len()>2048||url.contains(['?','#','@'])||url.chars().any(char::is_whitespace){return Err("Enter the relay service's HTTPS address".into());}
    let secure=url.starts_with("https://")&&url.len()>8;
    let local=url.strip_prefix("http://").is_some_and(|s|s.split('/').next().unwrap_or("").split(':').next().is_some_and(|host|host=="127.0.0.1"||host=="localhost"));
    if !secure&&!local{return Err("Use HTTPS for the relay service (HTTP is only allowed on localhost)".into());}Ok(())
}
pub fn parse(code:&str)->Result<Option<Invite>,String>{
    let Some(body)=code.trim().strip_prefix(PREFIX) else{return Ok(None)};
    if body.len()>8192{return Err("Invite code is too long".into());}
    let invite:Invite=serde_json::from_slice(&URL_SAFE_NO_PAD.decode(body).map_err(|_|"Invalid invite code")?).map_err(|_|"Invalid invite code")?;
    validate_url(&invite.url)?;
    if invite.id.len()!=32||!invite.id.bytes().all(|b|b.is_ascii_hexdigit())||invite.token.len()<32||invite.token.len()>128{return Err("Invalid invite code".into());}
    Ok(Some(invite))
}
fn request(url:&str,method:&str,token:&str,body:Option<Value>)->Result<Value,String>{
    let agent=ureq::AgentBuilder::new().redirects(0).timeout(Duration::from_secs(3)).build();
    let req=agent.request(method,url).set("Authorization",&format!("Bearer {token}"));
    let response=match body{Some(body)=>req.send_json(body),None=>req.call()}.map_err(|e|match e{ureq::Error::Status(401,_)=>"Relay access was denied. Check the access key or invite code".to_string(),ureq::Error::Status(404,_)=>"This invitation expired. Ask for a new one".into(),_=>"Could not reach the relay service. Check its address and connection".into()})?;
    let mut data=Vec::new();response.into_reader().take(768*1024+1).read_to_end(&mut data).map_err(|_|"Could not read relay response")?;
    if data.len()>768*1024{return Err("Relay response exceeds the supported size".into());}
    serde_json::from_slice(&data).map_err(|_|"Invalid relay response".into())
}
fn relay(value:&Value,only:bool)->Result<RelaySettings,String>{
    let urls=value["urls"].as_array().ok_or("Relay returned no TURN addresses")?;
    let settings=RelaySettings{url:urls.iter().map(|v|v.as_str().ok_or("Invalid TURN address")).collect::<Result<Vec<_>,_>>()?.join(","),username:value["username"].as_str().ok_or("Relay returned no username")?.into(),password:value["password"].as_str().ok_or("Relay returned no password")?.into(),relay_only:only,..Default::default()};
    if settings.url.is_empty(){return Err("Relay returned no TURN addresses".into());}settings.validate()?;Ok(settings)
}
#[derive(Clone)]
pub struct Service{pub url:String,pub key:String}
impl Service{
    pub fn configure(settings:&RelaySettings)->Result<Option<(Self,RelaySettings)>,String>{
        let public=settings.uses_public();
        if settings.service_url.is_empty()&&!public{return Ok(None);}
        let url=if public{std::env::var("QUATERNIO_PUBLIC_SERVICE_URL").unwrap_or_else(|_|PUBLIC_SERVICE.into())}else{settings.service_url.clone()};validate_url(&url)?;
        let mut service=Self{url:url.trim_end_matches('/').into(),key:settings.access_key.clone()};
        let data=if public {
            let value=request(&format!("{}/v1/public",service.url),"POST","",None)?;
            let token=value["access_token"].as_str().filter(|t|t.len()==64&&t.bytes().all(|b|b.is_ascii_hexdigit())).ok_or("Public service returned invalid session credentials")?;
            service.key=token.into();value
        }else{request(&format!("{}/v1/config",service.url),"GET",&service.key,None)?};
        let mut config=relay(&data,settings.relay_only)?;
        if let Some(token)=data["tunnel_token"].as_str(){config.tunnel_url=service.url.clone();config.tunnel_token=token.into();}
        Ok(Some((service,config)))
    }
}
impl Invite{
    pub fn path(&self)->String{format!("{}/v1/rooms/{}",self.url.trim_end_matches('/'),self.id)}
    #[cfg(test)]
    pub fn fetch(&self)->Result<(String,RelaySettings),String>{let data=request(&self.path(),"GET",&self.token,None)?;let mut config=relay(&data["relay"],false)?;if let Some(token)=data["relay"]["tunnel_token"].as_str(){config.tunnel_url=self.url.clone();config.tunnel_token=token.into();}Ok((data["offer"].as_str().ok_or("Relay returned no invitation")?.into(),config))}
    pub fn fetch_state(&self)->Result<(String,RelaySettings,u64),String>{let data=request(&self.path(),"GET",&self.token,None)?;let mut config=relay(&data["relay"],false)?;if let Some(token)=data["relay"]["tunnel_token"].as_str(){config.tunnel_url=self.url.clone();config.tunnel_token=token.into();}Ok((data["offer"].as_str().ok_or("Missing reconnect offer")?.into(),config,data["generation"].as_u64().unwrap_or(0)))}
    pub fn answer(&self,code:String)->Result<(),String>{request(&format!("{}/answer",self.path()),"POST",&self.token,Some(json!({"answer":code,"generation":self.generation})))?;Ok(())}
}
#[derive(Clone)]
pub struct HostLink{pub invite:Invite,pub host:String,pub resume:String,pub cancel:Arc<AtomicBool>}
pub enum Event{Code(String),Answer(String),Registered(String,HostLink),Reconnect(String,RelaySettings),Resume(Invite,String,RelaySettings),ResumeFailed(String),Error(String)}
pub struct Tasks{pub rx:mpsc::Receiver<Event>,tx:mpsc::SyncSender<Event>,stop:Arc<AtomicBool>,threads:Vec<std::thread::JoinHandle<()>>}
impl Tasks{
    pub fn new()->Self{let(tx,rx)=mpsc::sync_channel(32);Self{rx,tx,stop:Arc::new(AtomicBool::new(false)),threads:Vec::new()}}
    pub fn publish(&mut self,service:Service,peer:String,offer:String)->Result<(),String>{
        self.threads.retain(|t|!t.is_finished());
        if self.threads.len()>=7{return Err("Too many pending invitations".into());}
        let stop=self.stop.clone();let tx=self.tx.clone();
        self.threads.push(std::thread::Builder::new().name("quaternio-invite".into()).spawn(move||{
            let result=(||{
                let value=request(&format!("{}/v1/rooms",service.url),"POST",&service.key,Some(json!({"offer":offer})))?;
                let id=value["id"].as_str().ok_or("Relay returned no invitation ID")?.to_string();
                let host=value["host"].as_str().ok_or("Relay returned no host token")?.to_string();
                let guest=value["guest"].as_str().ok_or("Relay returned no guest token")?.to_string();
                let invite=Invite{url:service.url,id,token:guest,generation:0};
                let cancel=Arc::new(AtomicBool::new(false));
                tx.try_send(Event::Registered(peer.clone(),HostLink{invite:invite.clone(),host:host.clone(),resume:value["resume"].as_str().unwrap_or("").into(),cancel:cancel.clone()})).map_err(|_|"Session queue stopped")?;
                let code=format!("{PREFIX}{}",URL_SAFE_NO_PAD.encode(serde_json::to_vec(&invite).map_err(|e|e.to_string())?));
                parse(&code)?;
                tx.try_send(Event::Code(code)).map_err(|_|"Invitation queue stopped")?;
                let mut last_answer=String::new();let mut requested=None::<Instant>;let mut seen_generation=0;
                let mut unavailable=Instant::now();
                while !stop.load(Ordering::Acquire)&&!cancel.load(Ordering::Acquire){
                    match request(&format!("{}/answer",invite.path()),"GET",&host,None){
                        Ok(value)=>{
                            unavailable=Instant::now();let generation=value["generation"].as_u64().unwrap_or(0);
                            if generation!=seen_generation{seen_generation=generation;requested=None;}
                            if value["reconnect"].as_bool()==Some(true)&&requested.is_none_or(|at|at.elapsed()>Duration::from_secs(30)){
                                let mut config=relay(&value["relay"],false)?;config.tunnel_url=invite.url.clone();config.tunnel_token=value["relay"]["tunnel_token"].as_str().unwrap_or("").into();
                                tx.try_send(Event::Reconnect(peer.clone(),config)).map_err(|_|"Reconnect queue stopped")?;requested=Some(Instant::now());
                            }
                            if let Some(answer)=value["answer"].as_str(){if answer!=last_answer{tx.try_send(Event::Answer(answer.into())).map_err(|_|"Answer queue stopped")?;last_answer=answer.into();}}
                        },
                        Err(_) if unavailable.elapsed()<Duration::from_secs(120)=>{},
                        Err(error)=>return Err(error),
                    }
                    for _ in 0..20{if stop.load(Ordering::Acquire){break;}std::thread::sleep(Duration::from_millis(100));}
                }
                let _=request(&invite.path(),"DELETE",&host,None);Ok(())
            })();
            if let Err(error)=result{let _=tx.try_send(Event::Error(error));}
        }).map_err(|e|e.to_string())?);Ok(())
    }
    pub fn activate(&mut self,link:HostLink)->Result<(),String>{
        self.threads.retain(|thread|!thread.is_finished());if self.threads.len()>=24{return Err("Too many connection operations".into());}
        self.threads.push(std::thread::spawn(move||{let _=request(&format!("{}/activate",link.invite.path()),"POST",&link.host,Some(json!({})));}));Ok(())
    }
    pub fn renew(&mut self,link:HostLink,offer:String)->Result<(),String>{
        self.threads.retain(|thread|!thread.is_finished());if self.threads.len()>=24{return Err("Too many connection operations".into());}
        let tx=self.tx.clone();self.threads.push(std::thread::spawn(move||{
            if let Err(error)=request(&format!("{}/offer",link.invite.path()),"POST",&link.host,Some(json!({"offer":offer}))){let _=tx.try_send(Event::Error(error));}
        }));Ok(())
    }
    pub fn reconnect(&mut self,mut invite:Invite)->Result<(),String>{
        self.threads.retain(|thread|!thread.is_finished());if self.threads.len()>=24{return Err("Too many reconnect operations".into());}
        let tx=self.tx.clone();let stop=self.stop.clone();self.threads.push(std::thread::spawn(move||{
            let deadline=Instant::now()+Duration::from_secs(120);
            while !stop.load(Ordering::Acquire)&&Instant::now()<deadline{
                let _=request(&format!("{}/reconnect",invite.path()),"POST",&invite.token,Some(json!({"generation":invite.generation})));
                match invite.fetch_state(){
                    Ok((code,config,generation))if generation>invite.generation=>{invite.generation=generation;let _=tx.try_send(Event::Resume(invite,code,config));return;},
                    Err(error)if error.contains("invitation expired")=>{let _=tx.try_send(Event::ResumeFailed("The host ended this session.".into()));return;},
                    _=>{},
                }
                for _ in 0..10{if stop.load(Ordering::Acquire){return;}std::thread::sleep(Duration::from_millis(100));}
            }
            if !stop.load(Ordering::Acquire){let _=tx.try_send(Event::ResumeFailed("The host could not be reached. The session may have ended; your edits remain recoverable.".into()));}
        }));Ok(())
    }

}
impl Drop for Tasks{fn drop(&mut self){self.stop.store(true,Ordering::Release);for thread in self.threads.drain(..){let _=thread.join();}}}
