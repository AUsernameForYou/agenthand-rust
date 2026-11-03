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
 * ICC rule: pixel grading never uses the browser's colour-managed
 * image decoder. The PNG decoder below reads the file's chunks,
 * inflates IDAT, reverses the PNG row filters, and converts the
 * stored samples to RGBA itself. It deliberately ignores iCCP while
 * doing so, because Pillow's receipt hashes are also computed from
 * decoded sample values without applying an ICC transform. A
 * Display P3 file therefore grades on the same numbers as Python;
 * its profile is neither stripped nor converted by this checker.
 * (Profile preservation itself remains a receipt/Python check; the
 * browser scan checks the file bytes and decoded pixel bytes.)
 *
 * History: v1 flipped a byte inside the encoded PNG and aborted on
 * decode; v2 moved the byte flip to the hashing path only and added
 * the decoded-pixel flip; v3 added the live-progress surface. v4 is
 * the honesty pass above; the counting code is unchanged since v1.
 * v5 keeps that grading intact and adds a judge-first surface: the
 * frozen sample is already selected, the clean check is one click,
 * and the tamper check is a separate one-click run that flips
 * exactly one decoded pixel (the encoded files are not altered).
 * v6 keeps the v5 judge surface and replaces canvas decoding and
 * the iCCP refusal with the deterministic raw PNG decoder below.
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

/* ---------------- deterministic raw PNG decoding ----------------------
 * This decoder is the reason profiled files can be graded exactly.
 * It parses PNG chunks itself, inflates the IDAT stream, reverses
 * the per-row filters (including Adam7 passes), and turns stored
 * samples into RGBA bytes. Embedded profile chunks are ignored for
 * pixel values, matching Pillow's convert("RGBA") hash input: the
 * profile changes how an image is meant to be displayed, not the
 * stored sample values being audited here.
 *
 * Supported exact conversions are the PNG colour types Pillow's
 * RGBA conversion defines byte-for-byte for this pipeline:
 * grayscale (1/2/4/8-bit), truecolour (8/16-bit), palette
 * (1/2/4/8-bit), grayscale+alpha (8/16-bit), and truecolour+alpha
 * (8/16-bit). For 16-bit colour/alpha samples, Pillow's conversion
 * uses the most significant byte. 16-bit grayscale is refused:
 * Pillow opens it as I;16 and its conversion is a range mapping,
 * not the stored RGBA sample conversion this verifier audits.
 * Anything unsupported fails closed instead of being approximated.
 */

var PNG_MAX_PIXELS = 40000000; // matches agenthand's ingest cap

function pngReadU32(bytes, offset) {
  return (
    ((bytes[offset] << 24) |
      (bytes[offset + 1] << 16) |
      (bytes[offset + 2] << 8) |
      bytes[offset + 3]) >>>
    0
  );
}

function pngReadU16(bytes, offset) {
  return ((bytes[offset] << 8) | bytes[offset + 1]) >>> 0;
}

function pngConcatBytes(parts, totalLength) {
  var out = new Uint8Array(totalLength);
  var offset = 0;
  for (var i = 0; i < parts.length; i++) {
    out.set(parts[i], offset);
    offset += parts[i].length;
  }
  return out;
}

