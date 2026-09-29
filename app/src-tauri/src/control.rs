//! Control the existing app/session from a private Unix socket.
use tauri::{AppHandle,Manager,Emitter};
use engine::{clock::{Clock,SystemClock},types::Phase};
use reps_cli::control::Command;
use crate::{SharedCore,Runtime,daily};

#[derive(Clone,serde::Serialize)]
#[serde(rename_all="camelCase")]
pub struct DisplayState {pub notice:Option<String>,pub view:String,pub preview_only:bool,pub last_error:Option<String>,pub frames_seen:u64}
impl Default for DisplayState{fn default()->Self{Self{notice:None,view:"screen".into(),preview_only:false,last_error:None,frames_seen:0}}}
pub type SharedDisplay=std::sync::Mutex<DisplayState>;
#[tauri::command]
pub fn get_display_state(app:AppHandle)->DisplayState {app.state::<SharedDisplay>().lock().unwrap().clone()}
pub fn preview_only(app:&AppHandle)->bool{app.state::<SharedDisplay>().lock().unwrap().preview_only}
pub fn error(app:&AppHandle,message:String){app.state::<SharedDisplay>().lock().unwrap().last_error=Some(message);}
pub fn camera_frame(app:&AppHandle)->bool{let state=app.state::<SharedDisplay>();let mut s=state.lock().unwrap();if s.view!="camera"{return false;}s.frames_seen=s.frames_seen.saturating_add(1);true}
pub fn video_event(app:&AppHandle,event:&serde_json::Value){let state=app.state::<SharedDisplay>();let mut s=state.lock().unwrap();if s.view!="video"{return;}if event["event"]=="frame"{s.frames_seen=s.frames_seen.saturating_add(1);}if event["event"]=="error"{s.last_error=event["message"].as_str().map(str::to_string);}}
fn view(app:&AppHandle,name:&str,preview:bool){
 let state={let state=app.state::<SharedDisplay>();let mut s=state.lock().unwrap();s.view=name.into();s.preview_only=preview;s.last_error=None;s.frames_seen=0;s.clone()};
 let _=app.emit("display-state",state);
}
fn stop_preview(app:&AppHandle){if preview_only(app){crate::hub::disable_metric_now(app);}view(app,"screen",false);}
fn show(app:&AppHandle){if let Some(w)=app.get_webview_window("main"){let _=w.show();let _=w.unminimize();}}
/// `--all`: nothing left running. Previews, fixture video and the camera stop;
/// windows hide unless lock mode must keep them up.
fn close_all(app:&AppHandle){
 stop_preview(app);crate::stop_debug_process(app);crate::hub::disable_metric_now(app);
 if !app.state::<Runtime>().enforces_windows(){for w in app.webview_windows().values(){let _=w.hide();}}
}
/// Put a debug set into a detection state. Anything but `live` turns the camera
/// off so only the simulated state (and step/done/next) drives the UI.
fn detection_state(app:&AppHandle,state:&str){
 if state=="live"{return;}
 crate::hub::disable_metric_now(app);
 match state{
  "no-pose"=>{println!("[DETECT] 🔴 no pose in frame (simulated)");let _=app.emit("vision-pose",serde_json::json!({"detected":false}));},
  "vision-down"=>{let _=app.emit("vision-fallback",serde_json::json!({"reason":"debug: simulated vision failure"}));},
  _=>{},
 }
}
/// `rfp debug next`: advance exactly one phase using the same engine transitions
/// the timer, detector and weight entry use.
fn advance(app:&AppHandle,weight:Option<f64>)->Result<(),String>{
 let (now,today)=(SystemClock.now(),SystemClock.today());
 let state=app.state::<SharedCore>();
 let before=state.lock().unwrap().session.snapshot(now);
 match before.phase{
  Phase::Coding|Phase::ExerciseRequired|Phase::Unlocked=>{
   let mut core=state.lock().unwrap();
   match before.phase{
    Phase::Coding=>{core.session.configure_timer(0.,now);core.session.tick(now,&today);},
    Phase::ExerciseRequired=>core.session.begin_workout(),
    _=>{core.session.resume_coding(now);let minutes=core.store.setting("work_minutes","25").parse().unwrap_or(25.);core.session.configure_timer(minutes,now);},
   }
   let snap=crate::persist_and_snapshot(&mut core);drop(core);
   if snap.phase==before.phase{return Err(format!("Cannot advance from {:?}; nothing to prescribe",before.phase));}
   if snap.phase==Phase::WorkoutActive{crate::enable_metric_for(app,&snap);}
   if snap.phase==Phase::Coding{crate::hub::disable_metric_async(app);}
   crate::emit_snapshot(app,&snap);
   if snap.phase!=Phase::Coding{show(app);}
  },
  Phase::WorkoutActive=>{
   let rx=before.prescription.ok_or("Active set has no prescription")?;
   crate::simulate_progress(app.clone(),state,if rx.target_seconds>0.{rx.target_seconds}else{rx.target_reps as f64},true)?;
  },
  Phase::WeightConfirmation=>{
   let default=before.prescription.map(|rx|rx.default_weight).unwrap_or(0.);
   crate::confirm_weight(app.clone(),state,weight.unwrap_or(default));
  },
 }
 Ok(())
}
fn dispatch(app:&AppHandle, command:Command)->Result<serde_json::Value,String>{
 command.validate()?;

 match command {
  Command::Snapshot=>{},
  Command::Workday{end,warn_minutes}=>{
   let state=app.state::<SharedCore>();let core=state.lock().unwrap();
   if let Some(end)=end{core.store.set_setting("workday_end",&end).map_err(|e|e.to_string())?;}
   if let Some(minutes)=warn_minutes{core.store.set_setting("workday_warn_minutes",&minutes.to_string()).map_err(|e|e.to_string())?;}
  },
  Command::Mode{mode}=>{
   let selected=if mode=="debug"{crate::AppMode::Debug}else{crate::AppMode::Workout};
   if app.state::<Runtime>().mode==selected{return Ok(serde_json::json!({"mode":mode,"restarting":false}));}
   let phase=app.state::<SharedCore>().lock().unwrap().session.snapshot(SystemClock.now()).phase;
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
    "settings"=>return serde_json::to_value(crate::hub::get_camera_settings(app.clone())?).map_err(|e|e.to_string()),
    "set"=>{
     let phase=app.state::<SharedCore>().lock().unwrap().session.snapshot(SystemClock.now()).phase;
     if phase!=Phase::Coding||preview_only(app){return Err("Stop preview/workout before changing cameras".into());}
     let mut current=serde_json::to_value(crate::hub::get_camera_settings(app.clone())?).map_err(|e|e.to_string())?;
     for (k,v) in settings.as_ref().and_then(|v|v.as_object()).ok_or("Camera settings must be an object")?{current[k]=v.clone();}
     crate::hub::save_camera_settings(app.clone(),serde_json::from_value(current).map_err(|e|e.to_string())?)?;
     return serde_json::to_value(crate::hub::get_camera_settings(app.clone())?).map_err(|e|e.to_string());
    },
    "preview"=>{
     let phase=app.state::<SharedCore>().lock().unwrap().session.snapshot(SystemClock.now()).phase;
     if !matches!(phase,Phase::Coding|Phase::WorkoutActive){return Err("Finish, skip, or cancel the current break before starting camera preview".into());}
     let preview=phase==Phase::Coding;
     view(app,"camera",preview);show(app);
     if preview{crate::hub::enable_metric_async(app,"squat".into(),100000,0.);}
    },
    "stop"=>stop_preview(app),
    "status"=>{
     let state=app.state::<crate::hub::SharedHub>();let mut hub=state.lock().unwrap();
     let health=hub.as_mut().ok_or("Vision hub is starting")?.health().map_err(|e|e.to_string())?;
     return Ok(serde_json::json!({"visionHost":health.vision_host,"camera":health.camera,"enabledMetrics":health.enabled_metrics,"display":get_display_state(app.clone())}));
    },_=>unreachable!()
   }
  },
  Command::Debug{operation,exercise,value,video,state,reps,seconds,weight,all}=>{
   app.state::<Runtime>().require_debug()?;
   match operation.as_str(){
    "history"=>{let state=app.state::<SharedCore>();let core=state.lock().unwrap();return Ok(serde_json::json!({"source":"debug","records":core.store.records(None,None,0,100,false).map_err(|e|e.to_string())?}));},
    "exercises"=>return serde_json::to_value(crate::debug_exercises(app.clone())?).map_err(|e|e.to_string()),
    "videos"=>return serde_json::to_value(crate::debug_videos(app.clone())?).map_err(|e|e.to_string()),
    "start"=>{
     if (reps.is_some()||seconds.is_some()||weight.is_some())&&exercise.is_none(){return Err("Programming a set needs --exercise NAME".into());}
     stop_preview(app);
     crate::debug_program(app.clone(),app.state::<SharedCore>(),"workout".into(),exercise,|rx|{
      if let Some(n)=reps{rx.kind=engine::types::ExerciseKind::Rep;rx.target_reps=n;rx.target_seconds=0.;}
      if let Some(n)=seconds{rx.kind=engine::types::ExerciseKind::Continuous;rx.target_seconds=n;rx.target_reps=0;}
      if let Some(n)=weight{rx.default_weight=n;}
     })?;
     detection_state(app,state.as_deref().unwrap_or("live"));show(app);
    },
    "stop"=>{stop_preview(app);crate::debug_mode(app.clone(),app.state::<SharedCore>(),"coding".into(),None)?;if all{close_all(app);}},
    "next"=>{stop_preview(app);advance(app,weight)?;},
    "step"|"done"=>{
     let snap=app.state::<SharedCore>().lock().unwrap().session.snapshot(SystemClock.now());
     if snap.phase!=Phase::WorkoutActive{return Err("Start a debug workout first".into());}
     crate::simulate_progress(app.clone(),app.state::<SharedCore>(),value.unwrap_or(snap.progress.map(|p|p.value).unwrap_or(0.)+1.),operation=="done")?;
    },
    "video"=>{stop_preview(app);view(app,"video",false);show(app);crate::debug_stream_start(app.clone(),app.state::<crate::SharedDebugProcess>(),video.ok_or("Supply --file PATH")?,exercise.ok_or("Supply --exercise NAME")?)?;},
    "video-stop"=>{crate::stop_debug_process(app);view(app,"screen",false);},_=>unreachable!()
   }
  },
  Command::Display{window,visible,fullscreen,monitor}=>{
   let w=app.get_webview_window(&window).ok_or("Window unavailable")?;
   let monitors=w.available_monitors().map_err(|e|e.to_string())?;
   if let Some(index)=monitor{let m=monitors.get(index).ok_or("Invalid monitor index")?;w.set_position(tauri::Position::Physical(*m.position())).map_err(|e|e.to_string())?;}
   if let Some(on)=fullscreen{if app.state::<Runtime>().enforces_windows()&&!on{return Err("Disable lock mode first".into());}w.set_fullscreen(on).map_err(|e|e.to_string())?;}
   if let Some(on)=visible{if on{w.show()}else{w.hide()}.map_err(|e|e.to_string())?;}
   return Ok(serde_json::json!({"window":window,"monitors":monitors.iter().enumerate().map(|(i,m)|serde_json::json!({"index":i,"name":m.name(),"width":m.size().width,"height":m.size().height})).collect::<Vec<_>>()}));
  },
  Command::Start=>{stop_preview(app);daily::reminder_action(app.clone(),"start".into(),None)?;if let Some(w)=app.get_webview_window("main"){let _=w.show();let _=w.unminimize();}},
  Command::Snooze{minutes}=>daily::reminder_action(app.clone(),"snooze".into(),Some(minutes))?,
  Command::Skip=>daily::reminder_action(app.clone(),"skip".into(),None)?,
  Command::Cancel{all}=>{stop_preview(app);crate::stop_debug_process(app);crate::emergency_escape(app.clone(),app.state::<SharedCore>());if all{close_all(app);}},
  Command::Finish{weight,honor}=>{
   let phase=app.state::<SharedCore>().lock().unwrap().session.snapshot(SystemClock.now()).phase;
   if !matches!(phase,Phase::WorkoutActive|Phase::WeightConfirmation){return Err("No active workout to finish. Run: reps start".into());}
   if phase==Phase::WorkoutActive {
    if !honor{return Err("Target not yet detected. Use --honor only if you completed the prescribed set yourself".into());}
    crate::hub::honor_complete(app);
   }
   let phase=app.state::<SharedCore>().lock().unwrap().session.snapshot(SystemClock.now()).phase;
   if phase==Phase::WeightConfirmation {crate::confirm_weight(app.clone(),app.state::<SharedCore>(),weight);}
  },
  Command::Settings{minutes,lock_mode}=>{
   if lock_mode==Some(true){return Err("CLI-only displays cannot take over the terminal; lock mode must stay off".into());}
   let (current_minutes,current_lock)={let state=app.state::<SharedCore>();let core=state.lock().unwrap();(core.store.setting("work_minutes","25").parse().unwrap_or(25),core.store.setting("lock_mode","0")=="1")};
   if minutes.is_some()||lock_mode.is_some(){daily::save_daily_settings(app.clone(),minutes.unwrap_or(current_minutes),lock_mode.unwrap_or(current_lock))?;}
  },
  Command::Routine{json}=>{if let Some(s)=json{daily::save_routine(app.clone(),s)?;}return serde_json::from_str(&daily::routine_settings(app.clone())?).map_err(|e|e.to_string());},
  Command::Show=>{if let Some(w)=app.get_webview_window("main"){let _=w.show();let _=w.unminimize();}},
  Command::Hide=>{if app.state::<Runtime>().enforces_windows(){return Err("Disable lock mode before hiding screens: reps settings --lock-mode off".into());}for w in app.webview_windows().values(){let _=w.hide();}},
 }
 let snapshot=app.state::<SharedCore>().lock().unwrap().session.snapshot(SystemClock.now());
 Ok(serde_json::json!({"schemaVersion":1,"source":"local","capabilities":{"siteProfiles":true},"pid":std::process::id(),"mode":app.state::<Runtime>().mode,"sessionHome":app.state::<Runtime>().session_home,"display":get_display_state(app.clone()),"snapshot":snapshot,"settings":daily::daily_status(app.clone())}))
}
/// The display exposes only the two actions also available through the CLI.
#[tauri::command]
pub fn workout_action(app:AppHandle, action:String, weight:Option<f64>)->Result<serde_json::Value,String>{
 let command=match action.as_str(){
  "start"=>Command::Start,
  "finish"=>Command::Finish{weight:weight.ok_or("Enter a weight")?,honor:false},
  _=>return Err("Use the CLI for this action".into()),
 };
 dispatch(&app,command)
}
pub fn start(app:&AppHandle)->Result<(),String>{
 #[cfg(unix)] {
  use std::{os::unix::{net::UnixListener,fs::PermissionsExt,io::AsRawFd},io::{BufRead,BufReader,Read,Write},time::Duration};
  let path=app.state::<Runtime>().normal_home.join("control.sock");
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
