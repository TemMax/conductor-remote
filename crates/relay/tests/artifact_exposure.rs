//! Injected command execution, not a real Tailscale or browser verification.
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, SystemTime};

use conductor_remote::artifacts::exposure::{parent_origins, TailscaleArtifactExposure};
use conductor_remote::artifacts::store::{ArtifactStore, ReceiptPhase};
use conductor_remote::artifacts::*;
use conductor_remote::reads::extras::commands::{CommandError, Commands, Limits, Output};
use conductor_remote::service::{tailnet, CommandOutput, CommandRunner};
use serde_json::json;

#[derive(Default)]
struct Fake {
    mappings: Mutex<BTreeMap<u16, String>>,
    calls: Mutex<Vec<Vec<String>>>,
    fail_off: Mutex<bool>,
    keep_off: Mutex<bool>,
    crash_publish: Mutex<bool>,
    status_race: Mutex<Option<usize>>,
    off_gate: Mutex<Option<(Arc<Barrier>, Arc<Barrier>)>>,
    gate: Mutex<Option<(Arc<Barrier>, Arc<Barrier>)>>,
}
impl Fake {
    fn execute(&self, args: &[&str]) -> CommandOutput {
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|s| (*s).into()).collect());
        let mut success = true;
        let mut stdout = String::new();
        if args == ["serve", "status", "--json"] {
            let mut race = self.status_race.lock().unwrap();
            if let Some(remaining) = race.as_mut() {
                *remaining -= 1;
                if *remaining == 0 {
                    self.mappings
                        .lock()
                        .unwrap()
                        .insert(30000, "http://127.0.0.1:9999".into());
                    *race = None;
                }
            }
            let mut tcp = serde_json::Map::new();
            let mut web = serde_json::Map::new();
            for (port, target) in self.mappings.lock().unwrap().iter() {
                tcp.insert(port.to_string(), json!({"HTTPS":true}));
                web.insert(
                    format!("mac.example.ts.net:{port}"),
                    json!({"Handlers":{"/":{"Proxy":target}}}),
                );
            }
            stdout = json!({"TCP":tcp,"Web":web}).to_string();
        } else if args == ["status", "--json"] {
            stdout = json!({"Self":{"DNSName":"mac.example.ts.net."}}).to_string();
        } else if args.first() == Some(&"serve") {
            let port = args
                .iter()
                .find_map(|s| s.strip_prefix("--https="))
                .unwrap()
                .parse()
                .unwrap();
            if args.last() == Some(&"off") {
                if let Some((entered, release)) = self.off_gate.lock().unwrap().take() {
                    entered.wait();
                    release.wait();
                }
                success = !*self.fail_off.lock().unwrap();
                if success && !*self.keep_off.lock().unwrap() {
                    self.mappings.lock().unwrap().remove(&port);
                }
            } else {
                // This fake deliberately replaces: the adapter must check before invoking it.
                self.mappings
                    .lock()
                    .unwrap()
                    .insert(port, args.last().unwrap().to_string());
                if let Some((entered, release)) = self.gate.lock().unwrap().take() {
                    entered.wait();
                    release.wait();
                }
                assert!(
                    !*self.crash_publish.lock().unwrap(),
                    "simulated crash after publication"
                );
            }
        } else {
            panic!("unexpected command");
        }
        CommandOutput {
            success,
            stdout,
            stderr: "synthetic-private-output".into(),
        }
    }
    fn mutations(&self) -> Vec<Vec<String>> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|a| {
                a.first().map(String::as_str) == Some("serve")
                    && a.get(1).map(String::as_str) != Some("status")
            })
            .cloned()
            .collect()
    }
}
impl CommandRunner for Fake {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CommandOutput> {
        assert_eq!(program, "/test/tailscale");
        Ok(self.execute(args))
    }
}
impl Commands for Fake {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        limits: Limits,
    ) -> Result<Output, CommandError> {
        assert_eq!(program, "/test/tailscale");
        assert!(cwd.is_none());
        assert_eq!(limits.timeout, Duration::from_secs(15));
        let out = self.execute(args);
        Ok(Output {
            code: Some(if out.success { 0 } else { 1 }),
            stdout: out.stdout.into_bytes(),
            stderr: out.stderr.into_bytes(),
        })
    }
}
fn adapter(state: &Path, fake: Arc<Fake>) -> TailscaleArtifactExposure {
    let path = state.to_owned();
    TailscaleArtifactExposure::new(
        state,
        Some("/test/tailscale".into()),
        fake,
        Arc::new(move || {
            conductor_remote::state::settings::resolve(&path, &|_| None)
                .map(|(s, _)| s.expose == conductor_remote::state::settings::Expose::Tailnet)
                .map_err(|_| ArtifactError::Unavailable("settings unavailable".into()))
        }),
    )
    .unwrap()
}
fn reserve(state: &Path, https_port: u16) -> ArtifactExposureReceipt {
    let store = ArtifactStore::open(&state.join("artifacts")).unwrap();
    let mut guard = store.lock().unwrap();
    let binding = guard
        .reserve_origin(
            &"a".repeat(64),
            &format!("https://mac.example.ts.net:{https_port}"),
            &format!("conductor_preview_{}", "b".repeat(32)),
            SystemTime::now(),
        )
        .unwrap();
    ArtifactExposureReceipt {
        binding_id: binding.binding_id,
        loopback_port: 45678,
        https_port,
        origin: binding.origin,
        generation: guard.snapshot().unwrap().generation,
    }
}
fn store(state: &Path) -> ArtifactStore {
    ArtifactStore::open(&state.join("artifacts")).unwrap()
}
fn off(state: &Path, fake: &Fake, action: tailnet::TailnetAction) -> tailnet::TailnetReport {
    tailnet::run_configured(
        action,
        fake,
        Some(Path::new("/test/tailscale")),
        8790,
        state,
        &|_| None,
    )
}

