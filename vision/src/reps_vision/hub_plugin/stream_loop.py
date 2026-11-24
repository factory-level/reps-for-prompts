"""Full-rate capture loop: frames → landmarks → activity → emitted streams.

Camera-gating: the capture device is handed in already-open by the plugin's
``start_stream`` and released here when the loop stops — it is never open
while no stream runs.
"""

from __future__ import annotations

import threading
import time
import uuid
import math
import base64


class StreamLoop:
    def __init__(self, *, activity, spec, estimator, capture, emit, frame_times_ms=None, provenance=None, timing_calibration=None):
        self._timing = timing_calibration or {}
        self._epoch = str(uuid.uuid4())
        self._clock_offset = time.time() * 1000 - time.monotonic() * 1000
        self._preview_at = 0.0
        self._frame_times_ms = frame_times_ms
        self._provenance = provenance or {}
        self._activity = activity
        self._spec = spec
        self._estimator = estimator
        self._capture = capture
        self._emit = emit
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, name="reps-vision-stream", daemon=True)

    def start(self) -> None:
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        self._thread.join()

    def _timing_eligible(self, ts_us):
        uncertainty = self._timing.get("uncertaintyMs")
        if not isinstance(uncertainty, (int, float)) or not math.isfinite(uncertainty) or uncertainty < 0:
            return False
        if hasattr(self._capture, "reconnected"):
            delay = self._timing.get("delayMs")
            expiry = self._timing.get("validUntilMs")
            return (isinstance(delay, (int, float)) and math.isfinite(delay) and delay >= 0
                    and isinstance(expiry, (int, float)) and math.isfinite(expiry) and ts_us / 1000 <= expiry
                    and not self._capture.reconnected and not self._capture.timing_invalidated)
        offset = self._timing.get("offsetMs")
        return isinstance(offset, (int, float)) and math.isfinite(offset)

    def _run(self) -> None:
        last_value = 0.0
        last_unit = "reps"
        was_satisfied = False
        source_ended = False
        frame_index = 0
        try:
            while not self._stop.is_set():
                ok, frame = self._capture.read()
                if not ok:
                    if hasattr(self._capture, "reconnected"):
                        self._emit("progress", {**self._provenance, "value": last_value, "unit": last_unit, "satisfied": False,
                            "cameraStatus": "disconnected"})
                        continue
                    source_ended = True
                    break
                frame_index += 1
                ts_us = time.time_ns() // 1000
                source_ms = (self._frame_times_ms[frame_index - 1] if self._frame_times_ms is not None
                             else getattr(self._capture, "time_ms", None))
                if source_ms is None:
                    source_ms = time.monotonic() * 1000
                if hasattr(self._capture, "epoch") and self._epoch != self._capture.epoch:
                    self._epoch = self._capture.epoch
                    if hasattr(self._activity, "_reset"):
                        self._activity._reset("camera_reconnected")
                if hasattr(frame, "shape") and time.monotonic() - self._preview_at >= .05:
                    import cv2
                    ok_jpeg, jpeg = cv2.imencode(".jpg", frame, [cv2.IMWRITE_JPEG_QUALITY, 70])
                    if ok_jpeg:
                        self._emit("frame", {**self._provenance, "jpegB64": base64.b64encode(jpeg).decode(),
                            "sourceTimeMs": source_ms, "receivedAtMs": ts_us / 1000, "frameIndex": frame_index})
                    self._preview_at = time.monotonic()
                landmarks = self._estimator.landmarks(frame, timestamp_ms=source_ms)
                angle = (
                    self._spec.angle_from(landmarks)
                    if (self._spec is not None and landmarks is not None)
                    else None
                )
                visibility = (
                    sum(point[2] for point in landmarks.values()) / len(landmarks)
                    if landmarks
                    else 0.0
                )
                self._emit(
                    "landmarks",
                    {
                        **self._provenance,
                        "frameIndex": frame_index,
                        "sourceTimeMs": source_ms,
                        "poseDetected": landmarks is not None,
                        "visibility": visibility,
                        "landmarks": landmarks or {},
                        "angle": angle,
                        "measuredJoints": list(self._spec.measured_joints(landmarks) or ())
                        if (self._spec is not None and landmarks is not None)
                        else [],
                        "tsUs": ts_us,
                    },
                )
                activity_landmarks = landmarks
                # Pose coordinates normalize x and y independently. Temporal
                # geometry needs isotropic coordinates; retain normalized points
                # for overlays and preserve legacy tuned angle behavior.
                if landmarks and hasattr(self._activity, "movement") and hasattr(frame, "shape"):
                    aspect = frame.shape[1] / frame.shape[0]
                    activity_landmarks = {name: (p[0] * aspect, p[1], p[2]) for name, p in landmarks.items()}
                progress = self._activity.update(activity_landmarks, source_ms / 1000)
                if progress.unit == "reps" and progress.value > last_value:
                    cycle = getattr(self._activity, "completed_cycle", None)
                    evidence = None
                    uncertainty = self._timing.get("uncertaintyMs")
                    offset = self._timing.get("offsetMs")
                    if hasattr(self._capture, "reconnected"):
                        delay = self._timing.get("delayMs")
                        valid_until = self._timing.get("validUntilMs", 0)
                        if isinstance(delay, (int, float)) and math.isfinite(delay) and delay >= 0 and isinstance(valid_until, (int, float)) and ts_us / 1000 <= valid_until and not self._capture.reconnected and not self._capture.timing_invalidated:
                            offset = self._clock_offset - delay
                        else:
                            offset = None
                    if cycle and all(isinstance(v, (int, float)) and math.isfinite(v) for v in (offset, uncertainty)) and uncertainty >= 0:
                        evidence = {**cycle, "epoch": self._epoch, "subjectId": "scene-person-1",
                            "uncertaintyMs": uncertainty,
                            "phases": [{**p, "atMs": p["atMs"] + offset} for p in cycle["phases"]]}
                    self._emit("event", {"type": "rep_completed", "count": int(progress.value), "frameIndex": frame_index, "sourceTimeMs": source_ms,
                        "cycleEvidence": evidence, **self._provenance})
                self._emit(
                    "progress",
                    {
                        "value": progress.value,
                        "unit": progress.unit,
                        "satisfied": progress.satisfied,
                        "diagnostics": getattr(self._activity, "diagnostics", None),
                        "timingEligible": self._timing_eligible(ts_us),
                        "frameIndex": frame_index,
                        "sourceTimeMs": source_ms,
                        **self._provenance,
                        "tsUs": ts_us,
                    },
                )
                last_value = progress.value
                last_unit = progress.unit
                if progress.satisfied and not was_satisfied:
                    self._emit("event", {"type": "target_reached", "value": progress.value, **self._provenance})
                was_satisfied = progress.satisfied
            if source_ended:
                # File finished or camera died — consumers must not wait
                # forever for frames that will never come.
                self._emit("event", {"type": "stream_ended", "finalValue": last_value, **self._provenance})
        except Exception as exc:
            self._emit("event", {"type": "detector_error", "message": str(exc), **self._provenance})
        finally:
            self._capture.release()
            close = getattr(self._estimator, "close", None)
            if close is not None:
                close()
