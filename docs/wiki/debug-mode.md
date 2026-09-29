# Debug mode

The desktop app opens in **Debug** on first launch. The **Debug / Workout**
switch stays visible in release builds and remembers your choice. Switching
modes restarts the app and stops the detector first.

Debug uses temporary workout and hub data, so simulated sets never change your
real history or daily completion, and they are never uploaded.

## Testing manually

Debug starts idle in a normal movable window. Use **Start camera**, **+1**,
**Done** and **Stop test** to drive a set by hand, or open **Video inspection**
for the bundled squat clip. Video inspection displays the legacy counter
preview; it does not award workout credit.

**Open gym window** opens an optional second window, which can be closed and
reopened. Closing the main window exits and releases owned camera processes.

Choose an **Exercise** in the bottom toolbar, then press **F2 Start camera**.
All shipped detectors are available regardless of the daily routine — changing
the selection during a test restarts detection with a fresh count. Lift tests
target 10 reps; jump rope targets 60 seconds and stretching 30 seconds.

The same surface is available from the CLI:

```sh
rfp debug exercises|videos|history
rfp debug start --exercise squat
rfp debug step --value 3
rfp debug done|stop
rfp debug video --exercise squat --file PATH
rfp debug video-stop
```

### Walking every state from the CLI

`rfp debug next [--weight LB]` advances one phase through the same transitions
the timer, detector and weight entry use:

    CODING → EXERCISE_REQUIRED → WORKOUT_ACTIVE → WEIGHT_CONFIRMATION → UNLOCKED → CODING

The camera turns on only when a set becomes active, and turns off again once
the set is filled. In debug mode, the 3-second "LOGGED" beat does not return to
coding automatically, so run `next` once more.

### Detection states and programmed sets

```sh
rfp debug start --exercise squat --state live          # real camera (default)
rfp debug start --exercise squat --state counting      # camera off; drive with step/done/next
rfp debug start --exercise squat --state no-pose       # camera off; UI shows "Not in frame"
rfp debug start --exercise squat --state vision-down   # camera off; UI shows "Camera down"
rfp debug start --exercise squat --reps 3 --weight 40  # program the target and default weight
rfp debug start --exercise jumprope --seconds 20       # timed target instead of reps
```

Every state can still be logged: after `done`/`next` use `rfp finish --weight N`,
or finish the set in one step with `rfp debug complete`.

### Finishing a set yourself

`rfp debug complete [--weight LB]` completes the current set and logs it. It
works in **workout mode too**: for when detection misses reps, or you're done.
It is recorded as unverified, the same as `rfp finish --honor`.

### Closing everything

`rfp debug stop --all` and `rfp cancel --all` also stop the camera preview and
fixture video, release the camera, and hide every window. In lock mode the
windows stay up.

During a live set, the main window shows **In frame** / **Not in frame** as the
detector gains or loses your pose.

## Workout mode

Workout mode enables the automatic timer and the window lock behavior. The
emergency release is **Ctrl+Shift+Backspace held for three seconds**.

## Movement Studio

The hub's `/studio.html` provides reviewed recordings, AI-assisted
configuration, held-out evaluation and version activation. New workouts use the
active version of their movement; existing presets remain available until
qualification. See
[setup and release qualification](../production/movement-studio.md).