#[tokio::test]
async fn publish_persists_exact_receipt_and_cleanup_preserves_unrelated_ports() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    fake.mappings
        .lock()
        .unwrap()
        .insert(443, "http://127.0.0.1:8790".into());
    fake.mappings
        .lock()
        .unwrap()
        .insert(23000, "http://127.0.0.1:3456".into());
    let adapter = adapter(dir.path(), fake.clone());
    let receipt = reserve(dir.path(), 30000);
    adapter.publish(receipt.clone()).await.unwrap();
    let snapshot = store(dir.path()).snapshot().unwrap();
    assert_eq!(snapshot.receipts[0].receipt, receipt);
    assert_eq!(snapshot.receipts[0].phase, ReceiptPhase::Active);
    adapter.unpublish(receipt).await.unwrap();
    assert!(store(dir.path()).snapshot().unwrap().receipts.is_empty());
    assert_eq!(
        fake.mutations(),
        vec![
            vec![
                "serve",
                "--bg",
                "--yes",
                "--https=30000",
                "http://127.0.0.1:45678"
            ],
            vec!["serve", "--yes", "--https=30000", "--set-path=/", "off"]
        ]
    );
    assert_eq!(fake.mappings.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn occupied_port_is_never_overwritten_and_unrecorded_receipt_cannot_unmap() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let adapter = adapter(dir.path(), fake.clone());
    let receipt = reserve(dir.path(), 30000);
    fake.mappings
        .lock()
        .unwrap()
        .insert(30000, "http://127.0.0.1:9999".into());
    assert!(adapter.publish(receipt.clone()).await.is_err());
    adapter.unpublish(receipt.clone()).await.unwrap();
    fake.mappings
        .lock()
        .unwrap()
        .insert(30000, "http://127.0.0.1:45678".into());
    adapter.unpublish(receipt).await.unwrap();
    assert!(fake.mutations().is_empty());
    assert_eq!(fake.mappings.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn failed_cleanup_retains_receipt_until_confirmed_and_restart_retries() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let exposure = adapter(dir.path(), fake.clone());
    let receipt = reserve(dir.path(), 30000);
    exposure.publish(receipt.clone()).await.unwrap();
    *fake.fail_off.lock().unwrap() = true;
    let error = exposure
        .unpublish(receipt.clone())
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("synthetic-private-output"));
    assert_eq!(store(dir.path()).snapshot().unwrap().receipts.len(), 1);
    *fake.fail_off.lock().unwrap() = false;
    *fake.keep_off.lock().unwrap() = true;
    assert!(exposure.reconcile_startup().await.is_err());
    assert_eq!(store(dir.path()).snapshot().unwrap().receipts.len(), 1);
    *fake.keep_off.lock().unwrap() = false;
    adapter(dir.path(), fake.clone())
        .reconcile_startup()
        .await
        .unwrap();
    assert!(store(dir.path()).snapshot().unwrap().receipts.is_empty());
    assert!(store(dir.path()).snapshot().unwrap().bindings[0].tombstoned);
}

