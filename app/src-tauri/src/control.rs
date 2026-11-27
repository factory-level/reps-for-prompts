//! Control the existing app/session from a private Unix socket.
use tauri::{AppHandle,Manager,Emitter};
use engine::types::{ExerciseKind,Phase};
use reps_cli::control::Command;
use crate::{Lifecycle,Runtime,daily};

#[derive(Clone,serde::Serialize)]
#[serde(rename_all="camelCase")]
pub struct DisplayState {pub notice:Option<String>,pub view:String,pub preview_only:bool,pub last_error:Option<String>,pub frames_seen:u64,pub vision:crate::vision_status::VisionStatus}
impl Default for DisplayState{fn default()->Self{Self{notice:None,view:"screen".into(),preview_only:false,last_error:None,frames_seen:0,vision:Default::default()}}}
pub type SharedDisplay=std::sync::Mutex<DisplayState>;
#[tauri::command]
pub fn get_display_state(app:AppHandle)->DisplayState {app.state::<SharedDisplay>().lock().unwrap().clone()}
/// Show (Some) or clear (None) the notice; windows hear of it only on a change.
pub fn set_notice(app:&AppHandle,notice:Option<String>){let state={let state=app.state::<SharedDisplay>();let mut s=state.lock().unwrap();if s.notice==notice{return;}s.notice=notice;s.clone()};let _=app.emit("display-state",state);}
pub fn error(app:&AppHandle,message:String){app.state::<SharedDisplay>().lock().unwrap().last_error=Some(message);}
/// The camera/vision path changed state: tell the windows.
pub fn vision(app:&AppHandle,status:crate::vision_status::VisionStatus){let state={let state=app.state::<SharedDisplay>();let mut s=state.lock().unwrap();s.vision=status;s.clone()};let _=app.emit("display-state",state);}
pub fn camera_frame(app:&AppHandle)->bool{let state=app.state::<SharedDisplay>();let mut s=state.lock().unwrap();if s.view!="camera"{return false;}s.frames_seen=s.frames_seen.saturating_add(1);true}
pub fn video_event(app:&AppHandle,event:&serde_json::Value){let state=app.state::<SharedDisplay>();let mut s=state.lock().unwrap();if s.view!="video"{return;}if event["event"]=="frame"{s.frames_seen=s.frames_seen.saturating_add(1);}if event["event"]=="error"{s.last_error=event["message"].as_str().map(str::to_string);}}
fn view(app:&AppHandle,name:&str,preview:bool){
 let state={let state=app.state::<SharedDisplay>();let mut s=state.lock().unwrap();s.view=name.into();s.preview_only=preview;s.last_error=None;s.frames_seen=0;s.clone()};
 let _=app.emit("display-state",state);
}
fn stop_preview(app:&AppHandle){app.state::<Lifecycle>().preview_off();view(app,"screen",false);}
fn show(app:&AppHandle){if let Some(w)=app.get_webview_window("main"){let _=w.show();let _=w.unminimize();}}
/// `--all`: nothing left running. Previews, fixture video and the camera stop
/// and every window hides (RFP no longer locks, so none must stay up).
fn close_all(app:&AppHandle){
 stop_preview(app);crate::stop_debug_process(app);app.state::<Lifecycle>().release_camera();
 for w in app.webview_windows().values(){let _=w.hide();}
}
fn dispatch(app:&AppHandle, command:Command)->Result<serde_json::Value,String>{
 command.validate()?;

 match command {
  Command::Snapshot=>{},
  Command::Workday{end,warn_minutes}=>{
   let life=app.state::<Lifecycle>();let core=life.core();
   if let Some(end)=end{core.store.set_setting("workday_end",&end).map_err(|e|e.to_string())?;}
   if let Some(minutes)=warn_minutes{core.store.set_setting("workday_warn_minutes",&minutes.to_string()).map_err(|e|e.to_string())?;}
  },
  Command::Mode{mode}=>{
   let selected=if mode=="debug"{crate::AppMode::Debug}else{crate::AppMode::Workout};
   if app.state::<Runtime>().mode==selected{return Ok(serde_json::json!({"mode":mode,"restarting":false}));}
   let phase=app.state::<Lifecycle>().snapshot().phase;
   if matches!(phase,Phase::WorkoutActive|Phase::WeightConfirmation){return Err("Cancel or finish the current workout before switching modes".into());}
   // The CLI restarts systemd only after this response closes. A Tauri
   // relaunch can start before the old process releases its instance lock.
   app.state::<Runtime>().save_mode(selected)?;
   return Ok(serde_json::json!({"mode":mode,"restartRequired":true}));
  },
  Command::Camera{operation,settings}=>{
   match operation.as_str(){
    "list"=>{
     let mut cameras=Vec::new();if let Ok(entries)=std::fs::read_dir("/sys/class/video4linux"){for entry in entries.flatten(){cameras.push(serde_json::json!({"device":format!("/dev/{}",entry.file_name().to_string_lossy()),"name":std::fs::read_to_string(entry.path().join("name")).unwrap_or_default().trim()}));}}
     return Ok(serde_json::json!({"devices":cameras}));
    },
    "settings"=>return serde_json::to_value(crate::detector::load_camera_settings(&app.state::<Runtime>().normal_home)?.unwrap_or_default()).map_err(|e|e.to_string()),
    "set"=>{
     let phase=app.state::<Lifecycle>().snapshot().phase;
     if phase!=Phase::Coding||app.state::<Lifecycle>().previewing(){return Err("Stop preview/workout before changing cameras".into());}
     let mut current=serde_json::to_value(crate::detector::load_camera_settings(&app.state::<Runtime>().normal_home)?.unwrap_or_default()).map_err(|e|e.to_string())?;
     for (k,v) in settings.as_ref().and_then(|v|v.as_object()).ok_or("Camera settings must be an object")?{current[k]=v.clone();}
     crate::hub::save_camera_settings(app.clone(),serde_json::from_value(current).map_err(|e|e.to_string())?)?;
     return serde_json::to_value(crate::detector::load_camera_settings(&app.state::<Runtime>().normal_home)?.unwrap_or_default()).map_err(|e|e.to_string());
    },
    "preview"=>{let preview=app.state::<Lifecycle>().preview_on()?;view(app,"camera",preview);show(app);},
    "stop"=>stop_preview(app),
    "status"=>{
     let state=app.state::<crate::hub::SharedHub>();let mut hub=state.lock().unwrap();
     // No hub to ask: say why vision is unavailable rather than "starting" forever.
     let health=hub.as_mut().ok_or_else(||match app.state::<Lifecycle>().vision_status(){crate::vision_status::VisionStatus::Unavailable{reason}=>reason,_=>"Vision hub is starting".into()})?.health().map_err(|e|e.to_string())?;
     return Ok(serde_json::json!({"visionHost":health.vision_host,"camera":health.camera,"enabledMetrics":health.enabled_metrics,"display":get_display_state(app.clone())}));
    },_=>unreachable!()
   }
  },
  Command::Debug{operation,exercise,value,video,state,reps,seconds,weight,all}=>{
   app.state::<Runtime>().require_debug()?;
   match operation.as_str(){
    "history"=>{let life=app.state::<Lifecycle>();let core=life.core();return Ok(serde_json::json!({"source":"debug","records":core.store.records(None,None,0,100,false).map_err(|e|e.to_string())?}));},
    "exercises"=>return serde_json::to_value(crate::detector::debug_exercise_options()?).map_err(|e|e.to_string()),
    "videos"=>return serde_json::to_value(crate::debug_videos(app.clone())?).map_err(|e|e.to_string()),
    "start"=>{
     if (reps.is_some()||seconds.is_some()||weight.is_some())&&exercise.is_none(){return Err("Programming a set needs --exercise NAME".into());}
     let selected=exercise.map(|name|crate::detector::debug_exercise_options()?.into_iter().find(|rx|rx.exercise==name).ok_or_else(||format!("Unknown exercise: {name}"))).transpose()?
      .map(|mut rx|{
       if let Some(n)=reps{rx.kind=ExerciseKind::Rep;rx.target_reps=n;rx.target_seconds=0.;}
       if let Some(n)=seconds{rx.kind=ExerciseKind::Continuous;rx.target_seconds=n;rx.target_reps=0;}
       if let Some(n)=weight{rx.default_weight=n;}
       rx
      });
     view(app,"screen",false);crate::stop_debug_process(app);
     let life=app.state::<Lifecycle>();life.debug_start(selected)?;
     let state=state.as_deref().unwrap_or("live");life.debug_detection(state)?;
     // The camera is off, so no landmarks will say it: tell the windows directly.
     if state=="no-pose"{println!("[DETECT] 🔴 no pose in frame (simulated)");let _=app.emit("vision-pose",serde_json::json!({"detected":false}));}
     show(app);
    },
    "stop"=>{view(app,"screen",false);crate::stop_debug_process(app);app.state::<Lifecycle>().debug_stop()?;if all{close_all(app);}},
    "next"=>{stop_preview(app);let snap=app.state::<Lifecycle>().debug_next(weight)?;if !matches!(snap.phase,Phase::Coding|Phase::Idle){show(app);}},
    "step"|"done"=>{app.state::<Lifecycle>().debug_progress(value,operation=="done")?;},
    "video"=>{stop_preview(app);view(app,"video",false);show(app);crate::debug_stream_start(app.clone(),video.ok_or("Supply --file PATH")?,exercise.ok_or("Supply --exercise NAME")?)?;},
    "video-stop"=>{crate::stop_debug_process(app);view(app,"screen",false);},_=>unreachable!()
   }
  },
  Command::Display{window,visible,fullscreen,monitor}=>{
   let w=app.get_webview_window(&window).ok_or("Window unavailable")?;
   let monitors=w.available_monitors().map_err(|e|e.to_string())?;
   if let Some(index)=monitor{let m=monitors.get(index).ok_or("Invalid monitor index")?;w.set_position(tauri::Position::Physical(*m.position())).map_err(|e|e.to_string())?;}
   if let Some(on)=fullscreen{w.set_fullscreen(on).map_err(|e|e.to_string())?;}
   if let Some(on)=visible{if on{w.show()}else{w.hide()}.map_err(|e|e.to_string())?;}
   return Ok(serde_json::json!({"window":window,"monitors":monitors.iter().enumerate().map(|(i,m)|serde_json::json!({"index":i,"name":m.name(),"width":m.size().width,"height":m.size().height})).collect::<Vec<_>>()}));
  },
  // Idle: start the next routine's session. Otherwise: start the set that's due.
  Command::Start=>{view(app,"screen",false);if app.state::<Lifecycle>().snapshot().phase==Phase::Idle{crate::ui::begin_session(app)?;}else{daily::reminder_action(app.clone(),"start".into(),None)?;}},
  Command::Snooze{minutes}=>daily::reminder_action(app.clone(),"snooze".into(),Some(minutes))?,
  Command::Skip=>daily::reminder_action(app.clone(),"skip".into(),None)?,
  // Ends the session early: confirmed sets stay, the cycle does not move.
  Command::Cancel{all}=>{view(app,"screen",false);crate::stop_debug_process(app);if app.state::<Runtime>().is_debug(){app.state::<Lifecycle>().cancel();}else{app.state::<Lifecycle>().end_workout();}if all{close_all(app);}},
  Command::Finish{weight,honor}=>{app.state::<Lifecycle>().finish(weight,honor)?;},
  Command::Settings{minutes,lock_mode}=>{
   if lock_mode==Some(true){return Err("RFP never locks the desktop; lock mode was removed".into());}
   if let Some(minutes)=minutes{daily::save_daily_settings(app.clone(),minutes)?;}
  },
  Command::Routine{json}=>{if let Some(s)=json{daily::save_routine(app.clone(),s)?;}return serde_json::from_str(&daily::routine_settings(app.clone())?).map_err(|e|e.to_string());},
  Command::Show=>{if let Some(w)=app.get_webview_window("main"){let _=w.show();let _=w.unminimize();}},
  Command::Hide=>{for w in app.webview_windows().values(){let _=w.hide();}},
 }
 let snapshot=app.state::<Lifecycle>().snapshot();
 Ok(serde_json::json!({"schemaVersion":1,"source":"local","capabilities":{"siteProfiles":true},"pid":std::process::id(),"mode":app.state::<Runtime>().mode,"sessionHome":app.state::<Runtime>().session_home,"display":get_display_state(app.clone()),"snapshot":snapshot,"settings":daily::daily_status(app.clone())}))
}
pub fn start(app:&AppHandle)->Result<(),String>{
 #[cfg(unix)] {
  use std::{os::unix::{net::UnixListener,fs::PermissionsExt,io::AsRawFd},io::{BufRead,BufReader,Read,Write},time::Duration};
  let path=app.state::<Runtime>().normal_home.join("control.sock");
  // A Unix socket address is a fixed 108-byte sun_path on Linux, so a long data
  // directory makes bind fail with libstd's bare "path must be shorter than
  // SUN_LEN" — which Tauri then reports as an unexplained setup panic.
  // REPS_APP_HOME is a documented knob and temp paths blow the limit easily, so
  // say what is wrong and how to fix it.
  const SUN_PATH_MAX:usize=108;
  let length=path.as_os_str().as_encoded_bytes().len()+1; // +1 for the NUL
  if length>SUN_PATH_MAX{
   return Err(format!("data directory path is too long: the control socket {} needs {length} bytes, over the {SUN_PATH_MAX}-byte Unix socket limit. Point REPS_APP_HOME at a shorter path.",path.display()));
  }
  // The desktop instance lock is already held, so a prior socket is stale.
  if path.exists(){std::fs::remove_file(&path).map_err(|e|e.to_string())?;}
  let listener=UnixListener::bind(&path).map_err(|e|e.to_string())?;
  std::fs::set_permissions(&path,std::fs::Permissions::from_mode(0o600)).map_err(|e|e.to_string())?;
  let app=app.clone();
  std::thread::spawn(move||{
   for stream in listener.incoming(){
    let Ok(mut stream)=stream else{break};
    #[cfg(target_os="linux")] unsafe {
     let mut peer:libc::ucred=std::mem::zeroed();let mut len=std::mem::size_of::<libc::ucred>() as libc::socklen_t;
     if libc::getsockopt(stream.as_raw_fd(),libc::SOL_SOCKET,libc::SO_PEERCRED,&mut peer as *mut _ as *mut _,&mut len)!=0 || peer.uid!=libc::geteuid(){continue;}
    }
    let _=stream.set_read_timeout(Some(Duration::from_secs(2)));let _=stream.set_write_timeout(Some(Duration::from_secs(2)));
    let result=(||{
     let mut line=String::new();BufReader::new((&stream).take(65537)).read_line(&mut line).map_err(|_|"Could not read command".to_string())?;
     if line.len()>65536||!line.ends_with('\n'){return Err("Command too large or incomplete".into());}
     let command=reps_cli::control::parse(&line)?;
     dispatch(&app,command)
    })();
    let response=result.unwrap_or_else(|e|serde_json::json!({"error":e}));
    let _=stream.write_all(response.to_string().as_bytes());
   }
  });
 }
 Ok(())
}
