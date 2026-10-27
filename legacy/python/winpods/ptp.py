"""Passive IEEE 1588 (PTPv2) follower for AirPlay 2 receivers.

Current HomePod firmware times realtime audio with PTP.  The receiver (or the leader of
its speaker group) is the clock master and sends Sync / Follow_Up messages to every
timing peer named in the SETUP request.  WinPods only *listens*: it measures the offset
between the receiver's clock and this PC's monotonic clock so that the timing anchors we
send can be written in the receiver's timescale.  Nothing is ever sent from these
sockets - the same approach shairport-sync's nqptp takes.

Offset estimation: for each Sync we know the master's send time T1 (from Follow_Up) and
our receive time t2.  T1 - t2 equals the clock offset minus the one-way network delay,
so the largest value over a short window (the least-delayed packet) is the best
estimate.  A small fixed allowance is added back for the minimum delay.
"""

from __future__ import annotations

from collections import deque
import logging
import selectors
import socket
import struct
import threading
from time import monotonic, perf_counter_ns
from typing import Deque, Dict, Optional, Tuple

_LOGGER = logging.getLogger(__name__)

PTP_EVENT_PORT = 319
PTP_GENERAL_PORT = 320

MSG_SYNC = 0x0
MSG_FOLLOW_UP = 0x8
MSG_ANNOUNCE = 0xB

FLAG_TWO_STEP = 0x0200
HEADER_LEN = 34

WINDOW_NS = 4_000_000_000  # offset estimate = best sample of the last 4 s
STEP_NS = 5_000_000  # jumps larger than this are applied at once, smaller ones slewed
SLEW = 0.2
ASSUMED_PATH_DELAY_NS = 1_000_000  # typical minimum one-way delay to a Wi-Fi speaker
STALE_AFTER = 3.0  # seconds without samples before the clock counts as lost
FALLBACK_AFTER = 2.0  # accept any master if the expected one is silent this long
MAX_PENDING = 64


def _timestamp_ns(raw: bytes) -> int:
    return int.from_bytes(raw[0:6], "big") * 1_000_000_000 + int.from_bytes(raw[6:10], "big")


