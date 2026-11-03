# Offline replay package — the verified 2026-10-26 run, frozen

Everything needed to re-prove agenthand's Fidelity Lock claim with
**zero network and no API key**. Frozen from the live end-to-end run of
2026-10-26 23:13:47–23:14:23 UTC (identify → cut_out → draw, settled
cost $0.020044 — the run's own ledger lines are in
`run/ledger-lines.jsonl`, verbatim).

## Contents

- `run/assets/` — the run's five assets with their store sidecars:
  source photo, mask, product layer (foreground), background plate,
  final composite.
- `receipt.json` — the run's fidelity receipt: hashes for every asset,
  580,613 opaque pixels checked, **0 changed**, colour profile
  preserved. (The original run predates the expanded receipt schema;
  count/profile fields were re-derived from these frozen files with
  the current code at freeze time and reproduce the originally
  recorded numbers exactly — see the `provenance` block in the file.)
- `manifest.json` — sha256 of every frozen file; the replay checks it
  first, so a tampered freeze fails before anything else runs.
- Replay script (in the repo):
  `~/workspace/agenthand/scripts/replay_verified_run.py`

## Run it

```sh
env -u META_API_KEY python3 ~/workspace/agenthand/scripts/replay_verified_run.py
```

It verifies the manifest, re-derives the receipt from the files on
disk (`composite.verify_receipt_from_disk`), independently checks the
exported source/composite/mask (`agenthand.verify`), prints the
counts, and ends with `REPLAY: PASS` (exit 0) or `FAIL` (exit 1).

## When to cut to it

**On the first live failure on camera. No on-camera retries.** If the
live call fails, errors, or stalls during filming or the live final,
cut to this package and re-verify the recorded run in the room. Do not
re-roll the live call hoping for a clean take in front of judges.

## How it is labelled — the honesty rule

This package is **a recording of a real run, verified live in the
room by re-checking its files**. That is exactly what the film and the
presenter may say:

- ✅ "This is the run we recorded on October 26. Watch us re-verify
  its files right now — the hashes and the pixel count reproduce."
- ❌ Never present the replay as a fresh live call, a new run, or a
  result produced on stage. The verification is live; the run is not,
  and the labelling must keep that line bright.

The claim being re-proven is the precise one: the stored product
layer was never regenerated, and every one of its 580,613 opaque
pixels reached the final image byte-identical, colour profile intact.
