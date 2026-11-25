# V2 desktop (as built, 2026-11-23)

The desktop never locks the workstation or touches other apps' windows. It
nags, and you can always dismiss it, pause it, end the workout or quit.

## Windows (`src-tauri/src/windows.rs`)

| Label | Role | Behaviour |
| --- | --- | --- |
| `main` | Launcher, training plan, history, settings | Normal decorated window. Closing it hides it to the tray. |
| `gym` | Workout monitor | Goes on the remembered monitor (else the first non-primary one), fullscreen unless F11 or its button turned that off. With a single display it stays a plain window. Passive apart from Start session and Fullscreen. |
| `prompt` | Work-desktop reminder | 500×360, always on top, centred on a display that isn't the gym's. Asks for attention once and never forces focus. |

`src/tray.rs` adds Open, Show workout monitor, Progress & History, Snooze, Pause/Resume, End workout and Quit. The tray tooltip shows the state as text.

## Session (`engine/src/session.rs`, `src/lifecycle.rs`)

The new phase is `IDLE` (no session). The other phases keep their wire names:

`IDLE` → Start Workout (setup: gym shows READY) → Start session → `CODING` → `EXERCISE_REQUIRED` (set due) → `WORKOUT_ACTIVE` (camera, or *I finished this set*) → `WEIGHT_CONFIRMATION` (actual reps + lbs on the work desktop) → `UNLOCKED` (3 s SET LOGGED) → `CODING` … → the last set saved → `IDLE`. That last save marks the session `completed` and moves the cycle on exactly once.

- **Paused** is a snapshot flag that freezes the countdown and suppresses prompts. A session found at startup is restored paused, with the camera off.
- **End workout** (`rfp cancel`) keeps the confirmed sets and marks the session `partial`. The cycle does not move.
- Agent presence no longer gates the timer.

## Training plan (`src/training.rs`, `plan.json` in the app home)

- Routines.
- An ordered cycle of slots, with `nextSlot`.
- Weekdays and a weekly target.
- Three start-reminder times.
- Coding and snooze minutes.

On first run the plan is seeded from `resources/routine.json`. `rfp routine [--file]` reads or replaces it. A start reminder fires once per slot on a training day, within 10 minutes of its time and only if no session started that day. A missed slot is skipped, not delivered late.

## Prompt controller (`src/prompt.rs`, `src/ui.rs::tick`)

One logical prompt. Its kind comes from the phase: `due`, `exercising` or `confirm`, plus `start` while idle.

- A new kind shows at once.
- A dismissed `due`, `exercising` or `confirm` prompt comes back after 5 s.
- A dismissed `start` is gone for that occurrence.
- Nothing re-shows while the prompt is visible.

## Storage (`engine/src/store.rs`, schema v3)

- New table `workouts(id, routine, slot, status, started_at, completed_at, local_date)`.
- `exercise_history.workout_id` links each set to its session.

History, Week and the records views are computed from confirmed sets only.

## Deferred

- Airtable outbox.
- Exercise-dataset import.
- Goals.
- One prompt copy per monitor, and workspace-follow.
- Wayland diagnostics.
- A load convention stored per set (today it is only shown).
- Weekly-target revisions (today the current target applies to every week).
- Interleaving timed rows with lifts in a routine (DailyPlan runs lifts first).
