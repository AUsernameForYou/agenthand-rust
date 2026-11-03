"""The priced table: a room run rendered as one priced sheet.

Written only when a run is given comps (``--comps``). Three artifacts
land in the run's output dir:

- ``priced-table.png`` and ``priced-table.html``: one row per item in
  the run (failed and refuted items included, with their status
  shown), columns thumbnail | ITEM | PRICE RANGE | SUGGESTED |
  COMPS | CONFIDENCE.
- ``pricing-notes.md``: the per-item comp listings and distributions
  behind the table, plus the method notes.

Thumbnails are presentation copies: the item's receipted foreground
composited on the studio gray radial backdrop, fitted to the tile
(upright rotation is permitted as a display transform). The listing
composite the Fidelity Lock receipt attaches to is never touched by
anything in this module.
"""

from __future__ import annotations

import base64
import html
import io
from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw, ImageFilter, ImageFont

from . import backdrop, market_pricing

FOOTNOTE = (
    "Thumbnails are presentation copies (may be rotated or resized for display). "
    "The Fidelity Lock receipt attaches to the listing composite, whose product "
    "pixels are untouched. Prices are asking-price ranges from Facebook "
    "Marketplace comps; advisory only — a suggested price is a recommendation, "
    "never an order."
)

_INK = (17, 17, 17)
_CYAN = (0, 175, 225)
_MUTED = (96, 102, 110)
_THUMB_SIZE = 460


def _contact_shadow(size, bbox, fg_alpha, blur):
    """Soft elliptical shadow under a placed thumbnail item.

    Presentation copies only: the shadow's alpha is forced to zero at
    every fully opaque product pixel, exactly as in the prototype
    table builder.
    """
    width, height = size
    x0, y0, x1, y1 = bbox
    bw, bh = x1 - x0, y1 - y0
    layer = Image.new("RGBA", (width, height), (0, 0, 0, 0))
    draw = ImageDraw.Draw(layer)
    cx = (x0 + x1) / 2
    cy = y1 + 0.015 * height
    rx = bw * 0.44
    ry = max(5.0, bh * 0.055)
    draw.ellipse([cx - rx, cy - ry, cx + rx, cy + ry], fill=(58, 50, 40, 110))
    layer = layer.filter(ImageFilter.GaussianBlur(blur))
    arr = np.asarray(layer).copy()
    arr[:, :, 3] = np.where(fg_alpha == 255, 0, arr[:, :, 3])
    return Image.fromarray(arr, "RGBA")


def _primary_foreground_handle(record: dict) -> str | None:
    for sighting in record.get("sightings") or []:
        if (
            sighting.get("photo_index") == record.get("primary_photo_index")
            and sighting.get("status") == "ok"
        ):
            return sighting.get("foreground_asset")
    return None


def make_thumbnail(store, record: dict, size: int = _THUMB_SIZE) -> Image.Image:
    """One item's presentation thumbnail on the studio gray backdrop."""
    tile = backdrop.render_studio_gray(size, size).convert("RGBA")
    handle = _primary_foreground_handle(record) if store is not None else None
    if not handle:
        return tile.convert("RGB")
    try:
        foreground = store.read_image(handle)
    except Exception:
        return tile.convert("RGB")
    alpha = np.asarray(foreground)[:, :, 3]
    ys, xs = np.where(alpha > 10)
    if len(xs) == 0:
        return tile.convert("RGB")
    crop = foreground.crop((xs.min(), ys.min(), xs.max() + 1, ys.max() + 1))
    max_w, max_h = int(size * 0.86), int(size * 0.82)
    scale = min(max_w / crop.width, max_h / crop.height)
    item = crop.resize(
        (max(1, int(crop.width * scale)), max(1, int(crop.height * scale))),
        Image.LANCZOS,
    )
    x0 = (size - item.width) // 2
    y1 = int(size * 0.90)
    y0 = y1 - item.height
    item_alpha = np.asarray(item)[:, :, 3]
    canvas_alpha = np.zeros((size, size), np.uint8)
    canvas_alpha[y0:y1, x0 : x0 + item.width] = item_alpha
    shadow = _contact_shadow(
        (size, size), (x0, y0, x0 + item.width, y1), canvas_alpha,
        blur=max(5, size * 0.02),
    )
    tile.alpha_composite(shadow)
    tile.alpha_composite(item, (x0, y0))
    return tile.convert("RGB")


