# RFP design

RFP (reps for prompts) turns agent wait time into exercise. The coding countdown
only runs while a coding agent (Claude Code or Codex) is open, and when it expires
the next set is due. The webcam counts reps, you log the weight, and the countdown
starts again.

The current cross-repo acceptance requirements are the hub's
[production target](../../../usb-mcp-hub/docs/design/production-target.md).
RFP owns workouts, desktop behavior, pose features, and movement rules. The hub
owns recording, immutable versions, evaluation, activation, and event delivery.

## Product intent — 2026-09-30

- **Passive reminder, not a lock.** RFP nudges you and never takes over the
  machine. Lock mode stays off: startup forces it off and the CLI refuses to turn
  it on. The earlier xsecurelock design is superseded.
- **Only agent time counts.** The countdown pauses when no Claude Code or Codex
  process is running for the same user. RFP detects the processes; it never
  reads prompts or keystrokes.
- **Two windows, one debt.** The CODE window shows coding time and the WORKOUT
  window shows the set to do. Debt is `setsTotal − setsDone` for the day's
  routine ([two-window spec](../superpowers/specs/2026-08-31-workout-debt-desktop.md)).
- **Never strand the user, never invent reps.** If vision is unavailable, the
  user can honor-complete a set. It is recorded as unverified, and the app keeps
  retrying the hub in the background.
- **The CLI is a first-class control surface.** Anything the display can do,
  `rfp` can do (start, finish, snooze, routine, camera, display), so agents and
  scripts can drive RFP.
- **Public by opt-in only.** Posting to the web site requires `sharing` on, and
  location is opt-in. Demo filler profiles are always labeled DEMO. Uploads are
  deduplicated and rate-limited.

## Sensitivity intent — 2026-09-15

Exercise reps and jump-rope timing should ignore small incidental movements.
Legacy presets require a wider joint-angle excursion for the six affected lifts
and a larger frame-to-frame hip displacement for jump rope. These defaults are
starting points for camera calibration. Activated movement versions remain
immutable and require a new evaluated candidate to change sensitivity.

[As-built detection](../architecture/detection.md) records exact settings,
configuration precedence, verification, and limitations. [Architecture coverage](../architecture/index.md)
connects the broader production target to implementation and remaining gates.

## Related designs

- [Multi-camera consensus](multi-camera-consensus.md): strict 2-of-2, owned by the hub.

## Supporting design history

- [Workout debt desktop](../superpowers/specs/2026-08-31-workout-debt-desktop.md), implemented 2026-09-01.
- [Hub as SDK](../superpowers/specs/2026-07-21-hub-as-sdk-design.md), migration completed 2026-07-27.
- [Tauri rewrite](../superpowers/specs/2026-07-19-tauri-rewrite-design.md), implemented. Its xsecurelock lock was replaced by the passive reminder.
- [Workout lock display](../superpowers/specs/2026-07-19-workout-lock-display-design.md), superseded by the two-window spec.
