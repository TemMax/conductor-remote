//! Real preview lifecycle and loopback HTTP with synthetic exposure commands only.
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use conductor_remote::artifacts::exposure::TailscaleArtifactExposure;
use conductor_remote::artifacts::store::{ArtifactStore, ArtifactStoreSnapshot};
use conductor_remote::artifacts::*;
use conductor_remote::contract::{AppState, ConductorStatus, Services, Token};
use conductor_remote::reads::extras::commands::{CommandError, Commands, Limits, Output};
use conductor_remote::testing::{FakeConductor, MemoryAssets};
use reqwest::Client;
use serde_json::{json, Value};
use tempfile::TempDir;

pub const TOKEN: &str = "artifact-startup-synthetic-auth";
const REMOTE: &str = "https://relay.example.test";

// Keep independent persistence domains from reusing one another's just-closed local ports.
pub static LISTENERS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Default)]
pub struct ExposureCommands {
    pub fail_cleanup: AtomicBool,
    pub removals: AtomicUsize,
    mappings: Mutex<BTreeMap<u16, String>>,
}

impl Commands for ExposureCommands {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        _: Limits,
    ) -> Result<Output, CommandError> {
        assert_eq!(program, "/synthetic/tailscale");
        assert!(cwd.is_none());
        let mut mappings = self.mappings.lock().unwrap();
        let mut code = 0;
        let mut stdout = String::new();
        if args == ["serve", "status", "--json"] {
            let mut tcp = serde_json::Map::new();
            let mut web = serde_json::Map::new();
            for (port, target) in mappings.iter() {
                tcp.insert(port.to_string(), json!({"HTTPS":true}));
                web.insert(
                    format!("relay.example.test:{port}"),
                    json!({"Handlers":{"/":{"Proxy":target}}}),
                );
            }
            stdout = json!({"TCP":tcp,"Web":web}).to_string();
        } else {
            assert_eq!(args.first(), Some(&"serve"));
            let port = args
                .iter()
                .find_map(|s| s.strip_prefix("--https="))
                .unwrap()
                .parse()
                .unwrap();
            if args.last() == Some(&"off") {
                self.removals.fetch_add(1, Ordering::SeqCst);
                if self.fail_cleanup.load(Ordering::SeqCst) {
                    code = 1;
                } else {
                    mappings.remove(&port);
                }
            } else {
                mappings.insert(port, args.last().unwrap().to_string());
            }
        }
        Ok(Output {
            code: Some(code),
            stdout: stdout.into_bytes(),
            stderr: Vec::new(),
        })
    }
}

struct Clock;
impl ArtifactClock for Clock {
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }
    fn monotonic(&self) -> Duration {
        Duration::ZERO
    }
}
struct Entropy;
impl ArtifactEntropy for Entropy {
    fn fill(&self, bytes: &mut [u8]) -> Result<(), ArtifactError> {
        getrandom::fill(bytes).map_err(|_| ArtifactError::Unavailable("test entropy failed".into()))
    }
}

pub struct Fixture {
    pub temp: TempDir,
    pub manager: Arc<ArtifactPreviews>,
    pub commands: Arc<ExposureCommands>,
    pub store: ArtifactStore,
    pub before: ArtifactStoreSnapshot,
    pub parent: String,
    pub client: Client,
    config: ArtifactPreviewConfig,
}

