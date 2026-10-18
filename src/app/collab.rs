//! UI/session boundary: local gestures stay optimistic, host commits reconcile
//! by property, and rejected edits retain a saveable local branch.
use super::*;
use crate::collaboration::{OperationId,document::{self,Document,Patch,Member},session::{Session,Event,Participant}};

struct Bridge{
    epoch:String,actor:String,member:Member,revision:u64,sequence:u64,document:Document,last:Document,
    pending:Vec<Patch>,local_branch:Project,connected:bool,host:bool,undo:Vec<OperationId>,redo:usize,history_pending:bool,
}
#[derive(Default)]
pub(super) struct State{
    service:Option<Session>,bridge:Option<Bridge>,participants:Vec<Participant>,status:String,code:String,
    issues:Vec<String>,branches:Vec<Project>,start_gen:u64,observed_gen:u64,changed_at:Option<Instant>,leave_at:Option<Instant>,
}
pub(super) fn active(state:&AppState)->bool{state.collaboration.service.is_some()}
pub(super) fn readonly(state:&AppState)->bool{active(state)&&(state.collaboration.leave_at.is_some()||state.collaboration.bridge.as_ref().is_none_or(|b|!b.connected||!b.member.editor))}
pub(super) fn can_undo(state:&AppState)->bool{state.collaboration.bridge.as_ref().is_some_and(|b|b.connected&&b.member.editor&&!b.history_pending&&(!b.undo.is_empty()||!b.pending.is_empty()||state.editor.can_undo()))}
pub(super) fn can_redo(state:&AppState)->bool{state.collaboration.bridge.as_ref().is_some_and(|b|b.connected&&b.member.editor&&!b.history_pending&&b.redo>0)}
pub(super) fn sync(ui:&AppWindow,state:&AppState){
    let current=&state.collaboration;ui.set_session_connected(current.bridge.as_ref().is_some_and(|b|b.connected));ui.set_session_active(current.service.is_some());ui.set_session_readonly(readonly(state));
    ui.set_session_host(current.bridge.as_ref().is_some_and(|b|b.host));ui.set_session_status(current.status.clone().into());
    ui.set_session_code(current.code.clone().into());ui.set_session_issues(current.issues.iter().rev().take(16).map(|text|text.chars().take(2048).collect::<String>()).collect::<Vec<_>>().join("\n\n").into());ui.set_session_has_branch(!current.branches.is_empty());
    ui.set_session_peers(model(current.participants.iter().map(|p|PeerItem{id:p.id.clone().into(),name:p.name.clone().into(),role:if p.editor{"Editor"}else{"Viewer"}.into(),state:p.state.clone().into(),revocable:current.bridge.as_ref().is_some_and(|b|b.host&&b.actor!=p.id)&&p.state!="Revoked"}).collect()));
    if active(state){ui.set_can_undo(can_undo(state));ui.set_can_redo(can_redo(state));}
    ui.set_session_revision(current.bridge.as_ref().map(|b|format!("Revision {} · {} pending",b.revision,b.pending.len())).unwrap_or_default().into());
}
fn record_issue(state:&mut AppState,reason:String){
    let branch=state.collaboration.bridge.as_ref().filter(|b|document::capture(&state.editor.project)==b.last).map(|b|b.local_branch.clone()).unwrap_or_else(||state.editor.project.clone());
    let c=&mut state.collaboration;
    if c.issues.len()>=64{c.issues.remove(0);}c.issues.push(reason.clone());
    c.status=reason; if c.branches.len()>=8{c.branches.remove(0);}c.branches.push(branch);
}
fn flush(state:&mut AppState)->Result<bool,String>{
    let c=&mut state.collaboration;let Some(b)=c.bridge.as_mut()else{return Ok(false);};
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
    if command=="session"{ui.set_show_session(!ui.get_show_session());ui.set_show_review(false);ui.set_show_activity(false);if ui.get_session_name().is_empty(){ui.set_session_name(state.borrow().preferences.reviewer_name.clone().into());}sync(ui,&state.borrow());return true;}
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
        "session-host"=>{
            if active(&state.borrow()){return Err("Leave the current session first".into());}
            let s=state.borrow();if s.pointer.down||s.import_rx.is_some(){return Err("Finish the current drag or import before hosting".into());}
            let service=Session::host(s.editor.project.clone(),ui.get_session_name().to_string())?;let generation=s.edit_gen;drop(s);
            state.borrow_mut().collaboration=State{service:Some(service),status:"Starting your session…".into(),start_gen:generation,observed_gen:generation,..Default::default()};
        },
        "session-join"=>{
            if active(&state.borrow()){return Err("Leave the current session first".into());}
            if !confirm_unsaved(ui,state){return Ok(());}
            let service=Session::join(ui.get_session_input().as_str())?;let generation=state.borrow().edit_gen;
            state.borrow_mut().collaboration=State{service:Some(service),status:"Connecting…".into(),start_gen:generation,observed_gen:generation,..Default::default()};
        },
        "session-invite"=>state.borrow().collaboration.service.as_ref().ok_or("Host a session first")?.invite(ui.get_session_invitee().to_string(),ui.get_session_editor_role())?,
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
        "session-leave"=>{
            let mut s=state.borrow_mut();
            if s.pointer.down||s.import_rx.is_some(){return Err("Finish the current drag or import before leaving".into());}
            if s.collaboration.leave_at.is_some(){finish_leave(&mut s);}else{
                flush(&mut s)?;
                if s.collaboration.bridge.as_ref().is_some_and(|b|b.connected&&(!b.pending.is_empty()||b.history_pending)){
                    s.collaboration.leave_at=Some(Instant::now());s.collaboration.status="Waiting for the final edit acknowledgement. Choose Leave again to retain the local branch and disconnect immediately.".into();
                }else{finish_leave(&mut s);}
            }
        },
        _=>{if let Some(id)=command.strip_prefix("session-revoke:"){state.borrow().collaboration.service.as_ref().ok_or("No active session")?.revoke(id.into())?;}else{return Err("Unknown session action".into());}},
    }Ok(())})();
    if let Err(error)=result{state.borrow_mut().collaboration.status=error;}sync(ui,&state.borrow());true
}
fn finish_leave(state:&mut AppState){
    if state.collaboration.bridge.as_ref().is_some_and(|b|!b.pending.is_empty()||b.history_pending){record_issue(state,"The session ended with pending edits. A local branch has been retained.".into());}
    let c=&mut state.collaboration;c.service=None;c.bridge=None;c.participants.clear();c.leave_at=None;c.status="Session ended. Your document and retained branches remain available locally.".into();
    state.editor.clear_shared_history();state.editor.project.ensure_integrity();
}
pub(super) fn pump(ui:&AppWindow,state:&Rc<std::cell::RefCell<AppState>>){
    if !active(&state.borrow())||state.borrow().pointer.down||state.borrow().import_rx.is_some(){return;}
    let mut changed=false;
    {
        let mut s=state.borrow_mut();let generation=s.edit_gen;
        if s.collaboration.observed_gen!=generation{s.collaboration.observed_gen=generation;s.collaboration.changed_at=Some(Instant::now());}
        if s.collaboration.changed_at.is_some_and(|at|at.elapsed()>std::time::Duration::from_millis(120)){
            match flush(&mut s){Ok(flushed)=>{changed|=flushed;s.collaboration.changed_at=None;},Err(error)=>{record_issue(&mut s,error);if let Some(b)=&mut s.collaboration.bridge{b.connected=false;}changed=true;}}
        }
    }
    let events:Vec<_>={let s=state.borrow();let mut events=Vec::new();
        for _ in 0..64{match s.collaboration.service.as_ref().unwrap().rx.try_recv(){Ok(event)=>events.push(event),Err(mpsc::TryRecvError::Disconnected)=>{events.push(Event::Ended("Session service stopped. Your local document and retained outbox remain available.".into()));break;},Err(_)=>break}}
        events
    };
    if !events.is_empty(){
        // Flush a just-finished local gesture before applying remote commits.
        let mut s=state.borrow_mut();if let Err(error)=flush(&mut s){record_issue(&mut s,error);}drop(s);
    }
    for event in events{changed=true;let mut s=state.borrow_mut();match event{
        Event::Ready{epoch,actor,member,document,revision,next_sequence,host}=>{
            if s.collaboration.start_gen!=s.edit_gen&&s.collaboration.bridge.is_none(){s.collaboration.service=None;s.collaboration.status="The project changed while connecting. Your edits were retained; start the session again.".into();continue;}
            let last=document.clone();let mut project=match document::materialize(&document){Ok(p)=>p,Err(error)=>{s.collaboration.status=error;continue;}};
            project.next_id=member.first;
            if !host&&project.document_id!=s.editor.project.document_id{s.editor.path=None;s.editor.selected_clips.clear();s.editor.selected_media=None;}
            s.editor.apply_shared_project(project);
            let local_branch=s.editor.project.clone();
            s.collaboration.bridge=Some(Bridge{epoch,actor,member,revision,sequence:next_sequence,document,last,pending:Vec::new(),local_branch,connected:true,host,undo:Vec::new(),redo:0,history_pending:false});
            s.collaboration.status=if host{"Session started. Invite someone to edit with you."}else{"Connected. Your edits are shared automatically."}.into();note_borrowed(&mut s);
        },
        Event::Code(code)=>{s.collaboration.code=code;s.collaboration.status=if s.collaboration.bridge.as_ref().is_some_and(|b|b.host){"Invitation ready. Save it and send it privately."}else{"Your reply is ready. Send it back to the host."}.into();},
        Event::Participants(peers)=>s.collaboration.participants=peers,
        Event::Status(message)=>s.collaboration.status=match message.as_str(){"ICE New"|"ICE Checking"|"Peer New"|"Peer Connecting"=>"Connecting…".into(),"ICE Connected"|"ICE Completed"|"Peer Connected"=>"Connection established. Finishing setup…".into(),"ICE Disconnected"|"Peer Disconnected"=>"Connection interrupted. Rejoin with a new invitation.".into(),_=>message},
        Event::Conflict(reason)=>{if s.collaboration.issues.len()>=64{s.collaboration.issues.remove(0);}s.collaboration.issues.push(reason);},
        Event::Ended(reason)=>{record_issue(&mut s,reason);if let Some(b)=&mut s.collaboration.bridge{b.connected=false;}s.collaboration.service=None;s.editor.clear_shared_history();s.editor.project.ensure_integrity();},
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
    if changed{full_sync(ui,state);sync(ui,&state.borrow());}
}
