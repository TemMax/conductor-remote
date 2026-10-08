//! The image reads: a tool image by its reference and a repository's icon file.
//!
//! The rows come from the invented database of `support/seed_images.rs`. The first two tests
//! port the reference tests "images travel as references, never as bytes" and "image numbering
//! is per row, and the lookup walks it the same way" against a real SQLite file.

#[path = "support/seed_images.rs"]
mod seed_images;
mod support;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use conductor_remote::reads::images::RepoIconFile;
use conductor_remote::reads::Reads;
use conductor_remote::transcript::images::{tool_image_at, ToolImage};
use conductor_remote::transcript::{parse_message, StoredMessage};
use seed_images::*;
use support::TestDb;

/// The relay's reads over the seeded database. The `TestDb` is returned too: it owns the files.
fn seeded() -> (TestDb, Reads) {
    let test = TestDb::new();
    seed_images::seed(&test.conn());
    let reads = Reads::new(test.db(), test.root());
    (test, reads)
}

fn image(media_type: &str, base64: &str) -> ToolImage {
    ToolImage {
        media_type: media_type.to_owned(),
        bytes: STANDARD.decode(base64).unwrap(),
    }
}

/// The references the transcript parser emits for a row, in order.
fn emitted(test: &TestDb, rowid: i64) -> Vec<String> {
    let row = test
        .conn()
        .query_row(
            "SELECT rowid, id, content, created_at, sent_at, queue_order \
             FROM session_messages WHERE rowid = ?1",
            [rowid],
            |row| {
                Ok(StoredMessage {
                    rowid: row.get(0)?,
                    id: row.get(1)?,
                    content: row.get(2)?,
                    created_at: row.get(3)?,
                    sent_at: row.get(4)?,
                    queue_order: row.get(5)?,
                })
            },
        )
        .unwrap();
    parse_message(&row, None)
        .into_iter()
        .flat_map(|entry| entry.images)
        .collect()
}

#[test]
fn an_image_travels_as_a_reference_and_is_served_from_its_row() {
    let (test, reads) = seeded();
    assert_eq!(emitted(&test, ROW_SINGLE), ["1.0"]);
    assert_eq!(
        reads.tool_image("1.0").unwrap(),
        Some(image("image/png", PNG))
    );
    assert_eq!(reads.tool_image("1.1").unwrap(), None);
}

#[test]
fn numbering_runs_across_the_results_of_a_row_and_matches_the_parser() {
    let (test, reads) = seeded();
    let references = emitted(&test, ROW_TWO_RESULTS);
    assert_eq!(references, ["2.0", "2.1", "2.2"]);

    let served: Vec<_> = references
        .iter()
        .map(|reference| reads.tool_image(reference).unwrap())
        .collect();
    assert_eq!(
        served,
        [
            Some(image("image/png", PNG)),
            Some(image("image/gif", GIF)),
            Some(image("image/jpeg", JPEG)),
        ]
    );
    assert_eq!(reads.tool_image("2.3").unwrap(), None);
}

#[test]
fn every_reference_the_parser_emits_resolves_to_an_image() {
    let (test, reads) = seeded();
    for rowid in [
        ROW_SINGLE,
        ROW_TWO_RESULTS,
        ROW_SNIFFED,
        ROW_FOREIGN_TYPE,
        ROW_SURROGATE,
    ] {
        let references = emitted(&test, rowid);
        assert!(!references.is_empty(), "row {rowid} holds images");
        for (n, reference) in references.iter().enumerate() {
            assert_eq!(*reference, format!("{rowid}.{n}"));
            assert!(
                reads.tool_image(reference).unwrap().is_some(),
                "{reference} resolves"
            );
        }
        let next = format!("{rowid}.{}", references.len());
        assert_eq!(
            reads.tool_image(&next).unwrap(),
            None,
            "{next} is past the end"
        );
    }
}

