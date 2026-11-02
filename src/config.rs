//! Runtime configuration, ported from the Python `config.py`: read
//! from the environment at call time; a local `.env` (current dir,
//! then the crate dir) fills in variables the environment does not
//! set. The key value itself is never logged.

use std::path::PathBuf;
use std::sync::Once;

/// Absolute spend ceiling; no configuration may raise the cap above this.
pub const HARD_CEILING_USD: f64 = 150.00;
/// Default working spend cap when no cap is configured.
pub const DEFAULT_TOTAL_SPEND_CAP_USD: f64 = 120.00;

static DOTENV_ONCE: Once = Once::new();

fn load_dotenv() {
    DOTENV_ONCE.call_once(|| {
        let mut candidates = vec![PathBuf::from(".env")];
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent().and_then(|p| p.parent()) {
                candidates.push(dir.join(".env"));
            }
        }
        candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".env"));
        for path in candidates {
            if let Ok(text) = std::fs::read_to_string(&path) {
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    if let Some((name, value)) = line.split_once('=') {
                        let name = name.trim();
                        let value = value.trim().trim_matches('"').trim_matches('\'');
                        if !name.is_empty() && std::env::var_os(name).is_none() {
                            std::env::set_var(name, value);
                        }
                    }
                }
            }
        }
    });
}

fn env_trimmed(name: &str) -> String {
    load_dotenv();
    std::env::var(name).unwrap_or_default().trim().to_string()
}

/// The data dir holding assets and the ledger.
pub fn get_data_dir() -> PathBuf {
    let raw = env_trimmed("AGENTHAND_DATA_DIR");
    if !raw.is_empty() {
        return PathBuf::from(shellexpand_home(&raw));
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data")
}

fn shellexpand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return format!("{}/{}", home.to_string_lossy(), rest);
        }
    }
    path.to_string()
}

/// The JSONL ledger this context reads its own receipts from:
/// `AGENTHAND_LEDGER_PATH` when set and non-empty (the room runner
/// points it at the room's own ledger for the duration of a run, and
/// `billing::billed_call` mirrors every row into it — see
/// [`get_mirror_ledger_path`]), otherwise the data dir's
/// `ledger.jsonl`. This is NOT where the spend cap is checked: the
/// cap always runs against the cumulative store
/// ([`get_ledger_store`]).
pub fn get_ledger_path() -> PathBuf {
    get_mirror_ledger_path().unwrap_or_else(|| get_data_dir().join("ledger.jsonl"))
}

/// The mirror ledger (fix pass F4): `AGENTHAND_LEDGER_PATH`, when set
/// and non-empty, names a JSONL file that `billing::billed_call`
/// copies every pending and settled row into, IN ADDITION to the
/// cumulative store. The room runner points it at the room's own
/// ledger, so a run's cost stays provable from the room directory
/// alone (room totals read back from that file), while every
/// reservation is checked against the one cumulative ledger, so the
/// cap cannot reset per run. Before F4 this variable REPLACED the
/// JSONL store's path, which let each room run start at $0.
pub fn get_mirror_ledger_path() -> Option<PathBuf> {
    let raw = env_trimmed("AGENTHAND_LEDGER_PATH");
    if raw.is_empty() {
        None
    } else {
        Some(PathBuf::from(shellexpand_home(&raw)))
    }
}

/// Which ledger store `billing::billed_call` writes to, selected by
/// `AGENTHAND_LEDGER`:
/// - unset / `sqlite` — the SQLite ledger, `ledger.sqlite3` in the
///   data dir (the shipped default);
/// - `sqlite:<path>` — the SQLite ledger at that path;
/// - `jsonl` / `jsonl:<path>` — the JSONL ledger, still selectable,
///   and importable into SQLite via `sqlledger::import_jsonl`.
///
/// An unrecognized value is an error: a store we cannot identify
/// must stop spend, not silently pick one.
///
/// The selected store is the CUMULATIVE ledger: every billed call,
/// room runs included, reserves and settles there, so the spend cap
/// is checked against all spend, never against one run's slice.
///
/// Relation to `AGENTHAND_LEDGER_PATH` (fix pass F4): that variable
/// no longer relocates any store. It names a mirror JSONL file that
/// receives a copy of every row (see `get_mirror_ledger_path`); the
/// room runner pins it to `<room>/ledger.jsonl` so room totals stay
/// readable from the room directory. A plain `jsonl` selection means
/// the data dir's `ledger.jsonl`; use `jsonl:<path>` to move the
/// cumulative JSONL ledger elsewhere. (Before F4 the room runner also
/// pinned `AGENTHAND_LEDGER=jsonl` with the path override, so every
/// run spent against its own fresh ledger and the cap reset per run.)
pub enum LedgerStore {
    Jsonl(PathBuf),
    Sqlite(PathBuf),
}

