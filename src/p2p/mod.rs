//! The libp2p sync engine.
//!
//! One engine per daemon: a `Swarm` composing
//!
//! - **gossipsub** (topic [`GOSSIPSUB_TOPIC`]): mesh fan-out of signed
//!   actions (`ActionMsg` payloads);
//! - **request-response** (`/ph-reactor/sync/2.0.0`, length-prefixed
//!   JSON, [`SyncCodec`]): the per-drive protocol — hello handshake
//!   (version, identity, token), per-doc catch-up (vector-clock based),
//!   and summary exchange so both sides converge;
//! - **mDNS** (optional): LAN discovery, seeds the dialer.
//!
//! Drive lifecycle: `Connecting` (dial / handshake / catch-up) →
//! `Synced` (no known gaps, docs flowing) → `Paused` (user-paused) /
//! back to `Connecting` on disconnect; `RequiresAuth` and `Error` for
//! rejected or broken handshakes.
//! The engine talks to the daemon over two unbounded channels:
//! commands ([`EngineCommand`]) and events ([`EngineEvent`]).

pub mod codec;
pub mod invite;
use std::collections::{HashMap, HashSet, VecDeque};
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use libp2p::gossipsub;
use libp2p::identify;
use libp2p::identity::Keypair;
use libp2p::kad;
use libp2p::relay;
use libp2p::request_response::{self, OutboundRequestId};
use libp2p::swarm::behaviour::toggle::Toggle;
use libp2p::swarm::{dial_opts::DialOpts, NetworkBehaviour, Swarm, SwarmEvent};
use libp2p::{mdns, noise, tcp, yamux, Multiaddr, PeerId, StreamProtocol, SwarmBuilder};
use tokio::sync::mpsc;

use crate::action::Action;
use crate::doc::{DocId, Hash32, ModelRef, VecClock};
use crate::drives::{Drive, DriveStatus};
use crate::model::{l1::L1, model_def_hash};
use crate::p2p::codec::{
    ActionMsg, CatchUp, CatchUpAck, DocSummary, Hello, HelloAck, HelloError, ModelDef,
    ModelRequest, Summary, SummaryAck, SyncCodec, SyncMsg, CATCH_UP_MAX_ACTIONS, GOSSIPSUB_TOPIC,
    PROTOCOL_VERSION, SYNC_PROTOCOL,
};
use crate::store::Store;

use serde_json::Value;

type SyncProtocol = StreamProtocol;

const SYNC_PROTO: SyncProtocol = StreamProtocol::new(SYNC_PROTOCOL);
/// How often an authenticated drive gets a proactive catch-up tick.
const CATCH_UP_TICK: Duration = Duration::from_secs(30);
/// How often the engine wakes to check commands, idle state, and the
/// store's outbound op queue (independent of the per-drive cadence).
const TICK_INTERVAL: Duration = Duration::from_secs(5);
/// Bound on the published-action dedupe set (the tick's drain skips what
/// the outbound feed already published). Eviction is FIFO; a re-published
/// action is idempotent on the receiver (same content, LWW merge).
const PUBLISHED_CAP: usize = 8192;
/// A drive that has been silent this long is reported idle (the daemon
/// may let it sleep; the store stays warm).
const IDLE_AFTER_SILENCE: Duration = Duration::from_secs(30 * 60);
/// A peer that fails auth (wrong token) this many times within
/// [`AUTH_BAN_WINDOW`] is auto-banned.
const AUTH_BAN_AFTER: u32 = 3;
/// The window in which [`AUTH_BAN_AFTER`] failed auth attempts must occur
/// before a peer is auto-banned.
const AUTH_BAN_WINDOW: Duration = Duration::from_secs(10 * 60);

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// Loads the daemon's identity keypair from `path` (a 32-byte ed25519
/// seed) or generates a fresh one and persists it (mode 0600).
///
/// The same key signs the doc ops, the gossipsub messages, and is the
/// libp2p transport identity: one key is the node's whole identity.
pub fn load_or_create_identity(path: &std::path::Path) -> Result<Keypair, String> {
    if let Ok(bytes) = std::fs::read(path) {
        if bytes.len() == 32 {
            let mut buf = bytes;
            return Keypair::ed25519_from_bytes(&mut buf).map_err(|e| e.to_string());
        }
    }
    let kp = Keypair::generate_ed25519();
    let seed = kp
        .clone()
        .try_into_ed25519()
        .map_err(|e| e.to_string())?
        .secret()
        .as_ref()
        .to_vec();
    std::fs::write(path, &seed).map_err(|e| format!("writing {}: {e}", path.display()))?;
    set_perms_0600(path);
    Ok(kp)
}

fn set_perms_0600(path: &std::path::Path) {
    if let Ok(md) = std::fs::metadata(path) {
        let mut perms = md.permissions();
        perms.set_mode(0o600);
        let _ = std::fs::set_permissions(path, perms);
    }
}

/// The peer id derived from the identity keypair.
pub fn peer_id_of(kp: &Keypair) -> PeerId {
    kp.public().to_peer_id()
}
/// Parses a bootstrap multiaddr (`/ip4/…/tcp/…/p2p/<peer-id>`) into the
/// peer id and the dial address (the multiaddr without its `/p2p` tail).
pub fn parse_bootstrap(s: &str) -> Option<(PeerId, Multiaddr)> {
    let full: Multiaddr = s.parse().ok()?;
    let mut peer_id = None;
    let mut base = Vec::new();
    for proto in full.iter() {
        match proto {
            libp2p::multiaddr::Protocol::P2p(pid) => peer_id = Some(pid),
            other => base.push(other),
        }
    }
    let peer_id = peer_id?;
    let addr = base.into_iter().collect::<Multiaddr>();
    Some((peer_id, addr))
}

/// The 32-byte raw ed25519 public key of the identity.
pub fn public_key_bytes(kp: &Keypair) -> Result<[u8; 32], String> {
    let ed = kp.clone().try_into_ed25519().map_err(|e| e.to_string())?;
    Ok(ed.public().to_bytes())
}

/// The dalek [`SigningKey`] behind the identity (for the doc store's
/// op signatures).
pub fn signing_key(kp: &Keypair) -> Result<ed25519_dalek::SigningKey, String> {
    let ed = kp.clone().try_into_ed25519().map_err(|e| e.to_string())?;
    let secret = ed.secret();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(secret.as_ref());
    Ok(ed25519_dalek::SigningKey::from_bytes(&arr))
}

// ---------------------------------------------------------------------------
// Engine commands and events
// ---------------------------------------------------------------------------

