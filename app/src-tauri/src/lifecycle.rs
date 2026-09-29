//! A workout's lifecycle: the one module that applies session transitions.
//! Entry points (Tauri commands, the control socket, the tick loop, the hub
//! event pump) are adapters that call a `Lifecycle` method; what a transition
//! causes outside the session leaves through the `Effects` seam.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use engine::clock::Clock;
use engine::session::{Fact, Release};
use engine::types::{Phase, Prescription, Progress, Snapshot};
use hub_client::outbox::Publication;
use hub_client::VisionEvent;
use serde_json::{json, Value};

use crate::detector::{self, DetectorInputs, HonorReason};
use crate::hub::{enable_on_hub, wire_events, SharedHub, APP_ID, WORKOUT_METRIC};
use crate::vision_status::{Signal, Vision, VisionStatus};
use crate::{Active, Core, PlanState};
use crate::training;

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
    /// The detector runs only to show the camera: its frames count for nothing.
    preview: bool,
    /// When the tick loop first saw the set complete (the "LOGGED" beat).
    unlocked_since: Option<f64>,
}

/// What the tick loop works out before each tick.
#[derive(Default)]
pub struct Tick {
    /// Seconds since the last tick that do not count as coding time.
    pub defer: f64,
    /// The coding period ends now: a snooze ran out, or the workday warning fired.
    pub expire: bool,
    /// Reminders may fire: nothing is snoozed.
    pub remind: bool,
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
    // REPS_WORK_MINUTES overrides for demos and tests (0.1 = a 6s interval).
    std::env::var("REPS_WORK_MINUTES").ok().and_then(|v| v.parse().ok())
        .or(core.active.as_ref().map(|a| a.coding_minutes))
        .unwrap_or_else(|| core.store.setting("work_minutes", "25").parse().unwrap_or(25.))
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

