use std::fs;
use std::io::Write as _;

use conductor_remote::host::logbuf::{
    layer, tail, LogBuffer, Redactor, MAX_ENTRIES, MAX_TEXT, TAIL_BYTES,
};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;

#[test]
fn the_ring_keeps_the_newest_lines_in_order() {
    let buffer = LogBuffer::new();
    let shared = buffer.clone();
    for i in 0..MAX_ENTRIES + 50 {
        shared.push("info", format!("line {i}"));
    }

    let all = buffer.entries(usize::MAX);
    assert_eq!(all.len(), MAX_ENTRIES);
    assert_eq!(all.first().unwrap().text, "line 50");
    assert_eq!(
        all.last().unwrap().text,
        format!("line {}", MAX_ENTRIES + 49)
    );
    assert!(all.iter().all(|e| e.t >= buffer.started_at()));
    assert!(all.windows(2).all(|w| w[0].t <= w[1].t));

    let newest: Vec<String> = buffer.entries(3).into_iter().map(|e| e.text).collect();
    let n = MAX_ENTRIES + 50;
    assert_eq!(
        newest,
        [
            format!("line {}", n - 3),
            format!("line {}", n - 2),
            format!("line {}", n - 1)
        ]
    );
    assert!(buffer.entries(0).is_empty());
}

#[test]
fn a_fresh_ring_is_empty() {
    let buffer = LogBuffer::new();
    assert!(buffer.entries(10).is_empty());
    assert!(buffer.started_at() > 0);
}

#[test]
fn lines_are_clipped_by_characters_not_bytes() {
    let buffer = LogBuffer::new();
    // Each character is two bytes: a byte cut would land mid-character or keep half as many.
    buffer.push("warn", "é".repeat(MAX_TEXT));
    buffer.push("warn", "é".repeat(MAX_TEXT + 1));

    let entries = buffer.entries(2);
    assert_eq!(entries[0].text, "é".repeat(MAX_TEXT));
    let clipped = &entries[1].text;
    assert_eq!(clipped.chars().count(), MAX_TEXT);
    assert!(clipped.starts_with("éé"));
    assert!(clipped.ends_with("… [truncated]"));
    assert_eq!(entries[1].level, "warn");
}

#[test]
fn redacts_the_access_token_anywhere() {
    let redactor = Redactor::new();
    redactor.set_token("tok3n-ABCdef123");
    assert_eq!(
        redactor.redact("relay up, key tok3n-ABCdef123 loaded"),
        "relay up, key [redacted] loaded"
    );
    assert_eq!(
        redactor.redact("open https://mac.tail1234.ts.net/#token=tok3n-ABCdef123 on the phone"),
        "open https://mac.tail1234.ts.net/#token=[redacted] on the phone"
    );
    assert_eq!(
        redactor.redact("GET /api/state?a=1&token=tok3n-ABCdef123&b=2"),
        "GET /api/state?a=1&token=[redacted]&b=2"
    );
    assert_eq!(
        redactor.redact("http://host/ptok3n-ABCdef123q"),
        "http://host/p[redacted]q"
    );
}

#[test]
fn redacts_any_token_value_up_to_an_ampersand_or_whitespace() {
    let redactor = Redactor::new();
    assert_eq!(
        redactor.redact("old url ?token=rotated-value&x=1 still cached"),
        "old url ?token=[redacted]&x=1 still cached"
    );
    assert_eq!(
        redactor.redact("TOKEN=abc def Token=x\tnext"),
        "TOKEN=[redacted] def Token=[redacted]\tnext"
    );
    assert_eq!(
        redactor.redact("header \"token=quoted\" end"),
        "header \"token=[redacted]\" end"
    );
    assert_eq!(redactor.redact("token= nothing"), "token= nothing");
    assert_eq!(redactor.redact("ends with token="), "ends with token=");
}

#[test]
fn redacts_openai_keys_and_webhook_secrets() {
    let redactor = Redactor::new();
    redactor.set_token("relay-token");
    assert_eq!(
        redactor.redact(
            "accept sk-proj-abcdefghijklmnopqrstuvwxyz webhook whsec_YWJjZGVmZ2hpamtsbW5vcA=="
        ),
        "accept [redacted] webhook [redacted]"
    );
    assert_eq!(redactor.redact("(sk-ABCD_efgh-12)."), "([redacted]).");
    // Too short to be a key, or not at a word boundary.
    assert_eq!(
        redactor.redact("sk-short whsec_short"),
        "sk-short whsec_short"
    );
    assert_eq!(
        redactor.redact("task-abcdefghijkl xwhsec_abcdefghijkl"),
        "task-abcdefghijkl xwhsec_abcdefghijkl"
    );
}

