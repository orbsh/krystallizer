#!/usr/bin/env bash
# A REAL-BUCKET rehearsal of the lake (krystallizer PLAN, `## Open`): publish an interval of a live
# store into an S3 bucket, restore it back into an empty directory, and require the restored store to
# answer with the same rows. The directory form is covered by tests (`delta_readable.rs`); what has
# never run is the part where the target is a BUCKET.
#
# Why a script and not a test: it needs credentials and a running S3, which are properties of a
# machine, not of the code. A test that skipped whenever they were absent would be a test that never
# runs.
#
# Usage:
#   K10R_S3_KEY=... K10R_S3_SECRET=... scripts/lake-s3-rehearsal.sh
#
# Optional environment:
#   K10R_S3_ENDPOINT   default http://127.0.0.1:9000
#   K10R_S3_BUCKET     default krystallizer          (must already exist)
#   K10R_S3_REGION     default us-east-1
#   KRYSTALLIZER_BIN   default target/debug/krystallizer
#   KEEP_LOGS=1        keep the temp directory and print its path
#
# What it asserts, in order:
#   1. the service STARTS with the bucket as its lake (a credential or endpoint it cannot use is a
#      start failure by declaration — config/krystallizer.kdl: "a service that believes it is
#      publishing and is not is worse than one that refuses to start")
#   2. rows written through the HTTP door come back through `--restore` into an empty directory
#   3. the restored store answers the same thing as the original (per-endpoint JSON compare, with
#      the store's own clock fields dropped — a restore re-lands rows, it does not re-time them)
set -uo pipefail

