//! Durable document store: an in-memory read model over per-document
//! append-only logs (WAL), with periodic snapshots.
//!
//! Persistence rules:
//! - every op (local or remote) is appended fsync'd to `<docs>/<id>.log`
//!   *before* it is applied to the read model. Applying is idempotent
//!   (unknown-work rule), so crash-replay is safe;
//! - when a doc's live log reaches [`SNAPSHOT_OPS`] ops, its full state
//!   is written atomically to `<docs>/<id>.snap` and the log is
//!   truncated;
//! - `<docs>/index.json` (name -> id, ts hint) is a hint, not truth:
//!   startup rebuilds it from the snapshot/log scan, so a lost write
//!   self-heals;
//! - ops that fail signature verification are quarantined
//!   (`<docs>/quarantine.log`) and never applied.
//!
//! The first op of a new doc carries the reserved field `__name__`;
//! the store lifts it into `doc.name` so user-facing field maps never
//! contain it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::doc::{apply_op, Doc, DocId, Op, Origin, VecClock, ApplyResult};

/// Ops in a doc's live log before it is snapshot + truncated.
pub const SNAPSHOT_OPS: u64 = 1024;
/// Maximum doc name length.
pub const MAX_NAME_LEN: usize = 64;
/// Reserved first-field key carrying the doc name.
const NAME_KEY: &str = "__name__";

/// A document's full durable state (used for snapshots and catch-up).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocState {
    pub doc: Doc,
    pub clock: VecClock,
    pub deleted: bool,
    /// Ops kept in the live log on top of this snapshot.
    #[serde(default)]
    pub log_ops: u64,
}

#[derive(Debug)]
struct Entry {
    doc: Doc,
    clock: VecClock,
    deleted: bool,
    /// Ops in the live log since the last snapshot (kept in memory so
    /// catch-up can serve them).
    log: Vec<Op>,
}

