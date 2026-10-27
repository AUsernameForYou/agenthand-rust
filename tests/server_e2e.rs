//! End-to-end stdio test: spawn the server binary, speak JSON-RPC,
//! and run the offline paths — tools/list shape, NOT_CONFIGURED
//! errors without an API key, and a full placeholder-mode draw whose
//! receipt must report zero changed opaque pixels.

use agenthand::assets::{AssetStore, KIND_FOREGROUND, KIND_SOURCE};
use agenthand::image::RgbaImage;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

struct Server {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: i64,
}

impl Server {
    fn spawn(data_dir: &std::path::Path) -> Server {
        let mut child = Command::new(env!("CARGO_BIN_EXE_agenthand"))
            .env("AGENTHAND_DATA_DIR", data_dir)
            .env("AGENTHAND_DRAW_PLACEHOLDER_PLATE", "1")
            // Empty (not removed): config's .env loader never
            // overrides a variable that is already present, so an
            // empty env value deterministically means "no key",
            // even on a machine where the crate dir has a .env.
            .env("META_API_KEY", "")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn server");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Server {
            child,
            stdin,
            stdout,
            next_id: 0,
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{request}").unwrap();
        self.stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], id);
        response
    }

    fn notify(&mut self, method: &str) {
        let note = json!({"jsonrpc": "2.0", "method": method});
        writeln!(self.stdin, "{note}").unwrap();
        self.stdin.flush().unwrap();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn tool_result(response: &Value) -> Value {
    response["result"]["structuredContent"].clone()
}

#[test]
fn stdio_server_end_to_end_offline() {
    let data_dir = std::env::temp_dir().join(format!(
        "agenthand-e2e-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&data_dir);
    // Pre-seed a source + foreground in the server's data dir.
    let store = AssetStore::new(&data_dir);
    let source = store
        .put_image(&RgbaImage::solid(6, 4, [0, 0, 0, 255]), KIND_SOURCE, None)
        .unwrap();
    let mut fg = RgbaImage::solid(6, 4, [0, 0, 0, 0]);
    for y in 0..4u32 {
        for x in 0..3u32 {
            let i = ((y * 6 + x) * 4) as usize;
            fg.pixels[i..i + 4].copy_from_slice(&[255, 0, 0, 255]);
        }
    }
    let fg_handle = store
        .put_image(&fg, KIND_FOREGROUND, Some(&AssetStore::parse_handle(&source).unwrap()))
        .unwrap();

    let mut server = Server::spawn(&data_dir);

    let init = server.call(
        "initialize",
        json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "agenthand");
    server.notify("notifications/initialized");

    let tools = server.call("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["identify", "cut_out", "draw"]);

    // identify without a key: structured NOT_CONFIGURED, no call made.
    let resp = server.call(
        "tools/call",
        json!({"name": "identify", "arguments": {"image": fg_handle}}),
    );
    let result = tool_result(&resp);
    assert_eq!(result["status"], "error");
    assert_eq!(result["error"]["code"], "NOT_CONFIGURED");

    // cut_out without a key and without a concept: the internal plan
    // fails the same way, surfaced as NO_CONCEPT.
    let resp = server.call(
        "tools/call",
        json!({"name": "cut_out", "arguments": {"image": fg_handle}}),
    );
    let result = tool_result(&resp);
    assert_eq!(result["status"], "error");
    // Python semantics: the plan error's own code becomes the
    // cut_out error code, and the plan error rides at the top level.
    assert_eq!(result["error"]["code"], "NOT_CONFIGURED");
    assert_eq!(result["plan_error"]["code"], "NOT_CONFIGURED");

    // draw in placeholder mode: full fidelity path, offline.
    let resp = server.call(
        "tools/call",
        json!({"name": "draw", "arguments": {"asset": fg_handle, "scene_prompt": "a quiet studio"}}),
    );
    let result = tool_result(&resp);
    assert_eq!(result["status"], "ok", "draw result: {result}");
    assert_eq!(result["fidelity_receipt"]["changed_opaque_pixels"], 0);
    assert_eq!(result["fidelity_receipt"]["opaque_pixels"], 12);
    assert_eq!(result["fidelity_receipt"]["plate_source"], "local_placeholder");
    assert_eq!(result["cost_usd"], 0.0);
    // The preview image block rides along.
    let content = resp["result"]["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["mimeType"], "image/png");

    // resources/read returns the stored bytes.
    let uri = result["output_asset"].as_str().unwrap().to_string();
    let resp = server.call("resources/read", json!({"uri": uri}));
    assert!(resp["result"]["contents"][0]["blob"].is_string());

    let _ = std::fs::remove_dir_all(&data_dir);
}
