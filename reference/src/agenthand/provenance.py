"""Provenance signals: what a photo file says about itself.

Mirror of the Rust build's `provenance` module (same block
shape, same wording). Three reads over the original bytes a
photo arrived as:

1. EXIF/XMP observations (the provenance signals core):
   capture device, named editor software, the XMP edit-history
   chain, document identifiers, the two stated dates, and
   Content Credentials (C2PA) presence.
2. A container census: the JPEG segments / PNG chunks / HEIF
   items actually in the file, against what decoding it
   produced (stated vs decoded dimensions, trailing bytes).
3. The location read: GPS coordinates in the file are a
   publishing hazard, so the block carries the coordinates,
   a plain-language warning, and (when the caller mints one)
   a reference to a metadata-free public copy of the same
   pixels (`metadata_free_public_copy`).

HARD WORDING RULE (the whole point of the feature): signals
are observations with an explicit establishes /
does_not_establish pair, never a verdict. This module never
calls a photo edited or unedited, and the absence of a signal
is labelled meaningless in the block's own scope text,
because metadata is written by software, is routinely
stripped by honest tools, and proves nothing either way on
its own. The pixel-level claim stays where it belongs: the
Fidelity Lock receipt.

Reader notes (Python build): EXIF values come from Pillow
(with pillow-heif's opener registered by assets, HEIC reads
work too; a stripped environment without the plugin degrades
HEIC blocks to the container census with metadata_read
false — the Rust build always reads them). Container walks,
XMP, C2PA presence, and the strip are byte-level here, like
the Rust build. The HEIC XMP read is a bounded packet scan
of the file bytes rather than an item-location walk.
"""

from __future__ import annotations

import json
import math
import struct
import zlib
from io import BytesIO

SIGNALS_SCOPE = (
    "Observations about what the file itself contains and claims, read when the "
    "photo arrived. These are not a verdict: this record never calls a photo "
    "edited or unedited. Metadata is written by software and can be edited or "
    "stripped, so the absence of any signal below is not evidence that a file "
    "is untouched."
)

PUBLIC_COPY_LABEL = (
    "Derived public copy: all embedded metadata (including any location "
    "coordinates) was removed; the image pixels are identical to the original "
    "file, which is unchanged and still bound by its receipt."
)

_XMP_HEADER = b"http://ns.adobe.com/xap/1.0/\x00"
_XMP_EXT_HEADER = b"http://ns.adobe.com/xmp/extension/\x00"
_FIELD_CAP = 200
_HISTORY_CAP = 32
_INFLATE_CAP = 1024 * 1024


def round6(x: float) -> float:
    return math.floor(x * 1e6 + 0.5) / 1e6


# ---------------------------------------------------------------------------
# Container walks
# ---------------------------------------------------------------------------


def _detect_format(data: bytes) -> str:
    if data[:2] == b"\xff\xd8":
        return "jpeg"
    if data[:8] == b"\x89PNG\r\n\x1a\n":
        return "png"
    if len(data) >= 12 and data[4:8] == b"ftyp":
        brand = data[8:12]
        if brand in (
            b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx",
            b"hevm", b"hevs", b"mif1", b"mif2", b"msf1", b"avif", b"avis",
        ):
            return "heic"
    return "unknown"


class _Seg:
    __slots__ = ("marker", "start", "span", "payload")

    def __init__(self, marker, start, span, payload):
        self.marker = marker
        self.start = start
        self.span = span
        self.payload = payload


def _jpeg_walk(data: bytes):
    """(segments, has_scan, trailing_bytes_after_eoi) or None."""
    if data[:2] != b"\xff\xd8":
        return None
    segments: list[_Seg] = []
    pos = 2
    has_scan = False
    trailing = 0
    while pos + 4 <= len(data):
        if data[pos] != 0xFF:
            break
        marker = data[pos + 1]
        if marker in (0xD8, 0x01) or 0xD0 <= marker <= 0xD7:
            pos += 2
            continue
        seg_len = struct.unpack(">H", data[pos + 2 : pos + 4])[0]
        if seg_len < 2 or pos + 2 + seg_len > len(data):
            break
        start, end = pos, pos + 2 + seg_len
        segments.append(_Seg(marker, start, (start, end), data[pos + 4 : end]))
        pos = end
        if marker == 0xDA:
            has_scan = True
            scan = pos
            eoi_at = None
            while scan + 1 < len(data):
                if data[scan] == 0xFF:
                    nxt = data[scan + 1]
                    if nxt == 0x00 or 0xD0 <= nxt <= 0xD7:
                        scan += 2
                        continue
                    if nxt == 0xD9:
                        eoi_at = scan
                        break
                    scan += 1
                else:
                    scan += 1
            if eoi_at is not None:
                trailing = len(data) - (eoi_at + 2)
            break
        if marker == 0xD9:
            trailing = len(data) - (pos)
            break
    return segments, has_scan, trailing


def _png_walk(data: bytes):
    """(chunks, trailing) or None; chunk = (kind, span, payload)."""
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        return None
    chunks = []
    pos = 8
    trailing = 0
    while pos + 12 <= len(data):
        clen = struct.unpack(">I", data[pos : pos + 4])[0]
        kind = data[pos + 4 : pos + 8]
        end = pos + 8 + clen + 4
        if end > len(data):
            break
        chunks.append((kind, (pos, end), data[pos + 8 : pos + 8 + clen]))
        pos = end
        if kind == b"IEND":
            trailing = len(data) - pos
            break
    return chunks, trailing


def _top_level_boxes(data: bytes):
    boxes = []
    pos = 0
    while pos + 8 <= len(data):
        size = struct.unpack(">I", data[pos : pos + 4])[0]
        kind = data[pos + 4 : pos + 8]
        header = 8
        if size == 1:
            if pos + 16 > len(data):
                break
            size = struct.unpack(">Q", data[pos + 8 : pos + 16])[0]
            header = 16
        elif size == 0:
            size = len(data) - pos
        if size < header or pos + size > len(data):
            break
        boxes.append((kind, (pos, pos + size), data[pos + header : pos + size]))
        pos += size
    return boxes


