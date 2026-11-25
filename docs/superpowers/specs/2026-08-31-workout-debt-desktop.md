> **Superseded (2026-11-23)** by the V2 design, "annoy me, don't imprison me": no lock, no takeover of other apps, no workout debt. As built: `docs/architecture/v2-desktop.md`.

# Vision MCP — Workout Debt Desktop Frontend Tech Spec

**Date:** 2026-08-31
**Status:** Approved; implemented on `feature/workout-debt-desktop`.
**Supersedes the UI of:** `2026-07-19-workout-lock-display-design.md` (loop unchanged, surface replaced).

## 1. Objective

Build a lightweight desktop frontend around the existing Vision MCP workout system.

The application has one job:

> When workout debt exists, make that debt extremely obvious, guide the workout through the gym display, and return the computer to normal when the required workout is complete.

The implementation should prioritize **shipping immediately over architectural perfection**.

Existing routines, workout logic, vision detection, rep tracking, and workout-debt calculations should be reused wherever possible.

## 2. Technology

* Tauri
* Existing frontend framework already present in the repository (Vite + React 19)
* TypeScript
* Tauri Rust commands only where native OS/window behavior requires them

Avoid introducing additional infrastructure unless required.

## 3. Application Model

One Tauri application manages two displays.

```text
Vision MCP
│
├── Workout / Debt Engine
├── Vision Detection
├── Tauri Desktop Shell
│   ├── Primary Monitor Window
│   └── Gym Monitor Window
└── Shared Application State
```

Both windows consume the same application state. There should **not** be two independent applications or two separate workout sessions.

## 4. Core States

```text
CODE → WORKOUT_DEBT → WORKOUT_ACTIVE → CODE
```

Optional: `ERROR`. Do not build a complicated workflow engine for the first version.

## 5. Shared State

One shared application state object; both windows subscribe. The existing Vision MCP/workout engine remains the source of truth.

## 6. Window Architecture

**Primary window** — the programming monitor. Visually blocks normal computer use while workout debt exists, shows the current debt, makes the WORKOUT ↔ CODE transition obvious, is the visual element in recordings/time-lapses. In debt mode it is fullscreen/maximized and shows `WORKOUT / 🔒 24 / WORKOUT DEBT REMAINING`; the padlock and number are extremely prominent.

**Gym window** — fullscreen on the large gym display. Shows the current workout, rep progress, immediate feedback from vision detection: `WORKOUT / [animated character] / JUMP ROPE / 42 / 100`.

## 7. CODE Screen

Top 25–35%: very large `CODE`. Below: animated 16-bit character (blond hair, glasses, standing desk, typing). GIF/WebP/sprite/video loop — no game animation engine.

## 8. WORKOUT Screen

Mirrors the CODE composition. Large `WORKOUT`, Claude-inspired orange character with a retro 1980s exercise headband and jump-rope animation, then `JUMP ROPE / 038 / / 100 REPS`. Rep count is one of the largest elements. Animation need not sync with detected reps.

## 9. Visual Style

**16-bit / retro video-game workout operating system.** Pixel art, large typography, extremely high contrast, minimal text, arcade/game UI influence, playful not clinical. Cute, not techy. WORKOUT colours are Claude orange (mascot + claude.ai branding). Avoid dashboards, cards, sidebars, graphs, settings panels, SaaS UI.

## 10. Layout

Same three-band composition for both modes and both windows: title / character / status.

## 11. Workout Debt

The existing backend determines whether debt exists; the frontend does not duplicate scheduling logic. `debt > 0 → WORKOUT`, else `CODE`.

## 12. Vision Integration

The frontend consumes rep updates from the existing vision system. No polling if events already exist. Preference: existing MCP events, local WS/SSE, Tauri events, polling only as fallback.

## 13. Workout Completion

`completed >= required` marks the exercise complete. When all debt is satisfied: `WORKOUT → COMPLETE → CODE`, with a short "WORKOUT COMPLETE" beat.

## 14. Lockout Behaviour

**Soft lock** only: fullscreen, always-on-top, borderless, refocus when practical. Never malware, never touches OS authentication. Always retain an emergency/manual escape for webcam failure, vision failure, bugs, accessibility, urgent access — behind a deliberately inconvenient control.

## 15. Monitor Selection

Enumerate displays, put the primary window on the programming monitor and the gym window on the external display. If the gym monitor is unavailable, never prevent emergency access.

## 16–17. Structure / Synchronization

Keep native code minimal. All state changes originate from one controller; the two windows never keep independent counters.

## 18. First-Launch Scope

