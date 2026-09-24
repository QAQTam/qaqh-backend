#!/usr/bin/env bash
# Ringing v2 real-machine smoke test.
#
# Builds the daemon, runs it against an isolated data root, seeds a canonical
# session with resolved ask/permission/plan interactions, and probes the v2
# surface over a real HTTP socket:
#
#   open -> bootstrap -> driver claim/busy -> not_driver gate
#        -> first-answer-wins typed verdict (ask/permission/plan)
#        -> reliable reconnect (driver_changed replay)
#        -> command replay -> command status -> SSE subscribe
#        -> driver release / lease reclaim / restart reclaim
#        -> snapshot_missing
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
# `SEED` is discovered after `session_create`: the actor can only resume a
# daemon-created session, so the canonical log is seeded into that directory.

say() { printf '%s\n' "$*"; }
fail() {
    say "FAIL: $*"
    exit 1
}

json_get() { python3 -c "import json,sys;print(json.load(sys.stdin)$1)"; }

say "== building daemon =="
cargo build -q -p qaqh-daemon

start_daemon() {
    # A stale discovery file from a previous run would make the readiness loop
    # below succeed against a dead endpoint.
    mv "$DATA/daemon.json" "$DATA/daemon.json.prev" 2>/dev/null || true
    # Short lease TTL so the driver-seat reclaim path is reachable in one run.
    # `QAQH_SMOKE_LEASE_TTL_MS` can raise it when the host is under heavy load:
    # the phases between two lease renewals otherwise outlive a 6s TTL and the
    # explicit-release probe fails with `lease_required` (not a product bug).
    QAQH_DATA_DIR="$DATA" QAQH_TEST_LEASE_TTL_MS="${QAQH_SMOKE_LEASE_TTL_MS:-6000}" \
        "$ROOT/target/debug/qaqh-daemon" run > "$DATA/../daemon.out" 2>&1 &
    DAEMON_PID=$!
    for _ in $(seq 1 80); do
        [ -f "$DATA/daemon.json" ] && break
        sleep 0.25
    done
    [ -f "$DATA/daemon.json" ] || fail "daemon did not publish discovery ($DATA/daemon.json)"
    ENDPOINT="$(json_get "['endpoint']" < "$DATA/daemon.json")"
    TOKEN="$(json_get "['token']" < "$DATA/daemon.json")"
}
stop_daemon() {
    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
}

say "== starting daemon =="
start_daemon
trap 'kill "$DAEMON_PID" 2>/dev/null || true' EXIT
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
# `require_v2_lease` does NOT renew: a lease only refreshes on an explicit
# `/leases/renew`. The phases below spend seconds in SSE waits, so any phase
# that needs a *live* holder must renew it first — otherwise the 6s TTL lapses
# mid-script and the probe fails with `lease_required`/forwarded-command (a
# harness bug, not a product one).
renew() {
    curl -sS -X POST "$ENDPOINT/ringing/v2/leases/renew" \
        -H "authorization: Bearer $TOKEN" -H "x-qaqh-client-session-id: $1" \
        > /dev/null 2>&1 || true
}
# Poll the canonical seat via an observer lease. `-` means "no holder".
# The observer renews its own lease so it stays alive while another seat expires.
wait_driver() {
    local expected_holder="$1" expected_epoch="$2" observer="$3" got=""
    for _ in $(seq 1 80); do
        curl -sS -X POST "$ENDPOINT/ringing/v2/leases/renew" \
            -H "authorization: Bearer $TOKEN" -H "x-qaqh-client-session-id: $observer" \
            > /dev/null 2>&1 || true
        got="$(curl -sS "$ENDPOINT/ringing/v2/sessions/$SEED/bootstrap" \
            -H "authorization: Bearer $TOKEN" -H "x-qaqh-client-session-id: $observer" \
            | python3 -c "import json,sys;d=json.load(sys.stdin)['control']['state']['driver'];print(d.get('holder') or '-', d.get('driver_epoch'))")"
        [ "$got" = "$expected_holder $expected_epoch" ] && return 0
        sleep 0.5
    done
    fail "driver state: expected '$expected_holder $expected_epoch', got '$got'"
}