function pngParse(buffer, label) {
  var bytes =
    buffer instanceof Uint8Array ? buffer : new Uint8Array(buffer);
  var signature = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
  if (bytes.length < 8) throw new Error(label + " is not a PNG file");
  for (var s = 0; s < signature.length; s++) {
    if (bytes[s] !== signature[s]) {
      throw new Error(label + " is not a PNG file");
    }
  }

  var header = null;
  var palette = null;
  var transparency = null;
  var idatParts = [];
  var idatLength = 0;
  var ended = false;
  var offset = 8;
  var firstChunk = true;

  while (offset + 12 <= bytes.length) {
    var length = pngReadU32(bytes, offset);
    var type = String.fromCharCode(
      bytes[offset + 4],
      bytes[offset + 5],
      bytes[offset + 6],
      bytes[offset + 7]
    );
    var dataStart = offset + 8;
    var dataEnd = dataStart + length;
    if (dataEnd + 4 > bytes.length) {
      throw new Error(label + " has a truncated " + type + " chunk");
    }
    var data = bytes.subarray(dataStart, dataEnd);

    if (firstChunk && type !== "IHDR") {
      throw new Error(label + " does not start with an IHDR chunk");
    }
    firstChunk = false;

    if (type === "IHDR") {
      if (header) throw new Error(label + " has more than one IHDR chunk");
      if (length !== 13) throw new Error(label + " has a malformed IHDR chunk");
      header = {
        width: pngReadU32(data, 0),
        height: pngReadU32(data, 4),
        bitDepth: data[8],
        colorType: data[9],
        compression: data[10],
        filter: data[11],
        interlace: data[12],
      };
    } else if (type === "PLTE") {
      palette = data.slice();
    } else if (type === "tRNS") {
      transparency = data.slice();
    } else if (type === "IDAT") {
      idatParts.push(data.slice());
      idatLength += length;
    } else if (type === "IEND") {
      ended = true;
      break;
    } else if (type.charCodeAt(0) >= 65 && type.charCodeAt(0) <= 90) {
      // An unknown critical chunk changes the image's meaning; do
      // not guess at it. Ancillary chunks (including iCCP) do not
      // change stored sample values and are ignored.
      throw new Error(label + " uses unsupported PNG chunk " + type);
    }

    offset = dataEnd + 4;
  }

  if (!header) throw new Error(label + " has no IHDR chunk");
  if (!ended) throw new Error(label + " has no IEND chunk");
  if (!idatParts.length) throw new Error(label + " has no IDAT chunk");
  if (header.width === 0 || header.height === 0) {
    throw new Error(label + " has a zero dimension");
  }
  if (header.width * header.height > PNG_MAX_PIXELS) {
    throw new Error(label + " exceeds the verifier's 40 megapixel limit");
  }
  if (header.compression !== 0 || header.filter !== 0) {
    throw new Error(label + " uses an unknown PNG compression/filter method");
  }
  if (header.interlace !== 0 && header.interlace !== 1) {
    throw new Error(label + " uses an unknown PNG interlace method");
  }

  var channelsByType = { 0: 1, 2: 3, 3: 1, 4: 2, 6: 4 };
  var depthsByType = {
    0: [1, 2, 4, 8, 16],
    2: [8, 16],
    3: [1, 2, 4, 8],
    4: [8, 16],
    6: [8, 16],
  };
  if (channelsByType[header.colorType] === undefined) {
    throw new Error(label + " uses unknown PNG colour type " + header.colorType);
  }
  if (depthsByType[header.colorType].indexOf(header.bitDepth) === -1) {
    throw new Error(
      label +
        " uses unsupported PNG bit depth " +
        header.bitDepth +
        " for colour type " +
        header.colorType
    );
  }
  header.channels = channelsByType[header.colorType];

  if (header.colorType === 3) {
    if (!palette || palette.length === 0 || palette.length % 3 !== 0) {
      throw new Error(label + " has a missing or malformed PLTE chunk");
    }
  }
  if (transparency) {
    if (header.colorType === 0 && transparency.length !== 2) {
      throw new Error(label + " has a malformed tRNS chunk");
    }
    if (header.colorType === 2 && transparency.length !== 6) {
      throw new Error(label + " has a malformed tRNS chunk");
    }
    if (header.colorType === 3 && transparency.length > palette.length / 3) {
      throw new Error(label + " has a malformed tRNS chunk");
    }
    if (header.colorType === 4 || header.colorType === 6) {
      throw new Error(label + " has a tRNS chunk on an alpha colour type");
    }
  }

  return {
    header: header,
    palette: palette,
    transparency: transparency,
    compressed: pngConcatBytes(idatParts, idatLength),
  };
}

