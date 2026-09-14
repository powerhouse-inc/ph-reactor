//! Durable storage for model definitions registered at runtime.
//!
//! Models may be built into the binary *or* registered at runtime — both are
//! supported, and runtime registration is a product requirement, not a
//! convenience. But a runtime model that exists only in memory is a trap: the
//! store rebuilds every document by reducing its action log through the model
//! that wrote it, so after a restart a document whose model is missing comes
//! back with no name and no fields.
//!
//! These definitions are therefore written to `<state>/models.json` and loaded
//! **before** the replay. Built-in models never needed this because
//! `Store::open` seeds them itself, which is exactly why the gap stayed hidden
//! until the first domain model was deployed.

use std::path::Path;

use serde_json::Value;

/// Loads every persisted definition. A missing file is not an error — it just
/// means nothing has been registered at runtime yet.
pub fn load_all(path: &Path) -> Vec<Value> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    match serde_json::from_str::<Vec<Value>>(&raw) {
        Ok(v) => v,
        Err(e) => {
            // Refusing to start would be worse: the built-in models still work
            // and the operator can re-register. Losing the file silently would
            // not be acceptable, so this is loud.
            tracing::error!(
                "{} is unreadable ({e}); runtime-registered models will not be \
                 restored and their documents will replay empty",
                path.display()
            );
            Vec::new()
        }
    }
}

/// Adds or replaces one definition, keyed by `name` + `version`.
///
/// Replacing rather than appending matters: re-registering a model is the
/// normal way to correct one, and an append-only file would restore the stale
/// definition on the next start.
pub fn save(path: &Path, def: &Value) -> Result<(), String> {
    let key = |v: &Value| {
        (
            v.get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            v.get("version")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        )
    };
    let this = key(def);
    if this.0.is_empty() {
        return Err("a model definition needs a name".into());
    }
    let mut all = load_all(path);
    all.retain(|v| key(v) != this);
    all.push(def.clone());

    let body = serde_json::to_string_pretty(&all).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, body).map_err(|e| format!("writing {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn def(name: &str, version: &str, field: &str) -> Value {
        json!({
            "name": name, "version": version,
            "fields": { field: "string" },
            "reducers": { "init": { "payload": {}, "writes": {} } }
        })
    }

    #[test]
    fn a_missing_file_is_empty_not_an_error() {
        let dir = tempfile::tempdir().expect("tmp");
        assert!(load_all(&dir.path().join("nope.json")).is_empty());
    }

    #[test]
    fn definitions_round_trip() {
        let dir = tempfile::tempdir().expect("tmp");
        let p = dir.path().join("models.json");
        save(&p, &def("rfp", "1", "title")).expect("save");
        save(&p, &def("proposal", "1", "summary")).expect("save");
        let all = load_all(&p);
        assert_eq!(all.len(), 2);
        let names: Vec<_> = all
            .iter()
            .map(|v| v["name"].as_str().unwrap_or("").to_string())
            .collect();
        assert!(names.contains(&"rfp".to_string()));
        assert!(names.contains(&"proposal".to_string()));
    }

    /// Re-registering must REPLACE, not append: otherwise the next restart
    /// restores the stale definition alongside the corrected one.
    #[test]
    fn re_registering_replaces_the_same_name_and_version() {
        let dir = tempfile::tempdir().expect("tmp");
        let p = dir.path().join("models.json");
        save(&p, &def("rfp", "1", "old_field")).expect("save");
        save(&p, &def("rfp", "1", "new_field")).expect("save");
        let all = load_all(&p);
        assert_eq!(all.len(), 1, "same name+version must replace");
        assert!(all[0]["fields"].get("new_field").is_some());
        assert!(all[0]["fields"].get("old_field").is_none());
    }

    /// A different version is a different model and must coexist.
    #[test]
    fn a_new_version_coexists_with_the_old() {
        let dir = tempfile::tempdir().expect("tmp");
        let p = dir.path().join("models.json");
        save(&p, &def("rfp", "1", "a")).expect("save");
        save(&p, &def("rfp", "2", "b")).expect("save");
        assert_eq!(load_all(&p).len(), 2);
    }

    #[test]
    fn an_unnamed_definition_is_refused() {
        let dir = tempfile::tempdir().expect("tmp");
        let p = dir.path().join("models.json");
        assert!(save(&p, &json!({ "version": "1" })).is_err());
    }
}
