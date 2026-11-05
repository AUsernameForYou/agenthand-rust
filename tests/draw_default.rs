//! Decision 135: draw's default plate is `generated` (the upgraded
//! listing scene). `source_edit` stays one argument away, or one env
//! var away (`AGENTHAND_DRAW_PLATE=source_edit`).
//!
//! Offline: the draw-level checks stop at the spend cap, before any
//! request is built into a call, and read which plate call billing
//! refused ("plate generation" or "plate edit").

use std::path::PathBuf;
use std::sync::Mutex;

use agenthand::assets::{AssetStore, KIND_FOREGROUND, KIND_SOURCE};
use agenthand::backdrop;
use agenthand::image::RgbaImage;

static ENV_LOCK: Mutex<()> = Mutex::new(());

const ENV_VARS: [&str; 7] = [
    "AGENTHAND_DATA_DIR",
    "AGENTHAND_LEDGER",
    "AGENTHAND_LEDGER_PATH",
    "AGENTHAND_TOTAL_SPEND_CAP_USD",
    "META_API_KEY",
    "AGENTHAND_DRAW_PLATE",
    "AGENTHAND_DRAW_PLACEHOLDER_PLATE",
];

fn with_clean_env(body: impl FnOnce()) {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for var in ENV_VARS {
        std::env::remove_var(var);
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
    for var in ENV_VARS {
        std::env::remove_var(var);
    }
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

fn tmpdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agenthand-draw-default-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn generated_is_the_default_and_the_env_restores_source_edit() {
    with_clean_env(|| {
        assert_eq!(backdrop::DRAW_PLATE_DEFAULT, "generated");
        assert_eq!(backdrop::DRAW_PLATE_MODES[0], backdrop::DRAW_PLATE_DEFAULT);
        assert_eq!(backdrop::resolve_draw_plate(None).unwrap(), "generated");
        assert_eq!(
            backdrop::resolve_draw_plate(Some("  ")).unwrap(),
            "generated"
        );

        std::env::set_var("AGENTHAND_DRAW_PLATE", "source_edit");
        assert_eq!(backdrop::resolve_draw_plate(None).unwrap(), "source_edit");
        // The call's own argument still wins over the env.
        assert_eq!(
            backdrop::resolve_draw_plate(Some("generated")).unwrap(),
            "generated"
        );

        std::env::set_var("AGENTHAND_DRAW_PLATE", "generated");
        assert_eq!(backdrop::resolve_draw_plate(None).unwrap(), "generated");
        assert_eq!(
            backdrop::resolve_draw_plate(Some("source_edit")).unwrap(),
            "source_edit"
        );

        std::env::set_var("AGENTHAND_DRAW_PLATE", "bogus");
        assert!(backdrop::resolve_draw_plate(None).is_err());
    });
}

/// A draw without `plate` goes the generated route; with
/// `AGENTHAND_DRAW_PLATE=source_edit` it goes the edit route. Both are
/// refused by the spend cap before any call, so this runs offline.
#[test]
fn a_draw_without_plate_takes_the_generated_route() {
    for (env, step) in [
        (None, "plate generation"),
        (Some("source_edit"), "plate edit"),
        (Some("generated"), "plate generation"),
    ] {
        with_clean_env(|| {
            let dir = tmpdir(&format!("route-{}", env.unwrap_or("unset")));
            let data_dir = dir.join("data");
            std::env::set_var("AGENTHAND_DATA_DIR", &data_dir);
            std::env::set_var("META_API_KEY", "test-key-never-sent");
            std::env::set_var("AGENTHAND_TOTAL_SPEND_CAP_USD", "0.001");
            if let Some(mode) = env {
                std::env::set_var("AGENTHAND_DRAW_PLATE", mode);
            }
            let store = AssetStore::new(&data_dir);
            let photo = RgbaImage::solid(64, 48, [90, 90, 90, 255]);
            let photo_handle = store.put_image(&photo, KIND_SOURCE, None).unwrap();
            let mut fg = photo.clone();
            for (i, px) in fg.pixels.chunks_exact_mut(4).enumerate() {
                let (x, y) = (i % 64, i / 64);
                if !(16..48).contains(&x) || !(8..40).contains(&y) {
                    px[3] = 0;
                }
            }
            let parent = AssetStore::parse_handle(&photo_handle).unwrap();
            let fg_handle = store
                .put_image(&fg, KIND_FOREGROUND, Some(&parent))
                .unwrap();

            let (result, _) = agenthand::server::tool_draw(&fg_handle, "a quiet room").unwrap();
            assert_eq!(result["status"], "error", "{env:?}: {result}");
            assert_eq!(
                result["error"]["code"], "SPEND_CAP_REFUSED",
                "{env:?}: {result}"
            );
            assert_eq!(result["error"]["call_made"], serde_json::json!(false));
            let message = result["error"]["message"].as_str().unwrap();
            assert!(
                message.starts_with(&format!("Billing refused the {step} before any call")),
                "{env:?}: {message}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        });
    }
}
