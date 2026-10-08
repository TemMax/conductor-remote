use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use conductor_remote::reads::extras::commands::{
    resolve_program, CommandError, Commands, Limits, SystemCommands,
};
use conductor_remote::reads::extras::swr::{Pool, Revision, Swr};
use conductor_remote::reads::workspaces::resolve_repo_icon;
use conductor_remote::testing::{CommandCall, FakeCommands};

const WAIT: Duration = Duration::from_secs(5);

fn swr() -> (Arc<Pool>, Arc<Revision>, Swr<&'static str, u32>) {
    let pool = Pool::new(2);
    let revision = Arc::new(Revision::new());
    let cache = Swr::new(pool.clone(), revision.clone());
    (pool, revision, cache)
}

#[test]
fn swr_first_get_is_empty_and_the_next_has_the_value() {
    let (pool, _revision, cache) = swr();
    assert_eq!(cache.get(&"k", |_| false, || 7), None);
    pool.wait_idle();
    assert_eq!(cache.get(&"k", |_| false, || 8), Some(7));
}

#[test]
fn swr_fresh_entry_starts_no_refresh() {
    let (pool, _revision, cache) = swr();
    cache.get(&"k", |_| false, || 1);
    pool.wait_idle();
    let runs = Arc::new(AtomicUsize::new(0));
    let counter = runs.clone();
    let value = cache.get(
        &"k",
        |_| false,
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
            2
        },
    );
    pool.wait_idle();
    assert_eq!(value, Some(1));
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert_eq!(cache.peek(&"k"), Some(1));
}

#[test]
fn swr_stale_entry_is_returned_while_exactly_one_refresh_runs() {
    let (pool, _revision, cache) = swr();
    cache.get(&"k", |_| false, || 1);
    pool.wait_idle();

    let runs = Arc::new(AtomicUsize::new(0));
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let counter = runs.clone();
    let first = cache.get(
        &"k",
        |_| true,
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
            started_tx.send(()).unwrap();
            release_rx.recv_timeout(WAIT).unwrap();
            2
        },
    );
    assert_eq!(first, Some(1));
    started_rx.recv_timeout(WAIT).unwrap();

    for _ in 0..10 {
        let counter = runs.clone();
        let again = cache.get(
            &"k",
            |_| true,
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
                3
            },
        );
        assert_eq!(again, Some(1));
    }
    release_tx.send(()).unwrap();
    pool.wait_idle();
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(cache.peek(&"k"), Some(2));
}

#[test]
fn swr_bumps_the_revision_for_a_first_and_a_changed_value_only() {
    let (pool, revision, cache) = swr();
    assert_eq!(revision.get(), 0);
    cache.get(&"k", |_| true, || 1);
    pool.wait_idle();
    assert_eq!(revision.get(), 1);
    cache.get(&"k", |_| true, || 1);
    pool.wait_idle();
    assert_eq!(revision.get(), 1, "an equal value changes nothing");
    cache.get(&"k", |_| true, || 2);
    pool.wait_idle();
    assert_eq!(revision.get(), 2);
    cache.get(&"other", |_| true, || 2);
    pool.wait_idle();
    assert_eq!(revision.get(), 3, "a first value of another key counts");
}

#[test]
fn swr_stale_check_sees_the_time_of_the_refresh() {
    let (pool, _revision, cache) = swr();
    let before = Instant::now();
    cache.get(&"k", |_| false, || 1);
    pool.wait_idle();
    let mut seen = None;
    cache.get(
        &"k",
        |entry| {
            seen = Some(entry.at);
            false
        },
        || 1,
    );
    assert!(seen.unwrap() >= before);
}

#[test]
fn swr_panicking_refresh_leaves_the_key_usable() {
    let (pool, revision, cache) = swr();
    assert_eq!(cache.get(&"k", |_| false, || panic!("boom")), None);
    pool.wait_idle();
    assert_eq!(cache.peek(&"k"), None);
    assert_eq!(revision.get(), 0);
    assert_eq!(cache.get(&"k", |_| false, || 5), None);
    pool.wait_idle();
    assert_eq!(cache.peek(&"k"), Some(5));
    assert_eq!(revision.get(), 1);
}

#[test]
fn swr_peek_schedules_nothing() {
    let (pool, revision, cache) = swr();
    assert_eq!(cache.peek(&"k"), None);
    pool.wait_idle();
    assert_eq!(cache.peek(&"k"), None);
    assert_eq!(revision.get(), 0);
}

