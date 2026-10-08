//! Serves the router on a listener.

use crate::contract::{AppState, Config};
use crate::http;

/// Serve the relay on an already bound listener until `shutdown` resolves.
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: AppState,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, http::router(state))
        .with_graceful_shutdown(shutdown)
        .await
}

/// A relative state directory means HOME was not set when the configuration was read.
pub fn validate_config(config: &Config) -> Result<(), String> {
    if config.state_dir.is_absolute() {
        Ok(())
    } else {
        Err(format!(
            "the state directory `{}` is not absolute; is HOME set?",
            config.state_dir.display()
        ))
    }
}
