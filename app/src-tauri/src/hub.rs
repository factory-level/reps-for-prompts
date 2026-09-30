//! Vision-hub integration: RFP declares its model and workout
//! configs (resources/exercise_specs.json) and drives the bundled hub via
//! the `VisionHub` trait — enable metric on workout start, pump events into
//! the session, disable on completion. Honor-mode fallback when the hub is
//! down: never strand the user, never phantom reps.

use std::sync::Mutex;

use engine::clock::{Clock, SystemClock};
use engine::types::Progress;
use hub_client::{EnableMetric, HubSupervisor, HubSupervisorConfig, VisionEvent, VisionHub};
use tauri::{AppHandle, Emitter, Manager};

pub const EXERCISE_SPECS: &str = include_str!("../resources/exercise_specs.json");
pub const WORKOUT_METRIC: &str = "workout";
pub const APP_ID: &str = "reps";


#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CameraSettings {
    pub consensus: bool,
    pub usb_device: String,
    #[serde(default = "default_usb_rotation")]
    pub usb_rotation: u16,
    pub phone_url: String,
    pub phone_rotation: u16,
}
fn default_usb_rotation() -> u16 { 180 }
fn phone_stream_url(value: &str) -> bool {
    ["rtsp://", "rtsps://", "http://", "https://"].iter().any(|prefix| value.starts_with(prefix))
}
impl Default for CameraSettings {
    fn default() -> Self { Self { consensus: false, usb_device: "/dev/video0".into(), usb_rotation: default_usb_rotation(), phone_url: String::new(), phone_rotation: 0 } }
}
#[tauri::command]
pub fn get_camera_settings(app: AppHandle) -> Result<CameraSettings, String> {
    let path = app.state::<crate::Runtime>().normal_home.join("cameras.json");
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(CameraSettings::default()),
        Err(e) => Err(e.to_string()),
    }
}
#[tauri::command]
pub fn save_camera_settings(app: AppHandle, settings: CameraSettings) -> Result<(), String> {
    if !matches!(settings.usb_rotation, 0 | 90 | 180 | 270) || !matches!(settings.phone_rotation, 0 | 90 | 180 | 270) || !settings.usb_device.starts_with("/dev/video") {
        return Err("Choose a /dev/video device and a valid rotation".into());
    }
    if settings.consensus && (!phone_stream_url(&settings.phone_url) || settings.phone_url.contains('@')) {
        return Err("Enter an RTSP or HTTP MJPEG URL without embedded credentials".into());
    }
    let path = app.state::<crate::Runtime>().normal_home.join("cameras.json");
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    std::fs::write(&temporary, serde_json::to_vec(&settings).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    std::fs::rename(temporary, path).map_err(|e| e.to_string())
}

pub type SharedHub = Mutex<Option<Box<dyn VisionHub>>>;

/// The FieldLab application manifest (API 1.4): every durable event and
/// command reps publishes, declared up front. The hub rejects unregistered
/// types, so this list IS the app's event contract.
pub fn reps_manifest() -> serde_json::Value {
    let event = serde_json::json!({"schemaVersion": "1"});
    serde_json::json!({
        "events": {
            "coding_period_started": event, "coding_period_expired": event,
            "workout_prescribed": event, "desktop_locked": event,
            "detector_started": event, "rep_completed": event,
            "target_reached": event, "weight_logged": event,
            "workout_completed": event, "detector_stopped": event,
            "desktop_unlocked": event, "override_used": event,
            "system_error": event,
        },
        "commands": {
            // exposed:false — invokable only by the app itself in V1; remote
            // lock/camera control needs a safety story first.
            "desktop.lock_requested": {"schemaVersion": "1", "exposed": false},
            "detector.enable_requested": {"schemaVersion": "1", "exposed": false},
        },
    })
}

// ---- Durable publishing -------------------------------------------------
// Persist on the calling thread, publish in the background, and delete only
// after acknowledgement. No network request runs on a UI/state path.
use hub_client::outbox::{Outbox, Publication as Publish};

struct Publisher {
    outbox: Outbox,
    wake: std::sync::mpsc::SyncSender<()>,
    app: AppHandle,
}

static PUBLICATIONS_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static PUBLISHER: Mutex<Option<Publisher>> = Mutex::new(None);
static DETECTOR_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static SESSION_ID: Mutex<Option<String>> = Mutex::new(None);

fn new_session_id() -> String {
    let id = format!("sess-{}", uuid::Uuid::new_v4());
    *SESSION_ID.lock().unwrap() = Some(id.clone());
    id
}

fn session_id() -> Option<String> {
    SESSION_ID.lock().unwrap().clone()
}

fn queue(mut message: Publish) {
    if !PUBLICATIONS_ENABLED.load(std::sync::atomic::Ordering::SeqCst) { return; }
    // Hub timestamps describe acceptance, which may follow an offline period.
    // Preserve when the desktop observed the fact in the immutable queued body.
    let params = match &mut message {
        Publish::Event(p) | Publish::Command(p) | Publish::ActionResult(p) => p,
    };
    if let Some(payload) = params.get_mut("payload").and_then(serde_json::Value::as_object_mut) {
        payload.entry("occurredAtMs").or_insert_with(|| serde_json::json!(publication_time_ms()));
    }
    let failure = {
        let mut guard = PUBLISHER.lock().unwrap();
        match guard.as_mut() {
            Some(publisher) => match publisher.outbox.enqueue(&message) {
                Ok(()) => { let _ = publisher.wake.try_send(()); None }
                Err(error) => Some((publisher.app.clone(), error)),
            },
            None => {
                eprintln!("[EVENTS] publication rejected: durable outbox unavailable");
                None
            }
        }
    };
    if let Some((app, error)) = failure { publication_storage_error(&app, &error); }
}

fn publication_storage_error(app: &AppHandle, error: &str) {
    eprintln!("[EVENTS] durable publication failed: {error}");
    let _ = app.emit("vision-fallback", serde_json::json!({"reason": "local event storage unavailable", "details": error}));
}

/// Persist an app event locally, then publish asynchronously to the hub.
pub fn queue_event(event_type: &str, payload: serde_json::Value) {
    let mut params = serde_json::json!({"appId": APP_ID, "type": event_type, "id": format!("evt-{}", new_message_id()), "payload": payload});
    if let Some(sid) = session_id() {
        params["sessionId"] = serde_json::json!(sid);
    }
    queue(Publish::Event(params));
}

fn queue_command(command_type: &str, payload: serde_json::Value) -> String {
    let id = format!("cmd-{}", new_message_id());
    let mut params =
        serde_json::json!({"appId": APP_ID, "type": command_type, "id": id, "payload": payload});
    if let Some(sid) = session_id() {
        params["sessionId"] = serde_json::json!(sid);
    }
    queue(Publish::Command(params));
    id
}

fn queue_action_result(result_type: &str, command_id: &str, status: &str) {
    let mut params = serde_json::json!({
        "appId": APP_ID, "id": format!("result-{}", new_message_id()), "type": result_type, "commandId": command_id, "status": status,
        "payload": {},
    });
    if let Some(sid) = session_id() {
        params["sessionId"] = serde_json::json!(sid);
    }
    queue(Publish::ActionResult(params));
}

fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn publication_time_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64).unwrap_or(0)
}

