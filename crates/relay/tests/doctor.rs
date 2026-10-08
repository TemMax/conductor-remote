//! `doctor` over a fake: every branch of the report, the JSON shape, the exit codes, the `--out`
//! file and the argument parsing. The real probes are never called.

use std::cell::RefCell;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;

use clap::Parser;
use conductor_remote::contract::APP_BUNDLE_ID;
use conductor_remote::doctor::{
    build_report, execute, DoctorArgs, Outcome, ProbedIdentity, Probes, Session, EXIT_NO_TREE,
    EXIT_OUT_FAILED,
};
use conductor_remote::ui::driver::ViewReport;
use conductor_remote::ui::snapshot::{
    snapshot, NodeFields, NodeSnapshot, SnapshotLimits, SnapshotSource,
};
use serde_json::{json, Value};

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    args: DoctorArgs,
}

fn parse(arguments: &[&str]) -> DoctorArgs {
    Cli::try_parse_from(std::iter::once("doctor").chain(arguments.iter().copied()))
        .unwrap()
        .args
}

#[derive(Clone, Default)]
struct Fake {
    fields: NodeFields,
    children: Vec<Fake>,
}

impl SnapshotSource for Fake {
    fn read(&self) -> NodeFields {
        self.fields.clone()
    }

    fn children(&self) -> Vec<Fake> {
        self.children.clone()
    }
}

fn labelled(role: &str, title: &str, value: Option<&str>, children: Vec<Fake>) -> Fake {
    Fake {
        fields: NodeFields {
            role: Some(role.to_owned()),
            title: Some(title.to_owned()),
            value: value.map(str::to_owned),
            ..NodeFields::default()
        },
        children,
    }
}

fn fake_tree() -> Fake {
    labelled(
        "AXApplication",
        "Conductor",
        None,
        vec![labelled(
            "AXWindow",
            "Conductor",
            None,
            vec![labelled(
                "AXTextArea",
                "Composer",
                Some("a long draft message"),
                vec![],
            )],
        )],
    )
}

fn found_view() -> ViewReport {
    ViewReport {
        pane_header: Some("conductor-remote main".into()),
        chat_tabs: 3,
        selected_tab: Some(2),
        composer: true,
    }
}

/// Answers from fixed values and records which probes were called, with their arguments.
struct FakeProbes {
    identity: ProbedIdentity,
    trusted: bool,
    pid: Option<i32>,
    session: Option<Session>,
    windows: Result<Vec<String>, String>,
    view: Result<ViewReport, String>,
    calls: RefCell<Vec<String>>,
}

