//! Synthetic inputs around the real main router and artifact service. No project commands,
//! remote exposure, browser execution or application credentials are involved.
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use conductor_remote::artifacts::{
    ArtifactClock, ArtifactEntropy, ArtifactError, ArtifactPreviewConfig, ArtifactPreviewSession,
    ArtifactPreviews,
};
use conductor_remote::contract::{AppState, ConductorStatus, Services, Token};
use conductor_remote::http::router;
use conductor_remote::testing::{FakeConductor, MemoryAssets};
use reqwest::{Client, Method, RequestBuilder, Response};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub const COLLECTION: &str = "/api/artifact-previews";
// Test-only main authentication, never read from a running relay.
pub const TOKEN: &str = "artifact-acceptance-test-only";
pub const INJECTION: &str = "<script src=\"/__conductor_preview/diagnostics.js\"></script>";

#[derive(Default)]
pub struct Clock(AtomicU64);
impl Clock {
    pub fn advance(&self, seconds: u64) {
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
            .map_err(|_| ArtifactError::Unavailable("test randomness unavailable".into()))
    }
}

pub struct Fixture {
    _temp: TempDir,
    pub root: PathBuf,
    pub parent: String,
    pub manager: Arc<ArtifactPreviews>,
    pub clock: Arc<Clock>,
    pub client: Client,
    shutdown: oneshot::Sender<()>,
    server: JoinHandle<()>,
}
impl Fixture {
    pub async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let root = home.join("workspace/artifacts");
        fs::create_dir_all(&root).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let parent = format!("http://{}", listener.local_addr().unwrap());
        let clock = Arc::new(Clock::default());
        let manager = Arc::new(
            ArtifactPreviews::new(ArtifactPreviewConfig {
                home: Some(home.clone()),
                source_roots: vec![home.clone()],
                broad_roots: vec![home.clone()],
                state_dir: home.join("state"),
                allowed_parent_origins: [parent.clone()].into_iter().collect(),
                parent_origins: None,
                clock: clock.clone(),
                entropy: Arc::new(Entropy),
                exposure: None,
            })
            .unwrap(),
        );
        manager.reconcile_startup().await.unwrap();
        let app = router(AppState {
            token: Arc::new(Token::new(TOKEN)),
            conductor: Arc::new(FakeConductor::new(ConductorStatus::Running)),
            assets: Arc::new(MemoryAssets::default().with("index.html", "text/html", b"main app")),
            reads: None,
            writes: None,
            notify: None,
            services: Services {
                artifacts: Some(manager.clone()),
                ..Default::default()
            },
        });
        let (shutdown, stopped) = oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        Self {
            _temp: temp,
            root,
            parent,
            manager,
            clock,
            client: Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            shutdown,
            server,
        }
    }

    pub fn write(&self, name: &str, bytes: impl AsRef<[u8]>) {
        let path = self.root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    pub fn control(&self, method: Method, path: &str) -> RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.parent))
            .bearer_auth(TOKEN)
    }

    pub fn file_request(&self) -> Value {
        json!({"kind":"file", "reference":"~/workspace/artifacts/index.htm:650:2", "parentOrigin":self.parent})
    }

    pub async fn create(&self, request: Value) -> ArtifactPreviewSession {
        let response = self
            .control(Method::POST, COLLECTION)
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert!(!response.headers().contains_key("set-cookie"));
        response.json().await.unwrap()
    }

    pub async fn source(&self, session: &ArtifactPreviewSession) -> Response {
        self.control(Method::GET, &format!("{COLLECTION}/{}/source", session.id))
            .send()
            .await
            .unwrap()
    }

    pub async fn close(&self, session: &ArtifactPreviewSession) {
        let response = self
            .control(Method::DELETE, &format!("{COLLECTION}/{}", session.id))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.json::<Value>().await.unwrap(), json!({"ok":true}));
    }

    pub async fn authorize(&self, session: &ArtifactPreviewSession) -> Response {
        self.client
            .post(format!("{}/__conductor_preview/authorize", session.origin))
            .header("Origin", &session.origin)
            .json(&json!({"sessionId":session.id, "grant":session.grant}))
            .send()
            .await
            .unwrap()
    }

    pub async fn cookie(&self, session: &ArtifactPreviewSession) -> String {
        let response = self.authorize(session).await;
        assert_eq!(response.status(), 200);
        let value = response.headers()["set-cookie"].to_str().unwrap();
        for attribute in ["HttpOnly", "Path=/", "SameSite=Strict", "Max-Age=1800"] {
            assert!(value.contains(attribute));
        }
        assert!(!value.contains("Domain="));
        assert!(!value.contains("Secure")); // This fixture is loopback HTTP only.
        value.split(';').next().unwrap().into()
    }

    pub async fn resource(
        &self,
        session: &ArtifactPreviewSession,
        path: &str,
        cookie: &str,
    ) -> Response {
        self.client
            .get(format!("{}{path}", session.origin))
            .header("Cookie", cookie)
            .send()
            .await
            .unwrap()
    }

    pub async fn stop(self) {
        self.manager.close_all().await.unwrap();
        self.shutdown.send(()).unwrap();
        self.server.await.unwrap();
    }
}

pub fn document() -> String {
    format!(
        "<!DOCTYPE html>\r\n<html><head><meta charset=\"utf-8\"><link rel=\"stylesheet\" href=\"/css/main.css\"></head><body>\n{}<img src=\"/images/pixel.svg\"><img src=\"/missing.png\"><button id=\"final-marker\" onclick=\"this.textContent='clicked'\">final marker</button><script type=\"module\" src=\"/js/main.mjs\"></script></body></html>\n",
        "<!-- original line -->\n".repeat(700)
    )
}

/// The font is deliberately a transport fixture; HTTP checks do not prove font decoding.
pub const RESOURCES: &[(&str, &str, &[u8])] = &[
    ("/css/main.css", "text/css", b"@import './nested/theme.css'; @font-face{font-family:fixture;src:url('/fonts/test.woff2')} body{font-family:fixture}"),
    ("/css/nested/theme.css", "text/css", b"body{color:rgb(10,20,30);background-image:url('/images/pixel.svg')}"),
    ("/images/pixel.svg", "image/svg+xml", b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1\" height=\"1\"><rect width=\"1\" height=\"1\" fill=\"red\"/></svg>"),
    ("/fonts/test.woff2", "font/woff2", b"wOF2-synthetic-font-transport"),
    ("/data.json", "application/json", b"{\"ok\":true,\"marker\":\"full-document\"}"),
    ("/js/main.mjs", "text/javascript", b"import {marker} from './nested/module.js'; const lazy=await import('./nested/lazy.js'); const data=await fetch('/data.json').then(r=>r.json()); document.querySelector('#final-marker').dataset.loaded=marker+lazy.default+data.ok; new Worker('/worker.js'); fetch('/module.wasm'); throw new Error('synthetic artifact failure');"),
    ("/js/nested/module.js", "text/javascript", b"export const marker='module';"),
    ("/js/nested/lazy.js", "text/javascript", b"export default 'dynamic';"),
    ("/worker.js", "text/javascript", b"postMessage('worker');"),
    ("/module.wasm", "application/wasm", b"\0asm\x01\0\0\0"),
    ("/.context/artifact.json", "application/json", b"{\"safe\":true}"),
];