/// Commands the daemon sends to the engine.
#[derive(Debug)]
pub enum EngineCommand {
    /// Add (or re-add) a drive: dial it and start the hello handshake.
    AddDrive(Drive),
    /// Remove a drive: stops its sync work.
    RemoveDrive { name: String },
    /// Pause/resume a drive (stops/resumes its sync work).
    SetPaused { name: String, paused: bool },
    /// Force a re-sync: immediate catch-up tick.
    Resync { name: String },
    /// Bootstrap the DHT: seed the routing table from these peers.
    DhtBootstrap { peers: Vec<(PeerId, Multiaddr)> },
    /// Publish a provider record: this node provides the doc.
    PublishProvider { name: String, doc: DocId },
    /// Query the DHT for providers of a key (doc id bytes).
    FindProviders { key: Vec<u8> },
    /// Join a drive by a signed invite: add the drive and attach the
    /// join-proof to the first hello so the inviter can add a drive back.
    Join {
        drive: Drive,
        accept: invite::InviteAccept,
    },
    /// Ban a peer: its future handshakes are refused (it stays in the ban
    /// list). `peer` is the verified connection peer id.
    Ban { peer: PeerId },
    /// Unban a peer: allow its handshakes again.
    Unban { peer: PeerId },
    /// Shut the engine down (drain and return).
    Shutdown,
}

/// Events the engine reports to the daemon (for the status snapshot).
#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// The engine is up: identity and the listen address.
    Identity { peer_id: PeerId, listen: Multiaddr },
    /// A drive changed state.
    DriveStatus {
        name: String,
        status: DriveStatus,
        detail: Option<String>,
    },
    /// A doc changed (applied locally or received remotely).
    DocChanged { name: Option<String> },
    /// The DHT bootstrap finished (`true` = succeeded, `false` = failed).
    DhtBootstrap(bool),
    /// The DHT reported a provider for a key (doc id bytes).
    DhtProvider { key: Vec<u8>, peer: PeerId },
    /// A connection to a peer was established (direct or relayed).
    PeerConnected { peer: PeerId },
    /// A joiner was accepted on a valid join-proof; the daemon should persist
    /// the resulting drive so it survives a restart.
    DriveJoined { name: String, addr: Multiaddr },
    /// A peer was auto-banned after repeated failed auth attempts; the
    /// daemon should persist it to the ban list.
    PeerAutoBanned { peer: String },
    /// A joiner was accepted on a valid join-proof that carried a group
    /// grant; the daemon resolves the grant (nonce -> groups) and applies
    /// it. `nonce` is the invite's challenge echoed in the join-proof.
    InviteAccepted { peer: PeerId, nonce: Vec<u8> },
}

// ---------------------------------------------------------------------------
// Per-drive runtime
// ---------------------------------------------------------------------------

struct DriveRuntime {
    drive: Drive,
    peer: PeerId,
    status: DriveStatus,
    /// Per-doc clocks last seen from this drive.
    remote_clocks: HashMap<DocId, VecClock>,
    /// In-flight catch-up request ids (response attribution).
    in_flight: HashSet<OutboundRequestId>,
    /// Hello handshake completed for the current connection.
    handshaken: bool,
    /// Next proactive catch-up tick.
    next_catch_up: tokio::time::Instant,
    last_seen: tokio::time::Instant,
    /// A join-proof to attach to the first hello (set by [`EngineCommand::Join`]).
    accept: Option<invite::InviteAccept>,
}

// ---------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------

pub struct SyncEngine {
    swarm: Swarm<SyncBehaviour>,
    store: Arc<Store>,
    peer_id: PeerId,
    pub_name: String,
    listen: Multiaddr,
    /// Optional second listener for the WebSocket transport.
    listen_ws: Option<Multiaddr>,
    /// Addresses announced to peers via `add_external_address`.
    external: Vec<Multiaddr>,
    /// Local token (from the config's `p2p.tokenEnv` env var), if any.
    token: Option<String>,
    drives: HashMap<String, DriveRuntime>,
    /// Peers that presented a valid hello (their key is registered) even
    /// though no drive for them existed yet; a drive added later
    /// completes the handshake from this record.
    guests: HashSet<PeerId>,
    /// Peers the user has explicitly refused: their handshakes are rejected
    /// (no key registration, no drive added) even if their key is valid.
    banned: HashSet<PeerId>,
    /// Failed auth (token) attempts per peer: `(count, last-attempt)`. A
    /// peer that crosses [`AUTH_BAN_AFTER`] within [`AUTH_BAN_WINDOW`] is
    /// auto-banned.
    auth_failures: HashMap<PeerId, (u32, tokio::time::Instant)>,
    cmd_rx: mpsc::UnboundedReceiver<EngineCommand>,
    evt_tx: mpsc::UnboundedSender<EngineEvent>,
    running: bool,
    /// Whether the idle notice has already been emitted.
    idle_notified: bool,
    /// Whether the resolved listen address has been announced yet.
    identity_sent: bool,

    /// Remote actions that arrived under a model this peer does not have
    /// yet, held for re-application once the definition is fetched over
    /// the mesh (a missing model is a request, not a rejection).
    pending_actions: Vec<(ModelRef, Action)>,
    /// `(name, version)` of models already requested over the mesh (so a
    /// missing model is not re-requested in a loop).
    requested_models: HashSet<(String, String)>,
    /// Actions already published to the mesh (content hash -> true), so
    /// the periodic drain never publishes what the outbound feed already
    /// did. Bounded by [`PUBLISHED_CAP`] via `published_order`.
    published: HashSet<Hash32>,
    /// Insertion order for [`Self::published`] (FIFO eviction).
    published_order: VecDeque<Hash32>,
}

#[derive(NetworkBehaviour)]
struct SyncBehaviour {
    gossipsub: gossipsub::Behaviour,
    sync: request_response::Behaviour<SyncCodec>,
    mdns: Toggle<mdns::tokio::Behaviour>,
    identify: identify::Behaviour,
    kad: Toggle<kad::Behaviour<kad::store::MemoryStore>>,
    /// Circuit relay server: lets this node forward connections for peers
    /// behind NAT. Config-gated (`p2p.relay`).
    relay_server: Toggle<relay::Behaviour>,
    /// Circuit relay client: dials through a relay for NAT traversal.
    relay_client: relay::client::Behaviour,
}