# ---------------------------------------------------------------------------
# EXIF via Pillow
# ---------------------------------------------------------------------------


def _text(value) -> str | None:
    if value is None:
        return None
    if isinstance(value, bytes):
        value = value.decode("utf-8", "replace")
    if not isinstance(value, str):
        value = str(value)
    cleaned = value.strip("\x00").strip()
    return cleaned or None


def _dms_to_degrees(parts, ref: str | None) -> float | None:
    try:
        nums = [float(p) for p in parts]
    except (TypeError, ValueError):
        return None
    if len(nums) < 3:
        return None
    value = nums[0] + nums[1] / 60.0 + nums[2] / 3600.0
    if ref and ref.strip("\x00").strip()[:1] in ("S", "W"):
        value = -value
    return round6(value)


class _Exif:
    def __init__(self):
        self.present = False
        self.make = None
        self.model = None
        self.software = None
        self.datetime = None
        self.datetime_original = None
        self.pixel_dims = None
        self.gps = None
        self.has_thumbnail_ifd = False


def _exif_facts(data: bytes) -> _Exif:
    facts = _Exif()
    try:
        # Same guarded registration assets performs at import:
        # keeps this reader correct when used standalone.
        try:
            from pillow_heif import register_heif_opener

            register_heif_opener()
        except ImportError:
            pass
        from PIL import Image

        with Image.open(BytesIO(data)) as img:
            exif = img.getexif()
            if not exif:
                return facts
            facts.present = True
            facts.make = _text(exif.get(0x010F))
            facts.model = _text(exif.get(0x0110))
            facts.software = _text(exif.get(0x0131))
            facts.datetime = _text(exif.get(0x0132))
            try:
                exif_ifd = exif.get_ifd(0x8769)
            except Exception:
                exif_ifd = {}
            facts.datetime_original = _text(exif_ifd.get(0x9003))
            px, py = exif_ifd.get(0xA002), exif_ifd.get(0xA003)
            if isinstance(px, int) and isinstance(py, int) and px > 0 and py > 0:
                facts.pixel_dims = (px, py)
            try:
                gps_ifd = exif.get_ifd(0x8825)
            except Exception:
                gps_ifd = {}
            if gps_ifd:
                lat = _dms_to_degrees(gps_ifd.get(2), _text(gps_ifd.get(1)))
                lon = _dms_to_degrees(gps_ifd.get(4), _text(gps_ifd.get(3)))
                if lat is not None and lon is not None:
                    facts.gps = (lat, lon)
            # Second-IFD (thumbnail) presence from the raw TIFF
            # header chain, when Pillow kept the raw bytes.
            raw = (getattr(img, "info", None) or {}).get("exif")
            if isinstance(raw, (bytes, bytearray)) and len(raw) >= 8:
                facts.has_thumbnail_ifd = _tiff_has_next_ifd(bytes(raw))
    except Exception:
        return _Exif()
    return facts


def _tiff_has_next_ifd(tiff: bytes) -> bool:
    try:
        if len(tiff) < 8:
            return False
        if tiff[:2] == b"II":
            u16 = lambda b: struct.unpack("<H", b)[0]
            u32 = lambda b: struct.unpack("<I", b)[0]
        elif tiff[:2] == b"MM":
            u16 = lambda b: struct.unpack(">H", b)[0]
            u32 = lambda b: struct.unpack(">I", b)[0]
        else:
            return False
        ifd0 = u32(tiff[4:8])
        if ifd0 + 2 > len(tiff):
            return False
        count = u16(tiff[ifd0 : ifd0 + 2])
        nxt = ifd0 + 2 + count * 12
        if nxt + 4 > len(tiff):
            return False
        return u32(tiff[nxt : nxt + 4]) != 0
    except Exception:
        return False


# ---------------------------------------------------------------------------
# XMP (bounded packet scan, same caps as the Rust build)
# ---------------------------------------------------------------------------


def _xml_unescape(s: str) -> str:
    return (
        s.replace("&quot;", '"')
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
    )


def _attr_values(packet: str, name: str) -> list[str]:
    out = []
    pos = 0
    while True:
        i = packet.find(name, pos)
        if i < 0:
            return out
        j = i + len(name)
        while j < len(packet) and packet[j] in " \t":
            j += 1
        if j < len(packet) and packet[j] == "=":
            j += 1
            while j < len(packet) and packet[j] in " \t":
                j += 1
            if j < len(packet) and packet[j] in "\"'":
                quote = packet[j]
                k = packet.find(quote, j + 1)
                if k >= 0:
                    out.append(_xml_unescape(packet[j + 1 : k])[:_FIELD_CAP])
                    pos = k + 1
                    continue
        pos = j


def _elem_values(packet: str, name: str) -> list[str]:
    out = []
    pos = 0
    open_tag = f"<{name}>"
    close_tag = f"</{name}>"
    while True:
        i = packet.find(open_tag, pos)
        if i < 0:
            return out
        start = i + len(open_tag)
        k = packet.find(close_tag, start)
        if k < 0:
            return out
        out.append(_xml_unescape(packet[start:k].strip())[:_FIELD_CAP])
        pos = k + len(close_tag)


def _fields(packet: str, name: str) -> list[str]:
    values = _attr_values(packet, name) + _elem_values(packet, name)
    return [v for v in values if v]


