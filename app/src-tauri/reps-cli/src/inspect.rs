//! Report the daemon's process sample, never a separate CLI-side scan.
use serde_json::{json, Value};
use crate::control::{send, Command};
pub fn report(runtime: Result<Value,String>, now:f64)->Value {
 let s=match runtime {Ok(s)=>s,Err(error)=>return json!({"schemaVersion":1,"source":"local-daemon","state":"unreachable","armed":false,"statement":"RFP is not reachable. Run rfp service start, then inspect again.","error":error})};
 let checked=s["settings"]["checkedAt"].as_f64().unwrap_or(0.);
 let age=now-checked;let fresh=checked>0. && (0. ..=10.).contains(&age);
 let agents=&s["settings"]["agents"];
 let detected=agents["codex"]==true||agents["claude"]==true;
 let phase=s["snapshot"]["phase"].as_str().unwrap_or("UNKNOWN");
 let remaining=s["snapshot"]["remainingSeconds"].as_f64().unwrap_or(0.).max(0.).ceil() as u64;
 let (state,statement)=if !fresh {("stale","No fresh watcher check. Run rfp service logs.".into())}
 else if s["mode"]!="workout" {("debug","Debug mode: automatic reminders disabled. Run rfp mode workout.".into())}
 else if phase=="EXERCISE_REQUIRED" {("reminder","Workout due. WORKOUT display requested; rfp show reveals it, rfp start begins the set.".into())}
 else if phase!="CODING" {("in-progress",format!("Workout phase: {phase}. Run rfp status --json for details."))}
 else if s["snapshot"]["paused"]==true {("paused","Session paused: the countdown is frozen. Resume from the RFP window or tray.".into())}
 else if s["snapshot"]["day"]["complete"]==true {("complete","Today's routine is complete; no further reminders today.".into())}
 else if s["display"]["previewOnly"]==true {("preview","Countdown paused for camera preview. Run rfp camera stop.".into())}
 else if s["settings"]["snoozeUntil"].as_f64().unwrap_or(0.)>now {("snoozed",format!("Reminder snoozed for {} more seconds.",(s["settings"]["snoozeUntil"].as_f64().unwrap()-now).ceil() as u64))}
 else if !detected {("waiting","Countdown paused: waiting for Codex or Claude Code to open.".into())}
 else {("counting",format!("Coding agent detected. WORKOUT screen will appear in {}m {:02}s if an agent stays open.",remaining/60,remaining%60))};
 json!({"schemaVersion":1,"source":"local-daemon","pid":s["pid"],"state":state,"armed":state=="counting","statement":statement,"agents":agents,"checkedAt":checked,"sampleAgeSeconds":age.max(0.),"fresh":fresh,"pollIntervalSeconds":5,"mode":s["mode"],"phase":phase,"remainingSeconds":remaining,"detectionMethod":"Same-user process presence; no prompts, keystrokes or foreground activity are read.","workdayEnd":s["settings"]["workdayEnd"],"warnMinutes":s["settings"]["warnMinutes"],"lastDayWarning":s["settings"]["lastDayWarning"],"notice":s["display"]["notice"],"cameraPolicy":"Automatic reminders show the screen. Run rfp start to start the workout camera."})
}
pub fn run(args:&[String])->Result<(),String>{
 if args.iter().any(|s|!["--watch","--json"].contains(&s.as_str())) {return Err("Use: rfp inspect [--watch] [--json]".into());}
 loop {
  let v=report(send(&engine::activity::app_home(),&Command::Snapshot),chrono::Utc::now().timestamp_millis() as f64/1000.);
  if args.iter().any(|s|s=="--json"){println!("{v}");}
  else {
   println!("RFP inspection — {}",chrono::Local::now().format("%H:%M:%S"));
   if v["pid"].is_number(){println!("Background app PID {} | Codex: {} | Claude Code: {} | check age: {:.1}s",v["pid"],v["agents"]["codex"],v["agents"]["claude"],v["sampleAgeSeconds"].as_f64().unwrap_or(0.));}
   println!("{}",v["statement"].as_str().unwrap());
   if let Some(end)=v["workdayEnd"].as_str(){println!("Workday ends {end} local time; warning {} minutes before, at most once per day if sets remain.",v["warnMinutes"]);}
   if let Some(notice)=v["notice"].as_str(){println!("{notice}");}
   for key in ["cameraPolicy","detectionMethod"] {if let Some(s)=v[key].as_str(){println!("{s}");}}
  }
  if !args.iter().any(|s|s=="--watch"){return Ok(());}
  std::thread::sleep(std::time::Duration::from_secs(2));
 }
}
#[cfg(test)] mod tests {
 use super::*;
 fn sample()->Value{json!({"pid":123,"mode":"workout","settings":{"checkedAt":100.,"agents":{"codex":true,"claude":false}},"snapshot":{"phase":"CODING","remainingSeconds":61.},"display":{}})}
 #[test] fn fresh_daemon_arms(){let v=report(Ok(sample()),102.);assert_eq!(v["state"],"counting");assert_eq!(v["armed"],true);}
 #[test] fn offline_or_stale_never_arms(){assert_eq!(report(Ok(sample()),111.)["state"],"stale");assert_eq!(report(Err("offline".into()),102.)["armed"],false);}
 #[test] fn explains_pauses(){for(path,value,expected)in [(vec!["mode"],json!("debug"),"debug"),(vec!["settings","agents","codex"],json!(false),"waiting"),(vec!["settings","snoozeUntil"],json!(200.),"snoozed"),(vec!["display","previewOnly"],json!(true),"preview"),(vec!["snapshot","day","complete"],json!(true),"complete"),(vec!["snapshot","phase"],json!("EXERCISE_REQUIRED"),"reminder"),(vec!["snapshot","paused"],json!(true),"paused")]{let mut s=sample();let mut field=&mut s;for key in path{field=&mut field[key];}*field=value;let v=report(Ok(s),102.);assert_eq!(v["state"],expected);assert_eq!(v["armed"],false);}}
}
