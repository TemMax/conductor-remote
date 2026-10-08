use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use conductor_remote::notify::sender::{CurlSender, PushRequest, PushSender};
use conductor_remote::reads::extras::commands::{CommandError, Commands, Limits, Output};
use conductor_remote::testing::FakeCommands;

const CURL: &str = "/usr/bin/curl";
const SECRET_AUTH: &str = "vapid t=SECRET-JWT,k=SECRET-KEY";
const SECRET_BODY: &[u8] = b"SECRET-ENCRYPTED-BODY";
const ENDPOINT: &str = "https://push.example.test/send/abc";

/// What the files held and how they were protected while `curl` ran.
#[derive(Clone, Debug, Default)]
struct Snapshot {
    headers: String,
    body: Vec<u8>,
    modes: Vec<u32>,
}

/// Wraps `FakeCommands` and looks at the files `curl` would read, while it "runs".
struct Spy {
    inner: Arc<FakeCommands>,
    snapshots: Mutex<Vec<Snapshot>>,
}

impl Commands for Spy {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        cwd: Option<&Path>,
        limits: Limits,
    ) -> Result<Output, CommandError> {
        let after = |flag: &str| {
            let at = args.iter().position(|arg| *arg == flag).unwrap();
            args[at + 1].trim_start_matches('@').to_owned()
        };
        let (headers, body, response) = (after("-H"), after("--data-binary"), after("-o"));
        let modes = [&headers, &body, &response]
            .iter()
            .map(|path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777)
            .collect();
        self.snapshots.lock().unwrap().push(Snapshot {
            headers: std::fs::read_to_string(&headers).unwrap(),
            body: std::fs::read(&body).unwrap(),
            modes,
        });
        self.inner.run(program, args, cwd, limits)
    }
}

struct Rig {
    fake: Arc<FakeCommands>,
    spy: Arc<Spy>,
    sender: CurlSender,
    tmp_dir: PathBuf,
    _root: tempfile::TempDir,
}

fn rig() -> Rig {
    let root = tempfile::tempdir().unwrap();
    let tmp_dir = root.path().join("push-tmp");
    let fake = Arc::new(FakeCommands::new());
    let spy = Arc::new(Spy {
        inner: fake.clone(),
        snapshots: Mutex::new(Vec::new()),
    });
    let sender = CurlSender::new(spy.clone(), tmp_dir.clone());
    Rig {
        fake,
        spy,
        sender,
        tmp_dir,
        _root: root,
    }
}

fn request(endpoint: &str) -> PushRequest {
    PushRequest {
        endpoint: endpoint.to_owned(),
        authorization: SECRET_AUTH.to_owned(),
        body: SECRET_BODY.to_vec(),
        ttl_secs: 86_400,
    }
}

fn files_in(dir: &Path) -> Vec<String> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[test]
fn status_201_is_ok() {
    let rig = rig();
    rig.fake.on(CURL, &[], 0, "201");
    let result = rig.sender.send(&request(ENDPOINT));
    assert!(result.ok);
    assert_eq!(result.status, 201);
    assert_eq!(result.error, None);
    assert!(!result.gone);
}

#[test]
fn statuses_404_and_410_mean_the_subscription_is_gone() {
    for status in [404u16, 410] {
        let rig = rig();
        rig.fake.on(CURL, &[], 0, &status.to_string());
        let result = rig.sender.send(&request(ENDPOINT));
        assert!(!result.ok);
        assert!(result.gone, "{status}");
        assert_eq!(result.status, status);
        assert_eq!(result.error, Some(format!("HTTP {status}")));
    }
}

#[test]
fn status_500_is_an_error_but_not_gone() {
    let rig = rig();
    rig.fake.on(CURL, &[], 0, "500\n");
    let result = rig.sender.send(&request(ENDPOINT));
    assert!(!result.ok);
    assert!(!result.gone);
    assert_eq!(result.status, 500);
    assert_eq!(result.error.as_deref(), Some("HTTP 500"));
}

