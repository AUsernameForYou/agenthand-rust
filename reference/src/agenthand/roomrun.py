"""ROOM RUN — the multi-item room intake layer (prototype, not an MCP tool).

agenthand started as a one-photo chain: identify -> cut_out -> draw turns
ONE product photo into a staged listing with a Fidelity Lock receipt. The
product Daniel locked on 2026-10-28 (tracker decision 51) is room-scale:
the seller walks a room, names what goes ("this cup, this bear, this
chair"), and gets one staged listing per item — every item with its own
receipt. This module is that intake layer. It orchestrates the three
shipped tools over a SET of room photos (stills, plus frames sampled
from a walkthrough video); it adds no model calls of its own and is NOT
a fourth MCP tool — the three-tool surface stays exactly as shipped.

Pipeline:

0. Narration (optional). A narrated video or audio file the seller
   recorded while walking the room is transcribed by ONE Muse Spark
   Open call via Baseten (the narration link, added 2026-10-28). The
   call returns the verbatim transcript plus two item lists: SELLING
   — items the speaker explicitly marks for sale — and MENTIONED —
   items referred to in passing. Selling items join the work list as
   named items; mentioned items join it too, flagged as mentioned,
   so the visual pipeline below confirms or refutes each against the
   frames — a mentioned item is never silently dropped and never
   silently promoted to a listing the seller did not ask for. The
   transcript is stored beside the manifest and quoted in it.
   Narration failure (no audio track, a silent track, a refused or
   failed call) is an error row in the manifest, never an aborted
   room: with stills or typed items present the run continues
   stills-only.
1. Sources. Stills from the given files/dirs, plus frames sampled from
   each given video with ffmpeg (fixed interval, timestamps recorded).
   A video frame is just another source photo with provenance.
2. Plan pass. identify runs once per source photo. Its plan powers
   photo ranking, auto-enumerate mode, and listing copy (when the
   plan's concept actually matches the item — see the honesty note
   below). A photo whose plan fails is still ingested and probed;
   it just ranks blind.
3. Per item. Candidate photos are ranked by token overlap between the
   item's name and each photo's plan text (stills before frames on
   ties — they are sharper and larger). The probe cap can never drop
   a still: every still photo is always in the candidate set, and
   video frames fill the cap's remaining slots in rank order (probe
   order inside the set stays the rank order). cut_out probes the
   candidates with the item's name as the SAM concept (plus bounded
   deterministic variants: the photo plan's own phrase when it overlaps
   the name, a colour-stripped form, the head noun). Every probe is a
   sighting, success or failure, and lands in the manifest.
4. De-dup. One named item = one listing: its successful sightings
   across photos are the same physical item seen from several angles;
   the primary sighting (most opaque foreground pixels, the closest /
   clearest view) is the one staged, the rest are recorded as
   also-seen-in. In auto-enumerate mode, per-photo candidates are
   grouped by concept similarity — a group merges ONLY on strong
   evidence (high token similarity, or similar names whose masks on
   near-duplicate viewpoints overlap); the grey zone keeps both
   candidates and flags them possible duplicates. Across items, two
   listings whose primary foregrounds are pixel-identical, or whose
   primary masks on the same photo almost perfectly overlap, are
   flagged as possible duplicates of each other. The cross-item
   check runs at sighting level: any two successful sightings of
   two staged items, on the same photo, whose masks overlap at
   least 95% of the smaller mask also link the items as possible
   duplicates, primaries or not. NOTHING is ever silently merged
   away: a flag keeps both listings and says why.
   One collision shape gets a repair attempt on top of the flag:
   when a colliding sighting pair involves at least one item's
   primary, the object is contested; when exactly one item's own
   listing evidence names that item and not the other, the keeper
   keeps the object and the other item is displaced, but only when
   its own primary is one of the colliding sightings — it
   re-stages from its largest successful sighting that does not
   collide with any of the keeper's sightings, when it has one,
   under a fresh receipt and verification. A keeper whose primary
   is not the contested object is never moved. Ambiguous evidence,
   no clean alternate sighting, a failed re-stage, or a re-stage
   the run budget cannot cover all leave the original staged
   result standing. Duplicate flags are never removed by a
   re-selection: they describe the probing ambiguity, and the
   re-selection flag and run note record the resolution.
5. Stage. In the default "scene" stage mode, draw runs the primary
   foreground with ONE shared scene prompt for the whole room (the
   room's plate style), so the set of listings reads as one room's
   worth of staging. In the "wallpaper" and "studio_gray" stage
   modes the plate is rendered in code at the foreground's frame
   size (no model call, no spend), stored as a plate asset, and
   drawn through the same compositor, so the receipt pins the
   plate's hashes and names it code-rendered. The per-item fidelity
   receipt is the draw tool's own, persisted beside the composite
   in the store and linked from the manifest.
6. Verify. A staged item is not believed on draw's say-so: the
   repo's independent verifier (composite.verify_receipt_from_disk)
   re-reads the item's stored source, foreground, and composite
   files, re-checks their hashes, and recounts the changed opaque
   pixels from disk. Its verdict lands in the manifest as the item's
   "verification" field, distinct from the draw receipt it checks.
   An item whose verification fails is reported verify_failed, not
   staged — the demo-ready Gauntlet's fix #1: a room run "verifies
   end to end" only when an independent path re-proves each item.
7. Manifest. JSON + a readable summary: room id, sources, per-item
   status (staged / failed / verify_failed, with the stage and error
   that failed it), receipt links and pixel counts, the independent
   verification per item, sightings, duplicate flags, and the run's
   total cost read as the delta of the run's own cost ledger across
   the run — the amounts the spend cap actually counted, failed
   attempts included. Partial failure is a first-class outcome: a
   room where four items stage and two honestly fail is a successful
   room run with a truthful manifest.

Honesty notes (v1 limits, stated rather than smoothed over):

- identify plans ONE primary subject per photo. In a crowded room
  photo its listing copy describes that subject, so whole-photo
  listing copy is attached ONLY when the photo plan's concept
  matches the item. When it does not, the item-crop path runs
  instead: the primary source photo is cropped to the item's own
  foreground box (10% margin, clamped), the crop is stored as an
  asset, and identify runs on the crop. Only a successful crop
  identify attaches copy (source "item_crop"); any failure leaves
  the manifest saying copy was not generated for the item instead
  of dressing it in another item's words. Auto-enumerate likewise
  surfaces one candidate per viewpoint; several viewpoints of one
  room are what surface several items.
- The seller's name is the segmentation concept. SAM sometimes wants
  a different phrasing than a human uses; the bounded variant list is
  the recovery, and a name no variant can segment is a failed item
  with NO_MATCH in the manifest, never a quietly substituted object.

Spend discipline: every Meta model call goes through the shipped
tools, so every Meta dollar flows through meta_client.billed_call
under its fail-closed cap. The narration call is not a Meta API
call: it runs on Baseten's Muse Spark Open deployment and draws on
the $100 Baseten partner credit pool. Since fix pass F3 it runs
inside meta_client.billed_call too (tool NARRATION_TOOL, model
NARRATION_MODEL, priced in pricing.py at the published rates), so
it reserves its worst-case projection against the spend cap before
the call and leaves pending + settled ledger rows like every other
billable call. Its usage and estimated cost are still recorded in
the manifest's narration section, never in the room total (the
room's spend baseline is taken after narration), and a per-call
projection cap refuses an absurd call before it is made. Room
runs keep their OWN ledger (default: ledger.jsonl inside the output
dir; --ledger overrides): during a run, AGENTHAND_LEDGER_PATH points
at the room ledger, and billed_call MIRRORS every receipt the run
causes into it, so the run's cost stays provable from the room
directory alone. The spend cap is NOT the room's (fix pass F4):
every reservation and settle also lands in the operator's cumulative
ledger (config.get_cumulative_ledger_path) and the cap is checked
there, so it sees every run's spend. Before F4 the room ledger
REPLACED the cumulative ledger for the run, so the cap reset to $0
on every run ($0.90 + $0.90 passed a $1.00 cap); the old contract
that a room run never moves the cumulative ledger is reversed on
purpose. The per-run budget (default $1.00) plus the worst-case
projection checked before the first call bound one room's spend;
the manifest's total is the room ledger's delta across the run.
"""

from __future__ import annotations

import argparse
import array
import base64
import copy
import hashlib
import json
import math
import mimetypes
import os
import re
import shutil
import subprocess
import sys
import uuid
import wave
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

from . import backdrop, comps, config, ingest, ledger, market_pricing
from .assets import KIND_CROP, KIND_PLATE, AssetStore

# The shared plate style for a room run: one scene prompt for every
# item, so a room's listings stage into the same visual world. draw
# wraps it in its own fixed "empty scene, clear central surface"
# instruction; this string only has to name the room.
DEFAULT_SCENE = (
    "a bright, calm room with a plain warm-white wall and a light "
    "wooden table surface, soft daylight"
)

# Bounds on the probing work (and therefore the spend) per item.
MAX_PHOTOS_PER_ITEM = 3
MAX_VARIANTS_PER_PHOTO = 2
MAX_SAM_CALLS_PER_ITEM = 6

# De-dup evidence thresholds. Merging two enumerated candidates into
# one item needs name similarity at or above MERGE_SIMILARITY (or the
# mask-overlap corroboration in group_candidates); similarity in
# [FLAG_SIMILARITY, MERGE_SIMILARITY) is the grey zone — keep both,
# flag both.
MERGE_SIMILARITY = 0.6
FLAG_SIMILARITY = 0.4
MASK_IOU_FLAG = 0.9
# Collision re-selection: two staged primaries on one photo whose
# foreground masks overlap at least this fraction of the SMALLER
# mask are the same physical object for one of the two items.
MASK_OVERLAP_COLLISION = 0.95

# Leading adjectives the variant generator may strip to widen a SAM
# phrase ("gray storage box" -> "storage box"). Colours and sizes are
# the words sellers add that SAM least needs.
_STRIPPABLE_LEADERS = {
    "black", "white", "gray", "grey", "red", "orange", "yellow",
    "green", "blue", "purple", "pink", "brown", "wooden", "wood",
    "large", "small", "big", "little", "vintage", "antique",
}

_TOKEN_RE = re.compile(r"[a-z0-9]+")


# ---------------------------------------------------------------------------
# Pure helpers (no store, no API — unit-tested offline)
# ---------------------------------------------------------------------------


def tokens_of(text: str | None) -> list[str]:
    """Lowercase word tokens of a name or phrase, order preserved."""
    if not text:
        return []
    return _TOKEN_RE.findall(text.lower())


def _token_set(text: str | None) -> set[str]:
    return set(tokens_of(text))


def token_similarity(a: str | None, b: str | None) -> float:
    """Jaccard similarity of two phrases' token sets (0.0–1.0).

    A shared token counts when the tokens are equal or one is a
    prefix of the other with length >= 4 ("book" ~ "bookcase"), which
    is how a seller's short name meets identify's longer concepts.
    """
    set_a, set_b = _token_set(a), _token_set(b)
    if not set_a or not set_b:
        return 0.0

    def _matches(token: str, others: set[str]) -> bool:
        return any(
            token == other
            or (len(token) >= 4 and other.startswith(token))
            or (len(other) >= 4 and token.startswith(other))
            for other in others
        )

    shared = sum(1 for t in set_a if _matches(t, set_b))
    union = len(set_a) + len(set_b) - shared
    return shared / union if union else 0.0


def plan_text(plan: dict | None) -> str:
    """All the searchable text of an identify plan, joined."""
    if not plan:
        return ""
    parts = [
        plan.get("description") or "",
        plan.get("concept") or "",
        plan.get("suggested_title") or "",
        " ".join(plan.get("listing_bullets") or []),
        plan.get("condition_notes") or "",
    ]
    return " ".join(parts)


def plan_matches_item(plan: dict | None, item_name: str) -> bool:
    """True when the plan's own concept names this item.

    Listing copy is only honest when the plan was ABOUT the item;
    a crowded photo's plan describes its primary subject, which may
    be a different object entirely.
    """
    if not plan or not plan.get("concept"):
        return False
    return token_similarity(plan["concept"], item_name) >= MERGE_SIMILARITY


def concept_variants(item_name: str, plan: dict | None = None) -> list[str]:
    """Bounded SAM-phrase variants for one item, best first.

    Order: the seller's name (lowercased); the photo plan's own
    concept when it overlaps the name (identify's phrasing is tuned
    for SAM); the colour/size-stripped form; the head noun alone.
    Duplicates removed, order preserved.
    """
    name = " ".join(tokens_of(item_name))
    variants: list[str] = []
    if name:
        variants.append(name)
    if plan and plan.get("concept") and plan_matches_item(plan, item_name):
        concept = " ".join(tokens_of(plan["concept"]))
        if concept and concept not in variants:
            variants.append(concept)
    words = name.split()
    if len(words) > 1 and words[0] in _STRIPPABLE_LEADERS:
        stripped = " ".join(words[1:])
        if stripped not in variants:
            variants.append(stripped)
    if len(words) > 1:
        head = words[-1]
        if head not in variants:
            variants.append(head)
    return variants


def bbox_iou(a: list[float] | None, b: list[float] | None) -> float:
    """IoU of two [x0, y0, x1, y1] boxes (any consistent units)."""
    if not a or not b:
        return 0.0
    ix0, iy0 = max(a[0], b[0]), max(a[1], b[1])
    ix1, iy1 = min(a[2], b[2]), min(a[3], b[3])
    iw, ih = max(0.0, ix1 - ix0), max(0.0, iy1 - iy0)
    inter = iw * ih
    area_a = max(0.0, a[2] - a[0]) * max(0.0, a[3] - a[1])
    area_b = max(0.0, b[2] - b[0]) * max(0.0, b[3] - b[1])
    union = area_a + area_b - inter
    return inter / union if union > 0 else 0.0


def normalized_bbox(bbox: list | None, width: int | None, height: int | None):
    """A mask bbox in source pixels -> frame-relative [0..1] coords."""
    if not bbox or not width or not height:
        return None
    return [
        bbox[0] / width,
        bbox[1] / height,
        bbox[2] / width,
        bbox[3] / height,
    ]


@dataclass
class Candidate:
    """One enumerated candidate item from one photo's plan (auto mode)."""

    name: str
    photo_index: int
    plan: dict


def group_candidates(candidates: list[Candidate]) -> list[dict]:
    """Group auto-enumerated candidates into items, conservatively.

    Returns a list of groups: {"name", "photo_indices", "flags"}.
    A candidate joins a group only on strong evidence: concept
    similarity >= MERGE_SIMILARITY with the group's name. Similarity
    in the grey zone does NOT merge — the candidate starts its own
    group and both groups are flagged as possible duplicates of each
    other. This is the "when unsure, keep both and flag" rule.
    """
    groups: list[dict] = []
    for cand in candidates:
        matched = None
        grey: list[dict] = []
        for group in groups:
            sim = token_similarity(cand.name, group["name"])
            if sim >= MERGE_SIMILARITY:
                matched = group
                break
            if sim >= FLAG_SIMILARITY:
                grey.append(group)
        if matched is not None:
            if cand.photo_index not in matched["photo_indices"]:
                matched["photo_indices"].append(cand.photo_index)
            continue
        group = {
            "name": cand.name,
            "photo_indices": [cand.photo_index],
            "flags": [],
        }
        for other in grey:
            flag = (
                f"possible duplicate of '{other['name']}' "
                f"(name similarity in the grey zone; kept separate)"
            )
            group["flags"].append(flag)
            other["flags"].append(
                f"possible duplicate of '{cand.name}' "
                f"(name similarity in the grey zone; kept separate)"
            )
        groups.append(group)
    return groups


# ---------------------------------------------------------------------------
# Sources: stills + sampled video frames
# ---------------------------------------------------------------------------


@dataclass
class PhotoSource:
    """One source photo in the pool, with provenance and its plan."""

    path: Path
    origin: str  # "still" | "video_frame"
    video_path: str | None = None
    timestamp_s: float | None = None
    handle: str | None = None
    width: int | None = None
    height: int | None = None
    plan: dict | None = None
    plan_error: dict | None = None

    def manifest_entry(self, index: int) -> dict:
        plan = None
        if self.plan:
            plan = {
                "description": self.plan.get("description"),
                "concept": self.plan.get("concept"),
                "suggested_title": self.plan.get("suggested_title"),
            }
        return {
            "index": index,
            "path": str(self.path),
            "origin": self.origin,
            "video": self.video_path,
            "timestamp_s": self.timestamp_s,
            "handle": self.handle,
            "width": self.width,
            "height": self.height,
            "plan": plan,
            "plan_error": self.plan_error,
        }


_IMAGE_SUFFIXES = {".jpg", ".jpeg", ".png", ".webp", ".heic", ".heif", ".gif"}


def collect_stills(paths: list[Path]) -> list[Path]:
    """Expand files/dirs into a sorted list of image files."""
    found: list[Path] = []
    for raw in paths:
        path = Path(raw)
        if path.is_dir():
            found.extend(
                sorted(
                    p for p in path.rglob("*") if p.suffix.lower() in _IMAGE_SUFFIXES
                )
            )
        elif path.is_file():
            found.append(path)
    return found


# -- sampled-frame dedupe (token saver, 2026-10-31) -----------------------
# Frames are compared against the last KEPT frame as 16x16 grayscale
# box-mean thumbnails; a candidate whose mean absolute thumbnail
# difference is at most FRAME_DEDUPE_MAX_MEAN_ABS_DIFF gray levels is
# a near-duplicate and is dropped before any paid identify call. The
# bar is deliberately strict: the P12 run showed a second angle of
# the same item is information, not duplication, and a moved camera
# or repositioned object scores far above this bound (see the test
# pair in tests/test_roomrun.py). Every drop is logged with its
# similarity score in the run record ("frames_dropped").
FRAME_DEDUPE_CELLS = 16
FRAME_DEDUPE_MAX_MEAN_ABS_DIFF = 3.0


