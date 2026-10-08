use std::process::Command;

/// The built binary's `<subcommand> --help`, which prints and exits before anything else runs.
fn help(subcommand: &str) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_conductor-remote"))
        .args([subcommand, "--help"])
        .output()
        .expect("the built binary runs");
    assert!(
        output.status.success(),
        "`{subcommand} --help` exited with {}",
        output.status
    );
    String::from_utf8(output.stdout).expect("the help is UTF-8")
}

/// Whether `text` holds `word` as a word of its own: `off`, not the `off` of `offline`.
fn names(text: &str, word: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .any(|found| found == word)
}

#[test]
fn start_help_names_exit_with_parent() {
    let help = help("start");
    assert!(names(&help, "--exit-with-parent"), "{help}");
    assert!(names(&help, "--parent-pid"), "{help}");
}

#[test]
fn tailnet_help_names_the_actions_and_json() {
    let help = help("tailnet");
    for word in ["status", "ensure", "off", "--json"] {
        assert!(names(&help, word), "`{word}` is missing from:\n{help}");
    }
}