/// Open the durable queue before hub startup, including publications made while
/// the hub is unavailable. FIFO retries preserve command/result ordering.
fn start_publisher(app: AppHandle) -> Result<(), String> {
    let mut guard = PUBLISHER.lock().unwrap();
    if guard.is_some() { return Ok(()); }
    let dir = crate::dirs_next_data_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let outbox = Outbox::open(&dir.join("publications.sqlite"))?;
    let (wake, rx) = std::sync::mpsc::sync_channel(1);
    let worker_app = app.clone();
    std::thread::Builder::new().name("hub-publish".into()).spawn(move || loop {
        let pending = {
            let guard = PUBLISHER.lock().unwrap();
            guard.as_ref().map(|p| p.outbox.next(publication_time_ms())).transpose()
        };
        let pending = match pending {
            Ok(Some(Some(pending))) => pending,
            Ok(_) => { let _ = rx.recv_timeout(std::time::Duration::from_millis(250)); continue; }
            Err(error) => {
                publication_storage_error(&worker_app, &error);
                let _ = rx.recv_timeout(std::time::Duration::from_secs(2));
                continue;
            }
        };
        let outcome = {
            let state = worker_app.state::<SharedHub>();
            let mut hub = state.lock().unwrap();
            match hub.as_mut() {
                Some(hub) => hub.register_application(APP_ID, env!("CARGO_PKG_VERSION"), &reps_manifest())
                    .and_then(|()| match &pending.publication {
                        Publish::Event(p) => hub.publish_event(p),
                        Publish::Command(p) => hub.publish_command(p),
                        Publish::ActionResult(p) => hub.report_action_result(p),
                    }),
                None => Err(hub_client::HubError::Down),
            }
        };
        let saved = {
            let guard = PUBLISHER.lock().unwrap();
            let outbox = &guard.as_ref().expect("publisher initialized").outbox;
            match &outcome {
                Ok(()) => outbox.acknowledge(pending.sequence),
                Err(error) => outbox.retry(&pending, publication_time_ms(), &error.to_string()),
            }
        };
        if let Err(error) = saved { publication_storage_error(&worker_app, &error); }
        if let Err(error) = outcome {
            if pending.attempts == 0 || (pending.attempts + 1).is_power_of_two() {
                eprintln!("[EVENTS] publication retained for retry: {error}");
            }
        }
    }).map_err(|e| e.to_string())?;
    *guard = Some(Publisher { outbox, wake, app });
    Ok(())
}

/// The durable events implied by a phase change — pure so the mapping is
/// unit-testable. `lock_cycle` distinguishes the command-worthy transitions:
/// entering ExerciseRequired starts a session, returning to Coding ends it.
pub(crate) fn events_for_transition(
    prev: Option<&engine::types::Phase>,
    next: &engine::types::Phase,
    prescription: Option<&engine::types::Prescription>,
) -> Vec<(&'static str, serde_json::Value)> {
    use engine::types::Phase;
    match (prev, next) {
        (None, Phase::Coding) | (Some(Phase::Unlocked), Phase::Coding) => {
            vec![("coding_period_started", serde_json::json!({}))]
        }
        (Some(Phase::Coding), Phase::ExerciseRequired) => {
            let mut events = vec![("coding_period_expired", serde_json::json!({}))];
            if let Some(rx) = prescription {
                events.push((
                    "workout_prescribed",
                    serde_json::json!({
                        "exercise": rx.exercise,
                        "targetReps": rx.target_reps,
                        "targetSeconds": rx.target_seconds,
                    }),
                ));
            }
            events.push(("desktop_locked", serde_json::json!({})));
            events
        }
        (Some(Phase::WorkoutActive), Phase::Unlocked)
        | (Some(Phase::WeightConfirmation), Phase::Unlocked) => vec![
            ("workout_completed", serde_json::json!({})),
            ("desktop_unlocked", serde_json::json!({})),
        ],
        _ => vec![],
    }
}