#[tokio::test]
async fn crash_after_external_publication_keeps_pending_receipt_for_restart() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    *fake.crash_publish.lock().unwrap() = true;
    let exposure = adapter(dir.path(), fake.clone());
    let receipt = reserve(dir.path(), 30000);
    assert!(exposure.publish(receipt.clone()).await.is_err());
    assert_eq!(
        store(dir.path()).snapshot().unwrap().receipts[0].phase,
        ReceiptPhase::Pending
    );
    assert!(fake.mappings.lock().unwrap().contains_key(&30000));
    adapter(dir.path(), fake.clone())
        .reconcile_startup()
        .await
        .unwrap();
    assert!(!fake.mappings.lock().unwrap().contains_key(&30000));
    assert!(store(dir.path()).snapshot().unwrap().receipts.is_empty());
}

#[test]
fn supported_parents_are_exact_relay_vite_and_actual_tailnet_origins() {
    assert_eq!(
        parent_origins(8790, 5188, Some("https://mac.example.ts.net:8443"))
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
        vec![
            "http://127.0.0.1:5188",
            "http://127.0.0.1:8790",
            "http://localhost:5188",
            "http://localhost:8790",
            "https://mac.example.ts.net:8443"
        ]
    );
    assert!(parent_origins(8790, 5173, Some("https://evil.invalid/path")).is_err());
}

