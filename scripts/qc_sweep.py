#!/usr/bin/env python3
"""QC sweep: drive the shipped agenthand binary over MCP stdio across a
matrix of (photo, phrase) pairs and record what each stage did.

For each photo:   identify (once; reused for every phrase, see --identify-every-pair)
For each phrase:  cut_out(image, concept=phrase) -> draw(asset, scene_prompt)

Outputs, under --out:
  sweep.csv                 one appended row per (photo, phrase)
  <phrase-slug>/            mask, overlay, cut-out, staged composite, JSON results
  _identify/<photo>.json    identify result per photo
  agenthand-data-<stamp>/   AGENTHAND_DATA_DIR for this run (asset store + ledger)
  server-stderr.log         the binary's stderr (key redacted)

Image inputs: the binary's ingest accepts asset:// handles, http(s) URLs
and base64 data URIs only (src/ingest.rs:398-418), not bare file paths.
So a local path is read as raw bytes and sent as a data URI; nothing is
decoded here. HEIC decode happens inside the binary.

Every number in the CSV that matters is measured here, independently of
the tool's own claims: mask coverage, region count (4-connected, the
same rule as segmentation.rs) and bbox are computed from the stored mask
PNG; product px checked / px changed are recounted by comparing the
delivered composite with the stored source photo at every pixel where
the cut-out's alpha is 255. If the recount disagrees with the receipt,
the row says so.

Spend: the binary's own spend cap is pointed at --max-usd for this run
(AGENTHAND_TOTAL_SPEND_CAP_USD; its ledger lives in the per-run data
dir), and the script also stops starting new pairs once the settled
total reaches --max-usd.

The META_API_KEY value is never printed, logged or written: it is
passed to the child process environment only, and any text that came
back from the binary is scrubbed of it before it is shown or saved.

Requires Python 3.8+ and Pillow (pip3 install Pillow). --dry-run needs
neither the key nor Pillow: it validates the JSON-RPC handshake and
tools/list, checks the photo paths, and spends nothing.
"""

import argparse
import base64
import csv
import datetime as _dt
import io
import json
import os
import queue
import re
import subprocess
import sys
import threading
import time

DEFAULT_BINARY = "~/agenthand-release/agenthand-0.3.0-macos-arm64"
DEFAULT_PHOTOS = [
    "/home/claude/film/v8/sources/shoot-intake/IMG_1358.HEIC",
    "/home/claude/film/v8/sources/shoot-intake/IMG_1361.HEIC",
    "/home/claude/film/v8/sources/shoot-intake/IMG_1365.HEIC",
]
DEFAULT_PHRASES = [
    "lawn",
    "stones",
    "fence",
    "fence post",
    "patio set",
    "climbing dome",
    "cornhole board",
    "kids bike",
]
DEFAULT_SCENE = "a clean, sunlit backyard product photo setting, soft afternoon light"
PROTOCOL_VERSION = "2024-11-05"
CSV_FIELDS = [
    "phrase",
    "photo",
    "coverage_pct",
    "regions",
    "bbox",
    "product_px_checked",
    "px_changed",
    "identify_cost_usd",
    "cut_out_cost_usd",
    "draw_cost_usd",
    "identify_s",
    "cut_out_s",
    "draw_s",
    "status",
    "notes",
]
# The binary's default ingest byte cap (src/ingest.rs DEFAULT_MAX_IMAGE_BYTES).
INGEST_MAX_BYTES = 20 * 1024 * 1024
MIME_BY_EXT = {
    ".heic": "image/heic",
    ".heif": "image/heif",
    ".jpg": "image/jpeg",
    ".jpeg": "image/jpeg",
    ".png": "image/png",
}

_SECRET = os.environ.get("META_API_KEY", "").strip()


def redact(text):
    """Scrub the key from any text that came back from the binary."""
    text = "" if text is None else str(text)
    if _SECRET and len(_SECRET) >= 4:
        text = text.replace(_SECRET, "***")
    return text


def log(msg):
    print(redact(msg), flush=True)


class CallFailed(Exception):
    """A JSON-RPC call that produced no tool result (timeout, server
    exit, or a JSON-RPC error object)."""


