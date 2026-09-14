# ph-reactor Kubernetes Bootstrap Node Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run ph-reactor as an always-on, publicly reachable bootstrap node in the Powerhouse Kubernetes cluster, with a permanent peer id and multiaddr that every other reactor can be pointed at once and forever.

**Architecture:** Phase 1 ships the existing Rust binary unmodified. A distroless image wrapping the static musl binary that CI already builds runs as a single-replica StatefulSet in the GitOps-managed cluster. Its ed25519 identity comes from OpenBao (not from the PVC) so the peer id survives volume loss. A dedicated Hetzner load balancer exposes TCP 25422 to the internet; the unauthenticated console stays ClusterIP-only.

**Tech Stack:** Rust 1.83 (musl static), Docker/distroless, Kubernetes (k3s 1.36), ArgoCD, External Secrets Operator, OpenBao, Hetzner CCM, libp2p.

**Spec:** `docs/superpowers/specs/2026-09-14-k8s-bootstrap-deployment-design.md`

## Global Constraints

- **Two repos.** `ph-reactor` = `/home/f/projects/ph-reactor`. `powerhouse-k8s-hosting` = `/home/f/projects/powerhouse-k8s-hosting`. Each task names its repo. Commit separately in each.
- **No Rust source changes in phase 1.** If a task appears to need one, stop and escalate — it means the plan is wrong.
- **libp2p listen port: `25422`** (TCP). Never 4201 in any cluster artifact.
- **Console port: `4002`**, bound `0.0.0.0` *inside the pod only*. Never an Ingress, never a LoadBalancer.
- **PROXY protocol must be OFF** on the p2p Service. Do not copy `load-balancer.hetzner.cloud/uses-proxyprotocol` from the Traefik LB — a PROXY header corrupts the libp2p Noise handshake.
- **Namespace:** `ph-reactor`.
- **Image:** `cr.vetra.io/powerhouse-inc-powerhouse/ph-reactor`.
- **OpenBao path:** `powerhouse/shared/ph-reactor-bootstrap`, property `key` = base64 of the 32 raw identity bytes.
- **StorageClass:** `hcloud-volumes`, 10Gi, ReadWriteOnce.
- **DNS name:** `reactor.vetra.io`.
- **Non-root:** uid/gid `65532` everywhere; `fsGroup: 65532` so the PVC is writable.
- **OpenBao access:** `export BAO_ADDR=https://openbao.vetra.io`. Token in `~/.vault-token`, expires 2026-09-15T07:38Z — re-auth with `bao login -method=userpass username=frank` if commands start returning 403.
- **Never commit the identity key, the Harbor password, or any OpenBao value** into either repo.

---

### Task 1: Container image

**Repo:** `ph-reactor`

**Files:**
- Create: `Dockerfile`
- Create: `.dockerignore`
- Create: `scripts/build-image.sh`
- Create: `scripts/smoke-image.sh`

**Interfaces:**
- Consumes: nothing.
- Produces: a local image tagged `ph-reactor:dev` whose entrypoint is `/usr/local/bin/ph-reactor` running in the foreground, state dir `/var/lib/ph-reactor`, serving `GET /api/status` on port 4002 and listening for libp2p on 25422. Later tasks reference the image path `cr.vetra.io/powerhouse-inc-powerhouse/ph-reactor`.

- [ ] **Step 1: Write the failing smoke test**

Create `scripts/smoke-image.sh`:

```bash
#!/usr/bin/env bash
# Smoke-test a locally built ph-reactor image: the daemon must come up
# headless (no D-Bus, no tray), serve /api/status, and report a peer id.
set -euo pipefail

IMAGE="${1:-ph-reactor:dev}"
NAME="ph-reactor-smoke-$$"
STATE="$(mktemp -d)"
cleanup() { docker rm -f "$NAME" >/dev/null 2>&1 || true; rm -rf "$STATE"; }
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
docker logs "$NAME" 2>&1 | grep -qi 'panic' && { echo "FAIL: panic in logs" >&2; exit 1; }

echo "PASS: peer=$PEER listen=$LISTEN"
```

```bash
chmod +x scripts/smoke-image.sh
```

- [ ] **Step 2: Run it to verify it fails**

Run: `./scripts/smoke-image.sh ph-reactor:dev`
Expected: FAIL — `Unable to find image 'ph-reactor:dev' locally` / docker run errors, because no image exists yet.

- [ ] **Step 3: Write the `.dockerignore`**

Create `.dockerignore`:

```
target/
.git/
.github/
docs/
tests/
snap/
console/
views/
*.md
```

Note `dist/` is deliberately NOT ignored — the binary is copied from there.

- [ ] **Step 4: Write the Dockerfile**

Create `Dockerfile`:

```dockerfile
# ph-reactor container image.
#
# Deliberately NOT a from-source build: the release workflow already produces
# a fully static x86_64-musl binary, and reusing that exact artifact means the
# snap, the GitHub release asset and this image are bit-identical -- one
# binary, one sha256. Build it first (scripts/build-image.sh does both).
#
# Base is distroless/static, not scratch. TLS roots are not the reason:
# Cargo.lock pins webpki-roots rather than rustls-native-certs, so reqwest's
# CA bundle is compiled into the binary. The base is here for the `nonroot`
# uid in /etc/passwd, a writable /tmp, and a layer that receives CVE patches.

FROM alpine:3.20 AS prep
# distroless has no shell, so the state dir and its ownership are staged here.
RUN mkdir -p /var/lib/ph-reactor \
 && chown 65532:65532 /var/lib/ph-reactor \
 && chmod 700 /var/lib/ph-reactor

FROM gcr.io/distroless/static-debian12:nonroot

COPY --from=prep --chown=65532:65532 /var/lib/ph-reactor /var/lib/ph-reactor
COPY --chown=65532:65532 dist/ph-reactor /usr/local/bin/ph-reactor

ENV PH_REACTOR_STATE_DIR=/var/lib/ph-reactor

USER 65532:65532
WORKDIR /var/lib/ph-reactor

# libp2p (see docs: IANA-unassigned, RFC 6335 User range, below the Linux
# ephemeral floor) and the console. The console is ClusterIP-only in k8s --
# its API is unauthenticated by design.
EXPOSE 25422/tcp
EXPOSE 4002/tcp

# Foreground, never --daemonize: Kubernetes is the supervisor, so the fork
# and pidfile path is bypassed entirely. With no D-Bus session bus the tray
# self-disables, which is the documented headless path.
ENTRYPOINT ["/usr/local/bin/ph-reactor"]
CMD ["run"]
```

- [ ] **Step 5: Write the build script**

Create `scripts/build-image.sh`:

```bash
#!/usr/bin/env bash
# Build the ph-reactor container image from the same static musl binary the
# release workflow ships. Usage: scripts/build-image.sh [tag]
set -euo pipefail

TAG="${1:-ph-reactor:dev}"
cd "$(dirname "$0")/.."

rustup target add x86_64-unknown-linux-musl
CC_x86_64_unknown_linux_musl="${CC_x86_64_unknown_linux_musl:-musl-gcc}" \
  cargo build --release --locked --target x86_64-unknown-linux-musl

mkdir -p dist
cp target/x86_64-unknown-linux-musl/release/ph-reactor dist/ph-reactor

# A dynamically linked binary would fail at runtime on distroless/static.
if command -v file >/dev/null 2>&1; then
  file dist/ph-reactor | grep -q 'statically linked' \
    || { echo "FAIL: dist/ph-reactor is not statically linked" >&2; exit 1; }
fi

docker build -t "$TAG" .
echo "built $TAG"
```

```bash
chmod +x scripts/build-image.sh
```

- [ ] **Step 6: Build the image**

Run: `./scripts/build-image.sh ph-reactor:dev`
Expected: the static-link assertion passes and docker reports `built ph-reactor:dev`.

If `musl-gcc` is missing, install it (`sudo apt-get install -y musl-tools`) — `ring` needs a musl C toolchain, the same reason `release.yml` installs it.

- [ ] **Step 7: Run the smoke test to verify it passes**

Run: `./scripts/smoke-image.sh ph-reactor:dev`
Expected: `PASS: peer=12D3Koo... listen=/ip4/0.0.0.0/tcp/25422`

- [ ] **Step 8: Verify the image is non-root and small**

Run:
```bash
docker image inspect ph-reactor:dev --format '{{.Config.User}} {{.Size}}'
```
Expected: `65532:65532` and a size under 40 MB.

**Implementation notes (amended during execution — the committed scripts are authoritative):**

