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
use engine::types::{ExerciseKind, Phase, Snapshot};
use engine::workout::WorkoutEngine;
use serde::{Deserialize, Serialize};

/// Default daily routine, overridden by routine.json in the app data directory.
pub(crate) const ROUTINE_JSON: &str = include_str!("../resources/routine.json");

#[cfg(test)]
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
pub(crate) struct PlanState {
    pub(crate) date: String,
    pub(crate) done: Vec<(String, u32)>,
}
use tauri::{AppHandle, Emitter, Manager, RunEvent, State};

mod detector;
mod training;
mod prompt;
mod tray;
mod ui;
mod hub;
mod lifecycle;
mod vision_status;
mod windows;
mod runtime;
mod daily;
use lifecycle::Lifecycle;
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
    /// Where plan.json lives (the session home).
    pub(crate) home: PathBuf,
    /// The workout session under way, if any.
    pub(crate) active: Option<Active>,
    /// The routine just completed, for the gym's WORKOUT COMPLETE screen.
    pub(crate) last_completed: Option<String>,
}

/// One workout session: a routine from one cycle slot, persisted so a
/// restart can recover it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Active {
    pub(crate) id: String,
    pub(crate) routine_id: String,
    pub(crate) routine_name: String,
    pub(crate) slot_id: String,
    pub(crate) slot_index: usize,
    pub(crate) slot_count: usize,
    pub(crate) coding_minutes: f64,
}

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

    // V2: no session until the user starts one. A session that was under way
    // when the app stopped is restored (Lifecycle::begin brings it back paused).
    let mut active = serde_json::from_str::<Active>(&store.setting("active_workout", "")).ok();
    if let Some(a) = &active {
        let plan = training::load(dir, ROUTINE_JSON);
        match plan.routines.iter().find(|r| r.id == a.routine_id).map(|r| DailyPlan::from_routine_json(&training::routine_json(r), &SystemClock.today())) {
            Some(Ok(mut daily)) => {
                if let Ok(ps) = serde_json::from_str::<PlanState>(&store.setting("plan_state", "")) { daily.restore(&ps.date, &ps.done); }
                session.set_plan(daily);
            }
            _ => { eprintln!("could not restore the workout in progress"); active = None; }
        }
    }
    Core { session, store, home: dir.to_path_buf(), active, last_completed: None }
}

fn dirs_next_data_dir() -> std::path::PathBuf { engine::activity::app_home() }

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
        Phase::Idle => "[STATE] 💤 IDLE · no workout session".to_string(),
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

/// The desktop adapter of the lifecycle's `Effects` seam.
struct TauriEffects(AppHandle);

impl lifecycle::Effects for TauriEffects {
    fn snapshot(&self, snap: &Snapshot) {
        let app = &self.0;
        if app.state::<Runtime>().is_stopping() { return; }
        print_state(snap);
        let _ = app.emit("snapshot", snap);
        ui::publish(app);
    }
    fn vision(&self, status: vision_status::VisionStatus) { control::vision(&self.0, status); }
    fn camera_frame(&self, camera_id: &str, data: &serde_json::Value) {
        if control::camera_frame(&self.0) {
            let _ = self.0.emit_to("main", "vision-camera", serde_json::json!({"cameraId": camera_id, "data": data}));
        }
    }
    fn publish(&self, publication: hub_client::outbox::Publication) { hub::queue(publication); }
}

#[tauri::command]
fn get_snapshot(state: State<Lifecycle>) -> Snapshot {
    state.snapshot()
}

#[tauri::command]
fn get_app_mode(app: AppHandle) -> AppMode { app.state::<Runtime>().mode }

fn stop_debug_process(app: &AppHandle) {
    let state = app.state::<SharedDebugProcess>();
    let mut guard = state.lock().unwrap();
    guard.generation += 1;
    if let Some(mut child) = guard.child.take() { let _ = child.kill(); let _ = child.wait(); }
}

