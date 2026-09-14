//! Installing a verified package.
//!
//! Install is where the separate checks meet, in a deliberate order:
//!
//! 1. **Integrity** — the signature matches the content ([`super::Manifest::verify`]).
//! 2. **Authenticity** — the publisher is one this operator accepted
//!    ([`super::trust::TrustStore`]). Never inferred, never automatic.
//! 3. **Completeness** — the UI bundle is actually present, so a plugin cannot
//!    be half-installed and then fail to load when someone opens it.
//! 4. **Registration** — models and processors take effect.
//!
//! Only then is the package recorded as installed. A failure at any step
//! leaves nothing behind: a package is installed or it is not.
//!
//! Nothing here prompts. The decision to trust a publisher belongs to the
//! caller (CLI or console), so that this module can be tested without a human
//! and so the prompt can be written once, properly, where the operator is.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::trust::TrustStore;
use super::Manifest;
use crate::blob::BlobStore;

/// A package recorded as installed on this node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstalledPackage {
    pub manifest: Manifest,
    /// RFC 3339 UTC.
    #[serde(default)]
    pub installed_at: String,
}

/// Why an install was refused.
///
/// Distinct variants rather than strings: the caller reacts differently to
/// each — an untrusted publisher is a prompt, a missing bundle is a fetch.
#[derive(Debug, PartialEq, Eq)]
pub enum InstallError {
    /// The signature does not match the content.
    Integrity(String),
    /// The signature is valid, but this publisher has not been accepted.
    UntrustedPublisher {
        key: String,
        name: String,
    },
    /// The UI bundle has not been fully fetched yet.
    BundleIncomplete {
        missing: usize,
    },
    /// A model or processor in the package is malformed.
    Invalid(String),
    Io(String),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Integrity(e) => write!(f, "package failed verification: {e}"),
            Self::UntrustedPublisher { key, name } => write!(
                f,
                "publisher {name} ({key}) is not trusted on this node; \
                 approve it before installing"
            ),
            Self::BundleIncomplete { missing } => write!(
                f,
                "the UI bundle is incomplete ({missing} chunk(s) still to fetch)"
            ),
            Self::Invalid(e) => write!(f, "package contents are invalid: {e}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

/// The set of installed packages, persisted.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Installed {
    #[serde(default)]
    packages: Vec<InstalledPackage>,
}

impl Installed {
    pub fn load(path: &Path) -> Self {
        let Ok(raw) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        serde_json::from_str(&raw).unwrap_or_else(|e| {
            tracing::error!("{} is unreadable ({e}); no packages loaded", path.display());
            Self::default()
        })
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let body = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, body).map_err(|e| format!("writing {}: {e}", path.display()))
    }

    pub fn list(&self) -> &[InstalledPackage] {
        &self.packages
    }

    pub fn get(&self, name: &str) -> Option<&InstalledPackage> {
        self.packages.iter().find(|p| p.manifest.name == name)
    }

    /// Adds or replaces by package NAME, not name+version: installing 1.1.0
    /// must supersede 1.0.0 rather than leave both mounted, which would make
    /// "which editor is serving this route" ambiguous.
    pub fn put(&mut self, pkg: InstalledPackage) {
        self.packages
            .retain(|p| p.manifest.name != pkg.manifest.name);
        self.packages.push(pkg);
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.packages.len();
        self.packages.retain(|p| p.manifest.name != name);
        self.packages.len() != before
    }

    /// Every bundle still referenced — what the chunk store must keep.
    pub fn live_blobs(&self) -> Vec<crate::blob::BlobRef> {
        self.packages
            .iter()
            .filter_map(|p| p.manifest.bundle.clone())
            .collect()
    }
}

/// Checks a package without installing it.
///
/// Separated so a caller can decide what to do about each failure — prompt for
/// trust, fetch a bundle — rather than being told only that install failed.
pub fn check(
    manifest: &Manifest,
    trust: &TrustStore,
    blobs: &BlobStore,
) -> Result<(), InstallError> {
    manifest
        .verify()
        .map_err(|e| InstallError::Integrity(e.to_string()))?;

    if !trust.is_trusted(&manifest.publisher_key) {
        return Err(InstallError::UntrustedPublisher {
            key: manifest.publisher_key.clone(),
            name: manifest.publisher.name.clone(),
        });
    }

    // Sidebar entries are console chrome, so they are checked before install
    // rather than filtered at render time: a package that asks for something
    // it may not have is refused whole.
    manifest
        .ui
        .validate()
        .map_err(|e| InstallError::Invalid(format!("sidebar: {e}")))?;

    if let Some(bundle) = &manifest.bundle {
        let missing = blobs.missing(bundle).len();
        if missing > 0 {
            return Err(InstallError::BundleIncomplete { missing });
        }
    }

    // Fail before registering anything if a definition is malformed: a
    // partially registered package is worse than a refused one.
    for def in &manifest.document_models {
        crate::model::l1::L1::from_def(def.clone())
            .map_err(|e| InstallError::Invalid(format!("model definition: {e}")))?;
    }
    Ok(())
}

