//! The realistic document models: a small set of [`L1`](super::l1)
//! definitions standing in for the document-model packages a real
//! deployment syncs from the registry. They are deliberately small and
//! self-contained so a multi-client end-to-end test can exercise the
//! whole stack (store + sync + read models) with realistic data:
//!
//! - **`project@1`** — a project (doc name is the project name) with
//!   status, owner, due date, description.
//! - **`task@1`** — a task belonging to a project (the `project` field is
//!   the project's doc name) with status, assignee, priority.
//! - **`account@1`** — a finance account (doc name is the account name)
//!   with currency and a running balance.
//! - **`transaction@1`** — a transaction against an account (the `account`
//!   field is the account's doc name) with amount, category, date.
//!
//! The `project`/`account` fields are **relationships**: a field whose
//! value is another doc's name. [`realistic_relationships`] declares them
//! so the read-model layer can build a relationship graph without the
//! reducer knowing anything about querying.

use std::collections::BTreeMap;

use serde_json::json;

use crate::model::l1::L1;

/// The JSON definition of the `folder@1` model: a container that groups
/// documents by membership. Moving a doc into a folder appends its name to
/// the folder's `members`; the Documents view can show which folder a doc
/// lives in by scanning members.
pub fn folder_def() -> serde_json::Value {
    json!({
        "name": "folder",
        "version": "1",
        "fields": {
            "description": "string",
            "members": "string[]"
        },
        "reducers": {
            "init": {
                "payload": {
                    "name": "string",
                    "description": "string",
                    "members": "string[]"
                },
                "writes": {
                    "__name__": { "set": "$payload.name" },
                    "description": { "set": "$payload.description" },
                    "members": { "set": "$payload.members" }
                },
                "pre": []
            },
            "add-member": {
                "payload": { "member": "string" },
                "writes": { "members": { "append": "$payload.member" } },
                "pre": []
            },
            "remove-member": {
                "payload": { "member": "string" },
                "writes": { "members": { "remove": "$payload.member" } },
                "pre": []
            },
            "set-description": {
                "payload": { "description": "string" },
                "writes": { "description": { "set": "$payload.description" } },
                "pre": []
            }
        }
    })
}

/// The JSON definition of the `note@1` model: a plain text "file" — a title
/// and a body. The default content created straight into a group's drive;
/// the drive lists its name to the group's members. `edit` updates both at
/// once; `set-title` / `set-body` update a single field (what the rich
/// editor dispatches per changed field).
pub fn note_def() -> serde_json::Value {
    json!({
        "name": "note",
        "version": "1",
        "fields": {
            "title": "string",
            "body": "string"
        },
        "reducers": {
            "init": {
                "payload": {
                    "name": "string",
                    "title": "string",
                    "body": "string"
                },
                "writes": {
                    "__name__": { "set": "$payload.name" },
                    "title": { "set": "$payload.title" },
                    "body": { "set": "$payload.body" }
                },
                "pre": []
            },
            "edit": {
                "payload": {
                    "title": "string",
                    "body": "string"
                },
                "writes": {
                    "title": { "set": "$payload.title" },
                    "body": { "set": "$payload.body" }
                },
                "pre": []
            },
            "set-title": {
                "payload": { "title": "string" },
                "writes": { "title": { "set": "$payload.title" } },
                "pre": []
            },
            "set-body": {
                "payload": { "body": "string" },
                "writes": { "body": { "set": "$payload.body" } },
                "pre": []
            }
        }
    })
}

