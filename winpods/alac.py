"""Uncompressed ("escape") ALAC frames for 16-bit stereo audio.

AirPlay 2 receivers such as the HomePod accept ALAC for realtime streams but not raw
PCM.  ALAC has an escape mode that stores samples verbatim, so encoding costs almost
nothing and adds no latency.  Bitstream layout (see Apple's ALACDecoder.cpp):

    3 bits   element tag      1 = CPE (channel pair)
    4 bits   instance tag     0
   12 bits   unused           0
    1 bit    partial frame    1 = a 32-bit sample count follows
    2 bits   bytes shifted    0
    1 bit    escape           1 = uncompressed
   32 bits   sample count     (only when partial frame = 1)
   N x 2 x 16 bits            samples, left then right, MSB first
    3 bits   element tag      7 = END
             zero padding to a byte boundary
"""

from __future__ import annotations

_HEADER = (1 << 20) | (1 << 3) | 1  # CPE, tag 0, partial-frame flag, escape flag
_HEADER_BITS = 23
_COUNT_BITS = 32
_END_TAG = 0b111


def encode_uncompressed(pcm_be: bytes, frames: int) -> bytes:
    """Wrap interleaved big-endian stereo int16 PCM in one ALAC escape frame."""
    sample_bits = len(pcm_be) * 8
    value = (_HEADER << _COUNT_BITS) | frames
    value = (value << sample_bits) | int.from_bytes(pcm_be, "big")
    value = (value << 3) | _END_TAG
    total_bits = _HEADER_BITS + _COUNT_BITS + sample_bits + 3
    pad = -total_bits % 8
    return (value << pad).to_bytes((total_bits + pad) // 8, "big")


def frame_size(frames: int) -> int:
    """Size in bytes of an encoded stereo frame with `frames` samples per channel."""
    total_bits = _HEADER_BITS + _COUNT_BITS + frames * 32 + 3
    return (total_bits + 7) // 8
