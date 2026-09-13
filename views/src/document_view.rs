//! `DocumentView` — the built-in snapshot read model: every document
//! (live and deleted) held by id, indexed by model, with field-equality
//! queries. This is the primary query surface the daemon and CLI use, the
//! Rust analogue of the TypeScript reactor's `KyselyDocumentView` (which
//! is the SQL-backed snapshot index with `get`/`findByType`/`exists`).

use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::read_model::{DocSnapshot, ReadModel};

/// A field-equality predicate for [`DocumentView::find`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocFilter {
    pub field: String,
    pub value: Value,
}

impl DocFilter {
    pub fn new(field: impl Into<String>, value: Value) -> Self {
        DocFilter {
            field: field.into(),
            value,
        }
    }
}

struct Inner {
    by_id: HashMap<String, DocSnapshot>,
    by_name: HashMap<String, String>,
    by_model: HashMap<String, Vec<String>>,
    path: Option<PathBuf>,
}

/// The snapshot index: a derived, queryable view over every document.
pub struct DocumentView {
    inner: Mutex<Inner>,
}

impl Default for DocumentView {
    fn default() -> Self {
        Self::new(None)
    }
}

impl DocumentView {
    /// Create a view. `path` (if set) is where the view persists itself.
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                by_id: HashMap::new(),
                by_name: HashMap::new(),
                by_model: HashMap::new(),
                path,
            }),
        }
    }
}

impl ReadModel for DocumentView {
    fn name(&self) -> &str {
        "document-view"
    }

    fn apply(&self, snap: &DocSnapshot) -> Result<(), String> {
        let mut g = self.inner.lock();
        let id = snap.doc_id.to_string();

        // If the doc's model or liveness changed, drop it from the old
        // model's bucket. Owned values are extracted first so the mutable
        // bucket borrow below does not conflict with the id lookup.
        let prev = g.by_id.get(&id).map(|p| (p.model.name.clone(), p.deleted));
        if let Some((prev_model, prev_deleted)) = prev {
            if prev_model != snap.model.name || prev_deleted != snap.deleted {
                if let Some(list) = g.by_model.get_mut(&prev_model) {
                    list.retain(|x| x != &id);
                }
            }
        }

        // Re-point the name index at this doc (a name may be reused after a
        // delete; the latest writer wins, matching the store's name index).
        if !snap.name.is_empty() {
            g.by_name.insert(snap.name.clone(), id.clone());
        }

        if !snap.deleted {
            g.by_model
                .entry(snap.model.name.clone())
                .or_default()
                .push(id.clone());
            // De-duplicate on re-apply of the same model.
            g.by_model.get_mut(&snap.model.name).unwrap().dedup();
        } else if let Some(list) = g.by_model.get_mut(&snap.model.name) {
            list.retain(|x| x != &id);
        }

        g.by_id.insert(id, snap.clone());
        Ok(())
    }

    fn save(&self) -> Result<(), String> {
        let g = self.inner.lock();
        let Some(path) = &g.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        let docs: Vec<&DocSnapshot> = g.by_id.values().collect();
        let tmp = path.with_extension("tmp");
        let raw = serde_json::to_vec_pretty(&docs).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, raw).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn load(&self) -> Result<bool, String> {
        let g = self.inner.lock();
        let Some(path) = &g.path else {
            return Ok(false);
        };
        let Ok(raw) = std::fs::read_to_string(path) else {
            return Ok(false);
        };
        let docs: Vec<DocSnapshot> =
            serde_json::from_str(&raw).map_err(|e| format!("parse view: {e}"))?;
        drop(g);
        for d in docs {
            let _ = self.apply(&d);
        }
        Ok(true)
    }
}

/// The query surface over [`DocumentView`].
impl DocumentView {
    /// Look up a doc by name or (hex) id.
    pub fn get(&self, name_or_id: &str) -> Option<DocSnapshot> {
        let g = self.inner.lock();
        if let Some(id) = g.by_name.get(name_or_id) {
            return g.by_id.get(id).cloned();
        }
        g.by_id.get(name_or_id).cloned()
    }

    /// Every live doc of `model` (or all models when `model` is empty),
    /// optionally filtered by a field-equality predicate.
    pub fn find(&self, model: &str, filter: Option<&DocFilter>) -> Vec<DocSnapshot> {
        let g = self.inner.lock();
        let mut out = Vec::new();
        for snap in g.by_id.values() {
            if snap.deleted {
                continue;
            }
            if !model.is_empty() && snap.model.name != model {
                continue;
            }
            if let Some(f) = filter {
                let Some(v) = snap.fields.get(&f.field) else {
                    continue;
                };
                if *v != f.value {
                    continue;
                }
            }
            out.push(snap.clone());
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// The number of live docs of `model` (or all models when empty).
    pub fn count(&self, model: &str) -> usize {
        let g = self.inner.lock();
        if model.is_empty() {
            return g.by_id.values().filter(|s| !s.deleted).count();
        }
        g.by_id
            .values()
            .filter(|s| !s.deleted && s.model.name == model)
            .count()
    }

    /// Whether a live doc named `name` exists.
    pub fn exists(&self, name: &str) -> bool {
        let g = self.inner.lock();
        g.by_id
            .get(&g.by_name.get(name).cloned().unwrap_or_default())
            .map(|s| !s.deleted)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ph_reactor::doc::ModelRef;
    use std::collections::BTreeMap;

    fn snap(name: &str, model: &str, field: &str, value: &str) -> DocSnapshot {
        DocSnapshot {
            doc_id: ph_reactor::doc::DocId::new(),
            name: name.into(),
            model: ModelRef::new(model, "1"),
            fields: {
                let mut m = BTreeMap::new();
                m.insert(field.into(), Value::String(value.into()));
                m
            },
            deleted: false,
            ts: 1,
        }
    }

    #[test]
    fn find_filters_by_model_and_field() {
        let v = DocumentView::new(None);
        v.apply(&snap("a", "task", "status", "todo")).unwrap();
        v.apply(&snap("b", "task", "status", "done")).unwrap();
        v.apply(&snap("c", "project", "status", "active")).unwrap();

        assert_eq!(v.count("task"), 2);
        assert_eq!(v.count("project"), 1);
        assert_eq!(v.count(""), 3);

        let todo = v.find(
            "task",
            Some(&DocFilter::new("status", Value::String("todo".into()))),
        );
        assert_eq!(todo.len(), 1);
        assert_eq!(todo[0].name, "a");
    }

    #[test]
    fn delete_removes_from_index() {
        let v = DocumentView::new(None);
        // A doc's id is stable across its changes: the delete is the same
        // id with `deleted` flipped, not a new id.
        let id = ph_reactor::doc::DocId::new();
        let mut a = DocSnapshot {
            doc_id: id,
            name: "a".into(),
            model: ModelRef::new("task", "1"),
            fields: BTreeMap::new(),
            deleted: false,
            ts: 1,
        };
        v.apply(&a).unwrap();
        assert_eq!(v.count("task"), 1);
        a.deleted = true;
        v.apply(&a).unwrap();
        assert_eq!(v.count("task"), 0);
        assert!(!v.exists("a"));
    }

    #[test]
    fn save_and_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("view.json");
        let v = DocumentView::new(Some(path.clone()));
        v.apply(&snap("a", "task", "status", "todo")).unwrap();
        v.save().unwrap();

        let v2 = DocumentView::new(Some(path));
        assert!(v2.load().unwrap());
        assert_eq!(v2.get("a").map(|s| s.model.name), Some("task".to_string()));
    }
}
