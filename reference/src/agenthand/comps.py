"""Category comps pricing: a range plus a recommended price (mirror of Rust comps.rs).

Each priced item carries a RANGE of possible prices, a RECOMMENDED
price and a CONFIDENCE, computed in code from a comps file gathered
elsewhere (Facebook Marketplace asking prices). This runs AFTER
identify returns. It reads only fields identify already produced
(concept, alternates, suggested_title) or an item name, and never adds
a word to the identify prompt or a key to the schema the model sees
(longer identify wording was bisected as the cause of HTTP 200
responses with empty bodies). There is deliberately no price tool:
pricing rides on identify results and room-run item records only.

The comps file (``AGENTHAND_COMPS_FILE``, default
``<data dir>/pricing-comps.json``) holds a list of categories, each
with a key, an item description, search queries, comp listings
(price, title, location), stats {count,min,p25,median,p75,max},
``recommended`` (the median; ``suggested`` is an alias), ``range``
[min,max], ``percentile_band`` [p25,p75] and ``confidence``. The
numbers are passed through as written; this module only picks the
category.

Two tiers (owner's ruling, 2026-11-01): the ``categories`` list is
the LOCAL tier and always wins. A category with no local comps
(count 0, or no usable local prices) falls back to its wider-area
tier: a nested ``wide`` object of the same shape, else the entry
with the same key in a top-level ``categories_wide`` list. A block
priced that way carries ``scope: "wide"`` and the plain note
``WIDE_SCOPE_NOTE``; a local block carries ``scope: "local"``. With
neither tier the item is ``no_comps``, as before. A missing or
malformed comps file degrades to ``{"status": "no_comps", "note":
...}`` — never an error, never an invented price.

Matching: every text is normalised to tokens (lowercase, split on
non-alphanumerics and letter/digit boundaries, a light plural
strip, a few synonyms, stopwords and film production tags such as
"P12" dropped). Each category gets a token vocabulary: tokens of
its item text and queries weigh 1.0; tokens seen in its comp
titles weigh 0.5 + 0.5 x (share of titles containing the token).
A text's score for a category is the summed weight of its unique
tokens divided by their count. Gate: the head noun (last
non-numeric token) of at least one noun phrase (concept,
alternates, item name) must weigh at least HEAD_MIN_WEIGHT in the
category, so "bike helmet" never prices as a bike. The best
category over all candidate texts wins if it reaches
MIN_MATCH_SCORE; ties go to the category with more comps, then
the lower key.
"""

from __future__ import annotations

import json
import math
from dataclasses import dataclass, field
from pathlib import Path

from . import config

#: What the prices are, stamped on every priced block.
PRICING_BASIS = "asking prices, Facebook Marketplace comps"

#: The advisory sentence carried on every locally priced block.
PRICING_FOOTNOTE = "Prices are asking prices from local Facebook Marketplace comps, not sold prices. Advisory only: the recommended price is a suggestion, and the seller sets the price."

#: The advisory sentence on a block priced from the wider-area tier
#: (the local footnote would wrongly say "local").
PRICING_FOOTNOTE_WIDE = "Prices are asking prices from wider-area Facebook Marketplace comps, not sold prices. Advisory only: the recommended price is a suggestion, and the seller sets the price."

#: The note on every block priced from the wider-area tier.
WIDE_SCOPE_NOTE = "No local items found; priced from wider-area listings."

SCOPE_LOCAL = "local"
SCOPE_WIDE = "wide"

#: Minimum match score for a category to price an item.
MIN_MATCH_SCORE = 0.6

#: Minimum vocabulary weight the head noun needs in a category: an
#: item/query token (1.0) or a token in at least 15% of the comp
#: titles (0.5 + 0.5 x 0.15).
HEAD_MIN_WEIGHT = 0.575

#: Default comps file name inside the data dir.
DEFAULT_COMPS_FILE_NAME = "pricing-comps.json"