impl SyncEngine {
    /// Builds the engine. `listen` is the configured listen multiaddr.
    /// `token` is the local shared secret (already env-resolved).
    #[allow(clippy::too_many_arguments)] // every parameter is required construction input
    pub async fn new(
        key: &Keypair,
        store: Arc<Store>,
        pub_name: &str,
        listen: Multiaddr,
        listen_ws: Option<Multiaddr>,
        external: Vec<Multiaddr>,
        mdns_enabled: bool,
        dht_enabled: bool,
        relay_enabled: bool,
        token: Option<String>,
        banned: HashSet<PeerId>,
        cmd_rx: mpsc::UnboundedReceiver<EngineCommand>,
        evt_tx: mpsc::UnboundedSender<EngineEvent>,
    ) -> anyhow::Result<Self> {
        let peer_id = peer_id_of(key);

        // Pre-build the fallible behaviours before the builder (the relay
        // client is only available inside the `with_behaviour` closure, and
        // that closure must return a plain behaviour).
        let gs_config = gossipsub::ConfigBuilder::default()
            .heartbeat_interval(Duration::from_secs(1))
            .duplicate_cache_time(Duration::from_secs(15))
            .flood_publish(true)
            .validation_mode(gossipsub::ValidationMode::Strict)
            .build()
            .map_err(|e| anyhow::anyhow!("gossipsub config: {e}"))?;
        let gossipsub = gossipsub::Behaviour::new(
            gossipsub::MessageAuthenticity::Signed(key.clone()),
            gs_config,
        )
        .map_err(|e| anyhow::anyhow!("gossipsub behaviour: {e}"))?;
        let sync = request_response::Behaviour::new(
            [(SYNC_PROTO, request_response::ProtocolSupport::Full)],
            request_response::Config::default(),
        );
        let mdns = {
            let m = mdns::tokio::Behaviour::new(mdns::Config::default(), peer_id)
                .map_err(anyhow::Error::from)?;
            Toggle::from(if mdns_enabled { Some(m) } else { None })
        };
        let id_cfg = identify::Config::new("1.0.0".to_string(), key.public())
            .with_agent_version("ph-reactor/1.0.0".to_string());
        let identify = identify::Behaviour::new(id_cfg);
        let kad = {
            let k = if dht_enabled {
                let mut cfg = kad::Config::new(kad::PROTOCOL_NAME);
                cfg.set_query_timeout(Duration::from_secs(30));
                let store = kad::store::MemoryStore::new(peer_id);
                let mut k = kad::Behaviour::with_config(peer_id, store, cfg);
                k.set_mode(Some(kad::Mode::Server));
                Some(k)
            } else {
                None
            };
            Toggle::from(k)
        };

        let swarm = SwarmBuilder::with_existing_identity(key.clone())
            .with_tokio()
            .with_tcp(
                tcp::Config::default(),
                noise::Config::new,
                yamux::Config::default,
            )
            .map_err(|e| anyhow::anyhow!("tcp transport: {e}"))?
            // DNS first: the builder's phase order requires it before the
            // websocket step, and it is what makes /dns4/ and /dns6/
            // multiaddrs resolve at all -- without it a hostname-based
            // bootstrap address simply fails to dial.
            .with_dns()
            .map_err(|e| anyhow::anyhow!("dns transport: {e}"))?
            // WebSocket is a second transport, not a replacement: it lets a
            // peer reach this node over :443 through a reverse proxy, which
            // traverses networks that block an arbitrary high TCP port.
            .with_websocket(noise::Config::new, yamux::Config::default)
            .await
            .map_err(|e| anyhow::anyhow!("websocket transport: {e}"))?
            .with_relay_client(noise::Config::new, yamux::Config::default)
            .map_err(|e| anyhow::anyhow!("relay client: {e}"))?
            .with_behaviour(move |_kp, relay_client| {
                let relay_server = Toggle::from(
                    relay_enabled
                        .then(|| relay::Behaviour::new(peer_id_of(_kp), relay::Config::default())),
                );
                SyncBehaviour {
                    gossipsub,
                    sync,
                    mdns,
                    identify,
                    kad,
                    relay_server,
                    relay_client,
                }
            })
            .map_err(|e| anyhow::anyhow!("behaviour: {e}"))?
            .build();

        Ok(Self {
            swarm,
            store,
            peer_id,
            pub_name: pub_name.to_string(),
            listen,
            listen_ws,
            external,
            token,
            banned,
            drives: HashMap::new(),
            guests: HashSet::new(),
            auth_failures: HashMap::new(),
            cmd_rx,
            evt_tx,
            running: true,
            idle_notified: false,
            identity_sent: false,
            pending_actions: Vec::new(),
            requested_models: HashSet::new(),
            published: HashSet::new(),
            published_order: VecDeque::new(),
        })
    }

