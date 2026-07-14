import { KIND_LABELS } from "../config.js";

export function renderAll(root, snap, conn) {
  root.innerHTML = "";
  root.append(
    el("section", { class: "view", id: "funnel" }, [
      h2("1 · Funnel"),
      p("Offers → claims → results → receipts. Leaks are the product signal."),
      renderFunnel(snap.funnel),
    ]),
    el("section", { class: "view", id: "latency" }, [
      h2("2 · Latency"),
      p("Time-to-claim and time-to-result (seconds)."),
      renderLatency(snap.latency),
    ]),
    el("section", { class: "view", id: "economics" }, [
      h2("3 · Economics"),
      p(
        "From receipts / usage adjunct. measured_cost_tokens may be absent — view degrades, never blanks.",
      ),
      renderEconomics(snap.economics),
    ]),
    el("section", { class: "view", id: "census" }, [
      h2("4 · Seller census"),
      p("NIP-89 (31990) handler announces — harness_name + version."),
      renderCensus(snap.census),
    ]),
    el("section", { class: "view", id: "tail" }, [
      h2("5 · Raw live tail"),
      p("Newest events, kind-labeled. Expand a row for JSON."),
      renderTail(snap.tail),
    ]),
  );

  // connection banner lives outside views (app updates it)
  void conn;
}

export function renderConnection(node, conn) {
  const label =
    conn.state === "connected"
      ? "connected"
      : conn.state === "connecting"
        ? "connecting…"
        : conn.state === "reconnecting"
          ? "reconnecting…"
          : "disconnected";
  node.dataset.state = conn.state;
  node.textContent = `${label} · ${conn.url}${conn.detail ? " · " + conn.detail : ""}`;
}

function renderFunnel(f) {
  const leaks = f.leaks || { unclaimed: 0, unresulted: 0, unpaid: 0 };
  return el("div", { class: "funnel" }, [
    el("div", { class: "funnel-steps" }, [
      metric("offers", f.offers),
      arrow(),
      metric("claimed", f.claimed),
      arrow(),
      metric("resulted", f.resulted),
      arrow(),
      metric("receipted", f.receipted),
    ]),
    el("div", { class: "leaks" }, [
      leak("unclaimed offers", leaks.unclaimed),
      leak("unresulted claims", leaks.unresulted),
      leak("unpaid results", leaks.unpaid),
    ]),
    el("p", { class: "meta" }, [
      text(
        `${f.events} events ingested · ${f.parseSkips} parse skips (malformed dropped)`,
      ),
    ]),
  ]);
}

function renderLatency(lat) {
  return el("div", { class: "latency" }, [
    latBlock("time-to-claim", lat.toClaim),
    latBlock("time-to-result", lat.toResult),
  ]);
}

function latBlock(title, s) {
  return el("div", { class: "lat-block" }, [
    el("h3", {}, [text(title)]),
    el("dl", { class: "stats" }, [
      stat("n", s.n),
      stat("p50", fmtSec(s.p50)),
      stat("p90", fmtSec(s.p90)),
      stat("p99", fmtSec(s.p99)),
      stat("min", fmtSec(s.min)),
      stat("max", fmtSec(s.max)),
    ]),
  ]);
}

function renderEconomics(eco) {
  const groupRows = eco.groups.map(([key, g]) => {
    const [family, transport] = key.split("|");
    return el("tr", {}, [
      td(family),
      td(transport),
      td(String(g.n)),
      td(g.withCost ? avg(g.sumCost, g.withCost) : "—"),
      td(g.sumPaidTok ? avg(g.sumPaidTok, g.n) : "—"),
      td(g.sumPaidSat ? avg(g.sumPaidSat, g.n) : "—"),
    ]);
  });

  const detailRows = eco.rows.slice(0, 40).map((r) =>
    el("tr", {}, [
      td(shortId(r.id)),
      td(r.harness_family || "—"),
      td(r.usage_transport || "—"),
      td(fmtNum(r.total_tokens)),
      td(fmtNum(r.measured_cost_tokens)),
      td(fmtNum(r.paid_price_tokens)),
      td(fmtNum(r.paid_price_sats)),
    ]),
  );

  return el("div", { class: "economics" }, [
    el("h3", {}, [text("by harness_family × usage_transport")]),
    el("div", { class: "table-wrap" }, [
      el("table", {}, [
        el("thead", {}, [
          el("tr", {}, [
            th("harness_family"),
            th("usage_transport"),
            th("n"),
            th("avg measured_cost_tokens"),
            th("avg paid_price_tokens"),
            th("avg paid sats"),
          ]),
        ]),
        el("tbody", {}, groupRows.length ? groupRows : [emptyRow(6)]),
      ]),
    ]),
    el("h3", {}, [text("recent receipts")]),
    el("div", { class: "table-wrap" }, [
      el("table", {}, [
        el("thead", {}, [
          el("tr", {}, [
            th("id"),
            th("harness_family"),
            th("usage_transport"),
            th("total_tokens"),
            th("measured_cost_tokens"),
            th("paid_price_tokens"),
            th("paid sats"),
          ]),
        ]),
        el("tbody", {}, detailRows.length ? detailRows : [emptyRow(7)]),
      ]),
    ]),
  ]);
}

