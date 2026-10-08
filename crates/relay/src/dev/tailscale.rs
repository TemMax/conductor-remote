//! Publishing a dev-server port on the tailnet through the `tailscale` CLI.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::reads::extras::commands::{Commands, Limits, Output};

const LIMITS: Limits = Limits {
    timeout: Duration::from_secs(15),
    max_stdout: 1024 * 1024,
};

/// The serve ports: a range of their own, below the system's range for outgoing connections and
/// above the usual dev ports.
pub const SERVE_PORTS: std::ops::RangeInclusive<u16> = 20000..=29999;

/// What `tailscale serve status --json` says, reduced to what the relay needs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServeStatus {
    /// The ports `tailscale serve` already uses (the keys of `TCP`).
    pub ports: BTreeSet<u16>,
    /// HTTPS port → the proxy target of its `/` handler (`Web.<host>:<port>.Handlers./.Proxy`).
    pub proxies: BTreeMap<u16, String>,
}

/// `None` when `json` is not a JSON object (an empty object is an empty status).
pub fn parse_serve_status(json: &str) -> Option<ServeStatus> {
    let root = match serde_json::from_str::<Value>(json.trim()).ok()? {
        Value::Object(root) => root,
        Value::Null => return Some(ServeStatus::default()),
        _ => return None,
    };
    let mut status = ServeStatus::default();
    if let Some(tcp) = root.get("TCP").and_then(Value::as_object) {
        status.ports = tcp.keys().filter_map(|port| port.parse().ok()).collect();
    }
    if let Some(web) = root.get("Web").and_then(Value::as_object) {
        for (host_port, entry) in web {
            let port = host_port
                .rsplit_once(':')
                .and_then(|(_, port)| port.parse::<u16>().ok());
            let proxy = entry["Handlers"]["/"]["Proxy"].as_str();
            if let (Some(port), Some(proxy)) = (port, proxy) {
                status.proxies.insert(port, proxy.to_owned());
            }
        }
    }
    Some(status)
}

/// The first serve port tried for a dev server on `target`: 20000 plus the last four digits of
/// `target`, so the link stays recognisable and the same across restarts.
pub fn preferred_serve_port(target: u16) -> u16 {
    20000 + target % 10000
}

/// From `preferred_serve_port(target)` upwards through `SERVE_PORTS`, wrapping round once: the
/// first port that is neither in use by `tailscale serve`, nor reserved, nor one of the dev
/// server's own block (`target` and the nine after it). Never `target` itself: a dev server that
/// binds every address could not bind its port again while a browser still holds a connection to
/// the served one.
pub fn choose_serve_port(
    status: &ServeStatus,
    target: u16,
    reserved: &BTreeSet<u16>,
) -> Option<u16> {
    let first = preferred_serve_port(target);
    let own = target..=target.saturating_add(9);
    (first..=*SERVE_PORTS.end())
        .chain(*SERVE_PORTS.start()..first)
        .find(|port| {
            !status.ports.contains(port)
                && !status.proxies.contains_key(port)
                && !reserved.contains(port)
                && !own.contains(port)
        })
}

pub struct Tailscale {
    bin: PathBuf,
    commands: Arc<dyn Commands>,
}

impl Tailscale {
    pub fn new(bin: PathBuf, commands: Arc<dyn Commands>) -> Tailscale {
        Tailscale { bin, commands }
    }

    /// This Mac's tailnet name (`Self.DNSName` of `status --json` without its trailing dot).
    pub fn host(&self) -> Option<String> {
        let output = self.run("tailscale status", &["status", "--json"]).ok()?;
        let value: Value = serde_json::from_slice(&output.stdout).ok()?;
        let name = value["Self"]["DNSName"].as_str()?.trim_end_matches('.');
        (!name.is_empty()).then(|| name.to_owned())
    }

    pub fn serve_status(&self) -> Result<ServeStatus, String> {
        let output = self.run("tailscale serve status", &["serve", "status", "--json"])?;
        std::str::from_utf8(&output.stdout)
            .ok()
            .and_then(parse_serve_status)
            .ok_or_else(|| "tailscale serve status printed something unreadable".to_owned())
    }

    /// `serve --bg --yes --https=<serve_port> http://127.0.0.1:<bridge_port>`.
    pub fn serve(&self, serve_port: u16, bridge_port: u16) -> Result<(), String> {
        let https = format!("--https={serve_port}");
        let target = bridge_target(bridge_port);
        self.run(
            "tailscale serve",
            &["serve", "--bg", "--yes", &https, &target],
        )
        .map(drop)
    }

    /// `serve --yes --https=<serve_port> off`, only when that port's proxy is exactly
    /// `http://127.0.0.1:<bridge_port>` now; `Ok(false)` when it is someone else's or gone.
    pub fn unserve(&self, serve_port: u16, bridge_port: u16) -> Result<bool, String> {
        let status = self.serve_status()?;
        if status.proxies.get(&serve_port).map(String::as_str) != Some(&bridge_target(bridge_port))
        {
            return Ok(false);
        }
        let https = format!("--https={serve_port}");
        self.run("tailscale serve off", &["serve", "--yes", &https, "off"])
            .map(|_| true)
    }

    /// Runs the binary; `step` names it in the error, which never carries the program's output.
    fn run(&self, step: &str, args: &[&str]) -> Result<Output, String> {
        let program = self.bin.to_string_lossy();
        let output = self
            .commands
            .run(&program, args, None, LIMITS)
            .map_err(|_| format!("{step} could not be run"))?;
        if output.code != Some(0) {
            return Err(format!("{step} failed"));
        }
        Ok(output)
    }
}

fn bridge_target(bridge_port: u16) -> String {
    format!("http://127.0.0.1:{bridge_port}")
}
