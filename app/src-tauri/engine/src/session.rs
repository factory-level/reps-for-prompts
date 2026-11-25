//! The application state machine (V1 spec §20):
//! CODING → EXERCISE_REQUIRED → WORKOUT_ACTIVE → WEIGHT_CONFIRMATION → UNLOCKED → CODING.
//! Pure: driven by tick()/commands with explicit time, no threads, no IO.

use crate::plan::DailyPlan;
use crate::timer::CodingTimer;
use crate::types::{ExerciseKind, Phase, Prescription, Progress, SetRecord, Snapshot};
use crate::workout::WorkoutEngine;

/// Why a lock cycle ended without a completed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Release { Cancel, Snooze, Skip }

/// What a transition did. Every transition method returns the facts it
/// produced, in order; empty means "rejected / nothing happened". Callers
/// publish these instead of inferring history from consecutive snapshots.
#[derive(Debug, Clone, PartialEq)]
pub enum Fact {
    CodingStarted,
    Locked(Prescription),
    Started,
    HonorUsed,
    WeightLogged { exercise: String, weight: f64 },
    Completed,
    /// The last planned set of the routine was saved.
    RoutineCompleted,
    Released(Release),
}

pub struct Session {
    phase: Phase,
    timer: CodingTimer,
    workout: WorkoutEngine,
    // When set (from routine.json), the daily plan drives prescription +
    // completion ("one set per lock" until the day clears). Otherwise the
    // legacy infinite rotation (workout) is used.
    plan: Option<DailyPlan>,
    prescription: Option<Prescription>,
    progress: Option<Progress>,
    pending_record: Option<SetRecord>,
    verified: bool,
    /// Frozen countdown while paused (seconds left when the pause began).
    paused: Option<f64>,
}

impl Session {
    pub fn new(timer: CodingTimer, workout: WorkoutEngine) -> Self {
        Self {
            phase: Phase::Idle,
            timer,
            workout,
            plan: None,
            prescription: None,
            progress: None,
            pending_record: None,
            verified: true,
            paused: None,
        }
    }

    pub fn is_paused(&self) -> bool { self.paused.is_some() }

    /// Freeze the session. Rejected while a set is being tracked (finish or
    /// end it first) and when there is no session.
    pub fn pause(&mut self, now: f64) -> Result<(), String> {
        match self.phase {
            Phase::Idle => return Err("No workout to pause".into()),
            Phase::WorkoutActive => return Err("Finish this set first".into()),
            _ => {}
        }
        if self.paused.is_none() {
            self.paused = Some(self.timer.remaining(now));
            self.timer.stop();
        }
        Ok(())
    }

    /// Restore the phase and the countdown where the pause left them.
    pub fn resume(&mut self, now: f64) {
        if let Some(left) = self.paused.take() {
            if self.phase == Phase::Coding { self.timer.start_for(now, left); }
        }
    }

    /// End the session from any phase: confirmed sets stay saved, the rest
    /// of the plan is dropped. Empty when already idle.
    #[must_use]
    pub fn end(&mut self) -> Vec<Fact> {
        if self.phase == Phase::Idle { return vec![]; }
        let released = !matches!(self.phase, Phase::Coding | Phase::Unlocked);
        self.phase = Phase::Idle;
        self.plan = None;
        self.prescription = None;
        self.progress = None;
        self.paused = None;
        self.verified = true;
        self.timer.stop();
        if released { vec![Fact::Released(Release::Cancel)] } else { vec![Fact::CodingStarted] }
    }

    fn routine_done(&self) -> bool { self.plan.as_ref().is_some_and(|p| p.is_complete()) }

    /// Drive prescription from a daily routine plan instead of the rotation.
    pub fn set_plan(&mut self, plan: DailyPlan) {
        self.plan = Some(plan);
    }

    pub fn plan(&self) -> Option<&DailyPlan> {
        self.plan.as_ref()
    }