_STOPWORDS = frozenset(
    [
        # function words
        "an", "and", "or", "the", "with", "without", "for", "of", "in", "on",
        "to", "at", "by", "from", "this", "that", "is", "it", "its", "as",
        "into", "plus",
        # marketplace filler that names no product
        "great", "good", "excellent", "condition", "works", "working", "used",
        "like", "new", "nice", "clean", "sale", "obo", "firm", "only",
        "perfect", "gently", "lightly", "barely", "pick", "pickup", "up",
        "must", "go", "price", "priced", "cheap", "free", "comes", "included",
        "includes", "brand", "listing", "item", "product",
        # production notes in the comps item text
        "second",
    ]
)


def tokens(text: str) -> list[str]:
    """Lowercase, split, strip, map: the shared token normaliser."""
    lowered = text.lower()
    out: list[str] = []
    raw: list[str] = []

    def flush_raw() -> None:
        if not raw:
            return
        word = "".join(raw)
        raw.clear()
        if _is_production_tag(word):
            return
        for piece in _split_letter_digit(word):
            if len(piece) < 2:
                continue
            stemmed = _synonym(_stem(piece))
            if len(stemmed) < 2 or stemmed in _STOPWORDS:
                continue
            out.append(stemmed)

    for ch in lowered:
        if ch.isalnum():
            raw.append(ch)
        else:
            flush_raw()
    flush_raw()
    return out


def _unique_tokens(text: str) -> list[str]:
    """Unique tokens in first-seen order."""
    seen: set[str] = set()
    out = []
    for tok in tokens(text):
        if tok not in seen:
            seen.add(tok)
            out.append(tok)
    return out


def _is_production_tag(word: str) -> bool:
    """Film shot tags in the comps item text ("P12", "P16", "P18")."""
    return (
        word.startswith("p")
        and 2 <= len(word) <= 4
        and all(c in "0123456789" for c in word[1:])
    )


def _split_letter_digit(word: str) -> list[str]:
    pieces: list[str] = []
    current: list[str] = []
    current_digit: bool | None = None
    for ch in word:
        digit = ch in "0123456789"
        if current_digit is not None and current_digit != digit:
            pieces.append("".join(current))
            current = []
        current.append(ch)
        current_digit = digit
    if current:
        pieces.append("".join(current))
    return pieces


def _stem(word: str) -> str:
    """Light plural strip: "bikes" -> "bike", "kids" -> "kid"; leaves
    "glass", "plus", "chassis" and short words alone."""
    if (
        len(word) > 3
        and word.endswith("s")
        and not word.endswith("ss")
        and not word.endswith("us")
        and not word.endswith("is")
        and not all(c in "0123456789" for c in word)
    ):
        return word[:-1]
    return word


_SYNONYMS = {
    "bicycle": "bike", "bicycles": "bike", "bicycl": "bike",
    "child": "kid", "children": "kid", "childrens": "kid", "kiddie": "kid",
    "kiddo": "kid", "toddler": "kid", "youth": "kid", "boy": "kid",
    "girl": "kid",
    "bbq": "barbecue", "barbeque": "barbecue",
    "grille": "grill",
    "grey": "gray",
    "automobile": "car", "vehicle": "car",
    "woman": "women", "ladies": "women", "ladie": "women", "lady": "women",
    "womens": "women",
}


def _synonym(word: str) -> str:
    return _SYNONYMS.get(word, word)


def configured_comps_path() -> Path:
    """The configured comps file: ``AGENTHAND_COMPS_FILE`` when set and
    non-empty, else ``<data dir>/pricing-comps.json``."""
    return config.get_comps_file()


@dataclass
class Category:
    """One usable category, its priced fields fixed at load time."""

    key: str
    item: str
    comps_count: int
    #: The tier whose numbers price this category ("local" | "wide").
    scope: str
    vocab: dict[str, float]
    priced: dict

    def weight(self, token: str) -> float:
        """Vocabulary weight of a normalised token in this category."""
        return self.vocab.get(token, 0.0)

    def score(self, text: str) -> float:
        """Weighted coverage score of a text against this category."""
        toks = _unique_tokens(text)
        if not toks:
            return 0.0
        return sum(self.weight(t) for t in toks) / len(toks)


