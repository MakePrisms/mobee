/**
 * Deploy-tunable constants. Kind numbers live in js/kinds.js (single source) — never here.
 *
 * Relay pinned 2026-07-14: wss://mobee-relay.orveth.dev is live (anon open-read;
 * AUTH challenge may appear first — ignore it; historical REQ still served).
 */
export const RELAY_URL = "wss://mobee-relay.orveth.dev";

/** How many historical events to request on connect. */
export const HISTORY_LIMIT = 1000;
