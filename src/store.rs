//! The document store: the durable, per-document **action log** plus the
//! reduced per-field state.
//!
//! The v1 store merged unsigned per-field ops and used the peer's identity
//! as the authority. This store is the redesigned core:
//!
//! - **The WAL is the action log.** Each user action (a model `kind` plus
//!   its payload, signed, chained by `prev_hash`) is one line in
//!   `<id>.alog`. The per-field map is *derived* by reducing the log through
//!   each action's model — the same function `doc verify` runs on demand.
//! - **Actions are signed at creation** (and co-signed by required
//!   principals via [`CoSig`]); peers verify the signature + co-signatures
//!   before an action's field writes are applied.
//! - **Models** ([`ModelRegistry`]) interpret actions. The default is
//!   `open@1` (a 1:1 reducer reproducing the v1 per-field behaviour); richer
//!   Level-1 models and the `group` model register alongside it.
//!
//! Local and remote actions flow through the same [`Inner::apply_action`]
//! path. The per-field merge itself ([`apply_op`]) is unchanged — it is the
//! convergence primitive; the action log is the auditable layer on top.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{SigningKey, VerifyingKey};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::warn;

use crate::action::Action;
use crate::doc::{apply_op, ApplyResult, Doc, DocId, Hash32, ModelRef, Origin, VecClock};
use crate::model::{Model, ModelRegistry, QuorumSpec};

/// Actions in a doc's live log before it is snapshot + truncated.
pub const SNAPSHOT_OPS: u64 = 1024;
/// Maximum doc name length.
pub const MAX_NAME_LEN: usize = 64;
/// Reserved first-field key carrying the doc name.
const NAME_KEY: &str = "__name__";

/// The default open model reference (`open@1`).
fn open_ref() -> ModelRef {
    ModelRef::new("open", "1")
}

/// A document's full durable state (used for snapshots and catch-up).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocState {
    pub doc: Doc,
    /// Union of the clocks of every applied action in the live log.
    pub clock: VecClock,
    pub deleted: bool,
    /// Merkle fingerprint of the live action log (for `doc verify`).
    #[serde(default)]
    pub log_hash: Option<Hash32>,
    /// The model that governs this doc's field map (open@1 by default).
    #[serde(default = "default_model_ref")]
    pub model: ModelRef,
    /// The space this doc belongs to. Absent in snapshots written before
    /// spaces existed, which is exactly what "no space" means.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<DocId>,
}

fn default_model_ref() -> ModelRef {
    open_ref()
}

/// A notification that a document's state changed: a local or remote
/// action was applied, or a state was adopted. The read-model layer
/// subscribes to this feed to maintain derived indexes incrementally.
#[derive(Debug, Clone)]
pub struct DocChange {
    pub doc_id: DocId,
    pub name: String,
    /// The model that governs the doc.
    pub model: ModelRef,
    pub deleted: bool,
    /// The resulting full state after the change.
    pub state: DocState,
    /// The timestamp of the action that caused the change (ordering key).
    pub ts: u64,
    /// The reducer kind of the action that caused the change, if any
    /// (`None` for an adopted state, which has no action). Lets a processor
    /// match a *transition*, not just a resulting state.
    pub action_kind: Option<String>,
    /// The payload's `field`, if the action set one.
    pub action_field: Option<String>,
    /// The payload's `value`, if the action set one.
    pub action_value: Option<serde_json::Value>,
}

/// Persisted store metadata: the ts high-water mark (`index.json`).
#[derive(Serialize, Deserialize)]
struct Index {
    ts_hint: u64,
}

/// The check result for a single action in a verified document log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionCheck {
    pub ts: u64,
    pub origin: String,
    pub kind: String,
    pub model: String,
    pub ok: bool,
    pub problems: Vec<String>,
}

/// The result of verifying a document's action log (the audit report):
/// one entry per surviving action plus an overall verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    pub name: String,
    pub doc_id: DocId,
    /// The model the log was written under (from the first surviving action).
    pub model: Option<ModelRef>,
    /// True when the log was truncated by a snapshot (the verified chain
    /// then starts at the snapshot's stored log hash, not genesis).
    pub from_snapshot: bool,
    /// One entry per surviving action, in order.
    pub actions: Vec<ActionCheck>,
    /// True when every check passed.
    pub ok: bool,
}

impl VerifyReport {
    /// Render the human-readable audit report (the "legal artifact").
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("verify {}\n", self.name));
        out.push_str(&format!("  doc  {}\n", self.doc_id));
        if let Some(m) = &self.model {
            out.push_str(&format!("  model {m}\n"));
        }
        out.push_str(&format!(
            "  {} action(s){}\n\n",
            self.actions.len(),
            if self.from_snapshot {
                " since snapshot"
            } else {
                ""
            }
        ));
        for (i, a) in self.actions.iter().enumerate() {
            out.push_str(&format!(
                "  [{i}] {} ts={} origin={} kind={} model={}\n",
                if a.ok { "ok  " } else { "FAIL" },
                a.ts,
                a.origin,
                a.kind,
                a.model
            ));
            for p in &a.problems {
                out.push_str(&format!("        - {p}\n"));
            }
        }
        out.push('\n');
        if self.ok {
            out.push_str("VERIFIED: every signature, co-signature, and precondition\ncheck out, the hash chain is intact, and the re-folded field map matches\nthe stored doc.\n");
        } else {
            out.push_str("FAILED: one or more checks did not pass (see above).\n");
        }
        out
    }
}

/// A document and its live (not yet snapshotted) action log.
#[derive(Debug)]
struct Entry {
    doc: Doc,
    clock: VecClock,
    deleted: bool,
    log: Vec<Action>,
    /// The model that wrote the doc (open@1 until an action sets it).
    model: ModelRef,
    /// The space this document belongs to, taken from its first action and
    /// immutable after. `None` for documents written before spaces existed.
    space: Option<DocId>,
}

impl Entry {
    fn new(id: DocId) -> Self {
        Entry {
            doc: Doc {
                id,
                name: String::new(),
                fields: Default::default(),
            },
            clock: VecClock::default(),
            deleted: false,
            log: Vec::new(),
            model: open_ref(),
            space: None,
        }
    }
}

/// Moves the reserved name field out of the field map. The store lifts it
/// into `doc.name` so user-facing field maps never contain it.
fn lift_name(doc: &mut Doc) {
    if let Some(f) = doc.fields.remove(NAME_KEY) {
        doc.name = f.value.as_str().unwrap_or_default().to_string();
    }
}

/// The store. Wrap in `Arc`; all public methods take `&self` and lock
/// internally. Local and remote actions flow through the same apply path.
pub struct Store {
    inner: Mutex<Inner>,
}

struct Inner {
    docs_dir: PathBuf,
    key: SigningKey,
    /// Identity (peer id string) used as the origin of local actions.
    origin: Origin,
    /// Ed25519 public keys of valid signers: own identity plus every
    /// peer that completed a Hello handshake (peer id -> key bytes).
    known_keys: BTreeMap<Origin, [u8; 32]>,
    entries: BTreeMap<DocId, Entry>,
    names: BTreeMap<String, DocId>,
    ts_hint: u64,
    /// Local actions awaiting delivery to the sync layer.
    outbound: Vec<Action>,
    /// The sync layer's outbound feed (set by [`Store::connect_outbound`]):
    /// each applied local action is sent here immediately, so the sync
    /// layer publishes without waiting for a poll interval.
    outbound_feed: Option<mpsc::UnboundedSender<Action>>,
    /// Actions quarantined for failing verification.
    quarantined: u64,
    /// The models this peer knows how to reduce (open@1 by default).
    models: ModelRegistry,
    /// Read-model subscribers (doc-change feed). Senders are non-blocking.
    subscribers: Vec<mpsc::UnboundedSender<DocChange>>,
}

impl Store {
    /// Open (or create) a store rooted at `docs_dir`. Replays snapshots +
    /// action logs and rebuilds the name index.
    /// Opens a store, registering `extra` models **before** the replay.
    ///
    /// Ordering is the whole point. `replay()` rebuilds each document by
    /// reducing its action log through the model that wrote it; a model
    /// registered afterwards is too late, and its documents are left as empty
    /// shells. Built-in models avoid this because they are seeded here; every
    /// other model — the realistic set, and anything registered at runtime —
    /// has to come in through this door.
    pub fn open_with_models(
        docs_dir: &Path,
        key: &SigningKey,
        origin: &str,
        extra: Vec<Arc<dyn Model>>,
    ) -> Result<Arc<Self>, String> {
        Self::open_inner(docs_dir, key, origin, extra)
    }

    pub fn open(docs_dir: &Path, key: &SigningKey, origin: &str) -> Result<Arc<Self>, String> {
        Self::open_inner(docs_dir, key, origin, Vec::new())
    }

    fn open_inner(
        docs_dir: &Path,
        key: &SigningKey,
        origin: &str,
        extra: Vec<Arc<dyn Model>>,
    ) -> Result<Arc<Self>, String> {
        std::fs::create_dir_all(docs_dir).map_err(|e| e.to_string())?;
        let mut inner = Inner {
            docs_dir: docs_dir.to_path_buf(),
            key: key.clone(),
            origin: origin.to_string(),
            known_keys: BTreeMap::new(),
            entries: BTreeMap::new(),
            names: BTreeMap::new(),
            ts_hint: 0,
            outbound: Vec::new(),
            outbound_feed: None,
            quarantined: 0,
            models: ModelRegistry::seeded_with_builtins(),
            subscribers: Vec::new(),
        };
        inner
            .known_keys
            .insert(inner.origin.clone(), key.verifying_key().to_bytes());
        // Before the replay, never after.
        for m in extra {
            inner.models.insert(m);
        }
        inner.replay();
        Ok(Arc::new(Store {
            inner: Mutex::new(inner),
        }))
    }

    /// Register an additional document model (e.g. loaded from a drive),
    /// so its actions can be reduced and verified on this peer.
    pub fn add_model(&self, model: Arc<dyn Model>) {
        self.inner.lock().models.insert(model);
    }

    /// The `(name, version)` refs of every loaded model, sorted.
    pub fn model_refs(&self) -> Vec<ModelRef> {
        self.inner.lock().models.refs()
    }

    /// Whether this peer can reduce actions under `ref_` (the model is
    /// loaded). The mesh model-distribution path requests the definition
    /// over the mesh when this is false.
    pub fn model_available(&self, ref_: &ModelRef) -> bool {
        self.inner.lock().models.find(ref_).is_some()
    }

    /// The loaded model's definition by reference, if it has one (for
    /// answering a mesh model request).
    pub fn model_definition(&self, ref_: &ModelRef) -> Option<serde_json::Value> {
        self.inner.lock().models.definition(ref_)
    }

