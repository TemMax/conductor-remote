use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use conductor_remote::artifacts::*;
use conductor_remote::contract::Services;
use conductor_remote::delivery::BoxFuture;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};

// The TypeScript contract test consumes these same JSON literals, without a second wire fixture.
const REQUESTS_JSON: &str = r#"[
  {"kind":"file","reference":"/workspace/artifacts/page.htm:601:3","parentOrigin":"http://localhost:8790"},
  {"kind":"html","html":"<!DOCTYPE html>\n<p>inline</p>","parentOrigin":"http://localhost:5173"},
  {"kind":"html","html":"<p>with dependencies</p>","baseDirectory":"/workspace/artifacts","parentOrigin":"https://relay.example.test"}
]"#;
const SESSIONS_JSON: &str = r#"[
  {"id":"preview-one","origin":"http://localhost:31001","bootstrapUrl":"http://localhost:31001/__conductor_preview/bootstrap","entryPath":"/page.htm","grant":"synthetic-grant","expiresAt":"2026-10-16T12:30:00Z","path":"/workspace/artifacts/page.htm"},
  {"id":"preview-two","origin":"https://relay.example.test:31002","bootstrapUrl":"https://relay.example.test:31002/__conductor_preview/bootstrap","entryPath":"/index.html","grant":"synthetic-grant-two","expiresAt":"2026-10-16T12:30:00Z","path":null}
]"#;
const SOURCES_JSON: &str = r#"[
  {"path":"/workspace/artifacts/page.htm","content":"<!DOCTYPE html>\n<html><body>original</body></html>"},
  {"path":null,"content":"<p>inline</p>"}
]"#;
const CLOSE_JSON: &str = r#"{"ok":true}"#;
const MESSAGES_JSON: &str = r#"[
  {"channel":"conductor-artifact-preview","version":1,"sessionId":"preview-one","type":"ready"},
  {"channel":"conductor-artifact-preview","version":1,"sessionId":"preview-one","type":"authorize","grant":"synthetic-grant"},
  {"channel":"conductor-artifact-preview","version":1,"sessionId":"preview-one","type":"loaded"},
  {"channel":"conductor-artifact-preview","version":1,"sessionId":"preview-one","type":"diagnostic","kind":"javascript","message":"synthetic failure","source":"/app.js","line":12,"column":3},
  {"channel":"conductor-artifact-preview","version":1,"sessionId":"preview-one","type":"diagnostic","kind":"promise","message":"synthetic rejection"},
  {"channel":"conductor-artifact-preview","version":1,"sessionId":"preview-one","type":"diagnostic","kind":"resource","message":"missing image","source":"/missing.png"},
  {"channel":"conductor-artifact-preview","version":1,"sessionId":"preview-one","type":"diagnostic","kind":"http","message":"404","source":"/missing.css"}
]"#;

fn round_trip<T: DeserializeOwned + Serialize>(wire: &str) {
    let original: Value = serde_json::from_str(wire).unwrap();
    let parsed: T = serde_json::from_value(original.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), original);
}

#[test]
fn wire_shapes_round_trip_with_typescript_fixtures() {
    round_trip::<Vec<CreateArtifactPreviewRequest>>(REQUESTS_JSON);
    round_trip::<Vec<ArtifactPreviewSession>>(SESSIONS_JSON);
    round_trip::<Vec<ArtifactPreviewSource>>(SOURCES_JSON);
    round_trip::<CloseArtifactPreviewResult>(CLOSE_JSON);
    round_trip::<Vec<ArtifactPreviewEnvelope>>(MESSAGES_JSON);

    let nullable: CreateArtifactPreviewRequest = serde_json::from_value(json!({
        "kind": "html", "html": "", "baseDirectory": null,
        "parentOrigin": "http://localhost:8790"
    }))
    .unwrap();
    assert!(matches!(
        nullable,
        CreateArtifactPreviewRequest::Html {
            base_directory: None,
            ..
        }
    ));
}

#[test]
fn request_variants_reject_mixed_surplus_or_missing_fields() {
    for bad in [
        json!({"kind":"file", "reference":"/a.htm", "html":"", "parentOrigin":"http://localhost:8790"}),
        json!({"kind":"html", "html":"", "reference":"/a.htm", "parentOrigin":"http://localhost:8790"}),
        json!({"kind":"html", "html":"", "parentOrigin":"http://localhost:8790", "extra":true}),
        json!({"kind":"file", "reference":"/a.htm"}),
        json!({"kind":"html", "parentOrigin":"http://localhost:8790"}),
        json!({"kind":"other", "html":"", "parentOrigin":"http://localhost:8790"}),
    ] {
        assert!(serde_json::from_value::<CreateArtifactPreviewRequest>(bad).is_err());
    }
}

