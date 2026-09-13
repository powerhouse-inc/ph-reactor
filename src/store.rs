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
use tokio::sync::mpsc;
use serde::{Deserialize, Serialize};
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
    pub fn open(docs_dir: &Path, key: &SigningKey, origin: &str) -> Result<Arc<Self>, String> {
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
            quarantined: 0,
            models: ModelRegistry::seeded_with_builtins(),
            subscribers: Vec::new(),
        };
        inner
            .known_keys
            .insert(inner.origin.clone(), key.verifying_key().to_bytes());
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
        Store::validate_name(name)?;
        let mut inner = self.inner.lock();
        if inner.names.contains_key(name) {
            return Err(format!("a doc named {name} already exists"));
        }
        let id = DocId::new();
        inner.entries.insert(id, Entry::new(id));
        let action = inner.build_action(id, model, "init", payload)?;
        inner.apply_action(&action)?;
        Ok(id)
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
        inner.emit_change(state.doc.id, ts);
        Ok(())
    }

    /// Take the queued local actions (the sync layer publishes them to the
    /// mesh). Remote actions are not re-gossiped.
    pub fn drain_outbound(&self) -> Vec<Action> {
        let mut inner = self.inner.lock();
        std::mem::take(&mut inner.outbound)
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
            model: open_ref(),
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
            },
            None => Entry::new(id),
        };
        lift_name(&mut scratch.doc);
        let mut expected_prev = base.as_ref().and_then(|b| b.log_hash);

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

            // Hash chain.
            match (action.prev_hash, expected_prev) {
                (Some(got), Some(want)) if got != want => {
                    problems.push(format!(
                        "hash chain broken: prev_hash {got} != expected {want}"
                    ));
                }
                (Some(_), None) => {
                    problems.push("action carries a prev_hash but the chain starts here".into());
                }
                (None, Some(_)) => problems.push("chain gap: missing prev_hash".into()),
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

            expected_prev = Some(action.hash());
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
        let mut action = Action {
            doc_id: id,
            model: model.clone(),
            kind: kind.to_string(),
            payload: payload.clone(),
            ts,
            clock,
            origin: self.origin.clone(),
            cosig: Vec::new(),
            sig: [0; 64],
            prev_hash,
        };
        action.sign(&self.key);
        Ok(action)
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
        // 4. Precondition + reduce + per-field merge (one entries borrow).
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
            let (a, d) = apply_ops_to_entry(entry, model.as_ref(), action);
            entry.model = action.model.clone();
            entry.log.push(action.clone());
            (a, d)
        };
        // 5. Durability: append the action to the WAL (the field map is
        //    derived from the log on replay).
        if let Err(e) = persist_action(&self.docs_dir, action) {
            warn!("WAL write failed for {}: {e}", action.doc_id);
        }
        if applied_any {
            self.outbound.push(action.clone());
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
            self.emit_change(action.doc_id, action.ts);
        }
        Ok(ApplyResult {
            applied: applied_any,
            doc_deleted: deleted,
        })
    }

    /// Publish a doc-change event to every subscriber (non-blocking; dead
    /// senders are pruned). Called after a successful apply or adoption.
    fn emit_change(&mut self, doc_id: DocId, ts: u64) {
        let Some(entry) = self.entries.get(&doc_id) else {
            return;
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
            },
            ts,
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
        let mut seen: HashSet<&str> = HashSet::new();
        let mut in_group = 0usize;
        for cs in &action.cosig {
            if seen.insert(cs.origin.as_str()) && members.contains(&cs.origin) {
                in_group += 1;
            }
        }
        if in_group < spec.min {
            Some(format!(
                "quorum: {in_group} of {} co-signers are members of '{group_name}' (need {})",
                action.cosig.len(),
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

        // One co-signer is below quorum.
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
            s.apply_remote_action(&one).is_err(),
            "one co-signer < quorum"
        );

        // Two co-signers but the same origin are not distinct.
        let dup = make_group_action(
            id,
            "add-manager",
            serde_json::json!({ "member": "eve" }),
            "alice",
            &k_alice,
            &[("bob".into(), &k_bob), ("bob".into(), &k_bob)],
            clock1("alice", 4),
            4000,
        );
        assert!(
            s.apply_remote_action(&dup).is_err(),
            "duplicate co-signers are not distinct"
        );

        // One member + one outsider: only one counts.
        let outsider = make_group_action(
            id,
            "add-manager",
            serde_json::json!({ "member": "eve" }),
            "alice",
            &k_alice,
            &[("bob".into(), &k_bob), ("mallory".into(), &k_mallory)],
            clock1("alice", 5),
            5000,
        );
        assert!(
            s.apply_remote_action(&outsider).is_err(),
            "an outsider co-signer does not count"
        );

        // The rejected actions never added eve.
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
            !members.iter().any(|v| v == &serde_json::json!("eve")),
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
            clock1("alice", 6),
            6000,
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
}
