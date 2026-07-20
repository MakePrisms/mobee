#!/usr/bin/env bash
#
# discord-webhook.sh — mobee announce sink for a Discord channel webhook.
#
# Contract (see docs/skills/announce.md): reads ONE mobee lifecycle event as JSON on STDIN,
# formats it into a single human line, and POSTs it to a Discord webhook as `{"content": "…"}`.
# Wire it with `[seller_announce] command = ["/abs/path/to/sinks/discord-webhook.sh"]`.
#
# ── Environment contract ─────────────────────────────────────────────────────────────
#   DISCORD_WEBHOOK_URL  (required) the channel webhook URL. NEVER echoed.
#   ANNOUNCE_DRY_RUN=1   print the JSON body that WOULD be POSTed to stdout instead of sending.
#
# Fail-soft: any error (bad JSON, missing URL, curl failure) is logged to stderr and swallowed
# (exit 0) so the daemon is never affected.
#
set -uo pipefail

DRY_RUN="${ANNOUNCE_DRY_RUN:-0}"

log() { echo "discord-webhook: $*" >&2; }

event="$(cat)"

# Format the event → one human line (shared shape with the buzz sink).
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

# Build the Discord payload with jq so the message is JSON-escaped correctly.
payload="$(jq -cn --arg content "$line" '{content: $content}')"

if [[ "$DRY_RUN" == "1" ]]; then
  printf '%s\n' "$payload"
  exit 0
fi

if [[ -z "${DISCORD_WEBHOOK_URL:-}" ]]; then
  log "DISCORD_WEBHOOK_URL is required (ignored)"; exit 0
fi

if ! curl -fsS -X POST -H 'Content-Type: application/json' \
    --data "$payload" "$DISCORD_WEBHOOK_URL" >/dev/null 2>&1; then
  log "discord POST failed (continuing): $line"
fi
exit 0