class McpServer:
    """The binary in `serve` mode, spoken to over newline-delimited
    JSON-RPC on stdio. Respawned transparently if it exits or a call
    times out; the asset store lives on disk, so handles survive."""

    def __init__(self, binary, env, stderr_path):
        self.binary = binary
        self.env = env
        self.stderr_path = stderr_path
        self.proc = None
        self.lines = None
        self.next_id = 0
        self.server_info = None

    # -- process ---------------------------------------------------------

    def start(self):
        self.proc = subprocess.Popen(
            [self.binary, "serve"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=self.env,
            bufsize=0,
        )
        self.lines = queue.Queue()
        threading.Thread(target=self._pump_stdout, args=(self.proc, self.lines), daemon=True).start()
        threading.Thread(target=self._pump_stderr, args=(self.proc,), daemon=True).start()
        self.server_info = self._handshake()

    @staticmethod
    def _pump_stdout(proc, lines):
        for raw in iter(proc.stdout.readline, b""):
            lines.put(raw)
        lines.put(None)  # EOF

    def _pump_stderr(self, proc):
        with open(self.stderr_path, "a", encoding="utf-8") as f:
            for raw in iter(proc.stderr.readline, b""):
                f.write(redact(raw.decode("utf-8", "replace")))
                f.flush()

    def alive(self):
        return self.proc is not None and self.proc.poll() is None

    def stop(self):
        if self.proc is None:
            return
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        self.proc = None

    def kill(self):
        if self.proc is not None:
            self.proc.kill()
            self.proc.wait()
            self.proc = None

    def ensure(self):
        if not self.alive():
            if self.proc is not None:
                log("  server exited (code %s); respawning" % self.proc.returncode)
            self.start()

    # -- protocol --------------------------------------------------------

    def _send(self, obj):
        self.proc.stdin.write((json.dumps(obj) + "\n").encode("utf-8"))
        self.proc.stdin.flush()

    def request(self, method, params, timeout):
        self.next_id += 1
        rid = self.next_id
        try:
            self._send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        except (BrokenPipeError, OSError) as e:
            code = self.proc.poll() if self.proc else None
            self.kill()
            raise CallFailed("server not accepting input (exit code %s): %s" % (code, e))
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                self.kill()
                raise CallFailed("TIMEOUT after %ds (server killed; respawns for the next call)" % timeout)
            try:
                raw = self.lines.get(timeout=remaining)
            except queue.Empty:
                continue
            if raw is None:
                code = self.proc.wait() if self.proc else None
                self.proc = None
                raise CallFailed("server exited mid-call (exit code %s); see server-stderr.log" % code)
            try:
                msg = json.loads(raw)
            except ValueError:
                continue  # not protocol output
            if msg.get("id") != rid:
                continue  # notification or stale response
            if "error" in msg:
                err = msg["error"] or {}
                raise CallFailed("JSON-RPC error %s: %s" % (err.get("code"), redact(err.get("message"))))
            return msg.get("result")

    def notify(self, method):
        self._send({"jsonrpc": "2.0", "method": method})

    def _handshake(self):
        result = self.request(
            "initialize",
            {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "agenthand-qc-sweep", "version": "1"},
            },
            timeout=30,
        )
        self.notify("notifications/initialized")
        return result

    def tools(self):
        return self.request("tools/list", {}, timeout=30).get("tools", [])

    def call_tool(self, name, arguments, timeout):
        """(structuredContent, content blocks, seconds)."""
        self.ensure()
        t0 = time.monotonic()
        try:
            result = self.request("tools/call", {"name": name, "arguments": arguments}, timeout)
        finally:
            elapsed = time.monotonic() - t0
        return (result or {}).get("structuredContent") or {}, (result or {}).get("content") or [], elapsed

    def read_asset(self, handle, timeout=120):
        self.ensure()
        result = self.request("resources/read", {"uri": handle}, timeout)
        return base64.b64decode(result["contents"][0]["blob"])


# ---------------------------------------------------------------------------
# Schema check (dry run and every real run).
# ---------------------------------------------------------------------------


def check_tools(tools):
    """Problems with the tool surface; empty when it matches what this
    script sends (src/server.rs tool_definitions)."""
    expected = {
        "identify": (["image"], ["image"]),
        "cut_out": (["image"], ["image", "concept"]),
        "draw": (["asset", "scene_prompt"], ["asset", "scene_prompt"]),
    }
    by_name = {t.get("name"): t for t in tools}
    problems = []
    for name, (required, props) in expected.items():
        tool = by_name.get(name)
        if tool is None:
            problems.append("tool %r missing" % name)
            continue
        schema = tool.get("inputSchema") or {}
        if sorted(schema.get("required") or []) != sorted(required):
            problems.append("%s.required is %r, expected %r" % (name, schema.get("required"), required))
        missing = [p for p in props if p not in (schema.get("properties") or {})]
        if missing:
            problems.append("%s.inputSchema lacks %r" % (name, missing))
    return problems


