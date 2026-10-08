use std::cell::{Cell, RefCell};
use std::io;
use std::path::{Path, PathBuf};

use conductor_remote::contract::{Config, APP_BUNDLE_ID};
use conductor_remote::service::{
    build_plist, choose_https_port, config_set, find_tailscale, phone_url, render_config,
    run_with_tailscale, CommandOutput, CommandRunner, HttpsPort, PortProbe, ServiceCommand,
    ServicePaths,
};
use conductor_remote::state::settings;

const LABEL: &str = "com.temmax.conductor-remote";
const TAILSCALE: &str = "/opt/homebrew/bin/tailscale";

type Responder = Box<dyn Fn(&str) -> Option<CommandOutput>>;

/// Records every command and answers from a script; unscripted commands succeed silently.
struct FakeRunner {
    calls: RefCell<Vec<String>>,
    respond: Responder,
}

impl FakeRunner {
    fn new(respond: impl Fn(&str) -> Option<CommandOutput> + 'static) -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
            respond: Box::new(respond),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl CommandRunner for FakeRunner {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput> {
        let line = std::iter::once(program)
            .chain(args.iter().copied())
            .collect::<Vec<_>>()
            .join(" ");
        self.calls.borrow_mut().push(line.clone());
        if line == "id -u" {
            return Ok(ok("501\n"));
        }
        Ok((self.respond)(&line).unwrap_or_else(|| ok("")))
    }
}

fn ok(stdout: &str) -> CommandOutput {
    CommandOutput {
        success: true,
        stdout: stdout.to_owned(),
        stderr: String::new(),
    }
}

fn failed(stderr: &str) -> CommandOutput {
    CommandOutput {
        success: false,
        stdout: String::new(),
        stderr: stderr.to_owned(),
    }
}

/// Answers `is_free` from a flag and records the ports asked about; binds nothing.
struct FakePorts {
    free: bool,
    asked: RefCell<Vec<u16>>,
}

impl FakePorts {
    fn new(free: bool) -> Self {
        Self {
            free,
            asked: RefCell::new(Vec::new()),
        }
    }
}

impl PortProbe for FakePorts {
    fn is_free(&self, port: u16) -> bool {
        self.asked.borrow_mut().push(port);
        self.free
    }
}

const SERVE_THIS_RELAY: &str = r#"{"TCP":{"443":{"HTTPS":true}},
  "Web":{"mac.example.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8790"}}}}}"#;
const TS_STATUS: &str = r#"{"Self":{"DNSName":"mac.example.ts.net."}}"#;

struct Fixture {
    _dir: tempfile::TempDir,
    config: Config,
    paths: ServicePaths,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let paths = ServicePaths {
        plist: root.join("LaunchAgents").join(format!("{LABEL}.plist")),
        log_dir: root.join("Logs").join(LABEL),
        executable: root.join("bin/conductor-remote"),
    };
    let config = Config {
        port: 8790,
        state_dir: root.join("state"),
    };
    Fixture {
        _dir: dir,
        config,
        paths,
    }
}

fn run(
    fx: &Fixture,
    command: ServiceCommand,
    runner: &FakeRunner,
    tailscale: Option<&str>,
    token: Option<&str>,
) -> (anyhow::Result<()>, String) {
    run_with_ports(fx, command, runner, &FakePorts::new(true), tailscale, token)
}

fn run_with_ports(
    fx: &Fixture,
    command: ServiceCommand,
    runner: &FakeRunner,
    ports: &FakePorts,
    tailscale: Option<&str>,
    token: Option<&str>,
) -> (anyhow::Result<()>, String) {
    let mut out = Vec::new();
    let result = run_with_tailscale(
        command,
        &fx.config,
        &fx.paths,
        runner,
        ports,
        tailscale.map(Path::new),
        token,
        &mut out,
    );
    (result, String::from_utf8(out).unwrap())
}

fn plist_value(plist: &str, key: &str) -> String {
    let marker = format!("<key>{key}</key>");
    let rest = plist
        .split_once(&marker)
        .unwrap_or_else(|| panic!("missing key {key}"))
        .1;
    rest.trim_start().lines().next().unwrap().trim().to_owned()
}

