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
        Server::spawn_with_comps(data_dir, "")
    }

    fn spawn_with_comps(data_dir: &std::path::Path, comps_file: &str) -> Server {
        let mut child = Command::new(env!("CARGO_BIN_EXE_agenthand"))
            .env("AGENTHAND_DATA_DIR", data_dir)
            // Empty means the default (<data dir>/pricing-comps.json).
            .env("AGENTHAND_COMPS_FILE", comps_file)
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
    let data_dir = std::env::temp_dir().join(format!("agenthand-e2e-{}", std::process::id()));
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
        .put_image(
            &fg,
            KIND_FOREGROUND,
            Some(&AssetStore::parse_handle(&source).unwrap()),
        )
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
    // Exactly the three tools: pricing rides on identify, no price tool.
    assert_eq!(names, vec!["identify", "cut_out", "draw"]);
    // identify's MCP definition is untouched by the pricing work.
    let identify_def = &tools["result"]["tools"][0];
    assert_eq!(
        identify_def["inputSchema"],
        json!({
            "type": "object",
            "properties": {
                "image": {"type": "string", "description": "The product photo — an asset handle (asset://<id>), an http(s) URL, or a base64 data URI."}
            },
            "required": ["image"]
        })
    );

    // identify without a key: structured NOT_CONFIGURED, no call made.
    let resp = server.call(
        "tools/call",
        json!({"name": "identify", "arguments": {"image": fg_handle}}),
    );
    let result = tool_result(&resp);
    assert_eq!(result["status"], "error");
    assert_eq!(result["error"]["code"], "NOT_CONFIGURED");
    // An error result is not priced (and keeps its shape).
    assert!(result.get("pricing").is_none());

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
    assert_eq!(
        result["fidelity_receipt"]["plate_source"],
        "local_placeholder"
    );
    assert_eq!(result["cost_usd"], 0.0);
    // AB 723 verify sidecar: the result carries the QR asset, the
    // verify URL, and the exact ready-to-paste label line; the QR
    // decodes back to the URL, and the URL's fragment is the hash
    // of the persisted receipt with the annotation fields stripped.
    let verify_url = result["verify_url"].as_str().expect("verify_url present");
    assert!(
        verify_url.starts_with("https://ausernameforyou.github.io/agenthand/verify#"),
        "verify_url: {verify_url}"
    );
    let fragment = verify_url.rsplit('#').next().unwrap();
    assert_eq!(fragment.len(), 64);
    assert_eq!(
        result["label"],
        format!("Digitally altered. Original photo and proof: {verify_url}")
    );
    let qr_handle = result["qr_asset"].as_str().expect("qr_asset present");
    let qr_png = store.read_bytes(qr_handle).unwrap();
    let luma = image::load_from_memory(&qr_png).unwrap().to_luma8();
    let mut prepared = rqrr::PreparedImage::prepare(luma);
    let grids = prepared.detect_grids();
    assert_eq!(grids.len(), 1);
    let (_meta, content) = grids[0].decode().unwrap();
    assert_eq!(content, verify_url);
    let out_handle = result["output_asset"].as_str().unwrap();
    let persisted = store
        .load_receipt(out_handle)
        .unwrap()
        .expect("receipt persisted");
    assert_eq!(persisted["verify_url"], verify_url);
    assert_eq!(
        persisted["qr_sha256"],
        agenthand::image::sha256_hex(&qr_png)
    );
    let mut stripped = persisted.clone();
    stripped.as_object_mut().unwrap().remove("verify_url");
    stripped.as_object_mut().unwrap().remove("qr_sha256");
    assert_eq!(
        agenthand::image::sha256_hex(
            agenthand::server::canonical_receipt_json(&stripped).as_bytes()
        ),
        fragment,
        "stripping the annotations must reproduce the URL hash"
    );
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

#[test]
fn stdio_price_tool_is_removed() {
    // Owner's ruling: no price tool. With the real comps file
    // configured, tools/list is still exactly identify, cut_out, draw,
    // and a call to "price" is an unknown tool.
    let data_dir =
        std::env::temp_dir().join(format!("agenthand-e2e-noprice-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    std::fs::create_dir_all(&data_dir).unwrap();
    let comps = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pricing-comps.json");
    let mut server = Server::spawn_with_comps(&data_dir, comps.to_str().unwrap());
    server.call(
        "initialize",
        json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
    );
    let tools = server.call("tools/list", json!({}));
    let defs = tools["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = defs.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["identify", "cut_out", "draw"]);
    assert!(
        defs.iter().all(|t| !t.to_string().contains("item_name")),
        "no price tool schema left in tools/list"
    );

    let resp = server.call(
        "tools/call",
        json!({"name": "price", "arguments": {"item_name": "Haro Shredder kids BMX bike"}}),
    );
    assert!(resp.get("result").is_none(), "{resp}");
    assert_eq!(resp["error"]["message"], "Unknown tool: price");

    // The server keeps serving after the unknown call.
    let resp = server.call("ping", json!({}));
    assert_eq!(resp["result"], json!({}));

    let _ = std::fs::remove_dir_all(&data_dir);
}