/// Publish the durable events implied by a phase change. Called from
/// emit_snapshot — the single choke point every state mutation goes through.
pub(crate) fn publish_phase_transition(snap: &engine::types::Snapshot) {
    use engine::types::Phase;
    static LAST: Mutex<Option<Phase>> = Mutex::new(None);
    let prev = {
        let mut last = LAST.lock().unwrap();
        let prev = last.clone();
        *last = Some(snap.phase.clone());
        prev
    };
    if prev.as_ref() == Some(&snap.phase) {
        return;
    }
    // Session + lock-cycle bookkeeping around the pure mapping: a new lock
    // cycle mints the session id and records the lock as command → result;
    // returning to coding closes the session.
    if matches!((prev.as_ref(), &snap.phase), (Some(Phase::Coding), Phase::ExerciseRequired)) {
        new_session_id();
    }
    let events = events_for_transition(prev.as_ref(), &snap.phase, snap.prescription.as_ref());
    for (event_type, payload) in events {
        if event_type == "desktop_locked" {
            // The lock is a requested operation with a distinct outcome, not
            // just an event: command → action result → the desktop_locked fact.
            let command_id = queue_command("desktop.lock_requested", serde_json::json!({}));
            queue_action_result("desktop.lock_succeeded", &command_id, "succeeded");
        }
        queue_event(event_type, payload);
    }
    if matches!((prev.as_ref(), &snap.phase), (Some(Phase::Unlocked), Phase::Coding)) {
        *SESSION_ID.lock().unwrap() = None;
    }
}

/// Parse and sanity-check the shipped specs at startup: the model must be
/// declared and every exercise must name a known activity.
pub fn load_exercise_specs() -> Result<serde_json::Value, String> {
    let specs: serde_json::Value =
        serde_json::from_str(EXERCISE_SPECS).map_err(|e| format!("exercise_specs.json: {e}"))?;
    let model = specs
        .get("model")
        .and_then(|m| m.get("plugin"))
        .and_then(|p| p.as_str())
        .ok_or("exercise_specs.json: missing model.plugin declaration")?;
    if model.is_empty() {
        return Err("exercise_specs.json: empty model.plugin".into());
    }
    let exercises = specs
        .get("exercises")
        .and_then(|e| e.as_object())
        .ok_or("exercise_specs.json: missing exercises")?;
    for (name, entry) in exercises {
        let activity = entry.get("activity").and_then(|a| a.as_str()).unwrap_or("");
        if !matches!(activity, "lift" | "jumprope" | "stretch") {
            return Err(format!("exercise {name}: unknown activity {activity:?}"));
        }
    }
    Ok(specs)
}

/// The enable_metric config for a prescription: shipped exercise definition
/// + targets + camera selection.
pub fn metric_config_for(
    specs: &serde_json::Value,
    exercise: &str,
    target_reps: u32,
    target_seconds: f64,
) -> Option<(String, serde_json::Value)> {
    let plugin_id = specs["model"]["plugin"].as_str()?.to_string();
    let mut config = match specs["exercises"].get(exercise) {
        Some(config) => config.clone(),
        // A Studio movement needs no compiled exercise registry entry. Without
        // a legacy exercise fallback the hub requires an activated version.
        None if target_reps > 0 && !exercise.trim().is_empty() => {
            serde_json::json!({"activity": "lift", "movementId": exercise})
        }
        None => return None,
    };
    let object = config.as_object_mut()?;
    if object.get("activity").and_then(|a| a.as_str()) == Some("lift") {
        object.insert("movementId".into(), serde_json::json!(exercise));
        object.insert("targetReps".into(), serde_json::json!(target_reps));
    } else if target_seconds > 0.0 {
        object.insert("targetSeconds".into(), serde_json::json!(target_seconds));
    }
    let camera_index: i64 = std::env::var("REPS_CAMERA_INDEX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    // The rig's webcam is mounted upside-down, and every exercise was tuned on
    // frames rotated 180° — so the app must rotate identically or the joint
    // angles are inverted and no rep crosses its thresholds. Overridable for
    // other setups via REPS_CAMERA_ROTATE (0/90/180/270).
    let camera_rotate: i64 = std::env::var("REPS_CAMERA_ROTATE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(180);
    object.insert(
        "camera".into(),
        serde_json::json!({"source": "index", "value": camera_index, "id": "webcam", "rotate": camera_rotate}),
    );
    Some((plugin_id, config))
}

/// A camera-set declaration from exercise specs: registry entries to
/// register, the set to fuse across, and the fusion policy.
pub struct CameraSet {
    pub registry: Vec<serde_json::Value>,
    pub cameras: Vec<String>,
    /// Per-camera config deltas applied after enable (tuning overlays).
    pub overlays: Vec<(String, serde_json::Value)>,
}

/// When the specs declare a camera set, strip the injected single-camera
/// object, put the fusion policy into the config, and return the set.
pub fn apply_camera_set(
    specs: &serde_json::Value,
    config: &mut serde_json::Value,
) -> Option<CameraSet> {
    let block = specs.get("cameras")?;
    let cameras: Vec<String> = block
        .get("set")
        .and_then(|s| s.as_array())
        .map(|set| set.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if cameras.len() < 2 {
        eprintln!("hub: cameras.set needs >= 2 camera ids; falling back to single camera");
        return None;
    }
    let registry = block
        .get("registry")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    let object = config.as_object_mut()?;
    object.remove("camera");
    object.insert(
        "fusion".into(),
        block
            .get("fusion")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"policy": "best", "scoreField": "visibility"})),
    );
    let overlays = block
        .get("overlays")
        .and_then(|o| o.as_object())
        .map(|map| {
            map.iter()
                .filter(|(camera_id, _)| cameras.contains(camera_id))
                .map(|(camera_id, delta)| (camera_id.clone(), delta.clone()))
                .collect()
        })
        .unwrap_or_default();
    Some(CameraSet {
        registry,
        cameras,
        overlays,
    })
}