function pngPassSpecs(header) {
  if (header.interlace === 0) {
    return [{ xStart: 0, yStart: 0, xStep: 1, yStep: 1 }];
  }
  // Adam7 pass origins and strides, in transmission order.
  return [
    { xStart: 0, yStart: 0, xStep: 8, yStep: 8 },
    { xStart: 4, yStart: 0, xStep: 8, yStep: 8 },
    { xStart: 0, yStart: 4, xStep: 4, yStep: 8 },
    { xStart: 2, yStart: 0, xStep: 4, yStep: 4 },
    { xStart: 0, yStart: 2, xStep: 2, yStep: 4 },
    { xStart: 1, yStart: 0, xStep: 2, yStep: 2 },
    { xStart: 0, yStart: 1, xStep: 1, yStep: 2 },
  ];
}

function pngPassDimensions(header, pass) {
  return {
    width:
      header.width > pass.xStart
        ? Math.ceil((header.width - pass.xStart) / pass.xStep)
        : 0,
    height:
      header.height > pass.yStart
        ? Math.ceil((header.height - pass.yStart) / pass.yStep)
        : 0,
  };
}

function pngRowBytes(header, pixelCount) {
  return Math.ceil(
    (pixelCount * header.channels * header.bitDepth) / 8
  );
}

function pngExpectedRawLength(header) {
  var passes = pngPassSpecs(header);
  var total = 0;
  for (var i = 0; i < passes.length; i++) {
    var dims = pngPassDimensions(header, passes[i]);
    total += dims.height * (1 + pngRowBytes(header, dims.width));
  }
  return total;
}

function pngAdler32(bytes) {
  var a = 1;
  var b = 0;
  var index = 0;
  while (index < bytes.length) {
    var end = Math.min(index + 5552, bytes.length);
    for (var i = index; i < end; i++) {
      a += bytes[i];
      b += a;
    }
    a %= 65521;
    b %= 65521;
    index = end;
  }
  return (((b << 16) | a) >>> 0);
}

function pngInflate(compressed, expectedLength, label) {
  if (compressed.length < 6) {
    return Promise.reject(new Error(label + " has a truncated zlib stream"));
  }
  var cmf = compressed[0];
  var flg = compressed[1];
  if (((cmf << 8) + flg) % 31 !== 0 || (cmf & 15) !== 8 || (flg & 32) !== 0) {
    return Promise.reject(new Error(label + " has an unsupported zlib header"));
  }
  var expectedAdler = pngReadU32(
    compressed,
    compressed.length - 4
  );

  function checked(raw) {
    if (raw.length !== expectedLength) {
      throw new Error(
        label +
          " inflated to " +
          raw.length +
          " bytes; the PNG header requires " +
          expectedLength
      );
    }
    if (pngAdler32(raw) !== expectedAdler) {
      throw new Error(label + " failed the zlib Adler-32 check");
    }
    return raw;
  }

  if (typeof DecompressionStream !== "undefined") {
    var stream = new Blob([compressed])
      .stream()
      .pipeThrough(new DecompressionStream("deflate"));
    var reader = stream.getReader();
    var parts = [];
    var total = 0;
    function readNext() {
      return reader.read().then(function (result) {
        if (result.done) return checked(pngConcatBytes(parts, total));
        total += result.value.length;
        if (total > expectedLength) {
          throw new Error(label + " inflated beyond the PNG header's size");
        }
        parts.push(result.value);
        return readNext();
      });
    }
    return readNext().catch(function (err) {
      if (err && /^final output|^product layer/.test(err.message || "")) throw err;
      if (err && (err.message || "").indexOf(label) === 0) throw err;
      throw new Error(
        label +
          " could not be inflated (" +
          (err && err.message ? err.message : String(err)) +
          ")"
      );
    });
  }

  if (typeof require === "function") {
    try {
      return Promise.resolve(
        checked(new Uint8Array(require("zlib").inflateSync(compressed)))
      );
    } catch (err) {
      return Promise.reject(
        new Error(
          label +
            " could not be inflated (" +
            (err && err.message ? err.message : String(err)) +
            ")"
        )
      );
    }
  }

  return Promise.reject(
    new Error(
      label +
        " cannot be decoded here: this browser lacks DecompressionStream, so the raw PNG decoder is unavailable"
    )
  );
}

