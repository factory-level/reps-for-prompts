//! The training plan (V2 spec §6): reusable routines, an ordered repeating
//! cycle, a weekly commitment and the workout timing. Lives in `plan.json`.
//! Pure functions here; Lifecycle and the tick loop do the IO.
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    /// Detector name for vision-supported movements, else any custom id.
    pub exercise: String,
    pub label: String,
    /// "lift" (sets × reps, lbs) or "timed" (rounds × seconds).
    pub kind: String,
    pub sets: u32,
    #[serde(default)]
    pub reps: u32,
    #[serde(default)]
    pub seconds: f64,
    #[serde(default)]
    pub default_weight: Option<f64>,
    /// "total" | "per_dumbbell" | "none".
    #[serde(default = "total")]
    pub load: String,
}
fn total() -> String { "total".into() }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Routine {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub archived: bool,
    pub items: Vec<Item>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Slot { pub slot_id: String, pub routine_id: String }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrainingPlan {
    pub routines: Vec<Routine>,
    pub cycle: Vec<Slot>,
    pub next_slot: String,
    /// ISO weekdays, 1 = Monday.
    pub weekdays: Vec<u32>,
    pub weekly_target: u32,
    pub start_times: Vec<String>,
    pub coding_minutes: f64,
    pub snooze_minutes: u32,
}

/// Seed: the shipped routine.json as one routine, so first launch has a plan.
pub fn seed(routine_json: &str) -> TrainingPlan {
    #[derive(Deserialize)]
    struct Lift { exercise: String, sets: u32, reps: u32, label: Option<String>, weight: Option<f64> }
    #[derive(Deserialize)]
    struct Cond { exercise: String, rounds: Option<u32>, seconds: f64, label: Option<String> }
    #[derive(Deserialize)]
    struct File { name: Option<String>, #[serde(default)] lifts: Vec<Lift>, #[serde(default)] conditioning: Vec<Cond> }
    let file: File = serde_json::from_str(routine_json).unwrap_or(File { name: None, lifts: vec![], conditioning: vec![] });
    let mut items: Vec<Item> = file.lifts.into_iter().map(|l| Item {
        label: l.label.unwrap_or_else(|| l.exercise.clone()), exercise: l.exercise, kind: "lift".into(),
        sets: l.sets, reps: l.reps, seconds: 0., default_weight: l.weight, load: total(),
    }).collect();
    items.extend(file.conditioning.into_iter().map(|c| Item {
        label: c.label.unwrap_or_else(|| c.exercise.clone()), exercise: c.exercise, kind: "timed".into(),
        sets: c.rounds.unwrap_or(1), reps: 0, seconds: c.seconds, default_weight: None, load: "none".into(),
    }));
    TrainingPlan {
        routines: vec![Routine { id: "routine-1".into(), name: file.name.unwrap_or_else(|| "Full body".into()), archived: false, items }],
        cycle: vec![Slot { slot_id: "slot-1".into(), routine_id: "routine-1".into() }],
        next_slot: "slot-1".into(),
        weekdays: vec![1, 2, 4, 5],
        weekly_target: 4,
        start_times: vec!["09:00".into(), "12:00".into(), "15:00".into()],
        coding_minutes: 25.,
        snooze_minutes: 5,
    }
}

pub fn load(home: &Path, routine_json: &str) -> TrainingPlan {
    std::fs::read_to_string(home.join("plan.json")).ok()
        .and_then(|s| serde_json::from_str::<TrainingPlan>(&s).ok())
        .filter(|p| validate(p).is_ok())
        .unwrap_or_else(|| seed(routine_json))
}

pub fn save(home: &Path, plan: &TrainingPlan) -> Result<(), String> {
    validate(plan)?;
    let tmp = home.join("plan.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(plan).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    std::fs::rename(tmp, home.join("plan.json")).map_err(|e| e.to_string())
}

pub fn minutes_of(hhmm: &str) -> Option<u32> {
    let (h, m) = hhmm.split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

pub fn validate(p: &TrainingPlan) -> Result<(), String> {
    let mut ids = std::collections::HashSet::new();
    for r in &p.routines {
        if r.name.trim().is_empty() { return Err("Every routine needs a name".into()); }
        if !ids.insert(&r.id) { return Err(format!("Duplicate routine id {}", r.id)); }
        if r.items.is_empty() && !r.archived { return Err(format!("{}: add at least one exercise", r.name)); }
        for i in &r.items {
            if i.exercise.trim().is_empty() || i.label.trim().is_empty() { return Err(format!("{}: every row needs an exercise", r.name)); }
            if i.sets == 0 { return Err(format!("{}: sets must be at least 1", i.label)); }
            match i.kind.as_str() {
                "lift" if i.reps == 0 => return Err(format!("{}: reps must be at least 1", i.label)),
                "timed" if !(i.seconds.is_finite() && i.seconds > 0.) => return Err(format!("{}: seconds must be positive", i.label)),
                "lift" | "timed" => {}
                _ => return Err(format!("{}: unknown kind {}", i.label, i.kind)),
            }
            if i.default_weight.is_some_and(|w| !w.is_finite() || w < 0.) { return Err(format!("{}: weight must be 0 or more", i.label)); }
        }
    }
    if p.cycle.is_empty() { return Err("Add at least one routine to the cycle".into()); }
    let mut slots = std::collections::HashSet::new();
    for s in &p.cycle {
        if !slots.insert(&s.slot_id) { return Err("Duplicate cycle slot".into()); }
        if !p.routines.iter().any(|r| r.id == s.routine_id && !r.archived) { return Err("A cycle slot points at a missing or archived routine".into()); }
    }
    if !p.cycle.iter().any(|s| s.slot_id == p.next_slot) { return Err("Choose the next routine".into()); }
    if p.weekdays.iter().any(|d| !(1..=7).contains(d)) { return Err("Weekdays are 1–7".into()); }
    if !(1..=14).contains(&p.weekly_target) { return Err("Weekly target is 1–14 workouts".into()); }
    if p.start_times.len() != 3 || p.start_times.iter().any(|t| minutes_of(t).is_none()) { return Err("Set three start times (HH:MM)".into()); }
    let mut t: Vec<_> = p.start_times.iter().collect(); t.sort(); t.dedup();
    if t.len() != 3 { return Err("The three start times must differ".into()); }
    if !(p.coding_minutes.is_finite() && p.coding_minutes >= 0.1 && p.coding_minutes <= 240.) { return Err("Coding interval is 1–240 minutes".into()); }
    if !(1..=60).contains(&p.snooze_minutes) { return Err("Snooze is 1–60 minutes".into()); }
    Ok(())
}

/// The slot the next Start Workout runs, its position, and its routine.
pub fn next(p: &TrainingPlan) -> Option<(usize, &Slot, &Routine)> {
    let i = p.cycle.iter().position(|s| s.slot_id == p.next_slot)?;
    let slot = &p.cycle[i];
    Some((i, slot, p.routines.iter().find(|r| r.id == slot.routine_id)?))
}

/// After a full routine: the slot after `done`, wrapping. Only if the plan
/// still points at `done` (an edit meanwhile wins).
pub fn advance(p: &mut TrainingPlan, done: &str) {
    if p.next_slot != done { return; }
    if let Some(i) = p.cycle.iter().position(|s| s.slot_id == done) {
        p.next_slot = p.cycle[(i + 1) % p.cycle.len()].slot_id.clone();
    }
}

/// routine.json for one routine, so the engine's DailyPlan runs it.
pub fn routine_json(r: &Routine) -> String {
    let lifts: Vec<_> = r.items.iter().filter(|i| i.kind == "lift").map(|i| serde_json::json!({
        "exercise": i.exercise, "sets": i.sets, "reps": i.reps, "label": i.label, "weight": i.default_weight })).collect();
    let conditioning: Vec<_> = r.items.iter().filter(|i| i.kind == "timed").map(|i| serde_json::json!({
        "exercise": i.exercise, "rounds": i.sets, "seconds": i.seconds, "label": i.label })).collect();
    // ponytail: DailyPlan orders lifts before timed rows; interleaved order needs a single item list in the engine.
    serde_json::json!({"lifts": lifts, "conditioning": conditioning}).to_string()
}

/// Which start slot (index into start_times) to prompt now, if any: a
/// training day, no session started today, a slot whose time passed within
/// the grace window and was not yet delivered. At most one; an expired slot
/// is skipped rather than delivered late.
pub fn start_slot_due(p: &TrainingPlan, weekday: u32, minute: u32, started_today: bool, delivered: impl Fn(usize) -> bool) -> Option<usize> {
    const GRACE: u32 = 10;
    if started_today || !p.weekdays.contains(&weekday) { return None; }
    p.start_times.iter().enumerate().rev()
        .filter_map(|(i, t)| minutes_of(t).map(|m| (i, m)))
        .find(|&(_, m)| minute >= m && minute < m + GRACE)
        .map(|(i, _)| i)
        .filter(|&i| !delivered(i))
}

/// Monday of the ISO week holding `date` (YYYY-MM-DD).
pub fn week_start(date: &str) -> String {
    use chrono::{Datelike, NaiveDate};
    let d = NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap_or_default();
    (d - chrono::Duration::days(d.weekday().num_days_from_monday() as i64)).format("%Y-%m-%d").to_string()
}

pub fn add_days(date: &str, days: i64) -> String {
    let d = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap_or_default();
    (d + chrono::Duration::days(days)).format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    const ROUTINE: &str = include_str!("../resources/routine.json");

    fn plan() -> TrainingPlan {
        let mut p = seed(ROUTINE);
        let mut b = p.routines[0].clone(); b.id = "routine-2".into(); b.name = "Lower A".into();
        p.routines.push(b);
        p.cycle.push(Slot { slot_id: "slot-2".into(), routine_id: "routine-2".into() });
        p
    }

    #[test]
    fn seed_is_valid_and_runs_in_the_engine() {
        let p = seed(ROUTINE);
        validate(&p).unwrap();
        let (_, _, r) = next(&p).unwrap();
        let day = engine::plan::DailyPlan::from_routine_json(&routine_json(r), "2026-11-23").unwrap().to_day_plan();
        assert!(day.sets_total > 0);
    }

    #[test]
    fn cycle_advances_once_and_wraps() {
        let mut p = plan();
        advance(&mut p, "slot-1");
        assert_eq!(p.next_slot, "slot-2");
        advance(&mut p, "slot-1"); // stale: the plan already moved on
        assert_eq!(p.next_slot, "slot-2");
        advance(&mut p, "slot-2");
        assert_eq!(p.next_slot, "slot-1");
    }

    #[test]
    fn start_prompts_fire_once_per_slot_within_grace_on_training_days() {
        let p = plan(); // Mon/Tue/Thu/Fri, 09:00 12:00 15:00
        assert_eq!(start_slot_due(&p, 1, 9 * 60 + 2, false, |_| false), Some(0));
        assert_eq!(start_slot_due(&p, 1, 9 * 60 + 2, false, |i| i == 0), None); // delivered
        assert_eq!(start_slot_due(&p, 1, 9 * 60 + 30, false, |_| false), None); // expired, not late
        assert_eq!(start_slot_due(&p, 3, 12 * 60, false, |_| false), None); // Wednesday
        assert_eq!(start_slot_due(&p, 1, 12 * 60, true, |_| false), None); // started today
        assert_eq!(start_slot_due(&p, 5, 15 * 60 + 9, false, |_| false), Some(2));
    }

    #[test]
    fn validation_rejects_bad_plans() {
        let mut p = plan(); p.start_times[1] = "09:00".into(); assert!(validate(&p).is_err());
        let mut p = plan(); p.routines[0].items[0].sets = 0; assert!(validate(&p).is_err());
        let mut p = plan(); p.next_slot = "gone".into(); assert!(validate(&p).is_err());
        let mut p = plan(); p.routines[0].items[0].default_weight = Some(-1.); assert!(validate(&p).is_err());
    }

    #[test]
    fn weeks_start_monday() {
        assert_eq!(week_start("2026-11-29"), "2026-11-23"); // Sunday
        assert_eq!(week_start("2026-11-23"), "2026-11-23");
        assert_eq!(add_days("2026-11-23", 6), "2026-11-29");
    }
}
