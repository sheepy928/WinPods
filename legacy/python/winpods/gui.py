"""Settings window and tray icon (tkinter + pystray)."""

from __future__ import annotations

import asyncio
import logging
import os
import subprocess
import tkinter as tk
from tkinter import ttk
from typing import List, Optional

from . import APP_NAME, __version__
from .capture import DEFAULT_DEVICE, DEFAULT_DEVICE_LABEL, list_capture_devices
from .config import LATENCY_PRESETS, MAX_LATENCY_MS, MIN_LATENCY_MS, Config, app_dir
from .discovery import FoundDevice, scan_network
from .engine import Engine, State

_LOGGER = logging.getLogger(__name__)

try:
    import pystray
    from PIL import Image, ImageDraw
except ImportError:  # pragma: no cover - tray is optional
    pystray = None

STATE_COLORS = {
    State.DISCONNECTED: "#8a8a8a",
    State.CONNECTING: "#e0a000",
    State.RECONNECTING: "#e0a000",
    State.STREAMING: "#2eaa4a",
    State.ERROR: "#d04040",
}
CUSTOM_LABEL = "Custom..."
REFRESH_MS = 250


def _enable_dpi_awareness() -> None:
    """Render crisply on scaled (125%/150%/200%) displays instead of bitmap-stretched."""
    try:
        import ctypes

        ctypes.windll.shcore.SetProcessDpiAwareness(1)  # per-monitor aware (system DPI)
    except Exception:  # pylint: disable=broad-except
        try:
            ctypes.windll.user32.SetProcessDPIAware()
        except Exception:  # pylint: disable=broad-except
            pass


def _tray_image(color: str) -> "Image.Image":
    size = 64
    image = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    draw = ImageDraw.Draw(image)
    draw.ellipse((4, 4, size - 4, size - 4), fill=color)
    # a small "speaker" glyph
    draw.rectangle((18, 26, 26, 38), fill="white")
    draw.polygon([(26, 26), (38, 17), (38, 47), (26, 38)], fill="white")
    draw.arc((34, 22, 50, 42), start=-50, end=50, fill="white", width=3)
    return image


