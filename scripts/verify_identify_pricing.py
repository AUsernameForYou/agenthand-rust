#!/usr/bin/env python3
"""Check that identify still returns full (non-empty) bodies after the
pricing change, and print the pricing block it now carries.

Spawns the agenthand binary over MCP stdio, calls identify on one photo
--runs times, and fails if any call errors, retries (attempts > 1 means
the API sent a body that would not parse, the empty-200 symptom), or
returns an empty description / concept / title. Then calls the
code-only price tool on the returned concept.

Each run is one real identify call billed to META_API_KEY (about one
cent). Standard library only.

    python3 scripts/verify_identify_pricing.py \
        --bin ~/agenthand-rust/target/release/agenthand \
        --photo ~/agenthand-runs/IMG_1443.HEIC \
        --comps ~/agenthand-runs/pricing-comps.json --runs 3
"""
import argparse
import base64
import json
import os
import subprocess
import sys


class Server:
    def __init__(self, binary, env):
        self.proc = subprocess.Popen(
            [binary], stdin=subprocess.PIPE, stdout=subprocess.PIPE, env=env
        )
        self.next_id = 0

    def call(self, method, params):
        self.next_id += 1
        req = {"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}
        self.proc.stdin.write((json.dumps(req) + "\n").encode())
        self.proc.stdin.flush()
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise SystemExit("server closed stdout (crashed?)")
            msg = json.loads(line)
            if msg.get("id") == self.next_id:
                return msg

    def notify(self, method):
        note = {"jsonrpc": "2.0", "method": method}
        self.proc.stdin.write((json.dumps(note) + "\n").encode())
        self.proc.stdin.flush()

    def close(self):
        self.proc.stdin.close()
        self.proc.wait(timeout=30)


def data_uri(path):
    ext = os.path.splitext(path)[1].lstrip(".").lower() or "png"
    sub = {"jpg": "jpeg", "heif": "heic"}.get(ext, ext)
    with open(path, "rb") as fh:
        return "data:image/%s;base64,%s" % (sub, base64.b64encode(fh.read()).decode())


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", required=True, help="path to the agenthand release binary")
    ap.add_argument("--photo", required=True, help="photo to identify (jpg / png / heic)")
    ap.add_argument("--comps", default=os.environ.get("AGENTHAND_COMPS_FILE", ""),
                    help="comps file (default: $AGENTHAND_COMPS_FILE, else <data dir>/pricing-comps.json)")
    ap.add_argument("--runs", type=int, default=3)
    args = ap.parse_args()

    env = dict(os.environ)
    if args.comps:
        env["AGENTHAND_COMPS_FILE"] = os.path.abspath(os.path.expanduser(args.comps))
    if not env.get("META_API_KEY"):
        print("note: META_API_KEY not in the environment; the server may still read it from .env")

    server = Server(os.path.expanduser(args.bin), env)
    server.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                               "clientInfo": {"name": "verify-identify-pricing", "version": "1"}})
    server.notify("notifications/initialized")
    image = data_uri(os.path.expanduser(args.photo))

    failures = 0
    concept = None
    for run in range(1, args.runs + 1):
        resp = server.call("tools/call", {"name": "identify", "arguments": {"image": image}})
        if "error" in resp:
            print("run %d: JSON-RPC error: %s" % (run, resp["error"]))
            failures += 1
            continue
        result = resp["result"]["structuredContent"]
        empty = [k for k in ("description", "concept", "suggested_title")
                 if not (isinstance(result.get(k), str) and result[k].strip())]
        ok = result.get("status") == "ok" and result.get("attempts") == 1 and not empty
        pricing = result.get("pricing")
        print("run %d: status=%s attempts=%s concept=%r title=%r" % (
            run, result.get("status"), result.get("attempts"),
            result.get("concept"), result.get("suggested_title")))
        if result.get("status") != "ok":
            print("        error: %s" % json.dumps(result.get("error")))
        if empty:
            print("        EMPTY fields: %s" % ", ".join(empty))
        print("        pricing: %s" % json.dumps(pricing))
        if not ok:
            failures += 1
        concept = result.get("concept") or concept

    if concept:
        resp = server.call("tools/call", {"name": "price", "arguments": {"item_name": concept}})
        print("price(%r): %s" % (concept, json.dumps(resp.get("result", {}).get("structuredContent", resp))))
    server.close()

    if failures:
        print("FAIL: %d of %d identify runs were not clean (error, retry, or empty field)" % (failures, args.runs))
        sys.exit(1)
    print("PASS: %d of %d identify runs returned full bodies on the first attempt" % (args.runs, args.runs))


if __name__ == "__main__":
    main()
