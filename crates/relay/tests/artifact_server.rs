//! Real loopback HTTP against the production controller and scoped filesystem/store helpers.
//! These tests do not execute a browser, JavaScript, CSP or a real exposure command.
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use conductor_remote::artifacts::store::ArtifactStore;
use conductor_remote::artifacts::*;
use conductor_remote::delivery::BoxFuture;
use reqwest::{Client, Response};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct Clock(AtomicU64);
impl Clock {
    fn advance(&self, seconds: u64) {
        self.0.fetch_add(seconds, Ordering::SeqCst);
    }
}
impl ArtifactClock for Clock {
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000) + self.monotonic()
    }
    fn monotonic(&self) -> Duration {
        Duration::from_secs(self.0.load(Ordering::SeqCst))
    }
}
struct Entropy;
impl ArtifactEntropy for Entropy {
    fn fill(&self, bytes: &mut [u8]) -> Result<(), ArtifactError> {
        getrandom::fill(bytes)
            .map_err(|_| ArtifactError::Unavailable("test randomness failed".into()))
    }
}
struct Exposure {
    store: ArtifactStore,
    enabled: AtomicBool,
    fail_cleanup: AtomicBool,
    publications: AtomicUsize,
    collisions: AtomicUsize,
}
impl ArtifactExposure for Exposure {
    fn current_permission(&self) -> BoxFuture<Result<ArtifactExposurePermission, ArtifactError>> {
        let store = self.store.clone();
        let enabled = self.enabled.load(Ordering::SeqCst);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || store.current_permission(enabled))
                .await
                .unwrap()
        })
    }
    fn publish(&self, receipt: ArtifactExposureReceipt) -> BoxFuture<Result<(), ArtifactError>> {
        self.publications.fetch_add(1, Ordering::SeqCst);
        if self
            .collisions
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Box::pin(async {
                Err(ArtifactError::Unavailable(
                    "synthetic mapping allocation race".into(),
                ))
            });
        }
        let store = self.store.clone();
        let enabled = self.enabled.load(Ordering::SeqCst);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let mut guard = store.lock()?;
                guard.begin_publication(receipt.clone(), enabled)?;
                guard.mark_active(&receipt)
            })
            .await
            .unwrap()
        })
    }
    fn unpublish(&self, receipt: ArtifactExposureReceipt) -> BoxFuture<Result<(), ArtifactError>> {
        let store = self.store.clone();
        let fail = self.fail_cleanup.load(Ordering::SeqCst);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                if fail {
                    return Err(ArtifactError::Unavailable(
                        "synthetic cleanup failure".into(),
                    ));
                }
                store.lock()?.finish_cleanup(&receipt)?;
                Ok(())
            })
            .await
            .unwrap()
        })
    }
    fn reconcile_startup(&self) -> BoxFuture<Result<(), ArtifactError>> {
        let store = self.store.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let mut guard = store.lock()?;
                for owned in guard.snapshot()?.receipts {
                    guard.finish_cleanup(&owned.receipt)?;
                }
                Ok(())
            })
            .await
            .unwrap()
        })
    }
}

