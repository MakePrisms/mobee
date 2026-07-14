import { parseEvent } from "./parse.js";
import { createStore } from "./store.js";
import { createRelayClient } from "./relay.js";
import { renderAll, renderConnection } from "./views.js";

const store = createStore();
const viewsRoot = document.getElementById("views");
const connNode = document.getElementById("conn");
const retryBtn = document.getElementById("retry");

let conn = {
  state: "connecting",
  detail: "",
  url: "",
  attempt: 0,
};
let raf = 0;

function scheduleRender() {
  if (raf) return;
  raf = requestAnimationFrame(() => {
    raf = 0;
    try {
      renderConnection(connNode, conn);
      renderAll(viewsRoot, store.snapshot(), conn);
    } catch (err) {
      console.error("render failed", err);
    }
  });
}

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