def _xmp_packets(data: bytes, fmt: str) -> list[str]:
    packets: list[str] = []
    if fmt == "jpeg":
        walked = _jpeg_walk(data)
        if walked is None:
            return packets
        segments, _, _ = walked
        ext_total = 0
        for seg in segments:
            if seg.marker != 0xE1:
                continue
            if seg.payload.startswith(_XMP_HEADER):
                packets.append(
                    seg.payload[len(_XMP_HEADER) :].decode("utf-8", "replace")
                )
            elif seg.payload.startswith(_XMP_EXT_HEADER) and len(seg.payload) > 76:
                ext_total += 1
                raw = seg.payload[76:]
                limit = min(len(raw), 65536 * max(1, ext_total))
                packets.append(raw[:limit].decode("utf-8", "replace"))
    elif fmt == "png":
        walked = _png_walk(data)
        if walked is None:
            return packets
        chunks, _ = walked
        for kind, _, payload in chunks:
            if kind not in (b"iTXt", b"tEXt"):
                continue
            nul = payload.find(b"\x00")
            if nul < 0 or payload[:nul] != b"XML:com.adobe.xmp":
                continue
            if kind == b"tEXt":
                text = payload[nul + 1 :]
            else:
                if nul + 3 > len(payload):
                    continue
                flag = payload[nul + 1]
                rest = payload[nul + 3 :]
                l_nul = rest.find(b"\x00")
                if l_nul < 0:
                    continue
                lang_end = l_nul + 1
                t_nul = rest.find(b"\x00", lang_end)
                if t_nul < 0:
                    continue
                text = rest[t_nul + 1 :]
                if flag == 1:
                    try:
                        text = zlib.decompress(text)[:_INFLATE_CAP]
                    except Exception:
                        continue
            packets.append(text.decode("utf-8", "replace"))
    elif fmt == "heic":
        # Bounded packet scan of the file bytes (the mime item
        # carrying XMP is small and near the meta box).
        for marker in (b"<x:xmpmeta", b"<?xpacket"):
            at = data.find(marker)
            if at >= 0:
                end = min(len(data), at + 1024 * 1024)
                packets.append(data[at:end].decode("utf-8", "replace"))
                break
    return packets


class _Xmp:
    def __init__(self):
        self.present = False
        self.creator_tool = None
        self.document_id = None
        self.instance_id = None
        self.history: list[dict] = []


def _xmp_facts(data: bytes, fmt: str) -> _Xmp:
    facts = _Xmp()
    for packet in _xmp_packets(data, fmt):
        if not facts.present:
            facts.present = True
            creator = _fields(packet, "xmp:CreatorTool")
            facts.creator_tool = creator[0] if creator else None
            doc = _fields(packet, "xmpMM:DocumentID")
            facts.document_id = doc[0] if doc else None
            inst = _fields(packet, "xmpMM:InstanceID")
            facts.instance_id = inst[0] if inst else None
        if not facts.history:
            actions = _attr_values(packet, "stEvt:action") + _elem_values(
                packet, "stEvt:action"
            )
            whens = _attr_values(packet, "stEvt:when") + _elem_values(
                packet, "stEvt:when"
            )
            agents = _attr_values(packet, "stEvt:softwareAgent") + _elem_values(
                packet, "stEvt:softwareAgent"
            )
            for i, action in enumerate(actions[:_HISTORY_CAP]):
                facts.history.append(
                    {
                        "action": action,
                        "when": whens[i] if i < len(whens) else None,
                        "software_agent": agents[i] if i < len(agents) else None,
                    }
                )
    return facts


# ---------------------------------------------------------------------------
# C2PA presence (signature chain never inspected)
# ---------------------------------------------------------------------------


def _c2pa_present(data: bytes, fmt: str) -> bool:
    if fmt == "jpeg":
        walked = _jpeg_walk(data)
        if walked is None:
            return False
        segments, _, _ = walked
        for seg in segments:
            if seg.marker == 0xEB:  # APP11
                window = seg.payload[: 4 + 1024]
                if b"c2pa" in window:
                    return True
        return False
    if fmt == "png":
        walked = _png_walk(data)
        if walked is None:
            return False
        chunks, _ = walked
        return any(kind == b"caBX" for kind, _, _ in chunks)
    if fmt == "heic":
        for kind, _, payload in _top_level_boxes(data):
            if kind == b"jumb":
                if b"c2pa" in payload[: 64 * 1024]:
                    return True
            elif kind == b"uuid":
                if b"c2pa" in payload[: 64 * 1024]:
                    return True
        return False
    return False


# ---------------------------------------------------------------------------
# Container census
# ---------------------------------------------------------------------------


def _census_jpeg(data: bytes):
    walked = _jpeg_walk(data)
    if walked is None:
        return None, 0
    segments, has_scan, trailing = walked
    if not segments:
        return None, 0
    counts: dict[str, int] = {}
    dims = None
    for seg in segments:
        key = f"FF{seg.marker:02X}"
        counts[key] = counts.get(key, 0) + 1
        if seg.marker in (0xC0, 0xC1, 0xC2) and len(seg.payload) >= 5:
            height = struct.unpack(">H", seg.payload[1:3])[0]
            width = struct.unpack(">H", seg.payload[3:5])[0]
            dims = (width, height)
    return {
        "container": "jpeg",
        "segments": counts,
        "stated_dimensions": list(dims) if dims else None,
        "has_exif": any(s.marker == 0xE1 and s.payload.startswith(b"Exif\x00\x00") for s in segments),
        "has_xmp": any(s.marker == 0xE1 and s.payload.startswith(_XMP_HEADER) for s in segments),
    }, trailing


def _census_png(data: bytes):
    walked = _png_walk(data)
    if walked is None:
        return None, 0
    chunks, trailing = walked
    if not chunks:
        return None, 0
    counts: dict[str, int] = {}
    dims = None
    for kind, _, payload in chunks:
        key = kind.decode("ascii", "replace")
        counts[key] = counts.get(key, 0) + 1
        if kind == b"IHDR" and len(payload) >= 8:
            dims = struct.unpack(">II", payload[:8])
    kinds = {kind for kind, _, _ in chunks}
    return {
        "container": "png",
        "chunks": counts,
        "stated_dimensions": list(dims) if dims else None,
        "has_exif": b"eXIf" in kinds,
        "has_xmp": any(
            kind in (b"iTXt", b"tEXt")
            and payload.split(b"\x00", 1)[0] == b"XML:com.adobe.xmp"
            for kind, _, payload in chunks
        ),
    }, trailing


