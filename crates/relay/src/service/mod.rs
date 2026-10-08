//! Installing the relay as a login service and exposing it on the tailnet.
//!
//! Every side effect goes through [`CommandRunner`] (external commands) or [`ServicePaths`]
//! (files), so the logic is testable without touching the real service.

use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};

use crate::contract::{Config, APP_BUNDLE_ID, DEFAULT_PORT};
use crate::state::settings::{self, Expose, Source};

pub mod tailnet;

const LABEL: &str = APP_BUNDLE_ID;

/// HTTPS ports tried, in order, when exposing the relay on the tailnet.
const HTTPS_CANDIDATES: [u16; 9] = [443, 8443, 8444, 8445, 8446, 8447, 8448, 8449, 8450];

/// The `PATH` the service runs with: Homebrew's tools (`gh`, `git`) first, then the system's.
const SERVICE_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin";

const TAILSCALE_LOCATIONS: [&str; 3] = [
    "/opt/homebrew/bin/tailscale",
    "/usr/local/bin/tailscale",
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
];

#[derive(clap::Subcommand, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceCommand {
    /// Install the login service and expose it on the tailnet
    Install,
    /// Remove the login service and its tailnet mapping
    Uninstall,
    /// Restart the login service
    Restart,
    /// Show the service state and the phone URL
    Status,
}

/// What an external command printed and whether it succeeded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs an external command and returns its exit status, stdout and stderr.
pub trait CommandRunner {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CommandOutput>;
}

/// Whether a loopback port can be bound right now.
pub trait PortProbe {
    fn is_free(&self, port: u16) -> bool;
}

/// Where the service's files live; tests point these at a temporary directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServicePaths {
    /// `~/Library/LaunchAgents/com.temmax.conductor-remote.plist`
    pub plist: PathBuf,
    /// `~/Library/Logs/com.temmax.conductor-remote`
    pub log_dir: PathBuf,
    /// The running binary (`std::env::current_exe`).
    pub executable: PathBuf,
}

impl ServicePaths {
    /// The paths for this user and this binary.
    pub fn detect() -> anyhow::Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| !home.as_os_str().is_empty())
            .ok_or_else(|| anyhow!("HOME is not set"))?;
        let executable = std::env::current_exe().context("cannot locate the running binary")?;
        Ok(Self {
            plist: home
                .join("Library/LaunchAgents")
                .join(format!("{LABEL}.plist")),
            log_dir: log_dir(&home),
            executable,
        })
    }
}

/// Where the service's `relay.log` and `relay.err.log` live: `~/Library/Logs/<label>`.
pub fn log_dir(home: &Path) -> PathBuf {
    home.join("Library/Logs").join(LABEL)
}

/// The HTTPS port the relay is (or will be) served on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpsPort {
    /// The relay is already mapped on this port.
    Existing(u16),
    /// Nothing is mapped for the relay; this port is free to use.
    Free(u16),
}

