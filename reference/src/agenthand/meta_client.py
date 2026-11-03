"""The billing choke point: the ONLY place a billable call may originate.

Every external model call agenthand ever makes goes through billed_call().
Tools never touch the ledger or the gate directly. billed_call enforces
reserve-then-settle accounting against the spend cap:

1. Under a file lock (so two processes or threads cannot both pass the
   gate on the same stale total), the ledger total is checked against the
   cap and a "pending" reservation receipt is appended at the projected
   worst-case cost.
2. The call runs (the lock is NOT held across the network call; the
   reservation on disk is what makes the spend visible to other callers).
3. A "settled" receipt is appended with the actual cost, priced from the
   usage the provider returned, usage counts included. A priced call
   whose usage is missing (prices to $0) settles at its reserved
   projection instead: the reservation stays counted.
4. If the call raises, no settlement is written: the reservation stays
   counted at its projected cost. A call that may have billed us is
   treated as having billed us.

The live Meta client below IS constructed with SDK retries disabled
(max_retries=0): an SDK-level retry would re-run a billable call behind
the gate's back. Every attempt, including our own explicit repair
attempt, passes through billed_call so it is reserved and counted.

Timeouts (fix pass F0, Rust parity): every Meta call runs under a
per-call timeout of DEFAULT_HTTP_TIMEOUT_S (180 s) and a connect
timeout of HTTP_CONNECT_TIMEOUT_S (20 s), replacing the openai SDK
default (600 s read, 5 s connect). AGENTHAND_HTTP_TIMEOUT_S (a whole
number of seconds, > 0) overrides the per-call timeout; an empty,
unparseable or zero value keeps the 180 s default. The value is read
when the client is built (build_client). The SDK's timeout is per
phase (read, write, pool), not one whole-request deadline as in the
Rust build; for these non-streaming calls the provider sends nothing
until its reply is ready, so the 180 s read bound caps the provider's
think time the same way.

The key check happens BEFORE billed_call is invoked: a call attempted
with no META_API_KEY configured raises NotConfiguredError before any
reservation is written, so a missing key can never leave phantom spend
in the ledger.

One cap, all spend (fix pass F4): every reservation and settle goes to
the CUMULATIVE ledger (config.get_cumulative_ledger_path), and the cap
is checked there. When AGENTHAND_LEDGER_PATH names a mirror ledger (the
room runner's <room>/ledger.jsonl), each row is ALSO appended to that
JSONL file, so a room's own spend stays readable from its directory
without the room ever owning the cap. A mirror that names the
cumulative ledger itself is not written twice.

Settle is capped too (fix pass F5): at settle, the ledger total
WITHOUT this call's own rows (every other settled actual plus every
other open reservation) plus the actual cost is checked against the
cap, under the same file lock. Over the cap, the settle is still
recorded truthfully (status "over_cap", the overage in its note, the
mirror gets the same row), billed_call raises
ledger.SpendCapExceededAtSettle naming the breach, and every later
reservation is refused because the ledger total exceeds the cap.
"""

from __future__ import annotations

import base64
from dataclasses import dataclass, asdict
import fcntl
import json
import math
import re
import uuid
from collections.abc import Callable
from contextvars import ContextVar
from pathlib import Path

from . import config, ledger, pricing

# A usage dict as returned by a billable call: any of these keys.
UsageDict = dict[str, int]

_USAGE_KEYS = ("input_tokens", "output_tokens", "cached_input_tokens", "images")

# The call_id of the billed_call currently running its fn, if any.
# billed_call sets it so code deep inside a call (the identify response
# parser, say) can name its debug artifacts after the ledger call they
# belong to without threading the id through every signature.
_current_call_id: ContextVar[str | None] = ContextVar(
    "agenthand_current_call_id", default=None
)


def _lock_file(ledger_dir: Path):
    """Open (creating if needed) the ledger lock file. Caller closes it.

    The lock lives beside the cumulative LEDGER it guards, not beside
    the asset store: with AGENTHAND_LEDGER=jsonl:<path> separating the
    two (the dry-run gate), two processes sharing one ledger through
    different data dirs must still serialize on the same lock file.
    Mirror-ledger appends happen under the same lock.
    """
    ledger_dir.mkdir(parents=True, exist_ok=True)
    return (ledger_dir / "ledger.lock").open("a+")


def _mirror_ledger(cumulative: Path) -> Path | None:
    """The mirror ledger for this call, if any: AGENTHAND_LEDGER_PATH,
    unless it names the cumulative ledger's own file (a row is never
    written twice to one ledger)."""
    mirror = config.get_mirror_ledger_path()
    if mirror is None:
        return None
    if mirror == cumulative or mirror.resolve() == cumulative.resolve():
        return None
    return mirror


