/* agenthand — proof set piece + in-browser Fidelity Lock verifier.
 *
 * Everything run-specific lives in PROOF_RUN below. Swapping in a new
 * run's fixture (e.g. the future product-photo run) is a data change:
 * replace the file names, sizes and hashes with the new run's asset
 * records, drop the byte-exact PNGs next to this file, done.
 *
 * Hash sources for this fixture (asset store sidecar records,
 * ~/workspace/agenthand/data/assets/<id>.json):
 *   source photo  e3f0428fbc5e43c7826959a9eecbda50 (source_photo)
 *   product layer b8663615419e40389f4e48dbd0e38698 (foreground_png)
 *   mask          7dc19362be4c47abadac2edd7e69129e (mask_png)
 *   plate         4f1740f4ec3e493cbc380f97e49c7e8f (background_plate)
 *   composite     45deaf1acea14ab5839c641eee1cd277 (composite)
 * The receipt printed in the page carries the source FILE sha256 and
 * the layer/mask PIXEL sha256; the verifier checks those plus the
 * layer/output file sha256 from the same records.
 */
"use strict";

var PROOF_RUN = {
  id: "verified-2026-10-26",
  label: "verified live run, 2026-10-26",
  width: 1206,
  height: 1925,
  expectedChecked: 580613, // fully opaque product pixels in the layer
  expectedChanged: 0,
  files: {
    source: {
      url: "proof-source-v1.png",
      bytes: 644137,
      fileSha256: "f3dc3c11e57a837d4dc11f28123dd250ca6cde6fb1e2eda9a88548dc6e7aa78b",
    },
    layer: {
      url: "proof-layer-v1.png",
      bytes: 209055,
      fileSha256: "fba19fe69dbb10f4ee087177b4ae07c6c42440af9b77a0e97316bafba9704adf",
      pixelSha256: "99e8890ff5e3dcdf825275d327bf5a28c21c31673fc19ed29549a5aa0fb74100",
    },
    plate: {
      url: "proof-plate-v1.png",
      bytes: 2528814,
      fileSha256: "03eb75bfb812326f9f2632cfbdf747775b4d664953c91205b80333d49be907a7",
    },
    output: {
      url: "proof-output-v1.png",
      bytes: 2198917,
      fileSha256: "13b58a327820aa97e9aa306e861b9dd85c199d6a9ef5d53ea7755c3365a35451",
      pixelSha256: "8b8defea41e94004ddc0affd2abef697f32e20c5cf2fdab4c268236611c9e802",
    },
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

/* ---------------- browser wiring --------------------------------------- */

function initProof() {
  var run = PROOF_RUN;
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
  var tamperBox = document.getElementById("verify-tamper");
  var statusEl = document.getElementById("verify-status");
  var checkedEl = document.getElementById("verify-checked");
  var changedEl = document.getElementById("verify-changed");
  var linesEl = document.getElementById("verify-lines");
  if (!verifyBtn) return;

  var reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  var running = false;

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
    return fetch(url).then(function (res) {
      if (!res.ok) throw new Error("fetch failed: " + url);
      return res.arrayBuffer();
    });
  }

  function decodePixels(buffer) {
    return createImageBitmap(new Blob([buffer], { type: "image/png" })).then(
      function (bitmap) {
        var canvas = document.createElement("canvas");
        canvas.width = bitmap.width;
        canvas.height = bitmap.height;
        var ctx = canvas.getContext("2d", { willReadFrequently: true });
        ctx.drawImage(bitmap, 0, 0);
        var data = ctx.getImageData(0, 0, bitmap.width, bitmap.height);
        if (bitmap.close) bitmap.close();
        return data;
      }
    );
  }

  verifyBtn.addEventListener("click", function () {
    if (running) return;
    running = true;
    verifyBtn.disabled = true;
    linesEl.innerHTML = "";
    checkedEl.textContent = "0";
    changedEl.textContent = "0";
    changedEl.className = "vcount";
    var tamper = tamperBox && tamperBox.checked;
    statusEl.textContent = tamper
      ? "Tamper test: one byte and one pixel of the output copy are altered in this browser only. Watch both checks fail."
      : "Fetching the byte-exact files…";

    Promise.all([
      fetchBytes(run.files.source.url),
      fetchBytes(run.files.layer.url),
      fetchBytes(run.files.output.url),
    ])
      .then(function (buffers) {
        var sourceBuf = buffers[0];
        var layerBuf = buffers[1];
        var outputBuf = buffers[2];
        if (tamper) {
          var tampered = new Uint8Array(outputBuf.slice(0));
          tampered[100] = tampered[100] ^ 0xff; // one byte, client-side only
          outputBuf = tampered.buffer;
        }
        statusEl.textContent = "Hashing…";
        return Promise.all([
          sha256Hex(sourceBuf),
          sha256Hex(layerBuf),
          sha256Hex(outputBuf),
          decodePixels(layerBuf),
          decodePixels(outputBuf),
        ]).then(function (results) {
          return {
            sourceHash: results[0],
            layerHash: results[1],
            outputHash: results[2],
            layerImg: results[3],
            outputImg: results[4],
          };
        });
      })
      .then(function (r) {
        var hashesOk = true;
        function check(label, actual, expected) {
          var ok = actual === expected;
          if (!ok) hashesOk = false;
          line(label + " sha256 " + (ok ? "matches" : "MISMATCH"), ok);
        }
        check("source photo (file, = receipt)", r.sourceHash, run.files.source.fileSha256);
        check("product layer (file)", r.layerHash, run.files.layer.fileSha256);
        check("final output (file)", r.outputHash, run.files.output.fileSha256);

        // Pixel hashes (SHA-256 of the decoded RGBA bytes) pin the
        // receipt's printed layer hash. Exact for this fixture: the
        // PNGs carry no colour-profile chunks and the layer's alpha is
        // binary, so the browser decode is value-exact.
        return Promise.all([
          sha256Hex(r.layerImg.data.buffer.slice(0)),
          sha256Hex(r.outputImg.data.buffer.slice(0)),
        ]).then(function (pixelHashes) {
          check("product layer (pixel, = receipt)", pixelHashes[0], run.files.layer.pixelSha256);
          check("final output (pixel)", pixelHashes[1], run.files.output.pixelSha256);

          var outData = r.outputImg.data;
          if (tamper) {
            // Flip the first fully opaque product pixel in the decoded
            // output copy — the pixel-scan counterpart of the byte flip.
            for (var j = 0; j < outData.length; j += 4) {
              if (r.layerImg.data[j + 3] === 255) {
                outData[j] = outData[j] ^ 0xff;
                break;
              }
            }
          }
          statusEl.textContent = "Scanning every opaque product pixel…";
          return countChanged(r.layerImg.data, outData, run.width, run.height, {
            chunkRows: reduced ? 481 : 32,
            schedule: reduced
              ? function (fn) { setTimeout(fn, 0); }
              : function (fn) { requestAnimationFrame(fn); },
            onProgress: function (checked, changed) {
              checkedEl.textContent = fmt(checked);
              changedEl.textContent = fmt(changed);
            },
          }).then(function (counts) {
            checkedEl.textContent = fmt(counts.checked);
            changedEl.textContent = fmt(counts.changed);
            var scanOk = counts.checked === run.expectedChecked && counts.changed === run.expectedChanged;
            changedEl.className = "vcount" + (counts.changed === 0 ? " zero" : " bad");
            line(
              "pixels checked " + fmt(counts.checked) + " · changed " + fmt(counts.changed),
              scanOk
            );
            var pass = hashesOk && scanOk;
            statusEl.textContent = pass
              ? "Verified in your browser: the files are the run's files, and the product layer reached the output unchanged."
              : tamper
                ? "Tamper test worked: the altered copy fails the hash check and the pixel scan. The untouched files above it are the real thing — uncheck the box and run it again."
                : "A check failed. That is what the check is for — the files served here do not match the receipt.";
            running = false;
            verifyBtn.disabled = false;
          });
        });
      })
      .catch(function (err) {
        statusEl.textContent = "Verification could not run: " + err.message;
        running = false;
        verifyBtn.disabled = false;
      });
  });
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
    PROOF_RUN: PROOF_RUN,
    sha256Hex: sha256Hex,
    countChanged: countChanged,
  };
}
