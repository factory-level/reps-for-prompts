"""Jump-rope time-in-a-row via pose bounce detection.

Honest limitation: this measures sustained vertical motion, not individual
rope swings. Good enough to gate an unlock; tune thresholds against real
C920x footage before trusting the exact seconds.
"""

from __future__ import annotations

from ..exercises import Landmarks
from .base import BreakActivity, Progress


class JumpRopeActivity(BreakActivity):
    unit = "seconds"

    def __init__(
        self,
        target_seconds: float,
        *,
        bounce_threshold: float = 0.015,
        reset_after: float = 2.0,
    ) -> None:
        self._target = target_seconds
        self._threshold = bounce_threshold
        self._reset_after = reset_after
        self._streak = 0.0
        self._last_now: float | None = None
        self._last_y: float | None = None
        self._still_since: float | None = None

    def update(self, landmarks: Landmarks | None, now: float) -> Progress:
        y = self._body_y(landmarks)
        if self._last_now is None:
            self._last_now, self._last_y = now, y
            return Progress(0.0, "seconds", False)

        dt = now - self._last_now
        moving = (
            y is not None
            and self._last_y is not None
            and abs(y - self._last_y) >= self._threshold
        )
        if moving:
            self._still_since = None
            self._streak += dt
        else:
            if self._still_since is None:
                self._still_since = now
            elif now - self._still_since >= self._reset_after:
                self._streak = 0.0
        self._last_now, self._last_y = now, y
        return Progress(self._streak, "seconds", self._streak >= self._target)

    @staticmethod
    def _body_y(landmarks: Landmarks | None) -> float | None:
        if not landmarks:
            return None
        pts = [landmarks.get("left_hip"), landmarks.get("right_hip")]
        ys = [p[1] for p in pts if p is not None]
        return sum(ys) / len(ys) if ys else None