def billed_call(
    tool: str,
    model: str | None,
    projected_usd: float,
    fn: Callable[[], UsageDict | None],
    *,
    status: str = "ok",
    note: str = "",
) -> dict:
    """Run one billable call under the spend cap, reserve-then-settle.

    Args:
        tool: Tool name recorded on the receipts ("identify", ...).
        model: Priced model name, or None for a call with no model cost.
            An unknown (unpriced) model raises before anything is written:
            unpriced spend must never flow.
        projected_usd: Worst-case cost reserved before the call runs.
        fn: Zero-argument callable performing the call. Returns a usage
            dict (any of input_tokens, output_tokens, cached_input_tokens,
            images); the settled cost is priced from it. If a priced
            call's usage prices to $0 (missing or empty), the call
            settles at its reserved projection instead — missing usage
            never erases a reservation. The returned dict's
            usage_missing flag reports when that happened.
        status: Receipt status ("ok", or "stub" while a tool is unwired).
        note: Receipt note (stub explanations etc.).

    Returns a dict with call_id, the projected and actual USD amounts, and
    the usage fn reported.

    Raises ledger.SpendCapExceeded if the reservation would exceed the
    cap, ledger.SpendCapExceededAtSettle (a SpendCapExceeded) when the
    call ran but its actual cost pushed the ledger over the cap (the
    settle is recorded first, status over_cap), pricing.UnknownModelError
    for an unpriced model, and whatever fn raises (with the reservation
    left counted for unknown failures; completed SAM cut failures settle
    their observed image count before re-raising).
    """
    if model is not None and not pricing.is_known_model(model):
        raise pricing.UnknownModelError(
            f"Model {model!r} is not in the pricing table; refusing to run "
            "an unpriced billable call."
        )
    # Validate the projection before anything is written.
    ledger._validate_amount(projected_usd, "projected cost")

    # The cap ledger is the cumulative one (fix pass F4); the mirror,
    # when set, receives a copy of every row.
    ledger_path = config.get_cumulative_ledger_path()
    mirror_path = _mirror_ledger(ledger_path)
    cap_usd = config.get_total_spend_cap_usd()
    call_id = uuid.uuid4().hex

    # Reserve: check + append pending, atomically under the file lock.
    with _lock_file(ledger_path.parent) as lock_fh:
        fcntl.flock(lock_fh, fcntl.LOCK_EX)
        ledger.check_spend_cap(ledger_path, projected_usd, cap_usd)
        pending = ledger.Receipt(
            tool=tool,
            model=model,
            usd_estimate=float(projected_usd),
            status=status,
            phase=ledger.PHASE_PENDING,
            call_id=call_id,
            note=note,
        )
        ledger.append_receipt(ledger_path, pending)
        if mirror_path is not None:
            ledger.append_receipt(mirror_path, pending)

    # Unknown failures keep the reservation. A decoded cut failure carries
    # the exact completed image count, which can be settled before re-raising.
    # The call_id rides a context var while fn runs so nested code can
    # correlate its debug artifacts with this call's ledger records.
    from .segmentation import CutOutResultError

    cut_failure = None
    token = _current_call_id.set(call_id)
    try:
        usage = fn() or {}
    except CutOutResultError as exc:
        if tool != "cut_out" or model != "sam-3.1" or exc.images <= 0:
            raise
        cut_failure = exc
        usage = {"images": exc.images}
        status = "error"
        suffix = f"{exc.code}: settled {exc.images} completed SAM images; unknown call failures retain their reservation (fail-closed)"
        note = f"{note} | {suffix}" if note else suffix
    finally:
        _current_call_id.reset(token)
    usage = {key: int(usage.get(key, 0)) for key in _USAGE_KEYS}

    actual_usd = pricing.estimate_cost_usd(model, **usage) if model else 0.0

    # Fail-closed settlement (architecture Gauntlet fix 3): a priced
    # call whose reported usage prices to $0 has no trustworthy actual.
    # Settling $0 would supersede the reservation in the ledger total
    # and erase spend that may well have billed, so the call settles
    # at its reserved projection instead — the reservation stays
    # counted, and the settled receipt's note says why. A $0
    # reservation (the placeholder-plate stub path) is exempt: it
    # reserved nothing, so there is nothing to preserve.
    usage_missing = bool(model is not None and projected_usd > 0 and actual_usd == 0.0)
    settle_note = note
    if usage_missing:
        actual_usd = float(projected_usd)
        suffix = (
            "usage_missing: provider reported no usable usage; "
            "settled at the reserved projection (fail-closed)"
        )
        settle_note = f"{note} | {suffix}" if note else suffix

    # Settle: append the actual, with the usage counts stored on the
    # receipt, cap-checked (fix pass F5) under the same lock.
    with _lock_file(ledger_path.parent) as lock_fh:
        fcntl.flock(lock_fh, fcntl.LOCK_EX)
        settled = ledger.Receipt(
            tool=tool,
            model=model,
            usd_estimate=actual_usd,
            input_tokens=usage["input_tokens"],
            output_tokens=usage["output_tokens"],
            cached_input_tokens=usage["cached_input_tokens"],
            images=usage["images"],
            status=status,
            phase=ledger.PHASE_SETTLED,
            call_id=call_id,
            note=settle_note,
        )
        others = ledger.ledger_total_excluding_call_usd(ledger_path, call_id)
        overage = ledger.apply_settle_cap(settled, others, cap_usd)
        ledger.append_receipt(ledger_path, settled)
        if mirror_path is not None:
            ledger.append_receipt(mirror_path, settled)

    if cut_failure is not None:
        cut_failure.cost_counted_usd = actual_usd
        cut_failure.call_id = call_id
    if overage is not None:
        breach = ledger.SpendCapExceededAtSettle(
            f"SPEND CAP EXCEEDED at settle: {tool} call {call_id} reserved "
            f"${float(projected_usd):.6f} but actually cost ${actual_usd:.6f}, "
            f"bringing the ledger to ${overage.total_usd:.6f}, "
            f"${overage.overage_usd:.6f} over the ${overage.cap_usd:.2f} cap. "
            f'The settle is recorded truthfully with status "{ledger.STATUS_OVER_CAP}"; '
            "every further billable call will be refused until the cap is raised.",
            call_id=call_id,
            projected_usd=float(projected_usd),
            actual_usd=actual_usd,
            total_usd=overage.total_usd,
            overage_usd=overage.overage_usd,
            cap_usd=overage.cap_usd,
        )
        # The breach is the louder failure: it outranks a decoded cut
        # failure, which rides along as the cause.
        raise breach from cut_failure
    if cut_failure is not None:
        raise cut_failure

    return {
        "call_id": call_id,
        "projected_usd": float(projected_usd),
        "actual_usd": actual_usd,
        "usage": usage,
        "usage_missing": usage_missing,
    }


# ---------------------------------------------------------------------------
# The live Meta Model API client (OpenAI-compatible transport).
# ---------------------------------------------------------------------------

META_BASE_URL = "https://api.meta.ai/v1"

# Default per-call timeout for every Meta API request, in seconds (see
# the module docstring; overridable via HTTP_TIMEOUT_ENV). A live
# identify call measured 28.3 s, so a tight bound would cut off
# healthy, already-billable calls.
DEFAULT_HTTP_TIMEOUT_S = 180
# Connect timeout for every Meta API request, in seconds.
HTTP_CONNECT_TIMEOUT_S = 20
# Environment variable that overrides DEFAULT_HTTP_TIMEOUT_S.
HTTP_TIMEOUT_ENV = "AGENTHAND_HTTP_TIMEOUT_S"

_HTTP_TIMEOUT_RE = re.compile(r"\+?[0-9]+")
_U64_MAX = 2**64 - 1

# Bounded output for the identify call: the structured result is a short
# description, a few phrases, and listing copy. The bound is set
# explicitly on every request because the reservation is priced from
# it. 2048 (raised from 1024): the first live response (2026-10-26)
# came back truncated mid-string at the 1024 bound, unparseable.
IDENTIFY_MAX_TOKENS = 2048