1. **No host musl toolchain is required.** The dev machine had no `musl-gcc` and no passwordless sudo, so `build-image.sh` falls back to compiling inside a `ph-reactor-builder:1.83-alpine` image (Alpine is natively musl, so this is not cross-compilation). CI still uses the host path via `musl-tools`.
2. **Build caches live inside the bind-mounted repo** (`.build-cache/`), not in separate `-v` mounts. A mount whose host path does not exist yet is created by the Docker daemon as **root**, which the unprivileged build container then cannot write to — this cost one failed build.
3. **The static-link assertion tests the property, not one spelling.** A musl release build reports `static-pie linked`, not `statically linked`; the original `grep 'statically linked'` rejected a perfectly good binary. The check now fails only on `dynamically linked` or an undeterminable result.
4. **`smoke-image.sh` tears down as root in a container** and preserves the test's exit status. The daemon creates `run/`, `docs/` and `logs/` as uid 65532, which the host user cannot unlink, and the trap's failure was masking a PASS as `exit=1`.
5. **`.dockerignore` does not exclude source directories.** `views` is a cargo workspace member; excluding it would silently break the build if this ever becomes a from-source Dockerfile.

Verified result: `PASS: peer=12D3KooW… listen=/ip4/0.0.0.0/tcp/25422`, image `user=65532:65532`, size 36.6 MB.

- [ ] **Step 9: Commit**

```bash
git add Dockerfile .dockerignore .gitignore scripts/build-image.sh scripts/smoke-image.sh
git commit -m "build: container image for running ph-reactor headless

Wraps the same static musl binary the release workflow already ships, so
the snap, the release asset and the image are one artifact. distroless
static for the nonroot uid and CVE patching -- not for CA certs, which
are compiled in via webpki-roots.

Runs in the foreground: Kubernetes is the supervisor, so the daemonize
and pidfile path is bypassed. The tray self-disables with no session bus.

scripts/smoke-image.sh asserts the daemon serves /api/status, reports an
ed25519 peer id, listens on 25422 and writes a 0600 32-byte identity.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Push the image to Harbor from CI

**Repo:** `ph-reactor`

**Files:**
- Create: `.github/workflows/image.yml`

**Interfaces:**
- Consumes: the `Dockerfile` and `scripts/build-image.sh` from Task 1.
- Produces: images at `cr.vetra.io/powerhouse-inc-powerhouse/ph-reactor:<version>` and `:sha-<short>` on every `ph-reactor-v*` tag. Task 5's StatefulSet pins one of these tags.

**Prerequisite (human action, cannot be automated from here):** add repository secrets `HARBOR_USERNAME` and `HARBOR_PASSWORD` in GitHub. The values are the same robot account already in OpenBao — read them with:

```bash
export BAO_ADDR=https://openbao.vetra.io
bao kv get -mount=kv -format=json powerhouse/shared/harbor-credentials | jq -r '.data.data | "user=\(.username)"'
```

Do not print the password to a shared terminal; copy it into the GitHub secret directly.

- [ ] **Step 1: Write the workflow**

Create `.github/workflows/image.yml`:

```yaml
# Container image builds for ph-reactor.
#
# Runs on the same `ph-reactor-v<semver>` tags as release.yml and builds the
# identical static musl binary, so the image, the GitHub release asset and
# the snap all wrap one artifact.
#
# Pushes to Harbor (cr.vetra.io), which is where every workload in the
# powerhouse-k8s-hosting cluster pulls from.
name: Image

on:
  push:
    tags:
      - "ph-reactor-v*"
  workflow_dispatch:

permissions:
  contents: read

jobs:
  image:
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4

      - name: Install Rust (stable, musl target) + the musl C toolchain
        run: |
          rustup toolchain install stable
          rustup target add x86_64-unknown-linux-musl
          sudo apt-get update
          sudo apt-get install -y musl-tools

      - name: Build the static binary
        env:
          # cc-rs builds ring for the musl target; point it at musl-gcc so the
          # binary stays fully static (same rationale as release.yml).
          CC_x86_64_unknown_linux_musl: musl-gcc
        run: |
          cargo build --release --locked --target x86_64-unknown-linux-musl
          mkdir -p dist
          cp target/x86_64-unknown-linux-musl/release/ph-reactor dist/ph-reactor
          file dist/ph-reactor | grep -q 'statically linked'

      - name: Derive image tags
        id: tags
        run: |
          version="${GITHUB_REF_NAME#ph-reactor-v}"
          if [ "$version" = "$GITHUB_REF_NAME" ]; then version="0.0.0-dev"; fi
          echo "version=$version" >> "$GITHUB_OUTPUT"
          echo "sha=sha-$(git rev-parse --short HEAD)" >> "$GITHUB_OUTPUT"

      - name: Log in to Harbor
        uses: docker/login-action@v3
        with:
          registry: cr.vetra.io
          username: ${{ secrets.HARBOR_USERNAME }}
          password: ${{ secrets.HARBOR_PASSWORD }}

      - name: Build and push
        uses: docker/build-push-action@v6
        with:
          context: .
          push: true
          tags: |
            cr.vetra.io/powerhouse-inc-powerhouse/ph-reactor:${{ steps.tags.outputs.version }}
            cr.vetra.io/powerhouse-inc-powerhouse/ph-reactor:${{ steps.tags.outputs.sha }}
          cache-from: type=gha
          cache-to: type=gha,mode=max
```

- [ ] **Step 2: Validate the workflow parses**

Run:
```bash
python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/image.yml')); print('YAML OK')"
```
Expected: `YAML OK`

- [ ] **Step 3: Verify the tag-derivation logic in isolation**

Run:
```bash
GITHUB_REF_NAME=ph-reactor-v1.0.0 bash -c 'version="${GITHUB_REF_NAME#ph-reactor-v}"; [ "$version" = "1.0.0" ] && echo "PASS tag strip" || { echo FAIL; exit 1; }'
GITHUB_REF_NAME=main bash -c 'version="${GITHUB_REF_NAME#ph-reactor-v}"; if [ "$version" = "$GITHUB_REF_NAME" ]; then version="0.0.0-dev"; fi; [ "$version" = "0.0.0-dev" ] && echo "PASS dispatch fallback" || { echo FAIL; exit 1; }'
```
Expected: `PASS tag strip` then `PASS dispatch fallback`. This matters because `workflow_dispatch` has no tag and would otherwise push an image tagged `main`.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/image.yml
git commit -m "ci: build and push the ph-reactor image to Harbor

Same tag trigger and same musl binary as release.yml, so the image wraps
the identical artifact. Tags <version> and sha-<short>; workflow_dispatch
falls back to 0.0.0-dev rather than pushing a tag named after a branch.

Requires HARBOR_USERNAME / HARBOR_PASSWORD repository secrets.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Mint the bootstrap identity and store it in OpenBao

**Repo:** none — this is an operational task producing a secret and a fact. Nothing is committed.

**Files:** none.

**Interfaces:**
- Consumes: the `ph-reactor` binary built in Task 1 (`target/x86_64-unknown-linux-musl/release/ph-reactor`, or a plain `cargo build --release` binary — either derives the same peer id from the same key).
- Produces: **the bootstrap peer id** (a `12D3Koo…` base58 string) and an OpenBao entry at `powerhouse/shared/ph-reactor-bootstrap` with property `key`. Tasks 4, 7 and 8 all need the peer id; record it in the task's completion note.

**Why before the deploy:** the peer id is the bootstrap contract. Minting it first means the multiaddr can be written into manifests and docs in the same commits that create them, instead of being discovered afterwards and back-filled.

- [ ] **Step 1: Verify the OpenBao path is empty (do not clobber an existing identity)**

Run:
```bash
export BAO_ADDR=https://openbao.vetra.io
bao kv get -mount=kv powerhouse/shared/ph-reactor-bootstrap 2>&1 | head -5
```
Expected: `No value found at kv/data/powerhouse/shared/ph-reactor-bootstrap`.

**If a value already exists, STOP.** An identity was minted before; skip to Step 6 and use it. Overwriting it would break every node already configured with the old peer id.

- [ ] **Step 2: Generate the identity in a scratch state dir**

`ph-reactor doctor` truncates the peer id to 16 characters, so it cannot be used here. Start the daemon briefly and read the full id from the status API instead:

```bash
export SCRATCH="$(mktemp -d)"
cat > "$SCRATCH/config.json" <<'JSON'
{
  "schemaVersion": 2,
  "instance": { "name": "ph-bootstrap", "listen": "/ip4/127.0.0.1/tcp/25422" },
  "p2p": { "mdns": false, "dht": false, "relay": false, "bootstraps": [] },
  "drives": [],
  "settings": { "host": "127.0.0.1", "port": 14002 },
  "logLevel": "warn"
}
JSON
PH_REACTOR_STATE_DIR="$SCRATCH" ./target/release/ph-reactor run --daemonize
sleep 3
PH_REACTOR_STATE_DIR="$SCRATCH" ./target/release/ph-reactor status --json | jq -r '.reactor.peer_id'
```

Expected: a full `12D3Koo…` peer id (about 52 characters). Record it.

If `./target/release/ph-reactor` does not exist, run `cargo build --release --locked` first.

- [ ] **Step 3: Confirm the key is exactly 32 bytes at mode 0600**

Run:
```bash
stat -c '%s %a' "$SCRATCH/key"
```
Expected: `32 600` — this is what `src/p2p/mod.rs::load_or_create_identity` writes and what it will read back.

- [ ] **Step 4: Stop the scratch daemon**

Run:
```bash
PH_REACTOR_STATE_DIR="$SCRATCH" ./target/release/ph-reactor stop
```
Expected: clean shutdown, no error.

- [ ] **Step 5: Store the key in OpenBao**

```bash
export BAO_ADDR=https://openbao.vetra.io
PEER="$(PH_REACTOR_STATE_DIR="$SCRATCH" ./target/release/ph-reactor status --json | jq -r '.reactor.peer_id')"
bao kv put -mount=kv powerhouse/shared/ph-reactor-bootstrap \
  key="$(base64 -w0 "$SCRATCH/key")" \
  peer_id="$PEER" \
  note="libp2p identity for the k8s bootstrap node; 32-byte ed25519 seed, base64. Peer id is a published contract - never rotate without re-announcing."
