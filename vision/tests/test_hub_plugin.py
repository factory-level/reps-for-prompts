"""Tests for the hub SDK plugin surface (contract: usb-mcp-hub vision-host)."""

import time

from reps_vision.hub_plugin.plugin import RepsVisionPlugin

SQUAT_CONFIG = {
    "activity": "lift",
    "exercise": {
        "name": "squat",
        "joints": ["hip", "knee", "ankle"],
        "downBelow": 110.0,
        "upAbove": 160.0,
    },
    "targetReps": 2,
}


def leg_pose(bent: bool):
    """Left-side landmarks: straight leg ~180deg, bent leg ~90deg at the knee."""
    if bent:
        points = {"left_hip": (0.5, 0.2, 0.9), "left_knee": (0.5, 0.5, 0.9), "left_ankle": (0.8, 0.5, 0.9)}
    else:
        points = {"left_hip": (0.5, 0.2, 0.9), "left_knee": (0.5, 0.5, 0.9), "left_ankle": (0.5, 0.8, 0.9)}
    return points


class FakeEstimator:
    def __init__(self, frames):
        self.frames = list(frames)

    def landmarks(self, frame_bgr, *, timestamp_ms=None):
        return self.frames.pop(0) if self.frames else None

    def close(self):
        pass


class FakeCapture:
    def __init__(self, count):
        self.remaining = count
        self.released = False

    def read(self):
        if self.remaining > 0:
            self.remaining -= 1
            return True, object()
        return False, None

    def release(self):
        self.released = True


def collect(emitted, min_count, timeout=2.0):
    deadline = time.time() + timeout
    while len(emitted) < min_count and time.time() < deadline:
        time.sleep(0.01)
    return list(emitted)


def make_plugin(estimator, capture):
    return RepsVisionPlugin(
        estimator_factory=lambda: estimator,
        capture_factory=lambda camera: capture,
    )


def test_capabilities_declare_model_schema_and_observation_kinds():
    caps = RepsVisionPlugin().describe_capabilities()
    assert set(caps["supports"]) == {"evaluate", "stream"}
    # the consumer (RFP) declares the model it uses
    assert caps["model"]["name"] == "mediapipe-pose-landmarker"
    assert caps["model"]["variant"] == "full"
    field_names = [f["name"] for f in caps["configSchema"]["fields"]]
    assert "activity" in field_names
    assert "exercise.downBelow" in field_names
    # every observation is an event, a duration, or a min/max range
    kinds = {entry["kind"] for entry in caps["observationSchema"].values()}
    assert kinds == {"event", "duration", "range"}
    assert caps["observationSchema"]["rep_completed"]["kind"] == "event"
    assert caps["observationSchema"]["activeSeconds"]["kind"] == "duration"
    assert caps["observationSchema"]["angle"] == {"kind": "range", "min": 0, "max": 180}


def test_configure_builds_lift_from_config_without_name_registry():
    plugin = RepsVisionPlugin()
    effective = plugin.configure(SQUAT_CONFIG)
    assert effective["activity"] == "lift"
    assert effective["exercise"]["downBelow"] == 110.0
    assert effective["targetReps"] == 2


def test_configure_rejects_unknown_activity():
    plugin = RepsVisionPlugin()
    try:
        plugin.configure({"activity": "yoga"})
    except ValueError as exc:
        assert "unknown activity" in str(exc)
    else:
        raise AssertionError("expected ValueError")


def test_stream_emits_landmarks_progress_and_rep_events():
    # two full squats: up, down, up, down, up
    poses = [leg_pose(False), leg_pose(True), leg_pose(False), leg_pose(True), leg_pose(False)]
    estimator = FakeEstimator(poses)
    capture = FakeCapture(len(poses))
    plugin = make_plugin(estimator, capture)
    provenance = {"movementId": "squat", "movementVersion": "v1", "sessionId": "workout-1", "detectionId": "run-1"}
    plugin.configure({**SQUAT_CONFIG, **provenance})

    emitted = []
    plugin.start_stream({"camera": {"source": "index", "value": 0}}, lambda s, d: emitted.append((s, d)))
    frames = collect(emitted, 12)
    plugin.stop_stream()

    assert capture.released
    assert all(all(data.get(key) == value for key, value in provenance.items()) for _, data in frames)
    streams = {s for s, _ in frames}
    assert {"landmarks", "progress", "event"} <= streams
    landmark_frames = [d for s, d in frames if s == "landmarks"]
    assert landmark_frames[0]["poseDetected"] is True
    assert landmark_frames[0]["angle"] is not None
    assert "tsUs" in landmark_frames[0]
    # fusion election score: mean landmark visibility, 0.0 when no pose
    assert abs(landmark_frames[0]["visibility"] - 0.9) < 1e-6
    events = [d for s, d in frames if s == "event"]
    rep_events = [e for e in events if e["type"] == "rep_completed"]
    assert [e["count"] for e in rep_events] == [1, 2]
    assert any(e["type"] == "target_reached" for e in events)
    # the capture ran out of frames: consumers are told the stream ended
    assert events[-1]["type"] == "stream_ended"
    progress = [d for s, d in frames if s == "progress"]
    assert progress[-1]["value"] == 2.0
    assert progress[-1]["satisfied"] is True
    committed_count = 0
    for stream, data in frames:
        if stream == "event" and data["type"] == "rep_completed":
            committed_count = data["count"]
        if stream == "progress":
            assert data["value"] == committed_count


def test_stream_reports_zero_visibility_without_pose():
    estimator = FakeEstimator([None, None])
    capture = FakeCapture(2)
    plugin = make_plugin(estimator, capture)
    plugin.configure(SQUAT_CONFIG)
    emitted = []
    plugin.start_stream({"camera": {"source": "index", "value": 0}}, lambda s, d: emitted.append((s, d)))
    frames = collect(emitted, 2)
    plugin.stop_stream()
    landmark_frames = [d for s, d in frames if s == "landmarks"]
    assert landmark_frames[0]["visibility"] == 0.0


