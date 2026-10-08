use std::io::IsTerminal;
use std::net::SocketAddr;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use clap::{Parser, Subcommand};
use conductor_remote::contract::{AppState, Config, Services, APP_BUNDLE_ID, CONDUCTOR_BUNDLE_ID};
use conductor_remote::db::ConductorDb;
use conductor_remote::delivery::parked::{Deliverer, Notice, ParkedQueue, ParkedTimings};
use conductor_remote::delivery::service::{WriteDeps, WriteTimings, Writes};
use conductor_remote::dev::controller::{DevDeps, DevServers};
use conductor_remote::doctor::{self, DoctorArgs};
use conductor_remote::files::{ExposeMode, PreviewRoots};
use conductor_remote::host::logbuf::{self, LogBuffer, Redactor};
use conductor_remote::host::nosleep::{NoSleep, SystemSpawner};
use conductor_remote::host::parent;
use conductor_remote::host::restart::{RestartTimings, SystemAppControl};
use conductor_remote::host::service::{Host, HostParts, Supervisor};
use conductor_remote::notify::sender::CurlSender;
use conductor_remote::notify::service::{Notifier, NotifyConfig, DEFAULT_SUBJECT};
use conductor_remote::notify::NotifyService;
use conductor_remote::reads::extras::commands::SystemCommands;
use conductor_remote::reads::extras::Extras;
use conductor_remote::reads::{self, HostPaths, Reads};
use conductor_remote::search::index::{spawn_indexer, SearchIndex};
use conductor_remote::service::tailnet::{self, TailnetAction};
use conductor_remote::service::{find_tailscale, ServiceCommand, ServicePaths, SystemRunner};
use conductor_remote::state::prefs::Prefs;
use conductor_remote::state::store::{ParkedRow, Store};
use conductor_remote::ui::actions::Driver;
use conductor_remote::ui::actor::UiActor;
use conductor_remote::ui::system::SystemDesktop;
use conductor_remote::usage::plan::{PlanUsageService, SystemProbe};
use conductor_remote::usage::tools::ToolUsageService;
use conductor_remote::{app, http, lifecycle, service, state};
use tokio::net::TcpListener;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::oneshot;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Parser)]
#[command(
    name = "conductor-remote",
    version,
    about = "Phone control panel relay for local Conductor agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the relay (the default)
    Start {
        /// Stop when the process that started the relay is gone (the menu-bar app uses this)
        #[arg(long)]
        exit_with_parent: bool,
        /// The process id of the app that starts the relay; with it a parent that is already gone
        /// is noticed at once
        #[arg(long, value_name = "PID", requires = "exit_with_parent")]
        parent_pid: Option<u32>,
    },
    /// Manage the login service
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Check, read-only, what the relay can see of Conductor
    Doctor(#[command(flatten)] DoctorArgs),
    /// Show the settings and where each comes from, or change one
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
    /// Follow the service's log files
    Logs {
        /// How many lines of each file to show first
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: usize,
        /// Print the lines and exit instead of following
        #[arg(long)]
        no_follow: bool,
    },
    /// Show, make or remove the relay's place on the tailnet
    Tailnet {
        #[arg(value_enum)]
        action: TailnetAction,
        /// Print the report as one line of JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Save a setting in settings.json (an empty value removes it) and restart the service
    Set { name: String, value: String },
}

fn main() -> ExitCode {
    // Under launchd there is no terminal, and the Tailscale CLI, which is the app's own binary, then
    // tries to start the GUI instead of acting as a CLI. This variable makes it act as a CLI, and
    // every program the relay starts inherits it.
    std::env::set_var("TAILSCALE_BE_CLI", "1");
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Start {
        exit_with_parent: false,
        parent_pid: None,
    });
    match command {
        Command::Start {
            exit_with_parent,
            parent_pid,
        } => start(exit_with_parent, parent_pid),
        Command::Doctor(args) => doctor::run(&args),
        Command::Service { command } => {
            let config = state::load_config();
            if let Err(message) = app::validate_config(&config) {
                eprintln!("error: {message}");
                return ExitCode::FAILURE;
            }
            match service::run(command, &config) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("error: {error:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Config { action } => match config_command(action) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("error: {error:#}");
                ExitCode::FAILURE
            }
        },
        Command::Logs { lines, no_follow } => match logs_command(lines, no_follow) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("error: {error:#}");
                ExitCode::FAILURE
            }
        },
        Command::Tailnet { action, json } => {
            let config = state::load_config();
            if let Err(message) = app::validate_config(&config) {
                eprintln!("error: {message}");
                return ExitCode::FAILURE;
            }
            let report = tailnet::run_configured(
                action,
                &SystemRunner,
                find_tailscale(|path| path.exists()).as_deref(),
                config.port,
                &config.state_dir,
                &|name| std::env::var(name).ok(),
            );
            println!("{}", tailnet::render(&report, json));
            if report.error.is_none() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

/// `config`: the settings, the service and the token prefix; `config set NAME VALUE`: one setting.
fn config_command(action: Option<ConfigAction>) -> anyhow::Result<()> {
    let config = state::load_config();
    app::validate_config(&config).map_err(|message| anyhow::anyhow!(message))?;
    let paths = ServicePaths::detect()?;
    match action {
        None => {
            let (_, rows) =
                state::settings::resolve(&config.state_dir, &|name| std::env::var(name).ok())
                    .map_err(|message| anyhow::anyhow!(message))?;
            let token = service::read_token(&config.state_dir).unwrap_or_default();
            print!(
                "{}",
                service::render_config(&SystemRunner, &paths, &config.state_dir, &rows, &token)
            );
        }
        Some(ConfigAction::Set { name, value }) => {
            let message = service::config_set(&SystemRunner, &paths, &config, &name, &value)?;
            println!("{message}");
        }
    }
    Ok(())
}

/// `logs`: `tail` of the service's stdout and stderr files, followed unless `no_follow`.
fn logs_command(lines: usize, no_follow: bool) -> anyhow::Result<ExitCode> {
    let log_dir = ServicePaths::detect()?.log_dir;
    if !log_dir.join("relay.log").exists() && !log_dir.join("relay.err.log").exists() {
        eprintln!(
            "no log files yet in {}; the service writes them once it is installed \
             (conductor-remote service install)",
            log_dir.display()
        );
        return Ok(ExitCode::SUCCESS);
    }
    let mut tail = std::process::Command::new("tail");
    tail.arg("-n").arg(lines.to_string());
    if !no_follow {
        tail.arg("-F");
    }
    tail.arg(log_dir.join("relay.log"))
        .arg(log_dir.join("relay.err.log"));
    let status = tail.status().context("could not run `tail`")?;
    Ok(if status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn start(exit_with_parent: bool, parent_pid: Option<u32>) -> ExitCode {
    // First, so every line from here on reaches the phone's log too. The token is not known yet:
    // the redactor learns it as soon as it is loaded.
    let log_buffer = LogBuffer::new();
    let redactor = Redactor::new();
    tracing_subscriber::registry()
        .with(LevelFilter::INFO)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stdout)
                .with_ansi(std::io::stdout().is_terminal()),
        )
        .with(logbuf::layer(log_buffer.clone(), redactor.clone()))
        .init();

    // Early, so the parent noted is the one that started the relay. Without the flag nothing
    // watches the parent.
    let parent_gone = exit_with_parent.then(|| {
        parent::parent_gone(
            std::os::unix::process::parent_id,
            Duration::from_secs(1),
            parent_pid,
        )
    });

    // The settings are resolved once, here, and handed to whatever reads them.
    let state_dir = state::default_state_dir();
    let settings = match state::settings::resolve(&state_dir, &|name| std::env::var(name).ok()) {
        Ok((settings, _)) => settings,
        Err(message) => {
            tracing::error!("{message}");
            return ExitCode::FAILURE;
        }
    };
    let config = Config {
        port: settings.port,
        state_dir,
    };
    if let Err(message) = app::validate_config(&config) {
        tracing::error!("{message}");
        return ExitCode::FAILURE;
    }
    let token = match state::load_or_create_token(&config.state_dir) {
        Ok(token) => token,
        Err(error) => {
            tracing::error!("could not read or create the token: {error}");
            return ExitCode::FAILURE;
        }
    };
    redactor.set_token(token.expose());

    // Must run on the main thread: the main run loop below delivers its notifications.
    let conductor = lifecycle::start(CONDUCTOR_BUNDLE_ID);
    // Nothing is opened here: the database is read on the first request while Conductor runs.
    let home = state::home_dir();
    let log_dir = service::log_dir(&home);
    let paths = state::conductor_paths_from(
        settings
            .conductor_db
            .as_deref()
            .map(Path::to_string_lossy)
            .as_deref(),
        settings
            .conductor_workspaces
            .as_deref()
            .map(Path::to_string_lossy)
            .as_deref(),
        &home,
    );
    // Nothing runs at start either: the first request queues the first background refresh. A Mac
    // without `gh` or `git` simply shows no pull-request state or change statistics.
    let extras = Extras::new(std::sync::Arc::new(SystemCommands), home.clone());
    let preview_roots = PreviewRoots::system(&paths.workspaces_root, &home);
    // Public exposure is not supported: EXPOSE controls the Tailscale mapping, never the bind address.
    let expose = ExposeMode::Tailnet;
    // The plan allowances come from the CLIs Conductor bundles, else from the `PATH`; the tool
    // traffic is scanned from the database on request.
    let agent_binaries = home
        .join("Library")
        .join("Application Support")
        .join("com.conductor.app")
        .join("agent-binaries");
    let plan_usage = PlanUsageService::new(Arc::new(SystemProbe::new(agent_binaries)));
    let tool_usage = ToolUsageService::new(paths.db_path.clone());
    let mut reads = Reads::new(
        Arc::new(ConductorDb::new(paths.db_path.clone())),
        paths.workspaces_root,
    )
    .with_host_paths(HostPaths {
        home,
        state_dir: config.state_dir.clone(),
    })
    .with_extras(Arc::new(extras))
    .with_preview(preview_roots, expose)
    .with_usage(Arc::new(plan_usage), Arc::new(tool_usage));

    // The index lives next to the token. Without it, search finds workspaces by name only.
    let stop = Arc::new(AtomicBool::new(false));
    let mut indexer = None;
    match SearchIndex::open(&config.state_dir.join("search.db")) {
        Ok(index) => {
            let index = Arc::new(index);
            let running = conductor.clone();
            indexer = Some(spawn_indexer(
                index.clone(),
                paths.db_path,
                move || running.status().is_running(),
                stop.clone(),
            ));
            reads = reads.with_search(index);
        }
        Err(error) => tracing::error!("search runs without an index: {error}"),
    }
    let reads = Arc::new(reads);
    // The UI thread makes its own desktop: its elements never leave that thread.
    let ui = UiActor::spawn(|| Box::new(Driver::new(SystemDesktop::new())));
    // The relay's own database lives next to the token. Without it, prompts parked on a locked
    // Mac last until the relay restarts.
    let store = match open_store(&config) {
        Ok(store) => store,
        Err(error) => {
            tracing::error!("parked prompts last until restart: {error}");
            match Store::open_in_memory() {
                Ok(store) => store,
                Err(error) => {
                    tracing::error!("could not open the relay database in memory: {error}");
                    return ExitCode::FAILURE;
                }
            }
        }
    };
    let store = Arc::new(store);
    // Without a notifier the push routes answer 503 and the phone is never told.
    let notifier = match Notifier::new(
        store.clone(),
        reads.clone(),
        Arc::new(CurlSender::new(
            Arc::new(SystemCommands),
            config.state_dir.join("tmp"),
        )),
        NotifyConfig {
            enabled: settings.push_notify,
            subject: settings
                .push_subject
                .clone()
                .unwrap_or_else(|| DEFAULT_SUBJECT.to_owned()),
            ..NotifyConfig::from_env()
        },
    ) {
        Ok(notifier) => Some(notifier),
        Err(error) => {
            tracing::error!("push notifications are unavailable: {error}");
            None
        }
    };
    let parked = ParkedQueue::new(
        store.clone(),
        Arc::new(|| conductor_remote::ui::screen::session_state().map(|state| state.locked)),
        ParkedTimings::default(),
    );
    let writes = Writes::new(
        reads.clone(),
        ui.clone(),
        Arc::new(|| conductor_remote::ui::ax::is_trusted(false)),
        WriteTimings::default(),
        parked.clone(),
    )
    .configure(WriteDeps {
        state_dir: config.state_dir.clone(),
        store: store.clone(),
        commands: Arc::new(SystemCommands),
        locked: Arc::new(|| {
            conductor_remote::ui::screen::session_state().map(|state| state.locked)
        }),
    });
    let writes = Arc::new(writes);
    let dev = DevServers::new(DevDeps {
        reads: reads.clone(),
        ui: ui.clone(),
        commands: Arc::new(SystemCommands),
        home: state::home_dir(),
        state_dir: config.state_dir.clone(),
        tailscale: find_tailscale(|path| path.exists()),
    });
    // launchd names the job in the environment of what it starts.
    let launchd = std::env::var("XPC_SERVICE_NAME").is_ok_and(|name| name == APP_BUNDLE_ID);
    let host = Host::new(HostParts {
        logs: log_buffer,
        redactor,
        log_dir,
        nosleep: NoSleep::new(
            Arc::new(SystemSpawner::new()),
            settings.prevent_screen_lock,
            Arc::new(now_ms),
        ),
        app: Arc::new(SystemAppControl::new(conductor.clone())),
        reads: Some(reads.clone()),
        screen: Arc::new(|| {
            conductor_remote::ui::screen::session_state().map(|state| state.locked)
        }),
        managed: launchd,
        trusted: Arc::new(|| conductor_remote::ui::ax::is_trusted(false)),
        // The flag wins: the menu-bar app started this relay, whatever launchd says.
        supervisor: if exit_with_parent {
            Supervisor::App
        } else if launchd {
            Supervisor::Launchd
        } else {
            Supervisor::None
        },
        port: config.port,
        restart_timings: RestartTimings::default(),
        ui: Some(ui),
    });
    let services = Services {
        prefs: Some(Arc::new(Prefs::new(store.clone()))),
        host: Some(Arc::new(host)),
        dev: Some(dev.clone()),
    };
    let app_state = AppState {
        token: Arc::new(token),
        conductor,
        assets: http::embedded_assets(),
        reads: Some(reads),
        writes: Some(writes.clone()),
        notify: notifier
            .clone()
            .map(|notifier| notifier as Arc<dyn NotifyService>),
        services,
    };

    std::thread::spawn(move || {
        let served = run_server(
            config,
            app_state,
            writes,
            parked,
            notifier,
            dev,
            parent_gone,
        );
        let code = match served {
            Ok(()) => 0,
            Err(error) => {
                tracing::error!("{error:#}");
                1
            }
        };
        // The indexer may be in the middle of a commit: let it finish before the process ends.
        stop.store(true, Ordering::Relaxed);
        if let Some(indexer) = indexer {
            if indexer.join().is_err() {
                tracing::error!("the search indexer panicked");
            }
        }
        std::process::exit(code);
    });

    lifecycle::run_main_loop()
}

/// Creates the state directory (mode 0700 when created) and opens `relay.db` in it.
fn open_store(config: &Config) -> anyhow::Result<Store> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&config.state_dir)
        .with_context(|| format!("could not create {}", config.state_dir.display()))?;
    Ok(Store::open(&config.state_dir.join("relay.db"))?)
}

