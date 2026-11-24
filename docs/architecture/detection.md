# Detection and sensitivity as built

Reviewed 2026-09-15. Implements [sensitivity intent](../design/index.md#sensitivity-intent-2026-09-15).
Human instructions: [counting and tuning](../wiki/concepts/detection.md).

## Configuration precedence

[detector.rs](../../app/src-tauri/src/detector.rs) embeds
[exercise_specs.json](../../app/src-tauri/resources/exercise_specs.json) with Rust
`include_str!`. Shipped lifts carry their legacy exercise config plus a movement
ID. The hub resolves an active evaluated version for that ID when enabling the
metric; its stored rules/model identity take precedence. Without an active
version, a shipped lift uses the legacy preset. A custom ID needs an active
version. Jump rope and stretch use their activity configuration.

Changing embedded defaults requires rebuilding the desktop. Existing metrics
and installed binaries do not pick up this source edit automatically. Activated
versions and in-progress workouts are not retuned by the preset change.

## Legacy lift counter

[plugin.py](../../vision/src/reps_vision/hub_plugin/plugin.py) constructs
[LiftActivity](../../vision/src/reps_vision/activities/lift.py) from the supplied
exercise, which measures the most visible side's three joints and feeds
[RepStateMachine](../../vision/src/reps_vision/angles.py). An angle strictly below
`downBelow` arms the counter; a later angle strictly above `upAbove` counts one
rep. It can start at the bottom. There is no dwell, smoothing, or minimum cycle
time on this path. Missing poses do not clear an armed rep; body side can change
between frames. Wider thresholds reduce small-motion triggers but cannot rule
out false crossings from tracking errors.

### Preset changes on 2026-09-15

Angles are degrees. Visibility settings and joint triples are unchanged.

| Exercise | Previous down / up | Current down / up |
|---|---|---|
| Incline bench | 150 / 160 | 125 / 160 |
| Curl | 135 / 165 | 110 / 165 |
| Pull-up | 140 / 168 | 115 / 168 |
| Row | 60 / 85 | 60 / 105 |
| Squat | 145 / 165 | 130 / 165 |
| Deadlift | 145 / 175 | 130 / 175 |

Push-up remains 95 / 155, bench 95 / 150, overhead press 30 / 140.
The standalone Python registry in `exercises.py` has separate defaults; the
desktop supplies the JSON values above through `ExerciseSpec.from_config`.

## Versioned lift counter

[MovementActivity](../../vision/src/reps_vision/movement.py) validates named angle
features and ordered phases. It establishes the first phase, visits the others,
and returns to the first to count. Dwell, exponential smoothing, visibility,
minimum/maximum cycle time and tracking-loss reset govern transitions. Body side
stays selected until reset. Missing poses clear pending dwell.

[Templates](../../vision/src/reps_vision/movement_templates.json) currently use
80 ms dwell, 60 ms smoothing, visibility 0.65, 500 ms tracking-loss timeout,
350 ms minimum cycle and 15,000 ms maximum cycle. Template angles differ from
legacy presets. This sensitivity change does not alter templates or active versions.
The hub refuses live edits to pinned versions; create, evaluate and activate a
new candidate for later workouts. See [hub architecture](../../../usb-mcp-hub/docs/architecture/index.md).

## Jump rope and stretch

[JumpRopeActivity](../../vision/src/reps_vision/activities/jumprope.py) compares mean
hip y between successive frames. The desktop's `bounceThreshold` increased from
0.004 to **0.008** in normalized image height (0.8% of frame height). The Python
constructor and generic plugin schema retain a separate default of 0.015.
A moving frame adds elapsed time since the previous update. Still frames add no
time; after the 2-second grace period, stillness clears the accumulated streak.
Missing landmarks count as stillness. This detects vertical displacement, not
rope swings; it depends on frame rate and framing and does not filter hip
visibility. It can also respond to unrelated vertical motion.

[StretchActivity](../../vision/src/reps_vision/activities/stretch.py) is elapsed
time, without posture verification. Its behavior is unchanged.

## Verification and limits

For the preset change, 49 existing tests passed across `test_compound_lifts.py`,
`test_exercises.py`, `test_angles.py`, and `test_activities_jumprope.py`.
Additional synthetic checks rejected small excursions for all six changed lifts,
counted larger excursions, rejected 0.005 hip jitter and accepted a larger bounce.
The compound-lift suite derives inputs from configuration, so passing it alone
does not establish a useful real-world movement range.

No camera trial, installer rebuild, or new held-out accuracy qualification was
performed for these thresholds. Confirm camera placement and test real full reps,
partial reps, standing still, pauses and tracking interruptions before relying on
the new settings. See the [release gates](../../../usb-mcp-hub/docs/production/release-checklist.md).
