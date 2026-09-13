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
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use libp2p::gossipsub;
use libp2p::identify;
use libp2p::kad;
use libp2p::relay;
use libp2p::identity::Keypair;
use libp2p::request_response::{self, OutboundRequestId};
use libp2p::swarm::behaviour::toggle::Toggle;
use libp2p::swarm::{dial_opts::DialOpts, NetworkBehaviour, Swarm, SwarmEvent};
use libp2p::{mdns, noise, tcp, yamux, Multiaddr, PeerId, StreamProtocol, SwarmBuilder};
use tokio::sync::mpsc;

use crate::action::Action;
use crate::doc::{DocId, VecClock};
use crate::drives::{Drive, DriveStatus};
use crate::p2p::codec::{
    ActionMsg, CatchUp, CatchUpAck, DocSummary, Hello, HelloAck, HelloError, Summary, SummaryAck,
    SyncCodec, SyncMsg, CATCH_UP_MAX_ACTIONS, GOSSIPSUB_TOPIC, PROTOCOL_VERSION, SYNC_PROTOCOL,
};
use crate::store::Store;

type SyncProtocol = StreamProtocol;

const SYNC_PROTO: SyncProtocol = StreamProtocol::new(SYNC_PROTOCOL);
/// How often an authenticated drive gets a proactive catch-up tick.
const CATCH_UP_TICK: Duration = Duration::from_secs(30);
/// How often the engine wakes to check commands, idle state, and the
/// store's outbound op queue (independent of the per-drive cadence).
const TICK_INTERVAL: Duration = Duration::from_secs(5);
/// A drive that has been silent this long is reported idle (the daemon
/// may let it sleep; the store stays warm).
const IDLE_AFTER_SILENCE: Duration = Duration::from_secs(30 * 60);

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
    /// Last time a doc or hello was seen from this drive.
    last_seen: tokio::time::Instant,
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
    /// Local token (from the config's `p2p.tokenEnv` env var), if any.
    token: Option<String>,
    drives: HashMap<String, DriveRuntime>,
    /// Peers that presented a valid hello (their key is registered) even
    /// though no drive for them existed yet; a drive added later
    /// completes the handshake from this record.
    guests: HashSet<PeerId>,
    cmd_rx: mpsc::UnboundedReceiver<EngineCommand>,
    evt_tx: mpsc::UnboundedSender<EngineEvent>,
    running: bool,
    /// Whether the idle notice has already been emitted.
    idle_notified: bool,
    /// Whether the resolved listen address has been announced yet.
    identity_sent: bool,
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
    pub fn new(
        key: &Keypair,
        store: Arc<Store>,
        pub_name: &str,
        listen: Multiaddr,
        mdns_enabled: bool,
        dht_enabled: bool,
        relay_enabled: bool,
        token: Option<String>,
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
            .with_relay_client(noise::Config::new, yamux::Config::default)
            .map_err(|e| anyhow::anyhow!("relay client: {e}"))?
            .with_behaviour(move |_kp, relay_client| {
                let relay_server = Toggle::from(relay_enabled.then(|| {
                    relay::Behaviour::new(peer_id_of(_kp), relay::Config::default())
                }));
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
            token,
            drives: HashMap::new(),
            guests: HashSet::new(),
            cmd_rx,
            evt_tx,
            running: true,
            idle_notified: false,
            identity_sent: false,
        })
    }

    /// The main loop: swarm events, commands, and the catch-up tick.
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
            }
        }
        tracing::debug!("engine shutting down");
    }

    // -- commands -------------------------------------------------------

    fn handle_cmd(&mut self, cmd: EngineCommand) {
        match cmd {
            EngineCommand::AddDrive(drive) => {
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
                };
                let name = rt.drive.name.clone();
                let is_new = self.drives.insert(name.clone(), rt).is_none();
                if is_new {
                    tracing::info!("adding drive '{name}' ({peer})");
                }
                if !drive.paused {
                    // A hello from this peer may have arrived before
                    // the drive existed: that half-completed the
                    // handshake (their key is registered), so finish it
                    // now.
                    if self.guests.contains(&peer) {
                        if let Some(rt) = self.drives.get_mut(&name) {
                            rt.handshaken = true;
                            rt.last_seen = now;
                        }
                        self.set_status(&name, DriveStatus::Connecting, None);
                    }
                    // The peer may already be connected (their dial
                    // reached us before this drive existed, so the
                    // ConnectionEstablished handshake never fired for
                    // it): open the handshake on the existing
                    // connection.
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

    fn set_status(&self, name: &str, status: DriveStatus, detail: Option<String>) {
        let _ = self.evt_tx.send(EngineEvent::DriveStatus {
            name: name.into(),
            status,
            detail,
        });
    }

    // -- the tick ---------------------------------------------------------

    fn tick(&mut self, last_activity: tokio::time::Instant) {
        // Fan out newly applied local ops to the mesh (the store's
        // outbound queue is local-only; remote ops are not re-gossiped).
        let topic = gossipsub::IdentTopic::new(GOSSIPSUB_TOPIC);
        for action in self.store.drain_outbound() {
            let msg = ActionMsg {
                action,
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

        for (name, rt) in self.drives.iter() {
            if rt.drive.paused || !rt.handshaken || now < rt.next_catch_up {
                continue;
            }
            let own = self.store.summary();
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
            let own: Vec<(DocId, VecClock)> = self.store.summary().into_iter().collect();
            let _ = self
                .swarm
                .behaviour_mut()
                .sync
                .send_request(&peer, SyncMsg::Summary(Summary { clocks: own }));
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
                let _ = self.evt_tx.send(EngineEvent::PeerConnected { peer: peer_id });
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
                    peer_id,
                    info,
                    ..
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
                        kad::QueryResult::GetProviders(Ok(kad::GetProvidersOk::FoundProviders {
                            key,
                            providers,
                        })) => {
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
        self.apply_remote_action(&action);
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
            SyncMsg::Hello(h) => {
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
                // Token gate: if we require one, the hello must carry it.
                if let Some(need) = &self.token {
                    match h.token.as_deref() {
                        Some(got) if got == need => {}
                        _ => {
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
                        self.store.register_peer_key(&peer.to_base58(), arr);
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
            // Rejections/acks we initiated on the outbound side.
            SyncMsg::HelloAck(_)
            | SyncMsg::CatchUpAck(_)
            | SyncMsg::SummaryAck(_)
            | SyncMsg::HelloErr(_) => {}
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
                            self.store.register_peer_key(&peer.to_base58(), arr);
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
                };
                if let Some(rt) = self.drives.get_mut(&name) {
                    rt.status = status;
                }
                self.set_status(&name, status, Some(detail));
            }
            SyncMsg::CatchUpAck(ack) => {
                let mut applied = 0usize;
                for action in &ack.actions {
                    if self.apply_remote_action(action) {
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
            // We do not initiate these.
            SyncMsg::Hello(_) | SyncMsg::CatchUp(_) | SyncMsg::Summary(_) => {}
        }
    }

    // -- helpers ---------------------------------------------------------------

    /// Applies a remote action through the store (signature-verified).
    fn apply_remote_action(&mut self, action: &Action) -> bool {
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
        let pubkey = self.store.key().verifying_key().to_bytes();
        let hello = Hello {
            version: PROTOCOL_VERSION,
            name: self.pub_name.clone(),
            peer_id: self.peer_id.to_base58(),
            pubkey: hex::encode(pubkey),
            token: self.token.clone(),
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
