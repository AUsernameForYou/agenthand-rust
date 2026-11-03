"""The metadata-free public copy is an ALLOWLIST, not a denylist
(fix pass F1 / F2; mirror of the Rust build's
tests/provenance_public_copy.rs).

A location-carrying photo's public copy must drop every metadata
carrier the copy does not explicitly need: for JPEG, every APPn
segment except JFIF (APP0), ICC (APP2) and Adobe (APP14), every COM
segment, and every APPn/COM between the scans of a progressive file;
for PNG, every chunk outside the image/colour set (C2PA caBX, eXIf,
the text chunks, tIME and any private chunk). Pixels must come
through identical.
"""

from __future__ import annotations

import base64
import binascii
import struct
from io import BytesIO

from PIL import Image

from agenthand.provenance import metadata_free_public_copy


def _segment(marker: int, payload: bytes) -> bytes:
    """One length-prefixed JPEG marker segment."""
    return bytes([0xFF, marker]) + struct.pack(">H", len(payload) + 2) + payload


def _has_marker(data: bytes, marker: int) -> bool:
    """True when the two-byte sequence FF `marker` occurs anywhere."""
    return bytes([0xFF, marker]) in data


def _pixels(data: bytes) -> tuple[tuple[int, int], str, bytes]:
    with Image.open(BytesIO(data)) as img:
        img.load()
        return img.size, img.mode, img.tobytes()


def _baseline_jpeg() -> bytes:
    """A real baseline JPEG from Pillow, opening with a JFIF APP0."""
    img = Image.new("RGB", (16, 16))
    img.putdata(
        [(x * 16, y * 16, (x + y) * 8) for y in range(16) for x in range(16)]
    )
    buf = BytesIO()
    img.save(buf, format="JPEG", quality=90)
    data = buf.getvalue()
    if b"JFIF\x00" not in data:
        data = (
            data[:2]
            + _segment(0xE0, b"JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00")
            + data[2:]
        )
    return data


def _after_app0(data: bytes) -> int:
    """Offset just past the leading APP0 JFIF segment (or SOI)."""
    if data[2] == 0xFF and data[3] == 0xE0:
        return 4 + struct.unpack(">H", data[4:6])[0]
    return 2


def _metadata_segments() -> bytes:
    """EXIF (APP1), C2PA JUMBF (APP11), Ducky (APP12), Photoshop IRB
    (APP13) and a comment: the segments every JPEG test splices in."""
    exif = b"Exif\x00\x00" + b"II*\x00\x08\x00\x00\x00\x00\x00\x00\x00\x00\x00"
    return (
        _segment(0xE1, exif)
        + _segment(0xEB, b"JP\x00\x00\x00\x01\x00\x00\x00\x20jumbc2pa.manifest")
        + _segment(0xEC, b"Ducky\x00\x01\x00\x04\x00\x00\x00\x50")
        + _segment(0xED, b"Photoshop 3.0\x008BIM\x04\x04\x00\x00\x00\x00\x00\x00")
        + _segment(0xFE, b"a free-text comment")
    )


def _icc_segment() -> bytes:
    return _segment(0xE2, b"ICC_PROFILE\x00\x01\x01" + b"\x42" * 48)


def _assert_no_metadata_markers(copy: bytes) -> None:
    for marker in (0xE1, 0xEB, 0xEC, 0xED, 0xFE):
        assert not _has_marker(copy, marker), (
            f"marker FF{marker:02X} survived into the public copy"
        )
    for needle in (
        b"Exif\x00\x00",
        b"jumb",
        b"Ducky",
        b"Photoshop 3.0",
        b"a free-text comment",
    ):
        assert needle not in copy, (
            f"metadata payload {needle!r} survived into the public copy"
        )


def test_jpeg_public_copy_keeps_only_allowlisted_segments():
    clean = _baseline_jpeg()
    at = _after_app0(clean)
    dirty = (
        clean[:at]
        + _icc_segment()
        + _metadata_segments()
        + clean[at:]
        + b"TRAILING-BYTES"
    )

    copy, kind = metadata_free_public_copy(dirty)
    assert kind == "jpeg"
    _assert_no_metadata_markers(copy)
    assert b"JFIF\x00" in copy, "JFIF APP0 must be kept"
    assert b"ICC_PROFILE\x00" in copy, "ICC APP2 must be kept"
    assert copy.count(b"\xff\xda") == clean.count(b"\xff\xda")
    assert copy.endswith(b"\xff\xd9"), "the copy ends at EOI"
    assert b"TRAILING-BYTES" not in copy
    # Same pixels as the clean original.
    assert _pixels(copy) == _pixels(clean)