# Bounded retry for a known live flake: muse-spark-1.3 intermittently
# answers HTTP 200 with an empty body or a body cut mid-JSON (~2 of 3
# dry-run gate attempts, 2026-10-27). The retry loop lives in
# server._run_identify, NOT here: each attempt must be its own
# billed_call so the ledger shows (and charges) every attempt — a
# retry hidden inside one billed_call's fn would spend uncounted.
# Only body-parse failures retry (see is_identify_body_failure).
IDENTIFY_MAX_ATTEMPTS = 3

# Allowance for the text side of the identify request (instructions plus
# schema overhead) when projecting the worst-case input size.
_IDENTIFY_PROMPT_TOKENS = 300


class NotConfiguredError(RuntimeError):
    """Raised when a real Meta API call is attempted with no key configured.

    Raised BEFORE any reservation is written: a missing key is an
    operator setup problem, not spend, and must never touch the ledger.
    """


class IdentifyResultError(RuntimeError):
    """Raised when the model's identify response cannot be used.

    Covers malformed JSON, missing or mistyped fields, and a concept /
    alternates contract the repair attempt could not fix. When this
    propagates out of a billed_call fn, the reservation stays counted:
    the call may have billed, so the spend is treated as spent.

    Attributes:
        raw_response: The full raw model text when the failure is an
            unparseable response body, else None. Raw model text is
            never written to the ledger (the ledger records costs and
            counts only); vision_identify saves it to a debug file.
        debug_path: Path of the debug file raw_response was saved to,
            set by the capture step in vision_identify; None when no
            capture happened.
    """

    def __init__(self, message: str, *, raw_response: str | None = None):
        super().__init__(message)
        self.raw_response = raw_response
        self.debug_path: str | None = None


def is_identify_body_failure(exc: IdentifyResultError) -> bool:
    """True when the identify failure was an unparseable response body.

    The parse paths attach the raw body to the error (raw_response);
    contract violations on a body that DID parse leave it None. An
    empty body lands here too: the content falls back to "" and
    json.loads("") fails, so raw_response is "". Only body failures
    are worth re-rolling against a fresh completion — a parsed plan
    that breaks the contract has deterministic handling instead
    (phrase sanitize, then the one alternates-repair call), and
    retrying it would just spend.
    """
    return exc.raw_response is not None


def require_meta_api_key() -> str:
    """Return the configured Meta API key or raise NotConfiguredError."""
    key = config.get_meta_api_key()
    if not key:
        raise NotConfiguredError(
            "META_API_KEY is not set (environment or .env); the Meta "
            "Model API is not configured, so no billable call was made."
        )
    return key


def http_timeout_from(raw: str | None) -> int:
    """Resolve the per-call timeout from a raw AGENTHAND_HTTP_TIMEOUT_S
    value: a whole number of seconds greater than zero (surrounding
    whitespace and a leading '+' allowed), else DEFAULT_HTTP_TIMEOUT_S.
    Pure, and the same rule as the Rust build's http_timeout_from."""
    value = (raw or "").strip()
    if not _HTTP_TIMEOUT_RE.fullmatch(value):
        return DEFAULT_HTTP_TIMEOUT_S
    secs = int(value)
    if secs <= 0 or secs > _U64_MAX:
        return DEFAULT_HTTP_TIMEOUT_S
    return secs


def http_timeout_s() -> int:
    """The per-call timeout build_client applies, read from the
    environment (AGENTHAND_HTTP_TIMEOUT_S) at call time."""
    import os

    return http_timeout_from(os.environ.get(HTTP_TIMEOUT_ENV))


def build_client():
    """Construct the OpenAI-compatible client for the Meta Model API.

    max_retries=0 is load-bearing: SDK retries would re-run a billable
    call behind the spend gate's back. Raises NotConfiguredError when no
    key is configured.

    The timeout is http_timeout_s() (180 s unless
    AGENTHAND_HTTP_TIMEOUT_S overrides it) with a 20 s connect timeout
    (fix pass F0): the SDK default allowed 600 s per read.
    """
    from openai import OpenAI, Timeout

    api_key = require_meta_api_key()  # loads .env before the timeout is read
    return OpenAI(
        base_url=META_BASE_URL,
        api_key=api_key,
        max_retries=0,
        timeout=Timeout(float(http_timeout_s()), connect=float(HTTP_CONNECT_TIMEOUT_S)),
    )


def estimate_image_input_tokens(width: int | None, height: int | None) -> int:
    """Conservative upper bound on vision input tokens for one image.

    Vision encoders tile an image; one token per 32x32 patch (rounded
    up) plus slack is a safe over-estimate for reservation pricing.
    Unknown dimensions fall back to a mid-size bound.
    """
    if not width or not height:
        return 4096
    patches = math.ceil(width / 32) * math.ceil(height / 32)
    return patches + 256


def project_identify_cost_usd(
    model: str, width: int | None, height: int | None
) -> float:
    """Worst-case USD projection for one identify invocation.

    Covers the initial image call AND the one possible alternates-repair
    call at the same worst case (the repair call is text-only and
    cheaper in practice; projecting it at the full image-call price
    keeps the reservation a true upper bound).
    """
    input_tokens = estimate_image_input_tokens(width, height) + _IDENTIFY_PROMPT_TOKENS
    one_call = pricing.estimate_cost_usd(
        model, input_tokens=input_tokens, output_tokens=IDENTIFY_MAX_TOKENS
    )
    return round(2 * one_call, 6)


# NOTE: keep this schema and _IDENTIFY_INSTRUCTIONS close to the
# shape that works. A 2026-10-27 change that lengthened the
# instructions (and added per-property "description" keys here) made
# the Meta API answer HTTP 200 with an EMPTY completion, every time;
# bisection pinned the lengthened instructions as sufficient to
# reproduce it (the schema keys were removed in the same pass and
# never re-tested alone). A one-sentence guidance clause in the
# instructions is verified safe; anything more needs a live probe.
# The real enforcement is deterministic anyway: _sanitize_plan_phrases.
_IDENTIFY_SCHEMA: dict = {
    "name": "identify_result",
    "strict": True,
    "schema": {
        "type": "object",
        "additionalProperties": False,
        "properties": {
            "description": {"type": "string"},
            "concept": {"type": "string"},
            "alternates": {
                "type": "array",
                "items": {"type": "string"},
                "minItems": 2,
            },
            "suggested_title": {"type": "string"},
            "listing_bullets": {
                "type": "array",
                "items": {"type": "string"},
                "minItems": 3,
                "maxItems": 5,
            },
            "condition_notes": {"type": "string"},
        },
        "required": [
            "description",
            "concept",
            "alternates",
            "suggested_title",
            "listing_bullets",
            "condition_notes",
        ],
    },
}

