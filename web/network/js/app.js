import { parseEvent } from "./parse.js";
import { createStore } from "./store.js";
import { createRelayClient } from "./relay.js";
import { renderConnection, renderFeed, renderPulse, renderStats } from "./views.js";

const store = createStore();
const pulseNode = document.getElementById("pulse");
const feedNode = document.getElementById("feed");
const statsNode = document.getElementById("stats");
const connNode = document.getElementById("conn");
const retryBtn = document.getElementById("retry");
const tabFeed = document.getElementById("tab-feed");
const tabStats = document.getElementById("tab-stats");

/** Which tab is showing — feed is the landing view. */
let activeTab = "feed";
/** Job ids the user has expanded; preserved across live re-renders. */
const expanded = new Set();

let conn = { state: "connecting", detail: "", url: "", attempt: 0 };
let raf = 0;

function scheduleRender() {
  if (raf) return;
  raf = requestAnimationFrame(() => {
    raf = 0;
    try {
      const now = Math.floor(Date.now() / 1000);
      const snap = store.snapshot(now);
      renderConnection(connNode, conn);
      renderPulse(pulseNode, snap.pulse);
      if (activeTab === "feed") {
        renderFeed(feedNode, snap.jobs, expanded, toggleJob, now);
      } else {
        renderStats(statsNode, snap);
      }
    } catch (err) {
      console.error("render failed", err);
    }
  });
}

function toggleJob(id) {
  if (expanded.has(id)) expanded.delete(id);
  else expanded.add(id);
  scheduleRender();
}

function setTab(tab) {
  activeTab = tab;
  const feedSel = tab === "feed";
  tabFeed.setAttribute("aria-selected", String(feedSel));
  tabStats.setAttribute("aria-selected", String(!feedSel));
  feedNode.classList.toggle("hidden", !feedSel);
  statsNode.classList.toggle("hidden", feedSel);
  scheduleRender();
}

tabFeed.addEventListener("click", () => setTab("feed"));
tabStats.addEventListener("click", () => setTab("stats"));

const client = createRelayClient({
  onEvent(raw) {
    const normalized = parseEvent(raw);
    const { newAuthor } = store.ingest(normalized);
    if (normalized?.role === "profile") {
      client.markProfileDone(normalized.pubkey);
    } else if (newAuthor) {
      client.requestProfiles([newAuthor]);
    }
    scheduleRender();
  },
  onStatus(s) {
    conn = s;
    scheduleRender();
  },
});

retryBtn.addEventListener("click", () => {
  client.disconnect();
  client.connect();
});

client.connect();
scheduleRender();
