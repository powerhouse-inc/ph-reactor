//! Document models: a schema over a document's field map plus named
//! *reducers* that turn a signed [`Action`](crate::action::Action) into
//! field writes.
//!
//! A model is a **pure interpreter**: `reduce(state, action) -> Vec<Op>`
//! is a deterministic function with no I/O, no clock, and no RNG. That
//! purity is what makes the per-document action log replayable and
//! verifiable on any peer, and what keeps the whole layer auditable.
//!
//! Submodules (added as they are built):
//! - `open` — `open@1`, the backward-compatible open-field model (the
//!   default; a 1:1 reducer over `set` / `delete`), reproducing the v1
//!   per-field behaviour exactly.
//! - `l1` — the declarative Level-1 interpreter: a JSON definition
//!   (state schema + reducers with payload schema, write templates, and
//!   preconditions) interpreted by one fixed, auditable engine.
//! - `group` — the built-in `group` model (membership + quorum), which is
//!   itself an `l1` definition.
//! - `space` — the built-in `space` model: the unit of access. Everything a
//!   space contains is an app, not a field of the space.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;

use crate::action::Action;
use crate::doc::{Doc, Hash32, ModelRef, Op};
use crate::model::open::Open;

pub mod group;
pub mod l1;
pub mod open;
pub mod package;
pub mod release;
pub mod persist;
pub mod realistic;
pub mod space;

/// Why a model rejected an action or a state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reject {
    /// The model has no reducer of this kind.
    UnknownKind(String),
    /// The payload failed the reducer's payload schema.
    BadPayload(String),
    /// A precondition (authorization / state rule) failed.
    Precondition(String),
    /// The resulting state failed the model's state schema.
    BadState(String),
}

impl Reject {
    /// A one-line human description (for quarantine and `doc verify`).
    pub fn describe(&self) -> String {
        match self {
            Reject::UnknownKind(k) => format!("unknown kind '{k}'"),
            Reject::BadPayload(m) => format!("bad payload: {m}"),
            Reject::Precondition(m) => format!("precondition failed: {m}"),
            Reject::BadState(m) => format!("state violation: {m}"),
        }
    }
}

/// A quorum requirement declared by a reducer: the action needs `min`
/// distinct, valid co-signers whose origins are members of the `group`
/// document's `field` (default `members`). The store checks it (it needs
/// the group's state); the model only declares it so `reduce` and
/// `check_precondition` stay pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuorumSpec {
    /// The group document: a doc name, or `$self` for the action's own doc.
    pub group: String,
    /// The minimum number of distinct valid co-signers required.
    pub min: usize,
    /// The group's membership field. Defaults to `members`.
    pub field: String,
}

/// A document model.
///
/// Every method is a pure function of its arguments (no I/O, clock, or
/// RNG), so reducing the same action against the same state yields the
/// same field writes on every peer. `reduce` emits [`Op`]s; the caller
/// stamps them and feeds them to the per-field merge.
pub trait Model: Send + Sync {
    /// The model's reference (`name@version[#hash]`).
    fn ref_(&self) -> &ModelRef;

    /// Validate that `kind` is a known reducer and that `payload` matches
    /// its payload schema.
    fn validate_payload(&self, kind: &str, payload: &serde_json::Value) -> Result<(), Reject>;

    /// Check the reducer's preconditions against the current state.
    fn check_precondition(&self, state: &Doc, action: &Action) -> Result<(), Reject>;

    /// Reduce the action into the field writes it applies.
    fn reduce(&self, state: &Doc, action: &Action) -> Result<Vec<Op>, Reject>;

    /// Validate that a state satisfies the model's state schema.
    fn check_state(&self, state: &Doc) -> Result<(), Reject>;

