#!/usr/bin/env bash
#
# mobee-buzz-sidecar.sh — make a running mobee seller visible in buzz team chat.
#
# ZERO daemon change. This sidecar tails a seller's stderr logfile (the one an operator
# produces with `mobee sell … 2>&1 | tee <logfile>`) and forwards matched lifecycle
# lines to a buzz channel as one-line chat messages. It never writes to the daemon, holds
# no locks it shares with the daemon, and a buzz send failure is logged and swallowed —
# the sidecar can never affect the seller it is watching.
#
# Every forwarded event class is matched to a REAL `eprintln!` format string in
# crates/mobee-core/src/seller_daemon.rs; the source line is cited beside each matcher.
# If those daemon log formats change, this sidecar needs updating (see docs/skills/buzz-announce.md LIMITS).
#
# ── Environment contract ─────────────────────────────────────────────────────────────
#   SELLER_LOG        (required) path to the seller stderr logfile to tail.
#   BUZZ_CHANNEL      (required) buzz channel UUID to post into (from `buzz channels list`).
#   BUZZ_PRIVATE_KEY  (required) hex key the sidecar posts under. NEVER echoed or logged.
#   BUZZ_RELAY_URL    (default https://buzzrelay.orveth.dev) buzz relay.
#   SELLER_LABEL      (default: first 8 hex of the pubkey parsed from the "daemon online"
#                     line; "seller" until that line is seen) short prefix on each message.
#   BUZZ_BIN          (default /srv/forge/workspaces/buzz/target/release/buzz, else `buzz`
#                     on PATH) the buzz CLI.
#   SIDECAR_DRY_RUN=1 print the formatted message to stdout instead of sending to buzz.
#
# Rate-safety: exactly one buzz message per matched log line. A byte offset is persisted
# next to the log ("<SELLER_LOG>.sidecar-offset") so a sidecar restart resumes past lines
# already forwarded instead of re-announcing the whole log.
#
set -uo pipefail

# ── Config + required-env validation (fail fast, never print the key) ─────────────────
: "${SELLER_LOG:?SELLER_LOG (path to the seller stderr logfile) is required}"
: "${BUZZ_CHANNEL:?BUZZ_CHANNEL (buzz channel UUID) is required}"
if [[ -z "${BUZZ_PRIVATE_KEY:-}" ]]; then
  echo "sidecar: BUZZ_PRIVATE_KEY is required (not printed)" >&2
  exit 1
fi
BUZZ_RELAY_URL="${BUZZ_RELAY_URL:-https://buzzrelay.orveth.dev}"
DRY_RUN="${SIDECAR_DRY_RUN:-0}"
DEFAULT_BUZZ_BIN=/srv/forge/workspaces/buzz/target/release/buzz
BUZZ_BIN="${BUZZ_BIN:-$DEFAULT_BUZZ_BIN}"
if [[ ! -x "$BUZZ_BIN" ]]; then
  BUZZ_BIN="$(command -v buzz || true)"
fi
if [[ "$DRY_RUN" != "1" && -z "$BUZZ_BIN" ]]; then
  echo "sidecar: no buzz CLI found (set BUZZ_BIN or install `buzz` on PATH); run with SIDECAR_DRY_RUN=1 to test" >&2
  exit 1
fi

OFFSET_FILE="${SELLER_LOG}.sidecar-offset"
# Label defaults to "seller" until the daemon-online line reveals the pubkey.
LABEL="${SELLER_LABEL:-seller}"
LABEL_LOCKED=0
[[ -n "${SELLER_LABEL:-}" ]] && LABEL_LOCKED=1   # explicit env label wins over parsed pubkey

log_sidecar() { echo "sidecar: $*" >&2; }

# ── Send one line to buzz (body via STDIN — never inline, to dodge shell-quoting) ─────
# Fail-soft: a send failure is logged and swallowed so the seller is never affected.
send_to_buzz() {
  local body="$1"
  if [[ "$DRY_RUN" == "1" ]]; then
    printf '%s\n' "$body"
    return 0
  fi
  if ! printf '%s' "$body" \
      | BUZZ_PRIVATE_KEY="$BUZZ_PRIVATE_KEY" BUZZ_RELAY_URL="$BUZZ_RELAY_URL" \
        "$BUZZ_BIN" messages send --channel "$BUZZ_CHANNEL" --content - >/dev/null 2>&1; then
    log_sidecar "buzz send failed (continuing): $body"
  fi
}

