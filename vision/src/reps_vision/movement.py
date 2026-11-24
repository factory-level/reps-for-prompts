"""Versioned, declarative movement cycles. No model, camera, or network IO."""
from __future__ import annotations

from copy import deepcopy
import math

from .angles import angle
from .activities.base import BreakActivity, Progress

JOINTS = {"shoulder", "elbow", "wrist", "hip", "knee", "ankle"}


def validate_movement(value: dict) -> dict:
    m = deepcopy(value)
    if m.get("schemaVersion") != 1:
        raise ValueError("movement.schemaVersion must be 1")
    features = m.get("features", {})
    if not isinstance(features, dict) or not 1 <= len(features) <= 12:
        raise ValueError("movement requires 1..12 angle features")
    for name, joints in features.items():
        if not isinstance(name, str) or not isinstance(joints, list) or len(joints) != 3 or any(j not in JOINTS for j in joints) or len(set(joints)) != 3:
            raise ValueError("features require three distinct supported joints")
    phases = m.get("phases", [])
    if not isinstance(phases, list) or not 2 <= len(phases) <= 8:
        raise ValueError("movement requires 2..8 ordered phases")
    names = set()
    for phase in phases:
        if not isinstance(phase.get("name"), str) or not phase["name"] or phase["name"] in names:
            raise ValueError("phase names must be nonempty and unique")
        names.add(phase["name"])
        conditions = phase.get("conditions", [])
        if not isinstance(conditions, list) or not 1 <= len(conditions) <= 12:
            raise ValueError("each phase requires conditions")
        phase["minDwellMs"] = _number(phase.get("minDwellMs", 80), 0, 2000, "minDwellMs")
        for condition in conditions:
            if condition.get("feature") not in features:
                raise ValueError("condition references unknown feature")
            low = _number(condition.get("min", 0), 0, 180, "min")
            high = _number(condition.get("max", 180), 0, 180, "max")
            if low > high:
                raise ValueError("condition min exceeds max")
    for field, default, low, high in (
        ("minVisibility", .65, 0, 1), ("smoothingMs", 60, 0, 1000),
        ("trackingLossMs", 500, 50, 5000), ("minCycleMs", 350, 0, 10000),
        ("maxCycleMs", 15000, 100, 120000),
    ):
        m[field] = _number(m.get(field, default), low, high, field)
    if m["minCycleMs"] >= m["maxCycleMs"]:
        raise ValueError("minCycleMs must be below maxCycleMs")
    if m.get("side", "auto") not in ("auto", "left", "right"):
        raise ValueError("side must be auto, left, or right")
    return m


def _number(value, low, high, field):
    if isinstance(value, bool) or not isinstance(value, (float, int)) or not math.isfinite(value) or not low <= value <= high:
        raise ValueError(f"{field} must be finite and between {low} and {high}")
    return float(value)


class MovementActivity(BreakActivity):
    unit = "reps"
    isotropic_landmarks = True

    def __init__(self, movement: dict, target_reps: int):
        self.movement = validate_movement(movement)
        if target_reps < 1:
            raise ValueError("targetReps must be positive")
        self.target = target_reps
        self.count = 0
        self.side = None
        self.values = {}
        self.next_phase = 0
        self.started = None
        self.dwell_start = None
        self.last_valid = None
        self.last_time = None
        self.reason = "missing_starting_pose"
        self.phase_history = []
        self.completed_cycle = None
        self.cycle_visibility = 1.0

    @property
    def diagnostics(self):
        return {"phase": self.movement["phases"][self.next_phase]["name"],
                "reason": self.reason, "side": self.side, "features": dict(self.values),
                "tracking": self.reason != "insufficient_visibility"}

    def reset(self, reason):
        self.next_phase = 0
        self.started = None
        self.dwell_start = None
        self.side = None
        self.values = {}
        self.reason = reason
        self.phase_history = []
        self.cycle_visibility = 1.0

    def _measure(self, landmarks):
        required = {j for triple in self.movement["features"].values() for j in triple}
        sides = [self.side] if self.side else ([self.movement["side"]] if self.movement.get("side", "auto") != "auto" else ["left", "right"])
        best = None
        for side in sides:
            points = [landmarks.get(f"{side}_{joint}") for joint in required]
            if any(p is None or len(p) < 3 or not all(math.isfinite(v) for v in p[:3]) for p in points):
                continue
            confidence = min(p[2] for p in points)
            if confidence >= self.movement["minVisibility"] and (best is None or confidence > best[0]):
                best = confidence, side
        if best is None:
            return None
        self.cycle_visibility = min(self.cycle_visibility, best[0])
        side = best[1]
        try:
            values = {name: angle(*(landmarks[f"{side}_{j}"][:2] for j in joints))
                      for name, joints in self.movement["features"].items()}
        except ValueError:
            return None
        self.side = side
        return values

    def update(self, landmarks, now: float) -> Progress:
        ms = now * 1000
        if not math.isfinite(ms) or (self.last_time is not None and ms <= self.last_time):
            raise ValueError("movement timestamps must be finite and strictly increasing")
        dt = ms - self.last_time if self.last_time is not None else 0
        self.last_time = ms
        if self.last_valid is not None and ms - self.last_valid > self.movement["trackingLossMs"]:
            self.reset("interrupted_tracking")
        raw = self._measure(landmarks or {})
        if raw is None:
            self.dwell_start = None
            self.reason = "insufficient_visibility"
            return self._progress()
        self.last_valid = ms
        tau = self.movement["smoothingMs"]
        alpha = 1 - math.exp(-dt / tau) if tau > 0 else 1
        self.values = {key: self.values.get(key, value) + alpha * (value - self.values.get(key, value)) for key, value in raw.items()}
        if self.started is not None and ms - self.started > self.movement["maxCycleMs"]:
            self.reset("incomplete_range")
            return self._progress()
        phase = self.movement["phases"][self.next_phase]
        matches = all(c.get("min", 0) <= self.values[c["feature"]] <= c.get("max", 180) for c in phase["conditions"])
        if not matches:
            self.dwell_start = None
            self.reason = "missing_starting_pose" if self.started is None else "incomplete_range"
            return self._progress()
        if self.dwell_start is None:
            self.dwell_start = ms
        self.reason = "holding_phase"
        if ms - self.dwell_start < phase["minDwellMs"]:
            return self._progress()
        self.dwell_start = None
        if self.next_phase == 0:
            if self.started is not None:
                if ms - self.started < self.movement["minCycleMs"]:
                    self.reset("cycle_too_fast")
                    return self._progress()
                self.completed_cycle = {"cycleId": str(self.count + 1),
                    "phases": [*self.phase_history, {"name": phase["name"], "atMs": ms}],
                    "visibility": self.cycle_visibility}
                self.count += 1
                self.reason = "rep_completed"
            else:
                self.reason = "ready"
            self.started = ms
            self.phase_history = []
            self.cycle_visibility = 1.0
        else:
            self.reason = "phase_completed"
        self.phase_history.append({"name": phase["name"], "atMs": ms})
        self.next_phase = (self.next_phase + 1) % len(self.movement["phases"])
        return self._progress()

    def _progress(self):
        return Progress(float(self.count), "reps", self.count >= self.target)
