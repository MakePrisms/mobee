import assert from "node:assert/strict";
import { createStore } from "../js/store.js";
import {
  extractUsageAdjunct,
  parseEvent,
  percentile,
} from "../js/parse.js";

function ok(ev) {
  const n = parseEvent(ev);
  assert.ok(n, "expected parse success");
  return n;
}

// ——— defensive parse: hostile / malformed must not throw or blank store ———

const garbage = [
  null,
  undefined,
  "",
  42,
  [],
  {},
  { id: "x" },
  { id: "a".repeat(64), pubkey: "b".repeat(64), kind: "nope", created_at: 1 },
  {
    id: "c".repeat(64),
    pubkey: "d".repeat(64),
    kind: 5109,
    created_at: 1,
    tags: "not-array",
    content: null,
  },
  {
    id: "e".repeat(64),
    pubkey: "f".repeat(64),
    kind: 3400,
    created_at: 1,
    tags: [["amount", "3", "sat"], ["e", "offer1", "", "root"]],
    content: "{not json",
  },
  {
    id: "g".repeat(64),
    pubkey: "h".repeat(64),
    kind: 3400,
    created_at: 1,
    tags: [null, ["amount", 12], ["e", "offer1"]],
    content: JSON.stringify({
      usage_measure: { total_tokens: "NaN-ish" },
      measured_cost_tokens: { nested: true },
    }),
  },
];

for (const g of garbage) {
  assert.doesNotThrow(() => parseEvent(g));
}

const store = createStore();
for (const g of garbage) {
  assert.doesNotThrow(() => store.ingest(parseEvent(g)));
}

// one good offer after garbage — funnel still renders numbers
const offerId = "1".repeat(64);
store.ingest(
  ok({
    id: offerId,
    pubkey: "2".repeat(64),
    kind: 5109,
    created_at: 100,
    tags: [
      ["i", "task"],
      ["amount", "21", "sat"],
      ["t", "mobee"],
      ["v", "1"],
    ],
    content: "",
  }),
);

const funnel = store.funnel();
assert.equal(funnel.offers, 1);
assert.equal(funnel.leaks.unclaimed, 1);
assert.ok(funnel.parseSkips >= 1, "malformed events counted as skips");
assert.doesNotThrow(() => store.snapshot());

// ——— usage adjunct vocabulary (Scribe lock) ———

const adjunct = extractUsageAdjunct(
  {
    usage_measure: {
      total_tokens: 13693,
      input_tokens: 13346,
      output_tokens: 347,
      cache_read_tokens: 41088,
    },
    measured_cost_tokens: null,
    paid_price_tokens: 20000,
    usage_transport: "side-channel",
    harness_family: "cursor",
  },
  [
    ["amount", "21", "sat"],
    ["e", offerId, "", "root"],
  ],
);

assert.equal(adjunct.total_tokens, 13693);
assert.equal(adjunct.measured_cost_tokens, null);
assert.equal(adjunct.paid_price_tokens, 20000);
assert.equal(adjunct.usage_transport, "side-channel");
assert.equal(adjunct.harness_family, "cursor");
assert.equal(adjunct.paid_price_sats, 21);
// cache must NOT be folded into total by the parser
assert.notEqual(adjunct.total_tokens, 13346 + 347 + 41088);

// old receipt without adjunct fields — still parses
const oldReceipt = ok({
  id: "3".repeat(64),
  pubkey: "4".repeat(64),
  kind: 3400,
  created_at: 200,
  tags: [
    ["amount", "7", "sat"],
    ["e", offerId, "", "root"],
    ["e", "9".repeat(64), "", "reply"],
    ["mint", "https://testnut.cashu.space"],
  ],
  content: "",
});
assert.equal(oldReceipt.receipt.usage.total_tokens, null);
assert.equal(oldReceipt.receipt.usage.measured_cost_tokens, null);
assert.equal(oldReceipt.receipt.amount_sats, 7);

store.ingest(oldReceipt);
const eco = store.economics();
assert.ok(eco.rows.length >= 1);
assert.equal(eco.rows[0].measured_cost_tokens, null);

// ——— census harness_name + version ———

const handler = ok({
  id: "5".repeat(64),
  pubkey: "6".repeat(64),
  kind: 31990,
  created_at: 300,
  tags: [
    ["d", "seller-a"],
    ["k", "5109"],
  ],
  content: JSON.stringify({
    harness_name: "cursor-agent",
    version: "2026.07.09",
    name: "fallback-name",
  }),
});
assert.equal(handler.handler.harness_name, "cursor-agent");
assert.equal(handler.handler.version, "2026.07.09");
store.ingest(handler);
assert.equal(store.census()[0].harness_name, "cursor-agent");

// ——— latency path ———
store.ingest(
  ok({
    id: "7".repeat(64),
    pubkey: "8".repeat(64),
    kind: 7000,
    created_at: 130,
    tags: [
      ["status", "processing"],
      ["e", offerId],
      ["t", "mobee"],
      ["v", "1"],
    ],
    content: "",
  }),
);
store.ingest(
  ok({
    id: "a".repeat(64),
    pubkey: "b".repeat(64),
    kind: 6109,
    created_at: 180,
    tags: [
      ["e", offerId, "", "root"],
      ["amount", "21", "sat"],
      ["t", "mobee"],
      ["v", "1"],
    ],
    content: "done",
  }),
);
const lat = store.latency();
assert.equal(lat.toClaim.n, 1);
assert.equal(lat.toClaim.p50, 30);
assert.equal(lat.toResult.p50, 50);

assert.equal(percentile([1, 2, 3, 4], 50), 2.5);
assert.equal(percentile([], 50), null);

console.log("ok — parse/store suite passed");