@dataclass
class CompsBook:
    """A loaded comps file: the categories that passed validation, plus
    a note naming any that were skipped."""

    categories: list[Category] = field(default_factory=list)
    skipped: list[str] = field(default_factory=list)

    def category(self, key: str) -> Category | None:
        for cat in self.categories:
            if cat.key == key:
                return cat
        return None

    def match_category(
        self,
        phrases: list[tuple[str, str]],
        titles: list[tuple[str, str]],
    ) -> "CategoryMatch | None":
        """Pick the category for an item. ``phrases`` are noun phrases
        (label, text) whose head nouns gate the categories; ``titles``
        are extra texts (listing titles) that score but do not gate.
        With no usable head noun at all, every category is eligible."""
        heads = [
            head
            for _, phrase in phrases
            if (head := _head_token(phrase)) is not None
        ]
        best: tuple[float, int, str, str] | None = None
        for cat in self.categories:
            if heads and not any(
                cat.weight(h) >= HEAD_MIN_WEIGHT for h in heads
            ):
                continue
            cat_best: tuple[float, str] | None = None
            for label, text in [*phrases, *titles]:
                s = cat.score(text)
                if cat_best is None or s > cat_best[0] + 1e-12:
                    cat_best = (s, label)
            if cat_best is None:
                continue
            score, label = cat_best
            if best is None:
                better = True
            else:
                bs, bn, bk, _ = best
                if abs(score - bs) > 1e-9:
                    better = score > bs
                elif cat.comps_count != bn:
                    better = cat.comps_count > bn
                else:
                    better = cat.key < bk
            if better:
                best = (score, cat.comps_count, cat.key, label)
        if best is None:
            return None
        score, _, key, on = best
        if score + 1e-12 < MIN_MATCH_SCORE:
            return None
        return CategoryMatch(key=key, score=score, on=on)

    def priced_block(self, match: "CategoryMatch") -> dict:
        """The pricing block for a match."""
        cat = self.category(match.key)
        if cat is None:
            return no_comps()
        import copy

        block = copy.deepcopy(cat.priced)
        block["match"] = {
            "score": math.floor(match.score * 1000 + 0.5) / 1000,
            "on": match.on,
        }
        return block

    def price(
        self,
        phrases: list[tuple[str, str]],
        titles: list[tuple[str, str]],
    ) -> dict:
        match = self.match_category(phrases, titles)
        if match is None:
            return no_comps()
        return self.priced_block(match)


@dataclass
class CategoryMatch:
    """The chosen category for an item."""

    key: str
    score: float
    #: Which candidate text won ("concept", "alternates[0]",
    #: "suggested_title", "item_name", "listing.title").
    on: str


class CompsFileError(ValueError):
    """The comps file (or one tier of it) is unusable; the message is
    the one-line note the no_comps block carries."""


def _num(value: object) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    result = float(value)
    return result if math.isfinite(result) else None


def _pair(value: object) -> tuple[object, object, float, float] | None:
    if not isinstance(value, list) or len(value) != 2:
        return None
    a = _num(value[0])
    b = _num(value[1])
    if a is None or b is None:
        return None
    return (value[0], value[1], a, b)


def confidence_rule(n: int, p25: float, p75: float, median: float) -> str:
    """The documented confidence rule, for a category that omits it:
    high when n>=10 and (p75-p25)/median <= 0.8; medium when n>=6 and
    the ratio <= 1.3; otherwise low."""
    if median <= 0.0:
        return "low"
    ratio = (p75 - p25) / median
    if n >= 10 and ratio <= 0.8:
        return "high"
    if n >= 6 and ratio <= 1.3:
        return "medium"
    return "low"


def _entry_key(entry: object) -> str | None:
    if not isinstance(entry, dict):
        return None
    key = entry.get("key")
    if not isinstance(key, str):
        return None
    key = key.strip()
    return key or None


def _str_list(value: object) -> list[str]:
    if not isinstance(value, list):
        return []
    return [v for v in value if isinstance(v, str)]