fn plist_for_test() -> String {
    build_plist(
        LABEL,
        Path::new("/opt/bin/conductor-remote"),
        8790,
        Path::new("/logs/dir"),
    )
}

#[test]
fn label_is_the_bundle_id() {
    assert_eq!(APP_BUNDLE_ID, LABEL);
}

#[test]
fn plist_has_every_key() {
    let plist = plist_for_test();
    assert_eq!(
        plist_value(&plist, "Label"),
        "<string>com.temmax.conductor-remote</string>"
    );
    assert_eq!(plist_value(&plist, "RunAtLoad"), "<true/>");
    assert_eq!(plist_value(&plist, "KeepAlive"), "<true/>");
    assert_eq!(
        plist_value(&plist, "ProcessType"),
        "<string>Interactive</string>"
    );
    assert_eq!(
        plist_value(&plist, "StandardOutPath"),
        "<string>/logs/dir/relay.log</string>"
    );
    assert_eq!(
        plist_value(&plist, "StandardErrorPath"),
        "<string>/logs/dir/relay.err.log</string>"
    );
    assert!(plist.contains(
        "<key>ProgramArguments</key>\n\t<array>\n\t\t<string>/opt/bin/conductor-remote</string>\n\t\t<string>start</string>\n\t</array>"
    ));
    assert!(plist.contains(
        "<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>RELAY_PORT</key>\n\t\t<string>8790</string>\n\t\t<key>PATH</key>\n\t\t<string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>\n\t</dict>"
    ));
}

#[test]
fn plist_escapes_xml() {
    let plist = build_plist(
        LABEL,
        Path::new("/Applications/R&D <beta>/conductor-remote"),
        8790,
        Path::new("/logs/a&b"),
    );
    assert!(plist.contains("<string>/Applications/R&amp;D &lt;beta&gt;/conductor-remote</string>"));
    assert!(plist.contains("<string>/logs/a&amp;b/relay.log</string>"));
    assert!(!plist.contains("R&D"));
}

#[test]
fn no_mappings_picks_443() {
    for json in ["null", "{}", ""] {
        assert_eq!(
            choose_https_port(json, 8790).unwrap(),
            HttpsPort::Free(443),
            "input {json:?}"
        );
    }
}

#[test]
fn another_relay_on_443_moves_to_8443() {
    let json = r#"{"TCP":{"443":{"HTTPS":true}},
        "Web":{"mac.example.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8787"}}}}}"#;
    assert_eq!(
        choose_https_port(json, 8790).unwrap(),
        HttpsPort::Free(8443)
    );
}

#[test]
fn existing_mapping_to_this_relay_is_reused() {
    let json = r#"{"TCP":{"443":{"HTTPS":true},"8443":{"HTTPS":true}},
        "Web":{"mac.example.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8787"}}},
               "mac.example.ts.net:8443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8790"}}}}}"#;
    assert_eq!(
        choose_https_port(json, 8790).unwrap(),
        HttpsPort::Existing(8443)
    );
}

