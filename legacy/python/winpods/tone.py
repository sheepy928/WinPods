"""A generated test signal that stands in for Windows audio capture.

Plays a short, quiet 880 Hz beep once per second on the receiver only (nothing is played
on the PC), which makes it easy to check that streaming works and to judge the delay.
"""

from __future__ import annotations

import logging
import threading
from time import perf_counter, sleep
from typing import Optional

import numpy as np

from .ringbuffer import ByteRing

_LOGGER = logging.getLogger(__name__)

RATE = 44100
CHUNK = 220  # frames per write, about 5 ms like a WASAPI callback
BEEP_HZ = 880.0
BEEP_SECONDS = 0.12
AMPLITUDE = 0.08  # about -22 dBFS


class ToneSource:
    """Same interface as AudioCapture, but synthesises a beep track."""

    def __init__(self, ring: ByteRing) -> None:
        self.ring = ring
        self.device_name = "Test tone (880 Hz beep each second)"
        self.device_rate = RATE
        self.device_channels = 2
        self.peak = 0.0
        self.callbacks = 0
        self.frames_in = 0
        self.error: Optional[str] = None
        self._halt = threading.Event()
        self._thread: Optional[threading.Thread] = None

        t = np.arange(RATE) / RATE
        envelope = np.clip(np.minimum(t, BEEP_SECONDS - t) / 0.005, 0.0, 1.0)
        envelope[t >= BEEP_SECONDS] = 0.0
        mono = (AMPLITUDE * 32767 * envelope * np.sin(2 * np.pi * BEEP_HZ * t)).astype(np.int16)
        self._second = np.repeat(mono[:, None], 2, axis=1).reshape(-1).astype(">i2").tobytes()

    def start(self) -> None:
        self._thread = threading.Thread(target=self._run, name="winpods-tone", daemon=True)
        self._thread.start()
        _LOGGER.info("Test tone source started")

    def stop(self) -> None:
        self._halt.set()
        if self._thread is not None:
            self._thread.join(timeout=1.0)
            self._thread = None

    @property
    def running(self) -> bool:
        return self._thread is not None and self._thread.is_alive()

    def _run(self) -> None:
        start = perf_counter()
        written = 0
        position = 0
        second = self._second
        bytes_per_second = len(second)
        while not self._halt.is_set():
            due = int((perf_counter() - start) * RATE)
            while written + CHUNK <= due:
                begin = position * 4
                end = begin + CHUNK * 4
                if end <= bytes_per_second:
                    chunk = second[begin:end]
                else:
                    chunk = second[begin:] + second[: end - bytes_per_second]
                self.ring.write(chunk)
                written += CHUNK
                position = (position + CHUNK) % RATE
                self.frames_in = written
                self.callbacks += 1
                self.peak = AMPLITUDE if position < BEEP_SECONDS * RATE else 0.0
            sleep(0.002)
