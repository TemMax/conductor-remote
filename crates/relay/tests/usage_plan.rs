//! Plan usage: the two parsers behind a fake probe, the snapshot cache, binary discovery and the
//! two stdin protocols against small shell scripts.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use conductor_remote::usage::plan::{
    PlanProbe, PlanUsageProviderId, PlanUsageService, PlanUsageSnapshot, PlanUsageStatus,
    ProbeError, ProviderPlanUsage, SystemProbe,
};
use serde_json::{json, Value};
use tracing_subscriber::fmt::MakeWriter;

// ---------------------------------------------------------------- a fake probe

type Answer = Result<Value, ProbeError>;

/// Answers from fixed values, counts its runs, and can hold a run until the test lets it go.
struct FakeProbe {
    claude: Mutex<Answer>,
    codex: Mutex<Answer>,
    claude_runs: AtomicUsize,
    codex_runs: AtomicUsize,
    /// Probes that are inside a run right now, and the most there ever were together.
    inside: AtomicUsize,
    most_inside: AtomicUsize,
    /// When set, a run announces itself on `entered` and waits for `release`.
    gate: Mutex<Option<Gate>>,
    /// When set, a run waits (up to 2 s) until both probes are inside.
    rendezvous: bool,
}

struct Gate {
    entered: mpsc::Sender<()>,
    release: Arc<Mutex<mpsc::Receiver<()>>>,
}

impl FakeProbe {
    fn new(claude: Answer, codex: Answer) -> Arc<Self> {
        Arc::new(Self {
            claude: Mutex::new(claude),
            codex: Mutex::new(codex),
            claude_runs: AtomicUsize::new(0),
            codex_runs: AtomicUsize::new(0),
            inside: AtomicUsize::new(0),
            most_inside: AtomicUsize::new(0),
            gate: Mutex::new(None),
            rendezvous: false,
        })
    }

    fn meeting(claude: Answer, codex: Answer) -> Arc<Self> {
        let mut probe = Arc::into_inner(Self::new(claude, codex)).unwrap();
        probe.rendezvous = true;
        Arc::new(probe)
    }

    fn runs(&self) -> (usize, usize) {
        (
            self.claude_runs.load(Ordering::SeqCst),
            self.codex_runs.load(Ordering::SeqCst),
        )
    }

    fn run(&self, runs: &AtomicUsize, answer: &Mutex<Answer>) -> Answer {
        runs.fetch_add(1, Ordering::SeqCst);
        let now_inside = self.inside.fetch_add(1, Ordering::SeqCst) + 1;
        self.most_inside.fetch_max(now_inside, Ordering::SeqCst);
        if self.rendezvous {
            let deadline = Instant::now() + Duration::from_secs(2);
            while self.inside.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let gate = self
            .gate
            .lock()
            .unwrap()
            .as_ref()
            .map(|gate| (gate.entered.clone(), Arc::clone(&gate.release)));
        if let Some((entered, release)) = gate {
            let _ = entered.send(());
            let _ = release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10));
        }
        self.inside.fetch_sub(1, Ordering::SeqCst);
        answer.lock().unwrap().clone()
    }
}

impl PlanProbe for FakeProbe {
    fn claude(&self) -> Answer {
        self.run(&self.claude_runs, &self.claude)
    }

    fn codex(&self) -> Answer {
        self.run(&self.codex_runs, &self.codex)
    }
}

fn not_installed() -> Answer {
    Err(ProbeError::NotInstalled)
}

fn service(claude: Answer, codex: Answer) -> PlanUsageService {
    PlanUsageService::new(FakeProbe::new(claude, codex))
}

fn provider(snapshot: &PlanUsageSnapshot, id: PlanUsageProviderId) -> &ProviderPlanUsage {
    snapshot
        .providers
        .iter()
        .find(|entry| entry.provider == id)
        .expect("provider present")
}

