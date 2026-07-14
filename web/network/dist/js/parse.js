/**
 * Defensive Nostr event parsing for the network observatory.
 * One malformed / hostile event must never throw into the page.
 */

/**
 * @param {unknown} raw
 * @returns {object | null}
 */
export function parseEvent(raw) {
  try {
    if (!raw || typeof raw !== "object") return null;
    const ev = /** @type {Record<string, unknown>} */ (raw);

    const id = asString(ev.id);
    const pubkey = asString(ev.pubkey);
    const kind = asInt(ev.kind);
    const created_at = asInt(ev.created_at);
    if (!id || !pubkey || kind == null || created_at == null) return null;

    const tags = normalizeTags(ev.tags);
    const content = typeof ev.content === "string" ? ev.content : "";

    const base = {
      id,
      pubkey,
      kind,
      created_at,
      tags,
      content,
      contentJson: tryParseJson(content),
    };

    if (kind === 0) return { ...base, role: "profile", profile: parseProfile(base) };
    if (kind === 5109) return { ...base, role: "offer", offer: parseOffer(base) };
    if (kind === 7000) return { ...base, role: "feedback", feedback: parseFeedback(base) };
    if (kind === 6109) return { ...base, role: "result", result: parseResult(base) };
    if (kind === 3400) return { ...base, role: "receipt", receipt: parseReceipt(base) };
    if (kind === 31990) return { ...base, role: "handler", handler: parseHandler(base) };
    return { ...base, role: "other" };
  } catch {
    return null;
  }
}

/** Max kind-0 content bytes we will attempt to parse (hostile 10MB picture must not blank). */
export const PROFILE_CONTENT_MAX = 64 * 1024;
/** Max picture URL length retained for rendering. */
export const PROFILE_PICTURE_MAX = 2048;

/**
 * Defensive NIP-01 kind-0 metadata parse.
 * @param {{ content: string, created_at: number }} base
 */
export function parseProfile(base) {
  const empty = {
    name: null,
    display_name: null,
    picture: null,
    about: null,
  };
  try {
    let raw = typeof base.content === "string" ? base.content : "";
    if (raw.length > PROFILE_CONTENT_MAX) {
      raw = raw.slice(0, PROFILE_CONTENT_MAX);
    }
    const obj = tryParseJson(raw);
    if (!obj || typeof obj !== "object") return empty;

    const name = clampStr(obj.name, 128);
    const display_name = clampStr(obj.display_name ?? obj.displayName, 128);
    let picture = clampStr(obj.picture, PROFILE_PICTURE_MAX);
    if (picture && !isSafePictureUrl(picture)) picture = null;
    const about = clampStr(obj.about, 512);

    return { name, display_name, picture, about };
  } catch {
    return empty;
  }
}

function clampStr(v, max) {
  if (typeof v !== "string") return null;
  const t = v.trim();
  if (!t) return null;
  return t.length > max ? t.slice(0, max) : t;
}

function isSafePictureUrl(url) {
  try {
    const u = new URL(url);
    return u.protocol === "https:" || u.protocol === "http:";
  } catch {
    return false;
  }
}

/**
 * Extract usage adjunct fields using the locked vocabulary.
 * Missing fields stay null — never invent totals by summing siblings.
 * @param {unknown} contentJson
 * @param {string[][]} tags
 */
