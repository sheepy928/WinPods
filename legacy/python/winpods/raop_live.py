"""Real-time AirPlay audio session with a selectable receiver latency.

pyatv does the hard parts (transient HAP pairing, RTSP, ChaCha20 packet encryption,
event channel).  Its own packet loop is built for playing files: it paces from a wall
clock with a fixed ~1.5 s latency.  This module keeps pyatv's session plumbing but
replaces the loop with a capture-driven sender thread:

* audio packets go out the moment captured frames are available (no batching);
* once a second a timing anchor tells the receiver "the frame that was just sent must be
  played `latency` from now" - and `latency` is ours to choose (100 ms works);
* the anchors come from a smoothed model of the send timeline, so packet-to-packet
  jitter does not make the receiver re-synchronise;
* AirPlay 2 receivers get their anchors in their own PTP timescale (see ptp.py) and the
  audio as uncompressed ALAC; AirPlay 1 receivers get NTP sync packets and raw PCM;
* when Windows stops producing audio, silence keeps the RTP timeline continuous, and
  late audio that piles up after a capture hiccup is trimmed so the delay never grows.
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass, field
import logging
import socket
import struct
import threading
from time import monotonic, perf_counter, perf_counter_ns
from typing import Optional, Tuple
import uuid

from pyatv.auth.hap_channel import setup_channel
from pyatv.const import Protocol
from pyatv.interface import BaseConfig, BaseService
from pyatv.protocols.airplay.auth import extract_credentials, verify_connection
from pyatv.protocols.airplay.channels import EventChannel
from pyatv.protocols.airplay.utils import (
    AirPlayMajorVersion,
    get_protocol_version,
    pct_to_dbfs,
)
from pyatv.protocols.raop import timing
from pyatv.protocols.raop.fifo import PacketFifo
from pyatv.protocols.raop.packets import (
    AudioPacketHeader,
    RetransmitReqeust,
    SyncPacket,
    TimingPacket,
)
from pyatv.protocols.raop.parsers import (
    EncryptionType,
    MetadataType,
    get_encryption_types,
    get_metadata_types,
)
from pyatv.protocols.raop.protocols import StreamContext, StreamProtocol
from pyatv.protocols.raop.protocols.airplayv1 import AirPlayV1
from pyatv.protocols.raop.protocols.airplayv2 import (
    EVENTS_READ_INFO,
    EVENTS_SALT,
    EVENTS_WRITE_INFO,
    FEEDBACK_INTERVAL,
    AirPlayV2,
)
from pyatv.settings import AirPlayVersion
from pyatv.support.chacha20 import Chacha20Cipher8byteNonce
from pyatv.support.http import decode_bplist_from_body, http_connect
from pyatv.support.metadata import MediaMetadata
from pyatv.support.rtsp import FRAMES_PER_PACKET, RtspSession

from .alac import encode_uncompressed
from .ptp import PtpFollower
from .ringbuffer import ByteRing

_LOGGER = logging.getLogger(__name__)

TIMING_PTP = "PTP"
TIMING_NTP = "NTP"
CODEC_ALAC = "alac"
CODEC_PCM = "pcm"
AUDIO_FORMATS = {  # codec -> (AirPlay 2 audioFormat bit, compression type)
    CODEC_PCM: (0x800, 1),  # PCM 44100/16/2
    CODEC_ALAC: (0x40000, 2),  # ALAC 44100/16/2
}

SAMPLE_RATE = 44100
CHANNELS = 2
BYTES_PER_CHANNEL = 2
PACKET_BACKLOG_SIZE = 1000  # packets kept for retransmission requests
MAX_SILENCE_BURST = 16  # silence packets sent per wake-up while catching up
MAX_DEBT_FRAMES = SAMPLE_RATE  # never trim more than 1 s of late audio after a stall
DEFAULT_RENDER_MS = 70  # receiver arrival-to-render time when it does not report one
SAFETY_MS = 5  # network allowance before a missing packet would be late
MIN_STALL_MS = 40  # Windows loopback delivers in bursts with gaps of ~10-25 ms
NORMAL_BACKLOG_MS = 30  # ring content beyond this after a stall is piled-up late audio
PTP_LOCK_TIMEOUT = 5.0
TIMELINE_ALPHA = 0.005  # smoothing of the send timeline (per packet, ~1.6 s)

# AirPlay 2 timing anchor (control packet type 0xD7):
#   frame played at `time` | time in master ns | frame sent at `time` | master clock id
ANCHOR = struct.Struct(">BBHIQIQ")
U32 = 0xFFFFFFFF
U64 = 0xFFFFFFFFFFFFFFFF

FIREWALL_HINT = (
    "No clock (PTP) messages from the receiver reached this PC. Allow WinPods / "
    "python.exe through Windows Firewall for this network (see README), or switch "
    "the clock protocol to NTP."
)


def _device_id() -> str:
    node = uuid.getnode()
    return ":".join(f"{(node >> shift) & 0xFF:02X}" for shift in range(40, -1, -8))


@dataclass
class StreamStats:
    """Counters updated by the sender thread and the control/timing protocols."""

    started_at: float = 0.0
    packets_sent: int = 0
    silence_packets: int = 0
    frames_trimmed: int = 0
    stalls: int = 0
    retransmit_requests: int = 0
    retransmitted: int = 0
    timing_requests: int = 0
    last_timing: float = 0.0
    anchors_sent: int = 0
    send_errors: int = 0
    last_packet: float = 0.0
    render_ms: int = 0
    feedback: dict = field(default_factory=dict)


class Timeline:
    """Smoothed mapping between perf_counter_ns() and the RTP time of sent frames."""

    def __init__(self, rate: int = SAMPLE_RATE) -> None:
        self.rate = rate
        self._t0 = 0
        self._r0 = 0.0
        self.err = 0.0
        self.started = False

    def start(self, t_ns: int, rtp: int) -> None:
        self._t0, self._r0, self.err = t_ns, float(rtp), 0.0
        self.started = True

    def nominal(self, t_ns: int) -> float:
        return self._r0 + (t_ns - self._t0) * self.rate / 1e9

    def head_at(self, t_ns: int) -> float:
        return self.nominal(t_ns) + self.err

    def observe(self, t_ns: int, head: int) -> None:
        self.err += TIMELINE_ALPHA * ((head - self.nominal(t_ns)) - self.err)


# --------------------------------------------------------------------------- protocols
class LiveAirPlayV2(AirPlayV2):
    """pyatv's AirPlay 2 protocol with PTP timing, ALAC and our latency hints."""

    def __init__(
        self,
        context: StreamContext,
        rtsp: RtspSession,
        latency_min: int,
        latency_max: int,
        sender_name: str,
        codec: str = CODEC_ALAC,
        ptp: Optional[PtpFollower] = None,
    ) -> None:
        super().__init__(context, rtsp)
        self.latency_min = latency_min
        self.latency_max = latency_max
        self.sender_name = sender_name
        self.codec = codec
        self.ptp = ptp
        self.receiver_ids: list = []
        self.receiver_clock_id: Optional[int] = None
        self.setup_response: dict = {}
        self.stream_response: dict = {}
        self.last_feedback: dict = {}

    async def _setup_base(self, timing_server_port: int) -> None:
        self._verifier = await verify_connection(
            self.context.credentials, self.rtsp.connection
        )
        device_id = _device_id()
        body = {
            "deviceID": device_id,
            "sessionUUID": str(uuid.uuid4()).upper(),
            "timingProtocol": TIMING_PTP if self.ptp else TIMING_NTP,
            "isMultiSelectAirPlay": True,
            "groupContainsGroupLeader": False,
            "macAddress": device_id,
            "model": "iPhone14,3",
            "name": self.sender_name,
            "osBuildVersion": "20F66",
            "osName": "iPhone OS",
            "osVersion": "16.5",
            "senderSupportsRelay": False,
            "sourceVersion": "690.7.1",
            "statsCollectionEnabled": False,
        }
        if self.ptp is not None:
            peer = {
                "Addresses": [self.rtsp.connection.local_ip],
                "ID": str(uuid.uuid5(uuid.NAMESPACE_DNS, f"winpods-{device_id}")).upper(),
                "SupportsClockPortMatchingOverride": True,
            }
            if not self.ptp.standard_ports:
                # Tell the receiver which port we listen on for its clock messages
                peer["ClockPorts"] = {key: self.ptp.port for key in self.receiver_ids}
            body["timingPeerInfo"] = peer
        else:
            body["timingPort"] = timing_server_port

        resp = decode_bplist_from_body(await self.rtsp.setup(body=body))
        _LOGGER.debug("Setup response body: %s", resp)
        self.setup_response = resp
        peer_info = resp.get("timingPeerInfo") or {}
        clock_id = peer_info.get("ClockID")
        self.receiver_clock_id = int(clock_id) & U64 if clock_id is not None else None

        event_port = resp.get("eventPort", 0)
        retries = 5
        transport = None
        while transport is None:
            try:
                transport, _ = await setup_channel(
                    EventChannel,
                    self._verifier,
                    self.rtsp.connection.remote_ip,
                    event_port,
                    EVENTS_SALT,
                    EVENTS_READ_INFO,
                    EVENTS_WRITE_INFO,
                )
            except ConnectionRefusedError:
                retries -= 1
                if retries == 0:
                    raise
                _LOGGER.debug("Event channel connect failed, retrying")
                await asyncio.sleep(1.0)
        self.event_channel = transport

    async def setup_audio_stream(self, control_client_port: int) -> None:
        if self._verifier is None:
            raise RuntimeError("base stream not set up")

        out_key, _ = self._verifier.encryption_keys(
            EVENTS_SALT, EVENTS_WRITE_INFO, EVENTS_READ_INFO
        )
        shared_secret = out_key[0:32]
        audio_format, compression = AUDIO_FORMATS[self.codec]

        setup_resp = await self.rtsp.setup(
            body={
                "streams": [
                    {
                        "audioFormat": audio_format,
                        "audioMode": "default",
                        "controlPort": control_client_port,
                        "ct": compression,
                        "isMedia": True,
                        "latencyMax": self.latency_max,
                        "latencyMin": self.latency_min,
                        "shk": shared_secret,
                        "spf": FRAMES_PER_PACKET,
                        "sr": SAMPLE_RATE,
                        "type": 0x60,  # realtime stream
                        "supportsDynamicStreamID": False,
                        "streamConnectionID": self.rtsp.session_id,
                    }
                ]
            }
        )
        resp = decode_bplist_from_body(setup_resp)
        _LOGGER.debug("Setup stream response: %s", resp)
        self.stream_response = resp

        stream = resp["streams"][0]
        self.context.control_port = stream["controlPort"]
        self.context.server_port = stream["dataPort"]
        self._cipher = Chacha20Cipher8byteNonce(shared_secret, shared_secret)

    async def _feedback_task_loop(self) -> None:
        """Keep-alive; also remember what the receiver reports about the stream."""
        while True:
            try:
                resp = await self.rtsp.feedback()
                if resp.body:
                    self.last_feedback = decode_bplist_from_body(resp)
            except Exception as ex:  # pylint: disable=broad-except
                _LOGGER.debug("Feedback failed: %s", ex)
            await asyncio.sleep(FEEDBACK_INTERVAL)

    def build_packet(self, header: bytes, pcm: bytes) -> bytes:
        """Encode, encrypt and assemble one RTP audio packet (no I/O)."""
        payload = encode_uncompressed(pcm, FRAMES_PER_PACKET) if self.codec == CODEC_ALAC else pcm
        cipher = self._cipher
        if cipher is None:
            return header + payload
        nonce = cipher.out_nonce
        return header + cipher.encrypt(payload, aad=header[4:12]) + nonce[-8:]


