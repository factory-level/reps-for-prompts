use std::sync::{Mutex, atomic::Ordering};
use engine::{activity::{AgentPresence, detect}, clock::{Clock, SystemClock}, session::Release, types::Phase};
use serde::Serialize;
use tauri::{AppHandle, Manager};
use crate::{SharedCore, Runtime};

#[derive(Default)]
pub struct Daily { pub agents: AgentPresence, pub snooze_until: f64, pub checked_at: f64 }
pub type SharedDaily = Mutex<Daily>;
#[derive(Serialize)]
#[serde(rename_all="camelCase")]
pub struct Status { workday_end:String, warn_minutes:u32, last_day_warning:String, checked_at: f64, agents: AgentPresence, snooze_until: f64, work_minutes: f64, lock_mode: bool, pub sync: String }

pub fn daily_status(app: AppHandle) -> Status {
    let core = app.state::<SharedCore>(); let core = core.lock().unwrap();
    let daily = app.state::<SharedDaily>(); let daily = daily.lock().unwrap();
    Status { workday_end:core.store.setting("workday_end","18:00"), warn_minutes:core.store.setting("workday_warn_minutes","60").parse().unwrap_or(60), last_day_warning:core.store.setting("last_day_warning",""), checked_at: daily.checked_at, agents: daily.agents.clone(), snooze_until: daily.snooze_until,
        work_minutes: core.store.setting("work_minutes", "25").parse().unwrap_or(25.),
        lock_mode: app.state::<Runtime>().lock_mode.load(Ordering::SeqCst),
        sync: core.store.setting("sync_status", "Not configured") }
}

pub fn reminder_action(app: AppHandle, action: String, minutes: Option<u32>) -> Result<(), String> {
    if app.state::<Runtime>().is_debug() { return Err("Switch to Workout mode first".into()); }
    let now = SystemClock.now();
    let state = app.state::<SharedCore>(); let mut core = state.lock().unwrap();
    let phase = core.session.snapshot(now).phase;
    if action == "start" {
        let facts = core.session.start_workout(&SystemClock.today());
        if facts.is_empty() {
            return Err(if phase == Phase::Coding { "Today's routine is complete; nothing to start" } else { "A workout is already in progress" }.into());
        }
        // Before the detector is enabled: the enable path reads the session id.
        crate::hub::publish_facts(&facts);
        core.store.set_setting("snooze_until", "0").map_err(|e| e.to_string())?;
        app.state::<SharedDaily>().lock().unwrap().snooze_until = 0.;
        let snap = crate::persist_and_snapshot(&mut core); drop(core);
        crate::enable_metric_for(&app, &snap); crate::emit_snapshot(&app, &snap);
    } else if action == "snooze" || action == "skip" {
        if !matches!(phase, Phase::Coding | Phase::ExerciseRequired) { return Err("Finish or stop the current workout first".into()); }
        let until = if action == "snooze" {
            let m = minutes.unwrap_or(15); if ![5,15,30].contains(&m) { return Err("Choose 5, 15, or 30 minutes".into()); }
            now + m as f64 * 60.
        } else { 0. };
        core.store.set_setting("snooze_until", &until.to_string()).map_err(|e| e.to_string())?;
        app.state::<SharedDaily>().lock().unwrap().snooze_until = until;
        crate::hub::publish_facts(&core.session.release(if action == "snooze" { Release::Snooze } else { Release::Skip }, now));
        // Snooze means remind at expiry, rather than expiry plus a full interval.
        if until > 0. { core.session.configure_timer(0., now); }
        else { let interval = core.store.setting("work_minutes", "25").parse().unwrap_or(25.); core.session.configure_timer(interval, now); }
        let snap = crate::persist_and_snapshot(&mut core); drop(core);
        crate::hub::disable_metric_async(&app); crate::windows::release(&app); crate::emit_snapshot(&app, &snap);
    } else { return Err("Unknown reminder action".into()); }
    Ok(())
}