REPO=$(cd "$(dirname "$0")/.." && pwd)
BIN=${KRYSTALLIZER_BIN:-$REPO/target/debug/krystallizer}
ENDPOINT=${K10R_S3_ENDPOINT:-http://127.0.0.1:9000}
BUCKET=${K10R_S3_BUCKET:-krystallizer}
REGION=${K10R_S3_REGION:-us-east-1}
PORT=${K10R_PORT:-7791}
RESTORED_PORT=$((PORT + 1))
INTERVAL=${K10R_INTERVAL_SECS:-2}

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

[ -x "$BIN" ] || fail "no krystallizer binary at $BIN (cargo build -p mem-server --bin krystallizer)"

STAMP=$(date +%s)
PREFIX="$BUCKET/k10r-rehearsal-$STAMP"
# A bucket by default; a directory when one is named, which is how this script itself is exercised
# on a machine without a credential (and the directory form is already covered by tests — the point
# of running it twice is to prove the SCRIPT is not what breaks).
TARGET=${K10R_LAKE_TARGET:-s3://$PREFIX}
case "$TARGET" in s3://*) BUCKETED=1 ;; *) BUCKETED=0 ;; esac
if [ "$BUCKETED" = 1 ]; then
    [ -n "${K10R_S3_KEY:-}" ] || fail "K10R_S3_KEY is not set (a bucket needs a real credential)"
    [ -n "${K10R_S3_SECRET:-}" ] || fail "K10R_S3_SECRET is not set (a bucket needs a real credential)"
fi
ROOT=$(mktemp -d "${TMPDIR:-/tmp}/k10r-lake-XXXXXX")
LIVE="$ROOT/live"
RESTORED="$ROOT/restored"
mkdir -p "$LIVE" "$RESTORED"

cleanup() {
    local code=$?
    [ -n "${LIVE_PID:-}" ] && kill "$LIVE_PID" 2>/dev/null
    [ -n "${RESTORED_PID:-}" ] && kill "$RESTORED_PID" 2>/dev/null
    wait 2>/dev/null
    if [ "$code" -ne 0 ] || [ "${KEEP_LOGS:-0}" = 1 ]; then
        echo "logs kept in $ROOT" >&2
    else
        rm -rf "$ROOT"
    fi
    exit "$code"
}
trap cleanup EXIT INT TERM

# The deployment's own config, with two things changed: where the store lives, and the lake. Reading
# the real file rather than restating it is what keeps this rehearsal about the lake — a hand-written
# minimal config drifts the moment the service grows a required section.
write_config() {
    sed "s|^\s*data-dir .*|    data-dir \"$2\"|" "$REPO/config/krystallizer.kdl" >"$1"
    grep -q "data-dir \"$2\"" "$1" || fail "could not point $1 at $2 (no data-dir line in the template?)"
    {
        cat <<KDL
lake {
    target "$TARGET"
    interval-secs $INTERVAL
KDL
        if [ "$BUCKETED" = 1 ]; then
            cat <<KDL
    s3 {
        endpoint "$ENDPOINT"
        region "$REGION"
        key-env "K10R_S3_KEY"
        secret-env "K10R_S3_SECRET"
    }
KDL
        fi
        echo "}"
    } >>"$1"
}

echo "lake: $TARGET (endpoint $ENDPOINT, interval ${INTERVAL}s)"

# 1. the live store publishes itself
write_config "$ROOT/live.kdl" "$LIVE"
"$BIN" --config "$ROOT/live.kdl" --addr "127.0.0.1:$PORT" >"$ROOT/live.log" 2>&1 &
LIVE_PID=$!
for _ in $(seq 1 100); do
    curl -sf -o /dev/null "http://127.0.0.1:$PORT/channels/1/messages" && break
    sleep 0.2
done
kill -0 "$LIVE_PID" 2>/dev/null || fail "the service did not start: $(tail -5 "$ROOT/live.log")"

post() { # path, body
    curl -sf -X POST "http://127.0.0.1:$PORT$1" -H 'content-type: application/json' -d "$2" >/dev/null ||
        fail "POST $1 refused: $2"
}
post /channels/7/messages '{"ts_ms":1000,"sub":0,"turn_id":1,"role":0,"kind":0,"content":"the first line"}'
post /channels/7/messages '{"ts_ms":1001,"sub":0,"turn_id":1,"role":1,"kind":0,"content":"and the answer"}'
post /channels/9/messages '{"ts_ms":2000,"sub":0,"turn_id":1,"role":0,"kind":0,"content":"another line"}'
post /channels/7/members '{"member_id":"6f1d3d0e-0000-4000-8000-0000000000aa","display_name":"Alice","role":0,"since_ms":10}'

# The interval must actually fire: one wait, then a second cycle so a version that only ever lands
# the FIRST snapshot would still be caught by the row counts below.
sleep $((INTERVAL + 1))
post /channels/7/messages '{"ts_ms":1500,"sub":0,"turn_id":2,"role":0,"kind":0,"content":"after the first interval"}'
sleep $((INTERVAL + 1))

# Snapshot what the live store ANSWERS while it is still up: the second half of this rehearsal runs
# after it is gone, and comparing against a process that has been killed proves nothing.
for path in channels/7/messages channels/9/messages channels/7/members members; do
    out="$ROOT/live-$(echo "$path" | tr '/' '-').json"
    curl -sf "http://127.0.0.1:$PORT/$path" -o "$out" || fail "GET /$path failed"
done

kill "$LIVE_PID" 2>/dev/null
wait "$LIVE_PID" 2>/dev/null
LIVE_PID=
grep -qiE 'lake|publish|delta' "$ROOT/live.log" || echo "note: the boot log says nothing about the lake" >&2

# 2. the restore reads the bucket back into an empty directory
write_config "$ROOT/restore.kdl" "$RESTORED"
"$BIN" --config "$ROOT/restore.kdl" --restore "$TARGET" >"$ROOT/restore.log" 2>&1 ||
    fail "restore from $TARGET failed: $(tail -10 "$ROOT/restore.log")"

# 3. the restored store answers like the original
"$BIN" --config "$ROOT/restore.kdl" --addr "127.0.0.1:$RESTORED_PORT" >"$ROOT/restored.log" 2>&1 &
RESTORED_PID=$!
for _ in $(seq 1 100); do
    curl -sf -o /dev/null "http://127.0.0.1:$RESTORED_PORT/channels/1/messages" && break
    sleep 0.2
done
kill -0 "$RESTORED_PID" 2>/dev/null || fail "the restored store did not start: $(tail -5 "$ROOT/restored.log")"

python3 - "$ROOT" "$RESTORED_PORT" <<'PY' || exit 1
import json
import sys
import urllib.request

snapshot, restored = sys.argv[1], sys.argv[2]
# Fields the restore is allowed to answer differently: the store re-lands rows, so a clock it
# assigns on write is its own. Rows' own keys are NOT in this list — a restore that re-times a
# record has changed the record.
VOLATILE = {"read_at", "ts_ms_observed", "observed_ms"}


def get(port, path):
    with urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=10) as resp:
        return json.load(resp)


def snap(path):
    name = path.strip("/").replace("/", "-")
    with open(f"{snapshot}/live-{name}.json") as handle:
        return json.load(handle)


def strip(value):
    if isinstance(value, dict):
        return {k: strip(v) for k, v in value.items() if k not in VOLATILE}
    if isinstance(value, list):
        return [strip(v) for v in value]
    return value


paths = ["/channels/7/messages", "/channels/9/messages", "/channels/7/members", "/members"]
total = 0
for path in paths:
    a, b = strip(snap(path)), strip(get(restored, path))
    if a != b:
        raise SystemExit(f"FAIL: {path} differs after restore\nlive:     {a}\nrestored: {b}")
    if isinstance(b, list):
        total += len(b)

rows = strip(get(restored, "/channels/7/messages"))
if len(rows) != 3:
    raise SystemExit(f"FAIL: the restored channel 7 has {len(rows)} rows, expected 3: {rows}")
print(f"ok: {total} records came back through the lake and every endpoint answers the same")
PY

echo "PASS (lake: $TARGET)"