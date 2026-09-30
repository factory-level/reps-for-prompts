# Count with a USB webcam and Android phone

Open **Cameras**, enter your USB device and the phone app's RTSP or HTTP MJPEG URL, choose
**Both cameras must confirm**, set each camera rotation and save. The next rep workout
uses that camera set. Keep your whole body visible from both angles. Watch the
preview tiles and consensus status. A missing view never silently credits reps.

The Pixel 7 Pro test address is `rtsp://192.168.1.253:8554/live`, using IP Camera
by ShenYao. The working live test used authenticated HTTP MJPEG at
`http://192.168.1.253:8081/`; the RTSP endpoint did not deliver frames. For credentials use a private local
URL file and launch with `REPS_PHONE_URL_FILE` pointing at it.

Consensus requires an active qualified movement and measured timing calibration
in `camera-calibration.json` beside `cameras.json` in normal app data. Reconnect,
expiry or long delivery gaps require a new calibrated run. Do not use fabricated
clock values to make the counter advance. Until these checks pass, use the
single-camera option for ordinary workouts.

The full connection, calibration, recording and held-out comparison procedure is
in the sibling hub's [USB + phone guide](../../../usb-mcp-hub/docs/wiki/usb-phone-consensus.md).
From the workspace root, run
`reps-for-prompts/vision/.venv/bin/python reps-for-prompts/scripts/check-phone-camera.py`
for a short simultaneous connection check. It saves no video and releases both
cameras. Automated tests passing does not mean multi-camera accuracy is proven.

The Hub home page links to `/debug-cameras.html` for live previews of all
registered feeds with saved rotation. Stop these previews before a workout.
The physical USB + phone preview test delivered about 13 fps per camera; held-out
consensus accuracy and timing calibration remain unverified.