/// Milliseconds since the Unix epoch.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

/// Owns the tokio runtime; returns when the server has shut down or could not start.
fn run_server(
    config: Config,
    app_state: AppState,
    writes: Arc<Writes>,
    parked: Arc<ParkedQueue>,
    notifier: Option<Arc<Notifier>>,
    dev: Arc<DevServers>,
    parent_gone: Option<oneshot::Receiver<()>>,
) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("could not start the async runtime")?;
    runtime.block_on(async {
        let address = SocketAddr::from(([127, 0, 0, 1], config.port));
        let listener = TcpListener::bind(address)
            .await
            .with_context(|| format!("could not listen on port {}", config.port))?;
        if let Some(reads) = &app_state.reads {
            tokio::spawn(reads::close_when_not_running(
                reads.clone(),
                app_state.conductor.subscribe(),
            ));
        }
        // The ticker tells the phone when a turn ends; it idles while Conductor is not running.
        if let Some(notifier) = &notifier {
            let running = app_state.conductor.clone();
            notifier.start(Arc::new(move || running.status().is_running()));
        }
        // The pump sends parked prompts once the Mac is unlocked. The notice logs, and tells the
        // phone when there is a notifier.
        let first_prompts = writes.clone();
        let deliverer: Deliverer = Arc::new(move |row| writes.deliver_parked(row));
        let notice: Notice = Arc::new(move |row: &ParkedRow, error: Option<&str>| {
            match error {
                None => tracing::info!(session_id = %row.session_id, "a parked prompt was sent"),
                Some(error) => {
                    tracing::warn!(session_id = %row.session_id, %error, "a parked prompt failed");
                }
            }
            if let Some(notifier) = &notifier {
                notifier.notify_parked(row, error);
            }
        });
        parked.start(deliverer, notice, now_ms());
        // The pump sends the first prompts of new workspaces once Conductor has opened them.
        first_prompts.start_first_prompts();
        // The forwards of the last run come back first; then the sweeper closes the ones whose
        // server stopped listening.
        tokio::spawn(async move {
            dev.restore().await;
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
            loop {
                tick.tick().await;
                dev.sweep().await;
            }
        });
        let shutdown = shutdown_signal(parent_gone).context("could not install signal handlers")?;
        tracing::info!(
            "listening on {}, Conductor running: {}",
            listener.local_addr()?,
            app_state.conductor.status().is_running()
        );
        app::serve(listener, app_state, shutdown)
            .await
            .context("the server stopped")
    })
}

/// Resolves on SIGTERM, SIGINT, or `parent_gone` yielding: the process that started the relay is
/// gone. With `None`, or a channel that closed without yielding, that branch never resolves.
fn shutdown_signal(
    parent_gone: Option<oneshot::Receiver<()>>,
) -> std::io::Result<impl std::future::Future<Output = ()> + Send + 'static> {
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    Ok(async move {
        let parent_gone = async move {
            let gone = match parent_gone {
                Some(receiver) => receiver.await.is_ok(),
                None => false,
            };
            if !gone {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
            () = parent_gone => {
                tracing::info!("the process that started the relay is gone; stopping");
            }
        }
    })
}
