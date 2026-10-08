use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use conductor_remote::state::settings::{resolve, set, Expose, Settings, Source, NAMES};

fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    let map: HashMap<&str, &str> = pairs.iter().copied().collect();
    move |name| map.get(name).map(|value| (*value).to_owned())
}

fn write_settings(dir: &Path, json: &str) {
    fs::write(dir.join("settings.json"), json).unwrap();
}

fn resolved(dir: &Path, env: &[(&str, &str)]) -> Settings {
    resolve(dir, &env_of(env)).unwrap().0
}

fn source_of(dir: &Path, env: &[(&str, &str)], name: &str) -> Source {
    let (_, rows) = resolve(dir, &env_of(env)).unwrap();
    rows.iter().find(|(row, _, _)| *row == name).unwrap().2
}

fn error_of(dir: &Path, env: &[(&str, &str)]) -> String {
    resolve(dir, &env_of(env)).unwrap_err()
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn defaults_with_no_file_and_no_environment() {
    let tmp = tempfile::tempdir().unwrap();
    let (settings, rows) = resolve(tmp.path(), &env_of(&[])).unwrap();
    assert_eq!(
        settings,
        Settings {
            port: 8790,
            expose: Expose::Tailnet,
            prevent_screen_lock: false,
            push_notify: true,
            push_subject: None,
            conductor_db: None,
            conductor_workspaces: None,
        }
    );
    let names: Vec<&str> = rows.iter().map(|(name, _, _)| *name).collect();
    assert_eq!(names, NAMES);
    assert!(rows.iter().all(|(_, _, source)| *source == Source::Default));
    let shown: Vec<&str> = rows.iter().map(|(_, value, _)| value.as_str()).collect();
    assert_eq!(shown, ["8790", "tailnet", "off", "on", "", "", ""]);
}

#[test]
fn resolving_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("state");
    resolve(&dir, &env_of(&[])).unwrap();
    assert!(!dir.exists());
}

#[test]
fn every_setting_from_the_environment() {
    let tmp = tempfile::tempdir().unwrap();
    let env = [
        ("RELAY_PORT", "9000"),
        ("EXPOSE", "off"),
        ("PREVENT_SCREEN_LOCK", "on"),
        ("PUSH_NOTIFY", "off"),
        ("PUSH_SUBJECT", "mailto:me@example.com"),
        ("CONDUCTOR_DB", "/env/conductor.db"),
        ("CONDUCTOR_WORKSPACES", "/env/workspaces"),
    ];
    let (settings, rows) = resolve(tmp.path(), &env_of(&env)).unwrap();
    assert_eq!(
        settings,
        Settings {
            port: 9000,
            expose: Expose::Off,
            prevent_screen_lock: true,
            push_notify: false,
            push_subject: Some("mailto:me@example.com".to_owned()),
            conductor_db: Some(PathBuf::from("/env/conductor.db")),
            conductor_workspaces: Some(PathBuf::from("/env/workspaces")),
        }
    );
    assert!(rows
        .iter()
        .all(|(_, _, source)| *source == Source::Environment));
}

#[test]
fn every_setting_from_the_file() {
    let tmp = tempfile::tempdir().unwrap();
    write_settings(
        tmp.path(),
        r#"{
            "RELAY_PORT": "9001",
            "EXPOSE": "off",
            "PREVENT_SCREEN_LOCK": "on",
            "PUSH_NOTIFY": "false",
            "PUSH_SUBJECT": "https://example.com/me",
            "CONDUCTOR_DB": "/file/conductor.db",
            "CONDUCTOR_WORKSPACES": "/file/workspaces"
        }"#,
    );
    let (settings, rows) = resolve(tmp.path(), &env_of(&[])).unwrap();
    assert_eq!(
        settings,
        Settings {
            port: 9001,
            expose: Expose::Off,
            prevent_screen_lock: true,
            push_notify: false,
            push_subject: Some("https://example.com/me".to_owned()),
            conductor_db: Some(PathBuf::from("/file/conductor.db")),
            conductor_workspaces: Some(PathBuf::from("/file/workspaces")),
        }
    );
    assert!(rows.iter().all(|(_, _, source)| *source == Source::File));
}