    /// Honor-mode: the user attests the active set, so it completes at the
    /// prescription target but is not camera-verified. Resets once the set's
    /// record is built or the set is cancelled.
    #[must_use]
    pub fn honor_complete(&mut self) -> Vec<Fact> {
        if self.phase != Phase::WorkoutActive {
            return vec![];
        }
        self.verified = false;
        let (value, unit) = match &self.prescription {
            Some(rx) if rx.target_seconds > 0.0 => (rx.target_seconds, "seconds"),
            Some(rx) => (rx.target_reps as f64, "reps"),
            None => (0.0, "reps"),
        };
        let mut facts = vec![Fact::HonorUsed];
        facts.extend(self.report_progress(Progress { value, unit: unit.into(), satisfied: true }));
        facts
    }

    #[must_use]
    pub fn start(&mut self, now: f64, _today: &str) -> Vec<Fact> {
        self.phase = Phase::Coding;
        self.paused = None;
        self.timer.start(now);
        vec![Fact::CodingStarted]
    }

    pub fn defer_timer(&mut self, seconds: f64) { self.timer.defer(seconds); }

    pub fn configure_timer(&mut self, minutes: f64, now: f64) {
        self.timer.configure(minutes * 60.0, now);
    }

    /// Prescribe the next set and lock. None — and no state change — when
    /// nothing is owed (the day's plan is complete).
    fn lock(&mut self, today: &str) -> Option<Prescription> {
        let rx = match self.plan.as_mut() {
            Some(plan) => plan.prescribe(),
            None => self.workout.prescribe(today),
        }?;
        self.timer.stop();
        self.progress = None;
        self.verified = true;
        self.prescription = Some(rx.clone());
        self.phase = Phase::ExerciseRequired;
        Some(rx)
    }

    #[must_use]
    pub fn tick(&mut self, now: f64, today: &str) -> Vec<Fact> {
        if self.phase != Phase::Coding || self.paused.is_some() || !self.timer.expired(now) {
            return vec![];
        }
        match self.lock(today) {
            Some(rx) => vec![Fact::Locked(rx)],
            // Day complete: don't lock — re-arm the timer and stay coding.
            None => {
                self.timer.start(now);
                vec![]
            }
        }
    }

    /// Start the set: the one already prescribed (ExerciseRequired), or — an
    /// early start from Coding — the next one owed. Empty when a set is already
    /// under way or nothing is owed today.
    #[must_use]
    pub fn start_workout(&mut self, today: &str) -> Vec<Fact> {
        let mut facts = Vec::new();
        if self.phase == Phase::Coding {
            facts.extend(self.lock(today).map(Fact::Locked));
        }
        if self.phase == Phase::ExerciseRequired {
            self.phase = Phase::WorkoutActive;
            facts.push(Fact::Started);
        }
        facts
    }

    /// Leave the lock cycle without completion credit and re-arm the coding
    /// timer. Nothing to release while Coding; from Unlocked the set already
    /// completed, so this is just the return to coding.
    #[must_use]
    pub fn release(&mut self, why: Release, now: f64) -> Vec<Fact> {
        let mut facts = match self.phase {
            Phase::Coding | Phase::Idle => return vec![],
            Phase::Unlocked => vec![],
            _ => vec![Fact::Released(why)],
        };
        self.prescription = None;
        self.progress = None;
        self.verified = true;
        facts.extend(self.start(now, ""));
        facts
    }

    #[must_use]
    pub fn report_progress(&mut self, p: Progress) -> Vec<Fact> {
        if self.phase != Phase::WorkoutActive {
            return vec![];
        }
        let satisfied = p.satisfied;
        self.progress = Some(p);
        if !satisfied {
            return vec![];
        }
        match self.prescription.as_ref().map(|rx| rx.kind) {
            Some(ExerciseKind::Rep) => self.phase = Phase::WeightConfirmation,
            Some(ExerciseKind::Continuous) => {
                let rx = self.prescription.as_ref().unwrap();
                let date = self
                    .plan
                    .as_ref()
                    .map(|p| p.date().to_string())
                    .unwrap_or_else(|| self.workout.capacity_date().to_string());
                self.pending_record = Some(SetRecord {
                    date: date.clone(),
                    exercise: rx.exercise.clone(),
                    kind: rx.kind,
                    reps: 0,
                    seconds: rx.target_seconds,
                    weight: 0.0,
                    verified: self.verified,
                });
                self.verified = true;
                if let Some(plan) = self.plan.as_mut() {
                    plan.complete();
                } else {
                    self.workout.complete(&date);
                }
                self.phase = Phase::Unlocked;
                let mut facts = vec![Fact::Completed];
                if self.routine_done() { facts.push(Fact::RoutineCompleted); }
                return facts;
            }
            None => {}
        }
        vec![]
    }

