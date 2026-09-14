#!/usr/bin/env bash
# Smoke-test a locally built ph-reactor image: the daemon must come up
# headless (no D-Bus, no tray), serve /api/status, and report a peer id.
set -euo pipefail

IMAGE="${1:-ph-reactor:dev}"
NAME="ph-reactor-smoke-$$"
STATE="$(mktemp -d)"

# The daemon runs as uid 65532 and creates subdirectories (run/, docs/, logs/)
# the invoking user cannot unlink, so the tree is removed from inside a
# container as root. Every step is best-effort, and the test's own exit status
# is preserved -- otherwise a cleanup hiccup would mask a PASS.
cleanup() {
  local rc=$?
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  docker run --rm -v "$STATE:/s" alpine:3.20 \
    sh -c 'rm -rf /s/..?* /s/.[!.]* /s/*' >/dev/null 2>&1 || true
  rm -rf "$STATE" >/dev/null 2>&1 || true
  return "$rc"
}
trap cleanup EXIT

# The container runs as uid 65532; give it a writable state dir without sudo.
chmod 777 "$STATE"
cat > "$STATE/config.json" <<'JSON'
{
  "schemaVersion": 2,
  "instance": { "name": "smoke", "listen": "/ip4/0.0.0.0/tcp/25422" },
  "p2p": { "mdns": false, "dht": true, "relay": true, "bootstraps": [] },
  "drives": [],
  "settings": { "host": "0.0.0.0", "port": 4002 },
  "logLevel": "info"
}
JSON
chmod 666 "$STATE/config.json"

docker run -d --name "$NAME" \
  -v "$STATE:/var/lib/ph-reactor" \
  -p 14002:4002 -p 15422:25422 \
  "$IMAGE" >/dev/null

for i in $(seq 1 30); do
  if curl -fsS http://127.0.0.1:14002/api/status >/dev/null 2>&1; then break; fi
  if [ "$i" = 30 ]; then
    echo "FAIL: /api/status never answered" >&2
    docker logs "$NAME" >&2
    exit 1
  fi
  sleep 1
done

BODY="$(curl -fsS http://127.0.0.1:14002/api/status)"
PEER="$(echo "$BODY" | jq -r '.reactor.peer_id // empty')"
LISTEN="$(echo "$BODY" | jq -r '.reactor.listen // empty')"

[ -n "$PEER" ] || { echo "FAIL: no peer_id in /api/status" >&2; echo "$BODY" >&2; exit 1; }
case "$PEER" in 12D3Koo*) ;; *) echo "FAIL: peer_id not an ed25519 peer id: $PEER" >&2; exit 1;; esac
[ "$LISTEN" = "/ip4/0.0.0.0/tcp/25422" ] || { echo "FAIL: listen=$LISTEN, want /ip4/0.0.0.0/tcp/25422" >&2; exit 1; }

# The identity must have landed on the mounted state dir at mode 0600.
[ -f "$STATE/key" ] || { echo "FAIL: no identity key written" >&2; exit 1; }
[ "$(stat -c '%a' "$STATE/key")" = "600" ] || { echo "FAIL: key mode $(stat -c '%a' "$STATE/key"), want 600" >&2; exit 1; }
[ "$(stat -c '%s' "$STATE/key")" = "32" ] || { echo "FAIL: key size $(stat -c '%s' "$STATE/key"), want 32" >&2; exit 1; }

# The tray must be absent-but-fatal-free: the daemon is still serving.
if docker logs "$NAME" 2>&1 | grep -qi 'panic'; then
  echo "FAIL: panic in logs" >&2
  docker logs "$NAME" >&2
  exit 1
fi

echo "PASS: peer=$PEER listen=$LISTEN"
