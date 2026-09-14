//! `chat` — a channel, as an ordinary document in a space.
//!
//! This used to be four parallel arrays inside `group@1`. That made
//! conversation a *privileged feature of the container*: no plugin could add
//! anything like it, the group model had to grow a field for every feature
//! anyone wanted, and every post was a write to the one document that also
//! held the membership list.
//!
//! As a document it is none of those things. A channel is a document in a
//! space, exactly like an invoice or an RFP, and Chat is an app enabled in a
//! space, exactly like Achra. That is what "no privileged core" means in
//! practice, and it is the difference between a suite and a pile of plugins.
//!
//! Who may post is not stored here. It is the space's `members` list, checked
//! by the store through the `space-member` precondition -- so membership is
//! edited in one place and inherited everywhere, which is the entire promise
//! of a space.

use serde_json::json;

use crate::model::l1::L1;

pub fn chat_def() -> serde_json::Value {
    json!({
        "name": "chat",
        "version": "1",
        "$comment": "A channel. Messages are parallel arrays appended by one \
                     action, so they stay the same length and in order. Who may \
                     post comes from the space, not from this document.",
        "fields": {
            "channel": "string",
            "msg_from": "string[]",
            "msg_text": "string[]",
            "msg_ts": "number[]"
        },
        "reducers": {
            "init": {
                "payload": { "name": "string", "channel": "string" },
                "writes": {
                    "__name__": { "set": "$payload.name" },
                    "channel": { "set": "$payload.channel" },
                    "msg_from": { "set": [] },
                    "msg_text": { "set": [] },
                    "msg_ts": { "set": [] }
                },
                "pre": [ { "space-member": true } ]
            },
            "post": {
                "payload": { "text": "string" },
                "writes": {
                    "msg_from": { "append": "$actor" },
                    "msg_text": { "append": "$payload.text" },
                    "msg_ts": { "append": "$ts" }
                },
                "pre": [ { "space-member": true } ]
            }
        }
    })
}

pub fn chat() -> L1 {
    L1::from_def(chat_def()).expect("the built-in chat definition is well-formed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Model;

    #[test]
    fn the_definition_loads() {
        assert_eq!(chat().ref_().name, "chat");
    }

    /// Posting is gated by the *space*, not by a list kept here. If this ever
    /// stops being true, chat has started reinventing membership again.
    #[test]
    fn posting_defers_to_the_space() {
        let m = chat();
        assert_eq!(m.requires_space_member("post").as_deref(), Some("members"));
        assert!(
            !chat_def()["fields"]
                .as_object()
                .unwrap()
                .contains_key("members"),
            "a channel must not carry its own member list"
        );
    }
}