A="$(open_client smoke-a)"
B="$(open_client smoke-b)"
say "clients: a=${A:0:12}… b=${B:0:12}…"

say "== creating session through the daemon =="
BEFORE="$(ls -1 "$DATA/sessions" 2>/dev/null | grep -v '^index.jsonl$' | sort || true)"
command "$A" control \
    "{\"schema\":\"qaqh.Ringing\",\"version\":2,\"channel\":\"control\",\"command_id\":\"smoke-new-$$\",\"client_instance_id\":\"smoke-a\",\"client_session_id\":\"$A\",\"command\":{\"channel\":\"control\",\"type\":\"session_create\",\"close_current\":false,\"cwd\":\"/tmp\"}}" \
    > /dev/null
AFTER="$(ls -1 "$DATA/sessions" 2>/dev/null | grep -v '^index.jsonl$' | sort || true)"
SEED="$(comm -13 <(printf '%s\n' "$BEFORE") <(printf '%s\n' "$AFTER") | head -1)"
[ -n "$SEED" ] || fail "session_create did not produce a session directory"
say "session=$SEED"

# The actor can only resume a daemon-created session (meta + message store), so
# the canonical log is seeded *into* it rather than into a bare directory.
cargo run -q -p qaqh-session --example e2e_seed -- "$DATA/sessions/$SEED" --resolved \
    > "$DATA/../interactions.json"
INTERACTIONS="$(cat "$DATA/../interactions.json")"
ASK_ID="$(printf '%s' "$INTERACTIONS" | json_get "['ask']['interaction_id']")"
PERMISSION_CALL="$(printf '%s' "$INTERACTIONS" | json_get "['permission']['call_id']")"
PLAN_ID="$(printf '%s' "$INTERACTIONS" | json_get "['plan']['interaction_id']")"
say "seeded canonical log + resolved ask/permission/plan interactions"

say "== bootstrap =="
BOOTSTRAP="$(curl -sS "$ENDPOINT/ringing/v2/sessions/$SEED/bootstrap" \
    -H "authorization: Bearer $TOKEN" -H "x-qaqh-client-session-id: $A")"
[ "$(printf '%s' "$BOOTSTRAP" | json_get "['version']")" = "2" ] || fail "bootstrap version"
[ "$(printf '%s' "$BOOTSTRAP" | json_get "['seed']")" = "$SEED" ] || fail "bootstrap seed"
[ "$(printf '%s' "$BOOTSTRAP" | json_get "['control']['state']['driver']['can_claim']")" = "True" ] \
    || fail "driver can_claim before claim"
CURSOR="$(printf '%s' "$BOOTSTRAP" | json_get "['snapshot_cursor']")"

say "== driver claim / busy =="
CLAIM_A="$(driver "$A" claim)"
[ "$(printf '%s' "$CLAIM_A" | json_get "['accepted']")" = "True" ] || fail "claim a"
[ "$(printf '%s' "$CLAIM_A" | json_get "['reason']")" = "claim_requested" ] \
    || fail "claim a must be a forwarded request (canonical ledger allocates the epoch)"
# The actor's ToolLedger is the single writer: the authoritative seat arrives
# as the canonical DriverChanged fact, visible in the next bootstrap.
wait_driver "$A" 1 "$A"
CLAIM_B="$(driver "$B" claim)"
[ "$(printf '%s' "$CLAIM_B" | json_get "['reason']")" = "driver_busy" ] || fail "claim b busy"

