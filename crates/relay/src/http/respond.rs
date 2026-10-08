//! The response helpers every API read uses: the JSON envelope and the
//! one-parameter route matcher.

use std::io::Write;

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::Serialize;
use sha1::{Digest, Sha1};

const JSON_CONTENT_TYPE: &str = "application/json; charset=utf-8";
const NO_CACHE: &str = "no-cache";
const NO_STORE: &str = "no-store";
const COMPRESSION_THRESHOLD: usize = 1024;
const OCTET_STREAM: &str = "application/octet-stream";
const BINARY_CSP: &str = "sandbox; default-src 'none'; style-src 'unsafe-inline'";
const INTERNAL_ERROR: &[u8] = br#"{"error":"internal error"}"#;

/// A JSON response with the caching and compression every API read uses.
pub fn json_response(
    request_headers: &HeaderMap,
    method: &Method,
    status: StatusCode,
    body: &impl Serialize,
) -> Response {
    match serde_json::to_vec(body) {
        Ok(bytes) => json_bytes_response(request_headers, method, status, bytes),
        Err(_) => json_bytes_response(
            request_headers,
            method,
            StatusCode::INTERNAL_SERVER_ERROR,
            INTERNAL_ERROR.to_vec(),
        ),
    }
}

/// The same answer for a body that is already serialised JSON; `json_response` serialises and calls this.
pub fn json_bytes_response(
    request_headers: &HeaderMap,
    method: &Method,
    status: StatusCode,
    body: Vec<u8>,
) -> Response {
    if status != StatusCode::OK || method != Method::GET {
        return build(status, NO_STORE, None, false, body);
    }

    let etag = etag_of(&body);
    let matches = request_headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|value| value.as_bytes() == etag.as_bytes());
    if matches {
        return build(
            StatusCode::NOT_MODIFIED,
            NO_CACHE,
            Some(&etag),
            true,
            Vec::new(),
        );
    }

    let accepts_gzip = request_headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .any(|value| String::from_utf8_lossy(value.as_bytes()).contains("gzip"));
    if body.len() > COMPRESSION_THRESHOLD && accepts_gzip {
        if let Some(compressed) = gzip(&body) {
            let mut response = build(status, NO_CACHE, Some(&etag), true, compressed);
            response
                .headers_mut()
                .insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
            return response;
        }
    }
    build(status, NO_CACHE, Some(&etag), true, body)
}

/// An answer whose body is a file or an image.
pub fn bytes_response(
    status: StatusCode,
    content_type: &str,
    cache_control: &'static str,
    body: Vec<u8>,
) -> Response {
    let content_type = HeaderValue::from_str(content_type)
        .unwrap_or_else(|_| HeaderValue::from_static(OCTET_STREAM));
    let cache_control =
        HeaderValue::from_str(cache_control).unwrap_or_else(|_| HeaderValue::from_static(NO_STORE));
    let length = body.len();
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, content_type);
    headers.insert(header::CACHE_CONTROL, cache_control);
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(BINARY_CSP),
    );
    response
}

fn build(
    status: StatusCode,
    cache_control: &'static str,
    etag: Option<&str>,
    vary: bool,
    body: Vec<u8>,
) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(JSON_CONTENT_TYPE),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    if vary {
        headers.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
    }
    if let Some(etag) = etag {
        if let Ok(value) = HeaderValue::from_str(etag) {
            headers.insert(header::ETAG, value);
        }
    }
    response
}

fn etag_of(body: &[u8]) -> String {
    let digest = Sha1::digest(body);
    format!("W/\"{}\"", URL_SAFE_NO_PAD.encode(digest))
}

fn gzip(body: &[u8]) -> Option<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(body).ok()?;
    encoder.finish().ok()
}

/// Matches `path` against `head` + one segment + `tail`, and percent-decodes the segment.
/// `match_param("/api/workspaces/a%20b/sessions", "/api/workspaces/", "/sessions") == Some("a b")`.
pub fn match_param(path: &str, head: &str, tail: &str) -> Option<String> {
    if path.len() < head.len() + tail.len() {
        return None;
    }
    let rest = path.strip_prefix(head)?;
    let segment = rest.strip_suffix(tail)?;
    if segment.is_empty() || segment.contains('/') {
        return None;
    }
    percent_decode_strict(segment)
}

fn percent_decode_strict(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let high = hex_value(*bytes.get(i + 1)?)?;
            let low = hex_value(*bytes.get(i + 2)?)?;
            out.push(high << 4 | low);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