/// Drive a hub through camera-set setup: register cameras, enable the
/// metric, then apply per-camera overlays. Extracted from the async enable
/// path so the ordering is unit-testable against the fake hub.
pub fn enable_on_hub(
    hub: &mut dyn hub_client::VisionHub,
    camera_set: &Option<CameraSet>,
    request: &EnableMetric,
) -> Result<(), hub_client::HubError> {
    // A new lock cycle must never inherit a stale stream: disable first so a
    // leftover metric (rapid re-lock, missed disable) is torn down before the
    // fresh enable. Failing disable is normal — usually "unknown_metric".
    let _ = hub.disable_metric(&request.metric_id);
    if let Some(set) = camera_set {
        for camera in &set.registry {
            hub.add_camera(camera)?;
        }
    }
    hub.enable_metric(request)?;
    if let Some(set) = camera_set {
        for (camera_id, delta) in &set.overlays {
            if let Err(err) =
                hub.update_metric_config_for_camera(&request.metric_id, camera_id, delta)
            {
                eprintln!("hub: overlay for {camera_id} failed: {err}");
            }
        }
    }
    Ok(())
}

/// Why the hub slot is empty: still starting, or failed and retrying. Keeps
/// `rfp camera status` from claiming the hub "is starting" forever after a
/// start that actually failed.
static LAST_HUB_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn set_hub_error(error: Option<String>) {
    if let Ok(mut slot) = LAST_HUB_ERROR.lock() { *slot = error; }
}

pub fn unavailable_reason() -> String {
    match LAST_HUB_ERROR.lock().ok().and_then(|slot| slot.clone()) {
        Some(error) => format!("Vision hub is not running ({error}); retrying in the background"),
        None => "Vision hub is starting".into(),
    }
}

/// Start the supervisor in the background and pump its events into the
/// session. No-op when REPS_HUB_DISABLED is set (dev without a hub).
pub fn start(app: AppHandle) {
    PUBLICATIONS_ENABLED.store(!app.state::<crate::Runtime>().is_debug(), std::sync::atomic::Ordering::SeqCst);
    if let Err(error) = if app.state::<crate::Runtime>().is_debug() { Ok(()) } else { start_publisher(app.clone()) } {
        publication_storage_error(&app, &error);
        return;
    }
    if std::env::var("REPS_HUB_DISABLED").is_ok() {
        eprintln!("hub: disabled by REPS_HUB_DISABLED");
        return;
    }
    std::thread::Builder::new()
        .name("hub-start".into())
        .spawn(move || {
            let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .expect("repo root")
                .to_path_buf();
            let mut config = if cfg!(debug_assertions) {
                HubSupervisorConfig::dev(&repo_root)
            } else {
                match app.path().resource_dir() {
                    Ok(resources) => HubSupervisorConfig::bundled(&resources, &resources.join("reps-vision"), &crate::dirs_next_data_dir()),
                    Err(err) => {
                        let _ = app.emit("vision-fallback", serde_json::json!({"reason": err.to_string()}));
                        return;
                    }
                }
            };
            if app.state::<crate::Runtime>().is_debug() {
                config.env.extend([
                    ("HUB_DATA_DIR".into(), app.state::<crate::Runtime>().session_home.join("hub").display().to_string()),
                    ("PORT".into(), "0".into()), ("DEBUG_PORT".into(), "0".into()),
                    ("HUB_BIND_HOST".into(), "127.0.0.1".into()),
                    ("HUB_CERT_DIR".into(), app.state::<crate::Runtime>().session_home.join("no-certs").display().to_string()),
                ]);
            }
            // Retry rather than stranding the camera: a leaked hubd or a
            // previous instance mid-shutdown holds the port for seconds, and a
            // single failure used to mean honor mode until the app restarted.
            let supervisor = HubSupervisor::start_retrying(
                config,
                { let app = app.clone(); move || app.state::<crate::Runtime>().is_stopping() },
                { let app = app.clone(); move |error, delay| {
                    set_hub_error(Some(error.to_string()));
                    eprintln!("hub: failed to start ({error}); honor mode, retrying in {}s", delay.as_secs_f32());
                    let _ = app.emit("vision-fallback", serde_json::json!({"reason": error.to_string()}));
                } },
            );
            let state = app.state::<SharedHub>();
            let mut slot = state.lock().unwrap();
            if app.state::<crate::Runtime>().is_stopping() { return; }
            match supervisor {
                Some(mut supervisor) => {
                    set_hub_error(None);
                    if app.state::<crate::Runtime>().is_stopping() { return; }
                    // A desktop restart begins idle rather than restoring an old camera stream.
                    let _ = supervisor.disable_metric(WORKOUT_METRIC);
                    let receiver = supervisor.take_receiver();
                    // Register the app manifest before anything publishes; the
                    // hub rejects event types it has not seen registered.
                    if let Err(err) = supervisor.register_application(
                        APP_ID,
                        env!("CARGO_PKG_VERSION"),
                        &reps_manifest(),
                    ) {
                        eprintln!("hub: register_application failed: {err}");
                    }
                    *slot = Some(Box::new(supervisor));
                    drop(slot);
                    let _ = app.emit("vision-event", serde_json::json!({"kind": "hub_up"}));
                    if let Some(rx) = receiver {
                        pump_events(app, rx);
                    }
                }
                // Only reachable once the app is shutting down.
                None => {}
            }
        })
        .ok();
}