fn shutdown_runtime(app: &AppHandle) {
    stop_debug_process(app);
    hub::stop(app);
    app.state::<Runtime>().cleanup();
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

fn debug_videos(app: AppHandle) -> Result<Vec<DebugVideo>, String> {
    app.state::<Runtime>().require_debug()?;
    if cfg!(debug_assertions) { return Ok(list_debug_videos(&vision_dir()?)); }
    let resources = app.path().resource_dir().map_err(|e| e.to_string())?;
    let video = resources.join("debug-videos/squat_demo.webm");
    Ok(if video.is_file() { vec![DebugVideo { exercise: "squat".into(), path: video.display().to_string() }] } else { vec![] })
}

/// Managed state for the currently-running debug sidecar child, kept
/// separate from the `Lifecycle` since it guards an OS process handle rather
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

fn debug_stream_start(app: AppHandle, video: String, exercise: String) -> Result<(), String> {
    app.state::<Runtime>().require_debug()?;
    let state = app.state::<SharedDebugProcess>();
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
    app.state::<Lifecycle>().release_camera();
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
        "--jpeg-every", DEBUG_JPEG_EVERY, "--target", &detector::DEBUG_TARGET_REPS.to_string(),
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _instance = match daily::instance_lock(&dirs_next_data_dir()) {
        Ok(file) => file, Err(error) => { eprintln!("{error}"); return; }
    };
    let runtime = Runtime::load(dirs_next_data_dir()).expect("load application mode");
    let core = build_core(&runtime.session_home);
    // Lock mode is gone (V2): make sure no old setting can bring it back.
    let _ = core.store.set_setting("lock_mode", "0");
    let snooze_until = core.store.setting("snooze_until", "0").parse().unwrap_or(0.);
    let app = tauri::Builder::default()
        .manage(Mutex::new(control::DisplayState::default()))
        .manage(Mutex::new(daily::Daily { agents: Default::default(), snooze_until, checked_at: 0. }))
        .manage(Mutex::new(ui::Ui::default()) as ui::SharedUi)
        .manage(runtime)
        .manage(Mutex::new(DebugProcess {
            generation: 0,
            child: None,
        }) as SharedDebugProcess)
        .manage(hub::SharedHub::default())
        .invoke_handler(tauri::generate_handler![
            control::get_display_state,
            get_snapshot,
            get_app_mode,
            ui::get_app_state, ui::get_plan, ui::save_plan, ui::catalog,
            ui::prepare_workout, ui::cancel_setup, ui::start_session, ui::start_set,
            ui::manual_finish, ui::confirm_set, ui::snooze, ui::dismiss_prompt,
            ui::pause, ui::resume, ui::end_workout,
            ui::history, ui::exercise_history, ui::week,
            ui::show_gym, ui::show_main, ui::toggle_fullscreen,
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                match window.label() {
                    // Closing the launcher hides it to the tray; Quit is explicit.
                    "main" if window.app_handle().state::<Runtime>().is_debug() && !tray::available() => window.app_handle().exit(0),
                    // Closing a prompt is "not now": it comes back if still due.
                    "prompt" => ui::dismiss(window.app_handle()),
                    _ => { let _ = window.hide(); }
                }
            }
        })
        .setup(move |app| {
            let home = app.state::<Runtime>().normal_home.clone();
            app.manage(Lifecycle::new(
                core, Box::new(SystemClock), hub::SharedHub::clone(&app.state()),
                Box::new(TauriEffects(app.handle().clone())),
                Box::new(move || detector::load_inputs(&home)),
                app.state::<Runtime>().is_debug(), false,
            ));
            control::start(app.handle()).map_err(std::io::Error::other)?;
            if !app.state::<Runtime>().is_debug() {
                let home = dirs_next_data_dir(); let handle = app.handle().clone();
                std::thread::spawn(move || {
                    let mut delay = 300u64;
                    while !handle.state::<Runtime>().is_stopping() {
                        if home.join("upload.json").exists() || home.join("sites.json").exists() {
                            if let Err(error) = reps_cli::sync(&home) {
                                let _ = handle.state::<Lifecycle>().core().store.set_setting("sync_status", &error);
                                delay = (delay * 2).min(3600);
                            } else { delay = 300; }
                        }
                        std::thread::sleep(Duration::from_secs(delay));
                    }
                });
            }
            if let Err(error) = tray::build(app.handle()) { eprintln!("tray unavailable: {error}"); }
            hub::start(app.handle().clone());
            windows::place(app.handle());
            let handle = app.handle().clone();
            // Nothing counts down until Start Session; a recovered session comes back paused.
            handle.state::<Lifecycle>().begin();
            std::thread::spawn(move || {
                let mut previous = SystemClock.now();
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                    let clock = SystemClock;
                    let (now, today) = (clock.now(), clock.today());
                    let life = handle.state::<Lifecycle>();
                    let runtime = handle.state::<Runtime>();
                    if runtime.is_stopping() { break; }
                    let elapsed = (now - previous).max(0.); previous = now;
                    let (defer, snooze_expired, snoozed) = {
                        let daily_state = handle.state::<daily::SharedDaily>();
                        let mut daily = daily_state.lock().unwrap();
                        let snoozed = daily.snooze_until > now;
                        // Sleep/suspend is not coding time: a jump over 5s is deferred.
                        let defer = engine::activity::excluded_elapsed(elapsed, false, snoozed);
                        let snooze_expired = daily.snooze_until > 0. && daily.snooze_until <= now;
                        if snooze_expired { daily.snooze_until = 0.; }
                        (defer, snooze_expired, snoozed)
                    };
                    if snooze_expired { let _ = life.core().store.set_setting("snooze_until", "0"); }
                    let (snap, locked) = life.tick(now, &today, lifecycle::Tick {
                        defer,
                        expire: snooze_expired,
                        remind: !snoozed,
                    });
                    if locked { daily::notify("Time to move. Your next set is ready.".into()); }
                    ui::tick(&handle, &snap, now, snoozed);
                    if (now as u64) % 5 == 0 {
                        let status = serde_json::json!({"pid":std::process::id(),"updatedAt":now,"phase":snap.phase,"remainingSeconds":snap.remaining_seconds,"paused":snap.paused,"mode":runtime.mode,"lockMode":false});
                        let home = &runtime.session_home;
                        if std::fs::write(home.join("status.json.tmp"), status.to_string()).is_ok() { let _ = std::fs::rename(home.join("status.json.tmp"), home.join("status.json")); }
                    }
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|handle, event| {
        if let RunEvent::ExitRequested { api, code, .. } = event {
            // Closing the last window is not Quit while the tray can bring it back.
            if code.is_none() && tray::available() && !handle.state::<Runtime>().is_stopping() { api.prevent_exit(); return; }
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
        let options = detector::debug_exercise_options().unwrap();
        let specs = detector::load_exercise_specs().unwrap();
        assert_eq!(options.len(), specs["exercises"].as_object().unwrap().len());
        for rx in options {
            assert!(specs["exercises"].get(&rx.exercise).is_some());
            match rx.exercise.as_str() {
                "jumprope" => { assert_eq!(rx.kind, ExerciseKind::Continuous); assert_eq!(rx.target_seconds, 60.0); }
                "stretch" => { assert_eq!(rx.kind, ExerciseKind::Continuous); assert_eq!(rx.target_seconds, 30.0); }
                _ => { assert_eq!(rx.kind, ExerciseKind::Rep); assert_eq!(rx.target_reps, detector::DEBUG_TARGET_REPS); }
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
