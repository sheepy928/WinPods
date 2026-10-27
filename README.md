# WinPods

[![Build](https://github.com/sheepy928/WinPods/actions/workflows/build.yml/badge.svg)](https://github.com/sheepy928/WinPods/actions/workflows/build.yml)
[![Release](https://img.shields.io/github/v/release/sheepy928/WinPods)](https://github.com/sheepy928/WinPods/releases/latest)

Streams whatever Windows is playing to a HomePod (or any AirPlay speaker), TuneBlade-style,
with an **ultra low latency mode of 100 ms** (adjustable down to 75 ms).

It lives in the tray: pick the speaker, pick the latency, and it keeps streaming,
reconnecting by itself if the speaker or Wi-Fi drops out.

Tested with a HomePod mini on tvOS 27. Other AirPlay 2 receivers should behave the same;
AirPlay 1 devices (AirPort Express, shairport-sync) are supported through the NTP clock
option but have not been tested yet.

## Install

**Download:** grab `WinPods-vX.Y.Z-windows-x64.zip` from the
[latest release](https://github.com/sheepy928/WinPods/releases/latest), unzip it anywhere
and run `WinPods.exe`. The exe is unsigned, so SmartScreen may show a warning the first
time (*More info* → *Run anyway*).

**From source** (Python 3.11+ or [uv](https://github.com/astral-sh/uv)):

```bat
git clone https://github.com/sheepy928/WinPods.git
cd WinPods
setup.bat      :: creates .venv and installs dependencies
run.bat        :: starts WinPods (window + tray icon), or double-click WinPods.pyw
```

## First run

1. Enter the HomePod's IP address, or click **Find...** to pick it from a network scan.
2. Click **Connect**. When Windows asks whether to allow WinPods on the network, choose
   **Allow** (see [Windows Firewall](#windows-firewall) below).
3. Play something. Closing the window keeps WinPods running in the tray; quit from the
   tray menu.

To check the whole path without playing anything on the PC, stream a quiet test beep
(once per second, on the speaker only):

```bat
.venv\Scripts\python.exe -m winpods --headless --test-tone --ip 192.168.1.155 --duration 15
```

## Latency modes

| Preset | Latency | Notes |
|---|---|---|
| Ultra low | 100 ms | Same as TuneBlade's minimum. Wired PC + decent Wi-Fi. |
| Low | 250 ms | Comfortable for video with lip-sync tolerance. |
| Normal | 500 ms | Busy Wi-Fi. |
| Safe | 1 s | Congested networks. |
| AirPlay default | 2 s | What iTunes / Apple Music use. |

A HomePod needs about 70 ms between receiving audio and playing it (it reports this as
`arrivalToRenderLatencyMs: 69`). At 100 ms that leaves ~30 ms for Windows capture and
Wi-Fi. A custom value down to 75 ms can be entered; below ~90 ms expect occasional
dropouts unless the network is very quiet. Missing packets are re-sent on request, and
at 100 ms there is still time for that.

## Audio source

* **Windows default output (loopback)**: captures whatever plays on the current default
  playback device and follows it when you switch devices.
* Any other playback device's loopback, e.g. `CABLE Input (VB-Audio Virtual Cable)`.
  Set that cable as the Windows default device so nothing plays locally, and choose it
  here (or keep "Windows default output").
* Recording devices also work (shown with "(input)").

## Windows Firewall

The speaker sends its clock (PTP) messages to WinPods on UDP ports 319/320, so Windows
must allow incoming traffic for WinPods. On first start Windows normally asks; tick
**Private networks** and click *Allow access*. If your connection is set to *Public*
(Settings → Network & internet → Ethernet/Wi-Fi → Network profile type), either change it
to *Private* for your home network or also allow Public.

If WinPods shows *"No clock (PTP) messages from the receiver reached this PC"*, the
firewall is the usual cause. You can add the rule yourself from an **administrator**
PowerShell; point `-Program` at `WinPods.exe` (or at `.venv\Scripts\pythonw.exe` when
running from source):

```powershell
New-NetFirewallRule -DisplayName "WinPods" -Direction Inbound -Action Allow -Protocol UDP `
  -Program "C:\path\to\WinPods\WinPods.exe" -Profile Private
```

WinPods itself never changes firewall settings.

## Command line

```text
python -m winpods                      window + tray (same as run.bat)
python -m winpods --headless           no window; Ctrl+C to stop
    --ip 192.168.1.155                 receiver address
    --latency 100                      milliseconds
    --device "CABLE Input (VB-Audio Virtual Cable) [Loopback]"
    --volume 40                        set speaker volume on connect
    --test-tone                        stream a beep instead of Windows audio
    --timing PTP|NTP                   PTP for HomePods; NTP for older AirPlay devices
    --codec alac|pcm                   AirPlay 2 audio format (alac)
    --duration 60                      stop after N seconds
python -m winpods --list-devices       capture devices
python -m winpods --scan               AirPlay speakers on the network
```

Command-line overrides apply to that run only. Settings live in
`%APPDATA%\WinPods\config.json`, the log in `%APPDATA%\WinPods\winpods.log`.

## Building and releasing

```bat
build_exe.bat   :: -> dist\WinPods\WinPods.exe (no Python needed on the target PC)
```

GitHub Actions builds the exe on every push and runs the tests. Pushing a tag such as
`v0.2.0` (after bumping `__version__` in `winpods/__init__.py`) publishes a release with
the zipped build attached.

## How it works

```text
WASAPI loopback ──► resample to 44.1 kHz ──► ring buffer ──► sender thread ──► UDP/RTP ──► speaker
   (pyaudiowpatch)     (soxr, streaming)       (~5 ms)        ALAC + ChaCha20
                                                               timing anchors ◄── PTP clock of the speaker
```

* **Session setup** (pairing, RTSP, encryption keys, event channel) is
  [pyatv](https://github.com/postlund/pyatv)'s AirPlay 2 implementation. Its file
  player is replaced by a real-time sender: every 352-frame packet goes out the moment
  Windows has produced it.
* **Latency is set by the timing anchors.** Once a second WinPods tells the speaker
  "the frame sent right now must be played *latency* from now", written in the speaker's
  own PTP timescale. WinPods only listens to the speaker's clock (the speaker is the
  master); it never acts as a clock source.
* **Audio format:** current HomePod firmware accepts ALAC, AAC and AAC-ELD for realtime
  streams, so WinPods sends uncompressed ALAC frames (no encoder delay, no CPU cost).
* **Smoothing:** Windows delivers loopback audio in bursts (gaps of 10-25 ms). The
  anchors come from a smoothed model of the send timeline, so bursts never make the
  speaker re-synchronise, and silence is only inserted after 40 ms with no audio at all
  (nothing playing). Audio that piles up after a real capture hiccup is trimmed so the
  delay never grows over a session.

## Troubleshooting

* **Connect fails / keeps retrying**: check the IP, and that the speaker is not being used
  by another AirPlay sender. The status line shows the last error; details are in the log
  (*Open log folder*).
* **Dropouts**: raise the latency one preset. Wi-Fi congestion is the usual cause.
* **Nothing captured** ("Not capturing" or a flat meter): the selected device has no
  audio; with "Windows default output" check which device Windows is using.
* **Older AirPlay speakers**: set *Clock protocol* to NTP.

## Credits

Built on [pyatv](https://github.com/postlund/pyatv),
[PyAudioWPatch](https://github.com/s0d3s/PyAudioWPatch),
[python-soxr](https://github.com/dofuuz/python-soxr),
[pystray](https://github.com/moses-palmer/pystray) and
[pycaw](https://github.com/AndreMiras/pycaw).
