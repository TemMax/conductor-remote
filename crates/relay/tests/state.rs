use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use conductor_remote::contract::{APP_BUNDLE_ID, DEFAULT_PORT};
use conductor_remote::state::{
    conductor_paths_from, config_from, state_dir_from, token_from, ConductorPaths,
};

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn is_hex32(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[test]
fn default_dir_is_under_home() {
    let dir = state_dir_from(None, Path::new("/Users/x"));
    assert_eq!(
        dir,
        Path::new("/Users/x/Library/Application Support").join(APP_BUNDLE_ID)
    );
}

#[test]
fn empty_override_is_ignored() {
    let dir = state_dir_from(Some(""), Path::new("/Users/x"));
    assert!(dir.starts_with("/Users/x/Library/Application Support"));
}

#[test]
fn state_dir_override_wins() {
    let config = config_from(None, Some("/somewhere/else"), Path::new("/Users/x"));
    assert_eq!(config.state_dir, Path::new("/somewhere/else"));
}

#[test]
fn port_defaults_and_overrides() {
    let home = Path::new("/h");
    assert_eq!(config_from(None, None, home).port, DEFAULT_PORT);
    assert_eq!(config_from(Some("9000"), None, home).port, 9000);
    assert_eq!(config_from(Some("65535"), None, home).port, 65535);
    assert_eq!(config_from(Some("1"), None, home).port, 1);
}

#[test]
fn invalid_ports_fall_back() {
    let home = Path::new("/h");
    for bad in ["0", "70000", "abc", ""] {
        assert_eq!(
            config_from(Some(bad), None, home).port,
            DEFAULT_PORT,
            "{bad:?}"
        );
    }
}

#[test]
fn minted_token_is_32_lowercase_hex() {
    let tmp = tempfile::tempdir().unwrap();
    let token = token_from(None, &tmp.path().join("state")).unwrap();
    assert!(is_hex32(token.expose()), "{}", token.expose());
}

#[test]
fn two_mints_differ() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let ta = token_from(None, a.path()).unwrap();
    let tb = token_from(None, b.path()).unwrap();
    assert_ne!(ta.expose(), tb.expose());
}

#[test]
fn modes_are_private() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("a").join("state");
    token_from(None, &dir).unwrap();
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join("token")), 0o600);
}

#[test]
fn second_call_returns_same_token() {
    let tmp = tempfile::tempdir().unwrap();
    let first = token_from(None, tmp.path()).unwrap();
    let second = token_from(None, tmp.path()).unwrap();
    assert_eq!(first.expose(), second.expose());
}

#[test]
fn whitespace_in_file_is_trimmed() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("token"), "  abc123\n").unwrap();
    let token = token_from(None, tmp.path()).unwrap();
    assert_eq!(token.expose(), "abc123");
}

#[test]
fn explicit_token_wins_and_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("state");
    let token = token_from(Some("given"), &dir).unwrap();
    assert_eq!(token.expose(), "given");
    assert!(!dir.exists());
}

#[test]
fn empty_explicit_token_is_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let token = token_from(Some(""), tmp.path()).unwrap();
    assert!(is_hex32(token.expose()));
}

#[test]
fn empty_file_mints_a_new_token() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("token");
    fs::write(&path, "  \n").unwrap();
    let token = token_from(None, tmp.path()).unwrap();
    assert!(is_hex32(token.expose()));
    assert_eq!(fs::read_to_string(&path).unwrap(), token.expose());
    assert_eq!(mode(&path), 0o600);
}

#[test]
fn unreadable_file_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    // A directory named `token` cannot be read as a file.
    fs::create_dir(tmp.path().join("token")).unwrap();
    assert!(token_from(None, tmp.path()).is_err());
}

#[test]
fn conductor_paths_default_under_home() {
    let paths = conductor_paths_from(None, None, Path::new("/Users/x"));
    assert_eq!(
        paths,
        ConductorPaths {
            db_path: Path::new(
                "/Users/x/Library/Application Support/com.conductor.app/conductor.db"
            )
            .to_path_buf(),
            workspaces_root: Path::new("/Users/x/conductor/workspaces").to_path_buf(),
        }
    );
}

#[test]
fn conductor_paths_overrides_win_independently() {
    let home = Path::new("/Users/x");
    let both = conductor_paths_from(Some("/a/b.db"), Some("/w"), home);
    assert_eq!(both.db_path, Path::new("/a/b.db"));
    assert_eq!(both.workspaces_root, Path::new("/w"));

    let db_only = conductor_paths_from(Some("/a/b.db"), None, home);
    assert_eq!(db_only.db_path, Path::new("/a/b.db"));
    assert_eq!(
        db_only.workspaces_root,
        Path::new("/Users/x/conductor/workspaces")
    );

    let root_only = conductor_paths_from(None, Some("/w"), home);
    assert!(root_only
        .db_path
        .starts_with("/Users/x/Library/Application Support"));
    assert_eq!(root_only.workspaces_root, Path::new("/w"));
}

#[test]
fn empty_conductor_path_overrides_are_ignored() {
    let paths = conductor_paths_from(Some(""), Some(""), Path::new("/Users/x"));
    assert_eq!(
        paths,
        conductor_paths_from(None, None, Path::new("/Users/x"))
    );
}