export function extractUsageAdjunct(contentJson, tags = []) {
  try {
    const root = asObject(contentJson) || {};
    const adjunct =
      asObject(root.usage_adjunct) ||
      asObject(root.completion_usage_adjunct) ||
      asObject(root.usage) ||
      root;

    const measure =
      asObject(adjunct.usage_measure) ||
      asObject(root.usage_measure) ||
      null;

    const total_tokens = measure
      ? asNumberOrNull(measure.total_tokens)
      : asNumberOrNull(adjunct.total_tokens);

    return {
      total_tokens,
      measured_cost_tokens: asNumberOrNull(
        adjunct.measured_cost_tokens ?? root.measured_cost_tokens,
      ),
      paid_price_tokens: asNumberOrNull(
        adjunct.paid_price_tokens ?? root.paid_price_tokens,
      ),
      usage_transport: asEnumString(
        adjunct.usage_transport ?? root.usage_transport,
        ["acp-native", "side-channel"],
      ),
      harness_family: asEnumString(
        adjunct.harness_family ?? root.harness_family,
        ["codex", "claude", "cursor", "other"],
      ),
      paid_price_sats: amountSatsFromTags(tags),
    };
  } catch {
    return emptyUsage();
  }
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

function parseOffer(base) {
  return {
    task: firstTagValue(base.tags, "i"),
    amount_sats: amountSatsFromTags(base.tags),
    mint: firstTagValue(base.tags, "mint"),
    seller: firstTagValue(base.tags, "p"),
  };
}

function parseFeedback(base) {
  const status = firstTagValue(base.tags, "status");
  const offerId = firstETag(base.tags, "root") || firstETag(base.tags, null);
  return {
    status,
    isClaim: status === "processing",
    isAccept: status === "accepted",
    offerId,
  };
}

function parseResult(base) {
  return {
    offerId: firstETag(base.tags, "root") || firstETag(base.tags, null),
    amount_sats: amountSatsFromTags(base.tags),
  };
}

function parseReceipt(base) {
  const usage = extractUsageAdjunct(base.contentJson, base.tags);
  return {
    offerId: firstETag(base.tags, "root") || firstETag(base.tags, null),
    resultId: firstETag(base.tags, "reply"),
    amount_sats: amountSatsFromTags(base.tags),
    mint: firstTagValue(base.tags, "mint"),
    usage,
  };
}

function parseHandler(base) {
  const j = asObject(base.contentJson) || {};
  const harness_name =
    asString(j.harness_name) ||
    asString(j.name) ||
    asString(j.display_name) ||
    null;
  const version =
    asString(j.version) ||
    asString(j.harness_version) ||
    firstTagValue(base.tags, "version") ||
    null;
  return {
    harness_name,
    version,
    d: firstTagValue(base.tags, "d"),
    k: allTagValues(base.tags, "k"),
  };
}

function normalizeTags(tags) {
  if (!Array.isArray(tags)) return [];
  const out = [];
  for (const tag of tags) {
    if (!Array.isArray(tag) || tag.length === 0) continue;
    const row = [];
    let ok = true;
    for (const cell of tag) {
      if (typeof cell !== "string") {
        ok = false;
        break;
      }
      row.push(cell);
    }
    if (ok && row.length) out.push(row);
  }
  return out;
}

export function firstTagValue(tags, name) {
  for (const tag of tags) {
    if (tag[0] === name && tag[1]) return tag[1];
  }
  return null;
}

export function allTagValues(tags, name) {
  const vals = [];
  for (const tag of tags) {
    if (tag[0] === name && tag[1]) vals.push(tag[1]);
  }
  return vals;
}

/** Prefer marker (root/reply); else first e tag. */
export function firstETag(tags, marker) {
  if (marker) {
    for (const tag of tags) {
      if (tag[0] === "e" && tag[1] && tag[3] === marker) return tag[1];
    }
  }
  for (const tag of tags) {
    if (tag[0] === "e" && tag[1]) return tag[1];
  }
  return null;
}

export function amountSatsFromTags(tags) {
  for (const tag of tags) {
    if (tag[0] === "amount" && tag[1]) {
      const n = Number(tag[1]);
      if (Number.isFinite(n)) return n;
    }
  }
  return null;
}

function tryParseJson(text) {
  if (!text || typeof text !== "string") return null;
  const t = text.trim();
  if (!t || (t[0] !== "{" && t[0] !== "[")) return null;
  try {
    return JSON.parse(t);
  } catch {
    return null;
  }
}

function asObject(v) {
  return v && typeof v === "object" && !Array.isArray(v) ? v : null;
}

function asString(v) {
  return typeof v === "string" && v.length ? v : null;
}

function asInt(v) {
  if (typeof v === "number" && Number.isFinite(v)) return Math.trunc(v);
  if (typeof v === "string" && v.trim() !== "") {
    const n = Number(v);
    if (Number.isFinite(n)) return Math.trunc(n);
  }
  return null;
}

function asNumberOrNull(v) {
  if (v == null) return null;
  if (typeof v === "number" && Number.isFinite(v)) return v;
  if (typeof v === "string" && v.trim() !== "") {
    const n = Number(v);
    if (Number.isFinite(n)) return n;
  }
  return null;
}

function asEnumString(v, allowed) {
  const s = asString(v);
  if (!s) return null;
  return allowed.includes(s) ? s : s; // keep unknown strings visible; don't blank
}

/**
 * Percentile of a numeric array (sorted copy). Empty → null.
 * @param {number[]} values
 * @param {number} p 0..100
 */
export function percentile(values, p) {
  if (!values.length) return null;
  const sorted = [...values].sort((a, b) => a - b);
  if (sorted.length === 1) return sorted[0];
  const rank = (p / 100) * (sorted.length - 1);
  const lo = Math.floor(rank);
  const hi = Math.ceil(rank);
  if (lo === hi) return sorted[lo];
  const w = rank - lo;
  return sorted[lo] * (1 - w) + sorted[hi] * w;
}
