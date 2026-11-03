/* agenthand — proof set piece + in-browser Fidelity Lock verifier.
 *
 * The verifier grades the run's ACTUAL receipt, fetched at run time
 * from proof-receipt-v1.json (the frozen 2026-10-26 run's fidelity
 * receipt, published verbatim beside the images). Every expectation
 * below — file hashes, pixel hashes, the checked-pixel total — is
 * read out of that receipt; no run number is hard-coded in this
 * file. The one value that is NOT taken from the receipt is the
 * changed count a pass requires: zero, enforced here as code. A
 * receipt that itself reported changed pixels could not pass by
 * agreeing with itself.
 *
 * What stays in this file as data is page furniture only: the image
 * URLs. Swapping in a new run's fixture means dropping in its PNGs
 * and its receipt JSON; the grading follows the receipt.
 *
 * ICC rule: the browser's image decode applies colour conversion
 * for embedded ICC profiles, which would make a value-exact scan
 * meaningless — so a PNG carrying an iCCP chunk is REFUSED, not
 * graded approximately. (The Python verifier treats a profile
 * change between source and output as a fidelity failure; the
 * browser's honest equivalent is to decline profiled files.)
 * Decode also requests colorSpaceConversion: "none" where the
 * browser supports it. The fixture PNGs carry no profile chunks.
 * The hash checks are the final arbiter: if a decode ever drifted,
 * the pixel hashes would mismatch and the run would fail, never
 * pass falsely.
 *
 * History: v1 flipped a byte inside the encoded PNG and aborted on
 * decode; v2 moved the byte flip to the hashing path only and added
 * the decoded-pixel flip; v3 added the live-progress surface. v4 is
 * the honesty pass above; the counting code is unchanged since v1.
 * v5 keeps that grading intact and adds a judge-first surface: the
 * frozen sample is already selected, the clean check is one click,
 * and the tamper check is a separate one-click run that flips
 * exactly one decoded pixel (the encoded files are not altered).
 *
 * TODO(baseline-regeneration): no plain-regeneration baseline is
 * recorded in the frozen package, so this verifier deliberately
 * publishes no baseline count. Add one only from a separately
 * recorded same-photo regeneration run, with its own provenance.
 */
"use strict";

/* Page furniture: where the run's files live. The receipt URL is
 * the only grading input — see expectationsFromReceipt. */
var FIXTURE = {
  id: "verified-2026-10-26",
  label: "verified live run, 2026-10-26",
  receiptUrl: "proof-receipt-v1.json",
  files: {
    source: { url: "proof-source-v1.png" },
    layer: { url: "proof-layer-v1.png" },
    plate: { url: "proof-plate-v1.png" },
    output: { url: "proof-output-v1.png" },
  },
};

/* ---------------- pure helpers (also exercised headlessly) -------------- */

function sha256Hex(buffer) {
  return crypto.subtle.digest("SHA-256", buffer).then(function (digest) {
    var bytes = new Uint8Array(digest);
    var hex = "";
    for (var i = 0; i < bytes.length; i++) {
      hex += ("0" + bytes[i].toString(16)).slice(-2);
    }
    return hex;
  });
}

/* Derive every graded expectation from the run's receipt object.
 * Throws (fails the run closed) when a field the grader needs is
 * absent — grading against a partial receipt is not grading. */
function expectationsFromReceipt(receipt) {
  function need(field) {
    if (receipt[field] === undefined || receipt[field] === null) {
      throw new Error("the published receipt is missing " + field);
    }
    return receipt[field];
  }
  return {
    sourceFileSha256: need("source_sha256"),
    layerFileSha256: need("foreground_sha256"),
    layerPixelSha256: need("foreground_pixel_sha256"),
    outputFileSha256: need("output_sha256"),
    outputPixelSha256: need("output_pixel_sha256"),
    expectedChecked: need("opaque_pixels"),
    receiptChanged: need("changed_opaque_pixels"),
  };
}

/* The verdict law, stated once: the scan must cover exactly the
 * receipt's opaque total (> 0 — a check of nothing proves nothing),
 * the scan must find ZERO changed pixels (zero is code here, never
 * data), and the receipt itself must report zero changed. */
function verdictFor(expected, counts) {
  var checkedOk =
    counts.checked === expected.expectedChecked && counts.checked > 0;
  var changedOk = counts.changed === 0;
  var receiptOk = expected.receiptChanged === 0;
  return {
    checkedOk: checkedOk,
    changedOk: changedOk,
    receiptOk: receiptOk,
    pass: checkedOk && changedOk && receiptOk,
  };
}

