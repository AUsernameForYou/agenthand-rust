"""The site's proof fixture, pinned to the real files.

The badass Gauntlet found the browser verifier grading hard-coded
constants while the page claimed nothing was retyped. The honesty
pass (verifier-v4.js, extended by v5 and superseded by the raw-decoding verifier-v6.js)
grades the run's
published receipt instead —
and these tests pin the whole arrangement down:

- the 580,613 / 0 figure is what _measure actually finds on the
  real proof PNGs served by the site;
- the published receipt's hashes are the served files' hashes;
- the printed receipt on the page agrees with the receipt file;
- proof-run.json is the canonical home for the run's figures, and
  the replay data, README and page statics agree with it;
- the verifier JS contains no run constants to grade against;
- the counting code displayed on the page is the code the
  verifier file actually ships.
"""

import hashlib
import html as html_module
import json
from io import BytesIO
import re
import shutil
import subprocess
from pathlib import Path

import pytest
from PIL import Image, ImageCms

from agenthand import composite
from agenthand.assets import pixel_hash_of_image

DOCS = Path(__file__).resolve().parents[1] / "docs"
FIXTURES = Path(__file__).resolve().parent / "fixtures"
RECEIPT = json.loads((DOCS / "proof-receipt-v1.json").read_text(encoding="utf-8"))


def _sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def test_measure_on_the_real_proof_pngs():
    # THE pin: 580,613 checked, 0 changed is measured here, from
    # the byte-exact files the site serves — not asserted from the
    # page's own text.
    layer = Image.open(DOCS / "proof-layer-v1.png")
    output = Image.open(DOCS / "proof-output-v1.png")
    counts = composite._measure(layer, output)
    assert counts["opaque_pixels"] == 580_613
    assert counts["changed_opaque_pixels"] == 0
    assert counts["opaque_pixels"] == RECEIPT["opaque_pixels"]
    assert counts["changed_opaque_pixels"] == RECEIPT["changed_opaque_pixels"]


def test_published_receipt_matches_the_served_files():
    assert RECEIPT["source_sha256"] == _sha256(DOCS / "proof-source-v1.png")
    assert RECEIPT["foreground_sha256"] == _sha256(DOCS / "proof-layer-v1.png")
    assert RECEIPT["output_sha256"] == _sha256(DOCS / "proof-output-v1.png")
    layer = Image.open(DOCS / "proof-layer-v1.png")
    output = Image.open(DOCS / "proof-output-v1.png")
    mask = Image.open(DOCS / "proof-mask-v1.png")
    assert RECEIPT["foreground_pixel_sha256"] == pixel_hash_of_image(layer)
    assert RECEIPT["output_pixel_sha256"] == pixel_hash_of_image(output)
    assert RECEIPT["mask_pixels_total"] == RECEIPT["opaque_pixels"]
    assert mask.size == layer.size == output.size


def test_page_figures_agree_with_the_receipt():
    page = (DOCS / "index.html").read_text(encoding="utf-8")
    assert f"{RECEIPT['opaque_pixels']:,}" in page  # the hero figure
    # The printed receipt block carries the receipt file's hashes.
    assert RECEIPT["source_sha256"] in page
    assert RECEIPT["foreground_pixel_sha256"] in page


MANIFEST = json.loads((DOCS / "proof-run.json").read_text(encoding="utf-8"))
REPO_ROOT = Path(__file__).resolve().parents[1]


def test_proof_run_manifest_matches_the_published_receipt():
    # proof-run.json is the canonical home for the published run's
    # figures: every number in it must be the receipt's number, and
    # the receipt it names must be the file the site serves.
    assert MANIFEST["fixture"] is True
    assert MANIFEST["pixels"]["opaque_checked"] == RECEIPT["opaque_pixels"]
    assert MANIFEST["pixels"]["changed"] == RECEIPT["changed_opaque_pixels"]
    assert MANIFEST["pixels"]["edge_band_pixels"] == RECEIPT["edge_band_pixels"]
    assert (
        MANIFEST["pixels"]["color_profile_preserved"]
        == RECEIPT["color_profile_preserved"]
    )
    settled = RECEIPT["provenance"]["settled_costs_usd"]
    for tool in ("identify", "cut_out", "draw", "total"):
        assert MANIFEST["costs_usd"][tool] == pytest.approx(settled[tool])
    assert MANIFEST["call_ids"] == RECEIPT["provenance"]["call_ids"]
    assert MANIFEST["receipt"]["file"] == "proof-receipt-v1.json"
    assert MANIFEST["receipt"]["sha256"] == _sha256(DOCS / "proof-receipt-v1.json")
    assert (
        MANIFEST["receipt"]["foreground_pixel_sha256"]
        == RECEIPT["foreground_pixel_sha256"]
    )
    assert (
        MANIFEST["receipt"]["output_pixel_sha256"]
        == RECEIPT["output_pixel_sha256"]
    )