    /// The record is collected with `take_pending_record`.
    #[must_use]
    pub fn confirm_weight(&mut self, weight: f64, reps: Option<u32>, today: &str) -> Vec<Fact> {
        if self.phase != Phase::WeightConfirmation {
            return vec![];
        }
        let Some(rx) = self.prescription.as_ref() else { return vec![] };
        let record = SetRecord {
            date: today.to_string(),
            exercise: rx.exercise.clone(),
            kind: rx.kind,
            // What the user says they did; else what was detected; else the target.
            reps: reps.or_else(|| self.progress.as_ref().map(|p| p.value.floor() as u32)).unwrap_or(rx.target_reps),
            seconds: 0.0,
            weight,
            verified: self.verified,
        };
        self.verified = true;
        if let Some(plan) = self.plan.as_mut() {
            plan.complete();
        } else {
            self.workout.complete(today);
        }
        let mut facts = vec![Fact::WeightLogged { exercise: record.exercise.clone(), weight }, Fact::Completed];
        if self.routine_done() { facts.push(Fact::RoutineCompleted); }
        self.pending_record = Some(record);
        self.phase = Phase::Unlocked;
        facts
    }

    #[must_use]
    pub fn resume_coding(&mut self, now: f64) -> Vec<Fact> {
        if self.phase != Phase::Unlocked {
            return vec![];
        }
        // The last set saved: the session is over.
        if self.routine_done() { return self.end(); }
        self.release(Release::Cancel, now) // from Unlocked the reason is unused
    }

    /// Start a manually selected detector test, independent of the daily plan.
    /// The desktop caller must require Debug mode and use its isolated store.
    #[must_use]
    pub fn debug_start_exercise(&mut self, prescription: Prescription) -> Vec<Fact> {
        self.timer.stop();
        self.plan = None;
        self.pending_record = None;
        self.progress = None;
        self.verified = false;
        self.prescription = Some(prescription);
        self.phase = Phase::WorkoutActive;
        vec![Fact::Started]
    }

    pub fn take_pending_record(&mut self) -> Option<SetRecord> {
        self.pending_record.take()
    }

    pub fn snapshot(&self, now: f64) -> Snapshot {
        let day = self.plan.as_ref().map(|p| p.to_day_plan());
        let (capacity_used, capacity_limit, rotation, pointer) = match &day {
            Some(d) => (
                d.sets_done,
                d.sets_total,
                d.items.iter().map(|i| i.label.clone()).collect(),
                d.items.iter().position(|i| i.done < i.total).unwrap_or(0),
            ),
            None => (
                self.workout.capacity_used(),
                self.workout.capacity_limit(),
                self.workout.rotation_names(),
                self.workout.pointer(),
            ),
        };
        Snapshot {
            phase: self.phase,
            remaining_seconds: self.paused.unwrap_or_else(|| self.timer.remaining(now)),
            prescription: self.prescription.clone(),
            progress: self.progress.clone(),
            capacity_used,
            capacity_limit,
            rotation,
            pointer,
            day,
            paused: self.paused.is_some(),
        }
    }

    pub fn workout(&self) -> &WorkoutEngine {
        &self.workout
    }

    #[cfg(test)]
    pub fn workout_mut_for_test(&mut self) -> &mut WorkoutEngine {
        &mut self.workout
    }
}

#[cfg(test)]
mod tests {
    use super::{Fact, Release};
    use crate::plan::DailyPlan;
    use crate::timer::CodingTimer;
    use crate::types::{ExerciseDef, ExerciseKind, Phase, Prescription, Progress};
    use crate::workout::WorkoutEngine;

    const DAY: &str = "2026-07-19";

    fn session() -> super::Session {
        let rotation = vec![ExerciseDef {
            name: "bench".into(),
            kind: ExerciseKind::Rep,
            default_reps: 10,
            default_weight: 45.0,
            target_seconds: 0.0,
        }];
        let pool = vec![ExerciseDef {
            name: "jumprope".into(),
            kind: ExerciseKind::Continuous,
            default_reps: 0,
            default_weight: 0.0,
            target_seconds: 60.0,
        }];
        super::Session::new(CodingTimer::new(360.0), WorkoutEngine::new(rotation, pool, 20))
    }

