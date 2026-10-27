"""Finding AirPlay receivers with pyatv."""

from __future__ import annotations

import asyncio
from dataclasses import dataclass
from typing import List, Optional

import pyatv
from pyatv.const import Protocol
from pyatv.interface import BaseConfig

PROTOCOLS = {Protocol.RAOP, Protocol.AirPlay}


@dataclass
class FoundDevice:
    name: str
    address: str
    model: str
    config: BaseConfig


def _describe(conf: BaseConfig) -> FoundDevice:
    model = conf.device_info.raw_model or str(conf.device_info.model)
    return FoundDevice(conf.name, str(conf.address), model, conf)


async def find_device(address: str, timeout: int = 5) -> Optional[FoundDevice]:
    """Look up a single receiver by IP address (unicast mDNS, no multicast needed)."""
    loop = asyncio.get_running_loop()
    results = await pyatv.scan(loop, hosts=[address], protocol=PROTOCOLS, timeout=timeout)
    for conf in results:
        if conf.get_service(Protocol.RAOP) is not None:
            return _describe(conf)
    return None


async def scan_network(timeout: int = 5) -> List[FoundDevice]:
    """Discover every AirPlay audio receiver on the local network."""
    loop = asyncio.get_running_loop()
    results = await pyatv.scan(loop, protocol=PROTOCOLS, timeout=timeout)
    found = [_describe(conf) for conf in results if conf.get_service(Protocol.RAOP)]
    found.sort(key=lambda dev: dev.name.lower())
    return found
