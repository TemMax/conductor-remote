//! Attachment writes: upload into a chat's worktree, staging for a workspace to come, and the
//! discard of a staged file, with every answer they give. Everything happens in temporary
//! directories over a synthetic database; no desktop is involved.

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Bytes;
use conductor_remote::delivery::parked::{ParkedQueue, ParkedTimings};
use conductor_remote::delivery::service::{WriteDeps, WriteTimings, Writes};
use conductor_remote::delivery::{WriteAnswer, WriteService};
use conductor_remote::files::attachments::{
    attachment_token, ATTACHMENTS_DIR, MAX_ATTACHMENT_BYTES,
};
use conductor_remote::reads::Reads;
use conductor_remote::state::store::Store;
use conductor_remote::testing::FakeCommands;
use conductor_remote::ui::actor::UiActor;
use conductor_remote::ui::driver::UiDriver;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use support::TestDb;
use tempfile::TempDir;

/// A live workspace whose worktree exists on disk.
const WORKSPACE: &str = "ws-1";
/// A second live workspace whose worktree exists on disk.
const OTHER: &str = "ws-2";
/// A live workspace with no worktree on disk.
const BARE: &str = "ws-bare";
/// A workspace that is archived, so not live.
const ARCHIVED: &str = "ws-old";
const CHAT: &str = "s-1";
const OTHER_CHAT: &str = "s-2";
const BARE_CHAT: &str = "s-bare";
const OLD_CHAT: &str = "s-old";
const STAGING: &str = "attachment-staging";

struct Rig {
    test: TestDb,
    state: TempDir,
    writes: Writes,
}

fn seed(conn: &Connection) {
    conn.execute("INSERT INTO repos (id, name) VALUES ('r-1', 'relay')", [])
        .unwrap();
    let workspaces = [
        (WORKSPACE, "alpha", "ready"),
        (OTHER, "beta", "ready"),
        (BARE, "gamma", "setting_up"),
        (ARCHIVED, "delta", "archived"),
    ];
    for (id, directory, state) in workspaces {
        conn.execute(
            "INSERT INTO workspaces (local_id, id, repository_id, branch, workspace_name, \
             directory_name, state) VALUES (?1, ?1, 'r-1', ?2, ?2, ?2, ?3)",
            params![id, directory, state],
        )
        .unwrap();
    }
    let chats = [
        (CHAT, WORKSPACE),
        (OTHER_CHAT, OTHER),
        (BARE_CHAT, BARE),
        (OLD_CHAT, ARCHIVED),
    ];
    for (id, workspace) in chats {
        conn.execute(
            "INSERT INTO sessions (id, workspace_id, title, status, is_hidden, created_at, \
             updated_at) VALUES (?1, ?2, ?1, 'idle', 0, '2026-09-01 10:00:00', \
             '2026-09-01 10:00:00')",
            params![id, workspace],
        )
        .unwrap();
    }
}

fn writes_over(test: &TestDb) -> Writes {
    let ui = UiActor::spawn(|| -> Box<dyn UiDriver> { panic!("no desktop in this test") });
    let reads = Arc::new(Reads::new(test.db(), test.root()));
    let parked = ParkedQueue::new(
        Arc::new(Store::open_in_memory().expect("an in-memory store")),
        Arc::new(|| Some(false)),
        ParkedTimings::default(),
    );
    Writes::new(
        reads,
        ui,
        Arc::new(|| true),
        WriteTimings::default(),
        parked,
    )
}

impl Rig {
    fn new() -> Rig {
        let test = TestDb::new();
        seed(&test.conn());
        for directory in ["alpha", "beta"] {
            fs::create_dir_all(test.root().join("relay").join(directory).join(".git")).unwrap();
        }
        let state = tempfile::tempdir().expect("a state directory");
        let writes = writes_over(&test).configure(WriteDeps {
            state_dir: state.path().to_path_buf(),
            store: Arc::new(Store::open_in_memory().expect("an in-memory store")),
            commands: Arc::new(FakeCommands::new()),
            locked: Arc::new(|| Some(false)),
        });
        Rig {
            test,
            state,
            writes,
        }
    }