_IDENTIFY_INSTRUCTIONS = (
    "You are the planning step of a listing-production pipeline. Look at "
    "the product photo and return the structured result. Be brief in "
    "every field; the length limits are hard. description: a short "
    "factual description of the product, at most 40 words. concept: ONE "
    "short noun phrase naming the product, suitable for text-prompted "
    "image segmentation (e.g. 'brown leather wallet') — the product "
    "only, no background, no adjectives about the scene. Name the "
    "physical object itself, never the screen, photo, or frame it "
    "appears in. alternates: at least 2 different fallback noun "
    "phrases for the same product, for use if the first phrase "
    "segments poorly. suggested_title: a "
    "marketplace listing title. listing_bullets: 3 "
    "to 5 short selling points a buyer can verify from the photo, each "
    "at most 12 words. condition_notes: visible wear, damage, or "
    "defects, in at most 25 words; say so plainly, or state that none "
    "are visible."
)


def _usage_of(response) -> UsageDict:
    """Map an OpenAI-compatible usage object to our usage dict.

    prompt_tokens is the total input; its cached subset rides in
    prompt_tokens_details.cached_tokens (the convention pricing.py
    documents).

    A response with NO usage object raises IdentifyResultError: the
    call cannot be priced from nothing, and silently mapping it to {}
    used to settle the reservation at $0 — spend that may have billed
    vanished from the ledger (architecture Gauntlet fix 3). Raising
    leaves the reservation counted at its projection.
    """
    usage = getattr(response, "usage", None)
    if usage is None:
        raise IdentifyResultError(
            "The model's response carried no usage counts, so the "
            "call cannot be priced; treating the response as unusable "
            "and leaving the reservation counted."
        )
    cached = 0
    details = getattr(usage, "prompt_tokens_details", None)
    if details is not None:
        cached = getattr(details, "cached_tokens", 0) or 0
    return {
        "input_tokens": int(getattr(usage, "prompt_tokens", 0) or 0),
        "output_tokens": int(getattr(usage, "completion_tokens", 0) or 0),
        "cached_input_tokens": int(cached),
        "images": 0,
    }


def _merge_usage(a: UsageDict, b: UsageDict) -> UsageDict:
    return {key: int(a.get(key, 0)) + int(b.get(key, 0)) for key in _USAGE_KEYS}


# ---------------------------------------------------------------------------
# Segmentation-phrase hygiene: the plan's phrases must name a segmentable
# physical object, never the frame or the medium.
#
# The dry-run gate (2026-10-27) caught identify emitting whole-frame
# phrases — "phone screen", "smartphone display", "smartphone
# screenshot" — for a photo that IS a screenshot. SAM can never segment
# the medium itself, so cut_out burned its bounded retries on phrases
# that cannot match and the chain failed stochastically. The prompt now
# forbids these, and this deterministic layer enforces it regardless of
# what the model returns.
# ---------------------------------------------------------------------------

# Head nouns that name the frame / medium rather than an object in it.
_FRAME_HEAD_NOUNS = frozenset(
    {
        "screen",
        "display",
        "monitor",
        "photo",
        "photograph",
        "image",
        "picture",
        "background",
        "scene",
        "frame",
        "screenshot",
    }
)

_PHRASE_TOKEN_RE = re.compile(r"[a-z0-9]+(?:'[a-z]+)?")


def _phrase_tokens(phrase: str) -> list[str]:
    return _PHRASE_TOKEN_RE.findall(phrase.lower())


def _is_frame_phrase(phrase: str) -> bool:
    """True when a phrase names the frame/medium instead of an object.

    The head noun of an English noun phrase is its last token, so
    "phone screen" and "smartphone display" are frame phrases while
    "leather wallet" is not. "picture frame" is rejected too: frame is
    a medium noun under the plan contract even when a physical frame
    is the product (the model can name it differently, e.g. "framed
    print"). An empty/unparseable phrase counts as unusable too.
    """
    tokens = _phrase_tokens(phrase)
    if not tokens:
        return True
    head = tokens[-1]
    if head.endswith("s") and head[:-1] in _FRAME_HEAD_NOUNS:
        head = head[:-1]
    return head in _FRAME_HEAD_NOUNS


def _rewrite_frame_phrase(phrase: str) -> str | None:
    """Salvage a frame phrase by dropping its frame head noun.

    "smartphone display" -> "smartphone"; "phone screen" -> "phone".
    Returns None when nothing but the frame noun (and articles) is
    left — there is no object noun to segment.
    """
    tokens = _phrase_tokens(phrase)
    while tokens and _is_frame_phrase(" ".join(tokens)):
        tokens.pop()
    while tokens and tokens[0] in ("the", "a", "an"):
        tokens.pop(0)
    if not tokens:
        return None
    return " ".join(tokens)


# Tokens that end the opening noun phrase of a description: the object
# has been named once one of these shows up.
_DESCRIPTION_CUT_TOKENS = frozenset(
    {
        "on",
        "in",
        "against",
        "with",
        "beside",
        "near",
        "showing",
        "displaying",
        "featuring",
        "sits",
        "sitting",
        "placed",
        "lying",
        "standing",
        "resting",
        "of",
    }
)


def _phrase_from_description(description: str) -> str | None:
    """Last-resort object phrase from identify's own description.

    Takes the description's opening noun phrase (up to the first
    preposition/verb token or clause break), drops articles and any
    frame nouns, and returns what is left. Deterministic and
    conservative: returns None rather than inventing an object.
    """
    head_text = re.split(r"[.,;:]", description, maxsplit=1)[0]
    tokens = _phrase_tokens(head_text)
    while tokens and tokens[0] in ("the", "a", "an"):
        tokens.pop(0)
    kept: list[str] = []
    for token in tokens:
        if token in _DESCRIPTION_CUT_TOKENS:
            break
        if token in _FRAME_HEAD_NOUNS or (
            token.endswith("s") and token[:-1] in _FRAME_HEAD_NOUNS
        ):
            continue
        kept.append(token)
    if not kept or _is_frame_phrase(" ".join(kept)):
        return None
    return " ".join(kept)


