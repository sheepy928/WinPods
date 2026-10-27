"""Command line entry points."""

from __future__ import annotations

import argparse
import logging
from logging.handlers import RotatingFileHandler
import sys
import time

from . import APP_NAME, __version__
from .config import LATENCY_PRESETS, Config, log_path


def setup_logging(level: str, to_console: bool = True) -> None:
    root = logging.getLogger()
    root.setLevel(logging.DEBUG)
    fmt = logging.Formatter("%(asctime)s %(levelname)-7s %(name)s: %(message)s")

    file_handler = RotatingFileHandler(log_path(), maxBytes=2_000_000, backupCount=2, encoding="utf-8")
    file_handler.setFormatter(fmt)
    file_handler.setLevel(getattr(logging, level.upper(), logging.INFO))
    root.addHandler(file_handler)

    if to_console:
        console = logging.StreamHandler(sys.stderr)
        console.setFormatter(fmt)
        console.setLevel(getattr(logging, level.upper(), logging.INFO))
        root.addHandler(console)

    # pyatv is chatty at DEBUG; keep its noisiest loggers one notch quieter
    for name in ("pyatv.support.http", "pyatv.core.mdns", "zeroconf"):
        logging.getLogger(name).setLevel(logging.INFO)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="winpods", description=f"{APP_NAME} {__version__} - stream Windows audio to a HomePod"
    )
    parser.add_argument("--ip", help="receiver IP address (default: the one saved by the app)")
    parser.add_argument(
        "--latency", type=int, metavar="MS",
        help="receiver latency in milliseconds, e.g. 100 for ultra low latency",
    )
    parser.add_argument("--device", help='capture device name, or "default" for the Windows default output')
    parser.add_argument("--volume", type=int, metavar="PCT", help="set receiver volume (0-100) on connect")
    parser.add_argument("--headless", action="store_true", help="run without a window (Ctrl+C to stop)")
    parser.add_argument("--list-devices", action="store_true", help="list capture devices and exit")
    parser.add_argument("--scan", action="store_true", help="scan the network for AirPlay receivers and exit")
    parser.add_argument("--duration", type=float, metavar="SEC", help="headless: stop after this many seconds")
    parser.add_argument("--timing", choices=["PTP", "NTP"], help="clock protocol for AirPlay 2 receivers (default PTP)")
    parser.add_argument("--codec", choices=["alac", "pcm"], help="AirPlay 2 audio format (default alac)")
    parser.add_argument(
        "--test-tone", action="store_true",
        help="stream a quiet beep each second instead of Windows audio (plays on the receiver only)",
    )
    parser.add_argument("--log-level", default=None, help="DEBUG, INFO, WARNING (default INFO)")
    parser.add_argument("--version", action="version", version=f"{APP_NAME} {__version__}")
    return parser


def apply_args(config: Config, args: argparse.Namespace) -> None:
    if args.ip:
        config.device_ip = args.ip
    if args.latency:
        config.latency_ms = args.latency
    if args.device:
        config.capture_device = args.device
    if args.volume is not None:
        config.volume = args.volume
    if args.codec:
        config.codec = args.codec
    if args.timing:
        config.timing_protocol = args.timing
    if args.log_level:
        config.log_level = args.log_level


def cmd_list_devices() -> int:
    from .capture import DEFAULT_DEVICE_LABEL, default_loopback_name, list_capture_devices

    print(f'  "default"  -> {DEFAULT_DEVICE_LABEL}: {default_loopback_name()}')
    for dev in list_capture_devices():
        kind = "loopback" if dev.is_loopback else "input   "
        print(f'  {kind}  {dev.sample_rate:>6} Hz  {dev.channels} ch  "{dev.name}"')
    return 0


def cmd_scan() -> int:
    import asyncio

    from .discovery import scan_network

    print("Scanning for AirPlay receivers (5 s)...")
    found = asyncio.run(scan_network())
    if not found:
        print("  none found (multicast may be blocked; try --ip directly)")
    for dev in found:
        print(f"  {dev.address:<16} {dev.name}  [{dev.model}]")
    return 0


def run_headless(config: Config, duration: float | None, test_tone: bool = False) -> int:
    from .engine import Engine, State

    if not config.device_ip:
        print("No speaker address: pass --ip, or run `winpods --scan` to find one.", file=sys.stderr)
        return 2
    engine = Engine(config, test_tone=test_tone)
    engine.start()
    engine.connect()
    source = "a test tone" if test_tone else "Windows audio"
    print(f"Streaming {source} to {config.device_ip} with {config.latency_ms} ms latency. Ctrl+C to stop.")
    started = time.monotonic()
    last_state = None
    try:
        while True:
            time.sleep(1.0)
            snap = engine.snapshot()
            if snap["state"] != last_state:
                last_state = snap["state"]
                detail = f" - {snap['error']}" if snap["error"] else ""
                print(f"[{snap['state'].value}]{detail}")
            if snap["state"] == State.STREAMING and int(time.monotonic() - started) % 5 == 0:
                clock = (
                    f"PTP {'locked' if snap['timing_ok'] else 'LOST'} syncs={snap['ptp_syncs']} "
                    f"spread={snap['ptp_spread_ms']:.2f}ms"
                    if snap["timing"] == "PTP"
                    else f"NTP requests={snap['timing_requests']}"
                )
                print(
                    f"  {snap['uptime']:6.0f}s {snap['codec']} pkts={snap['packets']} "
                    f"silence={snap['silence']} stalls={snap['stalls']} trimmed={snap['trimmed']} "
                    f"retx={snap['retransmits']} anchors={snap['anchors']} | {clock} | "
                    f"ring={snap['ring_ms']:.0f}ms peak={snap['peak']:.2f}"
                )
                if snap["feedback"] and int(snap["uptime"]) % 15 < 5:
                    print(f"  receiver feedback: {snap['feedback']}")
            if duration and time.monotonic() - started >= duration:
                break
    except KeyboardInterrupt:
        pass
    finally:
        print("Stopping...")
        engine.shutdown()
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    config = Config.load()
    apply_args(config, args)
    overrides = ("ip", "latency", "device", "volume", "codec", "timing", "log_level")
    if args.headless and any(getattr(args, name) not in (None, False) for name in overrides):
        config._read_only = True  # pylint: disable=protected-access
    setup_logging(config.log_level, to_console=True)

    if args.list_devices:
        return cmd_list_devices()
    if args.scan:
        return cmd_scan()
    if args.headless:
        return run_headless(config, args.duration, test_tone=args.test_tone)

    from .gui import run_gui

    return run_gui(config)


def main_gui() -> int:
    """Entry point for the windowed build (no console)."""
    config = Config.load()
    setup_logging(config.log_level, to_console=False)
    logging.getLogger(__name__).info("%s %s starting", APP_NAME, __version__)
    try:
        from .gui import run_gui

        return run_gui(config)
    except Exception:
        logging.getLogger(__name__).exception("WinPods crashed")
        raise


if __name__ == "__main__":
    sys.exit(main())