    /// Model-declared authorization checked by the store *after* the
    /// signature check but *before* the reducer runs. Like [`quorum`], the
    /// model only *declares* the rule (kept in its definition); the store
    /// runs it because it can reach the document's full state — needed for
    /// rules that inspect a field the pure precondition DSL cannot reach
    /// (e.g. a group member posting to a *private* channel whose allow-list
    /// is a nested object, not a top-level `string[]` field). Default: no
    /// extra authorization (most models are fully described by their
    /// preconditions).
    fn authorize(&self, _state: &Doc, _action: &Action) -> Result<(), Reject> {
        Ok(())
    }

    /// The quorum requirement for a reducer kind, if any. Declared by the
    /// model; checked by the store (which has the group doc). Default: none.
    fn quorum(&self, _kind: &str) -> Option<QuorumSpec> {
        None
    }

    /// The model's canonical definition, if it has a distributable form
    /// (the JSON an [`L1`](l1::L1) interpreter is built from). Mesh
    /// model-distribution uses this: a peer that lacks the model requests
    /// it, verifies the reply against the [`ModelRef`](crate::doc::ModelRef)
    /// hash, and registers it. Built-in models with no stored definition
    /// return `None`.
    fn definition(&self) -> Option<Value> {
        None
    }
}

/// SHA-256 of a model's canonical JSON definition. `serde_json`'s default
/// `Map` is a sorted `BTreeMap`, so the serialization is canonical: the same
/// definition hashes to the same digest on every peer, which is how a
/// distributed model is verified against its [`ModelRef`] hash.
pub fn model_def_hash(def: &Value) -> Hash32 {
    let bytes = serde_json::to_vec(def).expect("model definition serializes");
    Hash32::of(&bytes)
}

/// Loaded models, keyed by `(name, version)`.
#[derive(Default)]
pub struct ModelRegistry {
    models: BTreeMap<(String, String), Arc<dyn Model>>,
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry seeded with the built-in `open@1` model (the default).
    pub fn seeded_with_open() -> Self {
        let mut r = Self::default();
        r.insert(Arc::new(Open::new()));
        r
    }

    /// A registry seeded with all built-in models (`open@1`, `group@1`,
    /// `package@1`, `release@1`).
    ///
    /// `package@1` is built in rather than shipped as a definition because of
    /// the obvious circularity: packages are how definitions are distributed,
    /// so the model that carries them cannot itself arrive in one.
    pub fn seeded_with_builtins() -> Self {
        let mut r = Self::seeded_with_open();
        r.insert(Arc::new(group::group()));
        r.insert(Arc::new(package::package()));
        r.insert(Arc::new(release::release()));
    r.insert(Arc::new(space::space()));
        r
    }

    /// Register a model (replacing any prior same-name/version entry).
    pub fn insert(&mut self, model: Arc<dyn Model>) {
        let r = model.ref_();
        self.models
            .insert((r.name.clone(), r.version.clone()), model);
    }

    /// Look up a model by reference. When the ref pins a hash, the loaded
    /// model's hash must match (otherwise `None` — a tampered model is
    /// never used). Returns an owned `Arc` so callers can hold it across
    /// other borrows of the registry.
    pub fn find(&self, ref_: &ModelRef) -> Option<Arc<dyn Model>> {
        let m = self
            .models
            .get(&(ref_.name.clone(), ref_.version.clone()))?
            .clone();
        if let Some(want) = ref_.hash {
            if m.ref_().hash != Some(want) {
                return None;
            }
        }
        Some(m)
    }

    /// All loaded `(name, version)` pairs, sorted.
    pub fn refs(&self) -> Vec<ModelRef> {
        self.models
            .keys()
            .map(|(n, v)| ModelRef::new(n, v))
            .collect()
    }

    /// The model's definition by reference, if loaded and it has one.
    /// Answers a mesh [`ModelRequest`](crate::p2p::codec::ModelRequest).
    pub fn definition(&self, ref_: &ModelRef) -> Option<Value> {
        self.find(ref_).and_then(|m| m.definition())
    }
}