def _sanitize_plan_phrases(
    concept: str, alternates: list[str], description: str
) -> tuple[str | None, list[str]]:
    """Constrain a plan's phrases to segmentable object phrases.

    Order of rescue, all deterministic:
    1. Phrases that already name an object pass through untouched
       (concept first, then alternates in the model's order).
    2. Frame phrases are rewritten by dropping the frame head noun
       ("smartphone display" -> "smartphone") and join the pool.
    3. If the pool is still empty, an object phrase is derived from
       identify's own description of the product.

    Returns (concept, alternates); concept is None when no usable
    phrase survives, which the caller treats as the repairable
    alternates-shortfall defect.
    """
    pool: list[str] = []

    def _add(phrase: str | None) -> None:
        if phrase and phrase.lower() not in {p.lower() for p in pool}:
            pool.append(phrase)

    for phrase in (concept, *alternates):
        if not _is_frame_phrase(phrase):
            _add(phrase.strip())
    for phrase in (concept, *alternates):
        if _is_frame_phrase(phrase):
            _add(_rewrite_frame_phrase(phrase))
    if not pool:
        _add(_phrase_from_description(description))
    if not pool:
        return None, []
    return pool[0], pool[1:]


def _parse_identify_payload(content: str) -> dict:
    """Parse and contract-check one identify response body.

    Raises IdentifyResultError on malformed JSON or any field that
    violates the contract (types, listing_bullets 3-5, alternates >= 2
    after cleaning). Cleaning includes the segmentation-phrase hygiene
    pass: concept/alternates must name a segmentable physical object,
    never the frame or the medium (see _sanitize_plan_phrases). The
    alternates shortfall — which a frame-phrase-only plan also lands
    in — is the one defect the caller may repair with a follow-up
    call, so it is raised with a distinguishable message prefix.
    """
    try:
        payload = json.loads(content)
    except (json.JSONDecodeError, TypeError) as exc:
        raise IdentifyResultError(
            f"Identify response is not valid JSON: {exc}",
            raw_response=content if isinstance(content, str) else None,
        ) from exc
    if not isinstance(payload, dict):
        raise IdentifyResultError("Identify response is not a JSON object.")

    for field in ("description", "concept", "suggested_title", "condition_notes"):
        if not isinstance(payload.get(field), str) or not payload[field].strip():
            raise IdentifyResultError(
                f"Identify response field {field!r} is missing or not a "
                "non-empty string."
            )
    bullets = payload.get("listing_bullets")
    if (
        not isinstance(bullets, list)
        or not 3 <= len(bullets) <= 5
        or not all(isinstance(b, str) and b.strip() for b in bullets)
    ):
        raise IdentifyResultError(
            "Identify response field 'listing_bullets' must be 3 to 5 "
            "non-empty strings."
        )

    concept = payload["concept"].strip()
    raw_alternates = payload.get("alternates")
    if not isinstance(raw_alternates, list):
        raise IdentifyResultError("ALTERNATES_SHORTFALL: 'alternates' is not a list.")
    cleaned: list[str] = []
    for item in raw_alternates:
        if isinstance(item, str):
            phrase = item.strip()
            if phrase and phrase.lower() != concept.lower() and phrase not in cleaned:
                cleaned.append(phrase)
    # Deterministic phrase hygiene: whatever the model returned, the
    # plan that reaches cut_out names segmentable physical objects
    # only — whole-frame phrases are promoted over, rewritten, or
    # replaced from the description (see _sanitize_plan_phrases). A
    # plan left with no concept, or fewer than 2 alternates, is the
    # repairable shortfall defect, same as before.
    concept_out, alternates = _sanitize_plan_phrases(
        concept, cleaned, payload["description"]
    )
    if concept_out is None:
        raise IdentifyResultError(
            "ALTERNATES_SHORTFALL: no segmentable object phrase in the "
            "plan — concept and alternates all name the frame or the "
            f"medium (concept was {concept!r})."
        )
    if len(alternates) < 2:
        raise IdentifyResultError(
            "ALTERNATES_SHORTFALL: fewer than 2 usable alternates after "
            f"cleaning (got {len(alternates)})."
        )

    return {
        "description": payload["description"].strip(),
        "concept": concept_out,
        # Hygiene honesty (architecture Gauntlet fix 4): when the
        # model's own concept did not survive as the plan's concept —
        # rewritten ("computer monitor" -> "computer"), promoted over,
        # or replaced from the description — the plan names what the
        # model actually said, so a caller can see the lead phrase is
        # not the model's choice. None when it passed through intact.
        "concept_rewritten_from": concept if concept_out != concept else None,
        "alternates": alternates,
        "suggested_title": payload["suggested_title"].strip(),
        "listing_bullets": [b.strip() for b in bullets],
        "condition_notes": payload["condition_notes"].strip(),
    }


def _capture_raw_response(content: str) -> Path:
    """Save a raw, unparseable model response under the data dir.

    The text lands in data/debug/identify-<call_id>.txt — operational
    state beside the asset store, named for the ledger call that
    produced it. It never goes in the ledger itself: the ledger records
    costs and counts only, never model text.
    """
    call_id = _current_call_id.get() or uuid.uuid4().hex
    debug_dir = config.get_data_dir() / "debug"
    debug_dir.mkdir(parents=True, exist_ok=True)
    path = debug_dir / f"identify-{call_id}.txt"
    path.write_text(content, encoding="utf-8")
    return path


def _with_debug_capture(exc: IdentifyResultError) -> IdentifyResultError:
    """Save an unparseable response's raw text and point the error at it."""
    if exc.raw_response is not None and exc.debug_path is None:
        exc.debug_path = str(_capture_raw_response(exc.raw_response))
    return exc


