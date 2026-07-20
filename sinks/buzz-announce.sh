#!/usr/bin/env bash
#
# buzz-announce.sh — FIRST-CLASS mobee announce sink for buzz team chat.
#
# Contract (see docs/skills/announce.md): reads ONE mobee lifecycle event as JSON on STDIN,
# formats it into a single human-readable line, and sends it to a buzz channel. The daemon
# spawns this once per lifecycle transition with the event JSON on stdin (`[seller_announce]
# command = ["/abs/path/to/sinks/buzz-announce.sh"]`).
#
# This is the daemon-native counterpart of scripts/mobee-buzz-sidecar.sh: same one-line message
# style, but fed a structured event by the daemon instead of scraped from stderr — so it also
# carries the `claimed` transition the log-tailing sidecar cannot see.
#
# ── Environment contract ─────────────────────────────────────────────────────────────
#   BUZZ_CHANNEL      (required) buzz channel UUID to post into (`buzz channels list`).
#   BUZZ_PRIVATE_KEY  (required) hex key the sink posts under. NEVER echoed or logged.
#   BUZZ_RELAY_URL    (default https://buzzrelay.orveth.dev) buzz relay.
#   BUZZ_BIN          (default /srv/forge/workspaces/buzz/target/release/buzz, else `buzz`
#                     on PATH) the buzz CLI.
#   ANNOUNCE_DRY_RUN=1  print the formatted message to stdout instead of sending to buzz
#                       (env checks relaxed) — use to verify formatting.
#
# Fail-soft: any error (bad JSON, missing CLI, send failure) is logged to stderr and swallowed
# with exit 0 so the daemon (which ignores this process anyway) is never affected.
#
set -uo pipefail

DRY_RUN="${ANNOUNCE_DRY_RUN:-0}"

log() { echo "buzz-announce: $*" >&2; }

# Read the whole event from stdin (one JSON object).
event="$(cat)"

# Format the event → one human line. Shared shape with mobee-buzz-sidecar.sh.
line="$(printf '%s' "$event" | jq -r '
  def tag: (.seller_pubkey // "seller")[0:8];
  "[\(tag)] " + (
    if   .event == "online"             then "online — relay=\(.relay) mint=\(.mint) nip42=\(.nip42)"
    elif .event == "claimed"            then "claimed — job_id=\(.job_id) buyer=\(.buyer_pubkey) amount=\(.amount) sats deadline=\(.deadline_unix)"
    elif .event == "delivered"          then "delivered — job_id=\(.job_id) result_id=\(.result_id) commit=\(.commit)"
    elif .event == "collected"          then "collected — job_id=\(.job_id) amount_received=\(.amount_received) expected=\(.expected) sats"
    elif .event == "refused"            then "refused — job_id=\(.job_id) reason_code=\(.reason_code) reason=\(.reason)"
    elif .event == "reconcile_released" then "reconcile released orphaned claim — job_id=\(.job_id) liveness=\(.liveness) reason=\(.reason)"
    elif .event == "job_failed"         then "job FAILED — job_id=\(.job_id) reason=\(.reason)"
    else "\(.event)" end
  )
' 2>/dev/null)"

if [[ -z "$line" ]]; then
  log "could not parse event (ignored): ${event:0:120}"
  exit 0
fi

if [[ "$DRY_RUN" == "1" ]]; then
  printf '%s\n' "$line"
  exit 0
fi

# Required env for a live send (relaxed under dry-run above).
if [[ -z "${BUZZ_CHANNEL:-}" ]]; then
  log "BUZZ_CHANNEL is required (ignored)"; exit 0
fi
if [[ -z "${BUZZ_PRIVATE_KEY:-}" ]]; then
  log "BUZZ_PRIVATE_KEY is required, not printed (ignored)"; exit 0
fi
BUZZ_RELAY_URL="${BUZZ_RELAY_URL:-https://buzzrelay.orveth.dev}"
DEFAULT_BUZZ_BIN=/srv/forge/workspaces/buzz/target/release/buzz
BUZZ_BIN="${BUZZ_BIN:-$DEFAULT_BUZZ_BIN}"
if [[ ! -x "$BUZZ_BIN" ]]; then
  BUZZ_BIN="$(command -v buzz || true)"
fi
if [[ -z "$BUZZ_BIN" ]]; then
  log "no buzz CLI found (set BUZZ_BIN or install buzz on PATH) (ignored)"; exit 0
fi

# Body always goes over STDIN (--content -) so the key/body never land in `ps`.
if ! printf '%s' "$line" \
    | BUZZ_PRIVATE_KEY="$BUZZ_PRIVATE_KEY" BUZZ_RELAY_URL="$BUZZ_RELAY_URL" \
      "$BUZZ_BIN" messages send --channel "$BUZZ_CHANNEL" --content - >/dev/null 2>&1; then
  log "buzz send failed (continuing): $line"
fi
exit 0
