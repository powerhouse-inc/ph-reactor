//! `drive` — a folder tree, as an ordinary document in a space.
//!
//! Lifted out of `group@1` for the same reason as [`super::chat`]: a drive was
//! a privileged feature of the container, and everything a space contains
//! should be an app instead.
//!
//! `items` is a flat array of `{ name, kind, model, parent }`; `parent` names
//! the enclosing folder or is null at the root, so the tree is a tree without
//! needing nested state. Members add; managers remove -- both decided by the
//! space, through `space-member`.

use serde_json::json;

use crate::model::l1::L1;

pub fn drive_def() -> serde_json::Value {
    json!({
        "name": "drive",
        "version": "1",
        "$comment": "A folder tree. `kind` is 'folder' (a container, no separate \
                     document) or 'doc' (a real document of `model`). Removal \
                     recomputes the whole array and sets it, which converges \
                     under the canonical fold.",
        "fields": {
            "items": "array"
        },
        "reducers": {
            "init": {
                "payload": { "name": "string" },
                "writes": {
                    "__name__": { "set": "$payload.name" },
                    "items": { "set": [] }
                },
                "pre": [ { "space-member": true } ]
            },
            "add-item": {
                "payload": { "item": "object" },
                "writes": { "items": { "append": "$payload.item" } },
                "pre": [ { "space-member": true } ]
            },
            "remove-item": {
                "payload": { "items": "array" },
                "writes": { "items": { "set": "$payload.items" } },
                "pre": [ { "space-member": "managers" } ]
            }
        }
    })
}

pub fn drive() -> L1 {
    L1::from_def(drive_def()).expect("the built-in drive definition is well-formed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Model;

    #[test]
    fn the_definition_loads() {
        assert_eq!(drive().ref_().name, "drive");
    }

    /// Adding and removing answer to different lists on the space -- one
    /// mechanism, two lists, rather than two precondition kinds.
    #[test]
    fn adding_is_for_members_and_removing_is_for_managers() {
        let m = drive();
        assert_eq!(
            m.requires_space_member("add-item").as_deref(),
            Some("members")
        );
        assert_eq!(
            m.requires_space_member("remove-item").as_deref(),
            Some("managers")
        );
    }
}
