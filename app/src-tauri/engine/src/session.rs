//! The application state machine (V1 spec §20):
//! CODING → EXERCISE_REQUIRED → WORKOUT_ACTIVE → WEIGHT_CONFIRMATION → UNLOCKED → CODING.
//! Pure: driven by tick()/commands with explicit time, no threads, no IO.

use crate::plan::DailyPlan;
use crate::timer::CodingTimer;
use crate::types::{ExerciseKind, Phase, Prescription, Progress, SetRecord, Snapshot};
use crate::workout::WorkoutEngine;

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
}

impl Session {
    pub fn new(timer: CodingTimer, workout: WorkoutEngine) -> Self {
        Self {
            phase: Phase::Coding,
            timer,
            workout,
            plan: None,
            prescription: None,
            progress: None,
            pending_record: None,
            verified: true,
        }
    }

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
    pub fn honor_complete(&mut self) -> bool {
        if self.phase != Phase::WorkoutActive {
            return false;
        }
        self.verified = false;
        let (value, unit) = match &self.prescription {
            Some(rx) if rx.target_seconds > 0.0 => (rx.target_seconds, "seconds"),
            Some(rx) => (rx.target_reps as f64, "reps"),
            None => (0.0, "reps"),
        };
        self.report_progress(Progress { value, unit: unit.into(), satisfied: true });
        true
    }

    pub fn start(&mut self, now: f64, _today: &str) {
        self.phase = Phase::Coding;
        self.timer.start(now);
    }

    pub fn defer_timer(&mut self, seconds: f64) { self.timer.defer(seconds); }

    pub fn configure_timer(&mut self, minutes: f64, now: f64) {
        self.timer.configure(minutes * 60.0, now);
    }

    pub fn tick(&mut self, now: f64, today: &str) -> bool {
        if self.phase == Phase::Coding && self.timer.expired(now) {
            self.timer.stop();
            self.progress = None;
            if let Some(plan) = self.plan.as_mut() {
                plan.roll_date(today);
                match plan.prescribe() {
                    Some(rx) => {
                        self.prescription = Some(rx);
                        self.phase = Phase::ExerciseRequired;
                        return true;
                    }
                    // Day complete: don't lock — re-arm the timer and stay coding.
                    None => {
                        self.prescription = None;
                        self.timer.start(now);
                        return false;
                    }
                }
            }
            self.prescription = self.workout.prescribe(today);
            self.phase = Phase::ExerciseRequired;
            return true;
        }
        false
    }

    pub fn begin_workout(&mut self) {
        if self.phase == Phase::ExerciseRequired && self.prescription.is_some() {
            self.phase = Phase::WorkoutActive;
        }
    }

