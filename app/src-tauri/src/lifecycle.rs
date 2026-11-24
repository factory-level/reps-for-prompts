//! A workout's lifecycle: the one module that applies session transitions.
//! Entry points (Tauri commands, the control socket, the tick loop, the hub
//! event pump) are adapters that call a `Lifecycle` method; what a transition
//! causes outside the session leaves through the `Effects` seam.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use engine::clock::Clock;
use engine::session::Fact;
use engine::types::Snapshot;
use hub_client::outbox::Publication;
use serde_json::{json, Value};

use crate::detector::{self, DetectorInputs, HonorReason};
use crate::hub::{enable_on_hub, wire_events, SharedHub, APP_ID, WORKOUT_METRIC};
use crate::vision_status::{Signal, Vision, VisionStatus};
use crate::{Core, PlanState};

/// The outbound seam. `TauriEffects` is the desktop adapter; tests record.
pub trait Effects: Send + Sync {
    fn snapshot(&self, snapshot: &Snapshot);
    /// The vision status changed.
    fn vision(&self, status: VisionStatus);
    /// A camera frame for the live view, while the detector runs.
    fn camera_frame(&self, camera_id: &str, data: &Value);
    /// A durable event, command or action result for the hub's history.
    fn publish(&self, publication: Publication);
}

/// Reads what a detector request needs; tests supply it without the filesystem.
pub type LoadInputs = Box<dyn Fn() -> Result<DetectorInputs, HonorReason> + Send + Sync>;

#[derive(Default)]
struct Gate {
    /// One per lock cycle: minted at the lock, cleared once the return to
    /// coding is recorded. Progress frames must carry it to count.
    session_id: Option<String>,
    /// Bumped by every enable/disable request. A detector job acts, and its
    /// result counts, only while its number is still the current one.
    generation: u64,
    /// The detector was asked to run and has not been turned off or failed.
    active: bool,
}

#[derive(Clone)]
pub struct Lifecycle(Arc<Inner>);

/// `Lifecycle`'s shared state; reached only through `Lifecycle`.
pub struct Inner {
    core: Mutex<Core>,
    clock: Box<dyn Clock + Send + Sync>,
    hub: SharedHub,
    effects: Box<dyn Effects>,
    inputs: LoadInputs,
    /// Debug mode publishes nothing and mints a session per detector start.
    debug: bool,
    /// Run detector jobs on the calling thread (tests) instead of a new one.
    inline: bool,
    stopped: AtomicBool,
    gate: Mutex<Gate>,
    vision: Mutex<Vision>,
}

impl std::ops::Deref for Lifecycle {
    type Target = Inner;
    fn deref(&self) -> &Inner { &self.0 }
}

fn new_session_id() -> String {
    format!("sess-{}", uuid::Uuid::new_v4())
}

impl Lifecycle {
    pub fn new(
        core: Core,
        clock: Box<dyn Clock + Send + Sync>,
        hub: SharedHub,
        effects: Box<dyn Effects>,
        inputs: LoadInputs,
        debug: bool,
        inline: bool,
    ) -> Self {
        Self(Arc::new(Inner {
            core: Mutex::new(core), clock, hub, effects, inputs, debug, inline,
            stopped: AtomicBool::new(false), gate: Mutex::default(), vision: Mutex::default(),
        }))
    }

