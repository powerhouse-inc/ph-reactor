//! `RelationshipIndex` — the relationship-graph read model: turns the
//! relationship fields declared by the model layer (a field whose value is
//! another doc's name) into a directed edge set, with path and ancestry
//! queries. The Rust analogue of the TypeScript reactor's `DocumentIndexer`
//! (the edge index with `getOutgoing`/`getIncoming`/`findPath`/
//! `findAncestors`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use parking_lot::Mutex;

use serde::{Deserialize, Serialize};

use crate::read_model::{DocSnapshot, ReadModel};

/// `(source doc name, field) -> set of target doc names`.
type EdgeMap = HashMap<String, BTreeMap<String, BTreeSet<String>>>;

#[derive(Serialize, Deserialize)]
struct Persisted {
    #[serde(flatten)]
    edges: BTreeMap<String, BTreeMap<String, Vec<String>>>,
}

/// The relationship graph over the store's documents.
pub struct RelationshipIndex {
    /// `(model name, field name) -> target model name`, declared by the
    /// model layer. Only fields in this set become edges.
    decl: BTreeMap<(String, String), String>,
    inner: Mutex<EdgeMap>,
    path: Option<PathBuf>,
}

impl RelationshipIndex {
    pub fn new(decl: BTreeMap<(String, String), String>, path: Option<PathBuf>) -> Self {
        Self {
            decl,
            inner: Mutex::new(HashMap::new()),
            path,
        }
    }

    /// The relationships declared for a model's fields.
    fn rels_for(&self, model: &str) -> Vec<String> {
        self.decl
            .iter()
            .filter(|((m, _), _)| m == model)
            .map(|((_, f), _)| f.clone())
            .collect()
    }
}

impl ReadModel for RelationshipIndex {
    fn name(&self) -> &str {
        "relationship-index"
    }

    fn apply(&self, snap: &DocSnapshot) -> Result<(), String> {
        let mut g = self.inner.lock();
        // A deleted (or re-modelled) doc drops its outgoing edges.
        g.remove(&snap.name);
        if snap.deleted || snap.name.is_empty() {
            return Ok(());
        }
        for field in self.rels_for(&snap.model.name) {
            let Some(v) = snap.fields.get(&field) else {
                continue;
            };
            let Some(target) = v.as_str() else {
                continue;
            };
            if target.is_empty() {
                continue;
            }
            g.entry(snap.name.clone())
                .or_default()
                .entry(field)
                .or_default()
                .insert(target.to_string());
        }
        Ok(())
    }

    fn save(&self) -> Result<(), String> {
        let g = self.inner.lock();
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let flat: BTreeMap<String, BTreeMap<String, Vec<String>>> = g
            .iter()
            .map(|(k, m)| {
                (
                    k.clone(),
                    m.iter()
                        .map(|(f, s)| (f.clone(), s.iter().cloned().collect()))
                        .collect(),
                )
            })
            .collect();
        let tmp = path.with_extension("tmp");
        let raw = serde_json::to_vec_pretty(&Persisted { edges: flat }).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, raw).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn load(&self) -> Result<bool, String> {
        let Some(path) = &self.path else {
            return Ok(false);
        };
        let Ok(raw) = std::fs::read_to_string(path) else {
            return Ok(false);
        };
        let p: Persisted = serde_json::from_str(&raw).map_err(|e| format!("parse index: {e}"))?;
        let mut g = self.inner.lock();
        g.clear();
        for (src, fields) in p.edges {
            let m = g
                .entry(src)
                .or_default();
            for (f, targets) in fields {
                m.entry(f).or_default().extend(targets);
            }
        }
        Ok(true)
    }
}

/// Graph queries over [`RelationshipIndex`].
impl RelationshipIndex {
    /// Outgoing target names from `source` for relationship `field`.
    pub fn outgoing(&self, source: &str, field: &str) -> Vec<String> {
        let g = self.inner.lock();
        g.get(source)
            .and_then(|m| m.get(field))
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Incoming source names to `target` for relationship `field`.
    pub fn incoming(&self, target: &str, field: &str) -> Vec<String> {
        let g = self.inner.lock();
        let mut out = Vec::new();
        for (src, m) in g.iter() {
            if m.get(field).map(|s| s.contains(target)).unwrap_or(false) {
                out.push(src.clone());
            }
        }
        out.sort();
        out
    }

    /// All directed paths from `from` to `to` (bounded), across any
    /// relationship.
    pub fn find_path(&self, from: &str, to: &str) -> Vec<Vec<String>> {
        let g = self.inner.lock();
        let mut paths = Vec::new();
        let mut stack = vec![vec![from.to_string()]];
        while let Some(path) = stack.pop() {
            let last = path.last().unwrap();
            if last == to {
                paths.push(path.clone());
                continue;
            }
            if path.len() > 8 {
                continue;
            }
            if let Some(m) = g.get(last) {
                for targets in m.values() {
                    for t in targets {
                        let mut next = path.clone();
                        next.push(t.clone());
                        stack.push(next);
                    }
                }
            }
        }
        paths.sort();
        paths
    }

    /// Every doc that (transitively) points at `name` — its ancestors.
    pub fn ancestors(&self, name: &str) -> Vec<String> {
        let g = self.inner.lock();
        let mut seen = BTreeSet::new();
        let mut stack = vec![name.to_string()];
        while let Some(n) = stack.pop() {
            for (src, m) in g.iter() {
                if src == &n || seen.contains(src) {
                    continue;
                }
                if m.values().any(|s| s.contains(&n)) {
                    seen.insert(src.clone());
                    stack.push(src.clone());
                }
            }
        }
        let mut v: Vec<String> = seen.into_iter().collect();
        v.sort();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ph_reactor::doc::ModelRef;

    fn decls() -> BTreeMap<(String, String), String> {
        let mut m = BTreeMap::new();
        m.insert(("task".to_string(), "project".to_string()), "project".to_string());
        m
    }

    fn snap(name: &str, model: &str, field: &str, value: &str) -> DocSnapshot {
        DocSnapshot {
            doc_id: ph_reactor::doc::DocId::new(),
            name: name.into(),
            model: ModelRef::new(model, "1"),
            fields: {
                let mut m = BTreeMap::new();
                m.insert(field.into(), serde_json::Value::String(value.into()));
                m
            },
            deleted: false,
            ts: 1,
        }
    }

    #[test]
    fn task_to_project_edges() {
        let ix = RelationshipIndex::new(decls(), None);
        ix.apply(&snap("p", "project", "status", "active")).unwrap();
        ix.apply(&snap("t1", "task", "project", "p")).unwrap();
        ix.apply(&snap("t2", "task", "project", "p")).unwrap();

        assert_eq!(ix.outgoing("t1", "project"), vec!["p".to_string()]);
        assert_eq!(ix.incoming("p", "project"), vec!["t1", "t2"]);
        assert_eq!(ix.ancestors("p"), vec!["t1", "t2"]);
    }

    #[test]
    fn path_follows_the_graph() {
        let mut d = decls();
        d.insert(("project".to_string(), "owner".to_string()), "account".to_string());
        let ix = RelationshipIndex::new(d, None);
        ix.apply(&snap("acc", "account", "currency", "USD")).unwrap();
        ix.apply(&snap("p", "project", "owner", "acc")).unwrap();
        ix.apply(&snap("t", "task", "project", "p")).unwrap();
        let path = ix.find_path("t", "acc");
        assert_eq!(path, vec![vec!["t", "p", "acc"]]);
    }
}