#[test]
fn pool_runs_at_most_max_parallel_jobs_and_in_order() {
    let pool = Pool::new(2);
    let (started_tx, started_rx) = mpsc::channel();
    let mut releases = Vec::new();
    let done = Arc::new(AtomicUsize::new(0));
    for i in 0..3 {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        releases.push(release_tx);
        let started = started_tx.clone();
        let done = done.clone();
        pool.submit(move || {
            started.send(i).unwrap();
            release_rx.recv_timeout(WAIT).unwrap();
            done.fetch_add(1, Ordering::SeqCst);
        });
    }
    let mut first_two = vec![
        started_rx.recv_timeout(WAIT).unwrap(),
        started_rx.recv_timeout(WAIT).unwrap(),
    ];
    first_two.sort();
    assert_eq!(first_two, [0, 1]);
    assert!(started_rx.recv_timeout(Duration::from_millis(200)).is_err());
    releases[0].send(()).unwrap();
    assert_eq!(started_rx.recv_timeout(WAIT).unwrap(), 2);
    releases[1].send(()).unwrap();
    releases[2].send(()).unwrap();
    pool.wait_idle();
    assert_eq!(done.load(Ordering::SeqCst), 3);
}

#[test]
fn pool_wait_idle_returns_after_all_jobs_ran_and_survives_a_panic() {
    let pool = Pool::new(1);
    let ran = Arc::new(AtomicUsize::new(0));
    pool.submit(|| panic!("boom"));
    for _ in 0..5 {
        let ran = ran.clone();
        pool.submit(move || {
            std::thread::sleep(Duration::from_millis(5));
            ran.fetch_add(1, Ordering::SeqCst);
        });
    }
    pool.wait_idle();
    assert_eq!(ran.load(Ordering::SeqCst), 5);
}

fn limits(timeout_ms: u64, max_stdout: usize) -> Limits {
    Limits {
        timeout: Duration::from_millis(timeout_ms),
        max_stdout,
    }
}

#[test]
fn system_commands_return_stdout_and_exit_code_zero() {
    let out = SystemCommands
        .run("/bin/echo", &["hello"], None, limits(5000, 1024))
        .unwrap();
    assert_eq!(out.code, Some(0));
    assert_eq!(out.stdout, b"hello\n");
}

#[test]
fn system_commands_find_a_bare_name() {
    let out = SystemCommands
        .run("echo", &["bare"], None, limits(5000, 1024))
        .unwrap();
    assert_eq!(out.stdout, b"bare\n");
}

#[test]
fn system_commands_return_a_non_zero_exit_code() {
    let out = SystemCommands
        .run(
            "/bin/sh",
            &["-c", "echo partial; exit 3"],
            None,
            limits(5000, 1024),
        )
        .unwrap();
    assert_eq!(out.code, Some(3));
    assert_eq!(out.stdout, b"partial\n");
}

#[test]
fn system_commands_report_no_code_when_a_signal_ended_the_program() {
    let out = SystemCommands
        .run("/bin/sh", &["-c", "kill -9 $$"], None, limits(5000, 1024))
        .unwrap();
    assert_eq!(out.code, None);
}

#[test]
fn system_commands_honour_the_directory() {
    let dir = tempfile::tempdir().unwrap();
    let out = SystemCommands
        .run("/bin/pwd", &[], Some(dir.path()), limits(5000, 1024))
        .unwrap();
    let printed = PathBuf::from(String::from_utf8(out.stdout).unwrap().trim_end());
    assert_eq!(
        printed.canonicalize().unwrap(),
        dir.path().canonicalize().unwrap()
    );
}

#[test]
fn system_commands_have_no_standard_input() {
    // `cat` ends at once when standard input is closed.
    let out = SystemCommands
        .run("/bin/cat", &[], None, limits(5000, 1024))
        .unwrap();
    assert_eq!(out.code, Some(0));
    assert!(out.stdout.is_empty());
}