    fn done(unit: &str, v: f64) -> Progress {
        Progress { value: v, unit: unit.into(), satisfied: true }
    }

    fn bench() -> Prescription {
        Prescription { exercise: "bench".into(), kind: ExerciseKind::Rep, target_reps: 10, target_seconds: 0.0, default_weight: 45.0 }
    }

    #[test]
    fn full_lift_cycle_reports_each_fact_once() {
        let mut s = session();
        assert_eq!(s.start(0.0, DAY), vec![Fact::CodingStarted]);
        assert_eq!(s.tick(359.0, DAY), vec![]);
        assert_eq!(s.tick(360.0, DAY), vec![Fact::Locked(bench())]);
        assert_eq!(s.snapshot(360.0).phase, Phase::ExerciseRequired);
        assert_eq!(s.tick(361.0, DAY), vec![]); // locked: no second lock

        assert_eq!(s.start_workout(DAY), vec![Fact::Started]);
        assert_eq!(s.start_workout(DAY), vec![]); // already under way
        assert_eq!(s.report_progress(Progress { value: 3.0, unit: "reps".into(), satisfied: false }), vec![]);
        assert_eq!(s.snapshot(400.0).phase, Phase::WorkoutActive);
        // a lift waits for the weight before it counts as completed
        assert_eq!(s.report_progress(done("reps", 10.0)), vec![]);
        assert_eq!(s.snapshot(401.0).phase, Phase::WeightConfirmation);

        assert_eq!(
            s.confirm_weight(145.0, None, DAY),
            vec![Fact::WeightLogged { exercise: "bench".into(), weight: 145.0 }, Fact::Completed],
        );
        let rec = s.take_pending_record().unwrap();
        assert_eq!((rec.weight, rec.reps, rec.verified), (145.0, 10, true));
        assert_eq!(s.snapshot(402.0).phase, Phase::Unlocked);

        assert_eq!(s.resume_coding(500.0), vec![Fact::CodingStarted]);
        let snap = s.snapshot(500.0);
        assert_eq!(snap.phase, Phase::Coding);
        assert_eq!(snap.remaining_seconds, 360.0);
        assert_eq!(snap.capacity_used, 1);
        assert!(snap.prescription.is_none());
    }

    #[test]
    fn continuous_set_completes_without_weight_confirmation() {
        let mut s = session();
        // force capacity spent so prescription is continuous
        s.workout_mut_for_test().restore(0, 20, DAY);
        let _ = s.start(0.0, DAY);
        let locked = s.tick(360.0, DAY);
        assert!(matches!(&locked[..], [Fact::Locked(rx)] if rx.kind == ExerciseKind::Continuous));
        assert_eq!(s.start_workout(DAY), vec![Fact::Started]);
        assert_eq!(s.report_progress(done("seconds", 60.0)), vec![Fact::Completed]);
        assert_eq!(s.snapshot(361.0).phase, Phase::Unlocked);
        let rec = s.take_pending_record().unwrap();
        assert_eq!(rec.exercise, "jumprope");
        assert_eq!(rec.weight, 0.0);
    }

    #[test]
    fn early_start_locks_and_starts_in_one_step() {
        let mut s = session();
        let _ = s.start(0.0, DAY);
        assert_eq!(s.start_workout(DAY), vec![Fact::Locked(bench()), Fact::Started]);
        assert_eq!(s.snapshot(1.0).phase, Phase::WorkoutActive);
        // the coding timer is stopped while a set is owed
        assert_eq!(s.snapshot(10_000.0).remaining_seconds, 360.0);
        assert_eq!(s.tick(10_000.0, DAY), vec![]);
    }

