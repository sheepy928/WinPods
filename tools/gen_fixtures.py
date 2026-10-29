#!/usr/bin/env python3
"""Generate the JSON test fixtures in crates/airplay/tests/fixtures/.

Every expected value is produced by the real Python implementations that WinPods v0.1
ran on: pyatv 0.18 (HAP pairing, TLV8, HAP framing, RTSP, event channel, RAOP packets,
mDNS), srptools 1.0.1 (SRP-6a) and the legacy WinPods modules in legacy/python/winpods
(raop_live.py, ptp.py, alac.py, discovery.py).  Network and audio I/O are replaced by
fakes; nothing here opens an RTSP/AirPlay session or binds a PTP/audio socket.

Run from the repository root:

    .venv\\Scripts\\python.exe -I tools/gen_fixtures.py [--offline]

The only network traffic is one unicast mDNS lookup: the DNS-SD queries pyatv's unicast
scan would send, each sent once from an ephemeral UDP port to RECEIVER_IP:5353, with
replies collected for MDNS_WAIT seconds.  With --offline no packet is sent, and
mdns.json is rebuilt from the sanitized responses (and note, if any) already stored in
it, so the file comes out byte-identical.  The same reuse happens when the receiver does
not answer.

Everything else is deterministic: fixed inputs, seeded searches, and randomness inside
the libraries (uuid4, os.urandom, randrange) patched to fixed values.
"""

from __future__ import annotations

import argparse
import asyncio
import binascii
from collections import deque
import contextlib
import hashlib
from importlib import metadata
import inspect
from ipaddress import IPv4Address
import json
import logging
from pathlib import Path
import plistlib
import random
import re
import socket
import struct
import sys
import threading
import time
import types
from typing import Any, Callable, Dict, List, Optional, Tuple
from unittest import mock
import uuid

sys.dont_write_bytecode = True  # leave legacy/ and tools/ untouched
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "legacy" / "python"))
OUT_DIR = ROOT / "crates" / "airplay" / "tests" / "fixtures"

# pylint: disable=wrong-import-position
from srptools import SRPContext, SRPServerSession  # noqa: E402
from srptools import constants as srp_constants  # noqa: E402
from srptools.utils import hex_from, int_to_bytes  # noqa: E402

import pyatv  # noqa: E402
from pyatv import conf as pyatv_conf  # noqa: E402
from pyatv.auth import hap_srp, hap_tlv8  # noqa: E402
from pyatv.auth.hap_session import HAPSession  # noqa: E402
from pyatv.auth.hap_srp import SRPAuthHandler, hkdf_expand  # noqa: E402
from pyatv.auth.hap_tlv8 import TlvValue  # noqa: E402
from pyatv.const import Protocol  # noqa: E402
from pyatv.core import mdns  # noqa: E402
from pyatv.core.scan import UnicastMdnsScanner  # noqa: E402
from pyatv.protocols.airplay.auth import (  # noqa: E402
    CONTROL_INPUT_INFO,
    CONTROL_OUTPUT_INFO,
    CONTROL_SALT,
)
from pyatv.protocols.airplay.auth.hap_transient import (  # noqa: E402
    TRANSIENT_PIN,
    AirPlayHapTransientPairVerifyProcedure,
)
from pyatv.protocols.airplay.channels import EventChannel  # noqa: E402
from pyatv.protocols.airplay.utils import parse_features, pct_to_dbfs  # noqa: E402
from pyatv.protocols.dmap import tags as dmap_tags  # noqa: E402
from pyatv.protocols.raop import timing as raop_timing  # noqa: E402
from pyatv.protocols.raop.fifo import PacketFifo  # noqa: E402
from pyatv.protocols.raop.packets import (  # noqa: E402
    AudioPacketHeader,
    RetransmitReqeust,
    SyncPacket,
)
from pyatv.protocols.raop.protocols import StreamContext  # noqa: E402
from pyatv.protocols.raop.protocols.airplayv2 import (  # noqa: E402
    EVENTS_READ_INFO,
    EVENTS_SALT,
    EVENTS_WRITE_INFO,
)
from pyatv.support.chacha20 import Chacha20Cipher8byteNonce  # noqa: E402
from pyatv.support.dns import DnsMessage, QueryType  # noqa: E402
from pyatv.support.http import HttpConnection, HttpResponse  # noqa: E402
from pyatv.support.metadata import MediaMetadata  # noqa: E402
from pyatv.support.rtsp import FRAMES_PER_PACKET, RtspSession  # noqa: E402

from winpods import alac as wp_alac  # noqa: E402
from winpods import discovery as wp_discovery  # noqa: E402
from winpods import ptp as wp_ptp  # noqa: E402
from winpods import raop_live  # noqa: E402

# --------------------------------------------------------------------------- constants
LOCAL_IP = "192.168.1.57"
RECEIVER_IP = "192.168.1.155"
MDNS_PORT = 5353
MDNS_WAIT = 3.0
RECEIVER_NAME = "Living Room"
RECEIVER_MODEL = "AudioAccessory5,1"

FAKE_SENDER_MAC = "02:11:22:33:44:55"  # patched raop_live._device_id()
FAKE_RECEIVER_MAC = "02:AA:BB:CC:DD:EE"
FAKE_RECEIVER_BTADDR = "02:BB:CC:DD:EE:FF"
FAKE_GID = "0A0A0A0A-0000-4000-8000-000000000001"
FAKE_PI = "0B0B0B0B-0000-4000-8000-000000000002"
FAKE_PSI = "0C0C0C0C-0000-4000-8000-000000000003"
FAKE_PK = "0123456789abcdef" * 4
FAKE_SESSION_UUID = uuid.UUID("2b5e7c1a-0000-4000-8000-0000000000aa")  # patched uuid4()
RECEIVER_CLOCK_ID = 0x02AABBFFFECCDDEE  # EUI-64 of FAKE_RECEIVER_MAC
RECEIVER_CLOCK_ID_SIGNED = int.from_bytes(bytes.fromhex("f2aabbfffeccddee"), "big", signed=True)
# Receiver IDs for the ClockPorts example. Entirely fake (no part of a real device's
# gid/pi/psi), distinct from FAKE_GID/FAKE_PI/FAKE_PSI and from the canned SETUP
# response's timingPeerInfo ID (0D0D0D0D-...-4) so tests can tell them apart.
CLOCK_PORTS_RECEIVER_IDS = [
    "0E0E0E0E-0000-4000-8000-000000000005",
    "0F0F0F0F-0000-4000-8000-000000000006",
]

RTSP_SESSION_ID = 0x12345678
DACP_ID = "0123456789ABCDEF"
ACTIVE_REMOTE = 1234567890
AIRTUNES_SERVER = "AirTunes/980.77.2"
TIMING_SERVER_PORT = 6002
CONTROL_CLIENT_PORT = 6001

# Sanitized copies of the HomePod's TXT records (same keys/lengths as the real ones).
CANNED_AIRPLAY_TXT = {
    "acl": "0",
    "btaddr": FAKE_RECEIVER_BTADDR,
    "deviceid": FAKE_RECEIVER_MAC,
    "c": "9",
    "fex": "AMp/StBLNTwQoa7YDQ",
    "features": "0x4A7FCA00,0x3C354BD0",
    "flags": "0x1a0404",
    "gid": FAKE_GID,
    "igl": "1",
    "gcgl": "1",
    "model": RECEIVER_MODEL,
    "protovers": "1.1",
    "pi": FAKE_PI.lower(),
    "psi": FAKE_PSI,
    "pk": FAKE_PK,
    "srcvers": "980.77.2",
    "osvers": "27.0",
    "vv": "1",
}
CANNED_RAOP_TXT = {
    "cn": "0,1,2,3",
    "da": "true",
    "et": "0,3,5",
    "ft": "0x4A7FCA00,0x3C354BD0",
    "sf": "0x1a0404",
    "md": "0,1,2",
    "am": RECEIVER_MODEL,
    "pk": FAKE_PK,
    "tp": "UDP",
    "vn": "65537",
    "vs": "980.77.2",
    "ov": "27.0",
    "vv": "1",
}

U32 = 0xFFFFFFFF
U64 = 0xFFFFFFFFFFFFFFFF

_LOGGER = logging.getLogger("gen_fixtures")


# ----------------------------------------------------------------------------- helpers
def hx(data: bytes) -> str:
    return bytes(data).hex()


def min_hex(value: int) -> str:
    """Minimal big-endian bytes of an int, as srptools' int_to_bytes/hex_from does."""
    return hex_from(value)


def fbits(value: float) -> str:
    """IEEE-754 bits of a float, big-endian hex (bit-exact companion of the repr)."""
    return struct.pack(">d", value).hex()


