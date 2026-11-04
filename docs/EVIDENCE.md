# Proof Graph CLI: `agenthand evidence`

`agenthand evidence` reads the evidence database (SQLite, schema v2) and
prints its claims, runs, model calls, receipts and film chapters, re-runs
the reference checks over them, and walks the graph edges between them.

- Read-only: the database is opened with `SQLITE_OPEN_READ_ONLY` and
  `PRAGMA query_only = ON`. No verb writes to the database or to any
  receipt file.
- Offline: no verb opens a network connection. No API key is read.
- The database contents, `schema.sql` and the importer
  (`import_inventory.py`) are maintained in the evidence layer. This
  subcommand mirrors them; it does not build or edit the database.

```sh
agenthand evidence verify --all
agenthand evidence trace chapter:3
agenthand evidence root
```

## Database location

First match wins:

1. `--db <path>`
2. `AGENTHAND_EVIDENCE_DB` (environment)
3. `evidence/evidence.db` under the current directory (the repo root,
   when run from there)
4. `evidence/evidence.db` beside the binary, or in any parent directory
   of the binary (covers `target/release/agenthand` inside the repo)

If `--db` or `AGENTHAND_EVIDENCE_DB` names a file that does not exist,
the command stops with exit 2 and names the path; it does not fall
through to the later locations. If nothing is found, the error lists
every path that was tried.

## Receipt base directory

`receipts.source_file` is either absolute or relative. Relative paths
resolve against:

1. `--base-dir <dir>`
2. `AGENTHAND_EVIDENCE_BASE` (environment)
3. the parent of the database's directory

Rule 3 is the rule `verify_db.py` uses (its `ROOT` is the parent of the
directory holding `evidence.db`). For a database at
`<repo>/evidence/evidence.db` the base is `<repo>`. `verify` prints the
base it used, and the rule that chose it, on a `# receipt base dir:`
line. Absolute paths are used as they are, whatever the base.

Receipts layout. The hashed receipt files ship in the repo next to the
database, one file per receipt, and `receipts.source_file` holds the
repo-relative path:

```
<repo>/
  evidence/
    evidence.db
    receipts/
      <receipt_id>.json      source_file = evidence/receipts/<receipt_id>.json
```

With no `--base-dir` and no `AGENTHAND_EVIDENCE_BASE`, rule 3 makes the
base `<repo>`, so `evidence/receipts/32.json` resolves to
`<repo>/evidence/receipts/32.json`. This holds whether the database was
named with `--db`, `AGENTHAND_EVIDENCE_DB`, or found as
`./evidence/evidence.db` from the repo root. A copy of `evidence.db`
moved somewhere else needs `--base-dir <repo>` (or the receipts copied
along with it at the same relative place); otherwise its receipts are
`SKIP` and the verdict is `INCOMPLETE`.

## Verbs

Implemented:

| Verb | What it prints |
|---|---|
| `list [--chapter <c>] [--status <s>]` | Claims with status, rubric criterion and chapters. `--status` is one of `verified`, `corrected`, `withdrawn`, `refused`. |
| `show <claim-id>` | One claim: text, scope fields, reason, source, each `evidence_map` row with its run (calls, totals, verify result) and receipt (sha256, source file, verify command, expected line), and the chapters that cite it. |
| `verify <claim-id>` / `verify --all` / `verify-all` | The reference checks, one line per check, then one `OVERALL:` line (`PASS`, `INCOMPLETE` or `FAIL`, with counts). See below. |
| `trace <node-id>` | The directed walk over `edges` from a node, as an indented tree, then the reverse walk (incoming edges). |
| `chapter <c>` | The film chapter at a timestamp (or by id or label), its claim, and its trace. |
| `root` | The receipts root recomputed from the `receipts.file_sha256` rows, and whether `meta` holds the same value and count. |
| `graph [--format mermaid\|dot]` | Every node in the `nodes` view and every row in `edges` (default `mermaid`). |
| `rubric` | `evidence_map` rows grouped by rubric criterion. |
| `calls [--by-model] [--run <id>]` | `calls` rows, or per-model counts and settled-cost sums. |
| `tamper <receipt-id>` | Reads the receipt file, flips one byte in memory, and shows the sha256 of the altered copy no longer matches the database. The file on disk is not modified. |
| `export <dir>` / `export --dir <dir>` | Writes a judge pack to `<dir>`: the database, the receipt files, `PACK.md` and `manifest.json`. The only verb that writes, and only inside `<dir>`. See below. |