impl FakeProbes {
    fn ready() -> FakeProbes {
        FakeProbes {
            identity: ProbedIdentity {
                executable: Some(
                    "/Apps/Conductor Remote.app/Contents/MacOS/conductor-remote".into(),
                ),
                bundle_identifier: Some(APP_BUNDLE_ID.into()),
            },
            trusted: true,
            pid: Some(4242),
            session: Some(Session {
                locked: false,
                on_console: true,
            }),
            windows: Ok(vec!["Conductor".into()]),
            view: Ok(found_view()),
            calls: RefCell::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    fn log(&self, call: impl Into<String>) {
        self.calls.borrow_mut().push(call.into());
    }
}

impl Probes for FakeProbes {
    fn identity(&self) -> ProbedIdentity {
        self.log("identity");
        self.identity.clone()
    }

    fn trusted(&self, prompt: bool) -> bool {
        self.log(format!("trusted({prompt})"));
        self.trusted
    }

    fn conductor_pid(&self) -> Option<i32> {
        self.log("pid");
        self.pid
    }

    fn session(&self) -> Option<Session> {
        self.log("session");
        self.session
    }

    fn windows(&self, pid: i32) -> Result<Vec<String>, String> {
        self.log(format!("windows({pid})"));
        self.windows.clone()
    }

    fn tree(&self, pid: i32, limits: SnapshotLimits) -> NodeSnapshot {
        self.log(format!("tree({pid})"));
        snapshot(&fake_tree(), limits)
    }

    fn locate(&self) -> Result<ViewReport, String> {
        self.log("locate");
        self.view.clone()
    }
}

fn json_of(report: &impl serde::Serialize) -> Value {
    serde_json::to_value(report).unwrap()
}

fn keys(value: &Value) -> Vec<&str> {
    value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

// ---- the report ----

#[test]
fn a_ready_mac_reports_everything_in_the_documented_shape() {
    let probes = FakeProbes::ready();
    let report = build_report(&parse(&["--tree", "--values", "6"]), &probes);
    let value = json_of(&report);
    assert_eq!(
        keys(&value),
        [
            "identity",
            "trusted",
            "conductor",
            "session",
            "windows",
            "tree"
        ]
    );
    assert_eq!(
        value["identity"],
        json!({
            "executable": "/Apps/Conductor Remote.app/Contents/MacOS/conductor-remote",
            "bundle_identifier": "com.temmax.conductor-remote",
            "expected_bundle": true
        })
    );
    assert_eq!(value["trusted"], json!(true));
    assert_eq!(value["conductor"], json!({"running": true, "pid": 4242}));
    assert_eq!(
        value["session"],
        json!({"locked": false, "on_console": true})
    );
    assert_eq!(value["windows"], json!(["Conductor"]));
    assert_eq!(value["tree"]["role"], json!("AXApplication"));
    assert_eq!(value["tree"]["children"][0]["title"], json!("Conductor"));
    assert_eq!(report.exit_code(), 0);
    assert_eq!(
        probes.calls(),
        [
            "identity",
            "trusted(false)",
            "pid",
            "session",
            "windows(4242)",
            "tree(4242)"
        ]
    );
}

#[test]
fn without_tree_the_tree_is_null_and_not_read() {
    let probes = FakeProbes::ready();
    let report = build_report(&parse(&[]), &probes);
    let value = json_of(&report);
    assert_eq!(value["tree"], Value::Null);
    assert_eq!(value["windows"], json!(["Conductor"]));
    assert_eq!(report.exit_code(), 0);
    assert!(!probes.calls().iter().any(|call| call.starts_with("tree")));
    assert!(!report.summary(None).contains("tree:"));
}

#[test]
fn not_trusted_reads_nothing_of_conductor() {
    let mut probes = FakeProbes::ready();
    probes.trusted = false;
    let report = build_report(&parse(&["--tree"]), &probes);
    let value = json_of(&report);
    assert_eq!(value["trusted"], json!(false));
    assert_eq!(value["conductor"], json!({"running": true, "pid": 4242}));
    assert_eq!(value["windows"], json!([]));
    assert_eq!(value["tree"], Value::Null);
    assert_eq!(report.exit_code(), EXIT_NO_TREE);
    assert_eq!(
        probes.calls(),
        ["identity", "trusted(false)", "pid", "session"]
    );
    let summary = report.summary(None);
    assert!(summary.contains("accessibility: not trusted"));
    assert!(summary.contains("tree: not read"));
}

#[test]
fn not_trusted_without_tree_still_exits_zero() {
    let mut probes = FakeProbes::ready();
    probes.trusted = false;
    assert_eq!(build_report(&parse(&[]), &probes).exit_code(), 0);
}

#[test]
fn conductor_not_running_has_a_null_pid_and_no_windows_or_tree() {
    let mut probes = FakeProbes::ready();
    probes.pid = None;
    let report = build_report(&parse(&["--tree"]), &probes);
    let value = json_of(&report);
    assert_eq!(value["conductor"], json!({"running": false, "pid": null}));
    assert_eq!(value["windows"], json!([]));
    assert_eq!(value["tree"], Value::Null);
    assert_eq!(report.exit_code(), EXIT_NO_TREE);
    assert_eq!(
        probes.calls(),
        ["identity", "trusted(false)", "pid", "session"]
    );
    assert!(report.summary(None).contains("conductor: not running"));
}

#[test]
fn a_locked_session_is_reported_and_still_read() {
    let mut probes = FakeProbes::ready();
    probes.session = Some(Session {
        locked: true,
        on_console: true,
    });
    let report = build_report(&parse(&["--tree"]), &probes);
    assert_eq!(
        json_of(&report)["session"],
        json!({"locked": true, "on_console": true})
    );
    assert!(report.summary(None).contains("session: locked, on console"));
    assert_eq!(report.exit_code(), 0);
}

#[test]
fn an_unknown_session_is_null() {
    let mut probes = FakeProbes::ready();
    probes.session = None;
    let report = build_report(&parse(&[]), &probes);
    assert_eq!(json_of(&report)["session"], Value::Null);
    assert!(report.summary(None).contains("session: unknown"));
}

#[test]
fn a_session_off_the_console_is_reported() {
    let mut probes = FakeProbes::ready();
    probes.session = Some(Session {
        locked: false,
        on_console: false,
    });
    let report = build_report(&parse(&[]), &probes);
    assert!(report
        .summary(None)
        .contains("session: unlocked, not on console"));
}

#[test]
fn only_prompt_asks_for_the_dialog() {
    let probes = FakeProbes::ready();
    build_report(&parse(&["--prompt"]), &probes);
    assert_eq!(probes.calls()[1], "trusted(true)");
}

#[test]
fn another_bundle_identifier_is_not_the_expected_one() {
    let mut probes = FakeProbes::ready();
    probes.identity.bundle_identifier = Some("com.example.other".into());
    let report = build_report(&parse(&[]), &probes);
    let value = json_of(&report);
    assert_eq!(value["identity"]["expected_bundle"], json!(false));
    assert_eq!(
        value["identity"]["bundle_identifier"],
        json!("com.example.other")
    );
    assert!(report
        .summary(None)
        .contains("expected com.temmax.conductor-remote"));
}

#[test]
fn no_bundle_identifier_is_null_and_not_expected() {
    let mut probes = FakeProbes::ready();
    probes.identity.bundle_identifier = None;
    probes.identity.executable = None;
    let report = build_report(&parse(&[]), &probes);
    let value = json_of(&report);
    assert_eq!(
        value["identity"],
        json!({"executable": null, "bundle_identifier": null, "expected_bundle": false})
    );
    assert!(report.summary(None).contains("not run from an app bundle"));
}

#[test]
fn unreadable_windows_are_an_empty_list_and_named_in_the_summary() {
    let mut probes = FakeProbes::ready();
    probes.windows = Err("the request timed out".into());
    let report = build_report(&parse(&[]), &probes);
    assert_eq!(json_of(&report)["windows"], json!([]));
    assert!(report
        .summary(None)
        .contains("windows: could not be read (the request timed out)"));
}

#[test]
fn the_tree_hides_values_by_default_and_cuts_them_with_values() {
    let probes = FakeProbes::ready();
    let hidden = json_of(&build_report(&parse(&["--tree"]), &probes));
    let composer = &hidden["tree"]["children"][0]["children"][0];
    assert_eq!(composer["title"], json!("Composer"));
    assert_eq!(composer["valueHidden"], json!(true));
    assert!(composer.get("value").is_none());

    let cut = json_of(&build_report(&parse(&["--tree", "--values", "6"]), &probes));
    let composer = &cut["tree"]["children"][0]["children"][0];
    assert_eq!(composer["value"], json!("a long…"));
    assert!(composer.get("valueHidden").is_none());
}

#[test]
fn the_tree_follows_depth_and_node_limits() {
    let probes = FakeProbes::ready();
    let shallow = json_of(&build_report(&parse(&["--tree", "--depth", "1"]), &probes));
    assert_eq!(shallow["tree"]["children"][0]["depthLimited"], json!(true));
    assert!(shallow["tree"]["children"][0].get("children").is_none());

    let small = json_of(&build_report(
        &parse(&["--tree", "--max-nodes", "1"]),
        &probes,
    ));
    assert_eq!(small["tree"]["omittedChildren"], json!(1));
}

#[test]
fn the_summary_lists_the_windows_and_says_where_the_file_went() {
    let probes = FakeProbes::ready();
    let report = build_report(&parse(&["--tree"]), &probes);
    let summary = report.summary(Some(Path::new("/tmp/out.json")));
    assert!(summary.contains("accessibility: trusted"));
    assert!(summary.contains("conductor: running (pid 4242)"));
    assert!(summary.contains("windows: 1"));
    assert!(summary.contains("\"Conductor\""));
    assert!(summary.contains("tree: included"));
    assert!(summary.ends_with("report written to /tmp/out.json\n"));
}

// ---- --locate ----

#[test]
fn locate_parses_and_is_off_by_default() {
    assert!(!parse(&[]).locate);
    assert!(parse(&["--locate"]).locate);
    assert!(parse(&["--tree", "--locate"]).tree);
}

#[test]
fn a_found_view_is_in_the_json_and_the_summary() {
    let probes = FakeProbes::ready();
    let report = build_report(&parse(&["--locate"]), &probes);
    let value = json_of(&report);
    assert_eq!(
        value["view"],
        json!({
            "paneHeader": "conductor-remote main",
            "chatTabs": 3,
            "selectedTab": 2,
            "composer": true
        })
    );
    let summary = report.summary(None);
    assert!(
        summary.contains(
            "view: pane \"conductor-remote main\", 3 chat tabs, selected 2, composer found\n"
        ),
        "{summary}"
    );
    assert_eq!(report.exit_code(), 0);
    assert_eq!(probes.calls().last().unwrap(), "locate");
    println!(
        "{summary}{}",
        serde_json::to_string_pretty(&value["view"]).unwrap()
    );
}

#[test]
fn a_missing_composer_reads_composer_missing() {
    let mut probes = FakeProbes::ready();
    probes.view = Ok(ViewReport {
        composer: false,
        ..found_view()
    });
    let report = build_report(&parse(&["--locate"]), &probes);
    let value = json_of(&report);
    assert_eq!(value["view"]["composer"], json!(false));
    let summary = report.summary(None);
    assert!(
        summary.contains(
            "view: pane \"conductor-remote main\", 3 chat tabs, selected 2, composer missing\n"
        ),
        "{summary}"
    );
    assert_eq!(report.exit_code(), 0);
    println!(
        "{summary}{}",
        serde_json::to_string_pretty(&value["view"]).unwrap()
    );
}

#[test]
fn a_view_with_nothing_found_says_none() {
    let mut probes = FakeProbes::ready();
    probes.view = Ok(ViewReport::default());
    let report = build_report(&parse(&["--locate"]), &probes);
    assert!(report
        .summary(None)
        .contains("view: pane none, 0 chat tabs, selected no tab selected, composer missing\n"));
    assert_eq!(json_of(&report)["view"]["paneHeader"], Value::Null);
}

#[test]
fn a_locate_error_reads_could_not_be_read() {
    let mut probes = FakeProbes::ready();
    probes.view = Err("Conductor has no window".into());
    let report = build_report(&parse(&["--locate"]), &probes);
    let value = json_of(&report);
    assert_eq!(value["view"], Value::Null);
    assert!(!keys(&value).contains(&"view"));
    assert!(report
        .summary(None)
        .contains("view: could not be read (Conductor has no window)\n"));
    assert_eq!(report.exit_code(), EXIT_NO_TREE);
}

#[test]
fn not_trusted_does_not_locate_and_exits_three() {
    let mut probes = FakeProbes::ready();
    probes.trusted = false;
    let report = build_report(&parse(&["--locate"]), &probes);
    assert_eq!(json_of(&report)["view"], Value::Null);
    assert!(report
        .summary(None)
        .contains("view: not read (needs the grant and a running Conductor)\n"));
    assert_eq!(report.exit_code(), EXIT_NO_TREE);
    assert!(!probes.calls().contains(&"locate".to_owned()));

    let mut idle = FakeProbes::ready();
    idle.pid = None;
    let report = build_report(&parse(&["--locate"]), &idle);
    assert_eq!(report.exit_code(), EXIT_NO_TREE);
    assert!(!idle.calls().contains(&"locate".to_owned()));
}

#[test]
fn without_locate_the_view_is_null_and_never_read() {
    let probes = FakeProbes::ready();
    let report = build_report(&parse(&[]), &probes);
    let value = json_of(&report);
    assert_eq!(value["view"], Value::Null);
    assert!(!keys(&value).contains(&"view"));
    assert!(!report.summary(None).contains("view:"));
    assert_eq!(report.exit_code(), 0);
    assert!(!probes.calls().contains(&"locate".to_owned()));
}

#[test]
fn a_failed_out_wins_over_a_missing_view() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing-directory").join("report.json");
    let mut probes = FakeProbes::ready();
    probes.view = Err("no window".into());
    let outcome = run(
        &["--locate", "--out", path.to_str().unwrap()],
        &probes,
        dir.path(),
    );
    assert_eq!(outcome.code, EXIT_OUT_FAILED);
}

// ---- exit codes and --out ----

fn run(args: &[&str], probes: &FakeProbes, cwd: &Path) -> Outcome {
    execute(&parse(args), probes, cwd)
}

#[test]
fn execute_exits_zero_when_it_reported() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = run(&["--tree"], &FakeProbes::ready(), dir.path());
    assert_eq!(outcome.code, 0);
    assert_eq!(outcome.error, None);
    assert!(outcome.summary.contains("accessibility: trusted"));
}

#[test]
fn execute_exits_three_when_the_tree_cannot_be_read() {
    let dir = tempfile::tempdir().unwrap();
    let mut untrusted = FakeProbes::ready();
    untrusted.trusted = false;
    assert_eq!(run(&["--tree"], &untrusted, dir.path()).code, 3);
    let mut idle = FakeProbes::ready();
    idle.pid = None;
    assert_eq!(run(&["--tree"], &idle, dir.path()).code, 3);
    assert_eq!(run(&[], &idle, dir.path()).code, 0);
}

#[test]
fn out_writes_the_json_with_mode_0600() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("report.json");
    let outcome = run(
        &["--tree", "--out", path.to_str().unwrap()],
        &FakeProbes::ready(),
        Path::new("/"),
    );
    assert_eq!(outcome.code, 0);
    assert!(outcome
        .summary
        .contains(&format!("report written to {}", path.display())));
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        keys(&value),
        [
            "identity",
            "trusted",
            "conductor",
            "session",
            "windows",
            "tree"
        ]
    );
    assert_eq!(value["conductor"]["pid"], json!(4242));
    assert_eq!(value["tree"]["role"], json!("AXApplication"));
}