    /// The main loop: swarm events, commands, the outbound feed, and the
    /// catch-up tick.
    pub async fn run(mut self) {
        // Subscribe to the mesh topic up front.
        let topic = gossipsub::IdentTopic::new(GOSSIPSUB_TOPIC);
        let _ = self.swarm.behaviour_mut().gossipsub.subscribe(&topic);

        // Listen. The resolved address (port 0 included) is only known
        // once the swarm polls the listener; it is announced through the
        // NewListenAddr event, not here.
        if let Err(err) = self.swarm.listen_on(self.listen.clone()) {
            tracing::warn!("cannot listen on {:?}: {err:?}", self.listen);
        }
        if let Some(ws) = self.listen_ws.clone() {
            match self.swarm.listen_on(ws.clone()) {
                Ok(_) => tracing::info!("websocket listener on {ws}"),
                Err(err) => tracing::warn!("cannot listen on {ws:?}: {err:?}"),
            }
        }

        // Announce the addresses peers must dial. A node behind a load
        // balancer or reverse proxy cannot observe these itself, so without
        // them it advertises only a private bind address that nobody can
        // reach -- and identify, Kademlia and the relay all propagate that
        // useless address to the rest of the mesh.
        for addr in self.external.clone() {
            tracing::info!("announcing external address {addr}");
            self.swarm.add_external_address(addr);
        }

        // The store's outbound feed: local actions are published as soon
        // as they are applied (the tick's drain remains as a backstop for
        // actions applied before this point; dedupe makes it lossless).
        let mut outbound_rx = self.store.connect_outbound();

        let mut tick = tokio::time::interval(TICK_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut last_activity = tokio::time::Instant::now();
        while self.running {
            tokio::select! {
                _ = tick.tick() => {
                    self.tick(last_activity);
                }
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(cmd) => self.handle_cmd(cmd),
                        None => break,
                    }
                }
                event = self.swarm.select_next_some() => {
                    self.on_swarm_event(event);
                    last_activity = tokio::time::Instant::now();
                }
                action = outbound_rx.recv() => {
                    if let Some(action) = action {
                        self.publish_action(&action);
                    }
                }
            }
        }
        tracing::debug!("engine shutting down");
    }

    /// Publishes one of our own actions to the mesh topic exactly once.
    /// The dedupe set covers the overlap between the immediate outbound
    /// feed and the periodic drain.
    fn publish_action(&mut self, action: &Action) {
        let h = action.hash();
        if !self.published.insert(h) {
            return;
        }
        self.published_order.push_back(h);
        while self.published_order.len() > PUBLISHED_CAP {
            if let Some(old) = self.published_order.pop_front() {
                self.published.remove(&old);
            }
        }
        let topic = gossipsub::IdentTopic::new(GOSSIPSUB_TOPIC);
        let msg = ActionMsg {
            action: action.clone(),
            name: Some(self.pub_name.clone()),
        };
        match serde_json::to_vec(&msg) {
            Ok(bytes) => {
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .gossipsub
                    .publish(topic.hash(), bytes);
            }
            Err(e) => tracing::warn!("gossip encode failed: {e}"),
        }
    }

    // -- commands -------------------------------------------------------

    fn handle_cmd(&mut self, cmd: EngineCommand) {
        match cmd {
            EngineCommand::AddDrive(drive) => {
                self.add_drive_internal(drive);
            }
            EngineCommand::Join { drive, accept } => {
                let name = drive.name.clone();
                self.add_drive_internal(drive);
                if let Some(rt) = self.drives.get_mut(&name) {
                    rt.accept = Some(accept);
                }
            }
            EngineCommand::Ban { peer } => {
                if self.banned.insert(peer) {
                    tracing::info!(%peer, "peer banned: its handshakes will be refused");
                }
            }
            EngineCommand::Unban { peer } => {
                if self.banned.remove(&peer) {
                    tracing::info!(%peer, "peer unbanned");
                }
            }
            EngineCommand::RemoveDrive { name } => {
                let was = self.drives.remove(&name).is_some();
                if was {
                    tracing::info!("removing drive '{name}'");
                    self.set_status(&name, DriveStatus::Offline, None);
                }
            }
            EngineCommand::SetPaused { name, paused } => {
                if let Some(rt) = self.drives.get_mut(&name) {
                    rt.drive.paused = paused;
                    rt.status = if paused {
                        DriveStatus::Paused
                    } else {
                        DriveStatus::Connecting
                    };
                    rt.next_catch_up = tokio::time::Instant::now();
                    if !paused {
                        let _ = self.swarm.dial(
                            DialOpts::unknown_peer_id()
                                .address(rt.drive.addr.clone())
                                .build(),
                        );
                    }
                }
            }
            EngineCommand::Resync { name } => {
                if let Some(rt) = self.drives.get_mut(&name) {
                    if rt.handshaken {
                        rt.next_catch_up = tokio::time::Instant::now();
                        self.set_status(
                            &name,
                            DriveStatus::Connecting,
                            Some("re-sync requested".into()),
                        );
                    } else {
                        self.set_status(
                            &name,
                            DriveStatus::Connecting,
                            Some("re-sync requested (handshaking)".into()),
                        );
                    }
                }
            }
            EngineCommand::DhtBootstrap { peers } => {
                // Seed the swarm's dialer with the bootstrap peers first
                // (no kad borrow yet), then hand them to the DHT.
                for (peer, addr) in &peers {
                    self.swarm.add_peer_address(*peer, addr.clone());
                }
                if let Some(kad) = self.swarm.behaviour_mut().kad.as_mut() {
                    for (peer, addr) in &peers {
                        kad.add_address(peer, addr.clone());
                    }
                    if peers.is_empty() {
                        tracing::debug!("dht: no bootstrap peers, skipping");
                    } else {
                        let _ = kad.bootstrap();
                        tracing::info!("dht: bootstrap initiated");
                    }
                }
            }
            EngineCommand::PublishProvider { name, doc } => {
                if let Some(kad) = self.swarm.behaviour_mut().kad.as_mut() {
                    let key = kad::RecordKey::new(&doc.to_string());
                    let _ = kad.start_providing(key);
                    tracing::info!(%doc, %name, "dht: published provider record");
                }
            }
            EngineCommand::FindProviders { key } => {
                if let Some(kad) = self.swarm.behaviour_mut().kad.as_mut() {
                    let rk = kad::RecordKey::new(&key);
                    let _ = kad.get_providers(rk);
                }
            }
            EngineCommand::Shutdown => {
                self.running = false;
            }
        }
    }

    /// Shared drive-add logic: register the runtime and start dialing.
    /// Used by [`EngineCommand::AddDrive`] and [`EngineCommand::Join`].
    fn add_drive_internal(&mut self, drive: Drive) {
        let peer = drive
            .addr
            .iter()
            .find_map(|p| {
                if let libp2p::multiaddr::Protocol::P2p(pid) = p {
                    Some(pid)
                } else {
                    None
                }
            })
            .unwrap_or_else(PeerId::random);
        let now = tokio::time::Instant::now();
        let rt = DriveRuntime {
            drive: drive.clone(),
            peer,
            status: if drive.paused {
                DriveStatus::Paused
            } else {
                DriveStatus::Connecting
            },
            remote_clocks: HashMap::new(),
            in_flight: HashSet::new(),
            handshaken: false,
            next_catch_up: now,
            last_seen: now,
            accept: None,
        };
        let name = rt.drive.name.clone();
        let is_new = self.drives.insert(name.clone(), rt).is_none();
        if is_new {
            tracing::info!("adding drive '{name}' ({peer})");
        }
        if !drive.paused {
            // A hello from this peer may have arrived before the drive
            // existed: that half-completed the handshake (their key is
            // registered), so finish it now.
            if self.guests.contains(&peer) {
                if let Some(rt) = self.drives.get_mut(&name) {
                    rt.handshaken = true;
                    rt.last_seen = now;
                }
                self.set_status(&name, DriveStatus::Connecting, None);
            }
            // The peer may already be connected (their dial reached us
            // before this drive existed, so the ConnectionEstablished
            // handshake never fired for it): open the handshake on the
            // existing connection.
            if self.drives.get(&name).is_some_and(|rt| !rt.handshaken)
                && self.swarm.is_connected(&peer)
            {
                self.open_handshake(&name);
            }
            let _ = self.swarm.dial(
                DialOpts::peer_id(peer)
                    .addresses(vec![drive.addr.clone()])
                    .build(),
            );
            self.set_status(&name, DriveStatus::Connecting, Some("dialing".into()));
        }
    }

    /// Verify a join-proof and add a drive for the joiner so the pair can
    /// sync. The proof is signed by the joiner's key (pinned via TOFU) and
    /// echoes the invite's nonce; on success a drive named after the
    /// joiner is added, addressed by the transport-verified connection peer.
    fn handle_invite_accept(&mut self, peer: PeerId, accept: invite::InviteAccept) {
        // A banned peer is refused even with a valid join-proof.
        if self.banned.contains(&peer) {
            tracing::warn!(%peer, "banned peer attempted to join; refusing");
            return;
        }
        if accept.verify().is_err() {
            tracing::warn!(%peer, "invite proof failed verification; ignoring");
            return;
        }
        let mut key_arr = [0u8; 32];
        key_arr.copy_from_slice(&accept.pubkey);
        if self
            .store
            .register_peer_key(&accept.peer_id, key_arr)
            .is_err()
        {
            tracing::warn!(%peer, "invite proof key mismatch (TOFU); not adding a drive");
            return;
        }
        let short = peer.to_base58();
        let name = format!("{} ({})", accept.name, &short[..short.len().min(12)]);
        let addr = match format!("/p2p/{short}").parse::<Multiaddr>() {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(%peer, "bad joiner addr {e}; not adding a drive");
                return;
            }
        };
        let drive = Drive {
            name: name.clone(),
            addr: addr.clone(),
            token_env: None,
            paused: false,
            available_offline: true,
        };
        tracing::info!(%peer, "join proof accepted; adding a drive for the joiner");
        self.add_drive_internal(drive);
        let _ = self.evt_tx.send(EngineEvent::DriveJoined { name, addr });
        let _ = self.evt_tx.send(EngineEvent::InviteAccepted {
            peer,
            nonce: accept.nonce.clone(),
        });
    }

    fn set_status(&self, name: &str, status: DriveStatus, detail: Option<String>) {
        let _ = self.evt_tx.send(EngineEvent::DriveStatus {
            name: name.into(),
            status,
            detail,
        });
    }

    /// Records a failed auth (wrong-token) attempt from `peer`. Returns
    /// `true` if the peer crossed [`AUTH_BAN_AFTER`] within
    /// [`AUTH_BAN_WINDOW`] and was just auto-banned.
    fn record_auth_failure(&mut self, peer: &PeerId) -> bool {
        let now = tokio::time::Instant::now();
        let count = {
            let entry = self.auth_failures.entry(*peer).or_insert((0, now));
            if now.duration_since(entry.1) > AUTH_BAN_WINDOW {
                entry.0 = 0;
            }
            entry.0 += 1;
            entry.1 = now;
            entry.0
        };
        if count >= AUTH_BAN_AFTER {
            self.auth_failures.remove(peer);
            self.banned.insert(*peer);
            let detail = format!("auto-banned after {count} failed auth attempts");
            if let Some(name) = self.drive_name_by_peer(*peer) {
                self.set_status(&name, DriveStatus::Error, Some(detail.clone()));
            }
            let _ = self.evt_tx.send(EngineEvent::PeerAutoBanned {
                peer: peer.to_base58(),
            });
            tracing::warn!(%peer, "auto-banned after {count} failed auth attempts");
            true
        } else {
            false
        }
    }

    // -- the tick ---------------------------------------------------------

    fn tick(&mut self, last_activity: tokio::time::Instant) {
        // Backstop for the outbound feed: publish whatever is still in
        // the store's queue (actions applied before the feed was
        // connected). The dedupe set makes a double delivery a no-op.
        for action in self.store.drain_outbound() {
            self.publish_action(&action);
        }
        // Idle accounting: any drive activity resets the window.
        let idle_for = last_activity.elapsed();
        if idle_for > IDLE_AFTER_SILENCE && !self.idle_notified {
            self.idle_notified = true;
            tracing::debug!(
                "engine idle for {idle_for:?} (the daemon keeps the store warm; nothing to do)"
            );
        } else if idle_for < IDLE_AFTER_SILENCE {
            self.idle_notified = false;
        }

        let now = tokio::time::Instant::now();
        let mut to_catch: Vec<(String, Vec<(DocId, VecClock)>)> = Vec::new();
        let mut to_summarize: Vec<(String, PeerId)> = Vec::new();

        let own = self.store.summary();
        for (name, rt) in self.drives.iter() {
            if rt.drive.paused || !rt.handshaken || now < rt.next_catch_up {
                continue;
            }
            // Docs where they are ahead of us.
            let mut gaps = Vec::new();
            for (id, their) in &rt.remote_clocks {
                let ours = own.get(id).cloned().unwrap_or_default();
                if !ours.covers(their) {
                    gaps.push((*id, ours));
                }
            }
            to_catch.push((name.clone(), gaps));
            to_summarize.push((name.clone(), rt.peer));
        }

        for (name, gaps) in to_catch {
            let Some(rt) = self.drives.get_mut(&name) else {
                continue;
            };
            rt.next_catch_up = now + CATCH_UP_TICK;
            for (id, have) in gaps {
                let req = SyncMsg::CatchUp(CatchUp { doc_id: id, have });
                let rq = self.swarm.behaviour_mut().sync.send_request(&rt.peer, req);
                rt.in_flight.insert(rq);
            }
        }
        // Proactive summary exchange: give them our clocks so they can
        // catch up to what we have.
        for (name, peer) in to_summarize {
            let clocks: Vec<(DocId, VecClock)> =
                own.iter().map(|(id, c)| (*id, c.clone())).collect();
            let _ = self
                .swarm
                .behaviour_mut()
                .sync
                .send_request(&peer, SyncMsg::Summary(Summary { clocks }));
            if let Some(rt) = self.drives.get_mut(&name) {
                if rt.status == DriveStatus::Connecting {
                    // A full exchange round completed with no gaps
                    // outstanding: the drive is synced.
                    rt.status = DriveStatus::Synced;
                    self.set_status(&name, DriveStatus::Synced, None);
                }
            }
        }
    }

    // -- swarm events ------------------------------------------------------

    fn on_swarm_event(&mut self, event: SwarmEvent<SyncBehaviourEvent>) {
        match event {
            SwarmEvent::NewListenAddr { address, .. } => {
                if !self.identity_sent {
                    self.identity_sent = true;
                    let _ = self.evt_tx.send(EngineEvent::Identity {
                        peer_id: self.peer_id,
                        listen: address,
                    });
                }
            }
            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                let _ = self
                    .evt_tx
                    .send(EngineEvent::PeerConnected { peer: peer_id });
                let Some(name) = self.drive_name_by_peer(peer_id) else {
                    return;
                };
                // Handshake: we open it with hello (either side may
                // have dialed).
                self.open_handshake(&name);
            }
            SwarmEvent::ConnectionClosed {
                peer_id,
                num_established,
                ..
            } => {
                if num_established == 0 {
                    if let Some(name) = self.drive_name_by_peer(peer_id) {
                        let status = if let Some(rt) = self.drives.get_mut(&name) {
                            rt.handshaken = false;
                            rt.in_flight.clear();
                            rt.status = if rt.drive.paused {
                                DriveStatus::Paused
                            } else {
                                DriveStatus::Connecting
                            };
                            rt.status
                        } else {
                            DriveStatus::Connecting
                        };
                        self.set_status(&name, status, Some("reconnecting".into()));
                    }
                }
            }
            SwarmEvent::Behaviour(ev) => match ev {
                SyncBehaviourEvent::Gossipsub(gossipsub::Event::Message { message, .. }) => {
                    self.on_gossip(&message);
                }
                SyncBehaviourEvent::Gossipsub(gossipsub::Event::Subscribed { topic, .. }) => {
                    tracing::debug!("gossipsub: subscribed {topic}");
                }
                SyncBehaviourEvent::Gossipsub(_) => {}
                SyncBehaviourEvent::Sync(ev) => self.on_sync(ev),
                SyncBehaviourEvent::Mdns(mdns::Event::Discovered(list)) => {
                    for (peer, a) in list {
                        self.swarm.add_peer_address(peer, a);
                    }
                }
                SyncBehaviourEvent::Mdns(_) => {}
                // Identify -> DHT: feed the routing table with the
                // addresses each peer advertises so it can be reached.
                SyncBehaviourEvent::Identify(identify::Event::Received {
                    peer_id, info, ..
                }) => {
                    if let Some(kad) = self.swarm.behaviour_mut().kad.as_mut() {
                        for addr in &info.listen_addrs {
                            kad.add_address(&peer_id, addr.clone());
                        }
                    }
                }
                SyncBehaviourEvent::Identify(_) => {}
                SyncBehaviourEvent::Kad(kad::Event::OutboundQueryProgressed { result, .. }) => {
                    match result {
                        kad::QueryResult::Bootstrap(Ok(_)) => {
                            let _ = self.evt_tx.send(EngineEvent::DhtBootstrap(true));
                        }
                        kad::QueryResult::Bootstrap(Err(e)) => {
                            tracing::debug!("dht bootstrap failed: {e:?}");
                            let _ = self.evt_tx.send(EngineEvent::DhtBootstrap(false));
                        }
                        kad::QueryResult::GetProviders(Ok(
                            kad::GetProvidersOk::FoundProviders { key, providers },
                        )) => {
                            for peer in providers {
                                if peer != self.peer_id {
                                    let _ = self.evt_tx.send(EngineEvent::DhtProvider {
                                        key: key.to_vec(),
                                        peer,
                                    });
                                }
                            }
                        }
                        _ => {}
                    }
                }
                SyncBehaviourEvent::Kad(kad::Event::RoutingUpdated { peer, .. }) => {
                    tracing::debug!("dht: routing table updated for {peer}");
                }
                SyncBehaviourEvent::Kad(_) => {}
                // Relay events (client + server) are handled internally by
                // the behaviours; nothing to fold into the engine here yet.
                SyncBehaviourEvent::RelayServer(_) => {}
                SyncBehaviourEvent::RelayClient(_) => {}
            },
            _ => {}
        }
    }

    // -- gossip -------------------------------------------------------------

    fn on_gossip(&mut self, message: &gossipsub::Message) {
        let Ok(ActionMsg { action, .. }) = serde_json::from_slice(&message.data) else {
            return;
        };
        self.apply_remote_action(message.source, &action);
    }

    // -- request/response ---------------------------------------------------

    fn on_sync(&mut self, ev: request_response::Event<SyncMsg, SyncMsg>) {
        match ev {
            request_response::Event::Message {
                peer,
                message:
                    request_response::Message::Request {
                        request, channel, ..
                    },
                ..
            } => self.serve_request(peer, request, channel),
            request_response::Event::Message {
                peer,
                message:
                    request_response::Message::Response {
                        request_id,
                        response,
                    },
                ..
            } => self.on_response(peer, request_id, response),
            request_response::Event::OutboundFailure { peer, .. } => {
                if let Some(name) = self.drive_name_by_peer(peer) {
                    let paused = if let Some(rt) = self.drives.get_mut(&name) {
                        rt.status == DriveStatus::Paused
                    } else {
                        false
                    };
                    if !paused {
                        if let Some(rt) = self.drives.get_mut(&name) {
                            rt.status = DriveStatus::Connecting;
                        }
                        self.set_status(
                            &name,
                            DriveStatus::Connecting,
                            Some("request failed (will retry)".into()),
                        );
                    }
                }
            }
            request_response::Event::InboundFailure { .. } => {}
            request_response::Event::ResponseSent { .. } => {}
        }
    }

    /// Serves an inbound request from a remote instance.
    fn serve_request(
        &mut self,
        peer: PeerId,
        request: SyncMsg,
        channel: request_response::ResponseChannel<SyncMsg>,
    ) {
        match request {
            SyncMsg::Hello(mut h) => {
                // Ban gate: a refused peer never completes a handshake here,
                // regardless of version, token, or key validity.
                if self.banned.contains(&peer) {
                    let _ = self
                        .swarm
                        .behaviour_mut()
                        .sync
                        .send_response(channel, SyncMsg::HelloErr(HelloError::Banned));
                    return;
                }
                // Version gate.
                if h.version != PROTOCOL_VERSION {
                    let _ = self.swarm.behaviour_mut().sync.send_response(
                        channel,
                        SyncMsg::HelloErr(HelloError::BadVersion {
                            expected: PROTOCOL_VERSION,
                            got: h.version,
                        }),
                    );
                    return;
                }
                // A joiner proves it holds a valid invite: verify the proof
                // and add a drive for it so the two can sync (the proof
                // carries the joiner's name, key, and the echoed nonce).
                if let Some(accept) = h.accept.take() {
                    self.handle_invite_accept(peer, accept);
                }
                // Token gate: if we require one, the hello must carry it.
                // A provided-but-wrong token is a failed auth attempt;
                // crossing the threshold auto-bans the peer.
                let need = self.token.clone();
                if let Some(need) = &need {
                    match h.token.as_deref() {
                        Some(got) if got == need => {}
                        Some(_) => {
                            let err = if self.record_auth_failure(&peer) {
                                HelloError::Banned
                            } else {
                                HelloError::BadToken
                            };
                            let _ = self
                                .swarm
                                .behaviour_mut()
                                .sync
                                .send_response(channel, SyncMsg::HelloErr(err));
                            return;
                        }
                        None => {
                            let _ = self
                                .swarm
                                .behaviour_mut()
                                .sync
                                .send_response(channel, SyncMsg::HelloErr(HelloError::BadToken));
                            return;
                        }
                    }
                }
                // Register the peer's key so its ops verify. The
                // store verifies ops by their origin (the writer's
                // own peer id), so the key is registered under the
                // *verified connection identity*, not the drive name.
                if let Ok(kb) = hex::decode(&h.pubkey) {
                    if kb.len() == 32 {
                        let mut arr = [0u8; 32];
                        arr.copy_from_slice(&kb);
                        if let Some(name) = self.drive_name_by_peer(peer) {
                            let auth_required = if let Some(rt) = self.drives.get_mut(&name) {
                                rt.drive.token_env.is_some() && h.token.is_none()
                            } else {
                                false
                            };
                            if auth_required {
                                let _ = self.swarm.behaviour_mut().sync.send_response(
                                    channel,
                                    SyncMsg::HelloErr(HelloError::AuthRequired),
                                );
                                return;
                            }
                            if let Some(rt) = self.drives.get_mut(&name) {
                                rt.handshaken = true;
                                rt.last_seen = tokio::time::Instant::now();
                                rt.remote_clocks.clear();
                                rt.status = DriveStatus::Connecting;
                            }
                            self.set_status(&name, DriveStatus::Connecting, None);
                        }
                        // Remember the peer even when no drive for it
                        // exists yet (a drive added later completes the
                        // handshake from this record).
                        self.guests.insert(peer);
                        if self
                            .store
                            .register_peer_key(&peer.to_base58(), arr)
                            .is_err()
                        {
                            tracing::warn!(%peer, "TOFU key mismatch: refusing the handshake");
                            let _ = self
                                .swarm
                                .behaviour_mut()
                                .sync
                                .send_response(channel, SyncMsg::HelloErr(HelloError::KeyMismatch));
                            return;
                        }
                    }
                }
                // Ack with our full doc summary: the dialer plans
                // catch-up from this.
                let own = self.store.summary();
                let docs: Vec<DocSummary> = own
                    .iter()
                    .map(|(id, clock)| DocSummary {
                        id: *id,
                        name: self.store.doc_name(*id),
                        clock: clock.clone(),
                    })
                    .collect();
                let ack = HelloAck {
                    version: PROTOCOL_VERSION,
                    name: self.pub_name.clone(),
                    docs,
                    pubkey: Some(hex::encode(self.store.key().verifying_key().to_bytes())),
                };
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .sync
                    .send_response(channel, SyncMsg::HelloAck(ack));
            }
            SyncMsg::CatchUp(c) => {
                let (state, mut actions) = self.store.catch_up(c.doc_id, &c.have);
                let more = actions.len() > CATCH_UP_MAX_ACTIONS;
                actions.truncate(CATCH_UP_MAX_ACTIONS);
                let ack = CatchUpAck {
                    doc_id: c.doc_id,
                    state,
                    actions,
                    more,
                };
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .sync
                    .send_response(channel, SyncMsg::CatchUpAck(ack));
            }
            SyncMsg::Summary(s) => {
                // Their clocks: remember them for the next tick and
                // answer with ours.
                if let Some(rt) = self.find_drive_rt_mut(peer).map(|(_, rt)| rt) {
                    for (id, clock) in &s.clocks {
                        rt.remote_clocks.insert(*id, clock.clone());
                    }
                    rt.last_seen = tokio::time::Instant::now();
                }
                let own: Vec<(DocId, VecClock)> = self.store.summary().into_iter().collect();
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .sync
                    .send_response(channel, SyncMsg::SummaryAck(SummaryAck { clocks: own }));
            }
            SyncMsg::ModelRequest(r) => {
                // A peer wants a model definition it does not have. Answer
                // with ours if we have one; the requester verifies the reply
                // against the pinned hash before registering it.
                let def = self.store.model_definition(&r.ref_);
                let _ = self
                    .swarm
                    .behaviour_mut()
                    .sync
                    .send_response(channel, SyncMsg::ModelDef(ModelDef { ref_: r.ref_, def }));
            }
            // Rejections/acks we initiated on the outbound side.
            SyncMsg::HelloAck(_)
            | SyncMsg::CatchUpAck(_)
            | SyncMsg::SummaryAck(_)
            | SyncMsg::HelloErr(_)
            | SyncMsg::ModelDef(_) => {}
        }
    }

    /// Handles a response to one of our outbound requests.
    fn on_response(&mut self, peer: PeerId, request_id: OutboundRequestId, response: SyncMsg) {
        let Some(name) = self.drive_name_by_peer(peer) else {
            return;
        };
        if let Some(rt) = self.drives.get_mut(&name) {
            rt.in_flight.remove(&request_id);
        }
        match response {
            SyncMsg::HelloAck(ack) => {
                // Register the responder's signing key under its verified
                // identity so its ops verify even when it never dials us.
                if let Some(pk) = &ack.pubkey {
                    if let Ok(kb) = hex::decode(pk) {
                        if kb.len() == 32 {
                            let mut arr = [0u8; 32];
                            arr.copy_from_slice(&kb);
                            if self
                                .store
                                .register_peer_key(&peer.to_base58(), arr)
                                .is_err()
                            {
                                self.set_status(
                                    &name,
                                    DriveStatus::Error,
                                    Some(
                                        "TOFU key mismatch: the peer presented a different key \
                                         than was pinned on first contact"
                                            .to_string(),
                                    ),
                                );
                                tracing::warn!(
                                    %peer,
                                    "TOFU key mismatch on HelloAck: drive marked as error"
                                );
                                return;
                            }
                        }
                    }
                }
                let fresh = if let Some(rt) = self.drives.get_mut(&name) {
                    if !rt.handshaken {
                        rt.handshaken = true;
                        rt.last_seen = tokio::time::Instant::now();
                        rt.remote_clocks.clear();
                        rt.status = DriveStatus::Connecting;
                    }
                    true
                } else {
                    false
                };
                if fresh {
                    self.set_status(&name, DriveStatus::Connecting, None);
                }
                let mut remote = if let Some(rt) = self.drives.get_mut(&name) {
                    rt.remote_clocks.clone()
                } else {
                    HashMap::new()
                };
                for d in &ack.docs {
                    remote.insert(d.id, d.clock.clone());
                }
                if let Some(rt) = self.drives.get_mut(&name) {
                    rt.remote_clocks = remote;
                }
                // Kick catch-up for anything they're ahead on.
                let own = self.store.summary();
                let gaps = if let Some(rt) = self.drives.get_mut(&name) {
                    rt.remote_clocks
                        .iter()
                        .filter(|(id, their)| {
                            !own.get(*id).cloned().unwrap_or_default().covers(their)
                        })
                        .map(|(id, _)| (*id, own.get(id).cloned().unwrap_or_default()))
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                for (doc_id, have) in gaps {
                    let req = SyncMsg::CatchUp(CatchUp { doc_id, have });
                    let rq = self.swarm.behaviour_mut().sync.send_request(&peer, req);
                    if let Some(rt) = self.drives.get_mut(&name) {
                        rt.in_flight.insert(rq);
                    }
                }
            }
            SyncMsg::HelloErr(err) => {
                let (status, detail) = match &err {
                    HelloError::BadVersion { expected, got } => (
                        DriveStatus::Error,
                        format!("version mismatch (theirs {got}, ours {expected})"),
                    ),
                    HelloError::AuthRequired => (
                        DriveStatus::RequiresAuth,
                        "the drive requires a token (p2p.tokenEnv)".to_string(),
                    ),
                    HelloError::BadToken => (
                        DriveStatus::RequiresAuth,
                        "token rejected by the drive".to_string(),
                    ),
                    HelloError::KeyMismatch => (
                        DriveStatus::Error,
                        "TOFU key mismatch: the peer's key differs from the pinned key".to_string(),
                    ),
                    HelloError::Banned => (
                        DriveStatus::Error,
                        "banned: the peer is refused by the remote (or is on our ban list)"
                            .to_string(),
                    ),
                };
                if let Some(rt) = self.drives.get_mut(&name) {
                    rt.status = status;
                }
                self.set_status(&name, status, Some(detail));
            }
            SyncMsg::CatchUpAck(ack) => {
                let mut applied = 0usize;
                for action in &ack.actions {
                    if self.apply_remote_action(Some(peer), action) {
                        applied += 1;
                    }
                }
                let mut synced = false;
                if let Some(rt) = self.drives.get_mut(&name) {
                    if let Some(clock) = ack.actions.last().map(|a| a.clock.clone()) {
                        rt.remote_clocks.insert(ack.doc_id, clock);
                    }
                    rt.last_seen = tokio::time::Instant::now();
                    if !ack.more && rt.in_flight.is_empty() && rt.status != DriveStatus::Paused {
                        rt.status = DriveStatus::Synced;
                        synced = true;
                    }
                }
                if synced {
                    self.set_status(
                        &name,
                        DriveStatus::Synced,
                        Some(format!("{applied} actions applied")),
                    );
                }
            }
            SyncMsg::SummaryAck(s) => {
                if let Some(rt) = self.drives.get_mut(&name) {
                    for (id, clock) in &s.clocks {
                        rt.remote_clocks.insert(*id, clock.clone());
                    }
                }
            }
            // A model definition arrived over the mesh: verify it against
            // the pinned hash, register it, and re-apply the actions that
            // were held waiting for it.
            SyncMsg::ModelDef(d) => {
                let Some(def) = d.def else {
                    return;
                };
                let name_ok = def.get("name").and_then(Value::as_str) == Some(d.ref_.name.as_str());
                let ver_ok =
                    def.get("version").and_then(Value::as_str) == Some(d.ref_.version.as_str());
                if !(name_ok && ver_ok) {
                    return;
                }
                if let Some(want) = d.ref_.hash {
                    if model_def_hash(&def) != want {
                        tracing::warn!(
                            "model {} definition failed hash verification; dropped",
                            d.ref_
                        );
                        return;
                    }
                }
                let l1 = match L1::from_def(def) {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::warn!("model {} definition rejected: {e}", d.ref_);
                        return;
                    }
                };
                let key = (d.ref_.name.clone(), d.ref_.version.clone());
                self.store.add_model(Arc::new(l1));
                self.requested_models.remove(&key);
                let to_apply: Vec<Action> = self
                    .pending_actions
                    .iter()
                    .filter(|(m, _)| m.name == key.0 && m.version == key.1)
                    .map(|(_, a)| a.clone())
                    .collect();
                self.pending_actions
                    .retain(|(m, _)| !(m.name == key.0 && m.version == key.1));
                for a in to_apply {
                    self.apply_remote_action(Some(peer), &a);
                }
            }
            // We do not initiate these.
            SyncMsg::Hello(_)
            | SyncMsg::CatchUp(_)
            | SyncMsg::Summary(_)
            | SyncMsg::ModelRequest(_) => {}
        }
    }

    // -- helpers ---------------------------------------------------------------

    /// Applies a remote action through the store (signature-verified).
    /// When the action's model is not loaded, the definition is requested
    /// from `peer` over the mesh and the action is held pending until the
    /// definition arrives (then re-applied). A `None` source is held
    /// pending without a request.
    fn apply_remote_action(&mut self, peer: Option<PeerId>, action: &Action) -> bool {
        if !self.store.model_available(&action.model) {
            if let Some(p) = peer {
                self.request_model(p, &action.model);
            }
            self.note_pending(action);
            return false;
        }
        match self.store.apply_remote_action(action) {
            Ok(r) if r.applied => {
                let name = self.store.doc_name(action.doc_id);
                let _ = self
                    .evt_tx
                    .send(EngineEvent::DocChanged { name: Some(name) });
                true
            }
            Ok(_) => false,
            Err(e) => {
                tracing::warn!("action from {} quarantined: {e}", action.origin);
                false
            }
        }
    }

    /// Ask `peer` for the definition of a model this peer lacks, once.
    fn request_model(&mut self, peer: PeerId, ref_: &ModelRef) {
        let key = (ref_.name.clone(), ref_.version.clone());
        if self.requested_models.contains(&key) {
            return;
        }
        self.requested_models.insert(key);
        let _ = self.swarm.behaviour_mut().sync.send_request(
            &peer,
            SyncMsg::ModelRequest(ModelRequest { ref_: ref_.clone() }),
        );
    }

    /// Hold a remote action pending until its model is available.
    fn note_pending(&mut self, action: &Action) {
        let h = action.hash();
        if self.pending_actions.iter().any(|(_, a)| a.hash() == h) {
            return;
        }
        self.pending_actions
            .push((action.model.clone(), action.clone()));
    }

    /// Opens the hello handshake to the drive's peer (a no-op when the
    /// drive already handshaken or is paused).
    fn open_handshake(&mut self, name: &str) {
        let should_send = self
            .drives
            .get(name)
            .is_some_and(|rt| !rt.handshaken && !rt.drive.paused);
        if !should_send {
            return;
        }
        let peer = self.drives.get(name).unwrap().peer;
        if self.banned.contains(&peer) {
            self.set_status(name, DriveStatus::Error, Some("banned by user".to_string()));
            return;
        }
        let accept = self.drives.get_mut(name).and_then(|rt| rt.accept.take());
        let pubkey = self.store.key().verifying_key().to_bytes();
        let hello = Hello {
            version: PROTOCOL_VERSION,
            name: self.pub_name.clone(),
            peer_id: self.peer_id.to_base58(),
            pubkey: hex::encode(pubkey),
            token: self.token.clone(),
            accept,
        };
        let _ = self
            .swarm
            .behaviour_mut()
            .sync
            .send_request(&peer, SyncMsg::Hello(hello));
    }
    fn drive_name_by_peer(&self, peer: PeerId) -> Option<String> {
        self.drives
            .iter()
            .find(|(_, rt)| rt.peer == peer)
            .map(|(n, _)| n.clone())
    }

    fn find_drive_rt_mut(&mut self, peer: PeerId) -> Option<(&String, &mut DriveRuntime)> {
        self.drives.iter_mut().find(|(_, rt)| rt.peer == peer)
    }
}

