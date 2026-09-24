#!/usr/bin/env bash
# Ringing v2 real-machine smoke test.
#
# Builds the daemon, runs it against an isolated data root, seeds a canonical
# session with a resolved ask interaction, and probes the v2 surface over a
# real HTTP socket:
#
#   open -> bootstrap -> driver claim/busy -> not_driver gate
#        -> first-answer-wins typed verdict -> command replay
#        -> command status -> SSE subscribe
#
# Usage: scripts/v2-smoke.sh [data-root]
# The data root must end in `qaqh` (daemon safety rule); default
# `$HOME/.qaqh-v2-smoke/qaqh` is created on demand.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export HOME="${HOME:-/home/$(id -un)}"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"

DATA="${1:-$HOME/.qaqh-v2-smoke/qaqh}"
mkdir -p "$DATA/sessions"
# Fresh seed per run: the canonical writer fence is append-only and stays
# leased for the seeder's TTL, so re-seeding the same dir fails with
# `WriterBusy` on the second invocation.
SEED="smoke-$(date +%s)-$$"

say() { printf '%s\n' "$*"; }
fail() {
    say "FAIL: $*"
    exit 1
}

json_get() { python3 -c "import json,sys;print(json.load(sys.stdin)$1)"; }

say "== building daemon =="
cargo build -q -p qaqh-daemon
cargo run -q -p qaqh-session --example e2e_seed -- "$DATA/sessions/$SEED" --resolved \
    > "$DATA/../interaction.txt"
INTERACTION="$(cat "$DATA/../interaction.txt")"
say "seeded canonical session $SEED with resolved interaction $INTERACTION"

say "== starting daemon =="
QAQH_DATA_DIR="$DATA" "$ROOT/target/debug/qaqh-daemon" run > "$DATA/../daemon.out" 2>&1 &
DAEMON_PID=$!
trap 'kill "$DAEMON_PID" 2>/dev/null || true' EXIT

for _ in $(seq 1 80); do
    [ -f "$DATA/daemon.json" ] && break
    sleep 0.25
done
[ -f "$DATA/daemon.json" ] || fail "daemon did not publish discovery ($DATA/daemon.json)"

ENDPOINT="$(json_get "['endpoint']" < "$DATA/daemon.json")"
TOKEN="$(json_get "['token']" < "$DATA/daemon.json")"
say "endpoint=$ENDPOINT"

open_client() {
    curl -sS -X POST "$ENDPOINT/ringing/v2/clients/open" \
        -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
        -d "{\"schema\":\"qaqh.Ringing\",\"version\":2,\"client_instance_id\":\"$1\"}" \
        | json_get "['client_session_id']"
}
command() {
    curl -sS -X POST "$ENDPOINT/ringing/v2/commands/$2" \
        -H "authorization: Bearer $TOKEN" -H "x-qaqh-client-session-id: $1" \
        -H 'content-type: application/json' -d "$3"
}
driver() {
    curl -sS -X POST "$ENDPOINT/ringing/v2/sessions/$SEED/driver/$2" \
        -H "authorization: Bearer $TOKEN" -H "x-qaqh-client-session-id: $1"
}

A="$(open_client smoke-a)"
B="$(open_client smoke-b)"
say "clients: a=${A:0:12}… b=${B:0:12}…"

say "== bootstrap =="
BOOTSTRAP="$(curl -sS "$ENDPOINT/ringing/v2/sessions/$SEED/bootstrap" \
    -H "authorization: Bearer $TOKEN" -H "x-qaqh-client-session-id: $A")"
[ "$(printf '%s' "$BOOTSTRAP" | json_get "['version']")" = "2" ] || fail "bootstrap version"
[ "$(printf '%s' "$BOOTSTRAP" | json_get "['seed']")" = "$SEED" ] || fail "bootstrap seed"
[ "$(printf '%s' "$BOOTSTRAP" | json_get "['control']['state']['driver']['can_claim']")" = "True" ] \
    || fail "driver can_claim before claim"