```

The `peer_id` property is stored alongside the key purely as documentation, so an operator reading the secret can confirm which node it belongs to without reconstructing it. Only `key` is consumed by the ExternalSecret.

If `status --json` now reports nothing because the daemon is stopped, read the peer id you recorded in Step 2 and pass it literally.

- [ ] **Step 6: Verify the round trip derives the same peer id**

This is the real test of the task — that what is in OpenBao reconstructs the identity the cluster will run with:

```bash
export BAO_ADDR=https://openbao.vetra.io
VERIFY="$(mktemp -d)"
bao kv get -mount=kv -format=json powerhouse/shared/ph-reactor-bootstrap \
  | jq -r '.data.data.key' | base64 -d > "$VERIFY/key"
chmod 600 "$VERIFY/key"
cp "$SCRATCH/config.json" "$VERIFY/config.json"
PH_REACTOR_STATE_DIR="$VERIFY" ./target/release/ph-reactor run --daemonize
sleep 3
ROUNDTRIP="$(PH_REACTOR_STATE_DIR="$VERIFY" ./target/release/ph-reactor status --json | jq -r '.reactor.peer_id')"
PH_REACTOR_STATE_DIR="$VERIFY" ./target/release/ph-reactor stop
echo "roundtrip=$ROUNDTRIP"
[ "$ROUNDTRIP" = "$PEER" ] && echo "PASS: identity round-trips" || { echo "FAIL: $ROUNDTRIP != $PEER"; exit 1; }
```

Expected: `PASS: identity round-trips`

- [ ] **Step 7: Destroy the scratch copies**

```bash
shred -u "$SCRATCH/key" "$VERIFY/key" 2>/dev/null || rm -f "$SCRATCH/key" "$VERIFY/key"
rm -rf "$SCRATCH" "$VERIFY"
```

The only surviving copy of the private key must be the one in OpenBao.

- [ ] **Step 8: Record the peer id for the remaining tasks**

Write it into the task completion note in the form the later tasks need:

```
BOOTSTRAP_PEER_ID=12D3Koo...
```

Nothing is committed in this task.

---

### Task 4: Workload manifests

**Repo:** `powerhouse-k8s-hosting`

**Files:**
- Create: `infrastructure/ph-reactor/00-external-secret-identity.yaml`
- Create: `infrastructure/ph-reactor/01-external-secret-harbor.yaml`
- Create: `infrastructure/ph-reactor/02-configmap.yaml`
- Create: `infrastructure/ph-reactor/03-statefulset.yaml`
- Create: `infrastructure/ph-reactor/validate.sh`

**Interfaces:**
- Consumes: `BOOTSTRAP_PEER_ID` from Task 3 (documentation only in this task); the image tag from Task 2.
- Produces: a StatefulSet named `ph-reactor` in namespace `ph-reactor`, with pod label `app: ph-reactor`. Task 5's Services select on that label.

**Image tag:** use the version tag pushed by Task 2. If Task 2 has not run against a real tag yet, use `0.0.0-dev` and correct it in Task 7's deploy step — but do not use `latest`, which defeats ArgoCD's ability to show drift.

- [ ] **Step 1: Write the failing validation script**

Create `infrastructure/ph-reactor/validate.sh`:

```bash
#!/usr/bin/env bash
# Server-side dry-run of every ph-reactor manifest, plus the invariants that
# are easy to get wrong and expensive to debug in a live mesh.
set -euo pipefail
cd "$(dirname "$0")"

fail=0
note() { echo "FAIL: $*" >&2; fail=1; }

for f in [0-9]*.yaml; do
  kubectl apply --dry-run=server -f "$f" >/dev/null 2>&1 \
    || { kubectl apply --dry-run=server -f "$f" || true; note "$f did not validate"; }
done

# PROXY protocol on the p2p Service prepends a header to the libp2p stream and
# corrupts the Noise handshake with an unhelpful error. It must never appear.
grep -rq 'uses-proxyprotocol' . && note "uses-proxyprotocol present -- it breaks the libp2p handshake"

# The console API is unauthenticated; nothing may publish it.
grep -rq 'kind: Ingress' . && note "an Ingress exists -- the console API has no auth"
for f in [0-9]*.yaml; do
  if grep -q 'type: LoadBalancer' "$f" && grep -q '4002' "$f"; then
    note "$f puts the console port on a LoadBalancer"
  fi
done

# The port is a published contract. Comment lines are excluded so that
# explaining the old default in a comment does not trip the guard.
grep -rn '4201' [0-9]*.yaml | grep -vE ':[0-9]+:[[:space:]]*#' \
  && note "port 4201 found in a live field -- the cluster contract is 25422"

echo "$([ "$fail" = 0 ] && echo PASS || echo FAILED) validate"
exit "$fail"
```

```bash
chmod +x infrastructure/ph-reactor/validate.sh
```

- [ ] **Step 2: Run it to verify it fails**

Run: `./infrastructure/ph-reactor/validate.sh`
Expected: FAIL — no `[0-9]*.yaml` files exist yet (the glob does not match, so the loop errors or no manifests validate).

- [ ] **Step 3: Write the identity ExternalSecret**

Create `infrastructure/ph-reactor/00-external-secret-identity.yaml`:

```yaml
# The bootstrap node's libp2p identity.
#
# This key IS the bootstrap contract: its peer id is published and written
# into every other reactor's p2p.bootstraps. It lives in OpenBao rather than
# being generated on the PVC so that losing the volume -- or moving the node,
# or recreating the namespace -- does not silently invalidate every joining
# node's config.
#
# Stored base64 in OpenBao; decodingStrategy Base64 materialises the raw
# 32-byte ed25519 seed that src/p2p/mod.rs::load_or_create_identity expects.
#
# Sync-wave -5: the Secret must exist before the StatefulSet's init container
# looks for it.
apiVersion: external-secrets.io/v1beta1
kind: ExternalSecret
metadata:
  name: ph-reactor-identity
  namespace: ph-reactor
  labels:
    app: ph-reactor
    component: infrastructure
  annotations:
    argocd.argoproj.io/sync-wave: "-5"
spec:
  refreshInterval: 1h
  secretStoreRef:
    name: openbao
    kind: ClusterSecretStore
  target:
    name: ph-reactor-identity
    creationPolicy: Owner
    # Retain: deleting the ExternalSecret must never destroy the only
    # in-cluster copy of a published identity.
    deletionPolicy: Retain
  data:
    - secretKey: key
      remoteRef:
        key: powerhouse/shared/ph-reactor-bootstrap
        property: key
        decodingStrategy: Base64
