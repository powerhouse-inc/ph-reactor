//! Performance probes for the sync hot path (the `multiproc` ticket).
//!
//! Two measurements:
//!
//! 1. **Store write throughput** — sustained local `update_field`
//!    actions through the full store path (sign -> verify -> reduce ->
//!    per-field merge -> WAL append -> change feed), with the same
//!    change-feed subscribers a live daemon has (a processor-like
//!    consumer). Reports ops/s.
//!
//! 2. **Two-node convergence latency** — a doc created on one engine
//!    reaching the other's store through the real mesh (hello
//!    handshake + gossipsub). The elapsed time is dominated by the
//!    engine's outbound publish cadence; reporting it makes any
//!    cadence change visible before/after.
//!
//! These print their numbers; they assert only generous ceilings so a
//! regression to absurdity fails the suite.

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

// ---------------------------------------------------------------------------
// 1. Store write throughput
// ---------------------------------------------------------------------------

#[tokio::test]
async fn store_write_throughput() {
    let dir = tempfile::tempdir().expect("temp dir");
    let key = Keypair::generate_ed25519();
    let signing = p2p::signing_key(&key).expect("signing key");
    let origin = key.public().to_peer_id().to_base58();
    let store = Store::open(&dir.path().join("docs"), &signing, &origin).expect("store");

    let fields = BTreeMap::from([
        ("a".into(), serde_json::json!(1)),
        ("b".into(), serde_json::json!("x")),
    ]);
    store.create_doc("bench", fields).expect("create");

    // A processor-like consumer of the change feed: a live daemon has one
    // (plus a read-model consumer when a query is active). Consuming it
    // keeps the feed's channel drained, as in production.
    let mut feed = store.subscribe_changes();
    let feeder = tokio::spawn(async move { while feed.recv().await.is_some() {} });
    // One field update per action — the common write.
    let n = 500u64;
    let start = std::time::Instant::now();
    for i in 1..=n {
        store
            .update_field("bench", "a", serde_json::json!(i))
            .expect("update");
    }
    let elapsed = start.elapsed();
    let ops = n as f64 / elapsed.as_secs_f64();
    eprintln!(
        "bench: store write throughput = {ops:.1} ops/s ({:?} for {} actions, 1 change-feed subscriber)",
        elapsed, n
    );
    feeder.abort();
    // Profile-dependent (ed25519 is ~100x slower unoptimized): the floor is a
    // collapse detector, not a target. It has to be per-profile, because one
    // number cannot be both. A single floor of 100 failed on healthy hardware
    // in debug -- measured 71 ops/s on a machine doing 19.4k in release -- and
    // a detector that fires on a healthy store is worse than no detector,
    // because the next person silently raises the threshold instead of reading
    // it. Debug catches an order-of-magnitude collapse; release catches a real
    // regression.
    let floor = if cfg!(debug_assertions) { 20.0 } else { 5_000.0 };
    assert!(
        ops > floor,
        "store write throughput collapsed: {ops:.1} ops/s (floor {floor:.0})"
    );
}

// ---------------------------------------------------------------------------
// 2. Two-node convergence latency
// ---------------------------------------------------------------------------

struct Node {
    store: Arc<Store>,
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
        false,
        false,
        false,
        None,
        std::collections::HashSet::new(),
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

async fn wait_synced(rx: &mut mpsc::UnboundedReceiver<EngineEvent>, name: &str) -> Result<(), ()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(250), rx.recv()).await {
            Ok(Some(EngineEvent::DriveStatus {
                name: n, status, ..
            })) if n == name && status == DriveStatus::Synced => return Ok(()),
            Ok(Some(_)) => {}
            Ok(None) => return Err(()),
            Err(_) => continue,
        }
    }
    Err(())
}

/// Measures how long a doc created on A takes to appear on B, `rounds`
/// times (a fresh doc each round so gossip dedup does not mask the
/// measurement).
#[tokio::test]
async fn convergence_latency_two_nodes() {
    let mut a = spawn_node("alpha").await;
    let mut b = spawn_node("beta").await;

    let drive_b = Drive {
        name: "beta".into(),
        addr: with_peer(b.listen.clone(), b.peer),
        token_env: None,
        paused: false,
        available_offline: false,
    };
    let drive_a = Drive {
        name: "alpha".into(),
        addr: with_peer(a.listen.clone(), a.peer),
        token_env: None,
        paused: false,
        available_offline: false,
    };
    a.cmd_tx.send(EngineCommand::AddDrive(drive_b)).unwrap();
    b.cmd_tx.send(EngineCommand::AddDrive(drive_a)).unwrap();

    wait_synced(&mut a.evt_rx, "beta")
        .await
        .expect("a<->b sync");
    wait_synced(&mut b.evt_rx, "alpha")
        .await
        .expect("b<->a sync");

    let rounds = 3;
    let mut total = Duration::ZERO;
    for i in 0..rounds {
        let name = format!("conv-{i}");
        let mut fields = BTreeMap::new();
        fields.insert("body".into(), serde_json::json!(i));
        let t0 = std::time::Instant::now();
        a.store.create_doc(&name, fields.clone()).expect("create");
        // Wait until B's store has the doc with the right field.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(d) = b.store.get(&name) {
                if d.fields.get("body").map(|f| &f.value) == Some(&serde_json::json!(i)) {
                    break;
                }
            }
            if tokio::time::Instant::now() > deadline {
                panic!("doc {name} did not reach beta within 30s");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let dt = t0.elapsed();
        total += dt;
        eprintln!("bench: convergence round {i}: {dt:?} (create -> peer's store)");
    }
    let avg = total / rounds;
    eprintln!("bench: avg convergence latency (2 nodes): {avg:?}");
    assert!(
        avg < Duration::from_secs(10),
        "average convergence latency {avg:?} is too high"
    );

    a.cmd_tx.send(EngineCommand::Shutdown).unwrap();
    b.cmd_tx.send(EngineCommand::Shutdown).unwrap();
}
