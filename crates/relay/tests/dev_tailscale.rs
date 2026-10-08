use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use conductor_remote::dev::tailscale::{
    choose_serve_port, parse_serve_status, ServeStatus, Tailscale,
};
use conductor_remote::reads::extras::commands::{CommandError, Commands, Limits, Output};

const BIN: &str = "/opt/homebrew/bin/tailscale";

const LIVE: &str = r#"{
  "TCP": {"443": {"HTTPS": true}, "8443": {"HTTPS": true}},
  "Web": {
    "mac.example.ts.net:443": {"Handlers": {"/": {"Proxy": "http://127.0.0.1:8790"}}},
    "mac.example.ts.net:8443": {"Handlers": {"/": {"Proxy": "http://127.0.0.1:5173"}}}
  }
}"#;

/// Answers by the first argument (`status`, or `serve` plus `status`), records every call.
struct Script {
    status_json: String,
    serve_status: Result<Output, ()>,
    serve_exit: i32,
    calls: Mutex<Vec<(String, Vec<String>, Limits)>>,
}

impl Script {
    fn new(serve_status: &str) -> Script {
        Script {
            status_json: r#"{"Self":{"DNSName":"mac.example.ts.net."}}"#.to_owned(),
            serve_status: Ok(output(0, serve_status)),
            serve_exit: 0,
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call.1.clone())
            .collect()
    }
}

fn output(code: i32, stdout: &str) -> Output {
    Output {
        code: Some(code),
        stdout: stdout.as_bytes().to_vec(),
        stderr: b"SECRET-STDERR".to_vec(),
    }
}

impl Commands for Script {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        _cwd: Option<&Path>,
        limits: Limits,
    ) -> Result<Output, CommandError> {
        self.calls.lock().unwrap().push((
            program.to_owned(),
            args.iter().map(|a| (*a).to_owned()).collect(),
            limits,
        ));
        match args {
            ["status", "--json"] => Ok(output(0, &self.status_json)),
            ["serve", "status", "--json"] => {
                self.serve_status.clone().map_err(|()| CommandError::Spawn {
                    program: program.to_owned(),
                })
            }
            _ => Ok(output(self.serve_exit, "SECRET-STDOUT")),
        }
    }
}

fn tailscale(script: &Arc<Script>) -> Tailscale {
    Tailscale::new(PathBuf::from(BIN), script.clone())
}

fn ports(ports: &[u16]) -> BTreeSet<u16> {
    ports.iter().copied().collect()
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_owned()).collect()
}

#[test]
fn parses_a_live_serve_status() {
    let status = parse_serve_status(LIVE).unwrap();
    assert_eq!(status.ports, ports(&[443, 8443]));
    assert_eq!(
        status.proxies,
        BTreeMap::from([
            (443, "http://127.0.0.1:8790".to_owned()),
            (8443, "http://127.0.0.1:5173".to_owned()),
        ])
    );
}

#[test]
fn an_empty_object_is_an_empty_status() {
    assert_eq!(parse_serve_status("{}"), Some(ServeStatus::default()));
    assert_eq!(parse_serve_status("  {}\n"), Some(ServeStatus::default()));
}

#[test]
fn garbage_is_not_a_status() {
    assert_eq!(parse_serve_status("not json"), None);
    assert_eq!(parse_serve_status(""), None);
    assert_eq!(parse_serve_status("[1,2]"), None);
}

#[test]
fn the_serve_port_is_never_the_target() {
    let status = ServeStatus::default();
    assert_eq!(choose_serve_port(&status, 8443, &ports(&[])), Some(28443));
    assert_eq!(choose_serve_port(&status, 55200, &ports(&[])), Some(25200));
    assert_eq!(choose_serve_port(&status, 3000, &ports(&[])), Some(23000));
}

#[test]
fn a_taken_serve_port_moves_up() {
    let status = ServeStatus {
        ports: ports(&[28443]),
        proxies: BTreeMap::new(),
    };
    assert_eq!(choose_serve_port(&status, 8443, &ports(&[])), Some(28444));
    let status = ServeStatus {
        ports: BTreeSet::new(),
        proxies: BTreeMap::from([(28443, "http://127.0.0.1:5173".to_owned())]),
    };
    assert_eq!(choose_serve_port(&status, 8443, &ports(&[])), Some(28444));
}

#[test]
fn reserved_ports_are_skipped() {
    let status = ServeStatus::default();
    assert_eq!(
        choose_serve_port(&status, 8443, &ports(&[28443, 28444])),
        Some(28445)
    );
}

#[test]
fn the_targets_own_block_is_skipped() {
    // 25200–25209 are the dev server's own.
    let status = ServeStatus::default();
    assert_eq!(choose_serve_port(&status, 25200, &ports(&[])), Some(25210));
}