```

- [ ] **Step 4: Write the Harbor pull-secret ExternalSecret**

Create `infrastructure/ph-reactor/01-external-secret-harbor.yaml`:

```yaml
# Harbor pull credentials for cr.vetra.io, same shared robot account and same
# dockerconfigjson template as powerhouse-chart/templates/external-secret-harbor.yaml.
# Sync-wave -2: must exist before the StatefulSet pulls.
apiVersion: external-secrets.io/v1beta1
kind: ExternalSecret
metadata:
  name: harbor-credentials
  namespace: ph-reactor
  labels:
    app: ph-reactor
    component: infrastructure
  annotations:
    argocd.argoproj.io/sync-wave: "-2"
spec:
  refreshInterval: 1h
  secretStoreRef:
    name: openbao
    kind: ClusterSecretStore
  target:
    name: harbor-credentials
    creationPolicy: Owner
    template:
      type: kubernetes.io/dockerconfigjson
      data:
        .dockerconfigjson: |
          {{ printf "{\"auths\":{\"%s\":{\"username\":\"%s\",\"password\":\"%s\",\"auth\":\"%s\"}}}" .registry .username .password (printf "%s:%s" .username .password | b64enc) }}
  data:
    - secretKey: registry
      remoteRef:
        key: powerhouse/shared/harbor-credentials
        property: registry
    - secretKey: username
      remoteRef:
        key: powerhouse/shared/harbor-credentials
        property: username
    - secretKey: password
      remoteRef:
        key: powerhouse/shared/harbor-credentials
        property: password
```

- [ ] **Step 5: Write the ConfigMap**

Create `infrastructure/ph-reactor/02-configmap.yaml`:

```yaml
# The reactor's config.json, rendered into the state dir by the init container
# on every pod start -- git is the source of truth.
#
# CONSEQUENCE, deliberate: drives added through the console or via
# `ph-reactor join` do NOT survive a restart. That is correct for a
# GitOps-managed node (declared state wins) but differs from running on a
# laptop, so it is documented here and in docs/ARCHITECTURE.md.
#
# Departures from the laptop defaults, and why:
#   mdns  false  multicast discovery is meaningless across a cluster network
#   dht   true   seeds joining reactors' routing tables -- most of what makes
#                this a *bootstrap* node rather than merely a reachable one
#   relay true   circuit relay forwards connections for peers behind NAT,
#                which is the other half
#   listen 25422 IANA-unassigned, RFC 6335 User range, below the Linux
#                ephemeral floor (32768). Never the ph-reactor default here.
#
# p2p.tokenEnv is deliberately unset: a bootstrap node exists to accept
# connections from nodes that know nobody yet, so a shared-token gate would
# defeat its purpose. Private channel confidentiality continues to rest on the
# group model's auth block, enforced on the store's single apply path.
apiVersion: v1
kind: ConfigMap
metadata:
  name: ph-reactor-config
  namespace: ph-reactor
  labels:
    app: ph-reactor
    component: infrastructure
data:
  config.json: |
    {
      "schemaVersion": 2,
      "instance": {
        "name": "ph-bootstrap",
        "listen": "/ip4/0.0.0.0/tcp/25422"
      },
      "p2p": {
        "mdns": false,
        "dht": true,
        "relay": true,
        "bootstraps": []
      },
      "drives": [],
      "settings": {
        "host": "0.0.0.0",
        "port": 4002
      },
      "logLevel": "info"
    }
```

- [ ] **Step 6: Write the StatefulSet**

Create `infrastructure/ph-reactor/03-statefulset.yaml`:

```yaml
# The Powerhouse bootstrap reactor.
#
# StatefulSet, not Deployment: stable identity, an exclusive volume, and no
# rescheduling that could ever run two reactors against one per-doc WAL.
#
# One replica on purpose. The store is a WAL on a single RWO PVC and the
# identity is a single keypair -- a second replica would be a second *peer*,
# not a failover.
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: ph-reactor
  namespace: ph-reactor
  labels:
    app: ph-reactor
    component: reactor
spec:
  replicas: 1
  serviceName: ph-reactor-console
  selector:
    matchLabels:
      app: ph-reactor
  template:
    metadata:
      labels:
        app: ph-reactor
        component: reactor
      annotations:
        # Roll the pod when the rendered config changes, since the init
        # container only reads the ConfigMap at start.
        reloader.stakater.com/auto: "true"
    spec:
      imagePullSecrets:
        - name: harbor-credentials
      securityContext:
        runAsUser: 65532
        runAsGroup: 65532
        runAsNonRoot: true
        # The PVC is provisioned root-owned; fsGroup makes it writable by the
        # non-root uid the image runs as.
        fsGroup: 65532
      initContainers:
        # distroless has no shell, so the state dir is seeded by a busybox
        # init container rather than an entrypoint script.
        - name: seed-state
          image: busybox:1.36
          command:
            - sh
            - -ec
            - |
              # The identity: 32 raw bytes at 0600, exactly what
              # load_or_create_identity() reads back.
              install -m 0600 /identity/key /var/lib/ph-reactor/key
              # config.json is re-rendered every start: git wins over anything
              # the console wrote into the volume.
              install -m 0600 /cfg/config.json /var/lib/ph-reactor/config.json
              # Fail loudly rather than letting the daemon mint a NEW identity
              # and silently break the published bootstrap contract.
              test "$(stat -c '%s' /var/lib/ph-reactor/key)" = "32"
          securityContext:
            runAsUser: 65532
            runAsGroup: 65532
            runAsNonRoot: true
            allowPrivilegeEscalation: false
            capabilities:
              drop: ["ALL"]
          volumeMounts:
            - name: state
              mountPath: /var/lib/ph-reactor
            - name: identity
              mountPath: /identity
              readOnly: true
            - name: config
              mountPath: /cfg
              readOnly: true
      containers:
        - name: ph-reactor
          image: cr.vetra.io/powerhouse-inc-powerhouse/ph-reactor:0.0.0-dev
          imagePullPolicy: IfNotPresent
          args: ["run"]
          ports:
            - name: p2p
              containerPort: 25422
              protocol: TCP
            - name: console
              containerPort: 4002
              protocol: TCP
          env:
            - name: PH_REACTOR_STATE_DIR
              value: /var/lib/ph-reactor
          securityContext:
            allowPrivilegeEscalation: false
            readOnlyRootFilesystem: true
            capabilities:
              drop: ["ALL"]
          volumeMounts:
            - name: state
              mountPath: /var/lib/ph-reactor
            - name: tmp
              mountPath: /tmp
          readinessProbe:
            # Gates the LoadBalancer backend: a reactor that has not finished
            # opening its store is not advertised to peers.
            httpGet:
              path: /api/status
              port: console
            initialDelaySeconds: 5
            periodSeconds: 10
            timeoutSeconds: 5
            failureThreshold: 6
          livenessProbe:
            httpGet:
              path: /api/status
              port: console
            initialDelaySeconds: 30
            periodSeconds: 30
            timeoutSeconds: 10
            failureThreshold: 5
          resources:
            requests:
              cpu: 100m
              memory: 256Mi
            limits:
              cpu: "2"
              memory: 2Gi
      volumes:
        - name: identity
          secret:
            secretName: ph-reactor-identity
            defaultMode: 0400
        - name: config
          configMap:
            name: ph-reactor-config
        - name: tmp
          emptyDir: {}
  volumeClaimTemplates:
    - metadata:
        name: state
        labels:
          app: ph-reactor
          component: storage
      spec:
        accessModes: ["ReadWriteOnce"]
        storageClassName: hcloud-volumes
        resources:
          requests:
            storage: 10Gi
```

- [ ] **Step 7: Create the namespace so server-side dry-run can resolve it**

Run:
```bash
kubectl create namespace ph-reactor --dry-run=client -o yaml | kubectl apply -f -
```
Expected: `namespace/ph-reactor created` (or `unchanged`). ArgoCD will own it via `CreateNamespace=true` in Task 6; this only unblocks validation.

- [ ] **Step 8: Run the validation script to verify it passes**

Run: `./infrastructure/ph-reactor/validate.sh`
Expected: `PASS validate`

If the two ExternalSecrets fail to validate with `no matches for kind "ExternalSecret"`, confirm the CRD version in the cluster matches: `kubectl get crd externalsecrets.external-secrets.io -o jsonpath='{.spec.versions[*].name}'`.

- [ ] **Step 9: Commit**

```bash
cd /home/f/projects/powerhouse-k8s-hosting
git add infrastructure/ph-reactor/
git commit -m "feat(ph-reactor): workload manifests for the bootstrap reactor

StatefulSet with one replica: the store is a WAL on a single RWO volume
and the identity is one keypair, so a second replica would be a second
peer rather than a failover.

