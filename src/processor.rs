//! User-configurable processors: subscriptions on the store's doc-change feed
//! that run a reaction when a filter matches. This is the daemon-side engine
//! for "a processor is a piece of code that listens on changes in documents":
//! a processor declares the document types (and optionally the action / a
//! field→value change) it reacts to, and a reaction (log / run a command /
//! emit an event / create a document) when one matches. The reference example
//! listens to `invoice` and, when `status` changes to `accepted`, runs a
//! payment command.
//!
//! Specs are persisted in `processors.json` (in the state dir) so they survive
//! restarts; the engine applies commands from the API without a restart.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::store::{DocChange, Store};

/// A subscription filter over the doc-change feed.
///
/// `models` empty = any model. The remaining constraints are ANDed against the
/// change's action delta; every constraint that is `Some` must match. Matching
/// a field→value change is what lets a processor react to a *transition*
/// (e.g. `status` → `accepted`) rather than only to a resulting state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ActionFilter {
    #[serde(default)]
    pub models: Vec<String>,
    /// Only match this reducer kind (e.g. `set`, `accept`).
    #[serde(default)]
    pub action_kind: Option<String>,
    /// Only match when this field was written.
    #[serde(default)]
    pub field: Option<String>,
    /// Only match when the written value equals this.
    #[serde(default)]
    pub value: Option<serde_json::Value>,
}

impl ActionFilter {
    /// Match a change against the filter.
    pub fn matches(&self, c: &DocChange) -> bool {
        if !self.models.is_empty() && !self.models.contains(&c.model.name) {
            return false;
        }
        if let Some(k) = &self.action_kind {
            if c.action_kind.as_deref() != Some(k.as_str()) {
                return false;
            }
        }
        if let Some(f) = &self.field {
            if c.action_field.as_deref() != Some(f.as_str()) {
                return false;
            }
        }
        if let Some(v) = &self.value {
            if c.action_value.as_ref() != Some(v) {
                return false;
            }
        }
        true
    }
}

/// What a processor does when its filter matches. `$doc` and `$model` in any
/// template are substituted with the triggering document's name and model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Reaction {
    /// Append a templated message to the daemon log and the fire history.
    Log {
        message: String,
    },
    /// Run a local command (a trusted, loopback-only capability) with a
    /// bounded timeout. The triggering document is exposed as `$PH_DOC` and
    /// the model as `$PH_MODEL`.
    Run {
        command: String,
    },
    /// Record a named event (for a UI feed) — no side effect beyond the fire
    /// history.
    Emit {
        event: String,
    },
    /// Create a document of `model` with `fields` (values may template
    /// `$doc`/`$model`); skipped if the name already exists.
    CreateDoc {
        model: String,
        name: String,
        fields: BTreeMap<String, serde_json::Value>,
    },
}

/// A record of a processor firing (bounded per spec, surfaced to the UI).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fire {
    pub spec: String,
    pub doc: String,
    pub model: String,
    /// The action kind that triggered the fire, if any.
    pub action: Option<String>,
    pub detail: String,
    /// Milliseconds since the epoch when it fired.
    pub ts: u64,
}

/// A persisted, user-configurable processor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessorSpec {
    pub name: String,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub action_kind: Option<String>,
    #[serde(default)]
    pub field: Option<String>,
    #[serde(default)]
    pub value: Option<serde_json::Value>,
    pub reaction: Reaction,
    /// Milliseconds since the epoch, when the spec was created.
    #[serde(default)]
    pub created: u64,
}

impl ProcessorSpec {
    pub fn filter(&self) -> ActionFilter {
        ActionFilter {
            models: self.models.clone(),
            action_kind: self.action_kind.clone(),
            field: self.field.clone(),
            value: self.value.clone(),
        }
    }
}

/// Commands the API sends to the running processor engine.
#[derive(Debug, Clone)]
pub enum RunnerCmd {
    /// Add or replace a spec by name.
    Set(ProcessorSpec),
    /// Remove a spec by name.
    Remove(String),
}

