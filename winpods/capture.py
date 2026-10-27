"""WASAPI audio capture producing 44.1 kHz / 16-bit / stereo big-endian PCM.

Captures either the loopback of a render device (what Windows is playing on it) or a
regular recording device such as the "CABLE Output" side of a virtual audio cable.
Everything is converted to the RAOP wire format (L16 stereo 44100) on the fly.
"""

from __future__ import annotations

from dataclasses import dataclass
import logging
from typing import List, Optional

import numpy as np
import pyaudiowpatch as pyaudio

try:
    import soxr
except ImportError:  # pragma: no cover - optional at runtime
    soxr = None

from .ringbuffer import ByteRing

_LOGGER = logging.getLogger(__name__)

TARGET_RATE = 44100
TARGET_CHANNELS = 2
BYTES_PER_FRAME = TARGET_CHANNELS * 2

DEFAULT_DEVICE = "default"  # follow the Windows default output device (loopback)
DEFAULT_DEVICE_LABEL = "Windows default output (loopback)"


@dataclass
class CaptureDevice:
    """A WASAPI endpoint that can be captured from."""

    index: int
    name: str
    is_loopback: bool
    sample_rate: int
    channels: int

    @property
    def label(self) -> str:
        return self.name


def list_capture_devices() -> List[CaptureDevice]:
    """Return all WASAPI endpoints with input channels (loopbacks and microphones)."""
    pa = pyaudio.PyAudio()
    try:
        api = pa.get_host_api_info_by_type(pyaudio.paWASAPI)
        devices = []
        for index in range(pa.get_device_count()):
            info = pa.get_device_info_by_index(index)
            if info["hostApi"] != api["index"] or info["maxInputChannels"] < 1:
                continue
            devices.append(
                CaptureDevice(
                    index=index,
                    name=info["name"],
                    is_loopback=bool(info.get("isLoopbackDevice")),
                    sample_rate=int(info["defaultSampleRate"]),
                    channels=int(info["maxInputChannels"]),
                )
            )
        # Loopbacks first (that is what people usually want), then microphones
        devices.sort(key=lambda dev: (not dev.is_loopback, dev.name.lower()))
        return devices
    finally:
        pa.terminate()


def windows_default_render_name() -> Optional[str]:
    """Friendly name of the Windows default playback device (multimedia role).

    PortAudio's idea of the default output can be stale or use a different role, so
    ask the Core Audio API directly through pycaw when it is available.
    """
    try:
        import warnings

        import comtypes  # type: ignore
        from pycaw.pycaw import AudioUtilities  # type: ignore
    except ImportError:
        return None
    try:
        comtypes.CoInitialize()
    except Exception:  # pylint: disable=broad-except
        pass
    try:
        with warnings.catch_warnings():
            warnings.simplefilter("ignore")
            device = AudioUtilities.GetSpeakers()
            return device.FriendlyName if device is not None else None
    except Exception:  # pylint: disable=broad-except
        _LOGGER.debug("pycaw default device lookup failed", exc_info=True)
        return None


def _find_loopback_for(pa: pyaudio.PyAudio, render_name: str) -> Optional[dict]:
    api = pa.get_host_api_info_by_type(pyaudio.paWASAPI)
    wanted = f"{render_name} [Loopback]"
    for index in range(pa.get_device_count()):
        info = pa.get_device_info_by_index(index)
        if info["hostApi"] == api["index"] and info.get("isLoopbackDevice") and info["name"] == wanted:
            return info
    return None


def resolve_default_loopback(pa: pyaudio.PyAudio) -> dict:
    """Loopback endpoint for whatever Windows currently uses as default output."""
    render_name = windows_default_render_name()
    if render_name:
        info = _find_loopback_for(pa, render_name)
        if info is not None:
            return info
        _LOGGER.debug("No loopback endpoint named for %r; using PortAudio default", render_name)
    return pa.get_default_wasapi_loopback()


def default_loopback_name() -> Optional[str]:
    """Name of the loopback endpoint of the current Windows default output."""
    pa = pyaudio.PyAudio()
    try:
        return resolve_default_loopback(pa)["name"]
    except Exception:  # pylint: disable=broad-except
        return None
    finally:
        pa.terminate()