#[test]
fn every_candidate_taken_is_an_error() {
    let ports = [443, 8443, 8444, 8445, 8446, 8447, 8448, 8449, 8450];
    let tcp = ports
        .iter()
        .map(|port| format!(r#""{port}":{{"HTTPS":true}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let json = format!(r#"{{"TCP":{{{tcp}}}}}"#);
    let err = choose_https_port(&json, 8790).unwrap_err().to_string();
    assert!(err.contains("443"), "{err}");
    assert!(err.contains("8450"), "{err}");
}

#[test]
fn a_port_only_in_web_counts_as_taken() {
    let json =
        r#"{"Web":{"mac.example.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:1"}}}}}"#;
    assert_eq!(
        choose_https_port(json, 8790).unwrap(),
        HttpsPort::Free(8443)
    );
}

#[test]
fn phone_url_forms() {
    assert_eq!(
        phone_url("mac.example.ts.net", 443, "abc"),
        "https://mac.example.ts.net/#token=abc"
    );
    assert_eq!(
        phone_url("mac.example.ts.net", 8443, "abc"),
        "https://mac.example.ts.net:8443/#token=abc"
    );
    assert_eq!(
        phone_url("mac.example.ts.net.", 443, "abc"),
        "https://mac.example.ts.net/#token=abc"
    );
}

#[test]
fn find_tailscale_takes_the_first_existing() {
    let all = |_: &Path| true;
    assert_eq!(
        find_tailscale(all),
        Some(PathBuf::from("/opt/homebrew/bin/tailscale"))
    );
    let later = |path: &Path| path != Path::new("/opt/homebrew/bin/tailscale");
    assert_eq!(
        find_tailscale(later),
        Some(PathBuf::from("/usr/local/bin/tailscale"))
    );
    let app_only = |path: &Path| path.starts_with("/Applications");
    assert_eq!(
        find_tailscale(app_only),
        Some(PathBuf::from(
            "/Applications/Tailscale.app/Contents/MacOS/Tailscale"
        ))
    );
    assert_eq!(find_tailscale(|_: &Path| false), None);
}

#[test]
fn install_writes_the_plist_and_runs_the_commands_in_order() {
    let fx = fixture();
    let runner = FakeRunner::new(|line| match line {
        l if l == format!("{TAILSCALE} serve status --json") => Some(ok("null")),
        _ => None,
    });
    let (result, _) = run(
        &fx,
        ServiceCommand::Install,
        &runner,
        Some(TAILSCALE),
        Some("tok"),
    );
    result.unwrap();

    let plist_path = fx.paths.plist.display();
    let target = format!("gui/501/{LABEL}");
    assert_eq!(
        runner.calls()[..9],
        [
            "id -u".to_owned(),
            format!("launchctl print {target}"),
            format!("launchctl bootout {target}"),
            format!("launchctl bootstrap gui/501 {plist_path}"),
            format!("launchctl enable {target}"),
            format!("launchctl kickstart -k {target}"),
            format!("{TAILSCALE} serve status --json"),
            format!("{TAILSCALE} serve --bg --https=443 http://127.0.0.1:8790"),
            // `Status` starts here.
            format!("launchctl print {target}"),
        ]
    );
    let written = std::fs::read_to_string(&fx.paths.plist).unwrap();
    assert_eq!(
        written,
        build_plist(LABEL, &fx.paths.executable, 8790, &fx.paths.log_dir)
    );
    assert!(fx.paths.log_dir.is_dir());
}

#[test]
fn install_prints_the_status_lines() {
    let fx = fixture();
    let runner = FakeRunner::new(|line| match line {
        l if l.ends_with("serve status --json") => Some(ok(SERVE_THIS_RELAY)),
        l if l.ends_with("tailscale status --json") => Some(ok(TS_STATUS)),
        _ => None,
    });
    let (result, out) = run(
        &fx,
        ServiceCommand::Install,
        &runner,
        Some(TAILSCALE),
        Some("tok"),
    );
    result.unwrap();
    assert!(
        out.contains("https://mac.example.ts.net/#token=tok"),
        "{out}"
    );
}

#[test]
fn install_with_an_existing_mapping_does_not_mount_again() {
    let fx = fixture();
    let runner = FakeRunner::new(|line| match line {
        l if l.ends_with("serve status --json") => Some(ok(SERVE_THIS_RELAY)),
        l if l.ends_with("tailscale status --json") => Some(ok(TS_STATUS)),
        _ => None,
    });
    let (result, _) = run(
        &fx,
        ServiceCommand::Install,
        &runner,
        Some(TAILSCALE),
        Some("tok"),
    );
    result.unwrap();
    assert!(
        runner.calls().iter().all(|call| !call.contains("--bg")),
        "{:?}",
        runner.calls()
    );
}

#[test]
fn install_without_tailscale_still_succeeds() {
    let fx = fixture();
    let runner = FakeRunner::new(|_| None);
    let (result, out) = run(&fx, ServiceCommand::Install, &runner, None, None);
    result.unwrap();
    assert!(fx.paths.plist.is_file());
    assert!(
        runner
            .calls()
            .iter()
            .all(|call| !call.contains("tailscale")),
        "{:?}",
        runner.calls()
    );
    assert!(
        out.contains("tailscale serve --bg --https=443 http://127.0.0.1:8790"),
        "{out}"
    );
}

#[test]
fn uninstall_removes_the_plist_and_unmounts() {
    let fx = fixture();
    let install_runner = FakeRunner::new(|_| None);
    run(&fx, ServiceCommand::Install, &install_runner, None, None)
        .0
        .unwrap();
    assert!(fx.paths.plist.is_file());

    let runner = FakeRunner::new(|line| match line {
        l if l.starts_with("launchctl bootout") => Some(failed("not loaded")),
        l if l.ends_with("serve status --json") => Some(ok(SERVE_THIS_RELAY)),
        _ => None,
    });
    let (result, _) = run(
        &fx,
        ServiceCommand::Uninstall,
        &runner,
        Some(TAILSCALE),
        None,
    );
    result.unwrap();
    assert!(!fx.paths.plist.exists());
    assert_eq!(
        runner.calls(),
        [
            "id -u".to_owned(),
            format!("launchctl bootout gui/501/{LABEL}"),
            format!("{TAILSCALE} serve status --json"),
            format!("{TAILSCALE} serve --https=443 off"),
        ]
    );
}

#[test]
fn uninstall_leaves_other_mappings_alone() {
    let fx = fixture();
    let runner = FakeRunner::new(|line| match line {
        l if l.ends_with("serve status --json") => Some(ok(
            r#"{"TCP":{"443":{"HTTPS":true}},"Web":{"h.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8787"}}}}}"#,
        )),
        _ => None,
    });
    let (result, _) = run(
        &fx,
        ServiceCommand::Uninstall,
        &runner,
        Some(TAILSCALE),
        None,
    );
    result.unwrap();
    assert!(runner.calls().iter().all(|call| !call.contains(" off")));
}

#[test]
fn restart_runs_one_command() {
    let fx = fixture();
    let runner = FakeRunner::new(|_| None);
    let (result, _) = run(&fx, ServiceCommand::Restart, &runner, Some(TAILSCALE), None);
    result.unwrap();
    assert_eq!(
        runner.calls(),
        [
            "id -u".to_owned(),
            format!("launchctl kickstart -k gui/501/{LABEL}")
        ]
    );
}

#[test]
fn status_prints_the_url_with_the_token() {
    let fx = fixture();
    let runner = FakeRunner::new(|line| match line {
        l if l.ends_with("serve status --json") => Some(ok(SERVE_THIS_RELAY)),
        l if l.ends_with("tailscale status --json") => Some(ok(TS_STATUS)),
        _ => None,
    });
    let (result, out) = run(
        &fx,
        ServiceCommand::Status,
        &runner,
        Some(TAILSCALE),
        Some("s3cret"),
    );
    result.unwrap();
    assert!(out.contains(&fx.paths.plist.display().to_string()), "{out}");
    assert!(out.contains("missing"), "{out}");
    assert!(out.contains("service: loaded"), "{out}");
    assert!(
        out.contains("https://mac.example.ts.net/#token=s3cret"),
        "{out}"
    );
}

#[test]
fn status_without_a_token_says_so() {
    let fx = fixture();
    let runner = FakeRunner::new(|line| match line {
        l if l.starts_with("launchctl print") => Some(failed("not found")),
        l if l.ends_with("serve status --json") => Some(ok(SERVE_THIS_RELAY)),
        l if l.ends_with("tailscale status --json") => Some(ok(TS_STATUS)),
        _ => None,
    });
    let (result, out) = run(&fx, ServiceCommand::Status, &runner, Some(TAILSCALE), None);
    result.unwrap();
    assert!(out.contains("service: not loaded"), "{out}");
    assert!(
        out.contains("phone URL: https://mac.example.ts.net/\n"),
        "{out}"
    );
    assert!(!out.contains("#token="), "{out}");
    assert!(out.contains("has not created a token yet"), "{out}");
}

#[test]
fn a_failing_bootstrap_is_an_error_with_stderr() {
    let fx = fixture();
    let runner = FakeRunner::new(|line| match line {
        l if l.starts_with("launchctl bootstrap") => {
            Some(failed("Bootstrap failed: 5: Input/output error"))
        }
        _ => None,
    });
    let (result, _) = run(&fx, ServiceCommand::Install, &runner, Some(TAILSCALE), None);
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("Bootstrap failed: 5: Input/output error"),
        "{err}"
    );
    assert!(
        runner.calls().iter().all(|call| !call.contains("enable")),
        "{:?}",
        runner.calls()
    );
}

/// Answers every `launchctl bootstrap` with a failure until `failures` of them have failed.
fn bootstrap_failing(failures: u32) -> FakeRunner {
    let failed_so_far = Cell::new(0);
    FakeRunner::new(move |line| {
        if line.starts_with("launchctl bootstrap") && failed_so_far.get() < failures {
            failed_so_far.set(failed_so_far.get() + 1);
            return Some(failed("Bootstrap failed: 5: Input/output error"));
        }
        None
    })
}

#[test]
fn a_bootstrap_that_fails_once_is_retried() {
    let fx = fixture();
    let runner = bootstrap_failing(1);
    let (result, _) = run(&fx, ServiceCommand::Install, &runner, Some(TAILSCALE), None);
    result.unwrap();
    let calls = runner.calls();
    let bootstraps = calls
        .iter()
        .filter(|call| call.starts_with("launchctl bootstrap"))
        .count();
    assert_eq!(bootstraps, 2, "{calls:?}");
    let last_bootstrap = calls
        .iter()
        .rposition(|call| call.starts_with("launchctl bootstrap"))
        .unwrap();
    let enable = calls
        .iter()
        .position(|call| call.starts_with("launchctl enable"))
        .unwrap();
    assert!(last_bootstrap < enable, "{calls:?}");
}

#[test]
fn a_bootstrap_that_fails_five_times_is_an_error_after_five_attempts() {
    let fx = fixture();
    let runner = bootstrap_failing(5);
    let (result, _) = run(&fx, ServiceCommand::Install, &runner, Some(TAILSCALE), None);
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("Bootstrap failed: 5: Input/output error"),
        "{err}"
    );
    let calls = runner.calls();
    let bootstraps = calls
        .iter()
        .filter(|call| call.starts_with("launchctl bootstrap"))
        .count();
    assert_eq!(bootstraps, 5, "{calls:?}");
    assert!(
        calls.iter().all(|call| !call.contains("enable")),
        "{calls:?}"
    );
}

