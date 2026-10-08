use std::io::Read;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use conductor_remote::http::respond::{
    bytes_response, json_bytes_response, json_response, match_param,
};
use http_body_util::BodyExt;
use serde::ser::{Error as _, Serializer};
use serde::Serialize;
use serde_json::{json, Value};

async fn body_bytes(response: Response<Body>) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec()
}

fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.insert(*name, HeaderValue::from_str(value).expect("header value"));
    }
    map
}

fn header_str<'a>(response: &'a Response, name: &str) -> Option<&'a str> {
    response
        .headers()
        .get(name)
        .map(|v| v.to_str().expect("ascii"))
}

fn small() -> Value {
    json!({ "ok": true })
}

fn large() -> Value {
    json!({ "text": "conductor ".repeat(500) })
}

fn get(body: &impl Serialize, request: &HeaderMap) -> Response {
    json_response(request, &Method::GET, StatusCode::OK, body)
}

#[tokio::test]
async fn a_200_get_carries_the_read_headers() {
    let response = get(&small(), &HeaderMap::new());
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_str(&response, "content-type"),
        Some("application/json; charset=utf-8")
    );
    assert_eq!(header_str(&response, "cache-control"), Some("no-cache"));
    assert_eq!(header_str(&response, "vary"), Some("accept-encoding"));
    let etag = header_str(&response, "etag").expect("etag").to_string();
    assert!(etag.starts_with("W/\"") && etag.ends_with('"'), "{etag}");
    let inner = &etag[3..etag.len() - 1];
    assert!(!inner.is_empty());
    assert!(
        inner
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "base64url without padding: {inner}"
    );
    assert_eq!(
        inner.len(),
        27,
        "SHA-1 is 20 bytes, 27 base64url characters"
    );
    assert_eq!(body_bytes(response).await, br#"{"ok":true}"#);
}

#[tokio::test]
async fn the_etag_is_the_sha1_of_the_body() {
    // SHA-1("abc") = a9993e364706816aba3e25717850c26c9cd0d89d
    let response = json_bytes_response(
        &HeaderMap::new(),
        &Method::GET,
        StatusCode::OK,
        b"abc".to_vec(),
    );
    assert_eq!(
        header_str(&response, "etag"),
        Some("W/\"qZk-NkcGgWq6PiVxeFDCbJzQ2J0\"")
    );
}

#[tokio::test]
async fn the_etag_is_stable_for_equal_bodies_and_differs_for_different_ones() {
    let a = get(&json!({ "a": 1 }), &HeaderMap::new());
    let b = get(&json!({ "a": 1 }), &HeaderMap::new());
    let c = get(&json!({ "a": 2 }), &HeaderMap::new());
    assert_eq!(header_str(&a, "etag"), header_str(&b, "etag"));
    assert_ne!(header_str(&a, "etag"), header_str(&c, "etag"));
}

#[tokio::test]
async fn a_matching_if_none_match_answers_304_with_an_empty_body() {
    let first = get(&small(), &HeaderMap::new());
    let etag = header_str(&first, "etag").expect("etag").to_string();

    let response = get(&small(), &headers(&[("if-none-match", &etag)]));
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(header_str(&response, "etag"), Some(etag.as_str()));
    assert_eq!(header_str(&response, "cache-control"), Some("no-cache"));
    assert!(body_bytes(response).await.is_empty());
}

#[tokio::test]
async fn a_304_is_answered_before_compression_for_a_large_body() {
    let first = get(&large(), &HeaderMap::new());
    let etag = header_str(&first, "etag").expect("etag").to_string();

    let response = get(
        &large(),
        &headers(&[("if-none-match", &etag), ("accept-encoding", "gzip")]),
    );
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(header_str(&response, "content-encoding"), None);
    assert!(body_bytes(response).await.is_empty());
}

#[tokio::test]
async fn a_different_if_none_match_answers_the_full_body() {
    let response = get(&small(), &headers(&[("if-none-match", "W/\"other\"")]));
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await, br#"{"ok":true}"#);
}

#[tokio::test]
async fn a_small_body_is_not_compressed_even_when_gzip_is_accepted() {
    let response = get(&small(), &headers(&[("accept-encoding", "gzip, br")]));
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_str(&response, "content-encoding"), None);
    assert_eq!(body_bytes(response).await, br#"{"ok":true}"#);
}

#[tokio::test]
async fn a_body_of_exactly_1024_bytes_is_not_compressed() {
    let body = vec![b' '; 1024];
    let response = json_bytes_response(
        &headers(&[("accept-encoding", "gzip")]),
        &Method::GET,
        StatusCode::OK,
        body.clone(),
    );
    assert_eq!(header_str(&response, "content-encoding"), None);
    assert_eq!(body_bytes(response).await, body);
}