/// Runs the real commands.
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CommandOutput> {
        let output = std::process::Command::new(program).args(args).output()?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

struct SystemPorts;

impl PortProbe for SystemPorts {
    fn is_free(&self, port: u16) -> bool {
        std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
    }
}

pub fn run(command: ServiceCommand, config: &Config) -> anyhow::Result<()> {
    let paths = ServicePaths::detect()?;
    let token = match command {
        // The relay has not necessarily started yet, so a first install mints the token.
        ServiceCommand::Install => Some(
            crate::state::load_or_create_token(&config.state_dir)
                .with_context(|| {
                    format!(
                        "cannot read or create the token in {}",
                        config.state_dir.display()
                    )
                })?
                .expose()
                .to_owned(),
        ),
        ServiceCommand::Status | ServiceCommand::Restart | ServiceCommand::Uninstall => {
            read_token(&config.state_dir)
        }
    };
    let mut stdout = std::io::stdout().lock();
    run_with(
        command,
        config,
        &paths,
        &SystemRunner,
        &SystemPorts,
        token.as_deref(),
        &mut stdout,
    )
}

/// The token file's trimmed contents; `None` when it is missing, unreadable or empty.
pub fn read_token(state_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(state_dir.join("token")).ok()?;
    let token = text.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

/// [`run_with_tailscale`] with the `tailscale` binary looked up on this machine.
pub fn run_with(
    command: ServiceCommand,
    config: &Config,
    paths: &ServicePaths,
    runner: &dyn CommandRunner,
    ports: &dyn PortProbe,
    token: Option<&str>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let tailscale = find_tailscale(|path| path.exists());
    run_with_tailscale(
        command,
        config,
        paths,
        runner,
        ports,
        tailscale.as_deref(),
        token,
        out,
    )
}

/// Runs `command` with every side effect injected, including where `tailscale` is.
// Every side effect is its own injected parameter, so the signature is deliberately flat.
#[allow(clippy::too_many_arguments)]
pub fn run_with_tailscale(
    command: ServiceCommand,
    config: &Config,
    paths: &ServicePaths,
    runner: &dyn CommandRunner,
    ports: &dyn PortProbe,
    tailscale: Option<&Path>,
    token: Option<&str>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let env = Env {
        config,
        paths,
        runner,
        ports,
        tailscale,
        uid: current_uid(runner)?,
    };
    match command {
        ServiceCommand::Install => install(&env, token, out),
        ServiceCommand::Uninstall => uninstall(&env),
        ServiceCommand::Restart => {
            let target = env.service_target();
            exec_checked(runner, "launchctl", &["kickstart", "-k", &target])?;
            Ok(())
        }
        ServiceCommand::Status => status(&env, token, out),
    }
}

struct Env<'a> {
    config: &'a Config,
    paths: &'a ServicePaths,
    runner: &'a dyn CommandRunner,
    ports: &'a dyn PortProbe,
    tailscale: Option<&'a Path>,
    uid: String,
}

impl Env<'_> {
    fn domain(&self) -> String {
        format!("gui/{}", self.uid)
    }

    fn service_target(&self) -> String {
        format!("gui/{}/{LABEL}", self.uid)
    }
}

fn current_uid(runner: &dyn CommandRunner) -> anyhow::Result<String> {
    let output = exec_checked(runner, "id", &["-u"])?;
    let uid = output.stdout.trim();
    if uid.is_empty() || !uid.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("`id -u` printed an unexpected user id: {uid:?}");
    }
    Ok(uid.to_owned())
}

fn exec(runner: &dyn CommandRunner, program: &str, args: &[&str]) -> anyhow::Result<CommandOutput> {
    runner
        .run(program, args)
        .with_context(|| format!("could not run `{}`", describe(program, args)))
}

/// Like [`exec`], but a non-zero exit is an error carrying the command's stderr.
fn exec_checked(
    runner: &dyn CommandRunner,
    program: &str,
    args: &[&str],
) -> anyhow::Result<CommandOutput> {
    let output = exec(runner, program, args)?;
    if !output.success {
        bail!(
            "`{}` failed: {}",
            describe(program, args),
            output.stderr.trim()
        );
    }
    Ok(output)
}

fn describe(program: &str, args: &[&str]) -> String {
    std::iter::once(program)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Runs `launchctl bootstrap` up to 5 times, sleeping 500 ms after a failed attempt that is not
/// the last. On every update of a running service the first bootstrap fails with `Bootstrap
/// failed: 5: Input/output error`, because the preceding bootout has not finished; a second
/// bootstrap a moment later succeeds. When all 5 attempts fail, the last failure is returned.
fn bootstrap_with_retry(
    runner: &dyn CommandRunner,
    domain: &str,
    plist_path: &str,
) -> anyhow::Result<CommandOutput> {
    const ATTEMPTS: u32 = 5;
    let mut attempt = 1;
    loop {
        match exec_checked(runner, "launchctl", &["bootstrap", domain, plist_path]) {
            Err(error) if attempt == ATTEMPTS => return Err(error),
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(500));
                attempt += 1;
            }
            Ok(output) => return Ok(output),
        }
    }
}

