//! A workout's lifecycle: the one module that applies session transitions.
//! Entry points (Tauri commands, the control socket, the tick loop, the hub
//! event pump) are adapters that call a `Lifecycle` method; what a transition
//! causes outside the session leaves through the `Effects` seam.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use engine::clock::Clock;
use engine::session::{Fact, Release};
use engine::types::{Phase, Snapshot};
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

/// The coding interval the timer returns to after a break.
fn work_minutes(core: &Core) -> f64 {
    core.store.setting("work_minutes", "25").parse().unwrap_or(25.)
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

    // ---- The step ----------------------------------------------------------

    /// What every transition causes, in one place: publish its facts, persist,
    /// and gate the detector. The camera runs only while a set is under way —
    /// `Started` turns it on; reaching the target, completing, releasing or
    /// returning to coding turns it off. (Found live: a completion path that
    /// turned the camera off by hand, and one that forgot to.)
    fn apply(&self, core: &mut Core, now: f64, was_active: bool, facts: &[Fact]) -> Snapshot {
        self.publish_facts(facts);
        let snapshot = persist_and_snapshot(core, now);
        // Honor completion and a satisfied target both leave WorkoutActive.
        let left_active = was_active && snapshot.phase != Phase::WorkoutActive;
        if facts.contains(&Fact::Started) {
            if let Some(rx) = &snapshot.prescription {
                self.enable(rx.exercise.clone(), rx.target_reps, rx.target_seconds);
            }
        } else if left_active
            || facts.iter().any(|fact| matches!(fact, Fact::Completed | Fact::Released(_) | Fact::CodingStarted))
        {
            self.disable();
        }
        snapshot
    }

    /// Apply one session transition under the lock, then tell the windows.
    /// `Err` from the transition means rejected: nothing else happens.
    fn step(
        &self,
        transition: impl FnOnce(&mut Core, f64, &str) -> Result<Vec<Fact>, String>,
    ) -> Result<Snapshot, String> {
        let (now, today) = (self.clock.now(), self.clock.today());
        let mut core = self.core();
        let was_active = core.session.snapshot(now).phase == Phase::WorkoutActive;
        let facts = transition(&mut core, now, &today)?;
        let snapshot = self.apply(&mut core, now, was_active, &facts);
        drop(core);
        self.effects.snapshot(&snapshot);
        Ok(snapshot)
    }

    // ---- Entry points ------------------------------------------------------

    /// Start the set that is owed: the one the lock prescribed, or — early,
    /// while coding — the next one of the day.
    pub fn start(&self) -> Result<Snapshot, String> {
        if self.debug { return Err("Switch to Workout mode first".into()); }
        self.step(|core, now, today| {
            let phase = core.session.snapshot(now).phase;
            let facts = core.session.start_workout(today);
            if facts.is_empty() {
                return Err(if phase == Phase::Coding { "Today's routine is complete; nothing to start" } else { "A workout is already in progress" }.into());
            }
            // Starting a set ends a snooze. The set is under way either way.
            if let Err(error) = core.store.set_setting("snooze_until", "0") { eprintln!("failed to clear snooze: {error}"); }
            Ok(facts)
        })
    }

    /// Put the reminder off for 5, 15 or 30 minutes.
    pub fn snooze(&self, minutes: u32) -> Result<Snapshot, String> {
        self.put_off(Release::Snooze, Some(minutes))
    }

    /// Skip this reminder; the next one comes after a full coding interval.
    pub fn skip(&self) -> Result<Snapshot, String> {
        self.put_off(Release::Skip, None)
    }

    fn put_off(&self, why: Release, snooze_minutes: Option<u32>) -> Result<Snapshot, String> {
        if self.debug { return Err("Switch to Workout mode first".into()); }
        self.step(|core, now, _| {
            if !matches!(core.session.snapshot(now).phase, Phase::Coding | Phase::ExerciseRequired) {
                return Err("Finish or stop the current workout first".into());
            }
            let until = match snooze_minutes {
                Some(minutes) if [5, 15, 30].contains(&minutes) => now + minutes as f64 * 60.,
                Some(_) => return Err("Choose 5, 15, or 30 minutes".into()),
                None => 0.,
            };
            core.store.set_setting("snooze_until", &until.to_string()).map_err(|e| e.to_string())?;
            let facts = core.session.release(why, now);
            // Snooze means remind at expiry, rather than expiry plus a full interval.
            core.session.configure_timer(if until > 0. { 0. } else { work_minutes(core) }, now);
            Ok(facts)
        })
    }

    /// Complete the set: log the weight once the target was reached, or —
    /// `honor` — attest a set the camera could not verify. Honor completion
    /// always works during an active workout.
    pub fn finish(&self, weight: f64, honor: bool) -> Result<Snapshot, String> {
        self.step(|core, now, today| {
            let phase = core.session.snapshot(now).phase;
            if !matches!(phase, Phase::WorkoutActive | Phase::WeightConfirmation) {
                return Err("No active workout to finish. Run: reps start".into());
            }
            if phase == Phase::WorkoutActive && !honor {
                return Err("Target not yet detected. Use --honor only if you completed the prescribed set yourself".into());
            }
            // Each is a no-op outside its phase.
            let mut facts = core.session.honor_complete();
            facts.extend(core.session.confirm_weight(weight, today));
            Ok(facts)
        })
    }

    /// Leave the lock cycle without credit and go back to coding. The
    /// emergency escape: available in every phase, never rejected.
    pub fn cancel(&self) -> Snapshot {
        self.step(|core, now, _| {
            let facts = core.session.release(Release::Cancel, now);
            core.session.configure_timer(work_minutes(core), now);
            Ok(facts)
        }).unwrap_or_else(|_| self.snapshot())
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

#[cfg(test)]
mod tests {
    use super::*;
    use engine::clock::FakeClock;
    use engine::session::Session;
    use engine::store::{default_continuous_pool, default_rotation, Store};
    use engine::timer::CodingTimer;
    use engine::workout::WorkoutEngine;
    use hub_client::FakeHub;

    const DAY: &str = "2026-11-23";
    const LIFTS: &str = r#"{"lifts":[{"exercise":"squat","sets":3,"reps":5}]}"#;

    #[derive(Default)]
    struct Recorded {
        snapshots: Vec<Snapshot>,
        vision: Vec<VisionStatus>,
        frames: usize,
        published: Vec<Publication>,
    }

    #[derive(Clone, Default)]
    struct Recording(Arc<Mutex<Recorded>>);

    impl Effects for Recording {
        fn snapshot(&self, snapshot: &Snapshot) { self.0.lock().unwrap().snapshots.push(snapshot.clone()); }
        fn vision(&self, status: VisionStatus) { self.0.lock().unwrap().vision.push(status); }
        fn camera_frame(&self, _camera_id: &str, _data: &Value) { self.0.lock().unwrap().frames += 1; }
        fn publish(&self, publication: Publication) { self.0.lock().unwrap().published.push(publication); }
    }

    #[derive(Clone)]
    struct SharedClock(Arc<Mutex<FakeClock>>);

    impl Clock for SharedClock {
        fn now(&self) -> f64 { self.0.lock().unwrap().now() }
        fn today(&self) -> String { self.0.lock().unwrap().today() }
    }

    struct Rig {
        life: Lifecycle,
        effects: Recording,
        clock: SharedClock,
        /// The fake hub's call log; stays empty when the hub is absent.
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl Rig {
        /// (type, session id) of everything published, in order.
        fn wire(&self) -> Vec<(String, Option<String>)> {
            self.effects.0.lock().unwrap().published.iter().map(|publication| {
                let (Publication::Event(p) | Publication::Command(p) | Publication::ActionResult(p)) = publication;
                (p["type"].as_str().unwrap().to_string(), p["sessionId"].as_str().map(str::to_string))
            }).collect()
        }
        fn types(&self) -> Vec<String> { self.wire().into_iter().map(|(event_type, _)| event_type).collect() }
        fn calls(&self) -> Vec<String> { self.calls.lock().unwrap().clone() }
        fn active(&self) -> bool { self.life.gate.lock().unwrap().active }
        fn verified(&self) -> Vec<bool> {
            self.life.core().store.history(10).unwrap().iter().map(|record| record.verified).collect()
        }
    }

    fn rig(routine: &str, hub: Option<FakeHub>, debug: bool) -> Rig {
        let mut session = Session::new(
            CodingTimer::new(1500.0),
            WorkoutEngine::new(default_rotation(), default_continuous_pool(), 20),
        );
        session.set_plan(engine::plan::DailyPlan::from_routine_json(routine, DAY).unwrap());
        let _ = session.start(1000.0, DAY);
        let core = Core { session, store: Store::open(std::path::Path::new(":memory:")).unwrap() };
        let calls = hub.as_ref().map(|hub| hub.calls.clone()).unwrap_or_default();
        let hub: SharedHub = Arc::new(Mutex::new(hub.map(|hub| Box::new(hub) as Box<dyn hub_client::VisionHub>)));
        let effects = Recording::default();
        let clock = SharedClock(Arc::new(Mutex::new(FakeClock::new(1000.0, DAY))));
        let inputs: LoadInputs = Box::new(|| Ok(DetectorInputs {
            specs: detector::load_exercise_specs().unwrap(),
            settings: Ok(None),
            phone_url_override: None,
            calibration: json!({}),
            camera_index: 0,
            camera_rotate: None,
        }));
        let life = Lifecycle::new(core, Box::new(clock.clone()), hub, Box::new(effects.clone()), inputs, debug, true);
        Rig { life, effects, clock, calls }
    }

    fn start(rig: &Rig) -> Snapshot {
        rig.life.start().unwrap()
    }

    /// Let the coding timer run out.
    fn lock(rig: &Rig) -> Snapshot {
        rig.clock.0.lock().unwrap().advance(1500.0);
        rig.life.step(|core, now, today| Ok(core.session.tick(now, today))).unwrap()
    }

    fn payload(rig: &Rig, event_type: &str) -> Value {
        rig.effects.0.lock().unwrap().published.iter().rev().find_map(|publication| match publication {
            Publication::Event(p) if p["type"] == event_type => Some(p["payload"].clone()),
            _ => None,
        }).unwrap()
    }

    const LOCK: [&str; 5] = [
        "coding_period_expired", "workout_prescribed",
        "desktop.lock_requested", "desktop.lock_succeeded", "desktop_locked",
    ];

    #[test]
    fn honor_finish_records_an_unverified_set_and_releases_the_camera() {
        let rig = rig(LIFTS, Some(FakeHub::new(vec![])), false);
        assert_eq!(start(&rig).phase, Phase::WorkoutActive);
        assert!(rig.active());

        let snapshot = rig.life.finish(95.0, true).unwrap();
        assert_eq!(snapshot.phase, Phase::Unlocked);
        assert_eq!(rig.verified(), vec![false]);
        assert!(!rig.active());
        assert_eq!(rig.calls().last().unwrap(), "disable:workout");
        assert_eq!(rig.types(), [&LOCK[..], &[
            "detector_started", "override_used", "weight_logged",
            "workout_completed", "desktop_unlocked", "detector_stopped",
        ]].concat());
        // One session id on every event of the lock cycle.
        let wire = rig.wire();
        assert!(wire[0].1.is_some());
        assert!(wire.iter().all(|(_, session)| *session == wire[0].1));
        assert_eq!(rig.effects.0.lock().unwrap().snapshots.last().unwrap().phase, Phase::Unlocked);
    }

    #[test]
    fn finishing_outside_an_active_workout_is_rejected_and_publishes_nothing() {
        let rig = rig(LIFTS, Some(FakeHub::new(vec![])), false);
        for honor in [false, true] {
            assert_eq!(rig.life.finish(95.0, honor).unwrap_err(), "No active workout to finish. Run: reps start");
        }
        start(&rig);
        let published = rig.wire().len();
        assert_eq!(
            rig.life.finish(95.0, false).unwrap_err(),
            "Target not yet detected. Use --honor only if you completed the prescribed set yourself",
        );
        assert_eq!(rig.wire().len(), published);
        assert_eq!(rig.verified(), Vec::<bool>::new());
        assert!(rig.active());
        assert_eq!(rig.life.snapshot().phase, Phase::WorkoutActive);
    }

    #[test]
    fn cancel_mid_set_releases_the_camera_and_ends_the_session() {
        let rig = rig(LIFTS, Some(FakeHub::new(vec![])), false);
        start(&rig);
        let snapshot = rig.life.cancel();
        assert_eq!(snapshot.phase, Phase::Coding);
        assert_eq!(snapshot.remaining_seconds, 1500.0);
        assert!(!rig.active());
        assert_eq!(rig.calls().last().unwrap(), "disable:workout");
        assert_eq!(rig.types(), [&LOCK[..], &[
            "detector_started", "desktop_unlocked", "coding_period_started", "detector_stopped",
        ]].concat());
        assert_eq!(payload(&rig, "desktop_unlocked"), json!({"reason": "cancel"}));
        // The session closes with the return to coding.
        assert!(rig.life.gate.lock().unwrap().session_id.is_none());
        assert_eq!(rig.verified(), Vec::<bool>::new());

        // Nothing to cancel while coding: no events, no hub call.
        let (published, calls) = (rig.wire().len(), rig.calls().len());
        assert_eq!(rig.life.cancel().phase, Phase::Coding);
        assert_eq!((rig.wire().len(), rig.calls().len()), (published, calls));
    }

    #[test]
    fn each_lock_cycle_gets_its_own_session_id() {
        let rig = rig(LIFTS, Some(FakeHub::new(vec![])), false);
        start(&rig);
        rig.life.finish(95.0, true).unwrap();
        // From Unlocked, cancel is just the return to coding.
        assert_eq!(rig.life.cancel().phase, Phase::Coding);
        start(&rig);
        let sessions: Vec<Option<String>> = rig.wire().into_iter()
            .filter(|(event_type, _)| event_type == "workout_prescribed").map(|(_, session)| session).collect();
        assert_eq!(sessions.len(), 2);
        assert!(sessions[0].is_some() && sessions[1].is_some());
        assert_ne!(sessions[0], sessions[1]);
    }

    #[test]
    fn start_locks_starts_and_enables_the_detector_for_the_prescription() {
        let rig = rig(LIFTS, Some(FakeHub::new(vec![])), false);
        let snapshot = start(&rig);
        assert_eq!(snapshot.phase, Phase::WorkoutActive);
        assert_eq!(payload(&rig, "workout_prescribed"), json!({"exercise": "squat", "targetReps": 5, "targetSeconds": 0.0}));
        assert_eq!(payload(&rig, "detector_started"), json!({"exercise": "squat"}));
        assert_eq!(rig.types(), [&LOCK[..], &["detector_started"]].concat());
        // Stale-stream teardown, then the enable.
        assert_eq!(rig.calls()[0], "disable:workout");
        assert!(rig.calls().last().unwrap().starts_with("enable:workout:"));
        assert_eq!(rig.life.vision_status(), VisionStatus::Available);
        assert_eq!(rig.life.core().store.setting("snooze_until", ""), "0");

        let published = rig.wire().len();
        assert_eq!(rig.life.start().unwrap_err(), "A workout is already in progress");
        assert_eq!(rig.wire().len(), published);
    }

    #[test]
    fn start_is_rejected_when_the_day_is_done_or_in_debug_mode() {
        let rig_done = rig(r#"{"lifts":[{"exercise":"squat","sets":1,"reps":5}]}"#, None, false);
        start(&rig_done);
        rig_done.life.finish(95.0, true).unwrap();
        rig_done.life.cancel();
        assert_eq!(rig_done.life.start().unwrap_err(), "Today's routine is complete; nothing to start");

        let debug = rig(LIFTS, None, true);
        assert_eq!(debug.life.start().unwrap_err(), "Switch to Workout mode first");
        assert_eq!(debug.life.skip().unwrap_err(), "Switch to Workout mode first");
    }

    #[test]
    fn snooze_unlocks_with_its_reason_and_reminds_at_expiry() {
        let rig = rig(LIFTS, Some(FakeHub::new(vec![])), false);
        assert_eq!(lock(&rig).phase, Phase::ExerciseRequired);
        assert_eq!(rig.life.snooze(7).unwrap_err(), "Choose 5, 15, or 30 minutes");

        let snapshot = rig.life.snooze(15).unwrap();
        assert_eq!(snapshot.phase, Phase::Coding);
        assert_eq!(snapshot.remaining_seconds, 0.0);
        assert_eq!(rig.life.core().store.setting("snooze_until", ""), (2500.0 + 900.0).to_string());
        assert_eq!(rig.types(), [&LOCK[..], &["desktop_unlocked", "coding_period_started"]].concat());
        assert_eq!(payload(&rig, "desktop_unlocked"), json!({"reason": "snooze"}));
    }

    #[test]
    fn skip_unlocks_with_its_reason_and_restarts_the_full_interval() {
        let rig = rig(LIFTS, Some(FakeHub::new(vec![])), false);
        lock(&rig);
        let snapshot = rig.life.skip().unwrap();
        assert_eq!(snapshot.phase, Phase::Coding);
        assert_eq!(snapshot.remaining_seconds, 1500.0);
        assert_eq!(rig.life.core().store.setting("snooze_until", ""), "0");
        assert_eq!(payload(&rig, "desktop_unlocked"), json!({"reason": "skip"}));

        // Not during a set: the way out of a set is finish or cancel.
        start(&rig);
        let published = rig.wire().len();
        for rejected in [rig.life.skip(), rig.life.snooze(5)] {
            assert_eq!(rejected.unwrap_err(), "Finish or stop the current workout first");
        }
        assert_eq!(rig.wire().len(), published);
        assert!(rig.active());
    }
}