def vision_identify(
    png_bytes: bytes,
    *,
    model: str,
    client=None,
) -> tuple[dict, UsageDict]:
    """One real identify call against the Meta Model API.

    Sends the normalized RGBA PNG to the vision model with a strict
    structured-output request and returns (plan, usage). If the first
    response's only defect is too few usable alternates, ONE text-only
    repair call asks for more phrases (its usage is added to the total);
    a response that is still unusable raises IdentifyResultError. An
    unparseable response body (e.g. output truncated at max_tokens) is
    saved verbatim to data/debug/identify-<call_id>.txt before raising,
    and the error carries the path as `debug_path`.

    This function performs network I/O and must only be invoked as the
    fn of billed_call, so the spend is reserved before it runs and
    settled (or left counted) afterwards. `client` is injectable for
    offline tests; when omitted, the real client is built (which raises
    NotConfiguredError if no key is configured).

    One invocation is one attempt: when the body comes back empty or
    truncated, the IdentifyResultError raised here is a body failure
    (is_identify_body_failure) and the caller — server._run_identify —
    may re-roll it as a FRESH billed_call, up to IDENTIFY_MAX_ATTEMPTS
    attempts in total. The retry never happens inside this function,
    where it would spend behind the gate's back.
    """
    if client is None:
        client = build_client()
    data_uri = "data:image/png;base64," + base64.b64encode(png_bytes).decode()
    response = client.chat.completions.create(
        model=model,
        max_tokens=IDENTIFY_MAX_TOKENS,
        response_format={"type": "json_schema", "json_schema": _IDENTIFY_SCHEMA},
        messages=[
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": _IDENTIFY_INSTRUCTIONS},
                    {"type": "image_url", "image_url": {"url": data_uri}},
                ],
            }
        ],
    )
    usage = _usage_of(response)
    content = response.choices[0].message.content or ""
    try:
        return _parse_identify_payload(content), usage
    except IdentifyResultError as exc:
        if exc.raw_response is not None:
            # Unparseable body (truncated, prose, ...): keep the raw
            # text in a debug file so the failure can be diagnosed, and
            # let the error carry the path back to the caller.
            raise _with_debug_capture(exc) from exc
        if not str(exc).startswith("ALTERNATES_SHORTFALL"):
            raise

    # Repair path: the response was usable except for its alternates.
    # One text-only follow-up asks for replacement phrases; the concept
    # and the rest of the plan are kept from the first response.
    partial = json.loads(content)
    concept = str(partial.get("concept", "")).strip()
    repair = client.chat.completions.create(
        model=model,
        max_tokens=256,
        response_format={
            "type": "json_schema",
            "json_schema": {
                "name": "identify_alternates_repair",
                "strict": True,
                "schema": {
                    "type": "object",
                    "additionalProperties": False,
                    "properties": {
                        "alternates": {
                            "type": "array",
                            "items": {"type": "string"},
                            "minItems": 2,
                        }
                    },
                    "required": ["alternates"],
                },
            },
        },
        messages=[
            {
                "role": "user",
                "content": (
                    "A segmentation plan for a product photo uses the "
                    f"noun phrase {concept!r}. Give at least 2 different "
                    "fallback noun phrases for the same product, each "
                    "short and naming the concrete physical object "
                    "itself — never the screen, display, photo, image, "
                    "screenshot, background, scene, or frame."
                ),
            }
        ],
    )
    usage = _merge_usage(usage, _usage_of(repair))
    repair_content = repair.choices[0].message.content or ""
    try:
        repair_payload = json.loads(repair_content)
        extra = repair_payload["alternates"]
        if not isinstance(extra, list):
            raise TypeError("'alternates' is not a list")
    except (json.JSONDecodeError, TypeError, KeyError) as exc:
        err = IdentifyResultError(
            f"Alternates repair response is unusable: {exc}",
            raw_response=repair_content,
        )
        raise _with_debug_capture(err) from exc
    merged = dict(partial)
    merged["alternates"] = list(partial.get("alternates") or []) + list(extra)
    # Final contract check; raises if the repair did not fix the shortfall.
    return _parse_identify_payload(json.dumps(merged)), usage


# ---------------------------------------------------------------------------
# SAM 3.1 segmentation calls (Responses API transport).
# ---------------------------------------------------------------------------

# Three fixed phrases plus at most five band tiles. All requests remain
# inside one pre-reserved billed_call, with actual processed images settled.
SAM_MAX_ATTEMPTS = 8


def project_cut_out_cost_usd(model: str, max_attempts: int = SAM_MAX_ATTEMPTS) -> float:
    """Worst-case USD projection for one cut_out invocation.

    SAM is priced per processed image: one image per attempt, times the
    maximum attempts the recovery loop may make.
    """
    return pricing.estimate_cost_usd(model, images=max_attempts)


def _responses_output_text(response) -> str:
    """Extract the text lane from a Responses API result.

    Prefers the SDK's ``output_text`` convenience property; falls back
    to concatenating the text parts of the output items (the shape a
    mocked or non-SDK response may carry). Missing text reads as "",
    which the SAM parser treats as a valid empty lane (zero-match).
    """
    text = getattr(response, "output_text", None)
    if isinstance(text, str):
        return text
    parts: list[str] = []
    for item in getattr(response, "output", None) or []:
        for content in getattr(item, "content", None) or []:
            value = getattr(content, "text", None)
            if isinstance(value, str):
                parts.append(value)
    return "".join(parts)


def segment_image(
    png_bytes: bytes,
    phrase: str,
    *,
    model: str,
    client=None,
) -> tuple[str, UsageDict]:
    """One real SAM segmentation call; returns (output_text, usage).

    Sends the normalized PNG and the concept phrase to sam-3.1 and
    returns the model's raw output text — the special-token
    segmentation lane that sam_masks.parse_sam_masks decodes. Usage
    reports images=1 (SAM is priced per processed image); any token
    counts the response carries are merged in for the receipt, though
    the pricing table meters sam-3.1 on images alone.

    Contract assumption, recorded for the live verification: SAM 3.1 on
    the Meta Model API is a Responses API model — the official
    meta-sam-parser is built around Responses streams — so the call is
    client.responses.create with an input_text phrase and an
    input_image data URI, and the segmentation text arrives as
    output_text. If the live contract differs, this function (and the
    sam_masks boundary) is where the correction lands.

    This function performs network I/O and must only be invoked inside
    a billed_call fn (segmentation.run_cut_pipeline). `client` is
    injectable for offline tests; when omitted, the real client is
    built (which raises NotConfiguredError if no key is configured).
    """
    if client is None:
        client = build_client()
    data_uri = "data:image/png;base64," + base64.b64encode(png_bytes).decode()
    response = client.responses.create(
        model=model,
        input=[
            {
                "role": "user",
                "content": [
                    {"type": "input_text", "text": phrase},
                    {"type": "input_image", "image_url": data_uri},
                ],
            }
        ],
    )
    usage: UsageDict = {"images": 1}
    raw_usage = getattr(response, "usage", None)
    if raw_usage is not None:
        usage["input_tokens"] = int(getattr(raw_usage, "input_tokens", 0) or 0)
        usage["output_tokens"] = int(getattr(raw_usage, "output_tokens", 0) or 0)
    return _responses_output_text(response), usage