class PtpFollower(threading.Thread):
    """Listens to a receiver's PTP master and tracks its clock offset."""

    def __init__(self, local_ip: str) -> None:
        super().__init__(name="winpods-ptp", daemon=True)
        self.local_ip = local_ip
        self._halt = threading.Event()
        self._lock = threading.Lock()
        self._locked = threading.Event()

        self.event_sock, self.general_sock = self._bind()
        self.port = self.event_sock.getsockname()[1]

        self._pending: Dict[Tuple[bytes, int], Tuple[int, int]] = {}
        self._samples: Deque[Tuple[int, int]] = deque()
        self._offset: Optional[float] = None

        self.master_ip: Optional[str] = None
        self.expected_clock: Optional[int] = None
        self._master_since: float = 0.0
        self.master_clock_id: int = 0  # clock identity the anchors refer to
        self.master_addr: str = ""

        # statistics
        self.syncs = 0
        self.follow_ups = 0
        self.announces = 0
        self.ignored = 0
        self.last_sample = 0.0
        self.last_delay_spread_ns = 0

    # ----------------------------------------------------------------- sockets
    @property
    def standard_ports(self) -> bool:
        return self.port == PTP_EVENT_PORT

    def _bind(self) -> Tuple[socket.socket, socket.socket]:
        try:
            event = self._socket(PTP_EVENT_PORT)
            try:
                general = self._socket(PTP_GENERAL_PORT)
            except OSError:
                event.close()
                raise
            return event, general
        except OSError as ex:
            _LOGGER.warning(
                "UDP ports 319/320 are in use (%s), probably by another WinPods instance "
                "or a PTP program; using a dynamic port, which some receivers reject",
                ex,
            )
        sock = self._socket(0)
        return sock, sock

    def _socket(self, port: int) -> socket.socket:
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.bind((self.local_ip, port))
        sock.setblocking(False)
        return sock

    # ------------------------------------------------------------------ control
    def follow(self, ip: str, clock_id: Optional[int]) -> None:
        """Start tracking the master clock of the receiver at `ip`."""
        with self._lock:
            self.master_ip = ip
            self.expected_clock = clock_id or None
            self._master_since = monotonic()
        _LOGGER.info(
            "PTP: following %s (expected clock %s, listening on port %d)",
            ip,
            f"{clock_id:016x}" if clock_id else "any",
            self.port,
        )

    def wait_locked(self, timeout: float) -> bool:
        return self._locked.wait(timeout)

    @property
    def locked(self) -> bool:
        return self._offset is not None and monotonic() - self.last_sample < STALE_AFTER

    @property
    def offset_ns(self) -> Optional[float]:
        return self._offset

    def to_master_ns(self, local_ns: int) -> Optional[int]:
        """Convert a perf_counter_ns() value to the master's timescale."""
        offset = self._offset
        if offset is None:
            return None
        return int(local_ns + offset) + ASSUMED_PATH_DELAY_NS

    def stop(self) -> None:
        self._halt.set()

    def close(self) -> None:
        self.stop()
        if self.is_alive():
            self.join(timeout=2.0)
        self.event_sock.close()
        if self.general_sock is not self.event_sock:
            self.general_sock.close()

    # --------------------------------------------------------------- messages
    def _accept(self, clock: int, ip: str) -> bool:
        if self.master_clock_id:
            return clock == self.master_clock_id
        if self.master_ip is None:
            return False
        chosen = False
        if self.expected_clock is not None:
            if clock == self.expected_clock:
                chosen = True
            elif monotonic() - self._master_since > FALLBACK_AFTER:
                _LOGGER.warning(
                    "PTP: expected clock %016x is silent; following %016x from %s instead",
                    self.expected_clock,
                    clock,
                    ip,
                )
                chosen = True
        elif ip == self.master_ip or monotonic() - self._master_since > FALLBACK_AFTER:
            chosen = True
        if chosen:
            self.master_clock_id = clock
            self.master_addr = ip
            _LOGGER.info("PTP: locked onto clock %016x at %s", clock, ip)
        return chosen

    def _on_datagram(self, data: bytes, addr: Tuple[str, int], t2: int) -> None:
        if len(data) < HEADER_LEN + 10 or (data[1] & 0x0F) != 2:
            return
        msg = data[0] & 0x0F
        source = data[20:30]
        clock = int.from_bytes(data[20:28], "big")
        seq = struct.unpack_from(">H", data, 30)[0]
        correction = struct.unpack_from(">q", data, 8)[0] >> 16  # ns

        if msg == MSG_SYNC:
            self.syncs += 1
            if not self._accept(clock, addr[0]):
                self.ignored += 1
                return
            flags = struct.unpack_from(">H", data, 6)[0]
            if flags & FLAG_TWO_STEP:
                self._pending[(source, seq)] = (t2, correction)
                if len(self._pending) > MAX_PENDING:
                    self._pending.pop(next(iter(self._pending)))
            else:
                t1 = _timestamp_ns(data[34:44])
                self._add_sample(t2, t1 + correction - t2)
        elif msg == MSG_FOLLOW_UP:
            self.follow_ups += 1
            entry = self._pending.pop((source, seq), None)
            if entry is None:
                return
            t2_sync, sync_correction = entry
            t1 = _timestamp_ns(data[34:44])
            self._add_sample(t2_sync, t1 + sync_correction + correction - t2_sync)
        elif msg == MSG_ANNOUNCE:
            self.announces += 1

    def _add_sample(self, t2: int, raw_offset: int) -> None:
        samples = self._samples
        samples.append((t2, raw_offset))
        while samples and t2 - samples[0][0] > WINDOW_NS:
            samples.popleft()
        values = [value for _, value in samples]
        best = max(values)
        self.last_delay_spread_ns = best - min(values)
        current = self._offset
        if current is None or abs(best - current) > STEP_NS:
            if current is not None:
                _LOGGER.info("PTP: clock offset stepped by %.2f ms", (best - current) / 1e6)
            self._offset = float(best)
        else:
            self._offset = current + SLEW * (best - current)
        self.last_sample = monotonic()
        self._locked.set()

    # -------------------------------------------------------------------- loop
    def run(self) -> None:
        selector = selectors.DefaultSelector()
        selector.register(self.event_sock, selectors.EVENT_READ)
        if self.general_sock is not self.event_sock:
            selector.register(self.general_sock, selectors.EVENT_READ)
        try:
            while not self._halt.is_set():
                for key, _ in selector.select(0.5):
                    sock = key.fileobj
                    while True:
                        try:
                            data, addr = sock.recvfrom(2048)
                        except (BlockingIOError, InterruptedError):
                            break
                        except OSError as ex:
                            _LOGGER.debug("PTP receive error: %s", ex)
                            break
                        t2 = perf_counter_ns()
                        try:
                            self._on_datagram(data, addr, t2)
                        except Exception:  # pylint: disable=broad-except
                            _LOGGER.exception("PTP message handling failed")
        finally:
            selector.close()
