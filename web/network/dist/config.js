/** Relay websocket — single config constant. Exact wss path pending infraguy. */
export const RELAY_URL = "wss://mobee-relay.orveth.dev";

/** Marketplace kinds (gift-wrap 1059 stays dark — never subscribed). */
export const KINDS = Object.freeze({
  OFFER: 5109,
  CLAIM: 7000,
  RESULT: 6109,
  RECEIPT: 3400,
  HANDLER: 31990,
});

export const KIND_LABELS = Object.freeze({
  5109: "offer",
  7000: "claim/feedback",
  6109: "result",
  3400: "receipt",
  31990: "handler (NIP-89)",
});

export const SUBSCRIBE_KINDS = Object.freeze([
  KINDS.OFFER,
  KINDS.CLAIM,
  KINDS.RESULT,
  KINDS.RECEIPT,
  KINDS.HANDLER,
]);

/** How many historical events to request on connect. */
export const HISTORY_LIMIT = 1000;
