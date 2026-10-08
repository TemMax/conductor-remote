//! The attachment file layout: names, ids, tokens, writing, staging and materialising, against
//! files this test creates in temporary directories.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

use conductor_remote::files::attachments::{
    attachment_id, attachment_name, attachment_token, discard_staged, materialize, prune_staged,
    staged_attachments, write_attachment, Written, ATTACHMENTS_DIR, MAX_ATTACHMENT_BYTES,
};
use tempfile::TempDir;

fn ids(written: &[&Written]) -> Vec<String> {
    written.iter().map(|w| w.id.clone()).collect()
}

fn is_id(text: &str) -> bool {
    text.len() == 6 && text.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

/// Set a directory's modification time to `age` ago.
fn age(path: &Path, age: Duration) {
    let dir = fs::File::open(path).expect("open directory");
    dir.set_modified(SystemTime::now() - age)
        .expect("set modification time");
}

#[test]
fn limit_is_twenty_five_mebibytes() {
    assert_eq!(MAX_ATTACHMENT_BYTES, 26_214_400);
    assert_eq!(ATTACHMENTS_DIR, ".context/attachments");
}

// attachment tokens

#[test]
fn token_matches_the_exact_syntax_conductor_stores() {
    assert_eq!(
        attachment_token("image.png", ".context/attachments/jOTeCX/image.png"),
        "@⟦image.png⟧(.context%2Fattachments%2FjOTeCX%2Fimage.png)"
    );
    assert_eq!(
        attachment_token(
            "Transcript of Approve plan.md",
            ".context/attachments/kuB8pt/Transcript of Approve plan.md"
        ),
        "@⟦Transcript of Approve plan.md⟧(.context%2Fattachments%2FkuB8pt%2FTranscript%20of%20Approve%20plan.md)"
    );
}

#[test]
fn token_keeps_parentheses_in_the_path() {
    assert_eq!(
        attachment_token(
            "diagram (old).png",
            ".context/attachments/jOTeCX/diagram (old).png"
        ),
        "@⟦diagram (old).png⟧(.context%2Fattachments%2FjOTeCX%2Fdiagram%20(old).png)"
    );
}

#[test]
fn token_encodes_like_encode_uri_component() {
    // Kept: letters, digits and - _ . ! ~ * ' ( ). Everything else, per UTF-8 byte, in upper case.
    assert_eq!(
        attachment_token("n", "a-b_c.d!e~f*g'h(i)j"),
        "@⟦n⟧(a-b_c.d!e~f*g'h(i)j)"
    );
    assert_eq!(
        attachment_token("n", "a b+c&d=e?f#g%h:i@j,k;l$m"),
        "@⟦n⟧(a%20b%2Bc%26d%3De%3Ff%23g%25h%3Ai%40j%2Ck%3Bl%24m)"
    );
    assert_eq!(
        attachment_token("é", "é/€/😀"),
        "@⟦é⟧(%C3%A9%2F%E2%82%AC%2F%F0%9F%98%80)"
    );
}

// attachment names

#[test]
fn name_preserves_a_plain_safe_name() {
    assert_eq!(
        attachment_name("Transcript of Select product colors.md"),
        "Transcript of Select product colors.md"
    );
}

#[test]
fn name_removes_path_traversal_and_is_always_usable() {
    let flat = attachment_name("a/b\\c");
    assert_eq!(flat, "a-b-c");
    assert!(!flat.contains(['/', '\\']));

    let traversal = attachment_name("../../etc/passwd");
    assert_eq!(traversal, "-..-etc-passwd");
    assert!(!traversal.starts_with('.'));

    assert_eq!(attachment_name("..."), "attachment");
    assert_eq!(attachment_name(""), "attachment");
    assert_eq!(attachment_name("  \t "), "attachment");
}

#[test]
fn name_drops_control_characters_leading_dots_and_edge_white_space() {
    assert_eq!(attachment_name("a\u{0}b\nc\u{1f}d\u{7f}e"), "abcde");
    assert_eq!(attachment_name(".hidden.md"), "hidden.md");
    assert_eq!(attachment_name(". a"), "a");
    assert_eq!(attachment_name("  spaced name.md  "), "spaced name.md");
    assert_eq!(attachment_name("keep.the.dots.md"), "keep.the.dots.md");
}

#[test]
fn name_is_limited_to_120_utf8_bytes() {
    for title in ["x".repeat(400), "😀".repeat(400)] {
        let name = attachment_name(&title);
        assert!(name.len() <= 120, "{} bytes", name.len());
        assert_eq!(name.len(), 120);
    }
}

#[test]
fn a_300_byte_multi_byte_name_clips_on_a_character_boundary() {
    // Two-byte characters: 150 of them are 300 bytes, 60 fit.
    let two = "é".repeat(150);
    assert_eq!(two.len(), 300);
    assert_eq!(attachment_name(&two), "é".repeat(60));

    // Three-byte characters after one byte: 1 + 3 * 39 = 118 fits, one more would be 121.
    let three = format!("a{}", "€".repeat(100));
    assert_eq!(three.len(), 301);
    let clipped = attachment_name(&three);
    assert_eq!(clipped, format!("a{}", "€".repeat(39)));
    assert_eq!(clipped.len(), 118);

    // The 300-byte name survives being written under its clipped name.
    let dir = TempDir::new().expect("temp dir");
    let written = write_attachment(dir.path(), &two, b"x", false).expect("write");
    assert_eq!(written.name, "é".repeat(60));
    assert!(dir.path().join(&written.path).is_file());
}

#[test]
fn name_trims_again_after_clipping() {
    // The cut lands just after a space, which must not be left at the end.
    let name = format!("{} {}", "a".repeat(119), "b".repeat(10));
    assert_eq!(attachment_name(&name), "a".repeat(119));
}

#[test]
fn id_is_six_alphanumerics_and_varies() {
    let all: HashSet<String> = (0..50).map(|_| attachment_id()).collect();
    assert!(all.iter().all(|id| is_id(id)), "{all:?}");
    assert!(all.len() > 40);
}

// attachment storage

#[test]
fn writes_text_and_binary_attachments_inside_unique_directories() {
    let root = TempDir::new().expect("temp dir");
    let root = root.path();
    let name = "Transcript of Select product colors.md";

    let written = write_attachment(root, name, b"# hi\n", false).expect("write");
    assert!(written.path.starts_with(".context/attachments/"));
    let parts: Vec<&str> = written.path.split('/').collect();
    assert_eq!(parts.len(), 4);
    assert!(is_id(parts[2]));
    assert_eq!(parts[2], written.id);
    assert_eq!(parts[3], name);
    assert_eq!(written.name, name);
    assert_eq!(written.bytes, 5);
    assert_eq!(fs::read(root.join(&written.path)).unwrap(), b"# hi\n");
    assert_eq!(
        written.token,
        attachment_token(&written.name, &written.path)
    );

    let evil = write_attachment(root, "../../../../tmp/pwned.md", b"x", false).expect("write");
    assert_eq!(evil.name, "-..-..-..-tmp-pwned.md");
    let target = root.join(&evil.path).canonicalize().unwrap();
    assert!(target.starts_with(root.canonicalize().unwrap()));
    assert_eq!(fs::read(&target).unwrap(), b"x");

    let second = write_attachment(root, name, b"# other\n", false).expect("write");
    assert_eq!(fs::read(root.join(&written.path)).unwrap(), b"# hi\n");
    assert_ne!(second.id, written.id);
    assert_eq!(fs::read(root.join(&second.path)).unwrap(), b"# other\n");

    let png = [0x89, 0x50, 0x4e, 0x47];
    let image = write_attachment(root, "image.png", &png, false).expect("write");
    assert_eq!(fs::read(root.join(&image.path)).unwrap(), png);
    assert_eq!(image.bytes, 4);
}

#[test]
fn written_serialises_with_its_field_names() {
    let written = Written {
        id: "jOTeCX".into(),
        name: "image.png".into(),
        path: ".context/attachments/jOTeCX/image.png".into(),
        bytes: 3,
        token: "t".into(),
    };
    assert_eq!(
        serde_json::to_value(&written).unwrap(),
        serde_json::json!({
            "id": "jOTeCX",
            "name": "image.png",
            "path": ".context/attachments/jOTeCX/image.png",
            "bytes": 3,
            "token": "t"
        })
    );
}

#[test]
fn private_writes_use_owner_only_modes() {
    let root = TempDir::new().expect("temp dir");
    let staging = root.path().join("staging");
    let written = write_attachment(&staging, "secret.txt", b"s", true).expect("write");

    let file = staging.join(&written.path);
    assert_eq!(mode(&file), 0o600);
    assert_eq!(mode(file.parent().unwrap()), 0o700);
    assert_eq!(mode(&staging.join(ATTACHMENTS_DIR)), 0o700);
    assert_eq!(mode(&staging.join(".context")), 0o700);
}

#[test]
fn public_writes_use_the_default_modes() {
    let root = TempDir::new().expect("temp dir");
    let written = write_attachment(root.path(), "plain.txt", b"s", false).expect("write");
    let file = root.path().join(&written.path);
    // Not owner-only: the default modes are 0666 and 0777 less the umask.
    assert_ne!(mode(&file), 0o600);
    assert_ne!(mode(file.parent().unwrap()), 0o700);
}

// staging, materialising, discarding

#[test]
fn stages_materialises_and_discards_an_attachment() {
    let root = TempDir::new().expect("temp dir");
    let staging = root.path().join("staging");
    let worktree = root.path().join("worktree");
    let staged = write_attachment(
        &staging,
        "ship plans (final).md",
        b"# Power circuits\n",
        true,
    )
    .unwrap();

    assert_eq!(staged.token, attachment_token(&staged.name, &staged.path));
    let found = staged_attachments(&staging, &ids(&[&staged])).expect("staged");
    assert_eq!(found, vec![staged.clone()]);

    materialize(&found, &staging, &worktree).expect("materialise");
    assert_eq!(
        fs::read_to_string(worktree.join(&staged.path)).unwrap(),
        "# Power circuits\n"
    );
    // Materialising copies: the staged file is still there.
    assert!(staged_attachments(&staging, &ids(&[&staged])).is_some());

    assert!(discard_staged(&staging, &staged.id));
    assert!(staged_attachments(&staging, &ids(&[&staged])).is_none());
    assert!(!discard_staged(&staging, &staged.id));
    // The worktree copy is not touched.
    assert!(worktree.join(&staged.path).is_file());
}

#[test]
fn materialise_accepts_an_identical_existing_file() {
    let root = TempDir::new().expect("temp dir");
    let staging = root.path().join("staging");
    let worktree = root.path().join("worktree");
    let staged = write_attachment(&staging, "a.md", b"same", true).unwrap();
    let found = staged_attachments(&staging, &ids(&[&staged])).unwrap();

    materialize(&found, &staging, &worktree).expect("first");
    materialize(&found, &staging, &worktree).expect("identical file already there");
    assert_eq!(fs::read(worktree.join(&staged.path)).unwrap(), b"same");
}

#[test]
fn materialise_refuses_a_different_existing_file_and_leaves_it() {
    let root = TempDir::new().expect("temp dir");
    let staging = root.path().join("staging");
    let worktree = root.path().join("worktree");
    let staged = write_attachment(&staging, "a.md", b"staged", true).unwrap();
    let found = staged_attachments(&staging, &ids(&[&staged])).unwrap();

    let existing = worktree.join(&staged.path);
    fs::create_dir_all(existing.parent().unwrap()).unwrap();
    fs::write(&existing, b"different").unwrap();

    let error = materialize(&found, &staging, &worktree).unwrap_err();
    assert_eq!(
        error,
        format!("an attachment already exists at {}", staged.path)
    );
    assert_eq!(fs::read(&existing).unwrap(), b"different");
}

#[test]
fn a_failed_copy_leaves_no_partial_file() {
    let root = TempDir::new().expect("temp dir");
    let staging = root.path().join("staging");
    let worktree = root.path().join("worktree");
    let staged = write_attachment(&staging, "a.md", b"staged", true).unwrap();
    let found = staged_attachments(&staging, &ids(&[&staged])).unwrap();

    let source = staging.join(&staged.path);
    let aside = root.path().join("aside.md");
    fs::rename(&source, &aside).unwrap();

    assert!(materialize(&found, &staging, &worktree).is_err());
    assert!(!worktree.join(&staged.path).exists());

    fs::rename(&aside, &source).unwrap();
    materialize(&found, &staging, &worktree).expect("second try after the source is back");
    assert_eq!(fs::read(worktree.join(&staged.path)).unwrap(), b"staged");
}

#[test]
fn materialise_creates_a_worktree_file_with_default_modes() {
    let root = TempDir::new().expect("temp dir");
    let staging = root.path().join("staging");
    let worktree = root.path().join("worktree");
    let staged = write_attachment(&staging, "a.md", b"x", true).unwrap();
    materialize(std::slice::from_ref(&staged), &staging, &worktree).unwrap();
    assert_ne!(mode(&worktree.join(&staged.path)), 0o600);
}

#[test]
fn materialise_refuses_a_path_that_leaves_the_worktree() {
    let root = TempDir::new().expect("temp dir");
    let staging = root.path().join("staging");
    let worktree = root.path().join("worktree");
    fs::create_dir_all(&worktree).unwrap();
    for path in ["../escape.md", "/etc/escape.md"] {
        let staged = Written {
            id: "abcdef".into(),
            name: "escape.md".into(),
            path: path.into(),
            bytes: 0,
            token: String::new(),
        };
        assert!(
            materialize(&[staged], &staging, &worktree).is_err(),
            "{path}"
        );
    }
    assert!(!root.path().join("escape.md").exists());
}

#[test]
fn staged_attachments_refuses_a_duplicate_id() {
    let root = TempDir::new().expect("temp dir");
    let staged = write_attachment(root.path(), "a.md", b"x", true).unwrap();
    assert!(staged_attachments(root.path(), std::slice::from_ref(&staged.id)).is_some());
    assert!(staged_attachments(root.path(), &[staged.id.clone(), staged.id.clone()]).is_none());
}

#[test]
fn staged_attachments_refuses_a_malformed_id() {
    let root = TempDir::new().expect("temp dir");
    let staged = write_attachment(root.path(), "a.md", b"x", true).unwrap();
    for bad in [
        "",
        "abc",
        "abcdefg",
        "../abc",
        "ab/def",
        "abc.ef",
        "abcdé1",
        &format!("{}/", staged.id),
    ] {
        assert!(
            staged_attachments(root.path(), &[bad.to_owned()]).is_none(),
            "{bad:?}"
        );
        assert!(!discard_staged(root.path(), bad), "{bad:?}");
    }
    // One bad id spoils the whole list.
    assert!(staged_attachments(root.path(), &[staged.id.clone(), "nope".into()]).is_none());
    // A well-formed id nothing was staged under.
    assert!(staged_attachments(root.path(), &["AAAAAA".into()]).is_none());
}

#[test]
fn staged_attachments_refuses_a_directory_with_two_files() {
    let root = TempDir::new().expect("temp dir");
    let staged = write_attachment(root.path(), "a.md", b"x", true).unwrap();
    let dir = root.path().join(&staged.path);
    fs::write(dir.parent().unwrap().join("b.md"), b"y").unwrap();
    assert!(staged_attachments(root.path(), std::slice::from_ref(&staged.id)).is_none());
}

#[test]
fn staged_attachments_refuses_an_empty_directory_and_an_unsafe_file_name() {
    let root = TempDir::new().expect("temp dir");
    let base = root.path().join(ATTACHMENTS_DIR);
    fs::create_dir_all(base.join("empty1")).unwrap();
    assert!(staged_attachments(root.path(), &["empty1".into()]).is_none());

    fs::create_dir_all(base.join("hidden")).unwrap();
    fs::write(base.join("hidden").join(".dot.md"), b"x").unwrap();
    assert!(staged_attachments(root.path(), &["hidden".into()]).is_none());
}

#[test]
fn staged_attachments_reports_size_and_keeps_the_given_order() {
    let root = TempDir::new().expect("temp dir");
    let first = write_attachment(root.path(), "one.txt", b"12345", true).unwrap();
    let second = write_attachment(root.path(), "two.txt", b"1", true).unwrap();
    let found = staged_attachments(root.path(), &ids(&[&second, &first])).unwrap();
    assert_eq!(found, vec![second, first]);
    assert_eq!(found[0].bytes, 1);
    assert_eq!(found[1].bytes, 5);
}

#[test]
fn prunes_only_old_unreferenced_staging_directories() {
    let root = TempDir::new().expect("temp dir");
    let staging = root.path().join("staging");
    let old = write_attachment(&staging, "old.txt", b"old", true).unwrap();
    let kept = write_attachment(&staging, "kept.txt", b"kept", true).unwrap();
    let recent = write_attachment(&staging, "recent.txt", b"recent", true).unwrap();
    for attachment in [&old, &kept] {
        let dir = staging.join(&attachment.path);
        age(dir.parent().unwrap(), Duration::from_secs(10));
    }

    let keep = HashSet::from([kept.id.clone()]);
    assert_eq!(prune_staged(&staging, Duration::from_secs(5), &keep), 1);
    assert!(staged_attachments(&staging, &ids(&[&old])).is_none());
    assert!(staged_attachments(&staging, &ids(&[&kept])).is_some());
    assert!(staged_attachments(&staging, &ids(&[&recent])).is_some());
}

#[test]
fn prune_leaves_directories_of_another_shape_and_stray_files() {
    let root = TempDir::new().expect("temp dir");
    let base = root.path().join(ATTACHMENTS_DIR);
    for name in ["short", "toolong1", "has.dot", "ab-def"] {
        fs::create_dir_all(base.join(name)).unwrap();
        fs::write(base.join(name).join("f"), b"x").unwrap();
        age(&base.join(name), Duration::from_secs(60));
    }
    fs::write(base.join("abcdef"), b"a file, not a directory").unwrap();

    assert_eq!(
        prune_staged(root.path(), Duration::from_secs(5), &HashSet::new()),
        0
    );
    for name in ["short", "toolong1", "has.dot", "ab-def", "abcdef"] {
        assert!(base.join(name).exists(), "{name}");
    }
}

#[test]
fn prune_with_no_staging_directory_removes_nothing() {
    let root = TempDir::new().expect("temp dir");
    assert_eq!(
        prune_staged(
            &root.path().join("missing"),
            Duration::ZERO,
            &HashSet::new()
        ),
        0
    );
}
