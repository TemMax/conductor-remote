use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use conductor_remote::artifacts::{
    ArtifactError, ArtifactPreviewService, ArtifactPreviewSession, ArtifactPreviewSource,
    CloseArtifactPreviewResult, CreateArtifactPreviewRequest,
};
use conductor_remote::contract::{AppState, ConductorStatus, Services, Token};
use conductor_remote::delivery::BoxFuture;
use conductor_remote::http::router;
use conductor_remote::testing::{FakeConductor, MemoryAssets};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const COLLECTION: &str = "/api/artifact-previews";
const SOURCE: &str = "/api/artifact-previews/preview-one/source";
const CLOSE: &str = "/api/artifact-previews/preview-one";
const PARENT: &str = "http://localhost:8790";
// Synthetic test-only auth and grant values.
const TOKEN: &str = "artifact-http-test";

#[derive(Debug, PartialEq)]
enum Call {
    Create(CreateArtifactPreviewRequest),
    Source(String),
    Close(String),
}

#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<Call>>,
    error: Option<ArtifactError>,
}

fn original() -> String {
    format!(
        "<!DOCTYPE html>\r\n{}<p>final marker</p>",
        "line\n".repeat(650)
    )
}

impl Fake {
    fn result(&self, valid: bool) -> Result<(), ArtifactError> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        if !valid {
            return Err(ArtifactError::Invalid("invalid control input".into()));
        }
        Ok(())
    }
}

impl ArtifactPreviewService for Fake {
    fn create(
        &self,
        request: CreateArtifactPreviewRequest,
    ) -> BoxFuture<Result<ArtifactPreviewSession, ArtifactError>> {
        let parent = match &request {
            CreateArtifactPreviewRequest::File { parent_origin, .. }
            | CreateArtifactPreviewRequest::Html { parent_origin, .. } => parent_origin,
        };
        // The service owns the exact configured allowlist, never the request Host.
        let result = self.result(parent == PARENT);
        self.calls.lock().unwrap().push(Call::Create(request));
        Box::pin(async move {
            result?;
            Ok(ArtifactPreviewSession {
                id: "preview-one".into(),
                origin: "http://localhost:31001".into(),
                bootstrap_url: "http://localhost:31001/__conductor_preview/bootstrap".into(),
                entry_path: "/index.html".into(),
                grant: "synthetic-grant".into(),
                expires_at: "2026-10-16T12:30:00Z".into(),
                path: None,
            })
        })
    }

    fn source(&self, id: String) -> BoxFuture<Result<ArtifactPreviewSource, ArtifactError>> {
        let result = self.result(id == "preview-one");
        self.calls.lock().unwrap().push(Call::Source(id));
        Box::pin(async move {
            result?;
            Ok(ArtifactPreviewSource {
                path: None,
                content: original(),
            })
        })
    }

    fn close(&self, id: String) -> BoxFuture<Result<CloseArtifactPreviewResult, ArtifactError>> {
        let result = self.result(id == "preview-one");
        self.calls.lock().unwrap().push(Call::Close(id));
        Box::pin(async move {
            result?;
            Ok(CloseArtifactPreviewResult { ok: true })
        })
    }
}

fn app(fake: Option<Arc<Fake>>) -> Router {
    router(AppState {
        token: Arc::new(Token::new(TOKEN)),
        conductor: Arc::new(FakeConductor::new(ConductorStatus::Running)),
        assets: Arc::new(MemoryAssets::default().with("index.html", "text/html", b"app")),
        reads: None,
        writes: None,
        notify: None,
        services: Services {
            artifacts: fake.map(|f| f as Arc<dyn ArtifactPreviewService>),
            ..Default::default()
        },
    })
}

fn file() -> Value {
    json!({"kind":"file", "reference":"/workspace/page.htm:601:3", "parentOrigin":PARENT})
}

fn request(method: Method, path: &str, body: impl Into<Body>, auth: bool) -> Request<Body> {
    let builder = Request::builder().method(method).uri(path);
    let builder = if auth {
        builder.header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
    } else {
        builder
    };
    builder.body(body.into()).unwrap()
}

async fn answer(app: Router, request: Request<Body>, status: StatusCode) -> Value {
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), status);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    assert!(!response.headers().contains_key(header::SET_COOKIE));
    assert!(!response.headers().contains_key(header::LOCATION));
    value(response).await
}

async fn value(response: Response) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