/// Load the persisted processor specs (an empty list on a missing/malformed
/// file). Tolerant: a bad file must never take the daemon down.
pub fn load_specs(path: &Path) -> Vec<ProcessorSpec> {
    match std::fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Persist the current specs (pretty-printed). Best-effort.
pub fn save_specs(path: &Path, specs: &[ProcessorSpec]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = serde_json::to_string_pretty(specs) {
        let _ = std::fs::write(path, s);
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn substitute(template: &str, doc: &str, model: &str) -> String {
    template
        .replace("$doc", doc)
        .replace("$model", model)
        .replace("$name", doc)
}

/// Drives the user-configured processors off the store's doc-change feed.
///
/// The daemon calls [`new`], keeps a [`ProcessorHandle`] for the API, and calls
/// [`spawn`](Self::spawn). `run` consumes the command receiver.
pub struct ProcessorRunner {
    store: Arc<Store>,
    path: std::path::PathBuf,
    cmd_tx: mpsc::UnboundedSender<RunnerCmd>,
    cmd_rx: mpsc::UnboundedReceiver<RunnerCmd>,
    specs: Arc<Mutex<Vec<ProcessorSpec>>>,
    history: Arc<Mutex<BTreeMap<String, VecDeque<Fire>>>>,
    /// Subscribed in the constructor so changes are captured from creation
    /// (not from when the run task schedules), i.e. no change is ever lost.
    feed: mpsc::UnboundedReceiver<DocChange>,
}

const HISTORY_CAP: usize = 50;

impl ProcessorRunner {
    /// Build a runner, loading any persisted specs.
    pub fn new(store: Arc<Store>, path: std::path::PathBuf) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let specs = Arc::new(Mutex::new(load_specs(&path)));
        let feed = store.subscribe_changes();
        Self {
            store,
            path,
            cmd_tx,
            cmd_rx,
            specs,
            history: Arc::new(Mutex::new(BTreeMap::new())),
            feed,
        }
    }

    /// A cheap, `Send + Sync` handle for the API: it sends commands and reads
    /// the live specs / fire history.
    pub fn handle(&self) -> ProcessorHandle {
        ProcessorHandle {
            cmd_tx: self.cmd_tx.clone(),
            specs: self.specs.clone(),
            history: self.history.clone(),
            path: self.path.clone(),
        }
    }

    /// Spawn the feed-driven loop as a background task.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(self.run())
    }

    /// Run the feed-driven loop until the feed or the command channel closes.
    async fn run(self) {
        let ProcessorRunner {
            store,
            path,
            cmd_tx: _,
            cmd_rx,
            specs,
            history,
            feed,
        } = self;
        let mut cmd_rx = cmd_rx;
        let mut feed = feed;
        loop {
            tokio::select! {
                maybe_change = feed.recv() => {
                    let Some(change) = maybe_change else { break };
                    dispatch(&store, &specs, &history, &change).await;
                }
                maybe_cmd = cmd_rx.recv() => {
                    let Some(cmd) = maybe_cmd else { break };
                    match cmd {
                        RunnerCmd::Set(spec) => set_spec(&path, &specs, spec),
                        RunnerCmd::Remove(name) => remove_spec(&path, &specs, &name),
                    }
                }
            }
        }
    }
}

fn set_spec(path: &Path, specs: &Mutex<Vec<ProcessorSpec>>, spec: ProcessorSpec) {
    let mut specs = specs.lock();
    if let Some(p) = specs.iter_mut().find(|s| s.name == spec.name) {
        *p = spec;
    } else {
        specs.push(spec);
    }
    specs.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.name.cmp(&b.name)));
    save_specs(path, &specs);
}

fn remove_spec(path: &Path, specs: &Mutex<Vec<ProcessorSpec>>, name: &str) {
    let mut specs = specs.lock();
    specs.retain(|s| s.name != name);
    save_specs(path, &specs);
}

/// Dispatch a change to every matching spec, running its reaction.
async fn dispatch(
    store: &Arc<Store>,
    specs: &Mutex<Vec<ProcessorSpec>>,
    history: &Mutex<BTreeMap<String, VecDeque<Fire>>>,
    change: &DocChange,
) {
    let specs = specs.lock().clone();
    for spec in &specs {
        if !spec.filter().matches(change) {
            continue;
        }
        let detail = perform(store, &spec, change).await;
        let fire = Fire {
            spec: spec.name.clone(),
            doc: change.name.clone(),
            model: change.model.name.clone(),
            action: change.action_kind.clone(),
            detail,
            ts: now_ms(),
        };
        let mut h = history.lock();
        let d = h.entry(fire.spec.clone()).or_default();
        d.push_back(fire);
        if d.len() > HISTORY_CAP {
            d.pop_front();
        }
    }
}

