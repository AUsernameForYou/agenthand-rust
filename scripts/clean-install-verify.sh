#!/usr/bin/env bash
# clean-install-verify.sh — the packaging gate for an agenthand release.
#
# Verifies a built artifact the way a judge's machine would meet it:
#   1. install (unpack the .mcpb, or take a bare binary) into a fresh
#      directory, with a fresh MCP client config and NO META_API_KEY;
#   2. handshake: initialize + tools/list over stdio; the server must
#      report its name/version and exactly the three tools;
#   3. frozen proof: `agenthand parity <replay-package>` must PASS
#      (580,613 opaque pixels, 0 changed, $0.020044 on the receipt);
#   4. HEIC smoke: the library's decode example must decode a real
#      camera original (exit 0) — the same decoder code the server
#      links (repo-side step; the shipped binary has no decode CLI);
#   5. cross-build: the Python build's verify_receipt_from_disk must
#      accept a receipt + store produced by this Rust build
#      (optional; needs --cross-* and the Python repo's venv).
#
# Usage:
#   scripts/clean-install-verify.sh --bundle agenthand-0.3.0-linux-x86_64.mcpb \
#       --replay ~/workspace/meta-hackathon/replay-package \
#       --heic  ~/workspace/meta-hackathon/rust-100/corpus/real/IMG_1340.HEIC \
#       [--binary agenthand-0.3.0-linux-x86_64] \
#       [--cross-receipt receipt.json --cross-store <store-dir> \
#        --python-repo ~/workspace/agenthand]
#
# Every step prints PASS/FAIL; the script exits non-zero on any FAIL
# and leaves a transcript in the install dir it creates (kept on
# failure, removed on success unless --keep is given).

set -uo pipefail

BUNDLE="" BINARY="" REPLAY="" HEIC="" CROSS_RECEIPT="" CROSS_STORE="" PYTHON_REPO="" KEEP=0
while [ $# -gt 0 ]; do
  case "$1" in
    --bundle) BUNDLE="$2"; shift 2;;
    --binary) BINARY="$2"; shift 2;;
    --replay) REPLAY="$2"; shift 2;;
    --heic) HEIC="$2"; shift 2;;
    --cross-receipt) CROSS_RECEIPT="$2"; shift 2;;
    --cross-store) CROSS_STORE="$2"; shift 2;;
    --python-repo) PYTHON_REPO="$2"; shift 2;;
    --keep) KEEP=1; shift;;
    *) echo "unknown argument: $1" >&2; exit 2;;
  esac
done
[ -n "$BUNDLE" ] || [ -n "$BINARY" ] || { echo "give --bundle or --binary" >&2; exit 2; }
[ -n "$REPLAY" ] || { echo "give --replay <replay-package-dir>" >&2; exit 2; }

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d /tmp/agenthand-clean-install.XXXXXX)"
FAILURES=0
say()  { echo "[clean-install] $*"; }
pass() { say "PASS: $*"; }
fail() { say "FAIL: $*"; FAILURES=$((FAILURES+1)); }

# --- 1. install -------------------------------------------------------
INSTALL="$WORK/install"; mkdir -p "$INSTALL"
if [ -n "$BUNDLE" ]; then
  (cd "$INSTALL" && python3 -c 'import sys,zipfile; zipfile.ZipFile(sys.argv[1]).extractall(".")' "$BUNDLE") \
    || { fail "bundle does not unpack"; }
  BIN="$INSTALL/bin/agenthand"
  [ -f "$INSTALL/manifest.json" ] || fail "bundle has no manifest.json"
else
  mkdir -p "$INSTALL/bin"; cp "$BINARY" "$INSTALL/bin/agenthand"; BIN="$INSTALL/bin/agenthand"
fi
[ -x "$BIN" ] || chmod +x "$BIN" 2>/dev/null
[ -x "$BIN" ] && pass "installed binary at $BIN" || fail "binary missing/not executable"

# Fresh client config, exactly what a judge would paste into a client.
cat > "$WORK/mcp-config.json" <<EOF
{"mcpServers": {"agenthand": {"command": "$BIN", "env": {}}}}
EOF
pass "fresh MCP client config written (no META_API_KEY)"

