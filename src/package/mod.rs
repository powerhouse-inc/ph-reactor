//! Plugin packages: the manifest, its signature, and publisher trust.
//!
//! A package bundles document models, processors and a UI so a network can
//! ship an application — Achra, a knowledge vault — to the nodes on it.
//!
//! The format is Powerhouse's `powerhouse.manifest.json`, adopted rather than
//! reinvented so a ph-reactor plugin *is* a Powerhouse package. Two things are
//! added, because their format does not have them:
//!
//! - **`publisher_key` + `sig`.** Their `publisher` is `{name, url}` —
//!   descriptive metadata that anyone can copy. Provenance needs a key.
//! - **`capabilities`.** What the plugin's UI may read and write, declared up
//!   front and shown to the operator at install time.
//!
//! One deliberate deviation: `documentModels` carries the full model
//! *definitions*, not `{id, name}` references. ph-reactor registers models
//! from their definitions, and a reference to something fetched elsewhere would
//! be a hole in exactly the provenance this module exists to establish.
//!
//! See docs/superpowers/specs/2026-09-14-plugin-packages-design.md.

pub mod install;
pub mod trust;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::blob::BlobRef;

/// Human-facing publisher metadata, as Powerhouse defines it.
///
/// Descriptive only — anyone can write "Powerhouse" here. [`Manifest::publisher_key`]
/// is what actually identifies a publisher.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublisherInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url: String,
}

/// What a plugin's UI is permitted to do.
///
/// The bridge refuses anything not listed here. This narrows what a plugin may
/// *attempt*; it never widens what the store permits, because a write still
/// passes the model's own `auth`, `pre` and quorum rules afterwards.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Models the UI may query, as `name@version`.
    #[serde(default)]
    pub read: Vec<String>,
    /// Reducers the UI may invoke.
    #[serde(default)]
    pub write: Vec<WriteCapability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteCapability {
    pub model: String,
    pub kinds: Vec<String>,
}

impl Capabilities {
    pub fn may_read(&self, model: &str) -> bool {
        self.read.iter().any(|m| m == model)
    }

    pub fn may_write(&self, model: &str, kind: &str) -> bool {
        self.write
            .iter()
            .any(|w| w.model == model && w.kinds.iter().any(|k| k == kind))
    }

    /// A plain-language summary for the install prompt.
    ///
    /// An operator cannot make a trust decision about a JSON blob, so the
    /// prompt shows this instead.
    pub fn describe(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.read.is_empty() {
            out.push(format!("read documents of type: {}", self.read.join(", ")));
        }
        for w in &self.write {
            out.push(format!("perform {} on {}", w.kinds.join(", "), w.model));
        }
        if out.is_empty() {
            out.push("nothing — this plugin requests no access".into());
        }
        out
    }
}


/// One sidebar entry a plugin asks the console to show.
///
/// These land among the console's own navigation, so they are trusted chrome
/// and [`PluginUi::validate`] is what keeps them honest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavItem {
    /// What the sidebar shows.
    pub label: String,
    /// A short glyph. Kept tiny deliberately: an icon slot wide enough for
    /// arbitrary text is a second label, and a second label is a place to
    /// write something misleading.
    #[serde(default)]
    pub icon: String,
    /// Which of the plugin's own views to open. Passed through to the editor
    /// in the handshake; empty means its default view.
    #[serde(default)]
    pub view: String,
}

/// The console chrome a plugin asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginUi {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nav: Vec<NavItem>,
}

impl PluginUi {
    /// Whether this asks for nothing, so it can be omitted from the signed
    /// bytes entirely. See the note on [`Manifest::ui`].
    pub fn is_empty(&self) -> bool {
        self.nav.is_empty()
    }
}

/// Names a plugin may not take.
///
/// The console's own items, and the product name. Without this a package could
/// add a second "Settings" that opens a page it controls — and because plugin
/// entries sit among the built-ins with no separating heading, that entry would
/// look exactly like the real one.
const RESERVED_NAV_LABELS: &[&str] = &[
    "home", "groups", "plugins", "settings", "profile", "reactor", "documents", "types",
    "folders", "overview", "spaces", "space",
];

/// At most this many entries per plugin. A sidebar is a shared surface; one
/// package should not be able to fill it.
pub const MAX_NAV_ITEMS: usize = 3;

