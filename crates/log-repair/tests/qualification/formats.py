"""Independent bounded header/byte/checksum oracles for selected real entries.

These helpers do not authenticate signatures, Merkle roots or SHIP semantics.
The Rust CLI remains the whole-signed-block framing implementation under test.
"""
import hashlib
import struct
import zlib

from .common import CHUNK, require

LIMIT = 256 << 20
SHIP_MAGIC = 0xC35D500000000000


def extent(source, offset, length):
    require(0 <= length <= LIMIT, "selected entry exceeds qualification memory bound")
    return b"".join(source.read(at, min(CHUNK, offset + length - at))
                    for at in range(offset, offset + length, CHUNK))


class FC:
    def __init__(self, data):
        self.data, self.at = data, 0

    def take(self, count):
        require(0 <= count <= len(self.data) - self.at, "incomplete bounded FC field")
        value = self.data[self.at:self.at + count]
        self.at += count
        return value

    def var(self):
        value = 0
        for shift in range(0, 35, 7):
            byte = self.take(1)[0]
            require(shift != 28 or byte <= 15, "varuint32 overflow")
            value |= (byte & 127) << shift
            if not byte & 128:
                require(shift == 0 or byte != 0, "noncanonical varuint32")
                return value
        raise RuntimeError("varuint32 overflow")


def block_header(body):
    cursor = FC(body)
    cursor.take(14)
    previous = cursor.take(32)
    cursor.take(68)
    optional = cursor.take(1)[0]
    require(optional in (0, 1), "invalid producer optional")
    if optional:
        cursor.take(4)
        count = cursor.var()
        require(count <= 4096, "too many producers")
        for _ in range(count):
            cursor.take(8)
            require(cursor.var() <= 1, "unsupported producer key in selected fixture")
            cursor.take(33)
    count = cursor.var()
    require(count <= 4096, "too many header extensions")
    for _ in range(count):
        cursor.take(2)
        cursor.take(cursor.var())
    number = int.from_bytes(previous[:4], "big") + 1
    require(number < 1 << 32, "block number overflow")
    digest = bytearray(hashlib.sha256(body[:cursor.at]).digest())
    digest[:4] = number.to_bytes(4, "big")
    return {"number": number, "id": digest.hex(), "previous": previous.hex(),
            "header_bytes": cursor.at}


def ship_payload(body):
    require(52 <= len(body) <= LIMIT and struct.unpack_from("<Q", body)[0] == SHIP_MAGIC,
            "unsupported SHIP entry")
    require(struct.unpack_from("<Q", body, 40)[0] == len(body) - 48, "SHIP size mismatch")
    marker = struct.unpack_from("<I", body, 48)[0]
    start = 60 if marker == 1 else 52
    require(len(body) >= start, "incomplete SHIP size marker")
    expected = struct.unpack_from("<Q", body, 52)[0] if marker == 1 else None
    require(expected is None or expected <= LIMIT, "SHIP declared expansion exceeds bound")
    stream = zlib.decompressobj()
    payload = stream.decompress(body[start:], LIMIT + 1)
    require(len(payload) <= LIMIT and stream.eof and not stream.unconsumed_tail
            and not stream.unused_data, "incomplete, trailing or oversized zlib stream")
    require(expected is None or expected == len(payload), "SHIP uncompressed size differs")
    require(marker <= 1 or marker == len(body) - start, "legacy SHIP compressed size differs")
    return {"block_id": body[8:40].hex(), "marker": marker, "zlib_offset": start,
            "uncompressed_bytes": len(payload),
            "uncompressed_sha256": hashlib.sha256(payload).hexdigest()}
