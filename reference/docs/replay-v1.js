/* agenthand — cached replay controls and receipt loader.
 *
 * This page is a presentation of a frozen recording. The script
 * never contacts the MCP server or a model API. It loads only the
 * versioned replay data and the published receipt beside it, checks
 * that the data explicitly says it is not live, and lets a reader
 * step through the three recorded calls.
 */
"use strict";

var REPLAY_DATA_URL = "replay-run-v1.json";
var REPLAY_RECEIPT_URL = "proof-receipt-v1.json";

function summarizeReplay(data) {
  if (!data || data.is_live !== false) {
    throw new Error("replay data does not carry the required not-live marker");
  }
  if (data.presentation !== "REPLAY OF A REAL RUN") {
    throw new Error("replay data does not carry the required replay label");
  }
  if (!Array.isArray(data.calls) || data.calls.length !== 3) {
    throw new Error("replay data must contain exactly three recorded calls");
  }
  var total = data.calls.reduce(function (sum, call) {
    return sum + call.settled_cost_usd;
  }, 0);
  if (Math.abs(total - data.run.total_settled_cost_usd) > 0.0000005) {
    throw new Error("replay call costs do not add to the recorded total");
  }
  return {
    callCount: data.calls.length,
    tools: data.calls.map(function (call) { return call.tool; }),
    totalSettledCostUsd: total,
    isLive: false,
  };
}

function replayStepLabel(data, index) {
  var call = data.calls[index];
  return (
    "[ REPLAY · STEP " + (index + 1) + " OF " + data.calls.length +
    " · " + call.tool.toUpperCase() + " ]"
  );
}

function initReplay() {
  var readout = document.getElementById("replay-readout");
  var receiptEl = document.getElementById("replay-receipt");
  var statusEl = document.getElementById("replay-data-status");
  var playBtn = document.getElementById("replay-play");
  var stepBtns = document.querySelectorAll("[data-replay-step]");
  var cards = document.querySelectorAll("[data-replay-card]");
  var data = null;
  var current = 0;
  var timer = null;
  var reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  function stopPlayback() {
    if (timer !== null) {
      window.clearInterval(timer);
      timer = null;
    }
    if (playBtn) playBtn.textContent = "Play the recording";
  }

  function selectStep(index, scroll) {
    current = Math.max(0, Math.min(index, cards.length - 1));
    for (var i = 0; i < cards.length; i++) {
      cards[i].classList.toggle("active", i === current);
    }
    for (var j = 0; j < stepBtns.length; j++) {
      var active = Number(stepBtns[j].getAttribute("data-replay-step")) === current;
      stepBtns[j].classList.toggle("active", active);
      stepBtns[j].setAttribute("aria-pressed", active ? "true" : "false");
    }
    if (data && readout) readout.textContent = replayStepLabel(data, current);
    if (scroll && cards[current]) {
      cards[current].scrollIntoView({
        behavior: reduced ? "auto" : "smooth",
        block: "start",
      });
    }
  }

  for (var i = 0; i < stepBtns.length; i++) {
    stepBtns[i].addEventListener("click", function (event) {
      stopPlayback();
      selectStep(Number(event.currentTarget.getAttribute("data-replay-step")), true);
    });
  }

  if (playBtn) {
    playBtn.addEventListener("click", function () {
      if (timer !== null) {
        stopPlayback();
        return;
      }
      if (reduced) {
        selectStep((current + 1) % cards.length, true);
        return;
      }
      if (current >= cards.length - 1) selectStep(0, true);
      playBtn.textContent = "Pause the recording";
      timer = window.setInterval(function () {
        if (current >= cards.length - 1) {
          stopPlayback();
          return;
        }
        selectStep(current + 1, true);
      }, 2600);
    });
  }

  Promise.all([
    fetch(REPLAY_DATA_URL).then(function (res) {
      if (!res.ok) throw new Error("replay data HTTP " + res.status);
      return res.json();
    }),
    fetch(REPLAY_RECEIPT_URL).then(function (res) {
      if (!res.ok) throw new Error("receipt HTTP " + res.status);
      return res.json();
    }),
  ])
    .then(function (results) {
      data = results[0];
      var summary = summarizeReplay(data);
      var receipt = results[1];
      if (receipt.opaque_pixels !== data.receipt.opaque_pixels ||
          receipt.changed_opaque_pixels !== data.receipt.changed_opaque_pixels) {
        throw new Error("replay receipt summary does not match the published receipt");
      }
      if (statusEl) {
        statusEl.textContent =
          "Replay data checked: " + summary.callCount + " recorded calls (" +
          summary.tools.join(", ") + "), total $" +
          summary.totalSettledCostUsd.toFixed(6) + ". No live call was made.";
      }
      if (receiptEl) receiptEl.textContent = JSON.stringify(receipt, null, 2);
      selectStep(0, false);
    })
    .catch(function (err) {
      if (statusEl) statusEl.textContent = "Replay data could not be checked: " + err.message;
      if (receiptEl) {
        receiptEl.textContent =
          "The receipt JSON could not be loaded by the replay script. Use the raw receipt link above.";
      }
    });
}

if (typeof document !== "undefined") {
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", initReplay);
  } else {
    initReplay();
  }
}

if (typeof window !== "undefined") {
  window.AgenthandReplay = {
    summarizeReplay: summarizeReplay,
    replayStepLabel: replayStepLabel,
  };
}

if (typeof module !== "undefined" && module.exports) {
  module.exports = {
    summarizeReplay: summarizeReplay,
    replayStepLabel: replayStepLabel,
  };
}