def _parse_tier(
    key: str,
    item: str,
    queries: list[str],
    cat: object,
    generated: object,
    scope: str,
) -> Category:
    """Parse one tier (the local category entry itself, or its wider-area
    object) into a priced category. ``item`` and ``queries`` are the
    matching text; the numbers all come from ``cat``. A tier with no
    comps (count 0) is an error: "no local comps" is what sends a
    category to its wide tier."""
    label = scope
    if not isinstance(cat, dict):
        raise CompsFileError(f"{key}: {label} tier is not an object")
    stats = cat.get("stats")
    stats = stats if isinstance(stats, dict) else {}
    comps = cat.get("comps")
    comps = comps if isinstance(comps, list) else []
    count = stats.get("count")
    if isinstance(count, int) and not isinstance(count, bool) and count >= 0:
        comps_count = count
    else:
        comps_count = len(comps)
    if comps_count == 0:
        raise CompsFileError(f"{key}: no {label} comps")

    pair = _pair(cat.get("range"))
    if pair is not None:
        range_lo_v, range_hi_v, range_lo, range_hi = pair
    else:
        lo = _num(stats.get("min"))
        hi = _num(stats.get("max"))
        if lo is None or hi is None:
            raise CompsFileError(f"{key}: no range")
        range_lo_v, range_hi_v, range_lo, range_hi = (
            stats.get("min"),
            stats.get("max"),
            lo,
            hi,
        )
    recommended_v: object = None
    recommended: float | None = None
    for field_name in ("recommended", "suggested"):
        value = _num(cat.get(field_name))
        if value is not None:
            recommended_v, recommended = cat.get(field_name), value
            break
    if recommended is None:
        value = _num(stats.get("median"))
        if value is not None:
            recommended_v, recommended = stats.get("median"), value
    if recommended is None:
        raise CompsFileError(f"{key}: no recommended price")
    if not (range_lo <= recommended <= range_hi):
        raise CompsFileError(
            f"{key}: recommended {recommended} is outside the range "
            f"[{range_lo}, {range_hi}]"
        )
    band_pair = _pair(cat.get("percentile_band"))
    if band_pair is not None:
        band: object = [band_pair[0], band_pair[1]]
    else:
        p25, p75 = _num(stats.get("p25")), _num(stats.get("p75"))
        if p25 is not None and p75 is not None:
            band = [stats.get("p25"), stats.get("p75")]
        else:
            band = None
    confidence_value = cat.get("confidence")
    if confidence_value in ("high", "medium", "low"):
        confidence = confidence_value
    else:
        p25, p75 = _num(stats.get("p25")), _num(stats.get("p75"))
        if p25 is not None and p75 is not None:
            confidence = confidence_rule(comps_count, p25, p75, recommended)
        else:
            confidence = "low"
    searched_at = cat.get("searched_at")
    if searched_at is None:
        searched_at = generated
    area = cat.get("area")

    # Vocabulary: item text and queries strong; comp titles by share.
    vocab: dict[str, float] = {}
    titles = [
        c["title"]
        for c in comps
        if isinstance(c, dict) and isinstance(c.get("title"), str)
    ]
    df: dict[str, int] = {}
    for title in titles:
        for tok in _unique_tokens(title):
            df[tok] = df.get(tok, 0) + 1
    for tok, n in df.items():
        vocab[tok] = 0.5 + 0.5 * n / max(1, len(titles))
    strong_text = item + "".join(" " + q for q in queries)
    for tok in tokens(strong_text):
        vocab[tok] = 1.0
    if not vocab:
        raise CompsFileError(
            f"{key}: no item text or comp titles to match against"
        )

    priced = {
        "status": "priced",
        "scope": scope,
        "category": key,
        "item": item,
        "range": [range_lo_v, range_hi_v],
        "recommended": recommended_v,
        "percentile_band": band,
        "confidence": confidence,
        "comps_count": comps_count,
        "searched_at": searched_at,
        "area": area,
        "basis": PRICING_BASIS,
        "advisory": True,
        "footnote": PRICING_FOOTNOTE if scope == SCOPE_LOCAL else PRICING_FOOTNOTE_WIDE,
    }
    if scope == SCOPE_WIDE:
        priced["note"] = WIDE_SCOPE_NOTE
    return Category(
        key=key,
        item=item,
        comps_count=comps_count,
        scope=scope,
        vocab=vocab,
        priced=priced,
    )