function renderCensus(rows) {
  const body = rows.map((r) =>
    el("tr", {}, [
      td(r.harness_name),
      td(r.version),
      td(shortId(r.pubkey)),
      td(r.k.join(", ") || "—"),
      td(fmtTime(r.created_at)),
    ]),
  );
  return el("div", { class: "table-wrap" }, [
    el("table", {}, [
      el("thead", {}, [
        el("tr", {}, [
          th("harness_name"),
          th("version"),
          th("pubkey"),
          th("k"),
          th("seen"),
        ]),
      ]),
      el("tbody", {}, body.length ? body : [emptyRow(5)]),
    ]),
  ]);
}

function renderTail(events) {
  const list = el("div", { class: "tail" }, []);
  for (const ev of events) {
    const summary = el("button", { class: "tail-row", type: "button" }, [
      el("span", { class: "kind" }, [text(KIND_LABELS[ev.kind] || String(ev.kind))]),
      el("span", { class: "id" }, [text(shortId(ev.id))]),
      el("span", { class: "pk" }, [text(shortId(ev.pubkey))]),
      el("span", { class: "ts" }, [text(fmtTime(ev.created_at))]),
    ]);
    const pre = el("pre", { class: "tail-json hidden" }, [
      text(
        JSON.stringify(
          {
            id: ev.id,
            pubkey: ev.pubkey,
            kind: ev.kind,
            created_at: ev.created_at,
            tags: ev.tags,
            content: ev.content,
          },
          null,
          2,
        ),
      ),
    ]);
    summary.addEventListener("click", () => {
      pre.classList.toggle("hidden");
    });
    list.append(el("div", { class: "tail-item" }, [summary, pre]));
  }
  if (!events.length) {
    list.append(el("p", { class: "meta" }, [text("No events yet.")]));
  }
  return list;
}

/* ——— DOM helpers ——— */

function el(tag, attrs = {}, children = []) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") node.className = v;
    else if (k === "dataset") Object.assign(node.dataset, v);
    else node.setAttribute(k, v);
  }
  for (const child of children) {
    if (child != null) node.append(child);
  }
  return node;
}

function text(s) {
  return document.createTextNode(String(s));
}
function h2(s) {
  return el("h2", {}, [text(s)]);
}
function p(s) {
  return el("p", { class: "lede" }, [text(s)]);
}
function metric(label, value) {
  return el("div", { class: "metric" }, [
    el("div", { class: "metric-value" }, [text(String(value))]),
    el("div", { class: "metric-label" }, [text(label)]),
  ]);
}
function arrow() {
  return el("div", { class: "arrow" }, [text("→")]);
}
function leak(label, value) {
  return el("div", { class: value > 0 ? "leak loud" : "leak" }, [
    el("span", { class: "leak-value" }, [text(String(value))]),
    el("span", { class: "leak-label" }, [text(label)]),
  ]);
}
function stat(k, v) {
  return el("div", { class: "stat" }, [
    el("dt", {}, [text(k)]),
    el("dd", {}, [text(v == null ? "—" : String(v))]),
  ]);
}
function th(s) {
  return el("th", {}, [text(s)]);
}
function td(s) {
  return el("td", {}, [text(s)]);
}
function emptyRow(cols) {
  return el("tr", {}, [
    el("td", { colspan: String(cols), class: "empty" }, [text("no data yet")]),
  ]);
}

function fmtSec(v) {
  if (v == null) return "—";
  return `${v.toFixed(1)}s`;
}
function fmtNum(v) {
  if (v == null) return "—";
  return String(v);
}
function avg(sum, n) {
  if (!n) return "—";
  return (sum / n).toFixed(1);
}
function shortId(id) {
  if (!id) return "—";
  return id.length > 12 ? `${id.slice(0, 8)}…` : id;
}
function fmtTime(ts) {
  if (ts == null) return "—";
  try {
    return new Date(ts * 1000).toISOString().replace("T", " ").replace(/\.\d+Z$/, "Z");
  } catch {
    return String(ts);
  }
}