# ---------------------------------------------------------------------------
# Image helpers (Pillow, imported lazily so --dry-run works without it).
# ---------------------------------------------------------------------------


def pil():
    try:
        from PIL import Image, ImageChops  # noqa: F401
    except ImportError:
        sys.exit("Pillow is required for a real sweep: pip3 install Pillow")
    Image.MAX_IMAGE_PIXELS = None  # full-res phone photos, trusted source
    return Image, ImageChops


def image_arg(photo):
    """The `image` argument for a photo: handles and URLs pass through;
    a local file becomes a base64 data URI of its raw bytes."""
    if photo.startswith(("asset://", "http://", "https://", "data:")):
        return photo
    path = os.path.expanduser(photo)
    with open(path, "rb") as f:
        data = f.read()
    if len(data) > INGEST_MAX_BYTES:
        log("  warning: %s is %d bytes, over the binary's default 20 MB ingest cap" % (photo, len(data)))
    mime = MIME_BY_EXT.get(os.path.splitext(path)[1].lower(), "image/heic")
    return "data:%s;base64,%s" % (mime, base64.b64encode(data).decode("ascii"))


_RUN = re.compile(rb"[^\x00]+")


def mask_stats(png_bytes):
    """(coverage %, regions, bbox) of a stored mask PNG. Regions are
    4-connected components (segmentation.rs label_components), found
    with run-length union-find so a 12 MP mask takes well under a
    second. bbox is half-open [x0, y0, x1, y1] like mask_review."""
    Image, _ = pil()
    im = Image.open(io.BytesIO(png_bytes))
    if im.mode != "L":
        im = im.convert("L")
    w, h = im.size
    data = im.tobytes()
    parent = []

    def find(a):
        while parent[a] != a:
            parent[a] = parent[parent[a]]
            a = parent[a]
        return a

    total = 0
    x0, y0, x1, y1 = w, h, -1, -1
    prev = []
    for y in range(h):
        cur = []
        j = 0
        for m in _RUN.finditer(data[y * w:(y + 1) * w]):
            s, e = m.span()
            total += e - s
            label = len(parent)
            parent.append(label)
            while j < len(prev) and prev[j][1] <= s:
                j += 1
            k = j
            while k < len(prev) and prev[k][0] < e:
                ra, rb = find(label), find(prev[k][2])
                if ra != rb:
                    parent[ra] = rb
                k += 1
            cur.append((s, e, label))
            x0, x1 = min(x0, s), max(x1, e)
            y0, y1 = min(y0, y), max(y1, y + 1)
        prev = cur
    regions = len({find(i) for i in range(len(parent))})
    coverage = 100.0 * total / (w * h) if w * h else 0.0
    bbox = "%d,%d,%d,%d" % (x0, y0, x1, y1) if total else ""
    return coverage, regions, bbox


def recount(source_png, cutout_png, composite_png):
    """(product px checked, px changed): every pixel where the cut-out's
    alpha is 255, composite vs stored source, exact RGBA equality."""
    Image, ImageChops = pil()
    src = Image.open(io.BytesIO(source_png)).convert("RGBA")
    fg = Image.open(io.BytesIO(cutout_png)).convert("RGBA")
    out = Image.open(io.BytesIO(composite_png)).convert("RGBA")
    if not (src.size == fg.size == out.size):
        raise ValueError("size mismatch: source %s, cut-out %s, composite %s" % (src.size, fg.size, out.size))
    product = fg.getchannel("A").point(lambda a: 255 if a == 255 else 0)
    checked = product.histogram()[255]
    r, g, b, a = ImageChops.difference(src, out).split()
    anydiff = ImageChops.lighter(ImageChops.lighter(r, g), ImageChops.lighter(b, a))
    anydiff = anydiff.point(lambda v: 255 if v else 0)
    changed = ImageChops.multiply(anydiff, product).histogram()[255]
    return checked, changed


# ---------------------------------------------------------------------------
# Sweep.
# ---------------------------------------------------------------------------


def slug(text):
    return re.sub(r"[^A-Za-z0-9._-]+", "-", text.strip()).strip("-") or "phrase"


def cost_of(result):
    """Settled cost of a call: cost_usd on success, the error's
    cost_counted_usd on a billed failure, blank when nothing billed."""
    if result.get("cost_usd") is not None:
        return result["cost_usd"]
    err = result.get("error") or {}
    if isinstance(err, dict) and err.get("cost_counted_usd") is not None:
        return err["cost_counted_usd"]
    return ""