The identity comes from OpenBao, not the PVC. Its peer id is published
and written into every other reactor's p2p.bootstraps, so losing the
volume must not invalidate it. The init container asserts the key is 32
bytes rather than letting the daemon mint a fresh one and silently break
the contract.

config.json is re-rendered from the ConfigMap every start -- git wins.
Consequence documented: console-added drives do not survive a restart.

validate.sh server-side dry-runs every manifest and guards the three
invariants that are cheap to break and expensive to debug: no PROXY
protocol, no Ingress on the unauthenticated console, no port 4201.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: Services and NetworkPolicy

**Repo:** `powerhouse-k8s-hosting`

**Files:**
- Create: `infrastructure/ph-reactor/04-service-p2p.yaml`
- Create: `infrastructure/ph-reactor/05-service-console.yaml`
- Create: `infrastructure/ph-reactor/06-networkpolicy.yaml`

**Interfaces:**
- Consumes: pod label `app: ph-reactor` and the named ports `p2p` / `console` from Task 4's StatefulSet.
- Produces: Service `ph-reactor-p2p` (LoadBalancer, TCP 25422, labels `app: ph-reactor` + `component: p2p`) whose external IP becomes the published multiaddr, and Service `ph-reactor-console` (ClusterIP, 4002) which is also the StatefulSet's `serviceName`. Task 7 Step 8 writes a `CiliumLoadBalancerIPPool` whose `serviceSelector` matches those two p2p labels exactly.

**Expect `EXTERNAL-IP` to stay `<pending>` after this task.** That is normal in this cluster and not a failure: the Hetzner CCM provisions the LB but does not populate Service status. It is fixed in Task 7 Step 8 with a Cilium IP pool, which cannot be written until the LB exists and has an address.

- [ ] **Step 1: Write the p2p LoadBalancer Service**

Create `infrastructure/ph-reactor/04-service-p2p.yaml`:

```yaml
# Public libp2p endpoint for the bootstrap reactor.
#
# A dedicated Hetzner LB (CCM provisions a second lb11, ~EUR 5.4/month).
# use-private-ip routes LB->node traffic over the private network, which is
# why this needs NO Terraform or node-firewall change.
#
# NOTE the annotation that is deliberately ABSENT:
# load-balancer.hetzner.cloud/uses-proxyprotocol. The k3s LB sets it for
# Traefik. A PROXY header prepended to a libp2p stream corrupts the Noise
# handshake and the resulting error says nothing useful. Never add it here.
#
# externalTrafficPolicy Local preserves the peer's real source IP. It matters
# little today, but phase 2's external-address announcing is built on
# identify's observed address, which is wrong under SNAT.
apiVersion: v1
kind: Service
metadata:
  name: ph-reactor-p2p
  namespace: ph-reactor
  labels:
    app: ph-reactor
    component: p2p
  annotations:
    # The name the Hetzner API lookup in Task 7 Step 6 searches for.
    load-balancer.hetzner.cloud/name: "ph-reactor"
    # The CCM default is fsn1 (HCLOUD_LOAD_BALANCERS_LOCATION), which is where
    # the Traefik LB sits. nbg1 instead, because that is where the workers are
    # and externalTrafficPolicy Local pins traffic to the one node running the
    # pod. Cross-location via the private network works either way -- the
    # Traefik LB already does it -- this just avoids the extra hop.
    load-balancer.hetzner.cloud/location: "nbg1"
    load-balancer.hetzner.cloud/type: "lb11"
    load-balancer.hetzner.cloud/use-private-ip: "true"
    load-balancer.hetzner.cloud/disable-private-ingress: "true"
    external-dns.alpha.kubernetes.io/hostname: reactor.vetra.io
spec:
  type: LoadBalancer
  externalTrafficPolicy: Local
  selector:
    app: ph-reactor
  ports:
    - name: p2p
      port: 25422
      targetPort: p2p
      protocol: TCP
```

- [ ] **Step 2: Write the console ClusterIP Service**

Create `infrastructure/ph-reactor/05-service-console.yaml`:

```yaml
# The reactor console and JSON API.
#
# ClusterIP ONLY, and there is deliberately no Ingress anywhere in this
# directory: src/settings/mod.rs serves /api/config, /api/drives, /api/quit,
# /api/join and the full document API with NO authentication -- the README is
# explicit that the bind address is the entire security boundary.
#
# Human access is `kubectl port-forward -n ph-reactor svc/ph-reactor-console 4002:4002`.
#
# Also the StatefulSet's serviceName, which gives the pod its stable DNS name.
apiVersion: v1
kind: Service
metadata:
  name: ph-reactor-console
  namespace: ph-reactor
  labels:
    app: ph-reactor
    component: console
spec:
  type: ClusterIP
  selector:
    app: ph-reactor
  ports:
    - name: console
      port: 4002
      targetPort: console
      protocol: TCP
```

- [ ] **Step 3: Write the NetworkPolicy**

Create `infrastructure/ph-reactor/06-networkpolicy.yaml`:

```yaml
# Defence in depth for the unauthenticated console.
#
# p2p (25422) must accept connections from anywhere -- that is the entire
# point of a public bootstrap node. The console (4002) is restricted to the
# namespace, so a compromised workload in another namespace cannot reach an
# admin API that has no auth of its own.
#
# kube-prometheus-stack scrapes from the monitoring namespace; that is
# allowed explicitly rather than by opening the port cluster-wide.
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: ph-reactor
  namespace: ph-reactor
  labels:
    app: ph-reactor
    component: infrastructure
spec:
  podSelector:
    matchLabels:
      app: ph-reactor
  policyTypes:
    - Ingress
  ingress:
    # Public p2p: unrestricted by design.
    - ports:
        - port: 25422
          protocol: TCP
    # Console: in-namespace only (this includes kubelet probes, which come
    # from the node and are not subject to the policy).
    - from:
        - namespaceSelector:
            matchLabels:
              kubernetes.io/metadata.name: ph-reactor
        - namespaceSelector:
            matchLabels:
              kubernetes.io/metadata.name: monitoring
      ports:
        - port: 4002
          protocol: TCP
```

- [ ] **Step 4: Run the validation script**

Run: `./infrastructure/ph-reactor/validate.sh`
Expected: `PASS validate` — in particular the `uses-proxyprotocol`, `kind: Ingress` and `4201` guards must all stay silent now that Services exist.

- [ ] **Step 5: Prove the PROXY-protocol guard actually works**

A guard that cannot fail is not a guard. Verify it catches the mistake:

```bash
cd infrastructure/ph-reactor
cp 04-service-p2p.yaml /tmp/svc-backup.yaml
sed -i 's|load-balancer.hetzner.cloud/name: "ph-reactor"|load-balancer.hetzner.cloud/uses-proxyprotocol: "true"\n    load-balancer.hetzner.cloud/name: "ph-reactor"|' 04-service-p2p.yaml
./validate.sh; echo "exit=$?"
cp /tmp/svc-backup.yaml 04-service-p2p.yaml && rm /tmp/svc-backup.yaml
./validate.sh
```
Expected: the middle run prints `FAIL: uses-proxyprotocol present` and `exit=1`; the final run prints `PASS validate`.

- [ ] **Step 6: Commit**

