//! Host-owned render service. Guests receive bounded, transient preview packets.
use std::collections::HashMap;
use std::sync::{Arc,Mutex,Condvar,mpsc,atomic::{AtomicBool,Ordering}};
use std::time::{Duration,Instant};
use serde::{Serialize,Deserialize};
use base64::{Engine,engine::general_purpose::STANDARD};
use crate::{project::{Project,Media,MediaKind,TrackKind,Clip},playback::{self,PreviewEngine,ImageBuf}};

#[derive(Clone,Debug,Serialize,Deserialize,PartialEq)]
pub struct Request{#[serde(default)]pub view:u8,pub id:u64,pub revision:u64,pub source:Option<u64>,pub frame:i64,pub width:u32,pub height:u32,pub audio_frames:u32,pub playing:bool,pub speed:f64}
impl Request{
    pub fn validate(&self,project:&Project)->Result<(),String>{
        if self.view>3||(self.view==0)!=self.source.is_none()||self.view>=2&&(self.playing||self.audio_frames!=0)||!self.speed.is_finite()||self.speed.abs()>8.||self.playing&&self.speed==0.||self.width<2||self.height<2||self.width>1280||self.height>720||self.frame<0||self.frame>i32::MAX as i64{return Err("Invalid preview dimensions or time".into());}
        let rate=if let Some(id)=self.source{project.media(id).ok_or("Unknown source media")?.source_rate()}else{project.sequence.frame_rate};
        if self.audio_frames>rate.fps().ceil().min(240.)as u32{return Err("Preview audio interval is too large".into());}
        Ok(())
    }
}
#[derive(Clone,Serialize,Deserialize)]
pub struct Packet{pub request:Request,pub jpeg:String,pub audio:String,pub error:Option<String>}
impl Packet{
    pub fn decode(&self)->Result<(ImageBuf,Vec<i16>),String>{
        use image::ImageDecoder;
        if let Some(error)=&self.error{return Err(error.chars().take(512).collect());}
        if self.request.width<2||self.request.height<2||self.request.width>1280||self.request.height>720||self.jpeg.len()>1_400_000||self.audio.len()>256_000{return Err("Preview packet exceeds limits".into());}
        let bytes=STANDARD.decode(&self.jpeg).map_err(|_|"Invalid preview image")?;
        let decoder=image::codecs::jpeg::JpegDecoder::new(std::io::Cursor::new(bytes)).map_err(|_|"Invalid preview image")?;
        if decoder.dimensions()!=(self.request.width,self.request.height){return Err("Preview dimensions do not match request".into());}
        let rgb=image::DynamicImage::from_decoder(decoder).map_err(|_|"Could not decode preview")?.into_rgb8().into_raw();
        let audio=if let Some(encoded)=self.audio.strip_prefix("z:"){
            use std::io::Read;
            let compressed=STANDARD.decode(encoded).map_err(|_|"Invalid preview audio")?;
            let mut bytes=Vec::new();flate2::read::ZlibDecoder::new(compressed.as_slice()).take(192_001).read_to_end(&mut bytes).map_err(|_|"Invalid compressed preview audio")?;bytes
        }else{STANDARD.decode(&self.audio).map_err(|_|"Invalid preview audio")?};
        if audio.len()%4!=0||audio.len()>192_000{return Err("Invalid stereo preview audio".into());}
        Ok((ImageBuf{width:self.request.width,height:self.request.height,rgb},audio.chunks_exact(2).map(|v|i16::from_le_bytes([v[0],v[1]])).collect()))
    }
}
struct Job{project:Arc<Project>,request:Request}
struct Mailbox{job:Option<Job>,stop:bool}
pub struct Renderer{mailbox:Arc<(Mutex<Mailbox>,Condvar)>,stop:Arc<AtomicBool>,pub rx:mpsc::Receiver<Packet>,thread:Option<std::thread::JoinHandle<()>>}
impl Renderer{
    pub fn new()->Self{
        let mailbox=Arc::new((Mutex::new(Mailbox{job:None,stop:false}),Condvar::new()));let shared=mailbox.clone();
        let stop=Arc::new(AtomicBool::new(false));let cancel=stop.clone();let(tx,rx)=mpsc::sync_channel(2);
        let thread=std::thread::spawn(move||{
            let preview=PreviewEngine::new_cancellable(cancel.clone());let mut key=None;let mut audio_cache=HashMap::new();
            while !cancel.load(Ordering::Acquire){
                let job={let(mut guard,ready)=(shared.0.lock().unwrap(),&shared.1);while guard.job.is_none()&&!guard.stop{guard=ready.wait(guard).unwrap();}if guard.stop{break;}guard.job.take().unwrap()};
                let started=Instant::now();let request=job.request;let project=if let Some(id)=request.source{let Some(media)=job.project.media(id)else{continue;};Arc::new(source_project(media))}else{job.project.clone()};
                if key.as_ref().is_none_or(|(previous,revision,source):&(Arc<Project>,u64,Option<u64>)|!Arc::ptr_eq(previous,&job.project)||*revision!=request.revision||*source!=request.source){preview.set_project((*project).clone());audio_cache.clear();key=Some((job.project.clone(),request.revision,request.source));}
                let token=preview.request_scrub(request.frame,request.width,request.height);let deadline=Instant::now()+Duration::from_secs(15);
                let image=loop{
                    if cancel.load(Ordering::Acquire)||shared.0.lock().unwrap().job.is_some(){break None;}
                    if let Some(image)=preview.poll(){if image.token==token{break Some(image);}}
                    if Instant::now()>deadline{break None;}std::thread::sleep(Duration::from_millis(3));
                };
                let Some(image)=image else{continue;};let mut error=image.error;let mut jpeg=Vec::new();
                if error.is_none(){if let Err(e)=image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg,65).encode(&image.image.rgb,image.image.width,image.image.height,image::ExtendedColorType::Rgb8){error=Some(e.to_string());}}
                let mut sound=Vec::new();
                if error.is_none()&&request.audio_frames>0{
                    let mut buffers=playback::WindowBuffers::new();
                    for(id,block)in playback::required_audio_windows(&project,request.frame,request.audio_frames as i64){
                        let Some(media)=project.media(id)else{continue;};let cache_key=(media.path.clone(),block);
                        if !audio_cache.contains_key(&cache_key){
                            if audio_cache.len()>=16{audio_cache.clear();}
                            match playback::decode_audio_window(&media.path,block,&cancel){Ok(pcm)=>{audio_cache.insert(cache_key.clone(),Arc::new(playback::PcmWindow{start_sample:block*playback::PREVIEW_WINDOW_SAMPLES,pcm}));},Err(e)=>{error=Some(e);break;}}
                        }
                        if let Some(window)=audio_cache.get(&cache_key){buffers.entry(id).or_default().push(window.clone());}
                    }
                    if error.is_none(){for sample in playback::mix_window_range(&project,request.frame,request.audio_frames as i64,&buffers){sound.extend_from_slice(&((sample.clamp(-1.,1.)*32767.)as i16).to_le_bytes());}}
                }
                // Never disclose host filenames in remote decoder diagnostics.
                if error.is_some(){error=Some("The host could not render this preview. Check media availability on the host.".into());}
                let audio={use std::io::Write;let mut encoder=flate2::write::ZlibEncoder::new(Vec::new(),flate2::Compression::fast());let _=encoder.write_all(&sound);match encoder.finish(){Ok(bytes) if bytes.len()<sound.len()=>format!("z:{}",STANDARD.encode(bytes)),_=>STANDARD.encode(sound)}};
                let failed=error.is_some();let packet=Packet{request:request.clone(),jpeg:STANDARD.encode(jpeg),audio,error};let _=tx.try_send(packet);
                if request.playing&&!failed{
                    let step=(project.sequence.frame_rate.fps()/15.).ceil().max(1.)as i64;
                    let interval=Duration::from_secs_f64(step as f64/project.sequence.frame_rate.fps().max(1.));
                    while started.elapsed()<interval&&!cancel.load(Ordering::Acquire)&&shared.0.lock().unwrap().job.is_none(){std::thread::sleep(Duration::from_millis(2));}
                    let mut next=request.clone();next.frame=(request.frame+(step as f64*request.speed).round()as i64).max(0);
                    next.audio_frames=if (request.speed-1.).abs()<0.01{step as u32}else{0};
                    let mut guard=shared.0.lock().unwrap();if guard.job.is_none()&&next.frame<project.sequence.duration_frames().max(1){guard.job=Some(Job{project:job.project,request:next});}
                }
            }
        });Self{mailbox,stop,rx,thread:Some(thread)}
    }
    pub fn submit(&self,project:Arc<Project>,request:Request){let mut guard=self.mailbox.0.lock().unwrap();guard.job=Some(Job{project,request});self.mailbox.1.notify_one();}
}
impl Drop for Renderer{fn drop(&mut self){self.stop.store(true,Ordering::Release);self.mailbox.0.lock().unwrap().stop=true;self.mailbox.1.notify_one();if let Some(thread)=self.thread.take(){let _=thread.join();}}}