/// Print a terminal line when the camera gains/loses your pose (deduped — no
/// per-frame angle spam), so you can tell from the terminal whether you're in
/// frame while a set runs.
fn print_detect(data: &serde_json::Value) {
    static LAST: std::sync::Mutex<Option<bool>> = std::sync::Mutex::new(None);
    let pose = data.get("poseDetected").and_then(|v| v.as_bool()).unwrap_or(false);
    if let Ok(mut last) = LAST.lock() {
        if *last != Some(pose) {
            println!(
                "{}",
                if pose {
                    "[DETECT] 🟢 pose in frame — tracking"
                } else {
                    "[DETECT] 🔴 no pose in frame"
                }
            );
            *last = Some(pose);
        }
    }
}

fn pump_events(app: AppHandle, rx: std::sync::mpsc::Receiver<VisionEvent>) {
    for event in rx {
        if app.state::<crate::Runtime>().is_stopping() { break; }
        match event {
            // Landmarks are terminal-only now: nothing in the webviews listens,
            // and pushing them at camera rate into both windows leaked memory.
            VisionEvent::CameraFrame { camera_id, data } => {
                if DETECTOR_ACTIVE.load(std::sync::atomic::Ordering::SeqCst) && crate::control::camera_frame(&app) {
                    let _ = app.emit_to("main", "vision-camera", serde_json::json!({"cameraId": camera_id, "data": data}));
                }
            }
            VisionEvent::Landmarks(data) => print_detect(&data),
            VisionEvent::Progress { value, unit, satisfied, context } => {
                if !DETECTOR_ACTIVE.load(std::sync::atomic::Ordering::SeqCst) || crate::control::preview_only(&app) { continue; }
                if !context.belongs_to(WORKOUT_METRIC, session_id().as_deref()) {
                    continue;
                }
                if let Some(status) = &context.consensus {
                    let _ = app.emit("vision-consensus", status);
                }
                let core_state = app.state::<crate::SharedCore>();
                let mut core = core_state.lock().unwrap();
                core.session.report_progress(Progress { value, unit, satisfied });
                let snap = crate::persist_and_snapshot(&mut core);
                drop(core);
                crate::emit_snapshot(&app, &snap);
                if satisfied {
                    disable_metric_async(&app);
                }
            }
            VisionEvent::Semantic { kind, payload } => {
                if kind == "detector_error" || kind == "stream_ended" {
                    crate::control::error(&app,format!("{kind}: {payload}"));
                    let _ = app.emit("vision-fallback", serde_json::json!({"reason": kind, "details": payload}));
                }
                if kind == "rep_completed" || kind == "target_reached" {
                    println!("[DETECT] ✓ {}", kind);
                }
                let _ = app.emit(
                    "vision-event",
                    serde_json::json!({"kind": kind, "payload": payload}),
                );
            }
            VisionEvent::Health(health) => {
                let failed = health.vision_host == "down";
                let _ = app.emit(
                    "vision-event",
                    serde_json::json!({"kind": "health", "payload": {
                        "visionHost": health.vision_host,
                        "camera": health.camera,
                    }}),
                );
                if failed {
                    let _ = app.emit(
                        "vision-fallback",
                        serde_json::json!({"reason": "hub down after restart"}),
                    );
                }
            }
            VisionEvent::ConnectionLost => {
                let _ = app.emit(
                    "vision-event",
                    serde_json::json!({"kind": "connection_lost"}),
                );
            }
        }
    }
}

