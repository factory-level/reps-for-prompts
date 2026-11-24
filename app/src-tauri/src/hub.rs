//! Vision-hub integration: supervise the bundled hub, pump its events into
//! the `Lifecycle`, and deliver the lifecycle's publications through the
//! durable queue. What the hub is asked to do during a workout — enable the
//! detector on start, disable it on completion, honor mode when the hub is
//! down — is decided in `lifecycle.rs`.

use std::sync::{Arc, Mutex};

use engine::session::{Fact, Release};
use hub_client::{EnableMetric, HubSupervisor, HubSupervisorConfig, VisionEvent, VisionHub};
use tauri::{AppHandle, Manager};

use crate::detector::{CameraSet, CameraSettings};
use crate::vision_status::Signal;
use crate::Lifecycle;

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

/// The slot the supervised hub lands in once it is up; `None` until then.
pub type SharedHub = Arc<Mutex<Option<Box<dyn VisionHub>>>>;

fn signal(app: &AppHandle, signal: Signal) {
    app.state::<Lifecycle>().signal(signal);
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

static PUBLISHER: Mutex<Option<Publisher>> = Mutex::new(None);

/// Persist a publication locally, then publish asynchronously to the hub.
pub(crate) fn queue(mut message: Publish) {
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
        Fact::RoutineCompleted => vec![("routine_completed", json!({}))],
        Fact::Released(why) => {
            let reason = match why { Release::Cancel => "cancel", Release::Snooze => "snooze", Release::Skip => "skip" };
            vec![("desktop_unlocked", json!({"reason": reason}))]
        }
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

/// Feed the hub's event stream to the lifecycle; the terminal lines and the
/// display's last error are this adapter's own.
fn pump_events(app: AppHandle, rx: std::sync::mpsc::Receiver<VisionEvent>) {
    let life = app.state::<Lifecycle>();
    for event in rx {
        if app.state::<crate::Runtime>().is_stopping() { break; }
        match &event {
            // Landmarks are terminal-only now: nothing in the webviews listens,
            // and pushing them at camera rate into both windows leaked memory.
            VisionEvent::Landmarks(data) => print_detect(data),
            VisionEvent::Semantic { kind, payload } => {
                if kind == "detector_error" || kind == "stream_ended" {
                    crate::control::error(&app, format!("{kind}: {payload}"));
                }
                if kind == "rep_completed" || kind == "target_reached" {
                    println!("[DETECT] ✓ {}", kind);
                }
            }
            _ => {}
        }
        life.progress(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector;

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
            *hub.calls.lock().unwrap(),
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
    app.state::<Lifecycle>().shutdown();
    let state = app.state::<SharedHub>();
    if let Some(mut hub) = state.lock().unwrap().take() {
        let _ = hub.disable_metric(WORKOUT_METRIC);
        drop(hub);
    };
}
