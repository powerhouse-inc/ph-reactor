//! The plugin asset server: a second listener that serves editor bundles and
//! nothing else.
//!
//! # Why a second listener
//!
//! The console API has no authentication — the bind address is its only
//! boundary. A plugin editor loaded same-origin could therefore drive
//! `/api/config`, `/api/drives` and `/api/quit` as the operator.
//!
//! Port is part of the browser's origin tuple, so `127.0.0.1:4003` is a
//! different origin from `127.0.0.1:4002` and the same-origin policy does the
//! enforcing for us. This router has **no API routes at all** and no handle on
//! the command channel or the store — the separation is structural, not a
//! filter someone can misconfigure.
//!
//! # Why a bundle is one file
//!
//! An editor bundle is a single self-contained HTML document, not an archive.
//! That is a deliberate simplification: there are no paths inside a bundle, so
//! there is no path traversal, no zip-slip, and no extraction step that could
//! write outside its directory. A build that inlines its JS and CSS produces
//! exactly this, and the multi-megabyte result is precisely what the chunked
//! blob transport exists to carry.
//!
//! See docs/superpowers/specs/2026-09-14-plugin-packages-design.md.

use std::sync::Arc;

use axum::extract::{Path as AxumPath, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;

use crate::blob::BlobStore;
use crate::package::install::Installed;
use crate::paths::StatePaths;

/// Everything the asset server is allowed to touch.
///
/// Note what is absent: no command channel, no store, no config. It can read
/// installed manifests and chunks, and that is all it can do.
#[derive(Clone)]
pub struct AssetState {
    pub paths: StatePaths,
    pub blobs: Arc<BlobStore>,
}

/// A Content-Security-Policy that keeps a plugin inside its own document.
///
/// `default-src 'none'` then allows back only what a self-contained bundle
/// needs. `connect-src 'none'` is the important one: even though the editor is
/// on a different origin, this stops it opening connections of its own, so the
/// capability bridge is the only way out.
const CSP: &str = "default-src 'none'; \
                   script-src 'unsafe-inline'; \
                   style-src 'unsafe-inline'; \
                   img-src data:; \
                   font-src data:; \
                   connect-src 'none'; \
                   form-action 'none'; \
                   base-uri 'none'; \
                   frame-ancestors *";

pub fn router(state: AssetState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/plugin/:name", get(bundle))
        .with_state(state)
}

/// Binds the asset listener.
pub async fn start(host: &str, port: u16, state: AssetState) -> Result<(), String> {
    let listener = TcpListener::bind((host, port))
        .await
        .map_err(|e| format!("bind {host}:{port} for plugin assets: {e}"))?;
    let app = router(state);
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::warn!("plugin asset server stopped: {e}");
        }
    });
    tracing::info!("plugin assets on http://{host}:{port}");
    Ok(())
}

/// Deliberately uninformative: this origin exists to serve bundles, and
/// enumerating what is installed is the console's job, on the console's origin.
async fn index() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "ph-reactor plugin assets",
    )
        .into_response()
}