#[test]
fn no_answer_is_could_not_reach_with_the_curl_exit_code() {
    let rig = rig();
    rig.fake.on(CURL, &[], 7, "000");
    let result = rig.sender.send(&request(ENDPOINT));
    assert!(!result.ok);
    assert!(!result.gone);
    assert_eq!(result.status, 0);
    assert_eq!(
        result.error.as_deref(),
        Some("could not reach the push service (curl exit 7)")
    );
}

#[test]
fn unparsable_status_counts_as_no_answer() {
    let rig = rig();
    rig.fake.on(CURL, &[], 28, "oops");
    let result = rig.sender.send(&request(ENDPOINT));
    assert_eq!(result.status, 0);
    assert_eq!(
        result.error.as_deref(),
        Some("could not reach the push service (curl exit 28)")
    );
}

#[test]
fn a_spawn_failure_is_reported_with_status_zero() {
    let rig = rig();
    // No rule: the fake answers `CommandError::Spawn`.
    let result = rig.sender.send(&request(ENDPOINT));
    assert!(!result.ok);
    assert!(!result.gone);
    assert_eq!(result.status, 0);
    assert_eq!(
        result.error,
        Some(
            CommandError::Spawn {
                program: CURL.to_owned()
            }
            .to_string()
        )
    );
}

#[test]
fn a_timeout_is_reported_with_status_zero_and_cleans_up() {
    let rig = rig();
    rig.fake.fail(CURL, &[]);
    let result = rig.sender.send(&request(ENDPOINT));
    assert_eq!(result.status, 0);
    assert_eq!(result.error.as_deref(), Some("/usr/bin/curl timed out"));
    assert!(files_in(&rig.tmp_dir).is_empty());
}

#[test]
fn the_argument_list_names_files_in_tmp_dir_and_holds_no_secret() {
    let rig = rig();
    rig.fake.on(CURL, &[], 0, "201");
    rig.sender.send(&request(ENDPOINT));

    let calls = rig.fake.calls();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.program, CURL);
    assert_eq!(call.cwd, None);
    assert_eq!(call.limits.timeout.as_secs(), 15);
    assert_eq!(call.limits.max_stdout, 64);

    let a = &call.args;
    assert_eq!(a.len(), 17);
    assert_eq!(
        a[..9],
        [
            "-sS",
            "--proto",
            "=https",
            "--max-time",
            "10",
            "-X",
            "POST",
            "-H",
            &a[8]
        ]
    );
    let path_of = |arg: &str| PathBuf::from(arg.strip_prefix('@').unwrap_or(arg));
    let (headers, body, response) = (path_of(&a[8]), path_of(&a[10]), path_of(&a[12]));
    assert!(a[8].starts_with('@') && a[10].starts_with('@') && !a[12].starts_with('@'));
    assert_eq!(a[9], "--data-binary");
    assert_eq!(a[11], "-o");
    assert_eq!(a[13..], ["-w", "%{http_code}", "--", ENDPOINT]);

    let name = |path: &Path, extension: &str| {
        assert_eq!(path.parent().unwrap(), rig.tmp_dir);
        let file = path.file_name().unwrap().to_str().unwrap().to_owned();
        let stem = file
            .strip_prefix("push-")
            .and_then(|rest| rest.strip_suffix(&format!(".{extension}")))
            .unwrap_or_else(|| panic!("unexpected file name {file}"))
            .to_owned();
        assert_eq!(stem.len(), 16);
        assert!(stem
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        stem
    };
    let ids = [
        name(&headers, "headers"),
        name(&body, "body"),
        name(&response, "response"),
    ];
    assert!(ids.iter().all(|id| *id == ids[0]));

    for arg in a {
        assert!(!arg.contains("SECRET"), "secret in argument {arg}");
        assert!(!arg.contains("vapid"));
    }
}