pub fn source_project(media:&Media)->Project{
    let mut project=Project::new_default("Source");project.sequence.width=media.width.max(16);project.sequence.height=media.height.max(16);project.sequence.frame_rate=media.source_rate();project.sequence.tracks.clear();
    let media_id=project.add_media(media.clone());
    for kind in [TrackKind::Video,TrackKind::Audio]{if !(if kind==TrackKind::Video{media.has_video||media.kind==MediaKind::Still}else{media.has_audio}){continue;}
        let track=project.add_track(kind);let id=project.alloc();project.sequence.tracks[track].clips.push(Clip{transform:Default::default(),id,media_id,name:media.name.clone(),timeline_start:0,duration:media.source_frames(),source_in:0,source_time_offset:0.,link:None,volume:1.,pos_x:0.,pos_y:0.,scale:1.,opacity:1.,grade:Default::default()});}
    project.ensure_integrity();project
}

/// Audio output has no media files or proxy cache on the guest.
pub struct Audio{tx:mpsc::SyncSender<(u64,Option<Vec<i16>>)>,generation:Arc<std::sync::atomic::AtomicU64>,thread:Option<std::thread::JoinHandle<()>>}
impl Audio{
    pub fn new()->Self{let(tx,rx)=mpsc::sync_channel::<(u64,Option<Vec<i16>>)>(4);let generation=Arc::new(std::sync::atomic::AtomicU64::new(0));let current=generation.clone();
        let thread=std::thread::spawn(move||{
            let output=rodio::OutputStream::try_default().ok();let sink=output.as_ref().and_then(|(_,handle)|rodio::Sink::try_new(handle).ok());let mut playing_generation=0;
            while let Ok((epoch,packet))=rx.recv(){if let Some(sink)=&sink{
                let active=current.load(Ordering::Acquire);if active!=playing_generation{sink.clear();playing_generation=active;}
                if epoch!=active{continue;}match packet{Some(samples)=>{if sink.len()<3{sink.append(rodio::buffer::SamplesBuffer::new(2,48000,samples));sink.play();}},None=>sink.clear()}
            }}
        });Self{tx,generation,thread:Some(thread)}
    }
    pub fn push(&self,samples:Vec<i16>){let _=self.tx.try_send((self.generation.load(Ordering::Acquire),Some(samples)));}
    pub fn clear(&self){let epoch=self.generation.fetch_add(1,Ordering::AcqRel)+1;let _=self.tx.try_send((epoch,None));}
}
impl Drop for Audio{fn drop(&mut self){self.generation.fetch_add(1,Ordering::AcqRel);let(tx,_)=mpsc::sync_channel(1);drop(std::mem::replace(&mut self.tx,tx));if let Some(thread)=self.thread.take(){let _=thread.join();}}}