#[test]
fn the_file_may_hold_numbers_and_booleans_written_by_hand() {
    let tmp = tempfile::tempdir().unwrap();
    write_settings(tmp.path(), r#"{"RELAY_PORT": 9002, "PUSH_NOTIFY": false}"#);
    let settings = resolved(tmp.path(), &[]);
    assert_eq!(settings.port, 9002);
    assert!(!settings.push_notify);
}

#[test]
fn environment_beats_file_beats_default() {
    let tmp = tempfile::tempdir().unwrap();
    write_settings(
        tmp.path(),
        r#"{"RELAY_PORT": "9001", "EXPOSE": "off", "CONDUCTOR_DB": "/file.db"}"#,
    );
    let env = [("RELAY_PORT", "9000"), ("CONDUCTOR_DB", "/env.db")];
    let (settings, _) = resolve(tmp.path(), &env_of(&env)).unwrap();
    assert_eq!(settings.port, 9000);
    assert_eq!(settings.conductor_db, Some(PathBuf::from("/env.db")));
    assert_eq!(settings.expose, Expose::Off);
    assert!(settings.push_notify);

    assert_eq!(
        source_of(tmp.path(), &env, "RELAY_PORT"),
        Source::Environment
    );
    assert_eq!(source_of(tmp.path(), &env, "EXPOSE"), Source::File);
    assert_eq!(source_of(tmp.path(), &env, "PUSH_NOTIFY"), Source::Default);
}

#[test]
fn empty_environment_value_counts_as_unset() {
    let tmp = tempfile::tempdir().unwrap();
    write_settings(tmp.path(), r#"{"RELAY_PORT": "9001"}"#);
    let env = [("RELAY_PORT", ""), ("EXPOSE", ""), ("PUSH_NOTIFY", "")];
    assert_eq!(resolved(tmp.path(), &env).port, 9001);
    assert_eq!(source_of(tmp.path(), &env, "RELAY_PORT"), Source::File);
    assert_eq!(source_of(tmp.path(), &env, "EXPOSE"), Source::Default);
    assert!(resolved(tmp.path(), &env).push_notify);
}

#[test]
fn empty_file_value_counts_as_unset() {
    let tmp = tempfile::tempdir().unwrap();
    write_settings(tmp.path(), r#"{"RELAY_PORT": "", "PUSH_SUBJECT": ""}"#);
    let (settings, rows) = resolve(tmp.path(), &env_of(&[])).unwrap();
    assert_eq!(settings.port, 8790);
    assert_eq!(settings.push_subject, None);
    assert!(rows.iter().all(|(_, _, source)| *source == Source::Default));
}

#[test]
fn values_are_trimmed_and_enums_are_case_insensitive() {
    let tmp = tempfile::tempdir().unwrap();
    let env = [
        ("RELAY_PORT", " 9000 "),
        ("EXPOSE", " OFF "),
        ("PREVENT_SCREEN_LOCK", "On"),
        ("PUSH_NOTIFY", " FALSE "),
        ("PUSH_SUBJECT", "  mailto:me@example.com "),
    ];
    let settings = resolved(tmp.path(), &env);
    assert_eq!(settings.port, 9000);
    assert_eq!(settings.expose, Expose::Off);
    assert!(settings.prevent_screen_lock);
    assert!(!settings.push_notify);
    assert_eq!(
        settings.push_subject.as_deref(),
        Some("mailto:me@example.com")
    );
}

#[test]
fn push_notify_accepts_every_spelling() {
    let tmp = tempfile::tempdir().unwrap();
    for off in ["off", "false", "0"] {
        let env = [("PUSH_NOTIFY", off)];
        assert!(!resolved(tmp.path(), &env).push_notify, "{off}");
    }
    for on in ["on", "true", "1"] {
        let env = [("PUSH_NOTIFY", on)];
        assert!(resolved(tmp.path(), &env).push_notify, "{on}");
    }
}

#[test]
fn invalid_port_is_an_error_naming_the_setting() {
    let tmp = tempfile::tempdir().unwrap();
    for bad in ["0", "70000", "-1", "abc", "80.5"] {
        let error = error_of(tmp.path(), &[("RELAY_PORT", bad)]);
        assert!(error.contains("RELAY_PORT"), "{bad}: {error}");
        assert!(error.contains("1 to 65535"), "{bad}: {error}");
    }
}

#[test]
fn port_bounds_are_accepted() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(resolved(tmp.path(), &[("RELAY_PORT", "1")]).port, 1);
    assert_eq!(resolved(tmp.path(), &[("RELAY_PORT", "65535")]).port, 65535);
}

#[test]
fn invalid_expose_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let error = error_of(tmp.path(), &[("EXPOSE", "lan")]);
    assert!(error.contains("EXPOSE"), "{error}");
    assert!(error.contains("tailnet or off"), "{error}");
}

#[test]
fn public_expose_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    for value in ["public", "PUBLIC", "funnel"] {
        let error = error_of(tmp.path(), &[("EXPOSE", value)]);
        assert!(error.contains("EXPOSE"), "{error}");
        assert!(
            error.contains(
                "public mode is not supported; the relay is reachable on your tailnet only"
            ),
            "{error}"
        );
    }
    write_settings(tmp.path(), r#"{"EXPOSE": "public"}"#);
    assert!(error_of(tmp.path(), &[]).contains("public mode is not supported"));
}

#[test]
fn invalid_booleans_are_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let error = error_of(tmp.path(), &[("PREVENT_SCREEN_LOCK", "yes")]);
    assert!(error.contains("PREVENT_SCREEN_LOCK"), "{error}");
    assert!(error.contains("on or off"), "{error}");
    let error = error_of(tmp.path(), &[("PUSH_NOTIFY", "maybe")]);
    assert!(error.contains("PUSH_NOTIFY"), "{error}");
}

