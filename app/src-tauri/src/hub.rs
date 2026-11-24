//! Vision-hub integration: RFP declares its model and workout
//! configs (resources/exercise_specs.json) and drives the bundled hub via
//! the `VisionHub` trait — enable metric on workout start, pump events into
//! the session, disable on completion. Honor-mode fallback when the hub is
//! down: never strand the user, never phantom reps.

use std::sync::Mutex;

use engine::session::{Fact, Release};
use engine::types::Progress;
use hub_client::{EnableMetric, HubSupervisor, HubSupervisorConfig, VisionEvent, VisionHub};
use tauri::{AppHandle, Emitter, Manager};

use crate::detector::{self, CameraSet, CameraSettings, HonorReason};
use crate::vision_status::{Signal, Vision};

pub const WORKOUT_METRIC: &str = "workout";
pub const APP_ID: &str = "reps";

pub fn save_camera_settings(app: AppHandle, settings: CameraSettings) -> Result<(), String> {
    settings.validate()?;
    let path = app.state::<crate::Runtime>().normal_home.join("cameras.json");
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    std::fs::write(&temporary, serde_json::to_vec(&settings).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    std::fs::rename(temporary, path).map_err(|e| e.to_string())
}

pub type SharedHub = Mutex<Option<Box<dyn VisionHub>>>;
pub type SharedVision = Mutex<Vision>;

/// Feed the one vision status. When it changes, the windows and the CLI see
/// it on display state. The lock is held across the emit so statuses arrive
/// in the order they were decided.
fn signal(app: &AppHandle, signal: Signal) {
    let state = app.state::<SharedVision>();
    let mut vision = state.lock().unwrap();
    if vision.apply(signal) { crate::control::vision(app, vision.status()); }
}

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
    crate::control::error(app, format!("local event storage unavailable: {error}"));
}

/// Persist an app event locally, then publish asynchronously to the hub.
fn queue_event(event_type: &str, payload: serde_json::Value) {
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

/// The durable events for one engine fact — pure so the mapping is
/// unit-testable. `Started` maps to nothing: detector_started/stopped publish
/// at the enable/disable sites.
pub(crate) fn wire_events(fact: &Fact) -> Vec<(&'static str, serde_json::Value)> {
    use serde_json::json;
    match fact {
        Fact::CodingStarted => vec![("coding_period_started", json!({}))],
        Fact::Locked(rx) => vec![
            ("coding_period_expired", json!({})),
            ("workout_prescribed", json!({
                "exercise": rx.exercise,
                "targetReps": rx.target_reps,
                "targetSeconds": rx.target_seconds,
            })),
            ("desktop_locked", json!({})),
        ],
        Fact::Started => vec![],
        // Honor mode is visible in durable history, never a silent degradation.
        Fact::HonorUsed => vec![("override_used", json!({"mode": "honor"}))],
        Fact::WeightLogged { exercise, weight } => {
            vec![("weight_logged", json!({"exercise": exercise, "weight": weight}))]
        }
        Fact::Completed => vec![("workout_completed", json!({})), ("desktop_unlocked", json!({}))],
        Fact::Released(why) => {
            let reason = match why { Release::Cancel => "cancel", Release::Snooze => "snooze", Release::Skip => "skip" };
            vec![("desktop_unlocked", json!({"reason": reason}))]
        }
    }
}

/// Publish the durable events for the facts a session transition returned.
/// Every transition call site passes its facts here, in order. This is the one
/// place the "debug mode publishes nothing" rule is applied to the lifecycle.
pub(crate) fn publish_facts(facts: &[Fact]) {
    if !PUBLICATIONS_ENABLED.load(std::sync::atomic::Ordering::SeqCst) { return; }
    for fact in facts {
        // Each lock cycle is one session: minted at the lock, closed once the
        // return to coding is recorded.
        if matches!(fact, Fact::Locked(_)) { new_session_id(); }
        for (event_type, payload) in wire_events(fact) {
            if event_type == "desktop_locked" {
                // The lock is a requested operation with a distinct outcome, not
                // just an event: command → action result → the desktop_locked fact.
                let command_id = queue_command("desktop.lock_requested", serde_json::json!({}));
                queue_action_result("desktop.lock_succeeded", &command_id, "succeeded");
            }
            queue_event(event_type, payload);
        }
        if matches!(fact, Fact::CodingStarted) { *SESSION_ID.lock().unwrap() = None; }
    }
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
                        signal(&app, Signal::HubStartFailed(format!("Vision hub is not running ({err})")));
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
                    signal(&app, Signal::HubStartFailed(format!("Vision hub is not running ({error}); retrying in the background")));
                    eprintln!("hub: failed to start ({error}); honor mode, retrying in {}s", delay.as_secs_f32());
                } },
            );
            let state = app.state::<SharedHub>();
            let mut slot = state.lock().unwrap();
            if app.state::<crate::Runtime>().is_stopping() { return; }
            match supervisor {
                Some(mut supervisor) => {
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
                    signal(&app, Signal::HubUp);
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
                // A disconnected marker repeats the last value, so it credits nothing new.
                let disconnected = context.camera_status.as_deref() == Some("disconnected")
                    || context.consensus.as_ref().is_some_and(|c| c["state"] == "disconnected");
                signal(&app, if disconnected { Signal::CameraDisconnected } else { Signal::LiveFrame });
                let core_state = app.state::<crate::SharedCore>();
                let mut core = core_state.lock().unwrap();
                publish_facts(&core.session.report_progress(Progress { value, unit, satisfied }));
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
                    if DETECTOR_ACTIVE.load(std::sync::atomic::Ordering::SeqCst) { signal(&app, Signal::DetectorFailed(kind.clone())); }
                }
                if kind == "rep_completed" || kind == "target_reached" {
                    println!("[DETECT] ✓ {}", kind);
                }
            }
            VisionEvent::Health(health) => {
                if health.vision_host == "down" { signal(&app, Signal::HubDown); }
            }
            VisionEvent::ConnectionLost => signal(&app, Signal::ConnectionLost),
        }
    }
}

