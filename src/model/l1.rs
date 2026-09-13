//! `l1` — the declarative Level-1 interpreter.
//!
//! One fixed engine interprets a JSON model definition over a small,
//! auditable vocabulary — no lambdas, no control flow, no I/O — so a
//! document's action log replays identically on every peer:
//!
//! - **typed fields** — a `fields` map of field name to type
//!   (`string`, `number`, `boolean`, `object`, `array`, and the `*[]`
//!   array types); `check_state` enforces it.
//! - **write templates** — each reducer's `writes` maps a field to
//!   `{ set | append | remove: <template> }`, where a template is a literal
//!   or `$actor` / `$ts` / `$payload.<f>`.
//! - **precondition DSL** — each reducer's `pre` is a list of single-key
//!   objects: `field-is`, `field-not`, `field-present`, `field-absent`,
//!   `actor-in`, `actor-not-in`, `actor-is`, `check` (a named check from
//!   the model's `checks` map), and `quorum`.
//!
//! The `quorum` precondition names a *group* document whose membership a
//! minimum number of distinct, valid co-signers must satisfy. That needs the
//! group's state (a different document), so the interpreter only *declares*
//! it (via [`Model::quorum`]) and the store checks it — keeping `reduce`
//! and `check_precondition` pure.
//!
//! Definition shape:
//! ```json
//! { "name": "...", "version": "...",
//!   "fields":   { "<field>": "<type>" },
//!   "reducers": { "<kind>": {
//!       "payload": { "<f>": "<type>" },
//!       "writes":  { "<field>": { "set|append|remove": <template> } },
//!       "pre":     [ <precond> ... ] } },
//!   "checks":   { "<name>": <precond> } }
//! ```

use serde_json::Value;

use crate::action::Action;
use crate::doc::{Doc, ModelRef, Op};
use crate::model::{Model, QuorumSpec, Reject};

/// The declarative interpreter. Holds its reference and the JSON definition
/// it interprets.
pub struct L1 {
    ref_: ModelRef,
    /// The full JSON definition this interpreter was built from (kept so
    /// the model can be distributed over the mesh and re-built elsewhere).
    def: Value,
    fields: Value,
    reducers: Value,
    checks: Value,
}