pub fn save_daily_settings(app: AppHandle, minutes: u32, lock_mode: bool) -> Result<(), String> {
    if !(1..=240).contains(&minutes) { return Err("Interval must be 1–240 minutes".into()); }
    let state = app.state::<SharedCore>(); let mut core = state.lock().unwrap();
    if core.session.snapshot(SystemClock.now()).phase != Phase::Coding { return Err("Change settings between workouts".into()); }
    core.store.set_setting("work_minutes", &minutes.to_string()).map_err(|e| e.to_string())?;
    core.store.set_setting("lock_mode", if lock_mode { "1" } else { "0" }).map_err(|e| e.to_string())?;
    core.session.configure_timer(minutes as f64, SystemClock.now());
    app.state::<Runtime>().lock_mode.store(lock_mode, Ordering::SeqCst);
    drop(core); if !lock_mode { crate::windows::release(&app); } Ok(())
}

/// Flock is held for the process lifetime; stale files cannot block restarts.
pub fn instance_lock(home: &std::path::Path) -> Result<std::fs::File, String> {
    use std::os::fd::AsRawFd;
    std::fs::create_dir_all(home).map_err(|e| e.to_string())?;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(home.join("desktop.lock")).map_err(|e| e.to_string())?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 { return Err("Reps is already running; open it from the tray".into()); }
    Ok(file)
}

pub fn refresh(daily: &mut Daily) {
    let agents = detect();
    if daily.checked_at == 0. || agents.codex != daily.agents.codex || agents.claude != daily.agents.claude {
        eprintln!("[RFP WATCHER] Codex={} Claude Code={} (same-user process check; rfp inspect for countdown)", agents.codex, agents.claude);
    }
    daily.agents = agents;
    daily.checked_at = SystemClock.now();
}

pub fn routine_settings(app:AppHandle) -> Result<String,String> {
    let path=app.state::<Runtime>().session_home.join("routine.json");
    match std::fs::read_to_string(path){Ok(s)=>Ok(s),Err(e) if e.kind()==std::io::ErrorKind::NotFound=>Ok(crate::ROUTINE_JSON.into()),Err(e)=>Err(e.to_string())}
}
pub fn save_routine(app:AppHandle,json:String)->Result<(),String>{
    let state=app.state::<SharedCore>();let mut core=state.lock().unwrap();
    if core.session.snapshot(SystemClock.now()).phase!=Phase::Coding{return Err("Change routine between workouts".into());}
    let mut plan=engine::plan::DailyPlan::from_routine_json(&json,&SystemClock.today())?;
    if let Some(current)=core.session.plan(){
        let d=current.to_day_plan();
        plan.restore(&SystemClock.today(),&d.items.iter().map(|i|(i.name.clone(),i.done)).collect::<Vec<_>>());
    }
    let home=&app.state::<Runtime>().session_home;
    std::fs::write(home.join("routine.json.tmp"),&json).map_err(|e|e.to_string())?;
    std::fs::rename(home.join("routine.json.tmp"),home.join("routine.json")).map_err(|e|e.to_string())?;
    core.session.set_plan(plan);Ok(())
}

#[cfg(test)] mod tests {
 use super::*;
 #[test] fn only_one_desktop_per_home() {
  let home=std::env::temp_dir().join(format!("rfp-lock-{}",uuid::Uuid::new_v4()));
  let first=instance_lock(&home).unwrap();assert!(instance_lock(&home).is_err());
  drop(first);assert!(instance_lock(&home).is_ok());std::fs::remove_dir_all(home).unwrap();
 }
}

/// Notifications have no actions; all workout controls stay in the terminal.
pub fn notify(message:String){
    eprintln!("[RFP REMINDER] {message}");
    std::thread::spawn(move||{
        match std::process::Command::new("timeout").args(["5s","notify-send","--app-name=RFP","--expire-time=15000","RFP: reps for prompts",&message]).status(){
            Ok(status) if status.success()=>{},
            other=>eprintln!("[RFP REMINDER] Desktop notification unavailable: {other:?}; see the RFP display and rfp inspect"),
        }
    });
}