const PORT_BUSY_ERROR: &str = "port 8790 is already in use by another program; set RELAY_PORT to a free port and run `service install` again";

#[test]
fn install_refuses_a_busy_port_when_the_service_is_not_loaded() {
    let fx = fixture();
    let runner = FakeRunner::new(|line| match line {
        l if l.starts_with("launchctl print") => Some(failed("not found")),
        _ => None,
    });
    let ports = FakePorts::new(false);
    let (result, out) = run_with_ports(
        &fx,
        ServiceCommand::Install,
        &runner,
        &ports,
        Some(TAILSCALE),
        Some("tok"),
    );
    assert_eq!(result.unwrap_err().to_string(), PORT_BUSY_ERROR);
    assert_eq!(*ports.asked.borrow(), [8790]);
    assert!(out.is_empty(), "{out}");
    assert!(!fx.paths.plist.exists());
    assert!(!fx.paths.log_dir.exists());
    assert_eq!(
        runner.calls(),
        [
            "id -u".to_owned(),
            format!("launchctl print gui/501/{LABEL}")
        ]
    );
    assert!(
        runner
            .calls()
            .iter()
            .all(|call| !call.contains("bootstrap")),
        "{:?}",
        runner.calls()
    );
}

#[test]
fn install_with_the_service_loaded_ignores_a_busy_port() {
    let fx = fixture();
    let runner = FakeRunner::new(|_| None);
    let ports = FakePorts::new(false);
    let (result, _) = run_with_ports(&fx, ServiceCommand::Install, &runner, &ports, None, None);
    result.unwrap();
    assert!(fx.paths.plist.is_file());
    assert!(
        runner.calls().contains(&format!(
            "launchctl bootstrap gui/501 {}",
            fx.paths.plist.display()
        )),
        "{:?}",
        runner.calls()
    );
}