def error_text(stage, result):
    err = result.get("error") or {}
    if isinstance(err, dict):
        return "%s %s: %s" % (stage, err.get("code", "ERROR"), redact(err.get("message", "")))
    return "%s error: %s" % (stage, redact(err))


def fmt_s(seconds):
    return "" if seconds is None else "%.1f" % seconds


def save(path, data):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    mode = "wb" if isinstance(data, (bytes, bytearray)) else "w"
    with open(path, mode) as f:
        f.write(data)


def save_json(path, obj):
    save(path, redact(json.dumps(obj, indent=2, sort_keys=True)))


def append_row(csv_path, row):
    new = not os.path.exists(csv_path) or os.path.getsize(csv_path) == 0
    with open(csv_path, "a", newline="", encoding="utf-8") as f:
        writer = csv.DictWriter(f, fieldnames=CSV_FIELDS)
        if new:
            writer.writeheader()
        writer.writerow({k: row.get(k, "") for k in CSV_FIELDS})


def run_identify(server, photo, args, out_dir):
    """Identify one photo; returns (handle or None, cost, seconds, status, notes)."""
    stem = os.path.splitext(os.path.basename(photo))[0]
    try:
        result, _, secs = server.call_tool("identify", {"image": image_arg(photo)}, args.timeout)
    except (CallFailed, OSError) as e:
        return None, "", None, "identify failed: %s" % redact(e), ""
    save_json(os.path.join(out_dir, "_identify", stem + ".json"), result)
    handle = result.get("image")
    if result.get("status") != "ok":
        return handle, cost_of(result), secs, error_text("identify", result), ""
    notes = "identify concept=%r" % result.get("concept")
    return handle, cost_of(result), secs, "ok", notes