def _box_children(payload: bytes, full_box: bool = False):
    """Children of a container box payload (meta is a full box)."""
    start = 4 if full_box else 0
    children = []
    pos = start
    while pos + 8 <= len(payload):
        size = struct.unpack(">I", payload[pos : pos + 4])[0]
        kind = payload[pos + 4 : pos + 8]
        header = 8
        if size == 1:
            if pos + 16 > len(payload):
                break
            size = struct.unpack(">Q", payload[pos + 8 : pos + 16])[0]
            header = 16
        elif size == 0:
            size = len(payload) - pos
        if size < header or pos + size > len(payload):
            break
        children.append((kind, payload[pos + header : pos + size]))
        pos += size
    return children


def _children_from(payload: bytes, start: int):
    """Box children of `payload` beginning at byte `start`."""
    children = []
    pos = start
    while pos + 8 <= len(payload):
        size = struct.unpack(">I", payload[pos : pos + 4])[0]
        kind = payload[pos + 4 : pos + 8]
        header = 8
        if size == 1:
            if pos + 16 > len(payload):
                break
            size = struct.unpack(">Q", payload[pos + 8 : pos + 16])[0]
            header = 16
        elif size == 0:
            size = len(payload) - pos
        if size < header or pos + size > len(payload):
            break
        children.append((kind, payload[pos + header : pos + size]))
        pos += size
    return children


def _infe_items(iinf: bytes) -> dict[int, dict]:
    """Item id -> {type, hidden} from an iinf box payload."""
    items: dict[int, dict] = {}
    if len(iinf) < 6:
        return items
    version = iinf[0]
    body_at = 4 + (2 if version == 0 else 4)  # full-box header + count
    for kind, payload in _children_from(iinf, body_at):
        if kind != b"infe" or len(payload) < 6:
            continue
        iver = payload[0]
        hidden = bool(payload[3] & 1) if len(payload) > 3 else False
        item_id = struct.unpack(">H", payload[4:6])[0]
        if iver >= 2 and len(payload) >= 12:
            itype = payload[8:12].decode("ascii", "replace")
        else:
            itype = "unknown"
        items[item_id] = {"type": itype, "hidden": hidden}
    return items


def _ipma_ispe(ipma: bytes, item_id: int, ipco_props):
    """The ispe (width, height) associated with `item_id`, or None.

    `ipco_props` is the ordered ipco property list of
    (kind, dims-or-None); association indices are 1-based
    positions in that list.
    """
    try:
        if len(ipma) < 7:
            return None
        version = ipma[0]
        flags = int.from_bytes(ipma[1:4], "big")
        pos = 4
        # entry_count is 32-bit in the current spec and in
        # Apple's writer regardless of the legacy flags bit.
        entry_count = struct.unpack(">I", ipma[pos : pos + 4])[0]
        pos += 4
        wide_index = bool(flags & 2)
        for _ in range(entry_count):
            if version == 1:
                iid = struct.unpack(">I", ipma[pos : pos + 4])[0]
                pos += 4
            else:
                iid = struct.unpack(">H", ipma[pos : pos + 2])[0]
                pos += 2
            assoc_count = ipma[pos]
            pos += 1
            found = None
            for _ in range(assoc_count):
                if wide_index:
                    index = ((ipma[pos] & 0x7F) << 8) | ipma[pos + 1]
                    pos += 2
                else:
                    index = ipma[pos] & 0x7F
                    pos += 1
                if iid == item_id and 1 <= index <= len(ipco_props):
                    kind, dims = ipco_props[index - 1]
                    if kind == b"ispe" and dims is not None:
                        found = dims
            if iid == item_id:
                return found
    except Exception:
        return None
    return None


def _census_heic(data: bytes):
    census: dict = {"container": "heic"}
    boxes = _top_level_boxes(data)
    if not boxes:
        return None, 0
    ftyp = next((p for k, _, p in boxes if k == b"ftyp"), None)
    if ftyp and len(ftyp) >= 4:
        census["major_brand"] = ftyp[:4].decode("ascii", "replace")
    meta = next((p for k, _, p in boxes if k == b"meta"), None)
    if meta is None:
        return census, 0
    children = {}
    for kind, payload in _box_children(meta, full_box=True):
        children.setdefault(kind, payload)

    items: dict[int, dict] = {}
    iinf = children.get(b"iinf")
    if iinf is not None and len(iinf) >= 6:
        version = iinf[0]
        if version == 0:
            census["items_total"] = struct.unpack(">H", iinf[4:6])[0]
        elif len(iinf) >= 8:
            census["items_total"] = struct.unpack(">I", iinf[4:8])[0]
        items = _infe_items(iinf)
    if items:
        by_type: dict[str, int] = {}
        for item in items.values():
            by_type[item["type"]] = by_type.get(item["type"], 0) + 1
        census["items_by_type"] = by_type
        census["hidden_items"] = sum(1 for i in items.values() if i["hidden"])
        census.setdefault("items_total", len(items))

    primary_id = None
    pitm = children.get(b"pitm")
    if pitm is not None and len(pitm) >= 6:
        if pitm[0] == 1 and len(pitm) >= 8:
            primary_id = struct.unpack(">I", pitm[4:8])[0]
        else:
            primary_id = struct.unpack(">H", pitm[4:6])[0]

    # ipma property indices address the whole ipco property
    # list in order, so keep every property's kind (and the
    # ispe dimensions where the property is one).
    ipco_props: list[tuple[bytes, tuple[int, int] | None]] = []
    aux_types: list[str] = []
    ipma = None
    iprp = children.get(b"iprp")
    if iprp is not None:
        for kind, payload in _box_children(iprp):
            if kind == b"ipco":
                for pkind, ppayload in _box_children(payload):
                    dims = None
                    if pkind == b"ispe" and len(ppayload) >= 12:
                        dims = struct.unpack(">II", ppayload[4:12])
                    ipco_props.append((pkind, dims))
                    if pkind == b"auxC" and len(ppayload) >= 5:
                        urn = ppayload[4:].split(b"\x00", 1)[0].decode(
                            "ascii", "replace"
                        )
                        if urn and urn not in aux_types:
                            aux_types.append(urn)
            elif kind == b"ipma":
                ipma = payload
    if primary_id is not None:
        if primary_id in items:
            census["primary_item_type"] = items[primary_id]["type"]
        if ipma is not None:
            dims = _ipma_ispe(ipma, primary_id, ipco_props)
            if dims:
                census["stated_dimensions"] = list(dims)
    if aux_types:
        census["aux_item_types"] = aux_types

    iref = children.get(b"iref")
    thumb = False
    if iref is not None:
        tiles = 0
        for kind, payload in _children_from(iref, 4):
            if len(payload) < 4:
                continue
            from_id = struct.unpack(">H", payload[:2])[0]
            ref_count = struct.unpack(">H", payload[2:4])[0]
            if kind == b"dimg" and (primary_id is None or from_id == primary_id):
                tiles += ref_count
            if kind == b"thmb" and ref_count > 0:
                thumb = True
        if tiles:
            census["image_tiles"] = tiles
    census["has_embedded_thumbnail"] = thumb
    census["depth_item_present"] = any("depth" in urn.lower() for urn in aux_types)
    return census, 0


