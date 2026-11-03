"""The asset store: images live on disk, tools pass handles.

Every image agenthand touches is stored as a file under the data dir and
referenced everywhere else as ``asset://<id>``. Each asset has a sidecar
JSON record (``<id>.json``, written via temp file + os.replace so a crash
mid-write cannot corrupt the store) next to its data file. A legacy
``assets.json`` index from the first scaffold is still READ as a fallback
so old data dirs keep working; new writes are sidecars only.

Two hashes are recorded per asset:

- ``sha256``: the hash of the stored file bytes.
- ``pixel_sha256``: the hash of the decoded RGBA pixel bytes, recorded
  when the asset decodes as an image. File bytes can change without the
  pixels changing (re-encoding) and pixels can change under the same file
  hash's blind spots (metadata-driven decode differences), so the
  Fidelity Lock checks both.

A third identity is recorded when the image carries one:
``color_profile_sha256``, the hash of the embedded ICC colour profile.
Pixel values alone do not determine displayed colour — a Display P3
photo whose profile is dropped renders differently in every viewer
while every pixel hash still matches (the J2 Gauntlet's strongest
attack). The pipeline therefore preserves the source's profile end to
end and records its identity on every asset, so a profile change is
detectable and counts as a fidelity failure in the draw receipt.

Lineage: each record names the asset it was derived from
(``source_asset_id``), forming a chain back to a source photo. The draw
tool refuses a foreground whose chain is anything other than
foreground -> source_photo or foreground -> mask -> source_photo: a
generated asset anywhere in the ancestry would put model pixels
inside the layer the Fidelity Lock protects.
"""

from __future__ import annotations

import hashlib
import json
import os
import uuid
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from io import BytesIO
from pathlib import Path

HANDLE_PREFIX = "asset://"

# Asset kinds. draw only accepts KIND_FOREGROUND, chained to KIND_SOURCE.
KIND_SOURCE = "source_photo"
KIND_CROP = "item_crop"
KIND_FOREGROUND = "foreground_png"
KIND_MASK = "mask_png"
KIND_PLATE = "background_plate"
KIND_COMPOSITE = "composite"
KIND_OVERLAY = "mask_overlay"
# Paper-fill reconstruction (decision 93): a code-rendered paper layer
# and its region map. Neither kind may ever appear in a foreground's
# ancestry — validate_foreground's chain-shape rule refuses them as
# product evidence, which is the point: reconstruction is disclosed,
# never receipted as real.
KIND_RECONSTRUCTION = "reconstruction_layer"
KIND_REGION_MAP = "reconstruction_region_map"
# The metadata-free public copy of a GPS-carrying source photo
# (provenance package): same pixels, no embedded metadata,
# always labelled a derived copy wherever it surfaces.
KIND_PUBLIC_COPY = "public_copy"


def _register_image_openers() -> None:
    """Teach Pillow the formats phones actually produce.

    iPhones save HEIC by default; without pillow-heif registered,
    Pillow cannot decode the format at all and ingest would refuse the
    most common seller photo there is (J2 Gauntlet, Fable lane). The
    dependency is declared in pyproject; the guard keeps a stripped
    environment honest (HEIC input then fails as undecodable, like any
    unsupported format) instead of crashing every import.
    """
    try:
        from pillow_heif import register_heif_opener

        register_heif_opener()
    except ImportError:
        pass


_register_image_openers()


class AssetIntegrityError(RuntimeError):
    """Raised when stored bytes no longer match their recorded hashes."""


@dataclass
class AssetRecord:
    asset_id: str
    sha256: str
    kind: str  # one of the KIND_* constants
    filename: str
    byte_size: int
    created_at: str
    source_asset_id: str | None = None  # the asset this one was derived from
    pixel_sha256: str | None = None  # hash of decoded RGBA bytes, if an image
    width: int | None = None
    height: int | None = None
    color_profile_sha256: str | None = None  # hash of the embedded ICC profile


def pixel_hash_of_image(image) -> str:
    """Hash of an image's decoded RGBA pixel bytes (PIL Image input)."""
    return hashlib.sha256(image.convert("RGBA").tobytes()).hexdigest()