impl PluginUi {
    /// Checks the entries before they are ever rendered.
    ///
    /// Called during install, so a package with a bad declaration is refused
    /// whole rather than installed and then partially trusted.
    pub fn validate(&self) -> Result<(), String> {
        if self.nav.len() > MAX_NAV_ITEMS {
            return Err(format!(
                "a plugin may add at most {MAX_NAV_ITEMS} sidebar entries, this one asks for {}",
                self.nav.len()
            ));
        }
        let mut seen: Vec<String> = Vec::new();
        for item in &self.nav {
            let label = item.label.trim();
            if label.is_empty() {
                return Err("a sidebar entry needs a label".into());
            }
            if label.chars().count() > 24 {
                return Err(format!("sidebar label too long (max 24): {label:?}"));
            }
            // Control characters and line breaks have no business in a label,
            // and are the shape of an attempt to break out of it.
            if label.chars().any(|c| c.is_control()) {
                return Err(format!("sidebar label contains control characters: {label:?}"));
            }
            let folded = label.to_lowercase();
            if RESERVED_NAV_LABELS.contains(&folded.as_str()) {
                return Err(format!(
                    "\"{label}\" is a reserved sidebar name — a plugin may not impersonate the console's own navigation"
                ));
            }
            if seen.contains(&folded) {
                return Err(format!("duplicate sidebar entry: {label:?}"));
            }
            seen.push(folded);

            if item.icon.chars().count() > 2 {
                return Err(format!("sidebar icon must be at most 2 characters: {:?}", item.icon));
            }
            if item.icon.chars().any(|c| c.is_control()) {
                return Err("sidebar icon contains control characters".into());
            }
            if item.view.chars().count() > 32
                || item
                    .view
                    .chars()
                    .any(|c| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
            {
                return Err(format!(
                    "a view name may only be letters, digits, - and _ (max 32): {:?}",
                    item.view
                ));
            }
        }
        Ok(())
    }

    /// A plain-language line for the install prompt.
    ///
    /// Adding to the console's own navigation is a grant, so the operator is
    /// told about it in the same place they are told about data access.
    pub fn describe(&self) -> Option<String> {
        if self.nav.is_empty() {
            return None;
        }
        let names: Vec<&str> = self.nav.iter().map(|n| n.label.as_str()).collect();
        Some(format!(
            "add {} item{} to your sidebar: {}",
            names.len(),
            if names.len() == 1 { "" } else { "s" },
            names.join(", ")
        ))
    }
}

/// A package manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub publisher: PublisherInfo,
    /// The publisher's ed25519 public key, hex-encoded. This, not
    /// `publisher.name`, is the identity a trust decision is made about.
    pub publisher_key: String,
    /// Full model definitions, registered on install.
    #[serde(default, rename = "documentModels")]
    pub document_models: Vec<Value>,
    #[serde(default)]
    pub processors: Vec<Value>,
    /// The UI bundle, delivered as content-addressed chunks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<BlobRef>,
    #[serde(default)]
    pub capabilities: Capabilities,
    /// Console chrome the plugin asks for — sidebar entries. Signed with the
    /// rest, so what appears in the navigation is what the publisher vouched
    /// for and the operator approved.
    ///
    /// `skip_serializing_if` is load-bearing, not tidiness. `message_bytes`
    /// serializes this whole struct, so a field that always appears changes the
    /// bytes every past signature was made over — and every package published
    /// before the field existed stops verifying. That is exactly what happened
    /// when this one was added: already-published packages went from "valid" to
    /// "bad signature" in the console, with nothing wrong with them.
    ///
    /// So: every optional field added from here on MUST omit itself when empty.
    /// `a_manifest_signed_before_a_field_existed_still_verifies` holds the line.
    #[serde(default, skip_serializing_if = "PluginUi::is_empty")]
    pub ui: PluginUi,
    /// ed25519 signature over [`Manifest::message_bytes`], hex-encoded.
    #[serde(default)]
    pub sig: String,
}

