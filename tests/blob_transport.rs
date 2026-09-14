//! A blob too large for one sync message travels between two real reactors.
//!
//! This is the transport plugin bundles need: a React editor carrying the
//! Powerhouse design system is several megabytes, while `MAX_MSG_BYTES` caps a
//! sync message at 1 MiB — and that cap is load-bearing, so the bundle is
//! chunked instead of the cap being raised.
//!
//! The assertion is deliberately end-to-end. Unit tests already cover chunking
//! and hash verification in isolation; what they cannot show is that a peer
//! actually serves chunks it holds and that the requester reassembles them into
//! the original bytes.
//!
//! See docs/superpowers/specs/2026-09-14-plugin-packages-design.md.

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use libp2p::identity::Keypair;
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};
use tokio::sync::mpsc;

use ph_reactor::blob::{BlobRef, BlobStore, CHUNK_BYTES};
use ph_reactor::drives::{Drive, DriveStatus};
use ph_reactor::p2p::{self, EngineCommand, EngineEvent, SyncEngine};
use ph_reactor::store::Store;

struct Node {
    _store: Arc<Store>,
    blobs: Arc<BlobStore>,
    cmd_tx: mpsc::UnboundedSender<EngineCommand>,
    evt_rx: mpsc::UnboundedReceiver<EngineEvent>,
    listen: Multiaddr,
    peer: PeerId,
    _dir: tempfile::TempDir,
}

async fn spawn_node(tag: &str) -> Node {
    let dir = tempfile::tempdir().expect("temp dir");
    let key = Keypair::generate_ed25519();
    let peer = key.public().to_peer_id();
    let signing = p2p::signing_key(&key).expect("signing key");
    let store = Store::open(&dir.path().join("docs"), &signing, &peer.to_base58()).expect("store");
    let blobs = Arc::new(BlobStore::open(&dir.path().join("blobs")).expect("blob store"));
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (evt_tx, mut evt_rx) = mpsc::unbounded_channel();
    let listen = Multiaddr::from_str("/ip4/127.0.0.1/tcp/0").expect("listen addr");

    let engine = SyncEngine::new(
        &key,
        store.clone(),
        tag,
        listen,
        None,
        Vec::new(),
        blobs.clone(),
        false,
        false,
        false,
        None,
        HashSet::new(),
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
        _store: store,
        blobs,
        cmd_tx,
        evt_rx,
        listen,
        peer,
        _dir: dir,
    }
}

async fn wait_drive(rx: &mut mpsc::UnboundedReceiver<EngineEvent>, name: &str, want: DriveStatus) {
    loop {
        match rx.recv().await {
            Some(EngineEvent::DriveStatus {
                name: n, status, ..
            }) if n == name && status == want => return,
            Some(_) => continue,
            None => panic!("event channel closed before {name} reached {want:?}"),
        }
    }
}

/// A bundle-sized payload: genuinely larger than one sync message, with a
/// partial chunk at the end so the remainder path is exercised too.
///
/// Five chunks rather than three on purpose — 3 x 256 KiB is only ~787 KB and
/// would fit in a single message, so it would not test the thing this file
/// exists to test.
fn bundle() -> Vec<u8> {
    let n = CHUNK_BYTES * 5 + 1234;
    (0..n).map(|i| (i % 251) as u8).collect()
}

#[tokio::test]
async fn a_multi_chunk_blob_transfers_between_peers() {
    let server = spawn_node("blob-server").await;
    let mut client = spawn_node("blob-client").await;

    // The server holds the bundle; the client has never seen it.
    let data = bundle();
    let blob: BlobRef = server.blobs.put(&data).expect("server stores the bundle");
    assert!(blob.chunks.len() >= 6, "must span several chunks");
    assert!(
        data.len() > ph_reactor::p2p::codec::MAX_MSG_BYTES as usize,
        "the point of this test is a payload larger than one message"
    );
    assert!(!client.blobs.is_complete(&blob), "client starts empty");
    assert_eq!(client.blobs.missing(&blob).len(), blob.chunks.len());

    // Connect them.
    let addr = server.listen.clone().with(Protocol::P2p(server.peer));
    client
        .cmd_tx
        .send(EngineCommand::AddDrive(Drive {
            name: "peer".into(),
            addr,
            token_env: None,
            paused: false,
            available_offline: false,
        }))
        .expect("add drive");
    tokio::time::timeout(
        Duration::from_secs(30),
        wait_drive(&mut client.evt_rx, "peer", DriveStatus::Synced),
    )
    .await
    .expect("drive should sync");

    // Ask for the blob and wait for it to complete.
    client
        .cmd_tx
        .send(EngineCommand::FetchBlob { blob: blob.clone() })
        .expect("fetch");

    let got = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if client.blobs.is_complete(&blob) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    assert!(
        got.is_ok(),
        "blob never completed; {} chunk(s) still missing",
        client.blobs.missing(&blob).len()
    );

    // Reassembly verifies against the blob hash, so this is byte-exact.
    assert_eq!(
        client.blobs.get(&blob).expect("reassemble"),
        data,
        "the client must reconstruct the original bundle exactly"
    );

    drop(server.cmd_tx);
}

/// Fetching something already held is a no-op, not an error — an install that
/// re-runs must not refetch megabytes.
#[tokio::test]
async fn fetching_a_blob_already_held_does_nothing() {
    let node = spawn_node("solo").await;
    let data = bundle();
    let blob = node.blobs.put(&data).expect("store");
    assert!(node.blobs.is_complete(&blob));

    node.cmd_tx
        .send(EngineCommand::FetchBlob { blob: blob.clone() })
        .expect("fetch");
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(node.blobs.is_complete(&blob));
    assert_eq!(node.blobs.get(&blob).expect("still intact"), data);
    drop(node.cmd_tx);
}