class AudioCapture:
    """Pulls audio from a WASAPI endpoint into a ByteRing in RAOP wire format."""

    def __init__(
        self, ring: ByteRing, device: str = DEFAULT_DEVICE, frames_per_buffer: int = 0
    ) -> None:
        self.ring = ring
        self.device = device
        self.frames_per_buffer = frames_per_buffer

        self._pa: Optional[pyaudio.PyAudio] = None
        self._stream = None
        self._resampler = None

        self.device_name: str = ""
        self.device_index: Optional[int] = None
        self.device_rate: int = 0
        self.device_channels: int = 0

        self.peak: float = 0.0  # 0.0 .. 1.0, peak of the most recent chunk
        self.callbacks: int = 0
        self.frames_in: int = 0  # frames written to the ring (after resampling)
        self.error: Optional[str] = None

    # ------------------------------------------------------------------ control
    def start(self) -> None:
        self._pa = pyaudio.PyAudio()
        info = self._resolve_device()
        self.device_index = int(info["index"])
        self.device_name = str(info["name"])
        self.device_rate = int(info["defaultSampleRate"])
        self.device_channels = int(info["maxInputChannels"])

        if self.device_rate != TARGET_RATE:
            if soxr is None:
                raise RuntimeError(
                    f"{self.device_name} runs at {self.device_rate} Hz; install "
                    "python-soxr to resample to 44100 Hz"
                )
            self._resampler = soxr.ResampleStream(
                self.device_rate, TARGET_RATE, TARGET_CHANNELS, dtype="int16", quality="HQ"
            )
        else:
            self._resampler = None

        frames = self.frames_per_buffer or max(64, int(self.device_rate * 0.005))
        self._stream = self._pa.open(
            format=pyaudio.paInt16,
            channels=self.device_channels,
            rate=self.device_rate,
            input=True,
            input_device_index=self.device_index,
            frames_per_buffer=frames,
            stream_callback=self._callback,
        )
        self._stream.start_stream()
        _LOGGER.info(
            "Capturing from %r (%d Hz, %d ch, %d frames/buffer)",
            self.device_name,
            self.device_rate,
            self.device_channels,
            frames,
        )

    def stop(self) -> None:
        if self._stream is not None:
            try:
                self._stream.stop_stream()
                self._stream.close()
            except Exception:  # pylint: disable=broad-except
                _LOGGER.debug("Error closing capture stream", exc_info=True)
            self._stream = None
        if self._pa is not None:
            self._pa.terminate()
            self._pa = None

    @property
    def running(self) -> bool:
        return self._stream is not None and self._stream.is_active()

    # ----------------------------------------------------------------- helpers
    def _resolve_device(self) -> dict:
        assert self._pa is not None
        if self.device == DEFAULT_DEVICE:
            return resolve_default_loopback(self._pa)

        api = self._pa.get_host_api_info_by_type(pyaudio.paWASAPI)
        for index in range(self._pa.get_device_count()):
            info = self._pa.get_device_info_by_index(index)
            if (
                info["hostApi"] == api["index"]
                and info["maxInputChannels"] > 0
                and info["name"] == self.device
            ):
                return info
        raise RuntimeError(f"Capture device not found: {self.device!r}")

    def _callback(self, in_data, frame_count, time_info, status):  # noqa: ARG002
        self.callbacks += 1
        try:
            self._process(in_data)
        except Exception as ex:  # pylint: disable=broad-except
            self.error = str(ex)
            _LOGGER.exception("Capture callback failed")
        return (None, pyaudio.paContinue)

    def _process(self, in_data: bytes) -> None:
        samples = np.frombuffer(in_data, dtype=np.int16)
        channels = self.device_channels
        if channels != TARGET_CHANNELS:
            frames = samples.reshape(-1, channels)
            if channels == 1:
                stereo = np.repeat(frames, 2, axis=1)
            else:
                stereo = frames[:, :2]
            samples = np.ascontiguousarray(stereo).reshape(-1)

        if self._resampler is not None:
            samples = self._resampler.resample_chunk(samples.reshape(-1, TARGET_CHANNELS))
            samples = samples.reshape(-1)

        if samples.size == 0:
            return

        self.peak = int(np.max(np.abs(samples.astype(np.int32)))) / 32768.0
        self.frames_in += samples.size // TARGET_CHANNELS
        # RTP L16 is big-endian; astype('>i2') byte-swaps in one vectorised pass.
        self.ring.write(samples.astype(">i2").tobytes())