/// Serves one installed package's editor bundle.
async fn bundle(State(st): State<AssetState>, AxumPath(name): AxumPath<String>) -> Response {
    let installed = Installed::load(&st.paths.packages_file());
    let Some(pkg) = installed.get(&name) else {
        return (StatusCode::NOT_FOUND, "no such package installed").into_response();
    };
    let Some(blob) = &pkg.manifest.bundle else {
        return (StatusCode::NOT_FOUND, "this package has no editor").into_response();
    };
    // get() verifies the reassembled bytes against the blob hash, so a bundle
    // altered on disk after install will not be served.
    let bytes = match st.blobs.get(blob) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("cannot serve bundle for {name}: {e}");
            return (StatusCode::SERVICE_UNAVAILABLE, "bundle unavailable").into_response();
        }
    };
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CONTENT_SECURITY_POLICY, CSP),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        bytes,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt; // for `oneshot`

    fn state_for(dir: &std::path::Path) -> AssetState {
        AssetState {
            paths: StatePaths::for_root(dir),
            blobs: Arc::new(BlobStore::open(&dir.join("blobs")).expect("blob store")),
        }
    }

    /// The whole security argument rests on this router having no API. Assert
    /// it, the way validate.sh asserts the console has no Ingress — a boundary
    /// nobody checks is a boundary that erodes.
    #[test]
    fn the_asset_router_exposes_no_api_route() {
        // Router does not expose its routes for inspection, so this guards the
        // source instead: any `/api` string in this file is a mistake.
        //
        // Only the part BEFORE the test module is scanned -- this test's own
        // filter mentions `.route(`, and without the split it matches itself,
        // which it duly did the first time it ran.
        let src = include_str!("assets.rs");
        let code = src.split("#[cfg(test)]").next().unwrap_or(src);
        let in_code: Vec<&str> = code
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && !t.starts_with("///") && !t.starts_with("//!")
            })
            .filter(|l| l.contains(".route("))
            .collect();
        assert_eq!(in_code.len(), 2, "exactly two routes: / and /plugin/:name");
        for line in in_code {
            assert!(
                !line.contains("/api"),
                "the asset server must never expose an API route: {line}"
            );
        }
    }

    /// `connect-src 'none'` is what stops a plugin talking to anything on its
    /// own, leaving the capability bridge as the only route out.
    #[test]
    fn the_csp_blocks_outbound_connections() {
        assert!(CSP.contains("default-src 'none'"));
        assert!(CSP.contains("connect-src 'none'"));
        assert!(CSP.contains("form-action 'none'"));
        assert!(
            CSP.contains("frame-ancestors *"),
            "the console must still be able to frame it"
        );
    }

    /// The claim this whole module rests on, asserted against the router
    /// itself rather than against the source: an editor on this origin cannot
    /// reach the console API, because the console API is not here.
    ///
    /// These are the paths that would matter most if it could: `/api/config`
    /// rewrites the node's configuration, `/api/drives` lists and creates
    /// drives, `/api/quit` stops the daemon.
    #[tokio::test]
    async fn the_console_api_is_unreachable_from_the_asset_origin() {
        let dir = tempfile::tempdir().expect("tmp");
        let app = router(state_for(dir.path()));

        for path in [
            "/api/config",
            "/api/drives",
            "/api/quit",
            "/api/status",
            "/api/plugins",
            "/api/groups",
            "/console",
            // Traversal back toward the console is a traversal attempt, not a
            // route, and must not be resolved into one.
            "/plugin/../api/config",
            "/plugin/..%2f..%2fapi%2fconfig",
        ] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("service call");
            assert_eq!(
                res.status(),
                StatusCode::NOT_FOUND,
                "{path} must not resolve on the asset origin"
            );
        }
    }

    /// A POST is the shape a capability call takes. The asset origin answers
    /// no method other than GET on the two routes it has, so an editor cannot
    /// submit an action here even to a path that does exist.
    #[tokio::test]
    async fn the_asset_origin_accepts_no_writes() {
        let dir = tempfile::tempdir().expect("tmp");
        let app = router(state_for(dir.path()));

        for (method, path) in [
            ("POST", "/plugin/achra"),
            ("POST", "/"),
            ("DELETE", "/plugin/achra"),
            ("POST", "/api/plugins/achra/action"),
        ] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("service call");
            assert!(
                res.status() == StatusCode::METHOD_NOT_ALLOWED
                    || res.status() == StatusCode::NOT_FOUND,
                "{method} {path} returned {}",
                res.status()
            );
        }
    }

    /// Asking for a package that is not installed must not leak anything, and
    /// must not be the path by which arbitrary files get read.
    #[tokio::test]
    async fn an_unknown_package_is_a_plain_not_found() {
        let dir = tempfile::tempdir().expect("tmp");
        let app = router(state_for(dir.path()));
        for name in ["nope", "..", "%2e%2e", "etc%2fpasswd"] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/plugin/{name}"))
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("service call");
            assert_eq!(
                res.status(),
                StatusCode::NOT_FOUND,
                "/plugin/{name} must be a plain 404"
            );
        }
    }

    /// Every served bundle carries the headers the isolation argument depends
    /// on. A bundle served without them is a bundle that can phone home.
    #[tokio::test]
    async fn a_served_bundle_carries_the_isolation_headers() {
        let dir = tempfile::tempdir().expect("tmp");
        let st = state_for(dir.path());

        // Install a minimal package whose bundle is a real blob.
        let body = b"<!doctype html><title>t</title>hello".to_vec();
        let blob = st.blobs.put(&body).expect("store bundle");
        let mut manifest = crate::package::Manifest {
            name: "achra".into(),
            version: "1.0.0".into(),
            description: String::new(),
            category: String::new(),
            publisher: Default::default(),
            publisher_key: String::new(),
            document_models: vec![],
            processors: vec![],
            bundle: Some(blob),
            capabilities: Default::default(),
            sig: String::new(),
        };
        let key = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]);
        manifest.publisher_key = hex::encode(key.verifying_key().to_bytes());
        manifest.sign(&key);

        let mut installed = Installed::default();
        installed.put(crate::package::install::InstalledPackage {
            manifest,
            installed_at: crate::status::rfc3339_now(),
        });
        installed
            .save(&st.paths.packages_file())
            .expect("save installed");

        let res = router(st)
            .oneshot(
                Request::builder()
                    .uri("/plugin/achra")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("service call");

        assert_eq!(res.status(), StatusCode::OK);
        let headers = res.headers();
        let csp = headers
            .get(header::CONTENT_SECURITY_POLICY)
            .expect("a bundle is always served with a CSP")
            .to_str()
            .expect("ascii");
        assert!(csp.contains("connect-src 'none'"));
        assert_eq!(
            headers
                .get(header::X_CONTENT_TYPE_OPTIONS)
                .map(|v| v.to_str().unwrap_or_default()),
            Some("nosniff")
        );

        let bytes = http_body_util::BodyExt::collect(res.into_body())
            .await
            .expect("body")
            .to_bytes();
        assert_eq!(bytes.as_ref(), body.as_slice(), "served verbatim");
    }
}