#[test]
fn invalid_environment_value_does_not_fall_through_to_the_file() {
    let tmp = tempfile::tempdir().unwrap();
    write_settings(tmp.path(), r#"{"RELAY_PORT": "9001"}"#);
    let error = error_of(tmp.path(), &[("RELAY_PORT", "nope")]);
    assert!(error.contains("RELAY_PORT"), "{error}");
}

#[test]
fn invalid_file_value_is_an_error_naming_the_setting_and_the_file() {
    let tmp = tempfile::tempdir().unwrap();
    write_settings(tmp.path(), r#"{"RELAY_PORT": "70000"}"#);
    let error = error_of(tmp.path(), &[]);
    assert!(error.contains("RELAY_PORT"), "{error}");
    assert!(error.contains("settings.json"), "{error}");
    // A valid environment value still wins over an invalid file value.
    assert_eq!(resolved(tmp.path(), &[("RELAY_PORT", "9000")]).port, 9000);
}

#[test]
fn file_value_of_the_wrong_type_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    write_settings(tmp.path(), r#"{"PUSH_SUBJECT": ["a"]}"#);
    let error = error_of(tmp.path(), &[]);
    assert!(error.contains("PUSH_SUBJECT"), "{error}");
}

#[test]
fn malformed_file_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    write_settings(tmp.path(), "{not json");
    let error = error_of(tmp.path(), &[]);
    assert!(error.contains("settings.json"), "{error}");
    write_settings(tmp.path(), "[1, 2]");
    let error = error_of(tmp.path(), &[]);
    assert!(error.contains("settings.json"), "{error}");
}

#[test]
fn unreadable_file_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    // A directory named `settings.json` cannot be read as a file.
    fs::create_dir(tmp.path().join("settings.json")).unwrap();
    let error = error_of(tmp.path(), &[]);
    assert!(error.contains("could not read"), "{error}");
    assert!(error.contains("settings.json"), "{error}");
}

#[test]
fn unknown_keys_in_the_file_are_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    write_settings(
        tmp.path(),
        r#"{"SOMETHING_ELSE": "x", "RELAY_PORT": "9001"}"#,
    );
    assert_eq!(resolved(tmp.path(), &[]).port, 9001);
}

#[test]
fn set_writes_a_normalized_value_that_resolve_reads_back() {
    let tmp = tempfile::tempdir().unwrap();
    set(tmp.path(), "RELAY_PORT", " 9100 ").unwrap();
    set(tmp.path(), "EXPOSE", "OFF").unwrap();
    set(tmp.path(), "PUSH_NOTIFY", "false").unwrap();
    set(tmp.path(), "CONDUCTOR_DB", "/x/conductor.db").unwrap();

    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(tmp.path().join("settings.json")).unwrap())
            .unwrap();
    assert_eq!(value["RELAY_PORT"], "9100");
    assert_eq!(value["EXPOSE"], "off");
    assert_eq!(value["PUSH_NOTIFY"], "off");

    let (settings, _) = resolve(tmp.path(), &env_of(&[])).unwrap();
    assert_eq!(settings.port, 9100);
    assert_eq!(settings.expose, Expose::Off);
    assert!(!settings.push_notify);
    assert_eq!(
        settings.conductor_db,
        Some(PathBuf::from("/x/conductor.db"))
    );
}

#[test]
fn set_creates_the_state_directory_privately() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("a").join("state");
    set(&dir, "RELAY_PORT", "9100").unwrap();
    assert_eq!(mode(&dir), 0o700);
}

