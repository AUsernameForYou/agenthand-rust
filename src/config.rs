//! Runtime configuration, ported from the Python `config.py`: read
//! from the environment at call time; a local `.env` (current dir,
//! then the crate dir) fills in variables the environment does not
//! set. The key value itself is never logged.

use std::path::PathBuf;
use std::sync::Once;

pub const HARD_CEILING_USD: f64 = 150.00;
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
    std::env::var(name)
        .unwrap_or_default()
        .trim()
        .to_string()
}

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

pub fn get_ledger_path() -> PathBuf {
    // Mirror of the Python config: AGENTHAND_LEDGER_PATH, when set and
    // non-empty, wins outright (the room runner points it at the room's
    // own ledger for the duration of a run); otherwise the ledger lives
    // in the data dir.
    let raw = env_trimmed("AGENTHAND_LEDGER_PATH");
    if !raw.is_empty() {
        return PathBuf::from(shellexpand_home(&raw));
    }
    get_data_dir().join("ledger.jsonl")
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

pub fn get_meta_api_key() -> Option<String> {
    let raw = env_trimmed("META_API_KEY");
    if raw.is_empty() {
        None
    } else {
        Some(raw)
    }
}

pub fn use_placeholder_plate() -> bool {
    matches!(
        env_trimmed("AGENTHAND_DRAW_PLACEHOLDER_PLATE")
            .to_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}
