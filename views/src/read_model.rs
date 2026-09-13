//! The read-model contract and the coordinator that keeps read models
//! current against the store's doc-change feed.
//!
//! The store is the source of truth. A read model is a *derived* index
//! maintained on top of it: it never writes to the store, it only projects
//! the store's documents into a queryable shape. [`ReadModelCoordinator`]
//! drives a set of read models — an initial snapshot from the store, then
//! incremental updates off [`Store::subscribe_changes`].

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use ph_reactor::doc::{DocId, ModelRef};
use ph_reactor::store::{DocChange, Store};

/// A read-only projection of one document: everything a read model needs to
/// maintain a derived index, independent of the store's internal layout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocSnapshot {
    pub doc_id: DocId,
    pub name: String,
    /// The model that governs the doc.
    pub model: ModelRef,
    /// The reduced field map (the `__name__` field is lifted into [`name`](Self::name)).
    pub fields: BTreeMap<String, Value>,
    pub deleted: bool,
    /// The timestamp of the change that produced this snapshot (ordering key).
    pub ts: u64,
}

impl DocSnapshot {
    /// Build a snapshot from the store's per-doc full state.
    pub fn from_state(state: &ph_reactor::store::DocState, ts: u64) -> Self {
        DocSnapshot {
            doc_id: state.doc.id,
            name: state.doc.name.clone(),
            model: state.model.clone(),
            fields: state
                .doc
                .fields
                .iter()
                .map(|(k, f)| (k.clone(), f.value.clone()))
                .collect(),
            deleted: state.deleted,
            ts,
        }
    }

    /// Build a snapshot from a doc-change event.
    pub fn from_change(c: &DocChange) -> Self {
        DocSnapshot {
            doc_id: c.doc_id,
            name: c.name.clone(),
            model: c.model.clone(),
            fields: c
                .state
                .doc
                .fields
                .iter()
                .map(|(k, f)| (k.clone(), f.value.clone()))
                .collect(),
            deleted: c.deleted,
            ts: c.ts,
        }
    }
}

/// A derived, queryable index maintained over the store's documents.
///
/// Every method takes `&self` (internal locking) so a read model can be
/// shared via `Arc` between the [`ReadModelCoordinator`] (writer) and the
/// query surface (reader).
pub trait ReadModel: Send + Sync {
    /// The unique name of this read model.
    fn name(&self) -> &str;
    /// Upsert `snap` into the index. A delete is an upsert of a `deleted`
    /// snapshot. Must be idempotent (re-applying the same snapshot is a
    /// no-op).
    fn apply(&self, snap: &DocSnapshot) -> Result<(), String>;
    /// Persist the index to disk.
    fn save(&self) -> Result<(), String>;
    /// Load the index from disk. Returns whether anything was loaded.
    fn load(&self) -> Result<bool, String>;
}

/// Keeps a set of read models current: an initial snapshot from the store,
/// then incremental updates off the store's doc-change feed.
pub struct ReadModelCoordinator {
    store: Arc<Store>,
    models: Vec<Arc<dyn ReadModel>>,
}

impl ReadModelCoordinator {
    pub fn new(store: Arc<Store>, models: Vec<Arc<dyn ReadModel>>) -> Self {
        Self { store, models }
    }

    /// The read models this coordinator drives.
    pub fn models(&self) -> &[Arc<dyn ReadModel>] {
        &self.models
    }

    /// Take the initial snapshot of every current doc and apply it to every
    /// read model. Idempotent; call once at startup before [`run`](Self::run).
    pub fn prime(&self) {
        for id in self.store.doc_ids() {
            let Some(state) = self.store.full_state(id) else {
                continue;
            };
            let snap = DocSnapshot::from_state(&state, 0);
            for m in &self.models {
                if let Err(e) = m.apply(&snap) {
                    tracing::warn!("read model {} failed to prime: {e}", m.name());
                }
            }
        }
    }

    /// Subscribe to the store's doc-change feed and drive the read models.
    /// Runs until the feed closes (the store is dropped) or the task is
    /// cancelled.
    pub async fn run(self: Arc<Self>) {
        self.prime();
        let mut rx = self.store.subscribe_changes();
        while let Some(change) = rx.recv().await {
            let snap = DocSnapshot::from_change(&change);
            for m in &self.models {
                if let Err(e) = m.apply(&snap) {
                    tracing::warn!("read model {} failed on change: {e}", m.name());
                }
            }
        }
    }
}
