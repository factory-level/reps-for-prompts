"""usb-mcp-hub plugin: pose estimation + config-driven workout activities.

The plugin is the model RFP ships into the hub SDK. It is fully
config-driven — nothing here names a specific exercise; the consumer passes
exercise definitions (joints, thresholds, targets) through ``configure``.

Observation taxonomy: every output is one of
  event    — discrete occurrences (``rep_completed``, ``target_reached``)
  duration — time spent doing an action (jump-rope streak, stretch hold)
  range    — a measured value with min/max bounds (joint angle)
"""

from __future__ import annotations

import json
from pathlib import Path

from ..activities.base import BreakActivity
from ..activities.jumprope import JumpRopeActivity
from ..activities.lift import LiftActivity
from ..activities.stretch import StretchActivity
from ..exercises import ExerciseSpec
from ..movement import MovementActivity, validate_movement
from ..pose import MODEL_SHA256
from .live_capture import Capture, LiveCapture
from .stream_loop import StreamLoop

MODEL = {
    "name": "mediapipe-pose-landmarker",
    "variant": "full",
    "sha256": MODEL_SHA256,
    "source": "rfp",
}

CONFIG_SCHEMA = {
    "fields": [
        {
            "name": "activity",
            "type": "enum",
            "values": ["lift", "jumprope", "stretch"],
            "default": "lift",
        },
        {"name": "exercise.downBelow", "type": "number", "min": 0, "max": 180, "step": 1,
         "default": 110, "label": "Down threshold (°)"},
        {"name": "exercise.upAbove", "type": "number", "min": 0, "max": 180, "step": 1,
         "default": 160, "label": "Up threshold (°)"},
        {"name": "exercise.minVisibility", "type": "number", "min": 0, "max": 1,
         "step": 0.05, "default": 0.5, "label": "Min landmark visibility"},
        {"name": "targetReps", "type": "number", "min": 1, "max": 100, "step": 1,
         "default": 10},
        {"name": "targetSeconds", "type": "number", "min": 5, "max": 600, "step": 5,
         "default": 60},
        {"name": "bounceThreshold", "type": "number", "min": 0.005, "max": 0.05,
         "step": 0.005, "default": 0.015, "label": "Jump-rope bounce threshold"},
        {"name": "resetAfter", "type": "number", "min": 0, "max": 10, "step": 0.5,
         "default": 2.0, "label": "Jump-rope grace (s)"},
        {"name": "holdSeconds", "type": "number", "min": 5, "max": 300, "step": 5,
         "default": 30, "label": "Stretch hold (s)"},
    ]
}

OBSERVATION_SCHEMA = {
    "rep_completed": {"kind": "event"},
    "target_reached": {"kind": "event"},
    "activeSeconds": {"kind": "duration"},
    "angle": {"kind": "range", "min": 0, "max": 180},
}


class RepsVisionPlugin:
    plugin_id = "reps_vision"

    def __init__(self, estimator_factory=None, capture_factory=None, imread=None):
        self._estimator_factory = estimator_factory or _mediapipe_estimator
        self._capture_factory = capture_factory or _cv2_capture
        self._imread = imread or _cv2_imread
        self._config: dict | None = None
        self._spec: ExerciseSpec | None = None
        self._loop: StreamLoop | None = None
        self._eval_estimator = None

    def describe_capabilities(self) -> dict:
        return {
            "supports": ["evaluate", "stream"],
            "cycleEvidenceVersion": 1,
            "model": MODEL,
            "configSchema": CONFIG_SCHEMA,
            "observationSchema": OBSERVATION_SCHEMA,
            "movementTemplates": json.loads((Path(__file__).parent.parent / "movement_templates.json").read_text()),
            "movementContract": "config.movement is a schemaVersion:1 temporal detector; features map names to joint triples, phases are ordered angle conditions, returning to the first phase completes a repetition. See movementTemplates for examples. Templates are unqualified until held-out evaluation passes.",
        }

    def configure(self, params: dict) -> dict:
        activity = params.get("activity", "lift")
        if activity == "lift":
            if "movement" in params:
                validate_movement(params["movement"])
                self._spec = None
            else:
                self._spec = ExerciseSpec.from_config(params["exercise"])
        elif activity in ("jumprope", "stretch"):
            self._spec = None
        else:
            raise ValueError(f"unknown activity: {activity}")
        self._config = dict(params)
        return self._effective_config()

    def evaluate(self, frame_path: str) -> dict:
        self._require_configured()
        if self._eval_estimator is None:
            self._eval_estimator = self._estimator_factory()
        frame = self._imread(frame_path)
        landmarks = self._eval_estimator.landmarks(frame)
        angle = self._spec.angle_from(landmarks) if (self._spec and landmarks) else None
        return {
            "poseDetected": landmarks is not None,
            "angle": angle,
            "measuredJoints": list(self._spec.measured_joints(landmarks) or ())
            if (self._spec and landmarks)
            else [],
        }

    def start_stream(self, config: dict, emit) -> None:
        self._require_configured()
        if self._loop is not None:
            raise RuntimeError("stream already running")
        camera = (config or {}).get("camera") or (self._config or {}).get("camera") or {}
        activity = self._build_activity()
        capture = self._capture_factory(camera)
        try:
            estimator = self._estimator_factory()
        except Exception:
            capture.release()
            raise
        self._loop = StreamLoop(
            activity=activity,
            spec=self._spec,
            estimator=estimator,
            capture=capture,
            emit=emit,
            frame_times_ms=(self._config or {}).get("replayTimesMs"),
            timing_calibration=(self._config or {}).get("timingCalibration"),
            provenance={key: self._config[key] for key in ("movementId", "movementVersion", "sessionId", "detectionId") if key in (self._config or {})},
        )
        self._loop.start()

    def stop_stream(self) -> None:
        if self._loop is None:
            return
        self._loop.stop()
        self._loop = None

    def _build_activity(self) -> BreakActivity:
        config = self._config or {}
        activity = config.get("activity", "lift")
        if activity == "lift":
            if "movement" in config:
                return MovementActivity(config["movement"], int(config.get("targetReps", 10)))
            assert self._spec is not None
            return LiftActivity(self._spec, int(config.get("targetReps", 10)))
        if activity == "jumprope":
            return JumpRopeActivity(
                float(config.get("targetSeconds", 60.0)),
                bounce_threshold=float(config.get("bounceThreshold", 0.015)),
                reset_after=float(config.get("resetAfter", 2.0)),
            )
        return StretchActivity(float(config.get("holdSeconds", 30.0)))

    def _effective_config(self) -> dict:
        config = dict(self._config or {})
        if self._spec is not None:
            config["exercise"] = {
                "name": self._spec.name,
                "joints": list(self._spec.joints),
                "downBelow": self._spec.down_below,
                "upAbove": self._spec.up_above,
                "minVisibility": self._spec.min_visibility,
            }
        return config

    def _require_configured(self) -> None:
        if self._config is None:
            raise RuntimeError("configure must be called before use")