#[tokio::test]
async fn a_large_body_is_compressed_when_the_client_accepts_gzip() {
    let expected = serde_json::to_vec(&large()).expect("serialise");
    assert!(expected.len() > 1024);

    let plain = get(&large(), &HeaderMap::new());
    let etag = header_str(&plain, "etag").expect("etag").to_string();

    let response = get(&large(), &headers(&[("accept-encoding", "br, gzip;q=0.8")]));
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_str(&response, "content-encoding"), Some("gzip"));
    assert_eq!(header_str(&response, "vary"), Some("accept-encoding"));
    assert_eq!(
        header_str(&response, "etag"),
        Some(etag.as_str()),
        "the etag is of the uncompressed body"
    );
    assert_eq!(
        header_str(&response, "content-type"),
        Some("application/json; charset=utf-8")
    );

    let compressed = body_bytes(response).await;
    assert!(compressed.len() < expected.len());
    let mut decoded = Vec::new();
    flate2::read::GzDecoder::new(compressed.as_slice())
        .read_to_end(&mut decoded)
        .expect("gunzip");
    assert_eq!(decoded, expected);
}

#[tokio::test]
async fn a_large_body_is_not_compressed_when_the_client_does_not_accept_gzip() {
    let expected = serde_json::to_vec(&large()).expect("serialise");

    let none = get(&large(), &HeaderMap::new());
    assert_eq!(header_str(&none, "content-encoding"), None);
    assert_eq!(body_bytes(none).await, expected);

    let other = get(&large(), &headers(&[("accept-encoding", "br, deflate")]));
    assert_eq!(header_str(&other, "content-encoding"), None);
    assert_eq!(body_bytes(other).await, expected);
}

#[tokio::test]
async fn a_post_is_no_store_without_etag_or_compression() {
    let request = headers(&[("accept-encoding", "gzip")]);
    let response = json_response(&request, &Method::POST, StatusCode::OK, &large());
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_str(&response, "content-type"),
        Some("application/json; charset=utf-8")
    );
    assert_eq!(header_str(&response, "cache-control"), Some("no-store"));
    assert_eq!(header_str(&response, "etag"), None);
    assert_eq!(header_str(&response, "content-encoding"), None);
    assert_eq!(
        body_bytes(response).await,
        serde_json::to_vec(&large()).expect("serialise")
    );
}

#[tokio::test]
async fn a_404_is_no_store_without_etag_or_compression() {
    let request = headers(&[("accept-encoding", "gzip")]);
    let response = json_response(
        &request,
        &Method::GET,
        StatusCode::NOT_FOUND,
        &json!({ "error": "no route", "pad": "x".repeat(2000) }),
    );
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        header_str(&response, "content-type"),
        Some("application/json; charset=utf-8")
    );
    assert_eq!(header_str(&response, "cache-control"), Some("no-store"));
    assert_eq!(header_str(&response, "etag"), None);
    assert_eq!(header_str(&response, "content-encoding"), None);
    let body: Value = serde_json::from_slice(&body_bytes(response).await).expect("json");
    assert_eq!(body["error"], "no route");
}

#[tokio::test]
async fn a_non_200_ignores_if_none_match() {
    let first = get(&small(), &HeaderMap::new());
    let etag = header_str(&first, "etag").expect("etag").to_string();
    let response = json_response(
        &headers(&[("if-none-match", &etag)]),
        &Method::GET,
        StatusCode::NOT_FOUND,
        &small(),
    );
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

struct Unserialisable;

impl Serialize for Unserialisable {
    fn serialize<S: Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
        Err(S::Error::custom("nope"))
    }
}

#[tokio::test]
async fn a_body_that_fails_to_serialise_answers_500() {
    let response = json_response(
        &HeaderMap::new(),
        &Method::GET,
        StatusCode::OK,
        &Unserialisable,
    );
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        header_str(&response, "content-type"),
        Some("application/json; charset=utf-8")
    );
    assert_eq!(header_str(&response, "cache-control"), Some("no-store"));
    assert_eq!(header_str(&response, "etag"), None);
    assert_eq!(body_bytes(response).await, br#"{"error":"internal error"}"#);
}

