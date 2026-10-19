//! Persistent binary bridge; payloads remain encrypted TURN datagrams.
use super::*;
use futures_util::{SinkExt,StreamExt};
use tokio_tungstenite::{connect_async_with_config,tungstenite::{client::IntoClientRequest,protocol::WebSocketConfig,Message}};

pub(super) fn start(path:&str,token:&str)->Result<Tunnel,String>{
    let socket=UdpSocket::bind("127.0.0.1:0").map_err(|_|"Could not open relay bridge")?;
    socket.set_nonblocking(true).map_err(|e|e.to_string())?;
    let address=socket.local_addr().map_err(|e|e.to_string())?;
    let url=format!("{}/ws",path.replacen("https://","wss://",1).replacen("http://","ws://",1));
    let mut request=url.into_client_request().map_err(|_|"Invalid WebSocket relay address")?;
    request.headers_mut().insert("Authorization",format!("Bearer {token}").parse().map_err(|_|"Invalid relay credentials")?);
    let stop=Arc::new(AtomicBool::new(false));let stopping=stop.clone();
    let(ready,opened)=std::sync::mpsc::sync_channel(1);
    let thread=std::thread::Builder::new().name("quaternio-relay-stream".into()).spawn(move||{
        let Ok(runtime)=tokio::runtime::Builder::new_current_thread().enable_all().build()else{let _=ready.send(false);return;};
        runtime.block_on(async move{
            let mut config=WebSocketConfig::default();config.max_message_size=Some(32768);config.max_frame_size=Some(32768);config.write_buffer_size=0;config.max_write_buffer_size=65536;
            let connected=tokio::time::timeout(Duration::from_secs(4),connect_async_with_config(request,Some(config),true)).await;
            let Ok(Ok((stream,_)))=connected else{let _=ready.send(false);return;};
            let Ok(socket)=tokio::net::UdpSocket::from_std(socket)else{let _=ready.send(false);return;};
            if ready.send(true).is_err(){return;}
            let(write,mut read)=stream.split();let destination=Arc::new(Mutex::new(None::<SocketAddr>));
            let incoming=async{
                while let Some(message)=read.next().await{
                    match message{
                        Ok(Message::Binary(bytes)) if bytes.len()>=4&&bytes.len()<=32768=>{let peer=*destination.lock().unwrap();if let Some(peer)=peer{if socket.send_to(&bytes,peer).await.is_err(){break;}}},
                        Ok(Message::Ping(_)|Message::Pong(_))=>{},
                        _=>break,
                    }
                }
            };
            let outgoing=async{
                let mut write=write;let mut buffer=[0u8;32768];let mut flush=tokio::time::interval(Duration::from_millis(100));let mut keepalive=tokio::time::interval(Duration::from_secs(15));
                loop{tokio::select!{
                    packet=socket.recv_from(&mut buffer)=>{let Ok((len,peer))=packet else{break;};if !peer.ip().is_loopback()||len<4{continue;}
                        {let mut known=destination.lock().unwrap();if known.is_some_and(|known|known!=peer){continue;}*known=Some(peer);}
                        if write.send(Message::Binary(buffer[..len].to_vec().into())).await.is_err(){break;}
                    },
                    _=keepalive.tick()=>{if write.send(Message::Ping(Vec::new().into())).await.is_err(){break;}},
                    _=flush.tick()=>{if stopping.load(Ordering::Acquire){let _=write.close().await;break;}if write.flush().await.is_err(){break;}},
                }}
            };
            let shutdown=async{while !stopping.load(Ordering::Acquire){tokio::time::sleep(Duration::from_millis(100)).await;}};
            tokio::select!{_=incoming=>{},_=outgoing=>{},_=shutdown=>{}}
            stopping.store(true,Ordering::Release);
        });
    }).map_err(|e|e.to_string())?;
    if opened.recv_timeout(Duration::from_secs(5))!=Ok(true){stop.store(true,Ordering::Release);let _=thread.join();return Err("Persistent relay unavailable".into());}
    Ok(Tunnel{stop,threads:vec![thread],address})
}