impl Manifest {
    /// The canonical bytes a publisher signs.
    ///
    /// The signature is excluded — a signature cannot cover itself — exactly as
    /// `Action::message_bytes` and `InviteToken::message_bytes` do. Serde's
    /// `Map` is a sorted `BTreeMap`, so the JSON is canonical and the same
    /// manifest produces the same bytes on every node.
    pub fn message_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.sig = String::new();
        serde_json::to_vec(&clone).expect("a manifest serializes")
    }

    pub fn sign(&mut self, key: &SigningKey) {
        let mb = self.message_bytes();
        self.sig = hex::encode(key.sign(&mb).to_bytes());
    }

    /// Verifies the signature against the key the manifest itself names.
    ///
    /// This establishes **integrity** — nobody altered the package after it was
    /// signed. It says nothing about whether that publisher should be trusted,
    /// which is a separate and deliberately human decision; see
    /// [`trust::TrustStore`]. Conflating the two is the classic supply-chain
    /// mistake.
    pub fn verify(&self) -> Result<(), String> {
        let key_bytes = hex::decode(&self.publisher_key)
            .map_err(|e| format!("publisher_key is not hex: {e}"))?;
        let key_arr: [u8; 32] = key_bytes
            .try_into()
            .map_err(|_| "publisher_key must be 32 bytes".to_string())?;
        let vk = VerifyingKey::from_bytes(&key_arr)
            .map_err(|e| format!("publisher_key is not a valid ed25519 key: {e}"))?;

        let sig_bytes = hex::decode(&self.sig).map_err(|e| format!("signature is not hex: {e}"))?;
        let sig_arr: [u8; 64] = sig_bytes
            .try_into()
            .map_err(|_| "signature must be 64 bytes".to_string())?;

        vk.verify(&self.message_bytes(), &Signature::from_bytes(&sig_arr))
            .map_err(|_| "signature does not verify: the package was altered".to_string())
    }

    /// `name@version`, how a package is referred to.
    pub fn id(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn manifest(k: &SigningKey) -> Manifest {
        let mut m = Manifest {
            name: "@powerhousedao/achra".into(),
            version: "1.0.0".into(),
            description: "Marketplace for global coordination".into(),
            category: "Coordination".into(),
            publisher: PublisherInfo {
                name: "Powerhouse".into(),
                url: "https://powerhouse.inc/".into(),
            },
            publisher_key: hex::encode(k.verifying_key().to_bytes()),
            document_models: vec![json!({ "name": "rfp", "version": "1" })],
            processors: vec![],
            bundle: None,
            capabilities: Capabilities {
                read: vec!["rfp@1".into()],
                write: vec![WriteCapability {
                    model: "proposal@1".into(),
                    kinds: vec!["init".into(), "withdraw".into()],
                }],
            },
            ui: Default::default(),
            sig: String::new(),
        };
        m.sign(k);
        m
    }

    #[test]
    fn a_signed_manifest_verifies() {
        let k = key(1);
        assert!(manifest(&k).verify().is_ok());
    }

    /// The whole point of signing: content cannot change after publication.
    #[test]
    fn tampering_with_any_field_breaks_the_signature() {
        let k = key(1);
        for (label, mutate) in [
            (
                "name",
                Box::new(|m: &mut Manifest| m.name = "evil".into()) as Box<dyn Fn(&mut Manifest)>,
            ),
            (
                "models",
                Box::new(|m: &mut Manifest| m.document_models.push(json!({"name":"backdoor"}))),
            ),
            (
                "capabilities",
                Box::new(|m: &mut Manifest| m.capabilities.read.push("*".into())),
            ),
            (
                "version",
                Box::new(|m: &mut Manifest| m.version = "9.9.9".into()),
            ),
        ] {
            let mut m = manifest(&k);
            mutate(&mut m);
            assert!(
                m.verify().is_err(),
                "tampering with {label} must break the signature"
            );
        }
    }

    /// Swapping in another key does not help an attacker: the signature was
    /// made over bytes that include the key, so it no longer matches.
    #[test]
    fn substituting_the_publisher_key_breaks_verification() {
        let mut m = manifest(&key(1));
        m.publisher_key = hex::encode(key(2).verifying_key().to_bytes());
        assert!(m.verify().is_err());
    }

    /// Re-signing with a different key produces a VALID manifest — which is
    /// precisely why integrity is not enough, and why the trust store exists.
    #[test]
    fn anyone_can_produce_a_validly_signed_package() {
        let impostor = key(9);
        let mut m = manifest(&key(1));
        m.publisher_key = hex::encode(impostor.verifying_key().to_bytes());
        m.publisher.name = "Powerhouse".into(); // the label is not identity
        m.sign(&impostor);
        assert!(
            m.verify().is_ok(),
            "a self-consistent package verifies; only the trust store can \
             tell this is not the publisher you meant"
        );
    }

    #[test]
    fn an_unsigned_manifest_does_not_verify() {
        let mut m = manifest(&key(1));
        m.sig = String::new();
        assert!(m.verify().is_err());
    }

    #[test]
    fn message_bytes_exclude_the_signature() {
        let k = key(1);
        let m = manifest(&k);
        let mut without = m.clone();
        without.sig = "deadbeef".into();
        assert_eq!(
            m.message_bytes(),
            without.message_bytes(),
            "the signature must not be part of what it covers"
        );
    }

    #[test]
    fn capability_checks_are_exact() {
        let c = Capabilities {
            read: vec!["rfp@1".into()],
            write: vec![WriteCapability {
                model: "proposal@1".into(),
                kinds: vec!["init".into()],
            }],
        };
        assert!(c.may_read("rfp@1"));
        assert!(!c.may_read("proposal@1"), "read is not implied by write");
        assert!(
            !c.may_read("rfp@2"),
            "a different version is a different model"
        );
        assert!(c.may_write("proposal@1", "init"));
        assert!(!c.may_write("proposal@1", "accept"), "kind must match");
        assert!(!c.may_write("rfp@1", "init"), "model must match");
    }

    #[test]
    fn capabilities_describe_themselves_in_plain_language() {
        let c = Capabilities {
            read: vec!["rfp@1".into()],
            write: vec![WriteCapability {
                model: "proposal@1".into(),
                kinds: vec!["init".into(), "withdraw".into()],
            }],
        };
        let lines = c.describe();
        assert!(lines.iter().any(|l| l.contains("read documents")));
        assert!(lines.iter().any(|l| l.contains("init, withdraw")));

        // A plugin asking for nothing must say so, not render an empty list.
        assert_eq!(Capabilities::default().describe().len(), 1);
        assert!(Capabilities::default().describe()[0].contains("nothing"));
    }

    /// A manifest must survive the round trip it will actually take: written
    /// into a document, synced, parsed on another node, and verified there.
    #[test]
    fn a_manifest_survives_json_round_trip_and_still_verifies() {
        let m = manifest(&key(1));
        let wire = serde_json::to_string(&m).expect("serialize");
        let back: Manifest = serde_json::from_str(&wire).expect("deserialize");
        assert_eq!(back, m);
        assert!(back.verify().is_ok());
    }

    #[test]
    fn a_reasonable_sidebar_declaration_is_accepted() {
        let ui = PluginUi {
            nav: vec![
                NavItem { label: "Achra".into(), icon: "\u{25c6}".into(), view: String::new() },
                NavItem { label: "My proposals".into(), icon: "\u{25a4}".into(), view: "proposals".into() },
            ],
        };
        assert!(ui.validate().is_ok());
        assert_eq!(
            ui.describe().expect("describes itself"),
            "add 2 items to your sidebar: Achra, My proposals"
        );
    }

    /// The entries sit among the console's own navigation with no separating
    /// heading, so a plugin that could call itself "Settings" would be
    /// indistinguishable from the real thing. It cannot.
    #[test]
    fn a_plugin_may_not_impersonate_the_consoles_own_navigation() {
        for name in [
            "Settings", "settings", "  Settings  ", "SETTINGS", "Profile", "Groups", "Home",
            "Plugins", "Reactor",
        ] {
            let ui = PluginUi {
                nav: vec![NavItem { label: name.into(), icon: String::new(), view: String::new() }],
            };
            assert!(
                ui.validate().is_err(),
                "{name:?} must be refused as a sidebar label"
            );
        }
    }

    #[test]
    fn a_sidebar_label_must_be_a_label() {
        let bad = [
            ("", "empty"),
            ("   ", "whitespace only"),
            ("Se\u{0}ttings", "a NUL"),
            ("two\nlines", "a line break"),
            ("an extremely long label that goes well past the limit", "too long"),
        ];
        for (label, why) in bad {
            let ui = PluginUi {
                nav: vec![NavItem { label: label.into(), icon: String::new(), view: String::new() }],
            };
            assert!(ui.validate().is_err(), "must reject {why}: {label:?}");
        }
    }

    #[test]
    fn one_plugin_cannot_fill_the_sidebar() {
        let item = |n: usize| NavItem {
            label: format!("Item {n}"),
            icon: String::new(),
            view: String::new(),
        };
        let ok = PluginUi { nav: (0..MAX_NAV_ITEMS).map(item).collect() };
        assert!(ok.validate().is_ok());
        let too_many = PluginUi { nav: (0..MAX_NAV_ITEMS + 1).map(item).collect() };
        assert!(too_many.validate().is_err());
    }

    #[test]
    fn duplicate_entries_are_refused() {
        let ui = PluginUi {
            nav: vec![
                NavItem { label: "Achra".into(), icon: String::new(), view: String::new() },
                NavItem { label: "achra".into(), icon: String::new(), view: "x".into() },
            ],
        };
        assert!(ui.validate().is_err(), "two entries that read the same are one too many");
    }

    /// A view name reaches a URL fragment and the handshake, so it is kept to
    /// a conservative alphabet rather than trusted to be harmless.
    #[test]
    fn a_view_name_is_a_plain_identifier() {
        for view in ["../admin", "a b", "x<script>", "'", &"v".repeat(33)] {
            let ui = PluginUi {
                nav: vec![NavItem { label: "Achra".into(), icon: String::new(), view: view.into() }],
            };
            assert!(ui.validate().is_err(), "must reject view {view:?}");
        }
        for view in ["", "proposals", "my-work", "tab_2"] {
            let ui = PluginUi {
                nav: vec![NavItem { label: "Achra".into(), icon: String::new(), view: view.into() }],
            };
            assert!(ui.validate().is_ok(), "must accept view {view:?}");
        }
    }

    #[test]
    fn a_plugin_asking_for_no_sidebar_says_nothing() {
        assert!(PluginUi::default().describe().is_none());
        assert!(PluginUi::default().validate().is_ok());
    }

    /// The sidebar declaration is covered by the signature, like everything
    /// else: a publisher vouches for what appears in the navigation.
    #[test]
    fn tampering_with_the_sidebar_breaks_the_signature() {
        let k = key(1);
        let mut m = manifest(&k);
        m.ui.nav.push(NavItem {
            label: "Free Money".into(),
            icon: "$".into(),
            view: String::new(),
        });
        assert!(m.verify().is_err());
    }

    /// Adding a field to this struct must not invalidate packages already
    /// published and signed. `message_bytes` covers the whole struct, so any
    /// field that always serializes changes the bytes every old signature was
    /// made over.
    ///
    /// Not hypothetical: adding `ui` did exactly this, and a package that had
    /// been installed happily showed up as "bad signature" the next time the
    /// daemon looked at it.
    #[test]
    fn a_manifest_signed_before_a_field_existed_still_verifies() {
        // A manifest as an older publisher wrote it: no `ui` key at all.
        let k = key(1);
        let mut older = manifest(&k);
        older.ui = PluginUi::default();
        older.sign(&k);
        let wire = serde_json::to_string(&older).expect("serialize");
        assert!(
            !wire.contains("\"ui\""),
            "an empty ui must not appear in the signed bytes, or old packages break: {wire}"
        );

        // Round-tripping through today's struct must still verify.
        let back: Manifest = serde_json::from_str(&wire).expect("deserialize");
        assert!(
            back.verify().is_ok(),
            "a package signed before `ui` existed must still verify"
        );
    }

    /// The rule above, enforced structurally: a manifest that asks for nothing
    /// optional must serialize only the keys an old publisher would have
    /// written. A new field without `skip_serializing_if` fails here.
    #[test]
    fn an_empty_manifest_serializes_only_its_required_keys() {
        let m = Manifest {
            name: "x".into(),
            version: "1".into(),
            description: String::new(),
            category: String::new(),
            publisher: PublisherInfo::default(),
            publisher_key: String::new(),
            document_models: vec![],
            processors: vec![],
            bundle: None,
            capabilities: Capabilities::default(),
            ui: PluginUi::default(),
            sig: String::new(),
        };
        let v: serde_json::Value = serde_json::to_value(&m).expect("serialize");
        let keys: Vec<&str> = v.as_object().expect("object").keys().map(String::as_str).collect();
        let expected = [
            "capabilities",
            "category",
            "description",
            "documentModels",
            "name",
            "processors",
            "publisher",
            "publisher_key",
            "sig",
            "version",
        ];
        assert_eq!(
            keys, expected,
            "a new optional field must omit itself when empty, or it \
             invalidates every signature made before it existed"
        );
    }
}
