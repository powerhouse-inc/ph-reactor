//! End-to-end sync: two full reactors (store + p2p engine) on loopback.
//! A doc created on one side must appear on the other — in both
//! directions — through the hello handshake, the gossipsub mesh, and
//! the summary/catch-up reconciliation.

fn init_log() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")),
        )
        .try_init();
}

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use libp2p::identity::Keypair;
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};
use tokio::sync::mpsc;

use ph_reactor::drives::{Drive, DriveStatus};
use ph_reactor::p2p::{self, EngineCommand, EngineEvent, SyncEngine};
use ph_reactor::store::Store;

struct Node {
    store: Arc<Store>,
    cmd_tx: mpsc::UnboundedSender<EngineCommand>,
    evt_rx: mpsc::UnboundedReceiver<EngineEvent>,
    listen: Multiaddr,
    peer: PeerId,
    /// Keeps the store's directory alive for the test's lifetime.
    _dir: tempfile::TempDir,
}

async fn spawn_node(tag: &str) -> Node {
    let dir = tempfile::tempdir().expect("temp dir");
    let key = Keypair::generate_ed25519();
    let peer = key.public().to_peer_id();
    let signing = p2p::signing_key(&key).expect("signing key");
    let store = Store::open(&dir.path().join("docs"), &signing, &peer.to_base58()).expect("store");
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (evt_tx, mut evt_rx) = mpsc::unbounded_channel();
    let listen = Multiaddr::from_str("/ip4/127.0.0.1/tcp/0").expect("listen addr");
    let engine = SyncEngine::new(
        &key,
        store.clone(),
        tag,
        listen,
        false,    // no mDNS in the test
        false,    // no DHT in the test
        None,     // no shared token
        cmd_rx,
        evt_tx,
    )
    .expect("engine");
    tokio::spawn(async move {
        engine.run().await;
    });

    // The identity event carries the resolved listen address (port 0
    // is replaced by the OS-assigned one).
    let listen = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(EngineEvent::Identity { listen, .. }) = evt_rx.recv().await {
                return listen;
            }
        }
    })
    .await
    .expect("identity event");

    Node {
        store,
        cmd_tx,
        evt_rx,
        listen,
        peer,
        _dir: dir,
    }
}

fn with_peer(addr: Multiaddr, peer: PeerId) -> Multiaddr {
    addr.with(Protocol::P2p(peer))
}

fn drive(name: &str, node: &Node) -> Drive {
    Drive {
        name: name.into(),
        addr: with_peer(node.listen.clone(), node.peer),
        token_env: None,
        paused: false,
        available_offline: false,
    }
}

async fn wait_status(rx: &mut mpsc::UnboundedReceiver<EngineEvent>, name: &str, want: DriveStatus) {
    loop {
        match rx.recv().await {
            Some(EngineEvent::DriveStatus {
                name: n, status, ..
            }) if n == name && status == want => {
                return;
            }
            Some(_) => {}
            None => panic!("engine went away before {name} reached {want:?}"),
        }
    }
}

async fn wait_field(store: &Store, doc: &str, field: &str, want: &serde_json::Value) {
    loop {
        if let Some(d) = store.get(doc) {
            if let Some(f) = d.fields.get(field) {
                if f.value == *want && !f.deleted {
                    return;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn two_engines_sync_docs_both_directions() {
    init_log();
    let a = spawn_node("alpha").await;
    let b = spawn_node("beta").await;

    // Cross-link the drives; both sides initiate (each dials the other).
    a.cmd_tx
        .send(EngineCommand::AddDrive(drive("beta", &b)))
        .unwrap();
    b.cmd_tx
        .send(EngineCommand::AddDrive(drive("alpha", &a)))
        .unwrap();

    let mut a_evt = a.evt_rx;
    let mut b_evt = b.evt_rx;
    tokio::time::timeout(Duration::from_secs(30), async {
        wait_status(&mut a_evt, "beta", DriveStatus::Synced).await;
        wait_status(&mut b_evt, "alpha", DriveStatus::Synced).await;
    })
    .await
    .expect("handshake did not complete");

    // A creates a doc; the mesh must carry it to B.
    let mut fields = BTreeMap::new();
    fields.insert("body".into(), serde_json::json!("hello from alpha"));
    a.store.create_doc("note", fields).unwrap();
    tokio::time::timeout(
        Duration::from_secs(30),
        wait_field(
            &b.store,
            "note",
            "body",
            &serde_json::json!("hello from alpha"),
        ),
    )
    .await
    .expect("doc did not reach beta");

    // B updates the same doc; it must reach A (bidirectional).
    b.store
        .update_field("note", "body", serde_json::json!("edited by beta"))
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(30),
        wait_field(
            &a.store,
            "note",
            "body",
            &serde_json::json!("edited by beta"),
        ),
    )
    .await
    .expect("update did not reach alpha");

    // Clean shutdown of both engines.
    a.cmd_tx.send(EngineCommand::Shutdown).unwrap();
    b.cmd_tx.send(EngineCommand::Shutdown).unwrap();
}

#[tokio::test]
async fn drive_added_after_peer_already_connected_completes_handshake() {
    // The race: A's dial reaches B before B knows about A (no drive yet
    // on B). A completes its own handshake from B's hello-ack; B's
    // half of the handshake must still complete when B adds the drive
    // later (from the guest record / the existing connection), and full
    // sync must follow.
    init_log();
    let a = spawn_node("alpha").await;
    let b = spawn_node("beta").await;
    let a_peer = a.peer;
    let a_listen = a.listen.clone();
    // (registering A as a guest) without a drive of its own.
    a.cmd_tx
        .send(EngineCommand::AddDrive(drive("beta", &b)))
        .unwrap();

    let mut a_evt = a.evt_rx;
    let mut b_evt = b.evt_rx;
    tokio::time::timeout(
        Duration::from_secs(30),
        wait_status(&mut a_evt, "beta", DriveStatus::Synced),
    )
    .await
    .expect("A's one-sided handshake did not complete");

    // Now B learns about A, with A already connected.
    b.cmd_tx
        .send(EngineCommand::AddDrive(Drive {
            name: "alpha".into(),
            addr: with_peer(a_listen, a_peer),
            token_env: None,
            paused: false,
            available_offline: false,
        }))
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(30),
        wait_status(&mut b_evt, "alpha", DriveStatus::Synced),
    )
    .await
    .expect("B's drive never handshaken after the late add");

    // Docs flow both ways over the completed handshake.
    let mut fields = BTreeMap::new();
    fields.insert("body".into(), serde_json::json!("late drive, early doc"));
    a.store.create_doc("late-note", fields).unwrap();
    tokio::time::timeout(
        Duration::from_secs(30),
        wait_field(
            &b.store,
            "late-note",
            "body",
            &serde_json::json!("late drive, early doc"),
        ),
    )
    .await
    .expect("doc did not reach beta");

    b.store
        .update_field("late-note", "body", serde_json::json!("beta saw it"))
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(30),
        wait_field(
            &a.store,
            "late-note",
            "body",
            &serde_json::json!("beta saw it"),
        ),
    )
    .await
    .expect("update did not reach alpha");

    a.cmd_tx.send(EngineCommand::Shutdown).unwrap();
    b.cmd_tx.send(EngineCommand::Shutdown).unwrap();
}