impl Fixture {
    pub fn new(parent: String) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        std::fs::create_dir(base.join("project")).unwrap();
        std::fs::write(base.join("project/index.html"), "<p>recovered preview</p>").unwrap();
        let state_dir = base.join("state");
        let store = ArtifactStore::open(&state_dir.join("artifacts")).unwrap();
        let commands = Arc::new(ExposureCommands::default());
        commands.fail_cleanup.store(true, Ordering::SeqCst);
        {
            let mut guard = store.lock().unwrap();
            for (index, port) in [30000, 30001].into_iter().enumerate() {
                let binding = guard
                    .reserve_origin(
                        &format!("{index:064x}"),
                        &format!("{REMOTE}:{port}"),
                        &format!("conductor_preview_{index:032x}"),
                        Clock.now(),
                    )
                    .unwrap();
                let receipt = ArtifactExposureReceipt {
                    binding_id: binding.binding_id.clone(),
                    loopback_port: 45000 + index as u16,
                    https_port: port,
                    origin: binding.origin,
                    generation: 0,
                };
                guard.begin_publication(receipt.clone(), true).unwrap();
                if index == 1 {
                    guard.mark_active(&receipt).unwrap();
                }
                commands
                    .mappings
                    .lock()
                    .unwrap()
                    .insert(port, format!("http://127.0.0.1:{}", receipt.loopback_port));
                guard.tombstone_binding(&binding.binding_id).unwrap();
            }
            guard.revoke_remote().unwrap();
        }
        let exposure = TailscaleArtifactExposure::new(
            &state_dir,
            Some("/synthetic/tailscale".into()),
            commands.clone(),
            Arc::new(|| Ok(true)),
        )
        .unwrap();
        let config = ArtifactPreviewConfig {
            home: None,
            source_roots: vec![base.clone()],
            broad_roots: vec![base],
            state_dir: state_dir.join("artifacts"),
            allowed_parent_origins: [parent.clone(), REMOTE.into()].into_iter().collect(),
            parent_origins: None,
            clock: Arc::new(Clock),
            entropy: Arc::new(Entropy),
            exposure: Some(Arc::new(exposure)),
        };
        let manager = Arc::new(ArtifactPreviews::new(config.clone()).unwrap());
        let before = store.snapshot().unwrap();
        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        Self {
            temp,
            manager,
            commands,
            store,
            before,
            parent,
            client,
            config,
        }
    }

    pub fn state(&self) -> AppState {
        AppState {
            token: Arc::new(Token::new(TOKEN)),
            conductor: Arc::new(FakeConductor::new(ConductorStatus::NotRunning)),
            assets: Arc::new(MemoryAssets::default()),
            reads: None,
            writes: None,
            notify: None,
            services: Services {
                artifacts: Some(self.manager.clone()),
                ..Default::default()
            },
        }
    }

    pub fn requests(&self) -> Vec<CreateArtifactPreviewRequest> {
        [self.parent.as_str(), REMOTE]
            .into_iter()
            .flat_map(|parent| {
                [
                    CreateArtifactPreviewRequest::File {
                        reference: self
                            .temp
                            .path()
                            .join("project/index.html")
                            .to_str()
                            .unwrap()
                            .into(),
                        parent_origin: parent.into(),
                    },
                    CreateArtifactPreviewRequest::Html {
                        html: "<p>inline recovery</p>".into(),
                        base_directory: None,
                        parent_origin: parent.into(),
                    },
                ]
            })
            .collect()
    }

    pub async fn assert_blocked(&self) {
        for request in self.requests() {
            assert!(matches!(
                self.manager.create(request).await,
                Err(ArtifactError::Unavailable(_))
            ));
        }
        assert_eq!(self.store.snapshot().unwrap(), self.before);
    }

    pub async fn assert_preview(&self, session: &ArtifactPreviewSession) {
        assert_eq!(
            self.client
                .get(&session.bootstrap_url)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        let authorized = self
            .client
            .post(format!("{}/__conductor_preview/authorize", session.origin))
            .header("Origin", &session.origin)
            .json(&json!({"sessionId":session.id,"grant":session.grant}))
            .send()
            .await
            .unwrap();
        assert_eq!(authorized.status(), 200);
        let cookie = authorized.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        let page = self
            .client
            .get(format!("{}{}", session.origin, session.entry_path))
            .header("Cookie", cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), 200);
        assert!(page.text().await.unwrap().contains("recovered preview"));
    }
}

