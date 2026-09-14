//! Serialization for `com.canonical.dbusmenu`.
//!
//! Pure functions over the [`MenuItem`] tree: no D-Bus connection, no locks,
//! so the wire shape can be unit-tested. The interface methods in
//! [`super`] are thin wrappers around these.
//!
//! The shape was verified against a live host rather than inferred: dumping
//! `GetLayout` from a working tray item yields
//! `(u revision, (i id, a{sv} props, av children))`, where each child variant
//! wraps the same `(ia{sv}av)` structure and the root has id 0 with
//! `children-display: submenu`.

use std::collections::HashMap;

use zbus::zvariant::{StructureBuilder, Type, Value};

use super::menu::MenuItem;

/// One node of the layout, with the spec's `(ia{sv}av)` signature.
///
/// `children` are variants rather than a nested `LayoutItem` array because the
/// spec types them `av`; the recursion therefore goes through [`Value`].
#[derive(Debug, serde::Serialize, Type)]
pub struct LayoutItem<'a> {
    pub id: i32,
    pub props: HashMap<String, Value<'a>>,
    pub children: Vec<Value<'a>>,
}

/// `-1` means "every level" in `GetLayout`.
const UNLIMITED: i32 = -1;

/// Builds the layout for the subtree rooted at `parent_id`.
///
/// `depth` follows the spec: `-1` is unlimited, `0` yields the item with no
/// children, `n` yields `n` levels below it. `filter` restricts which item
/// properties are returned; empty means all of them.
pub fn layout(
    root: &MenuItem,
    parent_id: i32,
    depth: i32,
    filter: &[String],
) -> Option<LayoutItem<'static>> {
    let node = find(root, parent_id)?;
    Some(build(node, depth, filter))
}

fn build(item: &MenuItem, depth: i32, filter: &[String]) -> LayoutItem<'static> {
    let children = if depth == 0 {
        Vec::new()
    } else {
        let next = if depth == UNLIMITED {
            UNLIMITED
        } else {
            depth - 1
        };
        item.children
            .iter()
            .filter(|c| c.visible)
            .map(|c| into_value(build(c, next, filter)))
            .collect()
    };
    LayoutItem {
        id: item.id as i32,
        props: props_of(item, filter),
        children,
    }
}

/// A `LayoutItem` as a variant, for nesting inside the `av` of its parent.
///
/// Takes ownership rather than cloning: `Value` is deliberately not `Clone`
/// (cloning one may allocate), and the recursion has no need to keep a copy.
fn into_value(item: LayoutItem<'static>) -> Value<'static> {
    Value::from(
        StructureBuilder::new()
            .add_field(item.id)
            .add_field(item.props)
            .add_field(item.children)
            .build(),
    )
}

/// The spec's item properties.
///
/// Defaults are omitted rather than sent explicitly — the spec says a missing
/// property takes its default, and hosts size their menus from what is
/// present. `enabled` and `visible` therefore appear only when false.
pub fn props_of(item: &MenuItem, filter: &[String]) -> HashMap<String, Value<'static>> {
    let mut out: HashMap<String, Value<'static>> = HashMap::new();
    let want = |k: &str| filter.is_empty() || filter.iter().any(|f| f == k);

    if item.kind == "separator" {
        if want("type") {
            out.insert("type".into(), Value::from("separator"));
        }
        // A separator carries nothing else; a label on one is a host-visible
        // artefact in some shells.
        return out;
    }

    if want("label") {
        out.insert("label".into(), Value::from(item.label.clone()));
    }
    if !item.enabled && want("enabled") {
        out.insert("enabled".into(), Value::from(false));
    }
    if !item.visible && want("visible") {
        out.insert("visible".into(), Value::from(false));
    }
    if let Some(icon) = &item.icon {
        if want("icon-name") {
            out.insert("icon-name".into(), Value::from(icon.clone()));
        }
    }
    if !item.children.is_empty() && want("children-display") {
        out.insert("children-display".into(), Value::from("submenu"));
    }
    out
}

/// Finds an item by id anywhere in the tree.
pub fn find(root: &MenuItem, id: i32) -> Option<&MenuItem> {
    if root.id as i32 == id {
        return Some(root);
    }
    root.children.iter().find_map(|c| find(c, id))
}