/// Installs a checked package: registers its models and records it.
///
/// `models_file` receives the definitions so they survive a restart — without
/// that, the package's documents replay empty, which is the failure this
/// codebase already learned once.
pub fn install(
    manifest: &Manifest,
    trust: &TrustStore,
    blobs: &BlobStore,
    store: &crate::store::Store,
    installed_file: &Path,
    models_file: &Path,
) -> Result<InstalledPackage, InstallError> {
    check(manifest, trust, blobs)?;

    for def in &manifest.document_models {
        let m = crate::model::l1::L1::from_def(def.clone())
            .map_err(|e| InstallError::Invalid(format!("model definition: {e}")))?;
        store.add_model(std::sync::Arc::new(m));
        crate::model::persist::save(models_file, def).map_err(InstallError::Io)?;
    }

    let pkg = InstalledPackage {
        manifest: manifest.clone(),
        installed_at: crate::status::rfc3339_now(),
    };
    let mut all = Installed::load(installed_file);
    all.put(pkg.clone());
    all.save(installed_file).map_err(InstallError::Io)?;
    Ok(pkg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{Capabilities, PublisherInfo};
    use ed25519_dalek::SigningKey;
    use serde_json::json;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn ticket_model() -> serde_json::Value {
        json!({
            "name": "ticket", "version": "1",
            "fields": { "title": "string" },
            "reducers": { "init": {
                "payload": { "name": "string", "title": "string" },
                "writes": {
                    "__name__": { "set": "$payload.name" },
                    "title": { "set": "$payload.title" }
                }
            } }
        })
    }

    fn manifest(k: &SigningKey, models: Vec<serde_json::Value>) -> Manifest {
        let mut m = Manifest {
            name: "@powerhousedao/achra".into(),
            version: "1.0.0".into(),
            description: "d".into(),
            category: "c".into(),
            publisher: PublisherInfo {
                name: "Powerhouse".into(),
                url: "https://powerhouse.inc/".into(),
            },
            publisher_key: hex::encode(k.verifying_key().to_bytes()),
            document_models: models,
            processors: vec![],
            bundle: None,
            capabilities: Capabilities::default(),
            ui: Default::default(),
            sig: String::new(),
        };
        m.sign(k);
        m
    }

    struct Env {
        _dir: tempfile::TempDir,
        blobs: BlobStore,
        store: std::sync::Arc<crate::store::Store>,
        installed_file: std::path::PathBuf,
        models_file: std::path::PathBuf,
    }

    fn env() -> Env {
        let dir = tempfile::tempdir().expect("tmp");
        let blobs = BlobStore::open(&dir.path().join("blobs")).expect("blobs");
        let store = crate::store::Store::open(
            &dir.path().join("docs"),
            &SigningKey::from_bytes(&[5u8; 32]),
            "origin",
        )
        .expect("store");
        Env {
            installed_file: dir.path().join("packages.json"),
            models_file: dir.path().join("models.json"),
            _dir: dir,
            blobs,
            store,
        }
    }

    fn trusting(m: &Manifest) -> TrustStore {
        let mut t = TrustStore::default();
        t.trust(&m.publisher_key, &m.publisher.name);
        t
    }

    /// The headline rule: a valid signature is not permission to install.
    #[test]
    fn an_untrusted_publisher_is_refused_even_with_a_valid_signature() {
        let e = env();
        let m = manifest(&key(1), vec![ticket_model()]);
        assert!(m.verify().is_ok(), "the signature itself is fine");

        let err = check(&m, &TrustStore::default(), &e.blobs).expect_err("must refuse");
        assert!(matches!(err, InstallError::UntrustedPublisher { .. }));
        assert!(err.to_string().contains("not trusted"));
    }

    #[test]
    fn a_tampered_package_is_refused_before_trust_is_even_considered() {
        let e = env();
        let mut m = manifest(&key(1), vec![ticket_model()]);
        let trust = trusting(&m);
        m.description = "now malicious".into(); // after signing

        let err = check(&m, &trust, &e.blobs).expect_err("must refuse");
        assert!(
            matches!(err, InstallError::Integrity(_)),
            "integrity is checked first, so a trusted publisher cannot mask tampering"
        );
    }

    #[test]
    fn installing_registers_the_models_and_persists_them() {
        let e = env();
        let m = manifest(&key(1), vec![ticket_model()]);
        let trust = trusting(&m);

        install(
            &m,
            &trust,
            &e.blobs,
            &e.store,
            &e.installed_file,
            &e.models_file,
        )
        .expect("install");

        // Usable immediately.
        e.store
            .create_doc_model(
                "t-1",
                &crate::doc::ModelRef::new("ticket", "1"),
                &json!({ "name": "t-1", "title": "works" }),
            )
            .expect("the package's model is registered");

        // And durable: without this the documents replay empty on restart.
        let persisted = crate::model::persist::load_all(&e.models_file);
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0]["name"], "ticket");

        let installed = Installed::load(&e.installed_file);
        assert_eq!(installed.list().len(), 1);
        assert_eq!(installed.list()[0].manifest.name, "@powerhousedao/achra");
        assert!(!installed.list()[0].installed_at.is_empty());
    }

    /// A bundle with genuinely distinct chunks. Uniform bytes would make every
    /// chunk identical and deduplicate to one, which is correct behaviour but
    /// tests nothing about counting what is missing.
    fn distinct_bundle(chunks: usize) -> Vec<u8> {
        (0..crate::blob::CHUNK_BYTES * chunks)
            .map(|i| ((i / crate::blob::CHUNK_BYTES) as u8).wrapping_add((i % 251) as u8))
            .collect()
    }

    #[test]
    fn a_package_whose_bundle_is_not_fetched_yet_is_refused() {
        let e = env();
        let mut m = manifest(&key(1), vec![]);
        // Describe a bundle without storing its chunks.
        let data = distinct_bundle(2);
        let bundle = crate::blob::BlobRef::of(&data);
        assert_ne!(bundle.chunks[0], bundle.chunks[1], "fixture must differ");
        m.bundle = Some(bundle);
        m.sign(&key(1));
        let trust = trusting(&m);

        let err = check(&m, &trust, &e.blobs).expect_err("must refuse");
        match err {
            InstallError::BundleIncomplete { missing } => assert_eq!(missing, 2),
            other => panic!("expected BundleIncomplete, got {other:?}"),
        }
    }

    /// Repeated content deduplicates, so a bundle of identical chunks needs
    /// only one fetch. Worth pinning: it is why a fixture of uniform bytes
    /// reports one missing chunk rather than several.
    #[test]
    fn identical_chunks_count_as_one_fetch() {
        let e = env();
        let mut m = manifest(&key(1), vec![]);
        m.bundle = Some(crate::blob::BlobRef::of(&vec![
            7u8;
            crate::blob::CHUNK_BYTES * 4
        ]));
        m.sign(&key(1));
        let trust = trusting(&m);

        match check(&m, &trust, &e.blobs).expect_err("must refuse") {
            InstallError::BundleIncomplete { missing } => {
                assert_eq!(missing, 1, "four identical chunks are one fetch")
            }
            other => panic!("expected BundleIncomplete, got {other:?}"),
        }
    }

    #[test]
    fn a_fetched_bundle_satisfies_the_completeness_check() {
        let e = env();
        let data = distinct_bundle(2);
        let bundle = e.blobs.put(&data).expect("store bundle");
        let mut m = manifest(&key(1), vec![]);
        m.bundle = Some(bundle);
        m.sign(&key(1));
        let trust = trusting(&m);
        assert!(check(&m, &trust, &e.blobs).is_ok());
    }

    /// Nothing may be registered when any part of the package is bad.
    #[test]
    fn a_malformed_model_refuses_the_whole_package() {
        let e = env();
        let m = manifest(&key(1), vec![json!({ "not": "a model" })]);
        let trust = trusting(&m);

        let err = install(
            &m,
            &trust,
            &e.blobs,
            &e.store,
            &e.installed_file,
            &e.models_file,
        )
        .expect_err("must refuse");
        assert!(matches!(err, InstallError::Invalid(_)));
        assert!(
            Installed::load(&e.installed_file).list().is_empty(),
            "a refused package must leave nothing recorded"
        );
    }

    #[test]
    fn reinstalling_a_newer_version_supersedes_the_old_one() {
        let e = env();
        let k = key(1);
        let mut v1 = manifest(&k, vec![ticket_model()]);
        let trust = trusting(&v1);
        v1.sign(&k);
        install(
            &v1,
            &trust,
            &e.blobs,
            &e.store,
            &e.installed_file,
            &e.models_file,
        )
        .expect("install v1");

        let mut v2 = manifest(&k, vec![ticket_model()]);
        v2.version = "2.0.0".into();
        v2.sign(&k);
        install(
            &v2,
            &trust,
            &e.blobs,
            &e.store,
            &e.installed_file,
            &e.models_file,
        )
        .expect("install v2");

        let installed = Installed::load(&e.installed_file);
        assert_eq!(installed.list().len(), 1, "one entry per package name");
        assert_eq!(installed.list()[0].manifest.version, "2.0.0");
    }

    #[test]
    fn removing_a_package_frees_its_bundle_for_collection() {
        let e = env();
        let data = vec![3u8; crate::blob::CHUNK_BYTES + 5];
        let bundle = e.blobs.put(&data).expect("bundle");
        let mut m = manifest(&key(1), vec![]);
        m.bundle = Some(bundle.clone());
        m.sign(&key(1));
        let trust = trusting(&m);
        install(
            &m,
            &trust,
            &e.blobs,
            &e.store,
            &e.installed_file,
            &e.models_file,
        )
        .expect("install");

        let mut installed = Installed::load(&e.installed_file);
        assert_eq!(installed.live_blobs().len(), 1, "the bundle is referenced");
        assert!(installed.remove("@powerhousedao/achra"));
        assert!(installed.live_blobs().is_empty());

        // With nothing referencing it, gc reclaims the chunks.
        let removed = e.blobs.gc(&installed.live_blobs()).expect("gc");
        assert_eq!(removed, bundle.chunks.len());
    }
}