#[test]
fn install_with_a_free_port_proceeds_when_the_service_is_not_loaded() {
    let fx = fixture();
    let runner = FakeRunner::new(|line| match line {
        l if l.starts_with("launchctl print") => Some(failed("not found")),
        _ => None,
    });
    let ports = FakePorts::new(true);
    let (result, _) = run_with_ports(&fx, ServiceCommand::Install, &runner, &ports, None, None);
    result.unwrap();
    assert_eq!(*ports.asked.borrow(), [8790]);
    assert!(fx.paths.plist.is_file());
    assert!(
        runner.calls().contains(&format!(
            "launchctl bootstrap gui/501 {}",
            fx.paths.plist.display()
        )),
        "{:?}",
        runner.calls()
    );
}

#[test]
fn status_prints_the_pid_and_the_log_paths_without_tailscale() {
    let fx = fixture();
    let runner = FakeRunner::new(|line| match line {
        l if l.starts_with("launchctl print") => {
            Some(ok("gui/501/x = {\n\tstate = running\n\tpid = 4242\n}\n"))
        }
        _ => None,
    });
    let (result, out) = run(&fx, ServiceCommand::Status, &runner, None, Some("tok"));
    result.unwrap();
    let logs = fx.paths.log_dir.display();
    assert!(out.contains("pid: 4242\n"), "{out}");
    assert!(
        out.contains(&format!("logs: {logs}/relay.log and {logs}/relay.err.log")),
        "{out}"
    );
    assert!(out.contains("tailnet: tailscale was not found"), "{out}");
}