fn install(env: &Env, token: Option<&str>, out: &mut dyn Write) -> anyhow::Result<()> {
    let paths = env.paths;
    // A relay that cannot bind its port would be restarted forever by `KeepAlive`. When the
    // service is loaded, the port is held by the service itself, so only an unloaded one is checked.
    let loaded = exec(env.runner, "launchctl", &["print", &env.service_target()])?.success;
    if !loaded && !env.ports.is_free(env.config.port) {
        bail!(
            "port {port} is already in use by another program; set RELAY_PORT to a free port \
             and run `service install` again",
            port = env.config.port
        );
    }
    std::fs::create_dir_all(&paths.log_dir)
        .with_context(|| format!("cannot create {}", paths.log_dir.display()))?;
    if let Some(parent) = paths.plist.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let plist = build_plist(LABEL, &paths.executable, env.config.port, &paths.log_dir);
    std::fs::write(&paths.plist, plist)
        .with_context(|| format!("cannot write {}", paths.plist.display()))?;

    let domain = env.domain();
    let target = env.service_target();
    let plist_path = paths.plist.to_string_lossy();
    // The service may not be loaded yet, so a failing bootout is expected.
    exec(env.runner, "launchctl", &["bootout", &target])?;
    bootstrap_with_retry(env.runner, &domain, &plist_path)?;
    exec_checked(env.runner, "launchctl", &["enable", &target])?;
    exec_checked(env.runner, "launchctl", &["kickstart", "-k", &target])?;

    let expose = settings::resolve(&env.config.state_dir, &|name| std::env::var(name).ok())
        .map_err(|error| anyhow!(error))?
        .0
        .expose;
    match (expose, env.tailscale) {
        (Expose::Off, _) => {
            writeln!(
                out,
                "EXPOSE is off: the relay is not mapped on the tailnet; it listens on \
                 http://127.0.0.1:{} only",
                env.config.port
            )?;
        }
        (Expose::Tailnet, Some(tailscale)) => {
            let serve = serve_status(env.runner, tailscale)?;
            if let HttpsPort::Free(port) = choose_https_port(&serve, env.config.port)? {
                let https = format!("--https={port}");
                let upstream = format!("http://127.0.0.1:{}", env.config.port);
                exec_checked(
                    env.runner,
                    &tailscale.to_string_lossy(),
                    &["serve", "--bg", &https, &upstream],
                )?;
            }
        }
        (Expose::Tailnet, None) => {
            writeln!(
                out,
                "tailscale was not found; to expose the relay on your tailnet, run:\n  \
                 tailscale serve --bg --https=443 http://127.0.0.1:{}",
                env.config.port
            )?;
        }
    }
    status(env, token, out)
}

fn uninstall(env: &Env) -> anyhow::Result<()> {
    let target = env.service_target();
    // The service may already be unloaded, so a failing bootout is expected.
    exec(env.runner, "launchctl", &["bootout", &target])?;
    match std::fs::remove_file(&env.paths.plist) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err)
                .with_context(|| format!("cannot remove {}", env.paths.plist.display()));
        }
    }
    if let Some(tailscale) = env.tailscale {
        if let Some(port) = existing_mapping(env.runner, tailscale, env.config.port)? {
            let https = format!("--https={port}");
            exec_checked(
                env.runner,
                &tailscale.to_string_lossy(),
                &["serve", &https, "off"],
            )?;
        }
    }
    Ok(())
}