```bash
git add infrastructure/ph-reactor/04-service-p2p.yaml \
        infrastructure/ph-reactor/05-service-console.yaml \
        infrastructure/ph-reactor/06-networkpolicy.yaml
git commit -m "feat(ph-reactor): p2p LoadBalancer, ClusterIP console, NetworkPolicy

The p2p Service gets a dedicated Hetzner LB on TCP 25422 with
use-private-ip, so LB->node traffic rides the private network and no
Terraform or node-firewall change is needed.

uses-proxyprotocol is deliberately absent: the k3s LB sets it for
Traefik, and a PROXY header prepended to a libp2p stream corrupts the
Noise handshake with an uninformative error. validate.sh guards it.

externalTrafficPolicy Local preserves peer source IPs, which phase 2's
external-address announcing depends on.

The console stays ClusterIP with no Ingress anywhere in the directory --
its API is unauthenticated by design -- and the NetworkPolicy limits it
to the namespace plus monitoring scrapes.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: ArgoCD Application and the external-dns source change

**Repo:** `powerhouse-k8s-hosting`

**Files:**
- Create: `argocd-apps/infrastructure/ph-reactor.yaml`
- Modify: `argocd-apps/infrastructure/external-dns.yaml` (the `sources:` list)

**Interfaces:**
- Consumes: everything in `infrastructure/ph-reactor/` from Tasks 4 and 5.
- Produces: an ArgoCD Application named `ph-reactor` that the root app-of-apps picks up, and DNS publication for LoadBalancer Services.

- [ ] **Step 1: Write the Application**

Create `argocd-apps/infrastructure/ph-reactor.yaml`:

```yaml
# ph-reactor -- the Powerhouse bootstrap node.
#
# An always-on, publicly reachable reactor that every other reactor is
# pointed at exactly once: a DHT seed, a circuit relay for NAT'd peers, and a
# durable vault that keeps syncing while every laptop in the mesh is shut.
#
# Workloads (infrastructure/ph-reactor/):
#   ph-reactor          StatefulSet, 1 replica, 10Gi hcloud-volumes PVC
#   ph-reactor-p2p      LoadBalancer, TCP 25422 -- the public endpoint
#   ph-reactor-console  ClusterIP, 4002 -- NO Ingress, the API has no auth
#
# The published bootstrap multiaddr is in docs/ARCHITECTURE.md. Changing the
# identity in OpenBao changes the peer id and breaks every node already
# configured against it.
apiVersion: argoproj.io/v1alpha1
kind: Application
metadata:
  name: ph-reactor
  namespace: argocd
  labels:
    app: ph-reactor
    component: infrastructure
    managed-by: app-of-apps
  finalizers:
    - resources-finalizer.argocd.argoproj.io
spec:
  project: default
  source:
    repoURL: https://github.com/powerhouse-inc/powerhouse-k8s-hosting.git
    targetRevision: main
    path: infrastructure/ph-reactor
  destination:
    server: https://kubernetes.default.svc
    namespace: ph-reactor
  syncPolicy:
    automated:
      prune: true
      selfHeal: true
    syncOptions:
      - CreateNamespace=true
      - ServerSideApply=true
      # SSA dry-run diff so k8s-defaulted fields don't read as drift; same
      # rationale as the docling and paperless apps.
      - ServerSideDiff=true
    retry:
      limit: 5
      backoff:
        duration: 10s
        factor: 2
        maxDuration: 5m
  ignoreDifferences:
    # The Hetzner CCM writes the allocated LB address into Service status and
    # annotates the Service; neither is ours to own.
    - group: ""
      kind: Service
      name: ph-reactor-p2p
      jsonPointers:
        - /status
```

`validate.sh` only globs `[0-9]*.yaml` inside `infrastructure/ph-reactor/`, so this file is intentionally outside its scope — it is validated in Step 4.

- [ ] **Step 2: Read the current external-dns sources**

Run:
```bash
grep -n -A3 'sources:' argocd-apps/infrastructure/external-dns.yaml
```
Expected: a `sources:` list containing only `- ingress`.

- [ ] **Step 3: Add `service` to the sources list**

Modify `argocd-apps/infrastructure/external-dns.yaml` — replace:

```yaml
        # Filter by annotation
        sources:
          - ingress
```

with:

```yaml
        # Filter by annotation
        #
        # `service` was added for the ph-reactor bootstrap node, whose public
        # libp2p endpoint is a LoadBalancer Service rather than an Ingress.
        # Blast radius is small: annotationFilter below already scopes
        # external-dns to objects that explicitly carry the hostname
        # annotation, so no existing Service is affected.
        sources:
          - ingress
          - service
```

- [ ] **Step 4: Validate both files**

Run:
```bash
kubectl apply --dry-run=server -f argocd-apps/infrastructure/ph-reactor.yaml
python3 -c "import yaml; d=yaml.safe_load(open('argocd-apps/infrastructure/external-dns.yaml')); s=d['spec']['source']['helm']['valuesObject']['sources']; assert s==['ingress','service'], s; print('PASS sources =', s)"
```
Expected: `application.argoproj.io/ph-reactor created (server dry run)` and `PASS sources = ['ingress', 'service']`

- [ ] **Step 5: Confirm no existing Service would be newly claimed**

The `service` source only acts on objects carrying the annotation. Verify that is true in this cluster before pushing:

```bash
kubectl get svc -A -o json \
  | jq -r '.items[] | select(.metadata.annotations["external-dns.alpha.kubernetes.io/hostname"]) | "\(.metadata.namespace)/\(.metadata.name) -> \(.metadata.annotations["external-dns.alpha.kubernetes.io/hostname"])"'
```
Expected: no output (or only `ph-reactor/ph-reactor-p2p` once Task 7 has deployed). This was verified against the live cluster while writing the plan — no Service carried the annotation — so the change had zero blast radius at that time. Re-check anyway, because the cluster moves. **If any other Service appears, STOP** and report the list before continuing: enabling the `service` source would start publishing DNS for it.

- [ ] **Step 6: Commit**

```bash
git add argocd-apps/infrastructure/ph-reactor.yaml argocd-apps/infrastructure/external-dns.yaml
git commit -m "feat(ph-reactor): ArgoCD app + let external-dns see Services

The bootstrap node's public endpoint is a LoadBalancer Service, not an
Ingress, so external-dns needed 'service' added to its sources. Blast
radius is small: annotationFilter already scopes it to objects carrying
the hostname annotation, and no other Service in the cluster has one
(verified before the change).

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: Deploy and verify the bootstrap node

**Repo:** `powerhouse-k8s-hosting` (push only; the verification is operational)

**Files:**
- Modify: `infrastructure/ph-reactor/03-statefulset.yaml` (pin the real image tag)

**Interfaces:**
- Consumes: `BOOTSTRAP_PEER_ID` from Task 3; the image pushed by Task 2; all manifests from Tasks 4–6.
- Produces: **the published bootstrap multiaddr** `/ip4/<lb-ip>/tcp/25422/p2p/<BOOTSTRAP_PEER_ID>`, which Task 8 documents.

- [ ] **Step 1: Confirm the image exists in Harbor**

Run:
```bash
export BAO_ADDR=https://openbao.vetra.io
HUSER="$(bao kv get -mount=kv -format=json powerhouse/shared/harbor-credentials | jq -r '.data.data.username')"
HPASS="$(bao kv get -mount=kv -format=json powerhouse/shared/harbor-credentials | jq -r '.data.data.password')"
echo "$HPASS" | docker login cr.vetra.io -u "$HUSER" --password-stdin
docker manifest inspect cr.vetra.io/powerhouse-inc-powerhouse/ph-reactor:<version> >/dev/null && echo "PASS image present"
```
Expected: `PASS image present`

If the image is absent, push the `ph-reactor-v<semver>` tag in the `ph-reactor` repo to trigger Task 2's workflow, or run `scripts/build-image.sh` and `docker push` manually.

- [ ] **Step 2: Pin the real image tag**

Modify `infrastructure/ph-reactor/03-statefulset.yaml`: replace
`image: cr.vetra.io/powerhouse-inc-powerhouse/ph-reactor:0.0.0-dev`
with the version tag confirmed in Step 1. Do not use `latest`.

- [ ] **Step 3: Commit and push both repos**

```bash
cd /home/f/projects/powerhouse-k8s-hosting
git add infrastructure/ph-reactor/03-statefulset.yaml
git commit -m "feat(ph-reactor): pin the released image tag

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
git push origin main
```

- [ ] **Step 4: Wait for ArgoCD to sync and the pod to become ready**

Run:
```bash
kubectl -n ph-reactor rollout status statefulset/ph-reactor --timeout=300s
kubectl -n ph-reactor get pod,svc,pvc
```
Expected: `statefulset rolling update complete 1 pods ready`.

If the pod is stuck in `Init:Error`, the identity check failed — inspect with `kubectl -n ph-reactor logs ph-reactor-0 -c seed-state`. A key that is not 32 bytes means the OpenBao value is not raw base64 of the seed.

- [ ] **Step 5: Verify the running peer id matches the minted one**

This is the central assertion of the whole deployment — that the node came up as the identity we published, not a freshly minted one:

```bash
kubectl -n ph-reactor port-forward svc/ph-reactor-console 14002:4002 >/dev/null 2>&1 &
PF=$!; sleep 3
RUNNING="$(curl -fsS http://127.0.0.1:14002/api/status | jq -r '.reactor.peer_id')"
kill $PF
echo "running=$RUNNING expected=$BOOTSTRAP_PEER_ID"
[ "$RUNNING" = "$BOOTSTRAP_PEER_ID" ] && echo "PASS: identity matches OpenBao" || { echo "FAIL: the node minted a different identity"; exit 1; }
```
Expected: `PASS: identity matches OpenBao`

