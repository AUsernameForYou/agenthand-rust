# M6 prep manifest — branch `m6-prep` (DELETE AT FLIP)

This branch is packaging prep for the 0.3.0 / M6 release, branched
off consolidate master `59782d7` on 2026-10-28 so it never touches
the two in-flight fix lanes (M5 fixes, SAM oversize). Nothing here
is a final binary. The ordered final packaging checklist is
`~/workspace/meta-hackathon/rust-100/M6-RUNBOOK.md`.

## On this branch

- `Cargo.toml` / `Cargo.lock`: version 0.2.0 → **0.3.0** (minor
  bump, the repo's pre-1.0 scheme: 0.3.0 carries the room run, the
  completed seller workflow, and native HEIC decode over 0.2.0).
  The submission freeze tag stays `v1.0.0-hackathon` (a submission
  marker, not the package version).
- `CHANGELOG.md`: 0.3.0 release-notes skeleton populated from the
  milestone record; the two fix-lane items are marked PENDING and
  must be un-marked only when those lanes land and re-verify.
- `.github/workflows/ci.yml` + the CI badge in `README.md`
  (winners-gems B3): fmt check + full test suite + a musl release
  build with a static-link assertion, on GitHub's machines.
- `SUBMISSION.md` (B5): the gallery text, byte-identical to
  `SUBMISSION-TEXT.md` §1 plus the decoder paragraph in the tracker
  decision-66 wording. At the Nov 3 claim lock the gallery text
  adopts the same decoder paragraph so repo and gallery agree.
- `README.md`: final pass — CI badge, freeze-tag note (B4),
  room-run section, Open Weight section (Baseten Spark Open
  narration, per the Discord staff ruling for the Best Build on
  Meta Open Weight Models track), six-subcommand architecture,
  native-HEIC install note, "It reads photos" section (decision-66
  wording), the measured baseline counterpoint table (claims
  #27/#29), and refusals updated for the decoder subset and the
  40 MP / 20 MB ingest budget. Install filenames moved to 0.3.0.
- `scripts/clean-install-verify.sh`: the scripted clean-install
  gate (fresh client config, handshake, frozen proof, HEIC decode
  smoke, optional cross-build receipt verify).

## Known open flags (do not silently resolve)

- **Install section wording:** README links GitHub Releases as the
  download channel. Decision 55 holds interim distribution on
  Google Drive with the repo private; the release-lane word on the
  final wording is still open. Flagged, not rewritten.
- **macOS builds cannot be produced on this VM** (no Darwin C
  toolchain; `cargo check --target aarch64-apple-darwin` fails in
  ring's build script). The final run builds macOS on the Mac, as
  0.2.0 was built.
- **License:** README states Proprietary (the standing text). The
  MIT-vs-Proprietary call is Daniel's and is not made here.