function pngPaeth(a, b, c) {
  var p = a + b - c;
  var pa = Math.abs(p - a);
  var pb = Math.abs(p - b);
  var pc = Math.abs(p - c);
  if (pa <= pb && pa <= pc) return a;
  if (pb <= pc) return b;
  return c;
}

function pngUnfilterPass(raw, offset, passWidth, passHeight, rowBytes, filterBytes, label) {
  var rows = [];
  var previous = null;
  for (var y = 0; y < passHeight; y++) {
    if (offset + 1 + rowBytes > raw.length) {
      throw new Error(label + " has truncated image data");
    }
    var filter = raw[offset];
    offset += 1;
    if (filter > 4) {
      throw new Error(label + " uses unknown PNG row filter " + filter);
    }
    var row = new Uint8Array(rowBytes);
    for (var x = 0; x < rowBytes; x++) {
      var left = x >= filterBytes ? row[x - filterBytes] : 0;
      var up = previous ? previous[x] : 0;
      var upLeft =
        previous && x >= filterBytes ? previous[x - filterBytes] : 0;
      var predictor = 0;
      if (filter === 1) predictor = left;
      else if (filter === 2) predictor = up;
      else if (filter === 3) predictor = Math.floor((left + up) / 2);
      else if (filter === 4) predictor = pngPaeth(left, up, upLeft);
      row[x] = (raw[offset + x] + predictor) & 255;
    }
    offset += rowBytes;
    rows.push(row);
    previous = row;
  }
  return { rows: rows, nextOffset: offset };
}

function pngPassSamples(row, passWidth, header) {
  var samples = new Uint32Array(passWidth * header.channels);
  var i;
  if (header.bitDepth === 8) {
    for (i = 0; i < samples.length; i++) samples[i] = row[i];
  } else if (header.bitDepth === 16) {
    for (i = 0; i < samples.length; i++) {
      samples[i] = (row[i * 2] << 8) | row[i * 2 + 1];
    }
  } else {
    var mask = (1 << header.bitDepth) - 1;
    for (i = 0; i < passWidth; i++) {
      var bitIndex = i * header.bitDepth;
      var shift = 8 - header.bitDepth - (bitIndex & 7);
      samples[i] = (row[bitIndex >> 3] >> shift) & mask;
    }
  }
  return samples;
}

function pngSampleByte(value, bitDepth) {
  return bitDepth === 16 ? (value >> 8) & 255 : value;
}

function pngWritePixel(out, outIndex, samples, sampleIndex, png) {
  var header = png.header;
  var colorType = header.colorType;
  var sample = function (channel) {
    return samples[sampleIndex + channel];
  };

  if (colorType === 0) {
    var rawGray = sample(0);
    var gray =
      header.bitDepth === 8
        ? rawGray
        : Math.round(
            (rawGray * 255) / ((1 << header.bitDepth) - 1)
          );
    out[outIndex] = gray;
    out[outIndex + 1] = gray;
    out[outIndex + 2] = gray;
    out[outIndex + 3] =
      png.transparency && rawGray === pngReadU16(png.transparency, 0)
        ? 0
        : 255;
    return;
  }

  if (colorType === 2) {
    var rawR = sample(0);
    var rawG = sample(1);
    var rawB = sample(2);
    out[outIndex] = pngSampleByte(rawR, header.bitDepth);
    out[outIndex + 1] = pngSampleByte(rawG, header.bitDepth);
    out[outIndex + 2] = pngSampleByte(rawB, header.bitDepth);
    out[outIndex + 3] =
      png.transparency &&
      rawR === pngReadU16(png.transparency, 0) &&
      rawG === pngReadU16(png.transparency, 2) &&
      rawB === pngReadU16(png.transparency, 4)
        ? 0
        : 255;
    return;
  }

  if (colorType === 3) {
    var paletteIndex = sample(0);
    var paletteOffset = paletteIndex * 3;
    if (paletteOffset + 2 >= png.palette.length) {
      throw new Error("PNG palette index " + paletteIndex + " is out of range");
    }
    out[outIndex] = png.palette[paletteOffset];
    out[outIndex + 1] = png.palette[paletteOffset + 1];
    out[outIndex + 2] = png.palette[paletteOffset + 2];
    out[outIndex + 3] =
      png.transparency && paletteIndex < png.transparency.length
        ? png.transparency[paletteIndex]
        : 255;
    return;
  }

  if (colorType === 4) {
    var gray16 = pngSampleByte(sample(0), header.bitDepth);
    out[outIndex] = gray16;
    out[outIndex + 1] = gray16;
    out[outIndex + 2] = gray16;
    out[outIndex + 3] = pngSampleByte(sample(1), header.bitDepth);
    return;
  }

  // Colour type 6: stored RGBA samples.
  out[outIndex] = pngSampleByte(sample(0), header.bitDepth);
  out[outIndex + 1] = pngSampleByte(sample(1), header.bitDepth);
  out[outIndex + 2] = pngSampleByte(sample(2), header.bitDepth);
  out[outIndex + 3] = pngSampleByte(sample(3), header.bitDepth);
}