/// `GetGroupProperties`: properties for each requested id, skipping ids that
/// no longer exist (the menu may have been rebuilt under the host).
pub fn group_properties(
    root: &MenuItem,
    ids: &[i32],
    filter: &[String],
) -> Vec<(i32, HashMap<String, Value<'static>>)> {
    ids.iter()
        .filter_map(|id| find(root, *id).map(|item| (*id, props_of(item, filter))))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{DriveStatusEntry, StatusSnapshot};
    use crate::tray::menu;
    use zbus::zvariant::Signature;

    fn snap() -> StatusSnapshot {
        let mut s = StatusSnapshot::empty(
            "1.0.0".into(),
            "/ip4/0.0.0.0/tcp/25422".into(),
            "http://127.0.0.1:4002".into(),
        );
        s.reactor.running = true;
        s.reactor.healthy = true;
        s.drives.push(DriveStatusEntry {
            name: "vault".into(),
            addr: "/ip4/10.0.0.2/tcp/25422/p2p/12D3KooWg8111".into(),
            paused: false,
            status: "synced".into(),
            detail: "ok".into(),
        });
        s
    }

    /// The single most important assertion here: a wrong signature is
    /// invisible in Rust and only shows up as a host silently refusing to
    /// render the menu.
    #[test]
    fn layout_item_signature_matches_the_spec() {
        assert_eq!(
            LayoutItem::signature(),
            Signature::try_from("(ia{sv}av)").expect("valid signature")
        );
    }

    #[test]
    fn root_is_id_zero_and_advertises_a_submenu() {
        let root = menu::build_menu(&snap());
        let l = layout(&root, 0, UNLIMITED, &[]).expect("root layout");
        assert_eq!(l.id, 0);
        assert!(
            l.props.contains_key("children-display"),
            "a host will not descend into a root without children-display"
        );
        assert!(!l.children.is_empty());
    }

    #[test]
    fn depth_zero_returns_the_item_without_children() {
        let root = menu::build_menu(&snap());
        let l = layout(&root, 0, 0, &[]).expect("root layout");
        assert!(l.children.is_empty(), "depth 0 must not recurse");
    }

    #[test]
    fn depth_one_returns_only_the_first_level() {
        let root = menu::build_menu(&snap());
        let full = layout(&root, 0, UNLIMITED, &[]).expect("full");
        let one = layout(&root, 0, 1, &[]).expect("one level");
        assert_eq!(one.children.len(), full.children.len());
        // Depth 1 means children exist but grandchildren do not; the Drives
        // submenu is the deep one in this fixture.
        let deep = menu::find_by_label(&root, "Drives").expect("drives node");
        assert!(!deep.children.is_empty(), "fixture should have a submenu");
        let drives_at_depth_1 = layout(&root, deep.id as i32, 1, &[]).expect("drives");
        assert!(!drives_at_depth_1.children.is_empty());
    }

    #[test]
    fn unknown_parent_id_yields_nothing() {
        let root = menu::build_menu(&snap());
        assert!(layout(&root, 9999, UNLIMITED, &[]).is_none());
    }

    #[test]
    fn separators_carry_a_type_and_nothing_else() {
        let root = menu::build_menu(&snap());
        let sep = root
            .children
            .iter()
            .find(|c| c.kind == "separator")
            .expect("a separator");
        let p = props_of(sep, &[]);
        assert_eq!(p.get("type"), Some(&Value::from("separator")));
        assert!(!p.contains_key("label"));
    }

    #[test]
    fn defaults_are_omitted_but_non_defaults_are_sent() {
        let root = menu::build_menu(&snap());
        let header = &root.children[0];
        assert!(!header.enabled, "fixture header should be disabled");
        let p = props_of(header, &[]);
        assert_eq!(p.get("enabled"), Some(&Value::from(false)));

        let enabled = menu::find_by_label(&root, "Quit ph-reactor").expect("quit");
        let p = props_of(enabled, &[]);
        assert!(
            !p.contains_key("enabled"),
            "enabled is the default and must be omitted"
        );
    }

    #[test]
    fn property_filter_is_honoured() {
        let root = menu::build_menu(&snap());
        let quit = menu::find_by_label(&root, "Quit ph-reactor").expect("quit");
        let p = props_of(quit, &["label".to_string()]);
        assert!(p.contains_key("label"));
        assert!(!p.contains_key("icon-name"), "filtered out");
    }

    #[test]
    fn group_properties_skips_ids_that_no_longer_exist() {
        let root = menu::build_menu(&snap());
        let quit = menu::find_by_label(&root, "Quit ph-reactor").expect("quit");
        let got = group_properties(&root, &[quit.id as i32, 9999], &[]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, quit.id as i32);
    }

    #[test]
    fn invisible_children_are_not_serialized() {
        let mut root = menu::build_menu(&snap());
        let before = layout(&root, 0, UNLIMITED, &[])
            .expect("before")
            .children
            .len();
        root.children[0].visible = false;
        let after = layout(&root, 0, UNLIMITED, &[])
            .expect("after")
            .children
            .len();
        assert_eq!(after, before - 1);
    }
}
