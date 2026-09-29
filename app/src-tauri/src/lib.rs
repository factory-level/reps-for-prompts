mod control;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use engine::clock::{Clock, SystemClock};
use engine::plan::DailyPlan;
use engine::session::Session;
use engine::store::{default_continuous_pool, Store};
use engine::timer::CodingTimer;
use engine::types::{ExerciseKind, Phase, Prescription, Progress, Snapshot};
use engine::workout::WorkoutEngine;
use serde::{Deserialize, Serialize};

/// Default daily routine, overridden by routine.json in the app data directory.
const ROUTINE_JSON: &str = include_str!("../resources/routine.json");

fn load_daily_plan(dir: &Path, today: &str) -> Result<DailyPlan, String> {
    let path = dir.join("routine.json");
    let json = match std::fs::read_to_string(&path) {
        Ok(json) => json,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ROUTINE_JSON.to_string(),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    DailyPlan::from_routine_json(&json, today)
}

/// Persisted per-item completion so the day survives app restarts.
#[derive(Serialize, Deserialize, Default)]
struct PlanState {
    date: String,
    done: Vec<(String, u32)>,
}
use tauri::{AppHandle, Emitter, Manager, RunEvent, State};

mod hub;
mod windows;
mod runtime;
mod daily;
use runtime::{AppMode, Runtime};

/// Installer diagnostic: use the same Tauri resource lookup and supervisor as
/// the desktop, with disposable hub state and ephemeral ports. No window, lock,
/// routine, or camera is started. Reuses/provisions the normal Python environment.
pub fn check_runtime() -> Result<serde_json::Value, String> {
    use hub_client::{HubSupervisor, HubSupervisorConfig, VisionHub};
    let context: tauri::Context<tauri::Wry> = tauri::generate_context!();
    let resources = tauri::utils::platform::resource_dir(context.package_info(), &tauri::utils::Env::default())
        .map_err(|e| e.to_string())?;
    let mut config = HubSupervisorConfig::bundled(&resources, &resources.join("reps-vision"), &dirs_next_data_dir());
    let data = std::env::temp_dir().join(format!("reps-runtime-check-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&data).map_err(|e| e.to_string())?;
    config.env.extend([
        ("PORT".into(), "0".into()), ("DEBUG_PORT".into(), "0".into()),
        ("HUB_BIND_HOST".into(), "127.0.0.1".into()),
        ("HUB_DATA_DIR".into(), data.display().to_string()),
        ("HUB_CERT_DIR".into(), data.join("no-certs").display().to_string()),
        ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
    ]);
    let result = (|| {
        let mut supervisor = HubSupervisor::start(config).map_err(|e| e.to_string())?;
        let health = supervisor.health().map_err(|e| e.to_string())?;
        if health.vision_host == "down" { return Err("vision host is down".into()); }
        Ok(serde_json::json!({ "status": "ready", "resources": resources,
            "visionHost": health.vision_host, "camera": health.camera,
            "enabledMetrics": health.enabled_metrics }))
    })();
    let _ = std::fs::remove_dir_all(data);
    result
}

pub(crate) struct Core {
    pub(crate) session: Session,
    pub(crate) store: Store,
}

pub(crate) type SharedCore = Mutex<Core>;

fn build_core(dir: &Path) -> Core {
    let store = Store::open(&dir.join("reps.sqlite")).expect("open sqlite");
    // REPS_WORK_MINUTES overrides the coding timer for testing (e.g. 0.15 = a
    // ~9s countdown so you can reach a locked set immediately); otherwise the
    // persisted setting (default 6 min).
    let work_minutes: f64 = std::env::var("REPS_WORK_MINUTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| store.setting("work_minutes", "25").parse().unwrap_or(25.0));
    let capacity: u32 = store.setting("max_weighted_sets", "20").parse().unwrap_or(20);
    let rotation = store.load_rotation().expect("load rotation");
    let mut workout = WorkoutEngine::new(rotation, default_continuous_pool(), capacity);
    let (pointer, used, date) = store.load_pointer_state().expect("load pointer");
    workout.restore(pointer, used, &date);
    let mut session = Session::new(CodingTimer::new(work_minutes * 60.0), workout);

    // Drive prescription from the routine.json daily plan (falls back to the
    // rotation if the routine fails to parse). Restore today's completion.
    let today = SystemClock.today();
    match load_daily_plan(&dir, &today) {
        Ok(mut plan) => {
            if let Ok(ps) = serde_json::from_str::<PlanState>(&store.setting("plan_state", "")) {
                if !ps.date.is_empty() {
                    plan.restore(&ps.date, &ps.done);
                    plan.roll_date(&today); // clears completion if it's a new day
                }
            }
            session.set_plan(plan);
        }
        Err(e) => eprintln!("routine.json failed to load, using rotation: {e}"),
    }

    Core { session, store }
}

fn dirs_next_data_dir() -> std::path::PathBuf { engine::activity::app_home() }

pub(crate) fn persist_and_snapshot(core: &mut Core) -> Snapshot {
    let clock = SystemClock;
    if let Some(rec) = core.session.take_pending_record() {
        match core.store.record_set(&rec) {
            Ok(()) => {}
            Err(e) => eprintln!("failed to record set: {e}"),
        }
    }
    let w = core.session.workout();
    match core
        .store
        .save_pointer_state(w.pointer(), w.capacity_used(), w.capacity_date())
    {
        Ok(()) => {}
        Err(e) => eprintln!("failed to save pointer state: {e}"),
    }
    // Persist the daily-plan completion so today's progress survives a restart.
    if let Some(plan) = core.session.plan() {
        let day=plan.to_day_plan();
        if let Err(error)=core.store.record_routine_day(plan.date(),day.sets_done,day.sets_total){eprintln!("Could not save routine progress: {error}");}
        let ps = PlanState { date: plan.date().to_string(), done: plan.done_state() };
        if let Ok(json) = serde_json::to_string(&ps) {
            if let Err(e) = core.store.set_setting("plan_state", &json) {
                eprintln!("failed to save plan state: {e}");
            }
        }
    }
    core.session.snapshot(clock.now())
}

/// Print a concise, human-readable line of the current session state to the
/// terminal (stdout) whenever it changes — so you can read your state while the
/// app runs without watching the webview.
pub(crate) fn print_state(snap: &Snapshot) {
    static LAST: Mutex<String> = Mutex::new(String::new());
    let rx = snap.prescription.as_ref();
    let ex = rx.map(|r| r.exercise.as_str()).unwrap_or("—");
    let target = rx
        .map(|r| match r.kind {
            ExerciseKind::Continuous => format!("{:.0}s", r.target_seconds),
            ExerciseKind::Rep => format!("{} reps", r.target_reps),
        })
        .unwrap_or_default();
    let value = snap.progress.as_ref().map(|p| p.value.round() as i64).unwrap_or(0);
    let line = match snap.phase {
        Phase::Coding => {
            let rem = snap.remaining_seconds.max(0.0) as u64;
            let next = snap.rotation.get(snap.pointer).map(|s| s.as_str()).unwrap_or("—");
            format!("[STATE] ⌨️  CODING · ~{} min left · next: {}", rem.div_ceil(60), next)
        }
        Phase::ExerciseRequired => format!("[STATE] 🔒 LOCKED · do {} · {}", ex, target),
        Phase::WorkoutActive => format!("[STATE] 🏋️  ACTIVE · {} · {} / {}", ex, value, target),
        Phase::WeightConfirmation => format!(
            "[STATE] ⚖️  CONFIRM WEIGHT · {} · {:.0} lbs",
            ex,
            rx.map(|r| r.default_weight).unwrap_or(0.0)
        ),
        Phase::Unlocked => "[STATE] ✅ UNLOCKED — set complete".to_string(),
    };
    if let Ok(mut last) = LAST.lock() {
        if *last != line {
            println!("{line}");
            *last = line;
        }
    }
}

pub(crate) fn emit_snapshot(app: &AppHandle, snap: &Snapshot) {
    if app.state::<Runtime>().is_stopping() { return; }
    print_state(snap);
    // Durable history: phase changes publish their reps.* events through the
    // hub (fire-and-forget; queues while the hub is down).
    if !app.state::<Runtime>().is_debug() { hub::publish_phase_transition(snap); }
    // Debt owed → the programming monitor is locked (fullscreen, on top).
    windows::apply_lock(app, snap.phase != Phase::Coding);
    let _ = app.emit("snapshot", snap);
}

/// Camera on only while working out: enable the metric for the prescription
/// (fire-and-forget; failure surfaces honor mode).
fn enable_metric_for(app: &AppHandle, snap: &Snapshot) {
    if let Some(rx) = snap.prescription.as_ref() {
        hub::enable_metric_async(app, rx.exercise.clone(), rx.target_reps, rx.target_seconds);
    }
}

#[tauri::command]
fn get_snapshot(state: State<SharedCore>) -> Snapshot {
    let mut core = state.lock().unwrap();
    persist_and_snapshot(&mut core)
}

#[tauri::command]
fn get_app_mode(app: AppHandle) -> AppMode { app.state::<Runtime>().mode }

#[tauri::command]
fn show_gym(app: AppHandle) -> Result<(), String> {
    if app.state::<Runtime>().is_stopping() { return Err("Application is restarting".into()); }
    let gym = app.get_webview_window("gym").ok_or("Gym window unavailable")?;
    gym.show().map_err(|e| e.to_string())
}

#[tauri::command]
fn set_app_mode(app: AppHandle, mode: AppMode) -> Result<(), String> {
    static SWITCH: Mutex<()> = Mutex::new(());
    let _guard = SWITCH.try_lock().map_err(|_| "Mode change already in progress")?;
    let runtime = app.state::<Runtime>();
    if runtime.is_stopping() { return Err("Application is restarting".into()); }
    if runtime.mode == mode { return Ok(()); }
    runtime.save_mode(mode)?;
    runtime.begin_stop();
    windows::release(&app);
    let handle = app.clone();
    std::thread::spawn(move || {
        shutdown_runtime(&handle);
        handle.restart();
    });
    Ok(())
}

fn stop_debug_process(app: &AppHandle) {
    let state = app.state::<SharedDebugProcess>();
    let mut guard = state.lock().unwrap();
    guard.generation += 1;
    if let Some(mut child) = guard.child.take() { let _ = child.kill(); let _ = child.wait(); }
}

fn shutdown_runtime(app: &AppHandle) {
    windows::release(app);
    stop_debug_process(app);
    hub::stop(app);
    app.state::<Runtime>().cleanup();
}

/// Emergency escape remains available in enforced Workout mode.
#[tauri::command]
fn emergency_escape(app: AppHandle, state: State<SharedCore>) -> Snapshot {
    let mut core = state.lock().unwrap();
    core.session.debug_force_coding(SystemClock.now());
    let minutes = core.store.setting("work_minutes", "25").parse().unwrap_or(25.);
    core.session.configure_timer(minutes, SystemClock.now());
    let snap = persist_and_snapshot(&mut core);
    drop(core);
    hub::disable_metric_async(&app);
    emit_snapshot(&app, &snap);
    snap
}

/// F11 in the gym window: flip it between maximized and fullscreen.
#[tauri::command]
fn toggle_gym_fullscreen(app: AppHandle) {
    windows::toggle_gym_fullscreen(&app);
}

/// Honor-mode completion: camera path failed, the user attests the set.
#[tauri::command]
fn honor_complete(app: AppHandle) -> Snapshot {
    hub::honor_complete(&app)
}

fn debug_exercise_options() -> Result<Vec<Prescription>, String> {
    let specs = hub::load_exercise_specs()?;
    let exercises = specs["exercises"].as_object().ok_or("Missing exercise specifications")?;
    Ok(exercises.iter().map(|(name, spec)| {
        let seconds = match spec["activity"].as_str() {
            Some("jumprope") => spec["targetSeconds"].as_f64().unwrap_or(60.0),
            Some("stretch") => spec["holdSeconds"].as_f64().unwrap_or(30.0),
            _ => 0.0,
        };
        Prescription { exercise: name.clone(), kind: if seconds > 0.0 { ExerciseKind::Continuous } else { ExerciseKind::Rep },
            target_reps: if seconds > 0.0 { 0 } else { 10 }, target_seconds: seconds, default_weight: 0.0 }
    }).collect())
}

#[tauri::command]
fn debug_exercises(app: AppHandle) -> Result<Vec<Prescription>, String> {
    app.state::<Runtime>().require_debug()?;
    debug_exercise_options()
}

/// DEBUG: force the session between "coding" and "workout" without waiting out
/// the coding timer, and flip the camera to match. Lets you exercise live
/// detection on demand from the debug toggle.
#[tauri::command]
fn debug_mode(app: AppHandle, state: State<SharedCore>, mode: String, exercise: Option<String>) -> Result<Snapshot, String> {
    debug_program(app, state, mode, exercise, |_| {})
}

/// `debug_mode` with a hook to reprogram the chosen exercise's target (CLI
/// `--reps/--seconds/--weight`).
pub(crate) fn debug_program(app: AppHandle, state: State<SharedCore>, mode: String, exercise: Option<String>, program: impl FnOnce(&mut Prescription)) -> Result<Snapshot, String> {
    app.state::<Runtime>().require_debug()?;
    if mode != "workout" && mode != "coding" { return Err("Unknown test action".into()); }
    let selected = if mode == "workout" {
        exercise.map(|name| debug_exercise_options()?.into_iter().find(|rx| rx.exercise == name)
            .ok_or_else(|| format!("Unknown exercise: {name}"))).transpose()?
            .map(|mut rx| { program(&mut rx); rx })
    } else { None };
    stop_debug_process(&app);
    hub::disable_metric_now(&app);
    let clock = SystemClock;
    let mut core = state.lock().unwrap();
    let workout = mode == "workout";
    if workout {
        if let Some(rx) = selected { core.session.debug_start_exercise(rx); }
        else { core.session.debug_force_workout(clock.now(), &clock.today()); }
    } else {
        core.session.debug_force_coding(clock.now());
    }
    let snap = persist_and_snapshot(&mut core);
    drop(core);
    if workout {
        enable_metric_for(&app, &snap);
    } else {
        hub::disable_metric_async(&app);
    }
    emit_snapshot(&app, &snap);
    Ok(snap)
}

#[tauri::command]
fn simulate_progress(
    app: AppHandle,
    state: State<SharedCore>,
    value: f64,
    satisfied: bool,
) -> Result<Snapshot, String> {
    app.state::<Runtime>().require_debug()?;
    let mut core = state.lock().unwrap();
    let unit = "reps".to_string();
    core.session.report_progress(Progress { value, unit, satisfied });
    let snap = persist_and_snapshot(&mut core);
    drop(core);
    if satisfied { hub::disable_metric_async(&app); }
    emit_snapshot(&app, &snap);
    Ok(snap)
}

#[tauri::command]
fn confirm_weight(app: AppHandle, state: State<SharedCore>, weight: f64) -> Snapshot {
    let clock = SystemClock;
    let mut core = state.lock().unwrap();
    if let Some(rec) = core.session.confirm_weight(weight, &clock.today()) {
        match core.store.record_set(&rec) {
            Ok(()) => {}
            Err(e) => eprintln!("failed to record set: {e}"),
        }
        core.session.take_pending_record(); // already persisted
        hub::queue_event(
            "weight_logged",
            serde_json::json!({"exercise": rec.exercise, "weight": weight}),
        );
    }
    let snap = persist_and_snapshot(&mut core);
    drop(core);
    // Camera gating: confirming the weight ENDS the workout, so it must
    // release the camera like every other completion path. Found live: a
    // simulated/early completion left the detector running (and counting)
    // after desktop_unlocked because only the hub-satisfied path disabled.
    hub::disable_metric_async(&app);
    emit_snapshot(&app, &snap);
    snap
}

// ---------------------------------------------------------------------------
// Detection debug view (Task C): stream exercise fixture videos through the
// Python detector sidecar for live visual debugging.
// ---------------------------------------------------------------------------

/// Walk up from `start` until we find a directory containing a `vision`
/// subdirectory, returning that subdirectory.
///
/// `start` is meant to be `CARGO_MANIFEST_DIR` (compile-time, `app/src-tauri`)
/// rather than the process's runtime cwd: `tauri dev`, `cargo check`, and the
/// test binary can all be invoked from different working directories, but
/// `CARGO_MANIFEST_DIR` is stable for a given checkout. Two levels up from
/// `app/src-tauri` is the repo root, which contains `vision/`. This embeds
/// the build machine's source path, which is fine for this dev-only debug
/// feature but would not be appropriate for an end-user installer build.
fn find_vision_dir(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join("vision");
        if candidate.is_dir() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}

fn vision_dir() -> Result<PathBuf, String> {
    find_vision_dir(Path::new(env!("CARGO_MANIFEST_DIR")))
        .ok_or_else(|| "could not locate vision/ directory".to_string())
}

#[derive(Serialize)]
struct DebugVideo {
    exercise: String,
    path: String,
}

/// Tracked fixture + youtube-manifest entries, read at request time.
fn list_debug_videos(vision_dir: &Path) -> Vec<DebugVideo> {
    let fixtures = vision_dir.join("tests/fixtures/videos");
    let mut out = Vec::new();

    let squat_demo = fixtures.join("squat_demo.webm");
    out.push(DebugVideo {
        exercise: "squat".to_string(),
        path: squat_demo
            .canonicalize()
            .unwrap_or(squat_demo)
            .to_string_lossy()
            .to_string(),
    });

    let manifest_path = fixtures.join("youtube/youtube_manifest.json");
    if let Ok(text) = std::fs::read_to_string(&manifest_path) {
        if let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(map) = manifest.as_object() {
                let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
                entries.sort_by_key(|(exercise, _)| exercise.as_str());
                for (exercise, entry) in entries {
                    let Some(file) = entry.get("file").and_then(|f| f.as_str()) else {
                        continue;
                    };
                    let path = fixtures.join("youtube").join(file);
                    out.push(DebugVideo {
                        exercise: exercise.clone(),
                        path: path.canonicalize().unwrap_or(path).to_string_lossy().to_string(),
                    });
                }
            }
        }
    }
    out
}

#[tauri::command]
fn debug_videos(app: AppHandle) -> Result<Vec<DebugVideo>, String> {
    app.state::<Runtime>().require_debug()?;
    if cfg!(debug_assertions) { return Ok(list_debug_videos(&vision_dir()?)); }
    let resources = app.path().resource_dir().map_err(|e| e.to_string())?;
    let video = resources.join("debug-videos/squat_demo.webm");
    Ok(if video.is_file() { vec![DebugVideo { exercise: "squat".into(), path: video.display().to_string() }] } else { vec![] })
}

/// Managed state for the currently-running debug sidecar child, kept
/// separate from `SharedCore` since it guards an OS process handle rather
/// than app/session data. `generation` disambiguates a reader thread's own
/// (possibly stale) child from one belonging to a newer `debug_stream_start`
/// call, so a slow-to-exit old process can never be reaped/waited-on twice
/// or mistaken for the new one.
struct DebugProcess {
    generation: u64,
    child: Option<Child>,
}

type SharedDebugProcess = Mutex<DebugProcess>;

const DEBUG_STREAM_EVENT: &str = "debug-stream";
const DEBUG_JPEG_EVERY: &str = "5";
const DEBUG_TARGET: &str = "10"; // matches WorkoutEngine's default_reps

/// Decide whether a just-spawned child belonging to `my_generation` should be
/// stored in `DebugProcess`, given the guard's generation at the moment the
/// spawning call re-acquires the lock after `Command::spawn()` returns.
///
/// Spawning happens without the lock held (it can block), so a second, later
/// `debug_stream_start` call can race ahead, kill/replace the tracked child,
/// and bump the generation before the first call gets back to storing its
/// own child. If that happened, `current_generation` will have moved past
/// `my_generation` and the newly spawned child is already orphaned: it must
/// be killed rather than stored, or it would silently replace (and leak) the
/// newer call's child handle.
fn should_store_spawn(current_generation: u64, my_generation: u64) -> bool {
    current_generation == my_generation
}

#[tauri::command]
fn debug_stream_start(
    app: AppHandle,
    state: State<SharedDebugProcess>,
    video: String,
    exercise: String,
) -> Result<(), String> {
    app.state::<Runtime>().require_debug()?;
    if !Path::new(&video).is_file() { return Err("Video is missing; choose an available fixture".into()); }
    let (vision_dir, plugin_dir, model) = if cfg!(debug_assertions) {
        let dir = vision_dir()?;
        (dir.clone(), dir.join("src"), None)
    } else {
        let resources = app.path().resource_dir().map_err(|e| e.to_string())?;
        (resources.join("hub-bundle/vision"), resources.join("reps-vision"), Some(resources.join("models/pose_landmarker_full.task")))
    };
    // Use the already provisioned environment directly: a single owned process,
    // no uv wrapper or writes into installed resources.
    let environment = std::env::var_os("UV_PROJECT_ENVIRONMENT").map(PathBuf::from).unwrap_or_else(|| {
        if cfg!(debug_assertions) { vision_dir.join(".venv") }
        else {
            dirs_next_data_dir().join("vision-env")
        }
    });
    let python = environment.join(if cfg!(windows) { "Scripts/python.exe" } else { "bin/python" });
    if !python.is_file() { return Err("Vision environment is not ready; wait for startup and try again".into()); }
    hub::disable_metric_now(&app);
    let my_generation = {
        let mut guard = state.lock().unwrap();
        if let Some(child) = guard.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        guard.generation += 1;
        guard.child = None;
        guard.generation
    };

    let mut command = Command::new(python);
    command.args([
        "-m", "reps_vision.stream", "--video", &video, "--exercise", &exercise,
        "--jpeg-every", DEBUG_JPEG_EVERY, "--target", DEBUG_TARGET,
    ]).current_dir(&vision_dir).env("PYTHONPATH", plugin_dir).env("PYTHONDONTWRITEBYTECODE", "1");
    if let Some(model) = model { command.env("REPS_POSE_MODEL", model); }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn debug sidecar: {e}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "debug sidecar has no stdout pipe".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "debug sidecar has no stderr pipe".to_string())?;

    {
        let mut guard = state.lock().unwrap();
        if should_store_spawn(guard.generation, my_generation) && !app.state::<Runtime>().is_stopping() {
            guard.child = Some(child);
        } else {
            // Superseded by a newer debug_stream_start call that raced ahead
            // while spawn() was blocking. That newer call already owns
            // `guard.child`; kill and reap this now-orphaned child instead of
            // overwriting the newer handle or leaking the process.
            drop(guard);
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    // Last ~20 stderr lines from the sidecar, shared between the stderr
    // reader thread (below) and the stdout reader thread's exit handling, so
    // a sidecar crash (e.g. missing mediapipe/opencv) surfaces its actual
    // error instead of just a bare exit code. We report the tail bundled
    // into the "exited" event (`{"event":"exited","code":...,"stderrTail":[...]}`)
    // rather than forwarding a live stderr stream, since stderr chatter is
    // only actionable once the process has stopped.
    const STDERR_TAIL_LEN: usize = 20;
    let stderr_tail: Arc<Mutex<VecDeque<String>>> =
        Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LEN)));

    let stderr_tail_writer = stderr_tail.clone();
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let mut tail = stderr_tail_writer.lock().unwrap();
            if tail.len() >= STDERR_TAIL_LEN {
                tail.pop_front();
            }
            tail.push_back(line);
        }
    });

    let handle = app.clone();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if handle.state::<Runtime>().is_stopping() { break; }
            let state = handle.state::<SharedDebugProcess>();
            let generation_guard = state.lock().unwrap();
            if generation_guard.generation != my_generation { break; }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<serde_json::Value>(trimmed) {
                Ok(value) => {
                    control::video_event(&handle,&value);
                    let _ = handle.emit_to("main", DEBUG_STREAM_EVENT, value);
                }
                Err(_) => {
                    let _ = handle.emit(
                        DEBUG_STREAM_EVENT,
                        serde_json::json!({"event": "error", "message": trimmed}),
                    );
                }
            }
        }

        let debug_state = handle.state::<SharedDebugProcess>();
        let mut guard = debug_state.lock().unwrap();
        if guard.generation != my_generation {
            // Superseded by a newer debug_stream_start call; that stream owns
            // reaping/emitting now, so stay quiet to avoid a stale "exited".
            return;
        }
        let code = guard
            .child
            .as_mut()
            .and_then(|c| c.wait().ok())
            .and_then(|status| status.code());
        guard.child = None;
        drop(guard);
        let tail: Vec<String> = stderr_tail.lock().unwrap().iter().cloned().collect();
        let _ = handle.emit(
            DEBUG_STREAM_EVENT,
            serde_json::json!({"event": "exited", "code": code, "stderrTail": tail}),
        );
    });

    Ok(())
}

