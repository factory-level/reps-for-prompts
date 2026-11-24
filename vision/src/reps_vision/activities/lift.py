"""Rep-counting break activity built on the existing angle state machine."""

from __future__ import annotations

from ..angles import RepStateMachine
from ..exercises import ExerciseSpec, Landmarks, get_spec
from .base import BreakActivity, Progress


class LiftActivity(BreakActivity):
    unit = "reps"

    def __init__(self, exercise: str | ExerciseSpec, target_reps: int) -> None:
        self._spec = exercise if isinstance(exercise, ExerciseSpec) else get_spec(exercise)
        self._machine = RepStateMachine(self._spec.down_below, self._spec.up_above)
        self._target = target_reps
        self._count = 0

    def update(self, landmarks: Landmarks | None, now: float) -> Progress:
        if landmarks is not None:
            angle = self._spec.angle_from(landmarks)
            if angle is not None and self._machine.update(angle):
                self._count += 1
        return Progress(float(self._count), "reps", self._count >= self._target)