    #[test]
    fn release_from_each_phase() {
        let coding = vec![Fact::CodingStarted];
        let released = |why| vec![Fact::Released(why), Fact::CodingStarted];

        let mut s = session();
        let _ = s.start(0.0, DAY);
        assert_eq!(s.release(Release::Skip, 1.0), vec![]); // Coding: nothing to release

        let _ = s.tick(360.0, DAY);
        assert_eq!(s.release(Release::Snooze, 361.0), released(Release::Snooze)); // ExerciseRequired
        let snap = s.snapshot(361.0);
        assert_eq!((snap.phase, snap.remaining_seconds), (Phase::Coding, 360.0));
        assert!(snap.prescription.is_none() && snap.progress.is_none());

        let _ = s.start_workout(DAY);
        assert_eq!(s.release(Release::Cancel, 362.0), released(Release::Cancel)); // WorkoutActive

        let _ = s.start_workout(DAY);
        let _ = s.report_progress(done("reps", 10.0));
        assert_eq!(s.release(Release::Cancel, 363.0), released(Release::Cancel)); // WeightConfirmation
        assert_eq!(s.snapshot(363.0).capacity_used, 0); // no completion credit

        let _ = s.start_workout(DAY);
        let _ = s.report_progress(done("reps", 10.0));
        let _ = s.confirm_weight(45.0, None, DAY);
        assert_eq!(s.release(Release::Cancel, 364.0), coding); // Unlocked: already completed
        assert_eq!(s.snapshot(364.0).capacity_used, 1);
    }

    #[test]
    fn wrong_phase_calls_are_rejected() {
        let mut s = session();
        let _ = s.start(0.0, DAY);
        assert_eq!(s.honor_complete(), vec![]);
        assert_eq!(s.confirm_weight(100.0, None, DAY), vec![]);
        assert_eq!(s.report_progress(done("reps", 10.0)), vec![]);
        assert_eq!(s.resume_coding(1.0), vec![]);
        let snap = s.snapshot(2.0);
        assert_eq!(snap.phase, Phase::Coding);
        assert!(snap.progress.is_none());

        let _ = s.tick(360.0, DAY);
        assert_eq!(s.honor_complete(), vec![]); // locked but not started
        assert_eq!(s.snapshot(360.0).phase, Phase::ExerciseRequired);
    }

    #[test]
    fn honor_is_reported_and_recorded_unverified() {
        let mut s = session();
        let _ = s.start(0.0, DAY);
        let _ = s.start_workout(DAY);
        assert_eq!(s.honor_complete(), vec![Fact::HonorUsed]); // lift: weight still to confirm
        let _ = s.confirm_weight(45.0, None, DAY);
        assert!(!s.take_pending_record().unwrap().verified);
        // the next set is verified again by default
        let _ = s.resume_coding(361.0);
        let _ = s.start_workout(DAY);
        let _ = s.report_progress(done("reps", 10.0));
        let _ = s.confirm_weight(50.0, None, DAY);
        assert!(s.take_pending_record().unwrap().verified);

        // a timed set has no weight step, so honor completes it immediately
        let mut s = session();
        s.workout_mut_for_test().restore(0, 20, DAY);
        let _ = s.start(0.0, DAY);
        let _ = s.start_workout(DAY);
        assert_eq!(s.honor_complete(), vec![Fact::HonorUsed, Fact::Completed]);
        assert!(!s.take_pending_record().unwrap().verified);
    }

    #[test]
    fn cancelled_honor_does_not_leak_into_the_next_set() {
        let mut s = session();
        let _ = s.start(0.0, DAY);
        let _ = s.start_workout(DAY);
        assert_eq!(s.honor_complete(), vec![Fact::HonorUsed]);
        assert_eq!(s.release(Release::Cancel, 2.0), vec![Fact::Released(Release::Cancel), Fact::CodingStarted]);
        let _ = s.tick(362.0, DAY);
        let _ = s.start_workout(DAY);
        let _ = s.report_progress(done("reps", 10.0));
        let _ = s.confirm_weight(45.0, None, DAY);
        assert!(s.take_pending_record().unwrap().verified);
    }

