//! The processor framework: a bounded job queue over the store's
//! doc-change feed, with per-processor dispatch and bounded retry. This is
//! the Rust analogue of the TypeScript reactor's job queue +
//! `ProcessorManager` — the event-driven processing layer that reacts to
//! doc changes (recompute a derived statistic, fan out a notification,
//! maintain a projection) rather than answering a query.
//!
//! A [`Processor`] declares which doc models it reacts to; each matching
//! change becomes a job on a bounded queue, run by a worker with a retry
//! budget. Changes arrive in the store's apply order, so a processor sees a
//! doc's changes in sequence.

use parking_lot::Mutex;
use std::sync::Arc;

use tokio::sync::mpsc;

use ph_reactor::store::Store;

use crate::read_model::DocSnapshot;

/// A processor that reacts to doc changes.
pub trait Processor: Send + Sync {
    /// The processor's name.
    fn name(&self) -> &str;
    /// The doc models this processor reacts to (empty = every model).
    fn models(&self) -> Vec<String> {
        Vec::new()
    }
    /// React to a doc's current snapshot.
    fn on_change(&mut self, snap: &DocSnapshot) -> Result<(), String>;
}

struct Job {
    /// Index into the manager's processor list.
    proc: usize,
    snap: DocSnapshot,
    attempts: u32,
}

/// Drives a set of [`Processor`]s off the store's doc-change feed.
///
/// Construct with [`new`], then `tokio::spawn(manager.run())`. `run`
/// consumes the manager (it owns the job queue) and runs until the store's
/// feed closes.
pub struct ProcessorManager {
    store: Arc<Store>,
    procs: Vec<Arc<Mutex<dyn Processor>>>,
    tx: mpsc::Sender<Job>,
    rx: mpsc::Receiver<Job>,
    max_attempts: u32,
}

impl ProcessorManager {
    pub fn new(store: Arc<Store>, procs: Vec<Arc<Mutex<dyn Processor>>>) -> Self {
        let (tx, rx) = mpsc::channel(1024);
        Self {
            store,
            procs,
            tx,
            rx,
            max_attempts: 3,
        }
    }

    /// Set the per-job retry budget (default 3).
    pub fn with_max_attempts(mut self, n: u32) -> Self {
        self.max_attempts = n;
        self
    }

    pub async fn run(self) {
        let ProcessorManager {
            store,
            procs,
            tx,
            rx,
            max_attempts,
        } = self;

        let mut job_rx = rx;
        let loop_tx = tx.clone();
        let worker_tx = tx;
        let worker_procs = procs.clone();

        // The worker: dequeue jobs, run the matching processor, retry on
        // failure up to the budget.
        let worker = tokio::spawn(async move {
            while let Some(job) = job_rx.recv().await {
                let (name, result) = {
                    let mut p = worker_procs[job.proc].lock();
                    (p.name().to_string(), p.on_change(&job.snap))
                };
                if let Err(e) = result {
                    tracing::warn!(
                        "processor {name} failed on doc '{}' (attempt {}): {e}",
                        job.snap.name,
                        job.attempts
                    );
                    if job.attempts < max_attempts {
                        let retry = Job {
                            proc: job.proc,
                            snap: job.snap,
                            attempts: job.attempts + 1,
                        };
                        if worker_tx.send(retry).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        // The dispatcher: subscribe to the store's feed and enqueue a job
        // per matching processor.
        let mut change_rx = store.subscribe_changes();
        while let Some(change) = change_rx.recv().await {
            let snap = DocSnapshot::from_change(&change);
            for (i, p) in procs.iter().enumerate() {
                let models = p.lock().models();
                if !models.is_empty() && !models.contains(&snap.model.name) {
                    continue;
                }
                let job = Job {
                    proc: i,
                    snap: snap.clone(),
                    attempts: 0,
                };
                if loop_tx.send(job).await.is_err() {
                    break;
                }
            }
        }
        worker.abort();
    }
}

/// A processor that keeps a running count of docs per model — the minimal
/// non-trivial processor, used to exercise the queue (and as a template).
pub struct DocCounter {
    name: String,
    models: Vec<String>,
    counts: Mutex<std::collections::HashMap<String, u64>>,
}

impl DocCounter {
    pub fn new(name: impl Into<String>, models: Vec<String>) -> Self {
        Self {
            name: name.into(),
            models,
            counts: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// The current per-model counts.
    pub fn counts(&self) -> std::collections::HashMap<String, u64> {
        self.counts.lock().clone()
    }
}

impl Processor for DocCounter {
    fn name(&self) -> &str {
        &self.name
    }

    fn models(&self) -> Vec<String> {
        self.models.clone()
    }

    fn on_change(&mut self, snap: &DocSnapshot) -> Result<(), String> {
        let mut c = self.counts.lock();
        let key = snap.model.name.clone();
        if snap.deleted {
            *c.entry(key.clone()).or_insert(0) = c[&key].saturating_sub(1);
        } else {
            *c.entry(key).or_insert(0) += 1;
        }
        Ok(())
    }
}
