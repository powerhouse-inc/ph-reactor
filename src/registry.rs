//! Powerhouse package registry checks (`GET <registry>/<name>`, the npm
//! metadata endpoint). The registry is the same npm-compatible one the
//! switchboard's `HttpPackageLoader` fetches document-model packages from
//! (`<registry>/-/cdn/<spec>/node/...`); `https://registry.dev.vetra.io`
//! by default.

use anyhow::{bail, Context, Result};
use std::sync::LazyLock;
use std::time::Duration;

static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("client builds")
});

/// Metadata for a registry package (the fields the daemon displays).
#[derive(Debug, Clone, PartialEq)]
pub struct PackageInfo {
    pub name: String,
    /// The registry's `latest` dist-tag version, when present.
    pub latest: Option<String>,
}

/// `true`-ish check: resolves `<registry>/<name>` (npm metadata) and
/// returns the package's name + latest version. Fails when the package
/// does not exist in the registry (404) or the registry is unreachable.
pub async fn check_package(registry: &str, name: &str) -> Result<PackageInfo> {
    let base = registry.trim_end_matches('/');
    // Scoped names are addressed as `/@scope/name`; most registries also
    // accept the url-encoded form, which we use to be safe.
    let path = if name.starts_with('@') {
        let encoded = name.replace('/', "%2f");
        format!("{base}/{encoded}")
    } else {
        format!("{base}/{name}")
    };
    let resp = HTTP
        .get(&path)
        .send()
        .await
        .with_context(|| format!("GET {path}"))?;
    let status = resp.status();
    if status.as_u16() == 404 || status.as_u16() == 403 {
        bail!("package {name} not found in {base}");
    }
    let resp = match resp.error_for_status() {
        Ok(r) => r,
        Err(_) => bail!("GET {path} answered {status}"),
    };
    let v: serde_json::Value = resp.json().await?;
    let latest = v["dist-tags"]["latest"].as_str().map(str::to_string);
    Ok(PackageInfo {
        name: v["name"].as_str().unwrap_or(name).to_string(),
        latest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::StatusCode, response::Response, routing::get, Router};
    use serde_json::json;
    use tokio::net::TcpListener;

    async fn start_mock() -> (String, tokio::task::JoinHandle<()>) {
        let handler = |path: axum::extract::Path<String>| async move {
            if path.ends_with("missing") || path.contains("%2fmissing") {
                return Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap();
            }
            // axum decodes %2f, so scoped names arrive as "@scope/name".
            let name = path.trim_start_matches('/').to_string();
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "name": name,
                        "dist-tags": {"latest": "6.2.3"},
                        "versions": {"6.2.3": {}},
                    })
                    .to_string(),
                ))
                .unwrap()
        };
        let app = Router::new().route("/*path", get(handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn resolves_package_from_registry() {
        let (base, _keep) = start_mock().await;
        let info = check_package(&base, "left-pad").await.unwrap();
        assert_eq!(info.name, "left-pad");
        assert_eq!(info.latest.as_deref(), Some("6.2.3"));

        let scoped = check_package(&base, "@powerhousedao/knowledge-note")
            .await
            .unwrap();
        assert_eq!(scoped.name, "@powerhousedao/knowledge-note");
        assert_eq!(scoped.latest.as_deref(), Some("6.2.3"));

        assert!(check_package(&base, "missing").await.is_err());
        assert!(check_package("http://127.0.0.1:1", "x").await.is_err());
    }
}
