"""Timed-hold stretch activity (honor-start; no posture detection in Phase 1)."""

from __future__ import annotations

from ..exercises import Landmarks
from .base import BreakActivity, Progress


class StretchActivity(BreakActivity):
    unit = "seconds"

    def __init__(self, target_seconds: float) -> None:
        self._target = target_seconds
        self._start: float | None = None

    def update(self, landmarks: Landmarks | None, now: float) -> Progress:
        if self._start is None:
            self._start = now
        held = now - self._start
        return Progress(held, "seconds", held >= self._target)