Planned, not implemented (the verb prints `planned, not implemented`
and exits 3):

| Verb | Status |
|---|---|
| `parity` | Cross-build parity node. The database has no parity rows yet. |

`--json` works on every verb and prints one JSON document on stdout.

Identifiers:

- Claim ids: `N` or `claim:N` (`show`, `verify`).
- Node ids (`trace`): `claim:N`, `run:N`, `call:N`, `receipt:N`,
  `chapter:N`, `criterion:<name>`, `model:<name>`, `build:<hash>`, as
  defined by the `nodes` view in `schema.sql`. A bare `N` means
  `claim:N`.
- Chapter selectors (`chapter`, `list --chapter`): a chapter id (`7`),
  `chapter:7`, a timestamp (`1:42`, `0:01:42`, `102.5`, `102.5s`), or
  the first token of the narration line (`Ch5`, `ChClose`). A bare
  integer is a chapter id, not seconds. Timestamps match
  `t_start_s <= t <= t_end_s`; on a shared boundary the chapter that
  starts there is chosen.

## `verify`

`verify --all` runs the checks of `verify_db.py`, in the same order,
with the same line text and the same statuses:

| Section | Check |
|---|---|
| (a) | Each receipt with a `file_sha256`: the sha256 of the file on disk equals it. A file that is not present is `SKIP` (`file not present locally`), not a failure. |
| (b) | Each run with a `total_settled_usd`: it equals the sum of its costed calls within 1e-6. No costed calls, or a difference with non-blank `runs.notes`, is `REPORT`. A difference with blank notes is `FAIL`. |
| (c) | No call or receipt without its run; every call and receipt has its `has_call` / `has_receipt` edge; each claim's directed walk reaches a receipt. Withdrawn and refused claims without a receipt are `PASS` by design. |
| (d) | `meta.receipts_root` and `meta.receipt_count_rooted` equal the recomputed root and count. |
| (e) | Each claim has `scope_photo`, `scope_build` and `scope_date`, or a reason; withdrawn and refused claims have a reason. |

Statuses: `PASS`, `FAIL`, `REPORT` (a fact with its explanation on
record), `SKIP` (not checked: file absent). The output starts with
`# db:` and `# receipt base dir:` lines; every other line has the same
text as `verify_db.py` prints.

### The `OVERALL` line

The last line carries three counts:

```
OVERALL: PASS (30 re-hashed, 0 skipped, 0 failing line(s))
OVERALL: INCOMPLETE (0 re-hashed, 30 skipped, 0 failing line(s))
OVERALL: FAIL (29 re-hashed, 0 skipped, 1 failing line(s))
```

- re-hashed: receipt files in scope whose sha256 was recomputed from
  disk and matched the database (section (a) `PASS` lines). A mismatch
  is a `FAIL` line, not a re-hash.
- skipped: hashed receipts in scope whose file was not present (section
  (a) `SKIP` lines).
- failing line(s): `FAIL` lines in all sections.

Verdict, in order:

1. Any `FAIL` line: `FAIL`, exit 1. This holds even when receipts were
   also skipped.
2. No `FAIL`, no `SKIP`, at least one receipt re-hashed: `PASS`, exit 0.
3. Otherwise `INCOMPLETE`, exit 4: nothing failed, but some receipt
   files were not checked, or none were. A scope with no hashed receipts
   at all (for example `verify` on a withdrawn claim, whose walk reaches
   no receipt) prints `INCOMPLETE (0 re-hashed, 0 skipped, 0 failing
   line(s))`: nothing was re-hashed, so nothing was proven.

A `PASS` therefore always means every hashed receipt in scope was read
from disk and matched. A copy of the database without its receipt files
cannot pass.

`--json` carries the same verdict in `overall` (`PASS`, `INCOMPLETE`,
`FAIL`) and the counts in `rehashed`, `skipped` and `failures`.