#[tokio::test]
async fn a_binary_answer_carries_the_five_headers_and_the_body() {
    let png = vec![0x89, b'P', b'N', b'G', 1, 2, 3];
    let response = bytes_response(StatusCode::OK, "image/png", "max-age=60", png.clone());
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_str(&response, "content-type"), Some("image/png"));
    assert_eq!(header_str(&response, "cache-control"), Some("max-age=60"));
    assert_eq!(header_str(&response, "content-length"), Some("7"));
    assert_eq!(
        header_str(&response, "x-content-type-options"),
        Some("nosniff")
    );
    assert_eq!(
        header_str(&response, "content-security-policy"),
        Some("sandbox; default-src 'none'; style-src 'unsafe-inline'")
    );
    assert_eq!(header_str(&response, "etag"), None);
    assert_eq!(header_str(&response, "content-encoding"), None);
    assert_eq!(body_bytes(response).await, png);
}

#[tokio::test]
async fn an_invalid_content_type_falls_back_to_octet_stream() {
    let response = bytes_response(StatusCode::OK, "image/png\nx", "no-store", vec![1]);
    assert_eq!(
        header_str(&response, "content-type"),
        Some("application/octet-stream")
    );
}

#[tokio::test]
async fn an_invalid_cache_control_does_not_panic() {
    let response = bytes_response(StatusCode::OK, "image/png", "max-age=60\nx", vec![1]);
    assert_eq!(header_str(&response, "cache-control"), Some("no-store"));
    assert_eq!(body_bytes(response).await, vec![1]);
}

#[tokio::test]
async fn an_empty_binary_body_has_content_length_zero() {
    let response = bytes_response(StatusCode::OK, "image/png", "no-store", Vec::new());
    assert_eq!(header_str(&response, "content-length"), Some("0"));
    assert!(body_bytes(response).await.is_empty());
}

const HEAD: &str = "/api/workspaces/";
const TAIL: &str = "/sessions";

#[test]
fn match_param_returns_a_plain_id() {
    assert_eq!(
        match_param("/api/workspaces/abc-123/sessions", HEAD, TAIL),
        Some("abc-123".to_string())
    );
}

#[test]
fn match_param_decodes_an_encoded_id() {
    assert_eq!(
        match_param("/api/workspaces/a%20b/sessions", HEAD, TAIL),
        Some("a b".to_string())
    );
    assert_eq!(
        match_param("/api/workspaces/caf%C3%A9/sessions", HEAD, TAIL),
        Some("café".to_string())
    );
    assert_eq!(
        match_param("/api/workspaces/a%2Fb/sessions", HEAD, TAIL),
        Some("a/b".to_string())
    );
    assert_eq!(
        match_param("/api/workspaces/a+b/sessions", HEAD, TAIL),
        Some("a+b".to_string()),
        "a plus is not a space in a path"
    );
}

#[test]
fn match_param_rejects_an_empty_segment() {
    assert_eq!(match_param("/api/workspaces//sessions", HEAD, TAIL), None);
}

#[test]
fn match_param_rejects_a_nested_path() {
    assert_eq!(
        match_param("/api/workspaces/a/b/sessions", HEAD, TAIL),
        None
    );
}

#[test]
fn match_param_rejects_a_wrong_head_or_tail() {
    assert_eq!(match_param("/api/other/abc/sessions", HEAD, TAIL), None);
    assert_eq!(
        match_param("/api/workspaces/abc/messages", HEAD, TAIL),
        None
    );
    assert_eq!(
        match_param("/api/workspaces/abc/sessions/extra", HEAD, TAIL),
        None
    );
    assert_eq!(match_param("/api/workspaces/sessions", HEAD, TAIL), None);
}

#[test]
fn match_param_with_an_empty_tail_takes_the_rest() {
    assert_eq!(
        match_param("/api/sessions/s%201", "/api/sessions/", ""),
        Some("s 1".to_string())
    );
    assert_eq!(match_param("/api/sessions/", "/api/sessions/", ""), None);
    assert_eq!(match_param("/api/sessions/a/b", "/api/sessions/", ""), None);
}

#[test]
fn match_param_rejects_a_bad_escape() {
    for path in [
        "/api/workspaces/a%/sessions",
        "/api/workspaces/a%2/sessions",
        "/api/workspaces/a%zz/sessions",
        "/api/workspaces/%g0/sessions",
    ] {
        assert_eq!(match_param(path, HEAD, TAIL), None, "{path}");
    }
}

#[test]
fn match_param_rejects_invalid_utf8_after_decoding() {
    assert_eq!(
        match_param("/api/workspaces/%ff/sessions", HEAD, TAIL),
        None
    );
    assert_eq!(
        match_param("/api/workspaces/%C3%28/sessions", HEAD, TAIL),
        None
    );
}
