use std::fs::{self, File};
use std::os::unix::fs::symlink;
use std::path::Path;
use std::sync::Arc;

use conductor_remote::artifacts::files::*;
use conductor_remote::artifacts::{ArtifactError, CreateArtifactPreviewRequest};
use tempfile::TempDir;

struct Fixture {
    temp: TempDir,
    policy: Arc<ArtifactFilePolicy>,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("project")).unwrap();
        let policy = Arc::new(
            ArtifactFilePolicy::new(&[temp.path().to_owned()], &[temp.path().to_owned()]).unwrap(),
        );
        Self { temp, policy }
    }

    fn project(&self) -> std::path::PathBuf {
        self.temp.path().join("project")
    }

    fn write(&self, name: &str, bytes: impl AsRef<[u8]>) {
        let path = self.project().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn document(&self) -> PreparedArtifact {
        self.policy
            .prepare_file(self.project().join("index.html").to_str().unwrap(), None)
            .unwrap()
    }
}

#[tokio::test]
async fn full_original_document_suffix_home_encoding_and_async_reads() {
    let fixture = Fixture::new();
    let source = format!(
        "\u{feff}<!DOCTYPE html>\r\n<meta charset=utf-8>\r\n{}<p>last</p>",
        "line\r\n".repeat(700)
    );
    fixture.write("a #é.html", &source);
    let reference = format!("{}:601:3", fixture.project().join("a #é.html").display());
    let document = prepare_document(
        fixture.policy.clone(),
        CreateArtifactPreviewRequest::File {
            reference,
            parent_origin: "http://localhost:8790".into(),
        },
        None,
    )
    .await
    .unwrap();
    assert_eq!(document.source.content, source);
    assert_eq!(document.entry_path, "/a%20%23%C3%A9.html");
    assert_eq!(
        document.source.path.as_deref(),
        fixture
            .project()
            .join("a #é.html")
            .canonicalize()
            .unwrap()
            .to_str()
    );
    let expanded = fixture
        .policy
        .prepare_file("~/project/a #é.html:601", Some(fixture.temp.path()))
        .unwrap();
    assert_eq!(expanded.identity, document.identity);
    let response = document
        .root
        .read_resource(document.entry_path.clone(), None, false)
        .await
        .unwrap();
    assert_eq!(response.body, source.as_bytes());
    assert_eq!(response.mime, "text/html");
    assert_eq!(response.status, 200);
    assert_eq!(RESOURCE_HEADERS[0], ("X-Content-Type-Options", "nosniff"));
    assert!(RESOURCE_HEADERS.contains(&("Cache-Control", "no-store")));
}

#[test]
fn inline_empty_root_identity_and_authorized_base() {
    let fixture = Fixture::new();
    fixture.write("module.js", "export default 1;");
    let empty = fixture
        .policy
        .prepare_inline("<!doctype html><p>inline</p>".into(), None)
        .unwrap();
    assert!(empty.source.path.is_none());
    assert!(empty.root.canonical_path().is_none());
    assert_eq!(empty.entry_path, "/index.html");
    assert!(matches!(
        empty.root.read_resource_blocking("/module.js", None, false),
        Err(ArtifactError::Missing(_))
    ));
    let based = fixture
        .policy
        .prepare_inline(empty.source.content.clone(), Some(&fixture.project()))
        .unwrap();
    assert_ne!(empty.identity, based.identity);
    assert_eq!(
        based.identity,
        fixture
            .policy
            .prepare_inline(empty.source.content, Some(&fixture.project()))
            .unwrap()
            .identity
    );
    assert_ne!(
        based.identity,
        fixture
            .policy
            .prepare_inline("different".into(), Some(&fixture.project()))
            .unwrap()
            .identity
    );
    assert_eq!(
        based
            .root
            .read_resource_blocking("/module.js", None, false)
            .unwrap()
            .body,
        b"export default 1;"
    );
    assert!(fixture
        .policy
        .prepare_inline("x".into(), Some(Path::new("relative")))
        .is_err());
    assert!(fixture
        .policy
        .prepare_inline("x".into(), Some(&fixture.project().join("missing")))
        .is_err());
    assert!(matches!(
        fixture
            .policy
            .prepare_inline("x".into(), Some(fixture.temp.path())),
        Err(ArtifactError::Forbidden(_))
    ));
    let outside = tempfile::tempdir().unwrap();
    assert!(matches!(
        fixture
            .policy
            .prepare_inline("x".into(), Some(outside.path())),
        Err(ArtifactError::Forbidden(_))
    ));
}