function decodePngRgba(buffer, label) {
  var png;
  try {
    png = pngParse(buffer, label);
    if (png.header.colorType === 0 && png.header.bitDepth === 16) {
      throw new Error(
        label +
          " is 16-bit grayscale; Pillow opens that as I;16, whose RGBA conversion is not the stored-sample conversion this verifier audits"
      );
    }
  } catch (err) {
    return Promise.reject(err);
  }

  var expectedLength = pngExpectedRawLength(png.header);
  return pngInflate(png.compressed, expectedLength, label).then(function (raw) {
    var header = png.header;
    var out = new Uint8ClampedArray(header.width * header.height * 4);
    var filterBytes = Math.max(
      1,
      Math.ceil((header.channels * header.bitDepth) / 8)
    );
    var offset = 0;
    var passes = pngPassSpecs(header);

    for (var p = 0; p < passes.length; p++) {
      var pass = passes[p];
      var dims = pngPassDimensions(header, pass);
      if (dims.width === 0 || dims.height === 0) continue;
      var rowBytes = pngRowBytes(header, dims.width);
      var decoded = pngUnfilterPass(
        raw,
        offset,
        dims.width,
        dims.height,
        rowBytes,
        filterBytes,
        label
      );
      offset = decoded.nextOffset;

      for (var py = 0; py < dims.height; py++) {
        var samples = pngPassSamples(decoded.rows[py], dims.width, header);
        var y = pass.yStart + py * pass.yStep;
        for (var px = 0; px < dims.width; px++) {
          var x = pass.xStart + px * pass.xStep;
          pngWritePixel(
            out,
            (y * header.width + x) * 4,
            samples,
            px * header.channels,
            png
          );
        }
      }
    }

    if (offset !== raw.length) {
      throw new Error(label + " has extra bytes after its image data");
    }
    return { data: out, width: header.width, height: header.height };
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
   * (proof-output-v1.png)"), so a failure never blames the wrong
   * file. Decoding is the raw PNG decoder above: no canvas, no
   * colour management, and no ICC conversion is applied to the
   * stored sample values. */
  function decodePixels(buffer, label) {
    return decodePngRgba(buffer, label);
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
        // No ICC refusal: decodePixels uses the raw PNG decoder,
        // which ignores iCCP for sample values exactly as Pillow's
        // receipt calculation does. Profiled and unprofiled files
        // take the same grading path.
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

  /* Deep link: ?tamper=1 runs the one-pixel tamper check on load, so
   * the detection case can be linked directly instead of described.
   * Any other query string leaves the page exactly as the buttons
   * alone leave it. */
  try {
    if (new URLSearchParams(window.location.search).get("tamper") === "1") {
      runVerification(true);
    }
  } catch (e) {
    /* No usable query string: the buttons remain the only entry. */
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
    decodePngRgba: decodePngRgba,
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
    decodePngRgba: decodePngRgba,
    tamperedCopy: tamperedCopy,
    flipFirstOpaquePixel: flipFirstOpaquePixel,
    expectationsFromReceipt: expectationsFromReceipt,
    verdictFor: verdictFor,
    onePixelTamperDetected: onePixelTamperDetected,
    pngHasICC: pngHasICC,
  };
}