    #[test]
    fn last_set_completes_the_routine_and_ends_the_session() {
        let mut s = session();
        s.set_plan(DailyPlan::from_routine_json(r#"{"lifts":[{"exercise":"squat","sets":2,"reps":5}]}"#, DAY).unwrap());
        let _ = s.start(0.0, DAY);
        let _ = s.start_workout(DAY);
        let _ = s.report_progress(done("reps", 5.0));
        assert_eq!(s.confirm_weight(45.0, None, DAY).last(), Some(&Fact::Completed)); // one set left
        assert_eq!(s.resume_coding(10.0), vec![Fact::CodingStarted]);
        let _ = s.start_workout(DAY);
        let _ = s.report_progress(done("reps", 4.0));
        // the user corrects the count before saving
        assert_eq!(s.confirm_weight(35.5, Some(3), DAY).last(), Some(&Fact::RoutineCompleted));
        assert_eq!(s.take_pending_record().unwrap().reps, 3);
        let _ = s.resume_coding(20.0);
        let snap = s.snapshot(20.0);
        assert_eq!(snap.phase, Phase::Idle);
        assert!(snap.day.is_none());
        // idle: nothing counts down, nothing locks, a new date changes nothing
        assert_eq!(s.tick(100_000.0, "2026-07-20"), vec![]);
        assert_eq!(s.start_workout(DAY), vec![]);
    }

    #[test]
    fn a_new_session_starts_idle_and_a_workout_spans_midnight() {
        let mut s = session();
        assert_eq!(s.snapshot(0.0).phase, Phase::Idle);
        assert_eq!(s.tick(10_000.0, DAY), vec![]);
        s.set_plan(DailyPlan::from_routine_json(r#"{"lifts":[{"exercise":"squat","sets":2,"reps":5}]}"#, DAY).unwrap());
        let _ = s.start(0.0, DAY);
        let _ = s.start_workout(DAY);
        let _ = s.report_progress(done("reps", 5.0));
        let _ = s.confirm_weight(45.0, None, DAY);
        let _ = s.resume_coding(10.0);
        // the date rolled: the routine keeps its progress
        assert!(matches!(&s.tick(370.0, "2026-07-20")[..], [Fact::Locked(_)]));
        assert_eq!(s.snapshot(370.0).day.unwrap().sets_done, 1);
    }

    #[test]
    fn pause_freezes_the_countdown_and_resume_restores_it() {
        let mut s = session();
        assert!(s.pause(0.0).is_err()); // idle
        let _ = s.start(0.0, DAY);
        s.pause(100.0).unwrap();
        let snap = s.snapshot(5_000.0);
        assert!(snap.paused);
        assert_eq!(snap.remaining_seconds, 260.0);
        assert_eq!(s.tick(5_000.0, DAY), vec![]);
        s.resume(5_000.0);
        assert_eq!(s.snapshot(5_000.0).remaining_seconds, 260.0);
        assert!(matches!(&s.tick(5_260.0, DAY)[..], [Fact::Locked(_)]));
        let _ = s.start_workout(DAY);
        assert!(s.pause(5_261.0).is_err()); // tracking a set
    }

    #[test]
    fn end_keeps_nothing_running() {
        let mut s = session();
        assert_eq!(s.end(), vec![]);
        let _ = s.start(0.0, DAY);
        let _ = s.start_workout(DAY);
        assert_eq!(s.end(), vec![Fact::Released(Release::Cancel)]);
        let snap = s.snapshot(1.0);
        assert_eq!(snap.phase, Phase::Idle);
        assert!(snap.prescription.is_none());
    }

    #[test]
    fn selected_debug_exercise_replaces_progress_and_uses_timed_target() {
        let mut s = session();
        let _ = s.start_workout("2026-09-15");
        let _ = s.report_progress(Progress { value: 3.0, unit: "reps".into(), satisfied: false });
        assert_eq!(s.debug_start_exercise(Prescription { exercise: "jumprope".into(), kind: ExerciseKind::Continuous,
            target_reps: 0, target_seconds: 60.0, default_weight: 0.0 }), vec![Fact::Started]);
        let snap = s.snapshot(1000.0);
        assert_eq!(snap.phase, Phase::WorkoutActive);
        assert!(snap.progress.is_none());
        assert_eq!(snap.prescription.unwrap().exercise, "jumprope");
        assert_eq!(s.tick(10000.0, "2026-09-15"), vec![]);
        assert_eq!(s.report_progress(done("seconds", 60.0)), vec![Fact::Completed]);
        let record = s.take_pending_record().unwrap();
        assert_eq!(record.exercise, "jumprope");
        assert_eq!(record.seconds, 60.0);
    }
}
