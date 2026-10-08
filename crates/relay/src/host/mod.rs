//! The host: the relay's log, keep-awake, Conductor restart, and the settings the phone reads.

pub mod logbuf;
pub mod nosleep;
pub mod parent;
pub mod restart;
pub mod service;

use crate::delivery::{BoxFuture, WriteAnswer};

/// What the host routes call. `service::Host` is the real one.
pub trait HostService: Send + Sync + 'static {
    /// `GET /api/logs?file=&limit=`: the phone's `LogsResponse` (status 200), or 404 for an unknown file.
    fn logs(&self, file: Option<String>, limit: Option<usize>) -> WriteAnswer;
    /// `GET /api/settings`: `{"settings":{},"nosleep":<NoSleepState>,"screenLocked":bool|null}`.
    fn settings(&self) -> WriteAnswer;
    /// `GET /api/nosleep`.
    fn nosleep(&self) -> WriteAnswer;
    /// `POST /api/nosleep` with `{seconds}`.
    fn arm_nosleep(&self, seconds: u64) -> WriteAnswer;
    /// `DELETE /api/nosleep`.
    fn disarm_nosleep(&self) -> WriteAnswer;
    /// `POST /api/conductor/restart` with `{stopAgents?}`.
    fn restart_conductor(&self, stop_agents: bool) -> BoxFuture<WriteAnswer>;
    /// `GET /api/host/status`: what a local supervisor shows about the relay. Default: 501.
    fn status(&self) -> WriteAnswer {
        WriteAnswer::error(501, "not implemented")
    }
    /// Notes that the phone or another client just used the API; the status reports how long
    /// ago. Default: nothing.
    fn note_request(&self) {}
}