`verify <claim-id>` runs the same sections limited to that claim: (a)
and (b) for the receipts and runs its directed walk reaches, (c) for its
walk, (d) as in `--all` (the root covers every receipt), and (e) for the
claim. A `# scope:` line states this.

## `root`

The construction is the one in `schema.sql` and
`import_inventory.py`:

```
root = sha256( concat( sort_ascending( receipts.file_sha256 where not NULL ) ) )
```

The hex digests are sorted as strings, concatenated with no separator,
and hashed as UTF-8 bytes; duplicates are kept. It is one hash over the
sorted list, not a Merkle tree: there are no per-leaf hashes and no
pairing, so it does not yield per-receipt inclusion proofs.

`root` always prints the recomputed value on its first line
(`receipts root: <hex>`). It then states whether `meta.receipts_root`
and `meta.receipt_count_rooted` match. A stored value is printed only
when it differs, and labelled as the stored value. Exit 1 when either
differs.

Value for `evidence.db` of 2026-11-02 (30 hashed receipts of 33):
`1179530968c27879e3d84aa5475998a475cba64ba5b95f1823a8e7310174ecc3`.

## `export`

`export <dir>` (or `--dir <dir>`) writes a judge pack. `<dir>` is
created if absent; if it exists it must be an empty directory, or the
command refuses with exit 2 and writes nothing. The source database is
opened read-only as for every other verb and is left byte-identical.
Nothing is written outside `<dir>`, and every name inside it is fixed
or built from an integer receipt id, so database contents cannot steer
a write elsewhere.

```
<dir>/
  evidence.db                 byte copy of the database
  receipts/<receipt_id>.json  each receipt whose source_file resolves (same rule as verify)
  manifest.json               db sha256, receipts root, row counts, per-file sha256 and
                              whether it matches the database, missing / no-file /
                              mismatched receipt ids (no timestamps; deterministic)
  PACK.md                     plain-language description, how to verify, the EXPORT line,
                              one line per file-backed receipt left out (with the
                              reason), and the receipts with no file by design
```

The `receipts/` folder matches the shipped layout: put `evidence.db`
and `receipts/` in a folder named `evidence` and run
`agenthand evidence verify --all` from its parent.

Only file-backed receipts count: a receipt whose row names a file
(non-empty `source_file`). Each included file is re-hashed and compared
with `receipts.file_sha256`. A receipt whose row names no file has
nothing to re-hash; it gets its own named line and never makes the
pack `PARTIAL`. On the shipped database that is receipts 5, 15 and 17,
so with all 30 receipt files present the output ends:

```
No file by design: receipts 5, 15, 17 (the database names no file to re-hash)
EXPORT: COMPLETE (30 of 30 file-backed receipt files included, 0 missing)
```

The `EXPORT` line is always last and has one shape:

| Verdict | When | Exit |
|---|---|---|
| `EXPORT: COMPLETE (N of N file-backed receipt files included, 0 missing)` | every file-backed receipt file was found, copied, and matches its database sha256 | 0 |
| `EXPORT: PARTIAL (I of N file-backed receipt files included, M missing)` | a file-backed receipt file was not found (listed on an `export: missing receipt file(s):` line) | 4, as `OVERALL: INCOMPLETE` |
| `EXPORT: FAIL (I of N ..., M missing, F not matching the database)` | an included file does not match its database sha256; it is still copied and named, so `verify` on the pack fails too. Overrides `PARTIAL` | 1 |

`--json` prints the manifest plus `status` and `dir`.

## `trace` output

```
downstream (outgoing edges):
  chapter:3  [0:37.080-0:51.850] Ch2 hero: ...
    supports -> claim:1  [verified] Fidelity Lock receipt for the hero bike run ...
      evidenced_by -> run:6  Hero run IMG_1443 (...)  build=166ea6b date=2026-11-02 total_settled_usd=0.019175
        has_call -> call:11  identify (muse-spark-1.3)  settled_usd=0.006675
          uses_model -> model:muse-spark-1.3
        has_receipt -> receipt:6  fidelity_receipt 5072763 checked / 0 changed
            sha256: c98fc09c...
            source_file: film-assets/P04-P09/run-1443-v102/receipt.json
            verify_cmd: agenthand verify source.png staged.png mask.png
            expected_line: verify PASS : checked 5,072,763 px, changed 0 px, ok=True
        built_with -> build:166ea6b
      evidenced_by -> receipt:6  (shown above)
      addresses -> criterion:Technical Depth & Execution
upstream (incoming edges into chapter:3): none
```

