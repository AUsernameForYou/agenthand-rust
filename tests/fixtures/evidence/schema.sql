-- agenthand evidence layer — schema v2 (2026-11-02)
-- Implements EVIDENCE-LAYER.md (schema v2 + graph layer).
-- Plain SQLite only: the shipped interface is the Rust binary's rusqlite
-- subcommand, so no Python-only tricks may creep in here.
--
-- Layout choices beyond the spec draft (all lossless w.r.t. the inventory):
--   * runs gains verify_by, source, notes — provenance for verify_result and
--     the place where inventory flags (UNSOURCED / MISMATCH) stay visible.
--   * calls gains mask_px (cut_out product) and mode (draw plate kind),
--     which the inventory records per call, not per run.
--   * The graph layer is split: `nodes` is a VIEW over the base tables
--     (a derived node can never drift from its row), while `edges` is a
--     TABLE materialised by the importer from the foreign keys at import
--     time (recursive-CTE traversal needs an indexable edge store).
--   * Receipts root construction: collect the sha256 hex digest of every
--     receipt that carries one, sort those hex strings ascending, and set
--     receipts_root = sha256 of their UTF-8 concatenation with no
--     separators. receipt_count_rooted records how many digests entered
--     the construction. Both values live in the meta table.

PRAGMA foreign_keys = ON;

-- ---------------------------------------------------------------------------
-- Base tables
-- ---------------------------------------------------------------------------

CREATE TABLE runs (
    run_id            INTEGER PRIMARY KEY,
    label             TEXT NOT NULL,
    run_date          TEXT,               -- ISO date as recorded; NULL if unrecorded
    build_hash        TEXT,               -- verbatim from the run record ('unknown' kept as-is)
    item              TEXT,
    package_path      TEXT,               -- package location, meta-hackathon-root relative
    total_settled_usd REAL,               -- NULL when no settled total is on record
    verify_result     TEXT,               -- verbatim verdict text from the package
    verify_by         TEXT,               -- who/what produced verify_result
    source            TEXT,               -- provenance: exact files the figures came from
    notes             TEXT                -- inventory notes + flags, verbatim
);

CREATE TABLE calls (
    call_id     INTEGER PRIMARY KEY,
    run_id      INTEGER NOT NULL REFERENCES runs(run_id),
    tool        TEXT NOT NULL,            -- 'identify' | 'cut_out' | 'draw'
    model       TEXT,                     -- NULL when the package does not record it
    settled_usd REAL,                     -- NULL when the cost is not broken out
    duration_s  REAL,                     -- NULL: the inventory records no durations
    mask_px     INTEGER,                  -- cut_out only: mask pixels produced
    mode        TEXT                      -- draw only: 'source_edit' | 'model_generated'
);

CREATE TABLE receipts (
    receipt_id  INTEGER PRIMARY KEY,
    run_id      INTEGER NOT NULL REFERENCES runs(run_id),
    kind        TEXT NOT NULL,            -- 'fidelity_receipt' | 'keep_record' | 'recount'
    checked_px  INTEGER,                  -- NULL when the record carries no pixel count
    changed_px  INTEGER,
    file_sha256 TEXT,                     -- sha256 of the receipt file on disk; NULL if none
    source_file TEXT,                     -- receipt file path (meta-hackathon-root relative, or absolute)
    verify_cmd  TEXT,                     -- exact re-check command documented in the package; NULL if none
    expected_line TEXT                    -- exact documented success line for verify_cmd; NULL if none
);

CREATE TABLE claims (
    claim_id    INTEGER PRIMARY KEY,
    text        TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'verified'
                CHECK (status IN ('verified', 'corrected', 'withdrawn', 'refused')),
    scope_photo TEXT,                     -- photo/run scope as a field, never buried in prose
    scope_build TEXT,                     -- build scope as a field
    scope_date  TEXT,                     -- date scope as a field
    reason      TEXT,                     -- required context for withdrawn/refused claims
    source_url  TEXT                      -- source citation; may be a local path reference
);

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE evidence_map (
    map_id          INTEGER PRIMARY KEY,
    claim_id        INTEGER NOT NULL REFERENCES claims(claim_id),
    rubric_criterion TEXT NOT NULL,       -- e.g. 'Technical Depth & Execution'
    run_id          INTEGER REFERENCES runs(run_id),
    receipt_id      INTEGER REFERENCES receipts(receipt_id)
);