# --- 2. handshake -----------------------------------------------------
python3 - "$BIN" "$WORK" <<'PYEOF'
import json, subprocess, sys
bin_path, work = sys.argv[1], sys.argv[2]
msgs = [
    {"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-03-26","capabilities":{},
        "clientInfo":{"name":"clean-install-verify","version":"1.0"}}},
    {"jsonrpc":"2.0","method":"notifications/initialized"},
    {"jsonrpc":"2.0","id":2,"method":"tools/list"},
]
proc = subprocess.run([bin_path], input="\n".join(json.dumps(m) for m in msgs)+"\n",
                      capture_output=True, text=True, timeout=60,
                      env={"PATH":"/usr/bin:/bin","HOME":work})
lines = [json.loads(l) for l in proc.stdout.splitlines() if l.strip()]
init = next((l for l in lines if l.get("id")==1), {})
tools = next((l for l in lines if l.get("id")==2), {})
info = init.get("result",{}).get("serverInfo",{})
names = sorted(t["name"] for t in tools.get("result",{}).get("tools",[]))
ok = info.get("name")=="agenthand" and names==["cut_out","draw","identify"]
print(f"[clean-install] serverInfo={info} tools={names}")
print("[clean-install] PASS: handshake" if ok else "[clean-install] FAIL: handshake")
sys.exit(0 if ok else 1)
PYEOF
[ $? -eq 0 ] || FAILURES=$((FAILURES+1))

# --- 3. frozen proof --------------------------------------------------
if "$BIN" parity "$REPLAY" > "$WORK/parity.json" 2>"$WORK/parity.err"; then
  python3 - "$WORK/parity.json" "$REPLAY/receipt.json" <<'PYEOF'
import json, sys
out = json.load(open(sys.argv[1])); receipt = json.load(open(sys.argv[2]))
checks = {c["check"]: c for c in out["checks"]}
ok = (out["parity"]=="PASS"
      and checks["recomposed_opaque_pixels"]["value"]=="580613"
      and checks["recomposed_changed_opaque_pixels"]["value"]=="0"
      and checks["frozen_receipt_verify_from_disk_ok"]["ok"] is True
      and receipt["provenance"]["settled_costs_usd"]["total"]==0.020044)
print("[clean-install] PASS: frozen proof (580,613 / 0 / $0.020044)" if ok
      else "[clean-install] FAIL: frozen proof numbers moved")
sys.exit(0 if ok else 1)
PYEOF
  [ $? -eq 0 ] || FAILURES=$((FAILURES+1))
else
  fail "parity run errored: $(cat "$WORK/parity.err")"
fi

# --- 4. HEIC decode smoke (repo-side, same library code) --------------
if [ -n "$HEIC" ]; then
  (cd "$REPO_ROOT" && cargo run --release --quiet --example heic_decode_one -- "$HEIC" \
      >"$WORK/heic-smoke.log" 2>&1)
  if [ $? -eq 0 ]; then pass "HEIC smoke decoded $HEIC"; else fail "HEIC smoke refused/failed (see heic-smoke.log)"; fi
else
  say "SKIP: HEIC smoke (no --heic given)"
fi

# --- 5. cross-build receipt verify (optional) --------------------------
if [ -n "$CROSS_RECEIPT" ] && [ -n "$CROSS_STORE" ] && [ -n "$PYTHON_REPO" ]; then
  "$PYTHON_REPO/.venv/bin/python" - "$PYTHON_REPO" "$CROSS_RECEIPT" "$CROSS_STORE" <<'PYEOF'
import json, sys
from pathlib import Path
sys.path.insert(0, sys.argv[1] + "/src")
from agenthand.assets import AssetStore
from agenthand.composite import verify_receipt_from_disk
receipt = json.load(open(sys.argv[2]))
store = AssetStore(Path(sys.argv[3]))
report = verify_receipt_from_disk(store, receipt["output_asset"], receipt)
print("[clean-install] cross-build verify report ok =", report["ok"])
print("[clean-install] PASS: Python build verifies the Rust receipt" if report["ok"]
      else "[clean-install] FAIL: cross-build verify")
sys.exit(0 if report["ok"] else 1)
PYEOF
  [ $? -eq 0 ] || FAILURES=$((FAILURES+1))
else
  say "SKIP: cross-build verify (needs --cross-receipt/--cross-store/--python-repo)"
fi

# --- verdict ------------------------------------------------------------
if [ "$FAILURES" -eq 0 ]; then
  say "ALL CHECKS PASSED"
  [ "$KEEP" -eq 1 ] || rm -rf "$WORK"
  exit 0
else
  say "$FAILURES check(s) failed; work dir kept at $WORK"
  exit 1
fi