    /// Subscribe to the doc-change feed: a [`DocChange`] is delivered here
    /// whenever a document's state changes on this store (a local or remote
    /// action applied, or a state adopted). The read-model layer consumes
    /// this to keep its derived indexes current.
    pub fn subscribe_changes(&self) -> mpsc::UnboundedReceiver<DocChange> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.inner.lock().subscribers.push(tx);
        rx
    }

    pub fn origin(&self) -> String {
        self.inner.lock().origin.clone()
    }

    /// A clone of the store's signing key (for the hello handshake).
    pub fn key(&self) -> SigningKey {
        self.inner.lock().key.clone()
    }

    /// May `peer` read this document?
    ///
    /// The one predicate behind all three enforcement points, so there is a
    /// single place to be wrong rather than three. `peer` is the origin of the
    /// node asking — the same key material the Noise handshake authenticated,
    /// because `signing_key()` derives the document key from the libp2p
    /// identity keypair. That identity is why any of this is enforceable: the
    /// peer on the far end of the session is the same principal a member list
    /// names.
    ///
    /// Defaults are chosen so that a mistake withholds rather than discloses:
    /// a document whose space document has not arrived yet is not served.
    pub fn may_peer_read(&self, peer: &str, id: DocId) -> bool {
        let inner = self.inner.lock();
        let Some(entry) = inner.entries.get(&id) else {
            return false;
        };
        let Some(space_id) = entry.space else {
            // Written before spaces existed. Replicates as it always did;
            // giving it a home is a migration, not something to do silently
            // here (and no default is safe -- see the design doc).
            return true;
        };

        // A space document is served to anyone its own copy names. Without
        // this the ACL is circular: to be told you are a member you must
        // already be able to read the document that says so. The tail is that
        // a removed member can never learn they were removed -- revocation is
        // forward-only and best-effort, and is documented as such.
        let gate = if space_id == id {
            entry
        } else {
            match inner.entries.get(&space_id) {
                Some(e) => e,
                None => return false,
            }
        };

        let visibility = crate::model::space::visibility_of(&gate.doc);
        match visibility.as_str() {
            crate::model::space::PUBLIC => true,
            crate::model::space::PRIVATE => false,
            _ => crate::model::space::members_of(&gate.doc)
                .iter()
                .any(|m| m == peer),
        }
    }

    /// The visibility of the space a document belongs to, if it has one.
    pub fn space_visibility(&self, id: DocId) -> Option<String> {
        let inner = self.inner.lock();
        let space_id = inner.entries.get(&id)?.space?;
        let gate = if space_id == id {
            inner.entries.get(&id)?
        } else {
            inner.entries.get(&space_id)?
        };
        Some(crate::model::space::visibility_of(&gate.doc))
    }

    /// Per-doc vector clocks, filtered to what `peer` may read.
    ///
    /// The unfiltered [`Store::summary`] leaks the *existence* of every
    /// document to every peer, which is metadata a protected space should not
    /// give up. Sync uses this; local callers use `summary`.
    pub fn summary_for(&self, peer: &str) -> BTreeMap<DocId, VecClock> {
        self.summary()
            .into_iter()
            .filter(|(id, _)| self.may_peer_read(peer, *id))
            .collect()
    }

    /// Per-doc vector clocks (for catch-up and reconciliation).
    pub fn summary(&self) -> BTreeMap<DocId, VecClock> {
        self.inner
            .lock()
            .entries
            .iter()
            .map(|(id, e)| (*id, e.clock.clone()))
            .collect()
    }

    pub fn doc_ids(&self) -> Vec<DocId> {
        self.inner.lock().entries.keys().copied().collect()
    }

    pub fn doc_name(&self, id: DocId) -> String {
        self.inner
            .lock()
            .entries
            .get(&id)
            .map(|e| e.doc.name.clone())
            .unwrap_or_default()
    }

    pub fn doc_count(&self) -> usize {
        self.inner.lock().names.len()
    }

    pub fn live_doc_count(&self) -> usize {
        self.inner
            .lock()
            .entries
            .values()
            .filter(|e| !e.deleted)
            .count()
    }

    pub fn quarantined_count(&self) -> u64 {
        self.inner.lock().quarantined
    }
    /// The peer ids (base58) whose keys this store has pinned — the *remote*
    /// peers it has authenticated, from any handshake. The local origin's key
    /// is also pinned (so local actions verify) but is filtered out here: the
    /// caller adds one for "yourself" to get a peer count that is never zero.
    pub fn known_peers(&self) -> Vec<String> {
        let inner = self.inner.lock();
        inner
            .known_keys
            .keys()
            .filter(|id| *id != &inner.origin)
            .cloned()
            .collect()
    }

    /// Record a peer's public key (from a Hello handshake) so its actions
    /// verify. Trust-on-first-use: the first key seen for a peer id is
    /// pinned; a later *different* key is a mismatch (the peer is refusing
    /// to verify) and returns `Err` so the caller can reject it. A matching
    /// key is idempotent.
    pub fn register_peer_key(&self, origin: &str, key: [u8; 32]) -> Result<(), String> {
        let mut inner = self.inner.lock();
        match inner.known_keys.get(origin) {
            Some(pinned) if *pinned == key => Ok(()),
            Some(_) => Err(format!(
                "key mismatch for peer {origin}: a different key was pinned on first contact (TOFU)"
            )),
            None => {
                inner.known_keys.insert(origin.to_string(), key);
                Ok(())
            }
        }
    }

    /// The pinned public key for `origin`, if this node has seen one.
    ///
    /// Exposed so a proposal's signatures can be verified before it is
    /// applied — an unknown signer must be an error, never a silently
    /// ignored one that still appears to count toward a quorum.
    pub fn peer_key(&self, origin: &str) -> Option<[u8; 32]> {
        self.inner.lock().known_keys.get(origin).copied()
    }

    pub fn create_doc(
        &self,
        name: &str,
        fields: BTreeMap<String, serde_json::Value>,
    ) -> Result<DocId, String> {
        Store::validate_name(name)?;
        let mut inner = self.inner.lock();
        if inner.names.contains_key(name) {
            return Err(format!("a doc named {name} already exists"));
        }
        let id = DocId::new();
        inner.entries.insert(id, Entry::new(id));
        // open@1: a create action lifts the name, then one set action per
        // field. Applied one at a time so each action's prev_hash chains
        // to the previous - building the whole batch against an empty log
        // would leave every link None.
        let create = inner.build_action(
            id,
            &open_ref(),
            "create",
            &serde_json::json!({ "field": NAME_KEY, "value": name }),
        )?;
        inner.apply_action(&create)?;
        for (field, value) in &fields {
            let set = inner.build_action(
                id,
                &open_ref(),
                "set",
                &serde_json::json!({ "field": field, "value": value }),
            )?;
            inner.apply_action(&set)?;
        }
        inner.names.insert(name.to_string(), id);
        Ok(id)
    }

    /// Create a document under a specific model's `init` reducer (the
    /// model-aware counterpart to [`create_doc`], which is always `open@1`).
    /// The `init` action must set `__name__` to `name`. Returns the new id.
    pub fn create_doc_model(
        &self,
        name: &str,
        model: &ModelRef,
        payload: &serde_json::Value,
    ) -> Result<DocId, String> {
        self.create_doc_in_space(name, model, payload, None)
    }

    /// Create a document that lives in `space`.
    ///
    /// The space is stamped on the entry before the `init` action is built, so
    /// `build_action` signs it into the first action -- which is what makes the
    /// binding immutable afterwards.
    pub fn create_doc_in_space(
        &self,
        name: &str,
        model: &ModelRef,
        payload: &serde_json::Value,
        space: Option<DocId>,
    ) -> Result<DocId, String> {
        Store::validate_name(name)?;
        let mut inner = self.inner.lock();
        if inner.names.contains_key(name) {
            return Err(format!("a doc named {name} already exists"));
        }
        let id = DocId::new();
        let mut entry = Entry::new(id);
        entry.space = space;
        inner.entries.insert(id, entry);
        let action = inner.build_action(id, model, "init", payload)?;
        inner.apply_action(&action)?;
        Ok(id)
    }

    /// Create a space document, which lives in itself.
    ///
    /// Self-reference is not a trick: it is what lets a member be told they
    /// are a member. Any other arrangement makes the ACL circular, because
    /// reading the document that names you would require already being able
    /// to read it.
    pub fn create_space(&self, name: &str, payload: &serde_json::Value) -> Result<DocId, String> {
        self.create_space_at(DocId::new(), name, payload)
    }

    /// Create a space at a chosen id. Migration uses this with
    /// [`DocId::derived`] so the same group becomes the same space on every
    /// node that migrates it.
    pub fn create_space_at(
        &self,
        id: DocId,
        name: &str,
        payload: &serde_json::Value,
    ) -> Result<DocId, String> {
        Store::validate_name(name)?;
        let mut inner = self.inner.lock();
        if inner.names.contains_key(name) {
            return Err(format!("a doc named {name} already exists"));
        }
        if inner.entries.contains_key(&id) {
            return Err(format!("a doc with id {id} already exists"));
        }
        let mut entry = Entry::new(id);
        entry.space = Some(id);
        inner.entries.insert(id, entry);
        let action = inner.build_action(id, &ModelRef::new("space", "1"), "init", payload)?;
        inner.apply_action(&action)?;
        Ok(id)
    }

    /// The space a document belongs to, if any.
    pub fn space_of(&self, id: DocId) -> Option<DocId> {
        self.inner.lock().entries.get(&id)?.space
    }

    pub fn update_field(
        &self,
        name: &str,
        field: &str,
        value: serde_json::Value,
    ) -> Result<(), String> {
        let id = self.doc_id_or_err(name)?;
        let action = self.inner.lock().build_action(
            id,
            &open_ref(),
            "set",
            &serde_json::json!({ "field": field, "value": value }),
        )?;
        self.inner.lock().apply_action(&action).map(|_| ())
    }

    pub fn delete_field(&self, name: &str, field: &str) -> Result<(), String> {
        let id = self.doc_id_or_err(name)?;
        let action = self.inner.lock().build_action(
            id,
            &open_ref(),
            "delete",
            &serde_json::json!({ "field": field }),
        )?;
        self.inner.lock().apply_action(&action).map(|_| ())
    }

    pub fn delete_doc(&self, name: &str) -> Result<(), String> {
        let id = self.doc_id_or_err(name)?;
        let action =
            self.inner
                .lock()
                .build_action(id, &open_ref(), "delete", &serde_json::json!({}))?;
        self.inner.lock().apply_action(&action).map(|_| ())
    }

    fn doc_id_or_err(&self, name: &str) -> Result<DocId, String> {
        let id = *self
            .inner
            .lock()
            .names
            .get(name)
            .ok_or_else(|| format!("no doc named {name}"))?;
        Ok(id)
    }

    pub fn get(&self, name: &str) -> Option<Doc> {
        let inner = self.inner.lock();
        let id = inner.names.get(name)?;
        let e = inner.entries.get(id)?;
        if e.deleted {
            return None;
        }
        Some(e.doc.live())
    }

    pub fn list(&self) -> Vec<Doc> {
        self.inner
            .lock()
            .entries
            .values()
            .filter(|e| !e.deleted)
            .map(|e| e.doc.live())
            .collect()
    }

    pub fn full_state(&self, id: DocId) -> Option<DocState> {
        let inner = self.inner.lock();
        let e = inner.entries.get(&id)?;
        Some(DocState {
            doc: e.doc.clone(),
            clock: e.clock.clone(),
            deleted: e.deleted,
            log_hash: e.log.last().map(|a| a.hash()),
            model: e.model.clone(),
            space: e.space,
        })
    }

    /// Wholesale state adoption (a peer far behind takes our reduced state).
    /// The adopted doc's action log is replaced by a single `adopt` action so
    /// the audit trail records the adoption.
    pub fn adopt_state(&self, state: &DocState) -> Result<(), String> {
        let mut inner = self.inner.lock();
        {
            let entry = inner
                .entries
                .entry(state.doc.id)
                .or_insert_with(|| Entry::new(state.doc.id));
            entry.doc = state.doc.clone();
            lift_name(&mut entry.doc);
            entry.clock = state.clock.clone();
            entry.deleted = state.deleted;
            entry.model = state.model.clone();
            entry.log.clear();
        }
        let ts = inner.ts_hint;
        inner.emit_change(state.doc.id, ts, None);
        Ok(())
    }

    /// Take the queued local actions (the sync layer publishes them to the
    /// mesh). Remote actions are not re-gossiped.
    pub fn drain_outbound(&self) -> Vec<Action> {
        let mut inner = self.inner.lock();
        std::mem::take(&mut inner.outbound)
    }

    /// Connect the sync layer's outbound feed: every applied local action
    /// is sent to the returned receiver as soon as it is applied (the
    /// store's lock is held when it is sent, so ordering matches the log).
    /// The sync layer publishes from this feed immediately and keeps
    /// [`drain_outbound`] as a backstop for actions applied before the
    /// feed was connected.
    pub fn connect_outbound(&self) -> mpsc::UnboundedReceiver<Action> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.inner.lock().outbound_feed = Some(tx);
        rx
    }

    /// Catch-up reply: the current state plus the actions the requester's
    /// clock does not cover (capped by the caller).
    pub fn catch_up(&self, id: DocId, have: &VecClock) -> (Option<DocState>, Vec<Action>) {
        let inner = self.inner.lock();
        let entry = match inner.entries.get(&id) {
            Some(e) => e,
            None => return (None, Vec::new()),
        };
        let state = Some(DocState {
            doc: entry.doc.clone(),
            clock: entry.clock.clone(),
            deleted: entry.deleted,
            log_hash: entry.log.last().map(|a| a.hash()),
            model: entry.model.clone(),
            space: entry.space,
        });
        let missing: Vec<Action> = entry
            .log
            .iter()
            .filter(|a| !have.covers(&a.clock))
            .cloned()
            .collect();
        (state, missing)
    }

    /// Apply a remote action (signature-verified through the model).
    pub fn apply_remote_action(&self, action: &Action) -> Result<ApplyResult, String> {
        self.inner.lock().apply_action(action)
    }

    pub fn snapshot(&self, id: &DocId) -> Result<(), String> {
        self.inner.lock().snapshot(id)
    }

    /// Verify a document's action log (read-only): replay every surviving
    /// action (re-reduced through its model), checking each signature,
    /// co-signature, and precondition, the hash chain, and that the
    /// re-folded field map equals the stored doc. The chain starts at the
    /// snapshot's stored log hash when the log was truncated, else at
    /// genesis. Returns a per-action audit report plus an overall verdict.
    pub fn verify(&self, name: &str) -> Result<VerifyReport, String> {
        let g = self.inner.lock();
        let id = *g
            .names
            .get(name)
            .ok_or_else(|| format!("no doc named '{name}'"))?;
        let entry = g
            .entries
            .get(&id)
            .ok_or_else(|| format!("no doc named '{name}'"))?;
        let stored = entry.doc.clone();

        // The snapshot is the trusted base when present; its stored log
        // hash is where the surviving log chain begins.
        let snap_path = g.docs_dir.join(format!("{id}.snap"));
        let base: Option<DocState> = std::fs::read_to_string(&snap_path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok());
        let mut scratch = match &base {
            Some(b) => Entry {
                doc: b.doc.clone(),
                clock: b.clock.clone(),
                deleted: b.deleted,
                log: Vec::new(),
                model: b.model.clone(),
                space: b.space,
            },
            None => Entry::new(id),
        };
        lift_name(&mut scratch.doc);
        // Every action hash seen so far, seeded with the snapshot's log hash
        // when the log was truncated. Concurrent writers make the log a DAG
        // rather than a line — two actions may legitimately chain to the same
        // parent — so what must hold is not "this action follows the previous
        // one" but "this action's prev_hash names an action that really
        // precedes it". Dropping an action from the log still orphans
        // everything that pointed at it, which is what the chain is for.
        let mut seen: Vec<Hash32> = base
            .as_ref()
            .and_then(|b| b.log_hash)
            .into_iter()
            .collect();

        let mut report = VerifyReport {
            name: name.to_string(),
            doc_id: id,
            model: None,
            from_snapshot: base.is_some(),
            actions: Vec::new(),
            ok: true,
        };

        for action in &entry.log {
            let mut problems: Vec<String> = Vec::new();
            let model = match g.models.find(&action.model) {
                Some(m) => Some(m),
                None => {
                    problems.push(format!("model {} not loaded", action.model.name));
                    None
                }
            };
            if report.model.is_none() {
                report.model = Some(action.model.clone());
            }

            // Hash chain (see `seen` above).
            match action.prev_hash {
                Some(got) if !seen.contains(&got) => {
                    problems.push(format!(
                        "hash chain broken: prev_hash {got} names no earlier action"
                    ));
                }
                None if !seen.is_empty() => {
                    problems.push("chain gap: missing prev_hash".into());
                }
                _ => {}
            }

            // Origin signature.
            match g.known_keys.get(&action.origin).copied() {
                Some(k) => match VerifyingKey::from_bytes(&k) {
                    Ok(pk) => {
                        if !action.verify(&pk) {
                            problems.push("origin signature invalid".into());
                        }
                    }
                    Err(_) => problems.push("origin key malformed".into()),
                },
                None => problems.push(format!("unknown origin {}", action.origin)),
            }

            // Co-signatures.
            for (i, cs) in action.cosig.iter().enumerate() {
                match g.known_keys.get(&cs.origin).copied() {
                    Some(k) => match VerifyingKey::from_bytes(&k) {
                        Ok(pk) => {
                            if !action.verify_cosig(i, &pk) {
                                problems
                                    .push(format!("co-signature {} from {} invalid", i, cs.origin));
                            }
                        }
                        Err(_) => problems.push(format!("co-signer {} key malformed", cs.origin)),
                    },
                    None => problems.push(format!("unknown co-signer {}", cs.origin)),
                }
            }

            // Precondition + quorum (against the re-folded pre-state).
            if let Some(m) = &model {
                if let Err(r) = m.check_precondition(&scratch.doc, action) {
                    problems.push(format!("precondition: {}", r.describe()));
                }
                if let Some(spec) = m.quorum(action.kind.as_str()) {
                    if let Some(p) = g.quorum_problem(&spec, action) {
                        problems.push(p);
                    }
                }
            }

            // Re-fold through the model (same merge as live application).
            if let Some(m) = &model {
                apply_ops_to_entry(&mut scratch, m.as_ref(), action);
            }

            seen.push(action.hash());
            let ok = problems.is_empty();
            if !ok {
                report.ok = false;
            }
            report.actions.push(ActionCheck {
                ts: action.ts,
                origin: action.origin.clone(),
                kind: action.kind.clone(),
                model: action.model.to_string(),
                ok,
                problems,
            });
        }

        // Field-map equality: the re-folded doc must equal the stored doc.
        if scratch.doc != stored {
            report.ok = false;
            let note = "re-folded field map does not match the stored doc".to_string();
            match report.actions.last_mut() {
                Some(last) => {
                    last.ok = false;
                    last.problems.push(note);
                }
                None => {
                    report.actions.push(ActionCheck {
                        ts: 0,
                        origin: String::new(),
                        kind: "(field map)".into(),
                        model: String::new(),
                        ok: false,
                        problems: vec![note],
                    });
                }
            }
        }

        Ok(report)
    }

    /// Build and apply a locally-signed model action (the store's own key)
    /// to an existing doc: the local path for `doc action`. The daemon
    /// publishes it to the mesh through the normal apply path.
    pub fn apply_local_action(
        &self,
        name: &str,
        model: &ModelRef,
        kind: &str,
        payload: &serde_json::Value,
    ) -> Result<Action, String> {
        let mut g = self.inner.lock();
        let id = *g
            .names
            .get(name)
            .ok_or_else(|| format!("no doc named '{name}'"))?;
        let action = g.build_action(id, model, kind, payload)?;
        g.apply_action(&action)?;
        Ok(action)
    }

    /// Builds and origin-signs a local action **without applying it**.
    ///
    /// The half of [`Self::apply_local_action`] that a quorum-gated reducer
    /// needs: the action has to travel to other members for co-signing before
    /// it can be applied, because the store rejects it until the quorum is
    /// met.
    ///
    /// The result is pinned to the document's current log position through
    /// `prev_hash`, so it must be applied before the document changes.
    pub fn build_local_action(
        &self,
        name: &str,
        model: &ModelRef,
        kind: &str,
        payload: &serde_json::Value,
    ) -> Result<Action, String> {
        let mut g = self.inner.lock();
        let id = *g
            .names
            .get(name)
            .ok_or_else(|| format!("no doc named '{name}'"))?;
        g.build_action(id, model, kind, payload)
    }

    /// Applies an already-signed action that this node originated.
    ///
    /// Used to submit a proposal once enough members have co-signed it. All
    /// the usual checks still run — signature, hash chain, preconditions and
    /// quorum — so a proposal that has not reached its quorum is refused here
    /// exactly as it would be on any other node.
    pub fn apply_signed_action(&self, action: &Action) -> Result<(), String> {
        let mut g = self.inner.lock();
        g.apply_action(action)?;
        Ok(())
    }

    pub fn wal_path(&self, id: &DocId) -> PathBuf {
        self.inner.lock().docs_dir.join(format!("{}.alog", id))
    }

    pub fn validate_name(name: &str) -> Result<(), String> {
        if name.is_empty() || name.len() > MAX_NAME_LEN {
            return Err(format!("name must be 1..={MAX_NAME_LEN} chars"));
        }
        let ok = name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'));
        if !ok || name.starts_with('.') {
            return Err(format!(
                "name may contain lowercase ascii, digits, '-', '_', '.' (not starting with '.'): {name}"
            ));
        }
        Ok(())
    }
}