- [ ] **Step 6: Capture the load balancer address from the Hetzner API**

**Do not read `status.loadBalancer.ingress` here — it will be empty, and that is expected.** The Hetzner CCM provisions the LB but does not populate Service status in this cluster; `argocd-server` has read `<pending>` for 248 days for exactly this reason, while its LB (`loadBalancerID=5506209`) exists and is reconciled. Traefik only shows an IP because a `CiliumLoadBalancerIPPool` gives it one. Step 9 fixes that for us; this step gets the real address directly from the source.

```bash
HCLOUD_TOKEN="$(kubectl -n kube-system get secret hcloud -o jsonpath='{.data.token}' | base64 -d)"
curl -fsS -H "Authorization: Bearer $HCLOUD_TOKEN" \
  'https://api.hetzner.cloud/v1/load_balancers' \
  | jq -r '.load_balancers[] | select(.name=="ph-reactor") | "ipv4=\(.public_net.ipv4.ip) ipv6=\(.public_net.ipv6.ip) id=\(.id)"'
```
Expected: one line with a public `ipv4=` and `ipv6=`. Record both.

Do not echo `HCLOUD_TOKEN`. If the LB is absent after ~3 minutes, check `kubectl -n kube-system logs deploy/hcloud-cloud-controller-manager --tail=50 | grep -i ph-reactor` for an `EnsuringLoadBalancer` / `EnsuredLoadBalancer` pair.

- [ ] **Step 7: Verify the port is reachable from outside the cluster**

This is the assertion that actually matters for a bootstrap node, and it deliberately uses the API-sourced address rather than Service status, so it does not depend on Step 9 having run yet.

```bash
LB="<ipv4 from step 6>"
timeout 10 bash -c "cat < /dev/null > /dev/tcp/$LB/25422" && echo "PASS: 25422 open" || { echo "FAIL: 25422 not reachable"; exit 1; }
```
Expected: `PASS: 25422 open`

If this fails while the pod is Ready, suspect `externalTrafficPolicy: Local` — the Hetzner LB health check must be passing against the single node running the pod. Check the targets:
```bash
curl -fsS -H "Authorization: Bearer $HCLOUD_TOKEN" \
  'https://api.hetzner.cloud/v1/load_balancers' \
  | jq -r '.load_balancers[] | select(.name=="ph-reactor") | .targets[].health_status'
```

- [ ] **Step 8: Write the Cilium IP pool manifest**

Without this, `status.loadBalancer.ingress` stays empty and external-dns will never publish `reactor.vetra.io` — it only publishes for objects whose status is populated.

Create `infrastructure/ph-reactor/07-cilium-lb-pool.yaml`, substituting the addresses from Step 6:

```yaml
# Publishes the Hetzner LB's address into the Service status.
#
# WHY THIS EXISTS: the Hetzner CCM provisions the load balancer but does NOT
# populate status.loadBalancer.ingress. argocd-server has read <pending> for
# 248 days for exactly this reason, while its LB is alive and reconciled.
# external-dns only publishes records for Services whose status is populated,
# so without this pool reactor.vetra.io would never appear.
#
# Same mechanism as the traefik-hetzner-lb pool -- except that one was applied
# by hand and never made it into git (the infrastructure/cilium-lb-pool/
# directory the README documents no longer exists). This one is GitOps-managed.
#
# The addresses below are ASSIGNED BY HETZNER, not chosen. If the LB is ever
# recreated they change and this file must be updated in the same breath.
apiVersion: cilium.io/v2alpha1
kind: CiliumLoadBalancerIPPool
metadata:
  name: ph-reactor-hetzner-lb
  labels:
    app: ph-reactor
    component: infrastructure
spec:
  blocks:
    - cidr: <IPV4_FROM_STEP_6>/32
    - cidr: <IPV6_FROM_STEP_6>/128
  serviceSelector:
    matchLabels:
      app: ph-reactor
      component: p2p
```

The `serviceSelector` matches the labels Task 5 put on `ph-reactor-p2p` (`app: ph-reactor`, `component: p2p`) and nothing else — in particular not the console Service, which carries `component: console`.

- [ ] **Step 9: Apply the pool and confirm the Service status populates**

```bash
git add infrastructure/ph-reactor/07-cilium-lb-pool.yaml
git commit -m "feat(ph-reactor): Cilium IP pool so the LB address reaches Service status

The Hetzner CCM provisions the LB but leaves status.loadBalancer.ingress
empty -- argocd-server has been <pending> for 248 days for this reason.
external-dns only publishes for Services with a populated status, so
reactor.vetra.io needs this pool. Unlike traefik-hetzner-lb, which was
applied by hand and never landed in git, this one is GitOps-managed.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
git push origin main

# Wait for ArgoCD, then confirm Cilium satisfied the request.
sleep 90
kubectl -n ph-reactor get svc ph-reactor-p2p -o jsonpath='{.status}{"\n"}' | jq '{conditions, loadBalancer}'
```
Expected: a condition `cilium.io/IPAMRequestSatisfied` with `status: "True"`, and `loadBalancer.ingress[0].ip` equal to the Step 6 IPv4.

If the pool reports `PoolConflict`, another pool already claims that CIDR — check `kubectl get ciliumloadbalancerippools.cilium.io`.

- [ ] **Step 10: Verify DNS publication**

Run:
```bash
sleep 60
dig +short reactor.vetra.io
```
Expected: the Step 6 IPv4. If empty, check `kubectl -n kube-system logs -l app.kubernetes.io/name=external-dns --tail=50 | grep -i ph-reactor` — silence there means the `sources` change from Task 6 did not take effect; an error means the annotation or the domain filter is wrong.

Note: DNS is a convenience in phase 1. The published bootstrap multiaddr is `/ip4/…`, which libp2p dials directly, so a DNS failure here does not block the node from working.

- [ ] **Step 11: Verify the bootstrap role end to end with a real second reactor**

The decisive test: a reactor that knows nobody must be able to reach the mesh with only the bootstrap multiaddr.

```bash
LB="<ipv4 from step 6>"
JOINER="$(mktemp -d)"
cat > "$JOINER/config.json" <<JSON
{
  "schemaVersion": 2,
  "instance": { "name": "joiner", "listen": "/ip4/0.0.0.0/tcp/0" },
  "p2p": { "mdns": false, "dht": true, "relay": false,
           "bootstraps": ["/ip4/$LB/tcp/25422/p2p/$BOOTSTRAP_PEER_ID"] },
  "drives": [],
  "settings": { "host": "127.0.0.1", "port": 14003 },
  "logLevel": "info"
}
JSON
PH_REACTOR_STATE_DIR="$JOINER" /home/f/projects/ph-reactor/target/release/ph-reactor run --daemonize
sleep 20
PH_REACTOR_STATE_DIR="$JOINER" /home/f/projects/ph-reactor/target/release/ph-reactor status --json | jq '.reactor, .drives'
grep -ci 'dht: bootstrap initiated\|Connected' "$JOINER/logs/reactor.log" || true
PH_REACTOR_STATE_DIR="$JOINER" /home/f/projects/ph-reactor/target/release/ph-reactor stop
rm -rf "$JOINER"
```
Expected: the joiner's log shows the DHT bootstrap initiating and a connection established to `$BOOTSTRAP_PEER_ID`. This is the behaviour the whole change exists to provide.

- [ ] **Step 12: Verify the identity survives pod loss**

```bash
kubectl -n ph-reactor delete pod ph-reactor-0
kubectl -n ph-reactor rollout status statefulset/ph-reactor --timeout=300s
kubectl -n ph-reactor port-forward svc/ph-reactor-console 14002:4002 >/dev/null 2>&1 &
PF=$!; sleep 5
AFTER="$(curl -fsS http://127.0.0.1:14002/api/status | jq -r '.reactor.peer_id')"
kill $PF
[ "$AFTER" = "$BOOTSTRAP_PEER_ID" ] && echo "PASS: identity survives pod loss" || { echo "FAIL: peer id changed to $AFTER"; exit 1; }
```
Expected: `PASS: identity survives pod loss`

- [ ] **Step 13: Record the published multiaddr**

Write it into the task completion note:

```
BOOTSTRAP_MULTIADDR=/ip4/<lb-ip>/tcp/25422/p2p/<BOOTSTRAP_PEER_ID>
```

---

### Task 8: Document the bootstrap contract

**Repo:** both

