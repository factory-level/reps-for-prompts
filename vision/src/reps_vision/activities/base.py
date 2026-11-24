"""The BreakActivity contract: one frame in, live Progress out."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Protocol

from ..exercises import Landmarks


@dataclass(frozen=True)
class Progress:
    value: float   # reps or seconds accumulated
    unit: str      # "reps" | "seconds"
    satisfied: bool


class BreakActivity(Protocol):
    """What StreamLoop uses. Activities inherit this and override what differs."""

    unit: str = "reps"  # the unit every Progress carries; known before the first frame
    # True when update() needs x scaled by the frame aspect ratio so that
    # distances and angles are isotropic (pose x and y normalize independently).
    isotropic_landmarks: bool = False
    completed_cycle: dict | None = None  # phases of the rep that last raised the count
    diagnostics: dict | None = None      # why the activity is (not) counting right now

    def update(self, landmarks: Landmarks | None, now: float) -> Progress: ...

    def reset(self, reason: str) -> None:
        """Drop in-flight state after a discontinuity (camera reconnect). Totals stay."""