/// Run one spec's reaction, returning a one-line summary for the fire record.
/// Failures are logged, never propagated (a bad reaction must not stall the
/// feed).
async fn perform(store: &Arc<Store>, spec: &ProcessorSpec, change: &DocChange) -> String {
    let doc = &change.name;
    let model = &change.model.name;
    match &spec.reaction {
        Reaction::Log { message } => {
            let m = substitute(message, doc, model);
            tracing::info!(processor = %spec.name, "log: {m}");
            m
        }
        Reaction::Emit { event } => {
            let e = substitute(event, doc, model);
            tracing::info!(processor = %spec.name, "emit: {e}");
            e
        }
        Reaction::Run { command } => {
            let cmd = substitute(command, doc, model);
            match tokio::time::timeout(
                Duration::from_secs(15),
                tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(&cmd)
                    .env("PH_DOC", doc)
                    .env("PH_MODEL", model)
                    .status(),
            )
            .await
            {
                Ok(Ok(status)) => {
                    let s = format!("ran: {cmd} (exit {status})");
                    tracing::info!(processor = %spec.name, "{s}");
                    s
                }
                Ok(Err(e)) => {
                    let s = format!("run failed: {cmd} ({e})");
                    tracing::warn!(processor = %spec.name, "{s}");
                    s
                }
                Err(_) => {
                    let s = format!("run timed out: {cmd}");
                    tracing::warn!(processor = %spec.name, "{s}");
                    s
                }
            }
        }
        Reaction::CreateDoc { model, name, fields } => {
            let doc_name = substitute(name, doc, model);
            if store.get(&doc_name).is_some() {
                return format!("doc '{doc_name}' already exists (skipped)");
            }
            let model_name = substitute(model, doc, model);
            let mut payload = serde_json::Map::new();
            payload.insert(
                "__name__".into(),
                serde_json::Value::String(doc_name.clone()),
            );
            for (k, v) in fields {
                let mut sv = v.clone();
                if let Some(s) = sv.as_str() {
                    sv = serde_json::Value::String(substitute(s, doc, model));
                }
                payload.insert(k.clone(), sv);
            }
            match store.create_doc_model(
                &doc_name,
                &crate::doc::ModelRef::new(&model_name, "1"),
                &serde_json::Value::Object(payload),
            ) {
                Ok(_) => format!("created '{doc_name}'"),
                Err(e) => {
                    let s = format!("create '{doc_name}' failed: {e}");
                    tracing::warn!(processor = %spec.name, "{s}");
                    s
                }
            }
        }
    }
}

/// The API-facing handle: sends spec commands and reads the live state.
#[derive(Clone)]
pub struct ProcessorHandle {
    cmd_tx: mpsc::UnboundedSender<RunnerCmd>,
    specs: Arc<Mutex<Vec<ProcessorSpec>>>,
    history: Arc<Mutex<BTreeMap<String, VecDeque<Fire>>>>,
    path: std::path::PathBuf,
}

impl ProcessorHandle {
    /// The current specs, newest first.
    pub fn specs(&self) -> Vec<ProcessorSpec> {
        let mut v = self.specs.lock().clone();
        v.reverse();
        v
    }

    /// The recent fires for a spec, newest first.
    pub fn fires(&self, name: &str) -> Vec<Fire> {
        let h = self.history.lock();
        h.get(name)
            .map(|d| d.iter().rev().cloned().collect())
            .unwrap_or_default()
    }

    /// Add or replace a spec (applied + persisted by the runner).
    pub fn set(&self, spec: ProcessorSpec) {
        let _ = self.cmd_tx.send(RunnerCmd::Set(spec));
    }

    /// Remove a spec (applied + persisted by the runner).
    pub fn remove(&self, name: &str) {
        let _ = self.cmd_tx.send(RunnerCmd::Remove(name.into()));
    }

