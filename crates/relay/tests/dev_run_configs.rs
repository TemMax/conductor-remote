//! Reading a repository's Run scripts from `.conductor/settings.toml` and `settings.local.toml`.

use std::fs;
use std::path::Path;

use conductor_remote::dev::run_configs::{display_name, run_configs};
use conductor_remote::dev::DevRunConfig;
use tempfile::TempDir;

fn repo(shared: Option<&str>, local: Option<&str>) -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join(".conductor")).unwrap();
    if let Some(shared) = shared {
        fs::write(dir.path().join(".conductor/settings.toml"), shared).unwrap();
    }
    if let Some(local) = local {
        fs::write(dir.path().join(".conductor/settings.local.toml"), local).unwrap();
    }
    dir
}

fn pairs(root: &Path) -> Vec<(String, String)> {
    run_configs(root)
        .into_iter()
        .map(|config| (config.id, config.command))
        .collect()
}

fn pair(id: &str, command: &str) -> (String, String) {
    (id.to_owned(), command.to_owned())
}

#[test]
fn no_files_give_no_configs() {
    let dir = TempDir::new().unwrap();
    assert!(run_configs(dir.path()).is_empty());
    let dir = repo(None, None);
    assert!(run_configs(dir.path()).is_empty());
}

#[test]
fn one_shared_config() {
    let dir = repo(
        Some("[scripts.run.api-docs]\ncommand = \"npm run docs\"\n"),
        None,
    );
    assert_eq!(
        run_configs(dir.path()),
        vec![DevRunConfig {
            id: "api-docs".to_owned(),
            name: "Api docs".to_owned(),
            command: "npm run docs".to_owned(),
        }]
    );
}

#[test]
fn two_configs_keep_file_order() {
    let dir = repo(
        Some(
            "[scripts.run.web]\ncommand = \"vite\"\n\n[other]\nx = 1\n\n[scripts.run.api]\ncommand = \"cargo run\"\n",
        ),
        None,
    );
    assert_eq!(
        pairs(dir.path()),
        vec![pair("web", "vite"), pair("api", "cargo run")]
    );
}

#[test]
fn local_overrides_a_command() {
    let dir = repo(
        Some("[scripts.run.web]\ncommand = \"vite\"\n[scripts.run.api]\ncommand = \"cargo run\"\n"),
        Some("[scripts.run.web]\ncommand = \"vite --host\"\n"),
    );
    assert_eq!(
        pairs(dir.path()),
        vec![pair("web", "vite --host"), pair("api", "cargo run")]
    );
}

#[test]
fn local_adds_an_id_after_the_shared_ones() {
    let dir = repo(
        Some("[scripts.run.web]\ncommand = \"vite\"\n[scripts.run.api]\ncommand = \"cargo run\"\n"),
        Some("[scripts.run.docs]\ncommand = \"mdbook serve\"\n[scripts.run.app]\ncommand = \"expo\"\n"),
    );
    assert_eq!(
        pairs(dir.path()),
        vec![
            pair("web", "vite"),
            pair("api", "cargo run"),
            pair("docs", "mdbook serve"),
            pair("app", "expo"),
        ]
    );
}

#[test]
fn hide_in_local_hides_a_shared_config() {
    let dir = repo(
        Some("[scripts.run.web]\ncommand = \"vite\"\n[scripts.run.api]\ncommand = \"cargo run\"\n"),
        Some("[scripts.run.web]\ncommand = \"vite\"\nhide = true\n"),
    );
    assert_eq!(pairs(dir.path()), vec![pair("api", "cargo run")]);
}

#[test]
fn hide_in_the_shared_file_drops_the_config() {
    let dir = repo(
        Some("[scripts.run.web]\ncommand = \"vite\"\nhide = true # not now\n"),
        None,
    );
    assert!(run_configs(dir.path()).is_empty());
}

#[test]
fn local_without_hide_shows_a_config_the_shared_file_hides() {
    let dir = repo(
        Some("[scripts.run.web]\ncommand = \"vite\"\nhide = true\n"),
        Some("[scripts.run.web]\ncommand = \"vite --open\"\n"),
    );
    assert_eq!(pairs(dir.path()), vec![pair("web", "vite --open")]);
}

#[test]
fn quoted_ids() {
    let dir = repo(
        Some(
            "[scripts.run.\"web\"]\ncommand = \"a\"\n[ scripts . run . 'api-docs' ]\ncommand = \"b\"\n[scripts.run.\"x.y\"]\ncommand = \"c\"\n",
        ),
        None,
    );
    assert_eq!(
        pairs(dir.path()),
        vec![pair("web", "a"), pair("api-docs", "b"), pair("x.y", "c")]
    );
}

#[test]
fn single_quoted_and_escaped_commands() {
    let dir = repo(
        Some(
            "[scripts.run.a]\ncommand = 'echo \"hi\" \\ there'\n[scripts.run.b]\ncommand = \"echo \\\"hi\\\" \\\\ there\"\n",
        ),
        None,
    );
    assert_eq!(
        pairs(dir.path()),
        vec![
            pair("a", "echo \"hi\" \\ there"),
            pair("b", "echo \"hi\" \\ there")
        ]
    );
}

#[test]
fn a_comment_after_the_value() {
    let dir = repo(
        Some("[scripts.run.web] # the site\ncommand = \"vite # not a comment\" # a comment\n"),
        None,
    );
    assert_eq!(pairs(dir.path()), vec![pair("web", "vite # not a comment")]);
}

#[test]
fn a_table_without_command_is_ignored() {
    let dir = repo(
        Some("[scripts.run.web]\nhide = false\n[scripts.run.api]\ncommand = \"cargo run\"\n"),
        None,
    );
    assert_eq!(pairs(dir.path()), vec![pair("api", "cargo run")]);
}

#[test]
fn a_command_of_another_table_does_not_leak() {
    let dir = repo(
        Some("[scripts.run.x]\n\n[scripts.setup]\ncommand = \"npm install\"\n"),
        None,
    );
    assert!(run_configs(dir.path()).is_empty());
    let dir = repo(
        Some("[scripts.run.x]\n[[other]]\ncommand = \"nope\"\n[scripts.run.x.env]\ncommand = \"no\"\n"),
        None,
    );
    assert!(run_configs(dir.path()).is_empty());
}

#[test]
fn unsupported_shapes_are_ignored() {
    let dir = repo(
        Some(
            "[scripts.run.multi]\ncommand = \"\"\"\nnpm run dev\n\"\"\"\n[scripts.run.inline]\ncommand = { sh = \"x\" }\n[scripts.run.open]\ncommand = \"never closed\n[scripts.run.ok]\ncommand = \"fine\"\n",
        ),
        None,
    );
    assert_eq!(pairs(dir.path()), vec![pair("ok", "fine")]);
}

#[test]
fn a_directory_in_place_of_a_file_contributes_nothing() {
    let dir = repo(Some("[scripts.run.web]\ncommand = \"vite\"\n"), None);
    fs::create_dir(dir.path().join(".conductor/settings.local.toml")).unwrap();
    assert_eq!(pairs(dir.path()), vec![pair("web", "vite")]);
}

#[test]
fn display_names() {
    assert_eq!(display_name("api-docs"), "Api docs");
    assert_eq!(display_name("web"), "Web");
    assert_eq!(display_name(""), "");
}