impl Inner {
    /// Rebuild in-memory state from snapshots + action logs.
    fn replay(&mut self) {
        let index_path = self.docs_dir.join("index.json");
        if let Ok(raw) = std::fs::read_to_string(&index_path) {
            if let Ok(idx) = serde_json::from_str::<Index>(&raw) {
                self.ts_hint = self.ts_hint.max(idx.ts_hint);
            }
        }
        // A doc has an `.alog` from its first action; the `.snap` (when
        // present) is just its base state. Scan the `.alog` stems.
        let mut ids: Vec<DocId> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.docs_dir) {
            for de in rd.filter_map(|e| e.ok()) {
                let p = de.path();
                if p.extension().and_then(|e| e.to_str()) == Some("alog") {
                    if let Some(id) = p
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .and_then(|s| DocId::parse(s).ok())
                    {
                        ids.push(id);
                    }
                }
            }
        }
        ids.sort();
        ids.dedup();
        for id in ids {
            self.replay_doc(id);
        }
        self.persist_index();
    }

    fn replay_doc(&mut self, id: DocId) {
        let snap_path = self.docs_dir.join(format!("{id}.snap"));
        let alog_path = self.docs_dir.join(format!("{id}.alog"));
        let base: Option<DocState> = std::fs::read_to_string(&snap_path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok());
        let mut entry = match base {
            Some(state) => Entry {
                doc: state.doc,
                clock: state.clock,
                deleted: state.deleted,
                log: Vec::new(),
                model: state.model.clone(),
                space: state.space,
            },
            None => Entry::new(id),
        };
        entry.doc.id = id;
        lift_name(&mut entry.doc);
        if let Ok(content) = std::fs::read_to_string(&alog_path) {
            for line in content.lines().filter(|l| !l.trim().is_empty()) {
                let action: Action = match serde_json::from_str(line) {
                    Ok(a) => a,
                    Err(e) => {
                        warn!("skipping malformed action in {alog_path:?}: {e}");
                        continue;
                    }
                };
                match self.models.find(&action.model) {
                    Some(model) => {
                        let (applied, deleted) =
                            apply_ops_to_entry(&mut entry, model.as_ref(), &action);
                        entry.deleted = deleted;
                        // Mirror the live apply path (apply_action): the doc's
                        // governing model is the model of its actions, not the
                        // open@1 default an unsnapshotted entry starts with.
                        entry.model = action.model.clone();
                        if applied {
                            entry.log.push(action);
                        }
                    }
                    None => {
                        warn!(
                            "cannot reduce action of {} (model {} not loaded); preserving for audit",
                            action.doc_id,
                            action.model.name
                        );
                        entry.log.push(action);
                    }
                }
            }
        }
        let empty = entry.doc.name.is_empty()
            && entry.doc.fields.is_empty()
            && entry.log.is_empty()
            && !entry.deleted;
        if empty {
            return;
        }
        let name = entry.doc.name.clone();
        let max_ts = entry.log.iter().map(|a| a.ts).max().unwrap_or(0);
        self.entries.insert(id, entry);
        if !name.is_empty() {
            self.names.insert(name, id);
        }
        self.ts_hint = self.ts_hint.max(max_ts);
    }

    /// Persist the ts high-water mark so it never goes backwards across a
    /// restart (SystemTime also floors it; this is belt-and-suspenders).
    fn persist_index(&mut self) {
        let index = Index {
            ts_hint: self.ts_hint,
        };
        if let Ok(json) = serde_json::to_vec(&index) {
            let _ = std::fs::write(self.docs_dir.join("index.json"), json);
        }
    }

    /// Build a signed local action. The clock is computed from the doc's
    /// current clock (one tick) without mutating it; the merge stamps the
    /// reduced ops and the log append bumps the doc clock.
    fn build_action(
        &mut self,
        id: DocId,
        model: &ModelRef,
        kind: &str,
        payload: &serde_json::Value,
    ) -> Result<Action, String> {
        let ts = self.next_ts();
        let clock = {
            let mut c = self.entries.get(&id).expect("doc exists").clock.clone();
            c.tick(&self.origin);
            c
        };
        let prev_hash = self
            .entries
            .get(&id)
            .and_then(|e| e.log.last().map(|a| a.hash()));
        // A document's space is decided by its first action and never moves.
        let space = self.entries.get(&id).and_then(|e| e.space);
        let mut action = Action {
            doc_id: id,
            model: self.canonical_model_ref(model),
            kind: kind.to_string(),
            payload: payload.clone(),
            ts,
            clock,
            origin: self.origin.clone(),
            cosig: Vec::new(),
            sig: [0; 64],
            prev_hash,
            space,
        };
        action.sign(&self.key);
        Ok(action)
    }

    /// The loaded model's canonical reference (with its content hash) for
    /// `ref_`, or `ref_` itself when the model is not loaded. Stamping the
    /// hash on every action is what lets a peer verify a distributed model
    /// definition against the exact model the doc was written under.
    fn canonical_model_ref(&self, ref_: &ModelRef) -> ModelRef {
        self.models
            .find(ref_)
            .map(|m| m.ref_().clone())
            .unwrap_or_else(|| ref_.clone())
    }

    /// The single apply path (local and remote). Verify -> model -> validate
    /// -> precondition -> reduce -> per-field merge -> log + WAL -> queue.
    fn apply_action(&mut self, action: &Action) -> Result<ApplyResult, String> {
        // 1. Signature + co-signature verification against known keys.
        let origin_key = match self.known_keys.get(&action.origin).copied() {
            Some(k) => k,
            None => return self.reject(action, "unknown origin"),
        };
        let pk = match VerifyingKey::from_bytes(&origin_key) {
            Ok(k) => k,
            Err(e) => return self.reject(action, &format!("bad origin key: {e}")),
        };
        if !action.verify(&pk) {
            return self.reject(action, "bad signature");
        }
        for (i, cs) in action.cosig.iter().enumerate() {
            let ck = match self.known_keys.get(&cs.origin).copied() {
                Some(k) => k,
                None => return self.reject(action, &format!("unknown co-signer {}", cs.origin)),
            };
            let cpk = match VerifyingKey::from_bytes(&ck) {
                Ok(k) => k,
                Err(e) => return self.reject(action, &format!("bad co-signer key: {e}")),
            };
            if !action.verify_cosig(i, &cpk) {
                return self.reject(action, "bad co-signature");
            }
        }
        // 2. Resolve the model (owned Arc; no outstanding borrow of the
        //    registry while we mutate the entries map below).
        let model = match self.models.find(&action.model) {
            Some(m) => m,
            None => {
                return self.reject(action, &format!("model {} unavailable", action.model.name))
            }
        };
        // 3. Payload schema.
        if let Err(r) = model.validate_payload(action.kind.as_str(), &action.payload) {
            return self.reject(action, &r.describe());
        }

        // 3b. Quorum (declared by the model; checked here because it needs
        //     the group's state — a different document). Runs before step 4,
        //     so a failed quorum never mutates the entry.
        if let Some(spec) = model.quorum(action.kind.as_str()) {
            if let Some(problem) = self.quorum_problem(&spec, action) {
                return self.reject(action, &problem);
            }
        }
        // 3c. Model-declared authorization (declared like quorum; checked here
        //     because it can reach this document's full state). For a group
        //     `post`, this is the *private-channel* gate: the actor must be in
        //     the target channel's allow-list — a nested object a plain
        //     `actor-in` cannot express. Runs before step 4, so a failed auth
        //     never mutates the entry. Skipped when the doc is not loaded yet
        //     (its preconditions/reduce decide the outcome).
        if let Some(entry) = self.entries.get(&action.doc_id) {
            if let Err(r) = model.authorize(&entry.doc, action) {
                return self.reject(action, &r.describe());
            }
        }
        // 3c-bis. Space membership. Declared by the model, checked here
        //     because it needs the *space* document's state -- exactly the
        //     arrangement quorum uses. This is how an app inherits its space's
        //     members instead of carrying its own copy of them.
        if let Some(list) = model.requires_space_member(action.kind.as_str()) {
            let Some(space_id) = action
                .space
                .or_else(|| self.entries.get(&action.doc_id).and_then(|e| e.space))
            else {
                return self.reject(action, "this reducer requires a space, and the document has none");
            };
            let members = match self.entries.get(&space_id) {
                Some(e) => crate::model::space::list_of(&e.doc, &list),
                // Refusing a write because the space document has not arrived
                // is recoverable -- the writer retries. Allowing it because we
                // could not check is not.
                None => return self.reject(action, "the space document is not here yet"),
            };
            if !members.iter().any(|m| m == &action.origin) {
                return self.reject(
                    action,
                    &format!("actor {} is not in this space's {list}", action.origin),
                );
            }
        }
        // 3d. Space binding. A document's space is decided by its first action
        //     and never moves. An action naming a different space is a document
        //     being dragged out of a protected space into a public one, which
        //     is the move putting `space` under the signature exists to stop.
        if let Some(entry) = self.entries.get(&action.doc_id) {
            if !entry.log.is_empty() && entry.space != action.space {
                return self.reject(action, "space mismatch: a document cannot change space");
            }
        }
        // 4. Precondition + reduce + per-field merge (one entries borrow).
        //
        // `append`/`remove` reduce to a *whole-array* write, and a field
        // merges last-writer-wins, so the fold depends on the order actions
        // are applied in. Two nodes that received the same two concurrent
        // actions in opposite orders ended up with different arrays — and
        // stayed that way: a member removed on one node was still a member on
        // the other, permanently. An action that arrives out of canonical
        // order therefore triggers a re-fold of the whole live log. In the
        // common case — actions arriving in order — this costs one compare.
        let out_of_order = self
            .entries
            .get(&action.doc_id)
            .and_then(|e| e.log.last())
            .map(|last| canon_key(action) < canon_key(last))
            .unwrap_or(false);
        let (applied_any, deleted) = {
            let entry = self
                .entries
                .entry(action.doc_id)
                .or_insert_with(|| Entry::new(action.doc_id));
            if let Err(r) = model.check_precondition(&entry.doc, action) {
                self.quarantined += 1;
                warn!("action quarantined (precondition): {}", r.describe());
                return Err(r.describe());
            }
            if entry.log.is_empty() {
                entry.space = action.space;
            }
            let (a, d) = apply_ops_to_entry(entry, model.as_ref(), action);
            entry.model = action.model.clone();
            entry.log.push(action.clone());
            (a, d)
        };
        if out_of_order {
            self.refold(action.doc_id)?;
        }
        // 5. Durability: append the action to the WAL (the field map is
        //    derived from the log on replay).
        if let Err(e) = persist_action(&self.docs_dir, action) {
            warn!("WAL write failed for {}: {e}", action.doc_id);
        }
        if applied_any {
            // Only local actions are re-published: the origin gossips its
            // own actions, and the mesh's dedupe + catch-up carry remote
            // ones. Re-gossipping remote actions would amplify traffic
            // (every peer re-sending what it received).
            if action.origin == self.origin {
                self.outbound.push(action.clone());
                if let Some(tx) = &self.outbound_feed {
                    // Immediate delivery to the sync layer: publishing
                    // does not wait for its poll interval. The queue above
                    // stays the backstop (drained on the sync layer's tick).
                    let _ = tx.send(action.clone());
                }
            }
            self.ts_hint = self.ts_hint.max(action.ts);
        }
        // 6. Snapshot when the live log grows past the threshold.
        if self
            .entries
            .get(&action.doc_id)
            .map(|e| e.log.len() as u64)
            .unwrap_or(0)
            >= SNAPSHOT_OPS
        {
            self.snapshot(&action.doc_id)?;
        }

        // 7. Keep the name index in sync with the doc's state: register a
        //    live doc's name, free a deleted doc's name. Covers local and
        //    remote actions alike.
        if let Some(e) = self.entries.get(&action.doc_id) {
            let name = e.doc.name.clone();
            let is_del = e.deleted;
            if is_del {
                if !name.is_empty() && self.names.get(&name).copied() == Some(action.doc_id) {
                    self.names.remove(&name);
                }
            } else if !name.is_empty() {
                self.names.insert(name, action.doc_id);
            }
        }
        if applied_any || deleted {
            self.emit_change(action.doc_id, action.ts, Some(action));
        }
        Ok(ApplyResult {
            applied: applied_any,
            doc_deleted: deleted,
        })
    }

    /// Publish a doc-change event to every subscriber (non-blocking; dead
    /// senders are pruned). Called after a successful apply or adoption.
    fn emit_change(&mut self, doc_id: DocId, ts: u64, action: Option<&Action>) {
        let Some(entry) = self.entries.get(&doc_id) else {
            return;
        };
        let (action_kind, action_field, action_value) = match action {
            Some(a) => {
                let (action_field, action_value) =
                    if let Some(f) = a.payload.get("field").and_then(|v| v.as_str()) {
                        // An explicit `field`/`value` action (the `open` model).
                        (Some(f.to_string()), a.payload.get("value").cloned())
                    } else if let Some(obj) = a.payload.as_object() {
                        // A single-field reducer (e.g. `set-status`): the lone
                        // payload key is the field being written, so a processor
                        // can match a transition on the field + value.
                        if obj.len() == 1 {
                            let (k, v) = obj.iter().next().expect("len == 1");
                            (Some(k.clone()), Some(v.clone()))
                        } else {
                            (None, None)
                        }
                    } else {
                        (None, None)
                    };
                (Some(a.kind.clone()), action_field, action_value)
            }
            None => (None, None, None),
        };
        let change = DocChange {
            doc_id,
            name: entry.doc.name.clone(),
            model: entry.model.clone(),
            deleted: entry.deleted,
            state: DocState {
                doc: entry.doc.clone(),
                clock: entry.clock.clone(),
                deleted: entry.deleted,
                log_hash: entry.log.last().map(|a| a.hash()),
                model: entry.model.clone(),
                space: entry.space,
            },
            ts,
            action_kind,
            action_field,
            action_value,
        };
        let mut dead = Vec::new();
        for (i, tx) in self.subscribers.iter().enumerate() {
            if tx.send(change.clone()).is_err() {
                dead.push(i);
            }
        }
        if !dead.is_empty() {
            let kept = std::mem::take(&mut self.subscribers)
                .into_iter()
                .enumerate()
                .filter(|(i, _)| !dead.contains(i))
                .map(|(_, s)| s)
                .collect();
            self.subscribers = kept;
        }
    }

    /// Quarantine: count it and report; the action is not applied.
    fn reject(&mut self, action: &Action, reason: &str) -> Result<ApplyResult, String> {
        self.quarantined += 1;
        warn!(
            "action {} from {} quarantined: {reason}",
            action.doc_id, action.origin
        );
        Err(reason.to_string())
    }

    /// Snapshot the doc (full state + log hash) and truncate the log.
    /// Rebuild a document from its snapshot base by folding every action in
    /// the live log in canonical order.
    ///
    /// Called when an action arrives out of order. Preconditions and the
    /// model's `auth` rule are re-checked at each action's *canonical*
    /// position, not at the position it happened to arrive in, so whether an
    /// action counts is also a function of the action set: an action that
    /// canonically precedes the one that authorized it does not apply. The
    /// log keeps every accepted action either way — a later arrival can move
    /// the canonical order and make it count on the next fold.
    fn refold(&mut self, doc_id: DocId) -> Result<(), String> {
        let snap_path = self.docs_dir.join(format!("{doc_id}.snap"));
        let base: Option<DocState> = std::fs::read_to_string(&snap_path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok());

        let Some(entry) = self.entries.get(&doc_id) else {
            return Ok(());
        };
        let clock = entry.clock.clone();
        let mut log = entry.log.clone();
        log.sort_by_key(canon_key);

        // Resolve every model up front: the fold below borrows the entry
        // mutably and cannot also borrow the registry.
        let mut models = Vec::with_capacity(log.len());
        for a in &log {
            models.push(
                self.models
                    .find(&a.model)
                    .ok_or_else(|| format!("model {} unavailable for re-fold", a.model.name))?,
            );
        }

        let mut fresh = match &base {
            Some(b) => Entry {
                doc: b.doc.clone(),
                clock: b.clock.clone(),
                deleted: b.deleted,
                log: Vec::new(),
                model: b.model.clone(),
                space: b.space,
            },
            None => Entry::new(doc_id),
        };
        lift_name(&mut fresh.doc);
        // The space comes from the canonically-first action, so a re-fold that
        // reorders the log can also settle which space the document is in.
        if fresh.space.is_none() {
            fresh.space = log.first().and_then(|a| a.space);
        }
        for (a, model) in log.iter().zip(models.iter()) {
            if model.check_precondition(&fresh.doc, a).is_err()
                || model.authorize(&fresh.doc, a).is_err()
            {
                continue;
            }
            apply_ops_to_entry(&mut fresh, model.as_ref(), a);
            fresh.model = a.model.clone();
        }
        // A vector clock is a per-origin maximum — order-independent — so the
        // clock the entry already carries is correct as it stands.
        fresh.clock = clock;
        fresh.log = log;
        if let Some(slot) = self.entries.get_mut(&doc_id) {
            *slot = fresh;
        }
        Ok(())
    }

    fn snapshot(&mut self, id: &DocId) -> Result<(), String> {
        let Some(entry) = self.entries.get_mut(id) else {
            return Ok(());
        };
        let state = DocState {
            doc: entry.doc.clone(),
            clock: entry.clock.clone(),
            deleted: entry.deleted,
            log_hash: entry.log.last().map(|a| a.hash()),
            model: open_ref(),
            space: entry.space,
        };
        let snap_path = self.docs_dir.join(format!("{}.snap", id));
        let json = serde_json::to_vec_pretty(&state).map_err(|e| e.to_string())?;
        std::fs::write(&snap_path, json).map_err(|e| e.to_string())?;
        // Truncate the WAL: the snapshot is now the base state.
        let alog_path = self.docs_dir.join(format!("{}.alog", id));
        if let Err(e) = std::fs::write(&alog_path, "") {
            warn!("WAL truncate failed for {id}: {e}");
        }
        entry.log.clear();
        Ok(())
    }

    fn next_ts(&mut self) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);
        self.ts_hint = self.ts_hint.max(now);
        self.ts_hint += 1;
        self.ts_hint
    }

    /// Check a declared quorum against the group's state. Returns `Some`
    /// with a problem description when the quorum is not met. The caller
    /// has already verified the origin and co-signature signatures.
    fn quorum_problem(&self, spec: &QuorumSpec, action: &Action) -> Option<String> {
        let group_name = if spec.group == "$self" {
            match self.entries.get(&action.doc_id).map(|e| e.doc.name.clone()) {
                Some(n) if !n.is_empty() => n,
                _ => return Some("quorum group '$self' has no name".into()),
            }
        } else {
            spec.group.clone()
        };
        let group_doc = match self
            .names
            .get(&group_name)
            .and_then(|id| self.entries.get(id).map(|e| e.doc.live()))
        {
            Some(gd) => gd,
            None => return Some(format!("quorum group '{group_name}' not found")),
        };
        let members: HashSet<String> = group_doc
            .fields
            .get(&spec.field)
            .map(|f| f.value.clone())
            .unwrap_or(serde_json::Value::Null)
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        // Distinct members who endorsed this action. The ORIGIN counts: it is
        // a verified signature by a member, and `min` names how many people
        // must agree, not how many must agree *with* the proposer. Excluding
        // it would make `min: 2` a three-person rule, which is neither what
        // "the two-person rule" means nor what a two-member group can ever
        // satisfy.
        //
        // Distinctness is what carries the weight: one node cannot reach a
        // quorum of 2 by co-signing its own action, because its origin and
        // its co-signature share an origin id.
        let mut seen: HashSet<&str> = HashSet::new();
        if members.contains(&action.origin) {
            seen.insert(action.origin.as_str());
        }
        for cs in &action.cosig {
            if members.contains(&cs.origin) {
                seen.insert(cs.origin.as_str());
            }
        }
        let endorsers = seen.len();
        if endorsers < spec.min {
            Some(format!(
                "quorum not met: {endorsers} distinct member(s) of '{group_name}' have signed this action (need {}). \
                 The proposer counts as one; {} more distinct member(s) must co-sign it. \
                 A single node cannot reach a quorum of {} by itself.",
                spec.min,
                spec.min.saturating_sub(endorsers),
                spec.min
            ))
        } else {
            None
        }
    }
}