struct Clock(std::time::Instant);
impl ArtifactClock for Clock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
    fn monotonic(&self) -> Duration {
        self.0.elapsed()
    }
}
struct Entropy;
impl ArtifactEntropy for Entropy {
    fn fill(&self, bytes: &mut [u8]) -> Result<(), ArtifactError> {
        getrandom::fill(bytes)
            .map_err(|_| ArtifactError::Unavailable("randomness unavailable".into()))
    }
}
async fn manager(state: &Path, fake: Arc<Fake>) -> ArtifactPreviews {
    let runtime = ArtifactPreviews::new(ArtifactPreviewConfig {
        home: None,
        parent_origins: None,
        source_roots: vec![state.to_owned()],
        broad_roots: vec![state.to_owned()],
        state_dir: state.join("artifacts"),
        allowed_parent_origins: parent_origins(8790, 5173, Some("https://mac.example.ts.net"))
            .unwrap(),
        clock: Arc::new(Clock(std::time::Instant::now())),
        entropy: Arc::new(Entropy),
        exposure: Some(Arc::new(adapter(state, fake))),
    })
    .unwrap();
    runtime.reconcile_startup().await.unwrap();
    runtime
}
fn html(parent: &str) -> CreateArtifactPreviewRequest {
    CreateArtifactPreviewRequest::Html {
        html: "<!doctype html><p>preview</p>".into(),
        base_directory: None,
        parent_origin: parent.into(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_and_disable_or_off_serialize_publication_then_revoke_without_resurrection() {
    for action in [tailnet::TailnetAction::Disable, tailnet::TailnetAction::Off] {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let runtime = manager(dir.path(), fake.clone()).await;
        let metadata = store(dir.path());
        let local = runtime.create(html("http://localhost:5173")).await.unwrap();
        assert!(local.origin.starts_with("http://localhost:"));
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        *fake.gate.lock().unwrap() = Some((entered.clone(), release.clone()));
        let creating = runtime.clone();
        let create =
            tokio::spawn(async move { creating.create(html("https://mac.example.ts.net")).await });
        tokio::task::spawn_blocking(move || entered.wait())
            .await
            .unwrap();
        assert_eq!(
            metadata.snapshot().unwrap().receipts[0].phase,
            ReceiptPhase::Pending
        );
        let off_started = Arc::new(Barrier::new(2));
        let started = off_started.clone();
        let path = dir.path().to_owned();
        let disabling = fake.clone();
        let disable = tokio::task::spawn_blocking(move || {
            started.wait();
            off(&path, &disabling, action)
        });
        tokio::task::spawn_blocking(move || off_started.wait())
            .await
            .unwrap();
        // The publication is paused with the lifecycle lock held. Off cannot return until it
        // can revoke that generation and clean the just-published exact target.
        assert!(!disable.is_finished());
        tokio::task::spawn_blocking(move || release.wait())
            .await
            .unwrap();
        let remote = create.await.unwrap();
        let report = disable.await.unwrap();
        assert!(report.error.is_none(), "{report:?}");
        assert!(fake.mappings.lock().unwrap().is_empty());
        assert!(
            !adapter(dir.path(), fake.clone())
                .current_permission()
                .await
                .unwrap()
                .enabled
        );
        if let Ok(remote) = remote {
            assert!(runtime.source(remote.id).await.is_err());
        }
        runtime.sweep().await.unwrap();
        assert!(runtime.source(local.id.clone()).await.is_ok());
        assert!(runtime
            .create(html("https://mac.example.ts.net"))
            .await
            .is_err());
        // A successful explicit Ensure may allow new sessions, never the revoked session IDs.
        let report = off(dir.path(), &fake, tailnet::TailnetAction::Enable);
        assert!(report.error.is_none(), "{report:?}");
        assert!(runtime.source(local.id).await.is_ok());
        runtime.close_all().await.unwrap();
        assert!(store(dir.path()).snapshot().unwrap().receipts.is_empty());
    }
}

#[tokio::test]
async fn disable_wins_before_publication_and_effective_settings_are_read_dynamically() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let exposure = adapter(dir.path(), fake.clone());
    let receipt = reserve(dir.path(), 30000);
    assert!(exposure.current_permission().await.unwrap().enabled);
    off(dir.path(), &fake, tailnet::TailnetAction::Off);
    assert!(matches!(
        exposure.publish(receipt.clone()).await,
        Err(ArtifactError::Forbidden(_))
    ));
    assert!(fake.mutations().is_empty());
    let report = off(dir.path(), &fake, tailnet::TailnetAction::Ensure);
    assert!(report.error.is_none());
    assert!(exposure.current_permission().await.unwrap().enabled);
    assert!(matches!(
        exposure.publish(receipt).await,
        Err(ArtifactError::Forbidden(_))
    ));
    conductor_remote::state::settings::set(dir.path(), "EXPOSE", "off").unwrap();
    assert!(!exposure.current_permission().await.unwrap().enabled);
}

#[tokio::test]
async fn occupied_candidate_retries_another_origin_and_tombstones_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let runtime = manager(dir.path(), fake.clone()).await;
    fake.mappings
        .lock()
        .unwrap()
        .insert(30000, "http://127.0.0.1:9000".into());
    let remote = runtime
        .create(html("https://mac.example.ts.net"))
        .await
        .unwrap();
    assert_eq!(remote.origin, "https://mac.example.ts.net:30001");
    runtime.close_all().await.unwrap();
    let snapshot = store(dir.path()).snapshot().unwrap();
    assert_eq!(snapshot.bindings.len(), 2);
    assert!(snapshot.bindings.iter().all(|b| b.tombstoned));
    let restarted = manager(dir.path(), fake.clone()).await;
    assert!(restarted.source(remote.id).await.is_err());
    let other = CreateArtifactPreviewRequest::Html {
        html: "different identity".into(),
        base_directory: None,
        parent_origin: "https://mac.example.ts.net".into(),
    };
    let other = restarted.create(other).await.unwrap();
    assert_eq!(other.origin, "https://mac.example.ts.net:30002");
    assert_eq!(
        fake.mappings.lock().unwrap().get(&30000).unwrap(),
        "http://127.0.0.1:9000"
    );
    restarted.close_all().await.unwrap();
}

#[tokio::test]
async fn cleanup_requires_persisted_exact_receipt_and_preserves_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let exposure = adapter(dir.path(), fake.clone());
    let receipt = reserve(dir.path(), 30000);
    exposure.publish(receipt.clone()).await.unwrap();
    let mut forged = receipt.clone();
    forged.loopback_port += 1;
    exposure.unpublish(forged).await.unwrap();
    assert_eq!(store(dir.path()).snapshot().unwrap().receipts.len(), 1);
    fake.mappings
        .lock()
        .unwrap()
        .insert(30000, "http://127.0.0.1:9000".into());
    exposure.unpublish(receipt).await.unwrap();
    assert_eq!(fake.mutations().len(), 1);
    assert_eq!(
        fake.mappings.lock().unwrap().get(&30000).unwrap(),
        "http://127.0.0.1:9000"
    );
}

