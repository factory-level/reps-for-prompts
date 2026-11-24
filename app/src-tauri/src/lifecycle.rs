//! A workout's lifecycle: the one module that applies session transitions.
//! Entry points (Tauri commands, the control socket, the tick loop, the hub
//! event pump) are adapters that call a `Lifecycle` method; what a transition
//! causes outside the session leaves through the `Effects` seam.

use std::sync::{Mutex, MutexGuard};

use engine::clock::Clock;
use engine::types::Snapshot;

use crate::{Core, PlanState};

/// The outbound seam. `TauriEffects` is the desktop adapter; tests record.
pub trait Effects: Send + Sync {
    fn snapshot(&self, snapshot: &Snapshot);
}

pub struct Lifecycle {
    core: Mutex<Core>,
    clock: Box<dyn Clock + Send + Sync>,
    effects: Box<dyn Effects>,
}

impl Lifecycle {
    pub fn new(core: Core, clock: Box<dyn Clock + Send + Sync>, effects: Box<dyn Effects>) -> Self {
        Self { core: Mutex::new(core), clock, effects }
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

    pub(crate) fn persist(&self, core: &mut Core) -> Snapshot {
        persist_and_snapshot(core, self.clock.now())
    }

    pub(crate) fn emit(&self, snapshot: &Snapshot) {
        self.effects.snapshot(snapshot);
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
