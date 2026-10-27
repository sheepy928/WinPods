"""Persistent settings stored in %APPDATA%\\WinPods\\config.json."""

from __future__ import annotations

from dataclasses import asdict, dataclass, fields
import json
import logging
import os
from pathlib import Path
from typing import Optional

from . import APP_NAME

_LOGGER = logging.getLogger(__name__)

# (label, milliseconds). "Ultra low" matches what TuneBlade offers.
MIN_LATENCY_MS = 75
MAX_LATENCY_MS = 5000

LATENCY_PRESETS = [
    ("Ultra low (100 ms)", 100),
    ("Low (250 ms)", 250),
    ("Normal (500 ms)", 500),
    ("Safe (1 s)", 1000),
    ("AirPlay default (2 s)", 2000),
]


def app_dir() -> Path:
    base = os.environ.get("APPDATA") or str(Path.home())
    path = Path(base) / APP_NAME
    path.mkdir(parents=True, exist_ok=True)
    return path


def config_path() -> Path:
    return app_dir() / "config.json"


def log_path() -> Path:
    return app_dir() / "winpods.log"


@dataclass
class Config:
    """User settings."""

    device_ip: str = ""  # set from the window ("Find...") or --ip
    device_name: str = ""
    latency_ms: int = 100
    capture_device: str = "default"  # "default" or an exact WASAPI endpoint name
    volume: Optional[int] = None  # receiver volume in percent; None = leave untouched
    auto_connect: bool = True
    start_minimized: bool = False
    timing_protocol: str = "PTP"  # "PTP" (HomePod, current firmware) or "NTP" (legacy)
    codec: str = "alac"  # AirPlay 2 audio format: "alac" (uncompressed ALAC) or "pcm"
    sender_name: str = APP_NAME
    log_level: str = "INFO"

    @classmethod
    def load(cls) -> "Config":
        path = config_path()
        if not path.exists():
            return cls()
        try:
            raw = json.loads(path.read_text(encoding="utf-8"))
        except Exception:  # pylint: disable=broad-except
            _LOGGER.exception("Could not read %s, using defaults", path)
            return cls()
        known = {f.name for f in fields(cls)}
        return cls(**{k: v for k, v in raw.items() if k in known})

    def save(self) -> None:
        if getattr(self, "_read_only", False):
            return  # command-line overrides apply to this run only
        path = config_path()
        tmp = path.with_suffix(".tmp")
        tmp.write_text(json.dumps(asdict(self), indent=2), encoding="utf-8")
        tmp.replace(path)
