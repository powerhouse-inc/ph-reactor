//! End-to-end: ten full reactors (store + p2p engine + read models) on
//! loopback, forming a mesh. Realistic documents (project management:
//! projects + tasks; finance: accounts + transactions) are created on a few
//! nodes and must converge to all ten, with every node's read models
//! (DocumentView + RelationshipIndex) answering queries correctly. This is
//! the "10 clients with realistic use cases" acceptance test for the
//! native reactor + the CQRS views crate.

fn init_log() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();
}

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use libp2p::identity::Keypair;
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use ph_reactor::doc::ModelRef;
use ph_reactor::drives::Drive;
use ph_reactor::model::realistic::{realistic_models, realistic_relationships};
use ph_reactor::p2p::{self, EngineCommand, EngineEvent, SyncEngine};
use ph_reactor::store::Store;

use ph_reactor_views::{
    DocFilter, DocumentView, QueryService, ReadModel, ReadModelCoordinator, RelationshipIndex,
};

const N: usize = 10;

struct Node {
    store: Arc<Store>,
    cmd_tx: mpsc::UnboundedSender<EngineCommand>,
    listen: Multiaddr,
    peer: PeerId,
    query: QueryService,
    _dir: tempfile::TempDir,
}

async fn spawn_node(tag: &str) -> Node {
    let dir = tempfile::tempdir().expect("temp dir");
    let key = Keypair::generate_ed25519();
    let peer = key.public().to_peer_id();
    let signing = p2p::signing_key(&key).expect("signing key");
    let store =
        Store::open(&dir.path().join("docs"), &signing, &peer.to_base58()).expect("store");
    // Load the realistic models so this peer can reduce their actions.
    for m in realistic_models() {
        store.add_model(Arc::new(m));
    }

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (evt_tx, mut evt_rx) = mpsc::unbounded_channel();
    let listen = Multiaddr::from_str("/ip4/127.0.0.1/tcp/0").expect("listen addr");
    let engine = SyncEngine::new(
        &key,
        store.clone(),
        tag,
        listen,
        false, // no mDNS
        false, // no DHT
        false, // no relay
        None,  // no shared token
        HashSet::new(),
        cmd_rx,
        evt_tx,
    )
    .expect("engine");
    tokio::spawn(engine.run());

    // The resolved listen address (port 0 -> OS-assigned).
    let listen = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(EngineEvent::Identity { listen, .. }) = evt_rx.recv().await {
                return listen;
            }
        }
    })
    .await
    .expect("identity event");

    // Read models driven off this store's change feed.
    let view = Arc::new(DocumentView::new(None));
    let index = Arc::new(RelationshipIndex::new(
        realistic_relationships(),
        None,
    ));
    let models: Vec<Arc<dyn ReadModel>> = vec![
        Arc::clone(&view) as Arc<dyn ReadModel>,
        Arc::clone(&index) as Arc<dyn ReadModel>,
    ];
    let coordinator = Arc::new(ReadModelCoordinator::new(store.clone(), models));
    tokio::spawn(coordinator.run());
    let query = QueryService::new(view, Some(index));

    Node {
        store,
        cmd_tx,
        listen,
        peer,
        query,
        _dir: dir,
    }
}

fn drive(name: &str, node: &Node) -> Drive {
    Drive {
        name: name.into(),
        addr: node.listen.clone().with(Protocol::P2p(node.peer)),
        token_env: None,
        paused: false,
        available_offline: false,
    }
}