#[test]
fn install_with_expose_off_skips_the_tailnet_mapping() {
    let fx = fixture();
    settings::set(&fx.config.state_dir, "EXPOSE", "off").unwrap();
    let runner = FakeRunner::new(|line| match line {
        l if l.ends_with("serve status --json") => Some(ok("null")),
        _ => None,
    });
    let (result, out) = run(
        &fx,
        ServiceCommand::Install,
        &runner,
        Some(TAILSCALE),
        Some("tok"),
    );
    result.unwrap();
    assert!(out.contains("EXPOSE is off"), "{out}");
    assert!(
        runner.calls().iter().all(|call| !call.contains("--bg")),
        "{:?}",
        runner.calls()
    );
}

/// The settings as `config` shows them: two from `settings.json`, the rest defaults.
fn config_rows(fx: &Fixture) -> Vec<settings::Row> {
    settings::set(&fx.config.state_dir, "PREVENT_SCREEN_LOCK", "on").unwrap();
    settings::set(
        &fx.config.state_dir,
        "PUSH_SUBJECT",
        "mailto:me@example.com",
    )
    .unwrap();
    settings::resolve(&fx.config.state_dir, &|_| None)
        .unwrap()
        .1
}

#[test]
fn render_config_shows_every_setting_the_service_and_the_token_prefix() {
    let fx = fixture();
    // `render_config` looks `tailscale` up on this Mac (the app's binary is `Tailscale`), else
    // takes it from the `PATH`: match the arguments only.
    let runner = FakeRunner::new(|line| match line {
        l if l.ends_with(" serve status --json") => Some(ok(SERVE_THIS_RELAY)),
        l if l.ends_with(" status --json") => Some(ok(TS_STATUS)),
        _ => None,
    });
    let rows = config_rows(&fx);
    let out = render_config(
        &runner,
        &fx.paths,
        &fx.config.state_dir,
        &rows,
        "tok-abcdefgh",
    );
    println!("{out}");
    for name in settings::NAMES {
        assert!(out.contains(&format!("  {name}")), "{name}: {out}");
    }
    assert!(
        out.contains("PREVENT_SCREEN_LOCK   on  (settings.json)"),
        "{out}"
    );
    assert!(
        out.contains("RELAY_PORT            8790  (default)"),
        "{out}"
    );
    assert!(
        out.contains("CONDUCTOR_DB          (none)  (default)"),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "state directory: {}\n",
            fx.config.state_dir.display()
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!("plist: {} (missing)\n", fx.paths.plist.display())),
        "{out}"
    );
    assert!(out.contains("service: loaded\n"), "{out}");
    assert!(
        out.contains("tailnet URL: https://mac.example.ts.net/\n"),
        "{out}"
    );
    assert!(out.contains("token: tok-…\n"), "{out}");
    assert!(!out.contains("abcdefgh"), "{out}");
    assert!(
        runner
            .calls()
            .contains(&format!("launchctl print gui/501/{LABEL}")),
        "{:?}",
        runner.calls()
    );
}