def _mediapipe_estimator():
    from ..pose import PoseEstimator

    return PoseEstimator()


def _redact_camera(camera: dict) -> dict:
    """Camera dict safe for error messages: strip URI credentials."""
    import re

    safe = dict(camera)
    if isinstance(safe.get("value"), str):
        safe["value"] = re.sub(r"//[^@/]+@", "//***@", safe["value"])
    return safe


def _cv2_capture(camera: dict):
    if camera.get("source") == "file":
        return _open_cv2_capture(camera)
    return LiveCapture(lambda: _open_cv2_capture(camera))


def _open_cv2_capture(camera: dict):
    import os
    os.environ.setdefault("OPENCV_FFMPEG_CAPTURE_OPTIONS", "rtsp_transport;tcp")
    import cv2

    source = camera.get("source", "index")
    value = camera.get("value", 0)
    if isinstance(value, str) and value.startswith(("rtsp://", "rtsps://")):
        capture = cv2.VideoCapture(value, cv2.CAP_FFMPEG, [cv2.CAP_PROP_OPEN_TIMEOUT_MSEC, 2000, cv2.CAP_PROP_READ_TIMEOUT_MSEC, 2000])
    else:
        capture = cv2.VideoCapture(value if source in ("file", "uri") else int(value))
    if not capture.isOpened():
        raise RuntimeError(f"cannot open camera: {_redact_camera(camera)}")
    if "width" in camera:
        capture.set(cv2.CAP_PROP_FRAME_WIDTH, int(camera["width"]))
    return _Capture(capture, camera)


class _Capture(Capture):
    def __init__(self, capture, camera):
        import cv2
        import uuid
        self.epoch = str(uuid.uuid4())
        self.capture = capture
        self.rotate = int(camera.get("rotate", 0))
        if self.rotate not in (0, 90, 180, 270):
            capture.release()
            raise ValueError("camera rotation must be 0, 90, 180, or 270")
        self.file = camera.get("source") == "file"
        self.fps = (capture.get(cv2.CAP_PROP_FPS) or 30) if self.file else 30
        self.index = 0
        self.time_ms = None

    def read(self):
        import cv2
        ok, frame = self.capture.read()
        if ok:
            if self.rotate:
                rotation = {90: cv2.ROTATE_90_CLOCKWISE, 180: cv2.ROTATE_180, 270: cv2.ROTATE_90_COUNTERCLOCKWISE}[self.rotate]
                frame = cv2.rotate(frame, rotation)
            if self.file:
                timestamp = self.capture.get(cv2.CAP_PROP_POS_MSEC)
                self.time_ms = timestamp if self.time_ms is None or timestamp > self.time_ms else self.time_ms + 1000 / self.fps
            self.index += 1
        return ok, frame

    def release(self):
        self.capture.release()


def _cv2_imread(frame_path: str):
    import cv2

    frame = cv2.imread(frame_path)
    if frame is None:
        raise ValueError(f"cannot read frame: {frame_path}")
    return frame
