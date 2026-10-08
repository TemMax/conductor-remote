//! The relay's place on the tailnet as a library function with a machine-readable report.
//!
//! The same mapping `service install` makes and `service uninstall` removes, without the
//! LaunchAgent. A report never holds the token or a program's output.

use std::path::Path;

use crate::state::settings::{self, Expose, Source};

use super::ServeStatus;
use super::{choose_https_port, dns_name, exec_checked, serve_status, CommandRunner, HttpsPort};

/// What `conductor-remote tailnet` is asked to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum TailnetAction {
    Status,
    Ensure,
    Off,
    /// Save EXPOSE=tailnet and set up the mapping.
    Enable,
    /// Save EXPOSE=off and remove the mapping.
    Disable,
}

/// The relay's place on the tailnet. Serialised in camelCase.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TailnetReport {
    /// Resolved EXPOSE; null when settings could not be read.
    pub enabled: Option<bool>,
    /// "environment", "file" or "default"; never a setting's contents.
    pub expose_source: Option<String>,
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
            enabled: None,
            expose_source: None,
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

/// Read EXPOSE for every command. Explicit enable/disable persist the preference before
/// reconciling the mapping; a failed Tailscale command never discards that preference.
pub fn run_configured(
    action: TailnetAction,
    runner: &dyn CommandRunner,
    tailscale: Option<&Path>,
    relay_port: u16,
    state_dir: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> TailnetReport {
    let (mut resolved, mut rows) = match settings::resolve(state_dir, env) {
        Ok(value) => value,
        Err(_) => {
            let mut report = TailnetReport::empty();
            report.fail("the relay settings could not be read; check EXPOSE and settings.json");
            return report;
        }
    };
    let source =
        |rows: &[settings::Row]| rows.iter().find(|row| row.0 == "EXPOSE").map(|row| row.2);
    let mut preference_error = None;
    if matches!(action, TailnetAction::Enable | TailnetAction::Disable) {
        if source(&rows) == Some(Source::Environment) {
            preference_error =
                Some("EXPOSE is controlled by the environment; remove that override to change it");
        } else {
            let value = if action == TailnetAction::Enable {
                "tailnet"
            } else {
                "off"
            };
            if settings::set(state_dir, "EXPOSE", value).is_err() {
                preference_error = Some("the access preference could not be saved");
            } else {
                match settings::resolve(state_dir, env) {
                    Ok(value) => (resolved, rows) = value,
                    Err(_) => {
                        let mut report = TailnetReport::empty();
                        report.fail("the saved access preference could not be read");
                        return report;
                    }
                }
            }
        }
    }
    let enabled = resolved.expose == Expose::Tailnet;
    let effective = if preference_error.is_some() {
        TailnetAction::Status
    } else {
        match action {
            TailnetAction::Enable => TailnetAction::Ensure,
            TailnetAction::Disable => TailnetAction::Off,
            TailnetAction::Ensure if !enabled => TailnetAction::Off,
            action => action,
        }
    };
    let mut report = run(effective, runner, tailscale, relay_port);
    report.enabled = Some(enabled);
    report.expose_source = source(&rows).map(|source| {
        match source {
            Source::Environment => "environment",
            Source::File => "file",
            Source::Default => "default",
        }
        .to_owned()
    });
    if let Some(error) = preference_error {
        report.fail(error);
    }
    report
}

/// Blocking. `tailscale` is the binary, `None` when this Mac has none.
pub fn run(
    action: TailnetAction,
    runner: &dyn CommandRunner,
    tailscale: Option<&Path>,
    relay_port: u16,
) -> TailnetReport {
    let mut report = TailnetReport::empty();
    if matches!(action, TailnetAction::Enable | TailnetAction::Disable) {
        report.fail("enable and disable require the persisted access settings");
        return report;
    }
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

    if action == TailnetAction::Off && report.mapped {
        report.fail("the mapping is still active; access could not be turned off");
    } else if action == TailnetAction::Ensure && !report.mapped && report.error.is_none() {
        report.fail("the mapping could not be confirmed");
    }

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
        TailnetAction::Status | TailnetAction::Enable | TailnetAction::Disable => None,
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
                exec_checked(runner, &program, &["serve", &https, "--set-path=/", "off"])
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
    if let Some(enabled) = report.enabled {
        lines.push(format!(
            "access: {}",
            if enabled {
                "enabled"
            } else {
                "off (local only)"
            }
        ));
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