/// The JSON definition of the `project@1` model.
pub fn project_def() -> serde_json::Value {
    json!({
        "name": "project",
        "version": "1",
        "fields": {
            "status": "string",
            "owner": "string",
            "due_date": "string",
            "description": "string"
        },
        "reducers": {
            "init": {
                "payload": {
                    "name": "string",
                    "status": "string",
                    "owner": "string",
                    "due_date": "string",
                    "description": "string"
                },
                "writes": {
                    "__name__": { "set": "$payload.name" },
                    "status": { "set": "$payload.status" },
                    "owner": { "set": "$payload.owner" },
                    "due_date": { "set": "$payload.due_date" },
                    "description": { "set": "$payload.description" }
                },
                "pre": []
            },
            "set-status": {
                "payload": { "status": "string" },
                "writes": { "status": { "set": "$payload.status" } },
                "pre": []
            },
            "set-owner": {
                "payload": { "owner": "string" },
                "writes": { "owner": { "set": "$payload.owner" } },
                "pre": []
            },
            "set-due-date": {
                "payload": { "due_date": "string" },
                "writes": { "due_date": { "set": "$payload.due_date" } },
                "pre": []
            }
        }
    })
}

/// The JSON definition of the `task@1` model: a task with a title, a `status`
/// (a dropdown — its allowed values are declared in `enums`), an assignee, a
/// `project` relationship, a numeric `priority`, and a `tags` list. Each
/// editable field has a `set-<field>` reducer so the console's rich editor can
/// persist it with the model's real reducer (not a generic field-set).
pub fn task_def() -> serde_json::Value {
    json!({
        "name": "task",
        "version": "1",
        "fields": {
            "title": "string",
            "status": "string",
            "assignee": "string",
            "project": "string",
            "priority": "number",
            "tags": "string[]"
        },
        "enums": {
            "status": ["open", "in-progress", "blocked", "done"]
        },
        "reducers": {
            "init": {
                "payload": {
                    "name": "string",
                    "title": "string",
                    "status": "string",
                    "assignee": "string",
                    "project": "string",
                    "priority": "number"
                },
                "writes": {
                    "__name__": { "set": "$payload.name" },
                    "title": { "set": "$payload.title" },
                    "status": { "set": "$payload.status" },
                    "assignee": { "set": "$payload.assignee" },
                    "project": { "set": "$payload.project" },
                    "priority": { "set": "$payload.priority" }
                },
                "pre": []
            },
            "set-title": {
                "payload": { "title": "string" },
                "writes": { "title": { "set": "$payload.title" } },
                "pre": []
            },
            "set-status": {
                "payload": { "status": "string" },
                "writes": { "status": { "set": "$payload.status" } },
                "pre": []
            },
            "set-assignee": {
                "payload": { "assignee": "string" },
                "writes": { "assignee": { "set": "$payload.assignee" } },
                "pre": []
            },
            "set-project": {
                "payload": { "project": "string" },
                "writes": { "project": { "set": "$payload.project" } },
                "pre": []
            },
            "set-priority": {
                "payload": { "priority": "number" },
                "writes": { "priority": { "set": "$payload.priority" } },
                "pre": []
            },
            "set-tags": {
                "payload": { "tags": "string[]" },
                "writes": { "tags": { "set": "$payload.tags" } },
                "pre": []
            }
        }
    })
}

/// The JSON definition of the `account@1` model.
pub fn account_def() -> serde_json::Value {
    json!({
        "name": "account",
        "version": "1",
        "fields": {
            "currency": "string",
            "balance": "number"
        },
        "reducers": {
            "init": {
                "payload": {
                    "name": "string",
                    "currency": "string",
                    "balance": "number"
                },
                "writes": {
                    "__name__": { "set": "$payload.name" },
                    "currency": { "set": "$payload.currency" },
                    "balance": { "set": "$payload.balance" }
                },
                "pre": []
            },
            "set-balance": {
                "payload": { "balance": "number" },
                "writes": { "balance": { "set": "$payload.balance" } },
                "pre": []
            }
        }
    })
}

