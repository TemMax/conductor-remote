//! The dev server of a workspace: its Run task, the ports it listens on and their forwards.

pub mod bridge;
pub mod controller;
pub mod ports;
pub mod run_configs;
pub mod store;
pub mod tailscale;

use serde::Serialize;

use crate::contract::Priority;
use crate::delivery::{BoxFuture, WriteAnswer};

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DevRunConfig {
    pub id: String,
    pub name: String,
    pub command: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DevServerForward {
    pub name: String,
    pub port: u16,
    pub running: bool,
    pub forwarded: bool,
    pub url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DevServerState {
    pub available: bool,
    pub running: bool,
    pub forwarded: bool,
    pub port: Option<u16>,
    pub url: Option<String>,
    pub forwards: Vec<DevServerForward>,
    pub run_configs: Vec<DevRunConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `DevServerState` flattened, plus `ok` and `changed`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DevServerResult {
    pub ok: bool,
    #[serde(flatten)]
    pub state: DevServerState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed: Option<bool>,
}

/// The dev-server routes' service. `controller::DevServers` is the real one.
pub trait DevServerService: Send + Sync + 'static {
    /// `GET /api/workspaces/:id/dev-server`.
    fn state(&self, workspace_id: String) -> BoxFuture<WriteAnswer>;
    /// `POST /api/workspaces/:id/dev-server` with `{runConfigId?}`.
    fn start(
        &self,
        workspace_id: String,
        run_config_id: Option<String>,
        priority: Priority,
    ) -> BoxFuture<WriteAnswer>;
    /// `DELETE /api/workspaces/:id/dev-server`.
    fn stop(&self, workspace_id: String, priority: Priority) -> BoxFuture<WriteAnswer>;
}