# ---------------------------------------------------------------------------
# Muse Image background plates (images API transport).
# ---------------------------------------------------------------------------

# Sizes the image call may request. The Meta image endpoint's exact
# size list is not yet verified live; these are the standard
# OpenAI-style sizes, and the request always uses the closest one to
# the product's dimensions. The compositor then fits the returned
# plate to the product's exact dimensions (plate only; the product
# layer is never scaled), so a size that is merely close costs nothing
# in fidelity. If the live endpoint accepts arbitrary WxH sizes, this
# list is the only thing to widen.
_IMAGE_SIZE_CANDIDATES: tuple[tuple[int, int], ...] = (
    (1024, 1024),
    (1536, 1024),
    (1024, 1536),
    (1792, 1024),
    (1024, 1792),
)

STAGE_SPEC_VERSION = "agenthand.stage-spec/2"
PLATE_MAX_ATTEMPTS = 2


@dataclass(frozen=True)
class StageSpec:
    stage_mode: str = "room"
    floor: str = "light wood"
    wall: str = "neutral plaster"
    lighting: str = "soft daylight"
    camera_class: str = "level architectural camera"
    version: str = STAGE_SPEC_VERSION

    def __post_init__(self):
        choices = {
            "stage_mode": {"room", "studio"},
            "floor": {"light wood", "matte concrete", "studio gray"},
            "wall": {"neutral plaster", "studio gray"},
            "lighting": {"soft daylight", "warm diffuse light"},
            "camera_class": {"level architectural camera"},
            "version": {STAGE_SPEC_VERSION},
        }
        for name, allowed in choices.items():
            if getattr(self, name) not in allowed:
                raise ValueError(f"unsupported StageSpec {name}")

    @classmethod
    def from_scene(cls, scene: str) -> "StageSpec":
        words = scene.lower()
        studio = "studio" in words
        return cls(
            stage_mode="studio" if studio else "room",
            floor="studio gray"
            if studio
            else "matte concrete"
            if "concrete" in words
            else "light wood",
            wall="studio gray" if studio else "neutral plaster",
            lighting="warm diffuse light" if "warm" in words else "soft daylight",
        )


# One versioned template. No caller prose, product pixels, or object-placement
# wording enters the image request. The compiled spec is disclosed in receipts.
_PLATE_INSTRUCTION = (
    "EMPTY architectural photograph. Continuous {floor} floor from the wall "
    "junction to every lower frame edge; {wall} wall; {lighting}; "
    "{camera_class}. Mode: {stage_mode}. One continuous floor plane. "
    "No raised slab, plinth, podium, platform, furniture, central object, "
    "vehicles, people, animals, text, logos or watermark; no product, and "
    "no object of the product's kind anywhere in the scene. "
    "Specification: {version}."
)


class ImageResultError(RuntimeError):
    """Raised when a muse-image plate call cannot produce a plate.

    Codes:
        IMAGE_REFUSED: the model refused the prompt (safety filter) or
            returned an explicit refusal instead of an image.
        BAD_IMAGE_RESPONSE: the call completed but carried no usable
            image (no data, undecodable bytes).
        IMAGE_CALL_FAILED: the transport call itself raised (HTTP
            error, timeout, ...).

    When this propagates out of the billed_call fn, the reservation
    stays counted: the call may have billed, so the spend is treated
    as spent.
    """

    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code


def wrap_plate_prompt(scene_prompt: str | StageSpec) -> str:
    """Compile a constrained versioned stage specification through one template."""
    spec = (
        scene_prompt
        if isinstance(scene_prompt, StageSpec)
        else StageSpec.from_scene(scene_prompt)
    )
    return _PLATE_INSTRUCTION.format(**asdict(spec))


def closest_image_size(width: int | None, height: int | None) -> str:
    """The supported request size closest to the given dimensions.

    Closest means nearest aspect ratio; ties go to the larger plate,
    because the compositor's fit downscales more gracefully than it
    upscales. Unknown dimensions fall back to the square default.
    """
    if not width or not height:
        return "1024x1024"
    target = math.log(width / height)

    def score(candidate: tuple[int, int]) -> tuple[float, int]:
        w, h = candidate
        return (abs(math.log(w / h) - target), -(w * h))

    best = min(_IMAGE_SIZE_CANDIDATES, key=score)
    return f"{best[0]}x{best[1]}"


def project_draw_cost_usd(model: str) -> float:
    """Worst-case USD projection for one draw invocation: one plate image."""
    return pricing.estimate_cost_usd(model, images=1)


def _plate_bytes_from_response(response) -> bytes:
    """Extract normalized RGBA PNG bytes from an images API response.

    Accepts b64_json (the usual OpenAI-compatible shape) or a url
    (fetched once). Raises ImageResultError: IMAGE_REFUSED for an
    explicit refusal, BAD_IMAGE_RESPONSE for anything else unusable.
    """
    data = getattr(response, "data", None) or []
    if not data:
        refusal = getattr(response, "refusal", None) or getattr(response, "error", None)
        if refusal:
            raise ImageResultError(
                "IMAGE_REFUSED",
                f"The image model refused the plate prompt: {refusal}",
            )
        raise ImageResultError(
            "BAD_IMAGE_RESPONSE",
            "The image model returned no image data for the plate prompt.",
        )
    item = data[0]
    refusal = getattr(item, "refusal", None)
    if refusal:
        raise ImageResultError(
            "IMAGE_REFUSED",
            f"The image model refused the plate prompt: {refusal}",
        )
    raw: bytes | None = None
    b64 = getattr(item, "b64_json", None)
    if isinstance(b64, str) and b64:
        try:
            raw = base64.b64decode(b64)
        except (ValueError, TypeError) as exc:
            raise ImageResultError(
                "BAD_IMAGE_RESPONSE",
                f"The plate image payload is not valid base64: {exc}",
            ) from exc
    if raw is None:
        url = getattr(item, "url", None)
        if isinstance(url, str) and url:
            import urllib.request

            try:
                with urllib.request.urlopen(url, timeout=60) as fh:
                    raw = fh.read()
            except Exception as exc:  # noqa: BLE001 - reported as a call failure
                raise ImageResultError(
                    "IMAGE_CALL_FAILED",
                    f"Fetching the generated plate image failed: {exc}",
                ) from exc
    if not raw:
        raise ImageResultError(
            "BAD_IMAGE_RESPONSE",
            "The image model returned an item with neither b64_json nor url.",
        )
    # Normalize whatever came back (PNG, JPEG, ...) to RGBA PNG bytes so
    # the asset store, hashes, and compositor all see one format.
    from io import BytesIO

    from PIL import Image

    try:
        with Image.open(BytesIO(raw)) as img:
            img.load()
            buffer = BytesIO()
            img.convert("RGBA").save(buffer, format="PNG")
            return buffer.getvalue()
    except Exception as exc:  # noqa: BLE001 - any decode failure is unusable
        raise ImageResultError(
            "BAD_IMAGE_RESPONSE",
            f"The plate payload does not decode as an image: {exc}",
        ) from exc