#[tokio::test]
async fn mapping_that_appears_between_status_reads_is_preserved_and_create_retries() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let runtime = manager(dir.path(), fake.clone()).await;
    *fake.status_race.lock().unwrap() = Some(2);
    let remote = runtime
        .create(html("https://mac.example.ts.net"))
        .await
        .unwrap();
    assert_eq!(remote.origin, "https://mac.example.ts.net:30001");
    assert!(!fake
        .mutations()
        .iter()
        .any(|args| args.iter().any(|arg| arg == "--https=30000")));
    assert_eq!(
        fake.mappings.lock().unwrap().get(&30000).unwrap(),
        "http://127.0.0.1:9999"
    );
    runtime.close_all().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn off_holding_lifecycle_lock_revokes_a_competing_create_before_it_can_publish() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let runtime = manager(dir.path(), fake.clone()).await;
    let metadata = store(dir.path());
    let remote = runtime
        .create(html("https://mac.example.ts.net"))
        .await
        .unwrap();
    let local = runtime.create(html("http://127.0.0.1:8790")).await.unwrap();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    *fake.off_gate.lock().unwrap() = Some((entered.clone(), release.clone()));
    let path = dir.path().to_owned();
    let disabling = fake.clone();
    let disable =
        tokio::task::spawn_blocking(move || off(&path, &disabling, tailnet::TailnetAction::Off));
    tokio::task::spawn_blocking(move || entered.wait())
        .await
        .unwrap();
    assert!(metadata.snapshot().unwrap().remote_revoked);
    let started = Arc::new(Barrier::new(2));
    let create_started = started.clone();
    let creating = runtime.clone();
    let create = tokio::spawn(async move {
        tokio::task::spawn_blocking(move || create_started.wait())
            .await
            .unwrap();
        creating.create(html("https://mac.example.ts.net")).await
    });
    tokio::task::spawn_blocking(move || started.wait())
        .await
        .unwrap();
    tokio::task::spawn_blocking(move || release.wait())
        .await
        .unwrap();
    assert!(disable.await.unwrap().error.is_none());
    assert!(create.await.unwrap().is_err());
    assert!(runtime.source(remote.id).await.is_err());
    assert!(runtime.source(local.id).await.is_ok());
    assert_eq!(
        fake.mutations()
            .iter()
            .filter(|args| args.iter().any(|arg| arg == "--bg"))
            .count(),
        1
    );
    runtime.close_all().await.unwrap();
}