Each line is `<relation> -> <node>` followed by the node's row. Lines
without `->` are attributes of the node above them. A node reached a
second time is printed once more with `(shown above)` and not expanded.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success; `verify`: `OVERALL: PASS`; `export`: `EXPORT: COMPLETE`; `root`: meta matches or is absent; `tamper`: the altered copy fails the check, or the file is not checked |
| 1 | `verify`: `OVERALL: FAIL`; `export`: `EXPORT: FAIL` (a packed receipt file does not match its database sha256); `root`: meta differs; `tamper`: the file on disk already differs from the database |
| 2 | Usage error, unknown id, database not found or not schema v2; `export`: target not empty or not writable |
| 3 | Planned verb, not implemented |
| 4 | Incomplete, nothing failed: `verify`: `OVERALL: INCOMPLETE` (receipt files skipped or none re-hashed); `export`: `EXPORT: PARTIAL` (file-backed receipt files left out of the pack) |

## Parity with `verify_db.py`

Recorded 2026-11-02 against `evidence.db`
(sha256 `67c50e21637fc90b5b2c3a136568c762285fa6cd2689e1e7d0f622c2eabbd930`)
and `verify_db.py`
(sha256 `351b16d028470cf6b96e719b4e3c1eb73799c06c2ef974fa584f961311950e51`):
after the two `# ` header lines, `agenthand evidence verify --all`
printed the same bytes as `python3 verify_db.py`, and both exited 0
with `OVERALL: PASS (0 failing line(s))`. On that machine none of the 29
hashed receipt files were present, so section (a) was 29 `SKIP` lines
in both. Three synthetic databases (all-pass; a mix of every `FAIL`,
`REPORT` and `SKIP` case; a wrong stored root) also gave identical
output and exit codes.

That record predates the `INCOMPLETE` verdict (2026-11-02). The run it
describes, 29 `SKIP` lines and no `FAIL`, is exactly the case that no
longer passes: agenthand now prints
`OVERALL: INCOMPLETE (0 re-hashed, 29 skipped, 0 failing line(s))` and
exits 4. `verify_db.py` takes the same ruling; the per-check lines are
unchanged, and the `OVERALL` line and exit code match once both carry
it.

## Tests

`tests/evidence_cli.rs` builds small databases from
`tests/fixtures/evidence/schema.sql` (a byte copy of the evidence
layer's `schema.sql`, sha256
`a61a63193c61ae652a76a25ed24effc7664113a5eb9e9aedf4ffaf3a4ad3bf67`) in a
temp dir and runs the binary: every receipt file present (`PASS`, exit
0); receipt files missing, including a database copy with an empty base
directory (`INCOMPLETE`, exit 4); a tampered receipt file with another
receipt skipped and a tampered database sha (`FAIL`, exit 1); the
`evidence/receipts/<id>.json` layout resolved from the repo root (by
default, from `./evidence/evidence.db`, with `--base-dir` and with
`AGENTHAND_EVIDENCE_BASE`, plus an absolute path); withdrawn claims with
and without a reason; the root for 1, 2 and 3 receipts against
hand-computed values; a wrong stored root; the database left unchanged
by every verb; trace, graph, tamper, planned verbs and database
location; `export`: a partial pack (2 of 3 file-backed, one receipt
with no file by design; exit 4, source database untouched,
pack database byte-identical, manifest hashes and root checked), a
complete pack (with a no-file receipt) that verifies `PASS` from its
own copy, a changed receipt file (`EXPORT: FAIL`, exit 1), and a
refused non-empty target. One `#[ignore]` test runs against the real database and its
30 receipt files and expects
`OVERALL: PASS (30 re-hashed, 0 skipped, 0 failing line(s))`:

```sh
AGENTHAND_EVIDENCE_DB=<repo>/evidence/evidence.db \
  cargo test --release --test evidence_cli -- --ignored
```

Set `AGENTHAND_EVIDENCE_BASE` as well if the receipts are not under the
database's repo root.
