/**
 * SINGLE SOURCE OF TRUTH for every Nostr `kind` number the observatory touches.
 *
 * v2 will renumber the marketplace kinds. When that happens this file is the ONLY
 * edit — every other module imports the named constants below and MUST NOT hard-code
 * a kind literal. A grep test (test/kinds.test.mjs) enforces that: the digits 5109,
 * 7000, 6109, 3400, 31990 may appear nowhere in the source except this file.
 */

/** NIP-01 profile metadata. Nostr-standard — will not renumber. */
export const PROFILE = 0;

/** Job offer the buyer posts. Sellers claim it. */
export const OFFER = 5109;
/** Job feedback / status update (NIP-90 family): claim, accept, error/refusal. */
export const CLAIM = 7000;
/** Job result the seller delivers. */
export const RESULT = 6109;
/** Co-signed payment receipt — the settlement proof. */
export const RECEIPT = 3400;
/** NIP-89 seller handler announce (a seller capability advert). */
export const HANDLER = 31990;
/**
 * Seller liveness heartbeat. Addressable (parameterized-replaceable): keyed by
 * (author, kind, d) — resolve the current one by AUTHOR + KIND (+ d), taking the
 * newest created_at. NEVER look it up by a published event id (a replaceable event
 * is superseded, so by-id lookups go empty and read as a false "offline").
 */
export const HEARTBEAT = 30340;

/** Plain-English labels for a kind, for any place a kind must surface to a human. */
export const KIND_LABELS = Object.freeze({
  [PROFILE]: "profile",
  [OFFER]: "offer",
  [CLAIM]: "claim/feedback",
  [RESULT]: "result",
  [RECEIPT]: "receipt",
  [HANDLER]: "handler (NIP-89)",
  [HEARTBEAT]: "heartbeat",
});

/** Marketplace kinds the relay subscription requests. Gift-wrap (1059) stays dark. */
export const SUBSCRIBE_KINDS = Object.freeze([
  OFFER,
  CLAIM,
  RESULT,
  RECEIPT,
  HANDLER,
  HEARTBEAT,
]);