#[test]
fn set_keeps_other_settings_and_unknown_keys() {
    let tmp = tempfile::tempdir().unwrap();
    write_settings(tmp.path(), r#"{"KEEP": "me", "RELAY_PORT": "9001"}"#);
    set(tmp.path(), "EXPOSE", "off").unwrap();
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(tmp.path().join("settings.json")).unwrap())
            .unwrap();
    assert_eq!(value["KEEP"], "me");
    assert_eq!(value["RELAY_PORT"], "9001");
    assert_eq!(value["EXPOSE"], "off");
}

#[test]
fn set_with_an_empty_value_removes_the_setting() {
    let tmp = tempfile::tempdir().unwrap();
    set(tmp.path(), "RELAY_PORT", "9100").unwrap();
    set(tmp.path(), "EXPOSE", "off").unwrap();
    set(tmp.path(), "RELAY_PORT", "").unwrap();

    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(tmp.path().join("settings.json")).unwrap())
            .unwrap();
    assert!(value.get("RELAY_PORT").is_none());
    assert_eq!(value["EXPOSE"], "off");
    assert_eq!(resolved(tmp.path(), &[]).port, 8790);
    assert_eq!(source_of(tmp.path(), &[], "RELAY_PORT"), Source::Default);
}

#[test]
fn removing_a_setting_that_is_not_there_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("state");
    set(&dir, "RELAY_PORT", "").unwrap();
    assert!(!dir.exists());
}

#[test]
fn set_validates_like_resolve() {
    let tmp = tempfile::tempdir().unwrap();
    let error = set(tmp.path(), "RELAY_PORT", "0").unwrap_err();
    assert!(
        error.contains("RELAY_PORT") && error.contains("1 to 65535"),
        "{error}"
    );
    let error = set(tmp.path(), "EXPOSE", "public").unwrap_err();
    assert!(
        error.contains("public mode is not supported; the relay is reachable on your tailnet only"),
        "{error}"
    );
    let error = set(tmp.path(), "EXPOSE", "lan").unwrap_err();
    assert!(
        error.contains("EXPOSE") && error.contains("tailnet or off"),
        "{error}"
    );
    let error = set(tmp.path(), "PREVENT_SCREEN_LOCK", "yes").unwrap_err();
    assert!(
        error.contains("PREVENT_SCREEN_LOCK") && error.contains("on or off"),
        "{error}"
    );
    let error = set(tmp.path(), "PUSH_NOTIFY", "maybe").unwrap_err();
    assert!(error.contains("PUSH_NOTIFY"), "{error}");
    // Nothing invalid reached the disk.
    assert!(!tmp.path().join("settings.json").exists());
}

#[test]
fn set_refuses_an_unknown_name() {
    let tmp = tempfile::tempdir().unwrap();
    let error = set(tmp.path(), "NOPE", "1").unwrap_err();
    assert!(error.contains("NOPE"), "{error}");
    assert!(!tmp.path().join("settings.json").exists());
}

#[test]
fn set_file_mode_is_private() {
    let tmp = tempfile::tempdir().unwrap();
    set(tmp.path(), "RELAY_PORT", "9100").unwrap();
    let path = tmp.path().join("settings.json");
    assert_eq!(mode(&path), 0o600);
    // A looser file is replaced by a private one.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    set(tmp.path(), "EXPOSE", "off").unwrap();
    assert_eq!(mode(&path), 0o600);
}

#[test]
fn set_leaves_no_temporary_file_behind() {
    let tmp = tempfile::tempdir().unwrap();
    set(tmp.path(), "RELAY_PORT", "9100").unwrap();
    set(tmp.path(), "RELAY_PORT", "9101").unwrap();
    let names: Vec<String> = fs::read_dir(tmp.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["settings.json"]);
}

#[test]
fn set_does_not_overwrite_a_malformed_file() {
    let tmp = tempfile::tempdir().unwrap();
    write_settings(tmp.path(), "{not json");
    let error = set(tmp.path(), "RELAY_PORT", "9100").unwrap_err();
    assert!(error.contains("settings.json"), "{error}");
    assert_eq!(
        fs::read_to_string(tmp.path().join("settings.json")).unwrap(),
        "{not json"
    );
}

#[test]
fn set_on_an_unreadable_file_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join("settings.json")).unwrap();
    let error = set(tmp.path(), "RELAY_PORT", "9100").unwrap_err();
    assert!(error.contains("could not read"), "{error}");
}