fn claude_usage(payload: Value) -> ProviderPlanUsage {
    let snapshot = service(Ok(payload), not_installed()).read(false);
    provider(&snapshot, PlanUsageProviderId::Claude).clone()
}

fn codex_usage(payload: Value) -> ProviderPlanUsage {
    let snapshot = service(not_installed(), Ok(payload)).read(false);
    provider(&snapshot, PlanUsageProviderId::Codex).clone()
}

// ---------------------------------------------------------------- the five reference cases

#[test]
fn normalizes_every_codex_rate_limit_bucket() {
    let usage = codex_usage(json!({
        "rateLimits": { "limitId": "legacy-that-must-not-be-duplicated" },
        "rateLimitsByLimitId": {
            "codex": {
                "limitId": "codex",
                "limitName": null,
                "planType": "pro",
                "primary": { "usedPercent": 6, "windowDurationMins": 10080, "resetsAt": 1_788_970_276 },
                "secondary": null
            },
            "codex_spark": {
                "limitId": "codex_spark",
                "limitName": "GPT-5.3-Codex-Spark",
                "planType": "pro",
                "primary": { "usedPercent": 12, "windowDurationMins": 300, "resetsAt": 1_788_412_239 },
                "secondary": { "usedPercent": 34, "windowDurationMins": 10080, "resetsAt": null }
            }
        }
    }));

    assert_eq!(usage.status, PlanUsageStatus::Available);
    assert_eq!(usage.label, "Codex");
    assert_eq!(usage.plan.as_deref(), Some("pro"));
    assert_eq!(usage.message, None);
    let labels: Vec<&str> = usage.buckets.iter().map(|b| b.label.as_str()).collect();
    assert_eq!(labels, ["Codex", "GPT-5.3-Codex-Spark"]);
    let windows: Vec<&str> = usage
        .buckets
        .iter()
        .flat_map(|b| b.windows.iter().map(|w| w.label.as_str()))
        .collect();
    assert_eq!(windows, ["Weekly limit", "5-hour limit", "Weekly limit"]);
    let first = &usage.buckets[0].windows[0];
    assert_eq!(first.resets_at, Some(1_788_970_276_000));
    assert_eq!(first.id, "codex:primary");
    assert_eq!(first.window_duration_mins, Some(Some(10080)));
    assert_eq!(usage.buckets[1].windows[1].resets_at, None);
}

#[test]
fn reads_claude_structured_limits_and_ignores_spend_shaped_records() {
    let usage = claude_usage(json!({
        "subscription_type": "max",
        "rate_limits_available": true,
        "rate_limits": {
            "limits": [
                { "kind": "session", "percent": 17, "resets_at": "2026-09-03T01:19:59Z", "is_active": false },
                { "kind": "weekly_all", "percent": 55, "resets_at": "2026-09-07T08:59:59Z" },
                {
                    "kind": "weekly_scoped",
                    "percent": 75,
                    "resets_at": null,
                    "is_active": true,
                    "scope": { "model": { "display_name": "Fable" } }
                },
                { "kind": "spend", "percent": 90 }
            ]
        }
    }));

    assert_eq!(usage.status, PlanUsageStatus::Available);
    assert_eq!(usage.label, "Claude Code");
    assert_eq!(usage.plan.as_deref(), Some("max"));
    assert_eq!(usage.buckets.len(), 1);
    let rows: Vec<(&str, f64, Option<bool>)> = usage.buckets[0]
        .windows
        .iter()
        .map(|w| (w.label.as_str(), w.used_percent, w.active))
        .collect();
    assert_eq!(
        rows,
        [
            ("Current session", 17.0, Some(false)),
            ("Current week", 55.0, Some(false)),
            ("Current week (Fable)", 75.0, Some(true)),
        ]
    );
    let ids: Vec<&str> = usage.buckets[0]
        .windows
        .iter()
        .map(|w| w.id.as_str())
        .collect();
    assert_eq!(
        ids,
        [
            "claude:session:0",
            "claude:weekly_all:1",
            "claude:weekly_scoped:Fable"
        ]
    );
    // 2026-09-03T01:19:59Z
    assert_eq!(
        usage.buckets[0].windows[0].resets_at,
        Some(1_788_398_399_000)
    );
    assert_eq!(usage.buckets[0].windows[2].resets_at, None);
    assert_eq!(usage.buckets[0].windows[0].window_duration_mins, None);
}

