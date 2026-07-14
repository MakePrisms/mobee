import { percentile } from "./parse.js";

/** In-memory aggregator for observatory views. */
export function createStore() {
  /** @type {Map<string, any>} */
  const byId = new Map();
  /** @type {Map<string, any>} */
  const offers = new Map();
  /** @type {Map<string, any[]>} */
  const claimsByOffer = new Map();
  /** @type {Map<string, any[]>} */
  const resultsByOffer = new Map();
  /** @type {Map<string, any[]>} */
  const receiptsByOffer = new Map();
  /** @type {Map<string, any>} */
  const handlers = new Map();
  /** @type {any[]} */
  const liveTail = [];
  let parseSkips = 0;

  function ingest(normalized) {
    if (!normalized) {
      parseSkips += 1;
      return false;
    }
    if (byId.has(normalized.id)) return false;
    byId.set(normalized.id, normalized);
    liveTail.unshift(normalized);
    if (liveTail.length > 200) liveTail.length = 200;

    switch (normalized.role) {
      case "offer":
        // Funnel only counts plausible marketplace offers; junk 5109s stay in the tail.
        if (isPlausibleOffer(normalized)) {
          offers.set(normalized.id, normalized);
        }
        break;
      case "feedback": {
        const fb = normalized.feedback;
        if (fb?.isClaim && fb.offerId) {
          pushMap(claimsByOffer, fb.offerId, normalized);
        }
        break;
      }
      case "result": {
        const offerId = normalized.result?.offerId;
        if (offerId) pushMap(resultsByOffer, offerId, normalized);
        break;
      }
      case "receipt": {
        const offerId = normalized.receipt?.offerId;
        if (offerId) pushMap(receiptsByOffer, offerId, normalized);
        break;
      }
      case "handler": {
        // d-tag or id as handler identity
        const key = normalized.handler?.d || normalized.id;
        const prev = handlers.get(key);
        if (!prev || prev.created_at <= normalized.created_at) {
          handlers.set(key, normalized);
        }
        break;
      }
      default:
        break;
    }
    return true;
  }

  function funnel() {
    let unclaimed = 0;
    let unresulted = 0;
    let unpaid = 0;
    let claimed = 0;
    let resulted = 0;
    let receipted = 0;

    for (const offerId of offers.keys()) {
      const claims = claimsByOffer.get(offerId) || [];
      const results = resultsByOffer.get(offerId) || [];
      const receipts = receiptsByOffer.get(offerId) || [];
      if (claims.length === 0) unclaimed += 1;
      else claimed += 1;
      if (claims.length > 0 && results.length === 0) unresulted += 1;
      if (results.length > 0) resulted += 1;
      if (results.length > 0 && receipts.length === 0) unpaid += 1;
      if (receipts.length > 0) receipted += 1;
    }

    return {
      offers: offers.size,
      claimed,
      resulted,
      receipted,
      leaks: { unclaimed, unresulted, unpaid },
      parseSkips,
      events: byId.size,
    };
  }

  function latency() {
    const toClaim = [];
    const toResult = [];

    for (const [offerId, offer] of offers) {
      const claims = claimsByOffer.get(offerId) || [];
      if (!claims.length) continue;
      const firstClaim = minBy(claims, (c) => c.created_at);
      if (firstClaim) {
        const dt = firstClaim.created_at - offer.created_at;
        if (dt >= 0) toClaim.push(dt);
      }
      const results = resultsByOffer.get(offerId) || [];
      if (!results.length || !firstClaim) continue;
      const firstResult = minBy(results, (r) => r.created_at);
      if (firstResult) {
        const dt = firstResult.created_at - firstClaim.created_at;
        if (dt >= 0) toResult.push(dt);
      }
    }

    return {
      toClaim: summarize(toClaim),
      toResult: summarize(toResult),
      samples: { toClaim: toClaim.length, toResult: toResult.length },
    };
  }

  function economics() {
    /** @type {any[]} */
    const rows = [];
    for (const list of receiptsByOffer.values()) {
      for (const ev of list) {
        const u = ev.receipt?.usage || emptyUsage();
        rows.push({
          id: ev.id,
          created_at: ev.created_at,
          paid_price_sats: u.paid_price_sats ?? ev.receipt?.amount_sats ?? null,
          paid_price_tokens: u.paid_price_tokens,
          measured_cost_tokens: u.measured_cost_tokens,
          total_tokens: u.total_tokens,
          usage_transport: u.usage_transport,
          harness_family: u.harness_family,
        });
      }
    }
    rows.sort((a, b) => b.created_at - a.created_at);

    /** @type {Map<string, {n:number, withCost:number, sumCost:number, sumPaidTok:number, sumPaidSat:number}>} */
    const groups = new Map();
    for (const row of rows) {
      const key = `${row.harness_family || "unknown"}|${row.usage_transport || "unknown"}`;
      let g = groups.get(key);
      if (!g) {
        g = { n: 0, withCost: 0, sumCost: 0, sumPaidTok: 0, sumPaidSat: 0 };
        groups.set(key, g);
      }
      g.n += 1;
      if (row.measured_cost_tokens != null) {
        g.withCost += 1;
        g.sumCost += row.measured_cost_tokens;
      }
      if (row.paid_price_tokens != null) g.sumPaidTok += row.paid_price_tokens;
      if (row.paid_price_sats != null) g.sumPaidSat += row.paid_price_sats;
    }

    return { rows: rows.slice(0, 100), groups: [...groups.entries()] };
  }

  function census() {
    const list = [...handlers.values()].sort((a, b) => b.created_at - a.created_at);
    return list.map((ev) => ({
      id: ev.id,
      pubkey: ev.pubkey,
      created_at: ev.created_at,
      harness_name: ev.handler?.harness_name || "(unnamed)",
      version: ev.handler?.version || "—",
      d: ev.handler?.d,
      k: ev.handler?.k || [],
    }));
  }

  function tail(n = 50) {
    return liveTail.slice(0, n);
  }

  function snapshot() {
    return {
      funnel: funnel(),
      latency: latency(),
      economics: economics(),
      census: census(),
      tail: tail(50),
    };
  }

  return { ingest, snapshot, funnel, latency, economics, census, tail };
}

function pushMap(map, key, value) {
  let arr = map.get(key);
  if (!arr) {
    arr = [];
    map.set(key, arr);
  }
  arr.push(value);
}

function minBy(arr, fn) {
  let best = null;
  let bestV = Infinity;
  for (const item of arr) {
    const v = fn(item);
    if (v < bestV) {
      bestV = v;
      best = item;
    }
  }
  return best;
}

function summarize(values) {
  return {
    n: values.length,
    p50: percentile(values, 50),
    p90: percentile(values, 90),
    p99: percentile(values, 99),
    min: values.length ? Math.min(...values) : null,
    max: values.length ? Math.max(...values) : null,
  };
}

function emptyUsage() {
  return {
    total_tokens: null,
    measured_cost_tokens: null,
    paid_price_tokens: null,
    usage_transport: null,
    harness_family: null,
    paid_price_sats: null,
  };
}

function isPlausibleOffer(ev) {
  const tags = ev.tags || [];
  const hasMobee = tags.some((t) => t[0] === "t" && t[1] === "mobee");
  const hasAmount = tags.some((t) => t[0] === "amount" && t[1]);
  return hasMobee || hasAmount;
}
