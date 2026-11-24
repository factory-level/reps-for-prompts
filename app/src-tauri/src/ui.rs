//! The desktop's commands and the shared app state every window renders
//! (V2 spec §10.1). Windows are views: they send commands and draw the
//! `snapshot` + `app-state` events; Lifecycle owns the session.
use std::sync::Mutex;

use engine::clock::{Clock, SystemClock};
use engine::types::{Phase, Snapshot};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use crate::prompt::{Act, Prompt};
use crate::training::{self, TrainingPlan};
use crate::{daily, windows, Lifecycle, Runtime, ROUTINE_JSON};

#[derive(Default)]
pub struct Ui {
    /// Start Workout was pressed: the gym is up, waiting for Start Session.
    pub setup: bool,
    pub prompt: Prompt,
    /// The scheduled start reminder showing (index into startTimes).
    pub start_slot: Option<usize>,
    last_minute: i64,
    last_state: String,
}
pub type SharedUi = Mutex<Ui>;

fn ui(app: &AppHandle) -> std::sync::MutexGuard<'_, Ui> { app.state::<SharedUi>().inner().lock().unwrap() }

fn home(app: &AppHandle) -> std::path::PathBuf { app.state::<Runtime>().session_home.clone() }

/// What the session asks of the work desktop right now.
fn prompt_kind(snap: &Snapshot, ui: &Ui) -> Option<&'static str> {
    if snap.paused { return None; }
    match snap.phase {
        Phase::ExerciseRequired => Some("due"),
        Phase::WorkoutActive => Some("exercising"),
        Phase::WeightConfirmation => Some("confirm"),
        Phase::Idle if !ui.setup && ui.start_slot.is_some() => Some("start"),
        _ => None,
    }
}

/// The state every window shows besides the snapshot.
pub fn state(app: &AppHandle) -> Value {
    let today = SystemClock.today();
    let home = home(app);
    let plan = training::load(&home, ROUTINE_JSON);
    let life = app.state::<Lifecycle>();
    let core = life.core();
    let snap = core.session.snapshot(SystemClock.now());
    let start = training::week_start(&today);
    let workouts = core.store.workouts_between(&start, &training::add_days(&start, 6)).unwrap_or_default();
    let done = workouts.iter().filter(|w| w["status"] == "completed").count();
    let next = training::next(&plan).map(|(i, _, r)| json!({"name": r.name, "index": i + 1, "count": plan.cycle.len()}));
    let exercise = snap.prescription.as_ref().map(|rx| rx.exercise.clone());
    let active_routine = core.active.as_ref().and_then(|a| plan.routines.iter().find(|r| r.id == a.routine_id));
    let item = exercise.as_ref().and_then(|e| active_routine.and_then(|r| r.items.iter().find(|i| &i.exercise == e)));
    let state = json!({
        "debug": app.state::<Runtime>().is_debug(),
        "routine": core.active.as_ref().map(|a| json!({"name": a.routine_name, "index": a.slot_index + 1, "count": a.slot_count})),
        "next": next,
        "week": {"done": done, "target": plan.weekly_target},
        "lastWeight": exercise.as_deref().and_then(|e| core.store.last_weight(e)),
        "load": item.map(|i| i.load.clone()).unwrap_or_else(|| "total".into()),
        "lastCompleted": core.last_completed,
        "codingMinutes": plan.coding_minutes,
        "snoozeMinutes": plan.snooze_minutes,
    });
    drop(core);
    let ui = ui(app);
    let mut state = state;
    state["setup"] = json!(ui.setup);
    state["prompt"] = json!(ui.prompt.kind);
    state["startTime"] = json!(ui.start_slot.and_then(|i| plan.start_times.get(i)));
    state
}

/// Broadcast the app state when it changed.
pub fn publish(app: &AppHandle) {
    let state = state(app);
    let text = state.to_string();
    {
        let mut ui = ui(app);
        if ui.last_state == text { return; }
        ui.last_state = text;
    }
    let _ = app.emit("app-state", &state);
}

/// Once a second: schedule start reminders and drive the prompt window.
pub fn tick(app: &AppHandle, snap: &Snapshot, now: f64, _snoozed: bool) {
    if app.state::<Runtime>().is_debug() { return; }
    // Scheduled start reminders, checked once a minute while idle.
    let local = chrono::Local::now();
    let minute = local.timestamp() / 60;
    let check = { let mut ui = ui(app); let due = ui.last_minute != minute; ui.last_minute = minute; due };
    if check && snap.phase == Phase::Idle {
        use chrono::{Datelike, Timelike};
        let today = SystemClock.today();
        let plan = training::load(&home(app), ROUTINE_JSON);
        let life = app.state::<Lifecycle>();
        let core = life.core();
        let key = |i: usize| format!("start_prompt:{today}:{i}");
        let slot = training::start_slot_due(&plan, local.weekday().number_from_monday(), local.hour() * 60 + local.minute(),
            core.store.started_on(&today), |i| !core.store.setting(&key(i), "").is_empty());
        if let Some(i) = slot {
            let _ = core.store.set_setting(&key(i), "delivered");
            drop(core);
            ui(app).start_slot = Some(i);
            daily::notify("Time for your workout.".into());
        }
    }
    let act = {
        let mut ui = ui(app);
        if snap.phase != Phase::Idle { ui.start_slot = None; }
        let kind = prompt_kind(snap, &ui);
        ui.prompt.update(kind, now)
    };
    match act {
        Act::Show => windows::show_prompt(app),
        Act::Hide => windows::hide_prompt(app),
        Act::Keep => {}
    }
    crate::tray::status(app, snap);
    publish(app);
}