def test_proof_run_manifest_matches_replay_readme_and_page():
    # The other figure-carrying surfaces must agree with the
    # manifest, so swapping in a new run is a one-file change plus
    # whatever the suite names here.
    replay = json.loads((DOCS / "replay-run-v1.json").read_text(encoding="utf-8"))
    assert MANIFEST["models"] == {
        call["tool"]: call["model"] for call in replay["calls"]
    }
    assert MANIFEST["run"]["started_at"] == replay["run"]["started_at"]
    assert MANIFEST["run"]["ended_at"] == replay["run"]["ended_at"]
    assert MANIFEST["costs_usd"]["total"] == pytest.approx(
        replay["run"]["total_settled_cost_usd"]
    )

    readme = (REPO_ROOT / "README.md").read_text(encoding="utf-8")
    assert f"{MANIFEST['pixels']['opaque_checked']:,}" in readme
    for tool in ("identify", "cut_out", "draw"):
        assert f"{tool} ${MANIFEST['costs_usd'][tool]}" in readme

    page = (DOCS / "index.html").read_text(encoding="utf-8")
    # The static fallback standing in the hero is the manifest's
    # figure, and the display script reads the manifest itself —
    # no run constant is baked into the page's JS.
    assert (
        f'<span id="hero-checked">{MANIFEST["pixels"]["opaque_checked"]:,}</span>'
        in page
    )
    assert "fetch('proof-run.json')" in page
    assert 'id="receipt-costs"' in page
    scripts = re.findall(r"<script>(.*?)</script>", page, flags=re.DOTALL)
    assert all("580613" not in s and "580,613" not in s for s in scripts)


def test_verifier_grades_the_receipt_not_constants():
    js = (DOCS / "verifier-v6.js").read_text(encoding="utf-8")
    assert "proof-receipt-v1.json" in js
    for field in (
        "source_sha256",
        "foreground_sha256",
        "foreground_pixel_sha256",
        "output_sha256",
        "output_pixel_sha256",
    ):
        assert RECEIPT[field] not in js, f"hard-coded {field} in verifier"
    assert "580613" not in js
    assert "580,613" not in js
    # Zero changed is the law in code, not an expectation from data.
    assert "counts.changed === 0" in js
    assert "counts.changed === 1" in js  # the separate tamper-detection law
    # v6 grades profiled files by decoding stored PNG samples
    # itself; it must not return to the old iCCP refusal or canvas
    # colour-managed decode.
    assert "function decodePngRgba" in js
    assert "DecompressionStream" in js
    assert "createImageBitmap" not in js
    assert "refuses it rather than grading approximately" not in js
    page = (DOCS / "index.html").read_text(encoding="utf-8")
    assert "embedded Display P3 profile does not change the pixel values" in page
    assert "file carrying an embedded colour profile is refused outright" not in page
    assert 'src="verifier-v6.js"' in page
    assert 'id="verify-btn"' in page
    assert 'id="verify-tamper-btn"' in page
    assert 'id="verify-tamper"' not in page  # no checkbox gate before the click


def test_displayed_counting_code_matches_the_verifier_file():
    page = (DOCS / "index.html").read_text(encoding="utf-8")
    start = page.index('<div class="opencode">')
    pre = page[page.index('<pre class="code">', start) : page.index("</pre>", start)]
    displayed = html_module.unescape(re.sub(r"<[^>]+>", "", pre)).strip()

    js = (DOCS / "verifier-v6.js").read_text(encoding="utf-8")
    match = re.search(
        r"/\* Replicates composite\.py.*?\n\}\n", js, flags=re.DOTALL
    )
    assert match, "countChanged not found in verifier-v6.js"
    shipped = match.group(0).strip()
    assert displayed == shipped