/// The JSON definition of the `transaction@1` model.
pub fn transaction_def() -> serde_json::Value {
    json!({
        "name": "transaction",
        "version": "1",
        "fields": {
            "amount": "number",
            "category": "string",
            "account": "string",
            "date": "string"
        },
        "reducers": {
            "init": {
                "payload": {
                    "name": "string",
                    "amount": "number",
                    "category": "string",
                    "account": "string",
                    "date": "string"
                },
                "writes": {
                    "__name__": { "set": "$payload.name" },
                    "amount": { "set": "$payload.amount" },
                    "category": { "set": "$payload.category" },
                    "account": { "set": "$payload.account" },
                    "date": { "set": "$payload.date" }
                },
                "pre": []
            },
            "set-amount": {
                "payload": { "amount": "number" },
                "writes": { "amount": { "set": "$payload.amount" } },
                "pre": []
            }
        }
    })
}

/// The JSON definition of the `invoice@1` model: an invoice with a status
/// lifecycle (draft -> sent -> accepted -> paid). The reference processor
/// example listens to `set-status` and fires when `status` becomes `accepted`.
pub fn invoice_def() -> serde_json::Value {
    json!({
        "name": "invoice",
        "version": "1",
        "fields": {
            "number": "string",
            "customer": "string",
            "total": "number",
            "currency": "string",
            "status": "string"
        },
        "reducers": {
            "init": {
                "payload": {
                    "name": "string",
                    "number": "string",
                    "customer": "string",
                    "total": "number",
                    "currency": "string",
                    "status": "string"
                },
                "writes": {
                    "__name__": { "set": "$payload.name" },
                    "number": { "set": "$payload.number" },
                    "customer": { "set": "$payload.customer" },
                    "total": { "set": "$payload.total" },
                    "currency": { "set": "$payload.currency" },
                    "status": { "set": "$payload.status" }
                },
                "pre": []
            },
            "set-status": {
                "payload": { "status": "string" },
                "writes": { "status": { "set": "$payload.status" } },
                "pre": []
            },
            "set-customer": {
                "payload": { "customer": "string" },
                "writes": { "customer": { "set": "$payload.customer" } },
                "pre": []
            },
            "set-total": {
                "payload": { "total": "number" },
                "writes": { "total": { "set": "$payload.total" } },
                "pre": []
            }
        }
    })
}

/// Every realistic model, as ready-to-register [`L1`] interpreters.
pub fn realistic_models() -> Vec<L1> {
    vec![
        L1::from_def(project_def()).expect("the project definition is well-formed"),
        L1::from_def(task_def()).expect("the task definition is well-formed"),
        L1::from_def(account_def()).expect("the account definition is well-formed"),
        L1::from_def(transaction_def()).expect("the transaction definition is well-formed"),
        L1::from_def(invoice_def()).expect("the invoice definition is well-formed"),
        L1::from_def(folder_def()).expect("the folder definition is well-formed"),
        L1::from_def(note_def()).expect("the note definition is well-formed"),
    ]
}

/// Relationship declarations: `(model name, field name) -> target model
/// name`. The read-model layer uses this to turn a relationship field
/// (a value that is another doc's name) into a graph edge, without the
/// reducer knowing anything about querying.
pub fn realistic_relationships() -> BTreeMap<(String, String), String> {
    let mut m = BTreeMap::new();
    m.insert(
        ("task".to_string(), "project".to_string()),
        "project".to_string(),
    );
    m.insert(
        ("transaction".to_string(), "account".to_string()),
        "account".to_string(),
    );
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Model;

    #[test]
    fn each_model_reduces_its_init() {
        for def in [project_def(), task_def(), account_def(), transaction_def()] {
            let name = def
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap()
                .to_string();
            let l1 = L1::from_def(def).unwrap();
            let r = l1.ref_();
            assert_eq!(r.name, name, "{name} model name");
            assert_eq!(r.version, "1");
        }
    }

    #[test]
    fn relationships_declare_the_realistic_edges() {
        let rels = realistic_relationships();
        assert_eq!(
            rels.get(&("task".to_string(), "project".to_string())),
            Some(&"project".to_string())
        );
        assert_eq!(
            rels.get(&("transaction".to_string(), "account".to_string())),
            Some(&"account".to_string())
        );
    }
}