def _row_for(record: dict) -> dict:
    """One table row's display values, derived from the item record."""
    listing = record.get("listing") or {}
    title = listing.get("title") or record["name"]
    sub = record["name"] if listing.get("title") else (listing.get("condition_notes") or "")
    if record["status"] != "staged":
        failure = record.get("failure") or {}
        status_text = f"status: {record['status']}"
        if failure.get("stage") or failure.get("code"):
            status_text += f" ({failure.get('stage')} / {failure.get('code')})"
        sub = f"{sub} — {status_text}" if sub else status_text
    pricing = record.get("pricing") or {}
    status = pricing.get("status")
    if status == market_pricing.STATUS_PRICED:
        price_range = f"${pricing['range']['quick']} – ${pricing['range']['premium']}"
        suggested = f"${pricing['suggested']}"
        confidence = pricing.get("confidence") or "—"
    elif status == market_pricing.STATUS_THIN:
        price_range = "—"
        suggested = "not priced — thin market"
        confidence = "—"
    else:
        price_range = "—"
        suggested = "not priced — no comps supplied"
        confidence = "—"
    return {
        "title": title,
        "sub": sub,
        "price_range": price_range,
        "suggested": suggested,
        "comps": str(pricing.get("comps_used", 0)),
        "confidence": confidence,
    }


def _font(size: int, bold: bool = False):
    name = "DejaVuSans-Bold.ttf" if bold else "DejaVuSans.ttf"
    try:
        return ImageFont.truetype(f"/usr/share/fonts/truetype/dejavu/{name}", size)
    except Exception:
        return ImageFont.load_default()


def _fit_font(
    draw: ImageDraw.ImageDraw, text: str, max_width: float, size: int, bold: bool = False
):
    """The largest font at or under ``size`` that fits ``text`` in the cell."""
    while size > 10:
        font = _font(size, bold=bold)
        if draw.textlength(text, font=font) <= max_width:
            return font
        size -= 1
    return _font(size, bold=bold)


def _wrap(draw: ImageDraw.ImageDraw, text: str, font, max_width: float) -> list[str]:
    words, lines, current = text.split(), [], ""
    for word in words:
        trial = (current + " " + word).strip()
        if draw.textlength(trial, font=font) <= max_width:
            current = trial
        else:
            if current:
                lines.append(current)
            current = word
    if current:
        lines.append(current)
    return lines


def _header_lines(manifest: dict, comps_data: dict | None) -> tuple[str, str]:
    title = "agenthand — the room, priced"
    pricing_source = manifest.get("pricing_source") or {}
    area = (comps_data or {}).get("area") or pricing_source.get("area") or "—"
    radius = (comps_data or {}).get("radius_mi")
    if radius is None:
        radius = pricing_source.get("radius_mi")
    pulled = (comps_data or {}).get("pulled_at") or pricing_source.get("pulled_at") or "—"
    radius_text = f"within {radius} mi of {area}" if radius is not None else f"area: {area}"
    subtitle = (
        f"Room {manifest.get('room_id')} · {len(manifest.get('items') or [])} items · "
        f"FB Marketplace asking comps {radius_text} · pulled {pulled} · "
        f"backdrop: studio gray"
    )
    return title, subtitle