/// A deterministic peer id for tests (ed25519 key derived from a fixed
/// seed, so multiaddr fixtures parse as valid `/p2p/` components).
#[cfg(test)]
pub(crate) fn test_peer_id() -> String {
    let sk = ed25519_dalek::SigningKey::from([7u8; 32]);
    let pk = sk.verifying_key().to_bytes();
    let mut mh = [0u8; 34];
    mh[1] = 0x20;
    mh[2..].copy_from_slice(&pk);
    PeerId::from_bytes(&mh)
        .expect("ed25519 multihash")
        .to_base58()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use libp2p::identity::Keypair;
    use std::str::FromStr;
    use tokio::sync::mpsc;

    fn kp_peer_id(kp: &Keypair) -> PeerId {
        kp.public().to_peer_id()
    }

    /// A minimal engine for exercising `record_auth_failure` without the
    /// full handshake (no event loop is run; only the fields matter).
    async fn engine_with_token(token: Option<&str>) -> SyncEngine {
        let key = Keypair::generate_ed25519();
        let peer = kp_peer_id(&key);
        let dir = tempfile::tempdir().expect("temp dir");
        let signing = super::signing_key(&key).expect("signing key");
        let store =
            Store::open(&dir.path().join("docs"), &signing, &peer.to_base58()).expect("store");
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (evt_tx, _evt_rx) = mpsc::unbounded_channel();
        let listen = Multiaddr::from_str("/ip4/127.0.0.1/tcp/0").expect("listen");
        let engine = SyncEngine::new(
            &key,
            store,
            "test",
            listen,
            None,       // no websocket listener
            Vec::new(), // no announced addresses
            false,      // no mDNS
            false,      // no DHT
            false,      // no relay
            token.map(str::to_string),
            HashSet::new(),
            cmd_rx,
            evt_tx,
        )
        .await
        .expect("engine");
        drop(cmd_tx);
        engine
    }

    /// A peer that fails auth (wrong token) `AUTH_BAN_AFTER` times in a row
    /// is auto-banned; a distinct peer is unaffected.
    #[tokio::test]
    async fn auto_ban_after_auth_threshold() {
        let mut e = engine_with_token(Some("secret")).await;
        let attacker = Keypair::generate_ed25519();
        let attacker_id = kp_peer_id(&attacker);

        // Below the threshold: the peer is not banned yet.
        for _ in 0..(AUTH_BAN_AFTER - 1) {
            assert!(!e.record_auth_failure(&attacker_id));
            assert!(!e.banned.contains(&attacker_id));
        }
        // Crossing the threshold auto-bans it.
        assert!(e.record_auth_failure(&attacker_id));
        assert!(e.banned.contains(&attacker_id));

        // A distinct peer is independent: it needs its own threshold.
        let other = Keypair::generate_ed25519();
        let other_id = kp_peer_id(&other);
        assert!(!e.record_auth_failure(&other_id));
        assert!(!e.banned.contains(&other_id));
    }
}
