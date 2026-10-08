use std::path::PathBuf;

use conductor_remote::app::validate_config;
use conductor_remote::contract::{Config, DEFAULT_PORT};

fn config(dir: &str) -> Config {
    Config {
        port: DEFAULT_PORT,
        state_dir: PathBuf::from(dir),
    }
}

#[test]
fn absolute_state_dir_is_accepted() {
    assert_eq!(validate_config(&config("/Users/x/state")), Ok(()));
}

#[test]
fn relative_state_dir_is_rejected() {
    let message = validate_config(&config("relative/state")).unwrap_err();
    assert!(message.contains("relative/state"), "{message}");
    assert!(message.contains("HOME"), "{message}");
}