# PNG text key carrying a mask asset's provenance JSON (source frame,
# SAM frame, scale method), written by cut_out and read by draw's
# receipt builder.
MASK_PROVENANCE_PNG_KEY = "agenthand_mask_provenance"
COMPOSITE_POLICY_PNG_KEY = "agenthand_composite_policy"
# PNG text key carrying a source asset's provenance signals
# block (provenance.py), written at ingest from the original
# file's bytes and republished by draw into the receipt.
PROVENANCE_PNG_KEY = "agenthand_provenance"


def png_text_of_bytes(data: bytes) -> dict[str, str]:
    """The text chunks embedded in PNG bytes ({} when none / not a PNG).

    Mask assets carry their provenance (source frame, SAM frame, scale
    method) as a PNG text chunk written at cut_out time, inside the
    file whose hashes the asset record pins — so the provenance is as
    tamper-evident as the mask itself and travels with the file, no
    sidecar to lose or desync. Reading is best-effort by design: a
    mask written before provenance existed simply has no chunk.
    """
    try:
        from PIL import Image

        with Image.open(BytesIO(data)) as img:
            if img.format != "PNG":
                return {}
            img.load()
            text = getattr(img, "text", None) or {}
            return {str(k): str(v) for k, v in dict(text).items()}
    except Exception:
        return {}


def icc_profile_of_image(image) -> bytes | None:
    """The image's embedded ICC profile bytes, or None if it has none."""
    info = getattr(image, "info", None) or {}
    icc = info.get("icc_profile")
    return bytes(icc) if icc else None


def color_profile_sha256_of_image(image) -> str | None:
    """Hash of an image's embedded ICC profile, or None if it has none."""
    icc = icc_profile_of_image(image)
    return hashlib.sha256(icc).hexdigest() if icc is not None else None


def describe_color_profile(icc: bytes | None) -> str:
    """Human-readable name for a colour profile, for receipts.

    Best effort: a parseable ICC profile reports its own description;
    anything else falls back to a generic label (the profile's hash,
    recorded alongside, is the identity that actually gets checked).
    """
    if not icc:
        return "sRGB (no embedded ICC profile)"
    try:
        from PIL import ImageCms

        profile = ImageCms.ImageCmsProfile(BytesIO(icc))
        description = profile.profile.profile_description
        if description:
            return str(description).strip()
    except Exception:
        pass
    return "embedded ICC profile (description unreadable)"


def carry_color_profile(source_image, target_image):
    """Copy the source image's ICC profile onto the target image.

    Several Pillow operations (alpha_composite, Image.composite) build
    a fresh image and drop ``info``; every pipeline step that derives
    pixels from a profiled image must re-attach the profile explicitly
    or the product silently changes colour space mid-chain.
    """
    icc = icc_profile_of_image(source_image)
    if icc is not None:
        target_image.info["icc_profile"] = icc
    return target_image


def _image_facts(
    data: bytes,
) -> tuple[str | None, int | None, int | None, str | None]:
    """(pixel_sha256, width, height, color_profile_sha256) for image bytes.

    All None when the data does not decode as an image.
    """
    try:
        from PIL import Image

        with Image.open(BytesIO(data)) as img:
            img.load()
            return (
                pixel_hash_of_image(img),
                img.width,
                img.height,
                color_profile_sha256_of_image(img),
            )
    except Exception:
        return None, None, None, None