class LiveControl(asyncio.DatagramProtocol):
    """Control channel: periodic timing anchors and retransmission requests."""

    def __init__(
        self,
        context: StreamContext,
        backlog: PacketFifo,
        stats: StreamStats,
        timeline: Timeline,
    ) -> None:
        self.transport: Optional[asyncio.DatagramTransport] = None
        self.context = context
        self.backlog = backlog
        self.stats = stats
        self.timeline = timeline
        self.ptp: Optional[PtpFollower] = None
        self._first = True
        self._task: Optional[asyncio.Task] = None

    @property
    def port(self) -> int:
        assert self.transport is not None
        return self.transport.get_extra_info("socket").getsockname()[1]

    def connection_made(self, transport) -> None:
        self.transport = transport

    def start(self, dest: Tuple[str, int]) -> None:
        self.send_anchor(dest)
        self._task = asyncio.ensure_future(self._anchor_loop(dest))

    async def _anchor_loop(self, dest: Tuple[str, int]) -> None:
        try:
            while self.transport is not None:
                await asyncio.sleep(1.0)
                self.send_anchor(dest)
        except asyncio.CancelledError:
            pass
        except Exception:  # pylint: disable=broad-except
            _LOGGER.exception("Anchor task failed")

    def send_anchor(self, dest: Tuple[str, int]) -> None:
        """Tell the receiver which RTP frame corresponds to *now* (plus latency)."""
        if self.transport is None:
            return
        t_ns = perf_counter_ns()
        if self.timeline.started:
            head = int(round(self.timeline.head_at(t_ns)))
        else:
            head = self.context.rtptime
        latency = self.context.latency
        marker = 0x90 if self._first else 0x80

        if self.ptp is not None:
            master_ns = self.ptp.to_master_ns(t_ns)
            if master_ns is None or not self.ptp.master_clock_id:
                return  # no clock yet; try again next second
            packet = ANCHOR.pack(
                marker,
                0xD7,
                0x0006,
                (head - latency) & U32,
                master_ns & U64,
                head & U32,
                self.ptp.master_clock_id,
            )
        else:
            now_sec, now_frac = timing.ntp2parts(timing.ntp_now())
            packet = SyncPacket.encode(
                marker, 0xD4, 0x0007, (head - latency) & U32, now_sec, now_frac, head & U32
            )
        self.transport.sendto(packet, dest)
        self._first = False
        self.stats.anchors_sent += 1

    def datagram_received(self, data: bytes, addr) -> None:
        if len(data) < 8:
            return
        if data[1] & 0x7F != 0x55:
            _LOGGER.debug("Unhandled control packet from %s: %s", addr, data[:16].hex())
            return
        request = RetransmitReqeust.decode(data, allow_excessive=True)
        self.stats.retransmit_requests += 1
        for i in range(request.lost_packets):
            seqno = (request.lost_seqno + i) % 65536
            packet = self.backlog.get(seqno)
            if packet is None or self.transport is None:
                continue
            self.transport.sendto(b"\x80\xd6" + packet[2:4] + packet, addr)
            self.stats.retransmitted += 1

    def error_received(self, exc) -> None:
        _LOGGER.debug("Control socket error: %s", exc)

    def connection_lost(self, exc) -> None:
        self.transport = None
        self.stop()

    def stop(self) -> None:
        if self._task:
            self._task.cancel()
            self._task = None

    def close(self) -> None:
        self.stop()
        if self.transport:
            self.transport.close()
            self.transport = None


