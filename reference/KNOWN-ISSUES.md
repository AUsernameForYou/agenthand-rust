# Known issues in this frozen snapshot

This folder is a frozen snapshot of the Python reference implementation of
agenthand, kept byte-honest so the shipped Rust build can be checked against
it (the Differential Conformance Suite). It is not edited after freezing.
This note sits beside the snapshot; nothing in the snapshot was changed to
write it.

## The 4 failing tests

The snapshot's own internal test suite collects 562 tests. Four fail, all in
`tests/test_cut_out.py`:

- `test_cut_out_success_with_provided_concept`
- `test_cut_out_recovers_with_first_alternate_after_zero_match`
- `test_cut_out_refuses_a_composite_handle`
- `test_cut_out_scales_oversize_source_and_maps_the_mask_back`

The same 4 failures occur on a pristine checkout of the freeze commit, so
they are not caused by anything done after the freeze. They are
state-sensitive artifacts of the frozen copy: the assertions assume a
response shape the frozen tree no longer produces. They are not defects
found in the living code.

## What is gated, and what passes

- The living Python implementation (the active code, outside this repo)
  passes its full suite: 648 passed / 0 failed.
- The checks that gate this submission are the Differential Conformance
  Suite (shared schema and test vectors, byte-identical in both builds)
  and the frozen replay parity check. Both pass.

## Why this is disclosed instead of fixed

Standing rule (Dan Wagner, 2026-11-04): frozen proof material is never
edited to make a check pass. Defects in it are disclosed beside it, like
this, and fixes land in the living code. Editing the snapshot to turn these
4 tests green would break the byte-honesty the conformance story rests on,
and the green would prove nothing about the product.