// Every fixture deliberately uses a separate persistence domain on the same loopback host.
// Serialize those domains so they cannot claim one another's just-released permanent ports.
// Multi-viewer creates/authorizations within each fixture still run concurrently.
static FIXTURES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    config: ArtifactPreviewConfig,
    manager: ArtifactPreviews,
    clock: Arc<Clock>,
    exposure: Arc<Exposure>,
    client: Client,
    _serial: tokio::sync::MutexGuard<'static, ()>,
}
impl Fixture {
    async fn new(remote: bool) -> Self {
        let serial = FIXTURES.lock().await;
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("project");
        fs::create_dir(&root).unwrap();
        let state_dir = base.join("state");
        let clock = Arc::new(Clock::default());
        let exposure = Arc::new(Exposure {
            store: ArtifactStore::open(&state_dir).unwrap(),
            enabled: AtomicBool::new(true),
            fail_cleanup: AtomicBool::new(false),
            publications: AtomicUsize::new(0),
            collisions: AtomicUsize::new(0),
        });
        let config = ArtifactPreviewConfig {
            source_roots: vec![base.clone()],
            broad_roots: vec![base],
            state_dir,
            allowed_parent_origins: [
                "http://localhost:8790",
                "http://127.0.0.1:8790",
                "http://localhost:5179",
                "https://relay.example.test",
            ]
            .map(str::to_owned)
            .into_iter()
            .collect(),
            clock: clock.clone(),
            entropy: Arc::new(Entropy),
            exposure: remote.then(|| exposure.clone() as Arc<dyn ArtifactExposure>),
        };
        let manager = ArtifactPreviews::new(config.clone()).unwrap();
        manager.reconcile_startup().await.unwrap();
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        Self {
            _temp: temp,
            root,
            config,
            manager,
            clock,
            exposure,
            client,
            _serial: serial,
        }
    }
    fn write(&self, name: &str, content: impl AsRef<[u8]>) {
        let path = self.root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    fn inline_request(&self, html: &str, parent: &str, base: bool) -> CreateArtifactPreviewRequest {
        CreateArtifactPreviewRequest::Html {
            html: html.into(),
            base_directory: base.then(|| self.root.to_str().unwrap().into()),
            parent_origin: parent.into(),
        }
    }
    async fn inline(&self, html: &str) -> ArtifactPreviewSession {
        self.manager
            .create(self.inline_request(html, "http://localhost:8790", false))
            .await
            .unwrap()
    }
    async fn file(&self) -> ArtifactPreviewSession {
        self.manager
            .create(CreateArtifactPreviewRequest::File {
                reference: format!("{}:601:3", self.root.join("index.html").display()),
                parent_origin: "http://localhost:8790".into(),
            })
            .await
            .unwrap()
    }
    async fn authorize(&self, session: &ArtifactPreviewSession) -> Response {
        self.client
            .post(format!("{}/__conductor_preview/authorize", session.origin))
            .header("Origin", &session.origin)
            .json(&json!({"sessionId":session.id,"grant":session.grant}))
            .send()
            .await
            .unwrap()
    }
    async fn cookie(&self, session: &ArtifactPreviewSession) -> String {
        let response = self.authorize(session).await;
        assert_eq!(response.status(), 200);
        let header = response
            .headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            header.contains("HttpOnly")
                && header.contains("SameSite=Strict")
                && header.contains("Path=/")
                && header.contains("Max-Age=1800")
        );
        assert!(!header.contains("Domain="));
        header.split(';').next().unwrap().into()
    }
    async fn get(&self, session: &ArtifactPreviewSession, path: &str, cookie: &str) -> Response {
        self.client
            .get(format!("{}{path}", session.origin))
            .header("Cookie", cookie)
            .send()
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn inline_full_source_bootstrap_and_cookie_authorization() {
    let f = Fixture::new(false).await;
    let html = format!("\u{feff}<!DOCTYPE html>\r\n<html><head><meta charset=utf-8></head><body>{}<button onclick='this.textContent=1'>last</button></body></html>", "line\n".repeat(701));
    let session = f.inline(&html).await;
    assert!(session.origin.starts_with("http://localhost:"));
    assert_eq!(
        f.manager.source(session.id.clone()).await.unwrap().content,
        html
    );
    let bootstrap = f.client.get(&session.bootstrap_url).send().await.unwrap();
    assert_eq!(bootstrap.status(), 200);
    let script = bootstrap.text().await.unwrap();
    for check in [
        "event.source!==parent",
        "event.origin!==parentOrigin",
        "m.version!==1",
        "m.sessionId!==sessionId",
        "m.type!=='authorize'",
        "parent.postMessage(envelope('ready'),parentOrigin)",
        "location.replace(entry)",
    ] {
        assert!(script.contains(check));
    }
    assert!(!script.contains(&session.grant));
    assert_eq!(
        f.get(&session, &session.entry_path, "unrelated=value")
            .await
            .status(),
        403
    );
    let cookie = f.cookie(&session).await;
    let response = f.get(&session, &session.entry_path, &cookie).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/html");
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(
        response.headers()["cross-origin-resource-policy"],
        "same-origin"
    );
    assert!(!response
        .headers()
        .contains_key("access-control-allow-origin"));
    let preview = response.text().await.unwrap();
    let preview_length = preview.len();
    assert!(preview.starts_with("\u{feff}<!DOCTYPE html>\r\n<html><head><meta charset=utf-8>"));
    assert!(preview.contains("last</button>"));
    let injection = "<script src=\"/__conductor_preview/diagnostics.js\"></script>";
    assert_eq!(preview.replace(injection, ""), html);
    let head = f
        .client
        .head(format!("{}{}", session.origin, session.entry_path))
        .header("Cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(head.status(), 200);
    assert_eq!(head.headers()["content-length"], preview_length.to_string());
    assert_eq!(head.headers()["accept-ranges"], "none");
    assert!(head.bytes().await.unwrap().is_empty());
    assert_eq!(f.get(&session, "/missing.css", &cookie).await.status(), 404);
    assert!(f
        .get(&session, "/__conductor_preview/diagnostics.js", &cookie)
        .await
        .text()
        .await
        .unwrap()
        .contains("unhandledrejection"));
    f.manager.close_all().await.unwrap();
}

#[tokio::test]
async fn file_refresh_resources_mime_ranges_and_reserved_routes() {
    let f = Fixture::new(false).await;
    let html = format!("<!doctype html><head><meta charset=utf-8><script src='/app.mjs'></script></head><body>{}<p>final</p>", "line\n".repeat(700));
    f.write("index.html", &html);
    for (path, value) in [("style.css", "@import '/nested/theme.css';"), ("nested/theme.css", "body{color:red}"), ("app.mjs", "import './nested/module.js';import('./nested/dynamic.js');new Worker('/worker.js');fetch('/data.json');"), ("nested/module.js", "export default 1"), ("nested/dynamic.js", "export default 2"), ("worker.js", "postMessage(1)"), ("data.json", "{\"ok\":true}"), (".context/artifact.json", "{}"), ("media.mp4", "0123456789"), ("font.woff2", "font"), ("image.png", "image"), ("module.wasm", "wasm")] { f.write(path, value); }
    f.write("__conductor_preview/private.js", "forbidden namespace");
    f.write(".env.test", "synthetic-protected-file");
    let session = f.file().await;
    let cookie = f.cookie(&session).await;
    assert_eq!(
        f.manager.source(session.id.clone()).await.unwrap().content,
        html
    );
    let preview = f
        .get(&session, &session.entry_path, &cookie)
        .await
        .text()
        .await
        .unwrap();
    assert!(
        preview.find("/__conductor_preview/diagnostics.js").unwrap()
            < preview.find("src='/app.mjs'").unwrap()
    );
    for (path, mime) in [
        ("/style.css", "text/css"),
        ("/nested/theme.css", "text/css"),
        ("/app.mjs", "text/javascript"),
        ("/nested/module.js", "text/javascript"),
        ("/nested/dynamic.js", "text/javascript"),
        ("/worker.js", "text/javascript"),
        ("/data.json", "application/json"),
        ("/.context/artifact.json", "application/json"),
        ("/font.woff2", "font/woff2"),
        ("/image.png", "image/png"),
        ("/module.wasm", "application/wasm"),
    ] {
        let response = f.get(&session, path, &cookie).await;
        assert_eq!(response.status(), 200, "{path}");
        assert_eq!(response.headers()["content-type"], mime);
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    }
    for (range, status, expected) in [
        ("bytes=2-5", 206, "2345"),
        ("bytes=-3", 206, "789"),
        ("bytes=99-", 416, ""),
        ("bytes=0-1,4-5", 200, "0123456789"),
    ] {
        let response = f
            .client
            .get(format!("{}/media.mp4", session.origin))
            .header("Cookie", &cookie)
            .header("Range", range)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        assert_eq!(response.headers()["accept-ranges"], "bytes");
        assert_eq!(response.text().await.unwrap(), expected);
    }
    let head = f
        .client
        .head(format!("{}/media.mp4", session.origin))
        .header("Cookie", &cookie)
        .header("Range", "bytes=1-2")
        .send()
        .await
        .unwrap();
    assert_eq!(head.status(), 200);
    assert_eq!(head.headers()["content-length"], "10");
    assert!(head.bytes().await.unwrap().is_empty());
    for path in [
        "/",
        "/nested/",
        "/.env.test",
        "/__conductor_preview/private.js",
        "/api/state",
        "/%2e%2e/outside",
        "/%252e%252e/outside",
        "/nested%2f..%2f..%2foutside",
    ] {
        assert!(
            f.get(&session, path, &cookie)
                .await
                .status()
                .is_client_error(),
            "{path}"
        );
    }
    let worker = f
        .client
        .get(format!("{}/worker.js", session.origin))
        .header("Cookie", &cookie)
        .header("Service-Worker", "script")
        .send()
        .await
        .unwrap();
    assert_eq!(worker.status(), 403);
    let post = f
        .client
        .post(format!("{}/data.json", session.origin))
        .header("Cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(post.status(), 405);
    let wrong_host = f
        .client
        .get(format!("{}{}", session.origin, session.entry_path))
        .header("Cookie", &cookie)
        .header("Host", "attacker.example")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_host.status(), 403);
    f.write("index.html", "<!DOCTYPE html><p>updated</p>");
    assert_eq!(
        f.manager.source(session.id.clone()).await.unwrap().content,
        "<!DOCTYPE html><p>updated</p>"
    );
    assert!(f
        .get(&session, &session.entry_path, &cookie)
        .await
        .text()
        .await
        .unwrap()
        .contains("updated"));
    let failures: Vec<Value> = f
        .get(
            &session,
            &format!("/__conductor_preview/failures?session={}", session.id),
            &cookie,
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(failures.iter().any(|failure| failure["kind"] == "http"));
    assert!(failures.len() <= 100);
    f.manager.close_all().await.unwrap();
}

#[tokio::test]
async fn grants_are_listener_bound_one_use_and_expire_at_thirty_seconds() {
    let f = Fixture::new(false).await;
    let first = f.inline("<p>one</p>").await;
    let other = f.inline("<p>two</p>").await;
    let wrong = f
        .client
        .post(format!("{}/__conductor_preview/authorize", other.origin))
        .header("Origin", &other.origin)
        .json(&json!({"sessionId":first.id,"grant":first.grant}))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 403);
    let parent_origin = f
        .client
        .post(format!("{}/__conductor_preview/authorize", first.origin))
        .header("Origin", "http://localhost:8790")
        .json(&json!({"sessionId":first.id,"grant":first.grant}))
        .send()
        .await
        .unwrap();
    assert_eq!(parent_origin.status(), 403);
    let cookie = f.cookie(&first).await;
    assert_eq!(f.authorize(&first).await.status(), 403);
    assert_eq!(
        f.get(&other, &other.entry_path, &cookie).await.status(),
        403
    );
    let duplicate = format!("{cookie}; {cookie}");
    assert_eq!(
        f.get(&first, &first.entry_path, &duplicate).await.status(),
        403
    );
    f.clock.advance(30);
    assert_eq!(f.authorize(&other).await.status(), 403);
    assert_eq!(
        f.client
            .get(&other.bootstrap_url)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        f.get(&first, &first.entry_path, &cookie).await.status(),
        200
    );
    let new = f.inline("<p>three</p>").await;
    let malformed = f
        .client
        .post(format!("{}/__conductor_preview/authorize", new.origin))
        .header("Origin", &new.origin)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status(), 400);
    let large = f
        .client
        .post(format!("{}/__conductor_preview/authorize", new.origin))
        .header("Origin", &new.origin)
        .body("x".repeat(2049))
        .send()
        .await
        .unwrap();
    assert_eq!(large.status(), 413);
    f.manager.close_all().await.unwrap();
}

#[tokio::test]
async fn simultaneous_viewers_share_cookie_and_closing_one_preserves_the_other() {
    let f = Fixture::new(false).await;
    let request = f.inline_request(
        "<!doctype html><p>shared</p>",
        "http://localhost:8790",
        false,
    );
    let (a, b) = tokio::join!(f.manager.create(request.clone()), f.manager.create(request));
    let a = a.unwrap();
    let b = b.unwrap();
    assert_eq!(a.origin, b.origin);
    assert!(a.id != b.id && a.grant != b.grant);
    let (ca, cb) = tokio::join!(f.cookie(&a), f.cookie(&b));
    assert!(ca == cb);
    let context = |id: &str| format!("/__conductor_preview/context?session={id}");
    assert_eq!(f.get(&a, &context(&a.id), &ca).await.status(), 200);
    assert_eq!(f.get(&b, &context(&b.id), &cb).await.status(), 200);
    f.manager.close(a.id.clone()).await.unwrap();
    assert_eq!(f.get(&b, &context(&a.id), &ca).await.status(), 403);
    let reload = f.client.get(&b.bootstrap_url).send().await.unwrap();
    assert_eq!(reload.status(), 200);
    assert!(!reload.headers().contains_key("set-cookie"));
    assert_eq!(f.get(&b, &b.entry_path, &cb).await.status(), 200);
    let third = f.inline("<!doctype html><p>shared</p>").await;
    let cc = f.cookie(&third).await;
    assert!(cc == cb);
    f.manager.close(b.id.clone()).await.unwrap();
    assert_eq!(f.get(&third, &third.entry_path, &cc).await.status(), 200);
    f.manager.close(third.id.clone()).await.unwrap();
    assert!(f.manager.source(b.id.clone()).await.is_err());
    if let Ok(response) = f
        .client
        .get(format!("{}{}", third.origin, third.entry_path))
        .header("Cookie", &cc)
        .send()
        .await
    {
        assert_eq!(response.status(), 403);
    }
    let reopened = f.inline("<!doctype html><p>shared</p>").await;
    assert_eq!(reopened.origin, a.origin);
    assert_eq!(
        f.get(&reopened, &reopened.entry_path, &cc).await.status(),
        403
    );
    assert_eq!(f.authorize(&a).await.status(), 403);
    let fresh = f.cookie(&reopened).await;
    assert!(fresh != cc);
    f.manager.close_all().await.unwrap();
}

#[tokio::test]
async fn inactive_cookie_retirement_preserves_active_siblings_and_new_leases_rotate() {
    let f = Fixture::new(false).await;
    let a = f.inline("a").await;
    let ca = f.cookie(&a).await;
    let b = f.inline("b").await;
    let cb = f.cookie(&b).await;
    f.manager.close(a.id.clone()).await.unwrap();
    let c = f.inline("c").await;
    let bootstrap = f
        .client
        .get(&c.bootstrap_url)
        .header("Cookie", format!("{ca}; {cb}"))
        .send()
        .await
        .unwrap();
    let retired: Vec<_> = bootstrap
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|h| h.to_str().unwrap())
        .collect();
    let a_name = ca.split('=').next().unwrap();
    let b_name = cb.split('=').next().unwrap();
    assert!(retired
        .iter()
        .any(|h| h.starts_with(a_name) && h.contains("Max-Age=0")));
    assert!(!retired.iter().any(|h| h.starts_with(b_name)));
    let cc = f.cookie(&c).await;
    assert_eq!(
        f.get(&b, &b.entry_path, &format!("{ca}; {cc}"))
            .await
            .status(),
        403
    );
    assert_eq!(
        f.get(&b, &b.entry_path, &format!("{ca}; {cb}; {cc}"))
            .await
            .status(),
        200
    );
    f.manager.close_all().await.unwrap();
}

#[tokio::test]
async fn idle_expiry_limit_and_restart_keep_identity_without_resurrecting_sessions() {
    let f = Fixture::new(false).await;
    let a = f.inline("identity-a").await;
    let old = f.cookie(&a).await;
    f.clock.advance(1799);
    assert_eq!(f.get(&a, &a.entry_path, &old).await.status(), 200);
    f.clock.advance(1800);
    f.manager.sweep().await.unwrap();
    assert!(f.manager.source(a.id.clone()).await.is_err());
    let b = f.inline("identity-b").await;
    assert!(a.origin != b.origin);
    f.manager.close_all().await.unwrap();
    let restarted = ArtifactPreviews::new(f.config.clone()).unwrap();
    restarted.reconcile_startup().await.unwrap();
    assert!(restarted.source(a.id.clone()).await.is_err());
    let new = restarted
        .create(f.inline_request("identity-a", "http://localhost:8790", false))
        .await
        .unwrap();
    assert_eq!(new.origin, a.origin);
    assert_eq!(f.get(&new, &new.entry_path, &old).await.status(), 403);
    assert_eq!(f.authorize(&a).await.status(), 403);
    for _ in 0..15 {
        restarted
            .create(f.inline_request("identity-a", "http://localhost:8790", false))
            .await
            .unwrap();
    }
    assert!(matches!(
        restarted
            .create(f.inline_request("identity-a", "http://localhost:8790", false))
            .await,
        Err(ArtifactError::Unavailable(_))
    ));
    restarted.close_all().await.unwrap();
    let snapshot = f.exposure.store.snapshot().unwrap();
    assert!(snapshot.bindings.iter().all(|binding| binding.tombstoned));
    // Persisted metadata contains digests/names only. No document, control ID, grant or value.
    let persisted = fs::read_to_string(f.config.state_dir.join("artifact-state.json")).unwrap();
    for secret in [&a.id, &a.grant, &old, "identity-a"] {
        assert!(!persisted.contains(secret));
    }
}

#[tokio::test]
async fn parent_origin_and_inline_root_policy_are_exact_and_local_needs_no_adapter() {
    let f = Fixture::new(false).await;
    for parent in [
        "http://localhost:8791",
        "http://localhost:5173",
        "http://localhost:8790/",
        "http://localhost:8790/path",
        "http://localhost:8790?x=1",
        "http://localhost:8790#x",
        "http://user@localhost:8790",
        "http://localhost.evil:8790",
        "https://relay.example.test.evil",
        "http://LOCALHOST:8790",
    ] {
        assert!(
            f.manager
                .create(f.inline_request("x", parent, false))
                .await
                .is_err(),
            "{parent}"
        );
    }
    for parent in ["http://127.0.0.1:8790", "http://localhost:5179"] {
        let session = f
            .manager
            .create(f.inline_request("x", parent, true))
            .await
            .unwrap();
        assert_eq!(session.origin.split(':').nth(1), parent.split(':').nth(1));
        let cookie = f.cookie(&session).await;
        assert_eq!(
            f.get(&session, &session.entry_path, &cookie).await.status(),
            200
        );
    }
    assert!(f
        .manager
        .create(f.inline_request("x", "https://relay.example.test", false))
        .await
        .is_err());
    let mut request = f.inline_request("x", "http://localhost:8790", false);
    if let CreateArtifactPreviewRequest::Html { base_directory, .. } = &mut request {
        *base_directory = Some(f.root.parent().unwrap().to_str().unwrap().into());
    }
    assert!(matches!(
        f.manager.create(request).await,
        Err(ArtifactError::Forbidden(_))
    ));
    f.manager.close_all().await.unwrap();
}

// Remote tests connect directly to the fake adapter's loopback target and supply its bound
// authority. This verifies controller permission/receipt behavior, not HTTPS/Tailscale.
async fn remote_request(
    f: &Fixture,
    session: &ArtifactPreviewSession,
    path: &str,
    cookie: Option<&str>,
    authorize: bool,
) -> Response {
    let snapshot = f.exposure.store.snapshot().unwrap();
    let receipt = &snapshot
        .receipts
        .iter()
        .find(|r| r.receipt.origin == session.origin)
        .unwrap()
        .receipt;
    let authority = session.origin.strip_prefix("https://").unwrap();
    let url = format!("http://127.0.0.1:{}{path}", receipt.loopback_port);
    let mut request = if authorize {
        f.client
            .post(url)
            .header("Origin", &session.origin)
            .json(&json!({"sessionId":session.id,"grant":session.grant}))
    } else {
        f.client.get(url)
    };
    request = request.header("Host", authority);
    if let Some(cookie) = cookie {
        request = request.header("Cookie", cookie);
    }
    request.send().await.unwrap()
}

#[tokio::test]
async fn remote_adapter_receipts_dynamic_revocation_and_local_survival() {
    let f = Fixture::new(true).await;
    let request = f.inline_request("remote", "https://relay.example.test", false);
    let a = f.manager.create(request.clone()).await.unwrap();
    let b = f.manager.create(request).await.unwrap();
    assert_eq!(a.origin, b.origin);
    assert_eq!(f.exposure.publications.load(Ordering::SeqCst), 1);
    let port = a.origin.rsplit(':').next().unwrap().parse::<u16>().unwrap();
    assert!((30000..=39999).contains(&port));
    let response = remote_request(&f, &a, "/__conductor_preview/authorize", None, true).await;
    assert_eq!(response.status(), 200);
    let header = response.headers()["set-cookie"].to_str().unwrap();
    assert!(header.contains("Secure"));
    let cookie = header.split(';').next().unwrap().to_owned();
    assert_eq!(
        remote_request(&f, &a, &a.entry_path, Some(&cookie), false)
            .await
            .status(),
        200
    );
    let local = f.inline("local").await;
    let local_cookie = f.cookie(&local).await;
    f.exposure.store.lock().unwrap().revoke_remote().unwrap();
    f.manager.sweep().await.unwrap();
    assert!(f.exposure.store.snapshot().unwrap().receipts.is_empty());
    assert!(f.manager.source(a.id).await.is_err());
    assert!(f.manager.source(b.id).await.is_err());
    assert_eq!(
        f.get(&local, &local.entry_path, &local_cookie)
            .await
            .status(),
        200
    );
    assert!(f
        .manager
        .create(f.inline_request("remote", "https://relay.example.test", false))
        .await
        .is_err());
    f.manager.close_all().await.unwrap();
}

#[tokio::test]
async fn restrictive_document_csp_is_preserved_and_instrumentation_is_only_attempted() {
    let f = Fixture::new(false).await;
    let html="<!DOCTYPE html><html><head><meta charset='utf-8'><meta http-equiv='Content-Security-Policy' content=\"default-src 'none'; script-src 'none'\"><script>throw new Error('blocked')</script></head></html>";
    let a = f.inline(html).await;
    let cookie = f.cookie(&a).await;
    let response = f.get(&a, &a.entry_path, &cookie).await;
    assert_eq!(response.status(), 200);
    let preview = response.text().await.unwrap();
    assert!(preview.contains("default-src 'none'; script-src 'none'"));
    assert!(
        preview.find("script-src 'none'").unwrap()
            < preview.find("/__conductor_preview/diagnostics.js").unwrap()
    );
    assert!(
        preview.find("/__conductor_preview/diagnostics.js").unwrap()
            < preview.find("throw new Error").unwrap()
    );
    assert_eq!(f.manager.source(a.id.clone()).await.unwrap().content, html);
    let diagnostics = f
        .get(&a, "/__conductor_preview/diagnostics.js", &cookie)
        .await
        .text()
        .await
        .unwrap();
    for event in [
        "unhandledrejection",
        "Resource failed to load",
        "Fetch failed",
        "XMLHttpRequest",
        "count>=100",
        "slice(0,max)",
        "parent.postMessage(m,parentOrigin)",
    ] {
        assert!(diagnostics.contains(event));
    }
    // With script-src 'none', a browser blocks the injected external script. It sends neither
    // loaded nor diagnostic; the existing parent watchdog reports instrumentation unavailable.
    // HTTP/static tests deliberately make no browser execution/CSP-enforcement claim.
    f.manager.close_all().await.unwrap();
}

#[tokio::test]
async fn raw_traversal_and_main_bearer_do_not_authorize_preview() {
    let f = Fixture::new(false).await;
    let a = f.inline("x").await;
    let port = a.origin.rsplit(':').next().unwrap().parse::<u16>().unwrap();
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let request=format!("GET /%2e%2e/elsewhere HTTP/1.1\r\nHost: localhost:{port}\r\nAuthorization: Bearer synthetic-main-token\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    assert!(response.starts_with(b"HTTP/1.1 403"));
    f.manager.close_all().await.unwrap();
}

#[tokio::test]
async fn diagnostic_polling_does_not_renew_idle_authority_or_cookie() {
    let f = Fixture::new(false).await;
    let a = f.inline("idle").await;
    let cookie = f.cookie(&a).await;
    f.clock.advance(1799);
    for endpoint in ["context", "failures"] {
        let response = f
            .get(
                &a,
                &format!("/__conductor_preview/{endpoint}?session={}", a.id),
                &cookie,
            )
            .await;
        assert_eq!(response.status(), 200);
        assert!(!response.headers().contains_key("set-cookie"));
    }
    f.clock.advance(1);
    assert_eq!(
        f.get(
            &a,
            &format!("/__conductor_preview/context?session={}", a.id),
            &cookie
        )
        .await
        .status(),
        403
    );
    assert!(f.manager.source(a.id.clone()).await.is_err());
    f.manager.close_all().await.unwrap();
}

#[tokio::test]
async fn cleanup_failure_retains_receipt_and_does_not_disable_local_previews() {
    let f = Fixture::new(true).await;
    let remote = f
        .manager
        .create(f.inline_request("remote", "https://relay.example.test", false))
        .await
        .unwrap();
    let _ = remote_request(&f, &remote, "/__conductor_preview/authorize", None, true).await;
    f.exposure.fail_cleanup.store(true, Ordering::SeqCst);
    f.exposure.store.lock().unwrap().revoke_remote().unwrap();
    assert!(f.manager.sweep().await.is_err());
    assert_eq!(f.exposure.store.snapshot().unwrap().receipts.len(), 1);
    let local = f.inline("still local").await;
    let cookie = f.cookie(&local).await;
    assert_eq!(
        f.get(&local, &local.entry_path, &cookie).await.status(),
        200
    );
    f.exposure.fail_cleanup.store(false, Ordering::SeqCst);
    f.manager.sweep().await.unwrap();
    assert!(f.exposure.store.snapshot().unwrap().receipts.is_empty());
    f.manager.close_all().await.unwrap();
}

#[tokio::test]
async fn publication_allocation_races_retry_without_reassigning_failed_origins() {
    let f = Fixture::new(true).await;
    f.exposure.collisions.store(1, Ordering::SeqCst);
    let a = f
        .manager
        .create(f.inline_request("remote", "https://relay.example.test", false))
        .await
        .unwrap();
    assert_eq!(f.exposure.publications.load(Ordering::SeqCst), 2);
    let snapshot = f.exposure.store.snapshot().unwrap();
    assert_eq!(snapshot.bindings.len(), 2);
    let failed = snapshot
        .bindings
        .iter()
        .find(|b| b.origin != a.origin)
        .unwrap();
    assert!(failed.tombstoned);
    assert_eq!(
        failed.identity,
        snapshot
            .bindings
            .iter()
            .find(|b| b.origin == a.origin)
            .unwrap()
            .identity
    );
    let b = f
        .manager
        .create(f.inline_request("another", "https://relay.example.test", false))
        .await
        .unwrap();
    assert!(b.origin != a.origin && b.origin != failed.origin);
    f.manager.close_all().await.unwrap();
}

#[tokio::test]
async fn remote_requests_observe_revocation_without_a_sweep() {
    let f = Fixture::new(true).await;
    let remote = f
        .manager
        .create(f.inline_request("remote", "https://relay.example.test", false))
        .await
        .unwrap();
    let auth = remote_request(&f, &remote, "/__conductor_preview/authorize", None, true).await;
    let cookie = auth.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    f.exposure.store.lock().unwrap().revoke_remote().unwrap();
    assert_eq!(
        remote_request(&f, &remote, &remote.entry_path, Some(&cookie), false)
            .await
            .status(),
        403
    );
    assert!(f.exposure.store.snapshot().unwrap().receipts.is_empty());
    f.manager.close_all().await.unwrap();
}

#[tokio::test]
async fn startup_reconciles_stale_pending_receipts_before_creates() {
    let f = Fixture::new(true).await;
    let a = f
        .manager
        .create(f.inline_request("remote", "https://relay.example.test", false))
        .await
        .unwrap();
    let snapshot = f.exposure.store.snapshot().unwrap();
    let receipt = snapshot.receipts[0].receipt.clone();
    let binding = snapshot.bindings[0].clone();
    f.manager.close_all().await.unwrap();
    {
        let mut guard = f.exposure.store.lock().unwrap();
        guard
            .reserve_origin(
                &binding.identity,
                &binding.origin,
                &binding.cookie_name,
                f.clock.now(),
            )
            .unwrap();
        guard.begin_publication(receipt, true).unwrap();
    }
    let restarted = ArtifactPreviews::new(f.config.clone()).unwrap();
    assert!(restarted
        .create(f.inline_request("remote", "https://relay.example.test", false))
        .await
        .is_err());
    restarted.reconcile_startup().await.unwrap();
    assert!(f.exposure.store.snapshot().unwrap().receipts.is_empty());
    assert!(restarted.source(a.id.clone()).await.is_err());
    let new = restarted
        .create(f.inline_request("remote", "https://relay.example.test", false))
        .await
        .unwrap();
    assert_eq!(new.origin, a.origin);
    assert!(new.id != a.id && new.grant != a.grant);
    restarted.close_all().await.unwrap();
}
