//! Which files the phone may preview, and reading them, against files this test creates in
//! temporary directories that stand in for the roots.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use conductor_remote::files::{
    file_preview, is_previewable_image, is_previewable_source, local_image, ExposeMode,
    FilePreview, ImageError, PreviewError, PreviewRoots,
};
use tempfile::TempDir;

const PUBLIC_REFUSAL: &str =
    "this relay is reachable from the internet, so it previews files inside Conductor workspaces only";
const TAILNET_REFUSAL: &str = "outside the files this relay may read";

const TAILNET: ExposeMode = ExposeMode::Tailnet;
const PUBLIC: ExposeMode = ExposeMode::Public;

/// Directories standing in for the workspaces root, the home directory, the bundled skills
/// directory and a temporary directory, plus one that is none of them. The paths are kept as
/// the system temporary directory gives them, so a symbolic link in them (`/var` on a Mac) is
/// part of every test.
struct Fixture {
    _dir: TempDir,
    ws: PathBuf,
    home: PathBuf,
    skills: PathBuf,
    tmp: PathBuf,
    outside: PathBuf,
    roots: PreviewRoots,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let base = dir.path().to_path_buf();
        let [ws, home, skills, tmp, outside] =
            ["ws", "home", "skills", "tmp", "outside"].map(|name| {
                let path = base.join(name);
                fs::create_dir(&path).expect("create root");
                path
            });
        let roots = PreviewRoots::new(&ws, &home, Some(&skills), &[tmp.as_path()]);
        Self {
            _dir: dir,
            ws,
            home,
            skills,
            tmp,
            outside,
            roots,
        }
    }

    fn preview(&self, reference: &str, mode: ExposeMode) -> Result<FilePreview, PreviewError> {
        file_preview(reference, &self.roots, mode)
    }

    fn preview_path(&self, path: &Path, mode: ExposeMode) -> Result<FilePreview, PreviewError> {
        self.preview(path.to_str().expect("utf-8 path"), mode)
    }

    fn image(
        &self,
        path: &Path,
        mode: ExposeMode,
    ) -> Result<conductor_remote::files::LocalImage, ImageError> {
        local_image(path.to_str().expect("utf-8 path"), &self.roots, mode)
    }
}

fn write(path: &Path, content: impl AsRef<[u8]>) {
    fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
    fs::write(path, content).expect("write file");
}

