//! Same-user, local-only desktop control. No HTTP port or cloud credential needed.
use serde::{Deserialize, Serialize};
use std::{io::{Read, Write}, path::Path, time::Duration};
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag="action", rename_all="snake_case", deny_unknown_fields)]
pub enum Command {
 Workday { end:Option<String>, warn_minutes:Option<u32> },
 Snapshot, Start, Snooze { minutes:u32 }, Skip,
 /// `all` also stops previews/fixture video, releases the camera and hides every window.
 Cancel { #[serde(default)] all:bool },
 Finish { weight:f64, #[serde(default)] honor:bool },
 Settings { minutes:Option<u32>, lock_mode:Option<bool> },
 Routine { json:Option<String> }, Show, Hide,
 Mode { mode:String },
 Camera { operation:String, settings:Option<serde_json::Value> },
 /// `state` picks a detection state for `start`; `reps`/`seconds`/`weight` program the set.
 Debug { operation:String, exercise:Option<String>, value:Option<f64>, video:Option<String>,
  #[serde(default)] state:Option<String>, #[serde(default)] reps:Option<u32>, #[serde(default)] seconds:Option<f64>,
  #[serde(default)] weight:Option<f64>, #[serde(default)] all:bool },
 Display { window:String, visible:Option<bool>, fullscreen:Option<bool>, monitor:Option<usize> },
}
pub const DEBUG_OPERATIONS:&[&str]=&["start","stop","step","done","next","exercises","videos","video","video-stop","history"];
/// live: real camera. counting: camera off, drive progress with step/done.
/// no-pose / vision-down: camera off, UI shows that detection state.
pub const DEBUG_STATES:&[&str]=&["live","counting","no-pose","vision-down"];
impl Command {
 pub fn validate(&self)->Result<(),String>{
  match self {
   Self::Workday{end,warn_minutes}=>{if let Some(s)=end{crate::workday::parse_time(s)?;}if warn_minutes.is_some_and(|m|!(1..=240).contains(&m)){return Err("Warning lead must be 1–240 minutes".into());}Ok(())},
   Self::Mode{mode} if !["debug","workout"].contains(&mode.as_str())=>Err("Mode must be debug or workout".into()),
   Self::Camera{operation,..} if !["list","settings","set","preview","stop","status"].contains(&operation.as_str())=>Err("Unknown camera operation".into()),
   Self::Debug{operation,value,..} if !DEBUG_OPERATIONS.contains(&operation.as_str()) || value.is_some_and(|v|!v.is_finite()||!(0.0..=100000.0).contains(&v))=>Err("Invalid debug operation or progress".into()),
   Self::Debug{state:Some(s),..} if !DEBUG_STATES.contains(&s.as_str())=>Err(format!("State must be one of: {}",DEBUG_STATES.join(", "))),
   Self::Debug{reps:Some(n),..} if !(1..=1000).contains(n)=>Err("Reps must be 1–1000".into()),
   Self::Debug{seconds:Some(n),..} if !n.is_finite()||!(1.0..=3600.0).contains(n)=>Err("Seconds must be 1–3600".into()),
   Self::Debug{weight:Some(n),..} if !n.is_finite()||!(0.0..=100000.0).contains(n)=>Err("Weight must be between 0 and 100000 lb".into()),
   Self::Debug{reps:Some(_),seconds:Some(_),..}=>Err("Program either --reps or --seconds, not both".into()),
   Self::Display{window,..} if !["main","gym"].contains(&window.as_str())=>Err("Window must be main or gym".into()),
   Self::Snooze{minutes} if ![5,15,30].contains(minutes)=>Err("Choose snooze of 5, 15, or 30 minutes".into()),
   Self::Settings{minutes:Some(m),..} if !(1..=240).contains(m)=>Err("Interval must be 1–240 minutes".into()),
   Self::Finish{weight,..} if !weight.is_finite() || !(0.0..=100000.0).contains(weight)=>Err("Weight must be between 0 and 100000 lb".into()),
   Self::Routine{json:Some(s)} if s.len()>60000=>Err("Routine is too large".into()),
   _=>Ok(())
  }
 }
}
pub fn parse(line:&str)->Result<Command,String>{
 let value:serde_json::Value=serde_json::from_str(line).map_err(|_|"Invalid command JSON")?;
 let command:Command=serde_json::from_value(value.clone()).map_err(|_|"Invalid CLI command")?;
 let canonical=serde_json::to_value(&command).map_err(|_|"Invalid CLI command")?;
 if value.as_object().ok_or("Command must be an object")?.keys().any(|k|canonical.get(k).is_none()){return Err("Unknown command field".into());}
 command.validate()?;Ok(command)
}
pub fn send(home:&Path, command:&Command)->Result<serde_json::Value,String>{
 command.validate()?;
 #[cfg(unix)] {
  use std::os::unix::net::UnixStream;
  let mut socket=UnixStream::connect(home.join("control.sock")).map_err(|_|"RFP is not running. Run: reps service start")?;
  socket.set_read_timeout(Some(Duration::from_secs(15))).map_err(|e|e.to_string())?;
  socket.set_write_timeout(Some(Duration::from_secs(5))).map_err(|e|e.to_string())?;
  let mut bytes=serde_json::to_vec(command).map_err(|e|e.to_string())?;bytes.push(b'\n');
  socket.write_all(&bytes).map_err(|e|e.to_string())?;
  let mut response=String::new();socket.take(131073).read_to_string(&mut response).map_err(|e|e.to_string())?;
  if response.len()>131072{return Err("Control response too large".into());}
  let value:serde_json::Value=serde_json::from_str(&response).map_err(|_|"Invalid desktop response")?;
  if let Some(error)=value.get("error").and_then(|e|e.as_str()){return Err(error.into());}
  Ok(value)
 }
 #[cfg(not(unix))] { let _=home;Err("Local desktop control currently requires Unix".into()) }
}
#[cfg(test)] mod tests {
 use super::*;
 #[test] fn rejects_invalid_controls(){
  assert!(Command::Finish{weight:f64::NAN,honor:false}.validate().is_err());
  assert!(Command::Finish{weight:-1.,honor:true}.validate().is_err());
  assert!(Command::Snooze{minutes:0}.validate().is_err());
  assert!(Command::Settings{minutes:Some(241),lock_mode:None}.validate().is_err());
  assert!(parse(r#"{"action":"start","unexpected":true}"#).is_err());
 }
 #[test] fn roundtrips_commands(){let c=Command::Finish{weight:10.,honor:true};let s=serde_json::to_string(&c).unwrap();let d:Command=serde_json::from_str(&s).unwrap();d.validate().unwrap();}
}

#[cfg(test)] mod dogfood_tests {
 use super::*;
 #[test] fn debug_and_camera_inputs_are_validated(){
  for input in [r#"{"action":"mode","mode":"production"}"#,r#"{"action":"debug","operation":"step","exercise":null,"value":-1,"video":null}"#,r#"{"action":"camera","operation":"delete","settings":null}"#,r#"{"action":"display","window":"other","visible":null,"fullscreen":null,"monitor":null}"#]{assert!(parse(input).is_err(),"{input}");}
  assert!(parse(r#"{"action":"camera","operation":"preview","settings":null}"#).is_ok());
  assert!(parse(r#"{"action":"mode","mode":"debug"}"#).is_ok());
 }
 #[test] fn debug_states_programs_and_close_all_are_validated(){
  for input in [r#"{"action":"debug","operation":"start","exercise":"squat","value":null,"video":null,"state":"asleep"}"#,
   r#"{"action":"debug","operation":"start","exercise":"squat","value":null,"video":null,"reps":0}"#,
   r#"{"action":"debug","operation":"start","exercise":"squat","value":null,"video":null,"reps":3,"seconds":30}"#,
   r#"{"action":"debug","operation":"jump","exercise":null,"value":null,"video":null}"#]{assert!(parse(input).is_err(),"{input}");}
  assert!(parse(r#"{"action":"debug","operation":"start","exercise":"squat","value":null,"video":null,"state":"no-pose","reps":3,"weight":20}"#).is_ok());
  assert!(parse(r#"{"action":"debug","operation":"next","exercise":null,"value":null,"video":null}"#).is_ok());
  assert!(parse(r#"{"action":"debug","operation":"stop","exercise":null,"value":null,"video":null,"all":true}"#).is_ok());
  assert!(parse(r#"{"action":"cancel","all":true}"#).is_ok());
  assert!(parse(r#"{"action":"cancel"}"#).is_ok(), "old CLIs send cancel without all");
 }
}