#[test]
fn keeps_the_named_claude_window_fallback_and_explains_accounts_without_plan_limits() {
    let fallback = claude_usage(json!({
        "subscription_type": "team",
        "rate_limits_available": true,
        "rate_limits": {
            "five_hour": { "utilization": 123, "resets_at": "bad timestamp" },
            "seven_day": { "utilization": 20, "resets_at": null },
            "model_scoped": [{ "display_name": "Opus", "utilization": 40, "resets_at": null }]
        }
    }));
    let rows: Vec<(&str, f64, Option<i64>)> = fallback.buckets[0]
        .windows
        .iter()
        .map(|w| (w.label.as_str(), w.used_percent, w.resets_at))
        .collect();
    assert_eq!(
        rows,
        [
            ("Current session", 100.0, None),
            ("Current week", 20.0, None),
            ("Current week (Opus)", 40.0, None),
        ]
    );
    assert!(fallback.buckets[0]
        .windows
        .iter()
        .all(|w| w.active.is_none()));

    let api_key =
        claude_usage(json!({ "subscription_type": null, "rate_limits_available": false }));
    assert_eq!(api_key.status, PlanUsageStatus::Unavailable);
    assert_eq!(api_key.plan, None);
    assert!(api_key.buckets.is_empty());
    assert!(api_key.message.as_deref().unwrap().contains("API-key"));

    let empty = claude_usage(json!({ "subscription_type": "pro", "rate_limits": {} }));
    assert_eq!(empty.status, PlanUsageStatus::Unavailable);
    assert_eq!(empty.plan.as_deref(), Some("pro"));
    assert_eq!(
        empty.message.as_deref(),
        Some("Claude Code returned no rolling plan limits for this account.")
    );
}

#[test]
fn codex_without_buckets_is_unavailable_and_labels_follow_the_duration() {
    let empty = codex_usage(json!({ "rateLimitsByLimitId": {}, "rateLimits": null }));
    assert_eq!(empty.status, PlanUsageStatus::Unavailable);
    assert_eq!(
        empty.message.as_deref(),
        Some("Codex returned no rolling plan limits for this account.")
    );

    let legacy = codex_usage(json!({
        "rateLimits": {
            "planType": "plus",
            "primary": { "usedPercent": 140, "windowDurationMins": 1440, "resetsAt": 1_788_970_276_000i64 },
            "secondary": { "usedPercent": 5, "windowDurationMins": 4320, "resetsAt": null }
        }
    }));
    assert_eq!(legacy.plan.as_deref(), Some("plus"));
    assert_eq!(legacy.buckets.len(), 1);
    assert_eq!(legacy.buckets[0].id, "codex");
    assert_eq!(legacy.buckets[0].label, "Codex");
    let windows = &legacy.buckets[0].windows;
    assert_eq!(windows[0].label, "Daily limit");
    assert_eq!(windows[0].used_percent, 100.0);
    assert_eq!(windows[0].resets_at, Some(1_788_970_276_000));
    assert_eq!(windows[1].label, "3-day limit");

    let odd = codex_usage(json!({
        "rateLimits": {
            "primary": { "usedPercent": 1, "windowDurationMins": 120, "resetsAt": null },
            "secondary": { "usedPercent": 2, "resetsAt": null }
        }
    }));
    assert_eq!(odd.buckets[0].windows[0].label, "2-hour limit");
    assert_eq!(odd.buckets[0].windows[1].label, "Secondary limit");
    assert_eq!(odd.buckets[0].windows[1].window_duration_mins, Some(None));
}