#[test]
fn a_missing_media_type_is_sniffed_from_the_base64_prefix() {
    let (_test, reads) = seeded();
    let served: Vec<_> = (0..5)
        .map(|n| {
            reads
                .tool_image(&format!("{ROW_SNIFFED}.{n}"))
                .unwrap()
                .unwrap()
        })
        .collect();
    assert_eq!(
        served,
        [
            image("image/png", PNG),
            image("image/jpeg", JPEG),
            image("image/gif", GIF),
            image("image/webp", WEBP),
            image("application/octet-stream", OTHER),
        ]
    );
}

#[test]
fn a_media_type_outside_the_four_is_replaced() {
    let (_test, reads) = seeded();
    let served: Vec<_> = (0..3)
        .map(|n| {
            reads
                .tool_image(&format!("{ROW_FOREIGN_TYPE}.{n}"))
                .unwrap()
                .unwrap()
        })
        .collect();
    assert_eq!(
        served,
        [
            image("image/png", PNG),
            image("application/octet-stream", OTHER),
            image("image/webp", WEBP),
        ]
    );
}

#[test]
fn a_row_with_a_lone_surrogate_escape_is_read_by_the_second_attempt() {
    let (_test, reads) = seeded();
    assert_eq!(
        reads.tool_image(&format!("{ROW_SURROGATE}.0")).unwrap(),
        Some(image("image/png", PNG))
    );
}

#[test]
fn data_that_is_missing_or_does_not_decode_gives_nothing() {
    let (_test, reads) = seeded();
    for reference in [
        format!("{ROW_BAD_DATA}.0"),
        format!("{ROW_NO_DATA}.0"),
        format!("{ROW_NO_DATA}.1"),
    ] {
        assert_eq!(reads.tool_image(&reference).unwrap(), None, "{reference}");
    }
}

#[test]
fn rows_without_images_give_nothing() {
    let (_test, reads) = seeded();
    for rowid in [ROW_PLAIN, ROW_NOT_A_LIST] {
        assert_eq!(reads.tool_image(&format!("{rowid}.0")).unwrap(), None);
    }
}

#[test]
fn a_malformed_reference_gives_nothing() {
    let (_test, reads) = seeded();
    for reference in [
        "",
        "1",
        ".",
        ".0",
        "1.",
        "a.0",
        "1.a",
        "+1.0",
        "1.+0",
        "-1.0",
        "1.-1",
        " 1.0",
        "1.0 ",
        "1. 0",
        "1..0",
        "1.0.0",
        "1,0",
        "1.0e0",
        "1.99999999999999999999999",
        "99999999999999999999.0",
        "\u{663}.0",
    ] {
        assert_eq!(reads.tool_image(reference).unwrap(), None, "{reference:?}");
    }
}

#[test]
fn an_unknown_row_gives_nothing() {
    let (_test, reads) = seeded();
    assert_eq!(reads.tool_image("999.0").unwrap(), None);
    assert_eq!(reads.tool_image("0.0").unwrap(), None);
}