say "== reliable reconnect replays driver handover =="
# `CURSOR` was taken before the claim, so the canonical DriverChanged fact must
# come back as a reliable control event on reconnect.
HANDOVER="$(curl -sS -N --max-time 3 \
    "$ENDPOINT/ringing/v2/sessions/$SEED/events/control?since_cursor=$CURSOR" \
    -H "authorization: Bearer $TOKEN" -H "x-qaqh-client-session-id: $A" 2>/dev/null | head -c 8192 || true)"
printf '%s' "$HANDOVER" | grep -q "event: ringing.event" || fail "reliable replay event name"
printf '%s' "$HANDOVER" | grep -q '"delivery":"reliable"' || fail "reliable replay delivery"
printf '%s' "$HANDOVER" | grep -q '"kind":"driver_changed"' || fail "driver_changed replay payload"

say "== not_driver gate =="
renew "$A"
GATED="$(command "$B" conversation \
    "{\"schema\":\"qaqh.Ringing\",\"version\":2,\"channel\":\"conversation\",\"command_id\":\"smoke-b-cancel\",\"client_instance_id\":\"smoke-b\",\"client_session_id\":\"$B\",\"seed\":\"$SEED\",\"command\":{\"channel\":\"conversation\",\"type\":\"conversation_cancel\"}}")"
[ "$(printf '%s' "$GATED" | json_get "['code']")" = "not_driver" ] || fail "not_driver gate"

say "== first-answer-wins typed verdict =="
LOSER="$(command "$B" control \
    "{\"schema\":\"qaqh.Ringing\",\"version\":2,\"channel\":\"control\",\"command_id\":\"smoke-second-answer\",\"client_instance_id\":\"smoke-b\",\"client_session_id\":\"$B\",\"seed\":\"$SEED\",\"command\":{\"channel\":\"control\",\"type\":\"interaction_ask_respond\",\"interaction_id\":\"$ASK_ID\",\"answers\":[]}}")"
[ "$(printf '%s' "$LOSER" | json_get "['code']")" = "interaction_already_resolved" ] \
    || fail "second answer code"
[ "$(printf '%s' "$LOSER" | json_get "['existing']['source']")" = "interaction_resolved" ] \
    || fail "second answer typed source"
[ "$(printf '%s' "$LOSER" | json_get "['existing']['result']['kind']")" = "ask_resolved" ] \
    || fail "second answer typed kind"

say "== permission first-answer-wins typed verdict =="
PERMISSION_LOSER="$(command "$B" tool \
    "{\"schema\":\"qaqh.Ringing\",\"version\":2,\"channel\":\"tool\",\"command_id\":\"smoke-second-permission\",\"client_instance_id\":\"smoke-b\",\"client_session_id\":\"$B\",\"seed\":\"$SEED\",\"command\":{\"channel\":\"tool\",\"type\":\"tool_permission_respond\",\"tool_call_id\":\"$PERMISSION_CALL\",\"approved\":false,\"trust_folder\":false}}")"
[ "$(printf '%s' "$PERMISSION_LOSER" | json_get "['code']")" = "interaction_already_resolved" ] \
    || fail "second permission answer code"
[ "$(printf '%s' "$PERMISSION_LOSER" | json_get "['existing']['result']['kind']")" = "permission_resolved" ] \
    || fail "second permission answer typed kind"
[ "$(printf '%s' "$PERMISSION_LOSER" | json_get "['existing']['result']['approved']")" = "True" ] \
    || fail "second permission answer winning verdict"

say "== plan first-answer-wins typed verdict =="
PLAN_LOSER="$(command "$B" control \
    "{\"schema\":\"qaqh.Ringing\",\"version\":2,\"channel\":\"control\",\"command_id\":\"smoke-second-plan\",\"client_instance_id\":\"smoke-b\",\"client_session_id\":\"$B\",\"seed\":\"$SEED\",\"command\":{\"channel\":\"control\",\"type\":\"plan_review_respond\",\"interaction_id\":\"$PLAN_ID\",\"approved\":true,\"autonomous\":false}}")"
