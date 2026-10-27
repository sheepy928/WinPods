"""Round-trip test for the uncompressed ALAC encoder.

The decoder here follows the escape-frame path of Apple's ALACDecoder.cpp so that any
drift in the bit layout shows up as a mismatch.
"""

import os
import struct

from winpods.alac import encode_uncompressed, frame_size

ID_CPE, ID_END = 1, 7
FRAMES = 352


class _Bits:
    def __init__(self, data: bytes) -> None:
        self.value = int.from_bytes(data, "big")
        self.total = len(data) * 8
        self.pos = 0

    def read(self, count: int) -> int:
        shift = self.total - self.pos - count
        assert shift >= 0, "read past end of frame"
        self.pos += count
        return (self.value >> shift) & ((1 << count) - 1)


def _decode(frame: bytes, bit_depth: int = 16):
    bits = _Bits(frame)
    out = []
    while True:
        tag = bits.read(3)
        if tag == ID_END:
            padding = (-bits.pos) % 8
            assert bits.read(padding) == 0 if padding else True
            assert bits.pos == bits.total, "trailing bytes after END"
            return out
        assert tag == ID_CPE
        assert bits.read(4) == 0  # element instance
        assert bits.read(12) == 0  # unused
        header = bits.read(4)
        partial, shifted, escape = header >> 3, (header >> 1) & 3, header & 1
        assert shifted == 0 and escape == 1
        count = FRAMES
        if partial:
            count = (bits.read(16) << 16) | bits.read(16)
        for _ in range(count):
            left = struct.unpack(">h", bits.read(bit_depth).to_bytes(2, "big"))[0]
            right = struct.unpack(">h", bits.read(bit_depth).to_bytes(2, "big"))[0]
            out.append((left, right))


def _samples(pcm_be: bytes):
    values = struct.unpack(">" + "h" * (len(pcm_be) // 2), pcm_be)
    return list(zip(values[0::2], values[1::2]))


def test_full_scale_extremes_survive():
    pcm = struct.pack(">" + "h" * (FRAMES * 2), *([32767, -32768] * FRAMES))
    assert _decode(encode_uncompressed(pcm, FRAMES)) == _samples(pcm)


def test_silence_frame_layout():
    frame = encode_uncompressed(bytes(FRAMES * 4), FRAMES)
    assert len(frame) == frame_size(FRAMES) == 1416
    assert frame[:3] == bytes.fromhex("200012")  # CPE, partial-frame + escape flags


def test_random_frames_round_trip():
    for _ in range(50):
        pcm = os.urandom(FRAMES * 4)
        encoded = encode_uncompressed(pcm, FRAMES)
        assert len(encoded) == frame_size(FRAMES)
        assert _decode(encoded) == _samples(pcm)
