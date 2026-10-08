use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use conductor_remote::contract::{AppState, ConductorStatus, StateResponse, Token};
use conductor_remote::http::{embedded_assets, router};
use conductor_remote::testing::{FakeConductor, MemoryAssets};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const TOKEN: &str = "secret-token";

fn app_with(conductor: Arc<FakeConductor>, assets: MemoryAssets) -> Router {
    router(AppState {
        token: Arc::new(Token::new(TOKEN)),
        conductor,
        assets: Arc::new(assets),
        reads: None,
        writes: None,
        notify: None,
        services: Default::default(),
    })
}

fn web_assets() -> MemoryAssets {
    MemoryAssets::default()
        .with(
            "index.html",
            "text/html; charset=utf-8",
            b"<html>app</html>",
        )
        .with("assets/app.js", "text/javascript", b"console.log(1)")
}

fn app() -> (Router, Arc<FakeConductor>) {
    let conductor = Arc::new(FakeConductor::new(ConductorStatus::Running));
    (app_with(conductor.clone(), web_assets()), conductor)
}

fn authed(method: Method, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap()
}

fn plain(method: Method, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

async fn text(response: Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn json_body(response: Response) -> Value {
    serde_json::from_str(&text(response).await).unwrap()
}

fn header_of(response: &Response, name: header::HeaderName) -> &str {
    response.headers().get(name).unwrap().to_str().unwrap()
}

#[tokio::test]
async fn request_without_token_is_unauthorized_with_exact_body() {
    let (app, _) = app();
    let response = app.oneshot(plain(Method::GET, "/api/state")).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(text(response).await, r#"{"error":"unauthorized"}"#);
}

#[tokio::test]
async fn wrong_token_of_the_same_length_is_unauthorized() {
    let (app, _) = app();
    let request = Request::builder()
        .uri("/api/state")
        .header(header::AUTHORIZATION, "Bearer secret-tokeN")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(text(response).await, r#"{"error":"unauthorized"}"#);
}

#[tokio::test]
async fn bearer_header_is_accepted() {
    let (app, _) = app();
    let response = app
        .oneshot(authed(Method::GET, "/api/state"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn token_query_parameter_is_accepted() {
    let (app, _) = app();
    let response = app
        .oneshot(plain(Method::GET, "/api/state?token=secret-token"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn wrong_token_query_parameter_is_unauthorized() {
    let (app, _) = app();
    let response = app
        .oneshot(plain(Method::GET, "/api/state?token=secret-tokeN"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn bare_api_path_is_gated() {
    let (app, _) = app();
    let response = app.oneshot(plain(Method::GET, "/api")).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn state_json_for_running_conductor() {
    let (app, _) = app();
    let response = app
        .oneshot(authed(Method::GET, "/api/state"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let expected = serde_json::to_value(StateResponse::skeleton(ConductorStatus::Running)).unwrap();
    assert_eq!(json_body(response).await, expected);
}

#[tokio::test]
async fn state_json_for_stopped_conductor() {
    let conductor = Arc::new(FakeConductor::new(ConductorStatus::NotRunning));
    let app = app_with(conductor, web_assets());
    let response = app
        .oneshot(authed(Method::GET, "/api/state"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let expected =
        serde_json::to_value(StateResponse::skeleton(ConductorStatus::NotRunning)).unwrap();
    assert_eq!(json_body(response).await, expected);
}

#[tokio::test]
async fn launch_success_returns_ok_and_calls_the_fake_once() {
    let (app, conductor) = app();
    let response = app
        .oneshot(authed(Method::POST, "/api/conductor/launch"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(text(response).await, r#"{"ok":true}"#);
    assert_eq!(conductor.launches(), 1);
}

#[tokio::test]
async fn launch_failure_returns_500_with_the_error_text() {
    let (app, conductor) = app();
    conductor.fail_launch_with("open exited with status 1");
    let response = app
        .oneshot(authed(Method::POST, "/api/conductor/launch"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        json_body(response).await,
        json!({"ok": false, "error": "could not launch Conductor: open exited with status 1"})
    );
}

#[tokio::test]
async fn unknown_api_path_with_token_is_not_found_json() {
    let (app, _) = app();
    let response = app.oneshot(authed(Method::GET, "/api/nope")).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(text(response).await, r#"{"error":"not found"}"#);
}

#[tokio::test]
async fn unknown_api_path_without_token_is_unauthorized() {
    let (app, _) = app();
    let response = app.oneshot(plain(Method::GET, "/api/nope")).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(text(response).await, r#"{"error":"unauthorized"}"#);
}

#[tokio::test]
async fn post_to_state_is_not_found() {
    let (app, _) = app();
    let response = app
        .oneshot(authed(Method::POST, "/api/state"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(text(response).await, r#"{"error":"not found"}"#);
}

#[tokio::test]
async fn api_responses_are_json_and_not_stored() {
    for (request, status) in [
        (plain(Method::GET, "/api/state"), StatusCode::UNAUTHORIZED),
        (authed(Method::GET, "/api/state"), StatusCode::OK),
        (authed(Method::GET, "/api/nope"), StatusCode::NOT_FOUND),
        (
            authed(Method::POST, "/api/conductor/launch"),
            StatusCode::OK,
        ),
    ] {
        let (app, _) = app();
        let is_state_read = status == StatusCode::OK && request.uri().path() == "/api/state";
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), status);
        if is_state_read {
            // The state read answers through the shared envelope: revalidated, with an etag.
            assert_eq!(header_of(&response, header::CACHE_CONTROL), "no-cache");
            assert_eq!(
                header_of(&response, header::CONTENT_TYPE),
                "application/json; charset=utf-8"
            );
            assert!(response.headers().contains_key(header::ETAG));
            continue;
        }
        assert_eq!(header_of(&response, header::CACHE_CONTROL), "no-store");
        assert_eq!(
            header_of(&response, header::CONTENT_TYPE),
            "application/json"
        );
    }
}

#[tokio::test]
async fn root_serves_index_html_with_no_cache() {
    let (app, _) = app();
    let response = app.oneshot(plain(Method::GET, "/")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_of(&response, header::CACHE_CONTROL), "no-cache");
    assert_eq!(
        header_of(&response, header::CONTENT_TYPE),
        "text/html; charset=utf-8"
    );
    assert_eq!(text(response).await, "<html>app</html>");
}

#[tokio::test]
async fn hashed_asset_is_immutable_with_its_content_type() {
    let (app, _) = app();
    let response = app
        .oneshot(plain(Method::GET, "/assets/app.js"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_of(&response, header::CACHE_CONTROL),
        "public, max-age=31536000, immutable"
    );
    assert_eq!(
        header_of(&response, header::CONTENT_TYPE),
        "text/javascript"
    );
    assert_eq!(text(response).await, "console.log(1)");
}

#[tokio::test]
async fn unknown_page_path_serves_index_html() {
    let (app, _) = app();
    let response = app
        .oneshot(plain(Method::GET, "/workspaces/42"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_of(&response, header::CACHE_CONTROL), "no-cache");
    assert_eq!(text(response).await, "<html>app</html>");
}

#[tokio::test]
async fn missing_asset_under_assets_is_not_found() {
    let (app, _) = app();
    let response = app
        .oneshot(plain(Method::GET, "/assets/gone.js"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(header_of(&response, header::CACHE_CONTROL), "no-cache");
    assert_eq!(text(response).await, "not found");
}

#[tokio::test]
async fn missing_index_html_is_not_found() {
    let conductor = Arc::new(FakeConductor::new(ConductorStatus::Running));
    let app = app_with(conductor, MemoryAssets::default());
    let response = app.oneshot(plain(Method::GET, "/anything")).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(text(response).await, "not found");
}

#[tokio::test]
async fn parent_directory_path_is_forbidden() {
    let (app, _) = app();
    let response = app
        .oneshot(plain(Method::GET, "/assets/../secret"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn percent_encoded_parent_directory_path_is_forbidden() {
    let (app, _) = app();
    let response = app
        .oneshot(plain(Method::GET, "/assets/%2e%2e/secret"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn post_to_root_is_method_not_allowed() {
    let (app, _) = app();
    let response = app.oneshot(plain(Method::POST, "/")).await.unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn head_serves_headers_without_a_body() {
    let (app, _) = app();
    let response = app.oneshot(plain(Method::HEAD, "/")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_of(&response, header::CACHE_CONTROL), "no-cache");
    assert_eq!(text(response).await, "");
}

#[test]
fn embedded_assets_return_none_for_a_path_that_does_not_exist() {
    assert!(embedded_assets().get("no/such/file.bin").is_none());
}