def _parse_category(
    cat: dict,
    wide_entry: object,
    generated: object,
) -> Category:
    """A local ``categories`` entry: its own numbers when it has local
    comps, else its wider-area tier (``wide_entry``: the nested ``wide``
    object, or the ``categories_wide`` entry with the same key)."""
    key = _entry_key(cat)
    if key is None:
        raise CompsFileError("category without a key")
    item = cat.get("item")
    item = item if isinstance(item, str) else ""
    queries = _str_list(cat.get("queries"))
    try:
        return _parse_tier(key, item, queries, cat, generated, SCOPE_LOCAL)
    except CompsFileError as exc:
        local_err = str(exc)
    if wide_entry is None:
        raise CompsFileError(local_err)
    wide = wide_entry
    wide_item_value = wide.get("item") if isinstance(wide, dict) else None
    if isinstance(wide_item_value, str) and wide_item_value.strip():
        wide_item = wide_item_value
    else:
        wide_item = item
    wide_queries = list(queries)
    for q in _str_list(wide.get("queries") if isinstance(wide, dict) else None):
        if q not in wide_queries:
            wide_queries.append(q)
    if wide_item != item and item.strip():
        wide_queries.append(item)
    try:
        return _parse_tier(key, wide_item, wide_queries, wide, generated, SCOPE_WIDE)
    except CompsFileError as exc:
        raise CompsFileError(f"{local_err}; {exc}") from exc


def _optional_list(doc: dict, field_name: str) -> list | None:
    """An optional top-level list: absent or null is empty; anything
    other than a list is an error."""
    value = doc.get(field_name)
    if value is None:
        return None
    if isinstance(value, list):
        return value
    raise CompsFileError(f'comps file "{field_name}" is not a list')


def parse_book(doc: object) -> CompsBook:
    """Parse a comps document already in memory: the local ``categories``
    list, plus the optional wider-area tier (nested ``wide`` objects
    and/or a top-level ``categories_wide`` list keyed the same way)."""
    if not isinstance(doc, dict):
        raise CompsFileError('comps file has no "categories" list')
    local = _optional_list(doc, "categories")
    wide_list = _optional_list(doc, "categories_wide")
    if local is None and wide_list is None:
        raise CompsFileError('comps file has no "categories" list')
    generated = doc.get("generated")
    categories: list[Category] = []
    skipped: list[str] = []

    # categories_wide by key, first entry per key.
    wide_by_key: dict[str, object] = {}
    wide_order: list[str] = []
    for entry in wide_list or []:
        k = _entry_key(entry)
        if k is None:
            skipped.append("categories_wide entry without a key")
        elif k in wide_by_key:
            skipped.append(f"{k}: duplicate categories_wide entry ignored")
        else:
            wide_by_key[k] = entry
            wide_order.append(k)

    local_keys: set[str] = set()
    for cat in local or []:
        if not isinstance(cat, dict):
            skipped.append("category without a key")
            continue
        key = _entry_key(cat)
        if key is not None:
            local_keys.add(key)
        # The nested `wide` object wins over a categories_wide entry.
        wide_entry = cat.get("wide")
        if wide_entry is None:
            wide_entry = wide_by_key.get(key) if key is not None else None
        try:
            categories.append(_parse_category(cat, wide_entry, generated))
        except CompsFileError as exc:
            skipped.append(str(exc))
    # Keys only in categories_wide: no local entry, so no local comps.
    for key in wide_order:
        if key in local_keys:
            continue
        entry = wide_by_key[key]
        item = entry.get("item") if isinstance(entry, dict) else None
        item = item if isinstance(item, str) else ""
        queries = _str_list(entry.get("queries") if isinstance(entry, dict) else None)
        try:
            categories.append(
                _parse_tier(key, item, queries, entry, generated, SCOPE_WIDE)
            )
        except CompsFileError as exc:
            skipped.append(str(exc))
    if not categories:
        detail = f" ({'; '.join(skipped)})" if skipped else ""
        raise CompsFileError(f"comps file has no usable categories{detail}")
    return CompsBook(categories=categories, skipped=skipped)


