//! The persisted record of which dev-server ports the relay forwarded: the receipts of its own
//! forwards, kept in `<state_dir>/dev-forwards.json`.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

use serde::{Deserialize, Serialize};

const FILE: &str = "dev-forwards.json";
const TEMPORARY: &str = "dev-forwards.json.tmp";

/// One port of a workspace's dev server published on the tailnet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Forward {
    pub workspace_id: String,
    /// The port the dev server listens on.
    pub target_port: u16,
    /// The HTTPS port `tailscale serve` publishes.
    pub serve_port: u16,
    /// The loopback port of the bridge that port is proxied to.
    pub bridge_port: u16,
    /// This Mac's tailnet name when the forward was made.
    pub host: String,
    /// The secret that proves the bridge on `bridge_port` is the relay's.
    pub bridge_token: String,
}

/// The stored forwards; a missing or broken file is none.
pub fn load(state_dir: &Path) -> Vec<Forward> {
    fs::read(state_dir.join(FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Replaces the file: the forwards are written to a temporary file (mode 0600), which is then
/// renamed over it.
pub fn save(state_dir: &Path, forwards: &[Forward]) -> io::Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(state_dir)?;
    let temporary = state_dir.join(TEMPORARY);
    // A leftover of an interrupted save may carry any mode; replace it rather than reuse it.
    match fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec(forwards)?)?;
    file.sync_all()?;
    fs::rename(&temporary, state_dir.join(FILE))
}