Two windows · monitor placement · CODE mode · WORKOUT mode · existing debt integration · existing rep-count integration · fullscreen lock screen · big WORKOUT/CODE typography · temporary animations · automatic transition back to CODE.

## 19. Out of Scope

Accounts, cloud sync, achievements, complex animation, themes, history dashboard, analytics, settings, sprite engine, mobile, perfect OS lockout, gamification, engine redesign, rebuilding Vision MCP.

## 20. MVP Acceptance

`debt = 0` → primary shows `CODE` + pixel programmer. Debt appears → primary fullscreen `WORKOUT 🔒 DEBT REMAINING`, gym in workout mode. Camera detects exercise → gym counts `1 / 100 … 100 / 100`. Debt hits zero → both screens transition, primary shows `CODE`, normal session resumes.

## 21. Product Principle

`Debt exists → WORKOUT. Debt paid → CODE.` The sophistication lives in Vision MCP and the engine; the desktop app is a physical-state visualization and enforcement surface.

---

## Implementation notes (2026-08-31)

- **Debt** = `snapshot.day.setsTotal − setsDone` (one set per lock, from `routine.json`). Already on every `snapshot` event; no engine change. Day-scoped: `plan.rs roll_date` clears it at midnight (carry-over is a one-method change when wanted).
- **Phase → mode**: `CODING`→CODE; `EXERCISE_REQUIRED` / `WORKOUT_ACTIVE` / `WEIGHT_CONFIRMATION`→WORKOUT (locked); `UNLOCKED`→"WORKOUT COMPLETE" beat (3s, then Rust auto-resumes coding).
- **Auto-start on lock** (user decision): the tick thread emits `EXERCISE_REQUIRED` (so `desktop_locked` events still fire), then immediately `begin_workout` + enables the metric. No ENGAGE button. Stretch/jump-rope timers therefore start at lock time.
- **Windows** are owned by Rust (`src-tauri/src/windows.rs`): `main` gets fullscreen + always-on-top + sticky-across-workspaces + decorations off while `phase != CODING`, re-raised and re-focused once a second while locked, and **minimized** once the debt is paid. `gym` is a draggable **maximized** window placed on the remembered monitor (settings `gym_monitor`, written whenever it's moved; `$REPS_GYM_MONITOR` overrides) else the first non-primary one; **F11** inside it toggles fullscreen (remembered as `gym_fullscreen`). It is never minimized — it stays up in CODE mode too. Both windows load the same SPA; the gym one is `index.html?window=gym`.
- **Takeover of other apps** (user-set): while debt is owed, every other app's normal window on every workspace is iconified (ICCCM `WM_CHANGE_STATE`, re-applied once a second) via `src-tauri/src/x11_windows.py` run with the system `python3` (needs `python3-xlib`; silent no-op without it or off X11). The ids are kept and mapped back when the debt is paid. Panels/docks/desktop are left alone.
- **Leaked-hub recovery** (2026-09-01): a hard-killed app (`tauri dev` rebuild, SIGKILL) leaves hubd *and* its vision-host behind; the host holds the camera and the next lock failed with `cannot open camera` → honor mode. `hub-client/src/supervisor.rs::free_stale_hub_port` now signals the whole process **group** of whatever holds :8443 (hubd is spawned as its own group), SIGTERM then SIGKILL, so the host dies with it. Covered by `free_stale_hub_port_kills_the_listeners_whole_group`. `scripts/free-hub.sh` stays as the manual fallback.
- **LOGGED beat**: 3s whole-screen takeover (green check + `LOGGED`) on both windows after a set is logged, then CODE.
- **Live count** = `snapshot.progress.value`; unit from `prescription.kind` (lifts = reps, jump rope / stretches = seconds).
- **Weight logging** for lifts stays on the primary window (`WEIGHT_CONFIRMATION`): number input, Enter confirms.
- **Honor mode**: on `vision-fallback` the primary shows `CAMERA DOWN · PRESS H`; `H` invokes `honor_complete` (set recorded unverified).
- **Emergency escape**: on the primary window hold **Ctrl + Shift + Backspace for 3 seconds** → `debug_mode coding` (releases the camera, re-arms the timer). No on-screen UI.
- **Dev controls** (`import.meta.env.DEV` only): the CODING/WORKOUT toggle plus `+1` / `done` (simulated progress) so the loop can be walked at the desk without a camera.
- **Assets**: mascot sprites + backgrounds generated with Higgsfield from `brand/mascot-source.png`, composited with PIL; the Seedance loops are shipped as **animated WebP `<img>`** (960×540, pixel-upscaled) with the still as fallback. NOT `<video loop>`: WebKitGTK's looping media pipeline leaked ~6 MB/s per window and froze the gym display (measured 2026-08-31).