def test_cv2_capture_passes_uri_sources_through():
    """source:'uri' (go2rtc RTSP restream) must reach cv2 as the string URI."""
    from reps_vision.hub_plugin import plugin as plugin_module

    class FakeCv2:
        CAP_PROP_FRAME_WIDTH = 3
        CAP_FFMPEG = 1900
        CAP_PROP_OPEN_TIMEOUT_MSEC = 53
        CAP_PROP_READ_TIMEOUT_MSEC = 54

        class FakeVideoCapture:
            def __init__(self, value):
                self.value = value

            def isOpened(self):
                return True

            def set(self, *_args):
                pass

        def VideoCapture(self, value, *args):
            self.opened = value
            return self.FakeVideoCapture(value)

    fake = FakeCv2()
    import sys
    real = sys.modules.get("cv2")
    sys.modules["cv2"] = fake
    try:
        capture = plugin_module._open_cv2_capture(
            {"source": "uri", "value": "rtsp://127.0.0.1:8554/front"}
        )
        assert capture.capture.value == "rtsp://127.0.0.1:8554/front"
        capture = plugin_module._open_cv2_capture({"source": "index", "value": "0"})
        assert capture.capture.value == 0
    finally:
        if real is not None:
            sys.modules["cv2"] = real
        else:
            sys.modules.pop("cv2", None)


def test_capture_error_redacts_uri_credentials():
    from reps_vision.hub_plugin.plugin import _redact_camera

    safe = _redact_camera({"source": "uri", "value": "rtsp://user:secret@10.0.0.5/front"})
    assert safe["value"] == "rtsp://***@10.0.0.5/front"
    assert "secret" not in str(safe)


def test_stream_jumprope_reports_duration():
    # hip bouncing every frame -> streak accrues
    poses = []
    for i in range(6):
        y = 0.5 + (0.03 if i % 2 else -0.03)
        poses.append({"left_hip": (0.5, y, 0.9), "right_hip": (0.5, y, 0.9)})
    estimator = FakeEstimator(poses)
    capture = FakeCapture(len(poses))
    plugin = make_plugin(estimator, capture)
    # zero target: satisfied as soon as the streak exists — the activity's
    # real timing behavior is covered by its own unit tests; this verifies
    # the duration plumbing through the stream.
    plugin.configure({"activity": "jumprope", "targetSeconds": 0.0, "resetAfter": 2.0})

    emitted = []
    plugin.start_stream({"camera": {"source": "index", "value": 0}}, lambda s, d: emitted.append((s, d)))
    frames = collect(emitted, 8)
    plugin.stop_stream()

    progress = [d for s, d in frames if s == "progress"]
    assert progress[-1]["unit"] == "seconds"
    assert any(p["satisfied"] for p in progress)


def test_disconnected_progress_keeps_the_stream_unit():
    class LiveFakeCapture(FakeCapture):
        reconnected = False
        timing_invalidated = False

    poses = [{"left_hip": (0.5, 0.5, 0.9), "right_hip": (0.5, 0.5, 0.9)}] * 3
    plugin = make_plugin(FakeEstimator(poses), LiveFakeCapture(len(poses)))
    plugin.configure({"activity": "jumprope", "targetSeconds": 30.0, "resetAfter": 2.0})

    emitted = []
    plugin.start_stream({"camera": {"source": "index", "value": 0}}, lambda s, d: emitted.append((s, d)))
    deadline = time.time() + 2.0
    while time.time() < deadline and not any(d.get("cameraStatus") for _, d in list(emitted)):
        time.sleep(0.01)
    plugin.stop_stream()

    disconnected = [d for s, d in emitted if s == "progress" and d.get("cameraStatus") == "disconnected"]
    assert disconnected and disconnected[0]["unit"] == "seconds"


def test_stream_requires_configure_first():
    plugin = make_plugin(FakeEstimator([]), FakeCapture(0))
    try:
        plugin.start_stream({}, lambda s, d: None)
    except RuntimeError as exc:
        assert "configure" in str(exc)
    else:
        raise AssertionError("expected RuntimeError")


def test_evaluate_single_frame_reports_pose_and_angle():
    plugin = RepsVisionPlugin(
        estimator_factory=lambda: FakeEstimator([leg_pose(True)]),
        imread=lambda path: object(),
    )
    plugin.configure(SQUAT_CONFIG)
    result = plugin.evaluate("/tmp/frame.jpg")
    assert result["poseDetected"] is True
    assert 80 < result["angle"] < 100
    plugin2 = RepsVisionPlugin(
        estimator_factory=lambda: FakeEstimator([None]),
        imread=lambda path: object(),
    )
    plugin2.configure(SQUAT_CONFIG)
    assert plugin2.evaluate("/tmp/f.jpg")["poseDetected"] is False


def test_replay_source_timestamps_reach_pose_estimator():
    from reps_vision.hub_plugin.stream_loop import StreamLoop
    from types import SimpleNamespace

    received = []
    estimator = FakeEstimator([None] * 3)
    estimator.landmarks = lambda frame, *, timestamp_ms: received.append(timestamp_ms)
    activity = SimpleNamespace(update=lambda *_: SimpleNamespace(value=0, unit='reps', satisfied=False))
    loop = StreamLoop(activity=activity, spec=None, estimator=estimator,
                      capture=FakeCapture(3), emit=lambda *_: None,
                      frame_times_ms=[0, 125, 900])
    loop._run()
    assert received == [0, 125, 900]
