//! Interface contract tests (EXPERIMENT — Grok lane prescription).
//!
//! Loads the single written contract (spec/interface.schema.json)
//! and the shared differential fixture
//! (tests/fixtures/interface/differential.json, byte-identical in
//! the Python tree) and holds THIS build to both:
//!
//! - round6 half-even, through the real `round6`;
//! - the scene-guard refusal wording, through the real guard;
//! - the live tool surface (tools/list over stdio from the real
//!   server binary) against the declared argument schemas, with the
//!   schema's own recorded differences as the only allowed deltas;
//! - the chamfer cap constant the fixture pins.
//!
//! Chamfer maps and receipt validation live in cfg(test) modules in
//! src/segmentation.rs and src/composite.rs (the functions under
//! test are private). The fixture's expected values were computed
//! independently of both builds.

use serde_json::{Map, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

fn fixture() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/interface/differential.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn schema() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/spec/interface.schema.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn round6_matches_contract_half_even() {
    let fx = fixture();
    for case in fx["round6"]["cases"].as_array().unwrap() {
        let input = case["input_f64"].as_f64().unwrap();
        let expected = case["expected"].as_f64().unwrap();
        assert_eq!(
            agenthand::round6(input),
            expected,
            "round6({}) [{}]",
            case["input_written"].as_str().unwrap(),
            case["note"].as_str().unwrap_or("")
        );
    }
}

#[test]
fn chamfer_cap_matches_contract() {
    let fx = fixture();
    assert_eq!(
        agenthand::segmentation::EDGE_DIST_CAP as u64,
        fx["chamfer"]["cap"].as_u64().unwrap()
    );
}

#[test]
fn scene_guard_refusal_wording_matches_contract() {
    let fx = fixture();
    let guard = agenthand::sceneguard::check_scene(
        Some("blue BMX bike"),
        "a bright showroom with a bike on display",
    );
    let probe = &fx["error_strings"]["scene_guard_probe"];
    let expected = fx["error_strings"]["scene_guard_refusal_template"]
        .as_str()
        .unwrap()
        .replace("{term}", probe["term"].as_str().unwrap())
        .replace("{concept}", probe["concept"].as_str().unwrap());
    assert_eq!(guard.refusal_message(), expected);
    let schema = schema();
    assert_eq!(
        guard.record()["rule"],
        schema["$defs"]["scene_guard_record"]["properties"]["rule"]["const"]
    );
    let skipped = agenthand::sceneguard::check_scene(None, "an empty studio");
    assert_eq!(
        skipped.record()["reason"],
        Value::from("no concept recorded in the foreground's mask; nothing to check the scene text against")
    );
}

// ------------------------------------------------------------ tools/list

struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Server {
    fn spawn() -> Server {
        let mut child = Command::new(env!("CARGO_BIN_EXE_agenthand"))
            .arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env_remove("META_API_KEY")
            .spawn()
            .unwrap();
        Server {
            stdin: child.stdin.take().unwrap(),
            stdout: BufReader::new(child.stdout.take().unwrap()),
            child,
            next_id: 1,
        }
    }
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let msg = obj(vec![
            ("jsonrpc", Value::from("2.0")),
            ("id", Value::from(id)),
            ("method", Value::from(method)),
            ("params", params),
        ]);
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
        loop {
            let mut line = String::new();
            self.stdout.read_line(&mut line).unwrap();
            let value: Value = serde_json::from_str(&line).unwrap();
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                return value;
            }
        }
    }
    fn notify(&mut self, method: &str) {
        let msg = obj(vec![("jsonrpc", Value::from("2.0")), ("method", Value::from(method))]);
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// Strip wire boilerplate that is not part of the contract: prose
/// descriptions and SDK-added keys.
fn obj(pairs: Vec<(&str, Value)>) -> Value {
    let mut map = Map::new();
    for (k, v) in pairs {
        map.insert(k.to_string(), v);
    }
    Value::Object(map)
}

fn normalize(raw: &Value) -> Value {
    let mut props = Map::new();
    for (name, sub) in raw["properties"].as_object().unwrap() {
        let mut keep = Map::new();
        for (k, v) in sub.as_object().unwrap() {
            if k != "title" && k != "description" {
                keep.insert(k.clone(), v.clone());
            }
        }
        props.insert(name.clone(), Value::Object(keep));
    }
    let mut required: Vec<Value> = raw["required"].as_array().cloned().unwrap_or_default();
    required.sort_by_key(|v| v.as_str().unwrap_or("").to_string());
    obj(vec![("properties", Value::Object(props)), ("required", Value::Array(required))])
}

#[test]
fn tools_list_matches_contract_with_only_recorded_deltas() {
    let schema = schema();
    let mut server = Server::spawn();
    server.request(
        "initialize",
        obj(vec![
            ("protocolVersion", Value::from("2025-06-18")),
            ("capabilities", obj(vec![])),
            (
                "clientInfo",
                obj(vec![("name", Value::from("contract-test")), ("version", Value::from("0"))]),
            ),
        ]),
    );
    server.notify("notifications/initialized");
    let listed = server.request("tools/list", obj(vec![]));
    let tools = listed["result"]["tools"].as_array().unwrap();
    let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["cut_out", "draw", "identify"]);
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        let mut observed = normalize(&tool["inputSchema"]);
        if name == "draw" {
            // The contract's canonical draw surface is the common
            // core; `plate` is this build's recorded addition
            // (x-differences in the schema), pinned explicitly below.
            observed["properties"].as_object_mut().unwrap().remove("plate");
        }
        let canonical = normalize(&schema["tools"][name]["inputSchema"]);
        assert_eq!(
            observed, canonical,
            "{name}: live inputSchema differs from the contract beyond its recorded deltas"
        );
    }
    // The recorded Rust-only divergence, pinned live: draw takes plate.
    let draw = tools.iter().find(|t| t["name"] == "draw").unwrap();
    assert_eq!(
        draw["inputSchema"]["properties"]["plate"]["enum"],
        Value::from(vec!["source_edit", "generated"])
    );
}
