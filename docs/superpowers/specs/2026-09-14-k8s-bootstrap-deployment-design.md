# ph-reactor on Kubernetes: the public bootstrap node — Design

## Problem

`ph-reactor` today is a laptop daemon: it assumes a user session (tray via
D-Bus), a home directory (`~/.ph/reactor`), a loopback console, and a peer
that is reachable only on a LAN or through an explicit multiaddr someone
pastes around. A mesh of such nodes has no stable rendezvous — every new
reactor needs someone to hand it a live peer address, and a peer behind NAT
has no way to be dialled at all.

We want one **always-on, publicly reachable reactor** running in the
Powerhouse Kubernetes cluster that every other reactor can be pointed at
exactly once, permanently:

- a **stable peer id and multiaddr** that can be written into documentation
  and into other nodes' `p2p.bootstraps` and never change;
- a **DHT seed** so a joining node's routing table is populated from first
  contact;
- a **circuit relay** so reactors behind NAT can reach each other through it;
- a durable vault that keeps syncing while every laptop in the mesh is shut.

The cluster it must live in (`powerhouse-k8s-hosting`) is GitOps-managed:
ArgoCD app-of-apps, images from Harbor, secrets from OpenBao, Traefik as the
only thing currently behind the Hetzner load balancer.

## Goals

1. **Run the existing binary unmodified** in phase 1. No Rust changes are
   required to ship a working bootstrap node; every phase-1 knob is already a
   config value.
2. **A permanent bootstrap contract.** The peer id is generated once,
   off-cluster, stored in OpenBao, and survives PVC loss, node replacement,
   and namespace recreation.
3. **Fit the cluster's conventions exactly** — in-repo directory app, Harbor
   image, ExternalSecret from OpenBao, automated sync + self-heal.
4. **Do not expose the unauthenticated admin API.** The console has no auth by
   design; the bind address is the entire security boundary.
5. **Leave a clean path to phase 2** (WebSocket-Secure transport through the
   existing Traefik ingress), without blocking phase 1 on it.

## Non-goals

- High availability. One replica. The store is a per-doc WAL on a single
  PVC and the identity is a single keypair; a second replica would be a
  second *peer*, not a failover, and would fight over the same volume.
- Multi-tenancy. This is one node in the `ph-reactor` namespace, not a
  per-tenant offering under `powerhouse-chart/`.
- Exposing the console to the public internet.
- Migrating existing laptop reactors. They join the mesh by adding the
  bootstrap multiaddr; nothing about their local state changes.

## Decisions and their rationale

### Port 25422

The libp2p listener moves from ph-reactor's default `4201` to **25422**.

Verified against IANA's *Service Name and Transport Protocol Port Number
Registry* (the official CSV, 15,404 rows, sanity-checked against `ssh/22` and
`https/443`): the registry **explicitly lists `25101–25470` as `Unassigned`**
for both TCP and UDP, and 25422 sits inside that block. Nearest neighbours are
`db2c-tls` (25100) and `rna` (25471, SCTP only).

It satisfies every relevant constraint:

| Criterion | Status |
|---|---|
| RFC 6335 User/Registered range (1024–49151) | yes — the correct range for a public service |
| Unassigned by IANA | yes, explicitly, not merely absent |
| Below the Linux ephemeral floor (32768) | yes — no bind race against outbound source ports |
| Not a p2p scan target | yes — unlike 4001, which IANA assigns to `newoak` and IPFS squats |

`4201` is unassigned too, but it is ph-reactor's *published default* and
therefore a worse choice for a public node. Registration of a `ph-reactor`
service name at 25422 via IANA Expert Review is possible and optional; it is
not a blocker.

Cost to change: **zero code**. `instance.listen` is a config value.

### Reachability: a dedicated Hetzner load balancer

`type: LoadBalancer` with `load-balancer.hetzner.cloud/use-private-ip: "true"`.
Hetzner CCM provisions a second `lb11` (~€5.4/month).

Rejected alternatives:

- **NodePort + firewall rule.** The default NodePort range is 30000–32767, so
  25422 would require widening `--service-node-port-range` cluster-wide; it
  also needs an inbound rule in the sibling `powerhouse-k8s-cluster` Terraform
  repo, and peers would dial node IPs that change when nodes are replaced —
  fatal for a contract that is supposed to be permanent.
- **WSS-only through Traefik.** Attractive (no new LB, no recurring cost) but
  requires Rust changes before anything can ship. Deferred to phase 2 rather
  than dropped.

Two failure modes this design must actively avoid:

- **PROXY protocol must be OFF.** The existing k3s LB sets
  `uses-proxyprotocol: "true"` for Traefik. A PROXY header prepended to a
  libp2p stream corrupts the Noise handshake, and the resulting error is
  uninformative. The p2p Service must not carry that annotation.