/// Enable the workout metric for the current prescription (fire-and-forget;
/// failures signal the vision status so the UI offers honor mode).
pub fn enable_metric_async(app: &AppHandle, exercise: String, target_reps: u32, target_seconds: f64) {
    let generation = app.state::<crate::Runtime>().detector_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    let app = app.clone();
    std::thread::spawn(move || {
        if app.state::<crate::Runtime>().is_stopping() { return; }
        let built = detector::load_inputs(&app.state::<crate::Runtime>().normal_home)
            .and_then(|inputs| detector::detector_request(&inputs, &exercise, target_reps, target_seconds));
        let (mut request, camera_set) = match built {
            Ok(built) => built,
            Err(HonorReason::BadSpecs(err)) => {
                eprintln!("hub: bad exercise specs: {err}");
                return;
            }
            Err(HonorReason::NoSpec(exercise)) => {
                eprintln!("hub: no spec for exercise {exercise}; honor mode");
                queue_event(
                    "system_error",
                    serde_json::json!({"stage": "enable_metric", "error": format!("no spec for {exercise}")}),
                );
                signal(&app, Signal::EnableFailed(format!("no spec for {exercise}")));
                return;
            }
            Err(HonorReason::PhoneUrlInvalid) => {
                signal(&app, Signal::EnableFailed("Cannot read phone stream credentials file".into()));
                return;
            }
            Err(HonorReason::Settings(error)) => {
                signal(&app, Signal::EnableFailed(error));
                return;
            }
        };
        let state = app.state::<SharedHub>();
        let mut guard = state.lock().unwrap();
        if app.state::<crate::Runtime>().is_stopping() ||
            app.state::<crate::Runtime>().detector_generation.load(std::sync::atomic::Ordering::SeqCst) != generation { return; }
        let sid = if app.state::<crate::Runtime>().is_debug() { new_session_id() } else { session_id().unwrap_or_else(new_session_id) };
        request.config["sessionId"] = serde_json::json!(sid);
        match guard.as_mut() {
            Some(hub) => {
                DETECTOR_ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
                match enable_on_hub(hub.as_mut(), &camera_set, &request) {
                    Ok(()) => {
                        drop(guard);
                        // The hub answered, even if it was restarted since it last said so.
                        signal(&app, Signal::HubUp);
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
                        signal(&app, Signal::EnableFailed(err.to_string()));
                    }
                }
            }
            None => {
                drop(guard);
                queue_event(
                    "system_error",
                    serde_json::json!({"stage": "enable_metric", "error": "hub not running"}),
                );
                signal(&app, Signal::EnableFailed("hub not running".into()));
            }
        }
    });
}

