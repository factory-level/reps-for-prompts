"""Latest-frame capture: a single reader owns the device and bounds backlog.

Decode arrival is explicitly not sensor capture time. Consensus requires a
measured delay/uncertainty calibration; reconnection invalidates that mapping.
"""
import threading
import time
import uuid
from typing import Protocol


class Capture(Protocol):
    """What StreamLoop uses. Defaults describe a finite source (a file)."""

    live: bool = False  # True: a failed read is a disconnect to wait out, not the end
    epoch: str          # changes whenever the device is reopened
    time_ms: float | None = None  # source time of the frame last returned by read()

    def read(self) -> tuple[bool, object]: ...

    def release(self) -> None: ...

    def timing_trusted(self) -> bool:
        """Can frame times still be mapped onto the calibrated clock?"""
        return True


class LiveCapture(Capture):
    live = True

    def __init__(self, open_capture):
        self._open = open_capture
        self._capture = open_capture()
        self._condition = threading.Condition()
        self._stop = threading.Event()
        self._latest = None
        self.time_ms = None
        self.epoch = str(uuid.uuid4())
        self.reconnected = False
        self.timing_invalidated = False
        self._last_frame_ms = None
        self._thread = threading.Thread(target=self._run, daemon=True, name="reps-camera-reader")
        self._thread.start()

    def _run(self):
        delay = .25
        try:
            while not self._stop.is_set():
                ok, frame = self._capture.read()
                if ok:
                    delay = .25
                    now_ms = time.monotonic() * 1000
                    if self._last_frame_ms is not None and now_ms - self._last_frame_ms > 200:
                        self.timing_invalidated = True
                    self._last_frame_ms = now_ms
                    with self._condition:
                        self._latest = (frame, now_ms)
                        self._condition.notify_all()
                    continue
                self._capture.release()
                self.reconnected = True
                self.epoch = str(uuid.uuid4())
                with self._condition:
                    self._latest = None
                while not self._stop.wait(delay):
                    try:
                        self._capture = self._open()
                        break
                    except RuntimeError:
                        delay = min(delay * 2, 5)
                if self._stop.is_set():
                    break
        finally:
            self._capture.release()
            with self._condition:
                self._condition.notify_all()

    def read(self):
        with self._condition:
            self._condition.wait_for(lambda: self._latest is not None or self._stop.is_set(), timeout=.5)
            if self._latest is None:
                return False, None
            frame, self.time_ms = self._latest
            self._latest = None
            return True, frame

    def timing_trusted(self):
        # Both flags are sticky: a reconnect or a >200 ms gap voids the
        # calibration for the rest of this capture; only a new stream
        # (new calibration) restores it.
        return not self.reconnected and not self.timing_invalidated

    def release(self):
        self._stop.set()
        with self._condition:
            self._condition.notify_all()
        self._thread.join(timeout=5)
        if self._thread.is_alive():
            raise RuntimeError("capture reader failed to stop within timeout")