def _request_plate_image(
    prompt: str,
    width: int | None,
    height: int | None,
    *,
    model: str,
    client=None,
) -> tuple[bytes, UsageDict]:
    """One real muse-image call; returns (plate PNG bytes, usage).

    `prompt` is the caller's raw scene description; the fixed plate
    instruction is wrapped around it here so no caller can forget it.
    Usage reports images=1 (the model is priced per image successfully
    returned); token counts, if the response carries them, are merged
    in for the receipt but do not affect the per-image price.

    This function performs network I/O and must only be invoked inside
    a billed_call fn (generate_background_plate, today). `client` is
    injectable for offline tests; when omitted, the real client is
    built (which raises NotConfiguredError if no key is configured).
    """
    if client is None:
        client = build_client()
    size = closest_image_size(width, height)
    try:
        response = client.images.generate(
            model=model,
            prompt=wrap_plate_prompt(prompt),
            size=size,
            n=1,
        )
    except ImageResultError:
        raise
    except Exception as exc:  # noqa: BLE001 - SDK/transport failure
        raise ImageResultError(
            "IMAGE_CALL_FAILED",
            f"The muse-image plate call failed: {exc}",
        ) from exc
    png_bytes = _plate_bytes_from_response(response)
    usage: UsageDict = {"images": 1}
    raw_usage = getattr(response, "usage", None)
    if raw_usage is not None:
        usage["input_tokens"] = int(getattr(raw_usage, "input_tokens", 0) or 0)
        usage["output_tokens"] = int(getattr(raw_usage, "output_tokens", 0) or 0)
    return png_bytes, usage


def generate_background_plate(
    prompt: str,
    width: int | None,
    height: int | None,
    *,
    model: str = "muse-image-1.0",
    store=None,
    client=None,
    foreground_bbox: list[int] | tuple[int, ...] | None = None,
) -> tuple[str, dict]:
    """Generate a background plate through the billing choke point.

    Compile StageSpec through one fixed template and inspect the fitted plate.
    Each of at most two attempts reserves and settles its own billed_call.
    Repeated quality failure selects a disclosed deterministic studio backdrop.
    Returns its handle and aggregate billing/quality evidence.

    The plate is requested at the supported size closest to
    (width, height) — the product's dimensions — and is deliberately
    NOT fitted here: the compositor fits the plate to the product's
    exact dimensions, which is the Fidelity Lock rule (the plate
    adapts, the product never does).

    Raises NotConfiguredError before any reservation when no key is
    configured, ImageResultError (spend counted) when the call fails
    or is refused, and SpendCapExceeded when the reservation would
    exceed the cap.
    """
    if store is None:
        from .assets import AssetStore

        store = AssetStore(config.get_data_dir())
    from .assets import KIND_PLATE

    from .plate_quality import assess_plate
    from . import backdrop

    spec = prompt if isinstance(prompt, StageSpec) else StageSpec.from_scene(prompt)
    bbox = foreground_bbox or [
        int(0.2 * width),
        int(0.2 * height),
        int(0.8 * width),
        int(0.75 * height),
    ]
    attempts = []
    billings = []
    handle = None
    for _ in range(PLATE_MAX_ATTEMPTS):
        outcome = {}

        def _call() -> UsageDict:
            png_bytes, usage = _request_plate_image(
                spec, width, height, model=model, client=client
            )
            outcome["handle"] = store.put_bytes(
                png_bytes, kind=KIND_PLATE, suffix=".png"
            )
            return usage

        projected = project_draw_cost_usd(model)
        try:
            billing = billed_call("draw", model, projected, _call)
        except ImageResultError as exc:
            exc.cost_counted_usd = round(
                sum(b["actual_usd"] for b in billings) + projected, 10
            )
            raise
        billings.append(billing)
        handle = outcome["handle"]
        quality = assess_plate(store.read_image(handle), bbox, (width, height))
        record = store.get_record(handle)
        attempts.append(
            {
                "plate_asset": handle,
                "sha256": record.sha256,
                "pixel_sha256": record.pixel_sha256,
                "quality": quality,
                "call_id": billing["call_id"],
            }
        )
        if (
            quality["grade"] == "PASS"
            or "WALL_FLOOR_LINE_IN_PLACEMENT" in quality["codes"]
        ):
            # The fixed template cannot move this architectural line. Do not
            # purchase an identical second attempt for an incompatible framing.
            break
    fallback = quality["grade"] != "PASS"
    if fallback:
        handle = store.put_image(
            backdrop.render_plate("studio_gray", width, height), kind=KIND_PLATE
        )
        quality = assess_plate(
            store.read_image(handle),
            bbox,
            (width, height),
            code_backdrop=backdrop.backdrop_info("studio_gray"),
        )
    combined = dict(billings[-1])
    combined.update(
        actual_usd=round(sum(b["actual_usd"] for b in billings), 10),
        projected_usd=round(sum(b["projected_usd"] for b in billings), 10),
        usage={"images": len(billings)},
        call_ids=[b["call_id"] for b in billings],
        plate_quality={
            "assessment": quality,
            "attempts": attempts,
            "fallback": fallback,
            "fallback_reason": (
                "placement_horizon_mismatch"
                if fallback
                and "WALL_FLOOR_LINE_IN_PLACEMENT" in attempts[-1]["quality"]["codes"]
                else "two_rejected_plates"
                if fallback
                else None
            ),
            "stage_spec": asdict(spec),
        },
    )
    return handle, combined