#[test]
fn out_is_written_with_null_tree_when_the_exit_status_is_three() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("report.json");
    let mut probes = FakeProbes::ready();
    probes.trusted = false;
    let outcome = run(
        &["--tree", "--out", path.to_str().unwrap()],
        &probes,
        dir.path(),
    );
    assert_eq!(outcome.code, EXIT_NO_TREE);
    let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(value["tree"], Value::Null);
    assert_eq!(value["windows"], json!([]));
    assert_eq!(value["trusted"], json!(false));
}

#[test]
fn a_relative_out_is_resolved_against_the_current_directory() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = run(
        &["--out", "relative.json"],
        &FakeProbes::ready(),
        dir.path(),
    );
    assert_eq!(outcome.code, 0);
    assert!(dir.path().join("relative.json").is_file());
    assert!(outcome.summary.contains(&format!(
        "report written to {}",
        dir.path().join("relative.json").display()
    )));
}

#[test]
fn an_existing_file_is_removed_first_and_the_new_one_is_private() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("report.json");
    std::fs::write(&path, "old").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let outcome = run(
        &["--out", path.to_str().unwrap()],
        &FakeProbes::ready(),
        dir.path(),
    );
    assert_eq!(outcome.code, 0);
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!std::fs::read_to_string(&path).unwrap().contains("old"));
}