#[test]
fn the_range_wraps_and_ends() {
    let status = ServeStatus {
        ports: ports(&[29999]),
        proxies: BTreeMap::new(),
    };
    assert_eq!(choose_serve_port(&status, 9999, &ports(&[])), Some(20000));
    let status = ServeStatus {
        ports: (20000..=29999).collect(),
        proxies: BTreeMap::new(),
    };
    assert_eq!(choose_serve_port(&status, 9999, &ports(&[])), None);
}

#[test]
fn host_strips_the_trailing_dot() {
    let script = Arc::new(Script::new("{}"));
    assert_eq!(
        tailscale(&script).host().as_deref(),
        Some("mac.example.ts.net")
    );
    let calls = script.calls.lock().unwrap();
    assert_eq!(calls[0].0, BIN);
    assert_eq!(calls[0].1, strings(&["status", "--json"]));
}

#[test]
fn host_is_none_without_a_name() {
    let mut script = Script::new("{}");
    script.status_json = r#"{"Self":{}}"#.to_owned();
    assert_eq!(tailscale(&Arc::new(script)).host(), None);
}

#[test]
fn serve_status_runs_serve_status_json() {
    let script = Arc::new(Script::new(LIVE));
    let status = tailscale(&script).serve_status().unwrap();
    assert_eq!(status.ports, ports(&[443, 8443]));
    assert_eq!(
        script.calls(),
        vec![strings(&["serve", "status", "--json"])]
    );
}

#[test]
fn every_call_has_a_timeout_and_an_output_limit() {
    let script = Arc::new(Script::new("{}"));
    let tailscale = tailscale(&script);
    tailscale.serve(8444, 9000).unwrap();
    tailscale.host();
    for (_, _, limits) in script.calls.lock().unwrap().iter() {
        assert_eq!(limits.timeout.as_secs(), 15);
        assert_eq!(limits.max_stdout, 1024 * 1024);
    }
}

#[test]
fn serve_runs_the_documented_arguments() {
    let script = Arc::new(Script::new("{}"));
    tailscale(&script).serve(8444, 9000).unwrap();
    assert_eq!(
        script.calls(),
        vec![strings(&[
            "serve",
            "--bg",
            "--yes",
            "--https=8444",
            "http://127.0.0.1:9000"
        ])]
    );
}

#[test]
fn unserve_turns_off_its_own_mapping() {
    let script = Arc::new(Script::new(LIVE));
    assert_eq!(tailscale(&script).unserve(8443, 5173), Ok(true));
    assert_eq!(
        script.calls(),
        vec![
            strings(&["serve", "status", "--json"]),
            strings(&["serve", "--yes", "--https=8443", "off"]),
        ]
    );
}

#[test]
fn unserve_leaves_a_port_proxied_elsewhere() {
    let script = Arc::new(Script::new(LIVE));
    // 8443 now proxies 5173, not the bridge on 9000; 443 is the relay's own mapping.
    assert_eq!(tailscale(&script).unserve(8443, 9000), Ok(false));
    assert_eq!(tailscale(&script).unserve(443, 9000), Ok(false));
    assert!(script
        .calls()
        .iter()
        .all(|args| !args.contains(&"off".to_owned())));
}

#[test]
fn unserve_of_a_gone_port_is_false() {
    let script = Arc::new(Script::new("{}"));
    assert_eq!(tailscale(&script).unserve(8444, 9000), Ok(false));
    assert_eq!(
        script.calls(),
        vec![strings(&["serve", "status", "--json"])]
    );
}

#[test]
fn a_prefix_of_the_bridge_is_not_the_bridge() {
    let script = Arc::new(Script::new(
        r#"{"Web":{"h.ts.net:8443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:90001"}}}}}"#,
    ));
    assert_eq!(tailscale(&script).unserve(8443, 9000), Ok(false));
}

#[test]
fn a_failing_program_is_an_error_without_its_output() {
    let mut script = Script::new(LIVE);
    script.serve_exit = 1;
    let script = Arc::new(script);
    let tailscale = tailscale(&script);
    let err = tailscale.serve(8444, 9000).unwrap_err();
    assert!(!err.is_empty() && !err.contains("SECRET"), "{err}");
    let err = tailscale.unserve(8443, 5173).unwrap_err();
    assert!(!err.is_empty() && !err.contains("SECRET"), "{err}");
}

#[test]
fn a_program_that_cannot_start_is_an_error() {
    let mut script = Script::new("{}");
    script.serve_status = Err(());
    let err = tailscale(&Arc::new(script)).serve_status().unwrap_err();
    assert!(!err.is_empty());
}

#[test]
fn unparsable_serve_status_is_an_error() {
    let script = Arc::new(Script::new("not json"));
    let err = tailscale(&script).serve_status().unwrap_err();
    assert!(!err.is_empty() && !err.contains("not json"), "{err}");
    assert!(tailscale(&script).unserve(8443, 9000).is_err());
}
