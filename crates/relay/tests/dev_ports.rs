//! Finding the ports of a workspace's Run task: the process listing, the `lsof` table and the probes.

use std::path::Path;
use std::time::{Duration, Instant};

use conductor_remote::dev::ports::{
    listening_ports_of, run_task_pids, run_task_ports, tcp_open, wait_for_port, ProcessSnapshot,
};
use conductor_remote::testing::FakeCommands;

const HOME: &str = "/Users/tester";
const WORKTREE: &str = "acme/feature/login";
const WRAPPER: &str = "zsh /Users/tester/.conductor/projects/acme--feature--login/run-run:1234.sh";

fn home() -> &'static Path {
    Path::new(HOME)
}

fn listing() -> String {
    [
        "    1     0 /sbin/launchd",
        "  400     1 /Applications/Conductor.app/Contents/MacOS/Conductor",
        &format!("  500   400 {WRAPPER}"),
        "  501   500 pnpm run dev",
        "  502   501 node /Users/tester/work/vite.js --port 5173",
        "  600   400 zsh /Users/tester/.conductor/projects/acme--other/run-run:99.sh",
        "  601   600 node other-server.js",
        "  700     1 /usr/bin/vim notes.txt",
    ]
    .join("\n")
}

const LSOF: &str = "\
COMMAND   PID  USER   FD   TYPE             DEVICE SIZE/OFF NODE NAME
node      502 tester   23u  IPv4 0x7a1b2c3d4e5f6a7b      0t0  TCP *:55200 (LISTEN)
node      502 tester   24u  IPv6 0x7a1b2c3d4e5f6a7c      0t0  TCP [::1]:5173 (LISTEN)
node      502 tester   25u  IPv4 0x7a1b2c3d4e5f6a7d      0t0  TCP 127.0.0.1:5173 (LISTEN)
node      501 tester   26u  IPv6 0x7a1b2c3d4e5f6a7e      0t0  TCP *:3000 (LISTEN)
node      501 tester   27u  IPv4 0x7a1b2c3d4e5f6a7f      0t0  TCP 127.0.0.1:3000 (LISTEN)
";

// ------------------------------------------------------------- run_task_pids

#[test]
fn run_task_pids_finds_the_wrapper_and_all_its_descendants() {
    assert_eq!(run_task_pids(&listing(), home(), WORKTREE), [500, 501, 502]);
}

#[test]
fn run_task_pids_ignores_the_unrelated_and_other_worktrees() {
    let pids = run_task_pids(&listing(), home(), WORKTREE);
    for other in [1, 400, 600, 601, 700] {
        assert!(!pids.contains(&other), "{other} is not part of the task");
    }
    assert_eq!(run_task_pids(&listing(), home(), "acme/other"), [600, 601]);
}

#[test]
fn run_task_pids_without_a_wrapper_is_empty() {
    assert!(run_task_pids(&listing(), home(), "acme/missing").is_empty());
    assert!(run_task_pids("", home(), WORKTREE).is_empty());
    assert!(run_task_pids("garbage\n  12\n", home(), WORKTREE).is_empty());
}

#[test]
fn run_task_pids_needs_the_run_prefix_of_the_exact_worktree() {
    let text = "  10     1 zsh /Users/tester/.conductor/projects/acme--feature--login-2/run-run:1.sh\n\
                  11     1 zsh /Users/tester/.conductor/projects/acme--feature--login/setup-run:1.sh\n";
    assert!(run_task_pids(text, home(), WORKTREE).is_empty());
}

// ------------------------------------------------------ listening_ports_of

#[test]
fn listening_ports_are_ascending_and_unique() {
    assert_eq!(listening_ports_of(LSOF), [3000, 5173, 55200]);
}