    fn worktree(&self, directory: &str) -> PathBuf {
        self.test.root().join("relay").join(directory)
    }

    fn staging(&self) -> PathBuf {
        self.state.path().join(STAGING)
    }

    async fn upload(
        &self,
        session: &str,
        workspace: Option<&str>,
        name: &str,
        bytes: &[u8],
    ) -> WriteAnswer {
        self.writes
            .upload_attachment(
                session.to_owned(),
                workspace.map(str::to_owned),
                name.to_owned(),
                Bytes::copy_from_slice(bytes),
            )
            .await
    }

    async fn stage(&self, name: &str, bytes: &[u8]) -> WriteAnswer {
        self.writes
            .stage_attachment(name.to_owned(), Bytes::copy_from_slice(bytes))
            .await
    }
}

fn error(status: u16, message: &str) -> WriteAnswer {
    WriteAnswer::error(status, message)
}

/// The names of the entries of a directory, sorted; empty when it is missing.
fn entries(dir: &Path) -> Vec<String> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn attachment_of(answer: &WriteAnswer) -> &Value {
    &answer.body["attachment"]
}

fn text<'a>(attachment: &'a Value, key: &str) -> &'a str {
    attachment[key].as_str().unwrap_or_else(|| panic!("{key}"))
}

fn no_attachments(rig: &Rig) -> bool {
    ["alpha", "beta"]
        .iter()
        .all(|d| !rig.worktree(d).join(ATTACHMENTS_DIR).exists())
        && !rig.staging().exists()
}

// ---- upload ----