    pub fn report_progress(&mut self, p: Progress) {
        if self.phase != Phase::WorkoutActive {
            return;
        }
        let satisfied = p.satisfied;
        self.progress = Some(p);
        if !satisfied {
            return;
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
            }
            None => {}
        }
    }

    pub fn confirm_weight(&mut self, weight: f64, today: &str) -> Option<SetRecord> {
        if self.phase != Phase::WeightConfirmation {
            return None;
        }
        let rx = self.prescription.as_ref()?;
        let record = SetRecord {
            date: today.to_string(),
            exercise: rx.exercise.clone(),
            kind: rx.kind,
            reps: rx.target_reps,
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
        self.pending_record = Some(record.clone());
        self.phase = Phase::Unlocked;
        Some(record)
    }

    pub fn resume_coding(&mut self, now: f64) {
        if self.phase == Phase::Unlocked {
            self.prescription = None;
            self.progress = None;
            self.start(now, "");
        }
    }

    /// DEBUG: jump straight into an active set from any phase — skips the coding
    /// timer and the lock so detection can be exercised on demand. Prescribes
    /// the next incomplete item (plan) or the rotation's next (legacy).
    pub fn debug_force_workout(&mut self, now: f64, today: &str) {
        self.timer.stop();
        self.progress = None;
        let _ = now;
        let rx = if let Some(plan) = self.plan.as_mut() {
            plan.roll_date(today);
            plan.prescribe()
        } else {
            self.workout.prescribe(today)
        };
        if let Some(rx) = rx {
            self.prescription = Some(rx);
            self.phase = Phase::WorkoutActive;
        }
    }

    /// Start a manually selected detector test, independent of the daily plan.
    /// The desktop caller must require Debug mode and use its isolated store.
    pub fn debug_start_exercise(&mut self, prescription: Prescription) {
        self.timer.stop();
        self.plan = None;
        self.pending_record = None;
        self.progress = None;
        self.verified = false;
        self.prescription = Some(prescription);
        self.phase = Phase::WorkoutActive;
    }

    /// DEBUG: return to the coding phase from any phase (the caller releases the
    /// camera). Re-arms the coding timer.
    pub fn debug_force_coding(&mut self, now: f64) {
        self.prescription = None;
        self.progress = None;
        self.verified = true;
        self.phase = Phase::Coding;
        self.timer.start(now);
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
            remaining_seconds: self.timer.remaining(now),
            prescription: self.prescription.clone(),
            progress: self.progress.clone(),
            capacity_used,
            capacity_limit,
            rotation,
            pointer,
            day,
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
    use crate::timer::CodingTimer;
    use crate::types::{ExerciseDef, ExerciseKind, Phase, Progress};
    use crate::workout::WorkoutEngine;

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

    #[test]
    fn full_lift_cycle() {
        let mut s = session();
        s.start(0.0, "2026-07-19");
        assert_eq!(s.snapshot(0.0).phase, Phase::Coding);

        assert!(!s.tick(359.0, "2026-07-19"));
        assert!(s.tick(360.0, "2026-07-19"));
        let snap = s.snapshot(360.0);
        assert_eq!(snap.phase, Phase::ExerciseRequired);
        assert_eq!(snap.prescription.as_ref().unwrap().exercise, "bench");

        s.begin_workout();
        assert_eq!(s.snapshot(361.0).phase, Phase::WorkoutActive);

        s.report_progress(Progress { value: 3.0, unit: "reps".into(), satisfied: false });
        assert_eq!(s.snapshot(400.0).phase, Phase::WorkoutActive);

        s.report_progress(done("reps", 10.0));
        assert_eq!(s.snapshot(401.0).phase, Phase::WeightConfirmation);

        let rec = s.confirm_weight(145.0, "2026-07-19").unwrap();
        assert_eq!(rec.weight, 145.0);
        assert_eq!(rec.reps, 10);
        assert!(rec.verified);
        assert_eq!(s.snapshot(402.0).phase, Phase::Unlocked);

        s.resume_coding(500.0);
        let snap = s.snapshot(500.0);
        assert_eq!(snap.phase, Phase::Coding);
        assert_eq!(snap.remaining_seconds, 360.0);
        assert_eq!(snap.capacity_used, 1);
    }

    #[test]
    fn continuous_skips_weight_confirmation() {
        let mut s = session();
        // force capacity spent so prescription is continuous
        s.workout_mut_for_test().restore(0, 20, "2026-07-19");
        s.start(0.0, "2026-07-19");
        s.tick(360.0, "2026-07-19");
        assert_eq!(
            s.snapshot(360.0).prescription.as_ref().unwrap().kind,
            ExerciseKind::Continuous
        );
        s.begin_workout();
        s.report_progress(done("seconds", 60.0));
        assert_eq!(s.snapshot(361.0).phase, Phase::Unlocked);
        let rec = s.take_pending_record().unwrap();
        assert_eq!(rec.exercise, "jumprope");
        assert_eq!(rec.weight, 0.0);
    }

    #[test]
    fn guards_ignore_wrong_phase_calls() {
        let mut s = session();
        s.start(0.0, "2026-07-19");
        s.begin_workout(); // Coding: no-op
        assert_eq!(s.snapshot(1.0).phase, Phase::Coding);
        assert!(s.confirm_weight(100.0, "2026-07-19").is_none());
        s.report_progress(done("reps", 10.0)); // no active workout: no-op
        assert_eq!(s.snapshot(2.0).phase, Phase::Coding);
    }

    #[test]
    fn timer_stops_while_locked() {
        let mut s = session();
        s.start(0.0, "2026-07-19");
        s.tick(360.0, "2026-07-19");
        // long workout: coding timer must not be running
        assert_eq!(s.snapshot(10_000.0).remaining_seconds, 360.0);
    }

    #[test]
    fn selected_debug_exercise_replaces_progress_and_uses_timed_target() {
        let mut s = session();
        s.debug_force_workout(0.0, "2026-09-15");
        s.report_progress(Progress { value: 3.0, unit: "reps".into(), satisfied: false });
        s.debug_start_exercise(crate::types::Prescription { exercise: "jumprope".into(), kind: ExerciseKind::Continuous,
            target_reps: 0, target_seconds: 60.0, default_weight: 0.0 });
        let snap = s.snapshot(1000.0);
        assert_eq!(snap.phase, Phase::WorkoutActive);
        assert!(snap.progress.is_none());
        assert_eq!(snap.prescription.unwrap().exercise, "jumprope");
        assert!(!s.tick(10000.0, "2026-09-15"));
        s.report_progress(done("seconds", 60.0));
        let record = s.take_pending_record().unwrap();
        assert_eq!(record.exercise, "jumprope");
        assert_eq!(record.seconds, 60.0);
    }

    #[test]
    fn debug_toggle_switches_between_coding_and_workout() {
        let mut s = session();
        s.start(0.0, "2026-07-19");
        assert_eq!(s.snapshot(0.0).phase, Phase::Coding);
        // Force into a set without waiting out the timer.
        s.debug_force_workout(1.0, "2026-07-19");
        let snap = s.snapshot(1.0);
        assert_eq!(snap.phase, Phase::WorkoutActive);
        assert!(snap.prescription.is_some());
        // Force back to coding.
        s.debug_force_coding(2.0);
        assert_eq!(s.snapshot(2.0).phase, Phase::Coding);
        assert!(s.snapshot(2.0).prescription.is_none());
    }

    #[test]
    fn honor_mode_records_are_unverified() {
        let mut s = session();
        s.start(0.0, "2026-07-19");
        s.tick(360.0, "2026-07-19");
        s.begin_workout();
        assert!(s.honor_complete()); // camera failed: user pressed Done
        let rec = s.confirm_weight(45.0, "2026-07-19").unwrap();
        assert!(!rec.verified);
    }

    #[test]
    fn verification_resets_after_each_set() {
        let mut s = session();
        s.start(0.0, "2026-07-19");
        s.tick(360.0, "2026-07-19");
        s.begin_workout();
        assert!(s.honor_complete());
        s.confirm_weight(45.0, "2026-07-19").unwrap();
        s.take_pending_record();
        s.resume_coding(361.0);
        // next workout is verified again by default
        s.tick(721.0, "2026-07-19");
        s.begin_workout();
        s.report_progress(done("reps", 10.0));
        let rec = s.confirm_weight(50.0, "2026-07-19").unwrap();
        assert!(rec.verified);
    }

    #[test]
    fn honor_outside_active_workout_is_rejected_and_cancel_restores_verification() {
        let mut s = session();
        s.start(0.0, "2026-07-19");
        assert!(!s.honor_complete());
        assert!(s.verified);
        assert_eq!(s.snapshot(0.0).phase, Phase::Coding);
        assert!(s.snapshot(0.0).progress.is_none());

        s.debug_force_workout(1.0, "2026-07-19");
        assert!(s.honor_complete());
        s.debug_force_coding(2.0); // cancelled: the honor must not leak into the next set
        s.tick(362.0, "2026-07-19");
        s.begin_workout();
        s.report_progress(done("reps", 10.0));
        let rec = s.confirm_weight(45.0, "2026-07-19").unwrap();
        assert!(rec.verified);
    }
}
