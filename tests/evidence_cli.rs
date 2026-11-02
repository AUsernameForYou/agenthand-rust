//! `agenthand evidence` (the Proof Graph CLI) end to end.
//!
//! Each test builds a small schema-v2 database from
//! tests/fixtures/evidence/schema.sql (a byte copy of the evidence
//! layer's schema.sql) in a temp dir laid out the way verify_db.py
//! expects: `<root>/evidence-layer/evidence.db`, receipt files under
//! `<root>/pkg/`, so relative receipt paths resolve against the parent
//! of the database's directory. Edges are derived with the importer's
//! SQL; the stored receipts root is computed here independently.
//! No network, no Python.

use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

const SCHEMA: &str = include_str!("fixtures/evidence/schema.sql");

// The importer's edge derivation (import_inventory.py), verbatim SQL.
const EDGE_SQL: [&str; 8] = [
    "INSERT INTO edges (from_node, to_node, relation, source) \
     SELECT 'claim:' || claim_id, 'run:' || run_id, 'evidenced_by', 'evidence_map.run_id' \
     FROM evidence_map WHERE run_id IS NOT NULL",
    "INSERT INTO edges (from_node, to_node, relation, source) \
     SELECT 'claim:' || claim_id, 'receipt:' || receipt_id, 'evidenced_by', 'evidence_map.receipt_id' \
     FROM evidence_map WHERE receipt_id IS NOT NULL",
    "INSERT INTO edges (from_node, to_node, relation, source) \
     SELECT 'claim:' || claim_id, 'criterion:' || rubric_criterion, 'addresses', 'evidence_map.rubric_criterion' \
     FROM evidence_map",
    "INSERT INTO edges (from_node, to_node, relation, source) \
     SELECT 'run:' || run_id, 'call:' || call_id, 'has_call', 'calls.run_id' FROM calls",
    "INSERT INTO edges (from_node, to_node, relation, source) \
     SELECT 'run:' || run_id, 'receipt:' || receipt_id, 'has_receipt', 'receipts.run_id' FROM receipts",
    "INSERT INTO edges (from_node, to_node, relation, source) \
     SELECT 'call:' || call_id, 'model:' || model, 'uses_model', 'calls.model' \
     FROM calls WHERE model IS NOT NULL",
    "INSERT INTO edges (from_node, to_node, relation, source) \
     SELECT 'run:' || run_id, 'build:' || build_hash, 'built_with', 'runs.build_hash' \
     FROM runs WHERE build_hash IS NOT NULL AND build_hash <> 'unknown'",
    "INSERT INTO edges (from_node, to_node, relation, source) \
     SELECT 'chapter:' || chapter_id, 'claim:' || claim_id, 'supports', 'video_chapters.claim_id' \
     FROM video_chapters WHERE claim_id IS NOT NULL",
];

// Hand-computed with Python hashlib: leaves are sha256("receipt-a"),
// sha256("receipt-b"), sha256("receipt-c"); root over the first n is
// sha256("".join(sorted(leaves[:n])).encode()).
const LEAF_A: &str = "0d91905593cb20769a8d573e62d556b7b50b9988cf6ecd6e4fa6100793606be4";
const LEAF_B: &str = "d1016d6ce5a4f5edd6a1214060550d96f27c1427d0a9a4fb419e1fa678d00c18";
const LEAF_C: &str = "84fcd22faee92919308b7beb8c214cda2457dc04de4615178a95db862919aa72";
const ROOT_1: &str = "e674b11aa8d6e95205eb81e056b435ea4f629bdc5584108589eaee34dac410e1";
const ROOT_2: &str = "b2406fbdadf08f0b58e353958b763f20fb8806df9cead0dfcf8be0336faa72bd";
const ROOT_3: &str = "504087b93537f119205c70fe5d74f6a1e882f0a1bb3130861e15e3ce575cc8ff";

/// Receipts root of the real evidence.db (29 hashed receipts), as
/// import_inventory.py stored it and verify_db.py recomputes it.
const REAL_ROOT: &str = "54763f911420b4db2104178b5c2420fb46d8ca7ba1af3907e23e1ecc1917b77e";

fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn temp_root(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("agenthand-evidence-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("evidence-layer")).unwrap();
    std::fs::create_dir_all(dir.join("pkg")).unwrap();
    dir
}

struct Fixture {
    root: PathBuf,
    db: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Derive edges and store the receipts root + count in meta.
fn finish_db(con: &Connection) {
    for sql in EDGE_SQL {
        con.execute(sql, []).unwrap();
    }
    let mut digests: Vec<String> = con
        .prepare("SELECT file_sha256 FROM receipts WHERE file_sha256 IS NOT NULL")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    digests.sort();
    con.execute(
        "INSERT INTO meta (key, value) VALUES ('receipts_root', ?1)",
        params![sha_hex(digests.concat().as_bytes())],
    )
    .unwrap();
    con.execute(
        "INSERT INTO meta (key, value) VALUES ('receipt_count_rooted', ?1)",
        params![digests.len().to_string()],
    )
    .unwrap();
}

/// The standard fixture: two runs whose call costs sum to their totals,
/// receipt 1 and 2 on disk, receipt 3 hashed but absent, receipt 4 with
/// no hash; claim 1 verified (run 1 + receipt 1, chapter 1), claim 2
/// withdrawn and claim 3 refused (criterion only, with reasons).
fn standard(name: &str, withdrawn_reason: Option<&str>) -> Fixture {
    let root = temp_root(name);
    let db = root.join("evidence-layer").join("evidence.db");
    let con = Connection::open(&db).unwrap();
    con.execute_batch(SCHEMA).unwrap();
    let mut shas = Vec::new();
    for i in 1..=2 {
        let bytes = format!("{{\"receipt\": {i}, \"changed_px\": 0}}\n");
        std::fs::write(root.join("pkg").join(format!("r{i}.json")), &bytes).unwrap();
        shas.push(sha_hex(bytes.as_bytes()));
    }
    con.execute_batch(
        "INSERT INTO runs (run_id, label, run_date, build_hash, total_settled_usd) VALUES
             (1, 'Hero run fixture', '2026-11-02', 'abc1234', 0.019175),
             (2, 'Second run fixture', '2026-11-01', 'unknown', 0.01);
         INSERT INTO calls (run_id, tool, model, settled_usd) VALUES
             (1, 'identify', 'muse-spark-1.3', 0.006675),
             (1, 'cut_out', 'sam-3.1', 0.0025),
             (1, 'draw', 'muse-image-1.0', 0.01),
             (2, 'draw', 'muse-image-1.0', 0.01);",
    )
    .unwrap();
    con.execute(
        "INSERT INTO receipts (receipt_id, run_id, kind, checked_px, changed_px, file_sha256, \
         source_file, verify_cmd, expected_line) VALUES \
         (1, 1, 'fidelity_receipt', 1000, 0, ?1, 'pkg/r1.json', 'agenthand verify s.png f.png m.png', \
          'verify PASS : checked 1,000 px, changed 0 px, ok=True'), \
         (2, 2, 'fidelity_receipt', 2000, 0, ?2, 'pkg/r2.json', NULL, NULL), \
         (3, 2, 'fidelity_receipt', 3000, 0, ?3, 'pkg/absent.json', NULL, NULL), \
         (4, 2, 'recount', 4000, 0, NULL, NULL, NULL, NULL)",
        params![shas[0], shas[1], sha_hex(b"absent")],
    )
    .unwrap();
    con.execute(
        "INSERT INTO claims (claim_id, text, status, scope_photo, scope_build, scope_date, reason) VALUES \
         (1, 'Hero fixture: 1,000 checked, 0 changed.', 'verified', 'IMG_1', 'abc1234', '2026-11-02', NULL), \
         (2, 'Fence fixture measurement.', 'withdrawn', 'IMG_2', 'pre-hardening', '2026-11-01', ?1), \
         (3, 'Estimator refuses fixture.', 'refused', 'IMG_2', 'main', '2026-11-02', 'strict ticks vanish')",
        params![withdrawn_reason],
    )
    .unwrap();
    con.execute_batch(
        "INSERT INTO evidence_map (claim_id, rubric_criterion, run_id, receipt_id) VALUES
             (1, 'Demo', 1, 1),
             (2, 'Technical Depth & Execution', NULL, NULL),
             (3, 'Technical Depth & Execution', NULL, NULL);
         INSERT INTO video_chapters (chapter_id, t_start_s, t_end_s, narration_line, claim_id) VALUES
             (1, 0.0, 10.0, 'Ch0 opening: hero fixture', 1),
             (2, 10.0, 20.0, 'Ch1 no claim here', NULL);",
    )
    .unwrap();
    finish_db(&con);
    drop(con);
    Fixture { root, db }
}

/// A DB whose only rooted receipts are the given leaves (inserted in the
/// given order), plus one receipt with no hash.
fn leaves_db(name: &str, leaves: &[&str]) -> Fixture {
    let root = temp_root(name);
    let db = root.join("evidence-layer").join("evidence.db");
    let con = Connection::open(&db).unwrap();
    con.execute_batch(SCHEMA).unwrap();
    con.execute(
        "INSERT INTO runs (run_id, label) VALUES (1, 'leaf run')",
        [],
    )
    .unwrap();
    for leaf in leaves {
        con.execute(
            "INSERT INTO receipts (run_id, kind, file_sha256, source_file) \
             VALUES (1, 'fidelity_receipt', ?1, 'pkg/missing.json')",
            params![leaf],
        )
        .unwrap();
    }
    con.execute(
        "INSERT INTO receipts (run_id, kind, file_sha256) VALUES (1, 'recount', NULL)",
        [],
    )
    .unwrap();
    finish_db(&con);
    drop(con);
    Fixture { root, db }
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run_with_env(args: &[&str], env: &[(&str, &str)]) -> Out {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agenthand"));
    cmd.arg("evidence")
        .args(args)
        .env_remove("AGENTHAND_EVIDENCE_DB")
        .env_remove("AGENTHAND_EVIDENCE_BASE");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn agenthand");
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn run_db(db: &Path, args: &[&str]) -> Out {
    let db = db.to_str().unwrap();
    let mut full = vec!["--db", db];
    full.extend_from_slice(args);
    run_with_env(&full, &[])
}

fn file_sha(p: &Path) -> String {
    sha_hex(&std::fs::read(p).unwrap())
}

#[test]
fn verify_all_passes_on_a_consistent_db() {
    let f = standard("pass", Some("False accept: phantom card fit."));
    let o = run_db(&f.db, &["verify", "--all"]);
    assert_eq!(o.code, 0, "stdout:\n{}\nstderr:\n{}", o.stdout, o.stderr);
    for want in [
        "== (a) receipt file hashes ==",
        "PASS   receipt 1 (run 1): sha256 matches pkg/r1.json",
        "PASS   receipt 2 (run 2): sha256 matches pkg/r2.json",
        "SKIP   receipt 3 (run 2): file not present locally: pkg/absent.json",
        "  -> 2 matched, 1 not-on-disk, 1 receipts carry no hash",
        "PASS   run 1 (Hero run fixture): total $0.019175 == calls sum",
        "PASS   no orphan calls or receipts (every row joins to a run)",
        "PASS   every receipt (4) and every call (4) has its run edge in the graph",
        "PASS   claim 1: directed walk reaches 1 receipt(s)",
        "PASS   claim 2 (withdrawn): no receipt by design; directed walk terminates at its rubric criterion",
        "PASS   claim 3 (refused): scope fields complete; reason present",
        "OVERALL: PASS (0 failing line(s))",
    ] {
        assert!(o.stdout.contains(want), "missing {want:?} in:\n{}", o.stdout);
    }
    assert!(!o.stdout.contains("FAIL "), "{}", o.stdout);
    // verify-all is the same check run.
    let alias = run_db(&f.db, &["verify-all"]);
    assert_eq!(alias.code, 0);
    assert_eq!(alias.stdout, o.stdout);
}

#[test]
fn tampered_receipt_file_fails_with_exit_1() {
    let f = standard("tamper-file", Some("reason"));
    let path = f.root.join("pkg").join("r1.json");
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[3] ^= 0x01;
    std::fs::write(&path, bytes).unwrap();
    let o = run_db(&f.db, &["verify", "--all"]);
    assert_eq!(o.code, 1, "{}", o.stdout);
    assert!(
        o.stdout
            .contains("FAIL   receipt 1 (run 1): sha256 MISMATCH for pkg/r1.json (db "),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains("OVERALL: FAIL (1 failing line(s))"),
        "{}",
        o.stdout
    );
    // The single-claim check reaches the same receipt and fails too.
    let one = run_db(&f.db, &["verify", "1"]);
    assert_eq!(one.code, 1, "{}", one.stdout);
    assert!(one.stdout.contains("OVERALL: FAIL"), "{}", one.stdout);
}

#[test]
fn tampered_db_sha_fails_hash_and_root_checks() {
    let f = standard("tamper-db", Some("reason"));
    {
        let con = Connection::open(&f.db).unwrap();
        con.execute(
            "UPDATE receipts SET file_sha256 = ?1 WHERE receipt_id = 2",
            params!["0".repeat(64)],
        )
        .unwrap();
    }
    let o = run_db(&f.db, &["verify", "--all"]);
    assert_eq!(o.code, 1, "{}", o.stdout);
    assert!(
        o.stdout
            .contains("FAIL   receipt 2 (run 2): sha256 MISMATCH"),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains("FAIL   receipts_root mismatch: stored "),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains("OVERALL: FAIL (2 failing line(s))"),
        "{}",
        o.stdout
    );
    let r = run_db(&f.db, &["root"]);
    assert_eq!(r.code, 1, "{}", r.stdout);
    assert!(
        r.stdout.contains("meta.receipts_root: differs"),
        "{}",
        r.stdout
    );
}

#[test]
fn withdrawn_claim_passes_by_design_and_needs_its_reason() {
    let f = standard("withdrawn", Some("False accept: phantom card fit."));
    let o = run_db(&f.db, &["verify", "claim:2"]);
    assert_eq!(o.code, 0, "{}", o.stdout);
    assert!(o.stdout.contains(
        "PASS   claim 2 (withdrawn): no receipt by design; directed walk terminates at its rubric criterion"
    ));
    assert!(o
        .stdout
        .contains("PASS   claim 2 (withdrawn): scope fields complete; reason present"));
    assert!(
        !o.stdout.contains("claim 1"),
        "scoped run leaked:\n{}",
        o.stdout
    );
    let listed = run_db(&f.db, &["list", "--status", "withdrawn"]);
    assert_eq!(listed.code, 0);
    assert!(
        listed.stdout.contains("claim:2  [withdrawn]"),
        "{}",
        listed.stdout
    );
    assert!(listed.stdout.contains("1 claim(s)"), "{}", listed.stdout);

    let g = standard("withdrawn-noreason", None);
    let o = run_db(&g.db, &["verify", "--all"]);
    assert_eq!(o.code, 1, "{}", o.stdout);
    assert!(o
        .stdout
        .contains("FAIL   claim 2 (withdrawn): withdrawn/refused claim has no reason"));
    assert!(
        o.stdout.contains("OVERALL: FAIL (1 failing line(s))"),
        "{}",
        o.stdout
    );
}

#[test]
fn receipts_root_matches_hand_computed_values_for_one_two_three_leaves() {
    // Library function against the hand-computed constants.
    let leaves = [LEAF_A, LEAF_B, LEAF_C].map(String::from);
    assert_eq!(agenthand::evidence::receipts_root(&leaves[..1]), ROOT_1);
    assert_eq!(agenthand::evidence::receipts_root(&leaves[..2]), ROOT_2);
    assert_eq!(agenthand::evidence::receipts_root(&leaves[..3]), ROOT_3);
    // Insertion order does not matter: the digests are sorted first.
    let reversed = [LEAF_C, LEAF_B, LEAF_A].map(String::from);
    assert_eq!(agenthand::evidence::receipts_root(&reversed), ROOT_3);

    // CLI against databases holding 1, 2, 3 rooted receipts (inserted
    // unsorted) plus one receipt with no hash.
    for (n, leaves, want) in [
        (1, vec![LEAF_A], ROOT_1),
        (2, vec![LEAF_B, LEAF_A], ROOT_2),
        (3, vec![LEAF_B, LEAF_C, LEAF_A], ROOT_3),
    ] {
        let f = leaves_db(&format!("root{n}"), &leaves);
        let o = run_db(&f.db, &["root", "--json"]);
        assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
        let v: serde_json::Value = serde_json::from_str(&o.stdout).unwrap();
        assert_eq!(v["receipts_root"], want, "n = {n}");
        assert_eq!(v["rooted_receipts"], n);
        assert_eq!(v["receipts_total"], n + 1);
        assert_eq!(v["stored_receipts_root"], "matches");
        let text = run_db(&f.db, &["root"]);
        assert_eq!(
            text.stdout.lines().next(),
            Some(format!("receipts root: {want}").as_str())
        );
    }
}

#[test]
fn root_recomputes_instead_of_echoing_the_stored_value() {
    let f = leaves_db("root-stored", &[LEAF_A, LEAF_B]);
    {
        let con = Connection::open(&f.db).unwrap();
        con.execute(
            "UPDATE meta SET value = ?1 WHERE key = 'receipts_root'",
            params!["f".repeat(64)],
        )
        .unwrap();
    }
    let o = run_db(&f.db, &["root"]);
    assert_eq!(o.code, 1, "{}", o.stdout);
    assert_eq!(
        o.stdout.lines().next(),
        Some(format!("receipts root: {ROOT_2}").as_str())
    );
    assert!(
        o.stdout.contains("meta.receipts_root: differs"),
        "{}",
        o.stdout
    );
    let v = run_db(&f.db, &["verify", "--all"]);
    assert_eq!(v.code, 1);
    assert!(
        v.stdout.contains(&format!("computed {ROOT_2} / count 2")),
        "{}",
        v.stdout
    );
}

#[test]
fn database_is_never_written() {
    let f = standard("readonly", Some("reason"));
    let before = file_sha(&f.db);
    for args in [
        vec!["verify", "--all"],
        vec!["root"],
        vec!["trace", "chapter:1"],
        vec!["list"],
        vec!["show", "1"],
        vec!["graph", "--format", "dot"],
        vec!["rubric"],
        vec!["calls", "--by-model"],
        vec!["tamper", "1"],
    ] {
        let o = run_db(&f.db, &args);
        assert_eq!(o.code, 0, "{args:?}: {}{}", o.stdout, o.stderr);
    }
    assert_eq!(file_sha(&f.db), before);
    let leftovers: Vec<_> = std::fs::read_dir(f.db.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != "evidence.db")
        .collect();
    assert!(leftovers.is_empty(), "side files written: {leftovers:?}");
}

#[test]
fn trace_walks_chapter_to_claim_run_and_receipt() {
    let f = standard("trace", Some("reason"));
    let o = run_db(&f.db, &["trace", "chapter:1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    let sha1 = file_sha(&f.root.join("pkg").join("r1.json"));
    for want in [
        "  chapter:1  [0:00.000-0:10.000] Ch0 opening: hero fixture",
        "    supports -> claim:1  [verified] Hero fixture: 1,000 checked, 0 changed.",
        "      evidenced_by -> run:1  Hero run fixture  build=abc1234",
        "        has_call -> call:1  identify (muse-spark-1.3)  settled_usd=0.006675",
        "          uses_model -> model:muse-spark-1.3",
        "        has_receipt -> receipt:1  fidelity_receipt 1000 checked / 0 changed",
        &format!("            sha256: {sha1}"),
        "            verify_cmd: agenthand verify s.png f.png m.png",
        "      evidenced_by -> receipt:1  (shown above)",
        "      addresses -> criterion:Demo",
    ] {
        assert!(
            o.stdout.contains(want),
            "missing {want:?} in:\n{}",
            o.stdout
        );
    }
    // From the receipt, the upstream walk reaches the run, claim and chapter.
    let up = run_db(&f.db, &["trace", "receipt:1"]);
    assert_eq!(up.code, 0);
    assert!(
        up.stdout
            .contains("upstream (incoming edges into receipt:1):"),
        "{}",
        up.stdout
    );
    assert!(
        up.stdout.contains("<- has_receipt -- run:1"),
        "{}",
        up.stdout
    );
    assert!(
        up.stdout.contains("<- supports -- chapter:1"),
        "{}",
        up.stdout
    );
    // A timestamp resolves to the chapter covering it.
    let ch = run_db(&f.db, &["chapter", "0:05"]);
    assert_eq!(ch.code, 0, "{}", ch.stderr);
    assert!(ch.stdout.starts_with("chapter:1  "), "{}", ch.stdout);
    let none = run_db(&f.db, &["chapter", "Ch1"]);
    assert!(none.stdout.contains("claim: none"), "{}", none.stdout);
    let listed = run_db(&f.db, &["list", "--chapter", "0:05"]);
    assert!(
        listed.stdout.contains("claim:1  [verified]"),
        "{}",
        listed.stdout
    );
    let missing = run_db(&f.db, &["trace", "claim:99"]);
    assert_eq!(missing.code, 2);
}

#[test]
fn graph_exports_mermaid_and_dot() {
    let f = standard("graph", Some("reason"));
    let m = run_db(&f.db, &["graph", "--format", "mermaid"]);
    assert_eq!(m.code, 0);
    assert!(m.stdout.starts_with("flowchart LR\n"), "{}", m.stdout);
    assert!(m.stdout.contains("-->|has_receipt|"), "{}", m.stdout);
    let d = run_db(&f.db, &["graph", "--format", "dot"]);
    assert_eq!(d.code, 0);
    assert!(
        d.stdout.starts_with("digraph proof_graph {"),
        "{}",
        d.stdout
    );
    assert!(
        d.stdout
            .contains("\"chapter:1\" -> \"claim:1\" [label=\"supports\"];"),
        "{}",
        d.stdout
    );
    let j = run_db(&f.db, &["graph", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&j.stdout).unwrap();
    let con = Connection::open(&f.db).unwrap();
    let n_edges: i64 = con
        .query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v["edges"].as_array().unwrap().len() as i64, n_edges);
    let bad = run_db(&f.db, &["graph", "--format", "svg"]);
    assert_eq!(bad.code, 2);
}

#[test]
fn tamper_flips_a_byte_in_memory_and_the_check_fails() {
    let f = standard("tamper-verb", Some("reason"));
    let receipt = f.root.join("pkg").join("r1.json");
    let before = file_sha(&receipt);
    let o = run_db(&f.db, &["tamper", "receipt:1"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(
        o.stdout.contains(&format!("on-disk sha256:   {before}")),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains("check: FAIL for the tampered copy"),
        "{}",
        o.stdout
    );
    assert_eq!(
        file_sha(&receipt),
        before,
        "tamper must not modify the file"
    );
    let absent = run_db(&f.db, &["tamper", "3"]);
    assert_eq!(absent.code, 0);
    assert!(
        absent.stdout.contains("not checked: file absent"),
        "{}",
        absent.stdout
    );
    let nohash = run_db(&f.db, &["tamper", "4"]);
    assert!(nohash
        .stdout
        .contains("not checked: receipt carries no file_sha256"));
}

#[test]
fn planned_verbs_are_separated_and_exit_3() {
    let help = run_with_env(&["--help"], &[]);
    assert_eq!(help.code, 0);
    let implemented = help.stdout.find("Implemented:").unwrap();
    let planned = help.stdout.find("Planned, not implemented:").unwrap();
    assert!(implemented < planned);
    assert!(help.stdout[planned..].contains("parity"));
    for verb in ["parity", "export"] {
        let o = run_with_env(&[verb], &[]);
        assert_eq!(o.code, 3, "{verb}");
        assert!(
            o.stdout.contains("planned, not implemented"),
            "{}",
            o.stdout
        );
    }
}

#[test]
fn database_location_comes_from_env_or_fails_clearly() {
    let f = standard("location", Some("reason"));
    let db = f.db.to_str().unwrap();
    let o = run_with_env(&["root"], &[("AGENTHAND_EVIDENCE_DB", db)]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let missing = f.root.join("nope.db");
    let o = run_with_env(
        &["root"],
        &[("AGENTHAND_EVIDENCE_DB", missing.to_str().unwrap())],
    );
    assert_eq!(o.code, 2);
    assert!(
        o.stderr.contains("evidence database not found"),
        "{}",
        o.stderr
    );
    assert!(o.stderr.contains("AGENTHAND_EVIDENCE_DB"), "{}", o.stderr);
    // --base-dir moves where relative receipt paths resolve.
    let other = f.root.join("elsewhere");
    std::fs::create_dir_all(&other).unwrap();
    let v = run_db(
        &f.db,
        &["--base-dir", other.to_str().unwrap(), "verify", "--all"],
    );
    assert_eq!(v.code, 0, "{}", v.stdout);
    assert!(v
        .stdout
        .contains("  -> 0 matched, 3 not-on-disk, 1 receipts carry no hash"));
    let unknown = run_db(&f.db, &["show", "1", "--status", "verified"]);
    assert_eq!(unknown.code, 2);
}

/// The real evidence database. Run on a machine that has it:
/// AGENTHAND_EVIDENCE_DB=/path/to/evidence.db cargo test --release \
///   --test evidence_cli -- --ignored
#[test]
#[ignore]
fn real_evidence_db_reaches_overall_pass_and_its_root() {
    let db = std::env::var("AGENTHAND_EVIDENCE_DB")
        .expect("set AGENTHAND_EVIDENCE_DB to the real evidence.db");
    let o = run_with_env(&["verify", "--all"], &[("AGENTHAND_EVIDENCE_DB", &db)]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(
        o.stdout.contains("OVERALL: PASS (0 failing line(s))"),
        "{}",
        o.stdout
    );
    let r = run_with_env(&["root", "--json"], &[("AGENTHAND_EVIDENCE_DB", &db)]);
    assert_eq!(r.code, 0, "{}{}", r.stdout, r.stderr);
    let v: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(v["receipts_root"], REAL_ROOT);
    assert_eq!(v["rooted_receipts"], 29);
    assert_eq!(v["stored_receipts_root"], "matches");
}
