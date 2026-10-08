use std::cell::RefCell;
use std::io;
use std::path::Path;
use std::rc::Rc;

use conductor_remote::service::tailnet::{render, run, TailnetAction, TailnetReport};
use conductor_remote::service::{CommandOutput, CommandRunner};

const TAILSCALE: &str = "/opt/homebrew/bin/tailscale";
const RELAY_PORT: u16 = 8790;
const TS_STATUS: &str = r#"{"Self":{"DNSName":"mac.example.ts.net."}}"#;
const SERVE_443: &str = r#"{"TCP":{"443":{"HTTPS":true}},
  "Web":{"mac.example.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8790"}}}}}"#;
const SERVE_8443: &str = r#"{"TCP":{"443":{"HTTPS":true},"8443":{"HTTPS":true}},
  "Web":{"mac.example.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:3000"}}},
         "mac.example.ts.net:8443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8790"}}}}}"#;
const SERVE_ELSEWHERE: &str = r#"{"TCP":{"443":{"HTTPS":true}},
  "Web":{"mac.example.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:3000"}}}}}"#;

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

    fn changes(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|line| line.contains(" serve --"))
            .collect()
    }
}

impl CommandRunner for FakeRunner {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput> {
        let line = std::iter::once(program)
            .chain(args.iter().copied())
            .collect::<Vec<_>>()
            .join(" ");
        self.calls.borrow_mut().push(line.clone());
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

/// A tailnet whose serve status is `serve` until a `serve --bg` or `serve … off` changes it.
fn tailnet(serve: &'static str, after_change: &'static str) -> FakeRunner {
    let changed = Rc::new(RefCell::new(false));
    FakeRunner::new(move |line| {
        if line.ends_with(" serve --bg --https=443 http://127.0.0.1:8790")
            || line.ends_with(" serve --bg --https=8443 http://127.0.0.1:8790")
            || line.ends_with(" serve --https=443 off")
            || line.ends_with(" serve --https=8443 off")
        {
            *changed.borrow_mut() = true;
            return Some(ok(""));
        }
        match line {
            l if l == format!("{TAILSCALE} serve status --json") => {
                Some(ok(if *changed.borrow() {
                    after_change
                } else {
                    serve
                }))
            }
            l if l == format!("{TAILSCALE} status --json") => Some(ok(TS_STATUS)),
            _ => None,
        }
    })
}

fn go(action: TailnetAction, runner: &FakeRunner) -> TailnetReport {
    run(action, runner, Some(Path::new(TAILSCALE)), RELAY_PORT)
}

#[test]
fn status_of_a_relay_mapped_on_443_has_a_url_without_a_port() {
    let runner = tailnet(SERVE_443, SERVE_443);
    let report = go(TailnetAction::Status, &runner);
    assert_eq!(
        report,
        TailnetReport {
            tailscale: true,
            host: Some("mac.example.ts.net".to_owned()),
            https_port: Some(443),
            mapped: true,
            url: Some("https://mac.example.ts.net/".to_owned()),
            error: None,
        }
    );
    assert!(runner.changes().is_empty(), "{:?}", runner.calls());
}

#[test]
fn status_of_a_relay_mapped_on_8443_has_a_url_with_the_port() {
    let runner = tailnet(SERVE_8443, SERVE_8443);
    let report = go(TailnetAction::Status, &runner);
    assert_eq!(report.https_port, Some(8443));
    assert!(report.mapped);
    assert_eq!(
        report.url.as_deref(),
        Some("https://mac.example.ts.net:8443/")
    );
}

#[test]
fn status_with_nothing_mapped_has_a_host_and_no_url() {
    let runner = tailnet("null", "null");
    let report = go(TailnetAction::Status, &runner);
    assert!(report.tailscale);
    assert_eq!(report.host.as_deref(), Some("mac.example.ts.net"));
    assert_eq!(report.https_port, None);
    assert!(!report.mapped);
    assert_eq!(report.url, None);
    assert_eq!(report.error, None);
    assert!(render(&report, false).contains("tailnet: not mapped"));
}

#[test]
fn ensure_maps_the_first_free_port_with_one_serve_command() {
    let runner = tailnet(SERVE_ELSEWHERE, SERVE_8443);
    let report = go(TailnetAction::Ensure, &runner);
    assert_eq!(
        runner.changes(),
        vec![format!(
            "{TAILSCALE} serve --bg --https=8443 http://127.0.0.1:8790"
        )]
    );
    assert_eq!(report.error, None);
    assert!(report.mapped);
    assert_eq!(report.https_port, Some(8443));
    assert_eq!(
        report.url.as_deref(),
        Some("https://mac.example.ts.net:8443/")
    );
}

#[test]
fn ensure_on_an_empty_serve_status_maps_443() {
    let runner = tailnet("null", SERVE_443);
    let report = go(TailnetAction::Ensure, &runner);
    assert_eq!(
        runner.changes(),
        vec![format!(
            "{TAILSCALE} serve --bg --https=443 http://127.0.0.1:8790"
        )]
    );
    assert_eq!(report.url.as_deref(), Some("https://mac.example.ts.net/"));
}

#[test]
fn ensure_with_an_existing_mapping_runs_no_serve_command() {
    let runner = tailnet(SERVE_8443, SERVE_8443);
    let report = go(TailnetAction::Ensure, &runner);
    assert!(runner.changes().is_empty(), "{:?}", runner.calls());
    assert!(!runner.calls().iter().any(|line| line.contains("--bg")));
    assert!(report.mapped);
    assert_eq!(report.https_port, Some(8443));
    assert_eq!(report.error, None);
}

#[test]
fn off_turns_its_own_mapping_off() {
    let runner = tailnet(SERVE_8443, SERVE_ELSEWHERE);
    let report = go(TailnetAction::Off, &runner);
    assert_eq!(
        runner.changes(),
        vec![format!("{TAILSCALE} serve --https=8443 off")]
    );
    assert!(!report.mapped);
    assert_eq!(report.url, None);
    assert_eq!(report.error, None);
}

#[test]
fn off_leaves_a_port_mapped_elsewhere_alone() {
    let runner = tailnet(SERVE_ELSEWHERE, SERVE_ELSEWHERE);
    let report = go(TailnetAction::Off, &runner);
    assert!(runner.changes().is_empty(), "{:?}", runner.calls());
    assert!(!report.mapped);
    assert_eq!(report.error, None);
}

#[test]
fn without_a_binary_nothing_runs_and_only_ensure_complains() {
    let runner = tailnet("null", "null");
    for action in [TailnetAction::Status, TailnetAction::Off] {
        let report = run(action, &runner, None, RELAY_PORT);
        assert!(!report.tailscale);
        assert_eq!(
            (report.host, report.https_port, report.mapped, report.url),
            (None, None, false, None)
        );
        assert_eq!(report.error, None);
    }
    let report = run(TailnetAction::Ensure, &runner, None, RELAY_PORT);
    assert!(!report.tailscale);
    assert_eq!(
        report.error.as_deref(),
        Some("tailscale was not found on this Mac")
    );
    assert!(runner.calls().is_empty());
}

#[test]
fn a_failing_serve_status_gives_a_report_with_an_error() {
    let runner = FakeRunner::new(|line| {
        if line.ends_with("serve status --json") {
            Some(failed("secret-output-of-tailscale"))
        } else if line.ends_with(" status --json") {
            Some(ok(TS_STATUS))
        } else {
            None
        }
    });
    for action in [
        TailnetAction::Status,
        TailnetAction::Ensure,
        TailnetAction::Off,
    ] {
        let report = go(action, &runner);
        assert!(report.tailscale);
        assert_eq!(report.host.as_deref(), Some("mac.example.ts.net"));
        assert!(!report.mapped);
        assert_eq!(report.url, None);
        let error = report.error.expect("an error");
        assert!(error.contains("serve status could not be read"), "{error}");
        assert!(!error.contains("secret-output"), "{error}");
    }
    assert!(runner.changes().is_empty(), "{:?}", runner.calls());
}

#[test]
fn a_failing_mapping_or_name_is_named_without_the_programs_output() {
    let runner = FakeRunner::new(|line| {
        if line.ends_with("serve status --json") {
            Some(ok("null"))
        } else if line.contains(" serve --bg") {
            Some(failed("secret-output-of-serve"))
        } else if line.ends_with(" status --json") {
            Some(failed("secret-output-of-status"))
        } else {
            None
        }
    });
    let report = go(TailnetAction::Ensure, &runner);
    assert_eq!(report.host, None);
    assert!(!report.mapped);
    let error = report.error.expect("an error");
    assert!(error.contains("the mapping could not be made"), "{error}");
    assert!(
        error.contains("the tailnet name could not be read"),
        "{error}"
    );
    assert!(!error.contains("secret-output"), "{error}");
}

#[test]
fn the_json_rendering_has_exactly_the_camel_case_keys() {
    let runner = tailnet(SERVE_8443, SERVE_8443);
    let report = go(TailnetAction::Status, &runner);
    let line = render(&report, true);
    println!("{line}");
    assert!(!line.contains('\n'));
    let value: serde_json::Value = serde_json::from_str(&line).unwrap();
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["error", "host", "httpsPort", "mapped", "tailscale", "url"]
    );
    assert_eq!(value["httpsPort"], 8443);
    assert_eq!(value["mapped"], true);
    assert_eq!(value["url"], "https://mac.example.ts.net:8443/");
    assert_eq!(value["error"], serde_json::Value::Null);
}

#[test]
fn no_rendering_holds_the_word_token() {
    for (serve, action) in [
        (SERVE_443, TailnetAction::Status),
        (SERVE_8443, TailnetAction::Ensure),
        ("null", TailnetAction::Ensure),
        ("null", TailnetAction::Off),
    ] {
        let report = go(action, &tailnet(serve, serve));
        for json in [true, false] {
            let text = render(&report, json).to_lowercase();
            assert!(!text.contains("token"), "{text}");
        }
    }
    let none = run(
        TailnetAction::Ensure,
        &tailnet("null", "null"),
        None,
        RELAY_PORT,
    );
    for json in [true, false] {
        assert!(!render(&none, json).to_lowercase().contains("token"));
    }
}

#[test]
fn the_human_rendering_reads_as_lines() {
    let runner = tailnet(SERVE_8443, SERVE_8443);
    let text = render(&go(TailnetAction::Status, &runner), false);
    assert_eq!(
        text,
        "tailscale: found\nhost: mac.example.ts.net\ntailnet: https://mac.example.ts.net:8443/"
    );
    let none = run(TailnetAction::Ensure, &runner, None, RELAY_PORT);
    assert_eq!(
        render(&none, false),
        "tailscale: not found\ntailnet: not mapped\nerror: tailscale was not found on this Mac"
    );
}