fn status(env: &Env, token: Option<&str>, out: &mut dyn Write) -> anyhow::Result<()> {
    let plist = &env.paths.plist;
    let exists = if plist.exists() { "exists" } else { "missing" };
    writeln!(out, "plist: {} ({exists})", plist.display())?;

    let target = env.service_target();
    let print = exec(env.runner, "launchctl", &["print", &target])?;
    let loaded = print.success;
    writeln!(
        out,
        "service: {}",
        if loaded { "loaded" } else { "not loaded" }
    )?;
    match loaded.then(|| service_pid(&print.stdout)).flatten() {
        Some(pid) => writeln!(out, "pid: {pid}")?,
        None => writeln!(out, "pid: not running")?,
    }
    let log_dir = &env.paths.log_dir;
    writeln!(
        out,
        "logs: {} and {}",
        log_dir.join("relay.log").display(),
        log_dir.join("relay.err.log").display()
    )?;

    let Some(tailscale) = env.tailscale else {
        writeln!(out, "tailnet: tailscale was not found")?;
        return Ok(());
    };
    let Some(https_port) = existing_mapping(env.runner, tailscale, env.config.port)? else {
        writeln!(
            out,
            "tailnet: the relay is not exposed; run `service install`"
        )?;
        return Ok(());
    };
    let dns_name = dns_name(env.runner, tailscale)?;
    match token {
        Some(token) => writeln!(
            out,
            "phone URL: {}",
            phone_url(&dns_name, https_port, token)
        )?,
        None => {
            let url = phone_url(&dns_name, https_port, "");
            let url = url.strip_suffix("#token=").unwrap_or(&url);
            writeln!(out, "phone URL: {url}")?;
            writeln!(
                out,
                "the relay has not created a token yet; start it once, then run `service status` again"
            )?;
        }
    }
    Ok(())
}

/// The `pid = N` line of `launchctl print`.
fn service_pid(print: &str) -> Option<u32> {
    print
        .lines()
        .find_map(|line| line.trim().strip_prefix("pid = "))
        .and_then(|pid| pid.trim().parse().ok())
}

fn serve_status(runner: &dyn CommandRunner, tailscale: &Path) -> anyhow::Result<String> {
    let output = exec_checked(
        runner,
        &tailscale.to_string_lossy(),
        &["serve", "status", "--json"],
    )?;
    Ok(output.stdout)
}

fn existing_mapping(
    runner: &dyn CommandRunner,
    tailscale: &Path,
    relay_port: u16,
) -> anyhow::Result<Option<u16>> {
    let json = serve_status(runner, tailscale)?;
    Ok(ServeStatus::parse(&json)?.mapping_to(relay_port))
}

fn dns_name(runner: &dyn CommandRunner, tailscale: &Path) -> anyhow::Result<String> {
    let output = exec_checked(runner, &tailscale.to_string_lossy(), &["status", "--json"])?;
    let value: serde_json::Value =
        serde_json::from_str(&output.stdout).context("`tailscale status --json` is not JSON")?;
    value["Self"]["DNSName"]
        .as_str()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("`tailscale status --json` has no Self.DNSName"))
}

/// The parts of `tailscale serve status --json` this module cares about.
struct ServeStatus {
    /// Ports listed under `TCP`.
    tcp_ports: Vec<u16>,
    /// Each `Web` entry: its HTTPS port and the target of its `/` handler.
    web: Vec<(u16, Option<String>)>,
}