fn endpoints() -> [(Method, &'static str, String); 3] {
    [
        (Method::POST, COLLECTION, file().to_string()),
        (Method::GET, SOURCE, String::new()),
        (Method::DELETE, CLOSE, String::new()),
    ]
}

#[tokio::test]
async fn all_controls_use_the_existing_main_auth_gate_and_ignore_preview_cookies() {
    let fake = Arc::new(Fake::default());
    let app = app(Some(fake.clone()));
    for (method, path, body) in endpoints() {
        for authorization in [None, Some("Bearer wrong-token")] {
            let mut req = request(method.clone(), path, body.clone(), false);
            req.headers_mut().insert(
                header::COOKIE,
                "preview_cookie=synthetic-grant".parse().unwrap(),
            );
            if let Some(auth) = authorization {
                req.headers_mut()
                    .insert(header::AUTHORIZATION, auth.parse().unwrap());
            }
            assert_eq!(
                answer(app.clone(), req, StatusCode::UNAUTHORIZED).await,
                json!({"error":"unauthorized"})
            );
        }
        let query = format!("{path}?token={TOKEN}");
        answer(
            app.clone(),
            request(method.clone(), &query, body.clone(), false),
            StatusCode::OK,
        )
        .await;
        // A wrong Bearer still takes precedence over the accepted query token.
        let mut req = request(method, &query, body, false);
        req.headers_mut()
            .insert(header::AUTHORIZATION, "Bearer wrong-token".parse().unwrap());
        answer(app.clone(), req, StatusCode::UNAUTHORIZED).await;
    }
    assert_eq!(fake.calls.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn typed_requests_responses_and_unclipped_original_source() {
    let fake = Arc::new(Fake::default());
    let app = app(Some(fake.clone()));
    for body in [
        file(),
        json!({"kind":"html", "html":"<!DOCTYPE html>\r\n<p>raw</p>", "parentOrigin":PARENT}),
        json!({"kind":"html", "html":"", "baseDirectory":null, "parentOrigin":PARENT}),
        json!({"kind":"html", "html":"x", "baseDirectory":"/workspace", "parentOrigin":PARENT}),
    ] {
        let session = answer(
            app.clone(),
            request(Method::POST, COLLECTION, body.to_string(), true),
            StatusCode::OK,
        )
        .await;
        assert_eq!(
            session,
            json!({"id":"preview-one", "origin":"http://localhost:31001", "bootstrapUrl":"http://localhost:31001/__conductor_preview/bootstrap", "entryPath":"/index.html", "grant":"synthetic-grant", "expiresAt":"2026-10-16T12:30:00Z", "path":null})
        );
        assert_eq!(
            fake.calls.lock().unwrap().last(),
            Some(&Call::Create(serde_json::from_value(body).unwrap()))
        );
    }
    let mut req = request(
        Method::GET,
        "/api/artifact-previews/preview%2Done/source",
        "ignored",
        true,
    );
    req.headers_mut()
        .insert(header::IF_NONE_MATCH, "*".parse().unwrap());
    req.headers_mut()
        .insert(header::ACCEPT_ENCODING, "gzip".parse().unwrap());
    assert_eq!(
        answer(app.clone(), req, StatusCode::OK).await,
        json!({"path":null, "content":original()})
    );
    assert_eq!(
        answer(
            app,
            request(Method::DELETE, CLOSE, "ignored", true),
            StatusCode::OK
        )
        .await,
        json!({"ok":true})
    );
    let calls = fake.calls.lock().unwrap();
    assert_eq!(calls[4], Call::Source("preview-one".into()));
    assert_eq!(calls[5], Call::Close("preview-one".into()));
}

#[tokio::test]
async fn malformed_surplus_mixed_and_wrongly_typed_bodies_never_reach_service() {
    let fake = Arc::new(Fake::default());
    let app = app(Some(fake.clone()));
    for body in [
        "",
        "{",
        "null",
        "[]",
        "{}",
        r#"{"kind":"html","html":2,"parentOrigin":"http://localhost:8790"}"#,
        r#"{"kind":"file","reference":"a","parentOrigin":"http://localhost:8790","extra":true}"#,
        r#"{"kind":"file","reference":"a","html":"x","parentOrigin":"http://localhost:8790"}"#,
        r#"{"kind":"html","html":"x","reference":"a","parentOrigin":"http://localhost:8790"}"#,
        r#"{"kind":"html","html":"x"}"#,
        r#"{"kind":"other","html":"x","parentOrigin":"http://localhost:8790"}"#,
        r#"{"kind":"html","html":"x","baseDirectory":3,"parentOrigin":"http://localhost:8790"}"#,
    ] {
        answer(
            app.clone(),
            request(Method::POST, COLLECTION, body.to_owned(), true),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn create_body_limit_is_one_mib_inclusive() {
    let fake = Arc::new(Fake::default());
    let app = app(Some(fake.clone()));
    let mut body = json!({"kind":"html", "html":"", "parentOrigin":PARENT}).to_string();
    body.push_str(&" ".repeat((1 << 20) - body.len()));
    answer(
        app.clone(),
        request(Method::POST, COLLECTION, body.clone(), true),
        StatusCode::OK,
    )
    .await;
    body.push(' ');
    answer(
        app,
        request(Method::POST, COLLECTION, body, true),
        StatusCode::PAYLOAD_TOO_LARGE,
    )
    .await;
    assert_eq!(fake.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn absent_service_is_unavailable_for_every_control() {
    for (method, path, body) in endpoints() {
        assert_eq!(
            answer(
                app(None),
                request(method, path, body, true),
                StatusCode::SERVICE_UNAVAILABLE
            )
            .await,
            json!({"error":"artifact previews are unavailable"})
        );
    }
}

#[tokio::test]
async fn every_typed_error_maps_for_every_control() {
    for (error, status) in [
        (
            ArtifactError::Invalid("invalid".into()),
            StatusCode::BAD_REQUEST,
        ),
        (
            ArtifactError::Forbidden("forbidden".into()),
            StatusCode::FORBIDDEN,
        ),
        (
            ArtifactError::Missing("missing".into()),
            StatusCode::NOT_FOUND,
        ),
        (
            ArtifactError::TooLarge("too large".into()),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        (
            ArtifactError::Unsupported("unsupported".into()),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            ArtifactError::Unavailable("unavailable".into()),
            StatusCode::SERVICE_UNAVAILABLE,
        ),
    ] {
        let app = app(Some(Arc::new(Fake {
            error: Some(error.clone()),
            ..Default::default()
        })));
        for (method, path, body) in endpoints() {
            assert_eq!(
                answer(app.clone(), request(method, path, body, true), status).await,
                json!({"error":error.to_string()})
            );
        }
    }
}

#[tokio::test]
async fn semantic_invalid_ids_and_parent_origins_are_service_errors() {
    let fake = Arc::new(Fake::default());
    let app = app(Some(fake.clone()));
    for origin in [
        "",
        "null",
        "ftp://localhost:8790",
        "http://localhost:8790/path",
        "http://user@localhost:8790",
        "http://localhost:8790?x=1",
        "http://localhost:8790#fragment",
        "http://localhost:9999",
        "http://localhost.evil:8790",
        "https://untrusted.example",
    ] {
        let mut body = file();
        body["parentOrigin"] = json!(origin);
        let mut req = request(Method::POST, COLLECTION, body.to_string(), true);
        req.headers_mut()
            .insert(header::HOST, "localhost:8790".parse().unwrap());
        answer(app.clone(), req, StatusCode::BAD_REQUEST).await;
    }
    for id in ["unknown", "%2F", "%00", "%2e%2e", "%20", "a%5Cb"] {
        for (method, suffix) in [(Method::GET, "/source"), (Method::DELETE, "")] {
            answer(
                app.clone(),
                request(method, &format!("{COLLECTION}/{id}{suffix}"), "", true),
                StatusCode::BAD_REQUEST,
            )
            .await;
        }
    }
}

#[tokio::test]
async fn unsupported_methods_and_route_collisions_keep_api_not_found_convention() {
    let fake = Arc::new(Fake::default());
    let app = app(Some(fake.clone()));
    for (method, path) in [
        (Method::GET, COLLECTION),
        (Method::DELETE, COLLECTION),
        (Method::PUT, COLLECTION),
        (Method::POST, SOURCE),
        (Method::DELETE, SOURCE),
        (Method::GET, CLOSE),
        (Method::POST, CLOSE),
        (Method::OPTIONS, CLOSE),
        (Method::POST, "/api/artifact-previews/"),
        (Method::POST, "/api/artifact-previews-extra"),
        (Method::GET, "/api/artifact-previews//source"),
        (Method::GET, "/api/artifact-previews/a/b/source"),
        (Method::GET, "/api/artifact-previews/a/source/extra"),
        (Method::GET, "/api/artifact-previews/%GG/source"),
        (Method::GET, "/api/artifact-previews/%FF/source"),
        (Method::DELETE, "/api/artifact-previews/a/b"),
        (
            Method::POST,
            "/api/artifact-previews/__conductor_preview/authorize",
        ),
    ] {
        // DELETE /:id/source is not a close route, since it has two segments.
        answer(
            app.clone(),
            request(method, path, "malformed body", true),
            StatusCode::NOT_FOUND,
        )
        .await;
    }
    let head = app
        .clone()
        .oneshot(request(Method::HEAD, SOURCE, "", true))
        .await
        .unwrap();
    assert_eq!(head.status(), StatusCode::NOT_FOUND);
    assert_eq!(head.headers()[header::CACHE_CONTROL], "no-store");
    assert!(head
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .is_empty());
    assert_eq!(
        app.oneshot(request(Method::GET, "/api/state", "", true))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert!(fake.calls.lock().unwrap().is_empty());
}