def _census(data: bytes, fmt: str, exif: _Exif, xmp: _Xmp):
    if fmt == "jpeg":
        census, trailing = _census_jpeg(data)
    elif fmt == "png":
        census, trailing = _census_png(data)
    elif fmt == "heic":
        census, trailing = _census_heic(data)
        if census is not None:
            census["has_exif"] = exif.present or census.get("has_exif", False)
            census["has_xmp"] = xmp.present or census.get("has_xmp", False)
            census.setdefault("has_embedded_thumbnail", False)
            if exif.has_thumbnail_ifd:
                census["has_embedded_thumbnail"] = True
    else:
        return None, 0
    if census is None:
        return None, 0
    if fmt in ("jpeg", "png"):
        census["has_embedded_thumbnail"] = exif.has_thumbnail_ifd
    return census, trailing


# ---------------------------------------------------------------------------
# The signals block
# ---------------------------------------------------------------------------


def _signal(signal, finding, detail, establishes, does_not_establish, extra=None):
    entry = {
        "signal": signal,
        "finding": finding,
        "detail": detail,
        "establishes": establishes,
        "does_not_establish": does_not_establish,
    }
    if extra:
        entry.update(extra)
    return entry


def _plausible_datetime(s: str) -> bool:
    parts = s.replace("T", " ").split(" ")
    date = parts[0].replace(":", "-").replace("/", "-").split("-")
    if len(date) != 3:
        return False
    try:
        year, month, day = (int(p) for p in date)
    except ValueError:
        return False
    return 1990 <= year <= 2100 and 1 <= month <= 12 and 1 <= day <= 31


