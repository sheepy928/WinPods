"""Thread-safe byte FIFO between the audio capture callback and the RTP sender."""

from __future__ import annotations

import threading
from time import perf_counter
from typing import Optional


class ByteRing:
    """A bounded FIFO of PCM bytes. Oldest data is dropped on overflow."""

    def __init__(self, capacity: int) -> None:
        self._buf = bytearray()
        self._capacity = capacity
        self._cond = threading.Condition()
        self.dropped_bytes = 0  # discarded because the consumer fell behind
        self.total_written = 0
        self.last_write: float = 0.0  # perf_counter() of the most recent write
        self.last_chunk: int = 0  # size in bytes of the most recent write

    def write(self, data: bytes) -> None:
        """Append data, waking any reader that is waiting for it."""
        with self._cond:
            self._buf += data
            self.total_written += len(data)
            self.last_write = perf_counter()
            self.last_chunk = len(data)
            if len(self._buf) > self._capacity:
                excess = len(self._buf) - self._capacity
                del self._buf[:excess]
                self.dropped_bytes += excess
            self._cond.notify_all()

    def read(self, size: int, timeout: Optional[float] = None) -> Optional[bytes]:
        """Return exactly `size` bytes, or None if they did not arrive in time."""
        with self._cond:
            if len(self._buf) < size and timeout:
                self._cond.wait_for(lambda: len(self._buf) >= size, timeout)
            if len(self._buf) < size:
                return None
            out = bytes(self._buf[:size])
            del self._buf[:size]
            return out

    def discard(self, size: int) -> int:
        """Drop up to `size` bytes from the front. Returns how many were dropped."""
        with self._cond:
            count = min(size, len(self._buf))
            del self._buf[:count]
            return count

    def clear(self) -> None:
        with self._cond:
            self._buf.clear()

    def __len__(self) -> int:
        return len(self._buf)