impl L1 {
    /// Build an interpreter from a definition value.
    pub fn from_def(def: Value) -> Result<Self, String> {
        let name = def
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            return Err("l1 model definition requires a non-empty 'name'".into());
        }
        let version = def
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("1")
            .to_string();
        // Pin the reference to the content hash of the canonical
        // definition so a distributed copy is verifiable against the exact
        // model the doc was written under.
        let hash = crate::model::model_def_hash(&def);
        let ref_ = ModelRef {
            name,
            version,
            hash: Some(hash),
        };
        let fields = def.get("fields").cloned().unwrap_or(Value::Null);
        let reducers = def.get("reducers").cloned().unwrap_or(Value::Null);
        let checks = def.get("checks").cloned().unwrap_or(Value::Null);
        Ok(Self {
            ref_,
            def,
            fields,
            reducers,
            checks,
        })
    }

    /// The reducer definition for `kind`, if the model declares one.
    fn reducer(&self, kind: &str) -> Option<&Value> {
        self.reducers.get(kind)
    }

    /// The reducer's precondition list (empty if none).
    fn pre_list(&self, kind: &str) -> Vec<&Value> {
        self.reducer(kind)
            .and_then(|r| r.get("pre"))
            .and_then(Value::as_array)
            .map(|a| a.iter().collect())
            .unwrap_or_default()
    }

    /// Compute the new value a write produces against the current state.
    fn write_value(
        &self,
        state: &Doc,
        action: &Action,
        field: &str,
        w: &Value,
    ) -> Result<Value, Reject> {
        let (key, tmpl) = match w.as_object().and_then(|o| o.iter().next()) {
            Some(x) => x,
            None => {
                return Err(Reject::BadState(format!(
                    "field '{field}': write must be exactly one of set/append/remove"
                )))
            }
        };
        match key.as_str() {
            "set" => Ok(self.eval_template(action, tmpl)),
            "append" => {
                let mut out = current_array(state, field);
                out.push(self.eval_template(action, tmpl));
                Ok(Value::Array(out))
            }
            "remove" => {
                let item = self.eval_template(action, tmpl);
                let out = current_array(state, field)
                    .into_iter()
                    .filter(|v| *v != item)
                    .collect();
                Ok(Value::Array(out))
            }
            _ => Err(Reject::BadState(format!(
                "field '{field}': write must be exactly one of set/append/remove"
            ))),
        }
    }

    /// Evaluate a template: `$actor`, `$ts`, `$payload.<f>`, or a literal.
    fn eval_template(&self, action: &Action, tmpl: &Value) -> Value {
        match tmpl {
            Value::String(s) => match s.as_str() {
                "$actor" => Value::String(action.origin.clone()),
                "$ts" => Value::Number(action.ts.into()),
                _ if s.starts_with("$payload.") => {
                    let f = &s["$payload.".len()..];
                    action.payload.get(f).cloned().unwrap_or(Value::Null)
                }
                other => Value::String(other.to_string()),
            },
            other => other.clone(),
        }
    }

    /// Evaluate one precondition against the current state. `depth` bounds
    /// named-check recursion.
    fn eval_precond(
        &self,
        state: &Doc,
        action: &Action,
        p: &Value,
        depth: usize,
    ) -> Result<(), Reject> {
        if depth > 8 {
            return Err(Reject::Precondition(
                "named-check recursion too deep".into(),
            ));
        }
        let (key, arg) = match p.as_object().and_then(|o| o.iter().next()) {
            Some(x) => x,
            None => {
                return Err(Reject::Precondition(
                    "precondition must be a single-key object".into(),
                ))
            }
        };
        match key.as_str() {
            "field-is" => {
                let f = field_name(arg, "field-is")?;
                let want = arg.get("value").cloned().unwrap_or(Value::Null);
                let got = current_value(state, &f);
                if got != want {
                    return Err(Reject::Precondition(format!(
                        "field '{f}' is {got}, expected {want}"
                    )));
                }
                Ok(())
            }
            "field-not" => {
                let f = field_name(arg, "field-not")?;
                let want = arg.get("value").cloned().unwrap_or(Value::Null);
                let got = current_value(state, &f);
                if got == want {
                    return Err(Reject::Precondition(format!(
                        "field '{f}' must not be {want}"
                    )));
                }
                Ok(())
            }
            "field-present" => {
                let f = string_arg(arg, "field-present")?;
                if is_present(state, &f) {
                    Ok(())
                } else {
                    Err(Reject::Precondition(format!("field '{f}' absent")))
                }
            }
            "field-absent" => {
                let f = string_arg(arg, "field-absent")?;
                if is_present(state, &f) {
                    Err(Reject::Precondition(format!("field '{f}' present")))
                } else {
                    Ok(())
                }
            }
            "actor-in" => {
                let f = string_arg(arg, "actor-in")?;
                if in_list(state, &f, &action.origin) {
                    Ok(())
                } else {
                    Err(Reject::Precondition(format!(
                        "actor {} is not in '{f}'",
                        action.origin
                    )))
                }
            }
            "actor-not-in" => {
                let f = string_arg(arg, "actor-not-in")?;
                if in_list(state, &f, &action.origin) {
                    Err(Reject::Precondition(format!(
                        "actor {} is in '{f}'",
                        action.origin
                    )))
                } else {
                    Ok(())
                }
            }
            "actor-is" => {
                let who = string_arg(arg, "actor-is")?;
                if action.origin.as_str() == who {
                    Ok(())
                } else {
                    Err(Reject::Precondition(format!(
                        "actor is {}, expected {who}",
                        action.origin
                    )))
                }
            }
            "check" => {
                let name = string_arg(arg, "check")?;
                let sub = self
                    .checks
                    .get(&name)
                    .ok_or_else(|| Reject::Precondition(format!("unknown named check '{name}'")))?;
                self.eval_precond(state, action, sub, depth + 1)
            }
            _ => Err(Reject::Precondition(format!(
                "unsupported precondition '{key}'"
            ))),
        }
    }
}

impl Model for L1 {
    fn ref_(&self) -> &ModelRef {
        &self.ref_
    }