#[test]
fn an_existing_link_is_replaced_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target.txt");
    std::fs::write(&target, "keep").unwrap();
    let link = dir.path().join("report.json");
    symlink(&target, &link).unwrap();
    let outcome = run(
        &["--out", link.to_str().unwrap()],
        &FakeProbes::ready(),
        dir.path(),
    );
    assert_eq!(outcome.code, 0);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
    assert!(!std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn an_unwritable_out_exits_four_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing-directory").join("report.json");
    let outcome = run(
        &["--out", path.to_str().unwrap()],
        &FakeProbes::ready(),
        dir.path(),
    );
    assert_eq!(outcome.code, EXIT_OUT_FAILED);
    assert!(outcome.error.unwrap().contains("could not write"));
    assert!(!outcome.summary.contains("report written"));
    assert!(outcome.summary.contains("accessibility: trusted"));
}

#[test]
fn a_failed_out_wins_over_a_missing_tree() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing-directory").join("report.json");
    let mut probes = FakeProbes::ready();
    probes.pid = None;
    let outcome = run(
        &["--tree", "--out", path.to_str().unwrap()],
        &probes,
        dir.path(),
    );
    assert_eq!(outcome.code, 4);
}

// ---- the arguments ----

#[test]
fn the_defaults() {
    let args = parse(&[]);
    assert!(!args.prompt);
    assert!(!args.tree);
    assert_eq!(args.depth, 30);
    assert_eq!(args.max_nodes, 5000);
    assert_eq!(args.values, None);
    assert_eq!(args.out, None);
}

#[test]
fn every_option_parses() {
    let args = parse(&[
        "--prompt",
        "--tree",
        "--depth",
        "7",
        "--max-nodes",
        "99",
        "--values",
        "40",
        "--out",
        "/tmp/conductor-remote-doctor.json",
    ]);
    assert!(args.prompt);
    assert!(args.tree);
    assert_eq!(args.depth, 7);
    assert_eq!(args.max_nodes, 99);
    assert_eq!(args.values, Some(40));
    assert_eq!(
        args.out.as_deref(),
        Some(Path::new("/tmp/conductor-remote-doctor.json"))
    );
}

#[test]
fn bad_arguments_are_usage_errors() {
    for bad in [
        &["--depth", "deep"][..],
        &["--depth", "-1"],
        &["--max-nodes"],
        &["--values", "x"],
        &["--out"],
        &["--unknown"],
    ] {
        let error = Cli::try_parse_from(std::iter::once("doctor").chain(bad.iter().copied()))
            .err()
            .unwrap_or_else(|| panic!("{bad:?} should not parse"));
        assert_eq!(error.exit_code(), 2, "{bad:?}");
    }
}