/// Wait until every node's store lists all of `names` (or the timeout).
async fn wait_docs_all(nodes: &[Node], names: &[&str], timeout: Duration) -> bool {
    let start = tokio::time::Instant::now();
    loop {
        let mut ok = true;
        for n in nodes {
            let have: HashSet<String> =
                n.store.list().into_iter().map(|d| d.name).collect();
            for name in names {
                if !have.contains(*name) {
                    ok = false;
                    break;
                }
            }
            if !ok {
                break;
            }
        }
        if ok {
            return true;
        }
        if start.elapsed() > timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Wait until every node's read models have the expected per-model counts
/// (the coordinator lags the store; this bridges the gap before asserting).
async fn wait_views_all(nodes: &[Node], timeout: Duration) -> bool {
    let start = tokio::time::Instant::now();
    loop {
        let mut ok = true;
        for n in nodes {
            let counts = |m: &str| n.query.query(m, None).as_array().map(|a| a.len());
            if counts("task") != Some(3)
                || counts("project") != Some(2)
                || counts("account") != Some(2)
                || counts("transaction") != Some(2)
            {
                ok = false;
                break;
            }
        }
        if ok {
            return true;
        }
        if start.elapsed() > timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn create_reactor_docs(n: &Node) {
    // Node 0 owns the project-management docs.
    n.store
        .create_doc_model(
            "atlas",
            &ModelRef::new("project", "1"),
            &json!({"name": "atlas", "status": "active", "owner": "froid", "due_date": "2026-10-01", "description": "the native reactor"}),
        )
        .unwrap();
    n.store
        .create_doc_model(
            "borealis",
            &ModelRef::new("project", "1"),
            &json!({"name": "borealis", "status": "planning", "owner": "froid", "due_date": "2026-12-01", "description": "the vault"}),
        )
        .unwrap();
    for (name, status, project) in [
        ("atlas-1", "todo", "atlas"),
        ("atlas-2", "done", "atlas"),
        ("borealis-1", "todo", "borealis"),
    ] {
        n.store
            .create_doc_model(
                name,
                &ModelRef::new("task", "1"),
                &json!({"name": name, "title": name, "status": status, "assignee": "froid", "project": project, "priority": 2}),
            )
            .unwrap();
    }
}

fn create_finance_docs(n: &Node) {
    // Node 3 owns the finance docs.
    n.store
        .create_doc_model(
            "operating",
            &ModelRef::new("account", "1"),
            &json!({"name": "operating", "currency": "USD", "balance": 5000.0}),
        )
        .unwrap();
    n.store
        .create_doc_model(
            "savings",
            &ModelRef::new("account", "1"),
            &json!({"name": "savings", "currency": "USD", "balance": 20000.0}),
        )
        .unwrap();
    n.store
        .create_doc_model(
            "op-1",
            &ModelRef::new("transaction", "1"),
            &json!({"name": "op-1", "amount": 42.5, "category": "groceries", "account": "operating", "date": "2026-01-15"}),
        )
        .unwrap();
    n.store
        .create_doc_model(
            "sav-1",
            &ModelRef::new("transaction", "1"),
            &json!({"name": "sav-1", "amount": 1000.0, "category": "transfer", "account": "savings", "date": "2026-01-20"}),
        )
        .unwrap();
}

#[tokio::test]
async fn ten_clients_converge_and_read_models_answer() {
    init_log();

    let mut nodes: Vec<Node> = Vec::with_capacity(N);
    for i in 0..N {
        nodes.push(spawn_node(&format!("peer-{i:02}")).await);
    }

    // Full mesh: every node adds a drive for every other node.
    for i in 0..N {
        for j in 0..N {
            if i == j {
                continue;
            }
            nodes[i]
                .cmd_tx
                .send(EngineCommand::AddDrive(drive(&format!("peer-{j:02}"), &nodes[j])))
                .unwrap();
        }
    }

    // Give the mesh a moment to form, then create the realistic docs.
    tokio::time::sleep(Duration::from_secs(3)).await;
    create_reactor_docs(&nodes[0]);
    create_finance_docs(&nodes[3]);

    let all_names = [
        "atlas",
        "borealis",
        "atlas-1",
        "atlas-2",
        "borealis-1",
        "operating",
        "savings",
        "op-1",
        "sav-1",
    ];

    assert!(
        wait_docs_all(&nodes, &all_names, Duration::from_secs(90)).await,
        "all ten clients must converge on the same document set"
    );

    // The read models are maintained asynchronously off the change feed;
    // wait for them to catch up to the converged store on every node.
    assert!(
        wait_views_all(&nodes, Duration::from_secs(90)).await,
        "every node's read models must converge to the full document set"
    );

    // Each node's read model must answer the project-management queries.
    for (i, n) in nodes.iter().enumerate() {
        let todos = n.query.query("task", Some(DocFilter::new("status", json!("todo"))));
        assert_eq!(
            todos.as_array().map(|a| a.len()),
            Some(2),
            "node {i}: expected 2 todo tasks, got {todos:?}"
        );
        // The relationship graph: tasks belonging to project "atlas".
        let atlas_tasks = n.query.incoming("atlas", "project");
        assert_eq!(
            atlas_tasks.as_array().map(|a| a.len()),
            Some(2),
            "node {i}: atlas should have 2 tasks, got {atlas_tasks:?}"
        );
        // The finance side: two accounts, two transactions.
        assert_eq!(
            n.query.query("account", None).as_array().map(|a| a.len()),
            Some(2),
            "node {i}: expected 2 accounts"
        );
    }

    // The read models must answer queries correctly on every client (tasks
    // across both creators: atlas-1, atlas-2, borealis-1).
    for (i, n) in nodes.iter().enumerate() {
        assert_eq!(
            n.query.query("task", None).as_array().map(|a| a.len()),
            Some(3),
            "node {i}: expected 3 tasks in the read model"
        );
    }

    // Known p2p-engine limitation (documented, not asserted): *ongoing*
    // changes to an already-converged doc set propagate reliably across a
    // two-peer drive (see the engine two-peer test) but are lossy in a
    // ten-peer full mesh, where a peer that misses the gossip can wait a
    // full 30s reconciliation tick. The reliable path is the catch-up that
    // runs when a peer connects (the handshake), which is what established
    // the convergence above. Tightening live convergence for large meshes
    // (faster reconciliation / more reliable gossip) is the follow-up.

    for n in nodes.iter() {
        n.cmd_tx.send(EngineCommand::Shutdown).unwrap();
    }
}
