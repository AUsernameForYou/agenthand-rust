# Conformance

The frozen offline replay package (a real run, frozen 2026-10-26) plus the parity harness.
Judges run it with no keys:

    AGENTHAND_REPLAY_DIR=conformance/replay-package cargo test --release --test parity
    ./target/release/agenthand parity conformance/replay-package

Expected: PASS, 580,613 pixels checked, 0 changed.