say "== driver claim / busy / release =="
CLAIM_A="$(driver "$A" claim)"
[ "$(printf '%s' "$CLAIM_A" | json_get "['accepted']")" = "True" ] || fail "claim a"
[ "$(printf '%s' "$CLAIM_A" | json_get "['driver_epoch']")" = "1" ] || fail "claim a epoch"
CLAIM_B="$(driver "$B" claim)"
[ "$(printf '%s' "$CLAIM_B" | json_get "['reason']")" = "driver_busy" ] || fail "claim b busy"

say "== not_driver gate =="
GATED="$(command "$B" conversation \
    "{\"schema\":\"qaqh.Ringing\",\"version\":2,\"channel\":\"conversation\",\"command_id\":\"smoke-b-cancel\",\"client_instance_id\":\"smoke-b\",\"client_session_id\":\"$B\",\"seed\":\"$SEED\",\"command\":{\"channel\":\"conversation\",\"type\":\"conversation_cancel\"}}")"
[ "$(printf '%s' "$GATED" | json_get "['code']")" = "not_driver" ] || fail "not_driver gate"

say "== first-answer-wins typed verdict =="
LOSER="$(command "$B" control \
    "{\"schema\":\"qaqh.Ringing\",\"version\":2,\"channel\":\"control\",\"command_id\":\"smoke-second-answer\",\"client_instance_id\":\"smoke-b\",\"client_session_id\":\"$B\",\"seed\":\"$SEED\",\"command\":{\"channel\":\"control\",\"type\":\"interaction_ask_respond\",\"interaction_id\":\"$INTERACTION\",\"answers\":[]}}")"
[ "$(printf '%s' "$LOSER" | json_get "['code']")" = "interaction_already_resolved" ] \
    || fail "second answer code"
[ "$(printf '%s' "$LOSER" | json_get "['existing']['source']")" = "interaction_resolved" ] \
    || fail "second answer typed source"
[ "$(printf '%s' "$LOSER" | json_get "['existing']['result']['kind']")" = "ask_resolved" ] \
    || fail "second answer typed kind"

say "== command replay + status =="
REPLAY_ID="smoke-replay-$$"
ATTACH="{\"schema\":\"qaqh.Ringing\",\"version\":2,\"channel\":\"control\",\"command_id\":\"$REPLAY_ID\",\"client_instance_id\":\"smoke-a\",\"client_session_id\":\"$A\",\"seed\":\"$SEED\",\"command\":{\"channel\":\"control\",\"type\":\"session_attach\",\"seed\":\"$SEED\"}}"
command "$A" control "$ATTACH" > /dev/null
REPLAY="$(command "$A" control "$ATTACH")"
[ "$(printf '%s' "$REPLAY" | json_get "['existing']['source']")" = "command_receipt" ] \
    || fail "replay typed source"
STATUS="$(curl -sS "$ENDPOINT/ringing/v2/commands/$REPLAY_ID" \
    -H "authorization: Bearer $TOKEN" -H "x-qaqh-client-session-id: $A")"
[ "$(printf '%s' "$STATUS" | json_get "['state']")" = "succeeded" ] || fail "command status"

say "== SSE subscribe =="
CURSOR="$(printf '%s' "$BOOTSTRAP" | json_get "['snapshot_cursor']")"
SSE_HEAD="$(curl -sS -i -N --max-time 2 \
    "$ENDPOINT/ringing/v2/sessions/$SEED/events/control?since_cursor=$CURSOR" \
    -H "authorization: Bearer $TOKEN" -H "x-qaqh-client-session-id: $A" 2>/dev/null | head -1 || true)"
printf '%s' "$SSE_HEAD" | grep -q "200" || fail "SSE subscribe ($SSE_HEAD)"

driver "$A" release > /dev/null
say "PASS: Ringing v2 real-machine smoke test"