**Files:**
- Modify: `/home/f/projects/ph-reactor/README.md` (the "Syncing a knowledge vault" section)
- Modify: `/home/f/projects/powerhouse-k8s-hosting/docs/ARCHITECTURE.md`
- Modify: `/home/f/projects/powerhouse-k8s-hosting/README.md` (runtime endpoints table)

**Interfaces:**
- Consumes: `BOOTSTRAP_MULTIADDR` from Task 7.
- Produces: nothing consumed downstream. This is the deliverable that makes the node usable by anyone other than its author.

- [ ] **Step 1: Add the bootstrap section to the ph-reactor README**

In `/home/f/projects/ph-reactor/README.md`, immediately after the "Syncing a knowledge vault" introduction, insert (substituting the real multiaddr):

```markdown
#### The Powerhouse bootstrap node

A permanently-running reactor in the Powerhouse Kubernetes cluster acts as the
mesh's rendezvous: a DHT seed, a circuit relay for peers behind NAT, and a
vault that keeps syncing while every laptop is shut. Point a new reactor at it
once and it never needs to change:

```sh
ph-reactor drive add /ip4/<lb-ip>/tcp/25422/p2p/<peer-id> --name "Powerhouse"
```

Or seed the DHT without pinning a drive, by adding it to `p2p.bootstraps` in
`~/.ph/reactor/config.json`:

```json
"p2p": { "dht": true, "bootstraps": ["/ip4/<lb-ip>/tcp/25422/p2p/<peer-id>"] }
```

The node listens on **TCP 25422**, not the 4201 default: 25422 is in IANA's
explicitly-unassigned `25101-25470` block, inside RFC 6335's User range, and
below the Linux ephemeral floor of 32768, so a fixed listener there cannot
lose a bind race against an outbound connection.

Its peer id is a stable contract — the identity lives in OpenBao, not on the
node's volume, so it survives the node being rebuilt.
```

- [ ] **Step 2: Add a ph-reactor section to the cluster ARCHITECTURE doc**

In `/home/f/projects/powerhouse-k8s-hosting/docs/ARCHITECTURE.md`, add a new section after "Registry":

```markdown
## ph-reactor (bootstrap node)

An always-on Powerhouse reactor in the `ph-reactor` namespace, acting as the
rendezvous point for the mesh of laptop reactors: a Kademlia seed, a circuit
relay for NAT'd peers, and a durable vault.

- **Public endpoint:** a **second Hetzner load balancer** (lb11, ~EUR 5.4/mo,
  separate from the Traefik lb11) forwarding TCP **25422** to the pod.
  `reactor.vetra.io` resolves to it.
- **Published multiaddr:** `/ip4/<lb-ip>/tcp/25422/p2p/<peer-id>` — a contract.
  Other reactors have this in their config; it must not change.
- **Identity:** OpenBao `powerhouse/shared/ph-reactor-bootstrap`, mounted by an
  init container. Deliberately *not* generated on the PVC, so losing the volume
  does not invalidate the peer id. **This secret is the single point of failure
  for the contract and must be covered by the OpenBao backup story.**
- **No PROXY protocol** on that LB, unlike the Traefik one: a PROXY header
  prepended to a libp2p stream corrupts the Noise handshake.
- **Console:** ClusterIP only, no Ingress — the reactor's JSON API is
  unauthenticated by design, so the bind address is the security boundary.
  Reach it with
  `kubectl port-forward -n ph-reactor svc/ph-reactor-console 4002:4002`.
- **`config.json` is re-rendered from the ConfigMap on every pod start**, so
  drives added through the console or via `ph-reactor join` do **not** survive
  a restart. Declare them in `infrastructure/ph-reactor/02-configmap.yaml`
  instead.
- **external-dns** has `service` in its `sources` because of this node; it is
  still scoped by `annotationFilter`.
```

- [ ] **Step 3: Add the console to the runtime endpoints table**

In `/home/f/projects/powerhouse-k8s-hosting/README.md`, add a row to the "Runtime endpoints" table:

```markdown
| `reactor.vetra.io:25422` | ph-reactor bootstrap node (libp2p, not HTTP). Console is ClusterIP-only: `kubectl port-forward -n ph-reactor svc/ph-reactor-console 4002:4002`. |
```

- [ ] **Step 4: Verify the documented multiaddr is the live one**

A stale multiaddr in the docs is worse than none, so check it rather than trusting the copy-paste:

```bash
LB="$(kubectl -n ph-reactor get svc ph-reactor-p2p -o jsonpath='{.status.loadBalancer.ingress[0].ip}')"
grep -rn "$LB" /home/f/projects/ph-reactor/README.md /home/f/projects/powerhouse-k8s-hosting/docs/ARCHITECTURE.md \
  && echo "PASS: docs carry the live LB address" \
  || { echo "FAIL: documented address does not match the live Service"; exit 1; }
grep -rn "$BOOTSTRAP_PEER_ID" /home/f/projects/ph-reactor/README.md \
  && echo "PASS: docs carry the live peer id" \
  || { echo "FAIL: peer id missing from README"; exit 1; }
```
Expected: both `PASS` lines.

- [ ] **Step 5: Commit both repos**

```bash
cd /home/f/projects/ph-reactor
git add README.md
git commit -m "docs: publish the Powerhouse bootstrap node multiaddr

Point a new reactor at it once with drive add, or seed the DHT via
p2p.bootstraps. Explains why the node listens on 25422 rather than the
4201 default, and that the peer id is a stable contract because the
identity lives in OpenBao rather than on the node's volume.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"

cd /home/f/projects/powerhouse-k8s-hosting
git add docs/ARCHITECTURE.md README.md
git commit -m "docs: describe the ph-reactor bootstrap node

Records the things that are expensive to rediscover: the second Hetzner
LB and its cost, why PROXY protocol must stay off it, that the console
is ClusterIP-only because its API has no auth, that config.json is
re-rendered from git every start so console-added drives do not persist,
and that the OpenBao identity is the single point of failure for the
published peer id.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

- [ ] **Step 6: Push both**

```bash
cd /home/f/projects/ph-reactor && git push origin main
cd /home/f/projects/powerhouse-k8s-hosting && git push origin main
```

---

## Self-Review

**Spec coverage:**

| Spec section | Task |
|---|---|
| Container image (distroless, foreground, headless) | 1 |
| CI push to Harbor | 2 |
| Identity from OpenBao, minted off-cluster | 3, verified again in 7 |
| StatefulSet, PVC, init container, probes | 4 |
| Reactor configuration (25422, mdns off, dht/relay on) | 4 |
| Dedicated Hetzner LB, no PROXY, externalTrafficPolicy Local | 5 |
| Console ClusterIP-only, NetworkPolicy | 5 |
| Cilium IP pool so Service status populates (two-phase deploy) | 7, steps 6/8/9 |
| external-dns `sources` change | 6 |
| ArgoCD Application | 6 |
| Bootstrap contract published | 7, 8 |
| Testing matrix (image, manifests, identity, reachability, bootstrap role, durability) | 1, 4, 5, 7 |
| Risks: PROXY, external-dns blast radius, console exposure, LB cost, operator confusion, OpenBao SPOF | guarded in 5, checked in 6, documented in 8 |

Phase 2 (WSS) is explicitly out of scope and has no task, matching the spec.

**Placeholder scan:** The only unresolved values are `<lb-ip>` / `<IPV4_FROM_STEP_6>` / `<IPV6_FROM_STEP_6>`, `<peer-id>` / `BOOTSTRAP_PEER_ID`, and `<version>`. Each is produced by a named earlier step (7.6, 3.8, 2) and cannot be known before then — the IPs are assigned by Hetzner and the peer id is derived from a key that does not exist until Task 3 mints it. Every consuming step says where its value comes from.

**Ordering constraint worth restating:** Task 7 is not a single sync. Steps 3–7 deploy and prove reachability; steps 8–10 add the Cilium pool and DNS. Do not collapse them — step 8's manifest cannot be written before step 6 has read the assigned address.

**Type consistency:** Pod label `app: ph-reactor` is used by the StatefulSet selector (Task 4) and both Service selectors (Task 5). Named ports `p2p` / `console` are declared in Task 4 and referenced by `targetPort` in Task 5 and by both probes in Task 4. Secret name `ph-reactor-identity` is produced in Task 4 Step 3 and mounted in Task 4 Step 6. `harbor-credentials` is produced in Task 4 Step 4 and referenced by `imagePullSecrets` in Task 4 Step 6. Service `ph-reactor-console` is the StatefulSet's `serviceName` (Task 4) and is created in Task 5 — ArgoCD applies the directory as one unit, so ordering within the sync is not an issue.