def _one_line(text: str) -> str:
    return " ".join(text.split())


#: A loaded comps file, or the one-line note saying why there is none.
#: Exactly one of the two is None.
LoadedBook = tuple[CompsBook | None, str | None]


def load_book(path: Path) -> LoadedBook:
    """Load and parse a comps file. Errors are one-line notes."""
    try:
        text = path.read_text(encoding="utf-8")
    except FileNotFoundError:
        return None, _one_line(f"pricing comps file not found at {path}")
    except OSError as exc:
        return None, _one_line(f"pricing comps file {path} unreadable: {exc}")
    try:
        doc = json.loads(text)
    except json.JSONDecodeError as exc:
        return None, _one_line(
            f"pricing comps file {path} is not valid JSON: {exc}"
        )
    try:
        return parse_book(doc), None
    except CompsFileError as exc:
        return None, _one_line(f"pricing comps file {path}: {exc}")


def load_configured_book() -> LoadedBook:
    """Load the configured comps file (see ``configured_comps_path``)."""
    return load_book(configured_comps_path())


def no_comps() -> dict:
    """The block for an item with no matching category."""
    return {"status": "no_comps"}


def no_comps_with_note(note: str) -> dict:
    """The block when the comps file itself is missing or malformed."""
    return {"status": "no_comps", "note": note}


def _head_token(phrase: str) -> str | None:
    """Head noun of a noun phrase: its last non-numeric token."""
    for tok in reversed(tokens(phrase)):
        if not all(c in "0123456789" for c in tok):
            return tok
    return None


def _nonempty(value: object) -> str | None:
    if not isinstance(value, str):
        return None
    value = value.strip()
    return value or None


def pricing_for_identify(loaded: LoadedBook, result: dict) -> dict:
    """Pricing for an identify result, from the concept, its alternates
    and the suggested title already in that result."""
    book, note = loaded
    if book is None:
        return no_comps_with_note(note or "pricing comps unavailable")
    phrases: list[tuple[str, str]] = []
    concept = _nonempty(result.get("concept"))
    if concept is not None:
        phrases.append(("concept", concept))
    alternates = result.get("alternates")
    if isinstance(alternates, list):
        for i, alt in enumerate(alternates):
            text = _nonempty(alt)
            if text is not None:
                phrases.append((f"alternates[{i}]", text))
    titles: list[tuple[str, str]] = []
    title = _nonempty(result.get("suggested_title"))
    if title is not None:
        titles.append(("suggested_title", title))
    if not phrases and not titles:
        return no_comps()
    return book.price(phrases, titles)


def pricing_for_item_name(
    loaded: LoadedBook,
    item_name: str,
    listing_title: str | None,
) -> dict:
    """Pricing for an item by name, with an optional listing title as
    extra (non-gating) evidence."""
    book, note = loaded
    if book is None:
        return no_comps_with_note(note or "pricing comps unavailable")
    phrases: list[tuple[str, str]] = []
    if item_name and item_name.strip():
        phrases.append(("item_name", item_name.strip()))
    titles: list[tuple[str, str]] = []
    if listing_title is not None and listing_title.strip():
        titles.append(("listing.title", listing_title.strip()))
    if not phrases and not titles:
        return no_comps()
    return book.price(phrases, titles)


def attach_identify_pricing(result: dict, loaded: LoadedBook) -> None:
    """Add the ``pricing`` block to an identify tool result in place. Only
    an ok result is priced; every existing field is left as it was."""
    if result.get("status") != "ok":
        return
    result["pricing"] = pricing_for_identify(loaded, result)