/// Close / Not now on the prompt.
pub fn dismiss(app: &AppHandle) {
    {
        let mut ui = ui(app);
        ui.prompt.dismiss(SystemClock.now());
        if ui.prompt.kind == Some("start") { ui.start_slot = None; }
    }
    windows::hide_prompt(app);
    publish(app);
}

/// Start Workout: put the gym up and wait for Start Session. With a session
/// already under way this is Resume: just show the monitor.
pub fn prepare(app: &AppHandle) {
    let idle = app.state::<Lifecycle>().snapshot().phase == Phase::Idle;
    if idle {
        app.state::<Lifecycle>().core().last_completed = None;
        let mut ui = ui(app);
        ui.setup = true;
        ui.start_slot = None;
    }
    windows::put_gym(app);
    publish(app);
}

/// Start the session (the CLI's `rfp start` while idle does both steps).
pub fn begin_session(app: &AppHandle) -> Result<(), String> {
    windows::put_gym(app);
    app.state::<Lifecycle>().start_session()?;
    { let mut ui = ui(app); ui.setup = false; ui.start_slot = None; }
    publish(app);
    Ok(())
}

fn done(app: &AppHandle, result: Result<Snapshot, String>) -> Result<(), String> {
    result?;
    publish(app);
    Ok(())
}

// ---- Tauri commands ---------------------------------------------------------

#[tauri::command]
pub fn get_app_state(app: AppHandle) -> Value { state(&app) }

#[tauri::command]
pub fn get_plan(app: AppHandle) -> TrainingPlan { training::load(&home(&app), ROUTINE_JSON) }

/// Saving a plan never starts a workout; a session under way keeps the
/// routine it started with.
#[tauri::command]
pub fn save_plan(app: AppHandle, plan: TrainingPlan) -> Result<TrainingPlan, String> {
    training::save(&home(&app), &plan)?;
    publish(&app);
    Ok(plan)
}

/// Exercises the routine builder offers: every vision-supported movement,
/// plus custom ones already in the plan (manual or timer only).
#[tauri::command]
pub fn catalog(app: AppHandle) -> Result<Vec<Value>, String> {
    let mut out: Vec<Value> = crate::detector::debug_exercise_options()?.into_iter().map(|rx| json!({
        "id": rx.exercise, "kind": if rx.target_seconds > 0. { "timed" } else { "lift" }, "seconds": rx.target_seconds, "vision": true,
    })).collect();
    for r in training::load(&home(&app), ROUTINE_JSON).routines {
        for i in r.items {
            if !out.iter().any(|e| e["id"] == i.exercise) {
                out.push(json!({"id": i.exercise, "kind": i.kind, "seconds": i.seconds, "vision": false}));
            }
        }
    }
    Ok(out)
}

#[tauri::command]
pub fn prepare_workout(app: AppHandle) { prepare(&app) }

#[tauri::command]
pub fn cancel_setup(app: AppHandle) { ui(&app).setup = false; publish(&app); }

#[tauri::command]
pub fn start_session(app: AppHandle) -> Result<(), String> { begin_session(&app) }

#[tauri::command]
pub fn start_set(app: AppHandle) -> Result<(), String> {
    daily::reminder_action(app.clone(), "start".into(), None)?;
    publish(&app);
    Ok(())
}

#[tauri::command]
pub fn manual_finish(app: AppHandle) -> Result<(), String> { let r = app.state::<Lifecycle>().manual_finish(); done(&app, r) }

#[tauri::command]
pub fn confirm_set(app: AppHandle, weight: f64, reps: Option<u32>) -> Result<(), String> {
    let r = app.state::<Lifecycle>().confirm_set(weight, reps); done(&app, r)
}

#[tauri::command]
pub fn snooze(app: AppHandle) -> Result<(), String> {
    let minutes = training::load(&home(&app), ROUTINE_JSON).snooze_minutes;
    daily::reminder_action(app.clone(), "snooze".into(), Some(minutes))?;
    publish(&app);
    Ok(())
}

