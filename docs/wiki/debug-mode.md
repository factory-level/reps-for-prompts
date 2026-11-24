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

## Workout mode

Workout mode enables the automatic timer and reminders. The emergency release
is `rfp cancel`, which stops the current workout without completion credit.

## Movement Studio

The hub's `/studio.html` provides reviewed recordings, AI-assisted
configuration, held-out evaluation and version activation. New workouts use the
active version of their movement; existing presets remain available until
qualification. See
[setup and release qualification](../production/movement-studio.md).