- **`externalTrafficPolicy: Local`.** Preserves the peer's real source IP.
  Matters little in phase 1, but phase 2's external-address announcing is
  built on identify's observed address, which is wrong under SNAT.

Because LB→node traffic rides the private network, **no Terraform or firewall
change is required.**

### A cluster-wide change to external-dns

`external-dns` is currently configured with `sources: [ingress]`, so a
LoadBalancer Service receives no DNS record. `service` is added to that list.

Blast radius is small: `annotationFilter:
"external-dns.alpha.kubernetes.io/hostname"` is already set, so only Services
that explicitly carry the hostname annotation are managed. This yields
`reactor.vetra.io` and is a prerequisite for phase 2.

This is the one change in this design that touches shared cluster
infrastructure, and it is called out as such so it can be reviewed on its own
merits.

### Identity from OpenBao, not from the PVC

The peer id *is* the bootstrap contract. If it is generated on first start and
lives only on the PVC, then losing the volume silently invalidates every
joining node's configuration.

Instead: generate the keypair once, off-cluster, store it in OpenBao at
`powerhouse/shared/ph-reactor-bootstrap`, and have an ExternalSecret + init
container place it at `<state>/key` (32 raw bytes, mode 0600 — verified
against `src/paths.rs::key_file`).

Generating it before the first deploy also means the multiaddr can be written
into documentation and manifests in the same commit that creates them, rather
than being discovered afterwards.

### Console stays inside the cluster

`src/settings/mod.rs` exposes `/api/config`, `/api/drives`, `/api/quit`,
`/api/join`, `/api/ban` and the full document API with **no authentication** —
the README is explicit that the bind address is the security boundary.

The console therefore gets a ClusterIP Service and a NetworkPolicy limiting
it to the namespace. Human access is `kubectl port-forward`. It binds
`0.0.0.0` inside the pod only so that probes and port-forward work.

### config.json is rendered from git on every start

The ConfigMap is the source of truth; the init container writes
`<state>/config.json` on every pod start.

**Consequence, stated explicitly:** drives added through the console or via
`ph-reactor join` do not survive a restart. For a GitOps-managed node this is
correct — the declared state wins — but it is a genuine behaviour difference
from running on a laptop and must be documented where operators will see it.

### No token gate on inbound hellos

`p2p.tokenEnv` is left unset. A bootstrap node exists to accept connections
from nodes that do not yet know anyone; a shared-token gate would defeat its
purpose. Confidentiality of private channel content continues to rest on the
group model's `auth` block, which the store enforces on its single apply path
after the signature check.

## Architecture

### Phase 1 components

**`ph-reactor` repo**

- `Dockerfile` — the CI-built static musl binary on
  `gcr.io/distroless/static-debian12:nonroot`. Note that TLS roots are *not*
  a reason for this base: `Cargo.lock` pins `webpki-roots` and not
  `rustls-native-certs`, so `reqwest`'s CA bundle is compiled into the binary
  and `scratch` would also work. Distroless static is chosen for the
  `nonroot` uid in `/etc/passwd`, a writable `/tmp`, and a base that gets
  patched — not for certificates. No shell, no package manager. Runs the
  daemon in the **foreground** (no
  `--daemonize`): Kubernetes is the supervisor, so the pidfile and fork path
  are bypassed entirely. The tray self-disables with no D-Bus session bus —
  the headless path the README already documents.
- `.github/workflows/image.yml` — on the existing `ph-reactor-v*` tag
  trigger, build and push
  `cr.vetra.io/powerhouse-inc/ph-reactor:{<version>,sha-<short>}`.

**`powerhouse-k8s-hosting` repo**

`infrastructure/ph-reactor/` (the in-repo directory pattern used by
`docling`):

| File | Contents |
|---|---|
| `00-external-secret-identity.yaml` | ed25519 key from OpenBao, sync-wave `-5` |
| `01-external-secret-harbor.yaml` | imagePullSecret, sync-wave `-2` |
| `02-configmap.yaml` | `config.json` |
| `03-statefulset.yaml` | init container + reactor, PVC `hcloud-volumes` 10Gi |
| `04-service-p2p.yaml` | `type: LoadBalancer`, TCP 25422 |
| `05-service-console.yaml` | ClusterIP :4002 |
| `06-networkpolicy.yaml` | console restricted to the namespace |

Plus `argocd-apps/infrastructure/ph-reactor.yaml` (the ArgoCD Application) and
the one-line `sources` addition to
`argocd-apps/infrastructure/external-dns.yaml`.

A **StatefulSet** rather than a Deployment: stable identity, an exclusive
volume, and no rescheduling that could run two reactors against one WAL.

