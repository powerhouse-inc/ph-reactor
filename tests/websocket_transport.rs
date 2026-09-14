//! The WebSocket transport carries a real sync session.
//!
//! This is the transport that lets a peer reach a reactor through a reverse
//! proxy on :443 instead of an arbitrary high TCP port. The listener speaks
//! plain `ws` because TLS is terminated at the proxy; a peer on the public
//! internet dials `/dns4/<host>/tcp/443/tls/ws/...` and the two ends agree,
//! because TLS is a transport concern the proxy handles.
//!
//! The assertion is deliberately end-to-end rather than "a listener was
//! created": a drive dialled over `/ws` must reach `Synced`, which only
//! happens after the Noise handshake, the hello exchange and reconciliation
//! all succeed over that transport.

use std::collections::HashSet;
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

/// Reserves a free localhost port by binding and immediately releasing it.
///
/// The websocket listener needs a port known *before* the engine starts,
/// because unlike the TCP listener its resolved address is not reported back
/// through the `Identity` event.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = l.local_addr().expect("local addr").port();
    drop(l);
    port
}

struct Node {
    store: Arc<Store>,
    cmd_tx: mpsc::UnboundedSender<EngineCommand>,
    evt_rx: mpsc::UnboundedReceiver<EngineEvent>,
    peer: PeerId,
    _dir: tempfile::TempDir,
}

async fn spawn_node(tag: &str, listen_ws: Option<Multiaddr>, external: Vec<Multiaddr>) -> Node {
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
        listen_ws,
        external,
        std::sync::Arc::new(
            ph_reactor::blob::BlobStore::open(&dir.path().join("blobs")).expect("blob store"),
        ),
        false,          // no mDNS in the test
        false,          // no DHT in the test
        false,          // no relay in the test
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

    // Wait until the engine is actually up before the caller dials it.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(EngineEvent::Identity { .. }) = evt_rx.recv().await {
                return;
            }
        }
    })
    .await
    .expect("identity event");

    Node {
        store,
        cmd_tx,
        evt_rx,
        peer,
        _dir: dir,
    }
}

async fn wait_status(rx: &mut mpsc::UnboundedReceiver<EngineEvent>, name: &str, want: DriveStatus) {
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

/// A drive dialled over `/ws` completes the handshake and reaches `Synced`.
#[tokio::test]
async fn drive_syncs_over_websocket_transport() {
    let ws_port = free_port();
    let ws_listen =
        Multiaddr::from_str(&format!("/ip4/127.0.0.1/tcp/{ws_port}/ws")).expect("ws listen addr");

    // The listener advertises a websocket address it is not bound to, which is
    // the production shape: the address peers must dial belongs to the proxy,
    // not to this process.
    let announced =
        Multiaddr::from_str("/dns4/reactor.example/tcp/443/tls/ws").expect("announced addr");

    let server = spawn_node("ws-server", Some(ws_listen.clone()), vec![announced]).await;
    let mut client = spawn_node("ws-client", None, Vec::new()).await;

    // Give the websocket listener a moment to bind.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let addr = ws_listen.clone().with(Protocol::P2p(server.peer));
    client
        .cmd_tx
        .send(EngineCommand::AddDrive(Drive {
            name: "over-ws".into(),
            addr,
            token_env: None,
            paused: false,
            available_offline: false,
        }))
        .expect("send AddDrive");

    tokio::time::timeout(
        Duration::from_secs(30),
        wait_status(&mut client.evt_rx, "over-ws", DriveStatus::Synced),
    )
    .await
    .expect("drive should reach Synced over the websocket transport");

    // Keep both stores alive to the end of the test.
    drop(server.store);
    drop(client.store);
    drop(server.cmd_tx);
}
