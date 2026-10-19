//! Carries opaque TURN datagrams over HTTPS; DTLS/SCTP still encrypts peer edits.
use std::net::{SocketAddr,UdpSocket};
use std::sync::{Arc,Mutex,atomic::{AtomicBool,Ordering}};
use std::time::Duration;
use std::io::Read;
use base64::{Engine,engine::general_purpose::STANDARD};
use serde_json::{Value,json};

fn request(agent:&ureq::Agent,url:&str,method:&str,token:&str,body:Option<Value>)->Result<Value,String>{
    let request=agent.request(method,url).set("Authorization",&format!("Bearer {token}"));
    let response=match body{Some(value)=>request.send_json(value),None=>request.call()}.map_err(|_|"HTTPS relay is unavailable")?;
    let mut bytes=Vec::new();response.into_reader().take(768*1024+1).read_to_end(&mut bytes).map_err(|_|"Could not read HTTPS relay")?;
    if bytes.len()>768*1024{return Err("HTTPS relay response is too large".into());}
    serde_json::from_slice(&bytes).map_err(|_|"Invalid HTTPS relay response".into())
}

pub struct Tunnel {stop:Arc<AtomicBool>,threads:Vec<std::thread::JoinHandle<()>>,pub address:SocketAddr}
impl Tunnel {
    pub fn start(url:&str,token:&str)->Result<Self,String>{
        super::signaling::validate_url(url)?;
        if token.len()>512||token.is_empty(){return Err("Invalid HTTPS relay credentials".into());}
        let base=format!("{}/v1/tunnels",url.trim_end_matches('/'));
        let agent=ureq::AgentBuilder::new().redirects(0).timeout(Duration::from_secs(4)).build();
        let value=request(&agent,&base,"POST",token,None)?;
        let id=value["id"].as_str().ok_or("HTTPS relay returned no connection ID")?;
        if id.len()!=32||!id.bytes().all(|b|b.is_ascii_hexdigit()){return Err("Invalid HTTPS relay connection ID".into());}
        let path=format!("{base}/{id}");
        let socket=Arc::new(UdpSocket::bind("127.0.0.1:0").map_err(|_|"Could not open local HTTPS relay bridge")?);
        socket.set_read_timeout(Some(Duration::from_millis(100))).map_err(|e|e.to_string())?;
        let address=socket.local_addr().map_err(|e|e.to_string())?;
        let destination=Arc::new(Mutex::new(None::<SocketAddr>));
        let mut bridge=Self{stop:Arc::new(AtomicBool::new(false)),threads:Vec::new(),address};
        {
            let socket=socket.clone();let stop=bridge.stop.clone();let destination=destination.clone();let path=path.clone();let token=token.to_string();let agent=agent.clone();
            bridge.threads.push(std::thread::Builder::new().name("quaternio-relay-send".into()).spawn(move||{
                let mut buffer=[0u8;32768];
                while !stop.load(Ordering::Acquire){
                    let (len,source)=match socket.recv_from(&mut buffer){Ok(packet)=>packet,Err(e) if matches!(e.kind(),std::io::ErrorKind::WouldBlock|std::io::ErrorKind::TimedOut)=>continue,Err(_)=>break};
                    let mut peer=destination.lock().unwrap();
                    if !source.ip().is_loopback()||peer.is_some_and(|known|known!=source){continue;}
                    *peer=Some(source);drop(peer);
                    if len<4{continue;}
                    let mut packets=vec![STANDARD.encode(&buffer[..len])];let mut bytes=len;
                    let _=socket.set_read_timeout(Some(Duration::from_millis(1)));
                    while packets.len()<64&&bytes<480*1024 {
                        match socket.recv_from(&mut buffer){
                            Ok((len,from)) if from==source&&len>=4=>{bytes+=len;packets.push(STANDARD.encode(&buffer[..len]));},
                            Ok(_)=>continue,
                            Err(_)=>break,
                        }
                    }
                    let _=socket.set_read_timeout(Some(Duration::from_millis(100)));
                    let value=json!({"packets":packets});
                    if request(&agent,&path,"POST",&token,Some(value)).is_err(){std::thread::sleep(Duration::from_millis(100));}
                }
            }).map_err(|e|e.to_string())?);
        }
        {
            let stop=bridge.stop.clone();let token=token.to_string();
            bridge.threads.push(std::thread::Builder::new().name("quaternio-relay-receive".into()).spawn(move||{
                let mut failures=0;
                while !stop.load(Ordering::Acquire){
                    match request(&agent,&path,"GET",&token,None){
                        Ok(value)=>{
                            failures=0;
                            let Some(packets)=value["packets"].as_array().filter(|packets|packets.len()<=16) else{break};
                            for packet in packets {
                                let Some(encoded)=packet.as_str().filter(|s|s.len()<=44000) else{continue};
                                if let Ok(bytes)=STANDARD.decode(encoded){if bytes.len()<=32768{if let Some(peer)=*destination.lock().unwrap(){let _=socket.send_to(&bytes,peer);}}}
                            }
                        },
                        Err(_)=>{failures+=1;if failures>=5{break;}std::thread::sleep(Duration::from_millis(200));},
                    }
                }
                stop.store(true,Ordering::Release);
                let _=request(&agent,&path,"DELETE",&token,None);
            }).map_err(|e|e.to_string())?);
        }
        Ok(bridge)
    }
}
impl Drop for Tunnel {fn drop(&mut self){self.stop.store(true,Ordering::Release);for thread in self.threads.drain(..){let _=thread.join();}}}
