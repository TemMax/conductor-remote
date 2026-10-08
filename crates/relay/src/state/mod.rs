//! The relay's own files: state directory, token, configuration.

pub mod prefs;
pub mod settings;
pub mod store;

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::contract::{Config, Token, APP_BUNDLE_ID, DEFAULT_PORT};

const TOKEN_FILE: &str = "token";

/// Where the relay keeps its files.
pub fn default_state_dir() -> PathBuf {
    state_dir_from(
        std::env::var("CONDUCTOR_REMOTE_STATE_DIR").ok().as_deref(),
        &home_dir(),
    )
}

/// Port and state directory. The port is resolved with the other settings (environment, then
/// `settings.json`, then the default); when they cannot be resolved the default port is used.
pub fn load_config() -> Config {
    let state_dir = default_state_dir();
    let port = match settings::resolve(&state_dir, &|name| std::env::var(name).ok()) {
        Ok((settings, _)) => settings.port,
        Err(error) => {
            tracing::warn!("{error}; using port {DEFAULT_PORT}");
            DEFAULT_PORT
        }
    };
    Config { port, state_dir }
}

/// The persisted token, minted on first use.
pub fn load_or_create_token(state_dir: &Path) -> io::Result<Token> {
    token_from(std::env::var("RELAY_TOKEN").ok().as_deref(), state_dir)
}

/// The user's home directory: `$HOME`, or an empty path when it is not set.
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// The override when it is non-empty, otherwise the app's directory under `home`.
pub fn state_dir_from(state_dir: Option<&str>, home: &Path) -> PathBuf {
    match state_dir {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home
            .join("Library")
            .join("Application Support")
            .join(APP_BUNDLE_ID),
    }
}

/// `port` is used only when it is an integer from 1 to 65535.
pub fn config_from(port: Option<&str>, state_dir: Option<&str>, home: &Path) -> Config {
    let port = port
        .and_then(|p| p.parse::<u16>().ok())
        .filter(|p| *p != 0)
        .unwrap_or(DEFAULT_PORT);
    Config {
        port,
        state_dir: state_dir_from(state_dir, home),
    }
}

/// Where Conductor keeps its database and its workspace checkouts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConductorPaths {
    pub db_path: PathBuf,
    pub workspaces_root: PathBuf,
}

/// `db` and `workspaces` win when non-empty; otherwise the defaults under `home`.
pub fn conductor_paths_from(
    db: Option<&str>,
    workspaces: Option<&str>,
    home: &Path,
) -> ConductorPaths {
    let non_empty = |value: Option<&str>| value.filter(|v| !v.is_empty()).map(PathBuf::from);
    ConductorPaths {
        db_path: non_empty(db).unwrap_or_else(|| {
            home.join("Library")
                .join("Application Support")
                .join("com.conductor.app")
                .join("conductor.db")
        }),
        workspaces_root: non_empty(workspaces)
            .unwrap_or_else(|| home.join("conductor").join("workspaces")),
    }
}

/// From `CONDUCTOR_DB` and `CONDUCTOR_WORKSPACES`.
pub fn load_conductor_paths() -> ConductorPaths {
    conductor_paths_from(
        std::env::var("CONDUCTOR_DB").ok().as_deref(),
        std::env::var("CONDUCTOR_WORKSPACES").ok().as_deref(),
        &home_dir(),
    )
}

/// An explicit non-empty token wins and writes nothing; otherwise the file in `dir`, otherwise
/// a freshly minted token persisted there.
pub fn token_from(explicit: Option<&str>, dir: &Path) -> io::Result<Token> {
    if let Some(token) = explicit.filter(|t| !t.is_empty()) {
        return Ok(Token::new(token));
    }
    let path = dir.join(TOKEN_FILE);
    if let Some(existing) = read_token_file(&path)? {
        return Ok(Token::new(existing));
    }

    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let token = mint()?;
    // An existing empty file may carry any mode; replace it rather than reuse it.
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(token.as_bytes())?;
            file.sync_all()?;
            Ok(Token::new(token))
        }
        // Another process minted one between our read and our create: use theirs.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => match read_token_file(&path)? {
            Some(existing) => Ok(Token::new(existing)),
            None => Err(e),
        },
        Err(e) => Err(e),
    }
}

fn read_token_file(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(contents) => {
            let trimmed = contents.trim();
            Ok((!trimmed.is_empty()).then(|| trimmed.to_owned()))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// 16 bytes from the operating system's random source, as 32 lowercase hex characters.
fn mint() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