class LiveTimingServer(asyncio.DatagramProtocol):
    """Answers NTP-style timing requests (AirPlay 1 and NTP mode only)."""

    def __init__(self, stats: StreamStats) -> None:
        self.transport: Optional[asyncio.DatagramTransport] = None
        self.stats = stats

    @property
    def port(self) -> int:
        assert self.transport is not None
        return self.transport.get_extra_info("socket").getsockname()[1]

    def connection_made(self, transport) -> None:
        self.transport = transport

    def datagram_received(self, data: bytes, addr) -> None:
        if self.transport is None or len(data) < TimingPacket.length:
            return
        if data[1] & 0x7F != 0x52:  # timing request
            return
        request = TimingPacket.decode(data, allow_excessive=True)
        now_sec, now_frac = timing.ntp2parts(timing.ntp_now())
        response = TimingPacket.encode(
            request.proto,
            0xD3,
            7,
            0,
            request.sendtime_sec,
            request.sendtime_frac,
            now_sec,
            now_frac,
            now_sec,
            now_frac,
        )
        self.transport.sendto(response, addr)
        self.stats.timing_requests += 1
        self.stats.last_timing = monotonic()

    def error_received(self, exc) -> None:
        _LOGGER.debug("Timing socket error: %s", exc)

    def connection_lost(self, exc) -> None:
        self.transport = None

    def close(self) -> None:
        if self.transport:
            self.transport.close()
            self.transport = None