fn numbered(lines: usize) -> String {
    (1..=lines)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ------------------------------------------------------------------ expose mode

#[test]
fn expose_mode_is_public_only_for_public_or_funnel() {
    for value in ["public", "funnel", "PUBLIC", " Funnel ", "\tpublic\n"] {
        assert_eq!(ExposeMode::from_env_value(Some(value)), PUBLIC, "{value:?}");
    }
    for value in ["tailnet", "serve", "private", "", "publicly", "pub lic"] {
        assert_eq!(
            ExposeMode::from_env_value(Some(value)),
            TAILNET,
            "{value:?}"
        );
    }
    assert_eq!(ExposeMode::from_env_value(None), TAILNET);
}

// ------------------------------------------------------------------ extensions

#[test]
fn source_and_image_extensions() {
    for path in [
        "/a/b.ts",
        "/a/b.TSX",
        "/a/b.md",
        "/a/b.svg",
        "/a/b.tar.gz.py",
        "/a/.md",
        "/a/b.YML",
        "/a/b.bash",
        "/a/b.mts",
        "/a/b.sql",
    ] {
        assert!(is_previewable_source(path), "{path}");
    }
    for path in [
        "/a/b",
        "/a/b.",
        "/a/.env",
        "/a/b.png",
        "/a/b.pdf",
        "/a/b.ts:12",
        "/a.ts/b",
        "/a.ts/.",
        "/a.ts/..",
    ] {
        assert!(!is_previewable_source(path), "{path}");
    }
    for path in [
        "/a/b.avif",
        "/a/b.gif",
        "/a/b.jpeg",
        "/a/b.jpg",
        "/a/b.png",
        "/a/b.webp",
        "/a/B.PNG",
    ] {
        assert!(is_previewable_image(path), "{path}");
    }
    for path in [
        "/a/b.svg",
        "/a/b.pdf",
        "/a/b",
        "/a/b.png:12",
        "/a/b.ts",
        "/a/png",
    ] {
        assert!(!is_previewable_image(path), "{path}");
    }
}

// ------------------------------------------------------------------ parsing

#[test]
fn parses_absolute_paths_and_locations() {
    let f = Fixture::new();
    let file = f.ws.join("repo/city/web/src/components/WorkspaceList.tsx");
    write(&file, numbered(600));

    let plain = f.preview_path(&file, PUBLIC).unwrap();
    assert_eq!(plain.path, file.to_str().unwrap());
    assert_eq!(plain.line, None);

    let with_line = f
        .preview(&format!("{}:468", file.display()), PUBLIC)
        .unwrap();
    assert_eq!(with_line.path, file.to_str().unwrap());
    assert_eq!(with_line.line, Some(468));

    let with_column = f
        .preview(&format!("{}:19:7", file.display()), PUBLIC)
        .unwrap();
    assert_eq!(with_column.path, file.to_str().unwrap());
    assert_eq!(with_column.line, Some(19));

    let manifest = f.ws.join("repo/city/package.json");
    write(&manifest, "{}");
    let preview = f.preview_path(&manifest, PUBLIC).unwrap();
    assert_eq!(preview.path, manifest.to_str().unwrap());
    assert_eq!(preview.line, None);
}

#[test]
fn only_the_last_location_suffix_counts() {
    let f = Fixture::new();
    let file = f.ws.join("a.ts");
    write(&file, numbered(30));
    // `:5:6:7` is the line `6` and column `7` of a file named `a.ts:5`, which has no extension.
    assert_eq!(
        f.preview(&format!("{}:5:6:7", file.display()), PUBLIC),
        Err(PreviewError::NotFound)
    );
    // A column of zero, or an empty column, is part of the name.
    assert_eq!(
        f.preview(&format!("{}:5:", file.display()), PUBLIC),
        Err(PreviewError::NotFound)
    );
    assert_eq!(
        f.preview(&format!("{}:5:0", file.display()), PUBLIC)
            .unwrap()
            .line,
        Some(5)
    );
}

#[test]
fn expands_the_home_path_an_agent_writes() {
    let f = Fixture::new();
    write(&f.home.join(".notes/plan.md"), numbered(40));

    let preview = f.preview("~/.notes/plan.md:12", TAILNET).unwrap();
    assert_eq!(
        preview.path,
        f.home.join(".notes/plan.md").to_str().unwrap()
    );
    assert_eq!(preview.line, Some(12));

    // Expanding grants nothing: a public relay still refuses the home directory.
    assert_eq!(
        f.preview("~/.notes/plan.md:12", PUBLIC),
        Err(PreviewError::Forbidden(PUBLIC_REFUSAL))
    );
}

#[test]
fn rejects_unsafe_or_non_file_references() {
    let f = Fixture::new();
    write(&f.ws.join("file.ts"), "x");
    write(&f.home.join("notes.md"), "x");
    let ws = f.ws.display();
    for reference in [
        "/w/a-workspace".to_owned(),
        // Another account's home is not this relay's to expand, and unexpanded it is not a path.
        "~someone/notes.md".to_owned(),
        "~/notes.md/../../../etc/hosts".to_owned(),
        "web/src/app.tsx:19".to_owned(),
        "".to_owned(),
        "~".to_owned(),
        "~/".to_owned(),
        format!("{ws}/.env:1"),
        format!("{ws}/file.ts:0"),
        format!("{ws}/file.ts:007"),
        format!("{ws}/file.ts:9007199254740992"),
        format!("{ws}/file.ts:99999999999999999999999999"),
        format!("{ws}/file.sh.png"),
    ] {
        assert_eq!(
            f.preview(&reference, TAILNET),
            Err(PreviewError::NotFound),
            "{reference}"
        );
    }
    // The largest safe line is a line.
    assert_eq!(
        f.preview(&format!("{ws}/file.ts:9007199254740991"), TAILNET)
            .unwrap()
            .line,
        Some(1)
    );
}

#[test]
fn dot_components_are_resolved_lexically() {
    let f = Fixture::new();
    write(&f.ws.join("repo/b.ts"), "inside");
    write(&f.outside.join("secret.ts"), "secret");
    write(&f.home.join("notes.md"), "home");

    let preview = f
        .preview(&format!("{}/repo/src/../b.ts", f.ws.display()), PUBLIC)
        .unwrap();
    assert_eq!(preview.path, f.ws.join("repo/b.ts").to_str().unwrap());
    assert_eq!(preview.content, "inside");

    let dotted = f
        .preview(&format!("{}/./repo/./b.ts", f.ws.display()), PUBLIC)
        .unwrap();
    assert_eq!(dotted.path, f.ws.join("repo/b.ts").to_str().unwrap());

    // `..` cannot climb out of the workspaces root.
    assert_eq!(
        f.preview(
            &format!("{}/repo/../../outside/secret.ts", f.ws.display()),
            PUBLIC
        ),
        Err(PreviewError::Forbidden(PUBLIC_REFUSAL))
    );
    assert_eq!(
        f.preview(
            &format!("{}/repo/../../outside/secret.ts", f.ws.display()),
            TAILNET
        ),
        Err(PreviewError::Forbidden(TAILNET_REFUSAL))
    );
    // It cannot reach the home directory from the workspaces root in public mode either.
    assert_eq!(
        f.preview(&format!("{}/../home/notes.md", f.ws.display()), PUBLIC),
        Err(PreviewError::Forbidden(PUBLIC_REFUSAL))
    );
    assert!(f
        .preview(&format!("{}/../home/notes.md", f.ws.display()), TAILNET)
        .is_ok());
    // And one written with `~/` is resolved before it is judged.
    assert_eq!(
        f.preview("~/../outside/secret.ts", TAILNET),
        Err(PreviewError::Forbidden(TAILNET_REFUSAL))
    );
    // Climbing above `/` stays at `/`.
    assert_eq!(
        f.preview("/../../etc/config.ts", TAILNET),
        Err(PreviewError::Forbidden(TAILNET_REFUSAL))
    );
}

// ------------------------------------------------------------------ access

#[test]
fn workspace_files_are_allowed_in_both_modes() {
    let f = Fixture::new();
    let file = f.ws.join("repo/city/src/server.ts");
    write(&file, "export {}");
    for mode in [PUBLIC, TAILNET] {
        let preview = f.preview_path(&file, mode).unwrap();
        assert_eq!(preview.content, "export {}");
        assert_eq!(preview.total_lines, 1);
        assert!(!preview.truncated);
    }
}

#[test]
fn home_and_skills_files_are_tailnet_only() {
    let f = Fixture::new();
    let note = f.home.join(".notes/builder-journey.md");
    let skill = f.skills.join("conductor/SKILL.md");
    write(&note, "note");
    write(&skill, "skill");

    assert_eq!(f.preview_path(&note, TAILNET).unwrap().content, "note");
    assert_eq!(f.preview_path(&skill, TAILNET).unwrap().content, "skill");
    assert_eq!(
        f.preview_path(&note, PUBLIC),
        Err(PreviewError::Forbidden(PUBLIC_REFUSAL))
    );
    assert_eq!(
        f.preview_path(&skill, PUBLIC),
        Err(PreviewError::Forbidden(PUBLIC_REFUSAL))
    );
}

#[test]
fn a_relay_without_a_skills_directory_refuses_it() {
    let f = Fixture::new();
    let skill = f.skills.join("conductor/SKILL.md");
    write(&skill, "skill");
    let roots = PreviewRoots::new(&f.ws, &f.home, None, &[]);
    assert_eq!(
        file_preview(skill.to_str().unwrap(), &roots, TAILNET),
        Err(PreviewError::Forbidden(TAILNET_REFUSAL))
    );
}

#[test]
fn files_outside_every_root_are_refused_with_the_mode_message() {
    let f = Fixture::new();
    let outside = f.outside.join("config.ts");
    write(&outside, "x");
    // A temporary directory is for images, not source.
    let temp_source = f.tmp.join("scratch.ts");
    write(&temp_source, "x");

    for path in [&outside, &temp_source] {
        assert_eq!(
            f.preview_path(path, TAILNET),
            Err(PreviewError::Forbidden(TAILNET_REFUSAL)),
            "{path:?}"
        );
        assert_eq!(
            f.preview_path(path, PUBLIC),
            Err(PreviewError::Forbidden(PUBLIC_REFUSAL)),
            "{path:?}"
        );
    }
    assert_eq!(
        f.preview("/etc/config.ts", TAILNET),
        Err(PreviewError::Forbidden(TAILNET_REFUSAL))
    );
}

#[test]
fn a_missing_file_answers_as_an_existing_one_in_the_same_place() {
    let f = Fixture::new();
    write(&f.outside.join("here.ts"), "x");
    write(&f.home.join("here.md"), "x");
    write(&f.ws.join("here.ts"), "x");

    let places: [(&Path, &str); 4] = [
        (&f.ws, "ts"),
        (&f.home, "md"),
        (&f.outside, "ts"),
        (Path::new("/nonexistent-root"), "ts"),
    ];
    for mode in [PUBLIC, TAILNET] {
        for (dir, ext) in places {
            let present = f.preview_path(&dir.join(format!("here.{ext}")), mode);
            let absent = f.preview_path(&dir.join(format!("gone.{ext}")), mode);
            if dir == f.outside || dir == Path::new("/nonexistent-root") {
                assert!(
                    matches!(
                        present,
                        Err(PreviewError::Forbidden(_)) | Err(PreviewError::NotFound)
                    ),
                    "{dir:?}"
                );
            }
            match (&present, &absent) {
                (Ok(_), Err(PreviewError::NotFound)) => {}
                (Err(a), Err(b)) => assert_eq!(a, b, "{dir:?} {mode:?}"),
                other => panic!("{dir:?} {mode:?}: {other:?}"),
            }
        }
    }
    // A missing home file is NotFound where home is readable and Forbidden where it is not.
    assert_eq!(
        f.preview_path(&f.home.join("gone.md"), TAILNET),
        Err(PreviewError::NotFound)
    );
    assert_eq!(
        f.preview_path(&f.home.join("gone.md"), PUBLIC),
        Err(PreviewError::Forbidden(PUBLIC_REFUSAL))
    );
    assert_eq!(
        f.preview_path(&f.ws.join("gone.ts"), PUBLIC),
        Err(PreviewError::NotFound)
    );
}

#[test]
fn a_directory_is_not_found() {
    let f = Fixture::new();
    let dir = f.ws.join("pkg.ts");
    fs::create_dir_all(&dir).unwrap();
    for mode in [PUBLIC, TAILNET] {
        assert_eq!(f.preview_path(&dir, mode), Err(PreviewError::NotFound));
    }
    // One outside the roots is still refused rather than reported as a directory.
    let outside_dir = f.outside.join("pkg.ts");
    fs::create_dir_all(&outside_dir).unwrap();
    assert_eq!(
        f.preview_path(&outside_dir, PUBLIC),
        Err(PreviewError::Forbidden(PUBLIC_REFUSAL))
    );
}

#[test]
fn a_missing_file_under_a_symbolic_linked_root_is_not_found() {
    let f = Fixture::new();
    let real = f.outside.join("real-workspaces");
    fs::create_dir_all(&real).unwrap();
    let link = f.outside.join("linked-workspaces");
    symlink(&real, &link).unwrap();
    let real = real.canonicalize().unwrap();
    write(&real.join("here.ts"), "x");

    // Whichever of the two spellings the root is given as, and the path is written in.
    for root in [&link, &real] {
        let roots = PreviewRoots::new(root, &f.home, None, &[]);
        for dir in [&link, &real] {
            let missing = dir.join("repo/gone.ts");
            let present = dir.join("here.ts");
            // A missing file is judged by its path against the root as given and canonicalised,
            // so the spelling through the link only matches a root given as the link.
            let judged = root == &link || dir == &real;
            for mode in [PUBLIC, TAILNET] {
                if judged {
                    assert_eq!(
                        file_preview(missing.to_str().unwrap(), &roots, mode),
                        Err(PreviewError::NotFound),
                        "root {root:?}, path {missing:?}"
                    );
                }
                assert!(
                    file_preview(present.to_str().unwrap(), &roots, mode).is_ok(),
                    "root {root:?}, path {present:?}"
                );
            }
        }
    }
}

#[test]
fn a_workspaces_root_that_does_not_exist_yet_is_used_as_given() {
    let f = Fixture::new();
    let later = f.outside.join("not-yet");
    let roots = PreviewRoots::new(&later, &f.home, None, &[]);
    let missing = later.join("repo/a.ts");
    assert_eq!(
        file_preview(missing.to_str().unwrap(), &roots, PUBLIC),
        Err(PreviewError::NotFound)
    );
    let elsewhere = f.outside.join("a.ts");
    assert_eq!(
        file_preview(elsewhere.to_str().unwrap(), &roots, PUBLIC),
        Err(PreviewError::Forbidden(PUBLIC_REFUSAL))
    );
    // Once the directory exists, the same file reads.
    write(&later.join("repo/a.ts"), "now here");
    assert_eq!(
        file_preview(missing.to_str().unwrap(), &roots, PUBLIC)
            .unwrap()
            .content,
        "now here"
    );
}

#[test]
fn a_link_out_of_a_root_answers_like_a_missing_file() {
    let f = Fixture::new();
    let target = f.outside.join("secret.ts");
    write(&target, "secret");
    let link = f.ws.join("repo/innocent.ts");
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    symlink(&target, &link).unwrap();
    // A link to a directory outside is no better.
    let dir_link = f.ws.join("repo/dir");
    symlink(&f.outside, &dir_link).unwrap();

    for path in [link.clone(), dir_link.join("secret.ts")] {
        for mode in [PUBLIC, TAILNET] {
            assert_eq!(
                f.preview_path(&path, mode),
                Err(PreviewError::NotFound),
                "{path:?}"
            );
        }
    }
    // A path written outside every root is refused whether or not it exists.
    for path in [target.clone(), f.outside.join("gone.ts")] {
        assert_eq!(
            f.preview_path(&path, PUBLIC),
            Err(PreviewError::Forbidden(PUBLIC_REFUSAL)),
            "{path:?}"
        );
        assert_eq!(
            f.preview_path(&path, TAILNET),
            Err(PreviewError::Forbidden(TAILNET_REFUSAL)),
            "{path:?}"
        );
    }
}

#[test]
fn an_existing_and_a_missing_file_behind_the_same_link_get_equal_answers() {
    let f = Fixture::new();
    write(&f.outside.join("present.ts"), "secret");
    write(&f.outside.join("present.png"), "x");
    let dir_link = f.ws.join("repo/out");
    fs::create_dir_all(dir_link.parent().unwrap()).unwrap();
    symlink(&f.outside, &dir_link).unwrap();

    for mode in [PUBLIC, TAILNET] {
        assert_eq!(
            f.preview_path(&dir_link.join("present.ts"), mode),
            f.preview_path(&dir_link.join("missing.ts"), mode)
        );
        assert_eq!(
            f.image(&dir_link.join("present.png"), mode).map(|_| ()),
            f.image(&dir_link.join("missing.png"), mode).map(|_| ())
        );
        assert_eq!(
            f.image(&dir_link.join("present.png"), mode).map(|_| ()),
            Err(ImageError::NotFound)
        );
    }
}

#[test]
fn a_symbolic_link_decides_by_where_it_leads() {
    let f = Fixture::new();
    // From the workspaces into the home directory: readable on a tailnet only.
    let note = f.home.join("notes.md");
    write(&note, "note");
    let link = f.ws.join("notes.md");
    symlink(&note, &link).unwrap();
    assert_eq!(f.preview_path(&link, TAILNET).unwrap().content, "note");
    assert_eq!(
        f.preview_path(&link, PUBLIC),
        Err(PreviewError::Forbidden(PUBLIC_REFUSAL))
    );
    // From outside into the workspaces: readable in both.
    let inner = f.ws.join("inner.ts");
    write(&inner, "inner");
    let outer = f.outside.join("outer.ts");
    symlink(&inner, &outer).unwrap();
    for mode in [PUBLIC, TAILNET] {
        let preview = f.preview_path(&outer, mode).unwrap();
        // `path` is the one the chat wrote, not the real one.
        assert_eq!(preview.path, outer.to_str().unwrap());
        assert_eq!(preview.content, "inner");
    }
    // A link that leads nowhere is judged by its own path.
    let dangling = f.ws.join("dangling.ts");
    symlink(f.ws.join("nowhere.ts"), &dangling).unwrap();
    assert_eq!(
        f.preview_path(&dangling, PUBLIC),
        Err(PreviewError::NotFound)
    );
}

#[test]
fn look_alike_prefixes_are_not_inside_a_root() {
    let f = Fixture::new();
    let sibling = |name: &str| {
        let mut os = f.ws.clone().into_os_string();
        os.push(name);
        PathBuf::from(os)
    };
    for suffix in ["-other", "2", ".old"] {
        let path = sibling(suffix).join("repo/a.ts");
        write(&path, "x");
        assert_eq!(
            f.preview_path(&path, PUBLIC),
            Err(PreviewError::Forbidden(PUBLIC_REFUSAL)),
            "{suffix}"
        );
        // Missing, in the same place: the same answer.
        assert_eq!(
            f.preview_path(&sibling(suffix).join("repo/gone.ts"), PUBLIC),
            Err(PreviewError::Forbidden(PUBLIC_REFUSAL)),
            "{suffix}"
        );
    }
    // The same for home and skills in tailnet mode.
    for root in [&f.home, &f.skills] {
        let mut os = root.clone().into_os_string();
        os.push("-other");
        let path = PathBuf::from(os).join("a.ts");
        write(&path, "x");
        assert_eq!(
            f.preview_path(&path, TAILNET),
            Err(PreviewError::Forbidden(TAILNET_REFUSAL)),
            "{root:?}"
        );
    }
}

#[test]
fn a_root_itself_is_not_a_file_inside_it() {
    let f = Fixture::new();
    // A workspaces root whose own name looks like a source file.
    let root = f.outside.join("root.ts");
    fs::create_dir(&root).unwrap();
    let roots = PreviewRoots::new(&root, &f.home, None, &[]);
    assert_eq!(
        file_preview(root.to_str().unwrap(), &roots, PUBLIC),
        Err(PreviewError::Forbidden(PUBLIC_REFUSAL))
    );
}

// ------------------------------------------------------------------ size and content

#[test]
fn a_source_over_512_kib_is_too_large() {
    let f = Fixture::new();
    let limit = 512 * 1024;
    let exact = f.ws.join("exact.txt");
    write(&exact, vec![b'a'; limit]);
    assert_eq!(f.preview_path(&exact, PUBLIC).unwrap().total_lines, 1);

    let over = f.ws.join("over.txt");
    write(&over, vec![b'a'; limit + 1]);
    assert_eq!(f.preview_path(&over, PUBLIC), Err(PreviewError::TooLarge));

    // Size is judged before content.
    let big_binary = f.ws.join("big.txt");
    write(&big_binary, vec![0u8; limit + 1]);
    assert_eq!(
        f.preview_path(&big_binary, PUBLIC),
        Err(PreviewError::TooLarge)
    );
}

#[test]
fn a_source_that_is_not_text_is_refused() {
    let f = Fixture::new();
    let nul = f.ws.join("nul.ts");
    write(&nul, b"let a = 1;\0let b = 2;");
    assert_eq!(f.preview_path(&nul, PUBLIC), Err(PreviewError::NotText));

    let invalid = f.ws.join("latin1.ts");
    write(&invalid, b"caf\xe9");
    assert_eq!(f.preview_path(&invalid, PUBLIC), Err(PreviewError::NotText));

    let truncated = f.ws.join("cut.ts");
    write(&truncated, &"é".as_bytes()[..1]);
    assert_eq!(
        f.preview_path(&truncated, PUBLIC),
        Err(PreviewError::NotText)
    );

    let fine = f.ws.join("utf8.ts");
    write(&fine, "const naïve = '日本語'");
    assert_eq!(
        f.preview_path(&fine, PUBLIC).unwrap().content,
        "const naïve = '日本語'"
    );
}

#[test]
fn an_empty_file_is_one_empty_line() {
    let f = Fixture::new();
    let file = f.ws.join("empty.ts");
    write(&file, "");
    assert_eq!(
        f.preview_path(&file, PUBLIC).unwrap(),
        FilePreview {
            path: file.to_str().unwrap().to_owned(),
            line: None,
            line_start: 1,
            line_end: 1,
            total_lines: 1,
            content: String::new(),
            truncated: false,
        }
    );
}

// ------------------------------------------------------------------ window

#[test]
fn without_a_line_the_first_500_lines_are_shown() {
    let f = Fixture::new();
    let long = f.ws.join("long.ts");
    write(&long, numbered(1000));
    let preview = f.preview_path(&long, PUBLIC).unwrap();
    assert_eq!(preview.line, None);
    assert_eq!((preview.line_start, preview.line_end), (1, 500));
    assert_eq!(preview.total_lines, 1000);
    assert!(preview.truncated);
    assert_eq!(preview.content, numbered(500));

    let exact = f.ws.join("exact.ts");
    write(&exact, numbered(500));
    let preview = f.preview_path(&exact, PUBLIC).unwrap();
    assert_eq!((preview.line_start, preview.line_end), (1, 500));
    assert!(!preview.truncated);

    let short = f.ws.join("short.ts");
    write(&short, "a\nb\nc\n");
    let preview = f.preview_path(&short, PUBLIC).unwrap();
    // The trailing newline starts a fourth, empty line.
    assert_eq!(preview.total_lines, 4);
    assert_eq!((preview.line_start, preview.line_end), (1, 4));
    assert_eq!(preview.content, "a\nb\nc\n");
    assert!(!preview.truncated);
}

#[test]
fn with_a_line_a_window_of_100_lines_each_side_is_shown() {
    let f = Fixture::new();
    let long = f.ws.join("long.ts");
    write(&long, numbered(1000));
    let at = |line: &str| {
        f.preview(&format!("{}:{line}", long.display()), PUBLIC)
            .unwrap()
    };

    let middle = at("300");
    assert_eq!(middle.line, Some(300));
    // start = 300 - 101 = 199, end = 300 + 100 = 400
    assert_eq!((middle.line_start, middle.line_end), (200, 400));
    assert_eq!(middle.total_lines, 1000);
    assert!(middle.truncated);
    assert!(middle.content.starts_with("line 200\n"));
    assert!(middle.content.ends_with("\nline 400"));
    assert_eq!(middle.content.lines().count(), 201);

    // The column is ignored.
    assert_eq!(at("300:14"), middle);

    let first = at("1");
    assert_eq!(first.line, Some(1));
    assert_eq!((first.line_start, first.line_end), (1, 101));
    assert!(first.truncated);

    // 101 is the last line whose window still starts at the top.
    assert_eq!(at("101").line_start, 1);
    assert_eq!(at("102").line_start, 2);
    assert_eq!(at("103").line_start, 3);
    assert_eq!(at("103").line_end, 203);

    let near_end = at("990");
    assert_eq!((near_end.line_start, near_end.line_end), (890, 1000));
    assert!(near_end.truncated);

    // A line past the end is clamped to the last line, and the window follows the clamp.
    let past = at("5000");
    assert_eq!(past.line, Some(1000));
    assert_eq!((past.line_start, past.line_end), (900, 1000));
    assert!(past.content.ends_with("line 1000"));
    assert!(past.truncated);

    let huge = at("9007199254740991");
    assert_eq!(huge.line, Some(1000));
    assert_eq!((huge.line_start, huge.line_end), (900, 1000));
}

#[test]
fn a_window_that_covers_the_file_is_not_truncated() {
    let f = Fixture::new();
    let file = f.ws.join("small.ts");
    write(&file, numbered(150));
    // focus 50: start 0, end 150
    let preview = f
        .preview(&format!("{}:50", file.display()), PUBLIC)
        .unwrap();
    assert_eq!((preview.line_start, preview.line_end), (1, 150));
    assert!(!preview.truncated);
    assert_eq!(preview.content, numbered(150));
    // focus 101: still from the top, but 201 > 150 so it ends at the file's end.
    let preview = f
        .preview(&format!("{}:101", file.display()), PUBLIC)
        .unwrap();
    assert_eq!((preview.line_start, preview.line_end), (1, 150));
    assert!(!preview.truncated);
    // focus 150: start 49
    let preview = f
        .preview(&format!("{}:150", file.display()), PUBLIC)
        .unwrap();
    assert_eq!((preview.line_start, preview.line_end), (50, 150));
    assert!(preview.truncated);
}

#[test]
fn a_preview_serialises_as_the_web_apps_response() {
    let f = Fixture::new();
    let file = f.ws.join("a.ts");
    write(&file, "one\ntwo");
    let preview = f.preview(&format!("{}:2", file.display()), PUBLIC).unwrap();
    assert_eq!(
        serde_json::to_value(&preview).unwrap(),
        serde_json::json!({
            "path": file.to_str().unwrap(),
            "line": 2,
            "lineStart": 1,
            "lineEnd": 2,
            "totalLines": 2,
            "content": "one\ntwo",
            "truncated": false,
        })
    );
    let no_line = f.preview_path(&file, PUBLIC).unwrap();
    assert_eq!(
        serde_json::to_value(&no_line).unwrap()["line"],
        serde_json::Value::Null
    );
}

// ------------------------------------------------------------------ local images

#[test]
fn every_image_type_has_its_content_type() {
    let f = Fixture::new();
    for (ext, content_type) in [
        ("avif", "image/avif"),
        ("gif", "image/gif"),
        ("jpeg", "image/jpeg"),
        ("jpg", "image/jpeg"),
        ("png", "image/png"),
        ("webp", "image/webp"),
        ("PNG", "image/png"),
        ("JpG", "image/jpeg"),
    ] {
        let file = f.ws.join(format!("qa/wide.{ext}"));
        write(&file, [1u8, 2, 3, 0, 255]);
        for mode in [PUBLIC, TAILNET] {
            let image = f.image(&file, mode).unwrap();
            assert_eq!(image.content_type, content_type, "{ext}");
            assert_eq!(image.bytes, [1u8, 2, 3, 0, 255]);
        }
    }
}

#[test]
fn an_image_reference_must_be_an_absolute_raster_path() {
    let f = Fixture::new();
    write(&f.ws.join("qa/result.png"), "png");
    write(&f.ws.join("qa/result.svg"), "<svg/>");
    write(&f.ws.join("qa/result.pdf"), "pdf");
    let ws = f.ws.display();
    for reference in [
        "qa/result.png".to_owned(),
        "".to_owned(),
        "~someone/result.png".to_owned(),
        format!("{ws}/qa/result.svg"),
        format!("{ws}/qa/result.pdf"),
        format!("{ws}/qa/result.png:12"),
        format!("{ws}/qa/result"),
    ] {
        assert_eq!(
            local_image(&reference, &f.roots, TAILNET).map(|_| ()),
            Err(ImageError::NotFound),
            "{reference}"
        );
    }
    // `..` is resolved before the check.
    let via_dots = format!("{ws}/qa/../qa/./result.png");
    assert_eq!(
        local_image(&via_dots, &f.roots, PUBLIC).unwrap().bytes,
        b"png"
    );
    assert_eq!(
        local_image(&format!("{ws}/../outside/x.png"), &f.roots, PUBLIC).map(|_| ()),
        Err(ImageError::NotFound)
    );
}

#[test]
fn image_paths_follow_the_same_roots_as_source() {
    let f = Fixture::new();
    let in_home = f.home.join(".context/qa/result.webp");
    let in_skills = f.skills.join("conductor/diagram.png");
    let in_ws = f.ws.join("repo/shot.png");
    for path in [&in_home, &in_skills, &in_ws] {
        write(path, "img");
    }
    // `~/` is the home directory.
    assert!(local_image("~/.context/qa/result.webp", &f.roots, TAILNET).is_ok());

    assert!(f.image(&in_ws, PUBLIC).is_ok());
    for path in [&in_home, &in_skills] {
        assert!(f.image(path, TAILNET).is_ok(), "{path:?}");
        // A public relay does not serve them, and does not say it is refusing.
        assert_eq!(f.image(path, PUBLIC).map(|_| ()), Err(ImageError::NotFound));
    }
    assert_eq!(
        local_image("~/.context/qa/result.webp", &f.roots, PUBLIC).map(|_| ()),
        Err(ImageError::NotFound)
    );
}

#[test]
fn temporary_directories_serve_images_in_either_mode() {
    let f = Fixture::new();
    let shot = f.tmp.join("qa/shot.png");
    write(&shot, "tmp image");
    for mode in [PUBLIC, TAILNET] {
        assert_eq!(f.image(&shot, mode).unwrap().bytes, b"tmp image");
    }
    // Through a symbolic link to the temporary directory, either way round.
    let link = f.outside.join("tmp-link");
    symlink(&f.tmp, &link).unwrap();
    assert!(f.image(&link.join("qa/shot.png"), PUBLIC).is_ok());
    let real = f.tmp.canonicalize().unwrap();
    assert!(f.image(&real.join("qa/shot.png"), PUBLIC).is_ok());

    // Look-alike prefix.
    let mut os = f.tmp.clone().into_os_string();
    os.push("-other");
    let other = PathBuf::from(os).join("shot.png");
    write(&other, "no");
    assert_eq!(
        f.image(&other, TAILNET).map(|_| ()),
        Err(ImageError::NotFound)
    );

    // A link out of the temporary directory is followed and refused.
    let secret = f.outside.join("secret.png");
    write(&secret, "secret");
    let escape = f.tmp.join("escape.png");
    symlink(&secret, &escape).unwrap();
    for mode in [PUBLIC, TAILNET] {
        assert_eq!(
            f.image(&escape, mode).map(|_| ()),
            Err(ImageError::NotFound)
        );
    }

    // A missing image under the temporary directory is simply not found.
    assert_eq!(
        f.image(&f.tmp.join("gone.png"), PUBLIC).map(|_| ()),
        Err(ImageError::NotFound)
    );
}

#[test]
fn an_image_outside_every_root_or_not_a_file_is_not_found() {
    let f = Fixture::new();
    write(&f.outside.join("a.png"), "x");
    assert_eq!(
        f.image(&f.outside.join("a.png"), TAILNET).map(|_| ()),
        Err(ImageError::NotFound)
    );
    assert_eq!(
        local_image("/etc/passwd.png", &f.roots, TAILNET).map(|_| ()),
        Err(ImageError::NotFound)
    );
    let dir = f.ws.join("folder.png");
    fs::create_dir_all(&dir).unwrap();
    assert_eq!(f.image(&dir, PUBLIC).map(|_| ()), Err(ImageError::NotFound));
    // A link out of the workspaces.
    let escape = f.ws.join("escape.png");
    symlink(f.outside.join("a.png"), &escape).unwrap();
    assert_eq!(
        f.image(&escape, PUBLIC).map(|_| ()),
        Err(ImageError::NotFound)
    );
    assert_eq!(
        f.image(&escape, TAILNET).map(|_| ()),
        Err(ImageError::NotFound)
    );
}

#[test]
fn an_image_over_10_mib_is_too_large() {
    let f = Fixture::new();
    let limit = 10 * 1024 * 1024;
    let exact = f.ws.join("exact.png");
    write(&exact, vec![7u8; limit]);
    assert_eq!(f.image(&exact, PUBLIC).unwrap().bytes.len(), limit);

    let over = f.ws.join("over.png");
    write(&over, vec![7u8; limit + 1]);
    assert_eq!(
        f.image(&over, PUBLIC).map(|_| ()),
        Err(ImageError::TooLarge)
    );
}

#[test]
fn the_system_roots_include_both_temporary_directories() {
    let f = Fixture::new();
    let roots = PreviewRoots::system(&f.ws, &f.home);

    let scratch = tempfile::Builder::new()
        .prefix("files-preview-")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let shot = scratch.path().join("shot.png");
    write(&shot, "system temp");
    for mode in [PUBLIC, TAILNET] {
        assert_eq!(
            local_image(shot.to_str().unwrap(), &roots, mode)
                .unwrap()
                .bytes,
            b"system temp"
        );
    }
    // The temporary directory is for images only.
    let source = scratch.path().join("scratch.ts");
    write(&source, "x");
    assert_eq!(
        file_preview(source.to_str().unwrap(), &roots, TAILNET),
        Err(PreviewError::Forbidden(TAILNET_REFUSAL))
    );

    if Path::new("/tmp").is_dir() {
        let slash_tmp = tempfile::Builder::new()
            .prefix("files-preview-")
            .tempdir_in("/tmp")
            .unwrap();
        let shot = slash_tmp.path().join("shot.png");
        write(&shot, "slash tmp");
        assert_eq!(
            local_image(shot.to_str().unwrap(), &roots, PUBLIC)
                .unwrap()
                .bytes,
            b"slash tmp"
        );
    }

    // The workspaces root and home are the ones given.
    write(&f.ws.join("a.ts"), "ws");
    write(&f.home.join("a.md"), "home");
    assert!(file_preview(f.ws.join("a.ts").to_str().unwrap(), &roots, PUBLIC).is_ok());
    assert!(file_preview(f.home.join("a.md").to_str().unwrap(), &roots, TAILNET).is_ok());
    assert_eq!(
        file_preview(f.home.join("a.md").to_str().unwrap(), &roots, PUBLIC),
        Err(PreviewError::Forbidden(PUBLIC_REFUSAL))
    );
    // Conductor's bundled skills are named, whether or not this machine has them.
    assert_eq!(
        file_preview(
            "/Applications/Conductor.app/Contents/Resources/conductor-skill/skills/x/SKILL.md",
            &roots,
            PUBLIC
        ),
        Err(PreviewError::Forbidden(PUBLIC_REFUSAL))
    );
    assert_eq!(
        file_preview(
            "/Applications/Conductor.app/Contents/Resources/conductor-skill/skills/x/SKILL.md",
            &roots,
            TAILNET
        ),
        Err(PreviewError::NotFound)
    );
}