# ── Match one log line to an event class and forward it (at most one message) ─────────
# Each matcher cites the seller_daemon.rs line whose eprintln! format it keys off.
handle_line() {
  local line="$1"
  local m

  # daemon online — seller_daemon.rs:1736
  #   "seller daemon online pubkey={} relay={} mint={} nip42={nip42_label}"
  if [[ "$line" == "seller daemon online "* ]]; then
    if [[ $LABEL_LOCKED -eq 0 && "$line" =~ pubkey=([0-9a-fA-F]+) ]]; then
      LABEL="${BASH_REMATCH[1]:0:8}"
    fi
    local relay="" mint="" nip42=""
    [[ "$line" =~ relay=([^[:space:]]+) ]] && relay="${BASH_REMATCH[1]}"
    [[ "$line" =~ mint=([^[:space:]]+) ]]  && mint="${BASH_REMATCH[1]}"
    [[ "$line" =~ nip42=([^[:space:]]+) ]] && nip42="${BASH_REMATCH[1]}"
    send_to_buzz "[$LABEL] online — relay=$relay mint=$mint nip42=$nip42"
    return
  fi

  # delivered / result published — seller_daemon.rs:1796
  #   "seller published 6109 result_id={result_id}"
  if [[ "$line" == "seller published 6109 result_id="* ]]; then
    m="${line#seller published 6109 result_id=}"
    send_to_buzz "[$LABEL] delivered — result_id=$m"
    return
  fi

  # payment received / redeemed (receipt journaled) — seller_daemon.rs:1762 (live) & :1801 (reconcile)
  #   "seller receipt job_id={} result_id={} amount_received={}"
  #   "seller receipt (reconcile) job_id={} amount_received={}"
  if [[ "$line" == "seller receipt "* ]]; then
    local job="" amt=""
    [[ "$line" =~ job_id=([^[:space:]]+) ]]         && job="${BASH_REMATCH[1]}"
    [[ "$line" =~ amount_received=([0-9]+) ]]       && amt="${BASH_REMATCH[1]}"
    send_to_buzz "[$LABEL] paid — job_id=$job amount_received=$amt sats"
    return
  fi

  # payment collected at the mint — seller_daemon.rs:122-133 (collect_ok_log_line), emitted :737
  #   "seller collect ok: job_id={} result_id={} amount_received={} expected={} mint={}"
  if [[ "$line" == "seller collect ok: "* ]]; then
    local job="" amt="" exp=""
    [[ "$line" =~ job_id=([^[:space:]]+) ]]   && job="${BASH_REMATCH[1]}"
    [[ "$line" =~ amount_received=([0-9]+) ]] && amt="${BASH_REMATCH[1]}"
    [[ "$line" =~ expected=([0-9]+) ]]        && exp="${BASH_REMATCH[1]}"
    send_to_buzz "[$LABEL] collected — job_id=$job amount_received=$amt expected=$exp"
    return
  fi

  # rate-gate refusal surfaced — seller_daemon.rs:594
  #   "seller under-rate refusal surfaced: kind-7000 error={id} offer={} (amount {} < rate_sats {})"
  if [[ "$line" == "seller under-rate refusal surfaced: "* ]]; then
    local offer="" amt="" rate=""
    [[ "$line" =~ offer=([^[:space:]]+) ]]        && offer="${BASH_REMATCH[1]}"
    [[ "$line" =~ amount\ ([0-9]+)\ \< ]]         && amt="${BASH_REMATCH[1]}"
    [[ "$line" =~ rate_sats\ ([0-9]+) ]]          && rate="${BASH_REMATCH[1]}"
    send_to_buzz "[$LABEL] refused under-rate — offer=$offer (amount $amt < rate_sats $rate)"
    return
  fi

  # reconcile released an orphaned claim — seller_daemon.rs:1672 (ok) & :1682 (kind-7000 deferred)
  #   "seller reconcile: released orphaned claim job_id={} liveness={:?} kind7000={id} reason={reason}"
  # (the count line "seller reconcile: released N orphaned claim(s) on startup" does NOT match —
  #  we forward the per-claim line, which carries the job_id, and avoid double-reporting.)
  if [[ "$line" == "seller reconcile: released orphaned claim "* ]]; then
    local job="" live="" reason=""
    [[ "$line" =~ job_id=([^[:space:]]+) ]]  && job="${BASH_REMATCH[1]}"
    [[ "$line" =~ liveness=([^[:space:]]+) ]] && live="${BASH_REMATCH[1]}"
    [[ "$line" =~ reason=(.+)$ ]]             && reason="${BASH_REMATCH[1]}"
    send_to_buzz "[$LABEL] reconcile released orphaned claim — job_id=$job liveness=$live reason=$reason"
    return
  fi

  # seller job failed — seller_daemon.rs:1813
  #   "seller job failed: {error}"
  if [[ "$line" == "seller job failed: "* ]]; then
    m="${line#seller job failed: }"
    send_to_buzz "[$LABEL] job FAILED — $m"
    return
  fi
}

# ── Resume point: skip lines already forwarded on a prior run ─────────────────────────
# Persisted byte offset next to the log. If the log is shorter than the stored offset
# (rotated/truncated), reset to 0 and re-read from the top.
start_offset=0
if [[ -f "$OFFSET_FILE" ]]; then
  start_offset="$(cat "$OFFSET_FILE" 2>/dev/null || echo 0)"
  [[ "$start_offset" =~ ^[0-9]+$ ]] || start_offset=0
fi
cur_size=0
[[ -f "$SELLER_LOG" ]] && cur_size="$(wc -c < "$SELLER_LOG" 2>/dev/null || echo 0)"
if (( start_offset > cur_size )); then
  log_sidecar "log shorter than saved offset ($cur_size < $start_offset) — rotated/truncated, resuming from 0"
  start_offset=0
fi

log_sidecar "watching $SELLER_LOG from byte $start_offset → channel $BUZZ_CHANNEL (dry_run=$DRY_RUN)"

# ── Tail from the resume offset and follow. `tail -c +N` is 1-based on the byte to start. ─
# Track a running byte offset so each processed line advances the persisted resume point.
offset="$start_offset"
tail -c "+$((start_offset + 1))" -F -- "$SELLER_LOG" 2>/dev/null | while IFS= read -r line; do
  handle_line "$line"
  # +1 for the newline `read` stripped; keeps the offset aligned with real file bytes.
  offset=$(( offset + ${#line} + 1 ))
  printf '%s' "$offset" > "$OFFSET_FILE"
done