#[tokio::test]
async fn failed_off_blocks_remote_http_immediately_and_local_previews_remain_available() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let runtime = manager(dir.path(), fake.clone()).await;
    let remote = runtime
        .create(html("https://mac.example.ts.net"))
        .await
        .unwrap();
    let local = runtime.create(html("http://localhost:8790")).await.unwrap();
    let port = store(dir.path()).snapshot().unwrap().receipts[0]
        .receipt
        .loopback_port;
    let url = format!(
        "http://127.0.0.1:{port}/__conductor_preview/bootstrap?session={}",
        remote.id
    );
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    assert!(client
        .get(&url)
        .header("host", remote.origin.strip_prefix("https://").unwrap())
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    *fake.fail_off.lock().unwrap() = true;
    assert!(off(dir.path(), &fake, tailnet::TailnetAction::Off)
        .error
        .is_some());
    assert_eq!(store(dir.path()).snapshot().unwrap().receipts.len(), 1);
    if let Ok(response) = client
        .get(&url)
        .header("host", remote.origin.strip_prefix("https://").unwrap())
        .send()
        .await
    {
        assert!(!response.status().is_success());
    }
    runtime.sweep().await.unwrap_err();
    assert!(runtime.source(local.id).await.is_ok());
    *fake.fail_off.lock().unwrap() = false;
    runtime.sweep().await.unwrap();
    assert!(store(dir.path()).snapshot().unwrap().receipts.is_empty());
    runtime.close_all().await.unwrap();
}

// Mutable, synthetic status for read-only parent discovery. Never invokes Tailscale.
struct ParentCommands {
    host: Mutex<String>,
    status: Mutex<serde_json::Value>,
    fail: Mutex<bool>,
    calls: Mutex<Vec<Vec<String>>>,
}
impl Default for ParentCommands {
    fn default() -> Self {
        Self {
            host: Mutex::new("mac.example.ts.net.".into()),
            status: Mutex::new(json!({})),
            fail: Mutex::new(false),
            calls: Mutex::default(),
        }
    }
}
impl Commands for ParentCommands {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        limits: Limits,
    ) -> Result<Output, CommandError> {
        assert_eq!(program, "/test/tailscale");
        assert!(cwd.is_none());
        assert_eq!(limits.timeout, Duration::from_secs(15));
        assert_eq!(limits.max_stdout, 1024 * 1024);
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|arg| (*arg).into()).collect());
        let value = match args {
            ["status", "--json"] => json!({"Self":{"DNSName":*self.host.lock().unwrap()}}),
            ["serve", "status", "--json"] => self.status.lock().unwrap().clone(),
            _ => panic!("parent discovery must be read-only"),
        };
        Ok(Output {
            code: Some(if *self.fail.lock().unwrap() { 1 } else { 0 }),
            stdout: value.to_string().into_bytes(),
            stderr: b"synthetic-private-output".to_vec(),
        })
    }
}