@pytest.mark.skipif(shutil.which("node") is None, reason="node not available")
def test_verifier_helpers_under_node(tmp_path):
    # The pure helpers, exercised headlessly: expectations come
    # from the real receipt, the verdict law refuses a doctored
    # receipt, and the ICC scan reads real chunk headers.
    script = tmp_path / "check.js"
    script.write_text(
        """
const fs = require("fs");
const v = require(process.argv[2]);
const receipt = JSON.parse(fs.readFileSync(process.argv[3], "utf8"));
const exp = v.expectationsFromReceipt(receipt);
if (exp.expectedChecked !== receipt.opaque_pixels) throw new Error("checked");
if (exp.layerFileSha256 !== receipt.foreground_sha256) throw new Error("hash");
let verdict = v.verdictFor(exp, { checked: exp.expectedChecked, changed: 0 });
if (!verdict.pass) throw new Error("clean run must pass");
// Grok's case: a fixture whose data expects changed = 5 must NOT
// pass — zero is law in code, and the receipt must report zero.
const doctored = Object.assign({}, exp, { receiptChanged: 5 });
verdict = v.verdictFor(doctored, { checked: exp.expectedChecked, changed: 5 });
if (verdict.pass) throw new Error("doctored receipt passed");
verdict = v.verdictFor(exp, { checked: exp.expectedChecked, changed: 1 });
if (verdict.pass) throw new Error("one changed pixel passed");
// The separate tamper case succeeds only on exact detection: full
// coverage, exactly one changed pixel, receipt still reporting zero,
// and the decoded output pixel hash no longer matching.
if (!v.onePixelTamperDetected(exp, { checked: exp.expectedChecked, changed: 1 }, false))
  throw new Error("exact one-pixel tamper not detected");
if (v.onePixelTamperDetected(exp, { checked: exp.expectedChecked, changed: 1 }, true))
  throw new Error("tamper passed while output pixel hash still matched");
if (v.onePixelTamperDetected(exp, { checked: exp.expectedChecked, changed: 2 }, false))
  throw new Error("two changed pixels accepted as the one-pixel case");
verdict = v.verdictFor(exp, { checked: 0, changed: 0 });
if (verdict.pass) throw new Error("zero coverage passed");
// ICC scan: the real fixture has no iCCP; a synthetic one does.
const layer = fs.readFileSync(process.argv[4]);
if (v.pngHasICC(layer.buffer.slice(layer.byteOffset, layer.byteOffset + layer.byteLength)))
  throw new Error("fixture PNG has iCCP?");
const sig = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
const len = Buffer.alloc(4); len.writeUInt32BE(3);
const chunk = Buffer.concat([len, Buffer.from("iCCP"), Buffer.from([1, 2, 3]), Buffer.alloc(4)]);
const withIcc = Buffer.concat([sig, chunk]);
if (!v.pngHasICC(withIcc.buffer.slice(withIcc.byteOffset, withIcc.byteOffset + withIcc.byteLength)))
  throw new Error("synthetic iCCP not detected");
console.log("node helpers ok");
""",
        encoding="utf-8",
    )
    result = subprocess.run(
        [
            "node",
            str(script),
            str(DOCS / "verifier-v6.js"),
            str(DOCS / "proof-receipt-v1.json"),
            str(DOCS / "proof-layer-v1.png"),
        ],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert result.returncode == 0, result.stderr
    assert "node helpers ok" in result.stdout


@pytest.mark.skipif(shutil.which("node") is None, reason="node not available")
def test_raw_png_decoder_matches_python_on_the_real_proof_pngs(tmp_path):
    # The raw decoder is not trusted because it agrees with itself:
    # on the site's real 1206x1925 fixture it must reproduce the
    # published receipt's Python/Pillow pixel hashes and counts.
    script = tmp_path / "raw-proof.js"
    script.write_text(
        """
const fs = require("fs");
const v = require(process.argv[2]);
function bytes(path) {
  const b = fs.readFileSync(path);
  return b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength);
}
(async () => {
  const receipt = JSON.parse(fs.readFileSync(process.argv[3], "utf8"));
  const exp = v.expectationsFromReceipt(receipt);
  const layer = await v.decodePngRgba(bytes(process.argv[4]), "product layer");
  const output = await v.decodePngRgba(bytes(process.argv[5]), "final output");
  const layerHash = await v.sha256Hex(layer.data.buffer.slice(0));
  const outputHash = await v.sha256Hex(output.data.buffer.slice(0));
  if (layerHash !== exp.layerPixelSha256) throw new Error("layer pixel hash");
  if (outputHash !== exp.outputPixelSha256) throw new Error("output pixel hash");
  const counts = await v.countChanged(
    layer.data, output.data, layer.width, layer.height,
    { schedule: (fn) => setTimeout(fn, 0) }
  );
  if (counts.checked !== exp.expectedChecked) throw new Error("checked count");
  if (counts.changed !== 0) throw new Error("changed count");
  if (!v.verdictFor(exp, counts).pass) throw new Error("clean verdict");
  console.log("node raw proof ok");
})().catch((err) => { console.error(err); process.exit(1); });
""",
        encoding="utf-8",
    )
    result = subprocess.run(
        [
            "node",
            str(script),
            str(DOCS / "verifier-v6.js"),
            str(DOCS / "proof-receipt-v1.json"),
            str(DOCS / "proof-layer-v1.png"),
            str(DOCS / "proof-output-v1.png"),
        ],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert result.returncode == 0, result.stderr
    assert "node raw proof ok" in result.stdout


@pytest.mark.skipif(shutil.which("node") is None, reason="node not available")
def test_display_p3_pngs_decode_to_pillow_hashes_under_node(tmp_path):
    # Regression for the filmed-run failure mode: a Display P3
    # iCCP chunk must not make the site verifier refuse the run's
    # own files. Python computes the ground truth with Pillow,
    # whose pixel hash ignores ICC transforms; the JS raw decoder
    # must land on exactly the same RGBA bytes.
    layer_path = FIXTURES / "display-p3-layer-v1.png"
    output_path = FIXTURES / "display-p3-output-v1.png"
    tampered_path = FIXTURES / "display-p3-output-tampered-v1.png"

    images = {}
    for key, path in (
        ("layer", layer_path),
        ("output", output_path),
        ("tampered", tampered_path),
    ):
        image = Image.open(path)
        image.load()
        images[key] = image
        icc = image.info.get("icc_profile")
        assert icc, f"{path.name} lost its ICC profile"
        description = ImageCms.ImageCmsProfile(BytesIO(icc)).profile
        assert description.profile_description.strip() == "Display P3"

    clean = composite._measure(images["layer"], images["output"])
    tampered = composite._measure(images["layer"], images["tampered"])
    assert clean["opaque_pixels"] == 6
    assert clean["changed_opaque_pixels"] == 0
    assert tampered["opaque_pixels"] == 6
    assert tampered["changed_opaque_pixels"] == 1

    pixel_hashes = {
        key: pixel_hash_of_image(image) for key, image in images.items()
    }
    script = tmp_path / "raw-p3.js"
    script.write_text(
        """
const fs = require("fs");
const v = require(process.argv[2]);
function bytes(path) {
  const b = fs.readFileSync(path);
  return b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength);
}
(async () => {
  const paths = {
    layer: process.argv[3],
    output: process.argv[4],
    tampered: process.argv[5],
  };
  const expectedHashes = {
    layer: process.argv[6],
    output: process.argv[7],
    tampered: process.argv[8],
  };
  const decoded = {};
  for (const key of Object.keys(paths)) {
    const raw = bytes(paths[key]);
    if (!v.pngHasICC(raw)) throw new Error(key + " iCCP not detected");
    decoded[key] = await v.decodePngRgba(raw, key);
    const hash = await v.sha256Hex(decoded[key].data.buffer.slice(0));
    if (hash !== expectedHashes[key]) throw new Error(key + " pixel hash");
  }
  const expected = {
    expectedChecked: 6,
    receiptChanged: 0,
  };
  const clean = await v.countChanged(
    decoded.layer.data, decoded.output.data,
    decoded.layer.width, decoded.layer.height,
    { schedule: (fn) => setTimeout(fn, 0) }
  );
  if (clean.checked !== 6 || clean.changed !== 0) throw new Error("P3 clean count");
  if (!v.verdictFor(expected, clean).pass) throw new Error("P3 clean verdict");
  const tampered = await v.countChanged(
    decoded.layer.data, decoded.tampered.data,
    decoded.layer.width, decoded.layer.height,
    { schedule: (fn) => setTimeout(fn, 0) }
  );
  if (tampered.checked !== 6 || tampered.changed !== 1) {
    throw new Error("P3 tamper count");
  }
  if (v.verdictFor(expected, tampered).pass) throw new Error("P3 tamper passed");
  console.log("node Display P3 raw decode ok");
})().catch((err) => { console.error(err); process.exit(1); });
""",
        encoding="utf-8",
    )
    result = subprocess.run(
        [
            "node",
            str(script),
            str(DOCS / "verifier-v6.js"),
            str(layer_path),
            str(output_path),
            str(tampered_path),
            pixel_hashes["layer"],
            pixel_hashes["output"],
            pixel_hashes["tampered"],
        ],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert result.returncode == 0, result.stderr
    assert "node Display P3 raw decode ok" in result.stdout