def render_table_png(
    manifest: dict, thumbs: list[Image.Image], comps_data: dict | None, out_path: Path
) -> None:
    """The PNG sheet, in the prototype table's visual language."""
    rows = [_row_for(record) for record in manifest.get("items") or []]
    _title, subtitle = _header_lines(manifest, comps_data)
    width, margin = 1560, 36
    header_h, colh_h, row_h = 118, 52, 196
    probe = ImageDraw.Draw(Image.new("RGB", (8, 8)))
    foot_font = _font(18)
    foot_lines = _wrap(probe, FOOTNOTE, foot_font, width - 2 * margin)
    height = header_h + 5 + colh_h + row_h * len(rows) + 30 + len(foot_lines) * 24 + 28
    img = Image.new("RGB", (width, height), (252, 252, 253))
    draw = ImageDraw.Draw(img)
    draw.rectangle([0, 0, width, header_h], fill=_INK)
    draw.text((margin, 26), "agenthand", font=_font(40, bold=True), fill=(255, 255, 255))
    agent_w = draw.textlength("agenthand", font=_font(40, bold=True))
    draw.text(
        (margin + agent_w + 14, 38), "—  the room, priced",
        font=_font(26), fill=_CYAN,
    )
    draw.text((margin, 78), subtitle, font=_font(21), fill=(196, 200, 205))
    draw.rectangle([0, header_h, width, header_h + 5], fill=_CYAN)

    cols = {"thumb": margin, "name": margin + 216, "rng": 880, "rec": 1095,
            "comps": 1265, "conf": 1370}
    y = header_h + 5
    draw.rectangle([0, y, width, y + colh_h], fill=(238, 240, 242))
    for x, label in [(cols["name"], "ITEM"), (cols["rng"], "PRICE RANGE"),
                     (cols["rec"], "SUGGESTED"), (cols["comps"], "COMPS"),
                     (cols["conf"], "CONFIDENCE")]:
        draw.text((x, y + 15), label, font=_font(20, bold=True), fill=(90, 96, 104))
    y += colh_h
    for i, row in enumerate(rows):
        if i % 2 == 1:
            draw.rectangle([0, y, width, y + row_h], fill=(244, 245, 247))
        thumb = thumbs[i].resize((180, 180), Image.LANCZOS)
        img.paste(thumb, (cols["thumb"], y + (row_h - 180) // 2))
        draw.text(
            (cols["name"], y + 52), row["title"],
            font=_fit_font(draw, row["title"], cols["rng"] - cols["name"] - 16, 27, bold=True),
            fill=_INK,
        )
        if row["sub"]:
            draw.text((cols["name"], y + 92), row["sub"], font=_font(19), fill=_MUTED)
        draw.text((cols["rng"], y + 76), row["price_range"], font=_font(25), fill=_INK)
        suggested_color = (0, 130, 170) if row["suggested"].startswith("$") else (120, 126, 133)
        draw.text(
            (cols["rec"], y + 68), row["suggested"],
            font=_fit_font(draw, row["suggested"], cols["comps"] - cols["rec"] - 12, 30, bold=True),
            fill=suggested_color,
        )
        draw.text((cols["comps"], y + 76), row["comps"], font=_font(25), fill=_INK)
        draw.text((cols["conf"], y + 76), row["confidence"], font=_font(25), fill=_INK)
        draw.line([margin, y + row_h, width - margin, y + row_h], fill=(222, 225, 229), width=1)
        y += row_h
    y += 22
    for line in foot_lines:
        draw.text((margin, y), line, font=foot_font, fill=_MUTED)
        y += 24
    img.save(out_path, optimize=True)


def render_table_html(
    manifest: dict, thumbs: list[Image.Image], comps_data: dict | None, out_path: Path
) -> None:
    """The HTML sheet: the same table, thumbnails embedded as data URIs."""
    rows = [_row_for(record) for record in manifest.get("items") or []]
    title, subtitle = _header_lines(manifest, comps_data)
    encoded = []
    for thumb in thumbs:
        buffer = io.BytesIO()
        thumb.save(buffer, format="PNG")
        encoded.append(base64.b64encode(buffer.getvalue()).decode("ascii"))
    body_rows = []
    for row, thumb_b64 in zip(rows, encoded):
        body_rows.append(
            "<tr>"
            f'<td><img alt="" width="120" height="120" src="data:image/png;base64,{thumb_b64}"></td>'
            f"<td><strong>{html.escape(row['title'])}</strong>"
            + (f"<br><small>{html.escape(row['sub'])}</small>" if row["sub"] else "")
            + "</td>"
            f"<td>{html.escape(row['price_range'])}</td>"
            f"<td>{html.escape(row['suggested'])}</td>"
            f"<td>{html.escape(row['comps'])}</td>"
            f"<td>{html.escape(row['confidence'])}</td>"
            "</tr>"
        )
    document = (
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head><meta charset=\"utf-8\">"
        f"<title>{html.escape(title)}</title></head>\n<body>\n"
        f"<h1>{html.escape(title)}</h1>\n<p>{html.escape(subtitle)}</p>\n"
        "<table>\n<thead><tr><th></th><th>ITEM</th><th>PRICE RANGE</th>"
        "<th>SUGGESTED</th><th>COMPS</th><th>CONFIDENCE</th></tr></thead>\n"
        "<tbody>\n" + "\n".join(body_rows) + "\n</tbody>\n</table>\n"
        f"<p>{html.escape(FOOTNOTE)}</p>\n</body>\n</html>\n"
    )
    out_path.write_text(document, encoding="utf-8")


def _notes_text(manifest: dict, comps_data: dict | None) -> str:
    lines = [
        "# Pricing notes",
        "",
        "## Method",
        "",
        "- Prices are Facebook Marketplace ASKING prices, not sold prices. "
        "What a seller asks is a ceiling signal, not a record of what "
        "anyone paid.",
        "- Listing age is a staleness signal: an ask that has sat for "
        "weeks is weaker evidence than a fresh one, and the comp "
        "listings below carry each listing's age so the reader can "
        "weigh it.",
        "- Marketplace sales commonly close about 10–20% under ask; the "
        "ranges in the table are asking-price ranges, and that "
        "negotiation discount assumption is the reader's to apply.",
        "- Confidence is capped at moderate on asking data alone: "
        "n >= 8 usable comps is moderate, 4–7 is low-moderate, "
        "2–3 is low, and under 2 usable comps the item is not priced.",
        "- The range is the interquartile spread of usable (not new) "
        "comps by linear interpolation: quick = p25, market = p50, "
        "premium = p75, each rounded to the nearest $5 with a $5 "
        "floor. The suggested price is the rounded market value.",
        "",
        "## Items",
        "",
    ]
    for record in manifest.get("items") or []:
        pricing = record.get("pricing") or {}
        lines.append(f"### {record['name']}")
        lines.append("")
        lines.append(f"- Run status: {record['status']}")
        lines.append(
            f"- Pricing: {pricing.get('status')} "
            f"(comps used: {pricing.get('comps_used', 0)})"
        )
        comps = market_pricing.find_comps(record["name"], comps_data) or []
        if comps:
            lines.append("- Comps supplied:")
            for comp in comps:
                if not isinstance(comp, dict):
                    continue
                excluded = " — excluded (listed as new)" if comp.get("is_new") else ""
                lines.append(
                    f"  - {comp.get('title')} — ask ${comp.get('price_usd')} — "
                    f"condition: {comp.get('condition')} — "
                    f"distance: {comp.get('distance_mi')} mi — "
                    f"age: {comp.get('age_days')} days{excluded}"
                )
        else:
            lines.append("- Comps supplied: none for this item.")
        prices = market_pricing.usable_prices(comps)
        if prices:
            lines.append(f"- Usable prices (sorted): {prices}")
            if len(prices) >= 2:
                raw = {
                    "p25": market_pricing.percentile_type7(prices, 0.25),
                    "p50": market_pricing.percentile_type7(prices, 0.50),
                    "p75": market_pricing.percentile_type7(prices, 0.75),
                }
                lines.append(
                    f"- Distribution: p25 {raw['p25']}, p50 {raw['p50']}, "
                    f"p75 {raw['p75']} before rounding"
                )
                lines.append(
                    f"- Computed range: ${pricing['range']['quick']} – "
                    f"${pricing['range']['premium']}; suggested "
                    f"${pricing['suggested']}; confidence "
                    f"{pricing['confidence']}"
                )
            else:
                lines.append(
                    "- Distribution: under 2 usable comps, so no range "
                    "is computed (thin market)."
                )
        lines.append("")
    return "\n".join(lines) + "\n"


def write_priced_table(
    out_dir: Path, manifest: dict, store, comps_data: dict | None
) -> dict:
    """Write priced-table.png/.html + pricing-notes.md into out_dir."""
    out_dir = Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    thumbs = [make_thumbnail(store, record) for record in manifest.get("items") or []]
    png_path = out_dir / "priced-table.png"
    html_path = out_dir / "priced-table.html"
    notes_path = out_dir / "pricing-notes.md"
    render_table_png(manifest, thumbs, comps_data, png_path)
    render_table_html(manifest, thumbs, comps_data, html_path)
    notes_path.write_text(_notes_text(manifest, comps_data), encoding="utf-8")
    return {"png": png_path, "html": html_path, "notes": notes_path}