/// Reduce an action through its model into per-field ops and apply them to
/// the entry (the per-field LWW merge in [`apply_op`]). Stamps the reduced
/// ops with the action's identity so the field map carries correct
/// provenance. Returns `(any_applied, deleted)`.
/// The canonical position of an action in its document's log.
///
/// Folded state must be a function of the *set* of actions a node holds, not
/// of the order they arrived in. `(ts, origin, hash)` is a total order every
/// node computes identically from the action itself, so every node folds the
/// same log the same way. The hash breaks ties between two origins that used
/// the same timestamp.
fn canon_key(a: &Action) -> (u64, String, Hash32) {
    (a.ts, a.origin.clone(), a.hash())
}

fn apply_ops_to_entry(entry: &mut Entry, model: &dyn Model, action: &Action) -> (bool, bool) {
    let mut ops = match model.reduce(&entry.doc, action) {
        Ok(o) => o,
        Err(r) => {
            warn!("reduce failed for {}: {}", action.doc_id, r.describe());
            return (false, entry.deleted);
        }
    };
    for op in &mut ops {
        op.doc_id = action.doc_id;
        op.ts = action.ts;
        op.clock = action.clock.clone();
        op.origin = action.origin.clone();
        op.sig = action.sig;
    }
    let mut applied_any = false;
    let mut deleted = entry.deleted;
    for op in &ops {
        let r = apply_op(&mut entry.doc, &mut entry.clock, &mut deleted, op);
        if r.applied {
            applied_any = true;
        }
    }
    entry.deleted = deleted;
    lift_name(&mut entry.doc);
    (applied_any, deleted)
}

