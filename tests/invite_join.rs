//! Invite/join: `alpha` issues a signed invite; `beta` consumes it. Beta
//! pins alpha (TOFU), adds a drive for alpha, and sends a signed join-proof
//! in its hello. Alpha verifies the proof and adds a drive back for beta.
//! The two then sync: a doc beta creates reaches alpha.

fn init_log() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")),
        )
        .try_init();
}

use std::collections::{BTreeMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use libp2p::identity::Keypair;
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};
use tokio::sync::mpsc;

use ph_reactor::drives::Drive;
use ph_reactor::drives::DriveStatus;
use ph_reactor::p2p::{self, invite, EngineCommand, EngineEvent, SyncEngine};
use ph_reactor::store::Store;

struct Node {
    cmd_tx: mpsc::UnboundedSender<EngineCommand>,
    evt_rx: mpsc::UnboundedReceiver<EngineEvent>,
    listen: Multiaddr,
    peer: PeerId,
    signing: SigningKey,
    store: Arc<Store>,
    _dir: tempfile::TempDir,
}

fn with_peer(addr: &Multiaddr, peer: PeerId) -> Multiaddr {
    addr.clone().with(Protocol::P2p(peer))
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
        None,       // no websocket listener in the test
        Vec::new(), // no announced addresses in the test
        std::sync::Arc::new(
            ph_reactor::blob::BlobStore::open(&dir.path().join("blobs")).expect("blob store"),
        ),
        false,          // no mDNS
        false,          // no DHT
        false,          // no relay
        None,           // no shared token
        HashSet::new(), // no banned peers
        cmd_rx,
        evt_tx,
    )
    .await
    .expect("engine");
    tokio::spawn(async move {
        engine.run().await;
    });
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
        cmd_tx,
        evt_rx,
        listen,
        peer,
        signing,
        store,
        _dir: dir,
    }
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
            Err(_) => panic!("timed out waiting for the expected event"),
        }
    }
}

#[tokio::test]
async fn invite_join_then_sync() {
    init_log();
    let mut a = spawn_node("alpha").await; // the inviter
    let mut b = spawn_node("beta").await; // the joiner

    // Alpha issues an invite for beta to join.
    let token = invite::InviteToken::make(
        "alpha",
        &a.peer.to_base58(),
        &a.signing,
        &with_peer(&a.listen, a.peer).to_string(),
        vec!["powerhouse".into()],
    )
    .unwrap();
    // Beta decodes (and verifies) the invite from its shareable form.
    let token = invite::InviteToken::decode(&token.encode().unwrap()).unwrap();
    assert_eq!(token.name, "alpha");

    // Beta's join-proof: echoes the nonce, signed by beta.
    let accept = invite::InviteAccept::make(&token.nonce, "beta", &b.peer.to_base58(), &b.signing);

    // Beta joins: add a drive for alpha (named after alpha) with the proof.
    let drive = Drive {
        name: token.name.clone(),
        addr: with_peer(&a.listen, a.peer),
        token_env: None,
        paused: false,
        available_offline: true,
    };
    b.cmd_tx
        .send(EngineCommand::Join { drive, accept })
        .unwrap();

    // Alpha adds a drive back for beta (named after beta) from the proof,
    // and beta's drive for alpha is established.
    wait_for(
        &mut a.evt_rx,
        |ev| matches!(ev, EngineEvent::DriveStatus { name, .. } if name.starts_with("beta")),
    )
    .await;
    wait_for(
        &mut b.evt_rx,
        |ev| matches!(ev, EngineEvent::DriveStatus { name, .. } if name == "alpha"),
    )
    .await;

    // Both sides reach Synced (handshake + initial catch-up complete).
    wait_for(&mut a.evt_rx, |ev| {
        matches!(
            ev,
            EngineEvent::DriveStatus { name, status, .. }
                if name.starts_with("beta") && *status == DriveStatus::Synced
        )
    })
    .await;
    wait_for(&mut b.evt_rx, |ev| {
        matches!(
            ev,
            EngineEvent::DriveStatus { name, status, .. }
                if name == "alpha" && *status == DriveStatus::Synced
        )
    })
    .await;

    // Beta creates a doc; it propagates to alpha over the synced pair.
    let mut fields = BTreeMap::new();
    fields.insert(
        "title".to_string(),
        serde_json::Value::String("hello".into()),
    );
    b.store
        .create_doc("shared-note", fields)
        .expect("beta creates the doc");

    // Poll alpha's store until the doc arrives (gossip/catch-up).
    let arrived = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if a.store
                .doc_ids()
                .iter()
                .any(|id| a.store.doc_name(*id) == "shared-note")
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("doc propagation timed out");
    assert!(arrived, "alpha should have received beta's doc");
}

/// A vault (beta) that bans a peer (alpha) refuses its handshake: alpha
/// dials beta, beta answers the hello with `HelloErr::Banned`, the drive
/// ends in an error state, and no doc crosses the link.
#[tokio::test]
async fn banned_peer_is_refused() {
    init_log();
    let mut a = spawn_node("alpha").await; // the peer that will be banned
    let b = spawn_node("beta").await; // the vault

    // Beta holds a doc it would normally share.
    let mut fields = BTreeMap::new();
    fields.insert(
        "title".to_string(),
        serde_json::Value::String("secret".into()),
    );
    b.store
        .create_doc("b-secret", fields)
        .expect("beta creates the doc");

    // Beta bans alpha before any link is established.
    b.cmd_tx.send(EngineCommand::Ban { peer: a.peer }).unwrap();

    // Alpha dials beta (adds a drive for it).
    let drive = Drive {
        name: "beta".into(),
        addr: with_peer(&b.listen, b.peer),
        token_env: None,
        paused: false,
        available_offline: true,
    };
    a.cmd_tx.send(EngineCommand::AddDrive(drive)).unwrap();

    // Beta refuses the hello; alpha's drive ends in an error state.
    wait_for(&mut a.evt_rx, |ev| {
        matches!(
            ev,
            EngineEvent::DriveStatus {
                name,
                status,
                ..
            } if name == "beta" && *status == DriveStatus::Error
        )
    })
    .await;

    // Give a moment for any (blocked) propagation, then assert none happened.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let leaked = a
        .store
        .doc_ids()
        .iter()
        .any(|id| a.store.doc_name(*id) == "b-secret");
    assert!(
        !leaked,
        "alpha should not have received the banned peer's doc"
    );
}