class App:
    """The WinPods desktop app."""

    def __init__(self, config: Config) -> None:
        self.config = config
        self.engine = Engine(config)
        self.engine.start()

        _enable_dpi_awareness()
        self.root = tk.Tk()
        self.root.title(f"{APP_NAME}")
        self.root.resizable(False, False)
        self._icon = None
        if pystray is not None:
            try:
                from PIL import ImageTk

                self._icon = ImageTk.PhotoImage(_tray_image(STATE_COLORS[State.STREAMING]))
                self.root.iconphoto(True, self._icon)
            except Exception:  # pylint: disable=broad-except
                pass

        self._devices: List[str] = []
        self._last_state: Optional[State] = None
        self._last_volume_sent: Optional[int] = None
        self._scan_future = None
        self._hinted_tray = False
        self.tray = None

        self._build()
        self._start_tray()
        self.root.protocol("WM_DELETE_WINDOW", self._on_close)
        self.root.after(REFRESH_MS, self._refresh)

        if config.auto_connect:
            self.engine.connect()
        if config.start_minimized and self.tray is not None:
            self.root.after(100, self.root.withdraw)

    # --------------------------------------------------------------------- UI
    def _build(self) -> None:
        pad = {"padx": 8, "pady": 4}
        main = ttk.Frame(self.root, padding=10)
        main.grid(sticky="nsew")

        # Receiver -----------------------------------------------------------
        recv = ttk.LabelFrame(main, text="Receiver", padding=8)
        recv.grid(row=0, column=0, sticky="ew", **pad)
        ttk.Label(recv, text="HomePod IP:").grid(row=0, column=0, sticky="w")
        self.ip_var = tk.StringVar(value=self.config.device_ip)
        ip_entry = ttk.Entry(recv, textvariable=self.ip_var, width=18)
        ip_entry.grid(row=0, column=1, sticky="w", padx=(6, 6))
        ip_entry.bind("<Return>", lambda _e: self._apply_ip())
        ip_entry.bind("<FocusOut>", lambda _e: self._apply_ip())
        self.scan_button = ttk.Button(recv, text="Find...", command=self._scan)
        self.scan_button.grid(row=0, column=2, sticky="w")
        self.device_label = ttk.Label(recv, text="", foreground="#555")
        self.device_label.grid(row=1, column=0, columnspan=3, sticky="w", pady=(4, 0))

        # Source -------------------------------------------------------------
        src = ttk.LabelFrame(main, text="Audio source (Windows)", padding=8)
        src.grid(row=1, column=0, sticky="ew", **pad)
        self.device_var = tk.StringVar()
        self.device_combo = ttk.Combobox(src, textvariable=self.device_var, state="readonly", width=44)
        self.device_combo.grid(row=0, column=0, sticky="w")
        self.device_combo.bind("<<ComboboxSelected>>", lambda _e: self._apply_device())
        ttk.Button(src, text="Refresh", command=self._load_devices).grid(row=0, column=1, padx=(6, 0))
        self.capture_label = ttk.Label(src, text="", foreground="#555")
        self.capture_label.grid(row=1, column=0, columnspan=2, sticky="w", pady=(4, 0))
        self.meter = ttk.Progressbar(src, orient="horizontal", length=360, mode="determinate", maximum=100)
        self.meter.grid(row=2, column=0, columnspan=2, sticky="ew", pady=(6, 0))
        self._load_devices()

        # Latency ------------------------------------------------------------
        lat = ttk.LabelFrame(main, text="Latency", padding=8)
        lat.grid(row=2, column=0, sticky="ew", **pad)
        self.latency_var = tk.StringVar()
        labels = [label for label, _ in LATENCY_PRESETS] + [CUSTOM_LABEL]
        self.latency_combo = ttk.Combobox(lat, textvariable=self.latency_var, values=labels, state="readonly", width=24)
        self.latency_combo.grid(row=0, column=0, sticky="w")
        self.latency_combo.bind("<<ComboboxSelected>>", lambda _e: self._apply_latency_preset())
        self.custom_var = tk.IntVar(value=self.config.latency_ms)
        self.custom_spin = ttk.Spinbox(
            lat, from_=MIN_LATENCY_MS, to=MAX_LATENCY_MS, increment=5, textvariable=self.custom_var, width=7
        )
        self.custom_spin.grid(row=0, column=1, padx=(8, 2))
        ttk.Label(lat, text="ms").grid(row=0, column=2)
        ttk.Button(lat, text="Apply", command=self._apply_custom_latency).grid(row=0, column=3, padx=(6, 0))
        ttk.Label(
            lat,
            text="The HomePod itself needs ~70 ms, so 100 ms leaves ~30 ms for Wi-Fi.\n"
            "Raise it if you hear dropouts; 80-90 ms can work on a quiet network.",
            foreground="#555",
        ).grid(row=1, column=0, columnspan=4, sticky="w", pady=(4, 0))
        self._sync_latency_widgets()

        # Volume -------------------------------------------------------------
        vol = ttk.LabelFrame(main, text="HomePod volume", padding=8)
        vol.grid(row=3, column=0, sticky="ew", **pad)
        self.volume_enabled = tk.BooleanVar(value=self.config.volume is not None)
        ttk.Checkbutton(vol, text="Control from here", variable=self.volume_enabled, command=self._apply_volume_toggle).grid(row=0, column=0, sticky="w")
        self.volume_var = tk.IntVar(value=self.config.volume if self.config.volume is not None else 40)
        self.volume_scale = ttk.Scale(vol, from_=0, to=100, orient="horizontal", length=260, variable=self.volume_var, command=lambda _v: self._update_volume_label())
        self.volume_scale.grid(row=0, column=1, padx=(10, 6))
        self.volume_scale.bind("<ButtonRelease-1>", lambda _e: self._apply_volume())
        self.volume_label = ttk.Label(vol, text="", width=5)
        self.volume_label.grid(row=0, column=2)
        self._update_volume_label()
        self._apply_volume_toggle(save=False)

        # Status + connect ---------------------------------------------------
        status = ttk.Frame(main, padding=(0, 4))
        status.grid(row=4, column=0, sticky="ew", **pad)
        self.connect_button = ttk.Button(status, text="Connect", width=16, command=self._toggle_connect)
        self.connect_button.grid(row=0, column=0, rowspan=2, sticky="w")
        self.state_label = ttk.Label(status, text="Disconnected", font=("Segoe UI", 10, "bold"))
        self.state_label.grid(row=0, column=1, sticky="w", padx=(12, 0))
        self.stats_label = ttk.Label(status, text="", foreground="#555")
        self.stats_label.grid(row=1, column=1, sticky="w", padx=(12, 0))

        # Options ------------------------------------------------------------
        opts = ttk.LabelFrame(main, text="Options", padding=8)
        opts.grid(row=5, column=0, sticky="ew", **pad)
        self.auto_var = tk.BooleanVar(value=self.config.auto_connect)
        ttk.Checkbutton(opts, text="Connect automatically at start", variable=self.auto_var, command=self._save_options).grid(row=0, column=0, sticky="w")
        self.min_var = tk.BooleanVar(value=self.config.start_minimized)
        ttk.Checkbutton(opts, text="Start minimized to tray", variable=self.min_var, command=self._save_options).grid(row=1, column=0, sticky="w")
        ttk.Label(opts, text="Clock protocol:").grid(row=2, column=0, sticky="w", pady=(4, 0))
        self.timing_var = tk.StringVar(value=self.config.timing_protocol)
        timing = ttk.Combobox(opts, textvariable=self.timing_var, values=["PTP", "NTP"], state="readonly", width=6)
        timing.grid(row=2, column=1, sticky="w", padx=(6, 0), pady=(4, 0))
        timing.bind("<<ComboboxSelected>>", lambda _e: self._apply_timing())
        ttk.Label(opts, text="(PTP for HomePod / current Apple firmware, NTP for older receivers)", foreground="#555").grid(row=3, column=0, columnspan=2, sticky="w")

        footer = ttk.Frame(main)
        footer.grid(row=6, column=0, sticky="ew", **pad)
        ttk.Label(footer, text=f"{APP_NAME} {__version__}", foreground="#888").grid(row=0, column=0, sticky="w")
        ttk.Button(footer, text="Open log folder", command=self._open_logs).grid(row=0, column=1, sticky="e", padx=(12, 0))
        footer.columnconfigure(0, weight=1)

    # ---------------------------------------------------------------- actions
    def _load_devices(self) -> None:
        try:
            devices = list_capture_devices()
        except Exception as ex:  # pylint: disable=broad-except
            _LOGGER.error("Could not list devices: %s", ex)
            devices = []
        self._devices = [DEFAULT_DEVICE] + [d.name for d in devices]
        labels = [DEFAULT_DEVICE_LABEL] + [
            f"{d.name}" + ("" if d.is_loopback else "  (input)") for d in devices
        ]
        self.device_combo["values"] = labels
        try:
            index = self._devices.index(self.config.capture_device)
        except ValueError:
            index = 0
        self.device_combo.current(index)

    def _apply_device(self) -> None:
        index = self.device_combo.current()
        if 0 <= index < len(self._devices):
            self.engine.set_capture_device(self._devices[index])

    def _apply_ip(self) -> None:
        address = self.ip_var.get().strip()
        if address and address != self.config.device_ip:
            self.engine.set_device_ip(address)

    def _sync_latency_widgets(self) -> None:
        for label, ms in LATENCY_PRESETS:
            if ms == self.config.latency_ms:
                self.latency_var.set(label)
                break
        else:
            self.latency_var.set(CUSTOM_LABEL)
        self.custom_var.set(self.config.latency_ms)

    def _apply_latency_preset(self) -> None:
        label = self.latency_var.get()
        for preset_label, ms in LATENCY_PRESETS:
            if preset_label == label:
                self.custom_var.set(ms)
                self.engine.set_latency(ms)
                return

    def _apply_custom_latency(self) -> None:
        try:
            ms = int(self.custom_var.get())
        except (tk.TclError, ValueError):
            return
        self.engine.set_latency(ms)
        self._sync_latency_widgets()

    def _update_volume_label(self) -> None:
        self.volume_label.config(text=f"{int(self.volume_var.get())}%")

    def _apply_volume_toggle(self, save: bool = True) -> None:
        enabled = self.volume_enabled.get()
        self.volume_scale.state(["!disabled"] if enabled else ["disabled"])
        if save:
            self.engine.set_volume(int(self.volume_var.get()) if enabled else None)
            self._last_volume_sent = int(self.volume_var.get()) if enabled else None

    def _apply_volume(self) -> None:
        if not self.volume_enabled.get():
            return
        value = int(self.volume_var.get())
        if value != self._last_volume_sent:
            self._last_volume_sent = value
            self.engine.set_volume(value)

    def _apply_timing(self) -> None:
        self.engine.set_timing_protocol(self.timing_var.get())

    def _save_options(self) -> None:
        self.config.auto_connect = self.auto_var.get()
        self.config.start_minimized = self.min_var.get()
        self.config.save()

    def _toggle_connect(self) -> None:
        if self.engine.wants_connection:
            self.engine.disconnect()
        else:
            self._apply_ip()
            self.engine.connect()

    def _open_logs(self) -> None:
        try:
            os.startfile(app_dir())  # type: ignore[attr-defined]
        except Exception:  # pylint: disable=broad-except
            subprocess.Popen(["explorer", str(app_dir())])

    def _scan(self) -> None:
        if self._scan_future is not None:
            return
        self.scan_button.state(["disabled"])
        self.device_label.config(text="Scanning the network for AirPlay speakers...")
        self._scan_future = asyncio.run_coroutine_threadsafe(scan_network(timeout=4), self.engine.loop)
        self.root.after(300, self._poll_scan)

    def _poll_scan(self) -> None:
        future = self._scan_future
        if future is None:
            return
        if not future.done():
            self.root.after(300, self._poll_scan)
            return
        self._scan_future = None
        self.scan_button.state(["!disabled"])
        try:
            found = future.result()
        except Exception as ex:  # pylint: disable=broad-except
            self.device_label.config(text=f"Scan failed: {ex}")
            return
        if not found:
            self.device_label.config(text="No AirPlay speakers found (multicast blocked?). Type the IP instead.")
            return
        self._show_scan_results(found)

    def _show_scan_results(self, found: List[FoundDevice]) -> None:
        win = tk.Toplevel(self.root)
        win.title("AirPlay speakers")
        win.transient(self.root)
        win.resizable(False, False)
        ttk.Label(win, text="Pick a speaker:", padding=(8, 8, 8, 0)).pack(anchor="w")
        box = tk.Listbox(win, width=50, height=min(10, max(3, len(found))))
        box.pack(padx=8, pady=6)
        for dev in found:
            box.insert("end", f"{dev.name}  -  {dev.address}  [{dev.model}]")
        box.selection_set(0)

        def choose(_event=None):
            selection = box.curselection()
            if selection:
                dev = found[selection[0]]
                self.ip_var.set(dev.address)
                self.engine.set_device_ip(dev.address)
                self.device_label.config(text=f"{dev.name} ({dev.model})")
            win.destroy()

        box.bind("<Double-Button-1>", choose)
        ttk.Button(win, text="Use", command=choose).pack(pady=(0, 8))

    # ------------------------------------------------------------------ status
    def _refresh(self) -> None:
        snap = self.engine.snapshot()
        state: State = snap["state"]
        if state != self._last_state:
            self._last_state = state
            self._update_tray(state)
        wants = self.engine.wants_connection
        self.connect_button.config(text="Disconnect" if wants else "Connect")

        text = state.value
        if state == State.RECONNECTING and snap["retry_in"]:
            text += f" (retry in {snap['retry_in']:.0f} s)"
        self.state_label.config(text=text, foreground=STATE_COLORS.get(state, "black"))

        if snap["error"] and state in (State.ERROR, State.RECONNECTING):
            self.stats_label.config(text=snap["error"][:90])
        elif state == State.STREAMING:
            uptime = int(snap["uptime"])
            clock = "clock ok" if snap["timing_ok"] else "clock lost"
            parts = [
                f"{uptime // 60:d}:{uptime % 60:02d}",
                f"{snap['latency_ms']} ms",
                f"{snap['codec']}",
                f"{snap['timing']} {clock}",
            ]
            if snap["render_ms"]:
                parts.append(f"headroom {snap['latency_ms'] - snap['render_ms']} ms")
            if snap["stalls"]:
                parts.append(f"{snap['stalls']} pauses")
            if snap["retransmits"]:
                parts.append(f"{snap['retransmits']} resent")
            if snap["ring_dropped"]:
                parts.append(f"{snap['ring_dropped']} dropped")
            self.stats_label.config(text="  |  ".join(parts))
        else:
            self.stats_label.config(text="")

        name = snap["device_name"] or ""
        model = snap["device_model"] or ""
        if name:
            self.device_label.config(text=f"{name}  ({model})" if model else name)
        elif self._scan_future is None and state != State.CONNECTING:
            self.device_label.config(text="")

        if snap["capture_name"]:
            rate = f" @ {snap['capture_rate']} Hz" if snap["capture_rate"] else ""
            self.capture_label.config(text=f"Capturing: {snap['capture_name']}{rate}")
        else:
            self.capture_label.config(text="Not capturing")
        peak = snap["peak"]
        self.meter["value"] = max(0.0, min(100.0, (peak ** 0.5) * 100))

        self.root.after(REFRESH_MS, self._refresh)

    # -------------------------------------------------------------------- tray
    def _start_tray(self) -> None:
        if pystray is None:
            _LOGGER.info("pystray/Pillow not installed; no tray icon")
            return

        def on_main(fn):
            return lambda *_a: self.root.after(0, fn)

        def latency_item(label: str, ms: int):
            return pystray.MenuItem(
                label,
                on_main(lambda ms=ms: (self.engine.set_latency(ms), self._sync_latency_widgets())),
                checked=lambda _i, ms=ms: self.config.latency_ms == ms,
                radio=True,
            )

        menu = pystray.Menu(
            pystray.MenuItem(f"Show {APP_NAME}", on_main(self._show), default=True),
            pystray.MenuItem(
                lambda _i: "Disconnect" if self.engine.wants_connection else "Connect",
                on_main(self._toggle_connect),
            ),
            pystray.MenuItem("Latency", pystray.Menu(*[latency_item(label, ms) for label, ms in LATENCY_PRESETS])),
            pystray.Menu.SEPARATOR,
            pystray.MenuItem("Quit", on_main(self._quit)),
        )
        self.tray = pystray.Icon(APP_NAME, _tray_image(STATE_COLORS[State.DISCONNECTED]), APP_NAME, menu)
        try:
            self.tray.run_detached()
        except Exception as ex:  # pylint: disable=broad-except
            _LOGGER.warning("Tray icon unavailable: %s", ex)
            self.tray = None

    def _update_tray(self, state: State) -> None:
        if self.tray is None:
            return
        try:
            self.tray.icon = _tray_image(STATE_COLORS.get(state, "#8a8a8a"))
            self.tray.title = f"{APP_NAME} - {state.value}"
        except Exception:  # pylint: disable=broad-except
            pass

    def _show(self) -> None:
        self.root.deiconify()
        self.root.lift()
        self.root.focus_force()

    def _on_close(self) -> None:
        if self.tray is None:
            self._quit()
            return
        self.root.withdraw()
        if not self._hinted_tray:
            self._hinted_tray = True
            try:
                self.tray.notify(f"{APP_NAME} keeps streaming from the tray. Right-click the icon to quit.", APP_NAME)
            except Exception:  # pylint: disable=broad-except
                pass

    def _quit(self) -> None:
        try:
            if self.tray is not None:
                self.tray.stop()
        except Exception:  # pylint: disable=broad-except
            pass
        self.engine.shutdown()
        self.root.quit()

    def run(self) -> None:
        self.root.mainloop()
        try:
            self.root.destroy()
        except tk.TclError:
            pass


_MUTEX_HANDLES: list = []


def acquire_single_instance() -> bool:
    """Return False when another WinPods window is already running on this desktop."""
    try:
        import ctypes

        kernel32 = ctypes.windll.kernel32
        handle = kernel32.CreateMutexW(None, False, f"Local\\{APP_NAME}-single-instance")
        if not handle:
            return True
        if kernel32.GetLastError() == 183:  # ERROR_ALREADY_EXISTS
            kernel32.CloseHandle(handle)
            return False
        _MUTEX_HANDLES.append(handle)  # held until the process exits
        return True
    except Exception:  # pylint: disable=broad-except
        return True


def run_gui(config: Config) -> int:
    if not acquire_single_instance():
        from tkinter import messagebox

        root = tk.Tk()
        root.withdraw()
        messagebox.showinfo(
            APP_NAME, f"{APP_NAME} is already running. Look for its icon in the tray."
        )
        root.destroy()
        return 1
    App(config).run()
    return 0
