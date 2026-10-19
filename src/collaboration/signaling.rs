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
pub struct Invite {pub url:String,pub id:String,pub token:String}

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
    fn path(&self)->String{format!("{}/v1/rooms/{}",self.url.trim_end_matches('/'),self.id)}
    pub fn fetch(&self)->Result<(String,RelaySettings),String>{let data=request(&self.path(),"GET",&self.token,None)?;let mut config=relay(&data["relay"],false)?;if let Some(token)=data["relay"]["tunnel_token"].as_str(){config.tunnel_url=self.url.clone();config.tunnel_token=token.into();}Ok((data["offer"].as_str().ok_or("Relay returned no invitation")?.into(),config))}
    pub fn answer(&self,code:String)->Result<(),String>{request(&format!("{}/answer",self.path()),"POST",&self.token,Some(json!({"answer":code})))?;Ok(())}
}
pub enum Event{Code(String),Answer(String),Error(String)}
pub struct Tasks{pub rx:mpsc::Receiver<Event>,tx:mpsc::SyncSender<Event>,stop:Arc<AtomicBool>,threads:Vec<std::thread::JoinHandle<()>>}
impl Tasks{
    pub fn new()->Self{let(tx,rx)=mpsc::sync_channel(32);Self{rx,tx,stop:Arc::new(AtomicBool::new(false)),threads:Vec::new()}}
    pub fn publish(&mut self,service:Service,offer:String)->Result<(),String>{
        self.threads.retain(|t|!t.is_finished());
        if self.threads.len()>=7{return Err("Too many pending invitations".into());}
        let stop=self.stop.clone();let tx=self.tx.clone();
        self.threads.push(std::thread::Builder::new().name("quaternio-invite".into()).spawn(move||{
            let result=(||{
                let value=request(&format!("{}/v1/rooms",service.url),"POST",&service.key,Some(json!({"offer":offer})))?;
                let id=value["id"].as_str().ok_or("Relay returned no invitation ID")?.to_string();
                let host=value["host"].as_str().ok_or("Relay returned no host token")?.to_string();
                let guest=value["guest"].as_str().ok_or("Relay returned no guest token")?.to_string();
                let invite=Invite{url:service.url,id,token:guest};
                let code=format!("{PREFIX}{}",URL_SAFE_NO_PAD.encode(serde_json::to_vec(&invite).map_err(|e|e.to_string())?));
                parse(&code)?;
                tx.try_send(Event::Code(code)).map_err(|_|"Invitation queue stopped")?;
                let deadline=Instant::now()+Duration::from_secs(600);
                while !stop.load(Ordering::Acquire)&&Instant::now()<deadline{
                    let value=request(&format!("{}/answer",invite.path()),"GET",&host,None)?;
                    if let Some(answer)=value["answer"].as_str(){tx.try_send(Event::Answer(answer.into())).map_err(|_|"Invitation queue stopped")?;let _=request(&invite.path(),"DELETE",&host,None);return Ok(());}
                    for _ in 0..5{if stop.load(Ordering::Acquire){break;}std::thread::sleep(Duration::from_millis(100));}
                }
                let _=request(&invite.path(),"DELETE",&host,None);
                if stop.load(Ordering::Acquire){Ok(())}else{Err("Invitation expired. Create a new invite code".into())}
            })();
            if let Err(error)=result{let _=tx.try_send(Event::Error(error));}
        }).map_err(|e|e.to_string())?);Ok(())
    }
}
impl Drop for Tasks{fn drop(&mut self){self.stop.store(true,Ordering::Release);for thread in self.threads.drain(..){let _=thread.join();}}}