[ "$(printf '%s' "$PLAN_LOSER" | json_get "['code']")" = "interaction_already_resolved" ] \
    || fail "second plan answer code"
[ "$(printf '%s' "$PLAN_LOSER" | json_get "['existing']['result']['kind']")" = "plan_review_resolved" ] \
    || fail "second plan answer typed kind"
[ "$(printf '%s' "$PLAN_LOSER" | json_get "['existing']['result']['approved']")" = "False" ] \
    || fail "second plan answer winning verdict"

say "== command replay + status =="
renew "$A"
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
renew "$A"
CURSOR="$(printf '%s' "$BOOTSTRAP" | json_get "['snapshot_cursor']")"
SSE_HEAD="$(curl -sS -i -N --max-time 2 \
    "$ENDPOINT/ringing/v2/sessions/$SEED/events/control?since_cursor=$CURSOR" \
    -H "authorization: Bearer $TOKEN" -H "x-qaqh-client-session-id: $A" 2>/dev/null | head -1 || true)"
printf '%s' "$SSE_HEAD" | grep -q "200" || fail "SSE subscribe ($SSE_HEAD)"

say "== driver release (explicit) =="
renew "$A"
RELEASE_A="$(driver "$A" release)"
[ "$(printf '%s' "$RELEASE_A" | json_get "['reason']")" = "release_requested" ] \
    || fail "release a must be a forwarded request"
wait_driver "-" 2 "$B"

say "== driver auto-reclaim on lease expiry =="
renew "$A"
CLAIM_AGAIN="$(driver "$A" claim)"
[ "$(printf '%s' "$CLAIM_AGAIN" | json_get "['reason']")" = "claim_requested" ] \
    || fail "re-claim a"
wait_driver "$A" 3 "$B"
# A's lease is no longer renewed: the daemon's reclaim task must release the
# canonical seat on its own (spec §9.3 自动移交), advancing the epoch.
wait_driver "-" 4 "$B"

say "== driver reclaim after daemon restart =="
# A's lease expired in the previous phase, so use a fresh client to hold the
# seat across the restart.
RESTART_HOLDER="$(open_client smoke-restart)"
CLAIM_RESTART="$(driver "$RESTART_HOLDER" claim)"
[ "$(printf '%s' "$CLAIM_RESTART" | json_get "['reason']")" = "claim_requested" ] \
    || fail "claim before restart"
wait_driver "$RESTART_HOLDER" 5 "$RESTART_HOLDER"
stop_daemon
start_daemon
# A's lease did not survive the restart. The persisted scan list must reclaim
# the seat on its own, with no client touching the session first.
OBSERVER="$(open_client smoke-observer)"
wait_driver "-" 6 "$OBSERVER"

say "== snapshot_missing probe =="
# A session that has an identity but no committed canonical log cannot mint a
# snapshot cursor: the route must report the documented `snapshot_missing`.
EMPTY_SEED="smoke-empty-$$"
PROBE_CLIENT="$(open_client smoke-probe)"
mkdir -p "$DATA/sessions/$EMPTY_SEED"
cp "$DATA/sessions/$SEED/canonical-identity.json" \
    "$DATA/sessions/$EMPTY_SEED/canonical-identity.json"
MISSING_STATUS="$(curl -sS -o "$DATA/../snapshot-missing.json" -w '%{http_code}' \
    "$ENDPOINT/ringing/v2/sessions/$EMPTY_SEED/bootstrap" \
    -H "authorization: Bearer $TOKEN" -H "x-qaqh-client-session-id: $PROBE_CLIENT")"
[ "$MISSING_STATUS" = "409" ] || fail "snapshot_missing status ($MISSING_STATUS)"
[ "$(cat "$DATA/../snapshot-missing.json" | json_get "['code']")" = "snapshot_missing" ] \
    || fail "snapshot_missing code"

say "PASS: Ringing v2 real-machine smoke test"