    /// Settings and storage reads, and configuration between workouts.
    /// Session transitions go through the methods below, never through this.
    pub(crate) fn core(&self) -> MutexGuard<'_, Core> {
        self.core.lock().unwrap()
    }

    /// The current state, without side effects.
    pub fn snapshot(&self) -> Snapshot {
        self.core().session.snapshot(self.clock.now())
    }

    pub fn vision_status(&self) -> VisionStatus {
        self.vision.lock().unwrap().status()
    }

    /// Feed the one vision status. When it changes, the windows and the CLI
    /// see it on display state. The lock is held across the effect so statuses
    /// arrive in the order they were decided.
    pub(crate) fn signal(&self, signal: Signal) {
        let mut vision = self.vision.lock().unwrap();
        if vision.apply(signal) { self.effects.vision(vision.status()); }
    }

    /// The app is exiting: no more publications, no detector job may act.
    pub(crate) fn shutdown(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        let mut gate = self.gate.lock().unwrap();
        gate.generation += 1;
        gate.active = false;
        gate.session_id = None;
    }

    pub(crate) fn persist(&self, core: &mut Core) -> Snapshot {
        persist_and_snapshot(core, self.clock.now())
    }

    pub(crate) fn emit(&self, snapshot: &Snapshot) {
        self.effects.snapshot(snapshot);
    }

    pub(crate) fn detector_active(&self) -> bool {
        self.gate.lock().unwrap().active
    }

    pub(crate) fn camera_frame(&self, camera_id: &str, data: &Value) {
        self.effects.camera_frame(camera_id, data);
    }

    pub(crate) fn session_id(&self) -> Option<String> {
        self.gate.lock().unwrap().session_id.clone()
    }

    // ---- Durable publishing ------------------------------------------------

    fn publish(&self, kind: fn(Value) -> Publication, session: Option<&str>, mut params: Value) {
        if self.debug || self.stopped.load(Ordering::SeqCst) { return; }
        if let Some(session) = session { params["sessionId"] = json!(session); }
        self.effects.publish(kind(params));
    }

    fn event(&self, session: Option<&str>, event_type: &str, payload: Value) {
        let id = format!("evt-{}", uuid::Uuid::new_v4());
        self.publish(Publication::Event, session, json!({"appId": APP_ID, "type": event_type, "id": id, "payload": payload}));
    }

    /// Publish the durable events for the facts a session transition returned,
    /// in order, and keep the session id in step with them.
    pub(crate) fn publish_facts(&self, facts: &[Fact]) {
        for fact in facts {
            // Each lock cycle is one session: minted at the lock, closed once
            // the return to coding is recorded.
            let session = {
                let mut gate = self.gate.lock().unwrap();
                if matches!(fact, Fact::Locked(_)) { gate.session_id = Some(new_session_id()); }
                gate.session_id.clone()
            };
            let session = session.as_deref();
            for (event_type, payload) in wire_events(fact) {
                if event_type == "desktop_locked" {
                    // The lock is a requested operation with a distinct outcome, not
                    // just an event: command → action result → the desktop_locked fact.
                    let command_id = format!("cmd-{}", uuid::Uuid::new_v4());
                    self.publish(Publication::Command, session, json!({"appId": APP_ID, "type": "desktop.lock_requested", "id": command_id, "payload": {}}));
                    self.publish(Publication::ActionResult, session, json!({
                        "appId": APP_ID, "id": format!("result-{}", uuid::Uuid::new_v4()), "type": "desktop.lock_succeeded",
                        "commandId": command_id, "status": "succeeded", "payload": {},
                    }));
                }
                self.event(session, event_type, payload);
            }
            if matches!(fact, Fact::CodingStarted) { self.gate.lock().unwrap().session_id = None; }
        }
    }

    // ---- Detector gating ---------------------------------------------------

    fn run(&self, job: impl FnOnce() + Send + 'static) {
        if self.inline { job() } else { std::thread::spawn(job); }
    }

    /// Ask the hub to run the detector for a prescription. Fire-and-forget; a
    /// failure signals the vision status so the UI offers honor mode.
    pub(crate) fn enable(&self, exercise: String, target_reps: u32, target_seconds: f64) {
        let (generation, session) = {
            let mut gate = self.gate.lock().unwrap();
            gate.generation += 1;
            if self.debug || gate.session_id.is_none() { gate.session_id = Some(new_session_id()); }
            (gate.generation, gate.session_id.clone().unwrap_or_default())
        };
        let this = self.clone();
        self.run(move || this.enable_now(generation, &session, &exercise, target_reps, target_seconds));
    }

    fn enable_now(&self, generation: u64, session: &str, exercise: &str, target_reps: u32, target_seconds: f64) {
        let built = (self.inputs)()
            .and_then(|inputs| detector::detector_request(&inputs, exercise, target_reps, target_seconds));
        // Err is (reason, whether it is recorded as a system_error).
        let result = match built {
            Err(HonorReason::BadSpecs(error)) => {
                eprintln!("hub: bad exercise specs: {error}");
                return;
            }
            Err(HonorReason::NoSpec(exercise)) => {
                eprintln!("hub: no spec for exercise {exercise}; honor mode");
                Err((format!("no spec for {exercise}"), true))
            }
            Err(HonorReason::PhoneUrlInvalid) => Err(("Cannot read phone stream credentials file".to_string(), false)),
            Err(HonorReason::Settings(error)) => Err((error, false)),
            Ok((mut request, camera_set)) => {
                let mut hub = self.hub.lock().unwrap();
                if self.gate.lock().unwrap().generation != generation { return; }
                request.config["sessionId"] = json!(session);
                match hub.as_mut() {
                    Some(hub) => {
                        self.gate.lock().unwrap().active = true;
                        enable_on_hub(hub.as_mut(), &camera_set, &request).map_err(|error| {
                            eprintln!("hub: enable_metric failed: {error}");
                            (error.to_string(), true)
                        })
                    }
                    None => Err(("hub not running".to_string(), true)),
                }
            }
        };
        // The camera did turn on, even if this request has since been superseded.
        if result.is_ok() { self.event(Some(session), "detector_started", json!({"exercise": exercise})); }
        // A superseded request's late result — success or failure — must not
        // touch detector state or the vision status: the newer request owns both.
        let mut gate = self.gate.lock().unwrap();
        if gate.generation != generation { return; }
        match result {
            // The hub answered, even if it was restarted since it last said so.
            Ok(()) => self.signal(Signal::HubUp),
            Err((error, recorded)) => {
                gate.active = false;
                if recorded {
                    self.event(Some(session), "system_error", json!({"stage": "enable_metric", "error": error}));
                }
                self.signal(Signal::EnableFailed(error));
            }
        }
    }

    /// Invalidate any detector job in flight and mark the detector off.
    fn detector_off(&self) -> (u64, Option<String>) {
        let mut gate = self.gate.lock().unwrap();
        gate.generation += 1;
        gate.active = false;
        self.signal(Signal::DetectorOff);
        (gate.generation, gate.session_id.clone())
    }

    /// Release the camera. Fire-and-forget.
    pub(crate) fn disable(&self) {
        let (generation, session) = self.detector_off();
        let this = self.clone();
        self.run(move || {
            let mut hub = this.hub.lock().unwrap();
            if this.gate.lock().unwrap().generation != generation { return; }
            let Some(running) = hub.as_mut() else { return };
            match running.disable_metric(WORKOUT_METRIC) {
                Err(error) => eprintln!("hub: disable_metric failed: {error}"),
                Ok(()) => {
                    drop(hub);
                    this.event(session.as_deref(), "detector_stopped", json!({}));
                }
            }
        });
    }

    /// Release the camera before returning, with no `detector_stopped` event:
    /// preview and debug teardown.
    pub(crate) fn release_camera(&self) {
        self.detector_off();
        if let Some(hub) = self.hub.lock().unwrap().as_mut() { let _ = hub.disable_metric(WORKOUT_METRIC); }
    }
}

pub(crate) fn persist_and_snapshot(core: &mut Core, now: f64) -> Snapshot {
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
    core.session.snapshot(now)
}