/// Disable the workout metric (camera released). Fire-and-forget.
pub fn disable_metric_async(app: &AppHandle) {
    let generation = app.state::<crate::Runtime>().detector_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    DETECTOR_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
    signal(app, Signal::DetectorOff);
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
    signal(app, Signal::DetectorOff);
    let state = app.state::<SharedHub>();
    if let Some(hub) = state.lock().unwrap().as_mut() { let _ = hub.disable_metric(WORKOUT_METRIC); };
}

/// Honor-mode completion: the camera path failed, the user pressed Done.
pub fn honor_complete(app: &AppHandle) -> Result<engine::types::Snapshot, String> {
    let core_state = app.state::<crate::SharedCore>();
    let mut core = core_state.lock().unwrap();
    let facts = core.session.honor_complete();
    if facts.is_empty() {
        return Err("No active workout to complete".into());
    }
    publish_facts(&facts);
    let snap = crate::persist_and_snapshot(&mut core);
    drop(core);
    crate::emit_snapshot(app, &snap);
    disable_metric_async(app);
    Ok(snap)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let inputs = detector::DetectorInputs {
            specs,
            settings: Ok(None),
            phone_url_override: None,
            calibration: serde_json::json!({}),
            camera_index: 0,
            camera_rotate: None,
        };
        let (request, camera_set) = detector::detector_request(&inputs, "squat", 5, 0.0).unwrap();
        let mut hub = hub_client::fake::FakeHub::new(vec![]);
        enable_on_hub(&mut hub, &camera_set, &request).unwrap();
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
    fn facts_map_to_the_v1_event_contract() {
        use engine::types::{ExerciseKind, Prescription};
        let rx = Prescription {
            exercise: "squat".into(),
            kind: ExerciseKind::Rep,
            target_reps: 10,
            target_seconds: 0.0,
            default_weight: 95.0,
        };
        let types = |facts: &[Fact]| -> Vec<&'static str> {
            facts.iter().flat_map(wire_events).map(|(t, _)| t).collect()
        };
        // the game loop, in order (detector_started/stopped publish at the
        // enable/disable sites, not here)
        let cycle = [
            Fact::CodingStarted,
            Fact::Locked(rx.clone()),
            Fact::Started,
            Fact::WeightLogged { exercise: "squat".into(), weight: 95.0 },
            Fact::Completed,
            Fact::CodingStarted,
        ];
        assert_eq!(types(&cycle), vec![
            "coding_period_started",
            "coding_period_expired", "workout_prescribed", "desktop_locked",
            "weight_logged",
            "workout_completed", "desktop_unlocked",
            "coding_period_started",
        ]);
        // prescription and weight payloads
        let locked = wire_events(&cycle[1]);
        assert_eq!(locked[1].1, serde_json::json!({"exercise": "squat", "targetReps": 10, "targetSeconds": 0.0}));
        assert_eq!(wire_events(&cycle[3])[0].1, serde_json::json!({"exercise": "squat", "weight": 95.0}));
        // a completion unlock carries no reason; a release says why
        assert_eq!(wire_events(&Fact::Completed)[1], ("desktop_unlocked", serde_json::json!({})));
        for (why, reason) in [(Release::Cancel, "cancel"), (Release::Snooze, "snooze"), (Release::Skip, "skip")] {
            assert_eq!(wire_events(&Fact::Released(why)), vec![("desktop_unlocked", serde_json::json!({"reason": reason}))]);
        }
        assert_eq!(wire_events(&Fact::HonorUsed), vec![("override_used", serde_json::json!({"mode": "honor"}))]);
        // every mapped type is declared in the manifest
        let manifest = reps_manifest();
        let all = [cycle.to_vec(), vec![Fact::HonorUsed, Fact::Released(Release::Cancel)]].concat();
        for t in types(&all) { assert!(manifest["events"].get(t).is_some(), "undeclared event {t}"); }
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