/* The one-pixel tamper case is a detection test, not a clean-run
 * verdict. It succeeds only when the full receipt coverage is
 * scanned, exactly one pixel is reported changed, the published
 * receipt still reports zero, and the altered decoded output no
 * longer matches the receipt's output pixel hash. */
function onePixelTamperDetected(expected, counts, outputPixelHashMatches) {
  return (
    counts.checked === expected.expectedChecked &&
    counts.checked > 0 &&
    counts.changed === 1 &&
    expected.receiptChanged === 0 &&
    outputPixelHashMatches === false
  );
}

/* Walk the PNG chunk headers (no decode) looking for an iCCP
 * chunk. Stops at the first IDAT: profiles live before it. */
function pngHasICC(buffer) {
  var bytes = new Uint8Array(buffer);
  // PNG signature: 89 50 4E 47 0D 0A 1A 0A
  if (bytes.length < 8 || bytes[0] !== 0x89 || bytes[1] !== 0x50) {
    return false; // not a PNG; the decode step will refuse it instead
  }
  var view = new DataView(buffer);
  var offset = 8;
  while (offset + 12 <= bytes.length) {
    var length = view.getUint32(offset);
    var type = String.fromCharCode(
      bytes[offset + 4],
      bytes[offset + 5],
      bytes[offset + 6],
      bytes[offset + 7]
    );
    if (type === "iCCP") return true;
    if (type === "IDAT" || type === "IEND") return false;
    offset += 12 + length;
  }
  return false;
}

/* Replicates composite.py's _measure for the claim that matters:
 * a foreground pixel with alpha 255 must appear in the composite
 * byte-identical. fgData / outData are RGBA byte arrays (ImageData.data).
 * Chunked so the caller can animate the "checked" counter; resolves to
 * {checked, changed}. */
function countChanged(fgData, outData, width, height, opts) {
  opts = opts || {};
  var chunkRows = opts.chunkRows || 64;
  var onProgress = opts.onProgress || function () {};
  var schedule = opts.schedule || function (fn) { setTimeout(fn, 0); };
  var checked = 0;
  var changed = 0;
  var row = 0;
  return new Promise(function (resolve) {
    function step() {
      var endRow = Math.min(row + chunkRows, height);
      for (var y = row; y < endRow; y++) {
        var rowStart = y * width * 4;
        for (var x = 0; x < width; x++) {
          var j = rowStart + x * 4;
          if (fgData[j + 3] === 255) {
            checked++;
            if (
              fgData[j] !== outData[j] ||
              fgData[j + 1] !== outData[j + 1] ||
              fgData[j + 2] !== outData[j + 2] ||
              fgData[j + 3] !== outData[j + 3]
            ) {
              changed++;
            }
          }
        }
      }
      row = endRow;
      onProgress(checked, changed);
      if (row < height) {
        schedule(step);
      } else {
        resolve({ checked: checked, changed: changed });
      }
    }
    step();
  });
}

/* Tamper helpers. tamperedCopy is retained from v4 for compatibility
 * with the headless helper tests; the v5 one-pixel check does NOT use
 * it. That check leaves every encoded file byte untouched, so all file
 * hashes must still match, and flips one pixel only after decoding.
 * The pixel flip alters already-decoded RGBA data, which always scans
 * fine and must produce exactly one changed pixel. */
function tamperedCopy(buffer) {
  var copy = new Uint8Array(buffer.slice(0));
  copy[100] = copy[100] ^ 0xff; // one byte, in the copy only
  return copy.buffer;
}

/* Flip the red channel of the first pixel that is fully opaque in the
 * foreground (layer) data, inside the decoded output data. Returns
 * true when a pixel was flipped. Exactly one pixel changes, so the
 * scan must report changed = 1. */
function flipFirstOpaquePixel(fgData, outData) {
  for (var j = 0; j < fgData.length; j += 4) {
    if (fgData[j + 3] === 255) {
      outData[j] = outData[j] ^ 0xff;
      return true;
    }
  }
  return false;
}

/* ---------------- browser wiring --------------------------------------- */