class AssetStore:
    def __init__(self, data_dir: Path):
        self.data_dir = Path(data_dir)
        self.assets_dir = self.data_dir / "assets"
        self.index_path = self.data_dir / "assets.json"  # legacy, read-only

    # -- internal record handling -------------------------------------------

    def _sidecar_path(self, asset_id: str) -> Path:
        return self.assets_dir / f"{asset_id}.json"

    def _load_legacy_index(self) -> dict[str, dict]:
        if not self.index_path.exists():
            return {}
        try:
            return json.loads(self.index_path.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, OSError):
            return {}

    def _load_record(self, asset_id: str) -> AssetRecord:
        sidecar = self._sidecar_path(asset_id)
        if sidecar.exists():
            return AssetRecord(**json.loads(sidecar.read_text(encoding="utf-8")))
        legacy = self._load_legacy_index()
        if asset_id in legacy:
            return AssetRecord(**legacy[asset_id])
        raise KeyError(f"Unknown asset id: {asset_id!r}")

    def _save_record(self, record: AssetRecord) -> None:
        self.assets_dir.mkdir(parents=True, exist_ok=True)
        sidecar = self._sidecar_path(record.asset_id)
        tmp = sidecar.with_suffix(f".json.tmp-{uuid.uuid4().hex}")
        tmp.write_text(
            json.dumps(asdict(record), indent=2, sort_keys=True), encoding="utf-8"
        )
        os.replace(tmp, sidecar)

    def _receipt_path(self, asset_id: str) -> Path:
        return self.assets_dir / f"{asset_id}.receipt.json"

    def receipt_path(self, handle: str) -> Path:
        """Where an asset's persisted fidelity receipt lives, if any.

        The receipt sits beside the asset's data file and its record
        sidecar, named ``<asset_id>.receipt.json`` so each composite
        keeps its own (a single shared receipt.json would be
        overwritten by the next draw).
        """
        return self._receipt_path(self.parse_handle(handle))

    def save_receipt(self, handle: str, receipt: dict) -> Path:
        """Persist a fidelity receipt beside its asset; returns its path.

        Written via temp file + os.replace, the same convention as the
        record sidecars, so a crash mid-write cannot leave a torn
        receipt behind: readers see the old file or the new one, never
        half of either.
        """
        self.assets_dir.mkdir(parents=True, exist_ok=True)
        path = self.receipt_path(handle)
        tmp = path.with_suffix(f".json.tmp-{uuid.uuid4().hex}")
        tmp.write_text(json.dumps(receipt, indent=2, sort_keys=True), encoding="utf-8")
        os.replace(tmp, path)
        return path

    def load_receipt(self, handle: str) -> dict | None:
        """The asset's persisted fidelity receipt, or None if none exists."""
        path = self.receipt_path(handle)
        if not path.exists():
            return None
        return json.loads(path.read_text(encoding="utf-8"))

    # -- public API ---------------------------------------------------------

    @staticmethod
    def parse_handle(handle: str) -> str:
        if not isinstance(handle, str) or not handle.startswith(HANDLE_PREFIX):
            raise ValueError(f"Not an asset handle: {handle!r}")
        asset_id = handle[len(HANDLE_PREFIX) :].strip()
        if not asset_id:
            raise ValueError(f"Empty asset id in handle: {handle!r}")
        return asset_id

    def put_bytes(
        self,
        data: bytes,
        *,
        kind: str,
        suffix: str = ".bin",
        source_asset_id: str | None = None,
    ) -> str:
        """Store bytes as a new asset; returns its asset:// handle."""
        self.assets_dir.mkdir(parents=True, exist_ok=True)
        asset_id = uuid.uuid4().hex
        filename = f"{asset_id}{suffix}"
        tmp = self.assets_dir / f"{filename}.tmp-{uuid.uuid4().hex}"
        tmp.write_bytes(data)
        os.replace(tmp, self.assets_dir / filename)

        pixel_sha256, width, height, color_profile_sha256 = _image_facts(data)
        record = AssetRecord(
            asset_id=asset_id,
            sha256=hashlib.sha256(data).hexdigest(),
            kind=kind,
            filename=filename,
            byte_size=len(data),
            created_at=datetime.now(timezone.utc).isoformat(),
            source_asset_id=source_asset_id,
            pixel_sha256=pixel_sha256,
            width=width,
            height=height,
            color_profile_sha256=color_profile_sha256,
        )
        self._save_record(record)
        return HANDLE_PREFIX + asset_id

    def put_image(
        self,
        image,
        *,
        kind: str,
        source_asset_id: str | None = None,
        png_text: dict[str, str] | None = None,
    ) -> str:
        """Store a PIL Image as a PNG asset; returns its asset:// handle.

        The image's embedded ICC colour profile, when it has one, is
        written into the PNG so the stored asset carries the same
        profile as the in-memory image it was stored from.
        """
        icc = icc_profile_of_image(image)
        converted = image.convert("RGBA")
        buffer = BytesIO()
        from PIL.PngImagePlugin import PngInfo

        metadata = PngInfo()
        for key, value in (png_text or {}).items():
            metadata.add_text(key, value)
        kwargs = {"pnginfo": metadata} if png_text else {}
        if icc is not None:
            kwargs["icc_profile"] = icc
        converted.save(buffer, format="PNG", **kwargs)
        return self.put_bytes(
            buffer.getvalue(), kind=kind, suffix=".png", source_asset_id=source_asset_id
        )

    def get_record(self, handle: str) -> AssetRecord:
        return self._load_record(self.parse_handle(handle))

    def resolve(self, handle: str) -> Path:
        record = self.get_record(handle)
        return self.assets_dir / record.filename

    def read_bytes(self, handle: str) -> bytes:
        return self.resolve(handle).read_bytes()

    def read_image(self, handle: str):
        """Decode the asset as a PIL Image in RGBA mode.

        The stored ICC colour profile rides along on the returned
        image so downstream steps can carry it forward explicitly.
        """
        from PIL import Image

        with Image.open(BytesIO(self.read_bytes(handle))) as img:
            img.load()
            converted = img.convert("RGBA")
            return carry_color_profile(img, converted)

    def sha256_of(self, handle: str) -> str:
        return self.get_record(handle).sha256

    def lineage(self, handle: str) -> list[AssetRecord]:
        """The derivation chain for a handle: [record, parent, ..., root].

        Raises KeyError if any link in the chain is missing; a chain that
        cannot be walked to its root must not be trusted by draw.
        """
        chain: list[AssetRecord] = []
        seen: set[str] = set()
        record = self.get_record(handle)
        while True:
            if record.asset_id in seen:
                raise AssetIntegrityError(
                    f"Cycle in asset lineage at {record.asset_id!r}."
                )
            seen.add(record.asset_id)
            chain.append(record)
            if record.source_asset_id is None:
                return chain
            record = self._load_record(record.source_asset_id)

    def verify_integrity(self, handle: str) -> None:
        """Re-hash the stored asset and RAISE on any mismatch.

        Checks the file-byte hash and, when one was recorded, the pixel
        hash. This is the check the Fidelity Lock receipt relies on: if
        the stored product layer no longer matches the hashes recorded
        when segmentation produced it, it must never be presented as the
        untouched product. Returns None on success; raises
        AssetIntegrityError (or KeyError for an unknown handle).
        """
        record = self.get_record(handle)
        data = self.read_bytes(handle)
        actual_file = hashlib.sha256(data).hexdigest()
        if actual_file != record.sha256:
            raise AssetIntegrityError(
                f"Asset {handle!r} file hash mismatch: recorded "
                f"{record.sha256}, actual {actual_file}. The stored bytes "
                "were modified after ingest."
            )
        if record.pixel_sha256 is not None or record.color_profile_sha256 is not None:
            pixel_sha256, _, _, color_profile_sha256 = _image_facts(data)
            if record.pixel_sha256 is not None and pixel_sha256 != record.pixel_sha256:
                raise AssetIntegrityError(
                    f"Asset {handle!r} pixel hash mismatch: recorded "
                    f"{record.pixel_sha256}, actual {pixel_sha256}. The "
                    "decoded pixels differ from what was stored."
                )
            if (
                record.color_profile_sha256 is not None
                and color_profile_sha256 != record.color_profile_sha256
            ):
                raise AssetIntegrityError(
                    f"Asset {handle!r} colour profile mismatch: recorded "
                    f"{record.color_profile_sha256}, actual "
                    f"{color_profile_sha256}. The stored image's colour "
                    "profile was changed after ingest; identical pixel "
                    "values would display as different colours."
                )