/// Enable the workout metric for the current prescription (fire-and-forget;
/// failures surface as vision-fallback so the UI offers honor mode).
pub fn enable_metric_async(app: &AppHandle, exercise: String, target_reps: u32, target_seconds: f64) {
    let generation = app.state::<crate::Runtime>().detector_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    let app = app.clone();
    std::thread::spawn(move || {
        if app.state::<crate::Runtime>().is_stopping() { return; }
        let mut specs = match load_exercise_specs() {
            Ok(specs) => specs,
            Err(err) => {
                eprintln!("hub: bad exercise specs: {err}");
                return;
            }
        };
        let Some((plugin_id, mut config)) =
            metric_config_for(&specs, &exercise, target_reps, target_seconds)
        else {
            eprintln!("hub: no spec for exercise {exercise}; honor mode");
            queue_event(
                "system_error",
                serde_json::json!({"stage": "enable_metric", "error": format!("no spec for {exercise}")}),
            );
            let _ = app.emit(
                "vision-fallback",
                serde_json::json!({"reason": format!("no spec for {exercise}")}),
            );
            return;
        };

        match get_camera_settings(app.clone()) {
            Ok(settings) if settings.consensus && config.get("movementId").is_some() => {
                // Require an active version: legacy exercise fallback has no cycle evidence.
                if let Some(object) = config.as_object_mut() { object.remove("exercise"); }
                let phone_source = match std::env::var("REPS_PHONE_URL_FILE") {
                    Ok(path) => match std::fs::read_to_string(path) {
                        Ok(value) if phone_stream_url(value.trim()) => value.trim().to_string(),
                        _ => { let _ = app.emit("vision-fallback", serde_json::json!({"reason": "Cannot read phone stream credentials file"})); return; }
                    },
                    Err(_) => settings.phone_url.clone(),
                };
                let calibration_path = app.state::<crate::Runtime>().normal_home.join("camera-calibration.json");
                let calibration: serde_json::Value = std::fs::read(calibration_path).ok()
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or(serde_json::json!({}));
                config["cameraCalibration"] = calibration;
                specs["cameras"] = serde_json::json!({ "set": ["webcam", "phone"],
                    "fusion": {"policy": "consensus", "quorum": 2},
                    "registry": [
                        {"cameraId": "webcam", "label": "USB webcam", "kind": "usb", "transport": "direct", "source": settings.usb_device, "rotate": settings.usb_rotation},
                        {"cameraId": "phone", "label": "Android phone", "kind": "rtsp", "transport": "direct", "source": phone_source, "rotate": settings.phone_rotation}
                    ] });
            }
            Ok(settings) => {
                if app.state::<crate::Runtime>().normal_home.join("cameras.json").exists() {
                    config["camera"]["source"] = serde_json::json!("uri");
                    config["camera"]["value"] = serde_json::json!(settings.usb_device);
                    config["camera"]["rotate"] = serde_json::json!(settings.usb_rotation);
                }
            },
            Err(error) => {
                let _ = app.emit("vision-fallback", serde_json::json!({"reason": error}));
                return;
            }
        }
        config["appId"] = serde_json::json!(APP_ID);
        let camera_set = apply_camera_set(&specs, &mut config);
        let state = app.state::<SharedHub>();
        let mut guard = state.lock().unwrap();
        if app.state::<crate::Runtime>().is_stopping() ||
            app.state::<crate::Runtime>().detector_generation.load(std::sync::atomic::Ordering::SeqCst) != generation { return; }
        let sid = if app.state::<crate::Runtime>().is_debug() { new_session_id() } else { session_id().unwrap_or_else(new_session_id) };
        config["sessionId"] = serde_json::json!(sid);
        match guard.as_mut() {
            Some(hub) => {
                DETECTOR_ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
                let request = EnableMetric {
                    metric_id: WORKOUT_METRIC.into(),
                    plugin_id,
                    cameras: camera_set.as_ref().map(|set| set.cameras.clone()),
                    config,
                };
                match enable_on_hub(hub.as_mut(), &camera_set, &request) {
                    Ok(()) => {
                        drop(guard);
                        queue_event("detector_started", serde_json::json!({"exercise": exercise}));
                    }
                    Err(err) => {
                        DETECTOR_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
                        drop(guard);
                        eprintln!("hub: enable_metric failed: {err}");
                        queue_event(
                            "system_error",
                            serde_json::json!({"stage": "enable_metric", "error": err.to_string()}),
                        );
                        let _ = app.emit(
                            "vision-fallback",
                            serde_json::json!({"reason": err.to_string()}),
                        );
                    }
                }
            }
            None => {
                queue_event(
                    "system_error",
                    serde_json::json!({"stage": "enable_metric", "error": "hub not running"}),
                );
                let _ = app.emit(
                    "vision-fallback",
                    serde_json::json!({"reason": "hub not running"}),
                );
            }
        }
    });
}

/// Disable the workout metric (camera released). Fire-and-forget.
pub fn disable_metric_async(app: &AppHandle) {
    let generation = app.state::<crate::Runtime>().detector_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    DETECTOR_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
    let app = app.clone();
    std::thread::spawn(move || {
        let state = app.state::<SharedHub>();
        let mut guard = state.lock().unwrap();
        if app.state::<crate::Runtime>().detector_generation.load(std::sync::atomic::Ordering::SeqCst) != generation { return; }
        if let Some(hub) = guard.as_mut() {
            if let Err(err) = hub.disable_metric(WORKOUT_METRIC) {
                eprintln!("hub: disable_metric failed: {err}");
            } else {
                drop(guard);
                queue_event("detector_stopped", serde_json::json!({}));
            }
        }
    });
}