# ------------------------------------------------------------------------ the session
class LiveStreamSession:
    """One streaming session to one receiver."""

    def __init__(
        self,
        conf: BaseConfig,
        ring: ByteRing,
        latency_ms: int = 100,
        sender_name: str = "WinPods",
        volume_pct: Optional[float] = None,
        protocol_version: AirPlayVersion = AirPlayVersion.Auto,
        timing_protocol: str = TIMING_PTP,
        codec: str = CODEC_ALAC,
        title: str = "Windows audio",
    ) -> None:
        self.conf = conf
        self.ring = ring
        self.latency_ms = latency_ms
        self.sender_name = sender_name
        self.volume_pct = volume_pct
        self.protocol_version = protocol_version
        self.timing_protocol = timing_protocol
        self.codec = codec if codec in AUDIO_FORMATS else CODEC_ALAC
        self.title = title

        self.stats = StreamStats()
        self.context = StreamContext()
        self.backlog: PacketFifo = PacketFifo(PACKET_BACKLOG_SIZE)
        self.timeline = Timeline()

        self.service: Optional[BaseService] = None
        self.connection = None
        self.rtsp: Optional[RtspSession] = None
        self.protocol: Optional[StreamProtocol] = None
        self.control: Optional[LiveControl] = None
        self.timing_server: Optional[LiveTimingServer] = None
        self.ptp: Optional[PtpFollower] = None
        self.info: dict = {}
        self.sock: Optional[socket.socket] = None
        self._sender: Optional[_Sender] = None
        self._stopping = False
        self.error: Optional[str] = None

    # ----------------------------------------------------------------- properties
    @property
    def latency_frames(self) -> int:
        return int(round(self.latency_ms * SAMPLE_RATE / 1000.0))

    @property
    def remote_ip(self) -> str:
        return str(self.conf.address)

    @property
    def is_airplay2(self) -> bool:
        return isinstance(self.protocol, LiveAirPlayV2)

    @property
    def end_reason(self) -> Optional[str]:
        """Why the session is no longer usable, or None while it is healthy."""
        if self.error:
            return self.error
        if self._stopping:
            return "stopped"
        if self._sender is None or not self._sender.is_alive():
            return "sender stopped"
        if self.connection is None or self.connection.transport is None:
            return "receiver closed the connection"
        return None

    @property
    def alive(self) -> bool:
        return self.end_reason is None

    @property
    def feedback(self) -> dict:
        return getattr(self.protocol, "last_feedback", {}) or {}

    # ------------------------------------------------------------------ lifecycle
    async def start(self) -> None:
        """Connect, negotiate the stream and start sending audio."""
        try:
            await self._connect()
            await self._begin_stream()
        except Exception:
            await self.stop()
            raise

    async def _connect(self) -> None:
        service = self.conf.get_service(Protocol.RAOP)
        if service is None:
            raise RuntimeError(f"{self.conf.name} does not advertise an AirPlay audio service")
        self.service = service

        self.connection = await http_connect(self.remote_ip, service.port)
        self.rtsp = RtspSession(self.connection)

        self.context.credentials = extract_credentials(service)
        self.context.password = service.password
        self.context.sample_rate = SAMPLE_RATE
        self.context.channels = CHANNELS
        self.context.bytes_per_channel = BYTES_PER_CHANNEL

        version = get_protocol_version(service, self.protocol_version)
        local_ip = self.connection.local_ip
        if version == AirPlayMajorVersion.AirPlayV2:
            if self.timing_protocol == TIMING_PTP:
                self.ptp = PtpFollower(local_ip)
                self.ptp.start()
            protocol = LiveAirPlayV2(
                self.context,
                self.rtsp,
                latency_min=min(self.latency_frames, 11025),
                latency_max=max(self.latency_frames, 88200),
                sender_name=self.sender_name,
                codec=self.codec,
                ptp=self.ptp,
            )
            ids = []
            airplay = self.conf.get_service(Protocol.AirPlay)
            for props in (airplay.properties if airplay else {}, service.properties):
                for key in ("gid", "pi", "psi"):
                    value = props.get(key)
                    if value:
                        ids.append(value.split("+", 1)[0])
            protocol.receiver_ids = list(dict.fromkeys(ids))
            self.protocol = protocol
        else:
            self.codec = CODEC_PCM
            self.protocol = AirPlayV1(self.context, self.rtsp)
        _LOGGER.info(
            "Connecting to %s: AirPlay %s, %s timing, %s audio",
            self.conf.name,
            "2" if version == AirPlayMajorVersion.AirPlayV2 else "1",
            "PTP" if self.ptp else "NTP",
            self.codec.upper(),
        )

        loop = asyncio.get_running_loop()
        _, control = await loop.create_datagram_endpoint(
            lambda: LiveControl(self.context, self.backlog, self.stats, self.timeline),
            local_addr=(local_ip, 0),
        )
        _, timing_server = await loop.create_datagram_endpoint(
            lambda: LiveTimingServer(self.stats), local_addr=(local_ip, 0)
        )
        self.control = control  # type: ignore[assignment]
        self.timing_server = timing_server  # type: ignore[assignment]
        assert self.control and self.timing_server
        self.control.ptp = self.ptp

        self.info = await self.rtsp.info()

        encryption = get_encryption_types(service.properties)
        model = service.properties.get("am", "")
        if EncryptionType.MFiSAP in encryption and model.startswith("AirPort"):
            await self.rtsp.auth_setup()

        await self.protocol.setup(self.timing_server.port, self.control.port)

        if self.is_airplay2:
            protocol = self.protocol  # type: ignore[assignment]
            stream = (protocol.stream_response.get("streams") or [{}])[0]
            self.stats.render_ms = int(stream.get("arrivalToRenderLatencyMs") or 0)
            if self.ptp is not None:
                self.ptp.follow(self.remote_ip, protocol.receiver_clock_id)
                locked = await asyncio.to_thread(self.ptp.wait_locked, PTP_LOCK_TIMEOUT)
                if not locked:
                    raise RuntimeError(FIREWALL_HINT)
                _LOGGER.info(
                    "PTP clock locked (offset %.3f ms)", (self.ptp.offset_ns or 0) / 1e6
                )

        render_ms = self.stats.render_ms or DEFAULT_RENDER_MS
        if self.latency_ms < render_ms + SAFETY_MS:
            _LOGGER.warning(
                "Latency %d ms is below what %s needs (~%d ms); expect dropouts",
                self.latency_ms,
                self.conf.name,
                render_ms + SAFETY_MS,
            )
        _LOGGER.info(
            "Stream negotiated: data port %d, control port %d, receiver render time %s ms",
            self.context.server_port,
            self.context.control_port,
            self.stats.render_ms or "?",
        )

    async def _begin_stream(self) -> None:
        assert self.rtsp and self.protocol and self.control and self.connection
        self.context.reset()
        self.context.latency = self.latency_frames

        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.bind((self.connection.local_ip, 0))
        sock.connect((self.remote_ip, self.context.server_port))
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 1 << 20)
        self.sock = sock

        if MetadataType.Text in get_metadata_types(self.service.properties):
            try:
                await self.rtsp.set_metadata(
                    self.context.rtsp_session,
                    self.context.rtpseq,
                    self.context.rtptime,
                    MediaMetadata(title=self.title, artist=self.sender_name, album="AirPlay"),
                )
            except Exception as ex:  # pylint: disable=broad-except
                _LOGGER.debug("Metadata not accepted: %s", ex)

        await self.protocol.start_feedback()

        skip_record = self.is_airplay2 and bool(
            self.protocol.setup_response.get("skipRecord")  # type: ignore[attr-defined]
        )
        if not skip_record:
            await self.rtsp.record()

        try:
            await self.rtsp.flush(
                headers={
                    "Range": "npt=0-",
                    "Session": self.context.rtsp_session,
                    "RTP-Info": f"seq={self.context.rtpseq};rtptime={self.context.rtptime}",
                }
            )
        except Exception as ex:  # pylint: disable=broad-except
            _LOGGER.debug("FLUSH not accepted: %s", ex)

        if self.volume_pct is not None:
            await self.set_volume_pct(self.volume_pct)

        # Audio captured while connecting is stale; starting with it would add its
        # duration to the latency for the whole session.
        self.ring.clear()
        self.timeline.start(perf_counter_ns(), self.context.rtptime)
        self.control.start((self.remote_ip, self.context.control_port))
        self.stats.started_at = monotonic()
        self._sender = _Sender(self)
        self._sender.start()
        _LOGGER.info("Streaming with %d ms latency", self.latency_ms)

    async def set_volume_pct(self, pct: float) -> None:
        """Set receiver volume, 0..100 percent."""
        if self.rtsp is None:
            return
        pct = max(0.0, min(100.0, float(pct)))
        dbfs = pct_to_dbfs(pct)
        await self.rtsp.set_parameter("volume", str(dbfs))
        self.context.volume = dbfs
        self.volume_pct = pct

    async def stop(self) -> None:
        """Stop streaming and release everything. Safe to call more than once."""
        self._stopping = True
        if self._sender is not None:
            self._sender.stop()
            await asyncio.to_thread(self._sender.join, 2.0)
            self._sender = None
        if self.control:
            self.control.stop()
        if self.rtsp and self.connection and self.connection.transport is not None:
            try:
                await asyncio.wait_for(self.rtsp.teardown(self.context.rtsp_session), 3.0)
            except Exception as ex:  # pylint: disable=broad-except
                _LOGGER.debug("Teardown failed: %s", ex)
        if self.protocol:
            try:
                self.protocol.teardown()
            except Exception:  # pylint: disable=broad-except
                _LOGGER.debug("Protocol teardown failed", exc_info=True)
        if self.control:
            self.control.close()
            self.control = None
        if self.timing_server:
            self.timing_server.close()
            self.timing_server = None
        if self.sock:
            self.sock.close()
            self.sock = None
        if self.connection:
            self.connection.close()
            self.connection = None
        if self.ptp is not None:
            await asyncio.to_thread(self.ptp.close)
            self.ptp = None
        self.backlog.clear()