#[test]
fn contains_probe_failures_and_logs_only_their_kind() {
    let log = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(log.clone())
        .with_ansi(false)
        .finish();
    let snapshot = tracing::subscriber::with_default(subscriber, || {
        service(
            Err(ProbeError::Failed("secret".to_owned())),
            Err(ProbeError::NotInstalled),
        )
        .read(false)
    });

    let claude = provider(&snapshot, PlanUsageProviderId::Claude);
    assert_eq!(claude.status, PlanUsageStatus::Error);
    assert_eq!(
        claude.message.as_deref(),
        Some("Could not read plan usage from Claude Code.")
    );
    assert!(claude.buckets.is_empty() && claude.plan.is_none());
    let codex = provider(&snapshot, PlanUsageProviderId::Codex);
    assert_eq!(codex.status, PlanUsageStatus::Unavailable);
    assert_eq!(
        codex.message.as_deref(),
        Some("Codex is not installed on this Mac.")
    );

    let logged = log.text();
    assert!(logged.contains("WARN"), "{logged}");
    assert!(
        logged.contains("Claude Code plan usage failed: secret"),
        "{logged}"
    );
    assert_eq!(
        logged.lines().count(),
        1,
        "only the failure is logged: {logged}"
    );
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogBuffer {
    type Writer = LogBuffer;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

// ---------------------------------------------------------------- the snapshot

#[test]
fn the_snapshot_lists_the_four_providers_in_order_with_the_unsupported_two_fixed() {
    let before = now_ms();
    let snapshot = service(not_installed(), not_installed()).read(false);
    let after = now_ms();

    let order: Vec<PlanUsageProviderId> = snapshot.providers.iter().map(|p| p.provider).collect();
    assert_eq!(
        order,
        [
            PlanUsageProviderId::Claude,
            PlanUsageProviderId::Codex,
            PlanUsageProviderId::Cursor,
            PlanUsageProviderId::Opencode
        ]
    );
    assert!((before..=after).contains(&snapshot.fetched_at));

    let cursor = &snapshot.providers[2];
    assert_eq!(cursor.label, "Cursor Agent");
    assert_eq!(cursor.status, PlanUsageStatus::Unavailable);
    assert_eq!(cursor.plan, None);
    assert!(cursor.buckets.is_empty());
    assert_eq!(
        cursor.message.as_deref(),
        Some("Cursor Agent does not expose plan limits through its CLI.")
    );
    let opencode = &snapshot.providers[3];
    assert_eq!(opencode.label, "OpenCode");
    assert_eq!(
        opencode.message.as_deref(),
        Some("OpenCode reports local token and cost totals, not provider plan limits.")
    );
    let claude = &snapshot.providers[0];
    assert_eq!(claude.label, "Claude Code");
    assert_eq!(
        claude.message.as_deref(),
        Some("Claude Code is not installed on this Mac.")
    );

    let wire = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(wire["providers"][0]["provider"], "claude");
    assert_eq!(wire["providers"][3]["provider"], "opencode");
    assert!(wire["fetchedAt"].is_i64());
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[test]
fn the_two_probes_of_one_read_run_on_two_threads() {
    let probe = FakeProbe::meeting(not_installed(), not_installed());
    PlanUsageService::new(probe.clone()).read(false);
    assert_eq!(probe.most_inside.load(Ordering::SeqCst), 2);
}

#[test]
fn a_snapshot_is_reused_for_the_ttl_and_force_skips_it() {
    let probe = FakeProbe::new(not_installed(), not_installed());
    let service = PlanUsageService::with_ttl(probe.clone(), Duration::from_millis(400));

    let first = service.read(false);
    assert_eq!(probe.runs(), (1, 1));
    let again = service.read(false);
    assert_eq!(again, first);
    assert_eq!(probe.runs(), (1, 1));

    let forced = service.read(true);
    assert_eq!(probe.runs(), (2, 2));
    assert!(forced.fetched_at >= first.fetched_at);
    // The forced answer is the one kept now.
    assert_eq!(service.read(false), forced);
    assert_eq!(probe.runs(), (2, 2));

    std::thread::sleep(Duration::from_millis(450));
    service.read(false);
    assert_eq!(probe.runs(), (3, 3));
}

#[test]
fn concurrent_reads_share_one_probe_run_and_force_joins_the_run_in_flight() {
    let probe = FakeProbe::new(not_installed(), not_installed());
    let (entered_tx, entered) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *probe.gate.lock().unwrap() = Some(Gate {
        entered: entered_tx,
        release: Arc::new(Mutex::new(release_rx)),
    });
    let service = Arc::new(PlanUsageService::with_ttl(
        probe.clone(),
        Duration::from_millis(1),
    ));

    let spawn_reader = |force: bool| {
        let service = Arc::clone(&service);
        std::thread::spawn(move || service.read(force))
    };
    // The first reader starts the run; both of its probes are inside before the others come.
    let mut readers = vec![spawn_reader(false)];
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    // A plain read, and two forced ones, join that run instead of starting their own.
    readers.extend([false, true, true].map(spawn_reader));
    std::thread::sleep(Duration::from_millis(300));
    release_tx.send(()).unwrap();
    release_tx.send(()).unwrap();

    let snapshots: Vec<PlanUsageSnapshot> =
        readers.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(snapshots.iter().all(|s| *s == snapshots[0]));
    assert_eq!(probe.runs(), (1, 1));
}

// ---------------------------------------------------------------- the real probe

/// Scripts are written and run one at a time: a fork in a sibling test while a script is still
/// open for writing makes the exec fail with "text file busy".
static SPAWNS: Mutex<()> = Mutex::new(());

fn spawning() -> std::sync::MutexGuard<'static, ()> {
    SPAWNS.lock().unwrap_or_else(|e| e.into_inner())
}

fn install(root: &Path, provider: &str, version: &str, script: &str, mode: u32) -> PathBuf {
    let dir = root.join(provider).join(version);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(provider);
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    path
}

fn probe(root: &Path) -> SystemProbe {
    SystemProbe::with_lookup(root.to_path_buf(), false)
}

/// A Claude stand-in that reports its arguments and the whole of its stdin to `record`, then
/// answers with `reply` (a line of JSON), so it only answers once stdin is closed.
fn claude_script(record: &Path, reply: &str) -> String {
    format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{record}/args'\ncat > '{record}/stdin'\nprintf '%s\\n' '{reply}'\n",
        record = record.display()
    )
}

fn claude_reply(payload: &Value) -> String {
    json!({
        "type": "control_response",
        "response": { "subtype": "success", "request_id": "plan-usage", "response": payload }
    })
    .to_string()
}

#[test]
fn nothing_is_found_when_no_executable_is_installed() {
    let _guard = spawning();
    let dir = tempfile::tempdir().unwrap();
    let probe = probe(dir.path());
    assert_eq!(probe.claude(), Err(ProbeError::NotInstalled));
    assert_eq!(probe.codex(), Err(ProbeError::NotInstalled));

    // Not executable, a file where a version directory belongs, and the wrong name.
    install(dir.path(), "claude", "1.0.0", "#!/bin/sh\n", 0o644);
    fs::write(dir.path().join("claude").join("stray"), "x").unwrap();
    install(dir.path(), "codex", "1.0.0", "#!/bin/sh\n", 0o755);
    fs::rename(
        dir.path().join("codex/1.0.0/codex"),
        dir.path().join("codex/1.0.0/other"),
    )
    .unwrap();
    assert_eq!(probe.claude(), Err(ProbeError::NotInstalled));
    assert_eq!(probe.codex(), Err(ProbeError::NotInstalled));
}

#[test]
fn the_highest_executable_version_wins_by_numeric_comparison() {
    let _guard = spawning();
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("record");
    fs::create_dir_all(&record).unwrap();
    for version in ["1.2.0", "1.9.0", "1.10.0", "1.10", "0.99.99"] {
        let reply = claude_reply(&json!({ "subscription_type": version }));
        install(
            dir.path(),
            "claude",
            version,
            &claude_script(&record, &reply),
            0o755,
        );
    }
    // The newest version is not executable (a half-downloaded one): the next one is used.
    let broken = claude_reply(&json!({ "subscription_type": "2.0.0" }));
    install(
        dir.path(),
        "claude",
        "2.0.0",
        &claude_script(&record, &broken),
        0o644,
    );
    let probe = probe(dir.path());

    let payload = probe.claude().unwrap();
    assert_eq!(payload["subscription_type"], "1.10.0");

    fs::remove_dir_all(dir.path().join("claude/1.10.0")).unwrap();
    assert_eq!(probe.claude().unwrap()["subscription_type"], "1.10");
    fs::remove_dir_all(dir.path().join("claude/1.10")).unwrap();
    assert_eq!(probe.claude().unwrap()["subscription_type"], "1.9.0");
}

#[test]
fn claude_gets_one_control_line_on_a_closed_stdin_and_its_payload_comes_back() {
    let _guard = spawning();
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("record");
    fs::create_dir_all(&record).unwrap();
    let payload = json!({ "subscription_type": "max", "rate_limits_available": true });
    // Noise, a control answer for another request and a non-JSON line come first.
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{record}/args'\ncat > '{record}/stdin'\n\
         printf '%s\\n' 'not json'\n\
         printf '%s\\n' '{{\"type\":\"system\",\"subtype\":\"init\"}}'\n\
         printf '%s\\n' '{{\"type\":\"control_response\",\"response\":{{\"subtype\":\"success\",\"request_id\":\"other\",\"response\":{{\"no\":1}}}}}}'\n\
         printf '%s\\n' '{reply}'\nexec sleep 5\n",
        record = record.display(),
        reply = claude_reply(&payload)
    );
    install(dir.path(), "claude", "3.1.4", &script, 0o755);

    let started = Instant::now();
    let answer = probe(dir.path()).claude().unwrap();
    // The child is killed at the end: its `sleep 5` does not hold the call.
    assert!(started.elapsed() < Duration::from_secs(4));

    assert_eq!(answer, payload);
    let args = fs::read_to_string(record.join("args")).unwrap();
    assert_eq!(
        args.lines().collect::<Vec<_>>(),
        [
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--no-session-persistence",
            "--safe-mode"
        ]
    );
    let stdin = fs::read_to_string(record.join("stdin")).unwrap();
    assert!(
        stdin.ends_with('\n') && stdin.lines().count() == 1,
        "{stdin:?}"
    );
    let line: Value = serde_json::from_str(stdin.trim_end()).unwrap();
    assert_eq!(
        line,
        json!({
            "type": "control_request",
            "request_id": "plan-usage",
            "request": { "subtype": "get_usage" }
        })
    );
}

#[test]
fn claude_failures_are_failures_without_the_output_in_the_detail() {
    let _guard = spawning();
    let failure = |script: &str| {
        let dir = tempfile::tempdir().unwrap();
        install(dir.path(), "claude", "1.0.0", script, 0o755);
        probe(dir.path()).claude()
    };
    let detail = |answer: Answer| match answer {
        Err(ProbeError::Failed(detail)) => detail,
        other => panic!("expected a failure, got {other:?}"),
    };

    // The control answer reports an error.
    let rejected = detail(failure(
        "#!/bin/sh\ncat > /dev/null\nprintf '%s\\n' '{\"type\":\"control_response\",\"response\":{\"subtype\":\"error\",\"request_id\":\"plan-usage\",\"error\":\"SECRET-TOKEN\"}}'\nsleep 1\n",
    ));
    assert!(!rejected.contains("SECRET"), "{rejected}");

    // The program ends without ever answering.
    let ended = detail(failure(
        "#!/bin/sh\ncat > /dev/null\nprintf '%s\\n' 'SECRET-TOKEN'\n",
    ));
    assert!(!ended.contains("SECRET"), "{ended}");

    // The output outgrows 4 MiB with no line end.
    let large = detail(failure(
        "#!/bin/sh\ncat > /dev/null\nhead -c 5000000 /dev/zero | tr '\\0' a\n",
    ));
    assert!(large.contains("too large"), "{large}");

    // The output passes 4 MiB over many lines.
    let many = detail(failure(
        "#!/bin/sh\ncat > /dev/null\nhead -c 5000000 /dev/zero | tr '\\0' a | fold -w 100\n",
    ));
    assert!(many.contains("too large"), "{many}");
}

#[test]
fn codex_is_asked_in_order_and_its_result_comes_back() {
    let _guard = spawning();
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("record");
    fs::create_dir_all(&record).unwrap();
    let result = json!({ "rateLimits": { "planType": "pro" } });
    // `initialized` and the request are only sent after the answer to `initialize`: each `read`
    // blocks until its line arrives.
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{record}/args'\n\
         read first\n\
         printf '%s\\n' 'not json'\n\
         printf '%s\\n' '{{\"method\":\"hello\"}}'\n\
         printf '%s\\n' '{{\"id\":1,\"result\":{{\"userAgent\":\"x\"}}}}'\n\
         read second\nread third\n\
         printf '%s\\n' \"$first\" \"$second\" \"$third\" > '{record}/lines'\n\
         printf '%s\\n' '{{\"id\":2,\"result\":{result}}}'\nexec sleep 5\n",
        record = record.display(),
        result = result
    );
    install(dir.path(), "codex", "0.5.0", &script, 0o755);

    let started = Instant::now();
    let answer = probe(dir.path()).codex().unwrap();
    assert!(started.elapsed() < Duration::from_secs(4));

    assert_eq!(answer, result);
    assert_eq!(
        fs::read_to_string(record.join("args")).unwrap(),
        "app-server\n"
    );
    let lines = fs::read_to_string(record.join("lines")).unwrap();
    let lines: Vec<Value> = lines
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        lines,
        [
            json!({
                "id": 1,
                "method": "initialize",
                "params": { "clientInfo": { "name": "conductor-remote", "version": "1" } }
            }),
            json!({ "method": "initialized" }),
            json!({ "id": 2, "method": "account/rateLimits/read", "params": null }),
        ]
    );
}

#[test]
fn codex_errors_and_silence_are_failures() {
    let _guard = spawning();
    let failure = |script: &str| {
        let dir = tempfile::tempdir().unwrap();
        install(dir.path(), "codex", "1.0.0", script, 0o755);
        probe(dir.path()).codex()
    };
    let detail = |answer: Answer| match answer {
        Err(ProbeError::Failed(detail)) => detail,
        other => panic!("expected a failure, got {other:?}"),
    };

    let rejected = detail(failure(
        "#!/bin/sh\nread a\nprintf '%s\\n' '{\"id\":1,\"result\":{}}'\nread b\nread c\nprintf '%s\\n' '{\"id\":2,\"error\":{\"message\":\"SECRET-TOKEN\"}}'\nsleep 1\n",
    ));
    assert!(!rejected.contains("SECRET"), "{rejected}");

    // The program ends after `initialize`, never answering the request.
    let ended = detail(failure(
        "#!/bin/sh\nread a\nprintf '%s\\n' '{\"id\":1,\"result\":{}}'\n",
    ));
    // (The request may reach a closed pipe first: that is a failure too.)
    assert!(
        ended.contains("exited") || ended.contains("could not write"),
        "{ended}"
    );

    // Found but impossible to start (its interpreter is missing): a failure, not an absence.
    let broken = detail(failure("#!/nonexistent/interpreter\n"));
    assert!(broken.contains("could not start"), "{broken}");
}