#[tokio::test]
async fn maintenance_retries_failed_startup_preserving_receipts_until_preview_recovery() {
    let _listeners = LISTENERS.lock().await;
    let f = Fixture::new("http://127.0.0.1:8790".into());
    assert!(f.manager.reconcile_startup().await.is_err());
    f.assert_blocked().await;
    assert!(
        f.manager.sweep().await.is_err(),
        "maintenance must retry failed startup"
    );
    f.assert_blocked().await;
    assert!(f.commands.removals.load(Ordering::SeqCst) >= 4);
    f.commands.fail_cleanup.store(false, Ordering::SeqCst);
    f.manager.sweep().await.unwrap();
    let recovered = f.store.snapshot().unwrap();
    assert!(recovered.receipts.is_empty());
    assert_eq!(recovered.bindings, f.before.bindings);
    assert_eq!(recovered.generation, f.before.generation);
    assert_eq!(recovered.remote_revoked, f.before.remote_revoked);
    let attempts = f.commands.removals.load(Ordering::SeqCst);
    f.manager.sweep().await.unwrap();
    assert_eq!(f.commands.removals.load(Ordering::SeqCst), attempts);
    let local = f.manager.create(f.requests().remove(0)).await.unwrap();
    f.assert_preview(&local).await;
    // Startup cleanup must not undo explicit remote revocation.
    assert!(matches!(
        f.manager.create(f.requests().remove(2)).await,
        Err(ArtifactError::Forbidden(_))
    ));
    f.store.lock().unwrap().allow_remote().unwrap();
    let remote = f.manager.create(f.requests().remove(2)).await.unwrap();
    assert_eq!(f.store.snapshot().unwrap().receipts.len(), 1);
    f.manager.close_all().await.unwrap();
    assert!(f.store.snapshot().unwrap().receipts.is_empty());
    assert!(f
        .store
        .snapshot()
        .unwrap()
        .bindings
        .iter()
        .all(|binding| binding.tombstoned));
    assert!(f.client.get(&local.bootstrap_url).send().await.is_err());
    assert!(f.manager.source(remote.id).await.is_err());
}

#[tokio::test]
async fn missing_adapter_keeps_creates_blocked_and_receipts_durable() {
    let f = Fixture::new("http://127.0.0.1:8790".into());
    let mut config = f.config.clone();
    config.exposure = None;
    let manager = ArtifactPreviews::new(config).unwrap();
    assert!(manager.reconcile_startup().await.is_err());
    assert!(manager.sweep().await.is_err());
    for request in f.requests() {
        assert!(matches!(
            manager.create(request).await,
            Err(ArtifactError::Unavailable(_))
        ));
    }
    assert!(manager.close_all().await.is_err());
    assert_eq!(f.store.snapshot().unwrap(), f.before);
}

#[tokio::test]
async fn shutdown_retries_pending_startup_and_preserves_failed_cleanup() {
    let f = Fixture::new("http://127.0.0.1:8790".into());
    assert!(f.manager.reconcile_startup().await.is_err());
    assert!(f.manager.close_all().await.is_err());
    assert_eq!(f.store.snapshot().unwrap(), f.before);
    f.commands.fail_cleanup.store(false, Ordering::SeqCst);
    f.manager.close_all().await.unwrap();
    assert!(f.store.snapshot().unwrap().receipts.is_empty());
}

// Used by the binary's startup regression as well as the standalone lifecycle tests.
pub async fn main_api_checks(f: Arc<Fixture>) -> ArtifactPreviewSession {
    let url = format!("{}/api/state", f.parent);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if f.client.get(&url).bearer_auth(TOKEN).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("main API must start even when artifact cleanup fails");
    assert_eq!(f.client.get(&url).send().await.unwrap().status(), 401);
    assert_eq!(
        f.client
            .get(&url)
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    for request in f.requests() {
        let response = f
            .client
            .post(format!("{}/api/artifact-previews", f.parent))
            .bearer_auth(TOKEN)
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 503);
    }
    assert_eq!(f.store.snapshot().unwrap(), f.before);
    f.commands.fail_cleanup.store(false, Ordering::SeqCst);
    let session: ArtifactPreviewSession = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let response = f
                .client
                .post(format!("{}/api/artifact-previews", f.parent))
                .bearer_auth(TOKEN)
                .json(&f.requests().remove(0))
                .send()
                .await
                .unwrap();
            if response.status() == 200 {
                break response.json().await.unwrap();
            }
            assert_eq!(response.status(), 503);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("maintenance must recover previews without restarting main");
    f.assert_preview(&session).await;
    assert!(f.store.snapshot().unwrap().receipts.is_empty());
    let state: Value = f
        .client
        .get(url)
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(state.is_object());
    session
}