#[test]
fn safe_context_resources_mime_and_no_listing() {
    let fixture = Fixture::new();
    fixture.write("index.html", "<p>ok</p>");
    for (path, content, mime) in [
        (".context/plot/data.json", "{}", "application/json"),
        ("style.css", "@import './more.css';", "text/css"),
        ("module.mjs", "export {};", "text/javascript"),
        ("worker.js", "self.onmessage=()=>{}", "text/javascript"),
        ("module.wasm", "wasm", "application/wasm"),
        ("font.woff2", "font", "font/woff2"),
        ("movie.mp4", "movie", "video/mp4"),
        ("unknown.xyzabc", "unknown", "application/octet-stream"),
    ] {
        fixture.write(path, content);
        let response = fixture
            .document()
            .root
            .read_resource_blocking(&format!("/{path}"), None, false)
            .unwrap();
        assert_eq!(response.mime, mime, "{path}");
        assert_eq!(response.body, content.as_bytes());
    }
    for path in ["/", "/.context/", "/.context", "/.context/plot"] {
        assert!(
            fixture
                .document()
                .root
                .read_resource_blocking(path, None, false)
                .is_err(),
            "{path}"
        );
    }
}

#[test]
fn protected_paths_traversal_decoding_and_types_are_refused() {
    let fixture = Fixture::new();
    fixture.write("index.html", "<p>ok</p>");
    let document = fixture.document();
    for path in [
        "/../outside.txt",
        "/%2e%2e/outside.txt",
        "/a/../index.html",
        "/a%2fb",
        "/a%5cb",
        "/a%00b",
        "//index.html",
        "index.html",
        "/./index.html",
        "/index.html?x=1",
        "/.git/config",
        "/.ssh/id_rsa",
        "/.aws/credentials",
        "/.env",
        "/.ENV.local",
        "/.env-production",
        "/private.key",
        "/cert.pem",
        "/backup.zip",
        "/secret.p12",
        "/.npmrc",
        "/agents.local.md",
        "/local.properties",
        "/gradle.properties",
        "/.envrc",
        "/__conductor_preview/bootstrap",
        "/%2egit/config",
    ] {
        assert!(
            matches!(
                document.root.read_resource_blocking(path, None, false),
                Err(ArtifactError::Forbidden(_))
            ),
            "{path}"
        );
    }
    assert!(matches!(
        document.root.read_resource_blocking("/%Q0", None, false),
        Err(ArtifactError::Invalid(_))
    ));
    fixture.write("%2e%2e/literal.txt", "literal percent name");
    assert_eq!(
        document
            .root
            .read_resource_blocking("/%252e%252e/literal.txt", None, false)
            .unwrap()
            .body,
        b"literal percent name"
    );
    fixture.write(".git/page.html", "synthetic protected fixture");
    assert!(matches!(
        fixture.policy.prepare_file(
            fixture.project().join(".git/page.html").to_str().unwrap(),
            None
        ),
        Err(ArtifactError::Forbidden(_))
    ));
    fixture.write("not-html.txt", "text");
    assert!(matches!(
        fixture.policy.prepare_file(
            fixture.project().join("not-html.txt").to_str().unwrap(),
            None
        ),
        Err(ArtifactError::Unsupported(_))
    ));
    let fifo =
        std::ffi::CString::new(fixture.project().join("fifo.json").to_str().unwrap()).unwrap();
    // SAFETY: fixture-only, NUL-terminated path; no reader/writer is created for the FIFO.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(matches!(
        document
            .root
            .read_resource_blocking("/fifo.json", None, false),
        Err(ArtifactError::Forbidden(_))
    ));
}

#[test]
fn pinned_directory_survives_ancestor_replacement_and_rejects_component_links() {
    let fixture = Fixture::new();
    fixture.write("index.html", "original");
    fixture.write("assets/data.json", "original resource");
    let document = fixture.document();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("index.html"), "outside").unwrap();
    fs::write(outside.path().join("data.json"), "outside resource").unwrap();
    symlink(
        outside.path().join("data.json"),
        fixture.project().join("linked.json"),
    )
    .unwrap();
    symlink(outside.path(), fixture.project().join("linked-dir")).unwrap();
    symlink(
        fixture.project().join("assets/data.json"),
        fixture.project().join("safe-link.json"),
    )
    .unwrap();
    for path in ["/linked.json", "/linked-dir/data.json", "/safe-link.json"] {
        assert!(
            matches!(
                document.root.read_resource_blocking(path, None, false),
                Err(ArtifactError::Forbidden(_))
            ),
            "{path}"
        );
    }
    fs::rename(
        fixture.project().join("assets"),
        fixture.project().join("old-assets"),
    )
    .unwrap();
    symlink(outside.path(), fixture.project().join("assets")).unwrap();
    assert!(matches!(
        document
            .root
            .read_resource_blocking("/assets/data.json", None, false),
        Err(ArtifactError::Forbidden(_))
    ));
    let original = fixture.temp.path().join("original-project");
    fs::rename(fixture.project(), &original).unwrap();
    symlink(outside.path(), fixture.project()).unwrap();
    assert_eq!(
        document
            .root
            .read_resource_blocking("/index.html", None, false)
            .unwrap()
            .body,
        b"original"
    );
    assert_eq!(
        document
            .root
            .read_resource_blocking("/old-assets/data.json", None, false)
            .unwrap()
            .body,
        b"original resource"
    );
    assert!(matches!(
        fixture
            .policy
            .prepare_file(fixture.project().join("index.html").to_str().unwrap(), None),
        Err(ArtifactError::Forbidden(_))
    ));
}