def provenance_block(data: bytes, decoded_dims=None) -> dict:
    """The provenance signals block for one photo file's bytes.

    `decoded_dims` is the (width, height) the file actually
    decoded to, when the caller decoded it; the dimensions
    signal compares the file's stated size against it.
    """
    fmt = _detect_format(data)
    exif = _exif_facts(data) if fmt != "unknown" else _Exif()
    xmp = _xmp_facts(data, fmt)
    census, trailing = _census(data, fmt, exif, xmp)
    c2pa = _c2pa_present(data, fmt)

    signals: list[dict] = []

    version_like = (
        lambda s: bool(s)
        and all(c.isdigit() or c == "." for c in s)
        and any(c.isdigit() for c in s)
    )
    capture_os_version = (
        exif.software if exif.software and version_like(exif.software) else None
    )

    # Capture device.
    if exif.make is None and exif.model is None:
        extra = (
            {"capture_software_version": capture_os_version}
            if capture_os_version
            else None
        )
        signals.append(
            _signal(
                "capture_device",
                "not_stated",
                "the file names no capture device in its metadata",
                "nothing; most files that pass through editing or messaging tools lose these tags",
                "a missing camera name is not evidence about the photo either way",
                extra,
            )
        )
    else:
        named = " ".join(p for p in (exif.make, exif.model) if p)
        signals.append(
            _signal(
                "capture_device",
                "stated",
                f"the file names its capture device as {named}",
                "the file's own statement of what captured it",
                "metadata is written by software and can be edited or stripped, so a named device does not prove the photo came from that device",
                {
                    "make": exif.make,
                    "model": exif.model,
                    "capture_software_version": capture_os_version,
                },
            )
        )

    # Editor software.
    editors: list[str] = []
    if exif.software and not version_like(exif.software):
        editors.append(exif.software)
    if xmp.creator_tool and xmp.creator_tool not in editors:
        editors.append(xmp.creator_tool)
    if editors:
        signals.append(
            _signal(
                "editor_software",
                "named",
                "the file's metadata names this software: " + ", ".join(editors),
                "an application in the file's own account of its past touched this file at some point",
                "naming software does not say what it did: a crop, a resize, a colour pass and a composite all leave the same kind of tag, and the tag says nothing about the product pixels this receipt pins",
                {"named": editors},
            )
        )
    else:
        signals.append(
            _signal(
                "editor_software",
                "none_named",
                "no editing software is named anywhere in the file's metadata",
                "nothing; this is the expected state of an untouched camera file",
                "silence is not cleanliness: an edited file whose metadata was stripped reads exactly like this, so this line can never clear a file",
            )
        )

    # Edit history.
    if xmp.history:
        shown = xmp.history[:8]
        detail = "; ".join(
            " ".join(p for p in (e["action"], e["when"] or "", e["software_agent"] or "") if p)
            for e in shown
        )
        if len(xmp.history) > 8:
            detail += f"; and {len(xmp.history) - 8} more recorded steps"
        signals.append(
            _signal(
                "edit_history",
                "recorded",
                f"the file records {len(xmp.history)} step(s) of edit history: {detail}",
                "the file's own recorded account of steps software says it performed, in the order recorded",
                "a history is written by the tools themselves, can be edited or removed, and is not a complete account: steps by tools that record none are invisible here",
                {"entries": xmp.history},
            )
        )
    else:
        signals.append(
            _signal(
                "edit_history",
                "none_recorded",
                "no edit history is recorded in the file",
                "nothing; most camera files carry no history packet at all",
                "an edit history appears only when Adobe-family tools wrote one and the packet survived; its absence says nothing about edits by other tools or after a strip",
            )
        )

    # Document identifiers.
    if xmp.document_id or xmp.instance_id:
        signals.append(
            _signal(
                "document_ids",
                "present",
                "the file carries XMP document identifiers, so copies derived from it can be recognised as related",
                "this file belongs to a document lineage the creating software tracked",
                "identifiers are copied and rewritten by software routinely; they identify a document family, not a person, and say nothing about pixel content",
                {"document_id": xmp.document_id, "instance_id": xmp.instance_id},
            )
        )
    else:
        signals.append(
            _signal(
                "document_ids",
                "absent",
                "no XMP document identifiers are present",
                "nothing",
                "identifiers speak only to whether copies can be recognised as related; their absence is the normal state of camera files",
            )
        )

    # Date consistency.
    d_orig, d_mod = exif.datetime_original, exif.datetime
    if d_orig and d_mod:
        if d_orig == d_mod:
            finding, detail, est = (
                "agree",
                f"the file's modification date and stated capture time agree ({d_orig})",
                "the file's two dates tell one consistent story",
            )
        elif _plausible_datetime(d_orig) and _plausible_datetime(d_mod) and d_mod > d_orig:
            finding, detail, est = (
                "modified_after_capture",
                f"the file's stated modification time ({d_mod}) is later than its stated capture time ({d_orig})",
                "the file was written again after the moment it states it was captured, by something",
            )
        elif _plausible_datetime(d_orig) and _plausible_datetime(d_mod):
            finding, detail, est = (
                "out_of_order",
                f"the file's stated modification time ({d_mod}) is earlier than its stated capture time ({d_orig})",
                "the file's two dates disagree in a way ordinary saving does not produce",
            )
        else:
            finding, detail, est = (
                "single_date_stated",
                f"the file states one usable date (capture {d_orig}, modified {d_mod})",
                "one date the file states about itself",
            )
        signals.append(
            _signal(
                "date_consistency",
                finding,
                detail,
                est,
                "this is the weakest line in this block: exports, messaging apps and cloud sync rewrite the modification date innocently, and agreement is easy to preserve or to fake, so this line can never clear or convict a file on its own",
            )
        )
    elif d_orig or d_mod:
        signals.append(
            _signal(
                "date_consistency",
                "single_date_stated",
                f"the file states one date (capture {d_orig}, modified {d_mod})".replace(
                    "None", "not stated"
                ),
                "one date the file states about itself",
                "a single date cannot be checked against anything, and dates are written by software clocks that can be wrong or set by hand",
            )
        )
    else:
        signals.append(
            _signal(
                "date_consistency",
                "no_dates_stated",
                "the file states no capture or modification dates",
                "nothing",
                "missing dates are the normal result of metadata stripping by honest tools as much as by anyone else",
            )
        )

    # Content credentials.
    if c2pa:
        signals.append(
            _signal(
                "content_credentials",
                "present",
                "a Content Credentials (C2PA) manifest is embedded in the file",
                "some signer in the credential chain asserted facts about this file's origin at signing time",
                "this read reports presence only: the signature was not checked against a trust list here, and a credential describes the file it was issued for, not the pixels this receipt pins",
            )
        )
    else:
        signals.append(
            _signal(
                "content_credentials",
                "absent",
                "no Content Credentials (C2PA) manifest is embedded. Most cameras add none, and no native iPhone photo carries one, so this absence means nothing",
                "nothing; absence of credentials is the normal state of most honest photos",
                "absence must never be read as suspicion: the large majority of unedited photos on earth carry no credential",
            )
        )

    # Dimensions consistency.
    stated = exif.pixel_dims
    if stated is None and census is not None:
        cd = census.get("stated_dimensions")
        if cd:
            stated = (cd[0], cd[1])
    if stated is not None and decoded_dims is not None:
        sw, sh = stated
        dw, dh = decoded_dims
        if (sw, sh) == (dw, dh) or (sw, sh) == (dh, dw):
            signals.append(
                _signal(
                    "dimensions_consistency",
                    "consistent",
                    f"the metadata's stated size ({sw}x{sh}) matches the decoded image",
                    "the file's stated size and its actual pixels describe the same frame",
                    "a consistent size says nothing about what happened inside the frame",
                )
            )
        else:
            signals.append(
                _signal(
                    "dimensions_consistency",
                    "mismatch",
                    f"the metadata states {sw}x{sh} but the decoded image is {dw}x{dh}",
                    "the file's stated dimensions disagree with the image it actually contains, which happens when a file was re-encoded or re-framed after its metadata was written",
                    "a mismatch does not show the picture itself was altered; editors and exporters rewrite dimensions routinely",
                )
            )
    else:
        signals.append(
            _signal(
                "dimensions_consistency",
                "not_stated",
                "the file's metadata states no pixel dimensions to check against the decoded image",
                "nothing",
                "most stripped files state no dimensions; the receipt's own pixel counts are measured from the decoded image, never from metadata",
            )
        )

    # Trailing data.
    if trailing > 0:
        signals.append(
            _signal(
                "trailing_data",
                "present",
                f"{trailing} byte(s) follow the end of the image data",
                "something was appended after the image proper; some tools append thumbnails or their own bookkeeping here",
                "appended bytes are not part of the image and do not change a single pixel; honest tools append data too",
                {"bytes": trailing},
            )
        )
    else:
        signals.append(
            _signal(
                "trailing_data",
                "none",
                "no bytes follow the end of the image data",
                "the file ends where its structure says it ends",
                "a clean ending says nothing about the image data itself",
            )
        )

    # Location.
    if exif.gps is not None:
        lat, lon = exif.gps
        warning = (
            f"This photo's file carries GPS location coordinates (latitude {lat}, "
            f"longitude {lon}), readable by anyone who receives the original file. "
            "Before the original is published anywhere public, for example on the "
            "original-photo page an AB 723 listing links to, publish the "
            "metadata-free public copy instead: it keeps every pixel and removes "
            "the location and the rest of the embedded metadata. The original "
            "file and its receipt are unchanged."
        )
        location = {
            "present": True,
            "latitude": lat,
            "longitude": lon,
            "warning": warning,
            "public_copy": None,
        }
    else:
        warning = None
        location = {
            "present": False,
            "latitude": None,
            "longitude": None,
            "warning": None,
            "public_copy": None,
        }

    return {
        "version": 1,
        "format": fmt,
        "metadata_read": bool(census is not None or exif.present or xmp.present),
        "scope": SIGNALS_SCOPE,
        "signals": signals,
        "census": census,
        "location": location,
        "location_warning": warning,
    }


