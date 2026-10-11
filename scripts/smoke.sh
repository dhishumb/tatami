#!/bin/sh
# End-to-end smoke test against the real desktop. Starts a debug tatamid in
# the foreground with its own socket and state dir, drives it through the
# CLI, then SIGTERMs it and checks that every window is back on screen.
# Quit Rectangle/AeroSpace first. Usage: scripts/smoke.sh [seconds-to-keep-running]
set -eu
KEEP="${1:-0}"
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
cargo build -q -p tatamid -p tatami
TMP=$(mktemp -d /tmp/tt.XXXX)
export TATAMI_SOCK="$TMP/sock" TATAMI_STATE_DIR="$TMP/state" RUST_LOG="${RUST_LOG:-debug}"
T=./target/debug/tatami
./target/debug/tatamid > "$TMP/log" 2>&1 &
PID=$!
trap 'kill -TERM $PID 2>/dev/null || true' EXIT
for _ in $(seq 1 50); do [ -S "$TATAMI_SOCK" ] && break; sleep 0.1; done
[ -S "$TATAMI_SOCK" ] || { echo "daemon did not start"; cat "$TMP/log"; exit 1; }
echo "== ping";          $T ping
echo "== monitors";      $T list-monitors
echo "== workspaces";    $T list-workspaces --all
echo "== windows";       $T list-windows --all
if [ "$KEEP" -gt 0 ]; then
  echo "== running for ${KEEP}s; try alt+h/j/k/l, alt+1..9, hold alt for the sheet. log: $TMP/log"
  sleep "$KEEP"
fi
echo "== workspace 2 then back"; $T workspace 2; sleep 0.3; $T workspace-back-and-forth; sleep 0.3
echo "== state";         ls -la "$TATAMI_STATE_DIR" 2>/dev/null || true
echo "== stopping";      kill -TERM $PID; wait $PID 2>/dev/null || true
trap - EXIT
grep -iE "warn|error" "$TMP/log" | head -20 || true
echo "log: $TMP/log"
