# Debug mode

The app starts in **Debug** until a mode is saved. The display windows have no
mode switch or test controls; everything here is driven from the `rfp` CLI.

```sh
rfp mode debug|workout
```

Switching restarts the installed `rfp.service`, so it needs the installed
service (it is refused when `REPS_APP_HOME` is set) and is refused while a
workout is active.

Debug uses temporary workout and hub data, so simulated sets never change your
real history or daily completion, and they are never uploaded. The automatic
timer and reminders are off, and `rfp start`, `snooze` and `skip` are refused.

## Testing manually

The `rfp debug` commands only work in Debug mode:

```sh
rfp debug exercises|videos|history          # list detectors, fixture clips, debug sets
rfp debug start [--exercise squat]          # start an isolated camera test
rfp debug step [--value N]                  # simulate progress (+1 without --value)
rfp debug done                              # mark the target reached
rfp debug stop                              # back to idle, camera released
rfp debug video --exercise squat --file PATH
rfp debug video-stop
```

`start` opens the display and turns the camera on. All shipped detectors are
available regardless of the daily routine; without `--exercise` it uses the
routine's next set. Lift tests target 10 reps, jump rope 60 seconds and
stretching 30 seconds.

`video` plays a fixture clip through the legacy counter and shows the annotated
frames in the main window. It does not award workout credit.

`rfp camera preview|stop` shows the live camera without workout credit and
works in either mode.

### Walking every phase

`rfp debug next [--weight LB]` advances one phase through the engine's own
transitions:

    CODING → EXERCISE_REQUIRED → WORKOUT_ACTIVE → WEIGHT_CONFIRMATION → UNLOCKED → CODING

From CODING it expires the timer and prescribes the next set; from
WORKOUT_ACTIVE it reaches the target; from WEIGHT_CONFIRMATION it logs
`--weight` (default: the set's default weight). Timed sets skip weight
confirmation. Debug has no 3-second "LOGGED" beat, so `next` also returns from
UNLOCKED to CODING. The camera runs only while a set is active. With no debug
session yet (idle), run `rfp debug start --exercise NAME` first.

### Detection states and programmed sets

```sh
rfp debug start --exercise squat --state live          # real camera (default)
rfp debug start --exercise squat --state counting      # camera off; drive with step/done/next
rfp debug start --exercise squat --state no-pose       # camera off; gym screen shows "Not in frame"
rfp debug start --exercise squat --state vision-down   # camera off; vision unavailable, manual mode
rfp debug start --exercise squat --reps 3 --weight 40  # program the target and default weight
rfp debug start --exercise jumprope --seconds 20       # timed target instead of reps
```

Programming a set needs `--exercise`. `vision-down` makes `display.vision`
unavailable until the set ends, as a failed detector would. During a live set
the gym screen shows **In frame** / **Not in frame** as the detector gains or
loses your pose.

### Finishing a set yourself

`rfp debug complete [--weight LB]` completes and logs the current set, in
either mode. It is recorded as unverified, the same as
`rfp finish --honor --weight LB`.

### Closing everything

`rfp debug stop --all` and `rfp cancel --all` also stop the camera preview and
fixture video, release the camera, and hide every window.

## Workout mode

Workout mode enables the automatic timer and reminders. The emergency release
is `rfp cancel`, which stops the current workout without completion credit.

## Movement Studio

The hub's `/studio.html` provides reviewed recordings, AI-assisted
configuration, held-out evaluation and version activation. New workouts use the
active version of their movement; existing presets remain available until
qualification. See
[setup and release qualification](../production/movement-studio.md).