#[test]
fn source_root_descriptor_is_pinned_before_document_preparation() {
    let fixture = Fixture::new();
    fixture.write("index.html", "original authorized inode");
    let renamed = fixture.temp.path().with_extension("moved");
    // Keep the moved directory under a second TempDir's ownership for automatic cleanup.
    let holder = tempfile::tempdir().unwrap();
    let renamed = holder.path().join(renamed.file_name().unwrap());
    fs::rename(fixture.temp.path(), &renamed).unwrap();
    fs::create_dir(fixture.temp.path()).unwrap();
    fs::create_dir(fixture.project()).unwrap();
    fs::write(fixture.project().join("index.html"), "replacement inode").unwrap();
    assert_eq!(
        fixture.document().source.content,
        "original authorized inode"
    );
}

#[test]
fn exact_size_limits_and_non_utf8_source() {
    let fixture = Fixture::new();
    fixture.write("index.html", vec![b'x'; DOCUMENT_MAX_BYTES as usize]);
    assert_eq!(
        fixture.document().source.content.len() as u64,
        DOCUMENT_MAX_BYTES
    );
    File::options()
        .write(true)
        .open(fixture.project().join("index.html"))
        .unwrap()
        .set_len(DOCUMENT_MAX_BYTES + 1)
        .unwrap();
    assert!(matches!(
        fixture
            .policy
            .prepare_file(fixture.project().join("index.html").to_str().unwrap(), None),
        Err(ArtifactError::TooLarge(_))
    ));
    assert!(matches!(
        fixture
            .policy
            .prepare_inline("x".repeat(DOCUMENT_MAX_BYTES as usize + 1), None),
        Err(ArtifactError::TooLarge(_))
    ));
    fixture.write("index.html", [0xff, 0xfe]);
    assert!(matches!(
        fixture
            .policy
            .prepare_file(fixture.project().join("index.html").to_str().unwrap(), None),
        Err(ArtifactError::Unsupported(_))
    ));
    fixture.write("index.html", "ok");
    let document = fixture.document();
    let resource = File::create(fixture.project().join("large.mp4")).unwrap();
    resource.set_len(RESOURCE_MAX_BYTES).unwrap();
    assert_eq!(
        document
            .root
            .read_resource_blocking("/large.mp4", None, true)
            .unwrap()
            .content_length,
        RESOURCE_MAX_BYTES
    );
    assert_eq!(
        document
            .root
            .read_resource_blocking("/large.mp4", Some("bytes=33554431-"), false)
            .unwrap()
            .body,
        [0]
    );
    resource.set_len(RESOURCE_MAX_BYTES + 1).unwrap();
    for head in [true, false] {
        assert!(matches!(
            document
                .root
                .read_resource_blocking("/large.mp4", Some("bytes=0-0"), head),
            Err(ArtifactError::TooLarge(_))
        ));
    }
}

#[test]
fn byte_ranges_head_and_empty_media() {
    let fixture = Fixture::new();
    fixture.write("index.html", "ok");
    fixture.write("movie.mp4", "0123456789");
    fixture.write("empty.mp4", "");
    let document = fixture.document();
    for (header, expected, content_range) in [
        ("bytes=2-4", "234", "bytes 2-4/10"),
        ("bytes=7-", "789", "bytes 7-9/10"),
        ("bytes=-3", "789", "bytes 7-9/10"),
        ("bytes=-30", "0123456789", "bytes 0-9/10"),
        ("bytes=8-99", "89", "bytes 8-9/10"),
    ] {
        let response = document
            .root
            .read_resource_blocking("/movie.mp4", Some(header), false)
            .unwrap();
        assert_eq!(response.status, 206);
        assert_eq!(response.body, expected.as_bytes());
        assert_eq!(response.content_length, expected.len() as u64);
        assert_eq!(response.content_range.as_deref(), Some(content_range));
    }
    for header in ["bytes=10-", "bytes=-0", "bytes=99-100"] {
        let response = document
            .root
            .read_resource_blocking("/movie.mp4", Some(header), false)
            .unwrap();
        assert_eq!(response.status, 416);
        assert_eq!(response.content_range.as_deref(), Some("bytes */10"));
        assert!(response.body.is_empty());
    }
    for header in [
        "other=0-2",
        "bytes=bad",
        "bytes=4-2",
        "bytes=0-1,4-5",
        "bytes=+0-2",
        "bytes=18446744073709551616-",
    ] {
        assert_eq!(select_range(Some(header), 10), ByteRange::Full);
    }
    let head = document
        .root
        .read_resource_blocking("/movie.mp4", Some("bytes=2-4"), true)
        .unwrap();
    assert_eq!(
        (head.status, head.content_length, head.body.len()),
        (200, 10, 0)
    );
    assert!(head.content_range.is_none());
    assert_eq!(
        document
            .root
            .read_resource_blocking("/empty.mp4", None, false)
            .unwrap()
            .status,
        200
    );
    assert_eq!(
        document
            .root
            .read_resource_blocking("/empty.mp4", Some("bytes=0-"), false)
            .unwrap()
            .status,
        416
    );
}