#[tauri::command]
fn debug_stream_stop(app: AppHandle) -> Result<(), String> {
    stop_debug_process(&app);
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _instance = match daily::instance_lock(&dirs_next_data_dir()) {
        Ok(file) => file, Err(error) => { eprintln!("{error}"); return; }
    };
    let runtime = Runtime::load(dirs_next_data_dir()).expect("load application mode");
    let core = build_core(&runtime.session_home);
    runtime.lock_mode.store(false, std::sync::atomic::Ordering::SeqCst);
    let _ = core.store.set_setting("lock_mode", "0");
    let snooze_until = core.store.setting("snooze_until", "0").parse().unwrap_or(0.);
    let app = tauri::Builder::default()
        .manage(Mutex::new(control::DisplayState::default()))
        .manage(Mutex::new(daily::Daily { agents: Default::default(), snooze_until, checked_at: 0. }))
        .manage(runtime)
        .manage(Mutex::new(core) as SharedCore)
        .manage(Mutex::new(DebugProcess {
            generation: 0,
            child: None,
        }) as SharedDebugProcess)
        .manage(Mutex::new(None) as hub::SharedHub)
        .invoke_handler(tauri::generate_handler![
            control::get_display_state,
            daily::daily_status,
            daily::recent_history,
            daily::routine_settings,
            daily::save_routine,
            daily::share_workout,
            daily::reminder_action,
            control::workout_action,
            daily::save_daily_settings,
            hub::get_camera_settings,
            hub::save_camera_settings,
            get_snapshot,
            get_app_mode,
            set_app_mode,
            show_gym,
            emergency_escape,
            simulate_progress,
            confirm_weight,
            honor_complete,
            toggle_gym_fullscreen,
            debug_mode,
            debug_exercises,
            debug_videos,
            debug_stream_start,
            debug_stream_stop
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    if window.app_handle().state::<Runtime>().is_debug() { window.app_handle().exit(0); }
                    else { let _ = window.hide(); }
                } else if window.app_handle().state::<Runtime>().is_debug() {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .setup(|app| {

            control::start(app.handle()).map_err(std::io::Error::other)?;
            if !app.state::<Runtime>().is_debug() {
                let home = dirs_next_data_dir(); let handle = app.handle().clone();
                std::thread::spawn(move || {
                    let mut delay = 300u64;
                    while !handle.state::<Runtime>().is_stopping() {
                        if home.join("upload.json").exists() || home.join("sites.json").exists() {
                            if let Err(error) = reps_cli::sync(&home) {
                                let state = handle.state::<SharedCore>();
                                let _ = state.lock().unwrap().store.set_setting("sync_status", &error);
                                delay = (delay * 2).min(3600);
                            } else { delay = 300; }
                        }
                        std::thread::sleep(Duration::from_secs(delay));
                    }
                });
            }
            hub::start(app.handle().clone());
            windows::place(app.handle());
            let handle = app.handle().clone();
            // The coding timer starts the moment the app does — no "start" button.
            {
                let clock = SystemClock;
                let state = handle.state::<SharedCore>();
                if !handle.state::<Runtime>().is_debug() {
                    state.lock().unwrap().session.start(clock.now(), &clock.today());
                }
            }
            std::thread::spawn(move || {
                let mut unlocked_since: Option<f64> = None;
                let mut previous = SystemClock.now();
                let mut last_poll = 0.;
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                    let clock = SystemClock;
                    let (now, today) = (clock.now(), clock.today());
                    let state = handle.state::<SharedCore>();
                    let mut core = state.lock().unwrap();
                    if handle.state::<Runtime>().is_stopping() { break; }
                    let daily_state = handle.state::<daily::SharedDaily>();
                    let mut daily = daily_state.lock().unwrap();
                    let poll_due = now - last_poll >= 5.;
                    if poll_due { daily::refresh(&mut daily); last_poll = now; }
                    let elapsed = (now - previous).max(0.); previous = now;
                    core.session.defer_timer(engine::activity::excluded_elapsed(elapsed, daily.agents.active() && !control::preview_only(&handle), daily.snooze_until > now));
                    if daily.snooze_until > 0. && daily.snooze_until <= now {
                        daily.snooze_until = 0.;
                        let _ = core.store.set_setting("snooze_until", "0");
                        core.session.configure_timer(0., now);
                    }
                    let can_remind = daily.agents.active() && daily.snooze_until <= now && !control::preview_only(&handle);
                    drop(daily);
                    if poll_due {if let Some(plan)=core.session.plan(){let day=plan.to_day_plan();let _=core.store.record_routine_day(plan.date(),day.sets_done,day.sets_total);}}
                    let before = core.session.snapshot(now);
                    let remaining = before.day.as_ref().map(|d|d.sets_total.saturating_sub(d.sets_done)).unwrap_or(0);
                    let end = reps_cli::workday::parse_time(&core.store.setting("workday_end","18:00")).unwrap_or(1080);
                    let lead = core.store.setting("workday_warn_minutes","60").parse().unwrap_or(60);
                    let warn = poll_due && reps_cli::workday::due(reps_cli::workday::local_minute(),end,lead,
                        !handle.state::<Runtime>().is_debug() && can_remind && matches!(before.phase,Phase::Coding|Phase::ExerciseRequired),
                        remaining,core.store.setting("last_day_warning","")==today);
                    if warn && core.store.set_setting("last_day_warning",&today).is_ok() {
                        let message=format!("Your workday is ending soon or has ended. {remaining} workout sets remain — you might miss today's routine. Start when ready: rfp start");
                        let display={let state=handle.state::<control::SharedDisplay>();let mut display=state.lock().unwrap();display.notice=Some(message.clone());display.clone()};
                        let _=handle.emit("display-state",display);
                        daily::notify(message);
                        if before.phase==Phase::Coding {core.session.configure_timer(0.,now);}
                        if let Some(window)=handle.get_webview_window("main"){let _=window.show();let _=window.unminimize();}
                    }
                    // Clear a warning after starting a set or when the local date changes.
                    if !matches!(before.phase,Phase::Coding|Phase::ExerciseRequired) || core.store.setting("last_day_warning","")!=today {
                        let changed={let state=handle.state::<control::SharedDisplay>();let mut display=state.lock().unwrap();display.notice.take().map(|_|display.clone())};
                        if let Some(display)=changed{let _=handle.emit("display-state",display);}
                    }
                    let locked_now = !handle.state::<Runtime>().is_debug() && can_remind && core.session.tick(now, &today);
                    let mut snap = core.session.snapshot(now);
                    if locked_now {
                        // Show the passive reminder; CLI starts the camera.
                        if !warn {daily::notify("Time for a little movement. Start your next workout from the terminal: rfp start".into());}
                        emit_snapshot(&handle, &snap);
                        if handle.state::<Runtime>().enforces_windows() {
                            core.session.begin_workout();
                            snap = persist_and_snapshot(&mut core);
                            enable_metric_for(&handle, &snap);
                        } else if let Some(window) = handle.get_webview_window("main") {
                            let _ = window.show(); let _ = window.unminimize();
                        }
                    }
                    // The "LOGGED" beat: 3s in UNLOCKED, then CODE (main minimizes).
                    if !handle.state::<Runtime>().is_debug() && snap.phase == Phase::Unlocked {
                        let since = *unlocked_since.get_or_insert(now);
                        if now - since >= 3.0 {
                            core.session.resume_coding(now);
                            let minutes = core.store.setting("work_minutes", "25").parse().unwrap_or(25.);
                            core.session.configure_timer(minutes, now);
                            snap = persist_and_snapshot(&mut core);
                            unlocked_since = None;
                            // Belt and braces: an aborted workout must release the camera.
                            hub::disable_metric_async(&handle);
                        }
                    } else {
                        unlocked_since = None;
                    }
                    let presence = handle.state::<daily::SharedDaily>().lock().unwrap().agents.clone();
                    let runtime_status = serde_json::json!({"pid":std::process::id(),"updatedAt":now,"agents":presence,"phase":snap.phase,"remainingSeconds":snap.remaining_seconds,"mode":handle.state::<Runtime>().mode,"lockMode":handle.state::<Runtime>().enforces_windows()});
                    let status_home = &handle.state::<Runtime>().session_home;
                    if poll_due && std::fs::write(status_home.join("status.json.tmp"), runtime_status.to_string()).is_ok() { let _ = std::fs::rename(status_home.join("status.json.tmp"), status_home.join("status.json")); }
                    drop(core);
                    emit_snapshot(&handle, &snap);
                    windows::refocus(&handle);
                    windows::assert_gym(&handle);
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|handle, event| {
        if let RunEvent::ExitRequested { .. } = event {
            handle.state::<Runtime>().begin_stop();
            shutdown_runtime(handle);
        }

    });
}

#[cfg(test)]
mod debug_view_tests {
    use super::*;
    #[test]
    fn selectable_exercises_cover_every_shipped_detector_with_correct_units() {
        let options = debug_exercise_options().unwrap();
        let specs = hub::load_exercise_specs().unwrap();
        assert_eq!(options.len(), specs["exercises"].as_object().unwrap().len());
        for rx in options {
            assert!(specs["exercises"].get(&rx.exercise).is_some());
            match rx.exercise.as_str() {
                "jumprope" => { assert_eq!(rx.kind, ExerciseKind::Continuous); assert_eq!(rx.target_seconds, 60.0); }
                "stretch" => { assert_eq!(rx.kind, ExerciseKind::Continuous); assert_eq!(rx.target_seconds, 30.0); }
                _ => { assert_eq!(rx.kind, ExerciseKind::Rep); assert_eq!(rx.target_reps, 10); }
            }
        }
    }

    use std::fs;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("reps-debug-view-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn local_routine_prescribes_a_custom_movement_without_a_build() {
        let dir = temp_dir("routine");
        assert!(load_daily_plan(&dir, "today").is_ok());
        fs::write(dir.join("routine.json"), r#"{
            "lifts":[{"exercise":"my-curl","sets":2,"reps":8}]
        }"#).unwrap();
        let mut plan = load_daily_plan(&dir, "today").unwrap();
        let prescription = plan.prescribe().unwrap();
        assert_eq!(prescription.exercise, "my-curl");
        assert_eq!(prescription.target_reps, 8);
        fs::write(dir.join("routine.json"), "invalid").unwrap();
        assert!(load_daily_plan(&dir, "today").is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn find_vision_dir_walks_up_to_repo_root() {
        let root = temp_dir("walk-up");
        let vision = root.join("vision");
        fs::create_dir_all(&vision).unwrap();
        let start = root.join("app").join("src-tauri");
        fs::create_dir_all(&start).unwrap();

        assert_eq!(find_vision_dir(&start), Some(vision));
    }

    #[test]
    fn find_vision_dir_returns_none_when_absent() {
        let root = temp_dir("absent");
        let start = root.join("app").join("src-tauri");
        fs::create_dir_all(&start).unwrap();

        assert_eq!(find_vision_dir(&start), None);
    }

    #[test]
    fn list_debug_videos_includes_tracked_fixture_and_manifest_entries() {
        let root = temp_dir("videos");
        let vision = root.join("vision");
        let fixtures = vision.join("tests/fixtures/videos");
        let youtube = fixtures.join("youtube");
        fs::create_dir_all(&youtube).unwrap();
        fs::write(fixtures.join("squat_demo.webm"), b"fake").unwrap();
        fs::write(youtube.join("bench.mp4"), b"fake").unwrap();
        fs::write(
            youtube.join("youtube_manifest.json"),
            r#"{"bench": {"file": "bench.mp4", "url": "u", "title": "t"}}"#,
        )
        .unwrap();

        let videos = list_debug_videos(&vision);
        let exercises: Vec<&str> = videos.iter().map(|v| v.exercise.as_str()).collect();
        assert_eq!(exercises, vec!["squat", "bench"]);
        assert!(videos[0].path.ends_with("squat_demo.webm"));
        assert!(videos[1].path.ends_with("bench.mp4"));
    }

    #[test]
    fn list_debug_videos_without_manifest_has_only_tracked_fixture() {
        let root = temp_dir("no-manifest");
        let vision = root.join("vision");
        let fixtures = vision.join("tests/fixtures/videos");
        fs::create_dir_all(&fixtures).unwrap();
        fs::write(fixtures.join("squat_demo.webm"), b"fake").unwrap();

        let videos = list_debug_videos(&vision);
        assert_eq!(videos.len(), 1);
        assert_eq!(videos[0].exercise, "squat");
    }

    #[test]
    fn should_store_spawn_when_generation_still_current() {
        // No newer debug_stream_start call raced ahead: store the child.
        assert!(should_store_spawn(3, 3));
    }

    #[test]
    fn should_store_spawn_false_when_superseded_by_newer_generation() {
        // A newer debug_stream_start call bumped the generation while our
        // spawn() was blocking: our child is orphaned and must be killed,
        // never stored (it would clobber the newer call's handle).
        assert!(!should_store_spawn(4, 3));
    }
}