def set_public_copy(block: dict, info: dict) -> None:
    block["location"]["public_copy"] = info


def set_public_copy_unavailable(block: dict, reason: str) -> None:
    block["location"]["public_copy"] = {
        "unavailable": (
            f"no metadata-free copy could be produced from this file ({reason}); "
            "do not publish the original file itself until a copy exists"
        )
    }


# ---------------------------------------------------------------------------
# PNG text injection + store-facing helpers
# ---------------------------------------------------------------------------

import binascii
import hashlib


def inject_png_text(data: bytes, key: str, value: str):
    """PNG bytes with a tEXt chunk inserted before IEND (an
    existing chunk with the same key is replaced). Pixels are
    untouched; returns None when `data` is not a PNG. The
    carrier the provenance block rides inside stored sources,
    mirroring the Rust build's inject_png_text."""
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        return None
    latin = key.encode("latin-1") + b"\x00" + value.encode("latin-1", "replace")
    chunk = struct.pack(">I", len(latin)) + b"tEXt" + latin
    chunk += struct.pack(">I", binascii.crc32(b"tEXt" + latin) & 0xFFFFFFFF)
    walked = _png_walk(data)
    if walked is None:
        return None
    chunks, _ = walked
    out = bytearray(data[:8])
    inserted = False
    for kind, span, payload in chunks:
        if kind == b"tEXt" and payload.split(b"\x00", 1)[0] == key.encode("latin-1"):
            continue  # replaced by the fresh chunk below
        if kind == b"IEND" and not inserted:
            out += chunk
            inserted = True
        out += data[span[0] : span[1]]
    if not inserted:
        return None
    return bytes(out)


def mint_public_copy(store, data: bytes, parent_asset_id: str | None = None) -> dict:
    """Mint the metadata-free public copy of `data` into the
    store and build the reference the block carries: the copy's
    handle, its file and pixel hashes, its format, and the
    label that must travel with it. The pixel hash is recomputed
    from the copy's own decoded pixels, so "pixels identical"
    is a checked equality, not a promise."""
    from .assets import KIND_PUBLIC_COPY, pixel_hash_of_image

    copy_bytes, copy_format = metadata_free_public_copy(data)
    suffix = ".jpg" if copy_format == "jpeg" else ".png"
    from PIL import Image

    with Image.open(BytesIO(copy_bytes)) as img:
        img.load()
        pixel_sha256 = pixel_hash_of_image(img)
    handle = store.put_bytes(
        copy_bytes,
        kind=KIND_PUBLIC_COPY,
        suffix=suffix,
        source_asset_id=parent_asset_id,
    )
    return {
        "asset": handle,
        "sha256": hashlib.sha256(copy_bytes).hexdigest(),
        "pixel_sha256": pixel_sha256,
        "format": copy_format,
        "label": PUBLIC_COPY_LABEL,
    }


def attach_public_copy(block: dict, store, data: bytes, parent_asset_id=None) -> None:
    """When a block reports location coordinates, mint its
    public copy and patch the reference in (or record honestly
    why no copy could be produced). No-op otherwise."""
    if block.get("location", {}).get("present") is not True:
        return
    try:
        set_public_copy(block, mint_public_copy(store, data, parent_asset_id))
    except Exception as exc:  # noqa: BLE001 — recorded in the block
        set_public_copy_unavailable(block, str(exc))


# ---------------------------------------------------------------------------
# The metadata-free public copy. JPEG and PNG copies are pure container
# surgery: the image data moves byte for byte, and only the segments /
# chunks on an explicit ALLOWLIST (decode and colour rendering needs)
# are kept; everything else, known metadata carrier or not, is dropped
# (fix pass F1/F2, Rust parity: the old denylist let C2PA, Photoshop
# IRB, private chunks and inter-scan APPn through). HEIC has no
# byte-level surgery available, so its copy is a lossless decode
# re-encoded as PNG with no metadata at all.
# ---------------------------------------------------------------------------


# The PNG chunks a public copy keeps: image data and the colour /
# display chunks needed to render it as the original renders,
# including the HDR colour chunks (cICP coding-independent code
# points, mDCv mastering display, cLLi content light level: colour
# rendering, not provenance). An ALLOWLIST: every other chunk (C2PA
# caBX, eXIf, the tEXt / zTXt / iTXt text and XMP carriers, tIME, and
# any ancillary or private chunk we do not recognise) is dropped.
# Same list, same order as the Rust build's PNG_PUBLIC_COPY_CHUNKS.
PNG_PUBLIC_COPY_CHUNKS = (
    b"IHDR", b"PLTE", b"tRNS", b"IDAT", b"IEND", b"gAMA", b"cHRM", b"sRGB",
    b"iCCP", b"sBIT", b"bKGD", b"pHYs", b"cICP", b"mDCv", b"cLLi",
)