#[test]
fn render_config_without_a_mapping_or_a_token_says_so() {
    let fx = fixture();
    let runner = FakeRunner::new(|line| match line {
        l if l.starts_with("launchctl print") => Some(failed("not found")),
        l if l.ends_with("serve status --json") => Some(ok("null")),
        _ => None,
    });
    let rows = config_rows(&fx);
    let out = render_config(&runner, &fx.paths, &fx.config.state_dir, &rows, "");
    assert!(out.contains("service: not loaded\n"), "{out}");
    assert!(
        out.contains("tailnet URL: none (tailscale serve does not map port 8790)"),
        "{out}"
    );
    assert!(out.contains("token: none yet"), "{out}");
}

#[test]
fn config_set_saves_the_setting_and_restarts_a_loaded_service() {
    let fx = fixture();
    let runner = FakeRunner::new(|_| None);
    let message = config_set(&runner, &fx.paths, &fx.config, "PUSH_NOTIFY", "false").unwrap();
    assert!(message.contains("PUSH_NOTIFY saved"), "{message}");
    assert!(message.contains("restarted"), "{message}");
    let (_, rows) = settings::resolve(&fx.config.state_dir, &|_| None).unwrap();
    assert!(rows.contains(&("PUSH_NOTIFY", "off".to_owned(), settings::Source::File)));
    let target = format!("gui/501/{LABEL}");
    assert_eq!(
        runner.calls(),
        [
            "id -u".to_owned(),
            format!("launchctl print {target}"),
            format!("launchctl kickstart -k {target}"),
        ]
    );
}

#[test]
fn config_set_leaves_an_unloaded_service_alone() {
    let fx = fixture();
    let runner = FakeRunner::new(|line| match line {
        l if l.starts_with("launchctl print") => Some(failed("not found")),
        _ => None,
    });
    let message = config_set(&runner, &fx.paths, &fx.config, "PUSH_NOTIFY", "false").unwrap();
    assert!(message.contains("not loaded"), "{message}");
    assert!(
        runner
            .calls()
            .iter()
            .all(|call| !call.contains("kickstart")),
        "{:?}",
        runner.calls()
    );
}

#[test]
fn config_set_of_the_port_never_restarts_the_service() {
    let fx = fixture();
    let runner = FakeRunner::new(|_| None);
    let message = config_set(&runner, &fx.paths, &fx.config, "RELAY_PORT", "8800").unwrap();
    assert!(message.contains("RELAY_PORT saved"), "{message}");
    assert!(message.contains("service install"), "{message}");
    let saved = std::fs::read_to_string(fx.config.state_dir.join("settings.json")).unwrap();
    assert!(saved.contains("8800"), "{saved}");
    assert!(
        runner
            .calls()
            .iter()
            .all(|call| !call.contains("launchctl")),
        "{:?}",
        runner.calls()
    );
}

#[test]
fn config_set_refuses_an_invalid_value_before_any_command() {
    let fx = fixture();
    let runner = FakeRunner::new(|_| None);
    let error = config_set(&runner, &fx.paths, &fx.config, "EXPOSE", "public")
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("EXPOSE: public mode is not supported"),
        "{error}"
    );
    assert!(runner.calls().is_empty(), "{:?}", runner.calls());
    assert!(!fx.config.state_dir.join("settings.json").exists());
}
