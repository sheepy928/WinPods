"""Background engine: owns the asyncio loop, capture and streaming session.

The GUI and CLI talk to the engine from other threads through the small thread-safe
API (connect/disconnect/set_*), and read `snapshot()` for status.
"""

from __future__ import annotations

import asyncio
from enum import Enum
import logging
import threading
from time import monotonic
from typing import Optional

from .capture import DEFAULT_DEVICE, AudioCapture, default_loopback_name
from .config import MAX_LATENCY_MS, MIN_LATENCY_MS, Config
from .discovery import FoundDevice, find_device
from .raop_live import LiveStreamSession
from .ringbuffer import ByteRing
from .tone import ToneSource

_LOGGER = logging.getLogger(__name__)

RING_SECONDS = 3
BACKOFF_STEPS = [2, 3, 5, 10, 20, 30]
DEFAULT_DEVICE_POLL = 5.0


class State(Enum):
    DISCONNECTED = "Disconnected"
    CONNECTING = "Connecting"
    STREAMING = "Streaming"
    RECONNECTING = "Reconnecting"
    ERROR = "Error"


class Engine:
    """Supervises capture + stream according to the desired state."""

    def __init__(self, config: Config, test_tone: bool = False) -> None:
        self.config = config
        self.test_tone = test_tone  # stream a generated beep instead of Windows audio
        self.ring = ByteRing(capacity=44100 * 4 * RING_SECONDS)
        self.loop = asyncio.new_event_loop()
        self._thread = threading.Thread(target=self._run, name="winpods-engine", daemon=True)
        self._wake = asyncio.Event()
        self._shutdown = False

        self.capture = None  # AudioCapture or ToneSource
        self.session: Optional[LiveStreamSession] = None
        self.device: Optional[FoundDevice] = None

        self.state = State.DISCONNECTED
        self.last_error: str = ""
        self._want_connected = False
        self._restart_stream = False
        self._restart_capture = False
        self._failures = 0
        self._retry_at = 0.0
        self._last_default_check = 0.0

    # ------------------------------------------------------------------ threading
    def start(self) -> None:
        self._thread.start()

    def _run(self) -> None:
        asyncio.set_event_loop(self.loop)
        self.loop.create_task(self._supervise())
        self.loop.run_forever()

    def _poke(self) -> None:
        if self.loop.is_running():
            self.loop.call_soon_threadsafe(self._wake.set)

    def shutdown(self) -> None:
        """Stop everything and end the engine thread (blocking, from any thread)."""
        self._want_connected = False
        self._shutdown = True
        self._poke()
        if self._thread.is_alive():
            self._thread.join(timeout=8.0)

    # --------------------------------------------------------------- public API
    def connect(self) -> None:
        self._want_connected = True
        self._failures = 0
        self._retry_at = 0.0
        self._poke()

    def disconnect(self) -> None:
        self._want_connected = False
        self._poke()

    @property
    def wants_connection(self) -> bool:
        return self._want_connected

    def set_latency(self, latency_ms: int) -> None:
        latency_ms = int(max(MIN_LATENCY_MS, min(MAX_LATENCY_MS, latency_ms)))
        if latency_ms == self.config.latency_ms:
            return
        self.config.latency_ms = latency_ms
        self.config.save()
        self._restart_stream = True
        self._poke()

    def set_capture_device(self, device: str) -> None:
        if device == self.config.capture_device:
            return
        self.config.capture_device = device
        self.config.save()
        self._restart_capture = True
        self._poke()

    def set_device_ip(self, address: str) -> None:
        address = address.strip()
        if address == self.config.device_ip:
            return
        self.config.device_ip = address
        self.config.device_name = ""
        self.config.save()
        self.device = None
        self._restart_stream = True
        self._poke()

    def set_timing_protocol(self, protocol: str) -> None:
        protocol = protocol.upper()
        if protocol not in ("PTP", "NTP") or protocol == self.config.timing_protocol:
            return
        self.config.timing_protocol = protocol
        self.config.save()
        self._restart_stream = True
        self._poke()

    def set_volume(self, pct: Optional[int]) -> None:
        self.config.volume = pct
        self.config.save()
        session = self.session
        if session is not None and pct is not None and self.loop.is_running():
            asyncio.run_coroutine_threadsafe(session.set_volume_pct(pct), self.loop)

    def snapshot(self) -> dict:
        """Thread-safe-enough status view for UIs."""
        session = self.session
        capture = self.capture
        stats = session.stats if session else None
        ptp = session.ptp if session else None
        uptime = monotonic() - stats.started_at if stats and stats.started_at else 0.0
        if ptp is not None:
            timing_ok = ptp.locked
        elif session is not None and session.is_airplay2:
            timing_ok = True  # NTP-timed AirPlay 2 receivers rarely ask for timing
        else:
            timing_ok = bool(stats and stats.last_timing and monotonic() - stats.last_timing < 15)
        return {
            "state": self.state,
            "error": self.last_error,
            "device_name": self.device.name if self.device else self.config.device_name,
            "device_ip": self.config.device_ip,
            "device_model": self.device.model if self.device else "",
            "latency_ms": self.config.latency_ms,
            "airplay2": bool(session and session.is_airplay2),
            "capture_name": capture.device_name if capture else "",
            "capture_rate": capture.device_rate if capture else 0,
            "peak": capture.peak if capture else 0.0,
            "uptime": uptime,
            "packets": stats.packets_sent if stats else 0,
            "silence": stats.silence_packets if stats else 0,
            "trimmed": stats.frames_trimmed if stats else 0,
            "stalls": stats.stalls if stats else 0,
            "retransmits": stats.retransmitted if stats else 0,
            "timing_requests": stats.timing_requests if stats else 0,
            "timing_ok": timing_ok,
            "timing": "PTP" if ptp else ("NTP" if session else ""),
            "codec": session.codec.upper() if session else "",
            "ptp_syncs": ptp.syncs if ptp else 0,
            "ptp_offset_ms": (ptp.offset_ns or 0) / 1e6 if ptp else 0.0,
            "ptp_spread_ms": ptp.last_delay_spread_ns / 1e6 if ptp else 0.0,
            "anchors": stats.anchors_sent if stats else 0,
            "render_ms": stats.render_ms if stats else 0,
            "feedback": session.feedback if session else {},
            "ring_ms": len(self.ring) / 4 / 44.1,
            "ring_dropped": self.ring.dropped_bytes // 4,
            "retry_in": max(0.0, self._retry_at - monotonic()) if self.state == State.RECONNECTING else 0.0,
        }

    # ------------------------------------------------------------------ supervisor
    async def _supervise(self) -> None:
        while not self._shutdown:
            try:
                await self._step()
            except Exception as ex:  # pylint: disable=broad-except
                _LOGGER.exception("Engine step failed")
                self.last_error = str(ex)
                self.state = State.ERROR
            try:
                await asyncio.wait_for(self._wake.wait(), timeout=1.0)
            except asyncio.TimeoutError:
                pass
            self._wake.clear()
        await self._stop_session()
        await self._stop_capture()
        # Cancel whatever is still in flight (e.g. a discovery that was interrupted)
        # so the loop can stop without "Task was destroyed but it is pending" noise.
        pending = [t for t in asyncio.all_tasks(self.loop) if t is not asyncio.current_task()]
        for task in pending:
            task.cancel()
        if pending:
            await asyncio.gather(*pending, return_exceptions=True)
        self.loop.stop()

    async def _step(self) -> None:
        if not self._want_connected:
            if self.session or self.capture:
                await self._stop_session()
                await self._stop_capture()
            if self.state != State.ERROR:
                self.state = State.DISCONNECTED
            return

        if not self.config.device_ip:
            self.last_error = "No speaker address set: enter the HomePod's IP or use Find..."
            self.state = State.ERROR
            self._want_connected = False
            return

        if self._restart_capture:
            self._restart_capture = False
            await self._stop_capture()
        await self._follow_default_device()
        if self.capture is None:
            await self._start_capture()

        if self.session is not None:
            if self._restart_stream:
                self._restart_stream = False
                _LOGGER.info("Restarting stream (settings changed)")
                await self._stop_session()
            elif not self.session.alive:
                self.last_error = self.session.end_reason or "stream ended"
                _LOGGER.warning("Stream died: %s", self.last_error)
                await self._stop_session()
                self._schedule_retry()
            else:
                self.state = State.STREAMING
                return

        if self.session is None:
            if monotonic() < self._retry_at:
                self.state = State.RECONNECTING
                return
            self._restart_stream = False
            await self._start_session()

    def _schedule_retry(self) -> None:
        delay = BACKOFF_STEPS[min(self._failures, len(BACKOFF_STEPS) - 1)]
        self._failures += 1
        self._retry_at = monotonic() + delay
        self.state = State.RECONNECTING
        _LOGGER.info("Retrying in %d s", delay)

    # ------------------------------------------------------------------ capture
    async def _start_capture(self) -> None:
        if self.test_tone:
            capture = ToneSource(self.ring)
        else:
            capture = AudioCapture(self.ring, self.config.capture_device)
        try:
            await asyncio.to_thread(capture.start)
        except Exception as ex:
            self.last_error = f"audio capture: {ex}"
            self.state = State.ERROR
            self._schedule_retry()
            raise
        self.capture = capture
        self.ring.clear()
        self._last_default_check = monotonic()

    async def _stop_capture(self) -> None:
        if self.capture is not None:
            capture, self.capture = self.capture, None
            await asyncio.to_thread(capture.stop)

    async def _follow_default_device(self) -> None:
        """When following the Windows default output, switch if the default changes."""
        if self.capture is None or self.test_tone or self.config.capture_device != DEFAULT_DEVICE:
            return
        if monotonic() - self._last_default_check < DEFAULT_DEVICE_POLL:
            return
        self._last_default_check = monotonic()
        name = await asyncio.to_thread(default_loopback_name)
        if name and name != self.capture.device_name:
            _LOGGER.info("Default output changed to %r; switching capture", name)
            await self._stop_capture()

    # ------------------------------------------------------------------ session
    async def _start_session(self) -> None:
        self.state = State.CONNECTING
        try:
            if self.device is None or self.device.address != self.config.device_ip:
                found = await find_device(self.config.device_ip)
                if found is None:
                    raise RuntimeError(f"No AirPlay receiver answered at {self.config.device_ip}")
                self.device = found
                if self.config.device_name != found.name:
                    self.config.device_name = found.name
                    self.config.save()
                _LOGGER.info("Found %s (%s) at %s", found.name, found.model, found.address)

            session = LiveStreamSession(
                self.device.config,
                self.ring,
                latency_ms=self.config.latency_ms,
                sender_name=self.config.sender_name,
                volume_pct=self.config.volume,
                timing_protocol=self.config.timing_protocol,
                codec=self.config.codec,
            )
            await session.start()
        except Exception as ex:  # pylint: disable=broad-except
            self.last_error = str(ex) or ex.__class__.__name__
            _LOGGER.error("Connect failed: %s", self.last_error)
            self.device = None
            self._schedule_retry()
            return
        self.session = session
        self.state = State.STREAMING
        self.last_error = ""
        self._failures = 0

    async def _stop_session(self) -> None:
        if self.session is not None:
            session, self.session = self.session, None
            try:
                await session.stop()
            except Exception:  # pylint: disable=broad-except
                _LOGGER.debug("Error stopping session", exc_info=True)