#[test]
fn the_files_hold_the_request_and_are_private_while_curl_runs() {
    let rig = rig();
    rig.fake.on(CURL, &[], 0, "201");
    rig.sender.send(&request(ENDPOINT));

    let snapshots = rig.spy.snapshots.lock().unwrap().clone();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(
        snapshots[0].headers,
        format!(
            "authorization: {SECRET_AUTH}\ncontent-encoding: aes128gcm\ncontent-type: application/octet-stream\nttl: 86400\nurgency: normal\n"
        )
    );
    assert_eq!(snapshots[0].body, SECRET_BODY);
    assert_eq!(snapshots[0].modes, [0o600, 0o600, 0o600]);
}

#[test]
fn the_directory_is_created_private_on_first_use() {
    let rig = rig();
    assert!(!rig.tmp_dir.exists());
    rig.fake.on(CURL, &[], 0, "201");
    rig.sender.send(&request(ENDPOINT));
    let mode = std::fs::metadata(&rig.tmp_dir)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700);
}

#[test]
fn the_files_are_removed_after_success_and_after_failure() {
    let rig = rig();
    rig.fake.on(CURL, &[], 0, "201");
    rig.sender.send(&request(ENDPOINT));
    assert!(files_in(&rig.tmp_dir).is_empty(), "after success");

    rig.fake.on(CURL, &[], 0, "500");
    rig.sender.send(&request(ENDPOINT));
    assert!(files_in(&rig.tmp_dir).is_empty(), "after an HTTP error");

    rig.fake.on(CURL, &[], 7, "000");
    rig.sender.send(&request(ENDPOINT));
    assert!(files_in(&rig.tmp_dir).is_empty(), "after no answer");

    let plain = tempfile::tempdir().unwrap();
    let sender = CurlSender::new(Arc::new(FakeCommands::new()), plain.path().to_owned());
    sender.send(&request(ENDPOINT));
    assert!(files_in(plain.path()).is_empty(), "after a spawn failure");
}

#[test]
fn new_removes_leftover_push_files_only() {
    let root = tempfile::tempdir().unwrap();
    for name in [
        "push-0123456789abcdef.headers",
        "push-0123456789abcdef.body",
        "push-0123456789abcdef.response",
    ] {
        std::fs::write(root.path().join(name), b"x").unwrap();
    }
    std::fs::write(root.path().join("keep.txt"), b"x").unwrap();
    let _sender = CurlSender::new(Arc::new(FakeCommands::new()), root.path().to_owned());
    assert_eq!(files_in(root.path()), ["keep.txt"]);
}

#[test]
fn new_does_not_need_the_directory_to_exist() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("nope");
    let _sender = CurlSender::new(Arc::new(FakeCommands::new()), missing.clone());
    assert!(!missing.exists());
}

#[test]
fn an_http_endpoint_runs_nothing() {
    let rig = rig();
    rig.fake.on(CURL, &[], 0, "201");
    let result = rig.sender.send(&request("http://push.example.test/send"));
    assert!(!result.ok);
    assert_eq!(result.status, 0);
    assert!(!result.gone);
    assert_eq!(
        result.error.as_deref(),
        Some("the push endpoint is not an https URL")
    );
    assert!(rig.fake.calls().is_empty());
    assert!(files_in(&rig.tmp_dir).is_empty());
}

#[test]
fn https_is_accepted_in_any_letter_case() {
    let rig = rig();
    rig.fake.on(CURL, &[], 0, "201");
    let result = rig.sender.send(&request("HTTPS://push.example.test/send"));
    assert!(result.ok);
    assert_eq!(rig.fake.calls().len(), 1);
}

#[test]
fn an_empty_or_short_endpoint_runs_nothing() {
    let rig = rig();
    rig.fake.on(CURL, &[], 0, "201");
    for endpoint in ["", "https:/", "ftp://x"] {
        let result = rig.sender.send(&request(endpoint));
        assert!(!result.ok, "{endpoint}");
    }
    assert!(rig.fake.calls().is_empty());
}