def _strip_png(data: bytes) -> bytes | None:
    """Strip all metadata from a PNG: only allowlisted chunks
    (PNG_PUBLIC_COPY_CHUNKS) move across, byte for byte (CRCs
    included) and in their original order; the walk ends at IEND, so
    trailing bytes are dropped. None when `data` is not a readable
    PNG. Mirrors the Rust strip_png."""
    walked = _png_walk(data)
    if walked is None:
        return None
    chunks, _ = walked
    out = bytearray(data[:8])
    for kind, span, _ in chunks:
        if kind in PNG_PUBLIC_COPY_CHUNKS:
            out += data[span[0] : span[1]]
    return bytes(out)


def _jpeg_segment_allowed(marker: int, payload: bytes, after_first_scan: bool) -> bool:
    """Is a JPEG marker segment allowed in the public copy? An
    ALLOWLIST: anything not named here (every other APPn, COM, and
    any marker we do not recognise) is dropped.

    Before the first scan the copy keeps only what decoding and
    colour rendering need: quantisation and Huffman tables (DQT,
    DHT), arithmetic-coding conditioning (DAC), restart interval
    (DRI), the frame header (SOFn, 0xC0-0xCF minus the DHT / JPG /
    DAC codes), JFIF/JFXX in APP0, the ICC profile in APP2 (colour
    rendering, not provenance) and the Adobe colour-transform flag
    in APP14. Between scans (progressive and multi-scan files) only
    table segments (DHT, DQT, DAC, DRI, DNL) survive: every APPn and
    COM there is dropped too. Mirrors the Rust jpeg_segment_allowed.
    """
    if marker in (0xDB, 0xC4, 0xCC, 0xDD):  # DQT, DHT, DAC, DRI
        return True
    if marker == 0xDC:  # DNL only follows a scan
        return after_first_scan
    if after_first_scan:
        return False
    if 0xC0 <= marker <= 0xCF:
        # SOFn (0xC4 / 0xCC matched above; 0xC8 is JPG, reserved).
        return marker != 0xC8
    if marker == 0xE0:
        return payload.startswith(b"JFIF\x00") or payload.startswith(b"JFXX\x00")
    if marker == 0xE2:
        return payload.startswith(b"ICC_PROFILE\x00")
    if marker == 0xEE:
        return payload.startswith(b"Adobe")
    return False


def _strip_jpeg(data: bytes) -> bytes:
    """The JPEG public copy: an allowlist walk over the whole stream,
    scans included. Segments are kept or dropped by
    _jpeg_segment_allowed; each scan's entropy-coded data moves byte
    for byte (0xFF00 stuffing and RST0-7 are data, not markers); the
    walk ends at EOI, which is emitted, and anything after it
    (trailing data) is dropped. A stream that ends inside scan data
    keeps the data it has and gains an EOI. Raises ValueError when
    the stream cannot be walked. Mirrors the Rust strip_jpeg.
    """
    n = len(data)
    out = bytearray(b"\xff\xd8")
    pos = 2
    after_first_scan = False
    while True:
        if pos >= n:
            # Ran out without an EOI: close the stream.
            if not after_first_scan:
                raise ValueError("JPEG has no SOS segment")
            out += b"\xff\xd9"
            return bytes(out)
        if data[pos] != 0xFF:
            raise ValueError(f"JPEG marker expected at byte {pos}")
        # Fill bytes: any run of 0xFF before the marker code.
        while pos + 1 < n and data[pos + 1] == 0xFF:
            pos += 1
        if pos + 1 >= n:
            pos = n
            continue
        marker = data[pos + 1]
        if marker == 0xD9:
            if not after_first_scan:
                raise ValueError("JPEG has no SOS segment")
            out += b"\xff\xd9"
            return bytes(out)
        if marker == 0x01 or 0xD0 <= marker <= 0xD8:
            # Standalone markers outside scan data (TEM, a stray RSTn,
            # a repeated SOI): no payload, not needed, dropped.
            pos += 2
            continue
        if pos + 4 > n:
            raise ValueError("JPEG segment header is truncated")
        seg_len = struct.unpack(">H", data[pos + 2 : pos + 4])[0]
        if seg_len < 2 or pos + 2 + seg_len > n:
            raise ValueError("JPEG segment runs past the end of the file")
        seg_end = pos + 2 + seg_len
        if marker == 0xDA:
            # SOS header, then its entropy-coded data up to the next
            # real marker.
            out += data[pos:seg_end]
            i = seg_end
            while i < n:
                if data[i] == 0xFF:
                    if i + 1 >= n:
                        break
                    nxt = data[i + 1]
                    if nxt == 0x00 or 0xD0 <= nxt <= 0xD7:
                        i += 2
                        continue
                    break
                i += 1
            out += data[seg_end:i]
            after_first_scan = True
            pos = i
            continue
        if _jpeg_segment_allowed(marker, data[pos + 4 : seg_end], after_first_scan):
            out += data[pos:seg_end]
        pos = seg_end


def metadata_free_public_copy(data: bytes):
    """(copy_bytes, format) with every embedded metadata carrier
    removed and the image pixels untouched: JPEG and PNG keep only
    allowlisted segments / chunks (see the section comment above).
    Raises ValueError when no copy can be produced. Mirrors the Rust
    build."""
    fmt = _detect_format(data)
    if fmt == "jpeg":
        walked = _jpeg_walk(data)
        if walked is None:
            raise ValueError("not a readable JPEG")
        _, has_scan, _ = walked
        if not has_scan:
            raise ValueError("JPEG carries no image scan to preserve")
        return _strip_jpeg(data), "jpeg"
    if fmt == "png":
        stripped = _strip_png(data)
        if stripped is None:
            raise ValueError("not a readable PNG")
        return stripped, "png"
    # HEIC and everything else: pixel-identical PNG transcode.
    try:
        from PIL import Image

        with Image.open(BytesIO(data)) as img:
            img.load()
            rgba = img.convert("RGBA")
            buffer = BytesIO()
            rgba.save(buffer, format="PNG")
            return buffer.getvalue(), "png"
    except Exception as exc:
        raise ValueError(f"file does not decode for a public copy: {exc}") from exc