#[test]
fn an_empty_or_unset_token_matches_nothing() {
    let redactor = Redactor::new();
    let text = "plain line with no secrets — ünïcode too";
    assert_eq!(redactor.redact(text), text);
    redactor.set_token("");
    assert_eq!(redactor.redact(text), text);
    assert_eq!(redactor.redact(""), "");
}

#[test]
fn a_token_set_later_reaches_every_clone() {
    let redactor = Redactor::new();
    let clone = redactor.clone();
    assert_eq!(clone.redact("key late-secret-1"), "key late-secret-1");
    redactor.set_token("late-secret-1");
    assert_eq!(clone.redact("key late-secret-1"), "key [redacted]");
    redactor.set_token("late-secret-2");
    assert_eq!(clone.redact("key late-secret-1"), "key late-secret-1");
    assert_eq!(clone.redact("key late-secret-2"), "key [redacted]");
}

#[test]
fn the_layer_copies_events_into_the_ring() {
    let buffer = LogBuffer::new();
    let redactor = Redactor::new();
    let subscriber = tracing_subscriber::registry()
        .with(LevelFilter::INFO)
        .with(layer(buffer.clone(), redactor.clone()));
    // The token is only known after the layer was built, as at startup.
    redactor.set_token("s3cret-relay-token");

    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(port = 8787, host = "127.0.0.1", "relay listening");
        tracing::warn!(
            url = %"https://mac.ts.net/#token=s3cret-relay-token",
            "phone url"
        );
        tracing::error!("delivery failed: {}", "boom");
        tracing::debug!("filtered out");
        tracing::trace!("filtered out");
        tracing::info!(key = "sk-proj-abcdefghijklmnop");
        tracing::info!("{}", "x".repeat(MAX_TEXT * 2));
    });

    let entries = buffer.entries(usize::MAX);
    let got: Vec<(&str, &str)> = entries
        .iter()
        .take(4)
        .map(|e| (e.level, e.text.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            ("info", "relay listening port=8787 host=127.0.0.1"),
            ("warn", "phone url url=https://mac.ts.net/#token=[redacted]"),
            ("error", "delivery failed: boom"),
            ("info", "key=[redacted]"),
        ]
    );
    assert_eq!(entries.len(), 5);
    assert_eq!(entries[4].text.chars().count(), MAX_TEXT);
    assert!(entries[4].text.ends_with("… [truncated]"));
    assert!(entries.iter().all(|e| !e.text.contains("s3cret")));
}

#[test]
fn tail_reads_the_last_lines_of_a_large_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay.log");
    let mut file = fs::File::create(&path).unwrap();
    let total = 10_000;
    for i in 0..total {
        // 40 bytes per line with the newline: 400 KB, more than the window.
        writeln!(file, "line {i:05} {}", "-".repeat(28)).unwrap();
    }
    drop(file);
    assert!(fs::metadata(&path).unwrap().len() > TAIL_BYTES);

    let last = tail(&path, 3).unwrap();
    assert_eq!(
        last,
        [
            format!("line {:05} {}", total - 3, "-".repeat(28)),
            format!("line {:05} {}", total - 2, "-".repeat(28)),
            format!("line {:05} {}", total - 1, "-".repeat(28)),
        ]
    );

    let window = tail(&path, usize::MAX).unwrap();
    // Only whole lines from the last TAIL_BYTES: the partial first line is dropped.
    assert_eq!(window.len(), (TAIL_BYTES / 40) as usize);
    assert!(window
        .iter()
        .all(|l| l.len() == 39 && l.starts_with("line ")));
    assert_eq!(
        window.last().unwrap(),
        &format!("line {:05} {}", total - 1, "-".repeat(28))
    );
}

#[test]
fn tail_of_a_small_file_keeps_every_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay.err.log");
    fs::write(&path, "first\nsecond\nthird without newline").unwrap();
    assert_eq!(
        tail(&path, 10).unwrap(),
        ["first", "second", "third without newline"]
    );
    assert_eq!(tail(&path, 1).unwrap(), ["third without newline"]);
    assert!(tail(&path, 0).unwrap().is_empty());

    let empty = dir.path().join("empty.log");
    fs::write(&empty, "").unwrap();
    assert!(tail(&empty, 10).unwrap().is_empty());
}

#[test]
fn tail_of_a_missing_file_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    assert!(tail(&dir.path().join("relay.log"), 50).unwrap().is_empty());
}