#[test]
fn the_walk_reads_a_frame_the_way_the_parser_does() {
    let png = format!(
        r#"{{"message":{{"content":[{{"type":"tool_result","content":[{{"type":"image","source":{{"data":"{PNG}"}}}}]}}]}}}}"#
    );
    assert_eq!(tool_image_at(&png, 0), Some(image("image/png", PNG)));
    assert_eq!(tool_image_at(&png, 1), None);
    assert_eq!(tool_image_at("not json", 0), None);
    assert_eq!(tool_image_at("[]", 0), None);
    assert_eq!(tool_image_at(r#"{"message":{"content":"text"}}"#, 0), None);
}

// Repository icons.

/// A repository named `name` whose root is `root`.
fn add_repo(test: &TestDb, name: &str, root: &std::path::Path) {
    test.conn()
        .execute(
            "INSERT INTO repos (id, name, root_path) VALUES (?1, ?2, ?3)",
            rusqlite::params![format!("img-repo-{name}"), name, root.to_str().unwrap()],
        )
        .unwrap();
}

/// A repository root in its own temporary directory (the icon lookup is cached per root).
fn repo_with(files: &[(&str, &[u8])]) -> (TestDb, Reads, tempfile::TempDir) {
    let (test, reads) = seeded();
    let root = tempfile::tempdir().unwrap();
    for (path, bytes) in files {
        let path = root.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    add_repo(&test, "img-project", root.path());
    (test, reads, root)
}

fn icon(content_type: &'static str, bytes: &[u8]) -> Option<RepoIconFile> {
    Some(RepoIconFile {
        content_type,
        bytes: bytes.to_vec(),
    })
}

#[test]
fn an_icon_is_found_by_the_candidate_order() {
    let (_t, reads, _root) = repo_with(&[("favicon.ico", b"ico")]);
    assert_eq!(
        reads.repo_icon("img-project").unwrap(),
        icon("image/x-icon", b"ico")
    );

    let (_t, reads, _root) =
        repo_with(&[("favicon.ico", b"ico"), ("public/favicon.svg", b"<svg/>")]);
    assert_eq!(
        reads.repo_icon("img-project").unwrap(),
        icon("image/svg+xml", b"<svg/>")
    );

    let (_t, reads, _root) = repo_with(&[
        ("favicon.ico", b"ico"),
        ("public/favicon.svg", b"<svg/>"),
        ("public/apple-touch-icon.png", b"png"),
    ]);
    assert_eq!(
        reads.repo_icon("img-project").unwrap(),
        icon("image/png", b"png")
    );
}

#[test]
fn an_unknown_repository_has_no_icon() {
    let (_t, reads, _root) = repo_with(&[("favicon.ico", b"ico")]);
    assert_eq!(reads.repo_icon("img-nobody").unwrap(), None);
    assert_eq!(reads.repo_icon("").unwrap(), None);
}

#[test]
fn a_repository_without_a_root_has_no_icon() {
    let (_test, reads) = seeded();
    assert_eq!(reads.repo_icon(REPO_NO_ROOT).unwrap(), None);
    assert_eq!(reads.repo_icon(REPO_EMPTY_ROOT).unwrap(), None);
}

#[test]
fn a_root_without_an_icon_file_has_no_icon() {
    let (_t, reads, _root) = repo_with(&[("README.md", b"text")]);
    assert_eq!(reads.repo_icon("img-project").unwrap(), None);

    let (test, reads) = seeded();
    let gone = test.dir().join("img-no-such-directory");
    add_repo(&test, "img-gone", &gone);
    assert_eq!(reads.repo_icon("img-gone").unwrap(), None);
}

#[test]
fn a_directory_in_place_of_the_icon_is_no_icon() {
    let (_t, reads, root) = repo_with(&[]);
    std::fs::create_dir_all(root.path().join("public/apple-touch-icon.png")).unwrap();
    assert_eq!(reads.repo_icon("img-project").unwrap(), None);
}

#[test]
fn a_file_larger_than_five_mebibytes_is_no_icon() {
    const LIMIT: usize = 5 * 1024 * 1024;
    let (_t, reads, _root) = repo_with(&[("favicon.ico", &vec![7; LIMIT])]);
    let served = reads.repo_icon("img-project").unwrap().unwrap();
    assert_eq!(served.bytes.len(), LIMIT);

    let (_t, reads, _root) = repo_with(&[("favicon.ico", &vec![7; LIMIT + 1])]);
    assert_eq!(reads.repo_icon("img-project").unwrap(), None);
}

#[cfg(unix)]
#[test]
fn a_file_that_cannot_be_read_is_no_icon() {
    use std::os::unix::fs::PermissionsExt;
    let (_t, reads, root) = repo_with(&[("favicon.ico", b"ico")]);
    let path = root.path().join("favicon.ico");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::File::open(&path).is_ok() {
        // Running with the right to read anything: the failure cannot be staged.
        return;
    }
    assert_eq!(reads.repo_icon("img-project").unwrap(), None);
}
