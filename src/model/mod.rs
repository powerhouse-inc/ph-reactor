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

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::action::Action;
use crate::doc::{Doc, ModelRef, Op};
use crate::model::open::Open;

pub mod open;

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
}