#[test]
fn system_commands_kill_a_program_that_times_out() {
    let started = Instant::now();
    let err = SystemCommands
        .run("/bin/sleep", &["5"], None, limits(100, 1024))
        .unwrap_err();
    assert!(matches!(err, CommandError::Timeout { .. }), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn system_commands_time_out_a_program_that_closes_its_output_and_lingers() {
    let started = Instant::now();
    let err = SystemCommands
        .run(
            "/bin/sh",
            &["-c", "exec >&-; exec /bin/sleep 5"],
            None,
            limits(100, 1024),
        )
        .unwrap_err();
    assert!(matches!(err, CommandError::Timeout { .. }), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn system_commands_stop_a_program_that_writes_too_much() {
    let started = Instant::now();
    let err = SystemCommands
        .run("/usr/bin/yes", &[], None, limits(10_000, 1000))
        .unwrap_err();
    assert!(matches!(err, CommandError::TooLarge { .. }), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn system_commands_accept_output_exactly_at_the_limit() {
    let out = SystemCommands
        .run("/bin/echo", &["abcd"], None, limits(5000, 5))
        .unwrap();
    assert_eq!(out.stdout, b"abcd\n");
}

#[test]
fn system_commands_fail_to_start_an_unknown_program() {
    for program in ["/bin/no-such-program-here", "no-such-program-here"] {
        let err = SystemCommands
            .run(program, &[], None, limits(1000, 1024))
            .unwrap_err();
        assert!(matches!(err, CommandError::Spawn { .. }), "{err:?}");
    }
}

fn executable(path: &Path, mode: u32) {
    std::fs::write(path, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn resolve_program_returns_a_name_with_a_slash_as_it_is() {
    assert_eq!(
        resolve_program("./tools/x", Some("/bin")),
        Some(PathBuf::from("./tools/x"))
    );
    assert_eq!(
        resolve_program("/no/such/file", None),
        Some(PathBuf::from("/no/such/file"))
    );
}

#[test]
fn resolve_program_searches_the_path_env_in_order() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    executable(&first.path().join("mytool"), 0o644);
    executable(&second.path().join("mytool"), 0o755);
    let path_env = format!("{}::{}", first.path().display(), second.path().display());
    assert_eq!(
        resolve_program("mytool", Some(&path_env)),
        Some(second.path().join("mytool"))
    );
    executable(&first.path().join("mytool"), 0o755);
    assert_eq!(
        resolve_program("mytool", Some(&path_env)),
        Some(first.path().join("mytool"))
    );
}

#[test]
fn resolve_program_falls_back_to_the_standard_directories() {
    let found = resolve_program("sh", Some("/no/such/dir")).unwrap();
    assert_eq!(found.file_name().unwrap(), "sh");
    let dir = found.parent().unwrap().to_str().unwrap();
    assert!(
        ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"].contains(&dir),
        "{dir}"
    );
    assert!(resolve_program("sh", None).is_some());
}

#[test]
fn resolve_program_knows_no_unknown_name() {
    assert_eq!(resolve_program("no-such-program-here", Some("/bin")), None);
    assert_eq!(resolve_program("", None), None);
}

#[test]
fn fake_commands_match_by_program_and_argument_prefix() {
    let fake = FakeCommands::new();
    fake.on("git", &["status"], 0, "clean\n");
    fake.on("git", &["diff", "--stat"], 1, "stat");
    let l = limits(1000, 10);
    let out = fake.run("git", &["status", "-s"], None, l).unwrap();
    assert_eq!((out.code, out.stdout), (Some(0), b"clean\n".to_vec()));
    let out = fake.run("git", &["diff", "--stat", "x"], None, l).unwrap();
    assert_eq!((out.code, out.stdout), (Some(1), b"stat".to_vec()));
    assert!(matches!(
        fake.run("git", &["diff"], None, l),
        Err(CommandError::Spawn { .. })
    ));
    assert!(matches!(
        fake.run("git", &[], None, l),
        Err(CommandError::Spawn { .. })
    ));
    assert!(matches!(
        fake.run("gh", &["status"], None, l),
        Err(CommandError::Spawn { .. })
    ));
}

#[test]
fn fake_commands_let_the_later_rule_win() {
    let fake = FakeCommands::new();
    let l = limits(1000, 10);
    fake.on("git", &[], 0, "any");
    fake.on("git", &["log"], 0, "log");
    assert_eq!(fake.run("git", &["log"], None, l).unwrap().stdout, b"log");
    assert_eq!(fake.run("git", &["show"], None, l).unwrap().stdout, b"any");
    fake.on("git", &["log"], 2, "again");
    let out = fake.run("git", &["log"], None, l).unwrap();
    assert_eq!((out.code, out.stdout), (Some(2), b"again".to_vec()));
    fake.fail("git", &["log"]);
    assert!(matches!(
        fake.run("git", &["log"], None, l),
        Err(CommandError::Timeout { .. })
    ));
}

#[test]
fn fake_commands_record_every_call() {
    let fake = FakeCommands::new();
    fake.on("git", &[], 0, "");
    let l = limits(250, 99);
    let _ = fake.run("git", &["a", "b"], Some(Path::new("/work")), l);
    let _ = fake.run("nope", &[], None, l);
    assert_eq!(
        fake.calls(),
        vec![
            CommandCall {
                program: "git".to_owned(),
                args: vec!["a".to_owned(), "b".to_owned()],
                cwd: Some(PathBuf::from("/work")),
                limits: l,
            },
            CommandCall {
                program: "nope".to_owned(),
                args: vec![],
                cwd: None,
                limits: l,
            },
        ]
    );
}

fn touch(root: &Path, rel: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "x").unwrap();
}

fn icon(root: &Path) -> Option<PathBuf> {
    resolve_repo_icon(root.to_str().unwrap())
}

#[test]
fn repo_icon_first_candidate_in_the_list_order_wins() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "favicon.ico");
    touch(dir.path(), "favicon.png");
    touch(dir.path(), "public/favicon.png");
    assert_eq!(
        icon(dir.path()),
        Some(dir.path().join("public/favicon.png"))
    );
    // The answer is kept: a better file appearing later is not seen at once.
    touch(dir.path(), "apple-touch-icon.png");
    assert_eq!(
        icon(dir.path()),
        Some(dir.path().join("public/favicon.png"))
    );
}

#[test]
fn repo_icon_is_nothing_without_a_candidate() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "README.md");
    touch(dir.path(), "public/other.png");
    assert_eq!(icon(dir.path()), None);
}

#[test]
fn repo_icon_skips_a_directory_and_a_symbolic_link() {
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    touch(elsewhere.path(), "real.png");
    std::fs::create_dir_all(dir.path().join("public/apple-touch-icon.png")).unwrap();
    symlink(
        elsewhere.path().join("real.png"),
        dir.path().join("apple-touch-icon.png"),
    )
    .unwrap();
    assert_eq!(icon(dir.path()), None);
    let again = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(again.path().join("public/apple-touch-icon.png")).unwrap();
    symlink(
        elsewhere.path().join("real.png"),
        again.path().join("apple-touch-icon.png"),
    )
    .unwrap();
    touch(again.path(), "favicon.svg");
    assert_eq!(icon(again.path()), Some(again.path().join("favicon.svg")));
}

#[test]
fn system_commands_return_standard_error() {
    let out = SystemCommands
        .run(
            "/bin/sh",
            &["-c", "echo out; echo oops >&2; exit 1"],
            None,
            limits(5000, 1024),
        )
        .unwrap();
    assert_eq!(out.code, Some(1));
    assert_eq!(out.stdout, b"out\n");
    assert_eq!(out.stderr, b"oops\n");
}

#[test]
fn system_commands_fill_both_pipes_and_cut_standard_error_at_the_limit() {
    // Interleaved writes: 100 KB to stdout (under the cap) and 300 KB to stderr (over it), far more
    // than a pipe holds, so the program would block if either stream were left unread.
    let script = "i=0; while [ $i -lt 100 ]; do \
        printf '%01000d' 0; printf '%03000d' 0 >&2; i=$((i+1)); done";
    let started = Instant::now();
    let out = SystemCommands
        .run("/bin/sh", &["-c", script], None, limits(10_000, 200_000))
        .unwrap();
    assert_eq!(out.code, Some(0));
    assert_eq!(out.stdout.len(), 100_000);
    assert_eq!(out.stderr.len(), 200_000);
    assert!(out.stderr.iter().all(|b| *b == b'0'));
    assert!(started.elapsed() < Duration::from_secs(8));
}

#[test]
fn system_commands_finish_when_both_streams_exceed_the_cap() {
    // Stdout beyond the cap is still an error; the call must end at once, not hang on stderr.
    let script = "i=0; while [ $i -lt 400 ]; do \
        printf '%02000d' 0; printf '%02000d' 0 >&2; i=$((i+1)); done";
    let started = Instant::now();
    let err = SystemCommands
        .run("/bin/sh", &["-c", script], None, limits(10_000, 1000))
        .unwrap_err();
    assert!(matches!(err, CommandError::TooLarge { .. }), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(8));
}

#[test]
fn system_commands_cut_standard_error_when_stdout_is_small() {
    let script =
        "printf abc; i=0; while [ $i -lt 200 ]; do printf '%02000d' 0 >&2; i=$((i+1)); done";
    let out = SystemCommands
        .run("/bin/sh", &["-c", script], None, limits(10_000, 1000))
        .unwrap();
    assert_eq!(out.code, Some(0));
    assert_eq!(out.stdout, b"abc");
    assert_eq!(out.stderr.len(), 1000);
}

#[test]
fn fake_commands_answer_their_standard_error() {
    let fake = FakeCommands::new();
    fake.on("gh", &["a"], 0, "fine");
    fake.on_with_stderr("gh", &["b"], 1, "", "boom");
    let ok = fake.run("gh", &["a"], None, limits(1000, 1024)).unwrap();
    assert!(ok.stderr.is_empty());
    let failed = fake.run("gh", &["b"], None, limits(1000, 1024)).unwrap();
    assert_eq!(failed.code, Some(1));
    assert_eq!(failed.stderr, b"boom");
}