/// Resolve `AGENTHAND_LEDGER` to the ledger store to use; unknown
/// values fail closed rather than spend against an unknown store.
pub fn get_ledger_store() -> Result<LedgerStore, String> {
    let raw = env_trimmed("AGENTHAND_LEDGER");
    if raw.is_empty() || raw == "sqlite" {
        return Ok(LedgerStore::Sqlite(get_data_dir().join("ledger.sqlite3")));
    }
    if raw == "jsonl" {
        return Ok(LedgerStore::Jsonl(get_data_dir().join("ledger.jsonl")));
    }
    if let Some(path) = raw.strip_prefix("jsonl:") {
        if path.trim().is_empty() {
            return Err(
                "AGENTHAND_LEDGER=jsonl: has an empty path; refusing to spend with an unknown ledger store."
                    .to_string(),
            );
        }
        return Ok(LedgerStore::Jsonl(PathBuf::from(shellexpand_home(path))));
    }
    if let Some(path) = raw.strip_prefix("sqlite:") {
        if path.trim().is_empty() {
            return Err(
                "AGENTHAND_LEDGER=sqlite: has an empty path; refusing to spend with an unknown ledger store."
                    .to_string(),
            );
        }
        return Ok(LedgerStore::Sqlite(PathBuf::from(shellexpand_home(path))));
    }
    Err(format!(
        "AGENTHAND_LEDGER={raw:?} is not understood (expected jsonl, jsonl:<path>, sqlite, or sqlite:<path>); refusing to spend with an unknown ledger store."
    ))
}

/// The working spend cap. The env var may lower the cap; values above
/// the hard ceiling clamp to it; unparseable/non-finite/negative
/// values are an error (a cap we cannot interpret stops spend).
pub fn get_total_spend_cap_usd() -> Result<f64, String> {
    let raw = env_trimmed("AGENTHAND_TOTAL_SPEND_CAP_USD");
    if raw.is_empty() {
        return Ok(DEFAULT_TOTAL_SPEND_CAP_USD);
    }
    let value: f64 = raw.parse().map_err(|_| {
        format!("AGENTHAND_TOTAL_SPEND_CAP_USD={raw:?} is not a number; refusing to run with an uninterpretable spend cap.")
    })?;
    if !value.is_finite() {
        return Err(format!(
            "AGENTHAND_TOTAL_SPEND_CAP_USD={raw:?} is not finite; refusing to run with an unbounded spend cap."
        ));
    }
    if value < 0.0 {
        return Err(format!(
            "AGENTHAND_TOTAL_SPEND_CAP_USD={raw:?} is negative; a spend cap cannot be negative."
        ));
    }
    Ok(value.min(HARD_CEILING_USD))
}

/// The pricing comps file: `AGENTHAND_COMPS_FILE` when set and
/// non-empty (a leading `~/` expands), else `pricing-comps.json` in
/// the data dir. Read at call time, so a new file needs no restart.
pub fn get_comps_file() -> PathBuf {
    comps_file_from(&env_trimmed("AGENTHAND_COMPS_FILE"), &get_data_dir())
}

/// Resolution rule behind `get_comps_file`, as a pure function.
pub fn comps_file_from(raw: &str, data_dir: &std::path::Path) -> PathBuf {
    let raw = raw.trim();
    if !raw.is_empty() {
        return PathBuf::from(shellexpand_home(raw));
    }
    data_dir.join(crate::comps::DEFAULT_COMPS_FILE_NAME)
}

/// The operator's Meta API key, from the environment or a local `.env`.
pub fn get_meta_api_key() -> Option<String> {
    let raw = env_trimmed("META_API_KEY");
    if raw.is_empty() {
        None
    } else {
        Some(raw)
    }
}

/// True when `AGENTHAND_DRAW_PLACEHOLDER_PLATE` opts into the offline
/// placeholder plate instead of a generated one.
pub fn use_placeholder_plate() -> bool {
    matches!(
        env_trimmed("AGENTHAND_DRAW_PLACEHOLDER_PLATE")
            .to_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// `AGENTHAND_DRAW_PLATE`: how draw builds its plate when the call
/// does not say (`source_edit`, the default: the original photo with
/// the product removed, edited in place; `generated`: the legacy
/// text-only scene). Empty means the default; validation happens in
/// `backdrop::resolve_draw_plate`.
pub fn get_draw_plate_mode() -> String {
    env_trimmed("AGENTHAND_DRAW_PLATE")
}