    fn validate_payload(&self, kind: &str, payload: &Value) -> Result<(), Reject> {
        let r = match self.reducer(kind) {
            Some(r) => r,
            None => return Err(Reject::UnknownKind(kind.into())),
        };
        let schema = r.get("payload").cloned().unwrap_or(Value::Null);
        if let Some(obj) = schema.as_object() {
            for (f, t) in obj {
                let want = t.as_str().unwrap_or("");
                let got = match payload.get(f) {
                    Some(v) => v,
                    None => {
                        return Err(Reject::BadPayload(format!(
                            "missing payload.{f} (want {want})"
                        )))
                    }
                };
                if !type_matches(got, want) {
                    return Err(Reject::BadPayload(format!("payload.{f} must be {want}")));
                }
            }
        }
        Ok(())
    }

    fn check_precondition(&self, state: &Doc, action: &Action) -> Result<(), Reject> {
        for p in self.pre_list(action.kind.as_str()) {
            // Quorum is declared here but checked by the store (it needs the
            // group's state, a different document).
            if is_quorum(p) {
                continue;
            }
            self.eval_precond(state, action, p, 0)?;
        }
        Ok(())
    }

    fn reduce(&self, state: &Doc, action: &Action) -> Result<Vec<Op>, Reject> {
        let r = match self.reducer(action.kind.as_str()) {
            Some(r) => r,
            None => return Err(Reject::UnknownKind(action.kind.clone())),
        };
        let writes = r.get("writes").cloned().unwrap_or(Value::Null);
        let mut ops = Vec::new();
        if let Some(obj) = writes.as_object() {
            for (field, w) in obj {
                let value = self.write_value(state, action, field, w)?;
                ops.push(Op {
                    doc_id: action.doc_id,
                    key: Some(field.clone()),
                    value: Some(value),
                    ts: action.ts,
                    clock: action.clock.clone(),
                    origin: action.origin.clone(),
                    sig: action.sig,
                });
            }
        }
        Ok(ops)
    }

    fn check_state(&self, state: &Doc) -> Result<(), Reject> {
        if let Some(obj) = self.fields.as_object() {
            for (field, t) in obj {
                let want = t.as_str().unwrap_or("");
                if let Some(f) = state.fields.get(field) {
                    if !f.deleted && !type_matches(&f.value, want) {
                        return Err(Reject::BadState(format!("field '{field}' must be {want}")));
                    }
                }
            }
        }
        Ok(())
    }

    fn quorum(&self, kind: &str) -> Option<QuorumSpec> {
        self.pre_list(kind).iter().copied().find_map(parse_quorum)
    }

    fn definition(&self) -> Option<Value> {
        Some(self.def.clone())
    }
}

// ---- field / precondition helpers (pure) ----------------------------------

fn current_value(state: &Doc, field: &str) -> Value {
    state
        .fields
        .get(field)
        .filter(|f| !f.deleted)
        .map(|f| f.value.clone())
        .unwrap_or(Value::Null)
}

fn current_array(state: &Doc, field: &str) -> Vec<Value> {
    current_value(state, field)
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn is_present(state: &Doc, field: &str) -> bool {
    state.fields.get(field).map(|f| !f.deleted).unwrap_or(false)
}

fn in_list(state: &Doc, field: &str, origin: &str) -> bool {
    current_array(state, field)
        .iter()
        .any(|v| v.as_str() == Some(origin))
}

/// `field-is` / `field-not` carry `{ field, value }`; read the field name.
fn field_name(arg: &Value, key: &str) -> Result<String, Reject> {
    arg.get("field")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| Reject::Precondition(format!("'{key}' requires a 'field' string")))
}

/// Precondition forms whose value is a plain string (the field / name).
fn string_arg(arg: &Value, key: &str) -> Result<String, Reject> {
    arg.as_str()
        .map(str::to_string)
        .ok_or_else(|| Reject::Precondition(format!("'{key}' requires a string argument")))
}

fn is_quorum(p: &Value) -> bool {
    p.as_object()
        .map(|o| o.contains_key("quorum"))
        .unwrap_or(false)
}

fn parse_quorum(p: &Value) -> Option<QuorumSpec> {
    let q = p.get("quorum")?;
    let group = q.get("group")?.as_str()?.to_string();
    let min = q.get("min")?.as_u64()? as usize;
    let field = q
        .get("field")
        .and_then(Value::as_str)
        .unwrap_or("members")
        .to_string();
    Some(QuorumSpec { group, min, field })
}