def run_pair(server, photo, phrase, source, ident, args, out_dir):
    """cut_out -> draw for one pair; returns the CSV row."""
    stem = os.path.splitext(os.path.basename(photo))[0]
    pdir = os.path.join(out_dir, slug(phrase))
    handle, id_cost, id_secs, id_status, id_notes = ident
    row = {
        "phrase": phrase,
        "photo": photo,
        "identify_cost_usd": id_cost,
        "identify_s": fmt_s(id_secs),
    }
    notes = [id_notes] if id_notes else []
    if id_status != "ok":
        notes.append(id_status)  # identify trouble is recorded; cut_out still runs with the phrase

    # cut_out with the phrase as the concept (no internal identify).
    try:
        cut, _, secs = server.call_tool("cut_out", {"image": source, "concept": phrase}, args.timeout)
    except (CallFailed, OSError) as e:
        row.update(status="cut_out failed: %s" % redact(e), notes="; ".join(notes))
        return row
    row.update(cut_out_s=fmt_s(secs), cut_out_cost_usd=cost_of(cut))
    save_json(os.path.join(pdir, stem + "-cut_out.json"), cut)
    attempts = cut.get("attempts") or (cut.get("error") or {}).get("attempts") or []
    if attempts:
        last = attempts[-1]
        records = last.get("records")
        if isinstance(records, int) and records > 1:
            notes.append("SAM returned %d records; the tool kept only the largest" % records)
        if last.get("outcome") == "poor_mask":
            notes.append(
                "rejected mask: coverage %s, components %s, dominant %s"
                % (last.get("coverage"), last.get("components"), last.get("dominant_fraction"))
            )
    if cut.get("status") != "ok":
        row.update(status=error_text("cut_out", cut), notes="; ".join(notes))
        return row

    review = cut.get("mask_review") or {}
    if review.get("touches_frame_edges"):
        notes.append("touches=" + ",".join(review["touches_frame_edges"]))
    for w in review.get("warnings") or []:
        notes.append("warn: " + w[:90])
    source_handle = cut.get("image") or handle
    try:
        mask_png = server.read_asset(cut["mask_asset"])
        cutout_png = server.read_asset(cut["output_asset"])
        save(os.path.join(pdir, stem + "-mask.png"), mask_png)
        save(os.path.join(pdir, stem + "-cutout.png"), cutout_png)
        if cut.get("mask_overlay_asset"):
            save(os.path.join(pdir, stem + "-overlay.png"), server.read_asset(cut["mask_overlay_asset"]))
        coverage, regions, bbox = mask_stats(mask_png)
        row.update(coverage_pct="%.4f" % coverage, regions=regions, bbox=bbox)
    except (CallFailed, OSError, ValueError, KeyError) as e:
        notes.append("could not read mask/cut-out: %s" % redact(e))
        cutout_png = None

    if args.skip_draw:
        row.update(status="ok (draw skipped)", notes="; ".join(notes))
        return row

    try:
        drawn, content, secs = server.call_tool(
            "draw", {"asset": cut["output_asset"], "scene_prompt": args.scene}, args.timeout
        )
    except (CallFailed, OSError) as e:
        row.update(status="draw failed: %s" % redact(e), notes="; ".join(notes))
        return row
    row.update(draw_s=fmt_s(secs), draw_cost_usd=cost_of(drawn))
    save_json(os.path.join(pdir, stem + "-draw.json"), drawn)
    receipt = drawn.get("fidelity_receipt") or (drawn.get("error") or {}).get("fidelity_receipt") or {}
    composite_png = None
    for block in content:
        if block.get("type") == "image" and block.get("data"):
            composite_png = base64.b64decode(block["data"])
    out_handle = drawn.get("output_asset") or (drawn.get("error") or {}).get("output_asset")
    if composite_png is None and out_handle:
        try:
            composite_png = server.read_asset(out_handle)
        except (CallFailed, OSError, KeyError) as e:
            notes.append("could not read composite: %s" % redact(e))
    if composite_png is not None:
        save(os.path.join(pdir, stem + "-staged.png"), composite_png)
        if cutout_png is not None and source_handle:
            try:
                checked, changed = recount(server.read_asset(source_handle), cutout_png, composite_png)
                row.update(product_px_checked=checked, px_changed=changed)
                if receipt and (
                    receipt.get("opaque_pixels") != checked or receipt.get("changed_opaque_pixels") != changed
                ):
                    notes.append(
                        "RECEIPT DISAGREES WITH RECOUNT: receipt says %s checked / %s changed"
                        % (receipt.get("opaque_pixels"), receipt.get("changed_opaque_pixels"))
                    )
            except (CallFailed, OSError, ValueError, KeyError) as e:
                notes.append("recount failed: %s" % redact(e))
    if receipt and row.get("product_px_checked", "") == "":
        row.update(
            product_px_checked=receipt.get("opaque_pixels", ""),
            px_changed=receipt.get("changed_opaque_pixels", ""),
        )
        notes.append("px columns are the receipt's claim (no independent recount)")
    if drawn.get("status") != "ok":
        row.update(status=error_text("draw", drawn), notes="; ".join(notes))
        return row
    row.update(status="ok", notes="; ".join(notes))
    return row