#[test]
fn iframe_protocol_rejects_wrong_channel_version_or_type() {
    let ready: Value = serde_json::from_str::<Vec<Value>>(MESSAGES_JSON).unwrap()[0].clone();
    for (field, wrong) in [
        ("channel", json!("other")),
        ("version", json!(2)),
        ("type", json!("operateRelay")),
    ] {
        let mut bad = ready.clone();
        bad[field] = wrong;
        assert!(serde_json::from_value::<ArtifactPreviewEnvelope>(bad).is_err());
    }
}

#[test]
fn artifact_source_is_full_and_existing_file_preview_shape_is_unchanged() {
    let content = format!(
        "<!DOCTYPE html>\r\n{}<p>last marker</p>",
        "line\n".repeat(601)
    );
    let source = ArtifactPreviewSource {
        path: None,
        content: content.clone(),
    };
    let wire = serde_json::to_value(&source).unwrap();
    assert_eq!(wire, json!({"path":null,"content":content}));
    assert_eq!(
        serde_json::from_value::<ArtifactPreviewSource>(wire).unwrap(),
        source
    );
    let preview = conductor_remote::files::FilePreview {
        path: "/workspace/page.htm".into(),
        line: Some(601),
        line_start: 501,
        line_end: 602,
        total_lines: 602,
        content: "excerpt".into(),
        truncated: true,
    };
    assert_eq!(
        serde_json::to_value(preview).unwrap(),
        json!({
            "path":"/workspace/page.htm", "line":601, "lineStart":501, "lineEnd":602,
            "totalLines":602, "content":"excerpt", "truncated":true
        })
    );
}

#[test]
fn typed_error_statuses_and_route_declarations_are_fixed() {
    for (error, status) in [
        (ArtifactError::Invalid("invalid".into()), 400),
        (ArtifactError::Forbidden("forbidden".into()), 403),
        (ArtifactError::Missing("missing".into()), 404),
        (ArtifactError::TooLarge("too large".into()), 413),
        (ArtifactError::Unsupported("unsupported".into()), 415),
        (ArtifactError::Unavailable("unavailable".into()), 503),
    ] {
        assert_eq!(error.status_code(), status);
        assert!(!error.to_string().is_empty());
    }
    let routes = include_str!("../../../web/src/contract/routes.ts");
    for route in [
        "createArtifactPreview: flat('POST', '/api/artifact-previews')",
        "artifactPreviewSource: param('GET', '/api/artifact-previews/:id/source')",
        "closeArtifactPreview: param('DELETE', '/api/artifact-previews/:id')",
        "filePreview: param('GET', '/api/files/:reference')",
    ] {
        assert!(routes.contains(route), "{route}");
    }
}

struct Clock;
impl ArtifactClock for Clock {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(42)
    }
    fn monotonic(&self) -> Duration {
        Duration::from_secs(12)
    }
}

struct Entropy;
impl ArtifactEntropy for Entropy {
    fn fill(&self, bytes: &mut [u8]) -> Result<(), ArtifactError> {
        bytes.fill(7);
        Ok(())
    }
}

#[derive(Default)]
struct Exposure(Mutex<Vec<ArtifactExposureReceipt>>);
impl ArtifactExposure for Exposure {
    fn current_permission(&self) -> BoxFuture<Result<ArtifactExposurePermission, ArtifactError>> {
        Box::pin(async {
            Ok(ArtifactExposurePermission {
                enabled: true,
                generation: 4,
            })
        })
    }
    fn publish(&self, receipt: ArtifactExposureReceipt) -> BoxFuture<Result<(), ArtifactError>> {
        self.0.lock().unwrap().push(receipt);
        Box::pin(async { Ok(()) })
    }
    fn unpublish(&self, receipt: ArtifactExposureReceipt) -> BoxFuture<Result<(), ArtifactError>> {
        self.0.lock().unwrap().retain(|owned| owned != &receipt);
        Box::pin(async { Ok(()) })
    }
    fn reconcile_startup(&self) -> BoxFuture<Result<(), ArtifactError>> {
        self.0.lock().unwrap().clear();
        Box::pin(async { Ok(()) })
    }
}