def frame_gray_thumbnail(path: Path) -> list[float] | None:
    """16x16 grayscale box-mean thumbnail of an image, or None.

    Gray uses integer Rec. 601 luma ((299R + 587G + 114B) // 1000);
    each cell averages the source box [c*W//16, (c+1)*W//16) on both
    axes. The Rust build computes the identical values; the two
    builds' drop decisions match on the same frames.
    """
    try:
        import numpy as np
        from PIL import Image

        with Image.open(path) as img:
            rgb = np.asarray(img.convert("RGB"), dtype=np.int64)
    except Exception:  # noqa: BLE001 - an undecodable frame is kept, never dropped
        return None
    height, width = rgb.shape[0], rgb.shape[1]
    if height == 0 or width == 0:
        return None
    gray = (rgb[:, :, 0] * 299 + rgb[:, :, 1] * 587 + rgb[:, :, 2] * 114) // 1000
    cells = FRAME_DEDUPE_CELLS
    thumb: list[float] = []
    for cy in range(cells):
        y0, y1 = cy * height // cells, max(cy * height // cells + 1, (cy + 1) * height // cells)
        for cx in range(cells):
            x0, x1 = cx * width // cells, max(cx * width // cells + 1, (cx + 1) * width // cells)
            thumb.append(float(gray[y0:y1, x0:x1].mean()))
    return thumb


def frame_mean_abs_diff(a: list[float], b: list[float]) -> float:
    """Mean absolute difference between two grayscale thumbnails."""
    return sum(abs(x - y) for x, y in zip(a, b)) / len(a)


def sample_video_frames(
    video_path: Path,
    out_dir: Path,
    *,
    every_seconds: float = 2.0,
    max_frames: int = 8,
    ffmpeg: str | None = None,
) -> tuple[list[tuple[Path, float]], str | None, list[dict]]:
    """Sample frames from a video with ffmpeg at a fixed interval.

    Returns (frames, note, dropped): frames are (path, timestamp_s)
    pairs that survived near-duplicate dedupe; dropped carries one
    {"timestamp_s", "similarity"} record per dropped frame. When
    ffmpeg is unavailable or the video cannot be probed, returns
    ([], reason, []) — video intake degrades to stills-only, loudly,
    in the manifest notes rather than failing the room.
    """
    ffmpeg = ffmpeg if ffmpeg is not None else shutil.which("ffmpeg")
    if not ffmpeg:
        return [], "ffmpeg not found on PATH; video frames not sampled", []
    probe = subprocess.run(
        [
            "ffprobe", "-v", "error", "-show_entries", "format=duration",
            "-of", "json", str(video_path),
        ],
        capture_output=True, text=True, timeout=30,
    )
    try:
        duration = float(json.loads(probe.stdout)["format"]["duration"])
    except Exception:
        return [], f"ffprobe could not read {video_path}; frames not sampled", []
    out_dir.mkdir(parents=True, exist_ok=True)
    frames: list[tuple[Path, float]] = []
    dropped: list[dict] = []
    last_thumb: list[float] | None = None
    t = 0.0
    while t < duration and len(frames) < max_frames:
        frame_path = out_dir / f"{video_path.stem}-t{t:05.1f}s.jpg"
        result = subprocess.run(
            [
                ffmpeg, "-v", "error", "-ss", f"{t:.2f}", "-i", str(video_path),
                "-frames:v", "1", "-q:v", "3", str(frame_path), "-y",
            ],
            capture_output=True, text=True, timeout=60,
        )
        if result.returncode == 0 and frame_path.exists() and frame_path.stat().st_size:
            thumb = frame_gray_thumbnail(frame_path)
            diff = (
                frame_mean_abs_diff(thumb, last_thumb)
                if thumb is not None and last_thumb is not None
                else None
            )
            if diff is not None and diff <= FRAME_DEDUPE_MAX_MEAN_ABS_DIFF:
                dropped.append(
                    {
                        "timestamp_s": round(t, 2),
                        "similarity": round(1.0 - diff / 255.0, 4),
                    }
                )
                frame_path.unlink(missing_ok=True)
            else:
                frames.append((frame_path, round(t, 2)))
                if thumb is not None:
                    last_thumb = thumb
        t += every_seconds
    if not frames:
        return [], f"ffmpeg extracted no frames from {video_path}", dropped
    return frames, None, dropped


# ---------------------------------------------------------------------------
# Narration: the seller's voice becomes the item list
#
# One Muse Spark Open call via Baseten per narrated file (the pattern
# proven by the 2026-10-28 voice spike): the audio rides as an
# audio_url content part carrying a base64 data: URL, and the reply
# carries the verbatim transcript plus the SELLING / MENTIONED item
# split. Baseten spend draws on the $100 partner pool. Since fix pass
# F3 each call runs inside meta_client.billed_call (tool
# NARRATION_TOOL), so the spend cap sees it and the ledger holds its
# pending + settled rows; the manifest still records it separately
# from the room's Meta total.
# ---------------------------------------------------------------------------

NARRATION_MODEL = "meta-models/muse-spark-open-1026"
# Ledger tool name for narration receipts (fix pass F3).
NARRATION_TOOL = "narration"
NARRATION_BASE_URL = "https://inference.baseten.co/v1"
# Spark Open always reasons before it answers, and the reasoning grows
# with the clip. The voice spike showed a 1024-token cap fully consumed
# by reasoning, leaving an empty reply; 4096 covered the short spikes
# but not a real walkthrough: on Daniel's 58.9 s backyard clip the
# model's reasoning alone ran 5,474 tokens (5,668 completion total)
# before any answer text, so the shipped 4096 cap returned an empty
# reply (film run P12, 2026-10-31; the same call at an 8000 cap
# recovered the full transcript and sort). The cap is 16384 PER PIECE:
# narration splits any clip longer than NARRATION_CHUNK_SECONDS into
# pieces (below), so no single call ever faces more than a minute of
# audio. A minute at the worst observed reasoning rate (~121 tokens/s,
# the 24 s voice spike) needs ~7,300 reasoning tokens plus its share
# of transcript, so 16384 carries >2x headroom per piece; for the
# 58.9 s diagnostic clip in one piece it is ~2.9x the whole observed
# completion. The cap is a fixed constant rather than duration-scaled
# on purpose: billing is per token actually used, so the cap only
# feeds the cost projection; a formula would add cross-build parity
# surface for no billing benefit. A piece whose reasoning still eats
# the whole budget fails loudly with a correction path (see
# interpret_narration_body), never a silent empty result.
NARRATION_MAX_TOKENS = 16384
# Pieces are a minute because that is the proven envelope: the
# diagnostic clip (58.9 s, one call) is the longest single call ever
# observed end to end, request size and latency included. A trailing
# remainder shorter than the merge bound joins the previous piece
# instead of becoming a sliver call of its own.
NARRATION_CHUNK_SECONDS = 60.0
NARRATION_CHUNK_MERGE_REMAINDER_S = 15.0
# Baseten's published Model API rates for Muse Spark Open (Baseten
# model library page, checked 2026-10-28). Estimates in the manifest
# use exactly these; the API returns tokens, not dollars.
NARRATION_INPUT_USD_PER_TOKEN = 1.25 / 1_000_000
NARRATION_OUTPUT_USD_PER_TOKEN = 4.25 / 1_000_000
# Audio bills as input tokens. The observed rate on a real 24 s
# walkthrough was ~55 audio tokens per second; the projection prices
# 150/s so it errs high, and adds a fixed prompt allowance.
NARRATION_AUDIO_TOKENS_PER_SECOND = 150.0
NARRATION_PROMPT_TOKENS = 512
# A narration file whose worst-case projection exceeds this is
# refused before any call is made. The projection is honestly
# worst-case (every piece priced at the full token cap), so the bound
# is sized for the supported envelope: clips up to 5 minutes are a
# product requirement (Daniel, 2026-10-31), and a 5-minute file
# projects to ~$0.41 worst case against ~$0.13 of expected actual
# usage at observed rates, so $0.50 covers the whole supported
# envelope with headroom and the gate only trips on a pathological
# input or a misprobed duration.
NARRATION_MAX_PROJECTED_USD = 0.50
# The narration limit: clips up to 5 minutes are accepted (chunked
# into per-minute calls); a longer clip gets the polite
# NARRATION_TOO_LONG refusal naming this limit (Daniel, 2026-10-31).
# Baseten caps files at 5 minutes too, so the limit also matches the
# upstream ceiling.
NARRATION_MAX_DURATION_S = 300.0
# A track whose peak never reaches 1% of full scale (-40 dBFS)
# carries no speech anyone could transcribe.
SILENCE_PEAK_FRACTION = 0.01

_AUDIO_SUFFIXES = {".wav", ".mp3", ".flac", ".ogg", ".m4a", ".aac", ".opus"}
_NARRATION_MIME = {
    ".wav": "audio/wav",
    ".mp3": "audio/mpeg",
    ".flac": "audio/flac",
    ".ogg": "audio/ogg",
    ".m4a": "audio/mp4",
    ".aac": "audio/aac",
    ".opus": "audio/ogg",
}

NARRATION_PROMPT = (
    "Transcribe this recording verbatim, word for word, and output it "
    "under the heading TRANSCRIPT. Then, under the heading SELLING, "
    "list each physical item the speaker explicitly says they want to "
    "sell (wording like 'sell this', 'this one's for sale', 'get rid "
    "of'), one per line, naming each item the way the speaker referred "
    "to it; if the speaker marks no item for sale, write NONE under "
    "that heading. Then, under the heading KEEP, list each physical "
    "item the speaker says they are keeping or not selling (wording "
    "like 'the bike stays', 'keep it', 'not for sale', 'keep it for "
    "insurance'), one per line; if there are none, write NONE. Then, "
    "under the heading MENTIONED, list each other physical item the "
    "speaker refers to in passing without saying they want to sell "
    "it or keep it, one per line; if there are none, write NONE. Use "
    "exactly those four headings, in that order, and do not add any "
    "other text. Do not invent items the speaker did not refer to."
)


def parse_narration_reply(text: str | None) -> dict:
    """Split a narration reply into transcript / selling / keep / mentioned.

    Pure and forgiving in exactly one direction: a reply with no
    recognizable heading is treated as all transcript with no items,
    never as items with no transcript. Item lines are cleaned of
    bullets and numbering; a NONE line means the empty list.
    """
    sections: dict[str, list[str]] = {
        "transcript": [],
        "selling": [],
        "keep": [],
        "mentioned": [],
    }
    headings = {
        "transcript": "transcript",
        "selling": "selling",
        "keep": "keep",
        "mentioned": "mentioned",
    }
    current: str | None = None
    found = False
    for raw_line in (text or "").splitlines():
        line = raw_line.strip()
        key = line.rstrip(":").strip().lower()
        if key in headings:
            current = headings[key]
            found = True
            continue
        if current is None:
            continue  # preamble before the first heading is not evidence
        sections[current].append(line)
    if not found:
        return {"transcript": (text or "").strip(), "selling": [], "keep": [], "mentioned": []}

    def _items(lines: list[str]) -> list[str]:
        out: list[str] = []
        for line in lines:
            cleaned = line.lstrip("-•*").strip()
            cleaned = re.sub(r"^\d+[.)]\s*", "", cleaned).strip()
            if not cleaned or cleaned.upper() == "NONE":
                continue
            if cleaned not in out:
                out.append(cleaned)
        return out

    return {
        "transcript": "\n".join(sections["transcript"]).strip(),
        "selling": _items(sections["selling"]),
        "keep": _items(sections["keep"]),
        "mentioned": _items(sections["mentioned"]),
    }


def plan_narration_chunks(duration_s: float) -> list[tuple[float, float]]:
    """Split a clip's timeline into narration pieces.

    Pure. A clip at or under NARRATION_CHUNK_SECONDS is one piece (the
    only shape older runs ever made). Longer clips cut into
    chunk-sized windows; a trailing remainder under
    NARRATION_CHUNK_MERGE_REMAINDER_S joins the previous window
    instead of becoming a sliver call. A 5-minute clip plans to
    exactly 5 pieces.
    """
    if duration_s <= NARRATION_CHUNK_SECONDS:
        return [(0.0, duration_s)]
    chunks: list[tuple[float, float]] = []
    start = 0.0
    while duration_s - start > NARRATION_CHUNK_SECONDS:
        chunks.append((start, start + NARRATION_CHUNK_SECONDS))
        start += NARRATION_CHUNK_SECONDS
    if chunks and duration_s - start < NARRATION_CHUNK_MERGE_REMAINDER_S:
        chunks[-1] = (chunks[-1][0], duration_s)
    else:
        chunks.append((start, duration_s))
    return chunks


def project_narration_cost_usd(
    duration_s: float, *, max_tokens: int = NARRATION_MAX_TOKENS
) -> float:
    """Worst-case price of narrating one file at the published rates.

    Output is priced per planned piece at the full token cap: with
    chunking, a long file's worst case is pieces x cap, not one cap.
    """
    input_tokens = duration_s * NARRATION_AUDIO_TOKENS_PER_SECOND + NARRATION_PROMPT_TOKENS
    pieces = len(plan_narration_chunks(duration_s))
    return (
        input_tokens * NARRATION_INPUT_USD_PER_TOKEN
        + pieces * max_tokens * NARRATION_OUTPUT_USD_PER_TOKEN
    )


def narration_cost_usd(usage: dict | None) -> float | None:
    """Price a finished call's returned usage; None when unreported."""
    if not usage:
        return None
    prompt = usage.get("prompt_tokens") or 0
    completion = usage.get("completion_tokens") or 0
    if not prompt and not completion:
        return None
    return (
        prompt * NARRATION_INPUT_USD_PER_TOKEN
        + completion * NARRATION_OUTPUT_USD_PER_TOKEN
    )


def _usage_count(usage: dict | None, key: str) -> int:
    value = (usage or {}).get(key) if isinstance(usage, dict) else None
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return 0
    if not math.isfinite(value) or value <= 0:
        return 0
    return int(round(value))


def narration_billing_usage(usage: dict | None, duration_s: float) -> dict:
    """The token usage a narration call settles at in the ledger.

    The provider's reported prompt / completion tokens, or, when the
    reply reports none, the worst-case projection's own token counts
    (audio at NARRATION_AUDIO_TOKENS_PER_SECOND plus the prompt
    allowance, the full cap per planned piece). An unreported call
    therefore settles at its projected cost, never at $0: money that
    may have been spent is counted as spent. Fix pass F3; the same
    rule as the Rust build's narration_billing_usage.
    """
    prompt = _usage_count(usage, "prompt_tokens")
    completion = _usage_count(usage, "completion_tokens")
    if prompt == 0 and completion == 0:
        pieces = len(plan_narration_chunks(duration_s))
        return {
            "input_tokens": math.ceil(
                duration_s * NARRATION_AUDIO_TOKENS_PER_SECOND + NARRATION_PROMPT_TOKENS
            ),
            "output_tokens": pieces * NARRATION_MAX_TOKENS,
        }
    return {"input_tokens": prompt, "output_tokens": completion}


# -- narration audio prep (token saver, 2026-10-31) -----------------------
# The narration model is billed per token of audio it processes, and
# walkthrough clips carry long pauses. Before the call, the extracted
# WAV is trimmed of silence and downsampled to a speech-appropriate
# rate. Detection is deliberately conservative: a 10 ms frame counts
# as speech when its PEAK reaches the same -40 dBFS floor the silence
# gate uses (SILENCE_PEAK_FRACTION), every kept region is padded
# 200 ms on both sides, and gaps up to 800 ms are bridged, so nothing
# at or above the floor can be clipped. Prep failure falls back to
# the original WAV: prep is an optimization, never a new failure mode.
NARRATION_PREP_TARGET_RATE = 16000
NARRATION_PREP_FRAME_S = 0.01
NARRATION_PREP_PAD_S = 0.2
NARRATION_PREP_BRIDGE_S = 0.8


