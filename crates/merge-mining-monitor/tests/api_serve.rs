//! HTTP-level smoke tests for the read API, driven through
//! `tower::ServiceExt::oneshot` (no network bind, no live database).
//!
//! Query parser edge cases live with `mmm-api::query`; these tests keep only
//! enough coverage to prove the router returns the shared envelopes before DB
//! checkout and that valid API routes reach the DB-backed path.

use axum::Router;
use axum::body::Body;
use axum::http::header::{CACHE_CONTROL, IF_MODIFIED_SINCE, LAST_MODIFIED};
use axum::http::{HeaderMap, Request, StatusCode};
use mmm_api::{self as api, AppState};
use mmm_pg::PgConfig;
use serde_json::Value;
use tower::ServiceExt; // for `oneshot`

const SOURCE_FILTERED_ROUTES: [&str; 1] = ["/api/v1/tree"];

const FRONTEND_CSS_FILES: [&str; 10] = [
    "www/css/tokens.css",
    "www/css/shell.css",
    "www/css/dialogs.css",
    "www/css/about-version.css",
    "www/css/layout.css",
    "www/css/controls.css",
    "www/css/tree-frame.css",
    "www/css/drawer.css",
    "www/css/tree-svg.css",
    "www/css/responsive.css",
];

fn test_www_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../www")
}

fn test_state() -> AppState {
    // Dummy config that is never connected; deadpool connects lazily.
    let pg = PgConfig {
        host: "127.0.0.1".to_owned(),
        port: 1,
        user: "mmm".to_owned(),
        password: None,
        database: "mmm".to_owned(),
    };
    AppState::new(api::build_pool(&pg, 1).expect("build pool"))
}

fn app() -> Router {
    api::router(test_state(), test_www_dir())
}

async fn send(request: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let response = app().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, headers, bytes.to_vec())
}

async fn request(uri: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    send(Request::builder().uri(uri).body(Body::empty()).unwrap()).await
}

