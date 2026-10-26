//! UI/session boundary: local gestures stay optimistic, host commits reconcile
//! by property, and rejected edits retain a saveable local branch.
use super::*;
use crate::collaboration::{OperationId,document::{self,Document,Patch,Member},session::{Session,Event,Participant}};

struct Bridge{
    epoch:String,actor:String,member:Member,revision:u64,sequence:u64,document:Document,last:Document,
    pending:Vec<Patch>,local_branch:Project,connected:bool,host:bool,undo:Vec<OperationId>,redo:usize,history_pending:bool,
}
#[derive(Default)]
enum Phase{#[default]Idle,Starting,Joining,Hosting,Connected,Reconnecting,Ended}
struct WaveView{key:(u64,u64,u64,u64,u16,u64,bool),id:u64,at:Instant,peaks:Vec<[f32;2]>}
struct RemoteView{request:crate::collaboration::media::Request,last_frame:i64,at:Instant}
#[derive(Default)]
pub(super) struct State{
    phase:Phase,error:String,inviting:bool,transfers:Vec<crate::collaboration::TransferProgress>,
    remote_document:Option<String>,export_requested:bool,export_sequence:Option<String>,filmstrip_request:Option<(u64,playback::filmstrips::FilmstripRequest,Instant)>,waveforms:std::collections::BTreeMap<u64,WaveView>,thumbnail_request:Option<(u64,Instant)>,upload_active:bool,deferred:VecDeque<Event>,remote_media:bool,remote_views:std::collections::BTreeMap<bool,RemoteView>,remote_id:u64,remote_audio:Option<crate::collaboration::media::Audio>,sources:Vec<(u64,PathBuf)>,
    pub(super) service:Option<Session>,bridge:Option<Bridge>,participants:Vec<Participant>,status:String,code:String,
    issues:Vec<String>,branches:Vec<Project>,start_gen:u64,observed_gen:u64,changed_at:Option<Instant>,leave_at:Option<Instant>,
}
pub(super) fn request_export(state:&Rc<std::cell::RefCell<AppState>>)->bool{
    if !guest(&state.borrow()){return false;}
    let mut current=state.borrow_mut();current.status=if readonly(&current){"Only connected editors can request a host export.".into()}else{match current.collaboration.service.as_ref().ok_or("Host disconnected".to_string()).and_then(|service|service.request_export(current.editor.project.active_sequence.clone())){Ok(())=>"Export requested. The host chooses the destination and renders the file.".into(),Err(error)=>error}};true
}
pub(super) fn guest(state:&AppState)->bool{
    state.collaboration.bridge.as_ref().map(|bridge|!bridge.host&&bridge.document.get("document/document_id").and_then(|v|v.as_str())==Some(state.editor.project.document_id.as_str()))
        .unwrap_or_else(||state.collaboration.remote_document.as_deref()==Some(state.editor.project.document_id.as_str()))
}
pub(super) fn remote_preview(ui:&AppWindow,state:&Rc<std::cell::RefCell<AppState>>,source:bool)->bool{
    if !guest(&state.borrow()){return false;}
    let mut current=state.borrow_mut();
    if !current.collaboration.remote_media{return true;}
    if source{
        let selected=current.editor.selected_media.and_then(|id|current.editor.project.media(id)).cloned();
        let Some(media)=selected else{ui.set_show_source_picture(false);return true;};
        if current.source.media_id!=Some(media.id){current.source.select(Some(media.id),media.source_rate(),media.source_frames());current.source_usable=true;current.source_feedback.clear();sync_source_controls(ui,&current);}
    }
    let (revision,connected)=current.collaboration.bridge.as_ref().map(|b|(b.revision,b.connected)).unwrap_or_default();
    if !connected{current.playing=false;current.source.stop();if let Some(audio)=&current.collaboration.remote_audio{audio.clear();}return true;}
    let media_id=if source{current.editor.selected_media}else{None};
    let frame=if source{current.source.frame}else{current.editor.project.sequence.playhead};
    let playing=if source{current.source.playing}else{current.playing};let speed=if source{current.source.speed}else{current.speed};
    let (seq_w,seq_h,rate)=if let Some(id)=media_id{let Some(media)=current.editor.project.media(id)else{return true;};(media.width,media.height,media.source_rate())}else{let seq=&current.editor.project.sequence;(seq.width,seq.height,seq.frame_rate)};
    let (w,h)=preview_canvas(seq_w.max(2),seq_h.max(2),640,360);let(w,h)=(w.max(2).min(1280),h.max(2).min(720));
    let seeking=if source{current.source.scrub_origin.is_some()}else{current.pointer.down&&current.pointer.seeking};
    let changed=current.collaboration.remote_views.get(&source).is_none_or(|old|old.request.sequence!=current.editor.project.active_sequence||old.request.source!=media_id||old.request.revision!=revision||old.request.width!=w||old.request.height!=h||old.request.playing!=playing||old.request.speed!=speed||(!playing||seeking)&&old.request.frame!=frame||old.at.elapsed()>std::time::Duration::from_secs(3));
    if changed{
        current.collaboration.remote_id=current.collaboration.remote_id.wrapping_add(1);
        let request=crate::collaboration::media::Request{sequence:current.editor.project.active_sequence.clone(),view:if source{1}else{0},id:current.collaboration.remote_id,revision,source:media_id,frame:frame.max(0),width:w,height:h,audio_frames:if playing&&(speed-1.).abs()<0.01{(rate.fps()/15.).ceil().max(1.)as u32}else{0},playing,speed};
        let result=current.collaboration.service.as_ref().ok_or("Host disconnected".to_string()).and_then(|service|service.preview(request.clone()));
        if let Err(error)=result{current.collaboration.status=error;}else{
            current.collaboration.remote_views.insert(source,RemoteView{request,last_frame:-1,at:Instant::now()});
            if !playing{if let Some(audio)=&current.collaboration.remote_audio{audio.clear();}}
            if source{current.source_request_pending=true;current.source_feedback.request(Instant::now());}else{current.program_request_pending=true;current.program_feedback.request(Instant::now());}
        }
    }
    sync_viewer_status(ui,&current);true
}
pub(super) fn remote_filmstrips(ui:&AppWindow,state:&Rc<std::cell::RefCell<AppState>>)->bool{
    if !guest(&state.borrow()){return false;}let mut current=state.borrow_mut();if !current.collaboration.remote_media{return true;}
    if current.collaboration.filmstrip_request.as_ref().is_some_and(|(_,_,at)|at.elapsed()<std::time::Duration::from_secs(5)){return true;}
    if let Some(picture)=filmstrips::remote_next(ui,&mut current){
        current.collaboration.remote_id=current.collaboration.remote_id.wrapping_add(1);let id=current.collaboration.remote_id;let revision=current.collaboration.bridge.as_ref().map_or(0,|b|b.revision);
        let request=crate::collaboration::media::Request{sequence:current.editor.project.active_sequence.clone(),view:3,id,revision,source:Some(picture.media.id),frame:picture.ordinal,width:picture.width,height:picture.height,audio_frames:0,playing:false,speed:0.};
        if let Some(service)=&current.collaboration.service{if service.preview(request).is_ok(){current.collaboration.filmstrip_request=Some((id,picture,Instant::now()));}}
    }true
}
pub(super) fn remote_waveforms(ui:&AppWindow,state:&Rc<std::cell::RefCell<AppState>>)->bool{
    if !guest(&state.borrow()){return false;}
    let mut current=state.borrow_mut();if !current.collaboration.remote_media{return true;}
    let revision=current.collaboration.bridge.as_ref().map_or(0,|b|b.revision);let seq=&current.editor.project.sequence;
    let mut visible=Vec::new();
    for row in rows_for(&current){if !waveform_row_visible(row.y,row.h,ui.get_timeline_scroll_y(),current.track_body){continue;}
        let track=&seq.tracks[row.track_index];if track.kind!=TrackKind::Audio{continue;}
        for clip in &track.clips{let x=(clip.timeline_start as f64-seq.scroll)as f32*seq.zoom as f32;let width=clip.duration as f32*seq.zoom as f32;
            let left=x.max(0.);let right=(x+width).min(current.timeline_w);let width=right-left;if width<1.||!width.is_finite(){continue;}
            let scale=1./(seq.zoom*seq.frame_rate.fps());let offset=(left-x)as f64*scale;let seconds=width as f64*scale;let count=(width.ceil()as u16).clamp(1,1024);
            visible.push((clip.id,(clip.media_id,clip.source_start_seconds(seq.frame_rate).to_bits(),offset.to_bits(),seconds.to_bits(),count,clip.creative.speed.to_bits(),clip.creative.freeze),offset,seconds));if visible.len()>=16{break;}
        }if visible.len()>=16{break;}
    }
    current.collaboration.waveforms.retain(|id,_|visible.iter().any(|(clip,_,_,_)|clip==id));
    for(clip,key,offset,seconds)in visible{
        let needs=current.collaboration.waveforms.get(&clip).is_none_or(|view|view.key!=key||view.peaks.is_empty()&&view.at.elapsed()>std::time::Duration::from_secs(5));
        if !needs{continue;}current.collaboration.remote_id=current.collaboration.remote_id.wrapping_add(1);let id=current.collaboration.remote_id;
        let request=crate::collaboration::media::WaveRequest{id,revision,clip,offset,seconds,columns:key.4};
        if let Some(service)=&current.collaboration.service{if service.waveform(request).is_ok(){current.collaboration.waveforms.insert(clip,WaveView{key,id,at:Instant::now(),peaks:Vec::new()});}}
    }true
}
pub(super) fn waveform_picture(state:&AppState,clip:&Clip,x:f32,width:f32,height:f32)->(slint::Image,f32,f32){
    let empty=||(slint::Image::default(),0.,0.);let Some(view)=state.collaboration.waveforms.get(&clip.id)else{return empty();};if view.peaks.is_empty(){return empty();}
    let left=x.max(0.);let visible=(x+width).min(state.timeline_w)-left;if visible<1.||height<24.{return empty();}
    let count=view.peaks.len();let rows=((height-24.).floor()as u32).clamp(4,64);let mut buffer=SharedPixelBuffer::<slint::Rgba8Pixel>::new(count as u32,rows);let pixels=buffer.make_mut_slice();
    for(column,peak)in view.peaks.iter().enumerate(){let center=(rows-1)as f32/2.;let top=(center-peak[1]*center).floor().max(0.)as usize;let bottom=(center-peak[0]*center).ceil().min((rows-1)as f32)as usize;for row in top..=bottom{pixels[row*count+column]=slint::Rgba8Pixel{r:207,g:236,b:214,a:190};}}
    (slint::Image::from_rgba8(buffer),left-x,visible)
}
pub(super) fn remote_thumbnails(ui:&AppWindow,state:&Rc<std::cell::RefCell<AppState>>)->bool{
    if !guest(&state.borrow()){return false;}
    let mut current=state.borrow_mut();if !current.collaboration.remote_media{return true;}
    if current.collaboration.thumbnail_request.is_some_and(|(_,at)|at.elapsed()<std::time::Duration::from_secs(5)){return true;}
    let candidate=current.editor.project.media.iter().filter(|m|m.has_video||m.kind==MediaKind::Still).filter(|m|current.current_bin.is_empty()||m.bin==current.current_bin).filter(|m|current.query.is_empty()||m.name.to_lowercase().contains(&current.query)).take(128).find(|m|!current.thumbnail_cache.iter().any(|(cached,_)|same_picture(cached,m))).cloned();
    if let Some(media)=candidate{
        current.collaboration.remote_id=current.collaboration.remote_id.wrapping_add(1);let id=current.collaboration.remote_id;
        let revision=current.collaboration.bridge.as_ref().map_or(0,|b|b.revision);let(w,h)=preview_canvas(media.width.max(2),media.height.max(2),160,90);
        let request=crate::collaboration::media::Request{sequence:current.editor.project.active_sequence.clone(),view:2,id,revision,source:Some(media.id),frame:0,width:w.max(2),height:h.max(2),audio_frames:0,playing:false,speed:0.};
        if let Some(service)=&current.collaboration.service{if service.preview(request).is_ok(){current.collaboration.thumbnail_request=Some((id,Instant::now()));}}
    }
    let _=ui;true
}
pub(super) fn active(state:&AppState)->bool{state.collaboration.service.is_some()}
pub(super) fn readonly(state:&AppState)->bool{active(state)&&(state.collaboration.leave_at.is_some()||state.collaboration.bridge.as_ref().is_none_or(|b|!b.connected||!b.member.editor))}
pub(super) fn can_undo(state:&AppState)->bool{state.collaboration.bridge.as_ref().is_some_and(|b|b.connected&&b.member.editor&&!b.history_pending&&(!b.undo.is_empty()||!b.pending.is_empty()||state.editor.can_undo()))}
pub(super) fn can_redo(state:&AppState)->bool{state.collaboration.bridge.as_ref().is_some_and(|b|b.connected&&b.member.editor&&!b.history_pending&&b.redo>0)}
pub(super) fn sync(ui:&AppWindow,state:&AppState){
    let current=&state.collaboration;if !active(state){ui.set_session_confirm_end(false);}
    ui.set_session_phase(match current.phase{Phase::Idle=>"",Phase::Starting=>"Starting…",Phase::Joining=>"Connecting…",Phase::Hosting=>"Hosting",Phase::Connected=>"Connected",Phase::Reconnecting=>"Reconnecting…",Phase::Ended=>"Session ended"}.into());
    ui.set_session_error(current.error.clone().into());ui.set_session_inviting(current.inviting);
    ui.set_session_viewer(current.bridge.as_ref().is_some_and(|b|b.connected&&!b.member.editor));
    ui.set_session_transfers(model(current.transfers.iter().map(|item|TransferItem{id:item.id.clone().into(),name:item.name.clone().into(),percent:if item.total==0{0}else{(item.done.saturating_mul(100)/item.total).min(100)as i32},detail:format!("{:.1} / {:.1} MB",item.done as f64/1_048_576.,item.total as f64/1_048_576.).into(),state:match item.stage{crate::collaboration::TransferStage::Queued=>"Waiting for host",crate::collaboration::TransferStage::Uploading=>"Uploading",crate::collaboration::TransferStage::Receiving=>"Receiving",crate::collaboration::TransferStage::Finishing=>"Checking upload"}.into(),cancellable:item.cancellable}).collect()));
    let current=&state.collaboration;ui.set_session_export_requested(current.export_requested);ui.set_session_upload_active(current.upload_active);ui.set_session_connected(current.bridge.as_ref().is_some_and(|b|b.connected));ui.set_session_active(current.service.is_some());ui.set_session_readonly(readonly(state));
    ui.set_session_auto_connect(if current.code.is_empty(){!state.preferences.relay.service_url.is_empty()||state.preferences.relay.uses_public()}else{current.code.starts_with("quaternio-join-v1:")});ui.set_session_simple_code(current.code.starts_with("quaternio-join-v1:"));
    ui.set_session_host(current.bridge.as_ref().is_some_and(|b|b.host));ui.set_session_status(current.status.clone().into());
    ui.set_session_code(current.code.clone().into());ui.set_session_issues(current.issues.iter().rev().take(16).map(|text|text.chars().take(2048).collect::<String>()).collect::<Vec<_>>().join("\n\n").into());ui.set_session_has_branch(!current.branches.is_empty());
    ui.set_session_peers(model(current.participants.iter().map(|p|PeerItem{id:p.id.clone().into(),name:p.name.clone().into(),role:if p.editor{"Editor"}else{"Viewer"}.into(),state:if p.state=="Connected"&&!p.route.is_empty(){format!("{} · {}",p.state,p.route)}else{p.state.clone()}.into(),revocable:current.bridge.as_ref().is_some_and(|b|b.host&&b.actor!=p.id)&&p.state!="Revoked"}).collect()));
    if active(state){ui.set_can_undo(can_undo(state));ui.set_can_redo(can_redo(state));}
    ui.set_session_revision(current.bridge.as_ref().map(|b|format!("Revision {} · {} pending",b.revision,b.pending.len())).unwrap_or_default().into());
}
fn record_issue(state:&mut AppState,reason:String){
    let preserve=state.collaboration.bridge.as_ref().is_some_and(|b|!b.pending.is_empty()||b.history_pending||document::capture(&state.editor.project)!=b.last);
    let branch=state.collaboration.bridge.as_ref().filter(|b|document::capture(&state.editor.project)==b.last).map(|b|b.local_branch.clone()).unwrap_or_else(||state.editor.project.clone());
    let c=&mut state.collaboration;
    if preserve{if c.issues.len()>=64{c.issues.remove(0);}c.issues.push(reason);c.error.clear();if c.branches.len()>=8{c.branches.remove(0);}c.branches.push(branch);}else{c.error=reason;}
}
fn flush(state:&mut AppState)->Result<bool,String>{
    let c=&mut state.collaboration;let Some(b)=c.bridge.as_mut()else{return Ok(false);};
    if b.host{let paths:Vec<_>=state.editor.project.media.iter().map(|m|(m.id,m.path.clone())).collect();if paths!=c.sources{c.service.as_ref().ok_or("Session stopped")?.update_sources(state.editor.project.clone())?;c.sources=paths;}}
    let local=document::capture(&state.editor.project);if local==b.last{return Ok(false);}
    if state.editor.project.document_id!=b.document.get("document/document_id").and_then(|v|v.as_str()).unwrap_or(""){
        return Err("Project changed. The previous live session has ended.".into());
    }
    if !b.connected{return Ok(false);}
    if !b.member.editor{
        let mut project=document::materialize(&b.document)?;project.next_id=state.editor.project.next_id;
        state.editor.apply_shared_project(project);b.last=document::capture(&state.editor.project);
        c.status="Viewer mode: document edits are read-only; transport and selection stay local.".into();return Ok(true);
    }
    if b.pending.len()>=16{return Err("The host is not acknowledging edits. Local work is retained; leave and reconnect.".into());}
    if state.editor.project.next_id>=b.member.limit{return Err("Participant object-ID range is exhausted. Save a branch and start a fresh session.".into());}
    let id=OperationId{author:b.actor.clone(),sequence:b.sequence};b.sequence=b.sequence.checked_add(1).ok_or("Operation sequence exhausted")?;
    let patch=Patch::between(b.epoch.clone(),id,b.revision,&b.last,&local);
    c.service.as_ref().ok_or("Session stopped")?.propose(patch.clone(),local.clone())?;
    b.pending.push(patch);b.last=local;b.local_branch=state.editor.project.clone();c.status="Publishing local edit…".into();Ok(true)
}
fn apply_projection(state:&mut AppState)->Result<(),String>{
    let c=&mut state.collaboration;let b=c.bridge.as_mut().ok_or("Session is not ready")?;
    let mut projected=b.document.clone();
    for patch in &b.pending{for edit in &patch.edits{match &edit.after{Some(v)=>{projected.insert(edit.key.clone(),v.clone());},None=>{projected.remove(&edit.key);}}}}
    let mut project=match document::materialize(&projected){Ok(p)=>p,Err(_)=>{
        if c.branches.len()>=8{c.branches.remove(0);}c.branches.push(b.local_branch.clone());
        c.status="A pending edit conflicts with a remote update. Its local branch is retained.".into();
        document::materialize(&b.document)?
    }};
    let cursor=state.editor.project.next_id.max(b.member.first);project.next_id=cursor;
    state.editor.apply_shared_project(project);
    b.last=document::capture(&state.editor.project);if b.pending.is_empty(){b.local_branch=state.editor.project.clone();}note_borrowed(state);
    if state.playing&&(state.speed-1.0).abs()<0.01{state.audio.update_mix(state.editor.project.clone());}
    Ok(())
}
fn open_connection(ui:&AppWindow,state:&Rc<std::cell::RefCell<AppState>>,path:&std::path::Path,command:&str)->Result<(),String>{
    use std::io::Read;
    let mut text=String::new();
    std::fs::File::open(path).map_err(|e|e.to_string())?.take(512*1024+1).read_to_string(&mut text).map_err(|e|e.to_string())?;
    if text.len()>512*1024{return Err("Connection file exceeds 512 KiB".into());}
    ui.set_session_input(text.into());
    if command!="session-load-code"{handle(ui,state,if command=="session-open-reply"{"session-accept"}else{"session-join"});}
    Ok(())
}
pub(super) fn handle(ui:&AppWindow,state:&Rc<std::cell::RefCell<AppState>>,command:&str)->bool{
    if command=="session"{let relay=state.borrow().preferences.relay.clone();ui.set_session_use_public(relay.use_public);ui.set_session_service_url(relay.service_url.into());ui.set_session_service_key(relay.access_key.into());ui.set_session_relay_url(relay.url.into());ui.set_session_relay_user(relay.username.into());ui.set_session_relay_password(relay.password.into());ui.set_session_relay_only(relay.relay_only);ui.set_show_session(!ui.get_show_session());ui.set_show_review(false);ui.set_show_activity(false);if ui.get_session_name().is_empty(){ui.set_session_name(state.borrow().preferences.reviewer_name.clone().into());}sync(ui,&state.borrow());return true;}
    if active(&state.borrow())&&matches!(command,"undo"|"redo"){
        if state.borrow().pointer.down{let mut s=state.borrow_mut();s.editor.cancel_gesture();s.pointer.finish();note_borrowed(&mut s);drop(s);full_sync(ui,state);return true;}
        let result=(||{
            let mut s=state.borrow_mut();flush(&mut s)?;
            let c=&mut s.collaboration;let b=c.bridge.as_mut().ok_or("Session is connecting")?;
            if !b.connected||!b.member.editor{return Err("An active editor connection is required".into());}
            let id=OperationId{author:b.actor.clone(),sequence:b.sequence};b.sequence=b.sequence.checked_add(1).ok_or("Operation sequence exhausted")?;b.history_pending=true;
            if command=="undo"{c.service.as_ref().unwrap().undo(id)}else{c.service.as_ref().unwrap().redo(id)}
        })();if let Err(e)=result{state.borrow_mut().collaboration.status=e;}sync(ui,&state.borrow());return true;
    }
    if active(&state.borrow())&&matches!(command,"render"|"add-to-queue"|"render-queue"){
        let result={let mut s=state.borrow_mut();if s.pointer.down{Err("Finish the current drag before exporting".into())}else{flush(&mut s).map(|_|())}};
        if let Err(error)=result{set_status(state,error);return true;}
        if state.borrow().collaboration.bridge.as_ref().is_none_or(|b|!b.pending.is_empty()||!b.connected||b.history_pending){
            set_status(state,"Wait for the host to acknowledge edits, or leave the session and export your local branch.".into());return true;
        }
    }
    if !command.starts_with("session-"){return false;}
    let result:Result<(),String>=(||{match command{
        "session-relay-save"=>{
            if active(&state.borrow()){return Err("Leave the session before changing connection settings".into());}
            let relay=crate::collaboration::peer::RelaySettings{use_public:ui.get_session_use_public(),service_url:ui.get_session_service_url().trim().trim_end_matches('/').into(),access_key:ui.get_session_service_key().to_string(),url:ui.get_session_relay_url().trim().into(),username:ui.get_session_relay_user().to_string(),password:ui.get_session_relay_password().to_string(),relay_only:ui.get_session_relay_only(),..Default::default()};
            relay.validate()?;
            let mut s=state.borrow_mut();let mut preferences=s.preferences.clone();preferences.relay=relay;preferences.save()?;s.preferences=preferences;
            s.collaboration.status="Connection settings saved. Public service is optional; private keys and passwords stay in memory until the app closes.".into();
        },
        "session-host"=>{
            if active(&state.borrow()){return Err("Leave the current session first".into());}
            let s=state.borrow();if s.pointer.down||s.import_rx.is_some(){return Err("Finish the current drag or import before hosting".into());}
            let service=Session::host_with_relay(s.editor.project.clone(),ui.get_session_name().to_string(),s.preferences.relay.clone())?;let generation=s.edit_gen;drop(s);
            state.borrow_mut().collaboration=State{phase:Phase::Starting,service:Some(service),status:"Starting your session…".into(),start_gen:generation,observed_gen:generation,..Default::default()};
        },
        "session-join"=>{
            if active(&state.borrow()){return Err("Leave the current session first".into());}
            if !confirm_unsaved(ui,state){return Ok(());}
            let service=Session::join_with_relay(ui.get_session_input().as_str(),state.borrow().preferences.relay.clone())?;let generation=state.borrow().edit_gen;
            state.borrow_mut().collaboration=State{phase:Phase::Joining,service:Some(service),status:"Connecting…".into(),start_gen:generation,observed_gen:generation,..Default::default()};
        },
        "session-export"=>{{let mut s=state.borrow_mut();s.collaboration.export_requested=false;if let Some(id)=s.collaboration.export_sequence.take(){s.editor.project.switch_sequence(&id)?;}}note(state);full_sync(ui,state);on_command(ui,state,"export-view");},
        "session-upload"=>{if readonly(&state.borrow()){return Err("Only connected editors can upload media".into());}import_media(ui,state);},
        "session-cancel-uploads"=>state.borrow().collaboration.service.as_ref().ok_or("No active session")?.cancel_upload(String::new())?,
        "session-dismiss-error"=>state.borrow_mut().collaboration.error.clear(),
        "session-invite"=>{let mut s=state.borrow_mut();if s.collaboration.inviting{return Ok(());}
            let name=ui.get_session_invitee().trim().to_string();let name=if name.is_empty(){format!("Guest {}",s.collaboration.participants.iter().filter(|p|p.state!="Revoked").count().max(1))}else{name};
            s.collaboration.service.as_ref().ok_or("Host a session first")?.invite(name,ui.get_session_editor_role())?;s.collaboration.inviting=true;s.collaboration.error.clear();
        },
        "session-accept"=>state.borrow().collaboration.service.as_ref().ok_or("Host a session first")?.signal(ui.get_session_input().to_string())?,
        "session-load-code"|"session-open-invitation"|"session-open-reply"=>{
            if let Some(path)=rfd::FileDialog::new().set_title("Open Quaternio connection file").add_filter("Connection",&["qpeer"]).pick_file(){
                open_connection(ui,state,&path,command)?;
            }
        },
        "session-save-code"=>{if let Some(path)=rfd::FileDialog::new().set_title("Save Quaternio connection file").add_filter("Connection",&["qpeer"]).set_file_name(if ui.get_session_host(){"invitation.qpeer"}else{"reply.qpeer"}).save_file(){crate::project::write_atomic(&path,ui.get_session_code().as_bytes())?;}},
        "session-recover-host"=>{
            if active(&state.borrow()){return Err("Leave the current session before recovering a host log".into());}
            if !confirm_unsaved(ui,state){return Ok(());}
            if let Some(path)=rfd::FileDialog::new().set_title("Recover acknowledged hosted edits").set_directory(crate::preferences::data_dir().join("sessions")).add_filter("Host session log",&["qsession"]).pick_file(){
                let (_audit,authority,rejected)=crate::collaboration::audit::Audit::recover(&path)?;
                let project=document::materialize(&authority.document)?;let mut s=state.borrow_mut();s.editor.path=None;s.editor.apply_shared_project(project);note_borrowed(&mut s);
                s.collaboration.status=format!("Recovered acknowledged revision {} as a local project. {} rejected operations remain in the original log.",authority.revision,rejected.len());
            }
        },
        "session-recover-branch"=>{
            if active(&state.borrow()){return Err("Leave the current session before recovering a branch".into());}
            if !confirm_unsaved(ui,state){return Ok(());}
            if let Some(path)=rfd::FileDialog::new().set_title("Recover a retained client branch").set_directory(crate::preferences::data_dir().join("sessions")).add_filter("Client outbox",&["qoutbox"]).pick_file(){
                let project=crate::collaboration::outbox::recover_branch(&path)?;
                let mut s=state.borrow_mut();s.editor.path=None;s.editor.apply_shared_project(project);note_borrowed(&mut s);s.collaboration.status="Recovered a local branch. Save it as a separate project.".into();
            }
        },
        "session-save-branch"=>{
            let branch=state.borrow().collaboration.branches.last().cloned().ok_or("No rejected branch to save")?;
            if let Some(path)=rfd::FileDialog::new().set_title("Save retained local branch").add_filter("Quaternio",&["quaternio"]).set_file_name("local-branch.quaternio").save_file(){branch.save_to(&path)?;}
        },
        "session-leave"|"session-end-confirmed"=>{
            let needs_confirmation={let s=state.borrow();s.collaboration.bridge.as_ref().is_some_and(|b|b.host)&&s.collaboration.participants.iter().any(|p|p.state=="Connected"&&p.id!=s.collaboration.bridge.as_ref().unwrap().actor)};
            if needs_confirmation&&command!="session-end-confirmed"{ui.set_session_confirm_end(true);return Ok(());}
            ui.set_session_confirm_end(false);
            let mut s=state.borrow_mut();
            if s.pointer.down||s.import_rx.is_some(){return Err("Finish the current drag or import before leaving".into());}
            if s.collaboration.leave_at.is_some(){finish_leave(&mut s);}else{
                flush(&mut s)?;
                if s.collaboration.bridge.as_ref().is_some_and(|b|b.connected&&(!b.pending.is_empty()||b.history_pending)){
                    s.collaboration.leave_at=Some(Instant::now());s.collaboration.status="Waiting for the final edit acknowledgement. Choose Leave again to retain the local branch and disconnect immediately.".into();
                }else{finish_leave(&mut s);}
            }
        },
        _=>{if let Some(id)=command.strip_prefix("session-cancel-upload:"){state.borrow().collaboration.service.as_ref().ok_or("No active session")?.cancel_upload(id.into())?;}else if let Some(id)=command.strip_prefix("session-revoke:"){state.borrow().collaboration.service.as_ref().ok_or("No active session")?.revoke(id.into())?;}else{return Err("Unknown session action".into());}},
    }Ok(())})();
    if let Err(error)=result{let mut s=state.borrow_mut();s.collaboration.error=error;s.collaboration.inviting=false;}sync(ui,&state.borrow());true
}
fn finish_leave(state:&mut AppState){
    if state.collaboration.bridge.as_ref().is_some_and(|b|!b.pending.is_empty()||b.history_pending){record_issue(state,"The session ended with pending edits. A local branch has been retained.".into());}
    state.playing=false;state.source.stop();state.audio.stop();
    let c=&mut state.collaboration;c.remote_audio=None;c.remote_media=false;c.remote_views.clear();c.filmstrip_request=None;c.thumbnail_request=None;c.waveforms.clear();c.deferred.clear();c.sources.clear();c.service=None;c.bridge=None;c.participants.clear();c.phase=Phase::Ended;c.inviting=false;c.transfers.clear();c.code.clear();c.leave_at=None;c.status="Session ended. Your document and retained branches remain available locally.".into();
    state.editor.clear_shared_history();state.editor.project.ensure_integrity();
}
pub(super) fn pump(ui:&AppWindow,state:&Rc<std::cell::RefCell<AppState>>){
    if !active(&state.borrow()){return;}
    let busy=state.borrow().pointer.down||state.borrow().import_rx.is_some();
    let mut changed=false;
    {
        let mut s=state.borrow_mut();let generation=s.edit_gen;
        if s.collaboration.observed_gen!=generation{s.collaboration.observed_gen=generation;s.collaboration.changed_at=Some(Instant::now());}
        if !busy&&s.collaboration.changed_at.is_some_and(|at|at.elapsed()>std::time::Duration::from_millis(120)){
            match flush(&mut s){Ok(flushed)=>{changed|=flushed;s.collaboration.changed_at=None;},Err(error)=>{record_issue(&mut s,error);if let Some(b)=&mut s.collaboration.bridge{b.connected=false;}changed=true;}}
        }
    }
    let mut events:Vec<_>={let mut s=state.borrow_mut();let mut events=if busy{Vec::new()}else{s.collaboration.deferred.drain(..).collect()};
        for _ in 0..64{match s.collaboration.service.as_ref().unwrap().rx.try_recv(){Ok(event)=>events.push(event),Err(mpsc::TryRecvError::Disconnected)=>{events.push(Event::Ended("Session service stopped. Your local document and retained outbox remain available.".into()));break;},Err(_)=>break}}
        events
    };
    if busy{
        let mut immediate=Vec::new();let mut s=state.borrow_mut();
        for event in events{if matches!(&event,Event::Preview(_)|Event::Waveform(_)|Event::Status(_)|Event::MediaAvailable(_)|Event::UploadState(_)|Event::Transfers(_)|Event::OperationFailed(_)){immediate.push(event);}else if s.collaboration.deferred.len()<256{s.collaboration.deferred.push_back(event);}else{record_issue(&mut s,"Too many remote changes during this gesture. Release the pointer to catch up.".into());}}
        events=immediate;
    }
    if !busy&&!events.is_empty(){
        // Flush a just-finished local gesture before applying remote commits.
        let mut s=state.borrow_mut();if let Err(error)=flush(&mut s){record_issue(&mut s,error);}drop(s);
    }
    let mut uploaded=Vec::new();
    for event in events{changed|=!matches!(&event,Event::Preview(_)|Event::Waveform(_));let mut s=state.borrow_mut();match event{
        Event::ExportRequested(name,sequence)=>{let label=s.editor.project.sequence_by_id(&sequence).map(|s|s.name.clone()).unwrap_or(sequence.clone());s.collaboration.export_requested=true;s.collaboration.export_sequence=Some(sequence);s.collaboration.status=format!("{name} requested an export of {label}. Choose Export on the host to review settings and destination.");},
        Event::MediaChanged=>{s.collaboration.remote_views.clear();s.collaboration.waveforms.clear();s.collaboration.filmstrip_request=None;s.collaboration.thumbnail_request=None;s.collaboration.filmstrip_request=None;s.thumbnail_cache.clear();s.filmstrips.reset();if let Some(audio)=&s.collaboration.remote_audio{audio.clear();}},
        Event::Waveform(packet)=>{if let Some(view)=s.collaboration.waveforms.get_mut(&packet.request.clip){if view.id==packet.request.id{view.peaks=packet.peaks;ui.set_clips(model(clip_views(&s)));}}},
        Event::OperationFailed(reason)=>{s.collaboration.error=reason;s.collaboration.inviting=false;},
        Event::Transfers(items)=>s.collaboration.transfers=items,
        Event::UploadState(active)=>s.collaboration.upload_active=active,
        Event::Reconnecting=>{s.collaboration.phase=Phase::Reconnecting;if let Some(bridge)=&mut s.collaboration.bridge{bridge.connected=false;}s.playing=false;s.source.stop();s.audio.stop();if let Some(audio)=&s.collaboration.remote_audio{audio.clear();}s.collaboration.remote_views.clear();s.collaboration.status="Connection interrupted. Reconnecting to the host automatically…".into();},
        Event::Resumed{document,revision,next_sequence}=>{s.collaboration.phase=Phase::Connected;s.collaboration.error.clear();
            if let Some(bridge)=&mut s.collaboration.bridge{bridge.document=document;bridge.revision=revision;bridge.sequence=bridge.sequence.max(next_sequence);bridge.pending.retain(|patch|patch.id.sequence>=next_sequence);bridge.connected=true;bridge.history_pending=false;}
            if let Err(error)=apply_projection(&mut s){record_issue(&mut s,error);}s.collaboration.remote_views.clear();s.collaboration.status="Reconnected. Your edits are synchronized.".into();
        },
        Event::Preview(packet)=>{
            if packet.request.view==3{
                if s.collaboration.filmstrip_request.as_ref().is_some_and(|(id,_,_)|*id==packet.request.id){let(_,request,_)=s.collaboration.filmstrip_request.take().unwrap();let image=packet.decode().ok().map(|(image,_)|image);filmstrips::remote_complete(&mut s,request,image);ui.set_clips(model(clip_views(&s)));}continue;
            }
            if packet.request.view==2{
                if s.collaboration.thumbnail_request.is_some_and(|(id,_)|id==packet.request.id){
                    s.collaboration.thumbnail_request=None;
                    if let Some(media)=packet.request.source.and_then(|id|s.editor.project.media(id)).cloned(){
                        let image=packet.decode().ok().map(|(image,_)|to_image(&image));s.thumbnail_cache.retain(|(cached,_)|cached.id!=media.id);s.thumbnail_cache.push_back((media,image));while s.thumbnail_cache.len()>128{s.thumbnail_cache.pop_front();}ui.set_bin_items(model(bin_views(&s)));
                    }
                }continue;
            }
            let source=packet.request.source.is_some();
            let valid=s.collaboration.remote_views.get(&source).is_some_and(|view|view.request.id==packet.request.id&&view.request.revision==packet.request.revision&&view.request.source==packet.request.source)
                &&s.collaboration.bridge.as_ref().is_some_and(|b|b.revision==packet.request.revision&&b.connected&&!b.host);
            if !valid||(!source&&packet.request.sequence!=s.editor.project.active_sequence){continue;}
            match packet.decode(){
                Ok((image,samples))=>{
                    if let Some(view)=s.collaboration.remote_views.get_mut(&source){view.last_frame=packet.request.frame;view.at=Instant::now();}
                    if source{ui.set_source_image(to_image(&image));ui.set_show_source_picture(true);s.source_request_pending=false;s.source_feedback.picture(true,Instant::now());s.source.waiting_picture=false;s.source.waiting_audio=false;
                        if s.source.playing{s.source.remote_picture(packet.request.frame);}
                    }else{picture::scopes(ui,&image);ui.set_viewer_image(to_image(&image));ui.set_show_picture(true);s.program_request_pending=false;s.program_feedback.picture(true,Instant::now());s.waiting_picture=false;s.waiting_audio=false;
                        if s.playing{s.editor.project.sequence.playhead=packet.request.frame;s.anchor_frame=packet.request.frame;s.anchor_at=Instant::now();}
                    }
                    if !samples.is_empty()&&if source{s.source.playing}else{s.playing}{if let Some(audio)=&s.collaboration.remote_audio{audio.push(samples);}}
                    push_clock(ui,&s);sync_source_controls(ui,&s);
                },
                Err(error)=>{if source{s.source_feedback.fail(error);}else{s.program_feedback.fail(error);}s.playing=false;s.source.stop();if let Some(audio)=&s.collaboration.remote_audio{audio.clear();}}
            }
        },
        Event::Uploaded(path)=>uploaded.push(path),
        Event::MediaAvailable(available)=>{s.collaboration.remote_media=available;if available&&s.collaboration.remote_audio.is_none(){s.collaboration.remote_audio=Some(crate::collaboration::media::Audio::new());}if !available{s.collaboration.status="This host needs an updated Quaternio build to stream media.".into();}},
        Event::Ready{epoch,actor,member,document,revision,next_sequence,host}=>{s.collaboration.phase=if host{Phase::Hosting}else{Phase::Connected};s.collaboration.error.clear();
            if s.collaboration.start_gen!=s.edit_gen&&s.collaboration.bridge.is_none(){s.collaboration.service=None;s.collaboration.phase=Phase::Ended;s.collaboration.error="The project changed while connecting. Your edits were retained; start the session again.".into();continue;}
            s.collaboration.remote_views.clear();s.collaboration.waveforms.clear();s.collaboration.filmstrip_request=None;s.collaboration.thumbnail_request=None;
            if !host{s.thumbnail_cache.clear();s.waveform_results.clear();s.filmstrips.reset();}
            let last=document.clone();let mut project=match document::materialize(&document){Ok(p)=>p,Err(error)=>{s.collaboration.status=error;continue;}};
            project.next_id=member.first;
            if !host&&project.document_id!=s.editor.project.document_id{s.editor.path=None;s.editor.selected_clips.clear();s.editor.selected_media=None;}
            s.collaboration.remote_document=if host{None}else{Some(project.document_id.clone())};
            s.editor.apply_shared_project(project);
            if !host{for media in &mut s.editor.project.media{media.path.clear();}}
            let local_branch=s.editor.project.clone();
            s.collaboration.bridge=Some(Bridge{epoch,actor,member,revision,sequence:next_sequence,document,last,pending:Vec::new(),local_branch,connected:true,host,undo:Vec::new(),redo:0,history_pending:false});
            s.collaboration.status=if host{"Session started. Invite someone to edit with you."}else{"Connected. Your edits are shared automatically."}.into();note_borrowed(&mut s);
        },
        Event::Code(code)=>{s.collaboration.inviting=false;let simple=code.starts_with("quaternio-join-v1:");s.collaboration.code=code;s.collaboration.status=if simple{"Invite ready. Send the code privately; your guest can join immediately."}else if s.collaboration.bridge.as_ref().is_some_and(|b|b.host){"Invitation ready. Save it and send it privately."}else{"Your reply is ready. Send it back to the host."}.into();},
        Event::Participants(peers)=>s.collaboration.participants=peers,
        Event::Status(message)=>s.collaboration.status=match message.as_str(){"ICE New"|"ICE Checking"|"Peer New"|"Peer Connecting"=>"Connecting…".into(),"ICE Connected"|"ICE Completed"|"Peer Connected"=>"Connection established. Finishing setup…".into(),"ICE Disconnected"|"Peer Disconnected"=>"Connection interrupted. Waiting to reconnect to the host…".into(),_=>message},
        Event::Conflict(reason)=>{if s.collaboration.issues.len()>=64{s.collaboration.issues.remove(0);}s.collaboration.issues.push(reason);},
        Event::Ended(reason)=>{s.collaboration.phase=Phase::Ended;s.collaboration.inviting=false;s.collaboration.transfers.clear();s.playing=false;s.source.stop();s.audio.stop();if let Some(audio)=&s.collaboration.remote_audio{audio.clear();}if s.collaboration.bridge.as_ref().is_some_and(|b|b.pending.is_empty()&&!b.history_pending&&document::capture(&s.editor.project)==b.last){s.collaboration.status=reason;s.collaboration.error.clear();}else{record_issue(&mut s,reason);}if let Some(b)=&mut s.collaboration.bridge{b.connected=false;}s.collaboration.service=None;s.editor.clear_shared_history();s.editor.project.ensure_integrity();},
        Event::Rejected{id,reason}=>{
            record_issue(&mut s,reason);if let Some(b)=&mut s.collaboration.bridge{b.pending.retain(|p|p.id!=id);b.history_pending=false;}
            if let Err(error)=apply_projection(&mut s){s.collaboration.status=error;}
        },
        Event::Commit(commit)=>{
            let result=if let Some(b)=&mut s.collaboration.bridge{
                let fresh=commit.revision>b.revision;
                let result=document::apply_commit(&mut b.document,&mut b.revision,&commit);
                if commit.patch.id.author==b.actor{b.pending.retain(|p|p.id!=commit.patch.id);b.history_pending=false;
                    if fresh { if let Some(id)=&commit.undoes{b.undo.retain(|old|old!=id);b.redo+=1;}else{b.undo.push(commit.patch.id.clone());b.redo=if commit.redid.is_some(){b.redo.saturating_sub(1)}else{0};} } }
                result
            }else{Err("Host sent an edit before the initial snapshot".into())};
            if let Err(error)=result{record_issue(&mut s,error);if let Some(b)=&mut s.collaboration.bridge{b.connected=false;}}
            else{
                for alternative in &commit.alternatives{let message=format!("Concurrent {}: previous {} · incoming {}. Both values are retained in the session log.",alternative.key,alternative.previous.as_ref().map(|v|v.to_string()).unwrap_or_default(),alternative.incoming.as_ref().map(|v|v.to_string()).unwrap_or_default());if s.collaboration.issues.len()>=64{s.collaboration.issues.remove(0);}s.collaboration.issues.push(message);}
                if let Err(error)=apply_projection(&mut s){record_issue(&mut s,error);}else{s.collaboration.status="Edits synchronized".into();}
            }
        },
    }}
    {
        let mut s=state.borrow_mut();
        if s.collaboration.leave_at.is_some_and(|at|at.elapsed()>std::time::Duration::from_secs(5))
            || (s.collaboration.leave_at.is_some()&&s.collaboration.bridge.as_ref().is_none_or(|b|b.pending.is_empty()&&!b.history_pending)){
            finish_leave(&mut s);changed=true;
        }
    }
    if !uploaded.is_empty(){import_paths(ui,state,uploaded);}
    if changed{full_sync(ui,state);sync(ui,&state.borrow());}
}