#[test]
fn listening_ports_skip_the_header_and_what_does_not_parse() {
    assert!(listening_ports_of("").is_empty());
    assert!(listening_ports_of("COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME\n").is_empty());
    let text = "node 1 u 3u IPv4 0x1 0t0 TCP *:notaport (LISTEN)\n\
                node 1 u 4u IPv4 0x1 0t0 TCP 99999 (LISTEN)\n\
                short line\n\
                node 1 u 5u IPv4 0x1 0t0 TCP *:8080 (LISTEN)\n";
    assert_eq!(listening_ports_of(text), [8080]);
}

// -------------------------------------------------------- run_task_ports

#[test]
fn run_task_ports_runs_ps_then_lsof_with_exact_arguments() {
    let fake = FakeCommands::new();
    fake.on("ps", &[], 0, &listing());
    fake.on("/usr/sbin/lsof", &[], 0, LSOF);

    assert_eq!(run_task_ports(&fake, home(), WORKTREE), [3000, 5173, 55200]);

    let calls = fake.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].program, "ps");
    assert_eq!(calls[0].args, ["-axww", "-o", "pid=,ppid=,args="]);
    assert_eq!(calls[0].limits.timeout, Duration::from_secs(10));
    assert_eq!(calls[0].limits.max_stdout, 16 * 1024 * 1024);
    assert_eq!(calls[1].program, "/usr/sbin/lsof");
    assert_eq!(
        calls[1].args,
        ["-nP", "-iTCP", "-sTCP:LISTEN", "-a", "-p", "500,501,502"]
    );
    assert_eq!(calls[1].limits.timeout, Duration::from_secs(10));
    assert_eq!(calls[1].limits.max_stdout, 1024 * 1024);
}

#[test]
fn run_task_ports_is_empty_when_lsof_finds_nothing() {
    let fake = FakeCommands::new();
    fake.on("ps", &[], 0, &listing());
    fake.on("/usr/sbin/lsof", &[], 1, "");
    assert!(run_task_ports(&fake, home(), WORKTREE).is_empty());
}

#[test]
fn run_task_ports_is_empty_when_a_program_fails() {
    let fake = FakeCommands::new();
    fake.fail("ps", &[]);
    fake.on("/usr/sbin/lsof", &[], 0, LSOF);
    assert!(run_task_ports(&fake, home(), WORKTREE).is_empty());
    assert_eq!(fake.calls().len(), 1, "no lsof without a process list");

    let fake = FakeCommands::new();
    fake.on("ps", &[], 1, "");
    fake.on("/usr/sbin/lsof", &[], 0, LSOF);
    assert!(run_task_ports(&fake, home(), WORKTREE).is_empty());

    let fake = FakeCommands::new();
    fake.on("ps", &[], 0, &listing());
    fake.fail("/usr/sbin/lsof", &[]);
    assert!(run_task_ports(&fake, home(), WORKTREE).is_empty());

    let fake = FakeCommands::new();
    fake.on("ps", &[], 0, &listing());
    fake.on("/usr/sbin/lsof", &[], 2, LSOF);
    assert!(run_task_ports(&fake, home(), WORKTREE).is_empty());
}

#[test]
fn run_task_ports_does_not_run_lsof_without_a_task() {
    let fake = FakeCommands::new();
    fake.on("ps", &[], 0, &listing());
    fake.on("/usr/sbin/lsof", &[], 0, LSOF);
    assert!(run_task_ports(&fake, home(), "acme/missing").is_empty());
    assert_eq!(fake.calls().len(), 1);
}

// ------------------------------------------------------- ProcessSnapshot

fn ps_calls(fake: &FakeCommands) -> usize {
    fake.calls()
        .iter()
        .filter(|call| call.program == "ps")
        .count()
}

#[test]
fn two_listings_within_the_age_run_ps_once() {
    let fake = FakeCommands::new();
    fake.on("ps", &[], 0, &listing());
    let snapshot = ProcessSnapshot::new();
    let age = Duration::from_secs(60);

    let first = snapshot.listing(&fake, age);
    let second = snapshot.listing(&fake, age);
    assert_eq!(first.as_deref(), Some(listing().as_str()));
    assert_eq!(second, first);
    assert_eq!(ps_calls(&fake), 1);
}