impl ServeStatus {
    fn parse(json: &str) -> anyhow::Result<Self> {
        let json = json.trim();
        let mut status = Self {
            tcp_ports: Vec::new(),
            web: Vec::new(),
        };
        if json.is_empty() {
            return Ok(status);
        }
        let value: serde_json::Value =
            serde_json::from_str(json).context("`tailscale serve status --json` is not JSON")?;
        let root = match &value {
            serde_json::Value::Null => return Ok(status),
            serde_json::Value::Object(root) => root,
            _ => bail!("`tailscale serve status --json` is not a JSON object"),
        };
        if let Some(tcp) = root.get("TCP").and_then(|tcp| tcp.as_object()) {
            status.tcp_ports = tcp.keys().filter_map(|key| key.parse().ok()).collect();
        }
        if let Some(web) = root.get("Web").and_then(|web| web.as_object()) {
            for (host_port, entry) in web {
                let Some(port) = host_port
                    .rsplit_once(':')
                    .and_then(|(_, port)| port.parse().ok())
                else {
                    continue;
                };
                let proxy = entry["Handlers"]["/"]["Proxy"].as_str().map(str::to_owned);
                status.web.push((port, proxy));
            }
        }
        Ok(status)
    }

    /// The lowest HTTPS port whose `/` handler proxies to the relay.
    fn mapping_to(&self, relay_port: u16) -> Option<u16> {
        let target = format!("http://127.0.0.1:{relay_port}");
        self.web
            .iter()
            .filter(|(_, proxy)| {
                proxy
                    .as_deref()
                    .is_some_and(|proxy| proxy.trim_end_matches('/') == target)
            })
            .map(|(port, _)| *port)
            .min()
    }

    fn is_taken(&self, port: u16) -> bool {
        self.tcp_ports.contains(&port) || self.web.iter().any(|(web, _)| *web == port)
    }
}

/// Picks the HTTPS port for the relay from the output of `tailscale serve status --json`.
pub fn choose_https_port(serve_status_json: &str, relay_port: u16) -> anyhow::Result<HttpsPort> {
    let status = ServeStatus::parse(serve_status_json)?;
    if let Some(port) = status.mapping_to(relay_port) {
        return Ok(HttpsPort::Existing(port));
    }
    HTTPS_CANDIDATES
        .iter()
        .copied()
        .find(|port| !status.is_taken(*port))
        .map(HttpsPort::Free)
        .ok_or_else(|| {
            let taken = HTTPS_CANDIDATES
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            anyhow!("no free HTTPS port on the tailnet; taken ports: {taken}")
        })
}

/// The URL to open on the phone.
pub fn phone_url(dns_name: &str, https_port: u16, token: &str) -> String {
    let host = dns_name.strip_suffix('.').unwrap_or(dns_name);
    if https_port == 443 {
        format!("https://{host}/#token={token}")
    } else {
        format!("https://{host}:{https_port}/#token={token}")
    }
}

/// The first existing `tailscale` binary among the usual install locations.
pub fn find_tailscale(exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    TAILSCALE_LOCATIONS
        .iter()
        .map(PathBuf::from)
        .find(|path| exists(path))
}