# A real progressive JPEG (16x16, 10 scans, a DHT before most scans),
# made once with Pillow (quality 90, progressive=True, no EXIF, no ICC)
# and inlined; the same bytes as the Rust build's fixture.
PROGRESSIVE_JPEG_B64 = (
    "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAMCAgMCAgMDAwMEAwMEBQgFBQQEBQoHBwYIDAoMDAsKCwsN"
    "DhIQDQ4RDgsLEBYQERMUFRUVDA8XGBYUGBIUFRT/2wBDAQMEBAUEBQkFBQkUDQsNFBQUFBQUFBQUFBQU"
    "FBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBT/wgARCAAQABADASIAAhEBAxEB/8QA"
    "FQABAQAAAAAAAAAAAAAAAAAABgf/xAAUAQEAAAAAAAAAAAAAAAAAAAAA/9oADAMBAAIQAxAAAAGPvGTw"
    "/8QAFhAAAwAAAAAAAAAAAAAAAAAAAAQF/9oACAEBAAEFAkpAlIEpAlIP/8QAFxEBAAMAAAAAAAAAAAAA"
    "AAAABgAhMf/aAAgBAwEBPwE8hy5//8QAFREBAQAAAAAAAAAAAAAAAAAAAwD/2gAIAQIBAT8BFr//xAAW"
    "EAADAAAAAAAAAAAAAAAAAAAAASH/2gAIAQEABj8CUFBQUP/EABQQAQAAAAAAAAAAAAAAAAAAACD/2gAI"
    "AQEAAT8hCqq//9oADAMBAAIAAwAAABBT/8QAFBEBAAAAAAAAAAAAAAAAAAAAAP/aAAgBAwEBPxB//8QA"
    "FhEAAwAAAAAAAAAAAAAAAAAAACEx/9oACAECAQE/EIM//8QAFRABAQAAAAAAAAAAAAAAAAAAAPH/2gAI"
    "AQEAAT8QgoKCgv/Z"
)


def test_progressive_jpeg_public_copy_drops_metadata_between_scans():
    clean = base64.b64decode(PROGRESSIVE_JPEG_B64)
    scans = clean.count(b"\xff\xda")
    assert scans == 10, "fixture is the 10-scan progressive file"
    # Metadata before the first scan, and again between scans: right
    # before the 2nd and the 5th SOS headers.
    sos_at = [i for i in range(len(clean) - 1) if clean[i : i + 2] == b"\xff\xda"]
    at = _after_app0(clean)
    exif_mm = _segment(0xE1, b"Exif\x00\x00" + b"MM\x00*\x00\x00\x00\x08\x00\x00")
    dirty = (
        clean[:at]
        + _metadata_segments()
        + clean[at : sos_at[1]]
        + _metadata_segments()
        + clean[sos_at[1] : sos_at[4]]
        + exif_mm
        + clean[sos_at[4] :]
        + b"TRAILING-BYTES"
    )

    copy, kind = metadata_free_public_copy(dirty)
    assert kind == "jpeg"
    _assert_no_metadata_markers(copy)
    assert copy.count(b"\xff\xda") == scans, "every scan is kept"
    assert b"JFIF\x00" in copy
    assert b"TRAILING-BYTES" not in copy
    # Nothing but the spliced metadata was removed.
    assert copy == clean
    assert _pixels(copy) == _pixels(clean)


def test_hand_built_two_scan_stream_drops_app1_between_scans():
    """A hand-built two-scan stream (structure only; not meant to
    decode): APP1 and COM between the scans go, the table segment
    between the scans stays, both scans' entropy data (stuffed FF00
    and an RST marker included) move byte for byte, and the bytes
    after EOI are dropped."""
    jfif = _segment(0xE0, b"JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00")
    dqt = _segment(0xDB, bytes(65))
    sof = _segment(0xC0, bytes([8, 0, 8, 0, 8, 1, 1, 0x11, 0]))
    dht1 = _segment(0xC4, bytes([0x00, 1] + [0] * 15 + [0x05]))
    sos = _segment(0xDA, bytes([1, 1, 0x00, 0, 0x3F, 0]))
    scan1 = bytes([0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD0, 0x56])
    app1 = _segment(0xE1, b"Exif\x00\x00" + b"II*\x00\x08\x00\x00\x00")
    com = _segment(0xFE, b"between the scans")
    dht2 = _segment(0xC4, bytes([0x10, 1] + [0] * 15 + [0x01]))
    scan2 = bytes([0x78, 0xFF, 0x00, 0x9A])

    def build(with_metadata: bool) -> bytes:
        out = b"\xff\xd8" + jfif + dqt + sof + dht1 + sos + scan1
        if with_metadata:
            out += app1 + com
        out += dht2 + sos + scan2 + b"\xff\xd9"
        if with_metadata:
            out += b"GARBAGE"
        return out

    copy, _ = metadata_free_public_copy(build(True))
    assert not _has_marker(copy, 0xE1), "APP1 between scans survived"
    assert not _has_marker(copy, 0xFE), "COM between scans survived"
    assert copy == build(False)


def _png_chunk(kind: bytes, payload: bytes) -> bytes:
    """One PNG chunk: length, type, payload, CRC."""
    crc = binascii.crc32(kind + payload) & 0xFFFFFFFF
    return struct.pack(">I", len(payload)) + kind + payload + struct.pack(">I", crc)