# ---------------------------------------------------------------------- sender thread
class _Sender(threading.Thread):
    """Moves captured frames from the ring to the receiver as fast as they arrive."""

    def __init__(self, session: LiveStreamSession) -> None:
        super().__init__(name="winpods-sender", daemon=True)
        self.session = session
        self._halt = threading.Event()

    def stop(self) -> None:
        self._halt.set()

    def run(self) -> None:
        try:
            self._run()
        except Exception as ex:  # pylint: disable=broad-except
            self.session.error = f"sender failed: {ex}"
            _LOGGER.exception("Sender thread failed")

    def _run(self) -> None:  # pylint: disable=too-many-locals,too-many-statements
        session = self.session
        ctx = session.context
        ring = session.ring
        stats = session.stats
        timeline = session.timeline
        sock = session.sock
        rtsp = session.rtsp
        protocol = session.protocol
        assert sock is not None and rtsp is not None and protocol is not None

        if isinstance(protocol, LiveAirPlayV2):
            build = protocol.build_packet
        else:

            def build(header: bytes, pcm: bytes) -> bytes:
                return header + pcm

        frame_size = ctx.frame_size
        packet_bytes = ctx.packet_size
        silence = bytes(packet_bytes)
        ssrc = rtsp.session_id

        # Silence is inserted only when Windows has really stopped delivering audio:
        # no capture data for `stall_s` *and* the timeline has run `slack` ahead of
        # what we sent.  Shorter gaps are normal loopback burstiness; the receiver's
        # buffer (latency minus its render time) rides them out.
        render_ms = stats.render_ms or DEFAULT_RENDER_MS
        slack_ms = min(max(session.latency_ms - render_ms - SAFETY_MS, 5), 250)
        slack_frames = slack_ms * SAMPLE_RATE / 1000.0
        stall_s = max(slack_ms, MIN_STALL_MS) / 1000.0
        normal_backlog = packet_bytes + int(NORMAL_BACKLOG_MS * SAMPLE_RATE / 1000) * frame_size
        _LOGGER.debug(
            "Sender: headroom %d ms, silence after %d ms without audio",
            session.latency_ms - render_ms,
            stall_s * 1000,
        )

        in_stall = False
        debt = 0  # frames of silence inserted during the current stall
        first = True

        def send(pcm: bytes) -> None:
            nonlocal first
            header = AudioPacketHeader.encode(
                0x80,
                0xE0 if first else 0x60,
                ctx.rtpseq,
                ctx.rtptime & U32,
                ssrc,
            )
            first = False
            packet = build(header, pcm)
            try:
                sock.send(packet)
            except OSError as ex:
                stats.send_errors += 1
                if stats.send_errors > 50:
                    raise RuntimeError(f"too many send errors ({ex})") from ex
            session.backlog[ctx.rtpseq] = packet
            ctx.rtpseq = (ctx.rtpseq + 1) % 65536
            ctx.head_ts += FRAMES_PER_PACKET
            stats.packets_sent += 1
            stats.last_packet = monotonic()
            timeline.observe(perf_counter_ns(), ctx.rtptime)

        while not self._halt.is_set():
            data = ring.read(packet_bytes, timeout=0.004)

            if data is not None:
                if in_stall:
                    in_stall = False
                    # Audio that piled up during a capture hiccup would add the
                    # hiccup to the latency for good; drop as much as we padded.
                    excess = len(ring) - normal_backlog
                    if debt > 0 and excess > 0:
                        trim = min(debt * frame_size, excess) // frame_size * frame_size
                        if trim:
                            ring.discard(trim)
                            stats.frames_trimmed += trim // frame_size
                            _LOGGER.debug("Trimmed %d ms of late audio", trim / frame_size / 44.1)
                    debt = 0
                send(data)
                continue

            # Not enough captured audio for a packet: has Windows stopped playing?
            target = timeline.head_at(perf_counter_ns())
            if not in_stall:
                quiet = perf_counter() - ring.last_write if ring.last_write else float("inf")
                if quiet < stall_s or target - ctx.rtptime < slack_frames:
                    continue
                in_stall = True
                stats.stalls += 1
                _LOGGER.debug("No audio from Windows; sending silence")

            burst = 0
            while ctx.rtptime + FRAMES_PER_PACKET <= target and burst < MAX_SILENCE_BURST:
                send(silence)
                stats.silence_packets += 1
                debt = min(debt + FRAMES_PER_PACKET, MAX_DEBT_FRAMES)
                burst += 1