pub(crate) fn disable_metric_now(app: &AppHandle) {
    app.state::<crate::Runtime>().detector_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    DETECTOR_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
    let state = app.state::<SharedHub>();
    if let Some(hub) = state.lock().unwrap().as_mut() { let _ = hub.disable_metric(WORKOUT_METRIC); };
}

/// Honor-mode completion: the camera path failed, the user pressed Done.
pub fn honor_complete(app: &AppHandle) -> engine::types::Snapshot {
    let clock = SystemClock;
    // Honor mode is visible in durable history, never a silent degradation.
    queue_event("override_used", serde_json::json!({"mode": "honor"}));
    let core_state = app.state::<crate::SharedCore>();
    let mut core = core_state.lock().unwrap();
    core.session.mark_unverified();
    let (value, unit) = match core.session.snapshot(clock.now()).prescription {
        Some(rx) if rx.target_seconds > 0.0 => (rx.target_seconds, "seconds".to_string()),
        Some(rx) => (rx.target_reps as f64, "reps".to_string()),
        None => (0.0, "reps".to_string()),
    };
    core.session.report_progress(Progress { value, unit, satisfied: true });
    let snap = crate::persist_and_snapshot(&mut core);
    drop(core);
    crate::emit_snapshot(app, &snap);
    disable_metric_async(app);
    snap
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_specs_parse_and_declare_the_model() {
        let specs = load_exercise_specs().unwrap();
        assert_eq!(specs["model"]["plugin"], "reps_vision");
        assert_eq!(specs["model"]["name"], "mediapipe-pose-landmarker");
        assert!(specs["exercises"].get("squat").is_some());
    }

    #[test]
    fn metric_config_carries_exercise_target_and_camera() {
        let specs = load_exercise_specs().unwrap();
        let (plugin, config) = metric_config_for(&specs, "squat", 12, 0.0).unwrap();
        assert_eq!(plugin, "reps_vision");
        assert_eq!(config["activity"], "lift");
        // downBelow is the (camera-tuned) spec value, not a hardcoded constant.
        assert_eq!(
            config["exercise"]["downBelow"],
            specs["exercises"]["squat"]["exercise"]["downBelow"]
        );
        assert!(config["exercise"]["downBelow"].is_number());
        assert_eq!(config["targetReps"], 12);
        assert_eq!(config["movementId"], "squat");
        assert_eq!(config["camera"]["source"], "index");
        assert_eq!(config["camera"]["id"], "webcam");
        // The rig is mounted 180°; frames must be rotated to match the tuned
        // thresholds (env REPS_CAMERA_ROTATE overrides; default 180).
        let expected_rotate: i64 = std::env::var("REPS_CAMERA_ROTATE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(180);
        assert_eq!(config["camera"]["rotate"], expected_rotate);
    }

    #[test]
    fn camera_set_spec_overrides_single_camera_config() {
        let specs = serde_json::json!({
            "model": {"plugin": "reps_vision"},
            "cameras": {
                "registry": [
                    {"cameraId": "front", "kind": "usb", "source": "v4l2:///dev/video0"},
                    {"cameraId": "side", "kind": "usb", "source": "v4l2:///dev/video1"}
                ],
                "set": ["front", "side"],
                "fusion": {"policy": "best", "scoreField": "visibility"}
            },
            "exercises": {"squat": {"activity": "lift",
                "exercise": {"name": "squat", "joints": ["hip","knee","ankle"],
                              "downBelow": 110.0, "upAbove": 160.0}}}
        });
        let (_, mut config) = metric_config_for(&specs, "squat", 5, 0.0).unwrap();
        let set = apply_camera_set(&specs, &mut config).unwrap();
        assert_eq!(set.cameras, vec!["front", "side"]);
        assert_eq!(set.registry.len(), 2);
        // exclusive with the injected single-camera object
        assert!(config.get("camera").is_none());
        assert_eq!(config["fusion"]["scoreField"], "visibility");
    }

    #[test]
    fn enable_on_hub_registers_cameras_then_enables_then_applies_overlays() {
        let specs = serde_json::json!({
            "model": {"plugin": "reps_vision"},
            "cameras": {
                "registry": [
                    {"cameraId": "front", "kind": "usb", "source": "v4l2:///dev/video0"},
                    {"cameraId": "side", "kind": "usb", "source": "v4l2:///dev/video1"}
                ],
                "set": ["front", "side"],
                "fusion": {"policy": "best", "scoreField": "visibility"},
                "overlays": {"side": {"exercise": {"downBelow": 95.0}}}
            },
            "exercises": {"squat": {"activity": "lift",
                "exercise": {"name": "squat", "joints": ["hip","knee","ankle"],
                              "downBelow": 110.0, "upAbove": 160.0}}}
        });
        let (plugin_id, mut config) = metric_config_for(&specs, "squat", 5, 0.0).unwrap();
        let camera_set = apply_camera_set(&specs, &mut config);
        let mut hub = hub_client::fake::FakeHub::new(vec![]);
        enable_on_hub(
            &mut hub,
            &camera_set,
            &EnableMetric {
                metric_id: WORKOUT_METRIC.into(),
                plugin_id,
                cameras: camera_set.as_ref().map(|set| set.cameras.clone()),
                config,
            },
        )
        .unwrap();
        // stale-stream teardown first, then registration, enable, overlays
        assert_eq!(
            hub.calls,
            vec![
                format!("disable:{WORKOUT_METRIC}"),
                "add_camera:front".to_string(),
                "add_camera:side".to_string(),
                format!("enable:{WORKOUT_METRIC}:reps_vision"),
                format!("update:{WORKOUT_METRIC}:side"),
            ]
        );
    }

    #[test]
    fn no_camera_set_leaves_single_camera_config_alone() {
        let specs = load_exercise_specs().unwrap();
        let (_, mut config) = metric_config_for(&specs, "squat", 5, 0.0).unwrap();
        assert!(apply_camera_set(&specs, &mut config).is_none());
        assert_eq!(config["camera"]["id"], "webcam");
    }

    #[test]
    fn metric_config_sets_duration_targets_for_continuous() {
        let specs = load_exercise_specs().unwrap();
        let (_, config) = metric_config_for(&specs, "jumprope", 0, 90.0).unwrap();
        assert_eq!(config["activity"], "jumprope");
        assert_eq!(config["targetSeconds"], 90.0);
    }

    #[test]
    fn unknown_exercise_yields_none() {
        let specs = load_exercise_specs().unwrap();
        assert!(metric_config_for(&specs, "wallsit", 0, 30.0).is_none());
    }

    #[test]
    fn custom_repetition_movement_requires_an_active_hub_version() {
        let specs = load_exercise_specs().unwrap();
        let (plugin, config) = metric_config_for(&specs, "my-curl", 8, 0.0).unwrap();
        assert_eq!(plugin, "reps_vision");
        assert_eq!(config["movementId"], "my-curl");
        assert_eq!(config["targetReps"], 8);
        assert!(config.get("exercise").is_none());
        assert!(metric_config_for(&specs, "", 8, 0.0).is_none());
    }

    #[test]
    fn manifest_declares_every_core_event_and_no_exposed_commands() {
        let manifest = reps_manifest();
        let events = manifest["events"].as_object().unwrap();
        for required in [
            "coding_period_started", "coding_period_expired", "workout_prescribed",
            "desktop_locked", "detector_started", "rep_completed", "target_reached",
            "weight_logged", "workout_completed", "detector_stopped",
            "desktop_unlocked", "override_used", "system_error",
        ] {
            assert!(events.contains_key(required), "manifest missing event {required}");
        }
        // V1: no reps command is remotely invokable until lock/camera control
        // has a safety story.
        for (name, command) in manifest["commands"].as_object().unwrap() {
            assert_eq!(command["exposed"], false, "command {name} must not be exposed");
        }
    }

    #[test]
    fn phase_transitions_map_to_the_v1_event_contract() {
        use engine::types::{ExerciseKind, Phase, Prescription};
        let rx = Prescription {
            exercise: "squat".into(),
            kind: ExerciseKind::Rep,
            target_reps: 10,
            target_seconds: 0.0,
            default_weight: 95.0,
        };
        let types = |prev: Option<&Phase>, next: &Phase| -> Vec<&'static str> {
            events_for_transition(prev, next, Some(&rx))
                .into_iter()
                .map(|(t, _)| t)
                .collect()
        };
        // the game loop, in order
        assert_eq!(types(None, &Phase::Coding), vec!["coding_period_started"]);
        assert_eq!(
            types(Some(&Phase::Coding), &Phase::ExerciseRequired),
            vec!["coding_period_expired", "workout_prescribed", "desktop_locked"],
        );
        // detector_started/stopped publish at the enable/disable sites, not here
        assert!(types(Some(&Phase::ExerciseRequired), &Phase::WorkoutActive).is_empty());
        assert!(types(Some(&Phase::WorkoutActive), &Phase::WeightConfirmation).is_empty());
        assert_eq!(
            types(Some(&Phase::WeightConfirmation), &Phase::Unlocked),
            vec!["workout_completed", "desktop_unlocked"],
        );
        // continuous activities (jumprope/stretch) skip weight confirmation
        assert_eq!(
            types(Some(&Phase::WorkoutActive), &Phase::Unlocked),
            vec!["workout_completed", "desktop_unlocked"],
        );
        assert_eq!(types(Some(&Phase::Unlocked), &Phase::Coding), vec!["coding_period_started"]);
        // prescription payload carries the exercise + targets
        let prescribed = events_for_transition(
            Some(&Phase::Coding),
            &Phase::ExerciseRequired,
            Some(&rx),
        );
        let (_, payload) = prescribed.iter().find(|(t, _)| *t == "workout_prescribed").unwrap();
        assert_eq!(payload["exercise"], "squat");
        assert_eq!(payload["targetReps"], 10);
    }
}


/// Wait for startup/enable operations, release capture, then drop our supervisor.
pub(crate) fn stop(app: &AppHandle) {
    PUBLICATIONS_ENABLED.store(false, std::sync::atomic::Ordering::SeqCst);
    DETECTOR_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
    app.state::<crate::Runtime>().detector_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    *SESSION_ID.lock().unwrap() = None;
    let state = app.state::<SharedHub>();
    if let Some(mut hub) = state.lock().unwrap().take() {
        let _ = hub.disable_metric(WORKOUT_METRIC);
        drop(hub);
    };
}