async fn request_if_modified_since(
    uri: &str,
    if_modified_since: &str,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    send(
        Request::builder()
            .uri(uri)
            .header(IF_MODIFIED_SINCE, if_modified_since)
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

async fn get(uri: &str) -> (StatusCode, Vec<u8>) {
    let (status, _, body) = request(uri).await;
    (status, body)
}

fn json_of(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).expect("response body is JSON")
}

fn cache_control(headers: &HeaderMap) -> &str {
    headers
        .get(CACHE_CONTROL)
        .expect("Cache-Control header")
        .to_str()
        .expect("Cache-Control is valid ASCII")
}

fn assert_frontend_stylesheet_order(html: &str) {
    let mut previous = 0;
    for file in FRONTEND_CSS_FILES {
        let href = file
            .strip_prefix("www")
            .expect("frontend css path is under www");
        let expected_href = if href == "/css/about-version.css" {
            "/css/about-version.css?v=20260702-footer-links"
        } else {
            href
        };
        let needle = format!(r#"<link rel="stylesheet" href="{expected_href}" />"#);
        let index = html.find(&needle).unwrap_or_else(|| {
            panic!("expected stylesheet link {needle:?} in HTML");
        });
        assert!(
            index >= previous,
            "{href} must be loaded after the preceding frontend stylesheet"
        );
        previous = index;
    }
    assert!(!html.contains("/css/app.css"));
}

fn assert_error_envelope(body: &[u8], expected_code: &str) {
    let value = json_of(body);
    assert_eq!(value["schema_version"], "v1");
    assert!(value["generated_at"].as_u64().unwrap() > 0);
    assert_eq!(value["error"]["code"], expected_code);
}

async fn assert_route_error(uri: &str, expected_status: StatusCode, expected_code: &str) {
    let (status, body) = get(uri).await;
    assert_eq!(status, expected_status, "{uri}");
    assert_error_envelope(&body, expected_code);
}

#[tokio::test]
async fn source_filtered_routes_use_the_shared_normalizer() {
    for route in SOURCE_FILTERED_ROUTES {
        let base = valid_query_prefix(route);
        assert_route_error(
            &format!("{route}?{base}source=bogus:namecoin"),
            StatusCode::BAD_REQUEST,
            "invalid_query",
        )
        .await;
        assert_route_error(
            &format!("{route}?{base}source=auxpow:not-a-registered-chain"),
            StatusCode::BAD_REQUEST,
            "unsupported_source",
        )
        .await;
    }
}

#[tokio::test]
async fn valid_or_queryless_routes_reach_the_db_path() {
    let anchor = "ab".repeat(32);
    for uri in [
        "/api/v1/sources?source=auxpow:syscoin".to_owned(),
        "/api/v1/navigator/stale".to_owned(),
        format!("/api/v1/navigator/stale?anchor_hash={anchor}"),
        "/api/v1/navigator/stale-branch".to_owned(),
        "/api/v1/navigator/orphan".to_owned(),
        "/api/v1/navigator/orphan?classification=strict_btc_orphan".to_owned(),
        "/api/v1/navigator/orphan-branch".to_owned(),
        "/api/v1/navigator/orphan-branch?classification=strict_btc_orphan".to_owned(),
        "/api/v1/tree".to_owned(),
        format!("/api/v1/tree?unheighted_anchor={anchor}"),
    ] {
        assert_route_error(&uri, StatusCode::INTERNAL_SERVER_ERROR, "internal_error").await;
    }
}

#[tokio::test]
async fn navigator_endpoint_validates_queries_before_db_checkout() {
    let bad_hash = "zz".repeat(32);
    let valid_hash = "ab".repeat(32);
    for (uri, status, code) in [
        (
            "/api/v1/navigator/orphan?limit=0".to_owned(),
            StatusCode::BAD_REQUEST,
            "invalid_query",
        ),
        (
            "/api/v1/navigator/orphan?limit=5000".to_owned(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "range_too_large",
        ),
        (
            "/api/v1/navigator/orphan?classification=bogus".to_owned(),
            StatusCode::BAD_REQUEST,
            "invalid_query",
        ),
        (
            format!("/api/v1/navigator/orphan?anchor_hash={bad_hash}"),
            StatusCode::BAD_REQUEST,
            "invalid_query",
        ),
        (
            "/api/v1/navigator/stale?limit=0".to_owned(),
            StatusCode::BAD_REQUEST,
            "invalid_query",
        ),
        (
            "/api/v1/navigator/stale?limit=5000".to_owned(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "range_too_large",
        ),
        (
            "/api/v1/navigator/stale?direction=older".to_owned(),
            StatusCode::BAD_REQUEST,
            "invalid_query",
        ),
        (
            format!(
                "/api/v1/navigator/stale?classification=strict_btc_orphan&anchor_hash={valid_hash}"
            ),
            StatusCode::BAD_REQUEST,
            "invalid_query",
        ),
    ] {
        assert_route_error(&uri, status, code).await;
    }
}

#[tokio::test]
async fn deleted_navigator_routes_are_not_served() {
    for uri in [
        "/api/v1/stales/page",
        "/api/v1/stale-branches",
        "/api/v1/orphans",
        "/api/v1/orphan-branches",
    ] {
        let (status, _) = get(uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn block_owns_the_invalid_hash_envelope() {
    let (status, body) = get("/api/v1/block/not-a-hash").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_error_envelope(&body, "invalid_hash");

    let (status, body) = get("/api/v1/block/%ff").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_error_envelope(&body, "invalid_hash");

    let (status, body) = get(&format!("/api/v1/block/{}", "ab".repeat(32))).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_error_envelope(&body, "internal_error");
}

#[tokio::test]
async fn static_index_is_served() {
    let (status, body) = get("/").await;
    assert_eq!(status, StatusCode::OK);
    let html = std::str::from_utf8(&body).expect("index html is utf-8");
    assert!(html.contains("app-shell"));
    assert!(html.contains("tree-card"));
    assert!(html.contains("Bitcoin Header Tree"));
    assert_frontend_stylesheet_order(html);
    assert!(html.contains(r#"<div id="kind-controls" class="stack-sm"></div>"#));
    assert!(html.contains(r#"<div id="info-dialogs"></div>"#));
    assert!(html.contains(r#"id="about-version""#));
    assert!(html.contains(r#"id="about-release-notes""#));
    assert!(html.contains(r#"name="treeHeight""#));
    assert!(html.contains(r#"name="treeTime""#));
    assert!(html.contains("/js/app.js"));
    assert!(html.contains("/vendor/d3.v7.min.js"));
}

#[tokio::test]
async fn cache_headers_match_public_launch_policy() {
    for uri in ["/", "/index.html"] {
        let (status, headers, _) = request(uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        let cache = cache_control(&headers);
        assert!(cache.contains("max-age=0"), "{uri}: {cache}");
        assert!(cache.contains("s-maxage=30"), "{uri}: {cache}");
        assert!(cache.contains("must-revalidate"), "{uri}: {cache}");
    }

    let (status, headers, _) = request("/js/app.js").await;
    assert_eq!(status, StatusCode::OK);
    let cache = cache_control(&headers);
    assert!(cache.contains("max-age=300"), "{cache}");
    assert!(cache.contains("s-maxage=3600"), "{cache}");
    assert!(cache.contains("stale-while-revalidate=86400"), "{cache}");
    assert!(!cache.contains("immutable"), "{cache}");

    let (status, headers, _) = request("/vendor/d3.v7.min.js").await;
    assert_eq!(status, StatusCode::OK);
    let cache = cache_control(&headers);
    assert!(cache.contains("max-age=31536000"), "{cache}");
    assert!(cache.contains("immutable"), "{cache}");

    let (status, headers, _) = request("/api/v1/version").await;
    assert_eq!(status, StatusCode::OK);
    let cache = cache_control(&headers);
    assert!(cache.contains("max-age=15"), "{cache}");
    assert!(cache.contains("s-maxage=30"), "{cache}");

    let (status, headers, _) = request("/missing-static-file.js").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(cache_control(&headers), "no-store");
}

#[tokio::test]
async fn conditional_static_revalidation_keeps_cache_policy() {
    let (status, headers, _) = request("/js/app.js").await;
    assert_eq!(status, StatusCode::OK);
    let last_modified = headers
        .get(LAST_MODIFIED)
        .expect("Last-Modified header on static asset")
        .to_str()
        .expect("Last-Modified is valid ASCII")
        .to_owned();

    let (status, headers, body) = request_if_modified_since("/js/app.js", &last_modified).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty());
    let cache = cache_control(&headers);
    assert!(cache.contains("max-age=300"), "{cache}");
    assert!(cache.contains("s-maxage=3600"), "{cache}");
    assert!(!cache.contains("no-store"), "{cache}");
}

#[tokio::test]
async fn liveness_and_readiness_routes_are_not_cached() {
    let (status, headers, body) = request("/health").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_empty());
    assert_eq!(cache_control(&headers), "no-store");

    let (status, headers, _) = request("/ready").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(cache_control(&headers), "no-store");
}

#[tokio::test]
async fn version_endpoint_serves_runtime_metadata_without_db_checkout() {
    let (status, body) = get("/api/v1/version").await;
    assert_eq!(status, StatusCode::OK);
    let value = json_of(&body);
    assert_eq!(value["schema_version"], "v1");
    assert!(value["generated_at"].as_u64().unwrap() > 0);
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(value["release_notes"]["source"], "RELEASE_NOTES.md");
    assert!(
        value["release_notes"]["release_count"]
            .as_u64()
            .is_some_and(|count| count > 0),
        "version endpoint reports total release-note sections"
    );
    assert!(value["release_notes"]["truncated"].is_boolean());
    let releases = value["release_notes"]["releases"]
        .as_array()
        .expect("release notes releases are an array");
    assert!(
        !releases.is_empty(),
        "version endpoint exposes release notes"
    );
    let current_release = releases
        .iter()
        .find(|release| release["version"] == env!("CARGO_PKG_VERSION"))
        .expect("release notes include the current Cargo package version");
    if let Some(date) = current_release["date"].as_str() {
        assert_eq!(date.len(), 10, "release dates use YYYY-MM-DD");
        assert_eq!(&date[4..5], "-");
        assert_eq!(&date[7..8], "-");
    }
    assert!(
        current_release["items"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        "current release note section includes items"
    );
}

#[tokio::test]
async fn tree_validates_queries_before_db_checkout() {
    let bad_anchor = "zz".repeat(32);
    let anchor = "ab".repeat(32);
    for (uri, status, code) in [
        (
            "/api/v1/tree?from_height=10&to_height=9".to_owned(),
            StatusCode::BAD_REQUEST,
            "invalid_query",
        ),
        (
            "/api/v1/tree?from_height=1&to_height=3000".to_owned(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "range_too_large",
        ),
        (
            "/api/v1/tree?from_height=1&to_height=2&include_near=yes".to_owned(),
            StatusCode::BAD_REQUEST,
            "invalid_query",
        ),
        (
            format!("/api/v1/tree?unheighted_anchor={bad_anchor}"),
            StatusCode::BAD_REQUEST,
            "invalid_query",
        ),
        (
            format!("/api/v1/tree?unheighted_anchor={anchor}&from_height=1&to_height=2"),
            StatusCode::BAD_REQUEST,
            "invalid_query",
        ),
    ] {
        assert_route_error(&uri, status, code).await;
    }
}

#[tokio::test]
async fn undecodable_query_string_is_invalid_query() {
    let (status, body) = get("/api/v1/tree?source=%ff").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_error_envelope(&body, "invalid_query");
}

fn valid_query_prefix(route: &str) -> &'static str {
    match route {
        "/api/v1/tree" => "from_height=1&to_height=2&",
        _ => "",
    }
}