fn type_matches(v: &Value, want: &str) -> bool {
    match want {
        "string" => v.is_string(),
        "number" => v.is_number(),
        "boolean" => v.is_boolean(),
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string[]" => v
            .as_array()
            .map(|a| a.iter().all(Value::is_string))
            .unwrap_or(false),
        "number[]" => v
            .as_array()
            .map(|a| a.iter().all(Value::is_number))
            .unwrap_or(false),
        "boolean[]" => v
            .as_array()
            .map(|a| a.iter().all(Value::is_boolean))
            .unwrap_or(false),
        // An unknown type string is permissive: the vocabulary is fixed,
        // but a definition can still use a plain `object`/`array`.
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A tiny declarative model for the interpreter tests.
    fn test_model() -> L1 {
        L1::from_def(json!({
            "name": "ledger",
            "version": "1",
            "fields": { "total": "number", "owner": "string" },
            "reducers": {
                "set-total": {
                    "payload": { "amount": "number" },
                    "writes": { "total": { "set": "$payload.amount" } },
                    "pre": []
                },
                "set-owner": {
                    "payload": { "who": "string" },
                    "writes": { "owner": { "set": "$payload.who" } },
                    "pre": [ { "actor-is": "alice" } ]
                },
                "stamp": {
                    "payload": {},
                    "writes": { "owner": { "set": "$actor" } },
                    "pre": []
                }
            }
        }))
        .expect("well-formed")
    }

    fn action(kind: &str, payload: Value, origin: &str) -> Action {
        Action {
            doc_id: crate::doc::DocId::new(),
            model: ModelRef::new("ledger", "1"),
            kind: kind.into(),
            payload,
            ts: 1,
            clock: Default::default(),
            origin: origin.into(),
            cosig: Vec::new(),
            prev_hash: None,
            sig: [0; 64],
        }
    }

    fn doc_with(fields: serde_json::Map<String, Value>) -> Doc {
        let mut fields = fields;
        let id = crate::doc::DocId::new();
        let mapped = std::mem::take(&mut fields)
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    crate::doc::Field {
                        value: v,
                        version: Default::default(),
                        ts: 0,
                        origin: "x".into(),
                        deleted: false,
                    },
                )
            })
            .collect();
        Doc {
            id,
            name: "d".into(),
            fields: mapped,
        }
    }

    #[test]
    fn payload_schema_rejects_unknown_kind() {
        let m = test_model();
        assert!(matches!(
            m.validate_payload("nope", &json!({})),
            Err(Reject::UnknownKind(k)) if k == "nope"
        ));
    }

    #[test]
    fn payload_schema_requires_typed_fields() {
        let m = test_model();
        assert!(m
            .validate_payload("set-total", &json!({ "amount": 5 }))
            .is_ok());
        // wrong type
        assert!(matches!(
            m.validate_payload("set-total", &json!({ "amount": "five" })),
            Err(Reject::BadPayload(_))
        ));
        // missing field
        assert!(matches!(
            m.validate_payload("set-total", &json!({})),
            Err(Reject::BadPayload(_))
        ));
    }

    #[test]
    fn reduce_set_writes_the_field() {
        let m = test_model();
        let a = action("set-total", json!({ "amount": 42 }), "alice");
        let ops = m.reduce(&doc_with(serde_json::Map::new()), &a).unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].key.as_deref(), Some("total"));
        assert_eq!(ops[0].value, Some(json!(42)));
    }

    #[test]
    fn reduce_template_uses_actor_and_ts() {
        let m = test_model();
        let mut a = action("stamp", json!({}), "bob");
        a.ts = 99;
        let ops = m.reduce(&doc_with(serde_json::Map::new()), &a).unwrap();
        assert_eq!(ops[0].value, Some(json!("bob")));
    }

    #[test]
    fn precondition_actor_is_enforced() {
        let m = test_model();
        let d = doc_with(serde_json::Map::new());
        assert!(m
            .check_precondition(&d, &action("set-owner", json!({ "who": "x" }), "alice"))
            .is_ok());
        assert!(matches!(
            m.check_precondition(&d, &action("set-owner", json!({ "who": "x" }), "eve")),
            Err(Reject::Precondition(_))
        ));
    }

    #[test]
    fn precondition_field_is_and_present() {
        let m = L1::from_def(json!({
            "name": "g", "version": "1",
            "fields": { "state": "string" },
            "reducers": {
                "close": {
                    "payload": {},
                    "writes": { "state": { "set": "closed" } },
                    "pre": [ { "field-is": { "field": "state", "value": "open" } } ]
                },
                "reopen": {
                    "payload": {},
                    "writes": { "state": { "set": "open" } },
                    "pre": [ { "field-not": { "field": "state", "value": "open" } } ]
                }
            }
        }))
        .unwrap();
        let open = {
            let mut mp = serde_json::Map::new();
            mp.insert("state".into(), json!("open"));
            mp
        };
        let d = doc_with(open);
        assert!(m
            .check_precondition(&d, &action("close", json!({}), "a"))
            .is_ok());
        assert!(matches!(
            m.check_precondition(&d, &action("reopen", json!({}), "a")),
            Err(Reject::Precondition(_))
        ));
    }

    #[test]
    fn check_state_enforces_types() {
        let m = test_model();
        // total must be a number
        let mut d = doc_with(serde_json::Map::new());
        let mut mp = serde_json::Map::new();
        mp.insert("total".into(), json!("not-a-number"));
        d.fields = mp
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    crate::doc::Field {
                        value: v,
                        version: Default::default(),
                        ts: 0,
                        origin: "x".into(),
                        deleted: false,
                    },
                )
            })
            .collect();
        assert!(matches!(m.check_state(&d), Err(Reject::BadState(_))));
        // a well-typed state passes
        let mut d2 = doc_with(serde_json::Map::new());
        let mut mp2 = serde_json::Map::new();
        mp2.insert("total".into(), json!(3));
        d2.fields = mp2
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    crate::doc::Field {
                        value: v,
                        version: Default::default(),
                        ts: 0,
                        origin: "x".into(),
                        deleted: false,
                    },
                )
            })
            .collect();
        assert!(m.check_state(&d2).is_ok());
    }

    #[test]
    fn quorum_spec_is_declared_not_checked() {
        // A model whose reducer declares a quorum: the interpreter returns
        // the spec from `quorum()` and skips it in `check_precondition`.
        let m = L1::from_def(json!({
            "name": "q", "version": "1",
            "fields": { "members": "string[]" },
            "reducers": {
                "add-manager": {
                    "payload": { "member": "string" },
                    "writes": { "members": { "append": "$payload.member" } },
                    "pre": [ { "quorum": { "group": "$self", "min": 2, "field": "members" } } ]
                }
            }
        }))
        .unwrap();
        let spec = m.quorum("add-manager").expect("declares a quorum");
        assert_eq!(spec.group, "$self");
        assert_eq!(spec.min, 2);
        assert_eq!(spec.field, "members");
        // check_precondition skips the quorum (the store checks it).
        assert!(m
            .check_precondition(
                &doc_with(serde_json::Map::new()),
                &action("add-manager", json!({ "member": "m" }), "a")
            )
            .is_ok());
        // a kind without a quorum yields None
        assert!(m.quorum("nope").is_none());
    }

    #[test]
    fn named_check_reuse_and_depth() {
        let m = L1::from_def(json!({
            "name": "n", "version": "1",
            "fields": { "f": "string" },
            "reducers": {
                "go": {
                    "payload": {},
                    "writes": { "f": { "set": "v" } },
                    "pre": [ { "check": "must-be-a" } ]
                }
            },
            "checks": { "must-be-a": { "actor-is": "a" } }
        }))
        .unwrap();
        assert!(m
            .check_precondition(
                &doc_with(serde_json::Map::new()),
                &action("go", json!({}), "a")
            )
            .is_ok());
        assert!(matches!(
            m.check_precondition(
                &doc_with(serde_json::Map::new()),
                &action("go", json!({}), "b")
            ),
            Err(Reject::Precondition(_))
        ));
        // an unknown named check is a precondition failure
        let bad = L1::from_def(json!({
            "name": "n2", "version": "1",
            "reducers": {
                "go": {
                    "payload": {},
                    "writes": { "f": { "set": "v" } },
                    "pre": [ { "check": "missing" } ]
                }
            }
        }))
        .unwrap();
        assert!(matches!(
            bad.check_precondition(
                &doc_with(serde_json::Map::new()),
                &action("go", json!({}), "a")
            ),
            Err(Reject::Precondition(_))
        ));
    }
}
