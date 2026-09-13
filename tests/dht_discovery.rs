//! DHT discovery: two reactors bootstrap each other, one publishes a
//! provider record, the other discovers that provider through the
//! Kademlia DHT (peer routing + provider replication over loopback).

fn init_log() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")),
        )
        .try_init();
}

use std::str::FromStr;
use std::time::Duration;

use libp2p::identity::Keypair;
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};
use tokio::sync::mpsc;

use ph_reactor::doc::DocId;
use ph_reactor::p2p::{self, EngineCommand, EngineEvent, SyncEngine};
use ph_reactor::store::Store;

struct Node {
    cmd_tx: mpsc::UnboundedSender<EngineCommand>,
    evt_rx: mpsc::UnboundedReceiver<EngineEvent>,
    listen: Multiaddr,
    peer: PeerId,
    _dir: tempfile::TempDir,
}

fn with_peer(addr: &Multiaddr, peer: PeerId) -> Multiaddr {
    addr.clone().with(Protocol::P2p(peer))
}

async fn spawn_dht_node(tag: &str) -> Node {
    let dir = tempfile::tempdir().expect("temp dir");
    let key = Keypair::generate_ed25519();
    let peer = key.public().to_peer_id();
    let signing = p2p::signing_key(&key).expect("signing key");
    let store =
        Store::open(&dir.path().join("docs"), &signing, &peer.to_base58()).expect("store");
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (evt_tx, mut evt_rx) = mpsc::unbounded_channel();
    let listen = Multiaddr::from_str("/ip4/127.0.0.1/tcp/0").expect("listen addr");
    let engine = SyncEngine::new(
        &key,
        store,
        tag,
        listen,
        false, // no mDNS
        true,   // DHT enabled
        false,  // no relay
        None,  // no shared token
        cmd_rx,
        evt_tx,
    )
    .expect("engine");
    tokio::spawn(async move {
        engine.run().await;
    });

    // The identity event carries the resolved listen address.
    let listen = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(EngineEvent::Identity { listen, .. }) = evt_rx.recv().await {
                return listen;
            }
        }
    })
    .await
    .expect("identity event");

    Node { cmd_tx, evt_rx, listen, peer, _dir: dir }
}

/// Drains `rx` until an event satisfying `pred` arrives (30s budget).
async fn wait_for(
    rx: &mut mpsc::UnboundedReceiver<EngineEvent>,
    pred: impl Fn(&EngineEvent) -> bool,
) -> EngineEvent {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ev)) if pred(&ev) => return ev,
            Ok(Some(_)) => {}
            Ok(None) => panic!("engine went away before the expected event"),
            Err(_) => panic!("timed out waiting for the expected DHT event"),
        }
    }
}

#[tokio::test]
async fn dht_provider_discovery() {
    init_log();

    let mut a = spawn_dht_node("a").await;
    let mut b = spawn_dht_node("b").await;

    // Mutual bootstrap: each node seeds the other's address + peer id.
    a.cmd_tx
        .send(EngineCommand::DhtBootstrap {
            peers: vec![(b.peer, with_peer(&b.listen, b.peer))],
        })
        .unwrap();
    b.cmd_tx
        .send(EngineCommand::DhtBootstrap {
            peers: vec![(a.peer, with_peer(&a.listen, a.peer))],
        })
        .unwrap();

    // Both nodes complete a bootstrap (their routing table learns the peer).
    wait_for(&mut a.evt_rx, |ev| matches!(ev, EngineEvent::DhtBootstrap(true))).await;
    wait_for(&mut b.evt_rx, |ev| matches!(ev, EngineEvent::DhtBootstrap(true))).await;

    // A publishes a provider record: "I provide this doc".
    let doc = DocId::new();
    let key = doc.to_string().into_bytes();
    a.cmd_tx
        .send(EngineCommand::PublishProvider { name: "a".into(), doc })
        .unwrap();

    // B asks the DHT who provides that key.
    b.cmd_tx.send(EngineCommand::FindProviders { key: key.clone() }).unwrap();

    // B learns A is a provider of the key (via the replicated record).
    wait_for(&mut b.evt_rx, |ev| {
        matches!(&ev, EngineEvent::DhtProvider { key: k, peer } if *k == key && *peer == a.peer)
    })
    .await;
}
