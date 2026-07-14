import { HISTORY_LIMIT, RELAY_URL, SUBSCRIBE_KINDS } from "../config.js";

/**
 * Minimal NIP-01 client with reconnect + visible connection state.
 * @param {{ onEvent: (ev: unknown) => void, onStatus: (s: ConnectionStatus) => void }} hooks
 */
export function createRelayClient(hooks) {
  let ws = null;
  let intentionalClose = false;
  let attempt = 0;
  let retryTimer = null;
  let subId = "mobee-network-1";

  function status(state, detail = "") {
    hooks.onStatus({ state, detail, url: RELAY_URL, attempt });
  }

  function connect() {
    intentionalClose = false;
    clearRetry();
    status(attempt === 0 ? "connecting" : "reconnecting", `attempt ${attempt + 1}`);

    let socket;
    try {
      socket = new WebSocket(RELAY_URL);
    } catch (err) {
      status("disconnected", String(err?.message || err));
      scheduleRetry();
      return;
    }
    ws = socket;

    socket.onopen = () => {
      attempt = 0;
      status("connected");
      const filter = { kinds: [...SUBSCRIBE_KINDS], limit: HISTORY_LIMIT };
      safeSend(socket, ["REQ", subId, filter]);
    };

    socket.onmessage = (msg) => {
      let data;
      try {
        data = JSON.parse(msg.data);
      } catch {
        return; // ignore junk frames
      }
      if (!Array.isArray(data) || data.length < 2) return;
      const type = data[0];
      if (type === "EVENT" && data[2]) {
        try {
          hooks.onEvent(data[2]);
        } catch {
          // never let a bad handler tear down the socket loop
        }
      } else if (type === "NOTICE") {
        status("connected", String(data[1] || ""));
      } else if (type === "CLOSED" && data[1] === subId) {
        status("disconnected", String(data[2] || "CLOSED"));
        scheduleRetry();
      }
    };

    socket.onerror = () => {
      status("disconnected", "socket error");
    };

    socket.onclose = () => {
      ws = null;
      if (!intentionalClose) {
        status("disconnected", "socket closed");
        scheduleRetry();
      } else {
        status("disconnected", "closed");
      }
    };
  }

  function scheduleRetry() {
    clearRetry();
    attempt += 1;
    const delay = Math.min(30000, 1000 * 2 ** Math.min(attempt - 1, 4));
    status("reconnecting", `retry in ${Math.round(delay / 1000)}s`);
    retryTimer = setTimeout(connect, delay);
  }

  function clearRetry() {
    if (retryTimer) {
      clearTimeout(retryTimer);
      retryTimer = null;
    }
  }

  function disconnect() {
    intentionalClose = true;
    clearRetry();
    if (ws) {
      try {
        safeSend(ws, ["CLOSE", subId]);
      } catch {
        /* ignore */
      }
      try {
        ws.close();
      } catch {
        /* ignore */
      }
      ws = null;
    }
    status("disconnected", "manual");
  }

  return { connect, disconnect, getUrl: () => RELAY_URL };
}

function safeSend(ws, payload) {
  if (ws.readyState === WebSocket.OPEN) {
    ws.send(JSON.stringify(payload));
  }
}

/**
 * @typedef {{ state: 'connecting'|'connected'|'reconnecting'|'disconnected', detail: string, url: string, attempt: number }} ConnectionStatus
 */