def child_env(args, data_dir, with_key):
    env = dict(os.environ)
    env["AGENTHAND_DATA_DIR"] = data_dir
    env["AGENTHAND_TOTAL_SPEND_CAP_USD"] = "%.2f" % args.max_usd
    # Never inherit offline dev switches by accident.
    env["AGENTHAND_DRAW_PLACEHOLDER_PLATE"] = "1" if args.placeholder_plate else ""
    if not with_key:
        # Empty, not removed: the binary's .env loader never overrides a
        # variable that is present, so this deterministically means "no key".
        env["META_API_KEY"] = ""
    return env


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--binary", default=DEFAULT_BINARY, help="agenthand binary (default: %(default)s)")
    ap.add_argument("--out", default=None, help="output dir (default: ./qc-sweep-<timestamp>)")
    ap.add_argument("--photos", nargs="+", default=DEFAULT_PHOTOS, help="photo paths, asset:// handles or URLs")
    ap.add_argument("--phrases", nargs="+", default=DEFAULT_PHRASES, help="subjects to cut out")
    ap.add_argument("--scene", default=DEFAULT_SCENE, help="scene_prompt for draw")
    ap.add_argument("--timeout", type=int, default=240, help="seconds per tool call (default: %(default)s)")
    ap.add_argument("--max-usd", type=float, default=2.00, help="spend ceiling for this run (default: %(default)s)")
    ap.add_argument("--identify-every-pair", action="store_true", help="re-run identify for every pair")
    ap.add_argument("--skip-draw", action="store_true", help="stop after cut_out (no plate spend)")
    ap.add_argument("--placeholder-plate", action="store_true", help="draw with the free local placeholder plate")
    ap.add_argument("--dry-run", action="store_true", help="handshake + tools/list only; no key, no spend")
    args = ap.parse_args(argv)

    binary = os.path.expanduser(args.binary)
    if not (os.path.isfile(binary) and os.access(binary, os.X_OK)):
        sys.exit("binary not found or not executable: %s" % binary)
    stamp = _dt.datetime.now().strftime("%Y%m%d-%H%M%S")
    out_dir = os.path.abspath(os.path.expanduser(args.out or "qc-sweep-" + stamp))
    os.makedirs(out_dir, exist_ok=True)
    data_dir = os.path.join(out_dir, "agenthand-data-" + stamp)
    stderr_path = os.path.join(out_dir, "server-stderr.log")
    key_set = bool(_SECRET)

    server = McpServer(binary, child_env(args, data_dir, with_key=key_set and not args.dry_run), stderr_path)
    try:
        server.start()
        info = (server.server_info or {}).get("serverInfo", {})
        tools = server.tools()
        problems = check_tools(tools)
    except (CallFailed, OSError) as e:
        server.kill()
        sys.exit("handshake failed: %s (see %s)" % (redact(e), stderr_path))
    log(
        "handshake ok: %s %s, protocol %s; tools: %s"
        % (
            info.get("name"),
            info.get("version"),
            (server.server_info or {}).get("protocolVersion"),
            ", ".join(t.get("name", "?") for t in tools),
        )
    )
    if problems:
        server.stop()
        sys.exit("tool schema mismatch: " + "; ".join(problems))
    log("tool schemas match what this script sends")
    log("META_API_KEY: %s" % ("set" if key_set else "NOT SET"))
    local = [p for p in args.photos if not p.startswith(("asset://", "http://", "https://", "data:"))]
    missing = [p for p in local if not os.path.isfile(os.path.expanduser(p))]
    log("photos: %d given, %d missing%s" % (len(args.photos), len(missing), (": " + ", ".join(missing)) if missing else ""))

    if args.dry_run:
        server.stop()
        log("dry run: nothing called, nothing spent")
        return 1 if missing else 0
    if not key_set:
        server.stop()
        sys.exit("META_API_KEY is not set in this shell; export it and re-run (it is never printed)")
    pil()  # fail fast before spending if Pillow is missing

    csv_path = os.path.join(out_dir, "sweep.csv")
    total = 0.0
    n = len(args.photos) * len(args.phrases)
    i = 0
    stopped = False
    for photo in args.photos:
        if stopped:
            break
        if photo in missing:
            for phrase in args.phrases:
                i += 1
                append_row(csv_path, {"phrase": phrase, "photo": photo, "status": "photo not found"})
            continue
        log("identify %s ..." % photo)
        ident = run_identify(server, photo, args, out_dir)
        log("  %s (%s s, $%s)" % (ident[3], fmt_s(ident[2]), ident[1]))
        if isinstance(ident[1], (int, float)):
            total += ident[1]
        try:
            source = ident[0] or image_arg(photo)
        except OSError as e:
            source = None
            log("  cannot read %s: %s" % (photo, e))
        first = True
        for phrase in args.phrases:
            i += 1
            if total >= args.max_usd:
                log("spend ceiling $%.2f reached (settled $%.4f); stopping" % (args.max_usd, total))
                stopped = True
                break
            if not first and args.identify_every_pair:
                ident = run_identify(server, photo, args, out_dir)
                if isinstance(ident[1], (int, float)):
                    total += ident[1]
                source = ident[0] or source
            elif not first:
                ident = (ident[0], "", None, ident[3], "identify reused from this photo's first row")
            if source is None:
                append_row(csv_path, {"phrase": phrase, "photo": photo, "status": "photo unreadable"})
                continue
            row = run_pair(server, photo, phrase, source, ident, args, out_dir)
            first = False
            for k in ("cut_out_cost_usd", "draw_cost_usd"):
                if isinstance(row.get(k), (int, float)):
                    total += row[k]
            append_row(csv_path, row)
            log(
                "[%d/%d] %s | %s | %s | cov %s%% regions %s | cut %s s, draw %s s | run total $%.4f"
                % (
                    i,
                    n,
                    os.path.basename(photo),
                    phrase,
                    row.get("status", "")[:120],
                    row.get("coverage_pct", "-"),
                    row.get("regions", "-"),
                    row.get("cut_out_s", "-"),
                    row.get("draw_s", "-"),
                    total,
                )
            )
    server.stop()
    log("done: %s (settled total $%.4f)" % (csv_path, total))
    return 0


if __name__ == "__main__":
    sys.exit(main())