#[tokio::test]
async fn live_main_origins_require_exact_host_root_target_https_and_supported_port() {
    use conductor_remote::artifacts::exposure::TailscaleArtifactParentOrigins;
    let commands = Arc::new(ParentCommands::default());
    let parents =
        TailscaleArtifactParentOrigins::new(8790, Some("/test/tailscale".into()), commands.clone());
    assert!(parents.current().await.unwrap().is_empty());
    let mut tcp = serde_json::Map::new();
    let mut web = serde_json::Map::new();
    // Every supported main port works; dev/preview/arbitrary ports remain excluded even
    // when their root target happens to be the relay.
    for port in [
        443, 8443, 8444, 8445, 8446, 8447, 8448, 8449, 8450, 8442, 8451, 23000, 30000,
    ] {
        tcp.insert(port.to_string(), json!({"HTTPS":true}));
        web.insert(
            format!("mac.example.ts.net:{port}"),
            json!({"Handlers":{"/":{"Proxy":"http://127.0.0.1:8790"}}}),
        );
    }
    *commands.status.lock().unwrap() = json!({"TCP":tcp,"Web":web});
    let origins = parents.current().await.unwrap();
    assert_eq!(origins.len(), 9);
    assert!(origins.contains("https://mac.example.ts.net"));
    for port in 8443..=8450 {
        assert!(origins.contains(&format!("https://mac.example.ts.net:{port}")));
    }
    for (host, port, target, path, https) in [
        (
            "mac.example.ts.net.evil",
            8443,
            "http://127.0.0.1:8790",
            "/",
            true,
        ),
        (
            "other.example.ts.net",
            8443,
            "http://127.0.0.1:8790",
            "/",
            true,
        ),
        (
            "mac.example.ts.net",
            8443,
            "http://127.0.0.1:8791",
            "/",
            true,
        ),
        (
            "mac.example.ts.net",
            8443,
            "http://localhost:8790",
            "/",
            true,
        ),
        (
            "mac.example.ts.net",
            8443,
            "http://127.0.0.1:8790/",
            "/",
            true,
        ),
        (
            "mac.example.ts.net",
            8443,
            "http://127.0.0.1:8790",
            "/nested",
            true,
        ),
        (
            "mac.example.ts.net",
            8443,
            "http://127.0.0.1:8790",
            "/",
            false,
        ),
    ] {
        *commands.status.lock().unwrap() = json!({"TCP":{port.to_string():{"HTTPS":https}},"Web":{format!("{host}:{port}"):{"Handlers":{path:{"Proxy":target}}}}});
        assert!(
            parents.current().await.unwrap().is_empty(),
            "{host}:{port} {path} {target}"
        );
    }
    *commands.status.lock().unwrap() = json!({"TCP":{"8443":{"HTTPS":true}},"Web":{"mac.example.ts.net:8443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8790"}}}}});
    assert_eq!(
        parents.current().await.unwrap(),
        ["https://mac.example.ts.net:8443".into()].into()
    );
    *commands.host.lock().unwrap() = "renamed.example.ts.net.".into();
    assert!(parents.current().await.unwrap().is_empty());
    *commands.fail.lock().unwrap() = true;
    assert!(!parents
        .current()
        .await
        .unwrap_err()
        .to_string()
        .contains("synthetic-private-output"));
    *commands.fail.lock().unwrap() = false;
    *commands.host.lock().unwrap() = "mac.example.ts.net/path".into();
    assert!(parents.current().await.is_err());
    *commands.host.lock().unwrap() = "mac.example.ts.net:8443".into();
    assert!(parents.current().await.is_err());
    *commands.host.lock().unwrap() = "mac.example.ts.net.".into();
    *commands.status.lock().unwrap() = json!("unreadable status shape");
    assert!(parents.current().await.is_err());
    let absent = TailscaleArtifactParentOrigins::new(8790, None, commands);
    assert!(absent.current().await.is_err());
}

