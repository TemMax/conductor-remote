//! The relay's place on the tailnet as a library function with a machine-readable report.
//!
//! The same mapping `service install` makes and `service uninstall` removes, without the
//! LaunchAgent. A report never holds the token or a program's output.

use std::path::Path;

use super::ServeStatus;
use super::{choose_https_port, dns_name, exec_checked, serve_status, CommandRunner, HttpsPort};

/// What `conductor-remote tailnet` is asked to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum TailnetAction {
    Status,
    Ensure,
    Off,
}

/// The relay's place on the tailnet. Serialised in camelCase.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TailnetReport {
    /// Whether a `tailscale` binary was found.
    pub tailscale: bool,
    /// This Mac's tailnet name, without the trailing dot.
    pub host: Option<String>,
    /// The HTTPS port mapped to the relay.
    pub https_port: Option<u16>,
    /// Whether `tailscale serve` proxies an HTTPS port to this relay's port.
    pub mapped: bool,
    /// `https://<host>/`, or `https://<host>:<port>/` when the port is not 443; never a token.
    pub url: Option<String>,
    /// Why the action or a reading failed, in plain words, without the program's output.
    pub error: Option<String>,
}

impl TailnetReport {
    fn empty() -> Self {
        Self {
            tailscale: false,
            host: None,
            https_port: None,
            mapped: false,
            url: None,
            error: None,
        }
    }

    /// Adds a failure; earlier ones are kept, so the first step that failed is named first.
    fn fail(&mut self, reason: &str) {
        self.error = Some(match self.error.take() {
            Some(earlier) => format!("{earlier}; {reason}"),
            None => reason.to_owned(),
        });
    }
}

/// Blocking. `tailscale` is the binary, `None` when this Mac has none.
pub fn run(
    action: TailnetAction,
    runner: &dyn CommandRunner,
    tailscale: Option<&Path>,
    relay_port: u16,
) -> TailnetReport {
    let mut report = TailnetReport::empty();
    let Some(tailscale) = tailscale else {
        if action == TailnetAction::Ensure {
            report.fail("tailscale was not found on this Mac");
        }
        return report;
    };
    report.tailscale = true;

    let mut reading = read_serve(runner, tailscale);
    if action != TailnetAction::Status {
        if let Ok((json, status)) = &reading {
            if let Some(acted) = act(action, runner, tailscale, relay_port, json, status) {
                match acted {
                    // What was changed is read back, as `Status` would.
                    Ok(()) => reading = read_serve(runner, tailscale),
                    Err(reason) => report.fail(reason),
                }
            }
        }
    }

    match reading {
        Ok((_, status)) => report.https_port = status.mapping_to(relay_port),
        Err(()) => report.fail("the serve status could not be read"),
    }
    report.mapped = report.https_port.is_some();

    match dns_name(runner, tailscale) {
        Ok(name) => report.host = Some(name.strip_suffix('.').unwrap_or(&name).to_owned()),
        Err(_) => report.fail("the tailnet name could not be read"),
    }
    if let (Some(host), Some(port)) = (&report.host, report.https_port) {
        report.url = Some(if port == 443 {
            format!("https://{host}/")
        } else {
            format!("https://{host}:{port}/")
        });
    }
    report
}

/// The serve status as printed and parsed; the error is deliberately bare, for it would carry
/// the program's output.
fn read_serve(runner: &dyn CommandRunner, tailscale: &Path) -> Result<(String, ServeStatus), ()> {
    let json = serve_status(runner, tailscale).map_err(|_| ())?;
    let status = ServeStatus::parse(&json).map_err(|_| ())?;
    Ok((json, status))
}

/// Makes or removes the relay's mapping. `None` when there is nothing to do.
fn act(
    action: TailnetAction,
    runner: &dyn CommandRunner,
    tailscale: &Path,
    relay_port: u16,
    json: &str,
    status: &ServeStatus,
) -> Option<Result<(), &'static str>> {
    let program = tailscale.to_string_lossy();
    match action {
        TailnetAction::Status => None,
        TailnetAction::Ensure => {
            let port = match choose_https_port(json, relay_port) {
                Ok(HttpsPort::Existing(_)) => return None,
                Ok(HttpsPort::Free(port)) => port,
                Err(_) => return Some(Err("no HTTPS port is free for the relay")),
            };
            let https = format!("--https={port}");
            let upstream = format!("http://127.0.0.1:{relay_port}");
            Some(
                exec_checked(runner, &program, &["serve", "--bg", &https, &upstream])
                    .map(|_| ())
                    .map_err(|_| "the mapping could not be made"),
            )
        }
        TailnetAction::Off => {
            let port = status.mapping_to(relay_port)?;
            let https = format!("--https={port}");
            Some(
                exec_checked(runner, &program, &["serve", &https, "off"])
                    .map(|_| ())
                    .map_err(|_| "the mapping could not be turned off"),
            )
        }
    }
}

/// The report as one line of JSON (`json == true`) or as the lines a person reads.
pub fn render(report: &TailnetReport, json: bool) -> String {
    if json {
        return serde_json::to_string(report).expect("a report of plain fields serialises");
    }
    let mut lines = vec![format!(
        "tailscale: {}",
        if report.tailscale {
            "found"
        } else {
            "not found"
        }
    )];
    if let Some(host) = &report.host {
        lines.push(format!("host: {host}"));
    }
    lines.push(match &report.url {
        Some(url) => format!("tailnet: {url}"),
        None => "tailnet: not mapped".to_owned(),
    });
    if let Some(error) = &report.error {
        lines.push(format!("error: {error}"));
    }
    lines.join("\n")
}