    /// The camera is on as a preview, outside any set.
    pub fn previewing(&self) -> bool {
        self.gate.lock().unwrap().preview
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
        let mut snapshot = persist_and_snapshot(core, now);
        if facts.contains(&Fact::RoutineCompleted) {
            complete_routine(core, &self.clock.today());
            snapshot = core.session.snapshot(now);
        }
        // Honor completion and a satisfied target both leave WorkoutActive.
        let left_active = was_active && snapshot.phase != Phase::WorkoutActive;
        if facts.contains(&Fact::Started) {
            if let Some(rx) = &snapshot.prescription {
                self.enable(rx.exercise.clone(), rx.target_reps, rx.target_seconds, false);
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

    /// The app is up: the coding timer starts now — there is no "start"
    /// button. Debug mode stays idle until a debug set is started.
    pub fn begin(&self) {
        if self.debug { return; }
        // A session left by a crash or restart comes back paused, camera off.
        let _ = self.step(|core, now, today| {
            if core.active.is_none() { return Err("idle".into()); }
            core.session.configure_timer(work_minutes(core), now);
            let facts = core.session.start(now, today);
            core.session.pause(now)?;
            Ok(facts)
        });
    }

    /// Start Session: run the next routine of the cycle from a fresh coding
    /// interval. One session at a time; a second call is rejected.
    pub fn start_session(&self) -> Result<Snapshot, String> {
        if self.debug { return Err("Switch to Workout mode first".into()); }
        self.preview_off();
        self.step(|core, now, today| {
            if core.session.snapshot(now).phase != Phase::Idle { return Err("A workout is already under way".into()); }
            let plan = training::load(&core.home, crate::ROUTINE_JSON);
            let (index, slot, routine) = training::next(&plan).ok_or("Configure a training plan first")?;
            let daily = engine::plan::DailyPlan::from_routine_json(&training::routine_json(routine), today)?;
            let active = Active {
                id: format!("wo-{}", uuid::Uuid::new_v4()), routine_id: routine.id.clone(), routine_name: routine.name.clone(),
                slot_id: slot.slot_id.clone(), slot_index: index, slot_count: plan.cycle.len(), coding_minutes: plan.coding_minutes,
            };
            core.store.insert_workout(&active.id, &active.routine_id, &active.routine_name, &active.slot_id, today).map_err(|e| e.to_string())?;
            core.store.set_setting("active_workout", &serde_json::to_string(&active).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            let _ = core.store.set_setting("snooze_until", "0");
            core.session.set_plan(daily);
            core.active = Some(active);
            core.last_completed = None;
            core.session.configure_timer(work_minutes(core), now);
            Ok(core.session.start(now, today))
        })
    }

    /// End the session early: confirmed sets stay, the cycle does not move.
    pub fn end_workout(&self) -> Snapshot {
        self.preview_off();
        self.step(|core, now, today| {
            let done = core.session.snapshot(now).day.map(|d| d.sets_done).unwrap_or(0);
            let facts = core.session.end();
            if let Some(active) = core.active.take() {
                let _ = core.store.finish_workout(&active.id, if done > 0 { "partial" } else { "canceled" }, today);
                let _ = core.store.set_setting("active_workout", "");
            }
            Ok(facts)
        }).unwrap_or_else(|_| self.snapshot())
    }

    pub fn pause(&self) -> Result<Snapshot, String> {
        self.step(|core, now, _| { core.session.pause(now)?; Ok(vec![]) })
    }

    pub fn resume(&self) -> Result<Snapshot, String> {
        self.step(|core, now, _| { core.session.resume(now); Ok(vec![]) })
    }

    /// Manual completion: the camera didn't (or can't) see it, the user says
    /// it's done. Rep sets go on to confirmation, where actual values are typed.
    pub fn manual_finish(&self) -> Result<Snapshot, String> {
        self.step(|core, now, _| {
            if core.session.snapshot(now).phase != Phase::WorkoutActive { return Err("No set is under way".into()); }
            Ok(core.session.honor_complete())
        })
    }

    /// Save the set with its actual reps and lbs. Commits once: a second
    /// call (another window, a retry) finds no confirmation pending.
    pub fn confirm_set(&self, weight: f64, reps: Option<u32>) -> Result<Snapshot, String> {
        if !weight.is_finite() || !(0.0..=100000.0).contains(&weight) { return Err("Enter a weight from 0 to 100000 lb".into()); }
        if reps.is_some_and(|r| r == 0 || r > 10000) { return Err("Reps must be 1 or more".into()); }
        self.step(|core, now, today| {
            if core.session.snapshot(now).phase != Phase::WeightConfirmation { return Err("This set was already saved".into()); }
            Ok(core.session.confirm_weight(weight, reps, today))
        })
    }

    /// Once a second: run the coding timer, lock when it expires, and return
    /// to coding three seconds after a completed set. Returns the snapshot
    /// and whether this tick locked.
    pub fn tick(&self, now: f64, today: &str, tick: Tick) -> (Snapshot, bool) {
        let mut core = self.core();
        core.session.defer_timer(tick.defer);
        if tick.expire { core.session.configure_timer(0., now); }
        let mut facts = if !self.debug && tick.remind { core.session.tick(now, today) } else { vec![] };
        let locked = !facts.is_empty();
        // The "LOGGED" beat: 3s in UNLOCKED, then CODE.
        let unlocked = !self.debug && core.session.snapshot(now).phase == Phase::Unlocked;
        let beat_over = {
            let mut gate = self.gate.lock().unwrap();
            let since = if unlocked { Some(*gate.unlocked_since.get_or_insert(now)) } else { None };
            let over = since.is_some_and(|since| now - since >= 3.0);
            if over || !unlocked { gate.unlocked_since = None; }
            over
        };
        if beat_over {
            facts.extend(core.session.resume_coding(now));
            if core.session.snapshot(now).phase == Phase::Coding {
                let minutes = work_minutes(&core);
                core.session.configure_timer(minutes, now);
            }
        }
        // Most ticks change nothing durable: only the countdown moves.
        let snapshot = if facts.is_empty() { core.session.snapshot(now) } else { self.apply(&mut core, now, false, &facts) };
        drop(core);
        self.effects.snapshot(&snapshot);
        (snapshot, locked)
    }

    /// Start the set that is owed: the one the lock prescribed, or — early,
    /// while coding — the next one of the day.
    pub fn start(&self) -> Result<Snapshot, String> {
        self.preview_off();
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
                Some(minutes) if (1..=60).contains(&minutes) => now + minutes as f64 * 60.,
                Some(_) => return Err("Choose 1 to 60 minutes".into()),
                None => 0.,
            };
            core.store.set_setting("snooze_until", &until.to_string()).map_err(|e| e.to_string())?;
            let facts = core.session.release(why, now);
            // Snooze means remind at expiry, rather than expiry plus a full interval.
            core.session.configure_timer(if until > 0. { 0. } else { work_minutes(core) }, now);
            Ok(facts)
        })
    }

    /// One event from the hub's stream. A progress frame counts only for the
    /// set it was measured for: the detector must be on for a set (not a
    /// preview) and the frame must carry this lock cycle's session id.
    pub fn progress(&self, event: VisionEvent) {
        match event {
            VisionEvent::Progress { value, unit, satisfied, context } => {
                // A disconnected marker repeats the last value, so it credits nothing new.
                let disconnected = context.camera_status.as_deref() == Some("disconnected")
                    || context.consensus.as_ref().is_some_and(|c| c["state"] == "disconnected");
                // Checked under the session lock, so a late frame cannot slip
                // into the set that follows a cancel or a preview.
                let _ = self.step(|core, _, _| {
                    let counts = {
                        let gate = self.gate.lock().unwrap();
                        gate.active && !gate.preview && context.belongs_to(WORKOUT_METRIC, gate.session_id.as_deref())
                    };
                    if !counts { return Err("ignored".into()); }
                    self.signal(if disconnected { Signal::CameraDisconnected } else { Signal::LiveFrame });
                    Ok(core.session.report_progress(Progress { value, unit, satisfied }))
                });
            }
            VisionEvent::CameraFrame { camera_id, data } => {
                if self.gate.lock().unwrap().active { self.effects.camera_frame(&camera_id, &data); }
            }
            VisionEvent::Semantic { kind, .. } => {
                if (kind == "detector_error" || kind == "stream_ended") && self.gate.lock().unwrap().active {
                    self.signal(Signal::DetectorFailed(kind));
                }
            }
            VisionEvent::Health(health) => {
                if health.vision_host == "down" { self.signal(Signal::HubDown); }
            }
            VisionEvent::ConnectionLost => self.signal(Signal::ConnectionLost),
            VisionEvent::Landmarks(_) => {}
        }
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
            facts.extend(core.session.confirm_weight(weight, None, today));
            Ok(facts)
        })
    }

    /// Leave the lock cycle without credit and go back to coding. The
    /// emergency escape: available in every phase, never rejected.
    pub fn cancel(&self) -> Snapshot {
        self.preview_off();
        self.step(|core, now, _| {
            let facts = core.session.release(Release::Cancel, now);
            core.session.configure_timer(work_minutes(core), now);
            Ok(facts)
        }).unwrap_or_else(|_| self.snapshot())
    }

    /// Show the camera. While coding that means running the detector as a
    /// preview (returns true); during a set the detector is already on.
    pub fn preview_on(&self) -> Result<bool, String> {
        let core = self.core();
        let phase = core.session.snapshot(self.clock.now()).phase;
        if !matches!(phase, Phase::Coding | Phase::WorkoutActive) {
            return Err("Finish, skip, or cancel the current break before starting camera preview".into());
        }
        let preview = phase == Phase::Coding;
        if preview { self.enable("squat".into(), 100000, 0., true); }
        Ok(preview)
    }

    /// End a preview and release its camera. A set's detector is not touched.
    pub fn preview_off(&self) {
        if self.previewing() { self.release_camera(); }
    }

    // ---- Debug mode's manual controls ---------------------------------------

    fn require_debug(&self) -> Result<(), String> {
        if self.debug { Ok(()) } else { Err("Manual testing requires Debug mode".into()) }
    }

    /// Start a set now, from any phase: the selected exercise, or — `None` —
    /// the next one owed after dropping the current set.
    pub fn debug_start(&self, exercise: Option<Prescription>) -> Result<Snapshot, String> {
        self.require_debug()?;
        self.release_camera();
        self.step(|core, now, today| Ok(match exercise {
            Some(prescription) => core.session.debug_start_exercise(prescription),
            None => {
                let mut facts = core.session.release(Release::Cancel, now);
                facts.extend(core.session.start_workout(today));
                facts
            }
        }))
    }

    /// Drop the current set and go back to coding.
    pub fn debug_stop(&self) -> Result<Snapshot, String> {
        self.require_debug()?;
        self.release_camera();
        self.step(|core, now, _| Ok(core.session.release(Release::Cancel, now)))
    }

    /// Stand in for the camera: report `value` reps (default: one more), or
    /// the target reached.
    pub fn debug_progress(&self, value: Option<f64>, satisfied: bool) -> Result<Snapshot, String> {
        self.require_debug()?;
        self.step(|core, now, _| {
            let snapshot = core.session.snapshot(now);
            if snapshot.phase != Phase::WorkoutActive { return Err("Start a debug workout first".into()); }
            let value = value.unwrap_or(snapshot.progress.map(|p| p.value).unwrap_or(0.) + 1.);
            Ok(core.session.report_progress(Progress { value, unit: "reps".into(), satisfied }))
        })
    }

    /// Hold a debug set in a detection state. Anything but `live` turns the
    /// camera off, so only step/done/next drive progress; `vision-down` also
    /// reports the vision path failed, as a dead detector would.
    pub fn debug_detection(&self, state: &str) -> Result<(), String> {
        self.require_debug()?;
        if state == "live" { return Ok(()); }
        self.release_camera();
        if state == "vision-down" { self.signal(Signal::DetectorFailed("debug: simulated vision failure".into())); }
        Ok(())
    }

    /// Advance exactly one phase through the engine's own transitions:
    /// code → locked → active → weight → logged → code.
    pub fn debug_next(&self, weight: Option<f64>) -> Result<Snapshot, String> {
        self.require_debug()?;
        self.step(|core, now, today| {
            let snapshot = core.session.snapshot(now);
            let facts = match snapshot.phase {
                Phase::Idle => return Err("No debug set yet. Run: rfp debug start --exercise NAME".into()),
                Phase::Coding => {
                    core.session.configure_timer(0., now);
                    core.session.tick(now, today)
                }
                Phase::ExerciseRequired => core.session.start_workout(today),
                Phase::WorkoutActive => {
                    let rx = snapshot.prescription.ok_or("The set has no prescription")?;
                    let (value, unit) = if rx.target_seconds > 0. { (rx.target_seconds, "seconds") } else { (rx.target_reps as f64, "reps") };
                    core.session.report_progress(Progress { value, unit: unit.into(), satisfied: true })
                }
                Phase::WeightConfirmation => {
                    let default = snapshot.prescription.map(|rx| rx.default_weight).unwrap_or(0.);
                    core.session.confirm_weight(weight.unwrap_or(default), None, today)
                }
                Phase::Unlocked => {
                    let facts = core.session.resume_coding(now);
                    if core.session.snapshot(now).phase == Phase::Coding {
                        let minutes = work_minutes(core);
                        core.session.configure_timer(minutes, now);
                    }
                    facts
                }
            };
            // A rep set reaching its target changes phase without a fact.
            if core.session.snapshot(now).phase == snapshot.phase { return Err(format!("Cannot advance from {:?}; nothing to prescribe", snapshot.phase)); }
            Ok(facts)
        })
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
    fn publish_facts(&self, facts: &[Fact]) {
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
    /// A preview runs under a session id of its own that is never the
    /// workout's, so none of its frames can count toward a set.
    fn enable(&self, exercise: String, target_reps: u32, target_seconds: f64, preview: bool) {
        let (generation, session) = {
            let mut gate = self.gate.lock().unwrap();
            gate.generation += 1;
            gate.preview = preview;
            let session = if preview { new_session_id() } else {
                if self.debug || gate.session_id.is_none() { gate.session_id = Some(new_session_id()); }
                gate.session_id.clone().unwrap_or_default()
            };
            (gate.generation, session)
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
        gate.preview = false;
        self.signal(Signal::DetectorOff);
        (gate.generation, gate.session_id.clone())
    }

    /// Release the camera. Fire-and-forget.
    fn disable(&self) {
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
    /// preview and debug teardown, and the debug video stream taking over.
    pub fn release_camera(&self) {
        self.detector_off();
        if let Some(hub) = self.hub.lock().unwrap().as_mut() { let _ = hub.disable_metric(WORKOUT_METRIC); }
    }
}

/// The routine's last set was saved: close the session as completed and move
/// the cycle on, exactly once (the session can only be closed while active).
fn complete_routine(core: &mut Core, today: &str) {
    let Some(active) = core.active.take() else { return };
    if core.store.finish_workout(&active.id, "completed", today).unwrap_or(false) {
        let mut plan = training::load(&core.home, crate::ROUTINE_JSON);
        training::advance(&mut plan, &active.slot_id);
        if let Err(error) = training::save(&core.home, &plan) { eprintln!("could not advance the cycle: {error}"); }
    }
    let _ = core.store.set_setting("active_workout", "");
    core.last_completed = Some(active.routine_name);
}

pub(crate) fn persist_and_snapshot(core: &mut Core, now: f64) -> Snapshot {
    if let Some(rec) = core.session.take_pending_record() {
        match core.store.record_set(&rec) {
            Ok(()) => if let Some(active) = &core.active { let _ = core.store.tag_last_set(&active.id); },
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
        let home = std::env::temp_dir().join(format!("rfp-life-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let core = Core { session, store: Store::open(std::path::Path::new(":memory:")).unwrap(), home, active: None, last_completed: None };
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

    /// Let `seconds` pass, then tick.
    fn tick(rig: &Rig, seconds: f64, tick: Tick) -> (Snapshot, bool) {
        rig.clock.0.lock().unwrap().advance(seconds);
        rig.life.tick(rig.clock.now(), DAY, tick)
    }

    fn reminding() -> Tick { Tick { remind: true, ..Tick::default() } }

    /// Let the coding timer run out.
    fn lock(rig: &Rig) -> Snapshot {
        tick(rig, 1500.0, reminding()).0
    }

    /// A progress frame as the hub stamps it for `session`.
    fn frame(session: Option<String>, value: f64, unit: &str, satisfied: bool) -> VisionEvent {
        VisionEvent::Progress {
            value, unit: unit.into(), satisfied,
            context: hub_client::ProgressContext { metric_id: Some(WORKOUT_METRIC.into()), session_id: session, ..Default::default() },
        }
    }

    fn session(rig: &Rig) -> Option<String> { rig.life.gate.lock().unwrap().session_id.clone() }

    /// The camera counts `value` reps of the current set.
    fn reps(rig: &Rig, value: f64, satisfied: bool) {
        rig.life.progress(frame(session(rig), value, "reps", satisfied));
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
        assert_eq!(rig.life.snooze(61).unwrap_err(), "Choose 1 to 60 minutes");

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

    #[test]
    fn a_preview_runs_under_its_own_session_and_start_takes_the_camera_over() {
        let rig = rig(LIFTS, Some(FakeHub::new(vec![])), false);
        assert_eq!(rig.life.preview_on(), Ok(true));
        assert!(rig.life.previewing() && rig.active());
        let preview_session = rig.wire()[0].1.clone();
        assert_eq!(rig.types(), ["detector_started"]);
        assert!(preview_session.is_some());
        // The preview's id is not the session's: there is no session while coding.
        assert!(rig.life.gate.lock().unwrap().session_id.is_none());

        start(&rig);
        assert!(!rig.life.previewing());
        let session = rig.life.gate.lock().unwrap().session_id.clone();
        assert!(session.is_some());
        assert_ne!(session, preview_session);
        assert!(rig.wire()[1..].iter().all(|(_, published)| *published == session));

        // During a set the camera is already on: showing it enables nothing.
        let calls = rig.calls().len();
        assert_eq!(rig.life.preview_on(), Ok(false));
        rig.life.preview_off();
        assert_eq!(rig.calls().len(), calls);
        assert!(rig.active());
    }

    #[test]
    fn preview_off_releases_the_camera_and_a_lock_refuses_preview() {
        let rig = rig(LIFTS, Some(FakeHub::new(vec![])), false);
        rig.life.preview_on().unwrap();
        rig.life.preview_off();
        assert!(!rig.life.previewing() && !rig.active());
        assert_eq!(rig.calls().last().unwrap(), "disable:workout");
        assert_eq!(rig.types(), ["detector_started"]);

        lock(&rig);
        assert_eq!(
            rig.life.preview_on().unwrap_err(),
            "Finish, skip, or cancel the current break before starting camera preview",
        );
    }

    #[test]
    fn a_verified_set_runs_start_to_finish_under_one_session() {
        let rig = rig(LIFTS, Some(FakeHub::new(vec![])), false);
        start(&rig);
        reps(&rig, 3.0, false);
        assert_eq!(rig.life.snapshot().progress.unwrap().value, 3.0);
        assert!(rig.active());

        reps(&rig, 5.0, true);
        // Target reached: the camera is released before the weight is asked for.
        assert_eq!(rig.life.snapshot().phase, Phase::WeightConfirmation);
        assert!(!rig.active());
        assert_eq!(rig.calls().last().unwrap(), "disable:workout");

        assert_eq!(rig.life.finish(95.0, false).unwrap().phase, Phase::Unlocked);
        assert_eq!(rig.verified(), vec![true]);
        assert_eq!(payload(&rig, "weight_logged"), json!({"exercise": "squat", "weight": 95.0}));
        assert_eq!(rig.types(), [&LOCK[..], &[
            "detector_started", "detector_stopped", "weight_logged", "workout_completed", "desktop_unlocked",
        ]].concat());
        let wire = rig.wire();
        assert!(wire[0].1.is_some());
        assert!(wire.iter().all(|(_, session)| *session == wire[0].1));
    }

    #[test]
    fn an_honor_set_does_not_make_the_next_set_unverified() {
        let rig = rig(LIFTS, Some(FakeHub::new(vec![])), false);
        start(&rig);
        rig.life.finish(95.0, true).unwrap();
        rig.life.cancel();
        assert_eq!(rig.verified(), vec![false]);

        start(&rig);
        reps(&rig, 5.0, true);
        rig.life.finish(95.0, false).unwrap();
        let mut verified = rig.verified();
        verified.sort();
        assert_eq!(verified, vec![false, true]);
        assert_eq!(rig.types().iter().filter(|event_type| *event_type == "override_used").count(), 1);
    }

    #[test]
    fn preview_frames_never_reach_a_set() {
        let rig = rig(LIFTS, Some(FakeHub::new(vec![])), false);
        rig.life.preview_on().unwrap();
        let preview_session = rig.wire()[0].1.clone();
        let emitted = rig.effects.0.lock().unwrap().snapshots.len();
        rig.life.progress(frame(preview_session.clone(), 100000.0, "reps", true));
        assert_eq!(rig.effects.0.lock().unwrap().snapshots.len(), emitted);

        start(&rig);
        // A frame the preview stream produced, arriving after the set began.
        rig.life.progress(frame(preview_session, 100000.0, "reps", true));
        rig.life.progress(frame(None, 5.0, "reps", true));
        let snapshot = rig.life.snapshot();
        assert_eq!(snapshot.phase, Phase::WorkoutActive);
        assert!(snapshot.progress.is_none());
        assert!(rig.active());
    }

    #[test]
    fn without_a_hub_the_set_is_unavailable_but_honor_still_completes_it() {
        for hub in [None, Some({ let mut hub = FakeHub::new(vec![]); hub.fail_next_enable = true; hub })] {
            let reason = if hub.is_some() { "hub is down" } else { "hub not running" };
            let rig = rig(LIFTS, hub, false);
            assert_eq!(start(&rig).phase, Phase::WorkoutActive);
            assert_eq!(rig.life.vision_status(), VisionStatus::Unavailable { reason: reason.into() });
            assert_eq!(rig.effects.0.lock().unwrap().vision.last().unwrap(), &rig.life.vision_status());
            assert_eq!(payload(&rig, "system_error"), json!({"stage": "enable_metric", "error": reason}));
            assert!(!rig.active());

            // No detector, so no frame is this set's: no phantom progress.
            reps(&rig, 5.0, true);
            assert!(rig.life.snapshot().progress.is_none());
            assert_eq!(rig.life.snapshot().phase, Phase::WorkoutActive);

            assert_eq!(rig.life.finish(95.0, true).unwrap().phase, Phase::Unlocked);
            assert_eq!(rig.verified(), vec![false]);
            assert_eq!(rig.types(), [&LOCK[..], &[
                "system_error", "override_used", "weight_logged", "workout_completed", "desktop_unlocked",
            ]].concat());
            // The set is over: the detector-level error goes with it.
            assert_ne!(rig.life.vision_status(), VisionStatus::Unavailable { reason: reason.into() });
        }
    }

    #[test]
    fn a_late_frame_after_cancel_is_ignored() {
        let rig = rig(LIFTS, Some(FakeHub::new(vec![])), false);
        start(&rig);
        let cancelled = session(&rig);
        rig.life.cancel();
        let emitted = rig.effects.0.lock().unwrap().snapshots.len();
        rig.life.progress(frame(cancelled.clone(), 5.0, "reps", true));
        assert_eq!(rig.effects.0.lock().unwrap().snapshots.len(), emitted);
        assert_eq!(rig.life.snapshot().phase, Phase::Coding);

        // Nor does it count toward the set started next.
        start(&rig);
        rig.life.progress(frame(cancelled, 5.0, "reps", true));
        assert_eq!(rig.life.snapshot().phase, Phase::WorkoutActive);
        assert!(rig.life.snapshot().progress.is_none());
    }

    #[test]
    fn a_superseded_enable_changes_nothing_when_it_finally_reports() {
        let rig = rig(LIFTS, Some(FakeHub::new(vec![])), false);
        start(&rig);
        let stale = rig.life.gate.lock().unwrap().generation - 1;
        let (published, calls) = (rig.wire().len(), rig.calls().len());

        // Its failure: no system_error, no "unavailable", the detector stays on.
        // (A timed set of an unknown exercise has no spec to run.)
        rig.life.enable_now(stale, "sess-stale", "no-such-exercise", 0, 30.0);
        // Its turn at the hub: it does not touch the running detector.
        rig.life.enable_now(stale, "sess-stale", "squat", 5, 0.0);

        assert_eq!(rig.life.vision_status(), VisionStatus::Available);
        assert!(rig.active());
        assert_eq!((rig.wire().len(), rig.calls().len()), (published, calls));
    }

    #[test]
    fn a_timed_set_completes_without_weight_confirmation() {
        let rig = rig(r#"{"conditioning":[{"exercise":"jumprope","seconds":60}]}"#, Some(FakeHub::new(vec![])), false);
        start(&rig);
        rig.life.progress(frame(session(&rig), 60.0, "seconds", true));
        assert_eq!(rig.life.snapshot().phase, Phase::Unlocked);
        assert!(!rig.active());
        let history = rig.life.core().store.history(10).unwrap();
        assert_eq!((history[0].exercise.as_str(), history[0].seconds, history[0].verified), ("jumprope", 60.0, true));
        assert_eq!(rig.types(), [&LOCK[..], &[
            "detector_started", "workout_completed", "desktop_unlocked", "routine_completed", "detector_stopped",
        ]].concat());
    }

    #[test]
    fn stream_trouble_reaches_the_vision_status_and_frames_flow_only_while_the_detector_runs() {
        let rig = rig(LIFTS, Some(FakeHub::new(vec![])), false);
        let camera = || VisionEvent::CameraFrame { camera_id: "webcam".into(), data: json!({}) };
        let ended = || VisionEvent::Semantic { kind: "stream_ended".into(), payload: json!({}) };
        rig.life.progress(camera());
        rig.life.progress(ended());
        assert_eq!(rig.effects.0.lock().unwrap().frames, 0);
        assert_eq!(rig.life.vision_status(), VisionStatus::Starting);

        start(&rig);
        rig.life.progress(camera());
        assert_eq!(rig.effects.0.lock().unwrap().frames, 1);
        rig.life.progress(ended());
        assert_eq!(rig.life.vision_status(), VisionStatus::Unavailable { reason: "stream_ended".into() });
        reps(&rig, 1.0, false);
        assert_eq!(rig.life.vision_status(), VisionStatus::Available);
        rig.life.progress(VisionEvent::ConnectionLost);
        assert_eq!(rig.life.vision_status(), VisionStatus::Unavailable { reason: "Vision hub connection lost".into() });
    }

    #[test]
    fn the_coding_timer_locks_only_when_reminders_may_fire() {
        let rig = rig(LIFTS, Some(FakeHub::new(vec![])), false);
        assert_eq!(tick(&rig, 1499.0, reminding()), (rig.life.snapshot(), false));
        // No agent at work, or snoozed: the expired timer waits.
        assert!(!tick(&rig, 1.0, Tick::default()).1);
        assert!(rig.types().is_empty());

        let (snapshot, locked) = tick(&rig, 1.0, reminding());
        assert!(locked);
        assert_eq!(snapshot.phase, Phase::ExerciseRequired);
        assert_eq!(snapshot.prescription.unwrap().exercise, "squat");
        assert_eq!(rig.types(), LOCK);
        // The lock waits for `start`: the camera stays off.
        assert!(rig.calls().is_empty());
        assert!(!tick(&rig, 1.0, reminding()).1);
        assert_eq!(rig.effects.0.lock().unwrap().snapshots.len(), 4);
    }

    #[test]
    fn deferred_time_does_not_count_and_an_expiry_locks_at_once() {
        let rig = rig(LIFTS, None, false);
        // The whole interval was spent away from coding.
        assert!(!tick(&rig, 1500.0, Tick { defer: 1500.0, ..reminding() }).1);
        assert_eq!(rig.life.snapshot().remaining_seconds, 1500.0);
        assert!(tick(&rig, 1.0, Tick { expire: true, ..reminding() }).1);
    }

    /// V2: an idle app, a two-routine cycle, one full session.
    fn v2_rig() -> Rig {
        let rig = rig(LIFTS, Some(FakeHub::new(vec![])), false);
        let _ = rig.life.end_workout(); // the rig starts mid-routine; V2 starts idle
        let home = rig.life.core().home.clone();
        let mut plan = training::seed(r#"{"name":"Upper A","lifts":[{"exercise":"squat","sets":2,"reps":5}]}"#);
        let mut lower = plan.routines[0].clone(); lower.id = "routine-2".into(); lower.name = "Lower A".into();
        plan.routines.push(lower);
        plan.cycle.push(training::Slot { slot_id: "slot-2".into(), routine_id: "routine-2".into() });
        training::save(&home, &plan).unwrap();
        rig
    }

    fn do_set(rig: &Rig, weight: f64) -> Result<Snapshot, String> {
        start(rig);
        rig.life.manual_finish().unwrap();
        rig.life.confirm_set(weight, Some(5))
    }

    #[test]
    fn a_full_session_completes_once_and_advances_the_cycle() {
        let rig = v2_rig();
        assert_eq!(rig.life.snapshot().phase, Phase::Idle);
        assert_eq!(rig.life.start_session().unwrap().phase, Phase::Coding);
        assert!(rig.life.start_session().is_err()); // one session at a time
        do_set(&rig, 35.5).unwrap();
        assert!(rig.life.confirm_set(35.5, None).is_err()); // a second save commits nothing
        tick(&rig, 1.0, reminding());
        tick(&rig, 3.0, reminding()); // the LOGGED beat, then the next interval
        assert_eq!(rig.life.snapshot().phase, Phase::Coding);
        do_set(&rig, 40.0).unwrap();
        let home = rig.life.core().home.clone();
        let core = rig.life.core();
        assert!(core.active.is_none());
        assert_eq!(core.last_completed.as_deref(), Some("Upper A"));
        let workouts = core.store.workouts_between("0000-01-01", "9999-12-31").unwrap();
        assert_eq!((workouts.len(), workouts[0]["status"].as_str(), workouts[0]["sets"].as_i64()), (1, Some("completed"), Some(2)));
        drop(core);
        assert_eq!(training::load(&home, crate::ROUTINE_JSON).next_slot, "slot-2");
        tick(&rig, 1.0, reminding());
        tick(&rig, 3.0, reminding());
        assert_eq!(rig.life.snapshot().phase, Phase::Idle);
        assert_eq!(rig.life.core().store.history(10).unwrap()[0].weight, 40.0);
    }

    #[test]
    fn ending_early_keeps_sets_without_credit_or_cycle_advance() {
        let rig = v2_rig();
        rig.life.start_session().unwrap();
        do_set(&rig, 35.0).unwrap();
        let snapshot = rig.life.end_workout();
        assert_eq!(snapshot.phase, Phase::Idle);
        let core = rig.life.core();
        let workouts = core.store.workouts_between("0000-01-01", "9999-12-31").unwrap();
        assert_eq!(workouts.last().unwrap()["status"], "partial");
        assert_eq!(core.store.history(10).unwrap().len(), 1);
        assert_eq!(training::load(&core.home, crate::ROUTINE_JSON).next_slot, "slot-1");
        assert!(!rig.active());
    }

    #[test]
    fn pause_freezes_reminders_until_resumed() {
        let rig = v2_rig();
        rig.life.start_session().unwrap();
        rig.life.pause().unwrap();
        let (snapshot, locked) = tick(&rig, 5000.0, reminding());
        assert!(!locked && snapshot.paused);
        rig.life.resume().unwrap();
        assert!(tick(&rig, 1500.0, reminding()).1);
    }

    #[test]
    fn three_seconds_after_a_completed_set_the_tick_returns_to_coding() {
        let rig = rig(LIFTS, Some(FakeHub::new(vec![])), false);
        lock(&rig);
        start(&rig);
        rig.life.finish(95.0, true).unwrap();
        let session = session(&rig);
        assert_eq!(tick(&rig, 1.0, reminding()).0.phase, Phase::Unlocked);
        assert_eq!(tick(&rig, 2.0, reminding()).0.phase, Phase::Unlocked);
        let (snapshot, locked) = tick(&rig, 1.0, reminding());
        assert_eq!((snapshot.phase, snapshot.remaining_seconds, locked), (Phase::Coding, 1500.0, false));
        // The return to coding closes the session it is recorded under.
        assert_eq!(rig.wire().last().unwrap(), &("coding_period_started".to_string(), session));
        assert!(self::session(&rig).is_none());
        assert_eq!(rig.types().iter().filter(|event_type| *event_type == "detector_stopped").count(), 1);
    }

    #[test]
    fn debug_mode_never_locks_and_publishes_nothing() {
        let rig = rig(LIFTS, Some(FakeHub::new(vec![])), true);
        assert!(!tick(&rig, 5000.0, reminding()).1);
        assert_eq!(rig.life.snapshot().phase, Phase::Coding);
        assert!(rig.types().is_empty());
    }

    #[test]
    fn debug_controls_drive_a_set_by_hand_and_publish_nothing() {
        let rig = rig(LIFTS, Some(FakeHub::new(vec![])), true);
        let squat = detector::debug_exercise_options().unwrap().into_iter().find(|rx| rx.exercise == "squat");
        assert_eq!(rig.life.debug_progress(None, false).unwrap_err(), "Start a debug workout first");

        assert_eq!(rig.life.debug_start(squat).unwrap().phase, Phase::WorkoutActive);
        assert!(rig.active());
        assert_eq!(rig.life.debug_progress(None, false).unwrap().progress.unwrap().value, 1.0);
        assert_eq!(rig.life.debug_progress(Some(7.0), false).unwrap().progress.unwrap().value, 7.0);
        assert_eq!(rig.life.debug_progress(None, true).unwrap().phase, Phase::WeightConfirmation);
        assert!(!rig.active());

        // Restart with the next set owed, then stop.
        assert_eq!(rig.life.debug_start(None).unwrap().phase, Phase::WorkoutActive);
        assert!(rig.active());
        assert_eq!(rig.life.debug_stop().unwrap().phase, Phase::Coding);
        assert!(!rig.active());
        assert!(rig.types().is_empty());

        let workout = self::rig(LIFTS, None, false);
        assert_eq!(workout.life.debug_start(None).unwrap_err(), "Manual testing requires Debug mode");
        assert_eq!(workout.life.debug_stop().unwrap_err(), "Manual testing requires Debug mode");
        assert_eq!(workout.life.debug_progress(None, true).unwrap_err(), "Manual testing requires Debug mode");
        assert_eq!(workout.life.debug_next(None).unwrap_err(), "Manual testing requires Debug mode");
    }

    /// `debug next` until the phase comes back around; returns the phases seen.
    fn walk(rig: &Rig, weight: Option<f64>) -> Vec<Phase> {
        let mut phases = vec![rig.life.snapshot().phase];
        while phases.len() == 1 || *phases.last().unwrap() != Phase::Coding {
            phases.push(rig.life.debug_next(weight).unwrap().phase);
            // The camera runs exactly while the set does: gated by the facts.
            assert_eq!(rig.active(), *phases.last().unwrap() == Phase::WorkoutActive);
        }
        phases
    }

    #[test]
    fn debug_next_walks_a_weighted_lift_from_coding_back_to_coding() {
        let rig = rig(LIFTS, Some(FakeHub::new(vec![])), true);
        assert_eq!(walk(&rig, Some(42.5)), [
            Phase::Coding, Phase::ExerciseRequired, Phase::WorkoutActive,
            Phase::WeightConfirmation, Phase::Unlocked, Phase::Coding,
        ]);
        let history = rig.life.core().store.history(10).unwrap();
        assert_eq!((history[0].exercise.as_str(), history[0].reps, history[0].weight), ("squat", 5, 42.5));
        assert_eq!(rig.life.snapshot().remaining_seconds, 1500.0);
        assert!(rig.types().is_empty());
    }

    #[test]
    fn debug_next_walks_a_timed_set_without_weight_confirmation() {
        let rig = rig(r#"{"conditioning":[{"exercise":"jumprope","seconds":60},{"exercise":"jumprope","seconds":60}]}"#, Some(FakeHub::new(vec![])), true);
        assert_eq!(walk(&rig, None), [
            Phase::Coding, Phase::ExerciseRequired, Phase::WorkoutActive, Phase::Unlocked, Phase::Coding,
        ]);
        let history = rig.life.core().store.history(10).unwrap();
        assert_eq!((history[0].exercise.as_str(), history[0].seconds), ("jumprope", 60.0));
    }

    #[test]
    fn debug_next_from_idle_says_to_start_a_debug_set() {
        let rig = rig(LIFTS, None, true);
        let _ = rig.life.end_workout();
        assert_eq!(rig.life.snapshot().phase, Phase::Idle);
        assert_eq!(rig.life.debug_next(None).unwrap_err(), "No debug set yet. Run: rfp debug start --exercise NAME");
    }

    #[test]
    fn debug_detection_states_turn_the_camera_off_and_vision_down_is_unavailable() {
        let rig = rig(LIFTS, Some(FakeHub::new(vec![])), true);
        let squat = || detector::debug_exercise_options().unwrap().into_iter().find(|rx| rx.exercise == "squat");
        rig.life.debug_start(squat()).unwrap();
        rig.life.debug_detection("live").unwrap();
        assert!(rig.active());

        rig.life.debug_detection("counting").unwrap();
        assert!(!rig.active());
        assert_eq!(rig.calls().last().unwrap(), "disable:workout");
        assert_eq!(rig.life.vision_status(), VisionStatus::Available);

        rig.life.debug_start(squat()).unwrap();
        rig.life.debug_detection("vision-down").unwrap();
        let down = VisionStatus::Unavailable { reason: "debug: simulated vision failure".into() };
        assert_eq!(rig.life.vision_status(), down);
        assert_eq!(rig.effects.0.lock().unwrap().vision.last().unwrap(), &down);
        // Progress still comes from the CLI, and the end of the set clears it.
        rig.life.debug_next(None).unwrap();
        assert_ne!(rig.life.vision_status(), down);
    }
}