impl Entry {
    fn new(id: DocId) -> Self {
        Entry {
            doc: Doc { id, name: String::new(), fields: Default::default() },
            clock: VecClock::default(),
            deleted: false,
            log: Vec::new(),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Index {
    #[serde(default)]
    names: BTreeMap<String, DocId>,
    #[serde(default)]
    ts_hint: u64,
}

/// Lift the reserved [`NAME_KEY`] field into `doc.name`. Call after
/// applying an op that may carry the name.
fn lift_name(doc: &mut Doc) {
    if let Some(f) = doc.fields.remove(NAME_KEY) {
        if let Some(s) = f.value.as_str() {
            // only lift when we don't have a name yet: a remote
            // `__name__` op (name collision across peers) must not
            // stomp an established name
            if !s.is_empty() && doc.name.is_empty() {
                doc.name = s.to_string();
            }
        }
    }
}
/// The store. Wrap in `Arc`; all public methods take `&self` and lock
/// internally. Local and remote ops flow through the same apply path.
pub struct Store {
    inner: Mutex<Inner>,
}

struct Inner {
    docs_dir: PathBuf,
    key: SigningKey,
    /// Identity (peer id string) used as the origin of local ops.
    origin: Origin,
    /// Ed25519 public keys of valid signers: own identity plus every
    /// peer that completed a Hello handshake (peer id -> key bytes).
    known_keys: BTreeMap<Origin, [u8; 32]>,
    entries: BTreeMap<DocId, Entry>,
    names: BTreeMap<String, DocId>,
    ts_hint: u64,
    /// Local ops awaiting delivery to the sync layer.
    outbound: Vec<Op>,
    /// Ops quarantined for failing signature verification.
    quarantined: u64,
}

impl Store {
    /// Open (or create) a store rooted at `docs_dir`. Replays
    /// snapshots + logs and rebuilds the name index.
    pub fn open(
        docs_dir: &Path,
        key: &SigningKey,
        origin: &str,
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
            quarantined: 0,
        };
        inner
            .known_keys
            .insert(inner.origin.clone(), key.verifying_key().to_bytes());
        inner.replay();
        Ok(Arc::new(Store { inner: Mutex::new(inner) }))
    }

    pub fn origin(&self) -> String {
        self.inner.lock().origin.clone()
    }

    /// The identity signing key (for the p2p layer to sign/verify).
    pub fn key(&self) -> SigningKey {
        self.inner.lock().key.clone()
    }

    /// Register a remote peer's ed25519 public key (from its Hello
    /// handshake) so its ops verify.
    pub fn register_peer_key(&self, origin: &str, public_key: [u8; 32]) {
        self.inner
            .lock()
            .known_keys
            .insert(origin.to_string(), public_key);
    }

    // -- local writes ---------------------------------------------------

    /// Create a local doc with initial fields.
    pub fn create_doc(
        &self,
        name: &str,
        fields: BTreeMap<String, serde_json::Value>,
    ) -> Result<DocId, String> {
        Self::validate_name(name)?;
        let mut g = self.inner.lock();
        if g.names.contains_key(name) {
            return Err(format!("doc already exists: '{name}'"));
        }
        let id = DocId::new();
        let mut ops = vec![(Some(NAME_KEY.to_string()), Some(serde_json::json!(name)))];
        for (k, v) in fields {
            ops.push((Some(k), Some(v)));
        }
        g.write_ops(id, ops)?;
        // lift the name out of the field map into doc.name
        if let Some(e) = g.entries.get_mut(&id) {
            if e.doc.name.is_empty() {
                e.doc.name = name.to_string();
            }
        }
        g.names.insert(name.to_string(), id);
        g.persist_index();
        Ok(id)
    }

    /// Upsert one field on an existing local doc.
    pub fn update_field(
        &self,
        name: &str,
        field: &str,
        value: serde_json::Value,
    ) -> Result<(), String> {
        let mut g = self.inner.lock();
        let id = *g
            .names
            .get(name)
            .ok_or_else(|| format!("no such doc: '{name}'"))?;
        g.write_ops(id, vec![(Some(field.to_string()), Some(value))])?;
        Ok(())
    }

    /// Delete one field on an existing local doc.
    pub fn delete_field(&self, name: &str, field: &str) -> Result<(), String> {
        let mut g = self.inner.lock();
        let id = *g
            .names
            .get(name)
            .ok_or_else(|| format!("no such doc: '{name}'"))?;
        g.write_ops(id, vec![(Some(field.to_string()), None)])?;
        Ok(())
    }

    /// Delete a local doc. Terminal for this doc id: the name is freed
    /// and a later `create_doc` with the same name allocates a new id.
    pub fn delete_doc(&self, name: &str) -> Result<(), String> {
        let mut g = self.inner.lock();
        let id = *g
            .names
            .get(name)
            .ok_or_else(|| format!("no such doc: '{name}'"))?;
        g.write_ops(id, vec![(None, None)])?;
        g.names.remove(name);
        g.persist_index();
        Ok(())
    }

    // -- remote apply ---------------------------------------------------

    /// Apply a remote op (signature-verified against registered peer
    /// keys). Returns the apply result; errors on quarantine.
    pub fn apply_remote(&self, op: &Op) -> Result<ApplyResult, String> {
        let mut g = self.inner.lock();
        g.append_op(op)
    }

    // -- reads ------------------------------------------------------------

    pub fn get(&self, name: &str) -> Option<Doc> {
        let g = self.inner.lock();
        let id = *g.names.get(name)?;
        Some(g.entries.get(&id)?.doc.live())
    }

    /// All docs (by name, sorted), live docs only.
    pub fn list(&self) -> Vec<Doc> {
        let g = self.inner.lock();
        let mut out: Vec<Doc> = g
            .names
            .values()
            .filter_map(|id| g.entries.get(id).map(|e| e.doc.live()))
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn doc_count(&self) -> usize {
        self.inner.lock().names.len()
    }

    /// Live (non-deleted) doc count.
    pub fn live_doc_count(&self) -> usize {
        let g = self.inner.lock();
        g.names
            .values()
            .filter(|id| !g.entries.get(*id).map(|e| e.deleted).unwrap_or(true))
            .count()
    }

    pub fn quarantined_count(&self) -> u64 {
        self.inner.lock().quarantined
    }

    /// Local ops not yet delivered to the sync layer.
    pub fn drain_outbound(&self) -> Vec<Op> {
        self.inner.lock().outbound.drain(..).collect()
    }

    /// Per-doc clocks of live docs (for `Summary` reconciliation).
    pub fn summary(&self) -> BTreeMap<DocId, VecClock> {
        self.inner
            .lock()
            .entries
            .iter()
            .filter(|(_, e)| !e.deleted)
            .map(|(id, e)| (*id, e.clock.clone()))
            .collect()
    }

    /// Catch-up for a peer that knows `have`: the current state plus
    /// the live-log ops its clock does not cover.
    pub fn catch_up(&self, id: DocId, have: &VecClock) -> (Option<DocState>, Vec<Op>) {
        let g = self.inner.lock();
        let Some(e) = g.entries.get(&id) else {
            return (None, Vec::new());
        };
        let state = DocState {
            doc: e.doc.clone(),
            clock: e.clock.clone(),
            deleted: e.deleted,
            log_ops: e.log.len() as u64,
        };
        let ops: Vec<Op> = e
            .log
            .iter()
            .filter(|op| !have.covers(&op.clock))
            .cloned()
            .collect();
        (Some(state), ops)
    }

    /// Full current state of a doc (no log filtering).
    pub fn full_state(&self, id: DocId) -> Option<DocState> {
        let g = self.inner.lock();
        let e = g.entries.get(&id)?;
        Some(DocState {
            doc: e.doc.clone(),
            clock: e.clock.clone(),
            deleted: e.deleted,
            log_ops: e.log.len() as u64,
        })
    }

    /// Ids of all known docs (live or deleted).
    pub fn doc_ids(&self) -> Vec<DocId> {
        self.inner.lock().entries.keys().copied().collect()
    }

    // -- internals --------------------------------------------------------

    /// Validate a doc name.
    pub fn validate_name(name: &str) -> Result<(), String> {
        if name.is_empty() || name.len() > MAX_NAME_LEN {
            return Err(format!("name must be 1..={MAX_NAME_LEN} chars (got {})", name.len()));
        }
        if name == "." || name == ".." || name.starts_with('.') {
            return Err(format!("name '{name}' must not be '.' or start with '.'"));
        }
        if name.contains('/') || name.contains('\0') {
            return Err(format!("name '{name}' must not contain '/' or NUL"));
        }
        for c in name.chars() {
            if !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') {
                return Err(format!("name '{name}' contains invalid char '{c}'"));
            }
        }
        Ok(())
    }
}

impl Inner {
    // -- replay ------------------------------------------------------------

    fn replay(&mut self) {
        let index_path = self.docs_dir.join("index.json");
        if let Ok(raw) = std::fs::read_to_string(&index_path) {
            if let Ok(idx) = serde_json::from_str::<Index>(&raw) {
                self.ts_hint = idx.ts_hint;
            }
        }
        let mut ids: Vec<DocId> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.docs_dir) {
            for de in rd.flatten() {
                let p = de.path();
                if p.extension().and_then(|e| e.to_str()) == Some("log") {
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
        let mut state: DocState = match std::fs::read_to_string(&snap_path) {
            Ok(raw) => match serde_json::from_str(&raw) {
                Ok(s) => s,
                Err(e) => {
                    warn!(doc = %id, "corrupt snapshot ({e}); starting empty");
                    DocState {
                        doc: Doc { id, name: String::new(), fields: Default::default() },
                        clock: VecClock::default(),
                        deleted: false,
                        log_ops: 0,
                    }
                }
            },
            Err(_) => DocState {
                doc: Doc { id, name: String::new(), fields: Default::default() },
                clock: VecClock::default(),
                deleted: false,
                log_ops: 0,
            },
        };

        let mut log_ops: Vec<Op> = Vec::new();
        if let Ok(raw) = std::fs::read_to_string(&self.docs_dir.join(format!("{id}.log"))) {
            for line in raw.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Op>(line) {
                    Ok(op) => {
                        let mut deleted = state.deleted;
                        apply_op(&mut state.doc, &mut state.clock, &mut deleted, &op);
                        state.deleted = deleted;
                        lift_name(&mut state.doc);
                        self.ts_hint = self.ts_hint.max(op.ts);
                        log_ops.push(op);
                    }
                    Err(e) => warn!(doc = %id, "skipping unparseable log line: {e}"),
                }
            }
        }
        lift_name(&mut state.doc);

        // rebuild the name index (last writer wins by create ts)
        if !state.doc.name.is_empty() && !state.deleted {
            let name = state.doc.name.clone();
            let this_ts = state
                .doc
                .fields
                .values()
                .map(|f| f.ts)
                .max()
                .unwrap_or(0);
            let existing_id = self.names.get(&name).copied();
            let existing_ts = existing_id
                .and_then(|eid| self.entries.get(&eid))
                .map(|e| {
                    e.doc
                        .fields
                        .values()
                        .map(|f| f.ts)
                        .max()
                        .unwrap_or(0)
                })
                .unwrap_or(0);
            if existing_id != Some(id) && this_ts >= existing_ts {
                self.names.insert(name, id);
            }
        }

        self.entries.insert(
            id,
            Entry {
                doc: state.doc,
                clock: state.clock,
                deleted: state.deleted,
                log: log_ops,
            },
        );
    }


    fn persist_index(&mut self) {
        let idx = Index { names: self.names.clone(), ts_hint: self.ts_hint };
        let raw = serde_json::to_string(&idx).expect("index serializes");
        let path = self.docs_dir.join("index.json");
        if std::fs::write(&path, raw).is_ok() {
            let _ = std::fs::File::open(&path).and_then(|f| f.sync_all());
        }
    }

    // -- op building --------------------------------------------------------

    fn next_ts(&mut self) -> u64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.ts_hint = self.ts_hint.max(now).saturating_add(1);
        self.ts_hint
    }

    /// Build (tick, ts, sign) a single op for `id`.
    fn build_op(
        &mut self,
        id: DocId,
        key: Option<String>,
        value: Option<serde_json::Value>,
    ) -> Op {
        let ts = self.next_ts();
        let origin = self.origin.clone();
        let entry = self.entries.entry(id).or_insert_with(|| Entry::new(id));
        entry.clock.tick(&origin);
        let mut op = Op {
            doc_id: id,
            key,
            value,
            ts,
            clock: entry.clock.clone(),
            origin,
            sig: [0; 64],
        };
        op.sign(&self.key);
        op
    }

    /// Build + persist + apply a run of local ops for one doc.
    fn write_ops(
        &mut self,
        id: DocId,
        seeds: Vec<(Option<String>, Option<serde_json::Value>)>,
    ) -> Result<(), String> {
        for (key, value) in seeds {
            let op = self.build_op(id, key, value);
            self.append_op(&op)?;
        }
        Ok(())
    }

    /// Persist (WAL) then apply one op. Shared by local and remote
    /// paths.
    fn append_op(&mut self, op: &Op) -> Result<ApplyResult, String> {
        // 1. signature check
        let known = self
            .known_keys
            .get(&op.origin)
            .ok_or_else(|| format!("unknown origin '{}' (no key registered)", op.origin))?;
        let pk = VerifyingKey::from_bytes(known)
            .map_err(|e| format!("bad public key for '{}': {e}", op.origin))?;
        if !op.verify(&pk) {
            self.quarantine(op, "bad signature");
            return Err(format!("signature check failed for '{}' op", op.origin));
        }

        // 2. persist first (WAL)
        self.append_to_log(op)?;

        // 3. apply (entry borrow scoped to the block)
        let name_before: Option<String> = self
            .entries
            .get(&op.doc_id)
            .map(|e| e.doc.name.clone());
        let res = {
            let entry = self.entries.entry(op.doc_id).or_insert_with(|| Entry::new(op.doc_id));
            let mut deleted = entry.deleted;
            let res = apply_op(&mut entry.doc, &mut entry.clock, &mut deleted, op);
            entry.deleted = deleted;
            if res.applied {
                entry.log.push(op.clone());
            }
            lift_name(&mut entry.doc);
            res
        };
        if !res.applied {
            // known work: the log line stays (harmless; apply is
            // idempotent), but nothing else changes
            return Ok(res);
        }
        self.ts_hint = self.ts_hint.max(op.ts);
        // 4. repoint the name index if a name was just lifted
        let (name, newly_named) = {
            let entry = self.entries.get_mut(&op.doc_id).expect("entry from step 3");
            let changed = name_before.as_deref() != Some(entry.doc.name.as_str());
            (entry.doc.name.clone(), !entry.doc.name.is_empty() && changed)
        };
        if newly_named {
            self.names.insert(name, op.doc_id);
            self.persist_index();
        }
        // 5. snapshot when the live log grows
        let big = self
            .entries
            .get(&op.doc_id)
            .map(|e| (e.log.len() as u64) >= SNAPSHOT_OPS)
            .unwrap_or(false);
        if big {
            self.snapshot(&op.doc_id);
        }
        // 6. queue for sync (local ops only)
        if op.origin == self.origin {
            self.outbound.push(op.clone());
        }
        debug!(doc = %op.doc_id, origin = %op.origin, "op applied");
        Ok(res)
    }

    fn append_to_log(&mut self, op: &Op) -> Result<(), String> {
        let path = self.docs_dir.join(format!("{}.log", op.doc_id));
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
        use std::io::Write;
        let line = format!("{}\n", serde_json::to_string(op).expect("op serializes"));
        f.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())
    }

    /// Snapshot a doc atomically and truncate its live log.
    fn snapshot(&mut self, id: &DocId) {
        let Some(entry) = self.entries.get_mut(id) else { return };
        let state = DocState {
            doc: entry.doc.clone(),
            clock: entry.clock.clone(),
            deleted: entry.deleted,
            log_ops: 0,
        };
        let raw = serde_json::to_string_pretty(&state).expect("state serializes");
        let tmp = self.docs_dir.join(format!("{id}.snap.tmp"));
        let path = self.docs_dir.join(format!("{id}.snap"));
        if std::fs::write(&tmp, raw).is_err() {
            return;
        }
        if std::fs::File::open(&tmp).and_then(|f| f.sync_all()).is_err() {
            return;
        }
        if std::fs::rename(&tmp, &path).is_err() {
            return;
        }
        // truncate the log (fsync the empty file so the truncation is
        // durable before we forget the ops in memory)
        let log = self.docs_dir.join(format!("{id}.log"));
        if std::fs::write(&log, "").is_ok() {
            let _ = std::fs::File::open(&log).and_then(|f| f.sync_all());
        }
        entry.log.clear();
    }

    fn quarantine(&mut self, op: &Op, reason: &str) {
        self.quarantined += 1;
        let path = self.docs_dir.join("quarantine.log");
        let line = format!(
            "{} origin={} key={:?} reason={reason}\n",
            op.doc_id, op.origin, op.key
        );
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            use std::io::Write;
            let _ = f.write_all(line.as_bytes());
        }
        warn!(doc = %op.doc_id, origin = %op.origin, "quarantined op: {reason}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap as BM;

    fn identity(seed: u8) -> SigningKey {
        let mut bytes = [seed; 32];
        bytes[0] = 0;
        SigningKey::from_bytes(&bytes)
    }

    fn open_store(dir: &Path) -> Arc<Store> {
        Store::open(dir, &identity(9), "test-origin")
            .expect("store opens")
    }

    #[test]
    fn create_read_update_delete_field() {
        let dir = tempfile::tempdir().unwrap();
        let s = open_store(dir.path());
        let mut fields: BM<String, serde_json::Value> = BM::new();
        fields.insert("title".into(), "hello".into());
        let id = s.create_doc("note-1", fields).unwrap();
        assert_eq!(s.get("note-1").unwrap().fields["title"].value, "hello");
        assert!(s.get("note-1").unwrap().fields.get("__name__").is_none());
        assert_eq!(s.list().len(), 1);

        s.update_field("note-1", "body", "world".into()).unwrap();
        assert_eq!(s.get("note-1").unwrap().fields["body"].value, "world");

        s.delete_field("note-1", "body").unwrap();
        assert!(s.get("note-1").unwrap().fields.get("body").is_none());

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
        // push enough ops to cross the snapshot threshold
        for i in 0..(SNAPSHOT_OPS as u64 + 50) {
            s.update_field("big", &format!("f{i}"), i.into()).unwrap();
        }
        let snap = dir.path().join(format!("{}.snap", s.get("big").unwrap().id));
        assert!(snap.exists(), "snapshot should have been written");
        drop(s);

        let s2 = open_store(dir.path());
        let doc = s2.get("big").unwrap();
        assert_eq!(doc.fields.len() as u64, SNAPSHOT_OPS + 50);
        assert_eq!(doc.fields["f5"].value, 5);
        drop(s2);
    }

    #[test]
    fn remote_op_applies_and_dedups() {
        let dir = tempfile::tempdir().unwrap();
        let s = open_store(dir.path());
        // a remote peer (different origin) signs ops
        let remote_key = identity(7);
        let remote_origin = "remote-peer";
        s.register_peer_key(remote_origin, remote_key.verifying_key().to_bytes());

        let id = s.create_doc("shared", BM::new()).unwrap();
        let entry_clock0 = s.summary()[&id].clone();

        let mut clock = entry_clock0;
        clock.tick(remote_origin);
        let ts = 1_000_000;
        let mut op = Op {
            doc_id: id,
            key: Some("rf".into()),
            value: Some("rv".into()),
            ts,
            clock: clock.clone(),
            origin: remote_origin.into(),
            sig: [0; 64],
        };
        op.sign(&remote_key);
        let r1 = s.apply_remote(&op).unwrap();
        assert!(r1.applied);
        assert_eq!(s.get("shared").unwrap().fields["rf"].value, "rv");

        // duplicate delivery is a no-op
        let r2 = s.apply_remote(&op).unwrap();
        assert!(!r2.applied);
        assert_eq!(s.get("shared").unwrap().fields["rf"].value, "rv");
    }

    #[test]
    fn bad_signature_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        let s = open_store(dir.path());
        let rogue = identity(3);
        s.register_peer_key("rogue", rogue.verifying_key().to_bytes());
        let id = s.create_doc("t", BM::new()).unwrap();
        let mut clock = s.summary()[&id].clone();
        clock.tick("rogue");
        let mut op = Op {
            doc_id: id,
            key: Some("k".into()),
            value: Some(1i64.into()),
            ts: 42,
            clock,
            origin: "rogue".into(),
            sig: [0; 64],
        };
        // sign with a *different* key than registered
        op.sign(&identity(4));
        assert!(s.apply_remote(&op).is_err());
        assert_eq!(s.quarantined_count(), 1);
        assert!(s.get("t").unwrap().fields.get("k").is_none());
        // unknown origin also rejected
        let mut op2 = op.clone();
        op2.origin = "ghost".into();
        assert!(s.apply_remote(&op2).is_err());
    }

    #[test]
    fn outbound_drain_carries_local_ops_only() {
        let dir = tempfile::tempdir().unwrap();
        let s = open_store(dir.path());
        s.create_doc("o", BM::new()).unwrap();
        let ops = s.drain_outbound();
        assert!(!ops.is_empty());
        assert!(ops.iter().all(|o| o.origin == s.origin()));
        assert!(s.drain_outbound().is_empty());
    }

    #[test]
    fn ts_strictly_increases_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let s = open_store(dir.path());
        s.create_doc("t", BM::new()).unwrap();
        let ts1 = s.drain_outbound().into_iter().map(|o| o.ts).max().unwrap();
        drop(s);
        let s2 = open_store(dir.path());
        s2.update_field("t", "x", 1i64.into()).unwrap();
        let ts2 = s2.drain_outbound().into_iter().map(|o| o.ts).max().unwrap();
        assert!(ts2 > ts1, "ts must not go backwards across restart ({ts1} -> {ts2})");
    }
}
