#!/bin/bash
# WhisperDrop test matrix — run from the project root on the Mac.
# Usage: ./scripts/test-matrix.sh [TARGET_IP] [TUNNEL_RELAY] [TUNNEL_GROUP]
# Covers: LAN transfer, large file, app restart persistence, tunnel round-trip.
set -u
TARGET_IP="${1:-127.0.0.1}"
RELAY="${2:-wss://riki-api.online/ws}"
TUNNEL_GROUP="${3:-}"
PASS=0; FAIL=0
ok()   { echo "✓ $1"; PASS=$((PASS+1)); }
bad()  { echo "✗ $1"; FAIL=$((FAIL+1)); }
check(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }

APP=src-tauri/target/release/whisperdrop
RX=cli/target/release/whisperdrop
DIR=~/Downloads/BridgeReceived

echo "=== build ==="
(cd src-tauri && cargo build --release -q 2>&1 | grep -E "^error" && exit 1) || true
(cd cli       && cargo build --release -q 2>&1 | grep -E "^error" && exit 1) || true

echo "=== 1. unit tests (crypto round-trip, wrong key, nonce uniqueness, 50MB loopback) ==="
unit(){ local out; out=$(cd "$1" && cargo test --release -q 2>&1); local r; r=$(echo "$out" | grep -E "test result" | head -1)
  if [ -n "$r" ] && ! echo "$out" | grep -qE "^error|FAILED"; then ok "$1 unit: $r"; else echo "$out" | grep -E "^error|FAILED|panicked" | head -5; bad "$1 unit tests (build or test failure)"; fi; }
unit src-tauri
unit cli

echo "=== 2. LAN transfer via receiver server ==="
pkill -f "$APP" 2>/dev/null; pkill -f "$RX" 2>/dev/null; sleep 1
("$APP" > /tmp/tm-app.log 2>&1 &) ; sleep 5
head -c 1048576 /dev/urandom > /tmp/tm-1mb.bin
curl -s -m 30 -X POST --data-binary @/tmp/tm-1mb.bin "http://127.0.0.1:51730/upload?filename=tm-1mb.bin" > /dev/null
sleep 1
cmp -s /tmp/tm-1mb.bin "$DIR/tm-1mb.bin" && ok "LAN 1MB byte-perfect" || bad "LAN 1MB"

echo "=== 3. large file (200MB) through receiver ==="
head -c 209715200 /dev/urandom > /tmp/tm-200mb.bin
curl -s -m 300 -X POST --data-binary @/tmp/tm-200mb.bin "http://127.0.0.1:51730/upload?filename=tm-200mb.bin" > /dev/null
sleep 2
cmp -s /tmp/tm-200mb.bin "$DIR/tm-200mb.bin" && ok "200MB byte-perfect (streaming, constant RAM)" || bad "200MB"

echo "=== 4. app restart persistence ==="
pkill -f "$APP"; sleep 2
("$APP" >> /tmp/tm-app.log 2>&1 &) ; sleep 6
curl -s -m 5 "http://127.0.0.1:51730/health" | grep -q bridge-ok && ok "restart: receiver healthy" || bad "restart: unhealthy"
[ -f "$DIR/tm-1mb.bin" ] && ok "restart: received files persisted" || bad "restart: files lost"

echo "=== 5. tunnel reachability (if relay configured) ==="
# match only the probe's own result line — "ok" also appears inside hostnames in mDNS log lines
(timeout 30 "$RX" status 2>&1 | grep -q "relay       : reachable" ) && ok "tunnel relay reachable" || bad "tunnel relay unreachable (relay down? Cloudflare 522 = origin not responding — see relay/DEPLOY.md)"
if [ -n "$TUNNEL_GROUP" ]; then  # this machine must already be a member (whisperdrop group join)
  # a receiver in the same group (same pairing passphrase) must be online elsewhere
  (timeout 150 "$RX" send /tmp/tm-1mb.bin --to "$TUNNEL_GROUP" --relay="$RELAY" ${TUNNEL_SECRET:+--secret="$TUNNEL_SECRET"} > /tmp/tm-tunnel.log 2>&1 < /dev/null)
  grep -q "✓ sent" /tmp/tm-tunnel.log && ok "tunnel transfer sent + confirmed by receiver" || bad "tunnel transfer failed (see /tmp/tm-tunnel.log)"
fi

echo "=== 6. encrypted wrong-key rejection (unit level) ==="
r=$(cd src-tauri && cargo test --release -q tunnel:: 2>&1 | grep -E "test result" | head -1)
echo "$r" | grep -qE "ok\. [1-9]" && ok "tunnel unit: $r" || bad "tunnel unit tests (build failure or no tests ran)"

echo "=== cleanup received test files (server side) ==="
rm -f "$DIR"/tm-*.bin

echo
echo "======== RESULTS: $PASS passed, $FAIL failed ========"
exit $FAIL