def prepare_narration_audio(wav_path: Path) -> tuple[Path, float | None]:
    """Trim silence from a narration WAV and downsample it for the call.

    Returns (path to send, prepared duration in seconds). The 5-minute
    refusal and the cost projection stay priced on the ORIGINAL
    duration (the caller probes before prepping); this only shrinks
    what is actually sent. On any prep failure, or when nothing would
    be saved, the original path comes back with None.
    """
    try:
        with wave.open(str(wav_path), "rb") as handle:
            channels = handle.getnchannels()
            width = handle.getsampwidth()
            rate = handle.getframerate()
            raw = handle.readframes(handle.getnframes())
    except Exception:  # noqa: BLE001 - prep must never become a failure mode
        return wav_path, None
    if width != 2 or channels not in (1, 2) or rate <= 0 or not raw:
        return wav_path, None
    samples = array.array("h")
    samples.frombytes(raw[: len(raw) - len(raw) % 2])
    if sys.byteorder == "big":
        samples.byteswap()
    if channels == 2:
        mono = array.array("h", bytes(2 * (len(samples) // 2)))
        for i in range(0, len(samples) - 1, 2):
            # Truncation toward zero, mirroring the Rust build's i32
            # division exactly (Python // floors negative sums).
            mono[i // 2] = int((samples[i] + samples[i + 1]) / 2)
        samples = mono
    total = len(samples)
    if total == 0:
        return wav_path, None
    frame_len = max(1, int(rate * NARRATION_PREP_FRAME_S))
    floor_peak = SILENCE_PEAK_FRACTION * 32767.0
    speech_frames: list[bool] = []
    for start in range(0, total, frame_len):
        chunk = samples[start : start + frame_len]
        speech_frames.append(
            bool(chunk) and max(max(chunk), -min(chunk)) >= floor_peak
        )
    if not any(speech_frames):
        # A (near-)silent clip: the silence gate upstream owns that
        # verdict; prep has nothing to trim.
        return wav_path, None
    pad = int(rate * NARRATION_PREP_PAD_S)
    bridge = int(rate * NARRATION_PREP_BRIDGE_S)
    regions: list[tuple[int, int]] = []
    for index, is_speech in enumerate(speech_frames):
        if not is_speech:
            continue
        lo = max(0, index * frame_len - pad)
        hi = min(total, (index + 1) * frame_len + pad)
        if regions and lo - regions[-1][1] <= bridge:
            regions[-1] = (regions[-1][0], hi)
        else:
            regions.append((lo, hi))
    kept = array.array("h")
    for lo, hi in regions:
        kept.extend(samples[lo:hi])
    factor = 1
    if rate % NARRATION_PREP_TARGET_RATE == 0:
        factor = rate // NARRATION_PREP_TARGET_RATE
    out_rate = rate
    if factor > 1:
        usable = len(kept) - len(kept) % factor
        down = array.array("h", bytes(2 * (usable // factor)))
        for i in range(0, usable, factor):
            mean = sum(kept[i : i + factor]) / factor
            down[i // factor] = max(-32768, min(32767, round(mean)))
        kept = down
        out_rate = rate // factor
    if len(kept) >= total and out_rate == rate:
        return wav_path, None
    out_path = wav_path.with_name(f"{wav_path.stem}-prepared.wav")
    try:
        with wave.open(str(out_path), "wb") as handle:
            handle.setnchannels(1)
            handle.setsampwidth(2)
            handle.setframerate(out_rate)
            if sys.byteorder == "big":
                kept.byteswap()
            handle.writeframes(kept.tobytes())
    except Exception:  # noqa: BLE001 - same fallback rule as above
        return wav_path, None
    return out_path, len(kept) / out_rate


def probe_wav(path: Path) -> tuple[float, float]:
    """(duration_s, peak fraction of full scale) of a PCM WAV, stdlib only."""
    with wave.open(str(path), "rb") as wav:
        frames = wav.getnframes()
        rate = wav.getframerate()
        width = wav.getsampwidth()
        raw = wav.readframes(frames)
    duration = frames / rate if rate else 0.0
    peak = 0.0
    if width == 2 and raw:
        samples = array.array("h")
        samples.frombytes(raw[: len(raw) - len(raw) % 2])
        if sys.byteorder == "big":
            samples.byteswap()
        if samples:
            peak = max(max(samples), -min(samples)) / 32768.0
    elif width == 1 and raw:
        peak = max(abs(b - 128) for b in raw) / 128.0
    elif width == 4 and raw:
        samples = array.array("i")
        samples.frombytes(raw[: len(raw) - len(raw) % 4])
        if sys.byteorder == "big":
            samples.byteswap()
        if samples:
            peak = max(max(samples), -min(samples)) / 2147483648.0
    return duration, peak


def _has_audio_stream(path: Path) -> bool:
    """ffprobe stream check; True when it cannot tell (don't mislabel)."""
    ffprobe = shutil.which("ffprobe")
    if not ffprobe:
        return True
    probe = subprocess.run(
        [
            "ffprobe", "-v", "error", "-select_streams", "a",
            "-show_entries", "stream=index", "-of", "json", str(path),
        ],
        capture_output=True, text=True, timeout=30,
    )
    try:
        return bool(json.loads(probe.stdout).get("streams"))
    except Exception:
        return True


def extract_narration_audio(
    source_path: Path,
    kind: str,
    work_dir: Path,
    *,
    ffmpeg: str | None = None,
) -> tuple[Path | None, dict | None]:
    """Decode a narrated input's audio track to a 48 kHz mono WAV.

    Returns (wav_path, None) or (None, error). A video with no audio
    stream is NO_AUDIO_TRACK, a distinct verdict from a decode
    failure: the seller filmed in silence, which the manifest should
    say plainly. A plain WAV input with no ffmpeg on the machine is
    used as-is — the stdlib can probe it and the call takes WAV.
    """
    ffmpeg = ffmpeg if ffmpeg is not None else shutil.which("ffmpeg")
    if kind == "audio" and source_path.suffix.lower() == ".wav" and not ffmpeg:
        return source_path, None
    if not ffmpeg:
        return None, {
            "code": "AUDIO_EXTRACT_FAILED",
            "message": "ffmpeg not found on PATH; the narration track could not be decoded",
        }
    work_dir.mkdir(parents=True, exist_ok=True)
    wav_path = work_dir / f"{source_path.stem}.wav"
    result = subprocess.run(
        [
            ffmpeg, "-v", "error", "-i", str(source_path), "-vn",
            "-ac", "1", "-ar", "48000", str(wav_path), "-y",
        ],
        capture_output=True, text=True, timeout=300,
    )
    if result.returncode != 0 or not wav_path.exists() or wav_path.stat().st_size <= 44:
        if kind == "video" and not _has_audio_stream(source_path):
            return None, {
                "code": "NO_AUDIO_TRACK",
                "message": f"{source_path.name} has no audio stream; nothing to transcribe",
            }
        return None, {
            "code": "AUDIO_EXTRACT_FAILED",
            "message": (
                f"ffmpeg could not decode an audio track from {source_path.name}: "
                f"{result.stderr.strip()[:300]}"
            ),
        }
    return wav_path, None


def narration_payload(data_url: str) -> dict:
    """The narration request body. Pure, so tests can pin the budget.

    The max_tokens value is the fix for the P12 failure: it must stay
    well above the reasoning a real walkthrough generates before any
    answer text (5,474 tokens on the 58.9 s diagnostic clip).
    """
    return {
        "model": NARRATION_MODEL,
        "messages": [
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": NARRATION_PROMPT},
                    {"type": "audio_url", "audio_url": {"url": data_url}},
                ],
            }
        ],
        "max_tokens": NARRATION_MAX_TOKENS,
    }


def _narration_budget_remedy(usage: dict) -> str:
    """Usage parenthetical for the budget error, when it was reported."""
    completion = usage.get("completion_tokens")
    if isinstance(completion, int):
        return f" (usage: {completion} completion tokens, budget {NARRATION_MAX_TOKENS})"
    return ""


def interpret_narration_body(body: dict, *, latency_ms: int) -> dict:
    """Turn a Baseten chat-completions body into the narration record.

    Pure, so tests can drive it with stubbed bodies. The failure this
    guards is silent degradation: a reply whose reasoning ate the
    whole budget comes back empty (or cut off mid-answer), and an
    empty transcript would otherwise flow into the manifest as if
    the seller said nothing. Both shapes raise with the cause and a
    correction path (decision 114); _narrate_one records the failure
    as a NARRATION_FAILED row and the stills run continues.
    """
    choices = body.get("choices") or []
    choice = choices[0] if choices else {}
    content = (choice.get("message") or {}).get("content") or ""
    usage = body.get("usage") or {}
    remedy = (
        "Trim the recording to about a minute or less and narrate "
        "that, or re-record a shorter walkthrough."
    )
    if not content.strip():
        raise RuntimeError(
            "Baseten narration call returned an empty reply: the "
            "model's reasoning spent the token budget before any "
            f"transcript was written{_narration_budget_remedy(usage)}. "
            + remedy
        )
    if choice.get("finish_reason") == "length":
        raise RuntimeError(
            "Baseten narration call was cut off by the token budget "
            "before the reply finished (finish_reason 'length'"
            f"{_narration_budget_remedy(usage)}); the transcript "
            "would be incomplete. " + remedy
        )
    return {
        "reply": content,
        "usage": usage,
        "latency_ms": latency_ms,
        "model": body.get("model") or NARRATION_MODEL,
    }


def _data_url_for(audio_path: Path) -> str:
    mime = _NARRATION_MIME.get(audio_path.suffix.lower(), "audio/wav")
    return f"data:{mime};base64," + base64.b64encode(audio_path.read_bytes()).decode("ascii")


def slice_wav_pieces(
    wav_path: Path, chunks: list[tuple[float, float]]
) -> list[Path]:
    """Write one WAV per planned chunk, beside the source WAV.

    Stdlib wave only, so it accepts what probe_wav accepts (PCM).
    The caller owns the piece files and deletes them; pieces are
    transport, never deliverables.
    """
    with wave.open(str(wav_path), "rb") as wav:
        params = wav.getparams()
        rate = wav.getframerate()
        frames = wav.readframes(wav.getnframes())
    frame_bytes = params.sampwidth * params.nchannels
    pieces: list[Path] = []
    for i, (start_s, end_s) in enumerate(chunks):
        start_f = int(round(start_s * rate))
        end_f = int(round(end_s * rate))
        piece = wav_path.with_name(f"{wav_path.stem}.piece{i + 1:02d}.wav")
        with wave.open(str(piece), "wb") as out:
            out.setparams(params)
            out.writeframes(frames[start_f * frame_bytes : end_f * frame_bytes])
        pieces.append(piece)
    return pieces


def merge_narration_replies(results: list[dict]) -> dict:
    """Fold per-piece narration records into the file's one record.

    Replies join in timeline order; parse_narration_reply reads the
    repeated headings as one document, so the transcript reads
    continuously (a sentence can straddle a piece boundary) and the
    item lists union. Usage sums the pieces' prompt/completion
    tokens, which is what the cost estimate prices.
    """
    usage: dict = {}
    if any(r.get("usage") for r in results):
        usage = {
            "prompt_tokens": sum(
                (r.get("usage") or {}).get("prompt_tokens") or 0 for r in results
            ),
            "completion_tokens": sum(
                (r.get("usage") or {}).get("completion_tokens") or 0
                for r in results
            ),
        }
    return {
        "reply": "\n".join(r["reply"] for r in results),
        "usage": usage,
        "latency_ms": sum(r.get("latency_ms") or 0 for r in results),
        "model": results[0].get("model") or NARRATION_MODEL,
    }


def _call_narration(data_url: str) -> dict:
    """ONE Muse Spark Open call via Baseten for one piece of audio.

    The voice-spike shape, verbatim where it matters: an audio_url
    content part carrying a base64 data: URL, the stored
    custom.baseten credential attached as an authd surrogate by
    dynamic_credentials (the raw key never reaches this process),
    redirects refused, and a generous token budget because Spark
    Open's reasoning eats small ones whole.
    """
    import time
    import urllib.error
    import urllib.request

    helper_dir = (
        os.environ.get("AGENTHAND_CREDENTIALS_BIN")
        or "/opt/muse/skills/custom-connector-setup/bin"
    )
    if helper_dir not in sys.path:
        sys.path.insert(0, helper_dir)
    try:
        from dynamic_credentials import add_surrogate_to_request, read_json_response
    except Exception as exc:  # noqa: BLE001 — reported as a narration failure row
        raise RuntimeError(
            f"the Baseten credential helper is unavailable ({exc}); "
            "narration cannot run on this machine"
        ) from exc

    payload = narration_payload(data_url)
    request = urllib.request.Request(
        f"{NARRATION_BASE_URL}/chat/completions",
        data=json.dumps(payload).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    add_surrogate_to_request(request, "custom.baseten", allowed_hosts=["inference.baseten.co"])

    class _NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, *args):
            raise RuntimeError("redirect refused, verify the endpoint")

    opener = urllib.request.build_opener(_NoRedirect())
    started = time.monotonic()
    try:
        with opener.open(request, timeout=300) as response:
            body = read_json_response(response)
    except urllib.error.HTTPError as exc:
        detail = exc.read().decode("utf-8", "replace")[:500]
        raise RuntimeError(
            f"Baseten narration call failed: HTTP {exc.code}: {detail}"
        ) from exc
    latency_ms = round((time.monotonic() - started) * 1000)
    return interpret_narration_body(body, latency_ms=latency_ms)


def _default_narration_fn(audio_path: Path) -> dict:
    """Production narration for one extracted WAV.

    Clips up to a minute make the single call older runs always made.
    Longer clips are sliced into planned pieces, one call per piece,
    and the replies merge into the file's one record, so a 5-minute
    walkthrough narrates end to end instead of dying on the first
    call's token budget. A WAV the stdlib cannot slice (a non-PCM
    original used as-is) falls back to the single call, which then
    succeeds or fails loudly on its own budget.
    """
    try:
        duration_s, _peak = probe_wav(audio_path)
    except Exception:  # noqa: BLE001 — unprobeable: the single call decides
        duration_s = 0.0
    chunks = plan_narration_chunks(duration_s)
    if len(chunks) <= 1:
        return _call_narration(_data_url_for(audio_path))
    try:
        pieces = slice_wav_pieces(audio_path, chunks)
    except Exception:  # noqa: BLE001 — unslicable: the single call decides
        return _call_narration(_data_url_for(audio_path))
    try:
        results: list[dict] = []
        for i, piece in enumerate(pieces):
            try:
                results.append(_call_narration(_data_url_for(piece)))
            except Exception as exc:  # noqa: BLE001 — re-raised with piece context
                raise RuntimeError(
                    f"narration piece {i + 1} of {len(pieces)} failed: {exc}"
                ) from exc
        return merge_narration_replies(results)
    finally:
        for piece in pieces:
            piece.unlink(missing_ok=True)


def _narrate_one(
    source_path: Path,
    kind: str,
    work_dir: Path,
    narration_fn,
    *,
    max_projected_usd: float,
) -> dict:
    """One narrated input -> its manifest record. Never raises.

    Every failure is a verdict in the record (no track, silent,
    over-long, cost-refused, call failed) and the room run moves on:
    narration is an input, not a load-bearing wall.
    """
    record: dict = {
        "path": str(source_path),
        "kind": kind,
        "audio_file": None,
        "duration_s": None,
        "prepared_duration_s": None,
        "peak_fraction": None,
        "transcript": None,
        "named_items": [],
        "kept_items": [],
        "mentioned_items": [],
        "usage": None,
        "projected_cost_usd": None,
        "cost_usd_estimate": None,
        "latency_ms": None,
        "error": None,
    }
    wav_path, error = extract_narration_audio(source_path, kind, work_dir)
    if error is not None:
        record["error"] = error
        return record
    record["audio_file"] = str(wav_path)
    try:
        duration_s, peak = probe_wav(wav_path)
    except Exception as exc:  # noqa: BLE001 — a probe failure is a row, not a crash
        record["error"] = {
            "code": "AUDIO_PROBE_FAILED",
            "message": f"could not measure the narration track: {exc}",
        }
        return record
    record["duration_s"] = round(duration_s, 2)
    record["peak_fraction"] = round(peak, 4)
    if duration_s > NARRATION_MAX_DURATION_S:
        record["error"] = {
            "code": "NARRATION_TOO_LONG",
            "message": (
                f"narration runs {duration_s:.0f}s, over the 5-minute "
                "limit for a narrated clip; upload a shorter video "
                "(5 minutes or less), or trim this one to the part "
                "that matters, and run it again"
            ),
        }
        return record
    projected = project_narration_cost_usd(duration_s)
    record["projected_cost_usd"] = round(projected, 6)
    if projected > max_projected_usd:
        record["error"] = {
            "code": "NARRATION_COST_REFUSED",
            "message": (
                f"projected narration cost ${projected:.4f} exceeds the "
                f"per-file cap ${max_projected_usd:.2f}; the narration "
                "was refused before any call was made"
            ),
        }
        return record
    if peak < SILENCE_PEAK_FRACTION:
        record["error"] = {
            "code": "SILENT_TRACK",
            "message": (
                f"the narration track peaks at {peak:.4f} of full scale, "
                "under the -40 dBFS floor; there is no speech to transcribe"
            ),
        }
        return record
    send_path, prepared_s = prepare_narration_audio(wav_path)
    if prepared_s is not None:
        record["prepared_duration_s"] = round(prepared_s, 2)
    # Billing (fix pass F3): the Baseten call runs inside
    # meta_client.billed_call, like every Meta call. The worst-case
    # projection is reserved against the spend cap BEFORE the call (a
    # pending ledger row, or a refusal with no call made) and the
    # reported usage settles it after (a settled row). A failed call
    # leaves its reservation counted. The record's own fields are
    # unchanged: cost_usd_estimate is still priced from the usage.
    from . import meta_client

    outcome: dict = {}

    def _call() -> dict:
        outcome["started"] = True
        result = narration_fn(send_path)
        outcome["result"] = result
        return narration_billing_usage(result.get("usage"), duration_s)

    try:
        meta_client.billed_call(
            NARRATION_TOOL,
            NARRATION_MODEL,
            projected,
            _call,
            note=f"narration of {source_path.name}",
        )
    except Exception as exc:  # noqa: BLE001 — the call boundary, as elsewhere here
        if "result" in outcome:
            # The call ran but its settle failed (for example, the
            # actual cost pushed the ledger over the cap).
            record["error"] = {"code": "NARRATION_BILLING_FAILED", "message": str(exc)}
        elif outcome.get("started"):
            record["error"] = {
                "code": "NARRATION_FAILED",
                "message": f"{type(exc).__name__}: {exc}",
            }
        else:
            # Refused before the call ran: the spend cap cannot cover
            # the projection (or the ledger state is unusable).
            code = (
                "SPEND_CAP_REFUSED"
                if str(exc).startswith("Spend cap")
                else "NARRATION_BILLING_FAILED"
            )
            record["error"] = {
                "code": code,
                "message": f"the narration was refused before any call was made: {exc}",
            }
        return record
    result = outcome["result"]
    parsed = parse_narration_reply(result.get("reply") or "")
    record["transcript"] = parsed["transcript"]
    record["named_items"] = parsed["selling"]
    record["kept_items"] = parsed["keep"]
    record["mentioned_items"] = parsed["mentioned"]
    record["usage"] = result.get("usage") or {}
    record["latency_ms"] = result.get("latency_ms")
    cost = narration_cost_usd(record["usage"])
    record["cost_usd_estimate"] = (
        round(cost, 6) if cost is not None else record["projected_cost_usd"]
    )
    return record


# ---------------------------------------------------------------------------
# The chain: shipped tools behind injectable adapters (offline tests
# substitute fakes; production uses server.identify/cut_out/draw).
# ---------------------------------------------------------------------------


def _default_verify(store: AssetStore, composite_handle: str) -> dict:
    """The production verifier: the repo's independent receipt check.

    composite.verify_receipt_from_disk re-reads the stored source,
    foreground, and composite files, re-checks every file against its
    recorded hashes, and recounts the changed opaque pixels fresh
    from disk — nothing is taken from draw's in-memory receipt except
    the claims being checked. The receipt persisted beside the
    composite is the one graded, so what verifies is what a judge
    with the store would find.
    """
    from . import composite

    return composite.verify_receipt_from_disk(store, composite_handle)


def _run_verification(ctx: "_RunContext", composite_handle: str) -> dict:
    """Run the verifier over one staged item; shape its manifest field.

    The returned dict is the manifest's "verification" record —
    deliberately distinct from the item's "receipt" (draw's own
    numbers): these are the independent recount's numbers. A
    verifier that cannot run at all (no persisted receipt, an
    unknown handle) is a FAILED verification, never an exception
    that aborts the room and never a silent pass: an item nobody
    could check has not been shown to be intact.
    """
    try:
        report = ctx.verify_fn(ctx.store, composite_handle)
    except Exception as exc:  # noqa: BLE001 — verifier boundary, as in composite
        return {
            "ok": False,
            "changed_opaque_pixels": None,
            "opaque_pixels": None,
            "color_profile_preserved": None,
            "problems": [
                f"the independent verifier could not run: "
                f"{type(exc).__name__}: {exc}"
            ],
            "checks_failed": [],
        }
    recomputed = report.get("recomputed") or {}
    checks = report.get("checks") or {}
    return {
        "ok": bool(report.get("ok")),
        "changed_opaque_pixels": recomputed.get("changed_opaque_pixels"),
        "opaque_pixels": recomputed.get("opaque_pixels"),
        "color_profile_preserved": recomputed.get("color_profile_preserved"),
        "problems": list(report.get("problems") or []),
        "checks_failed": sorted(k for k, v in checks.items() if not v),
    }


def draw_with_code_plate(
    store: AssetStore,
    foreground_handle: str,
    stage_mode: str,
    *,
    paper_fill: bool = True,
    occluder_mask_handles: tuple[str, ...] = (),
) -> dict:
    """Backdrop-mode draw: render the plate in code, store it, draw.

    No model call and no billed call: the plate is rendered at the
    foreground's frame size, stored as a plate asset, and passed to
    composite.draw_product as the plate handle, so the Fidelity Lock
    receipt pins the plate's hashes and carries the code-rendered
    backdrop description. Returns the chain's draw-result shape.

    Paper fill (decision 93) is on by default here: before the draw,
    paperfill.build_reconstruction renders the motif-paper layer for
    the cut's empty regions from the stored source and mask, and the
    draw composites plate, paper, product. A receipt that comes back
    with changed product pixels, paper overlapping the product, or a
    broken colour profile is a FIDELITY_VIOLATION error result, not
    a staged item — this path used to return ok unconditionally.
    """
    from . import composite

    foreground = store.read_image(foreground_handle)
    recon_kwargs: dict = {}
    if paper_fill:
        from . import paperfill

        report = paperfill.build_reconstruction(
            store, foreground_handle, occluder_mask_handles=occluder_mask_handles
        )
        recon_kwargs = {
            "reconstruction_handle": report.get("layer_handle"),
            "region_map_handle": report.get("region_map_handle"),
            "reconstruction_report": report,
        }
    composite.preflight_cut(
        store,
        foreground_handle,
        reconstruction_handle=recon_kwargs.get("reconstruction_handle"),
        require_pass=True,
        occluder_mask_handles=occluder_mask_handles,
    )
    plate = backdrop.render_plate(stage_mode, foreground.width, foreground.height)
    plate_handle = store.put_image(plate, kind=KIND_PLATE)
    handle, receipt = composite.draw_product(
        store,
        foreground_handle,
        f"code-rendered {stage_mode} backdrop",
        plate_handle=plate_handle,
        plate_kind="code_rendered",
        backdrop=backdrop.backdrop_info(stage_mode),
        **recon_kwargs,
    )
    recon = receipt.get("reconstruction") or {}
    if (
        receipt["changed_opaque_pixels"] != 0
        or recon.get("reconstruction_overlap_product_pixels") != 0
        or receipt["color_profile_preserved"] is not True
    ):
        return {
            "status": "error",
            "error": {
                "code": "FIDELITY_VIOLATION",
                "message": (
                    "the backdrop draw's receipt does not hold: "
                    f"changed_opaque_pixels={receipt['changed_opaque_pixels']}, "
                    "reconstruction_overlap_product_pixels="
                    f"{recon.get('reconstruction_overlap_product_pixels')}, "
                    "color_profile_preserved="
                    f"{receipt['color_profile_preserved']}"
                ),
                "fidelity_receipt": receipt,
            },
            "output_asset": handle,
            "fidelity_receipt": receipt,
        }
    return {
        "status": "ok",
        "output_asset": handle,
        "plate_asset": plate_handle,
        "fidelity_receipt": receipt,
        "reconstruction": recon,
        "cost_usd": 0.0,
    }


def _default_chain() -> dict:
    from . import server

    def identify_fn(image_ref: str) -> dict:
        return server.identify(image_ref)

    def cut_out_fn(handle: str, concept: str) -> dict:
        return server.cut_out(handle, concept)

    def draw_fn(handle: str, scene: str) -> dict:
        result = server.draw(handle, scene)
        return result[0] if isinstance(result, list) else result

    def draw_backdrop_fn(handle: str, stage_mode: str) -> dict:
        return draw_with_code_plate(
            AssetStore(config.get_data_dir()), handle, stage_mode
        )

    return {
        "identify": identify_fn,
        "cut_out": cut_out_fn,
        "draw": draw_fn,
        "draw_backdrop": draw_backdrop_fn,
    }


def _data_uri(path: Path) -> str:
    mime = mimetypes.guess_type(path.name)[0] or "image/jpeg"
    if not mime.startswith("image/"):
        mime = "image/jpeg"
    return f"data:{mime};base64," + base64.b64encode(path.read_bytes()).decode()


def _call_cost(result: dict) -> float:
    """What one tool call counted as spend, success or failure."""
    if not isinstance(result, dict):
        return 0.0
    if result.get("cost_usd") is not None:
        return float(result["cost_usd"])
    error = result.get("error") or {}
    if error.get("cost_counted_usd") is not None:
        return float(error["cost_counted_usd"])
    return 0.0


def _opaque_pixels(store: AssetStore, handle: str | None) -> int | None:
    """Opaque (alpha > 0) pixel count of a stored foreground image."""
    if not handle:
        return None
    try:
        import numpy as np

        image = store.read_image(handle)
        alpha = np.asarray(image.getchannel("A"))
        return int(np.count_nonzero(alpha > 0))
    except Exception:
        return None


@dataclass
class _RunContext:
    store: AssetStore
    chain: dict
    scene: str
    stage_mode: str = "scene"
    verify_fn: object = None  # (store, composite_handle) -> report dict
    max_photos_per_item: int = MAX_PHOTOS_PER_ITEM
    max_sam_calls_per_item: int = MAX_SAM_CALLS_PER_ITEM
    sam_calls: int = 0  # per item; reset by the item loop
    flags: list[str] = field(default_factory=list)


def _rank_photos(item_name: str, sources: list[PhotoSource]) -> list[int]:
    """Source indices in probe order for one item.

    Ranked by plan-text evidence for the item, strongest signal
    first: a photo whose plan names the item's HEAD NOUN (the item's
    last token — "hat", "suitcase", "box") outranks every photo
    whose plan merely shares a modifier with the name. The flat
    overlap this replaced let a colour word do exactly that damage
    in the first real room run: three photos whose plans mentioned
    "black" (a suitcase's trim, a railing, a purifier's grille)
    outranked every photo that actually held the black hat, because
    the hat-bearing plans said "hat" and never "black" — and the
    probe cap then spent itself on the wrong photos. Within the
    head-noun tier, token overlap of the full name decides; ties
    break toward stills (sharper, larger) and then toward the
    earlier source. The full ordering is returned — mentioning
    photos first, then every silent photo as coverage fill (stills
    before frames) — and the caller's probe cap bounds how far down
    the order a run actually probes. A silent plan is weak evidence
    of absence: identify describes only a photo's primary subject,
    so the bookcase photo whose plan says "bookshelf" can still hold
    the hat on its top shelf.
    """
    item_tokens = tokens_of(item_name)
    head_noun = item_tokens[-1] if item_tokens else None

    def _matches(token: str, others: set[str]) -> bool:
        return any(
            token == other
            or (len(token) >= 4 and other.startswith(token))
            or (len(other) >= 4 and token.startswith(other))
            for other in others
        )

    def score(idx: int) -> float:
        plan = sources[idx].plan
        if not plan:
            return 0.0
        text_tokens = _token_set(plan_text(plan))
        item_token_set = _token_set(item_name)
        if not text_tokens or not item_token_set:
            return 0.0
        shared = sum(1 for t in item_token_set if _matches(t, text_tokens))
        overlap = shared / len(item_token_set)
        head_bonus = 1.0 if head_noun and _matches(head_noun, text_tokens) else 0.0
        return head_bonus + overlap

    scored = [(score(i), i) for i in range(len(sources))]
    order = sorted(
        scored,
        key=lambda pair: (
            -pair[0],
            0 if sources[pair[1]].origin == "still" else 1,
            pair[1],
        ),
    )
    return [i for _, i in order]


def _select_candidate_indices(
    item_name: str,
    ranked: list[int],
    sources: list[PhotoSource],
    cap: int,
) -> list[int]:
    """Apply the probe cap to one item's ranked candidates.

    Coverage rule: every still photo is always selected — a cap
    filled by higher-ranked video frames must never silently drop
    the still that holds an item the frames' plans never named.
    Video frames fill the cap's remaining slots in rank order; when
    the stills alone meet or exceed the cap, the set is the stills.
    A still missing from the ranked list (auto-enumerate groups
    carry only their own photos) is added and probes after the
    ranked candidates, in its own rank order among the added
    stills. Probe order inside the selected set is the rank order.
    """
    ranked = list(dict.fromkeys(ranked))
    rank_pos = {idx: pos for pos, idx in enumerate(ranked)}
    full_pos = {idx: pos for pos, idx in enumerate(_rank_photos(item_name, sources))}

    def order_key(idx: int) -> tuple:
        if idx in rank_pos:
            return (0, rank_pos[idx])
        return (1, full_pos.get(idx, idx))

    selected = {
        i for i, source in enumerate(sources) if source.origin == "still"
    }
    remaining = max(0, cap - len(selected))
    for idx in ranked:
        if remaining <= 0:
            break
        if sources[idx].origin == "still" or idx in selected:
            continue
        selected.add(idx)
        remaining -= 1
    return sorted(selected, key=order_key)


def _probe_item(
    ctx: _RunContext,
    item_name: str,
    sources: list[PhotoSource],
    candidates: list[int],
    *,
    stop_on_success: bool,
) -> list[dict]:
    """cut_out probes for one item across its candidate photos.

    Returns the sighting records (successes and failures). Bounded by
    the context's per-item SAM call budget: variants are tried
    best-first per photo. With stop_on_success, probing ends at the
    first photo that cuts cleanly (the cheaper mode); without it,
    every candidate is probed so the manifest carries the full
    cross-photo sighting evidence the de-dup record is built from.
    """
    sightings: list[dict] = []
    for idx in candidates:
        source = sources[idx]
        if source.handle is None:
            sightings.append(
                {
                    "photo_index": idx,
                    "status": "error",
                    "concept_used": None,
                    "foreground_asset": None,
                    "mask_asset": None,
                    "mask_overlay_asset": None,
                    "opaque_pixels": None,
                    "bbox_normalized": None,
                    "mask_warnings": [],
                    "cost_usd": 0.0,
                    "error": {"code": "NO_HANDLE", "message": "photo was not ingested"},
                }
            )
            continue
        variants = concept_variants(item_name, source.plan)
        tried = 0
        photo_cost = 0.0
        success = None
        last_error = None
        for variant in variants:
            if tried >= MAX_VARIANTS_PER_PHOTO or ctx.sam_calls >= ctx.max_sam_calls_per_item:
                break
            tried += 1
            ctx.sam_calls += 1
            result = ctx.chain["cut_out"](source.handle, variant)
            photo_cost += _call_cost(result)
            if result.get("status") == "ok":
                success = (variant, result)
                break
            last_error = result.get("error") or {"code": "UNKNOWN", "message": str(result)}
        if success is not None:
            variant, result = success
            review = result.get("mask_review") or {}
            sightings.append(
                {
                    "photo_index": idx,
                    "status": "ok",
                    "concept_used": variant,
                    "foreground_asset": result.get("output_asset"),
                    "mask_asset": result.get("mask_asset"),
                    "mask_overlay_asset": result.get("mask_overlay_asset"),
                    "opaque_pixels": _opaque_pixels(ctx.store, result.get("output_asset")),
                    "bbox_normalized": normalized_bbox(
                        review.get("bbox"), source.width, source.height
                    ),
                    "mask_warnings": review.get("warnings", []),
                    "cost_usd": photo_cost,
                    "error": None,
                }
            )
            if stop_on_success:
                break
        else:
            sightings.append(
                {
                    "photo_index": idx,
                    "status": "error",
                    "concept_used": variants[0] if variants else None,
                    "variants_tried": tried,
                    "foreground_asset": None,
                    "mask_asset": None,
                    "mask_overlay_asset": None,
                    "opaque_pixels": None,
                    "bbox_normalized": None,
                    "mask_warnings": [],
                    "cost_usd": photo_cost,
                    "error": last_error,
                }
            )
    return sightings


_NO_COPY_NOTE = (
    "the source photo's identify plan described a different "
    "primary subject; per-item listing copy was not generated "
    "rather than borrowed from another item"
)


def _none_listing(note: str) -> dict:
    return {
        "title": None,
        "bullets": [],
        "condition_notes": None,
        "source": "none",
        "note": note,
        "crop_asset": None,
        "plan_concept": None,
    }


def _crop_listing(
    ctx: _RunContext, name: str, sources: list[PhotoSource], primary: dict
) -> tuple[dict, float]:
    """Listing copy from the item's own crop of its primary photo.

    The whole-photo identify plan describes the photo's main subject,
    which in a room photo is usually a different item. So when that
    plan does not yield copy for this item, the PRIMARY SOURCE image
    is cropped to this item's own foreground box in that photo (the
    primary sighting's box, expanded by 10% on each side and clamped
    to the frame), the crop is stored as an asset, and the chain's
    identify runs on the crop alone. Copy is never borrowed from
    another item or photo: the crop comes only from this item's own
    primary sighting. Returns (listing record, identify cost). Any
    failure (no recorded box, an unreadable source, a failed
    identify) degrades to the honest "none" record.
    """
    source = sources[primary["photo_index"]]
    bbox = primary.get("bbox_normalized")
    if not bbox or source.handle is None:
        return _none_listing(
            _NO_COPY_NOTE + "; no foreground box was recorded for the item crop"
        ), 0.0
    try:
        image = ctx.store.read_image(source.handle)
    except Exception:
        return _none_listing(
            _NO_COPY_NOTE + "; the item crop could not be read from the source photo"
        ), 0.0
    width, height = image.width, image.height
    box_w = (bbox[2] - bbox[0]) * width
    box_h = (bbox[3] - bbox[1]) * height
    x0 = max(0, math.floor(bbox[0] * width - 0.10 * box_w))
    y0 = max(0, math.floor(bbox[1] * height - 0.10 * box_h))
    x1 = min(width, math.ceil(bbox[2] * width + 0.10 * box_w))
    y1 = min(height, math.ceil(bbox[3] * height + 0.10 * box_h))
    if x1 <= x0 or y1 <= y0:
        return _none_listing(_NO_COPY_NOTE + "; the item crop box was empty"), 0.0
    crop = image.crop((x0, y0, x1, y1))
    try:
        crop_handle = ctx.store.put_image(
            crop,
            kind=KIND_CROP,
            source_asset_id=AssetStore.parse_handle(source.handle),
        )
    except Exception:
        return _none_listing(
            _NO_COPY_NOTE + "; the item crop could not be stored"
        ), 0.0
    try:
        result = ctx.chain["identify"](crop_handle)
    except Exception:
        return _none_listing(_NO_COPY_NOTE + "; the item-crop identify attempt failed"), 0.0
    cost = _call_cost(result)
    if result.get("status") == "ok" and result.get("suggested_title"):
        return {
            "title": result.get("suggested_title"),
            "bullets": result.get("listing_bullets") or [],
            "condition_notes": result.get("condition_notes"),
            "source": "item_crop",
            "note": None,
            "crop_asset": crop_handle,
            # The crop plan's own concept, kept as the listing's
            # evidence for the duplicate-collision keeper check:
            # the title alone ("Woven Straw Sun Hat") is wordier
            # than the concept the plan actually segmented.
            "plan_concept": result.get("concept"),
        }, cost
    return _none_listing(_NO_COPY_NOTE + "; the item-crop identify attempt failed"), cost


def _sighting_primary_key(sources: list[PhotoSource], sighting: dict) -> tuple:
    """Primary-selection ordering for one successful sighting.

    The clearest view wins: most opaque product pixels first; ties
    toward stills, then the earlier photo. Primary selection and
    collision re-selection share this exact ordering.
    """
    source = sources[sighting["photo_index"]]
    return (
        -(sighting.get("opaque_pixels") or 0),
        0 if source.origin == "still" else 1,
        sighting["photo_index"],
    )


def _stage_primary(
    ctx: _RunContext, record: dict, sources: list[PhotoSource], primary: dict
) -> bool:
    """Stage one item from the given primary sighting, in place.

    Sets the record's primary / also-seen-in, recomputes its listing
    from that primary (the photo plan when that plan was about this
    item, else the item-crop path), draws in the run's stage mode,
    records the receipt, and runs the independent verification.
    Returns True only when the item ends staged; on a draw or
    verify failure the record carries the failure instead. Costs of
    every call made count into the record either way. Used for the
    first staging and for a collision re-selection, so a re-staged
    item is produced by exactly the same path as a staged one.
    """
    name = record["name"]
    record["primary_photo_index"] = primary["photo_index"]
    record["also_seen_in"] = [
        s["photo_index"]
        for s in record["sightings"]
        if s["status"] == "ok" and s is not primary
    ]

    # Listing copy: the primary photo's plan when that plan was about
    # this item; otherwise the item-crop path (identify run on a crop
    # of this item's own primary sighting). Copy is never borrowed
    # from another item (see the module docstring's honesty note).
    plan = sources[primary["photo_index"]].plan
    if plan_matches_item(plan, name):
        record["listing"] = {
            "title": plan.get("suggested_title"),
            "bullets": plan.get("listing_bullets") or [],
            "condition_notes": plan.get("condition_notes"),
            "source": "photo_plan",
            "note": None,
            "crop_asset": None,
            "plan_concept": plan.get("concept"),
        }
    else:
        listing, crop_cost = _crop_listing(ctx, name, sources, primary)
        record["listing"] = listing
        record["cost_usd"] += crop_cost

    record["stage_reached"] = "draw"
    if ctx.stage_mode in backdrop.BACKDROP_STAGE_MODES:
        draw_backdrop_fn = ctx.chain.get("draw_backdrop")
        if draw_backdrop_fn is not None:
            draw_result = draw_backdrop_fn(primary["foreground_asset"], ctx.stage_mode)
        else:
            try:
                draw_result = draw_with_code_plate(
                    ctx.store, primary["foreground_asset"], ctx.stage_mode
                )
            except Exception as exc:  # noqa: BLE001 — a draw failure is a record
                draw_result = {
                    "status": "error",
                    "error": {
                        "code": "DRAW_FAILED",
                        "message": f"backdrop draw failed: {type(exc).__name__}: {exc}",
                    },
                }
    else:
        draw_result = ctx.chain["draw"](primary["foreground_asset"], ctx.scene)
    record["cost_usd"] += _call_cost(draw_result)
    if draw_result.get("status") != "ok":
        error = draw_result.get("error") or {}
        record["failure"] = {
            "stage": "draw",
            "code": error.get("code", "DRAW_FAILED"),
            "message": error.get("message", "draw failed"),
        }
        return False

    receipt = draw_result.get("fidelity_receipt") or {}
    record["output_asset"] = draw_result.get("output_asset")
    record["receipt"] = {
        "source_asset": receipt.get("source_asset"),
        "foreground_pixel_sha256": receipt.get("foreground_pixel_sha256"),
        "opaque_pixels": receipt.get("opaque_pixels"),
        "changed_opaque_pixels": receipt.get("changed_opaque_pixels"),
        "color_profile_preserved": receipt.get("color_profile_preserved"),
        "plate_kind": receipt.get("plate_kind"),
        "backdrop": receipt.get("backdrop"),
        "cost_usd": draw_result.get("cost_usd"),
    }
    # Angle lock (staging.py): the draw result's record rides on the
    # item record when the draw produced one. It never enters the
    # fidelity receipt.
    if draw_result.get("angle_lock") is not None:
        record["angle_lock"] = draw_result["angle_lock"]
    receipt_path = ctx.store.receipt_path(record["output_asset"])
    record["receipt_file"] = str(receipt_path) if receipt_path.exists() else None

    # Verify loop (demo-ready Gauntlet fix #1): draw's receipt is
    # draw grading its own work. The item counts as staged only when
    # the independent verifier, re-reading the stored files, agrees.
    record["stage_reached"] = "verify"
    verification = _run_verification(ctx, record["output_asset"])
    record["verification"] = verification
    if verification["ok"]:
        record["status"] = "staged"
        record["stage_reached"] = "done"
        record["failure"] = None
        return True
    record["status"] = "verify_failed"
    problems = verification["problems"]
    record["failure"] = {
        "stage": "verify",
        "code": "VERIFY_FAILED",
        "message": (
            "the staged output did not pass independent verification: "
            + ("; ".join(problems) if problems else "verifier reported failure")
        ),
    }
    return False


def _intent_of_origin(origin: str | None) -> str:
    """The manifest intent vocabulary, identical in both builds:
    selling | keep | mentioned, derived from the item's origin."""
    return {
        "narration_mentioned": "mentioned",
        "narration_keep": "keep",
    }.get(origin or "", "selling")


def _keep_cut_quality(primary: dict) -> dict:
    """Grade a kept item's cut from the sighting's review signals:
    a mask box running off a frame edge is the occlusion signature
    (RESHOOT), as are mask-review warnings. For keep items the
    grade rides INSIDE the ownership record with its retake
    instruction instead of blocking the record (decision 114)."""
    touches: list[str] = []
    bbox = primary.get("bbox_normalized") or []
    if len(bbox) == 4:
        if bbox[0] == 0.0:
            touches.append("left")
        if bbox[1] == 0.0:
            touches.append("top")
        if bbox[2] == 1.0:
            touches.append("right")
        if bbox[3] == 1.0:
            touches.append("bottom")
    warnings = primary.get("mask_warnings") or []
    if not touches and not warnings:
        return {"grade": "PASS", "reshoot_instruction": None, "mask_warnings": warnings}
    if touches:
        edge_text = (
            f"it currently runs off the {' and '.join(touches)} "
            f"edge{'s' if len(touches) > 1 else ''}"
        )
    else:
        edge_text = "the mask review flagged this cut"
    return {
        "grade": "RESHOOT",
        "reshoot_instruction": (
            "Retake the photo a step further back so the whole item "
            f"sits inside the frame — {edge_text}. The ownership "
            "record stands as issued; a cleaner photo can re-issue it."
        ),
        "mask_warnings": warnings,
    }


def _exif_capture_time(path: Path) -> str | None:
    """The capture time a photo file states about itself, via
    Pillow's EXIF reader (DateTimeOriginal preferred, DateTime as
    fallback). HEIC files Pillow cannot open without a plugin
    return None here; the Rust build reads those through its
    container parser — the file hash binds either way."""
    try:
        from PIL import Image

        with Image.open(path) as img:
            exif = img.getexif()
            if not exif:
                return None
            dt = None
            try:
                dt = exif.get_ifd(0x8769).get(0x9003)
            except Exception:
                dt = None
            dt = dt or exif.get(0x0132)
            if isinstance(dt, bytes):
                dt = dt.decode("ascii", "replace")
            if isinstance(dt, str) and len(dt) >= 10 and dt[4] == ":":
                return dt.strip("\x00").strip()
    except Exception:
        return None
    return None


def _verify_ownership_record(store: AssetStore, receipt: dict) -> dict:
    """Independent verification of an ownership record, mirroring
    the Rust build: re-derive the crop from the stored source
    photo's pixels and compare pixel hashes. "0 changed" is a
    recomputation: the crop provably comes from exactly the stored
    source pixels, and the receipt's source file hash binds those
    pixels to the original file as ingested."""
    from io import BytesIO

    from .assets import _image_facts

    problems: list[str] = []
    checks: list[dict] = []

    def note(name: str, ok: bool, detail: str) -> None:
        if not ok:
            problems.append(f"{name}: {detail}")
        checks.append({"name": name, "ok": ok, "detail": detail})

    crop_checked = 0
    crop_changed = 0
    source = receipt.get("source") or {}
    crop = receipt.get("crop") or {}
    try:
        source_image = store.read_image(source.get("asset"))
    except Exception as exc:  # noqa: BLE001 — a verify failure is data
        source_image = None
        note("source_pixels_match", False, f"source photo unreadable: {exc}")
    if source_image is not None:
        buf = BytesIO()
        source_image.save(buf, format="PNG")
        actual = _image_facts(buf.getvalue())[0]
        claimed = source.get("pixel_sha256")
        note(
            "source_pixels_match",
            actual == claimed,
            f"stored source pixels hash {actual}",
        )
        rect = crop.get("rect_px") or []
        if len(rect) == 4 and rect[2] > rect[0] and rect[3] > rect[1]:
            recomputed = source_image.crop(tuple(rect))
            crop_checked = recomputed.width * recomputed.height
            buf = BytesIO()
            recomputed.save(buf, format="PNG")
            actual_crop = _image_facts(buf.getvalue())[0]
            claimed_crop = crop.get("pixel_sha256")
            if actual_crop == claimed_crop:
                note(
                    "crop_recomputed_from_source",
                    True,
                    "re-cropping the stored source at the receipt's rect reproduces the crop pixel hash",
                )
            else:
                # Count differing pixels on the raw RGBA bytes
                # when the stored crop has the same shape, so
                # "changed" stays a count, never a guess.
                try:
                    stored_crop = store.read_image(crop.get("asset"))
                    if stored_crop.size == recomputed.size:
                        a = stored_crop.convert("RGBA").tobytes()
                        b = recomputed.convert("RGBA").tobytes()
                        crop_changed = sum(
                            1
                            for i in range(0, len(a), 4)
                            if a[i : i + 4] != b[i : i + 4]
                        )
                except Exception:
                    crop_changed = 0
                note(
                    "crop_recomputed_from_source",
                    False,
                    f"recomputed crop hash {actual_crop} != receipt {claimed_crop}",
                )
        else:
            note(
                "crop_recomputed_from_source",
                False,
                "receipt crop rect is missing or empty",
            )
    return {
        "ok": not problems,
        "method": "ownership_recompute",
        "checks": checks,
        "problems": problems,
        "crop_pixels_checked": crop_checked,
        "crop_pixels_changed": crop_changed,
    }


def _keep_primary(
    ctx: _RunContext, record: dict, sources: list[PhotoSource], primary: dict
) -> None:
    """Issue the ownership record for one keep item, in place.

    identify + cut_out only: no listing copy, no staging, no draw.
    The crop identify (the same single call the listing path rides)
    supplies the condition notes. The receipt binds the full source
    FILE's hash and its EXIF capture time, both read from the
    seller's file at ingest, plus the crop's own proof. Its claim
    is exactly: this crop comes from exactly this file, captured at
    the time the file states, unchanged since — never that the file
    was unedited at capture, which no tool can know.
    """
    name = record["name"]
    record["primary_photo_index"] = primary["photo_index"]
    record["also_seen_in"] = [
        s["photo_index"]
        for s in record["sightings"]
        if s["status"] == "ok" and s is not primary
    ]

    def fail(code: str, message: str) -> None:
        record["failure"] = {"stage": "ownership", "code": code, "message": message}

    source = sources[primary["photo_index"]]
    bbox = primary.get("bbox_normalized")
    if not bbox or source.handle is None:
        fail(
            "OWNERSHIP_INCOMPLETE",
            f"no foreground box was recorded for {name}; there is no crop "
            "to bind into an ownership record",
        )
        return
    try:
        image = ctx.store.read_image(source.handle)
    except Exception as exc:  # noqa: BLE001 — recorded, not raised
        fail(
            "OWNERSHIP_INCOMPLETE",
            f"the source photo for {name} could not be read back ({exc}); "
            "the ownership record cannot be issued",
        )
        return
    source_record = ctx.store.get_record(source.handle)
    width, height = image.width, image.height
    box_w = (bbox[2] - bbox[0]) * width
    box_h = (bbox[3] - bbox[1]) * height
    x0 = max(0, math.floor(bbox[0] * width - 0.10 * box_w))
    y0 = max(0, math.floor(bbox[1] * height - 0.10 * box_h))
    x1 = min(width, math.ceil(bbox[2] * width + 0.10 * box_w))
    y1 = min(height, math.ceil(bbox[3] * height + 0.10 * box_h))
    if x1 <= x0 or y1 <= y0:
        fail(
            "OWNERSHIP_INCOMPLETE",
            f"the crop box for {name} was empty; the ownership record "
            "cannot be issued",
        )
        return
    crop = image.crop((x0, y0, x1, y1))
    crop_handle = ctx.store.put_image(
        crop, kind=KIND_CROP, source_asset_id=AssetStore.parse_handle(source.handle)
    )
    crop_record = ctx.store.get_record(crop_handle)
    # Condition notes ride the crop identify (one billed call).
    try:
        result = ctx.chain["identify"](crop_handle)
    except Exception as exc:  # noqa: BLE001 — the record still issues
        result = {
            "status": "error",
            "error": {"code": "IDENTIFY_FAILED", "message": str(exc)},
        }
    record["cost_usd"] += _call_cost(result)
    plan = result if result.get("status") == "ok" else None
    if plan is not None and isinstance(plan.get("plan"), dict):
        plan = plan["plan"]
    condition_notes = plan.get("condition_notes") if plan else None
    condition_note = None
    if plan is None:
        condition_note = (
            f"identify did not return a plan for the crop of {name}; the "
            "record carries no condition notes rather than borrowed ones"
        )
    # The file-level binding: hash + stated capture time of the
    # original file, read from the seller's own bytes.
    try:
        file_bytes = source.path.read_bytes()
        file_sha256: str | None = hashlib.sha256(file_bytes).hexdigest()
    except Exception:
        file_sha256 = None
    receipt = {
        "receipt_type": "ownership",
        "receipt_schema": 1,
        "item": name,
        "intent": "keep",
        "recorded_at": datetime.now(timezone.utc).isoformat(),
        "claim": (
            "this crop comes from exactly this file, captured at the "
            "time the file states, unchanged since"
        ),
        "source": {
            "asset": source.handle,
            "file_name": source.path.name,
            "file_sha256": file_sha256,
            "pixel_sha256": source_record.pixel_sha256,
            "exif_capture_time": _exif_capture_time(source.path),
            "width": width,
            "height": height,
        },
        "crop": {
            "asset": crop_handle,
            "rect_px": [x0, y0, x1, y1],
            "pixel_sha256": crop_record.pixel_sha256,
            "sha256": crop_record.sha256,
        },
        "foreground_asset": primary.get("foreground_asset"),
        "mask_asset": primary.get("mask_asset"),
        "condition_notes": condition_notes,
        "condition_source": "identify (muse-spark-1.3) over the item crop",
        "condition_note": condition_note,
        "cut_quality": _keep_cut_quality(primary),
        "identify_call_id": result.get("call_id"),
    }
    # Provenance signals (mirrors the Rust build): read from the
    # same original file bytes the binding above hashes; a keeper
    # whose file carries location coordinates also gets its
    # metadata-free public copy minted here, parented to the
    # source asset. Additive: the ownership verifier reads named
    # fields only.
    try:
        keep_bytes = source.path.read_bytes()
    except Exception:
        keep_bytes = b""
    if keep_bytes:
        from . import provenance as _provenance

        block = _provenance.provenance_block(keep_bytes, (width, height))
        _provenance.attach_public_copy(
            block, ctx.store, keep_bytes, source_record.asset_id
        )
        receipt["provenance"] = block
    ctx.store.save_receipt(crop_handle, receipt)
    record["output_asset"] = crop_handle
    record["receipt"] = receipt
    record["ownership"] = receipt
    receipt_path = ctx.store.receipt_path(crop_handle)
    record["receipt_file"] = str(receipt_path) if receipt_path.exists() else None
    record["stage_reached"] = "ownership"
    verification = _verify_ownership_record(ctx.store, receipt)
    record["verification"] = verification
    if verification["ok"]:
        record["status"] = "kept"
        record["stage_reached"] = "done"
        record["failure"] = None
    else:
        record["status"] = "verify_failed"
        problems = verification["problems"]
        record["failure"] = {
            "stage": "verify",
            "code": "VERIFY_FAILED",
            "message": (
                "the ownership record did not pass independent verification: "
                + ("; ".join(problems) if problems else "verifier reported failure")
            ),
        }


def _build_keep_inventory(
    store: AssetStore, out_dir: Path, room_id: str, records: list[dict]
) -> dict | None:
    """Collect a run's kept items into the ownership inventory:
    inventory.json plus a human-readable inventory.html sheet,
    both rendered from the same entries (mirrors the Rust build).
    Returns the manifest section, or None when nothing was kept."""
    import html as html_mod

    entries: list[dict] = []
    rows: list[str] = []
    for item in records:
        if item.get("intent") != "keep" or item.get("status") != "kept":
            continue
        ownership = item.get("ownership") or {}
        canonical = json.dumps(
            ownership, sort_keys=True, separators=(",", ":"), ensure_ascii=False
        )
        receipt_sha256 = hashlib.sha256(canonical.encode("utf-8")).hexdigest()
        source = ownership.get("source") or {}
        entry = {
            "name": item.get("name"),
            "crop_asset": item.get("output_asset"),
            "condition_notes": ownership.get("condition_notes"),
            "recorded_at": ownership.get("recorded_at"),
            "source_file": source.get("file_name"),
            "source_file_sha256": source.get("file_sha256"),
            "exif_capture_time": source.get("exif_capture_time"),
            "cut_grade": (ownership.get("cut_quality") or {}).get("grade"),
            "location_warning": (ownership.get("provenance") or {}).get(
                "location_warning"
            ),
            "public_copy": (
                (ownership.get("provenance") or {}).get("location") or {}
            ).get("public_copy"),
            "receipt_sha256": receipt_sha256,
            "receipt_file": item.get("receipt_file"),
        }
        entries.append(entry)
        try:
            crop_bytes = store.read_bytes(item.get("output_asset"))
            img_tag = (
                '<img alt="crop" style="max-width:240px" '
                f'src="data:image/png;base64,{base64.b64encode(crop_bytes).decode()}">'
            )
        except Exception:
            img_tag = "<em>crop image unavailable</em>"

        def text_of(value: object) -> str:
            return html_mod.escape(str(value)) if value else "—"

        rows.append(
            "<tr>"
            f"<td>{img_tag}</td>"
            f"<td><strong>{text_of(entry['name'])}</strong></td>"
            f"<td>{text_of(entry['condition_notes'])}</td>"
            f"<td>{text_of(entry['exif_capture_time'])}</td>"
            f'<td class="mono">{receipt_sha256}</td>'
            "</tr>"
        )
        if entry.get("location_warning"):
            rows.append(
                "<tr><td></td>"
                f'<td colspan="4"><strong>Location warning:</strong> '
                f"{text_of(entry['location_warning'])}</td></tr>"
            )
    if not entries:
        return None
    inventory = {
        "room_id": room_id,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "items": entries,
    }
    (out_dir / "inventory.json").write_text(
        json.dumps(inventory, indent=2, sort_keys=True), encoding="utf-8"
    )
    sheet = (
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n"
        f"<title>Ownership inventory — room {html_mod.escape(room_id)}</title>\n"
        "<style>\n"
        "body { font-family: system-ui, sans-serif; color: #111; max-width: 60rem; margin: 2rem auto; padding: 0 1rem; }\n"
        "table { border-collapse: collapse; width: 100%; } td { border-top: 1px solid #ddd; padding: 0.5rem; vertical-align: top; }\n"
        ".mono { font-family: ui-monospace, Menlo, monospace; font-size: 0.8em; word-break: break-all; }\n"
        "</style>\n</head>\n<body>\n"
        "<h1>Ownership inventory</h1>\n"
        f"<p>Items the owner marked to keep, recorded during room run "
        f'<span class="mono">{html_mod.escape(room_id)}</span>. '
        "Each crop is bound by its receipt to the exact source photo file "
        "it came from, unchanged since ingest.</p>\n"
        "<table>\n"
        "<tr><td><strong>Crop</strong></td><td><strong>Item</strong></td>"
        "<td><strong>Condition notes</strong></td>"
        "<td><strong>Captured (per file)</strong></td>"
        "<td><strong>Receipt SHA-256</strong></td></tr>\n"
        + "\n".join(rows)
        + "\n</table>\n</body>\n</html>\n"
    )
    (out_dir / "inventory.html").write_text(sheet, encoding="utf-8")
    return {
        "json_file": "inventory.json",
        "sheet_file": "inventory.html",
        "items": len(entries),
    }


def _process_item(
    ctx: _RunContext,
    item: dict,
    sources: list[PhotoSource],
    *,
    probe_all: bool,
) -> dict:
    """Run one item through probe -> primary -> draw; build its record."""
    name = item["name"]
    record: dict = {
        "name": name,
        "status": "failed",
        "stage_reached": "probe",
        "failure": None,
        "narration": item.get("narration"),
        "origin": item.get("origin", "named"),
        "intent": _intent_of_origin(item.get("origin")),
        "stage_mode": ctx.stage_mode,
        "sightings": [],
        "primary_photo_index": None,
        "also_seen_in": [],
        "output_asset": None,
        "receipt": None,
        "receipt_file": None,
        "verification": None,
        "listing": None,
        "ownership": None,
        "pricing": None,
        "possible_duplicate_of": [],
        "flags": list(item.get("flags", [])),
        "cost_usd": 0.0,
    }
    ctx.sam_calls = 0
    sightings = _probe_item(
        ctx,
        name,
        sources,
        item["candidate_photo_indices"],
        stop_on_success=not probe_all,
    )
    record["sightings"] = sightings
    record["cost_usd"] += sum(s.get("cost_usd", 0.0) for s in sightings)

    successes = [s for s in sightings if s["status"] == "ok"]
    if not successes:
        errors = [s.get("error") or {} for s in sightings if s["status"] == "error"]
        first = errors[0] if errors else {}
        code = first.get("code", "NO_MATCH")
        if record["origin"] == "narration_mentioned":
            # A mentioned item the photos refute is a NO_MATCH by
            # definition: it was probed and not found. The underlying
            # probe code stays visible in the sighting records.
            code = "NO_MATCH"
        record["failure"] = {
            "stage": "cut_out",
            "code": code,
            "message": (
                f"no candidate photo yielded a cut-out for {name!r} "
                f"({len(sightings)} photo(s) probed)"
            ),
        }
        return record

    primary = sorted(
        successes, key=lambda s: _sighting_primary_key(sources, s)
    )[0]
    if record["origin"] == "narration_keep":
        # KEEP: identify + cut_out only. No listing, no staging,
        # no draw — the ownership record is the deliverable.
        _keep_primary(ctx, record, sources, primary)
        return record
    _stage_primary(ctx, record, sources, primary)
    return record


def _flag_cross_item_duplicates(
    records: list[dict], sources: list[PhotoSource], store: AssetStore | None = None
) -> None:
    """Flag (never merge) items that look like the same physical object.

    Evidence: identical primary-foreground pixel hashes, primary
    masks on the SAME photo whose normalized bboxes almost perfectly
    overlap (two names segmenting one object), or — at sighting
    level — any pair of successful sightings of the two items on
    the same photo whose foreground masks overlap at least
    MASK_OVERLAP_COLLISION of the smaller mask. The sighting-level
    evidence needs the store to read the masks from; without one it
    is skipped and the primary-level evidence still applies.
    """
    staged = [r for r in records if r["status"] == "staged"]
    by_hash: dict[str, list[dict]] = {}
    for record in staged:
        fg_hash = (record.get("receipt") or {}).get("foreground_pixel_sha256")
        if fg_hash:
            by_hash.setdefault(fg_hash, []).append(record)
    for group in by_hash.values():
        if len(group) > 1:
            for record in group:
                others = [g["name"] for g in group if g is not record]
                record["possible_duplicate_of"] = sorted(
                    set(record["possible_duplicate_of"]) | set(others)
                )
                record["flags"].append(
                    "primary foreground pixel hash is identical to "
                    + ", ".join(others)
                    + "; listings kept separate pending a human call"
                )
    primaries = {}
    for record in staged:
        for sighting in record["sightings"]:
            if sighting["photo_index"] == record["primary_photo_index"]:
                primaries[record["name"]] = (record, sighting)
    names = list(primaries)
    for i, name_a in enumerate(names):
        for name_b in names[i + 1 :]:
            rec_a, sight_a = primaries[name_a]
            rec_b, sight_b = primaries[name_b]
            if sight_a["photo_index"] != sight_b["photo_index"]:
                continue
            iou = bbox_iou(
                sight_a.get("bbox_normalized"), sight_b.get("bbox_normalized")
            )
            if iou >= MASK_IOU_FLAG:
                for rec, other in ((rec_a, name_b), (rec_b, name_a)):
                    if other not in rec["possible_duplicate_of"]:
                        rec["possible_duplicate_of"].append(other)
                    rec["flags"].append(
                        f"primary masks on photo {sight_a['photo_index']} "
                        f"overlap {other!r} with IoU {iou:.2f}; kept separate"
                    )
    if store is not None:
        _flag_sighting_level_duplicates(staged, store)


def _flag_sighting_level_duplicates(staged: list[dict], store: AssetStore) -> None:
    """Mutual possible-duplicate flags from sighting-level collisions.

    Any colliding sighting pair (see _colliding_sighting_pairs)
    between two staged items links them in both directions, whether
    or not either sighting is a primary.
    """
    for i, rec_a in enumerate(staged):
        for rec_b in staged[i + 1 :]:
            for sight_a, sight_b, overlap in _colliding_sighting_pairs(
                store, rec_a, rec_b
            ):
                photo_index = sight_a["photo_index"]
                for rec, sight, other in (
                    (rec_a, sight_a, rec_b["name"]),
                    (rec_b, sight_b, rec_a["name"]),
                ):
                    if other not in rec["possible_duplicate_of"]:
                        rec["possible_duplicate_of"].append(other)
                    kind = (
                        "primary sighting"
                        if sight["photo_index"] == rec["primary_photo_index"]
                        else "sighting"
                    )
                    rec["flags"].append(
                        f"{kind} on photo {photo_index} overlaps a "
                        f"sighting of {other!r} on the same photo with "
                        f"mask overlap {overlap:.2f} of the smaller "
                        "mask; kept separate"
                    )


# ---------------------------------------------------------------------------
# Collision re-selection: two staged items, one physical object
# ---------------------------------------------------------------------------


def _mask_overlap_fraction(
    store: AssetStore, handle_a: str | None, handle_b: str | None
) -> float:
    """Overlap of two stored foreground masks, as a fraction of the
    smaller mask: |A and B| / min(|A|, |B|) over opaque (alpha > 0)
    pixels, read from the stored foreground assets.

    Masks are only ever compared for sightings on the same source
    photo, where the foregrounds share the photo's dimensions. If
    two assets nevertheless differ in size, both are padded with
    transparent pixels to the common canvas (top-left aligned)
    before counting. A missing or empty mask overlaps nothing.
    """
    if not handle_a or not handle_b:
        return 0.0
    try:
        import numpy as np

        mask_a = np.asarray(store.read_image(handle_a).getchannel("A")) > 0
        mask_b = np.asarray(store.read_image(handle_b).getchannel("A")) > 0
    except Exception:
        return 0.0
    if mask_a.shape != mask_b.shape:
        height = max(mask_a.shape[0], mask_b.shape[0])
        width = max(mask_a.shape[1], mask_b.shape[1])
        padded_a = np.zeros((height, width), dtype=bool)
        padded_a[: mask_a.shape[0], : mask_a.shape[1]] = mask_a
        padded_b = np.zeros((height, width), dtype=bool)
        padded_b[: mask_b.shape[0], : mask_b.shape[1]] = mask_b
        mask_a, mask_b = padded_a, padded_b
    smaller = min(int(np.count_nonzero(mask_a)), int(np.count_nonzero(mask_b)))
    if smaller == 0:
        return 0.0
    return int(np.count_nonzero(mask_a & mask_b)) / smaller


def _successful_sightings(record: dict) -> list[dict]:
    """A record's successful sightings that carry a foreground."""
    return [
        s
        for s in record.get("sightings", [])
        if s.get("status") == "ok" and s.get("foreground_asset")
    ]


def _sightings_by_photo(record: dict) -> dict[int, list[dict]]:
    """A record's successful sightings grouped by photo index."""
    by_photo: dict[int, list[dict]] = {}
    for sighting in _successful_sightings(record):
        by_photo.setdefault(sighting["photo_index"], []).append(sighting)
    return by_photo


def _colliding_sighting_pairs(
    store: AssetStore, rec_a: dict, rec_b: dict
) -> list[tuple[dict, dict, float]]:
    """Sighting pairs of two items that are the same object instance.

    Every pair of successful sightings, one from each record, on
    the same photo whose foreground masks overlap at least
    MASK_OVERLAP_COLLISION of the smaller mask. Returns
    (sighting_a, sighting_b, overlap) triples, in photo order.
    """
    by_photo_b = _sightings_by_photo(rec_b)
    pairs: list[tuple[dict, dict, float]] = []
    for sight_a in _successful_sightings(rec_a):
        for sight_b in by_photo_b.get(sight_a["photo_index"], []):
            overlap = _mask_overlap_fraction(
                store,
                sight_a.get("foreground_asset"),
                sight_b.get("foreground_asset"),
            )
            if overlap >= MASK_OVERLAP_COLLISION:
                pairs.append((sight_a, sight_b, overlap))
    return pairs


def _primary_sighting(record: dict) -> dict | None:
    """The successful sighting a staged record is staged from."""
    for sighting in record.get("sightings", []):
        if (
            sighting.get("status") == "ok"
            and sighting.get("photo_index") == record.get("primary_photo_index")
        ):
            return sighting
    return None


def _listing_evidence_plan(record: dict, sources: list[PhotoSource]) -> dict | None:
    """The plan a staged item's own listing evidence came from.

    photo_plan listings carry the primary photo's identify plan;
    item_crop listings carry the crop identify's plan, represented
    by the concept that plan returned (stored on the listing as
    plan_concept when the crop ran, with the listing title as the
    fallback for records that predate the field). A "none" listing
    is no evidence.
    """
    listing = record.get("listing") or {}
    source_kind = listing.get("source")
    if source_kind == "photo_plan":
        primary_index = record.get("primary_photo_index")
        if primary_index is None:
            return None
        return sources[primary_index].plan
    if source_kind == "item_crop":
        concept = listing.get("plan_concept") or listing.get("title")
        if not concept:
            return None
        return {"concept": concept}
    return None


def _evidence_names_only(
    record: dict, other_name: str, sources: list[PhotoSource]
) -> bool:
    """True when an item's listing evidence names it, and only it.

    The match is the existing plan-match machinery applied to the
    item's evidence plan: the evidence's concept must match the
    item's own name and must not match the other item's name.
    """
    plan = _listing_evidence_plan(record, sources)
    if plan is None:
        return False
    return plan_matches_item(plan, record["name"]) and not plan_matches_item(
        plan, other_name
    )


_STAGED_RESULT_FIELDS = (
    "primary_photo_index",
    "also_seen_in",
    "listing",
    "output_asset",
    "receipt",
    "receipt_file",
    "verification",
    "status",
    "stage_reached",
    "failure",
)


def _restage_from_sighting(
    ctx: _RunContext, record: dict, sources: list[PhotoSource], sighting: dict
) -> bool:
    """Re-stage a displaced item from an alternate sighting.

    Runs the full staging path (draw in the run's stage mode,
    receipt, independent verification, listing recomputed from the
    new primary). On success the new staged result stands. On any
    failure the original staged result is restored field for field,
    except cost: the re-stage's calls were really spent, so their
    cost stays counted into the item.
    """
    snapshot = {key: copy.deepcopy(record.get(key)) for key in _STAGED_RESULT_FIELDS}
    cost_before = record["cost_usd"]
    if _stage_primary(ctx, record, sources, sighting):
        return True
    attempt_cost = record["cost_usd"] - cost_before
    for key, value in snapshot.items():
        record[key] = value
    record["cost_usd"] = cost_before + attempt_cost
    return False


def _collision_contest(
    ctx: _RunContext, rec_a: dict, rec_b: dict, sources: list[PhotoSource]
) -> tuple[dict, dict, int] | None:
    """(keeper, displaced, contested photo index) for one pair, or
    None when the pair is not a displacement contest.

    A contest exists when some colliding sighting pair (see
    _colliding_sighting_pairs) involves at least one item's
    primary: the colliding object is then claimed by a staged
    listing. The keeper is the item whose own listing evidence
    names it and not the other item, when exactly one item
    qualifies. The displaced item is the other one, and only when
    its own primary is one of the colliding sightings: a keeper
    whose primary is the contested object does not displace an
    item whose primary sits elsewhere, and a keeper whose own
    primary is not the contested object is never moved.
    """
    primary_a, primary_b = _primary_sighting(rec_a), _primary_sighting(rec_b)
    if primary_a is None or primary_b is None:
        return None
    pairs = _colliding_sighting_pairs(ctx.store, rec_a, rec_b)
    if not pairs:
        return None
    a_contested = any(
        sight_a["photo_index"] == primary_a["photo_index"] for sight_a, _, _ in pairs
    )
    b_contested = any(
        sight_b["photo_index"] == primary_b["photo_index"] for _, sight_b, _ in pairs
    )
    if not a_contested and not b_contested:
        return None  # colliding sightings, but neither primary claims the object
    a_only = _evidence_names_only(rec_a, rec_b["name"], sources)
    b_only = _evidence_names_only(rec_b, rec_a["name"], sources)
    if a_only and not b_only:
        keeper, displaced, displaced_contested = rec_a, rec_b, b_contested
    elif b_only and not a_only:
        keeper, displaced, displaced_contested = rec_b, rec_a, a_contested
    else:
        return None  # no unique keeper: flags stand
    if not displaced_contested:
        return None
    return keeper, displaced, displaced["primary_photo_index"]


def _find_alternate_sighting(
    ctx: _RunContext,
    displaced: dict,
    keeper: dict,
    sources: list[PhotoSource],
) -> dict | None:
    """The displaced item's largest successful sighting that does
    not collide with ANY of the keeper's successful sightings, in
    primary-selection order; None when every successful sighting
    collides. A candidate on a photo where the keeper has no
    successful sighting cannot be the contested instance and
    qualifies on that ground alone."""
    keeper_by_photo = _sightings_by_photo(keeper)
    successes = _successful_sightings(displaced)
    successes.sort(key=lambda s: _sighting_primary_key(sources, s))
    for sighting in successes:
        collides = any(
            _mask_overlap_fraction(
                ctx.store,
                sighting.get("foreground_asset"),
                keeper_sighting.get("foreground_asset"),
            )
            >= MASK_OVERLAP_COLLISION
            for keeper_sighting in keeper_by_photo.get(sighting["photo_index"], [])
        )
        if not collides:
            return sighting
    return None


def _resolve_primary_collisions(
    ctx: _RunContext,
    records: list[dict],
    sources: list[PhotoSource],
    *,
    budget_ok=None,
) -> list[str]:
    """One re-selection pass over staged items in collision contests.

    Contests are detected at sighting level (see _collision_contest):
    a colliding sighting pair that involves a primary, with exactly
    one evidence keeper, displaces the item whose primary is
    contested and is not the keeper. The displaced item re-stages
    from its largest successful sighting that does not collide with
    any of the keeper's successful sightings (see
    _find_alternate_sighting). The pass runs once: an item already
    re-selected is not examined again, and a re-selected primary
    that collides with a third item is flagged, not iterated.
    Duplicate flags are never removed by a re-selection. Returns
    manifest notes.
    """
    notes: list[str] = []
    reselected: set[str] = set()
    staged = [r for r in records if r["status"] == "staged"]
    for i, rec_a in enumerate(staged):
        for rec_b in staged[i + 1 :]:
            if rec_a["name"] in reselected or rec_b["name"] in reselected:
                continue
            if rec_a["status"] != "staged" or rec_b["status"] != "staged":
                continue
            contest = _collision_contest(ctx, rec_a, rec_b, sources)
            if contest is None:
                continue  # no contest, or no unique keeper: flags stand
            keeper, displaced, contested_photo = contest
            alternate = _find_alternate_sighting(ctx, displaced, keeper, sources)
            if alternate is None:
                continue  # no clean alternate: the duplicate flag stands
            if budget_ok is not None and not budget_ok(alternate["photo_index"]):
                displaced["flags"].append(
                    "primary re-selection after duplicate collision with "
                    f"{keeper['name']!r} was skipped: the re-stage would "
                    "exceed the run budget; the duplicate flag stands"
                )
                notes.append(
                    f"primary re-selection for {displaced['name']!r} after "
                    f"duplicate collision with {keeper['name']!r} was "
                    "skipped: the re-stage would exceed the run budget"
                )
                continue
            if not _restage_from_sighting(ctx, displaced, sources, alternate):
                displaced["flags"].append(
                    "primary re-selection after duplicate collision with "
                    f"{keeper['name']!r} failed; the original primary "
                    "and the duplicate flag stand"
                )
                notes.append(
                    f"primary re-selection for {displaced['name']!r} after "
                    f"duplicate collision with {keeper['name']!r} failed; "
                    "the original staged result stands"
                )
                continue
            reselected.add(displaced["name"])
            displaced["flags"].append("primary_reselected_after_duplicate_collision")
            displaced["flags"].append(
                f"primary re-selected from photo {alternate['photo_index']} "
                f"after duplicate collision with {keeper['name']!r} "
                f"on photo {contested_photo}"
            )
            notes.append(
                f"primary re-selected for {displaced['name']!r} after "
                f"duplicate collision with {keeper['name']!r}: re-staged "
                f"from photo {alternate['photo_index']}"
            )
            # No second pass: a fresh primary that collides with a
            # third item is flagged on the displaced item only.
            _flag_third_collisions(ctx, displaced, keeper, alternate, staged)
    return notes


def _flag_third_collisions(
    ctx: _RunContext,
    displaced: dict,
    keeper: dict,
    alternate: dict,
    staged: list[dict],
) -> None:
    """Flag a re-selected primary that also collides with any
    successful sighting of a third staged item, on the alternate's
    photo. The pass does not iterate: the flag is the outcome."""
    for other in staged:
        if other is displaced or other is keeper or other["status"] != "staged":
            continue
        other_sightings = [
            s
            for s in _successful_sightings(other)
            if s["photo_index"] == alternate["photo_index"]
        ]
        collides = any(
            _mask_overlap_fraction(
                ctx.store,
                alternate.get("foreground_asset"),
                other_sighting.get("foreground_asset"),
            )
            >= MASK_OVERLAP_COLLISION
            for other_sighting in other_sightings
        )
        if not collides:
            continue
        if other["name"] not in displaced["possible_duplicate_of"]:
            displaced["possible_duplicate_of"].append(other["name"])
        displaced["flags"].append(
            f"re-selected primary on photo {alternate['photo_index']} "
            f"also overlaps {other['name']!r}; flagged, not re-selected again"
        )


def run_room(
    still_paths: list[Path],
    video_paths: list[Path],
    out_dir: Path,
    *,
    item_names: list[str] | None = None,
    auto: bool = False,
    scene: str = DEFAULT_SCENE,
    room_id: str | None = None,
    budget_usd: float = 1.00,
    frame_every_seconds: float = 2.0,
    max_frames_per_video: int = 8,
    max_photos_per_item: int = MAX_PHOTOS_PER_ITEM,
    probe_all_candidates: bool = True,
    store: AssetStore | None = None,
    chain: dict | None = None,
    spend_probe=None,
    verify_fn=None,
    ledger_path: Path | None = None,
    narrated_videos: list[Path] | None = None,
    narrated_audios: list[Path] | None = None,
    narration_fn=None,
    narration_max_projected_usd: float = NARRATION_MAX_PROJECTED_USD,
    stage_mode: str = "scene",
    comps_data: dict | None = None,
) -> dict:
    """Run one room end to end and return (and write) its manifest.

    probe_all_candidates: when True (the default), every ranked
    candidate photo is probed for sightings even after a success —
    that cross-photo evidence is what the de-dup record is built
    from. When False, probing stops at the first success per item
    (cheaper, thinner manifest).

    ledger_path: the run's own cost ledger. The default is
    ledger.jsonl inside out_dir. For the run's duration
    AGENTHAND_LEDGER_PATH points at this ledger, so every receipt
    billed_call writes for the run is mirrored into it; the previous
    value is restored on exit. The manifest records the path and its
    total is this ledger's delta across the run. The spend cap is
    checked against the cumulative ledger, never this one (fix pass
    F4), so the run's spend also lands there.

    verify_fn: (store, composite_handle) -> verifier report. The
    default is the repo's independent verifier
    (composite.verify_receipt_from_disk) — every staged item is
    re-proven from the stored files before the manifest calls it
    staged.

    narrated_videos / narrated_audios: inputs whose narration names
    the items. Each gets ONE Baseten Muse Spark Open call through
    narration_fn (default: _default_narration_fn) after its audio is
    extracted and measured; the call's projection is refused above
    narration_max_projected_usd. Selling items from the transcript
    join the work list as named items; mentioned items join flagged,
    for the visual pipeline to confirm or refute. Narration is
    billed through meta_client.billed_call (reserved against the spend
    cap, pending + settled ledger rows; fix pass F3) and lands in the
    manifest's narration section, never in total_cost_usd: the room's
    spend baseline is taken after narration.

    stage_mode: "scene" (default) stages through the model draw
    chain; "wallpaper" / "studio_gray" stage on code-rendered
    plates (see the module docstring). The manifest records the
    mode at top level and on every item record.

    comps_data: agent-supplied Marketplace comps (the parsed --comps
    JSON). Every item record gains a deterministic pricing record
    from market_pricing; when comps are supplied the run also
    writes the priced table artifacts (priced-table.png,
    priced-table.html, pricing-notes.md) into out_dir.
    """
    store = store if store is not None else AssetStore(config.get_data_dir())
    chain = chain if chain is not None else _default_chain()
    verify_fn = verify_fn if verify_fn is not None else _default_verify
    room_id = room_id or f"room-{uuid.uuid4().hex[:8]}"
    out_dir = Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    room_ledger = (
        Path(ledger_path) if ledger_path is not None else out_dir / "ledger.jsonl"
    )

    # The room's cost stays provable from the room directory alone:
    # every billed call during the run is MIRRORED into the room's own
    # JSONL ledger (AGENTHAND_LEDGER_PATH, pinned here for the run's
    # duration and restored afterwards), and the run's totals are read
    # back from that file. The spend cap is not the room's (fix pass
    # F4): reservations and settles always go to the cumulative ledger
    # too, so the cap sees every run's spend.
    previous_ledger_env = os.environ.get("AGENTHAND_LEDGER_PATH")
    os.environ["AGENTHAND_LEDGER_PATH"] = str(room_ledger)
    try:
        return _run_room_body(
            still_paths,
            video_paths,
            out_dir,
            item_names=item_names,
            auto=auto,
            scene=scene,
            room_id=room_id,
            budget_usd=budget_usd,
            frame_every_seconds=frame_every_seconds,
            max_frames_per_video=max_frames_per_video,
            max_photos_per_item=max_photos_per_item,
            probe_all_candidates=probe_all_candidates,
            store=store,
            chain=chain,
            spend_probe=spend_probe,
            verify_fn=verify_fn,
            room_ledger=room_ledger,
            narrated_videos=list(narrated_videos or []),
            narrated_audios=list(narrated_audios or []),
            narration_fn=narration_fn,
            narration_max_projected_usd=narration_max_projected_usd,
            stage_mode=stage_mode,
            comps_data=comps_data,
        )
    finally:
        if previous_ledger_env is None:
            os.environ.pop("AGENTHAND_LEDGER_PATH", None)
        else:
            os.environ["AGENTHAND_LEDGER_PATH"] = previous_ledger_env


def _run_room_body(
    still_paths: list[Path],
    video_paths: list[Path],
    out_dir: Path,
    *,
    item_names: list[str] | None,
    auto: bool,
    scene: str,
    room_id: str,
    budget_usd: float,
    frame_every_seconds: float,
    max_frames_per_video: int,
    max_photos_per_item: int,
    probe_all_candidates: bool,
    store: AssetStore,
    chain: dict,
    spend_probe,
    verify_fn,
    room_ledger: Path,
    narrated_videos: list[Path],
    narrated_audios: list[Path],
    narration_fn,
    narration_max_projected_usd: float,
    stage_mode: str = "scene",
    comps_data: dict | None = None,
) -> dict:
    from . import server  # local import: server pulls in the MCP SDK

    if stage_mode not in backdrop.STAGE_MODES:
        raise ValueError(
            f"unknown stage_mode {stage_mode!r}; expected one of {backdrop.STAGE_MODES}"
        )

    if spend_probe is None:

        def spend_probe() -> float:  # noqa: F811 — closure default
            return ledger.ledger_total_usd(room_ledger)

    # -- sources ------------------------------------------------------------
    sources: list[PhotoSource] = []
    video_notes: list[dict] = []
    for path in collect_stills([Path(p) for p in still_paths]):
        sources.append(PhotoSource(path=path, origin="still"))
    frames_dir = out_dir / "frames"
    for video in video_paths:
        video = Path(video)
        frames, note, dropped = sample_video_frames(
            video,
            frames_dir / video.stem,
            every_seconds=frame_every_seconds,
            max_frames=max_frames_per_video,
        )
        video_notes.append(
            {
                "path": str(video),
                "frames_sampled": len(frames),
                "frames_dropped": dropped,
                "frame_every_seconds": frame_every_seconds,
                "note": note,
            }
        )
        for frame_path, timestamp_s in frames:
            sources.append(
                PhotoSource(
                    path=frame_path,
                    origin="video_frame",
                    video_path=str(video),
                    timestamp_s=timestamp_s,
                )
            )

    # -- narration (optional): the seller's voice names the items ------------
    narration_records: list[dict] = []
    narration_named: list[tuple[str, str]] = []  # (item name, source path)
    narration_keep: list[tuple[str, str]] = []
    narration_mentioned: list[tuple[str, str]] = []
    transcript_file: Path | None = None
    if narrated_videos or narrated_audios:
        effective_narration_fn = narration_fn or _default_narration_fn
        narration_dir = out_dir / "narration"
        for video in narrated_videos:
            narration_records.append(
                _narrate_one(
                    Path(video), "video", narration_dir, effective_narration_fn,
                    max_projected_usd=narration_max_projected_usd,
                )
            )
        for audio in narrated_audios:
            narration_records.append(
                _narrate_one(
                    Path(audio), "audio", narration_dir, effective_narration_fn,
                    max_projected_usd=narration_max_projected_usd,
                )
            )
        for record in narration_records:
            for name in record["named_items"]:
                narration_named.append((name, record["path"]))
            for name in record["kept_items"]:
                narration_keep.append((name, record["path"]))
            for name in record["mentioned_items"]:
                narration_mentioned.append((name, record["path"]))
        spoken = [r for r in narration_records if r["transcript"]]
        if spoken:
            transcript_file = out_dir / "transcript.txt"
            transcript_file.write_text(
                "\n\n".join(
                    f"# Narration transcript — {r['path']}\n{r['transcript']}"
                    for r in spoken
                )
                + "\n",
                encoding="utf-8",
            )

    # The room's Meta spend baseline, measured AFTER narration: the
    # Baseten narration calls are billed through the ledger (fix pass
    # F3, so the spend cap sees them) but stay out of the room's Meta
    # total and run budget, exactly as before.
    spend_before = spend_probe()

    # -- worst-case projection gate -----------------------------------------
    from . import meta_client

    def _dims(source: PhotoSource) -> tuple[int, int]:
        from PIL import Image

        with Image.open(source.path) as img:
            return img.width, img.height

    identify_projection = 0.0
    for source in sources:
        try:
            w, h = _dims(source)
        except Exception:
            w, h = 1200, 1600
        source.width, source.height = w, h
        identify_projection += (
            meta_client.project_identify_cost_usd(server.IDENTIFY_MODEL, w, h)
            * meta_client.IDENTIFY_MAX_ATTEMPTS
        )
    n_narration_items = (
        len(narration_named) + len(narration_keep) + len(narration_mentioned)
    )
    if item_names:
        n_items_estimate = len(item_names) + n_narration_items
    elif auto:
        n_items_estimate = len(sources) + n_narration_items
    else:
        n_items_estimate = n_narration_items
    per_item_projection = (
        MAX_SAM_CALLS_PER_ITEM
        * meta_client.project_cut_out_cost_usd(server.CUT_OUT_MODEL, 1)
        + meta_client.project_draw_cost_usd(server.DRAW_BACKGROUND_MODEL)
    )
    worst_case = identify_projection + n_items_estimate * per_item_projection
    notes: list[str] = []
    if worst_case > budget_usd:
        raise RuntimeError(
            f"Room run refused before any call: worst-case projection "
            f"${worst_case:.4f} exceeds the ${budget_usd:.2f} run budget "
            f"({len(sources)} photos, ~{n_items_estimate} items). Reduce "
            "the item set or the photo pool, or raise --budget-usd."
        )
    notes.append(
        f"worst-case projection ${worst_case:.4f} within the ${budget_usd:.2f} run budget"
    )
    for record in narration_records:
        if record["error"] is not None:
            notes.append(
                f"narration from {record['path']} failed "
                f"[{record['error']['code']}]: {record['error']['message']}"
            )
        else:
            notes.append(
                f"narration from {record['path']}: "
                f"{len(record['named_items'])} item(s) named for sale, "
                f"{len(record['kept_items'])} kept, "
                f"{len(record['mentioned_items'])} mentioned in passing"
            )

    # -- plan pass ------------------------------------------------------------
    for source in sources:
        result = chain["identify"](_data_uri(source.path))
        if result.get("status") == "ok":
            source.plan = result
            source.handle = result.get("image")
        else:
            source.plan_error = result.get("error") or {
                "code": "IDENTIFY_FAILED",
                "message": str(result),
            }
            try:
                source.handle = ingest.ingest(_data_uri(source.path), store)
            except Exception as exc:  # unreadable photo: record, move on
                source.plan_error = {
                    "code": "INGEST_FAILED",
                    "message": f"{source.plan_error.get('message', '')} | ingest: {exc}",
                }

    # -- work list -------------------------------------------------------------
    items: list[dict] = []
    if auto:
        candidates = [
            Candidate(name=source.plan["concept"], photo_index=i, plan=source.plan)
            for i, source in enumerate(sources)
            if source.plan and source.plan.get("concept")
        ]
        for group in group_candidates(candidates):
            items.append(
                {
                    "name": group["name"],
                    "candidate_photo_indices": group["photo_indices"],
                    "flags": group["flags"],
                    "origin": "named",
                }
            )
        mode = "auto"
    else:
        mode = "named"
        for name in item_names or []:
            items.append(
                {
                    "name": name,
                    "candidate_photo_indices": _rank_photos(name, sources),
                    "flags": [],
                    "origin": "named",
                }
            )
    # Narration items join the work list. A narration name matching an
    # item already listed (typed or enumerated) annotates that item
    # instead of duplicating it — the same keep-evidence, no-silent-
    # merge rule as the rest of the de-dup; a for-sale naming beats a
    # passing mention when both land on one item. A genuinely new
    # mentioned item is appended with its flag: it may stage only if
    # the visual pipeline confirms it against the photos.
    def _attach_narration(name: str, source_path: str, kind: str) -> None:
        for existing in items:
            if token_similarity(existing["name"], name) >= MERGE_SIMILARITY:
                current = existing.get("narration")
                # Intent precedence, as in the Rust build: an
                # explicit "named" (sell) beats "keep", and both
                # explicit intents beat a passing "mentioned".
                replace = current is None or (
                    current["kind"] == "mentioned" and kind in ("named", "keep")
                ) or (current["kind"] == "keep" and kind == "named")
                if replace:
                    existing["narration"] = {"kind": kind, "source": source_path}
                    if kind == "named" and existing.get("origin") in (
                        "narration_mentioned",
                        "narration_keep",
                    ):
                        existing["origin"] = "narration_selling"
                    elif kind == "keep" and existing.get("origin") == "narration_mentioned":
                        existing["origin"] = "narration_keep"
                return
        entry: dict = {
            "name": name,
            "candidate_photo_indices": _rank_photos(name, sources),
            "flags": [],
            "narration": {"kind": kind, "source": source_path},
            "origin": {
                "named": "narration_selling",
                "keep": "narration_keep",
                "mentioned": "narration_mentioned",
            }[kind],
        }
        if kind == "mentioned":
            entry["flags"].append(
                "mentioned in passing in the seller's narration, not "
                "marked for sale; included so the visual pipeline can "
                "confirm or refute it against the photos"
            )
        if kind == "keep":
            entry["flags"].append(
                "marked keep in the seller's narration: identified "
                "and cut out for an ownership record; never staged "
                "or listed for sale"
            )
        items.append(entry)

    for narrated_name, narrated_source in narration_named:
        _attach_narration(narrated_name, narrated_source, "named")
    for narrated_name, narrated_source in narration_keep:
        _attach_narration(narrated_name, narrated_source, "keep")
    for narrated_name, narrated_source in narration_mentioned:
        _attach_narration(narrated_name, narrated_source, "mentioned")
    if narration_records and not auto and not item_names:
        mode = "narrated"

    # Cap candidate probing per item. The cap is also the spend
    # bound the projection gate priced, but it can never drop a
    # still: every still is always a candidate, and video frames
    # fill the cap's remaining slots in rank order (probe order
    # inside the selected set stays the rank order).
    candidate_cap = min(max_photos_per_item, MAX_PHOTOS_PER_ITEM)
    for item in items:
        item["candidate_photo_indices"] = _select_candidate_indices(
            item["name"], item["candidate_photo_indices"], sources, candidate_cap
        )

    ctx = _RunContext(
        store=store,
        chain=chain,
        scene=scene,
        stage_mode=stage_mode,
        verify_fn=verify_fn,
        max_photos_per_item=max_photos_per_item,
    )

    records: list[dict] = []
    budget_stopped = False
    for item in items:
        if spend_probe() - spend_before > budget_usd:
            budget_stopped = True
        if budget_stopped:
            records.append(
                {
                    "name": item["name"],
                    "status": "failed",
                    "stage_reached": "none",
                    "failure": {
                        "stage": "budget",
                        "code": "BUDGET_STOP",
                        "message": (
                            f"run spend crossed the ${budget_usd:.2f} budget "
                            "before this item was attempted"
                        ),
                    },
                    "narration": item.get("narration"),
                    "origin": item.get("origin", "named"),
                    "intent": _intent_of_origin(item.get("origin")),
                    "stage_mode": stage_mode,
                    "sightings": [],
                    "primary_photo_index": None,
                    "also_seen_in": [],
                    "output_asset": None,
                    "receipt": None,
                    "receipt_file": None,
                    "verification": None,
                    "listing": None,
                    "ownership": None,
                    "pricing": None,
                    "possible_duplicate_of": [],
                    "flags": list(item.get("flags", [])),
                    "cost_usd": 0.0,
                }
            )
            continue
        records.append(
            _process_item(ctx, item, sources, probe_all=probe_all_candidates)
        )

    _flag_cross_item_duplicates(records, sources, store=store)

    # Collision re-selection post-pass, alongside the de-dup flags:
    # a displaced item may re-stage from a clean alternate sighting.
    # Its spend is real run spend (the manifest total below is read
    # after this pass), and a re-stage is only attempted when the
    # projected cost of one draw plus one crop identify, priced the
    # same way the up-front projection gate prices them, keeps the
    # run within its budget.
    def _restage_budget_ok(photo_index: int) -> bool:
        source = sources[photo_index]
        projected = meta_client.project_identify_cost_usd(
            server.IDENTIFY_MODEL, source.width, source.height
        ) * meta_client.IDENTIFY_MAX_ATTEMPTS
        if stage_mode not in backdrop.BACKDROP_STAGE_MODES:
            projected += meta_client.project_draw_cost_usd(
                server.DRAW_BACKGROUND_MODEL
            )
        return spend_probe() - spend_before + projected <= budget_usd

    notes.extend(
        _resolve_primary_collisions(
            ctx, records, sources, budget_ok=_restage_budget_ok
        )
    )

    # Pricing is a pure post-pass over the finished records: every
    # item gains its pricing record, comps or no comps.
    for record in records:
        record["pricing"] = market_pricing.pricing_for_item(record["name"], comps_data)

    # Comps pricing (category comps file, comps.py): each record's
    # listing also carries listing.pricing (range + recommended),
    # matched code-side from the item name and the identify-written
    # listing title. A --comps document in the category format (a
    # local `categories` list and/or a wider-area `categories_wide`
    # list) is used directly; otherwise the configured comps file is
    # read. Local comps price first; a category with none falls back
    # to its wide tier and its block says so (scope "wide" + note).
    if isinstance(comps_data, dict) and (
        "categories" in comps_data or "categories_wide" in comps_data
    ):
        try:
            comps_book: comps.LoadedBook = (comps.parse_book(comps_data), None)
        except comps.CompsFileError as exc:
            comps_book = (None, comps._one_line(str(exc)))
    else:
        comps_book = comps.load_configured_book()
    for record in records:
        listing = record.get("listing")
        if isinstance(listing, dict):
            title = listing.get("title")
            listing["pricing"] = comps.pricing_for_item_name(
                comps_book,
                record["name"],
                title if isinstance(title, str) else None,
            )

    total_cost = round(spend_probe() - spend_before, 6)
    narration_section = None
    baseten_cost = None
    if narration_records:
        usage_totals = {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
        for record in narration_records:
            usage = record.get("usage") or {}
            for key in usage_totals:
                usage_totals[key] += usage.get(key) or 0
        baseten_cost = round(
            sum(r["cost_usd_estimate"] or 0.0 for r in narration_records), 6
        )
        narration_section = {
            "model": NARRATION_MODEL,
            "pool": "baseten_partner_credits",
            "files": narration_records,
            "transcript_file": str(transcript_file) if transcript_file else None,
            "usage": usage_totals,
            "cost_usd_estimate": baseten_cost,
        }
    pricing_source = None
    if comps_data is not None:
        pricing_source = {
            "source": market_pricing.PRICING_SOURCE,
            "area": comps_data.get("area") if isinstance(comps_data, dict) else None,
            "radius_mi": comps_data.get("radius_mi") if isinstance(comps_data, dict) else None,
            "pulled_at": comps_data.get("pulled_at") if isinstance(comps_data, dict) else None,
        }
    manifest = {
        "room_id": room_id,
        "created_at": datetime.now(timezone.utc).isoformat(),
        "mode": mode,
        "stage_mode": stage_mode,
        "pricing_source": pricing_source,
        "room_style": scene,
        "budget_usd": budget_usd,
        "ledger_path": str(room_ledger),
        "total_cost_usd": total_cost,
        "baseten_cost_usd_estimate": baseten_cost,
        "narration": narration_section,
        "sources": [s.manifest_entry(i) for i, s in enumerate(sources)],
        "videos": video_notes,
        "items": records,
        "notes": notes,
    }
    manifest["inventory"] = _build_keep_inventory(store, out_dir, room_id, records)
    (out_dir / "manifest.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True), encoding="utf-8"
    )
    (out_dir / "summary.txt").write_text(summarize_manifest(manifest), encoding="utf-8")
    if comps_data is not None:
        from . import priced_table

        priced_table.write_priced_table(out_dir, manifest, store, comps_data)
    return manifest


def summarize_manifest(manifest: dict) -> str:
    """The readable half of the manifest."""
    lines = [
        f"ROOM RUN {manifest['room_id']} — {manifest['created_at']}",
        f"mode: {manifest['mode']} | sources: {len(manifest['sources'])} "
        f"| items: {len(manifest['items'])} | total cost: ${manifest['total_cost_usd']:.6f}",
        f"room style: {manifest['room_style']}",
        "",
    ]
    for entry in manifest["sources"]:
        origin = entry["origin"]
        if entry["timestamp_s"] is not None:
            origin += f" @ {entry['timestamp_s']}s"
        concept = (entry["plan"] or {}).get("concept") or "—"
        lines.append(f"  photo {entry['index']}: [{origin}] {entry['path']} — plan: {concept}")
    narration = manifest.get("narration")
    if narration:
        lines.append("")
        lines.append(
            "NARRATION (Baseten pool — separate from the room total above):"
        )
        for record in narration["files"]:
            if record["error"] is not None:
                lines.append(
                    f"  {record['path']}: ERROR [{record['error']['code']}] "
                    f"{record['error']['message']}"
                )
            else:
                kept_part = ""
                if "kept_items" in record:
                    kept_part = f"kept: {record['kept_items'] or 'none'}; "
                lines.append(
                    f"  {record['path']}: {record['duration_s']}s of speech — "
                    f"named for sale: {record['named_items'] or 'none'}; "
                    f"{kept_part}"
                    f"mentioned: {record['mentioned_items'] or 'none'}; "
                    f"est. ${record['cost_usd_estimate']:.6f}"
                )
                lines.append("  --- transcript ---")
                lines.extend(
                    f"  {line}" for line in (record["transcript"] or "").splitlines()
                )
                lines.append("  --- end transcript ---")
        lines.append(
            f"  Baseten total: ${narration['cost_usd_estimate']:.6f} "
            f"(usage {narration['usage']}); transcript: {narration['transcript_file']}"
        )
    lines.append("")
    staged = 0
    for item in manifest["items"]:
        narration_tag = ""
        item_narration = item.get("narration") or {}
        if item_narration.get("kind"):
            narration_tag = f" [narration: {item_narration['kind']}]"
        if item["status"] == "staged":
            staged += 1
            receipt = item["receipt"] or {}
            seen = f" (also seen in photos {item['also_seen_in']})" if item["also_seen_in"] else ""
            lines.append(
                f"  STAGED  {item['name']}{narration_tag}: photo {item['primary_photo_index']}{seen} — "
                f"{receipt.get('opaque_pixels')} opaque px, "
                f"{receipt.get('changed_opaque_pixels')} changed, "
                f"profile preserved: {receipt.get('color_profile_preserved')} — "
                f"${item['cost_usd']:.6f}"
            )
            lines.append(f"          receipt: {item['receipt_file']}")
            verification = item.get("verification") or {}
            lines.append(
                f"          verified independently: "
                f"{verification.get('opaque_pixels')} opaque px recounted from disk, "
                f"{verification.get('changed_opaque_pixels')} changed"
            )
            listing = item.get("listing") or {}
            lines.append(
                f"          listing title: {listing.get('title') or '—'}"
            )
            pricing = item.get("pricing") or {}
            if pricing.get("status") == "priced":
                price_range = pricing.get("range") or {}
                lines.append(
                    f"          pricing: ${price_range.get('quick')}–${price_range.get('premium')} "
                    f"range, suggested ${pricing.get('suggested')} "
                    f"({pricing.get('confidence')}, {pricing.get('comps_used')} comps)"
                )
            elif pricing.get("status") == "unpriced_thin_market":
                lines.append(
                    f"          pricing: not priced — thin market "
                    f"({pricing.get('comps_used')} comps)"
                )
            else:
                lines.append("          pricing: no comps supplied")
        elif item["status"] == "verify_failed":
            failure = item["failure"] or {}
            lines.append(
                f"  VERIFY FAILED  {item['name']}{narration_tag}: the staged output did not "
                f"pass independent verification — {failure.get('message')}"
            )
            lines.append(f"          receipt: {item['receipt_file']}")
        else:
            failure = item["failure"] or {}
            lines.append(
                f"  FAILED  {item['name']}{narration_tag}: stage {failure.get('stage')} "
                f"[{failure.get('code')}] {failure.get('message')}"
            )
        for flag in item.get("flags", []):
            lines.append(f"          flag: {flag}")
    lines.append("")
    lines.append(f"{staged} of {len(manifest['items'])} items staged.")
    return "\n".join(lines) + "\n"


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="room_run",
        description=(
            "ROOM RUN: stage every named item in a room's photos, one "
            "Fidelity Lock receipt per item, one manifest per room."
        ),
    )
    parser.add_argument(
        "--photos",
        nargs="+",
        default=[],
        help="Still photos (files or folders) of the room.",
    )
    parser.add_argument(
        "--video",
        action="append",
        default=[],
        help="Walkthrough video; frames are sampled into the photo pool. Repeatable.",
    )
    parser.add_argument(
        "--narrated-video",
        action="append",
        default=[],
        help=(
            "Video whose narration names the items: its audio is "
            "transcribed by one Baseten Muse Spark Open call, and the "
            "items the seller names join the work list. Repeatable. "
            "Pass the same file to --video too if its frames should "
            "join the photo pool."
        ),
    )
    parser.add_argument(
        "--narrated-audio",
        action="append",
        default=[],
        help="Audio file (wav/mp3/flac/ogg/m4a) whose narration names the items. Repeatable.",
    )
    parser.add_argument(
        "--narration-max-usd",
        type=float,
        default=NARRATION_MAX_PROJECTED_USD,
        help="Per-call projected-cost cap for a narration call (Baseten pool).",
    )
    parser.add_argument(
        "--items",
        default="",
        help='Comma-separated item names as the seller would say them ("straw hat, orange suitcase").',
    )
    parser.add_argument(
        "--auto",
        action="store_true",
        help="Auto-enumerate mode: candidate items come from each photo's identify plan.",
    )
    parser.add_argument("--scene", default=DEFAULT_SCENE, help="Shared room plate style.")
    parser.add_argument(
        "--stage-mode",
        choices=list(backdrop.STAGE_MODES),
        default="scene",
        help=(
            "How items are staged: 'scene' (default) generates one shared "
            "room plate with the image model; 'wallpaper' and 'studio_gray' "
            "render the plate in code per item, with no model call."
        ),
    )
    parser.add_argument(
        "--comps",
        type=Path,
        default=None,
        help=(
            "Path to a comps JSON file (agent-supplied Facebook Marketplace "
            "asking prices). Each item gains a pricing record, and the run "
            "also writes priced-table.png, priced-table.html, and "
            "pricing-notes.md into the output dir."
        ),
    )
    parser.add_argument(
        "--out",
        type=Path,
        default=None,
        help="Output dir for manifest.json + summary.txt + sampled frames (default: ./roomrun-<room_id>).",
    )
    parser.add_argument("--room-id", default=None)
    parser.add_argument("--budget-usd", type=float, default=1.00)
    parser.add_argument("--frame-every", type=float, default=2.0)
    parser.add_argument("--max-frames-per-video", type=int, default=8)
    parser.add_argument(
        "--first-success-only",
        action="store_true",
        help="Stop probing an item at its first clean cut (cheaper, thinner de-dup evidence).",
    )
    parser.add_argument(
        "--data-dir",
        type=Path,
        default=None,
        help="Isolate the asset store here (AGENTHAND_DATA_DIR).",
    )
    parser.add_argument(
        "--ledger",
        type=Path,
        default=None,
        help=(
            "The run's own cost ledger (default: ledger.jsonl inside the "
            "output dir). Every billed call during the run is mirrored "
            "into it, and the manifest total is read from it. The spend "
            "cap is checked against the cumulative ledger (the data dir's "
            "ledger.jsonl, or AGENTHAND_LEDGER=jsonl:<path>), so the "
            "run's spend always counts there too (fix pass F4)."
        ),
    )
    args = parser.parse_args(argv)

    if args.data_dir:
        os.environ["AGENTHAND_DATA_DIR"] = str(args.data_dir)

    item_names = [name.strip() for name in args.items.split(",") if name.strip()]
    if not args.auto and not item_names and not args.narrated_video and not args.narrated_audio:
        parser.error(
            "name the items with --items, use --auto to enumerate, "
            "or let a narration name them (--narrated-video/--narrated-audio)"
        )
    if not args.photos and not args.video:
        parser.error("give at least one --photos file/folder or --video")

    comps_data = None
    if args.comps is not None:
        try:
            comps_data = json.loads(Path(args.comps).read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            parser.error(f"could not read comps file {args.comps}: {exc}")
        if not isinstance(comps_data, dict):
            parser.error(f"comps file {args.comps} must contain a JSON object")

    room_id = args.room_id or f"room-{uuid.uuid4().hex[:8]}"
    out_dir = args.out or Path.cwd() / f"roomrun-{room_id}"
    try:
        manifest = run_room(
            [Path(p) for p in args.photos],
            [Path(v) for v in args.video],
            out_dir,
            item_names=item_names or None,
            auto=args.auto,
            scene=args.scene,
            stage_mode=args.stage_mode,
            comps_data=comps_data,
            room_id=room_id,
            budget_usd=args.budget_usd,
            frame_every_seconds=args.frame_every,
            max_frames_per_video=args.max_frames_per_video,
            probe_all_candidates=not args.first_success_only,
            ledger_path=args.ledger,
            narrated_videos=[Path(v) for v in args.narrated_video],
            narrated_audios=[Path(a) for a in args.narrated_audio],
            narration_max_projected_usd=args.narration_max_usd,
        )
    except RuntimeError as exc:
        print(f"ROOM RUN REFUSED: {exc}", file=sys.stderr)
        return 2
    print(summarize_manifest(manifest), end="")
    print(f"manifest: {out_dir / 'manifest.json'}")
    staged = sum(1 for i in manifest["items"] if i["status"] == "staged")
    return 0 if staged == len(manifest["items"]) else 1


if __name__ == "__main__":
    raise SystemExit(main())
