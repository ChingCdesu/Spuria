#!/usr/bin/env bash
# End-to-end smoke test for Spuria.
#
# Boots the signaling server, the relay server, a fake RDP service (an echo
# server) and two clients (host + controller), then pushes bytes through the
# tunnel via the controller's local listener and verifies the echo.
#
# Usage: scripts/smoke-test.sh [p2p|relay|both]   (default: both)
#
# Windows note: cleanup uses `taskkill`; run under Git Bash.
set -uo pipefail
cd "$(dirname "$0")/.."
BIN=target/debug
MODE="${1:-both}"
# Local test fixture only; production servers must use a private random key.
export SPURIA_RELAY_SECRET="spuria-smoke-only-ticket-secret-32-bytes"

PIDS=()
cleanup() {
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  taskkill //F //IM spuria-signaling.exe //IM spuria-relay.exe //IM spuria.exe //IM tcptool.exe >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "building (debug)..."
cargo build --workspace --bins --examples >/dev/null 2>&1 || { echo "BUILD FAILED"; exit 1; }

run_one() {
  local label="$1" extra="$2" msg="$3"
  echo "── $label ──"
  rm -rf data/host data/ctrl ./*.log
  "$BIN/spuria-signaling.exe" --secret test --relay-addr 127.0.0.1:21118 >sig.log 2>&1 & PIDS+=($!)
  "$BIN/spuria-relay.exe" >relay.log 2>&1 & PIDS+=($!)
  "$BIN/examples/tcptool.exe" echo 127.0.0.1:3390 >echo.log 2>&1 & PIDS+=($!)
  sleep 1
  # shellcheck disable=SC2086
  "$BIN/spuria.exe" --data-dir data/host --device-id 111111111 --secret test $extra host --rdp 127.0.0.1:3390 >host.log 2>&1 & PIDS+=($!)
  for _ in $(seq 1 30); do grep -q "registered with signaling" host.log && break; sleep 0.3; done
  # shellcheck disable=SC2086
  "$BIN/spuria.exe" --data-dir data/ctrl --device-id 222222222 --secret test $extra control 111111111 --listen 127.0.0.1:33389 >ctrl.log 2>&1 & PIDS+=($!)
  for _ in $(seq 1 40); do grep -q "RDP tunnel READY" ctrl.log && break; sleep 0.3; done
  sleep 1
  "$BIN/examples/tcptool.exe" probe 127.0.0.1:33389 "$msg"
  local rc=$?
  taskkill //F //IM spuria-signaling.exe //IM spuria-relay.exe //IM spuria.exe //IM tcptool.exe >/dev/null 2>&1 || true
  PIDS=()
  sleep 0.5
  return $rc
}

rc=0
if [ "$MODE" = "p2p" ] || [ "$MODE" = "both" ]; then run_one "P2P (QUIC)"   ""             "hello-spuria-p2p"   || rc=1; fi
if [ "$MODE" = "relay" ] || [ "$MODE" = "both" ]; then run_one "Relay (Noise)" "--force-relay" "hello-spuria-relay" || rc=1; fi

if [ $rc -eq 0 ]; then echo "✅ ALL SMOKE TESTS PASSED"; else echo "❌ SMOKE TEST FAILURES"; fi
exit $rc