#[tauri::command]
pub fn dismiss_prompt(app: AppHandle) { dismiss(&app) }

#[tauri::command]
pub fn pause(app: AppHandle) -> Result<(), String> { let r = app.state::<Lifecycle>().pause(); done(&app, r) }

#[tauri::command]
pub fn resume(app: AppHandle) -> Result<(), String> { let r = app.state::<Lifecycle>().resume(); done(&app, r) }

#[tauri::command]
pub fn end_workout(app: AppHandle) { app.state::<Lifecycle>().end_workout(); ui(&app).setup = false; publish(&app); }

/// Sessions and confirmed sets between two local dates (H01).
#[tauri::command]
pub fn history(app: AppHandle, from: String, to: String) -> Result<Value, String> {
    let life = app.state::<Lifecycle>();
    let core = life.core();
    Ok(json!({
        "workouts": core.store.workouts_between(&from, &to).map_err(|e| e.to_string())?,
        "sets": core.store.sets(Some(&from), Some(&to), None).map_err(|e| e.to_string())?,
    }))
}

/// Every confirmed set of one exercise, plus the exercises with history (H02).
#[tauri::command]
pub fn exercise_history(app: AppHandle, exercise: Option<String>) -> Result<Value, String> {
    let life = app.state::<Lifecycle>();
    let core = life.core();
    let all = core.store.sets(None, None, None).map_err(|e| e.to_string())?;
    let mut names: Vec<String> = all.iter().filter_map(|s| s["exercise"].as_str().map(str::to_string)).collect();
    names.sort(); names.dedup();
    let sets = match exercise { Some(e) => core.store.sets(None, None, Some(&e)).map_err(|e| e.to_string())?, None => vec![] };
    Ok(json!({"exercises": names, "sets": sets}))
}

/// One week's contribution against the commitment, and a year of activity (H03).
#[tauri::command]
pub fn week(app: AppHandle, start: Option<String>) -> Result<Value, String> {
    let today = SystemClock.today();
    let start = training::week_start(&start.unwrap_or_else(|| today.clone()));
    let end = training::add_days(&start, 6);
    let plan = training::load(&home(&app), ROUTINE_JSON);
    let life = app.state::<Lifecycle>();
    let core = life.core();
    let workouts = core.store.workouts_between(&start, &end).map_err(|e| e.to_string())?;
    let sets = core.store.sets(Some(&start), Some(&end), None).map_err(|e| e.to_string())?;
    let year = core.store.sets(Some(&training::add_days(&today, -364)), Some(&today), None).map_err(|e| e.to_string())?;
    let mut heat = std::collections::BTreeMap::<String, u32>::new();
    for s in &year { *heat.entry(s["date"].as_str().unwrap_or("").to_string()).or_default() += 1; }
    let days: Vec<Value> = (0..7).map(|d| {
        let date = training::add_days(&start, d);
        let on: Vec<&Value> = workouts.iter().filter(|w| w["date"] == date.as_str()).collect();
        let status = if on.iter().any(|w| w["status"] == "completed") { "completed" }
            else if on.iter().any(|w| w["status"] == "partial") { "partial" }
            else if on.iter().any(|w| w["status"] == "active") { "active" }
            else if plan.weekdays.contains(&(d as u32 + 1)) { "planned" } else { "rest" };
        json!({"date": date, "status": status})
    }).collect();
    Ok(json!({
        // ponytail: the target is today's plan; spec §8.4 wants per-week commitment revisions.
        "start": start, "end": end, "target": plan.weekly_target,
        "done": workouts.iter().filter(|w| w["status"] == "completed").count(),
        "partial": workouts.iter().filter(|w| w["status"] == "partial").count(),
        "sets": sets.len(), "reps": sets.iter().map(|s| s["reps"].as_i64().unwrap_or(0)).sum::<i64>(),
        "days": days, "heat": heat, "next": training::next(&plan).map(|(_, _, r)| r.name.clone()),
    }))
}

#[tauri::command]
pub fn show_gym(app: AppHandle, reset: Option<bool>) {
    if reset == Some(true) { windows::reset_gym(&app); } else { windows::put_gym(&app); }
}

/// Open the launcher on a view: "home", "plan", "history", "settings".
#[tauri::command]
pub fn show_main(app: AppHandle, view: Option<String>) { open(&app, view.as_deref().unwrap_or("home")) }

pub fn open(app: &AppHandle, view: &str) {
    windows::show_main(app);
    let _ = app.emit_to("main", "navigate", view);
}

/// Only the gym window goes fullscreen (spec §10.2).
#[tauri::command]
pub fn toggle_fullscreen(app: AppHandle, window: tauri::WebviewWindow) -> Result<bool, String> {
    if window.label() != "gym" { return Err("Only the workout monitor goes fullscreen".into()); }
    Ok(windows::toggle_gym_fullscreen(&app))
}