#[derive(Clone,Debug,Serialize,Deserialize)]
pub struct WaveRequest{pub id:u64,pub revision:u64,pub clip:u64,pub offset:f64,pub seconds:f64,pub columns:u16}
#[derive(Clone,Serialize,Deserialize)]
pub struct WavePacket{pub request:WaveRequest,pub peaks:Vec<[f32;2]>}
#[derive(Serialize,Deserialize)]
pub enum Message{Picture(Packet),Waveform(WavePacket)}
struct WaveJob{peer:String,media:Media,request:WaveRequest}
pub struct Waveforms{engine:playback::timeline_waveforms::WaveformEngine,pending:Vec<WaveJob>,cache:std::collections::VecDeque<(Media,Arc<playback::timeline_waveforms::WaveformPeaks>)>,ready:Vec<(String,WavePacket)>}
impl Waveforms{
    pub fn new()->Self{Self{engine:playback::timeline_waveforms::WaveformEngine::new(),pending:Vec::new(),cache:Default::default(),ready:Vec::new()}}
    fn packet(request:WaveRequest,media:&Media,peaks:&playback::timeline_waveforms::WaveformPeaks)->WavePacket{
        let start=request.offset;let end=(start+request.seconds).min(media.source_duration_seconds());
        let mut merged=vec![[0f32;2];request.columns as usize];
        for channel in 0..peaks.channels{for(dst,source)in merged.iter_mut().zip(peaks.columns(channel,start,end,request.columns as usize)){dst[0]=dst[0].min(source.min);dst[1]=dst[1].max(source.max);}}
        WavePacket{request,peaks:merged}
    }
    pub fn submit(&mut self,peer:&str,project:&Project,mut request:WaveRequest)->Result<(),String>{
        if !request.offset.is_finite()||!request.seconds.is_finite()||request.offset<0.||request.seconds<=0.||request.columns==0||request.columns>1024{return Err("Invalid waveform interval".into());}
        let clip=project.clip(request.clip).ok_or("Unknown waveform clip")?;let rate=project.sequence.frame_rate;
        if request.offset+request.seconds>rate.frame_to_seconds(clip.duration)+0.1{return Err("Waveform interval exceeds clip".into());}
        let media=project.media(clip.media_id).ok_or("Unknown waveform media")?;if !media.has_audio{return Ok(());}
        // Convert a clip-relative interval to source time on the host.
        request.offset+=clip.source_start_seconds(rate);
        if let Some((_,peaks))=self.cache.iter().find(|(old,_)|old.id==media.id&&old.path==media.path&&old.duration_secs==media.duration_secs){self.ready.push((peer.into(),Self::packet(request,media,peaks)));return Ok(());}
        self.pending.retain(|job|job.peer!=peer||job.request.clip!=request.clip);
        if self.pending.len()>=112{return Err("Waveform request queue is full".into());}
        self.engine.request(media.clone());self.pending.push(WaveJob{peer:peer.into(),media:media.clone(),request});Ok(())
    }
    pub fn poll(&mut self)->Vec<(String,WavePacket)>{
        while let Some(result)=self.engine.poll(){
            let jobs:Vec<_>=self.pending.iter().filter(|job|job.media.id==result.media.id&&job.media.path==result.media.path).map(|job|(job.peer.clone(),job.request.clone(),job.media.clone())).collect();
            self.pending.retain(|job|job.media.id!=result.media.id||job.media.path!=result.media.path);
            if let Ok(peaks)=result.peaks{
                for(peer,request,media)in jobs{self.ready.push((peer,Self::packet(request,&media,&peaks)));}
                self.cache.retain(|(media,_)|media.id!=result.media.id);
                while self.cache.len()>=16||self.cache.iter().map(|(_,p)|p.bytes()).sum::<usize>()+peaks.bytes()>8_000_000{if self.cache.pop_back().is_none(){break;}}
                self.cache.push_front((result.media,peaks));
            }
        }
        std::mem::take(&mut self.ready)
    }
    pub fn cancel_peer(&mut self,peer:&str){self.pending.retain(|job|job.peer!=peer);self.ready.retain(|(id,_)|id!=peer);}
}