function initProof() {
  var run = FIXTURE;
  var loadBtn = document.getElementById("proof-load");
  var stage = document.getElementById("proof-stage");
  var plateImg = document.getElementById("proof-plate");
  var layerImg = document.getElementById("proof-layer");
  var sourceImg = document.getElementById("proof-source");
  var outputImg = document.getElementById("proof-output");
  var sceneBtns = document.querySelectorAll("[data-scene]");
  var evidenceLoaded = false;

  function setScene(scene) {
    for (var i = 0; i < sceneBtns.length; i++) {
      var active = sceneBtns[i].getAttribute("data-scene") === scene;
      sceneBtns[i].classList.toggle("active", active);
      sceneBtns[i].setAttribute("aria-pressed", active ? "true" : "false");
    }
    stage.setAttribute("data-scene", scene);
    plateImg.style.display = scene === "plate" ? "block" : "none";
    sourceImg.style.display = scene === "source" ? "block" : "none";
    outputImg.style.display = scene === "output" ? "block" : "none";
    layerImg.style.display =
      scene === "source" || scene === "output" ? "none" : "block";
  }

  if (loadBtn) {
    loadBtn.addEventListener("click", function () {
      if (evidenceLoaded) return;
      evidenceLoaded = true;
      plateImg.src = run.files.plate.url;
      layerImg.src = run.files.layer.url;
      sourceImg.src = run.files.source.url;
      outputImg.src = run.files.output.url;
      stage.hidden = false;
      loadBtn.textContent = "Evidence loaded";
      loadBtn.disabled = true;
      setScene("plate");
    });
  }
  for (var i = 0; i < sceneBtns.length; i++) {
    sceneBtns[i].addEventListener("click", function (ev) {
      if (!evidenceLoaded && loadBtn) loadBtn.click();
      setScene(ev.currentTarget.getAttribute("data-scene"));
    });
  }

  /* ---------------- verifier ---------------- */
  var verifyBtn = document.getElementById("verify-btn");
  var tamperBtn = document.getElementById("verify-tamper-btn");
  var statusEl = document.getElementById("verify-status");
  var checkedEl = document.getElementById("verify-checked");
  var changedEl = document.getElementById("verify-changed");
  var linesEl = document.getElementById("verify-lines");
  var progressEl = document.getElementById("verify-progress");
  if (!verifyBtn) return;

  var reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  var running = false;

  function setButtonsDisabled(disabled) {
    verifyBtn.disabled = disabled;
    if (tamperBtn) tamperBtn.disabled = disabled;
  }

  function line(text, ok) {
    var div = document.createElement("div");
    var mark = document.createElement("span");
    mark.textContent = ok === null ? "…" : ok ? "✓" : "✗";
    mark.className = ok === null ? "k" : ok ? "tick" : "fail";
    div.appendChild(mark);
    div.appendChild(document.createTextNode(" " + text));
    linesEl.appendChild(div);
    return div;
  }

  function fmt(n) {
    return n.toString().replace(/\B(?=(\d{3})+(?!\d))/g, ",");
  }

  function fetchBytes(url) {
    return fetch(url).then(
      function (res) {
        if (!res.ok) throw new Error("fetch failed: " + url + " (HTTP " + res.status + ")");
        return res.arrayBuffer();
      },
      function () {
        throw new Error("fetch failed: " + url + " (network error)");
      }
    );
  }

  function fetchReceipt(url) {
    return fetchBytes(url).then(function (buffer) {
      var text = new TextDecoder("utf-8").decode(buffer);
      try {
        return JSON.parse(text);
      } catch (e) {
        throw new Error("the published receipt (" + url + ") is not valid JSON");
      }
    });
  }

  /* label names the file in any error (e.g. "final output
   * (proof-output-v1.png)"), so a failure never blames the wrong file
   * the way the browser's bare "source image could not be decoded"
   * message did. Decoding asks for no colour-space conversion; if a
   * browser refuses the option it falls back to a plain decode, and
   * the hash checks remain the arbiter of exactness either way. */
  function decodePixels(buffer, label) {
    var blob = new Blob([buffer], { type: "image/png" });
    function toPixels(bitmap) {
      var canvas = document.createElement("canvas");
      canvas.width = bitmap.width;
      canvas.height = bitmap.height;
      var ctx = canvas.getContext("2d", { willReadFrequently: true });
      ctx.drawImage(bitmap, 0, 0);
      var data = ctx.getImageData(0, 0, bitmap.width, bitmap.height);
      if (bitmap.close) bitmap.close();
      return data;
    }
    function undecodable() {
      throw new Error(label + " could not be decoded");
    }
    return createImageBitmap(blob, { colorSpaceConversion: "none" }).then(
      toPixels,
      function () {
        return createImageBitmap(blob).then(toPixels, undecodable);
      }
    );
  }

  function runVerification(tamper) {
    if (running) return;
    running = true;
    setButtonsDisabled(true);
    linesEl.innerHTML = "";
    checkedEl.textContent = "0";
    changedEl.textContent = "0";
    changedEl.className = "vcount";
    var t0 = performance.now();
    function elapsed() {
      return ((performance.now() - t0) / 1000).toFixed(1) + "s";
    }
    function progress(text) {
      if (progressEl) progressEl.textContent = text;
    }
    progress("fetching the run's receipt and the byte-exact files · 0.0s");
    statusEl.textContent = tamper
      ? "One-pixel tamper check: the encoded files stay untouched; exactly one fully opaque pixel in the decoded output copy will be flipped in this browser only."
      : "Fetching the run's receipt and the byte-exact files…";

    Promise.all([
      fetchReceipt(run.receiptUrl),
      fetchBytes(run.files.source.url),
      fetchBytes(run.files.layer.url),
      fetchBytes(run.files.output.url),
    ])
      .then(function (results) {
        var expected = expectationsFromReceipt(results[0]);
        var sourceBuf = results[1];
        var layerBuf = results[2];
        var outputBuf = results[3];
        // ICC refusal, before any decode: a profiled PNG cannot be
        // graded value-exact through a browser decode.
        var named = [
          ["product layer (" + run.files.layer.url + ")", layerBuf],
          ["final output (" + run.files.output.url + ")", outputBuf],
        ];
        for (var i = 0; i < named.length; i++) {
          if (pngHasICC(named[i][1])) {
            throw new Error(
              named[i][0] +
                " carries an embedded colour profile (iCCP); a browser decode would convert its colours, so this check refuses it rather than grading approximately"
            );
          }
        }
        // The one-pixel case is intentionally pixel-only: the encoded
        // output buffer is hashed exactly as served, so its file hash
        // must still match. The flip happens after decoding, below.
        var outputHashBuf = outputBuf;
        progress("fetched · hashing with SHA-256 · " + elapsed());
        statusEl.textContent = "Hashing…";
        return Promise.all([
          sha256Hex(sourceBuf),
          sha256Hex(layerBuf),
          sha256Hex(outputHashBuf),
          decodePixels(layerBuf, "product layer (" + run.files.layer.url + ")"),
          decodePixels(outputBuf, "final output (" + run.files.output.url + ")"),
        ]).then(function (hashes) {
          return {
            expected: expected,
            sourceHash: hashes[0],
            layerHash: hashes[1],
            outputHash: hashes[2],
            layerImg: hashes[3],
            outputImg: hashes[4],
          };
        });
      })
      .then(function (r) {
        var expected = r.expected;
        var hashesOk = true;
        function check(label, actual, expectation) {
          var ok = actual === expectation;
          if (!ok) hashesOk = false;
          line(label + " sha256 " + (ok ? "matches the receipt" : "MISMATCH vs the receipt"), ok);
          return ok;
        }
        check("source photo (file)", r.sourceHash, expected.sourceFileSha256);
        check("product layer (file)", r.layerHash, expected.layerFileSha256);
        check("final output (file)", r.outputHash, expected.outputFileSha256);

        var outData = r.outputImg.data;
        if (tamper) {
          // Flip one pixel in the DECODED output copy, before the
          // pixel hashes are taken: the output pixel hash mismatches
          // and the scan below counts exactly 1 changed pixel.
          flipFirstOpaquePixel(r.layerImg.data, outData);
        }

        // Pixel hashes (SHA-256 of the decoded RGBA bytes) pin the
        // receipt's layer and output hashes. If a decode ever
        // drifted from the stored values, these lines fail — the
        // check cannot pass on an approximate decode.
        return Promise.all([
          sha256Hex(r.layerImg.data.buffer.slice(0)),
          sha256Hex(outData.buffer.slice(0)),
        ]).then(function (pixelHashes) {
          check("product layer (pixel)", pixelHashes[0], expected.layerPixelSha256);
          var outputPixelHashMatches =
            pixelHashes[1] === expected.outputPixelSha256;
          if (tamper) {
            line(
              "final output (pixel) sha256 " +
                (outputPixelHashMatches
                  ? "still matches the receipt — the one-pixel flip did not take"
                  : "differs from the receipt after the one-pixel flip, as it must"),
              !outputPixelHashMatches
            );
            if (outputPixelHashMatches) hashesOk = false;
          } else {
            check("final output (pixel)", pixelHashes[1], expected.outputPixelSha256);
          }

          statusEl.textContent = "Scanning every opaque product pixel…";
          return countChanged(
            r.layerImg.data,
            outData,
            r.layerImg.width,
            r.layerImg.height,
            {
              chunkRows: reduced ? 481 : 32,
              schedule: reduced
                ? function (fn) { setTimeout(fn, 0); }
                : function (fn) { requestAnimationFrame(fn); },
              onProgress: function (checked, changed) {
                checkedEl.textContent = fmt(checked);
                changedEl.textContent = fmt(changed);
                progress(
                  "scanning · " + fmt(checked) + " of " + fmt(expected.expectedChecked) +
                  " pixels · " + Math.floor((checked / expected.expectedChecked) * 100) +
                  "% · " + elapsed()
                );
              },
            }
          ).then(function (counts) {
            checkedEl.textContent = fmt(counts.checked);
            changedEl.textContent = fmt(counts.changed);
            var verdict = verdictFor(expected, counts);
            var tamperDetected = tamper &&
              onePixelTamperDetected(
                expected,
                counts,
                outputPixelHashMatches
              );
            changedEl.className = "vcount" + (counts.changed === 0 ? " zero" : " bad");
            line(
              "pixels checked " + fmt(counts.checked) + " · changed " + fmt(counts.changed),
              tamper ? tamperDetected : verdict.checkedOk && verdict.changedOk
            );
            if (!verdict.receiptOk) {
              line(
                "the receipt itself reports " + fmt(expected.receiptChanged) +
                  " changed pixels — a receipt cannot pass by agreeing with itself",
                false
              );
            }
            var pass = !tamper && hashesOk && verdict.pass;
            var tamperPass = tamper && hashesOk && tamperDetected;
            progress(
              "finished in " + elapsed() + " · " +
              new Date().toLocaleTimeString([], { hour12: false })
            );
            statusEl.textContent = pass
              ? "Verified in your browser: the files are the run's files, graded against the run's own receipt, and the product layer reached the output unchanged."
              : tamperPass
                ? "One-pixel tamper check worked: every file hash still matches, the decoded output pixel hash differs, and the scan finds exactly 1 changed pixel. Run the frozen sample check to see the untouched files pass."
                : tamper
                  ? "The one-pixel tamper case was not detected exactly as specified. That is a failed check — the scan, hashes, and receipt are shown above."
                  : "A check failed. That is what the check is for — the files served here do not match the receipt.";
            running = false;
            setButtonsDisabled(false);
          });
        });
      })
      .catch(function (err) {
        progress("stopped after " + elapsed());
        statusEl.textContent = "Verification could not run: " + err.message;
        running = false;
        setButtonsDisabled(false);
      });
  }

  verifyBtn.addEventListener("click", function () {
    runVerification(false);
  });
  if (tamperBtn) {
    tamperBtn.addEventListener("click", function () {
      runVerification(true);
    });
  }
}

if (typeof document !== "undefined") {
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", initProof);
  } else {
    initProof();
  }
}

if (typeof window !== "undefined") {
  window.AgenthandVerifier = {
    FIXTURE: FIXTURE,
    sha256Hex: sha256Hex,
    countChanged: countChanged,
    tamperedCopy: tamperedCopy,
    flipFirstOpaquePixel: flipFirstOpaquePixel,
    expectationsFromReceipt: expectationsFromReceipt,
    verdictFor: verdictFor,
    onePixelTamperDetected: onePixelTamperDetected,
    pngHasICC: pngHasICC,
  };
}

if (typeof module !== "undefined" && module.exports) {
  module.exports = {
    FIXTURE: FIXTURE,
    sha256Hex: sha256Hex,
    countChanged: countChanged,
    tamperedCopy: tamperedCopy,
    flipFirstOpaquePixel: flipFirstOpaquePixel,
    expectationsFromReceipt: expectationsFromReceipt,
    verdictFor: verdictFor,
    onePixelTamperDetected: onePixelTamperDetected,
    pngHasICC: pngHasICC,
  };
}