#[test]
fn a_listing_with_a_zero_age_always_reads() {
    let fake = FakeCommands::new();
    fake.on("ps", &[], 0, &listing());
    let snapshot = ProcessSnapshot::new();

    assert!(snapshot.listing(&fake, Duration::ZERO).is_some());
    assert!(snapshot.listing(&fake, Duration::ZERO).is_some());
    assert_eq!(ps_calls(&fake), 2);
    // What a zero-age read kept is still there for a caller that accepts it.
    assert!(snapshot.listing(&fake, Duration::from_secs(60)).is_some());
    assert_eq!(ps_calls(&fake), 2);
}

#[test]
fn a_listing_older_than_the_age_is_read_again() {
    let fake = FakeCommands::new();
    fake.on("ps", &[], 0, &listing());
    let snapshot = ProcessSnapshot::new();
    let age = Duration::from_millis(50);

    assert!(snapshot.listing(&fake, age).is_some());
    std::thread::sleep(Duration::from_millis(120));
    assert!(snapshot.listing(&fake, age).is_some());
    assert_eq!(ps_calls(&fake), 2);
}

#[test]
fn a_failing_ps_gives_none_and_is_run_again() {
    let age = Duration::from_secs(60);

    let fake = FakeCommands::new();
    fake.fail("ps", &[]);
    let snapshot = ProcessSnapshot::new();
    assert_eq!(snapshot.listing(&fake, age), None);
    assert_eq!(snapshot.listing(&fake, age), None);
    assert_eq!(ps_calls(&fake), 2);

    let fake = FakeCommands::new();
    fake.on("ps", &[], 1, "");
    let snapshot = ProcessSnapshot::new();
    assert_eq!(snapshot.listing(&fake, age), None);
    assert_eq!(snapshot.listing(&fake, age), None);
    assert_eq!(ps_calls(&fake), 2);
}

// ------------------------------------------------------ tcp_open, wait_for_port

/// A port nothing listens on: a listener's, after it is dropped.
async fn closed_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

#[tokio::test]
async fn tcp_open_sees_an_ipv4_listener_and_its_end() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    assert!(tcp_open(port).await);
    drop(listener);
    assert!(!tcp_open(port).await);
}

#[tokio::test]
async fn tcp_open_sees_an_ipv6_listener() {
    let Ok(listener) = tokio::net::TcpListener::bind("[::1]:0").await else {
        eprintln!("no IPv6 loopback on this machine; skipped");
        return;
    };
    let port = listener.local_addr().unwrap().port();
    assert!(tcp_open(port).await);
    drop(listener);
    assert!(!tcp_open(port).await);
}

#[tokio::test]
async fn wait_for_port_returns_when_the_port_is_already_open() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    assert!(wait_for_port(port, true, Duration::from_secs(2)).await);
}

#[tokio::test]
async fn wait_for_port_waits_for_a_listener_to_appear() {
    let port = closed_port().await;
    let opener = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        tokio::net::TcpListener::bind(("127.0.0.1", port)).await
    });
    assert!(wait_for_port(port, true, Duration::from_secs(5)).await);
    drop(opener.await.unwrap());
}

#[tokio::test]
async fn wait_for_port_waits_for_a_listener_to_go() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let closer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        drop(listener);
    });
    assert!(wait_for_port(port, false, Duration::from_secs(5)).await);
    closer.await.unwrap();
}

#[tokio::test]
async fn wait_for_port_gives_up_after_the_timeout() {
    let port = closed_port().await;
    let started = Instant::now();
    assert!(!wait_for_port(port, true, Duration::from_millis(600)).await);
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(600), "{took:?}");
    assert!(took < Duration::from_secs(3), "{took:?}");
}