#[tokio::test(flavor = "multi_thread")]
async fn upload_writes_the_file_into_the_chats_worktree() {
    let rig = Rig::new();
    let answer = rig.upload(CHAT, None, "notes.txt", b"hello phone").await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(answer.retry_after_secs, None);
    assert_eq!(answer.body["ok"], json!(true));
    let attachment = attachment_of(&answer);
    assert_eq!(attachment["name"], json!("notes.txt"));
    assert_eq!(attachment["bytes"], json!(11));
    assert_eq!(attachment.as_object().unwrap().len(), 4, "{attachment}");

    let path = text(attachment, "path");
    let id = path
        .strip_prefix(".context/attachments/")
        .and_then(|rest| rest.strip_suffix("/notes.txt"))
        .unwrap_or_else(|| panic!("unexpected path {path}"));
    assert_eq!(id.len(), 6);
    assert!(id.bytes().all(|b| b.is_ascii_alphanumeric()));
    assert_eq!(
        text(attachment, "token"),
        attachment_token("notes.txt", path)
    );

    let written = rig.worktree("alpha").join(path);
    assert_eq!(fs::read(&written).unwrap(), b"hello phone");
    assert_eq!(
        entries(&rig.worktree("alpha").join(ATTACHMENTS_DIR)),
        vec![id.to_owned()]
    );
    assert!(entries(&rig.worktree("beta"))
        .iter()
        .all(|e| e != ".context"));
    assert!(!rig.staging().exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_with_the_chats_own_workspace_id_is_the_same() {
    let rig = Rig::new();
    let answer = rig
        .upload(OTHER_CHAT, Some(OTHER), "a.bin", &[0, 1, 2, 255])
        .await;
    assert_eq!(answer.status, 200, "{answer:?}");
    let path = text(attachment_of(&answer), "path");
    assert_eq!(
        fs::read(rig.worktree("beta").join(path)).unwrap(),
        [0, 1, 2, 255]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_names_cannot_leave_their_directory() {
    let rig = Rig::new();
    let answer = rig.upload(CHAT, None, "../../evil.txt", b"x").await;
    assert_eq!(answer.status, 200, "{answer:?}");
    let attachment = attachment_of(&answer);
    assert_eq!(attachment["name"], json!("-..-evil.txt"));
    let written = rig.worktree("alpha").join(text(attachment, "path"));
    assert_eq!(fs::read(written).unwrap(), b"x");
    assert_eq!(entries(&rig.worktree("alpha")), vec![".context", ".git"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_uploads_of_one_name_get_two_directories() {
    let rig = Rig::new();
    let first = rig.upload(CHAT, None, "same.txt", b"one").await;
    let second = rig.upload(CHAT, None, "same.txt", b"two").await;
    assert_eq!(first.status, 200);
    assert_eq!(second.status, 200);
    let (a, b) = (
        text(attachment_of(&first), "path"),
        text(attachment_of(&second), "path"),
    );
    assert_ne!(a, b);
    assert_eq!(fs::read(rig.worktree("alpha").join(a)).unwrap(), b"one");
    assert_eq!(fs::read(rig.worktree("alpha").join(b)).unwrap(), b"two");
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_to_an_unknown_workspace_is_404() {
    let rig = Rig::new();
    let missing = error(404, "workspace for session not found");
    for (session, workspace) in [
        ("s-nobody", None),
        ("s-nobody", Some("ws-nobody")),
        (CHAT, Some("ws-nobody")),
        // A workspace that is not live is as good as none.
        (OLD_CHAT, None),
        (CHAT, Some(ARCHIVED)),
    ] {
        let answer = rig.upload(session, workspace, "a.txt", b"x").await;
        assert_eq!(answer, missing, "{session} {workspace:?}");
    }
    assert!(no_attachments(&rig));
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_of_a_chat_in_another_workspace_is_409() {
    let rig = Rig::new();
    let mismatch = error(409, "chat is not in that workspace");
    for (session, workspace) in [
        // The chat is in a different live workspace.
        (CHAT, OTHER),
        // The chat is unknown but the workspace is live.
        ("s-nobody", WORKSPACE),
        // The chat belongs to a workspace that is not live.
        (OLD_CHAT, WORKSPACE),
    ] {
        let answer = rig.upload(session, Some(workspace), "a.txt", b"x").await;
        assert_eq!(answer, mismatch, "{session} {workspace}");
    }
    assert!(no_attachments(&rig));
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_without_a_worktree_is_409() {
    let rig = Rig::new();
    for workspace in [None, Some(BARE)] {
        let answer = rig.upload(BARE_CHAT, workspace, "a.txt", b"x").await;
        assert_eq!(
            answer,
            error(409, "worktree path unresolved"),
            "{workspace:?}"
        );
    }
    assert!(no_attachments(&rig));
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_without_a_name_is_400() {
    let rig = Rig::new();
    let answer = rig.upload(CHAT, None, "", b"x").await;
    assert_eq!(answer, error(400, "missing attachment name"));
    // The name is judged before the body.
    let answer = rig.upload(CHAT, None, "", b"").await;
    assert_eq!(answer, error(400, "missing attachment name"));
    assert!(no_attachments(&rig));
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_of_no_bytes_is_400() {
    let rig = Rig::new();
    let answer = rig.upload(CHAT, None, "a.txt", b"").await;
    assert_eq!(answer, error(400, "empty attachment"));
    assert!(no_attachments(&rig));
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_over_the_limit_is_413_and_the_limit_itself_is_accepted() {
    let rig = Rig::new();
    let answer = rig
        .upload(CHAT, None, "big.bin", &vec![7u8; MAX_ATTACHMENT_BYTES + 1])
        .await;
    assert_eq!(answer, error(413, "attachments are limited to 25 MB"));
    assert!(no_attachments(&rig));

    let answer = rig
        .upload(CHAT, None, "big.bin", &vec![7u8; MAX_ATTACHMENT_BYTES])
        .await;
    assert_eq!(answer.status, 200, "{answer:?}");
    assert_eq!(
        attachment_of(&answer)["bytes"],
        json!(MAX_ATTACHMENT_BYTES as u64)
    );
}

// ---- stage ----

#[tokio::test(flavor = "multi_thread")]
async fn stage_writes_under_the_state_directory_and_answers_201() {
    let rig = Rig::new();
    let answer = rig.stage("photo.png", b"\x89PNG data").await;
    assert_eq!(answer.status, 201, "{answer:?}");
    assert_eq!(answer.body["ok"], json!(true));
    let attachment = attachment_of(&answer);
    assert_eq!(attachment.as_object().unwrap().len(), 5, "{attachment}");
    assert_eq!(attachment["name"], json!("photo.png"));
    assert_eq!(attachment["bytes"], json!(9));

    let id = text(attachment, "stageId");
    assert_eq!(id.len(), 6);
    assert!(id.bytes().all(|b| b.is_ascii_alphanumeric()));
    let path = text(attachment, "path");
    assert_eq!(path, format!(".context/attachments/{id}/photo.png"));
    assert_eq!(
        text(attachment, "token"),
        attachment_token("photo.png", path)
    );

    // `<state>/attachment-staging/.context/attachments/<id>/<name>`, and nothing else.
    let root = rig.staging();
    assert_eq!(entries(rig.state.path()), vec![STAGING]);
    assert_eq!(entries(&root), vec![".context"]);
    assert_eq!(entries(&root.join(".context")), vec!["attachments"]);
    assert_eq!(entries(&root.join(ATTACHMENTS_DIR)), vec![id.to_owned()]);
    let dir = root.join(ATTACHMENTS_DIR).join(id);
    assert_eq!(entries(&dir), vec!["photo.png"]);
    assert_eq!(fs::read(dir.join("photo.png")).unwrap(), b"\x89PNG data");

    // Nothing reached a worktree.
    assert!(no_attachments_in_worktrees(&rig));
}

fn no_attachments_in_worktrees(rig: &Rig) -> bool {
    ["alpha", "beta"]
        .iter()
        .all(|d| !rig.worktree(d).join(ATTACHMENTS_DIR).exists())
}

#[tokio::test(flavor = "multi_thread")]
async fn staged_files_are_private() {
    let rig = Rig::new();
    let answer = rig.stage("secret.txt", b"shh").await;
    assert_eq!(answer.status, 201, "{answer:?}");
    let id = text(attachment_of(&answer), "stageId").to_owned();
    let root = rig.staging();
    let dir = root.join(ATTACHMENTS_DIR).join(&id);
    assert_eq!(mode(&dir.join("secret.txt")), 0o600);
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&root.join(ATTACHMENTS_DIR)), 0o700);
    assert_eq!(mode(&root.join(".context")), 0o700);
}

#[tokio::test(flavor = "multi_thread")]
async fn stage_names_cannot_leave_their_directory() {
    let rig = Rig::new();
    let answer = rig.stage("../../evil.txt", b"x").await;
    assert_eq!(answer.status, 201, "{answer:?}");
    let attachment = attachment_of(&answer);
    assert_eq!(attachment["name"], json!("-..-evil.txt"));
    let dir = rig
        .staging()
        .join(ATTACHMENTS_DIR)
        .join(text(attachment, "stageId"));
    assert_eq!(entries(&dir), vec!["-..-evil.txt"]);
    assert_eq!(entries(rig.state.path()), vec![STAGING]);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_staged_files_get_two_ids() {
    let rig = Rig::new();
    let first = rig.stage("same.txt", b"one").await;
    let second = rig.stage("same.txt", b"two").await;
    let (a, b) = (
        text(attachment_of(&first), "stageId"),
        text(attachment_of(&second), "stageId"),
    );
    assert_ne!(a, b);
    assert_eq!(entries(&rig.staging().join(ATTACHMENTS_DIR)).len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn stage_without_a_name_is_400() {
    let rig = Rig::new();
    assert_eq!(
        rig.stage("", b"x").await,
        error(400, "missing attachment name")
    );
    assert_eq!(
        rig.stage("", b"").await,
        error(400, "missing attachment name")
    );
    assert!(!rig.staging().exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn stage_of_no_bytes_is_400() {
    let rig = Rig::new();
    assert_eq!(
        rig.stage("a.txt", b"").await,
        error(400, "empty attachment")
    );
    assert!(!rig.staging().exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn stage_over_the_limit_is_413_and_the_limit_itself_is_accepted() {
    let rig = Rig::new();
    let answer = rig
        .stage("big.bin", &vec![7u8; MAX_ATTACHMENT_BYTES + 1])
        .await;
    assert_eq!(answer, error(413, "attachments are limited to 25 MB"));
    assert!(!rig.staging().exists());

    let answer = rig.stage("big.bin", &vec![7u8; MAX_ATTACHMENT_BYTES]).await;
    assert_eq!(answer.status, 201, "{answer:?}");
    assert_eq!(
        attachment_of(&answer)["bytes"],
        json!(MAX_ATTACHMENT_BYTES as u64)
    );
}

// ---- discard ----

#[tokio::test(flavor = "multi_thread")]
async fn discard_removes_a_staged_file_and_answers_200() {
    let rig = Rig::new();
    let keep = rig.stage("keep.txt", b"keep").await;
    let drop = rig.stage("drop.txt", b"drop").await;
    let keep_id = text(attachment_of(&keep), "stageId").to_owned();
    let drop_id = text(attachment_of(&drop), "stageId").to_owned();

    let answer = rig.writes.discard_staged(drop_id.clone()).await;
    assert_eq!(answer, WriteAnswer::json(200, json!({ "ok": true })));
    assert_eq!(
        entries(&rig.staging().join(ATTACHMENTS_DIR)),
        vec![keep_id.clone()]
    );
    assert_eq!(
        fs::read(
            rig.staging()
                .join(ATTACHMENTS_DIR)
                .join(&keep_id)
                .join("keep.txt")
        )
        .unwrap(),
        b"keep"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn discard_of_something_not_staged_is_404_with_ok_true() {
    let rig = Rig::new();
    let gone = WriteAnswer::json(404, json!({ "ok": true }));

    // Nothing was ever staged.
    assert_eq!(rig.writes.discard_staged("abc123".to_owned()).await, gone);

    // A second discard of the same id.
    let staged = rig.stage("a.txt", b"x").await;
    let id = text(attachment_of(&staged), "stageId").to_owned();
    assert_eq!(
        rig.writes.discard_staged(id.clone()).await,
        WriteAnswer::json(200, json!({ "ok": true }))
    );
    assert_eq!(rig.writes.discard_staged(id).await, gone);
}

#[tokio::test(flavor = "multi_thread")]
async fn discard_of_a_malformed_id_touches_nothing() {
    let rig = Rig::new();
    let staged = rig.stage("a.txt", b"x").await;
    let id = text(attachment_of(&staged), "stageId").to_owned();
    let gone = WriteAnswer::json(404, json!({ "ok": true }));
    for bad in ["", "..", "../..", "a/b", "short", "toolong1", "ab c12"] {
        assert_eq!(
            rig.writes.discard_staged(bad.to_owned()).await,
            gone,
            "{bad:?}"
        );
    }
    assert_eq!(entries(&rig.staging().join(ATTACHMENTS_DIR)), vec![id]);
    assert_eq!(entries(rig.state.path()), vec![STAGING]);
}

// ---- no deps ----

#[tokio::test(flavor = "multi_thread")]
async fn without_deps_every_attachment_write_is_503() {
    let test = TestDb::new();
    seed(&test.conn());
    fs::create_dir_all(test.root().join("relay/alpha/.git")).unwrap();
    let writes = writes_over(&test);
    let unavailable = error(503, "attachments are unavailable");

    let answer = writes
        .upload_attachment(
            CHAT.to_owned(),
            None,
            "a.txt".to_owned(),
            Bytes::from_static(b"x"),
        )
        .await;
    assert_eq!(answer, unavailable);
    let answer = writes
        .stage_attachment("a.txt".to_owned(), Bytes::from_static(b"x"))
        .await;
    assert_eq!(answer, unavailable);
    let answer = writes.discard_staged("abc123".to_owned()).await;
    assert_eq!(answer, unavailable);
    assert!(!test
        .root()
        .join("relay/alpha")
        .join(ATTACHMENTS_DIR)
        .exists());
}