/// What `conductor-remote config` prints: every setting with its value and where it came from,
/// the state directory, the plist and whether the service is loaded, the tailnet URL when
/// `tailscale serve` maps the relay's port, and the token cut to its first 4 characters.
pub fn render_config(
    runner: &dyn CommandRunner,
    paths: &ServicePaths,
    state_dir: &Path,
    settings: &[(&str, String, Source)],
    token: &str,
) -> String {
    let mut out = String::from("settings:\n");
    let width = settings
        .iter()
        .map(|(name, _, _)| name.len())
        .max()
        .unwrap_or(0);
    for (name, value, source) in settings {
        let value = if value.is_empty() { "(none)" } else { value };
        let source = match source {
            Source::Environment => "environment",
            Source::File => "settings.json",
            Source::Default => "default",
        };
        let _ = writeln!(out, "  {name:<width$}  {value}  ({source})");
    }
    let _ = writeln!(out, "state directory: {}", state_dir.display());
    let exists = if paths.plist.exists() {
        "exists"
    } else {
        "missing"
    };
    let _ = writeln!(out, "plist: {} ({exists})", paths.plist.display());
    let service = match current_uid(runner) {
        Ok(uid) => {
            let target = format!("gui/{uid}/{LABEL}");
            match exec(runner, "launchctl", &["print", &target]) {
                Ok(print) if print.success => "loaded".to_owned(),
                Ok(_) => "not loaded".to_owned(),
                Err(error) => format!("unknown ({error})"),
            }
        }
        Err(error) => format!("unknown ({error})"),
    };
    let _ = writeln!(out, "service: {service}");
    let port = settings
        .iter()
        .find(|(name, _, _)| *name == "RELAY_PORT")
        .and_then(|(_, value, _)| value.parse().ok())
        .unwrap_or(DEFAULT_PORT);
    let tailscale =
        find_tailscale(|path| path.exists()).unwrap_or_else(|| PathBuf::from("tailscale"));
    let url = match existing_mapping(runner, &tailscale, port) {
        Ok(Some(https_port)) => match dns_name(runner, &tailscale) {
            Ok(dns_name) => {
                let url = phone_url(&dns_name, https_port, "");
                url.strip_suffix("#token=").unwrap_or(&url).to_owned()
            }
            Err(error) => format!("unknown ({error})"),
        },
        Ok(None) => format!("none (tailscale serve does not map port {port})"),
        Err(error) => format!("unknown ({error})"),
    };
    let _ = writeln!(out, "tailnet URL: {url}");
    if token.is_empty() {
        let _ = writeln!(
            out,
            "token: none yet (the relay creates it on its first start)"
        );
    } else {
        let start: String = token.chars().take(4).collect();
        let _ = writeln!(out, "token: {start}…");
    }
    out
}

/// `conductor-remote config set NAME VALUE`: validates and saves the setting (an empty value
/// removes it), then restarts the service when it is loaded so it reads the new value (never for
/// `RELAY_PORT`, which the service takes from its plist). Returns what to tell the user.
pub fn config_set(
    runner: &dyn CommandRunner,
    paths: &ServicePaths,
    config: &Config,
    name: &str,
    value: &str,
) -> anyhow::Result<String> {
    settings::set(&config.state_dir, name, value).map_err(|error| anyhow!(error))?;
    let mut message = if value.trim().is_empty() {
        format!("{name} removed from settings.json")
    } else {
        format!("{name} saved in settings.json")
    };
    if name == "RELAY_PORT" {
        let _ = write!(
            message,
            "\nthe service takes its port from {}; run `service install` to move it",
            paths.plist.display()
        );
        // The plist's environment wins over `settings.json`, so a restart would change nothing and
        // only drop the phone's connections: leave the service alone.
        return Ok(message);
    }
    let target = format!("gui/{}/{LABEL}", current_uid(runner)?);
    if exec(runner, "launchctl", &["print", &target])?.success {
        exec_checked(runner, "launchctl", &["kickstart", "-k", &target])?;
        message.push_str("\nthe service was restarted to apply it");
    } else {
        message.push_str("\nthe service is not loaded; it applies the next time the relay starts");
    }
    Ok(message)
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The LaunchAgent property list that starts the relay at login and keeps it running.
pub fn build_plist(label: &str, executable: &Path, port: u16, log_dir: &Path) -> String {
    let label = xml_escape(label);
    let executable = xml_escape(&executable.to_string_lossy());
    let stdout_log = xml_escape(&log_dir.join("relay.log").to_string_lossy());
    let stderr_log = xml_escape(&log_dir.join("relay.err.log").to_string_lossy());
    let mut plist = String::new();
    let _ = write!(
        plist,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{executable}</string>
		<string>start</string>
	</array>
	<key>EnvironmentVariables</key>
	<dict>
		<key>RELAY_PORT</key>
		<string>{port}</string>
		<key>PATH</key>
		<string>{SERVICE_PATH}</string>
	</dict>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>StandardOutPath</key>
	<string>{stdout_log}</string>
	<key>StandardErrorPath</key>
	<string>{stderr_log}</string>
</dict>
</plist>
"#
    );
    plist
}