#[tokio::test]
async fn main_mapping_enabled_after_manager_start_is_discovered_and_off_still_revokes() {
    use conductor_remote::artifacts::exposure::TailscaleArtifactParentOrigins;
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    conductor_remote::state::settings::set(dir.path(), "EXPOSE", "off").unwrap();
    let runtime = ArtifactPreviews::new(ArtifactPreviewConfig {
        home: None,
        parent_origins: Some(Arc::new(TailscaleArtifactParentOrigins::new(
            8790,
            Some("/test/tailscale".into()),
            fake.clone(),
        ))),
        source_roots: vec![dir.path().to_owned()],
        broad_roots: vec![dir.path().to_owned()],
        state_dir: dir.path().join("artifacts"),
        allowed_parent_origins: parent_origins(8790, 5173, None).unwrap(),
        clock: Arc::new(Clock(std::time::Instant::now())),
        entropy: Arc::new(Entropy),
        exposure: Some(Arc::new(adapter(dir.path(), fake.clone()))),
    })
    .unwrap();
    runtime.reconcile_startup().await.unwrap();
    let local = runtime.create(html("http://localhost:5173")).await.unwrap();
    assert!(fake.calls.lock().unwrap().is_empty());
    assert!(runtime
        .create(html("https://mac.example.ts.net"))
        .await
        .is_err());
    let enabled = off(dir.path(), &fake, tailnet::TailnetAction::Enable);
    assert!(enabled.error.is_none(), "{enabled:?}");
    let remote = runtime
        .create(html("https://mac.example.ts.net"))
        .await
        .unwrap();
    assert!(remote.origin.starts_with("https://mac.example.ts.net:3"));
    // The main mapping moves, without restarting the relay.
    fake.mappings.lock().unwrap().remove(&443);
    fake.mappings
        .lock()
        .unwrap()
        .insert(8443, "http://127.0.0.1:8790".into());
    assert!(matches!(
        runtime.create(html("https://mac.example.ts.net")).await,
        Err(ArtifactError::Forbidden(_))
    ));
    let moved = runtime
        .create(html("https://mac.example.ts.net:8443"))
        .await
        .unwrap();
    assert_eq!(remote.origin, moved.origin);
    assert!(off(dir.path(), &fake, tailnet::TailnetAction::Off)
        .error
        .is_none());
    assert!(runtime.source(remote.id).await.is_err());
    assert!(runtime.source(moved.id).await.is_err());
    assert!(runtime.source(local.id).await.is_ok());
    assert!(runtime
        .create(html("https://mac.example.ts.net:8443"))
        .await
        .is_err());
    runtime.close_all().await.unwrap();
    assert!(store(dir.path()).snapshot().unwrap().receipts.is_empty());
}

#[tokio::test]
async fn missing_or_unreadable_tailscale_does_not_query_or_block_local_preview() {
    use conductor_remote::artifacts::exposure::TailscaleArtifactParentOrigins;
    for binary in [None, Some("/test/tailscale".into())] {
        let dir = tempfile::tempdir().unwrap();
        let commands = Arc::new(ParentCommands::default());
        *commands.fail.lock().unwrap() = true;
        let runtime = ArtifactPreviews::new(ArtifactPreviewConfig {
            home: None,
            source_roots: vec![dir.path().to_owned()],
            broad_roots: vec![dir.path().to_owned()],
            state_dir: dir.path().join("artifacts"),
            allowed_parent_origins: parent_origins(8790, 5173, None).unwrap(),
            parent_origins: Some(Arc::new(TailscaleArtifactParentOrigins::new(
                8790,
                binary,
                commands.clone(),
            ))),
            clock: Arc::new(Clock(std::time::Instant::now())),
            entropy: Arc::new(Entropy),
            exposure: None,
        })
        .unwrap();
        runtime.reconcile_startup().await.unwrap();
        let local = runtime.create(html("http://127.0.0.1:8790")).await.unwrap();
        assert!(commands.calls.lock().unwrap().is_empty());
        assert_eq!(
            runtime.source(local.id).await.unwrap().content,
            "<!doctype html><p>preview</p>"
        );
        assert!(runtime
            .create(html("https://mac.example.ts.net"))
            .await
            .is_err());
        runtime.close_all().await.unwrap();
    }
}