def _png_chunk_kinds(data: bytes) -> list[bytes]:
    """PNG chunk kinds in file order."""
    kinds = []
    pos = 8
    while pos + 12 <= len(data):
        length = struct.unpack(">I", data[pos : pos + 4])[0]
        kinds.append(data[pos + 4 : pos + 8])
        pos += 12 + length
    return kinds


def _rgba_pixels(data: bytes) -> tuple[tuple[int, int], bytes]:
    with Image.open(BytesIO(data)) as img:
        img.load()
        rgba = img.convert("RGBA")
        return rgba.size, rgba.tobytes()


def _clean_png(width: int, height: int, color, first=None) -> bytes:
    img = Image.new("RGBA", (width, height), color)
    if first is not None:
        img.putpixel((0, 0), first)
    buf = BytesIO()
    img.save(buf, format="PNG")
    data = buf.getvalue()
    # Signature, then IHDR (25 bytes), then the image data: the
    # splice point below is right after IHDR.
    assert data[12:16] == b"IHDR"
    return data


def test_png_public_copy_keeps_only_allowlisted_chunks():
    clean = _clean_png(6, 4, (10, 20, 30, 255), first=(200, 100, 50, 128))
    gama = _png_chunk(b"gAMA", struct.pack(">I", 45455))
    phys = _png_chunk(b"pHYs", bytes([0, 0, 0x0B, 0x13, 0, 0, 0x0B, 0x13, 1]))
    # Splice metadata chunks after IHDR (8-byte signature + 25-byte
    # IHDR chunk), plus colour chunks that must survive.
    dirty = (
        clean[:33]
        + gama
        + phys
        + _png_chunk(b"caBX", b"\x00\x00\x00\x20jumbc2pa.manifest")
        + _png_chunk(b"eXIf", b"II*\x00\x08\x00\x00\x00\x00\x00")
        + _png_chunk(b"tEXt", b"Comment\x00shot at 45.5N 122.75W")
        + _png_chunk(b"prVt", b"private vendor payload")
        + clean[33:]
        + b"TRAILING-BYTES"
    )

    copy, kind = metadata_free_public_copy(dirty)
    assert kind == "png"
    kinds = _png_chunk_kinds(copy)
    for banned in (b"caBX", b"eXIf", b"tEXt", b"prVt"):
        assert banned not in kinds, (
            f"chunk {banned.decode()} survived into the public copy"
        )
    for needle in (b"jumb", b"45.5N", b"private vendor payload", b"TRAILING-BYTES"):
        assert needle not in copy
    # Allowlisted chunks survive, in order, with their CRCs.
    assert kinds[0] == b"IHDR" and kinds[-1] == b"IEND"
    assert b"gAMA" in kinds and b"pHYs" in kinds and b"IDAT" in kinds
    assert gama in copy, "gAMA moves byte for byte, CRC included"
    assert phys in copy
    # Pixels identical.
    assert _rgba_pixels(copy) == _rgba_pixels(clean)


def test_png_public_copy_keeps_hdr_colour_chunks():
    """The HDR colour chunks (cICP coding-independent code points,
    mDCv mastering display, cLLi content light level) are colour
    rendering, not provenance: they move across byte for byte; the
    metadata carriers still do not."""
    clean = _clean_png(5, 3, (40, 80, 160, 255))
    # BT.2020 primaries, PQ transfer, RGB matrix, full range.
    cicp = _png_chunk(b"cICP", bytes([9, 16, 0, 1]))
    mdcv = _png_chunk(
        b"mDCv",
        struct.pack(">8H", 35400, 14600, 8500, 39850, 6550, 2300, 15635, 16450)
        + struct.pack(">II", 10_000_000, 1),
    )
    clli = _png_chunk(b"cLLi", struct.pack(">II", 10_000_000, 4_000_000))
    dirty = (
        clean[:33]
        + cicp
        + mdcv
        + clli
        + _png_chunk(b"caBX", b"\x00\x00\x00\x20jumbc2pa.manifest")
        + _png_chunk(b"eXIf", b"II*\x00\x08\x00\x00\x00\x00\x00")
        + _png_chunk(b"tEXt", b"Comment\x00shot at 45.5N 122.75W")
        + clean[33:]
    )

    copy, kind = metadata_free_public_copy(dirty)
    assert kind == "png"
    kinds = _png_chunk_kinds(copy)
    for name, chunk in (("cICP", cicp), ("mDCv", mdcv), ("cLLi", clli)):
        assert chunk in copy, (
            f"{name} must move across byte for byte (CRC included); "
            f"chunks: {[k.decode() for k in kinds]}"
        )
    for banned in (b"caBX", b"eXIf", b"tEXt"):
        assert banned not in kinds, (
            f"chunk {banned.decode()} survived into the public copy"
        )
    assert b"45.5N" not in copy and b"jumb" not in copy
    # Order kept: the colour chunks sit after IHDR, before IDAT.
    at = kinds.index
    assert at(b"IHDR") < at(b"cICP") < at(b"mDCv") < at(b"cLLi") < at(b"IDAT")
    assert _rgba_pixels(copy) == _rgba_pixels(clean)