struct FakePreviews {
    _config: ArtifactPreviewConfig,
    lifecycle: Mutex<Vec<&'static str>>,
}
impl FakePreviews {
    // Pins the future concrete constructor's owned input and fallible output, without a runtime stub.
    fn new(config: ArtifactPreviewConfig) -> Result<Self, ArtifactError> {
        if config.allowed_parent_origins.is_empty() {
            return Err(ArtifactError::Invalid("no parent origins".into()));
        }
        Ok(Self {
            _config: config,
            lifecycle: Mutex::default(),
        })
    }
}
impl ArtifactPreviewService for FakePreviews {
    fn create(
        &self,
        _: CreateArtifactPreviewRequest,
    ) -> BoxFuture<Result<ArtifactPreviewSession, ArtifactError>> {
        Box::pin(async {
            Ok(
                serde_json::from_str::<Vec<ArtifactPreviewSession>>(SESSIONS_JSON)
                    .unwrap()
                    .remove(0),
            )
        })
    }
    fn source(&self, _: String) -> BoxFuture<Result<ArtifactPreviewSource, ArtifactError>> {
        Box::pin(async {
            Ok(ArtifactPreviewSource {
                path: None,
                content: "<p>fake</p>".into(),
            })
        })
    }
    fn close(&self, _: String) -> BoxFuture<Result<CloseArtifactPreviewResult, ArtifactError>> {
        Box::pin(async { Ok(CloseArtifactPreviewResult { ok: true }) })
    }
}
impl ArtifactPreviewLifecycle for FakePreviews {
    fn reconcile_startup(&self) -> BoxFuture<Result<(), ArtifactError>> {
        self.lifecycle.lock().unwrap().push("startup");
        Box::pin(async { Ok(()) })
    }
    fn sweep(&self) -> BoxFuture<Result<(), ArtifactError>> {
        self.lifecycle.lock().unwrap().push("sweep");
        Box::pin(async { Ok(()) })
    }
    fn close_all(&self) -> BoxFuture<Result<(), ArtifactError>> {
        self.lifecycle.lock().unwrap().push("close_all");
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn environment_service_exposure_and_lifecycle_are_object_safe_and_injectable() {
    assert!(Services::default().artifacts.is_none());
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let exposure = Arc::new(Exposure::default());
    let config = ArtifactPreviewConfig {
        source_roots: vec![root.clone()],
        broad_roots: vec![root.clone()],
        state_dir: root.join("state"),
        allowed_parent_origins: BTreeSet::from(["http://localhost:8790".into()]),
        clock: Arc::new(Clock),
        entropy: Arc::new(Entropy),
        exposure: Some(exposure.clone()),
    };
    assert_eq!(
        config.clock.now(),
        SystemTime::UNIX_EPOCH + Duration::from_secs(42)
    );
    assert_eq!(config.clock.monotonic(), Duration::from_secs(12));
    let mut random = [0; 32];
    config.entropy.fill(&mut random).unwrap();
    assert_eq!(random, [7; 32]);
    let adapter = config.exposure.clone().unwrap();
    let permission = adapter.current_permission().await.unwrap();
    assert!(permission.enabled);
    let receipt = ArtifactExposureReceipt {
        binding_id: "binding-one".into(),
        loopback_port: 41001,
        https_port: 31001,
        origin: "https://relay.example.test:31001".into(),
        generation: permission.generation,
    };
    round_trip::<ArtifactExposureReceipt>(&serde_json::to_string(&receipt).unwrap());
    adapter.publish(receipt.clone()).await.unwrap();
    assert_eq!(*exposure.0.lock().unwrap(), vec![receipt.clone()]);
    adapter.unpublish(receipt.clone()).await.unwrap();
    assert!(exposure.0.lock().unwrap().is_empty());
    adapter.publish(receipt).await.unwrap();
    adapter.reconcile_startup().await.unwrap();
    assert!(exposure.0.lock().unwrap().is_empty());

    let mut local = config.clone();
    local.exposure = None;
    assert!(FakePreviews::new(local).is_ok());
    let fake = Arc::new(FakePreviews::new(config).unwrap());
    let services = Services {
        artifacts: Some(fake.clone()),
        ..Services::default()
    };
    let service = services.artifacts.unwrap();
    let request = serde_json::from_str::<Vec<CreateArtifactPreviewRequest>>(REQUESTS_JSON)
        .unwrap()
        .remove(0);
    let session = service.create(request).await.unwrap();
    assert_eq!(
        service.source(session.id.clone()).await.unwrap().content,
        "<p>fake</p>"
    );
    assert!(service.close(session.id).await.unwrap().ok);
    let lifecycle: &dyn ArtifactPreviewLifecycle = fake.as_ref();
    lifecycle.reconcile_startup().await.unwrap();
    lifecycle.sweep().await.unwrap();
    lifecycle.close_all().await.unwrap();
    assert_eq!(
        *fake.lifecycle.lock().unwrap(),
        ["startup", "sweep", "close_all"]
    );
}