### Data flow on start

```
ArgoCD sync
  └─ ExternalSecret (wave -5) ──> Secret: ph-reactor-identity (32 raw bytes)
  └─ ExternalSecret (wave -2) ──> Secret: harbor-credentials
  └─ StatefulSet (wave 0)
       └─ initContainer
            ├─ cp /identity/key  ->  /var/lib/ph-reactor/key   (chmod 0600)
            └─ cp /cfg/config.json -> /var/lib/ph-reactor/config.json (0600)
       └─ ph-reactor (foreground)
            ├─ loads identity  ->  peer id 12D3Koo… (fixed, forever)
            ├─ listens /ip4/0.0.0.0/tcp/25422
            ├─ console on 0.0.0.0:4002 (ClusterIP only)
            ├─ Kademlia: serves routing queries for joining nodes
            └─ relay server: forwards circuits for NAT'd peers
```

### Reactor configuration

```json
{
  "schemaVersion": 2,
  "instance": { "name": "ph-bootstrap", "listen": "/ip4/0.0.0.0/tcp/25422" },
  "p2p": { "mdns": false, "dht": true, "relay": true, "bootstraps": [] },
  "drives": [],
  "settings": { "host": "0.0.0.0", "port": 4002 },
  "logLevel": "info"
}
```

Three deliberate departures from the laptop defaults:

- **`mdns: false`** — multicast discovery is meaningless across a cluster
  network and only produces noise.
- **`dht: true`** — the node seeds joining reactors' routing tables. This is
  most of what makes it a *bootstrap* node rather than merely a reachable one.
- **`relay: true`** — the circuit relay server forwards connections for peers
  behind NAT, which is the other half.

### Health checks

Readiness and liveness probe `GET /api/status` on the console port
(`src/settings/mod.rs:79`). Readiness gates the LoadBalancer's backend, so a
reactor that has not finished opening its store is not advertised to peers.

## The bootstrap contract

Published once and never changed:

```
/ip4/<lb-ip>/tcp/25422/p2p/12D3Koo…
```

A joining reactor adds it to `p2p.bootstraps` in its own `config.json`, or
pins it as a drive:

```sh
ph-reactor drive add /ip4/<lb-ip>/tcp/25422/p2p/12D3Koo… --name "Powerhouse"
```

Documented in the `ph-reactor` README and in
`powerhouse-k8s-hosting/docs/ARCHITECTURE.md`.

## Phase 2 (follow-up, out of scope here)

Add WebSocket-Secure as a second transport so peers can also reach the node on
443 through the existing Traefik ingress — no second load balancer, and it
traverses restrictive corporate firewalls that block 25422 outbound.

Requires actual Rust changes, which is why it is not phase 1:

1. Enable libp2p's `websocket` feature and add a `/ip4/0.0.0.0/tcp/<port>/ws`
   listener.
2. Add **external-address announcing** — `Swarm::add_external_address` is
   never called today (verified: no occurrence anywhere in `src/`), so the
   node cannot advertise a public address it does not itself observe. Needs a
   config field (e.g. `instance.external`) carrying announce multiaddrs.
3. Advertise both multiaddrs in the bootstrap contract.

## Testing

| Level | What |
|---|---|
| Image | `docker run` the image, confirm the daemon starts headless, `/api/status` answers, and the tray absence is non-fatal |
| Manifests | `kubeconform`/`kubectl --dry-run=server` before commit; ArgoCD diff must be clean after sync |
| Identity | Peer id reported by the running pod matches the one derived from the OpenBao key *before* deployment |
| Reachability | From outside the cluster, TCP-connect to `<lb-ip>:25422`; then a real reactor dials the full multiaddr and completes the hello handshake |
| Bootstrap role | A second reactor with only `p2p.bootstraps` set (no drives) discovers a peer through the DHT |
| Durability | Delete the pod; confirm the peer id is unchanged and the store survives |
| Regression | The existing 107 lib tests and e2e suite still pass — phase 1 changes no Rust code, so this is a guard against accidental drift |

## Risks

| Risk | Mitigation |
|---|---|
| PROXY protocol accidentally inherited from the Traefik LB pattern | Explicitly assert its absence in the manifest and in the reachability test |
| external-dns `sources` change affects other Services | `annotationFilter` already scopes it; only annotated Services are managed |
| Console accidentally exposed later | No Ingress in the directory at all; NetworkPolicy as defence in depth |
| Second Hetzner LB cost drifts unnoticed | Documented in ARCHITECTURE.md alongside the existing lb11 |
| Operator confusion over reverted console changes | Documented explicitly in both repos |
| Losing the OpenBao identity entry | It is the single point of failure for the contract; it must be part of the OpenBao backup story before the node is advertised |