/// Append one action to the doc's WAL file (`.alog`), one JSON line each.
fn persist_action(docs_dir: &Path, action: &Action) -> std::io::Result<()> {
    let path = docs_dir.join(format!("{}.alog", action.doc_id));
    let mut line = serde_json::to_string(action)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    line.push('\n');
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    f.write_all(line.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::CoSig;
    use ed25519_dalek::Signer;
    use std::collections::BTreeMap as BM;

    fn identity(seed: u8) -> SigningKey {
        let mut bytes = [seed; 32];
        bytes[0] = 0;
        SigningKey::from_bytes(&bytes)
    }

    fn open_store(dir: &Path) -> Arc<Store> {
        Store::open(dir, &identity(9), "test-origin").expect("store opens")
    }

    #[test]
    fn tofu_pinning_rejects_key_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let s = open_store(dir.path());
        let origin = "peer-x";
        let good = identity(11);
        let evil = identity(12);

        // First contact pins the key; re-pinning the same key is idempotent.
        s.register_peer_key(origin, good.verifying_key().to_bytes())
            .unwrap();
        s.register_peer_key(origin, good.verifying_key().to_bytes())
            .unwrap();

        // A different key for the same origin is a TOFU mismatch (rejected,
        // and does not replace the pinned key).
        let err = s
            .register_peer_key(origin, evil.verifying_key().to_bytes())
            .unwrap_err();
        assert!(err.contains("TOFU"), "mismatch should mention TOFU: {err}");

        // The pinned (good) key still wins: re-registering it succeeds, and
        // the evil key is still rejected (it never got pinned).
        s.register_peer_key(origin, good.verifying_key().to_bytes())
            .unwrap();
        s.register_peer_key(origin, evil.verifying_key().to_bytes())
            .unwrap_err();
    }

    /// Build a signed open@1 `set` action from `origin` with `key`.
    fn make_set_action(
        id: DocId,
        origin: &str,
        key: &SigningKey,
        clock: VecClock,
        field: &str,
        value: &serde_json::Value,
        ts: u64,
    ) -> Action {
        let mut a = Action {
            doc_id: id,
            model: open_ref(),
            kind: "set".into(),
            payload: serde_json::json!({ "field": field, "value": value }),
            ts,
            clock,
            origin: origin.into(),
            cosig: vec![],
            sig: [0; 64],
            prev_hash: None,
            space: None,
        };
        a.sign(key);
        a
    }

    #[allow(clippy::too_many_arguments)]
    /// Build a signed `group` action with co-signers. Each co-signer signs
    /// the same canonical message (the co-signatures are excluded from it).
    fn make_group_action(
        id: DocId,
        kind: &str,
        payload: serde_json::Value,
        origin: &str,
        key: &SigningKey,
        cosigners: &[(String, &SigningKey)],
        clock: VecClock,
        ts: u64,
    ) -> Action {
        let mut a = Action {
            doc_id: id,
            model: ModelRef::new("group", "1"),
            kind: kind.into(),
            payload,
            ts,
            clock,
            origin: origin.into(),
            cosig: cosigners
                .iter()
                .map(|(o, _)| CoSig {
                    origin: o.clone(),
                    sig: [0; 64],
                })
                .collect(),
            sig: [0; 64],
            prev_hash: None,
            space: None,
        };
        let mb = a.message_bytes();
        a.sig = key.sign(&mb).to_bytes();
        for (i, (_, ck)) in cosigners.iter().enumerate() {
            a.cosig[i].sig = ck.sign(&mb).to_bytes();
        }
        a
    }

    /// A single-origin clock at counter `n` (empty when `n == 0`).
    fn clock1(origin: &str, n: u64) -> VecClock {
        let mut c = VecClock::default();
        for _ in 0..n {
            c.tick(origin);
        }
        c
    }

    #[test]
    fn doc_change_carries_the_action_delta() {
        let dir = tempfile::tempdir().unwrap();
        let s = open_store(dir.path());
        let mut rx = s.subscribe_changes();

        s.create_doc("note-delta", BM::new()).unwrap();
        s.update_field("note-delta", "status", serde_json::json!("accepted"))
            .unwrap();

        // The feed carries both changes; the last is the `set`.
        let mut last: Option<DocChange> = None;
        while let Ok(c) = rx.try_recv() {
            last = Some(c);
        }
        let c = last.expect("a change was emitted");
        assert_eq!(c.name, "note-delta");
        assert_eq!(c.action_kind.as_deref(), Some("set"));
        assert_eq!(c.action_field.as_deref(), Some("status"));
        assert_eq!(
            c.action_value.as_ref(),
            Some(&serde_json::json!("accepted"))
        );
    }

    #[test]
    fn create_read_update_delete_field() {
        let dir = tempfile::tempdir().unwrap();
        let s = open_store(dir.path());
        let mut fields: BM<String, serde_json::Value> = BM::new();
        fields.insert("title".into(), "hello".into());
        let id = s.create_doc("note-1", fields).unwrap();
        assert_eq!(s.get("note-1").unwrap().fields["title"].value, "hello");
        assert!(!s.get("note-1").unwrap().fields.contains_key("__name__"));
        assert_eq!(s.list().len(), 1);

        s.update_field("note-1", "body", "world".into()).unwrap();
        assert_eq!(s.get("note-1").unwrap().fields["body"].value, "world");

        s.delete_field("note-1", "body").unwrap();
        assert!(!s.get("note-1").unwrap().fields.contains_key("body"));

        // duplicate name rejected
        assert!(s.create_doc("note-1", BM::new()).is_err());
        // missing doc errors
        assert!(s.update_field("nope", "f", 1i64.into()).is_err());

        s.delete_doc("note-1").unwrap();
        assert!(s.get("note-1").is_none());
        // name is freed: recreating gets a new id
        let id2 = s.create_doc("note-1", BM::new()).unwrap();
        assert_ne!(id, id2);
        assert_eq!(s.doc_count(), 1);
        drop(s);
    }

    #[test]
    fn name_validation() {
        assert!(Store::validate_name("ok-name_1.x").is_ok());
        assert!(Store::validate_name("").is_err());
        assert!(Store::validate_name(&"a".repeat(65)).is_err());
        assert!(Store::validate_name(".hidden").is_err());
        assert!(Store::validate_name("a/b").is_err());
        assert!(Store::validate_name("a b").is_err());
    }

    #[test]
    fn crash_recovery_replays_log() {
        let dir = tempfile::tempdir().unwrap();
        let s = open_store(dir.path());
        let mut fields: BM<String, serde_json::Value> = BM::new();
        fields.insert("a".into(), 1i64.into());
        fields.insert("b".into(), 2i64.into());
        s.create_doc("persist", fields).unwrap();
        s.update_field("persist", "a", 99i64.into()).unwrap();
        drop(s);

        // reopen: replay must reproduce the state
        let s2 = open_store(dir.path());
        let doc = s2.get("persist").unwrap();
        assert_eq!(doc.name, "persist");
        assert_eq!(doc.fields["a"].value, 99);
        assert_eq!(doc.fields["b"].value, 2);
        assert_eq!(s2.doc_count(), 1);
    }

    /// A doc created under a specific model (via its `init` reducer) must
    /// replay with *that* model after a restart, not the `open@1` default an
    /// unsnapshotted entry starts with. Regression: `replay_doc` reduced an
    /// action's ops but never set `entry.model`, so a `group` created before
    /// its first snapshot came back as an `open` doc and dropped out of the
    /// group projection.
    #[test]
    fn replay_preserves_model_of_model_created_doc() {
        let dir = tempfile::tempdir().unwrap();
        let s = open_store(dir.path());
        let group_ref = ModelRef::parse("group@1").unwrap();
        s.create_doc_model(
            "devs",
            &group_ref,
            &serde_json::json!({
                "name": "devs",
                "members": ["alice", "bob"],
                "managers": ["carol"],
            }),
        )
        .unwrap();
        drop(s);

        let s2 = open_store(dir.path());
        let id = s2.get("devs").expect("the group survives a restart").id;
        let state = s2.full_state(id).expect("the group has a full state");
        assert_eq!(
            state.model.name, "group",
            "replay must preserve the group model"
        );
        assert_eq!(state.model.version, "1");
        assert_eq!(
            state.doc.fields["members"].value,
            serde_json::json!(["alice", "bob"])
        );
        assert_eq!(s2.doc_count(), 1);
    }

    #[test]
    fn snapshot_boundary_and_replay() {
        let dir = tempfile::tempdir().unwrap();
        let s = open_store(dir.path());
        s.create_doc("big", BM::new()).unwrap();
        // push enough actions to cross the snapshot threshold
        for i in 0..(SNAPSHOT_OPS + 50) {
            s.update_field("big", &format!("f{i}"), i.into()).unwrap();
        }
        let snap = dir
            .path()
            .join(format!("{}.snap", s.get("big").unwrap().id));
        assert!(snap.exists(), "snapshot should have been written");
        drop(s);

        let s2 = open_store(dir.path());
        let doc = s2.get("big").unwrap();
        assert_eq!(doc.fields.len() as u64, SNAPSHOT_OPS + 50);
        assert_eq!(doc.fields["f5"].value, 5);
        drop(s2);
    }

    #[test]
    fn remote_action_applies_and_dedups() {
        let dir = tempfile::tempdir().unwrap();
        let s = open_store(dir.path());
        // a remote peer (different origin) signs actions
        let remote_key = identity(7);
        let remote_origin = "remote-peer";
        s.register_peer_key(remote_origin, remote_key.verifying_key().to_bytes())
            .unwrap();

        let id = s.create_doc("shared", BM::new()).unwrap();
        let entry_clock0 = s.summary()[&id].clone();

        let mut clock = entry_clock0.clone();
        clock.tick(remote_origin);
        let action = make_set_action(
            id,
            remote_origin,
            &remote_key,
            clock,
            "rf",
            &"rv".into(),
            1_000_000,
        );
        let r1 = s.apply_remote_action(&action).unwrap();
        assert!(r1.applied);
        assert_eq!(s.get("shared").unwrap().fields["rf"].value, "rv");

        // duplicate delivery is a no-op
        let r2 = s.apply_remote_action(&action).unwrap();
        assert!(!r2.applied);
        assert_eq!(s.get("shared").unwrap().fields["rf"].value, "rv");
    }

    #[test]
    fn bad_signature_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        let s = open_store(dir.path());
        let rogue = identity(3);
        s.register_peer_key("rogue", rogue.verifying_key().to_bytes())
            .unwrap();
        let id = s.create_doc("t", BM::new()).unwrap();
        let mut clock = s.summary()[&id].clone();
        clock.tick("rogue");
        // build with the registered key, then re-sign with a *different* key
        let mut action = make_set_action(id, "rogue", &rogue, clock, "k", &1i64.into(), 42);
        action.sign(&identity(4));
        assert!(s.apply_remote_action(&action).is_err());
        assert_eq!(s.quarantined_count(), 1);
        assert!(!s.get("t").unwrap().fields.contains_key("k"));
        // unknown origin also rejected
        let mut action2 = action.clone();
        action2.origin = "ghost".into();
        assert!(s.apply_remote_action(&action2).is_err());
    }

    #[test]
    fn outbound_drain_carries_local_actions_only() {
        let dir = tempfile::tempdir().unwrap();
        let s = open_store(dir.path());
        s.create_doc("o", BM::new()).unwrap();
        let actions = s.drain_outbound();
        assert!(!actions.is_empty());
        assert!(actions.iter().all(|a| a.origin == s.origin()));
        assert!(s.drain_outbound().is_empty());
    }

    #[test]
    fn outbound_feed_delivers_local_actions_immediately_not_remote() {
        let dir = tempfile::tempdir().unwrap();
        let s = open_store(dir.path());
        let mut rx = s.connect_outbound();

        // A local action arrives on the feed as soon as it is applied.
        s.create_doc("fed", BM::new()).unwrap();
        let a1 = rx.try_recv().expect("create action on feed");
        assert_eq!(a1.origin, s.origin());
        s.update_field("fed", "x", 1i64.into()).unwrap();
        let a2 = rx.try_recv().expect("update action on feed");
        assert_eq!(a2.origin, s.origin());
        assert_ne!(a1.hash(), a2.hash());

        // A remote action does not enter the feed (not re-gossiped).
        let remote_key = identity(7);
        let remote_origin = "remote-peer";
        s.register_peer_key(remote_origin, remote_key.verifying_key().to_bytes())
            .unwrap();
        let id = s
            .doc_ids()
            .into_iter()
            .find(|id| s.doc_name(*id) == "fed")
            .unwrap();
        let mut clock = s.summary()[&id].clone();
        clock.tick(remote_origin);
        let action = make_set_action(
            id,
            remote_origin,
            &remote_key,
            clock,
            "rf",
            &"rv".into(),
            2_000_000,
        );
        s.apply_remote_action(&action).unwrap();
        assert!(
            rx.try_recv().is_err(),
            "remote actions must not enter the outbound feed"
        );
        // ...but the periodic drain still carries exactly the local set.
        let drained = s.drain_outbound();
        assert_eq!(drained.len(), 2);
    }

    #[test]
    fn ts_strictly_increases_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let s = open_store(dir.path());
        s.create_doc("t", BM::new()).unwrap();
        let ts1 = s.drain_outbound().into_iter().map(|a| a.ts).max().unwrap();
        drop(s);
        let s2 = open_store(dir.path());
        s2.update_field("t", "x", 1i64.into()).unwrap();
        let ts2 = s2.drain_outbound().into_iter().map(|a| a.ts).max().unwrap();
        assert!(
            ts2 > ts1,
            "ts must not go backwards across restart ({ts1} -> {ts2})"
        );
    }

    #[test]
    fn group_add_manager_two_person_rule() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = open_store(dir.path());
        let k_alice = identity(1);
        let k_bob = identity(2);
        let k_carol = identity(3);
        let k_mallory = identity(4);
        for (name, k) in [
            ("alice", &k_alice),
            ("bob", &k_bob),
            ("carol", &k_carol),
            ("mallory", &k_mallory),
        ] {
            s.register_peer_key(name, k.verifying_key().to_bytes())
                .unwrap();
        }

        // Bootstrap: members [alice bob carol], managers [alice].
        let id = DocId::new();
        let init = make_group_action(
            id,
            "init",
            serde_json::json!({
                "name": "core",
                "members": ["alice", "bob", "carol"],
                "managers": ["alice"],
            }),
            "alice",
            &k_alice,
            &[],
            clock1("alice", 1),
            1000,
        );
        assert!(s.apply_remote_action(&init).is_ok(), "init applies");
        let g = s.get("core").expect("group doc created");
        assert_eq!(
            g.fields.get("members").expect("members").value,
            serde_json::json!(["alice", "bob", "carol"])
        );

        // Two distinct member co-signers meet the quorum -> manager added.
        let ok = make_group_action(
            id,
            "add-manager",
            serde_json::json!({ "member": "dave" }),
            "alice",
            &k_alice,
            &[("bob".into(), &k_bob), ("carol".into(), &k_carol)],
            clock1("alice", 2),
            2000,
        );
        assert!(s.apply_remote_action(&ok).is_ok(), "quorum met");
        let managers = s
            .get("core")
            .unwrap()
            .fields
            .get("managers")
            .unwrap()
            .value
            .as_array()
            .cloned()
            .unwrap();
        assert!(managers.iter().any(|v| v == &serde_json::json!("dave")));

        // The proposer counts toward the quorum, so one co-signer is enough:
        // alice proposed and bob co-signed -- two distinct people agreed,
        // which is what "the two-person rule" names.
        let one = make_group_action(
            id,
            "add-manager",
            serde_json::json!({ "member": "eve" }),
            "alice",
            &k_alice,
            &[("bob".into(), &k_bob)],
            clock1("alice", 3),
            3000,
        );
        assert!(
            s.apply_remote_action(&one).is_ok(),
            "proposer + one co-signer = two distinct members"
        );

        // A proposer alone cannot reach a quorum of 2.
        let alone = make_group_action(
            id,
            "add-manager",
            serde_json::json!({ "member": "frank" }),
            "alice",
            &k_alice,
            &[],
            clock1("alice", 4),
            4000,
        );
        assert!(
            s.apply_remote_action(&alone).is_err(),
            "a lone proposer must not satisfy a quorum of 2"
        );

        // Nor by co-signing their own action: origin and co-signer share an
        // id, so they collapse to one distinct endorser. This is the property
        // that keeps a single node from promoting itself.
        let selfsign = make_group_action(
            id,
            "add-manager",
            serde_json::json!({ "member": "frank" }),
            "alice",
            &k_alice,
            &[("alice".into(), &k_alice)],
            clock1("alice", 5),
            5000,
        );
        assert!(
            s.apply_remote_action(&selfsign).is_err(),
            "a node must not reach a quorum by co-signing itself"
        );

        // Duplicate co-signers collapse: bob twice is still one person, so
        // with alice that is 2 -- which now MEETS the quorum. Distinctness is
        // enforced, and alice+bob is a legitimate pair regardless of how many
        // times bob signs.
        let dup = make_group_action(
            id,
            "add-manager",
            serde_json::json!({ "member": "eve" }),
            "alice",
            &k_alice,
            &[("bob".into(), &k_bob), ("bob".into(), &k_bob)],
            clock1("alice", 6),
            6000,
        );
        assert!(
            s.apply_remote_action(&dup).is_ok(),
            "alice + bob (however many times bob signs) is still two people"
        );

        // An outsider does not count: alice + mallory is one member, not two.
        let outsider = make_group_action(
            id,
            "add-manager",
            serde_json::json!({ "member": "eve" }),
            "alice",
            &k_alice,
            &[("mallory".into(), &k_mallory)],
            clock1("alice", 7),
            7000,
        );
        assert!(
            s.apply_remote_action(&outsider).is_err(),
            "a non-member co-signer must not count toward the quorum"
        );

        // "frank" was only ever attempted by actions that must be rejected
        // (a lone proposer, and a proposer co-signing itself), so its absence
        // proves those rejections did not mutate the group.
        let members = s
            .get("core")
            .unwrap()
            .fields
            .get("members")
            .unwrap()
            .value
            .as_array()
            .cloned()
            .unwrap();
        assert!(
            !members.iter().any(|v| v == &serde_json::json!("frank")),
            "rejected actions must not mutate the group"
        );

        // Tamper: a wrong-key origin signature is rejected and quarantined.
        let mut tampered = make_group_action(
            id,
            "add-manager",
            serde_json::json!({ "member": "eve" }),
            "alice",
            &k_alice,
            &[("bob".into(), &k_bob), ("carol".into(), &k_carol)],
            clock1("alice", 8),
            8000,
        );
        tampered.sig = k_bob.sign(&tampered.message_bytes()).to_bytes();
        assert!(
            s.apply_remote_action(&tampered).is_err(),
            "a wrong-key signature is rejected"
        );
        assert!(
            s.quarantined_count() > 0,
            "rejections are counted as quarantined"
        );
    }

    #[test]
    fn verify_valid_doc_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let s = open_store(dir.path());
        let mut fields: BM<String, serde_json::Value> = BM::new();
        fields.insert("title".into(), "hello".into());
        s.create_doc("v1", fields).unwrap();
        s.update_field("v1", "body", serde_json::json!("world"))
            .unwrap();
        s.update_field("v1", "body", serde_json::json!("world2"))
            .unwrap();
        let report = s.verify("v1").unwrap();
        assert!(
            report.ok,
            "a valid log verifies clean: {:?}",
            report.actions
        );
        assert_eq!(report.actions.len(), 4, "create + set + two updates");
        assert!(report.actions.iter().all(|a| a.problems.is_empty()));
        assert!(!report.from_snapshot, "a small doc has no snapshot");
        let rendered = report.render();
        assert!(rendered.contains("verify v1"));
        assert!(rendered.contains("VERIFIED"));
    }

    #[test]
    fn verify_cosigned_action_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let s = open_store(dir.path());
        let k_alice = identity(1);
        let k_bob = identity(2);
        let k_carol = identity(3);
        for (name, k) in [("alice", &k_alice), ("bob", &k_bob), ("carol", &k_carol)] {
            s.register_peer_key(name, k.verifying_key().to_bytes())
                .unwrap();
        }
        let id = DocId::new();
        let init = make_group_action(
            id,
            "init",
            serde_json::json!({
                "name": "core",
                "members": ["alice", "bob", "carol"],
                "managers": ["alice"],
            }),
            "alice",
            &k_alice,
            &[],
            clock1("alice", 1),
            1000,
        );
        s.apply_remote_action(&init).unwrap();
        let mut add = make_group_action(
            id,
            "add-manager",
            serde_json::json!({ "member": "bob" }),
            "alice",
            &k_alice,
            &[("bob".into(), &k_bob), ("carol".into(), &k_carol)],
            clock1("alice", 2),
            2000,
        );
        // Chain to the init action and re-sign: prev_hash is part of the
        // signed message, so the origin and every co-signer must re-sign.
        add.prev_hash = Some(init.hash());
        let mb = add.message_bytes();
        add.sig = k_alice.sign(&mb).to_bytes();
        add.cosig[0].sig = k_bob.sign(&mb).to_bytes();
        add.cosig[1].sig = k_carol.sign(&mb).to_bytes();
        s.apply_remote_action(&add).unwrap();
        let report = s.verify("core").unwrap();
        assert!(
            report.ok,
            "a cosigned doc verifies clean: {:?}",
            report.actions
        );
        let add = report
            .actions
            .iter()
            .find(|a| a.kind == "add-manager")
            .unwrap();
        assert!(add.ok, "the co-signatures check out: {:?}", add.problems);
        let m = report.model.as_ref().expect("the model is reported");
        assert_eq!(m.name, "group");
    }

    #[test]
    fn verify_flags_tampered_signature() {
        let dir = tempfile::tempdir().unwrap();
        {
            let s = open_store(dir.path());
            let mut fields: BM<String, serde_json::Value> = BM::new();
            fields.insert("title".into(), "hello".into());
            s.create_doc("v1", fields).unwrap();
            let alog = s.wal_path(&s.get("v1").unwrap().id);
            let mut lines: Vec<Action> = std::fs::read_to_string(&alog)
                .unwrap()
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            lines[0].sig[0] ^= 0xff; // corrupt the genesis action's signature
            let mut out = String::new();
            for a in &lines {
                out.push_str(&serde_json::to_string(a).unwrap());
                out.push('\n');
            }
            std::fs::write(&alog, out).unwrap();
        }
        // The open-time replay re-reduces the log but trusts its signatures;
        // `verify` is the independent audit that catches the corruption.
        let s = open_store(dir.path());
        let report = s.verify("v1").unwrap();
        assert!(!report.ok, "a tampered signature is flagged");
        assert!(
            report
                .actions
                .iter()
                .any(|a| a.problems.iter().any(|p| p.contains("signature"))),
            "the bad signature is named: {:?}",
            report.actions
        );
    }

    #[test]
    fn verify_flags_broken_chain() {
        let dir = tempfile::tempdir().unwrap();
        {
            let s = open_store(dir.path());
            let mut fields: BM<String, serde_json::Value> = BM::new();
            fields.insert("title".into(), "hello".into());
            s.create_doc("v1", fields).unwrap();
            s.update_field("v1", "body", serde_json::json!("world"))
                .unwrap();
            let alog = s.wal_path(&s.get("v1").unwrap().id);
            let mut lines: Vec<Action> = std::fs::read_to_string(&alog)
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            // Break the chain: the second action's prev_hash no longer
            // matches the first action's content hash. Re-sign so the
            // signature is still valid - only the chain is broken.
            lines[1].prev_hash = Some(Hash32::of(b"tampered"));
            lines[1].sign(&identity(9));
            let mut out = String::new();
            for a in &lines {
                out.push_str(&serde_json::to_string(a).unwrap());
                out.push('\n');
            }
            std::fs::write(&alog, out).unwrap();
        }
        let s = open_store(dir.path());
        let report = s.verify("v1").unwrap();
        assert!(!report.ok, "a broken chain is flagged");
        assert!(
            report
                .actions
                .iter()
                .any(|a| a.problems.iter().any(|p| p.contains("chain"))),
            "the chain break is named: {:?}",
            report.actions
        );
    }

    /// Two members post at the same time, each on their own node, then the
    /// nodes exchange the actions. Both messages must survive and both nodes
    /// must agree.
    ///
    /// They do not today: `append` reduces to a *whole-array* write, and the
    /// field merges last-writer-wins, so the node that applies the later-`ts`
    /// action last keeps only its own array. The nodes diverge permanently.
    #[test]
    fn concurrent_posts_converge_and_keep_both_messages() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let a = open_store(dir_a.path());
        let b = open_store(dir_b.path());

        let k_alice = identity(1);
        let k_bob = identity(2);
        for s in [&a, &b] {
            s.register_peer_key("alice", k_alice.verifying_key().to_bytes())
                .unwrap();
            s.register_peer_key("bob", k_bob.verifying_key().to_bytes())
                .unwrap();
        }

        let id = DocId::new();
        let init = make_group_action(
            id,
            "init",
            serde_json::json!({
                "name": "core",
                "members": ["alice", "bob"],
                "managers": ["alice"],
            }),
            "alice",
            &k_alice,
            &[],
            clock1("alice", 1),
            1000,
        );
        a.apply_remote_action(&init).unwrap();
        b.apply_remote_action(&init).unwrap();

        // Alice's clock knows only her own work; Bob's knows the init plus
        // his own. Neither dominates the other: these are concurrent.
        let from_alice = make_group_action(
            id,
            "post",
            serde_json::json!({ "text": "from alice", "channel": "general" }),
            "alice",
            &k_alice,
            &[],
            clock1("alice", 2),
            2000,
        );
        let mut bob_clock = clock1("alice", 1);
        bob_clock.tick("bob");
        let from_bob = make_group_action(
            id,
            "post",
            serde_json::json!({ "text": "from bob", "channel": "general" }),
            "bob",
            &k_bob,
            &[],
            bob_clock,
            2001,
        );

        // Each node sees its own author first, then the other's.
        a.apply_remote_action(&from_alice).unwrap();
        b.apply_remote_action(&from_bob).unwrap();
        a.apply_remote_action(&from_bob).unwrap();
        b.apply_remote_action(&from_alice).unwrap();

        let texts = |s: &Arc<Store>| -> Vec<String> {
            s.get("core")
                .unwrap()
                .fields
                .get("msg_text")
                .and_then(|f| f.value.as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        };
        let (ta, tb) = (texts(&a), texts(&b));
        assert_eq!(ta, tb, "the two nodes must converge: {ta:?} vs {tb:?}");
        assert_eq!(ta.len(), 2, "neither message may be lost: {ta:?}");
    }

    /// A manager removes Bob while another manager concurrently adds Dave.
    /// Both intents must survive. Today one whole-array write wins outright,
    /// so Bob is silently restored to `members`.
    #[test]
    fn a_concurrent_add_must_not_resurrect_a_removed_member() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let a = open_store(dir_a.path());
        let b = open_store(dir_b.path());

        let k_alice = identity(1);
        let k_carol = identity(3);
        for s in [&a, &b] {
            s.register_peer_key("alice", k_alice.verifying_key().to_bytes())
                .unwrap();
            s.register_peer_key("carol", k_carol.verifying_key().to_bytes())
                .unwrap();
        }

        let id = DocId::new();
        let init = make_group_action(
            id,
            "init",
            serde_json::json!({
                "name": "core",
                "members": ["alice", "bob", "carol"],
                "managers": ["alice", "carol"],
            }),
            "alice",
            &k_alice,
            &[],
            clock1("alice", 1),
            1000,
        );
        a.apply_remote_action(&init).unwrap();
        b.apply_remote_action(&init).unwrap();

        let remove_bob = make_group_action(
            id,
            "remove-member",
            serde_json::json!({ "member": "bob" }),
            "alice",
            &k_alice,
            &[],
            clock1("alice", 2),
            2000,
        );
        let mut carol_clock = clock1("alice", 1);
        carol_clock.tick("carol");
        let add_dave = make_group_action(
            id,
            "add-member",
            serde_json::json!({ "member": "dave" }),
            "carol",
            &k_carol,
            &[],
            carol_clock,
            2001,
        );

        a.apply_remote_action(&remove_bob).unwrap();
        b.apply_remote_action(&add_dave).unwrap();
        a.apply_remote_action(&add_dave).unwrap();
        b.apply_remote_action(&remove_bob).unwrap();

        let members = |s: &Arc<Store>| -> Vec<String> {
            s.get("core")
                .unwrap()
                .fields
                .get("members")
                .and_then(|f| f.value.as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        };
        let (ma, mb) = (members(&a), members(&b));
        assert_eq!(ma, mb, "the two nodes must converge: {ma:?} vs {mb:?}");
        assert!(!ma.contains(&"bob".to_string()), "bob was removed: {ma:?}");
        assert!(ma.contains(&"dave".to_string()), "dave was added: {ma:?}");
    }

    /// Concurrent writers make the action log a DAG: two actions legitimately
    /// carry the same `prev_hash`. `doc verify` walked the log as a straight
    /// line, so any document that ever took a concurrent write reported
    /// "hash chain broken" — the store's own audit calling honest data
    /// tampered.
    #[test]
    fn a_document_with_concurrent_actions_still_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let s = open_store(dir.path());
        let k_alice = identity(1);
        let k_bob = identity(2);
        s.register_peer_key("alice", k_alice.verifying_key().to_bytes())
            .unwrap();
        s.register_peer_key("bob", k_bob.verifying_key().to_bytes())
            .unwrap();

        let id = DocId::new();
        let init = make_group_action(
            id,
            "init",
            serde_json::json!({
                "name": "core",
                "members": ["alice", "bob"],
                "managers": ["alice"],
            }),
            "alice",
            &k_alice,
            &[],
            clock1("alice", 1),
            1000,
        );
        s.apply_remote_action(&init).unwrap();

        // Both posts chain to the init action: neither saw the other.
        let mut from_alice = make_group_action(
            id,
            "post",
            serde_json::json!({ "text": "a", "channel": "general" }),
            "alice",
            &k_alice,
            &[],
            clock1("alice", 2),
            2000,
        );
        from_alice.prev_hash = Some(init.hash());
        let mb = from_alice.message_bytes();
        from_alice.sig = k_alice.sign(&mb).to_bytes();

        let mut bob_clock = clock1("alice", 1);
        bob_clock.tick("bob");
        let mut from_bob = make_group_action(
            id,
            "post",
            serde_json::json!({ "text": "b", "channel": "general" }),
            "bob",
            &k_bob,
            &[],
            bob_clock,
            2001,
        );
        from_bob.prev_hash = Some(init.hash());
        let mb = from_bob.message_bytes();
        from_bob.sig = k_bob.sign(&mb).to_bytes();

        s.apply_remote_action(&from_alice).unwrap();
        s.apply_remote_action(&from_bob).unwrap();

        let report = s.verify("core").unwrap();
        assert!(
            report.ok,
            "two actions sharing a parent is a DAG, not a broken chain: {:?}",
            report
                .actions
                .iter()
                .map(|a| a.problems.clone())
                .collect::<Vec<_>>()
        );
    }

    /// A document's space is fixed by its first action. An action naming a
    /// different one is a document being dragged out of a protected space
    /// into a public one, and must not apply.
    #[test]
    fn a_document_cannot_change_space() {
        let dir = tempfile::tempdir().unwrap();
        let s = open_store(dir.path());
        let k = identity(1);
        s.register_peer_key("alice", k.verifying_key().to_bytes())
            .unwrap();

        let id = DocId::new();
        let home = DocId::new();
        let elsewhere = DocId::new();

        let mut init = make_group_action(
            id,
            "init",
            serde_json::json!({ "name": "core", "members": ["alice"], "managers": ["alice"] }),
            "alice",
            &k,
            &[],
            clock1("alice", 1),
            1000,
        );
        init.space = Some(home);
        let mb = init.message_bytes();
        init.sig = k.sign(&mb).to_bytes();
        s.apply_remote_action(&init).unwrap();

        let mut moved = make_group_action(
            id,
            "post",
            serde_json::json!({ "text": "hi", "channel": "general" }),
            "alice",
            &k,
            &[],
            clock1("alice", 2),
            2000,
        );
        moved.space = Some(elsewhere);
        let mb = moved.message_bytes();
        moved.sig = k.sign(&mb).to_bytes();

        let err = match s.apply_remote_action(&moved) {
            Err(e) => e,
            Ok(_) => panic!("a document must not be able to change space"),
        };
        assert!(
            err.contains("space"),
            "the rejection names the reason: {err}"
        );
    }

    // ---- spaces: the read gate -------------------------------------------

    /// Create a space document of the given visibility, plus one ordinary
    /// document that lives in it. Returns (space id, doc id).
    fn seed_space(
        s: &Arc<Store>,
        visibility: &str,
        members: &[&str],
        owner: &str,
        key: &SigningKey,
    ) -> (DocId, DocId) {
        let space_id = DocId::new();
        let mut init = Action {
            doc_id: space_id,
            model: ModelRef::new("space", "1"),
            kind: "init".into(),
            payload: serde_json::json!({
                "name": format!("space-{visibility}"),
                "visibility": visibility,
                "members": members,
                "managers": [owner],
            }),
            ts: 1000,
            clock: clock1(owner, 1),
            origin: owner.into(),
            cosig: Vec::new(),
            sig: [0; 64],
            prev_hash: None,
            // A space document lives in itself: that is what lets a member be
            // told they are a member without already being able to read it.
            space: Some(space_id),
        };
        let mb = init.message_bytes();
        init.sig = key.sign(&mb).to_bytes();
        s.apply_remote_action(&init).unwrap();

        let doc_id = DocId::new();
        let mut doc = Action {
            doc_id,
            model: ModelRef::new("group", "1"),
            kind: "init".into(),
            payload: serde_json::json!({
                "name": format!("doc-in-{visibility}"),
                "members": members,
                "managers": [owner],
            }),
            ts: 1001,
            clock: clock1(owner, 2),
            origin: owner.into(),
            cosig: Vec::new(),
            sig: [0; 64],
            prev_hash: None,
            space: Some(space_id),
        };
        let mb = doc.message_bytes();
        doc.sig = key.sign(&mb).to_bytes();
        s.apply_remote_action(&doc).unwrap();
        (space_id, doc_id)
    }

    fn space_store(dir: &Path) -> (Arc<Store>, SigningKey) {
        let s = open_store(dir);
        let k = identity(1);
        for who in ["alice", "bob", "mallory"] {
            s.register_peer_key(who, k.verifying_key().to_bytes()).ok();
        }
        // Distinct keys would be more faithful, but the read gate is about
        // membership, not signatures, and every action here is alice's.
        (s, k)
    }

    #[test]
    fn a_public_space_is_readable_by_anyone() {
        let dir = tempfile::tempdir().unwrap();
        let (s, k) = space_store(dir.path());
        let (_, doc) = seed_space(&s, "public", &["alice"], "alice", &k);
        assert!(s.may_peer_read("mallory", doc), "public means public");
    }

    #[test]
    fn a_protected_space_is_withheld_from_a_non_member() {
        let dir = tempfile::tempdir().unwrap();
        let (s, k) = space_store(dir.path());
        let (_, doc) = seed_space(&s, "protected", &["alice", "bob"], "alice", &k);
        assert!(s.may_peer_read("bob", doc), "a member reads it");
        assert!(
            !s.may_peer_read("mallory", doc),
            "a non-member must not be served a protected document"
        );
    }

    #[test]
    fn a_private_space_is_served_to_nobody() {
        let dir = tempfile::tempdir().unwrap();
        let (s, k) = space_store(dir.path());
        let (_, doc) = seed_space(&s, "private", &["alice"], "alice", &k);
        assert!(
            !s.may_peer_read("alice", doc),
            "private means it never leaves this node -- not even to its owner's peers"
        );
    }

    /// The ACL is circular without this: to be told you are a member you must
    /// already be able to read the document that says so.
    #[test]
    fn a_space_document_is_served_to_anyone_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let (s, k) = space_store(dir.path());
        let (space, _) = seed_space(&s, "protected", &["alice", "bob"], "alice", &k);
        assert!(
            s.may_peer_read("bob", space),
            "a member must be able to fetch the document that names them"
        );
        assert!(
            !s.may_peer_read("mallory", space),
            "and a stranger must not"
        );
    }

    /// Defaults must withhold, not disclose.
    #[test]
    fn a_document_whose_space_is_unknown_is_withheld() {
        let dir = tempfile::tempdir().unwrap();
        let (s, k) = space_store(dir.path());
        let orphan = DocId::new();
        let missing_space = DocId::new();
        let mut a = Action {
            doc_id: orphan,
            model: ModelRef::new("group", "1"),
            kind: "init".into(),
            payload: serde_json::json!({
                "name": "orphan", "members": ["alice"], "managers": ["alice"],
            }),
            ts: 1000,
            clock: clock1("alice", 1),
            origin: "alice".into(),
            cosig: Vec::new(),
            sig: [0; 64],
            prev_hash: None,
            space: Some(missing_space),
        };
        let mb = a.message_bytes();
        a.sig = k.sign(&mb).to_bytes();
        s.apply_remote_action(&a).unwrap();
        assert!(
            !s.may_peer_read("alice", orphan),
            "a space document that has not arrived means withhold, not serve"
        );
    }

    /// A document written before spaces existed replicates as it always did.
    #[test]
    fn a_document_with_no_space_still_replicates() {
        let dir = tempfile::tempdir().unwrap();
        let (s, k) = space_store(dir.path());
        let id = DocId::new();
        let init = make_group_action(
            id,
            "init",
            serde_json::json!({ "name": "legacy", "members": ["alice"], "managers": ["alice"] }),
            "alice",
            &k,
            &[],
            clock1("alice", 1),
            1000,
        );
        s.apply_remote_action(&init).unwrap();
        assert!(s.may_peer_read("mallory", id));
    }

    /// An unfiltered summary hands over the existence and name of every
    /// document on the node -- metadata a protected space must not give up.
    #[test]
    fn a_summary_hides_documents_the_peer_cannot_read() {
        let dir = tempfile::tempdir().unwrap();
        let (s, k) = space_store(dir.path());
        let (prot_space, prot_doc) = seed_space(&s, "protected", &["alice"], "alice", &k);

        let full = s.summary();
        assert!(full.contains_key(&prot_doc), "we hold it ourselves");

        let theirs = s.summary_for("mallory");
        assert!(
            !theirs.contains_key(&prot_doc),
            "a stranger must not learn the document exists"
        );
        assert!(
            !theirs.contains_key(&prot_space),
            "nor that the space exists"
        );

        let members = s.summary_for("alice");
        assert!(members.contains_key(&prot_doc), "a member still syncs it");
    }

    /// The point of a space: an app does not carry its own access list, it
    /// inherits the space's. A member may post to a channel in that space; a
    /// non-member may not -- and `chat@1` says nothing about either of them.
    #[test]
    fn an_app_inherits_its_spaces_membership() {
        let dir = tempfile::tempdir().unwrap();
        let (s, k) = space_store(dir.path());
        let (space_id, _) = seed_space(&s, "protected", &["alice", "bob"], "alice", &k);

        let chat_id = DocId::new();
        let mk = |kind: &str, payload: serde_json::Value, who: &str, ts: u64, n: u64| {
            let mut a = Action {
                doc_id: chat_id,
                model: ModelRef::new("chat", "1"),
                kind: kind.into(),
                payload,
                ts,
                clock: clock1(who, n),
                origin: who.into(),
                cosig: Vec::new(),
                sig: [0; 64],
                prev_hash: None,
                space: Some(space_id),
            };
            let mb = a.message_bytes();
            a.sig = k.sign(&mb).to_bytes();
            a
        };

        s.apply_remote_action(&mk(
            "init",
            serde_json::json!({ "name": "general", "channel": "general" }),
            "alice",
            2000,
            2,
        ))
        .expect("a member opens a channel");

        s.apply_remote_action(&mk(
            "post",
            serde_json::json!({ "text": "hello" }),
            "bob",
            2001,
            1,
        ))
        .expect("another member posts");

        let err = match s.apply_remote_action(&mk(
            "post",
            serde_json::json!({ "text": "let me in" }),
            "mallory",
            2002,
            1,
        )) {
            Err(e) => e,
            Ok(_) => panic!("a non-member must not be able to post"),
        };
        assert!(
            err.contains("members"),
            "the refusal names the space's list: {err}"
        );

        let texts: Vec<String> = s
            .get("general")
            .unwrap()
            .fields
            .get("msg_text")
            .and_then(|f| f.value.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        assert_eq!(texts, vec!["hello".to_string()]);
    }

    /// Refusing because we cannot check is recoverable; allowing because we
    /// cannot check is not.
    #[test]
    fn a_write_is_refused_while_the_space_document_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let (s, k) = space_store(dir.path());
        let absent = DocId::new();
        let chat_id = DocId::new();
        let mut a = Action {
            doc_id: chat_id,
            model: ModelRef::new("chat", "1"),
            kind: "init".into(),
            payload: serde_json::json!({ "name": "ghost", "channel": "general" }),
            ts: 2000,
            clock: clock1("alice", 1),
            origin: "alice".into(),
            cosig: Vec::new(),
            sig: [0; 64],
            prev_hash: None,
            space: Some(absent),
        };
        let mb = a.message_bytes();
        a.sig = k.sign(&mb).to_bytes();
        let err = match s.apply_remote_action(&a) {
            Err(e) => e,
            Ok(_) => panic!("must not apply against a space we do not have"),
        };
        assert!(err.contains("space"), "{err}");
    }
}