    /// The specs file path (for diagnostics).
    pub fn path(&self) -> std::path::PathBuf {
        self.path.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::DocId;

    fn change(kind: Option<&str>, field: Option<&str>, value: Option<&str>, model: &str) -> DocChange {
        use crate::doc::ModelRef;
        use crate::store::DocState;
        let id = DocId::new();
        DocChange {
            doc_id: id,
            name: "d".into(),
            model: ModelRef::new(model, "1"),
            deleted: false,
            state: DocState {
                doc: crate::doc::Doc {
                    id,
                    name: "d".into(),
                    fields: Default::default(),
                },
                clock: Default::default(),
                deleted: false,
                log_hash: None,
                model: ModelRef::new(model, "1"),
            },
            ts: 1,
            action_kind: kind.map(String::from),
            action_field: field.map(String::from),
            action_value: value.map(|v| serde_json::Value::String(v.to_string())),
        }
    }

    #[test]
    fn filter_matches_by_model_and_delta() {
        // any model, any change.
        assert!(ActionFilter::default()
            .matches(&change(
                Some("set"),
                Some("status"),
                Some("accepted"),
                "invoice"
            )));
        // model must match.
        let f = ActionFilter {
            models: vec!["invoice".into()],
            ..Default::default()
        };
        assert!(f.matches(&change(Some("set"), None, None, "invoice")));
        assert!(!f.matches(&change(Some("set"), None, None, "task")));
        // action kind must match.
        let f = ActionFilter {
            action_kind: Some("set".into()),
            ..Default::default()
        };
        assert!(f.matches(&change(Some("set"), None, None, "invoice")));
        assert!(!f.matches(&change(Some("delete"), None, None, "invoice")));
        // field + value must match (the transition case).
        let f = ActionFilter {
            models: vec!["invoice".into()],
            field: Some("status".into()),
            value: Some(serde_json::json!("accepted")),
            ..Default::default()
        };
        assert!(f.matches(&change(
            Some("set"),
            Some("status"),
            Some("accepted"),
            "invoice"
        )));
        assert!(!f.matches(&change(
            Some("set"),
            Some("status"),
            Some("sent"),
            "invoice"
        )));
        assert!(!f.matches(&change(
            Some("set"),
            Some("customer"),
            Some("acme"),
            "invoice"
        )));
    }

    #[test]
    fn spec_round_trips() {
        let spec = ProcessorSpec {
            name: "invoice-paid".into(),
            models: vec!["invoice".into()],
            action_kind: None,
            field: Some("status".into()),
            value: Some(serde_json::json!("accepted")),
            reaction: Reaction::Run {
                command: "echo paying for $doc".into(),
            },
            created: 5,
        };
        let s = serde_json::to_string(&spec).unwrap();
        let back: ProcessorSpec = serde_json::from_str(&s).unwrap();
        assert_eq!(back, spec);
    }

    #[tokio::test]
    async fn processor_fires_on_matching_change() {
        use ed25519_dalek::SigningKey;
        let dir = tempfile::tempdir().unwrap();
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let store = crate::store::Store::open(dir.path(), &key, "test-origin").unwrap();
        // Pre-seed a spec on disk: the load-on-startup path (deterministic —
        // the spec is present before any change is published).
        let path = dir.path().join("processors.json");
        save_specs(
            &path,
            &[ProcessorSpec {
                name: "on-accepted".into(),
                models: vec![],
                action_kind: None,
                field: Some("status".into()),
                value: Some(serde_json::json!("accepted")),
                reaction: Reaction::Emit { event: "accepted $doc".into() },
                created: 1,
            }],
        );
        let runner = ProcessorRunner::new(store.clone(), path);
        let handle = runner.handle();
        runner.spawn();

        store.create_doc("inv", std::collections::BTreeMap::new()).unwrap();
        store.update_field("inv", "status", serde_json::json!("accepted")).unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let fires = handle.fires("on-accepted");
        assert_eq!(fires.len(), 1, "expected exactly one fire");
        assert_eq!(fires[0].doc, "inv");
        assert_eq!(fires[0].detail, "accepted inv");
    }
}