CREATE TABLE video_chapters (
    chapter_id     INTEGER PRIMARY KEY,
    t_start_s      REAL NOT NULL,
    t_end_s        REAL,
    narration_line TEXT,
    claim_id       INTEGER REFERENCES claims(claim_id)
);

-- ---------------------------------------------------------------------------
-- Graph layer
-- ---------------------------------------------------------------------------

-- Nodes are a view: one row per graph node, derived from the base tables.
-- node_id is the stable address used by edges and by `agenthand evidence trace`:
--   claim:<id>  run:<id>  call:<id>  receipt:<id>  chapter:<id>
--   criterion:<name>  model:<name>  build:<build_hash>
CREATE VIEW nodes AS
    SELECT 'claim:'     || claim_id   AS node_id, 'claim'     AS node_type,
           claim_id AS ref_id, text AS label FROM claims
    UNION ALL
    SELECT 'run:'       || run_id,    'run',       run_id,    label FROM runs
    UNION ALL
    SELECT 'call:'      || call_id,   'call',      call_id,
           tool || COALESCE(' (' || model || ')', '') FROM calls
    UNION ALL
    SELECT 'receipt:'   || receipt_id,'receipt',   receipt_id,
           kind || COALESCE(' ' || checked_px || ' checked / ' || changed_px || ' changed', '')
           FROM receipts
    UNION ALL
    SELECT 'chapter:'   || chapter_id,'chapter',   chapter_id,
           COALESCE(narration_line, 'chapter ' || chapter_id) FROM video_chapters
    UNION ALL
    SELECT 'criterion:' || rubric_criterion, 'criterion', NULL, rubric_criterion
           FROM (SELECT DISTINCT rubric_criterion FROM evidence_map)
    UNION ALL
    SELECT 'model:'     || model,     'model',     NULL, model
           FROM (SELECT DISTINCT model FROM calls WHERE model IS NOT NULL)
    UNION ALL
    SELECT 'build:'     || build_hash,'build',     NULL, build_hash
           FROM (SELECT DISTINCT build_hash FROM runs
                 WHERE build_hash IS NOT NULL AND build_hash <> 'unknown');

-- Edges are materialised by the importer, derived from the foreign keys —
-- never edited by hand. `source` names the FK each edge was derived from,
-- so any edge can be audited back to the row that produced it.
CREATE TABLE edges (
    edge_id   INTEGER PRIMARY KEY,
    from_node TEXT NOT NULL,
    to_node   TEXT NOT NULL,
    relation  TEXT NOT NULL,   -- 'evidenced_by' | 'addresses' | 'has_call' |
                               -- 'has_receipt' | 'uses_model' | 'built_with' | 'supports'
    source    TEXT NOT NULL    -- e.g. 'evidence_map.run_id', 'calls.run_id'
);

-- ---------------------------------------------------------------------------
-- Indexes a judging agent (or the trace walker) needs
-- ---------------------------------------------------------------------------

CREATE INDEX idx_runs_label        ON runs(label);
CREATE INDEX idx_runs_build        ON runs(build_hash);
CREATE INDEX idx_runs_date         ON runs(run_date);
CREATE INDEX idx_calls_run         ON calls(run_id);
CREATE INDEX idx_calls_tool        ON calls(tool);
CREATE INDEX idx_calls_model       ON calls(model);
CREATE INDEX idx_receipts_run      ON receipts(run_id);
CREATE INDEX idx_receipts_sha      ON receipts(file_sha256);
CREATE INDEX idx_map_claim         ON evidence_map(claim_id);
CREATE INDEX idx_map_criterion     ON evidence_map(rubric_criterion);
CREATE INDEX idx_map_run           ON evidence_map(run_id);
CREATE INDEX idx_chapters_claim    ON video_chapters(claim_id);
CREATE INDEX idx_chapters_start    ON video_chapters(t_start_s);
CREATE INDEX idx_edges_from        ON edges(from_node);
CREATE INDEX idx_edges_to          ON edges(to_node);
CREATE INDEX idx_edges_relation    ON edges(relation);