def plist_json(value: Any) -> Any:
    """Plist value -> JSON: dict/list/str/bool/int/float as-is, bytes -> {"$data": hex}."""
    if isinstance(value, dict):
        return {str(k): plist_json(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [plist_json(v) for v in value]
    if value is None or isinstance(value, (bool, int, float, str)):
        return value
    if isinstance(value, (bytes, bytearray)):
        return {"$data": bytes(value).hex()}
    raise TypeError(f"cannot encode {type(value).__name__} in a fixture")


def bplist(value: Dict[str, Any]) -> bytes:
    return plistlib.dumps(value, fmt=plistlib.FMT_BINARY)  # same call pyatv uses


def check(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def meta(**extra: Any) -> Dict[str, Any]:
    info = {
        "generator": "tools/gen_fixtures.py",
        "pyatv": metadata.version("pyatv"),
        "srptools": metadata.version("srptools"),
    }
    info.update(extra)
    return info


def _scalar(value: Any) -> bool:
    return value is None or isinstance(value, (bool, int, float, str))


def _numeric(value: Any) -> bool:
    return value is None or isinstance(value, (bool, int, float))


def _dump(value: Any, level: int) -> str:
    pad = "  " * level
    if isinstance(value, dict):
        if not value:
            return "{}"
        items = [f"{pad}  {json.dumps(k)}: {_dump(v, level + 1)}" for k, v in value.items()]
        return "{\n" + ",\n".join(items) + "\n" + pad + "}"
    if isinstance(value, list):
        if not value:
            return "[]"
        inline = json.dumps(value)
        numeric = all(_numeric(v) for v in value) or all(
            isinstance(v, list) and all(_numeric(x) for x in v) for v in value
        )
        if numeric or (all(_scalar(v) for v in value) and len(inline) <= 100):
            return inline
        items = [f"{pad}  {_dump(v, level + 1)}" for v in value]
        return "[\n" + ",\n".join(items) + "\n" + pad + "]"
    return json.dumps(value)


def write_fixture(name: str, data: Dict[str, Any]) -> Path:
    text = _dump(data, 0) + "\n"
    json.loads(text)  # round-trip sanity check
    path = OUT_DIR / name
    path.write_text(text, encoding="utf-8", newline="\n")
    return path


@contextlib.contextmanager
def no_network():
    """Fail loudly if anything tries to bind/connect/send on a real socket."""

    def refuse(*_args, **_kwargs):
        raise AssertionError("network access attempted while generating offline fixtures")

    with contextlib.ExitStack() as stack:
        for name in ("bind", "connect", "connect_ex", "sendto"):
            stack.enter_context(mock.patch.object(socket.socket, name, refuse))
        yield


# ================================================================================= SRP
def srp_context() -> SRPContext:
    """Exactly the context pyatv.auth.hap_srp.SRPAuthHandler.step1 builds (pin 3939)."""
    return SRPContext(
        "Pair-Setup",
        str(TRANSIENT_PIN),
        prime=srp_constants.PRIME_3072,
        generator=srp_constants.PRIME_3072_GEN,
        hash_func=hashlib.sha512,
    )


N_BYTES = len(int_to_bytes(int(srp_constants.PRIME_3072, 16)))
SRP_CASES = [  # name, length of a, condition that must hold (all others must not)
    ("random", 128, None),
    ("salt_leading_zero", 128, "salt"),
    ("A_leading_zero", 128, "A"),
    ("B_leading_zero", 128, "B"),
    ("S_leading_zero", 128, "S"),
    ("a_32_bytes", 32, None),
]


def srp_flags(a: bytes, salt: bytes, b: bytes, want: Optional[str]) -> bool:
    """Lazily evaluate the leading-zero properties; True if exactly `want` holds."""
    ctx = SRP_CTX
    if (salt[0] == 0) != (want == "salt"):
        return False
    a_int, b_int = int.from_bytes(a, "big"), int.from_bytes(b, "big")
    A = ctx.get_client_public(a_int)
    if (len(int_to_bytes(A)) < N_BYTES) != (want == "A"):
        return False
    x = ctx.get_common_password_hash(salt)
    v = ctx.get_common_password_verifier(x)
    B = ctx.get_server_public(v, b_int)
    if (len(int_to_bytes(B)) < N_BYTES) != (want == "B"):
        return False
    u = ctx.get_common_secret(B, A)
    S = ctx.get_client_premaster_secret(x, B, a_int, u)
    return (len(int_to_bytes(S)) < N_BYTES) == (want == "S")


def srp_search(name: str, a_len: int, want: Optional[str]) -> Tuple[str, bytes, bytes, bytes]:
    for n in range(100_000):
        seed = f"winpods-srp-{name}-{n}"
        rng = random.Random(seed)  # str seeds hash with SHA-512: stable across runs
        a, salt, b = rng.randbytes(a_len), rng.randbytes(16), rng.randbytes(32)
        if srp_flags(a, salt, b, want):
            return seed, a, salt, b
    raise RuntimeError(f"no seed found for SRP case {name}")


def _text(value: Any) -> str:
    """srptools' hex_from returns bytes for bytes input and str for ints."""
    return value.decode() if isinstance(value, bytes) else value


def srp_run(a: bytes, salt: bytes, b: bytes) -> Dict[str, Any]:
    """Client = pyatv SRPAuthHandler (step1/step2), server = srptools SRPServerSession."""
    ctx = srp_context()
    x_server = ctx.get_common_password_hash(salt)  # raw salt bytes, like the client
    verifier = ctx.get_common_password_verifier(x_server)
    server = SRPServerSession(srp_context(), hex_from(verifier), private=b.hex())
    B_bytes = binascii.unhexlify(server.public)

    handler = SRPAuthHandler()
    handler._auth_private = a  # pylint: disable=protected-access
    handler.step1(TRANSIENT_PIN)
    A_bytes, M1 = handler.step2(B_bytes, salt)
    session = handler._session  # pylint: disable=protected-access

    server.process(session.public, salt.hex())
    check(server.verify_proof(session.key_proof), "server rejected client proof")
    check(server.key == session.key, "server and client derived different keys")
    check(server.key_proof_hash == session.key_proof_hash, "M2 mismatch")

    # Recompute with the srptools context primitives and check every value.
    a_int, b_int = int.from_bytes(a, "big"), int.from_bytes(b, "big")
    A = ctx.get_client_public(a_int)
    B = int.from_bytes(B_bytes, "big")
    check(A_bytes == int_to_bytes(A), "A bytes are not the minimal encoding")
    check(B == ctx.get_server_public(verifier, b_int), "B mismatch")
    u = ctx.get_common_secret(B, A)
    x = ctx.get_common_password_hash(salt)
    S = ctx.get_client_premaster_secret(x, B, a_int, u)
    check(S == ctx.get_server_premaster_secret(verifier, b_int, A, u), "S mismatch")
    K = ctx.get_common_session_key(S)
    check(K == hashlib.sha512(int_to_bytes(S)).digest(), "K != H(minimal S)")
    check(K.hex() == _text(session.key), "K mismatch with session.key")
    check(M1 == ctx.get_common_session_key_proof(K, salt, B, A), "M1 mismatch")
    check(M1.hex() == _text(session.key_proof), "M1 != session.key_proof")
    M2 = ctx.get_common_session_key_proof_hash(K, M1, A)
    check(M2.hex() == _text(session.key_proof_hash), "M2 != session.key_proof_hash")

    # The same values from plain hashlib, documenting the (minimal) encodings srptools uses.
    sha = lambda *parts: hashlib.sha512(b"".join(parts)).digest()  # noqa: E731
    n_int, g_int = ctx._prime, ctx._gen  # pylint: disable=protected-access
    h_n = int.from_bytes(sha(int_to_bytes(n_int)), "big")
    h_g = int.from_bytes(sha(int_to_bytes(g_int)), "big")
    h_i = int.from_bytes(sha(b"Pair-Setup"), "big")
    check(
        M1 == sha(int_to_bytes(h_n ^ h_g), int_to_bytes(h_i), salt, A_bytes, B_bytes, K),
        "M1 formula mismatch",
    )
    check(M2 == sha(A_bytes, M1, K), "M2 formula mismatch")
    pad = lambda v: int_to_bytes(v).rjust(N_BYTES, b"\0")  # noqa: E731
    check(u == int.from_bytes(sha(pad(A), pad(B)), "big"), "u formula mismatch")
    check(
        x == int.from_bytes(sha(salt, sha(b"Pair-Setup:" + str(TRANSIENT_PIN).encode())), "big"),
        "x formula mismatch",
    )

    m3 = hap_tlv8.write_tlv(  # exactly as hap_transient.verify_credentials builds M3
        {TlvValue.SeqNo: b"\x03", TlvValue.PublicKey: A_bytes, TlvValue.Proof: M1}
    )
    hkdf = {
        "control_write": hkdf_expand(CONTROL_SALT, CONTROL_OUTPUT_INFO, K),
        "control_read": hkdf_expand(CONTROL_SALT, CONTROL_INPUT_INFO, K),
        "events_write": hkdf_expand(EVENTS_SALT, EVENTS_WRITE_INFO, K),
        "events_read": hkdf_expand(EVENTS_SALT, EVENTS_READ_INFO, K),
    }
    procedure = AirPlayHapTransientPairVerifyProcedure(None, handler)
    check(
        procedure.encryption_keys(CONTROL_SALT, CONTROL_OUTPUT_INFO, CONTROL_INPUT_INFO)
        == (hkdf["control_write"], hkdf["control_read"]),
        "transient verifier keys differ from hkdf_expand",
    )
    return {
        "handler": handler,
        "values": {
            "A": A_bytes,
            "B": B_bytes,
            "x": x,
            "u": u,
            "S": S,
            "K": K,
            "M1": M1,
            "M2": M2,
            "verifier": verifier,
            "m3_tlv": m3,
            "hkdf": hkdf,
        },
    }


SRP_CTX = srp_context()


def gen_srp() -> Tuple[Dict[str, Any], Dict[str, Dict[str, Any]]]:
    ctx = SRP_CTX
    h = ctx.hash
    constants = {
        "N": min_hex(ctx._prime),  # pylint: disable=protected-access
        "g": ctx._gen,  # pylint: disable=protected-access
        "k": min_hex(ctx._mult),  # pylint: disable=protected-access
        "H_N_xor_H_g": min_hex(h(ctx._prime) ^ h(ctx._gen)),  # pylint: disable=protected-access
        "H_I": min_hex(h("Pair-Setup")),
        "I": "Pair-Setup",
        "P": str(TRANSIENT_PIN),
    }
    vectors = []
    runs: Dict[str, Dict[str, Any]] = {}
    for name, a_len, want in SRP_CASES:
        seed, a, salt, b = srp_search(name, a_len, want)
        run = srp_run(a, salt, b)
        val = run["values"]
        run.update(a=a, salt=salt, b=b, seed=seed)
        runs[name] = run
        _LOGGER.info("SRP %-18s seed %s (A %d B, B %d B, S %d B)", name, seed,
                     len(val["A"]), len(val["B"]), len(int_to_bytes(val["S"])))
        vectors.append(
            {
                "case": name,
                "seed": seed,
                "a": a.hex(),
                "salt": salt.hex(),
                "b": b.hex(),
                "A": hx(val["A"]),
                "B": hx(val["B"]),
                "x": min_hex(val["x"]),
                "u": min_hex(val["u"]),
                "S": min_hex(val["S"]),
                "K": hx(val["K"]),
                "M1": hx(val["M1"]),
                "M2": hx(val["M2"]),
                "verifier": min_hex(val["verifier"]),
                "m3_tlv": hx(val["m3_tlv"]),
                "hkdf": {k: hx(v) for k, v in val["hkdf"].items()},
            }
        )
    by_case = {v["case"]: v for v in vectors}
    check(by_case["salt_leading_zero"]["salt"].startswith("00"), "salt case")
    check(len(by_case["A_leading_zero"]["A"]) < 2 * N_BYTES, "A case")
    check(len(by_case["B_leading_zero"]["B"]) < 2 * N_BYTES, "B case")
    check(len(by_case["S_leading_zero"]["S"]) < 2 * N_BYTES, "S case")
    check(len(by_case["a_32_bytes"]["a"]) == 64, "a_32 case")
    return {"_meta": meta(), "constants": constants, "vectors": vectors}, runs


# ======================================================================== fake network
class FakeSocket:
    def __init__(self, local: Tuple[str, int], remote: Tuple[str, int]) -> None:
        self._local, self._remote = local, remote

    def getsockname(self) -> Tuple[str, int]:
        return self._local

    def getpeername(self) -> Tuple[str, int]:
        return self._remote


class ScriptedTransport:
    """asyncio.Transport stand-in: records writes and answers each with a canned reply."""

    def __init__(self, protocol: asyncio.Protocol, remote_port: int = 7000) -> None:
        self.protocol = protocol
        self.sock = FakeSocket((LOCAL_IP, 49152), (RECEIVER_IP, remote_port))
        self.written: List[bytes] = []
        self.responder: Optional[Callable[[bytes], Optional[bytes]]] = None

    def get_extra_info(self, name: str, default: Any = None) -> Any:
        return self.sock if name == "socket" else default

    def write(self, data: bytes) -> None:
        self.written.append(bytes(data))
        reply = self.responder(bytes(data)) if self.responder else None
        if reply is not None:
            # Deliver on the next loop iteration, after the caller registered its request.
            asyncio.get_running_loop().call_soon(self.protocol.data_received, reply)

    def close(self) -> None:
        pass

    def is_closing(self) -> bool:
        return False


def request_cseq(request: bytes) -> Optional[int]:
    head = request.split(b"\r\n\r\n", 1)[0] + b"\r\n"  # headers only, never the body
    match = re.search(rb"\r\nCSeq: (\d+)\r\n", head)
    return int(match.group(1)) if match else None


def rtsp_reply(
    cseq: Optional[int],
    body: bytes = b"",
    content_type: Optional[str] = None,
    headers: Tuple[Tuple[str, str], ...] = (),
    status: str = "200 OK",
    protocol: str = "RTSP/1.0",
) -> bytes:
    lines = [f"{protocol} {status}", f"Content-Length: {len(body)}"]
    if content_type:
        lines.append(f"Content-Type: {content_type}")
    lines.extend(f"{key}: {value}" for key, value in headers)
    lines.append(f"Server: {AIRTUNES_SERVER}")
    if cseq is not None:
        lines.append(f"CSeq: {cseq}")
    return ("\r\n".join(lines) + "\r\n\r\n").encode("utf-8") + body


def http_reply(body: bytes = b"") -> bytes:
    return rtsp_reply(
        None, body, "application/octet-stream" if body else None, protocol="HTTP/1.1"
    )


def new_connection() -> Tuple[HttpConnection, ScriptedTransport]:
    connection = HttpConnection()
    transport = ScriptedTransport(connection)
    connection.connection_made(transport)  # reads local/remote IP from the fake socket
    check(connection.local_ip == LOCAL_IP and connection.remote_ip == RECEIVER_IP, "IPs")
    return connection, transport


# ============================================================ pairing (hap_transient)
async def capture_pairing(run: Dict[str, Any]) -> Dict[str, Any]:
    """Run AirPlayHapTransientPairVerifyProcedure.verify_credentials over a fake link."""
    val = run["values"]
    m2_body = hap_tlv8.write_tlv(
        {TlvValue.SeqNo: b"\x02", TlvValue.Salt: run["salt"], TlvValue.PublicKey: val["B"]}
    )
    m4_body = hap_tlv8.write_tlv({TlvValue.SeqNo: b"\x04", TlvValue.Proof: val["M2"]})
    replies = [
        ("pair_pin_start", http_reply()),
        ("pair_setup_m2", http_reply(m2_body)),
        ("pair_setup_m4", http_reply(m4_body)),
    ]
    connection, transport = new_connection()
    queue = deque(reply for _, reply in replies)
    transport.responder = lambda _request: queue.popleft()

    x25519_private = bytes(range(0x40, 0x60))
    random_bytes = deque([run["a"], x25519_private])

    def urandom(size: int) -> bytes:
        value = random_bytes.popleft()
        check(len(value) == size, "unexpected os.urandom size")
        return value

    srp = SRPAuthHandler()
    verifier = AirPlayHapTransientPairVerifyProcedure(connection, srp)
    with mock.patch.object(hap_srp, "os", types.SimpleNamespace(urandom=urandom)):
        check(await verifier.verify_credentials(), "verify_credentials failed")
    check(not random_bytes and not queue, "pairing did not consume all canned data")
    check(len(transport.written) == 3, "expected three pairing posts")

    m3_request = transport.written[2]
    check(m3_request.endswith(val["m3_tlv"]), "M3 body differs from srp.json m3_tlv")
    check(
        verifier.encryption_keys(CONTROL_SALT, CONTROL_OUTPUT_INFO, CONTROL_INPUT_INFO)
        == (val["hkdf"]["control_write"], val["hkdf"]["control_read"]),
        "control keys after pairing differ",
    )
    m1_request = transport.written[1]
    return {
        "verifier": verifier,
        "requests": [
            ("pair_pin_start", transport.written[0]),
            ("pair_setup_m1", m1_request),
            ("pair_setup_m3", m3_request),
        ],
        "replies": replies,
        "m1_body": m1_request.split(b"\r\n\r\n", 1)[1],
    }


# ======================================================================= setup bodies
class _StopConnect(Exception):
    """Raised where LiveStreamSession._connect would start binding UDP sockets."""


def fake_ptp_follower(port: int) -> wp_ptp.PtpFollower:
    """A PtpFollower without sockets or thread; only what _setup_base reads."""
    follower = wp_ptp.PtpFollower.__new__(wp_ptp.PtpFollower)
    follower.port = port
    follower.start = lambda: None  # _connect starts the follower thread
    return follower


def receiver_conf(airplay_txt: Dict[str, str], raop_txt: Dict[str, str],
                  airplay_port: int = 7000, raop_port: int = 7000) -> pyatv_conf.AppleTV:
    conf = pyatv_conf.AppleTV(IPv4Address(RECEIVER_IP), RECEIVER_NAME)
    conf.add_service(pyatv_conf.ManualService(
        airplay_txt.get("deviceid"), Protocol.AirPlay, airplay_port, airplay_txt))
    conf.add_service(pyatv_conf.ManualService(
        airplay_txt.get("deviceid"), Protocol.RAOP, raop_port, raop_txt))
    return conf


async def connect_until_sockets(
    conf: Any, latency_ms: int, codec: str, timing_protocol: str, ptp_port: int = 319
) -> raop_live.LiveStreamSession:
    """Run the real LiveStreamSession._connect up to its first socket and stop there.

    This yields the LiveAirPlayV2 object exactly as WinPods configures it (latency_min,
    latency_max, codec, receiver_ids) without any I/O.
    """
    session = raop_live.LiveStreamSession(
        conf, ring=None, latency_ms=latency_ms, codec=codec, timing_protocol=timing_protocol
    )
    connection = types.SimpleNamespace(local_ip=LOCAL_IP, remote_ip=RECEIVER_IP, transport=None)
    loop = asyncio.get_running_loop()
    with mock.patch.object(raop_live, "http_connect", mock.AsyncMock(return_value=connection)), \
            mock.patch.object(raop_live, "PtpFollower", lambda _ip: fake_ptp_follower(ptp_port)), \
            mock.patch.object(loop, "create_datagram_endpoint", side_effect=_StopConnect):
        try:
            await session._connect()  # pylint: disable=protected-access
        except _StopConnect:
            pass
        else:
            raise AssertionError("_connect got past the socket setup")
    return session


BASE_RESPONSES = {
    "ptp": {
        "eventPort": 50001,
        "timingPort": 0,
        "timingPeerInfo": {
            "Addresses": [RECEIVER_IP, "fe80::11:22ff:fe33:4455"],
            "ClockID": RECEIVER_CLOCK_ID,
            "ClockPorts": {},
            "DeviceType": 0,
            "ID": "0D0D0D0D-0000-4000-8000-000000000004",
            "SupportsClockPortMatchingOverride": True,
        },
    },
    "ptp_signed_clock": {
        "eventPort": 50003,
        "timingPort": 0,
        "timingPeerInfo": {
            "Addresses": [RECEIVER_IP],
            "ClockID": RECEIVER_CLOCK_ID_SIGNED,  # some receivers send it as a signed int64
            "ID": "0D0D0D0D-0000-4000-8000-000000000004",
            "SupportsClockPortMatchingOverride": True,
        },
    },
    "ntp": {"eventPort": 50002, "timingPort": 0},
}
STREAM_RESPONSE = {
    "streams": [
        {
            "audioBufferSize": 8388608,
            "controlPort": 50011,
            "dataPort": 50010,
            "streamID": 1,
            "type": 96,
        }
    ]
}


async def run_setup(
    session: raop_live.LiveStreamSession,
    verifier: Any,
    base_response: Dict[str, Any],
    receiver_ids: Optional[List[str]] = None,
) -> Dict[str, Any]:
    protocol = session.protocol
    check(isinstance(protocol, raop_live.LiveAirPlayV2), "expected an AirPlay 2 session")
    protocol.rtsp.session_id = RTSP_SESSION_ID
    if receiver_ids is not None:
        protocol.receiver_ids = list(receiver_ids)
    bodies: List[Dict[str, Any]] = []
    responses = deque([bplist(base_response), bplist(STREAM_RESPONSE)])

    async def fake_setup(headers=None, body=None):
        check(headers is None, "raop_live passes no SETUP headers")
        bodies.append(body)
        return HttpResponse("RTSP", "1.0", 200, "OK", {"CSeq": str(len(bodies))},
                            responses.popleft())

    protocol.rtsp.setup = fake_setup
    channel = mock.AsyncMock(return_value=(types.SimpleNamespace(close=lambda: None), None))
    with mock.patch.object(raop_live, "verify_connection", mock.AsyncMock(return_value=verifier)), \
            mock.patch.object(raop_live, "setup_channel", channel), \
            mock.patch.object(raop_live, "_device_id", return_value=FAKE_SENDER_MAC), \
            mock.patch.object(raop_live.uuid, "uuid4", return_value=FAKE_SESSION_UUID):
        await protocol._setup_base(TIMING_SERVER_PORT)  # pylint: disable=protected-access
        await protocol.setup_audio_stream(CONTROL_CLIENT_PORT)
    check(len(bodies) == 2 and channel.await_count == 1, "unexpected SETUP flow")
    factory, ch_verifier, address, port, salt, out_info, in_info = channel.await_args.args
    check(factory is EventChannel and ch_verifier is verifier, "setup_channel arguments")

    shk = bodies[1]["streams"][0]["shk"]
    probe = Chacha20Cipher8byteNonce(shk, shk).encrypt(b"probe", aad=b"12345678")
    check(protocol._cipher.encrypt(b"probe", aad=b"12345678") == probe,  # pylint: disable=protected-access
          "audio cipher is not keyed with shk")
    return {
        "bodies": bodies,
        "read_base": {
            "receiver_clock_id": protocol.receiver_clock_id,
            "event_channel": {
                "address": address,
                "port": port,
                "salt": salt,
                "output_info": out_info,
                "input_info": in_info,
            },
        },
        "read_stream": {
            "control_port": protocol.context.control_port,
            "server_port": protocol.context.server_port,
            "cipher_key": hx(shk),
        },
    }


async def gen_setup_bodies(verifier: Any) -> Tuple[Dict[str, Any], Dict[str, Any]]:
    conf = receiver_conf(CANNED_AIRPLAY_TXT, CANNED_RAOP_TXT)
    events_write, _ = verifier.encryption_keys(EVENTS_SALT, EVENTS_WRITE_INFO, EVENTS_READ_INFO)

    base_cases = [
        ("ptp_standard_ports", raop_live.TIMING_PTP, 319, None, "ptp"),
        ("ntp", raop_live.TIMING_NTP, 319, None, "ntp"),
        ("ptp_dynamic_port", raop_live.TIMING_PTP, 50123, CLOCK_PORTS_RECEIVER_IDS,
         "ptp_signed_clock"),
    ]
    base_out = []
    keep: Dict[str, Any] = {}
    for name, timing_protocol, ptp_port, ids, response_name in base_cases:
        session = await connect_until_sockets(conf, 100, raop_live.CODEC_ALAC, timing_protocol,
                                              ptp_port)
        derived_ids = list(session.protocol.receiver_ids)
        result = await run_setup(session, verifier, BASE_RESPONSES[response_name], ids)
        body = result["bodies"][0]
        ptp = session.protocol.ptp
        base_out.append(
            {
                "name": name,
                "timing_protocol": timing_protocol,
                "ptp": None if ptp is None else {"port": ptp.port,
                                                 "standard_ports": ptp.standard_ports},
                "timing_server_port": TIMING_SERVER_PORT,
                "receiver_ids": list(session.protocol.receiver_ids),
                "receiver_ids_from_txt": derived_ids,
                "body": plist_json(body),
                "body_bplist": hx(bplist(body)),
                "response_name": response_name,
                "read": result["read_base"],
            }
        )
        keep[name] = body
    check("ClockPorts" in base_out[2]["body"]["timingPeerInfo"], "ClockPorts missing")
    check("ClockPorts" not in base_out[0]["body"]["timingPeerInfo"], "unexpected ClockPorts")
    check(base_out[1]["body"]["timingPort"] == TIMING_SERVER_PORT, "NTP timingPort")
    check(base_out[2]["read"]["receiver_clock_id"] == 0xF2AABBFFFECCDDEE, "signed ClockID")

    stream_out = []
    for codec, latency_ms in [(raop_live.CODEC_ALAC, ms) for ms in
                              (75, 85, 100, 250, 2000, 2001, 5000)] + [(raop_live.CODEC_PCM, 100)]:
        session = await connect_until_sockets(conf, latency_ms, codec, raop_live.TIMING_PTP)
        protocol = session.protocol
        frames = session.latency_frames
        check(frames == int(round(latency_ms * raop_live.SAMPLE_RATE / 1000.0)), "frames")
        # LiveStreamSession._connect: min(frames, 11025) / max(frames, 88200)
        check(protocol.latency_min == min(frames, 11025), "latency_min")
        check(protocol.latency_max == max(frames, 88200), "latency_max")
        result = await run_setup(session, verifier, BASE_RESPONSES["ptp"])
        body = result["bodies"][1]
        check(body["streams"][0]["shk"] == events_write[0:32], "shk derivation")
        stream_out.append(
            {
                "codec": codec,
                "latency_ms": latency_ms,
                "latency_frames": frames,
                "latency_min": protocol.latency_min,
                "latency_max": protocol.latency_max,
                "control_client_port": CONTROL_CLIENT_PORT,
                "body": plist_json(body),
                "body_bplist": hx(bplist(body)),
                "read": result["read_stream"],
            }
        )
        if codec == raop_live.CODEC_ALAC and latency_ms == 100:
            keep["stream_alac_100"] = body

    data = {
        "_meta": meta(),
        "device_id": FAKE_SENDER_MAC,
        "session_uuid": str(FAKE_SESSION_UUID).upper(),
        "timing_peer_id": str(uuid.uuid5(uuid.NAMESPACE_DNS,
                                         f"winpods-{FAKE_SENDER_MAC}")).upper(),
        "rtsp_session_id": RTSP_SESSION_ID,
        "local_ip": LOCAL_IP,
        "remote_ip": RECEIVER_IP,
        "sender_name": "WinPods",
        "events_write_key": hx(events_write),
        "receiver_txt": {"_airplay._tcp": CANNED_AIRPLAY_TXT, "_raop._tcp": CANNED_RAOP_TXT},
        "base": base_out,
        "stream": stream_out,
        "responses": {
            **{name: {"plist": plist_json(resp), "bplist": hx(bplist(resp))}
               for name, resp in BASE_RESPONSES.items()},
            "stream": {"plist": plist_json(STREAM_RESPONSE), "bplist": hx(bplist(STREAM_RESPONSE))},
        },
    }
    keep["shk"] = events_write[0:32]
    return data, keep


# ====================================================================== RTSP requests
INFO_RESPONSE = {
    "deviceID": FAKE_RECEIVER_MAC,
    "features": 0x3C354BD04A7FCA00,
    "keepAliveLowPower": True,
    "keepAliveSendStatsAsBody": True,
    "macAddress": FAKE_RECEIVER_MAC,
    "model": RECEIVER_MODEL,
    "name": RECEIVER_NAME,
    "pi": FAKE_PI.lower(),
    "pk": bytes.fromhex(FAKE_PK),
    "protocolVersion": "1.1",
    "psi": FAKE_PSI,
    "sourceVersion": "980.77.2",
    "statusFlags": 0x1A0404,
}
FEEDBACK_RESPONSE = {"streams": [{"sr": 44100.0, "streamConnectionID": RTSP_SESSION_ID, "type": 96}]}


async def gen_rtsp_requests(keep: Dict[str, Any], pairing: Dict[str, Any]) -> Dict[str, Any]:
    connection, transport = new_connection()
    with mock.patch("pyatv.support.rtsp.randrange", return_value=0):
        rtsp = RtspSession(connection)
    rtsp.session_id = RTSP_SESSION_ID
    rtsp.dacp_id = DACP_ID
    rtsp.active_remote = ACTIVE_REMOTE

    context = StreamContext()  # the values raop_live passes after context.reset()
    context.rtpseq = 12345
    context.start_ts = raop_timing.ntp2ts(0xEA5F3C0080000000, context.sample_rate)
    context.head_ts = context.start_ts
    context.latency = 4410  # latency_frames for 100 ms
    seq, rtptime = context.rtpseq, context.rtptime
    check(rtptime == 4410, "rtptime")

    volume = str(pct_to_dbfs(40.0))
    metadata_obj = MediaMetadata(title="Windows audio", artist="WinPods", album="AirPlay")
    bp = "application/x-apple-binary-plist"
    # The order LiveStreamSession sends these in (raop_live.py), so the CSeq values match
    # a real session: _connect sends GET /info, then protocol.setup() pairs over HTTP
    # (verify_connection, no CSeq) and sends SETUP base and SETUP stream.  _begin_stream
    # then sends SET_PARAMETER metadata (the receiver's TXT md=0,1,2 includes Text),
    # calls start_feedback() (which only creates the task) and RECORD; RECORD's request is
    # written before its first await, so the feedback task's first POST /feedback gets the
    # next CSeq.  FLUSH follows, then SET_PARAMETER volume (only when a volume is
    # configured).  TEARDOWN comes last, from close().
    exchanges = [
        ("info", lambda: rtsp.info(),
         lambda c: rtsp_reply(c, bplist(INFO_RESPONSE), bp)),
        ("setup_base", lambda: rtsp.setup(body=keep["ptp_standard_ports"]),
         lambda c: rtsp_reply(c, bplist(BASE_RESPONSES["ptp"]), bp)),
        ("setup_stream", lambda: rtsp.setup(body=keep["stream_alac_100"]),
         lambda c: rtsp_reply(c, bplist(STREAM_RESPONSE), bp)),
        ("set_metadata", lambda: rtsp.set_metadata(context.rtsp_session, seq, rtptime,
                                                   metadata_obj),
         lambda c: rtsp_reply(c)),
        ("record", lambda: rtsp.record(),
         lambda c: rtsp_reply(c, headers=(("Audio-Latency", "0"),))),
        ("feedback", lambda: rtsp.feedback(),
         lambda c: rtsp_reply(c, bplist(FEEDBACK_RESPONSE), bp)),
        ("flush", lambda: rtsp.flush(headers={
            "Range": "npt=0-",
            "Session": context.rtsp_session,
            "RTP-Info": f"seq={seq};rtptime={rtptime}",
        }), lambda c: rtsp_reply(c)),
        ("set_parameter_volume", lambda: rtsp.set_parameter("volume", volume),
         lambda c: rtsp_reply(c)),
        ("teardown", lambda: rtsp.teardown(context.rtsp_session),
         lambda c: rtsp_reply(c)),
    ]
    rtsp_requests = []
    rtsp_replies = []
    parsed_setup = None
    info = None
    for name, call, make_reply in exchanges:
        sent: List[bytes] = []

        def responder(request: bytes, make_reply=make_reply, sent=sent) -> bytes:
            reply = make_reply(request_cseq(request))
            sent.append(reply)
            return reply

        transport.responder = responder
        written_before = len(transport.written)
        response = await call()
        check(len(transport.written) == written_before + 1, f"{name}: expected one request")
        raw = transport.written[-1]
        rtsp_requests.append({"name": name, "cseq": request_cseq(raw), "bytes": hx(raw)})
        rtsp_replies.append({"name": name, "cseq": request_cseq(sent[0]), "bytes": hx(sent[0])})
        if name == "setup_stream":
            parsed_setup = response
        if name == "info":
            info = response
    check(info == INFO_RESPONSE, "info() did not decode the plist")
    check([r["cseq"] for r in rtsp_requests] == list(range(len(exchanges))),
          "CSeq values are not sequential in send order")

    metadata_body = transport.written[[n for n, *_ in exchanges].index("set_metadata")]
    metadata_body = metadata_body.split(b"\r\n\r\n", 1)[1]

    # Chronological order: the HTTP pairing posts go between GET /info and the first SETUP.
    pair_requests = [{"name": name, "cseq": request_cseq(raw), "bytes": hx(raw)}
                     for name, raw in pairing["requests"]]
    pair_replies = [{"name": name, "cseq": request_cseq(raw), "bytes": hx(raw)}
                    for name, raw in pairing["replies"]]
    check(all(r["cseq"] is None for r in pair_requests + pair_replies),
          "pairing posts must not carry a CSeq")
    requests = rtsp_requests[:1] + pair_requests + rtsp_requests[1:]
    replies = rtsp_replies[:1] + pair_replies + rtsp_replies[1:]

    check(parsed_setup is not None, "no SETUP response")
    return {
        "_meta": meta(),
        "session": {
            "local_ip": LOCAL_IP,
            "remote_ip": RECEIVER_IP,
            "session_id": RTSP_SESSION_ID,
            "dacp_id": DACP_ID,
            "active_remote": ACTIVE_REMOTE,
            "uri": rtsp.uri,
            "rtpseq": seq,
            "rtptime": rtptime,
            "rtsp_session": context.rtsp_session,
            "volume_value": volume,
        },
        "requests": requests,
        "responses_used": replies,
        "parsed_setup_response": {
            "protocol": parsed_setup.protocol,
            "version": parsed_setup.version,
            "code": parsed_setup.code,
            "message": parsed_setup.message,
            "headers": dict(parsed_setup.headers.items()),
            "body": hx(parsed_setup.body),
            "body_plist": plist_json(plistlib.loads(parsed_setup.body)),
        },
    }, metadata_body


# ============================================================================== TLV8
def gen_tlv8(pairing: Dict[str, Any], runs: Dict[str, Dict[str, Any]]) -> Dict[str, Any]:
    m1 = hap_tlv8.write_tlv(  # as hap_transient.verify_credentials builds M1
        {
            TlvValue.Method: b"\x00",
            TlvValue.SeqNo: b"\x01",
            TlvValue.Flags: int.to_bytes(hap_tlv8.Flags.TransientPairing.value, 1, "big"),
        }
    )
    check(m1 == pairing["m1_body"], "M1 differs from the body hap_transient posted")
    value = bytes((i * 37 + 11) & 0xFF for i in range(600))
    fragmented = hap_tlv8.write_tlv({TlvValue.PublicKey: value})
    check(hap_tlv8.read_tlv(fragmented)[TlvValue.PublicKey] == value, "fragment round trip")

    run = runs["random"]
    cases = [
        ("m1", m1),
        ("m2_salt_and_public_key", hap_tlv8.write_tlv({
            TlvValue.SeqNo: b"\x02", TlvValue.Salt: run["salt"],
            TlvValue.PublicKey: run["values"]["B"]})),
        ("m3", run["values"]["m3_tlv"]),
        ("m4_proof", hap_tlv8.write_tlv({TlvValue.SeqNo: b"\x04",
                                         TlvValue.Proof: run["values"]["M2"]})),
        ("error_authentication", hap_tlv8.write_tlv({TlvValue.SeqNo: b"\x04",
                                                     TlvValue.Error: b"\x02"})),
        ("error_backoff", hap_tlv8.write_tlv({TlvValue.SeqNo: b"\x02", TlvValue.Error: b"\x03",
                                              TlvValue.BackOff: b"\x3c\x00"})),
        ("exactly_255_bytes", hap_tlv8.write_tlv({TlvValue.EncryptedData: value[:255]})),
        ("exactly_256_bytes", hap_tlv8.write_tlv({TlvValue.EncryptedData: value[:256]})),
        ("exactly_510_bytes", hap_tlv8.write_tlv({TlvValue.EncryptedData: value[:510]})),
        ("empty_value_item", bytes([TlvValue.SeqNo, 1, 2, TlvValue.Error, 0])),
    ]
    decode_cases = []
    for name, encoded in cases:
        decoded = hap_tlv8.read_tlv(encoded)
        decode_cases.append({
            "name": name,
            "encoded": hx(encoded),
            "decoded": {str(int(tag)): hx(val) for tag, val in decoded.items()},
        })
    return {
        "_meta": meta(),
        "m1": hx(m1),
        "fragmented": {"tag": int(TlvValue.PublicKey), "value": hx(value), "encoded": hx(fragmented)},
        "decode_cases": decode_cases,
    }


# ======================================================================== HAP frames
def gen_hap_frames() -> Dict[str, Any]:
    output_key = bytes(range(0x00, 0x20))
    input_key = bytes(range(0x20, 0x40))
    sender = HAPSession()
    sender.enable(output_key, input_key)
    receiver = HAPSession()
    receiver.enable(input_key, output_key)  # swapped: decrypts what `sender` encrypts
    streamed = HAPSession()
    streamed.enable(input_key, output_key)

    messages = []
    stream = b""
    for size in (0, 1, 1023, 1024, 1025, 3000):
        plaintext = bytes((i * 31 + size * 7 + 3) & 0xFF for i in range(size))
        ciphertext = sender.encrypt(plaintext)
        check(receiver.decrypt(ciphertext) == plaintext, f"decrypt {size}")
        frames = -(-size // HAPSession.FRAME_LENGTH)
        check(len(ciphertext) == size + frames * (2 + HAPSession.AUTH_TAG_LENGTH), "frame size")
        messages.append({"plaintext": hx(plaintext), "ciphertext": hx(ciphertext)})
        stream += ciphertext
    # Feeding the whole stream in odd-sized pieces must give the same plaintext.
    out = b""
    for pos in range(0, len(stream), 777):
        out += streamed.decrypt(stream[pos:pos + 777])
    check(out == b"".join(bytes.fromhex(m["plaintext"]) for m in messages), "chunked decrypt")
    return {"_meta": meta(), "output_key": hx(output_key), "input_key": hx(input_key),
            "messages": messages}


# ===================================================================== event channel
def event_body() -> bytes:
    def txt(record: Dict[str, str]) -> bytes:
        items = [f"{k}={v}".encode() for k, v in record.items()]
        return b"".join(bytes([len(item)]) + item for item in items)

    value = {
        "deviceID": FAKE_RECEIVER_MAC,
        "features": 0x3C354BD04A7FCA00,
        "gid": FAKE_GID,
        "groupContainsGroupLeader": False,
        "isGroupLeader": True,
        "macAddress": FAKE_RECEIVER_MAC,
        "model": RECEIVER_MODEL,
        "name": RECEIVER_NAME,
        "pi": FAKE_PI.lower(),
        "psi": FAKE_PSI,
        "sourceVersion": "980.77.2",
        "statusFlags": 0x1A0404,
        "txtAirPlay": txt(CANNED_AIRPLAY_TXT),
        "txtRAOP": txt(CANNED_RAOP_TXT),
        "volume": -18.0,
        "audioLatencies": [],
    }
    for i in range(64):
        body = bplist({"type": "updateInfo", "value": value})
        if len(body) >= 2000:
            return body
        value["audioLatencies"].append(
            {"audioType": "default", "inputLatencyMicros": 0,
             "outputLatencyMicros": 400000 + 1000 * i, "type": 100})
    raise AssertionError("event body never reached 2 KB")


def device_request(cseq: Optional[int], body: bytes, server: Optional[str] = None) -> bytes:
    lines = ["POST /command RTSP/1.0", f"Content-Length: {len(body)}",
             "Content-Type: application/x-apple-binary-plist"]
    if cseq is not None:
        lines.append(f"CSeq: {cseq}")
    if server:
        lines.append(f"Server: {server}")
    return ("\r\n".join(lines) + "\r\n\r\n").encode() + body


def gen_event_channel() -> Dict[str, Any]:
    big = event_body()
    small = bplist({"type": "updateMRSupportedCommands", "params": {"mrSupportedCommandsFromSender": []}})
    single = device_request(1, big)
    cases = [
        ("single_command", [single]),
        ("two_concatenated", [device_request(1, big) + device_request(2, small)]),
        ("server_header", [device_request(3, small, AIRTUNES_SERVER)]),
        ("no_cseq", [device_request(None, small)]),
        ("split_mid_body", [single[:len(single) - 700], single[len(single) - 700:]]),
        ("split_mid_header", [single[:20], single[20:]]),
    ]
    out = []
    for name, chunks in cases:
        channel = EventChannel(bytes(32), bytes(32))  # dummy keys; send() is replaced
        sent: List[bytes] = []
        channel.send = sent.append
        per_chunk = []
        for chunk in chunks:
            before = len(sent)
            channel.buffer += chunk
            channel.handle_received()
            per_chunk.append([hx(r) for r in sent[before:]])
        entry: Dict[str, Any] = {"name": name, "input": hx(b"".join(chunks)),
                                 "responses": [hx(r) for r in sent]}
        if len(chunks) > 1:
            entry["chunks"] = [hx(c) for c in chunks]
            entry["responses_per_chunk"] = per_chunk
        entry["buffer_after"] = hx(channel.buffer)
        out.append(entry)
    check(len(out[1]["responses"]) == 2, "two responses for concatenated requests")
    check(out[4]["responses_per_chunk"][0] == [], "split request answered early")
    return {"_meta": meta(), "cases": out}


# ===================================================================== audio packets
def packet_samples(index: int) -> List[List[int]]:
    frames = []
    for i in range(FRAMES_PER_PACKET):
        left = ((i * 1031 + index * 7919 + 258) % 65536) - 32768
        right = ((i * 2053 + index * 104729 + 12345) % 65536) - 32768
        if left == right:
            right = (right + 1 + 32768) % 65536 - 32768
        frames.append([left, right])
    if index == 0:
        frames[:6] = [[0x0102, -2], [-32768, 32767], [32767, -32768], [-2, 0x0102],
                      [0, -1], [1, 0]]
    return frames


def pcm_be(frames: List[List[int]]) -> bytes:
    return struct.pack(">" + "h" * (2 * len(frames)), *(v for pair in frames for v in pair))


def gen_audio_packets(key: bytes) -> Tuple[Dict[str, Any], Dict[int, bytes]]:
    ssrc = RTSP_SESSION_ID
    sequences = []
    backlog: Dict[int, bytes] = {}
    plan = [
        (raop_live.CODEC_ALAC, 0xFFFE, 0xFFFFFFFF - 703, 4, 0),  # seq and ts both wrap
        (raop_live.CODEC_PCM, 100, 4410, 2, 4),
    ]
    for codec, seq0, ts0, count, sample_base in plan:
        protocol = raop_live.LiveAirPlayV2.__new__(raop_live.LiveAirPlayV2)
        protocol._cipher = Chacha20Cipher8byteNonce(key, key)  # pylint: disable=protected-access
        protocol.codec = codec
        check_cipher = Chacha20Cipher8byteNonce(key, key)
        packets = []
        for n in range(count):
            seq = (seq0 + n) % 65536
            rtptime = ts0 + n * FRAMES_PER_PACKET
            first = n == 0
            header = AudioPacketHeader.encode(0x80, 0xE0 if first else 0x60, seq, rtptime & U32,
                                              ssrc)
            frames = packet_samples(sample_base + n)
            pcm = pcm_be(frames)
            payload = wp_alac.encode_uncompressed(pcm, FRAMES_PER_PACKET) \
                if codec == raop_live.CODEC_ALAC else pcm
            packet = protocol.build_packet(header, pcm)
            nonce = packet[-8:]
            check(nonce == n.to_bytes(8, "little"), "nonce counter")
            check(check_cipher.decrypt(packet[12:-8], nonce=nonce, aad=header[4:12]) == payload,
                  "packet does not decrypt to the payload")
            if codec == raop_live.CODEC_ALAC:
                backlog[seq] = packet
            packets.append({
                "seq": seq,
                "ts": rtptime & U32,
                "rtptime": rtptime,
                "first": first,
                "samples": frames,
                "header": hx(header),
                "payload": hx(payload),
                "nonce": hx(nonce),
                "packet": hx(packet),
            })
        sequences.append({"codec": codec, "packets": packets})
    return {"_meta": meta(), "key": hx(key), "ssrc": ssrc, "frames_per_packet": FRAMES_PER_PACKET,
            "sequences": sequences}, backlog


# ============================================================================== ALAC
def gen_alac() -> Dict[str, Any]:
    vectors = [
        ("zeros", [[0, 0]] * 352),
        ("ramp", [[-32768 + i * 186, 32767 - i * 186] for i in range(352)]),
        ("alternating_extremes", [[32767, -32768] if i % 2 == 0 else [-32768, 32767]
                                  for i in range(352)]),
        ("asymmetric", [[0x0102 + i, -2 - 3 * i] for i in range(352)]),
        ("one_frame", [[0x0102, -2]]),
        ("three_frames", [[1, -1], [-32768, 32767], [0x7F80, 0x00FF]]),
    ]
    out = []
    for name, frames in vectors:
        for left, right in frames:
            check(-32768 <= left <= 32767 and -32768 <= right <= 32767, f"{name} range")
        pcm = pcm_be(frames)
        encoded = wp_alac.encode_uncompressed(pcm, len(frames))
        check(len(encoded) == wp_alac.frame_size(len(frames)), "frame_size")
        out.append({"name": name, "frames": len(frames), "samples": frames, "alac": hx(encoded),
                    "pcm_be": hx(pcm)})
    return {"_meta": meta(), "vectors": out}


# =========================================================================== anchors
class CaptureDatagram:
    def __init__(self) -> None:
        self.sent: List[Tuple[bytes, Any]] = []

    def sendto(self, data: bytes, addr: Any = None) -> None:
        self.sent.append((bytes(data), addr))

    def get_extra_info(self, _name: str, default: Any = None) -> Any:
        return default

    def close(self) -> None:
        pass


def bare_ptp_follower() -> wp_ptp.PtpFollower:
    """PtpFollower with every attribute __init__ sets, except its sockets and Thread state."""
    follower = wp_ptp.PtpFollower.__new__(wp_ptp.PtpFollower)
    follower.local_ip = LOCAL_IP
    follower._halt = threading.Event()  # pylint: disable=protected-access
    follower._lock = threading.Lock()  # pylint: disable=protected-access
    follower._locked = threading.Event()  # pylint: disable=protected-access
    follower.port = wp_ptp.PTP_EVENT_PORT
    follower._pending = {}  # pylint: disable=protected-access
    follower._samples = deque()  # pylint: disable=protected-access
    follower._offset = None  # pylint: disable=protected-access
    follower.master_ip = None
    follower.expected_clock = None
    follower._master_since = 0.0  # pylint: disable=protected-access
    follower.master_clock_id = 0
    follower.master_addr = ""
    follower.syncs = 0
    follower.follow_ups = 0
    follower.announces = 0
    follower.ignored = 0
    follower.last_sample = 0.0
    follower.last_delay_spread_ns = 0
    source = inspect.getsource(wp_ptp.PtpFollower.__init__)
    expected = set(re.findall(r"self\.(\w+)(?:\s*:\s*[^=]+)?\s*=", source))
    expected |= {"event_sock", "general_sock"} & set(re.findall(r"self\.(\w+)", source))
    missing = expected - set(vars(follower)) - {"event_sock", "general_sock"}
    check(not missing, f"bare_ptp_follower misses attributes: {missing}")
    return follower


def gen_anchors(backlog: Dict[int, bytes]) -> Dict[str, Any]:
    context = StreamContext()
    stats = raop_live.StreamStats()
    timeline = raop_live.Timeline()
    fifo: PacketFifo = PacketFifo(raop_live.PACKET_BACKLOG_SIZE)
    control = raop_live.LiveControl(context, fifo, stats, timeline)
    transport = CaptureDatagram()
    control.transport = transport
    dest = (RECEIVER_IP, 50011)

    # ---- AirPlay 2 timing anchors (0xD7) with a real (socket-less) PtpFollower
    ptp = bare_ptp_follower()
    ptp._offset = -1_234_567_890.75  # pylint: disable=protected-access
    control.ptp = ptp
    d7 = []
    d7_skipped = []
    ptp.master_clock_id = 0
    timeline.start(50_000_000_000, 4410)
    first, packet = anchor_raw(control, transport, dest, context, 50_000_000_000, 4410)
    check(packet is None and control._first, "anchor sent without a PTP master")  # pylint: disable=protected-access
    d7_skipped.append({"reason": "master_clock_id == 0", "first": first, "head": 4410,
                       "latency": 4410, "bytes": None})
    ptp.master_clock_id = RECEIVER_CLOCK_ID
    d7_cases = [
        (50_000_000_000, 4410, 0.0, 4410, None),
        (51_000_000_000, 4410 + 44100, 0.25, 4410, None),
        (52_000_000_000, 92610, 0.5, 4410, None),        # round half to even -> 92610
        (53_000_000_000, 92611, 0.5, 4410, None),        # round half to even -> 92612
        (54_000_000_000, 3000, 0.0, 4410, None),         # head - latency < 0
        (55_000_000_000, 2**32 + 1000, 0.0, 4410, None),  # head above 2^32
        (56_000_000_000, 2**32 + 100, -0.4, 4410, None),
        (57_000_000_000, 3 * 2**32 + 7, 0.0, 88200, None),
        (58_000_000_000, 4410, 0.0, 4410, True),         # first marker again
        (59_000_000_000, 2**32 - 1, 0.0, 0, 0xF2AABBFFFECCDDEE),
    ]
    for t_ns, head, frac, latency, override in d7_cases:
        if override is True:
            control._first = True  # pylint: disable=protected-access
        elif override is not None:
            ptp.master_clock_id = override
        timeline.start(t_ns, head)  # nominal(t_ns) == head
        timeline.err = frac  # head_at(t_ns) == head + frac
        head_at = timeline.head_at(t_ns)
        first, packet = anchor_raw(control, transport, dest, context, t_ns, latency)
        check(packet is not None, "no D7 packet")
        rounded = int(round(head_at))
        master_ns = ptp.to_master_ns(t_ns)
        expected = raop_live.ANCHOR.pack(0x90 if first else 0x80, 0xD7, 0x0006,
                                         (rounded - latency) & U32, master_ns & U64,
                                         rounded & U32, ptp.master_clock_id)
        check(packet == expected, "D7 layout")
        d7.append({"first": first, "head_at": head_at, "head_at_bits": fbits(head_at),
                   "head": rounded, "latency": latency, "t_ns": t_ns,
                   "ptp_offset_ns": ptp.offset_ns, "master_ns": master_ns,
                   "clock_id": ptp.master_clock_id, "bytes": hx(packet)})
    ptp.master_clock_id = RECEIVER_CLOCK_ID

    # timeline not started -> head = context.rtptime
    timeline.started = False
    context.start_ts, context.head_ts, context.latency = 1_000_000, 1_000_000 + 3520, 4410
    first, packet = anchor_raw(control, transport, dest, context, 60_000_000_000, 4410)
    d7.append({"first": first, "head_at": None, "head_at_bits": None, "head": context.rtptime,
               "latency": 4410, "t_ns": 60_000_000_000, "ptp_offset_ns": ptp.offset_ns,
               "master_ns": ptp.to_master_ns(60_000_000_000), "clock_id": ptp.master_clock_id,
               "bytes": hx(packet), "note": "timeline not started: head = context.rtptime"})

    # ---- AirPlay 1 / NTP sync packets (0xD4)
    control.ptp = None
    control._first = True  # pylint: disable=protected-access
    d4 = []
    for ntp, head, latency in [
        (0xEA5F3C0080000000, 4410, 4410),
        (0xEA5F3C0180001234, 4410 + 44100, 4410),
        (0xEA5F3C02FFFFFFFF, 2**32 + 1000, 4410),
        (0xEA5F3C0300000001, 1000, 4410),
    ]:
        timeline.start(10_000_000_000, head)
        with mock.patch.object(raop_timing, "ntp_now", return_value=ntp):
            first, packet = anchor_raw(control, transport, dest, context, 10_000_000_000, latency)
        ntp_sec, ntp_frac = raop_timing.ntp2parts(ntp)
        expected = SyncPacket.encode(0x90 if first else 0x80, 0xD4, 0x0007,
                                     (head - latency) & U32, ntp_sec, ntp_frac, head & U32)
        check(packet == expected, "D4 layout")
        d4.append({"first": first, "head": head, "latency": latency, "ntp": ntp,
                   "ntp_sec": ntp_sec, "ntp_frac": ntp_frac, "bytes": hx(packet)})

    # ---- retransmission requests (0xD5) answered from the packet backlog (0xD6 replies)
    for seq, packet in backlog.items():
        fifo[seq] = packet
    retransmit = []
    addr = (RECEIVER_IP, 50011)
    requests = [
        ("single", RetransmitReqeust.encode(0x80, 0xD5, 1, 0xFFFF, 1)),
        ("range_across_wrap", RetransmitReqeust.encode(0x80, 0xD5, 2, 0xFFFE, 4)),
        ("partially_missing", RetransmitReqeust.encode(0x80, 0xD5, 3, 1, 3)),
        ("none_stored", RetransmitReqeust.encode(0x80, 0xD5, 4, 500, 2)),
        ("no_marker_bit", RetransmitReqeust.encode(0x80, 0x55, 5, 0, 1)),
        ("trailing_bytes", RetransmitReqeust.encode(0x80, 0xD5, 6, 0, 1) + b"\x00\x00"),
        ("other_type_ignored", RetransmitReqeust.encode(0x80, 0xD4, 7, 0, 1)),
        ("too_short", RetransmitReqeust.encode(0x80, 0xD5, 8, 0, 1)[:7]),
    ]
    for name, request in requests:
        before = len(transport.sent)
        control.datagram_received(request, addr)
        replies = transport.sent[before:]
        for reply, reply_addr in replies:
            check(reply_addr == addr, "reply address")
            check(reply[:2] == b"\x80\xd6" and reply[4:] in backlog.values(), "reply layout")
        retransmit.append({"name": name, "request": hx(request), "from": list(addr),
                           "replies": [hx(r) for r, _ in replies]})
    return {
        "_meta": meta(),
        "d7": d7,
        "d7_not_sent": d7_skipped,
        "d4": d4,
        "retransmit": {
            "backlog": {str(seq): hx(packet) for seq, packet in backlog.items()},
            "requests": retransmit,
        },
    }


def anchor_raw(control, transport, dest, context, t_ns: int, latency: int):
    context.latency = latency
    first = control._first  # pylint: disable=protected-access
    before = len(transport.sent)
    with mock.patch.object(raop_live, "perf_counter_ns", return_value=t_ns):
        control.send_anchor(dest)
    sent = transport.sent[before:]
    check(len(sent) <= 1, "more than one anchor")
    return first, (sent[0][0] if sent else None)


# =============================================================================== PTP
C1 = RECEIVER_CLOCK_ID
C2 = 0x02DDEEFFFE001122
C3 = 0x02CCDDFFFE556677
C4 = 0x0244BBFFFE998877
OTHER_IP_A = "192.168.1.20"
OTHER_IP_B = "192.168.1.30"
MASTER_OFFSET = 1_000_000_000_000_000  # master clock = local clock + ~11.6 days


def ptp_ts(ns: int) -> bytes:
    return (ns // 10**9).to_bytes(6, "big") + (ns % 10**9).to_bytes(4, "big")


def ptp_message(msg_type: int, seq: int, clock: int, *, flags: int = 0, correction: int = 0,
                origin_ns: int = 0, version: int = 0x02, port: int = 1, extra: bytes = b"",
                truncate: Optional[int] = None) -> bytes:
    """Build a PTPv2 message (IEEE 1588-2008 common header + body)."""
    if msg_type == wp_ptp.MSG_ANNOUNCE:
        body = ptp_ts(origin_ns) + struct.pack(">hBBBBHB", 37, 0, 128, 248, 0xFE, 0xFFFF, 128) \
            + clock.to_bytes(8, "big") + struct.pack(">HB", 0, 0xA0)
        control, log_interval = 5, 1
    else:
        body = ptp_ts(origin_ns)
        control = 0 if msg_type == wp_ptp.MSG_SYNC else 2
        log_interval = -3
    body += extra
    length = 34 + len(body)
    header = struct.pack(">BBHBBHqI8sHHBb", msg_type & 0x0F, version, length, 0, 0, flags,
                         correction, 0, clock.to_bytes(8, "big"), port, seq, control,
                         log_interval)
    data = header + body
    return data[:truncate] if truncate is not None else data


APPLE_FOLLOW_UP_TLV = bytes.fromhex("0003001c0080c2000001") + bytes(22)


def ptp_cases() -> List[Dict[str, Any]]:
    o = MASTER_OFFSET
    sync, fup, ann = wp_ptp.MSG_SYNC, wp_ptp.MSG_FOLLOW_UP, wp_ptp.MSG_ANNOUNCE
    two = wp_ptp.FLAG_TWO_STEP
    s = 10**9

    def dg(note, data, ip, t2, port=319):
        return {"kind": "datagram", "note": note, "data": data, "from": ip, "port": port, "t2": t2}

    def follow(ip, clock, t):
        return {"kind": "follow", "ip": ip, "clock_id": clock, "mono_ns": t}

    a = 7200 * s
    case_a = [
        dg("sync before follow(): no master ip yet, ignored",
           ptp_message(sync, 1, C1, flags=two), RECEIVER_IP, a - 100_000_000),
        follow(RECEIVER_IP, C1, a),
        dg("announce: counted only", ptp_message(ann, 1, C1, origin_ns=a + o), RECEIVER_IP,
           a + 50_000_000, 320),
        dg("two-step sync (flags 0x0208): locks onto C1, pending",
           ptp_message(sync, 2, C1, flags=two | 0x0008), RECEIVER_IP, a + 100_000_000),
        dg("follow_up: first sample, offset initialised",
           ptp_message(fup, 2, C1, origin_ns=a + 100_000_000 + o - 2_400_000), RECEIVER_IP,
           a + 100_000_100, 320),
        dg("sync from another clock: ignored", ptp_message(sync, 77, C2, flags=two), OTHER_IP_A,
           a + 110_000_000),
        dg("follow_up from another clock: no pending sync",
           ptp_message(fup, 77, C2, origin_ns=a + o), OTHER_IP_A, a + 110_000_100, 320),
        dg("two-step sync with correction 1500.5 ns",
           ptp_message(sync, 3, C1, flags=two, correction=(1500 << 16) + 0x8000), RECEIVER_IP,
           a + 225_000_000),
        dg("follow_up with correction 250 ns: better sample, slewed",
           ptp_message(fup, 3, C1, correction=250 << 16,
                       origin_ns=a + 225_000_000 + o - 1_800_000), RECEIVER_IP,
           a + 225_000_100, 320),
        dg("one-step sync, negative correction (-700 ns - 5/65536): worse sample, slewed",
           ptp_message(sync, 4, C1, correction=-((700 << 16) + 5),
                       origin_ns=a + 350_000_000 + o - 3_000_000), RECEIVER_IP, a + 350_000_000),
        dg("PTPv1 packet: ignored", ptp_message(sync, 5, C1, version=0x01,
                                                origin_ns=a + 475_000_000 + o), RECEIVER_IP,
           a + 475_000_000),
        dg("43-byte packet: ignored", ptp_message(sync, 5, C1, truncate=43), RECEIVER_IP,
           a + 476_000_000),
        dg("follow_up without matching sync: ignored",
           ptp_message(fup, 99, C1, origin_ns=a + o), RECEIVER_IP, a + 477_000_000, 320),
        dg("PTPv2.1 (versionPTP byte 0x12) two-step sync: accepted",
           ptp_message(sync, 6, C1, flags=two, version=0x12), RECEIVER_IP, a + 600_000_000),
        dg("follow_up with Apple TLV, master stepped +8 ms: offset steps",
           ptp_message(fup, 6, C1, origin_ns=a + 600_000_000 + o + 8_000_000 - 2_000_000,
                       extra=APPLE_FOLLOW_UP_TLV), RECEIVER_IP, a + 600_000_100, 320),
        dg("announce 3.5 s after the last sample: clock no longer locked",
           ptp_message(ann, 2, C1, origin_ns=a + 4_100_000_000 + o), RECEIVER_IP,
           a + 4_100_000_000, 320),
        dg("two-step sync 4.6 s later", ptp_message(sync, 7, C1, flags=two), RECEIVER_IP,
           a + 5_200_000_000),
        dg("follow_up: older samples left the 4 s window, offset steps back",
           ptp_message(fup, 7, C1, origin_ns=a + 5_200_000_000 + o - 2_200_000), RECEIVER_IP,
           a + 5_200_000_100, 320),
        dg("two-step sync", ptp_message(sync, 8, C1, flags=two), RECEIVER_IP, a + 5_325_000_000),
        dg("follow_up: small improvement, slewed",
           ptp_message(fup, 8, C1, origin_ns=a + 5_325_000_000 + o - 2_100_000), RECEIVER_IP,
           a + 5_325_000_100, 320),
    ]
    b = 100 * s
    case_b = [
        follow(RECEIVER_IP, None, b),
        dg("sync from another ip within 2 s: ignored", ptp_message(sync, 1, C3, flags=two),
           OTHER_IP_A, b + 500_000_000),
        dg("two-step sync from the receiver ip: chosen", ptp_message(sync, 10, C1, flags=two),
           RECEIVER_IP, b + 600_000_000),
        dg("follow_up: first sample",
           ptp_message(fup, 10, C1, origin_ns=b + 600_000_000 + o - 2_500_000), RECEIVER_IP,
           b + 600_000_100, 320),
        dg("sync from the other clock after locking: ignored",
           ptp_message(sync, 2, C3, flags=two), OTHER_IP_A, b + 3_000_000_000),
    ]
    c = 200 * s
    case_c = [
        follow(RECEIVER_IP, C1, c),
        dg("expected clock silent for 1 s: other clock ignored",
           ptp_message(sync, 1, C4, flags=two), OTHER_IP_B, c + 1_000_000_000),
        dg("expected clock silent for 2.5 s: fall back to the other clock",
           ptp_message(sync, 2, C4, flags=two), OTHER_IP_B, c + 2_500_000_000),
        dg("follow_up: first sample",
           ptp_message(fup, 2, C4, origin_ns=c + 2_500_000_000 + o - 3_100_000), OTHER_IP_B,
           c + 2_500_000_100, 320),
        dg("expected clock appears later: ignored (already locked)",
           ptp_message(sync, 3, C1, flags=two), RECEIVER_IP, c + 2_700_000_000),
    ]
    d = 300 * s
    case_d = [
        follow(RECEIVER_IP, 0, d),
        dg("master ip silent for 1 s: other ip ignored", ptp_message(sync, 1, C4, flags=two),
           OTHER_IP_B, d + 1_000_000_000),
        dg("master ip silent for 2.1 s: one-step sync from other ip accepted",
           ptp_message(sync, 2, C4, origin_ns=d + 2_100_000_000 + o - 1_900_000), OTHER_IP_B,
           d + 2_100_000_000),
    ]
    return [
        {"name": "expected_clock_set", "events": case_a},
        {"name": "expected_clock_none_ip_rule", "events": case_b},
        {"name": "expected_clock_silent_fallback", "events": case_c},
        {"name": "expected_none_master_silent_fallback", "events": case_d},
    ]


def gen_ptp() -> Dict[str, Any]:
    clock = {"now": 0.0}
    out_cases = []
    with mock.patch.object(wp_ptp, "monotonic", lambda: clock["now"]):
        for case in ptp_cases():
            follower = bare_ptp_follower()
            events = []
            for event in case["events"]:
                if event["kind"] == "follow":
                    clock["now"] = event["mono_ns"] / 1e9
                    follower.follow(event["ip"], event["clock_id"])
                    events.append({"kind": "follow", "ip": event["ip"],
                                   "clock_id": event["clock_id"], "mono": clock["now"],
                                   "expected_clock": follower.expected_clock})
                    continue
                t2 = event["t2"]
                clock["now"] = t2 / 1e9
                follower._on_datagram(event["data"], (event["from"], event["port"]), t2)  # pylint: disable=protected-access
                offset = follower.offset_ns
                events.append({
                    "kind": "datagram",
                    "note": event["note"],
                    "datagram": hx(event["data"]),
                    "from": event["from"],
                    "port": event["port"],
                    "t2": t2,
                    "mono": clock["now"],
                    "offset_after": offset,
                    "offset_after_bits": None if offset is None else fbits(offset),
                    "master_clock_id": follower.master_clock_id,
                    "master_addr": follower.master_addr,
                    "locked": follower.locked,
                    "to_master_ns_at_t2": follower.to_master_ns(t2),
                    "samples_in_window": len(follower._samples),  # pylint: disable=protected-access
                    "pending": len(follower._pending),  # pylint: disable=protected-access
                    "last_delay_spread_ns": follower.last_delay_spread_ns,
                    "counters": {"syncs": follower.syncs, "follow_ups": follower.follow_ups,
                                 "announces": follower.announces, "ignored": follower.ignored},
                })
            out_cases.append({"name": case["name"], "events": events})
    first = out_cases[0]["events"]
    check(first[0]["offset_after"] is None and first[4]["offset_after"] is not None, "PTP lock")
    check(any(not e.get("locked", True) for e in first if e["kind"] == "datagram"
              and e["offset_after"] is not None), "stale case missing")
    return {
        "_meta": meta(),
        "constants": {
            "window_ns": wp_ptp.WINDOW_NS,
            "step_ns": wp_ptp.STEP_NS,
            "slew": wp_ptp.SLEW,
            "assumed_path_delay_ns": wp_ptp.ASSUMED_PATH_DELAY_NS,
            "stale_after_s": wp_ptp.STALE_AFTER,
            "fallback_after_s": wp_ptp.FALLBACK_AFTER,
            "max_pending": wp_ptp.MAX_PENDING,
        },
        "cases": out_cases,
    }


# ============================================================================ volume
def gen_volume(metadata_body: bytes) -> Dict[str, Any]:
    levels = []
    for pct in [0, 0.5, 1, 10, 33, 33.3, 40, 50, 66.7, 99.9, 100]:
        clamped = max(0.0, min(100.0, float(pct)))  # LiveStreamSession.set_volume_pct
        dbfs = pct_to_dbfs(clamped)
        text = str(dbfs)
        levels.append({"pct": pct, "dbfs": dbfs, "dbfs_bits": fbits(dbfs), "str": text,
                       "body": f"volume: {text}"})
    payload = dmap_tags.string_tag("minm", "Windows audio") \
        + dmap_tags.string_tag("asal", "AirPlay") + dmap_tags.string_tag("asar", "WinPods")
    dmap = dmap_tags.container_tag("mlit", payload)
    check(dmap == metadata_body, "dmap body differs from RtspSession.set_metadata")
    return {"_meta": meta(), "levels": levels,
            "metadata": {"title": "Windows audio", "artist": "WinPods", "album": "AirPlay",
                         "dmap": hx(dmap)}}


# ============================================================================== mDNS
SENSITIVE_TXT_KEYS = {"deviceid", "btaddr", "pk", "pi", "psi", "gid", "hgid", "hmid", "tsid",
                      "serialnumber", "serial", "sn", "macaddress", "mac"}
FAKE_DIGITS = {
    "deviceid": FAKE_RECEIVER_MAC.replace(":", ""),
    "btaddr": FAKE_RECEIVER_BTADDR.replace(":", ""),
    "gid": FAKE_GID.replace("-", ""),
    "pi": FAKE_PI.replace("-", ""),
    "psi": FAKE_PSI.replace("-", ""),
    "pk": FAKE_PK.upper(),
}
MAC_RE = re.compile(rb"(?<![0-9A-Fa-f:])(?:[0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}(?![0-9A-Fa-f:])")
UUID_RE = re.compile(rb"(?<![0-9A-Fa-f])[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-"
                     rb"[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}(?![0-9A-Fa-f])")
HEXRUN_RE = re.compile(rb"(?<![0-9A-Fa-f])[0-9A-Fa-f]{12,}(?![0-9A-Fa-f])")
INSTANCE_MAC_RE = re.compile(r"^([0-9A-Fa-f]{12})@")
AAAA = 28


def _records(message: DnsMessage):
    return message.answers + message.authorities + message.resources


def collect_identifiers(messages: List[DnsMessage]) -> Tuple[Dict[str, str], List[bytes]]:
    """Find device identifiers: canonical upper-case hex digits -> TXT key they came from."""
    found: Dict[str, str] = {}
    ipv6: List[bytes] = []

    def add(raw: bytes, key: str) -> None:
        digits = re.sub(rb"[^0-9A-Fa-f]", b"", raw).decode().upper()
        if len(digits) >= 12:
            found.setdefault(digits, key)

    for message in messages:
        for record in _records(message):
            names = [record.qname]
            if record.qtype == QueryType.PTR:
                names.append(record.rd)
            elif record.qtype == QueryType.SRV:
                names.append(record.rd["target"])
            for name in names:
                match = INSTANCE_MAC_RE.match(name)
                if match:
                    add(match.group(1).encode(), "deviceid")
            if record.qtype == QueryType.TXT:
                for key, value in record.rd.items():
                    key = key.lower()
                    for rx in (MAC_RE, UUID_RE):
                        for match in rx.finditer(value):
                            add(match.group(), key)
                    if key in SENSITIVE_TXT_KEYS:
                        rest = UUID_RE.sub(b"-", MAC_RE.sub(b"-", value))
                        for match in HEXRUN_RE.finditer(rest):
                            add(match.group(), key)
            if record.qtype == AAAA:
                ipv6.append(bytes(record.rd))
    return found, ipv6


def fake_for(canonical: str, key: str, used: set) -> str:
    preferred = FAKE_DIGITS.get(key)
    if preferred and len(preferred) == len(canonical) and preferred not in used:
        return preferred
    n = len(used) + 1
    while True:
        candidate = (f"{n:02X}" + "0123456789ABCDEF" * 8)[: len(canonical)]
        if candidate not in used:
            return candidate
        n += 1


def _forms(digits: str) -> List[str]:
    forms = [digits]
    if len(digits) == 12:
        pairs = [digits[i:i + 2] for i in range(0, 12, 2)]
        forms += [":".join(pairs), "-".join(pairs)]
    if len(digits) == 32:
        forms.append("-".join([digits[0:8], digits[8:12], digits[12:16], digits[16:20],
                               digits[20:32]]))
    return [f for form in forms for f in (form.upper(), form.lower())]


def fake_ipv6(original: bytes, index: int) -> bytes:
    """fe80::11:22ff:fe33:44xx for link-local, 2001:db8::11:22ff:fe33:44xx otherwise."""
    if original[:2] == b"\xfe\x80":
        base = bytes.fromhex("fe80000000000000001122fffe334400")
    else:
        base = bytes.fromhex("20010db800000000001122fffe334400")
    return base[:15] + bytes([0x55 + index])


def sanitize_responses(raw: List[bytes]) -> Tuple[List[bytes], Dict[str, Any]]:
    messages = [DnsMessage().unpack(data) for data in raw]
    identifiers, ipv6 = collect_identifiers(messages)
    used: set = set()
    mapping: Dict[str, str] = {}
    for canonical in sorted(identifiers, key=lambda c: (-len(c), c)):
        fake = fake_for(canonical, identifiers[canonical], used)
        used.add(fake)
        mapping[canonical] = fake
    pairs: List[Tuple[bytes, bytes]] = []
    for canonical, fake in mapping.items():
        for original_form, fake_form in zip(_forms(canonical), _forms(fake)):
            pairs.append((original_form.encode(), fake_form.encode()))
        if len(canonical) == 12:  # a MAC could also appear as 6 raw bytes
            pairs.append((bytes.fromhex(canonical), bytes.fromhex(fake)))
    ipv6_map = {addr: fake_ipv6(addr, i) for i, addr in enumerate(dict.fromkeys(ipv6))}
    pairs.extend(ipv6_map.items())
    pairs.sort(key=lambda p: -len(p[0]))

    sanitized = []
    for data in raw:
        out = data
        for old, new in pairs:
            check(len(old) == len(new), "replacement changes length")
            out = out.replace(old, new)
        check(len(out) == len(data), "sanitized packet changed length")
        projection = re.sub(rb"[^0-9A-Fa-f]", b"", out).decode().upper()
        for canonical in mapping:
            check(canonical not in projection, "an identifier survived sanitization")
            if len(canonical) == 12:
                check(bytes.fromhex(canonical) not in out, "raw MAC survived sanitization")
        for addr in ipv6_map:
            check(addr not in out, "an IPv6 address survived sanitization")
        before, after = DnsMessage().unpack(data), DnsMessage().unpack(out)
        check([(r.qtype, r.rd_length) for r in _records(before)]
              == [(r.qtype, r.rd_length) for r in _records(after)], "record layout changed")
        sanitized.append(out)
    return sanitized, {"identifiers": list(mapping), "ipv6": list(ipv6_map)}


def mdns_query_once(queries: List[bytes]) -> List[bytes]:
    """Send each query once to the receiver from an ephemeral port and collect replies."""
    replies: List[bytes] = []
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.bind(("0.0.0.0", 0))
        for query in queries:
            sock.sendto(query, (RECEIVER_IP, MDNS_PORT))
        deadline = time.monotonic() + MDNS_WAIT
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                break
            sock.settimeout(remaining)
            try:
                data, addr = sock.recvfrom(9000)
            except socket.timeout:
                break
            except ConnectionResetError:  # ICMP port unreachable on Windows
                continue
            if addr[0] == RECEIVER_IP:
                replies.append(data)
    return replies


async def scan_with(response: mdns.Response, hosts: List[str]):
    """Run pyatv.scan / WinPods discovery with only the UDP exchange replaced."""
    seen: List[List[str]] = []

    async def process(self, timeout):  # replaces UnicastMdnsScanner.process (network)
        seen.append(list(self.services))
        self.handle_response(response)

    with mock.patch.object(UnicastMdnsScanner, "process", process):
        found = await wp_discovery.find_device(hosts[0], timeout=1)
    return seen[0], found


async def pyatv_services() -> List[str]:
    services, _ = await scan_with(mdns.Response([], False, None), [RECEIVER_IP])
    return services


async def mdns_expected(responses: List[bytes]) -> Dict[str, Any]:
    parser = mdns.ServiceParser()
    for data in responses:
        parser.add_message(DnsMessage().unpack(data))
    services = parser.parse()
    # as UnicastDnsSdClientProtocol.get_response builds it
    response = mdns.Response(services=services, deep_sleep=False,
                             model=mdns._get_model(services))  # pylint: disable=protected-access
    _, found = await scan_with(response, [RECEIVER_IP])
    check(found is not None, "WinPods discovery did not find the receiver")
    conf = found.config
    raop = conf.get_service(Protocol.RAOP)
    airplay = conf.get_service(Protocol.AirPlay)
    raop_txt = dict(raop.properties.items()) if raop else {}
    airplay_txt = dict(airplay.properties.items()) if airplay else {}
    session = await connect_until_sockets(conf, 100, raop_live.CODEC_ALAC, raop_live.TIMING_PTP)
    features = {}
    if airplay:
        features["_airplay._tcp"] = int(parse_features(airplay_txt.get("features", "0x0")))
    if raop:
        features["_raop._tcp"] = int(parse_features(raop_txt.get("ft", "0x0")))
    return {
        "name": found.name,
        "address": found.address,
        "model": found.model,
        "device_info_model": str(conf.device_info.model),
        "raop_port": raop.port if raop else None,
        "airplay_port": airplay.port if airplay else None,
        "txt": {"_raop._tcp": raop_txt, "_airplay._tcp": airplay_txt},
        "features": features,
        "features_hex": {k: f"0x{v:016X}" for k, v in features.items()},
        "airplay2": isinstance(session.protocol, raop_live.LiveAirPlayV2),
        "receiver_ids": list(getattr(session.protocol, "receiver_ids", [])),
        "parsed_services": [
            {"type": s.type, "name": s.name, "address": None if s.address is None else str(s.address),
             "port": s.port}
            for s in services
        ],
    }


# ============================================================================== main
def load_existing(name: str) -> Optional[Dict[str, Any]]:
    path = OUT_DIR / name
    if path.exists():
        return json.loads(path.read_text(encoding="utf-8"))
    return None


def leak_check(paths: List[Path], identifiers: List[str], ipv6: List[bytes]) -> None:
    """Make sure no real identifier from the live mDNS replies ended up in any fixture.

    Besides the full identifier, UUID-sized and longer identifiers (gid/pi/psi/pk) are also
    checked for their leading 8 and trailing 12 digits, both as text and as ASCII inside a
    hex blob (e.g. a bplist string), so a partially kept real ID is caught too.
    """
    for path in paths:
        text = path.read_text(encoding="utf-8")
        lower = text.lower()
        projection = re.sub(r"[^0-9A-Fa-f]", "", text).upper()
        for canonical in identifiers:
            check(canonical not in projection, f"{path.name} contains a real identifier")
            if len(canonical) < 32:
                continue
            head, tail = canonical[:8].lower(), canonical[-12:].lower()
            check(re.search(rf"(?<![0-9a-f]){head}", lower) is None
                  and re.search(rf"{tail}(?![0-9a-f])", lower) is None,
                  f"{path.name} contains part of a real identifier")
            for fragment in (head, tail):
                for form in (fragment, fragment.upper()):
                    check(form.encode().hex() not in lower,
                          f"{path.name} contains part of a real identifier as ASCII hex")
        for addr in ipv6:
            check(addr.hex() not in text.lower(), f"{path.name} contains a real IPv6 address")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--offline", action="store_true",
                        help="do not query the receiver; reuse responses in mdns.json")
    args = parser.parse_args()
    logging.basicConfig(level=logging.INFO, format="%(message)s")
    for noisy in ("pyatv", "winpods", "asyncio"):
        logging.getLogger(noisy).setLevel(logging.ERROR)
    OUT_DIR.mkdir(parents=True, exist_ok=True)

    # --- mDNS: the one network exchange (everything else runs under no_network()).
    services = asyncio.run(_with_no_network(pyatv_services()))
    queries = mdns.create_service_queries(services, QueryType.PTR)
    real_ids: List[str] = []
    real_ipv6: List[bytes] = []
    note = None
    responses: List[bytes] = []
    existing = load_existing("mdns.json")
    if not args.offline:
        _LOGGER.info("mDNS: sending %d queries once to %s:%d", len(queries), RECEIVER_IP, MDNS_PORT)
        raw = mdns_query_once(queries)
        _LOGGER.info("mDNS: %d replies", len(raw))
        if raw:
            responses, found = sanitize_responses(raw)
            real_ids, real_ipv6 = found["identifiers"], found["ipv6"]
    if not responses:
        if existing and existing.get("responses"):
            check(existing["queries"] == [hx(q) for q in queries], "stored queries differ")
            responses = [bytes.fromhex(r) for r in existing["responses"]]
            # Keep exactly what is stored (including the absence of a note), so an
            # offline run reproduces the committed mdns.json byte for byte.
            note = existing.get("note")
            _LOGGER.info("mDNS: reusing %d stored sanitized responses", len(responses))
        elif args.offline:
            note = "offline run without stored responses; only the queries are recorded"
        else:
            note = "the receiver did not answer; only the queries are recorded"

    async def build_async(srp_runs):
        pairing = await capture_pairing(srp_runs["a_32_bytes"])
        setup_data, keep = await gen_setup_bodies(pairing["verifier"])
        rtsp_data, metadata_body = await gen_rtsp_requests(keep, pairing)
        expected = await mdns_expected(responses) if responses else None
        return pairing, setup_data, keep, rtsp_data, metadata_body, expected

    with no_network():
        srp_data, srp_runs = gen_srp()
    pairing, setup_data, keep, rtsp_data, metadata_body, expected = asyncio.run(
        _with_no_network(build_async(srp_runs)))

    with no_network():
        fixtures = {
            "srp.json": srp_data,
            "tlv8.json": gen_tlv8(pairing, srp_runs),
            "hap_frames.json": gen_hap_frames(),
            "event_channel.json": gen_event_channel(),
            "setup_bodies.json": setup_data,
            "rtsp_requests.json": rtsp_data,
        }
        audio, backlog = gen_audio_packets(keep["shk"])
        fixtures["audio_packets.json"] = audio
        fixtures["alac.json"] = gen_alac()
        fixtures["anchors.json"] = gen_anchors(backlog)
        fixtures["ptp.json"] = gen_ptp()
        mdns_data: Dict[str, Any] = {
            "_meta": meta(receiver_ip=RECEIVER_IP, port=MDNS_PORT, wait_s=MDNS_WAIT),
            "services": services,
            "queries": [hx(q) for q in queries],
            "responses": [hx(r) for r in responses],
            "expected": expected,
        }
        if note:
            mdns_data["note"] = note
        fixtures["mdns.json"] = mdns_data
        fixtures["volume.json"] = gen_volume(metadata_body)

    paths = [write_fixture(name, data) for name, data in fixtures.items()]
    if real_ids or real_ipv6:
        leak_check(paths + [OUT_DIR / "README.md"] if (OUT_DIR / "README.md").exists() else paths,
                   real_ids, real_ipv6)
    for path in paths:
        _LOGGER.info("wrote %-22s %8d bytes", path.name, path.stat().st_size)
    return 0


async def _with_no_network(coro):
    with no_network():
        return await coro


if __name__ == "__main__":
    sys.exit(main())
