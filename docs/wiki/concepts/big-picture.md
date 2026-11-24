# Start here

The desktop contains the workout timer and daily plan. A bundled local camera
hub runs the Reps pose detector. Live detection runs locally; optional cloud
assistance is used only when you consent to authoring from selected frames.

The app starts in **Debug**. The windows have no test controls; drive a test
from the CLI: `rfp debug start --exercise squat` turns the camera on,
`rfp debug step` and `rfp debug done` simulate progress, and `rfp debug stop`
ends the test (see [debug mode](../debug-mode.md)). Debug uses temporary workout and hub state and does not change your real
completion history. Video inspection is a legacy-counter preview and awards no
workout credit.

Switch to **Workout** for automatic timer and lock behavior. Switching modes
restarts the app and stops detection first. See [the loop](the-loop.md).
To build locally, see [development commands](../reference/cli.md).
