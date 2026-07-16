//! Static file serving for the web dashboard.
//!
//! Serves the compiled `web/dist/` directory from the filesystem at runtime.
//! The directory path is configured via `gateway.web_dist_dir`.

use axum::{
    extract::State,
    http::{HeaderName, HeaderValue, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

use super::AppState;

#[cfg(feature = "embedded-web")]
use include_dir::{Dir, include_dir};

#[cfg(feature = "embedded-web")]
static EMBEDDED_WEB_DIST: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../web/dist");

const STATIC_RESOURCE_CSP: &str =
    "default-src 'none'; object-src 'none'; frame-ancestors 'none'; sandbox";

fn is_safe_static_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn apply_dashboard_security_headers(mut response: Response, csp: &str) -> Response {
    let headers = response.headers_mut();
    for (name, value) in [
        ("cross-origin-opener-policy", "same-origin"),
        ("cross-origin-resource-policy", "same-origin"),
        (
            "permissions-policy",
            "camera=(), microphone=(), geolocation=(), payment=(), usb=()",
        ),
        ("referrer-policy", "strict-origin-when-cross-origin"),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
    ] {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    if let Ok(value) = HeaderValue::from_str(csp) {
        headers.insert(HeaderName::from_static("content-security-policy"), value);
    }
    response
}

fn prepare_index_html(source: &str, path_prefix: &str, nonce: &str) -> String {
    let mut html = if path_prefix.is_empty() {
        source.to_string()
    } else {
        let json_prefix = serde_json::to_string(path_prefix)
            .unwrap_or_else(|_| "\"\"".to_string())
            .replace('<', "\\u003c")
            .replace('>', "\\u003e")
            .replace('&', "\\u0026");
        let html_prefix = path_prefix.replace('&', "&amp;");
        let script = format!("<script>window.__ZEROCLAW_BASE__={json_prefix};</script>");
        source
            .replace("/_app/", &format!("{html_prefix}/_app/"))
            .replace("<head>", &format!("<head>{script}"))
    };
    html = html.replace("<script>", &format!("<script nonce=\"{nonce}\">"));
    html
}

/// Serve static files from `/_app/*` path.
pub async fn handle_static(State(state): State<AppState>, uri: Uri) -> Response {
    let path = uri
        .path()
        .strip_prefix("/_app/")
        .unwrap_or(uri.path())
        .trim_start_matches('/');

    #[cfg(feature = "embedded-web")]
    if let Some(response) = serve_embedded_file(path) {
        return response;
    }

    serve_fs_file(state.web_dist_dir.as_ref(), path).await
}

/// Serve the SPA index for non-API, non-static GET requests.
pub async fn handle_spa_fallback(State(state): State<AppState>) -> Response {
    let Some(bytes) = load_index_html_bytes(state.web_dist_dir.as_ref()).await else {
        return apply_dashboard_security_headers(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Web dashboard not available. Build the frontend with `cargo web build` \
                 and point gateway.web_dist_dir at the resulting web/dist directory. \
                 The daemon API remains reachable independently of the dashboard.",
            )
                .into_response(),
            STATIC_RESOURCE_CSP,
        );
    };

    let html = String::from_utf8_lossy(&bytes);
    let nonce = Uuid::new_v4().simple().to_string();
    let html = prepare_index_html(&html, &state.path_prefix, &nonce);
    let csp = format!(
        "default-src 'self'; base-uri 'none'; object-src 'none'; frame-ancestors 'none'; \
         form-action 'self'; script-src 'self' 'nonce-{nonce}'; script-src-attr 'none'; \
         style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; \
         media-src 'self' data: blob:; connect-src 'self' ws: wss:; \
         frame-src 'self' data: blob:; worker-src 'self' blob:; manifest-src 'self'"
    );

    apply_dashboard_security_headers(
        (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8".to_string()),
                (header::CACHE_CONTROL, "no-cache".to_string()),
            ],
            html,
        )
            .into_response(),
        &csp,
    )
}

async fn load_index_html_bytes(dist_dir: Option<&PathBuf>) -> Option<Vec<u8>> {
    #[cfg(feature = "embedded-web")]
    if let Some(file) = EMBEDDED_WEB_DIST.get_file("index.html") {
        return Some(file.contents().to_vec());
    }

    let dir = dist_dir?;
    let index_path = dir.join("index.html");
    tokio::fs::read(&index_path).await.ok()
}

async fn serve_fs_file(dist_dir: Option<&PathBuf>, path: &str) -> Response {
    let Some(dir) = dist_dir else {
        return apply_dashboard_security_headers(
            (StatusCode::NOT_FOUND, "Not found").into_response(),
            STATIC_RESOURCE_CSP,
        );
    };

    if !is_safe_static_path(path) {
        return apply_dashboard_security_headers(
            (StatusCode::BAD_REQUEST, "Invalid path").into_response(),
            STATIC_RESOURCE_CSP,
        );
    }

    let file_path = dir.join(path);
    match tokio::fs::read(&file_path).await {
        Ok(content) => {
            let mime = mime_guess::from_path(path)
                .first_or_octet_stream()
                .to_string();
            let cache = if path.starts_with("assets/") {
                "public, max-age=31536000, immutable".to_string()
            } else {
                "no-cache".to_string()
            };

            apply_dashboard_security_headers(
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, cache)],
                    content,
                )
                    .into_response(),
                STATIC_RESOURCE_CSP,
            )
        }
        Err(_) => apply_dashboard_security_headers(
            (StatusCode::NOT_FOUND, "Not found").into_response(),
            STATIC_RESOURCE_CSP,
        ),
    }
}

#[cfg(feature = "embedded-web")]
fn serve_embedded_file(path: &str) -> Option<Response> {
    if !is_safe_static_path(path) {
        return Some(apply_dashboard_security_headers(
            (StatusCode::BAD_REQUEST, "Invalid path").into_response(),
            STATIC_RESOURCE_CSP,
        ));
    }

    let file = EMBEDDED_WEB_DIST.get_file(path)?;
    let mime = mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string();
    let cache = if path.starts_with("assets/") {
        "public, max-age=31536000, immutable".to_string()
    } else {
        "no-cache".to_string()
    };

    Some(apply_dashboard_security_headers(
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, cache)],
            file.contents().to_vec(),
        )
            .into_response(),
        STATIC_RESOURCE_CSP,
    ))
}

#[cfg(test)]
mod tests {
    use super::{is_safe_static_path, prepare_index_html};

    #[test]
    fn static_paths_reject_traversal_and_platform_separators() {
        assert!(is_safe_static_path("assets/app.js"));
        assert!(!is_safe_static_path("../config.toml"));
        assert!(!is_safe_static_path("assets\\app.js"));
        assert!(!is_safe_static_path("C:\\Windows\\win.ini"));
        assert!(!is_safe_static_path("assets/app.js\nheader"));
    }

    #[test]
    fn index_injection_uses_a_nonce_and_escapes_the_prefix() {
        let html = "<html><head></head><body><script>ready()</script><script type=\"module\" src=\"/_app/app.js\"></script></body></html>";
        let prepared = prepare_index_html(html, "/tenant&x", "nonce-value");

        assert!(prepared.contains("<script nonce=\"nonce-value\">ready()</script>"));
        assert!(prepared.contains("window.__ZEROCLAW_BASE__=\"/tenant\\u0026x\""));
        assert!(prepared.contains("/tenant&amp;x/_app/app.js"));
    }
}
