//! Search results: the full-text hits folded per workspace, merged with the name matches.
//!
//! The source is the synthetic Conductor database of `support` and `seed_search`; the index is a
//! real `SearchIndex` in a temporary directory. The scope cases port the reference tests "keeps
//! archived chats in the default scope", "excludes only archived chats before full-text ranking"
//! and "applies the same archive scope to workspace-name matches", and the index's "an empty list
//! matches nothing, never everything" at the route level.

#[path = "support/seed_search.rs"]
mod seed_search;
mod support;

use std::collections::HashMap;

use conductor_remote::reads::workspaces::{RepoIcon, SearchWorkspace};
use conductor_remote::reads::Reads;
use conductor_remote::search::index::{ChunkHit, SearchIndex};
use conductor_remote::search::query::match_query;
use conductor_remote::search::results::{fold_hits, SearchParams, SearchResponse, SearchRole};
use rusqlite::Connection;
use seed_search::say;
use support::TestDb;

struct Fixture {
    test: TestDb,
    index_dir: tempfile::TempDir,
    index: SearchIndex,
    reads: Reads,
}

/// The synthetic rows, the messages `fill` adds, and an index that has caught up.
fn fixture(fill: impl FnOnce(&Connection)) -> Fixture {
    let test = TestDb::new();
    {
        let conn = test.conn();
        seed_search::seed(&conn, test.root());
        fill(&conn);
    }
    let index_dir = tempfile::tempdir().unwrap();
    let index = SearchIndex::open(&index_dir.path().join("search.db")).expect("open the index");
    let source = test.db();
    while index.index_step(&source).expect("index step") {}
    let reads = Reads::new(test.db(), test.root());
    Fixture {
        test,
        index_dir,
        index,
        reads,
    }
}

/// Every workspace says "lantern" in some chat, as follows.
///
/// - `srch-live`: five matches in two chats; `srch-chat-live` holds the densest one
/// - `srch-archived`, `srch-unknown`, `srch-quiet`, `srch-shelved`: one each
/// - `srch-calm`: two, one of them dense
/// - an orphan chat, which belongs to no workspace
fn lantern(conn: &Connection) {
    let messages = [
        (
            "m01",
            "srch-chat-live",
            "2026-09-10T10:00:00.000Z",
            "lantern lantern lantern wick",
        ),
        (
            "m02",
            "srch-chat-live-2",
            "2026-09-12T10:00:00.000Z",
            "trim the lantern",
        ),
        (
            "m03",
            "srch-chat-live",
            "2026-09-11T10:00:00.000Z",
            "the lantern again",
        ),
        (
            "m04",
            "srch-chat-live",
            "2026-09-09T10:00:00.000Z",
            "one more lantern remark",
        ),
        (
            "m05",
            "srch-chat-live-2",
            "2026-09-08T10:00:00.000Z",
            "lantern glass cracked",
        ),
        (
            "m06",
            "srch-chat-archived",
            "2026-08-01T10:00:00.000Z",
            "old lantern notes",
        ),
        (
            "m07",
            "srch-chat-unknown",
            "2026-08-02T10:00:00.000Z",
            "lantern unknown chat",
        ),
        (
            "m08",
            "srch-chat-quiet",
            "2026-08-03T10:00:00.000Z",
            "a lantern on the shelf",
        ),
        (
            "m09",
            "srch-chat-calm",
            "2026-08-04T10:00:00.000Z",
            "lantern lantern lantern lantern calm",
        ),
        (
            "m10",
            "srch-chat-calm",
            "2026-08-05T10:00:00.000Z",
            "lantern again",
        ),
        (
            "m11",
            "srch-chat-orphan",
            "2026-08-06T10:00:00.000Z",
            "lantern in nowhere",
        ),
        (
            "m12",
            "srch-chat-shelved",
            "2026-08-07T10:00:00.000Z",
            "lantern in the attic",
        ),
    ];
    for (id, session, at, text) in messages {
        say(conn, id, session, at, text);
    }
}

fn nothing(_: &Connection) {}

/// A source rowid in base 36, written independently of the relay's own.
fn base36(rowid: i64) -> String {
    let mut digits = Vec::new();
    let mut rest = u64::try_from(rowid).expect("a rowid is not negative");
    loop {
        digits.push(char::from_digit((rest % 36) as u32, 36).unwrap());
        rest /= 36;
        if rest == 0 {
            break;
        }
    }
    digits.iter().rev().collect()
}

fn params(q: &str, repos: &[&str], archived: bool, limit: usize) -> SearchParams {
    SearchParams {
        q: q.to_owned(),
        repos: repos.iter().map(|r| (*r).to_owned()).collect(),
        archived,
        limit,
    }
}

fn search(fx: &Fixture, q: &str, repos: &[&str], archived: bool, limit: usize) -> SearchResponse {
    fx.reads
        .search(Some(&fx.index), &params(q, repos, archived, limit))
        .expect("an answer")
}

fn ids(response: &SearchResponse) -> Vec<&str> {
    response
        .results
        .iter()
        .map(|r| r.workspace.id.as_str())
        .collect()
}

fn sorted(mut values: Vec<&str>) -> Vec<&str> {
    values.sort_unstable();
    values
}

// ------------------------------------------------------------------ archive scope

#[test]
fn keeps_archived_chats_in_the_default_scope() {
    let fx = fixture(lantern);
    let response = search(&fx, "lantern", &[], true, 50);
    let archived = response
        .results
        .iter()
        .find(|r| r.workspace.id == "srch-archived")
        .expect("the archived workspace is found");
    assert!(archived.workspace.archived);
    assert_eq!(archived.hits, 1);
    assert_eq!(archived.session_id.as_deref(), Some("srch-chat-archived"));
    // Found by its chat alone as well: the archived workspace with no name match.
    let shelved = response
        .results
        .iter()
        .find(|r| r.workspace.id == "srch-shelved")
        .expect("a chat-only archived workspace is found");
    assert!(!shelved.by_name);
    assert_eq!(shelved.hits, 1);
}

#[test]
fn excludes_only_archived_chats_before_full_text_ranking() {
    let fx = fixture(|conn| {
        lantern(conn);
        // More dense archived chunks than the query keeps: unscoped, they would take every slot.
        let tx = conn.unchecked_transaction().unwrap();
        for i in 0..320 {
            say(
                &tx,
                &format!("dense{i}"),
                "srch-chat-archived",
                "2026-08-10T10:00:00.000Z",
                &"lantern ".repeat(30),
            );
        }
        tx.commit().unwrap();
    });
    let unscoped = fx
        .index
        .search(&match_query("lantern").unwrap(), None, 300)
        .unwrap();
    assert!(unscoped
        .iter()
        .all(|h| h.session_id == "srch-chat-archived"));

    let response = search(&fx, "lantern", &[], false, 50);
    // `srch-unknown` has no state and stays; only archived work is out.
    assert_eq!(
        sorted(ids(&response)),
        ["srch-calm", "srch-live", "srch-quiet", "srch-unknown"]
    );
    let live = response
        .results
        .iter()
        .find(|r| r.workspace.id == "srch-live")
        .unwrap();
    assert_eq!(live.hits, 5);

    let one = search(&fx, "lantern", &["srch-one"], false, 50);
    assert_eq!(ids(&one), ["srch-live"]);
    assert_eq!(one.results[0].hits, 5);
}

#[test]
fn applies_the_same_archive_scope_to_workspace_name_matches() {
    let fx = fixture(nothing);
    let with_archived = search(&fx, "lantern", &[], true, 50);
    assert!(ids(&with_archived).contains(&"srch-archived"));
    assert!(with_archived.results.iter().all(|r| r.by_name));

    let without = search(&fx, "lantern", &[], false, 50);
    assert_eq!(sorted(ids(&without)), ["srch-live", "srch-unknown"]);

    let scoped = search(&fx, "lantern", &["srch-one"], false, 50);
    assert_eq!(ids(&scoped), ["srch-live"]);
}

#[test]
fn an_empty_scope_matches_nothing() {
    let fx = fixture(lantern);
    // A repo with no workspace: the scope holds no chat, and an empty list is not "every chat".
    let unknown_repo = search(&fx, "lantern", &["srch-nowhere"], true, 50);
    assert!(unknown_repo.results.is_empty());
    // A repo whose only workspace is archived, with archived work excluded.
    let only_archived = search(&fx, "lantern", &["srch-three"], false, 50);
    assert!(only_archived.results.is_empty());
    // The same repo with archived work kept finds its chat.
    let kept = search(&fx, "lantern", &["srch-three"], true, 50);
    assert_eq!(ids(&kept), ["srch-shelved"]);
}

// ------------------------------------------------------------------ the fold

fn workspace(id: &str) -> SearchWorkspace {
    SearchWorkspace {
        id: id.to_owned(),
        workspace_name: None,
        pr_title: None,
        branch: None,
        directory_name: None,
        state: None,
        updated_at: "2026-01-01".to_owned(),
        repo_name: None,
        icon: None,
        archived: false,
    }
}

fn hit(session: &str, rowid: i64, role: &str, at: Option<&str>, score: f64) -> ChunkHit {
    ChunkHit {
        session_id: session.to_owned(),
        src_rowid: rowid,
        role: role.to_owned(),
        at: at.map(str::to_owned),
        score,
        snippet: format!("text {rowid}"),
    }
}

/// Sessions `a1`, `a2` belong to workspace `a`; `b1` to `b`; `c1` to `c`; `ghost` to none.
fn fold(hits: &[ChunkHit]) -> Vec<conductor_remote::search::results::SearchResult> {
    let workspaces: HashMap<&str, SearchWorkspace> = [
        ("a1", workspace("a")),
        ("a2", workspace("a")),
        ("b1", workspace("b")),
        ("c1", workspace("c")),
    ]
    .into_iter()
    .collect();
    fold_hits(hits, |session| workspaces.get(session))
}

#[test]
fn the_fold_counts_every_hit_and_keeps_the_first_three_as_snippets_and_score() {
    let hits = [
        hit("a1", 1, "user", None, 5.0),
        hit("a1", 2, "assistant", None, 4.0),
        hit("a1", 3, "thinking", None, 3.0),
        hit("a1", 4, "user", None, 2.0),
        hit("a1", 5, "user", None, 1.0),
    ];
    let results = fold(&hits);
    assert_eq!(results.len(), 1);
    let result = &results[0];
    assert_eq!(result.hits, 5);
    assert_eq!(result.score, 12.0);
    assert!(!result.by_name);
    assert_eq!(result.session_title, None);
    let cursors: Vec<&str> = result.snippets.iter().map(|s| s.cursor.as_str()).collect();
    assert_eq!(cursors, ["m1", "m2", "m3"]);
    let roles: Vec<SearchRole> = result.snippets.iter().map(|s| s.role).collect();
    assert_eq!(
        roles,
        [
            SearchRole::User,
            SearchRole::Assistant,
            SearchRole::Thinking
        ]
    );
    assert_eq!(result.snippets[1].text, "text 2");
    assert_eq!(result.snippets[1].session_id, "a1");
}

#[test]
fn the_fold_takes_the_greatest_time_and_a_snippet_without_one_says_empty() {
    let hits = [
        hit("a1", 1, "user", Some("2026-09-02"), 4.0),
        hit("a1", 2, "user", Some("2026-09-10"), 3.0),
        hit("a1", 3, "user", None, 2.0),
        hit("a1", 4, "user", Some("2026-09-05"), 1.0),
    ];
    let result = &fold(&hits)[0];
    assert_eq!(result.at.as_deref(), Some("2026-09-10"));
    let ats: Vec<&str> = result.snippets.iter().map(|s| s.at.as_str()).collect();
    assert_eq!(ats, ["2026-09-02", "2026-09-10", ""]);

    let timeless = &fold(&[hit("a1", 1, "user", None, 1.0)])[0];
    assert_eq!(timeless.at, None);
    assert_eq!(timeless.snippets[0].at, "");
}

#[test]
fn the_fold_names_the_session_of_the_best_single_hit_not_the_best_sum() {
    // `a1` has three middling hits, `a2` one better hit that is not among the first three.
    let hits = [
        hit("a1", 1, "user", None, 2.0),
        hit("a1", 2, "user", None, 2.0),
        hit("a1", 3, "user", None, 2.0),
        hit("a2", 4, "user", None, 3.0),
    ];
    let result = &fold(&hits)[0];
    assert_eq!(result.session_id.as_deref(), Some("a2"));
    assert_eq!(result.snippets.len(), 3);
    assert_eq!(result.score, 6.0);

    // Equal best hits: the session that matched first.
    let tie = [
        hit("a2", 1, "user", None, 2.0),
        hit("a1", 2, "user", None, 2.0),
    ];
    assert_eq!(fold(&tie)[0].session_id.as_deref(), Some("a2"));
}

#[test]
fn the_fold_drops_hits_whose_session_resolves_to_no_workspace() {
    let hits = [
        hit("ghost", 1, "user", None, 9.0),
        hit("a1", 2, "user", None, 1.0),
        hit("ghost", 3, "user", None, 8.0),
    ];
    let results = fold(&hits);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].workspace.id, "a");
    assert_eq!(results[0].hits, 1);
    assert_eq!(results[0].score, 1.0);
    assert!(fold(&[hit("ghost", 1, "user", None, 9.0)]).is_empty());
}

#[test]
fn the_fold_sorts_by_score_and_keeps_first_seen_order_between_equal_scores() {
    let hits = [
        hit("a1", 1, "user", None, 1.0),
        hit("b1", 2, "user", None, 5.0),
        hit("c1", 3, "user", None, 1.0),
        hit("a2", 4, "user", None, 0.5),
    ];
    let results = fold(&hits);
    let order: Vec<&str> = results.iter().map(|r| r.workspace.id.as_str()).collect();
    assert_eq!(order, ["b", "a", "c"]);
    assert_eq!(results[1].score, 1.5);
}

#[test]
fn the_cursor_is_m_and_the_rowid_in_base_36() {
    let rowids = [
        (0, "m0"),
        (35, "mz"),
        (36, "m10"),
        (1295, "mzz"),
        (1296, "m100"),
    ];
    for (rowid, cursor) in rowids {
        let result = &fold(&[hit("a1", rowid, "user", None, 1.0)])[0];
        assert_eq!(result.snippets[0].cursor, cursor, "rowid {rowid}");
    }
}

#[test]
fn a_search_folds_a_workspaces_hits_from_the_index() {
    let fx = fixture(lantern);
    let expr = match_query("lantern").unwrap();
    let hits = fx.index.search(&expr, None, 300).unwrap();
    let in_live: Vec<&ChunkHit> = hits
        .iter()
        .filter(|h| h.session_id.starts_with("srch-chat-live"))
        .collect();
    assert_eq!(in_live.len(), 5);

    let response = search(&fx, "lantern", &[], true, 50);
    let live = response
        .results
        .iter()
        .find(|r| r.workspace.id == "srch-live")
        .unwrap();
    assert_eq!(live.hits, 5);
    assert_eq!(live.snippets.len(), 3);
    let expected_score: f64 = in_live.iter().take(3).map(|h| h.score).sum();
    assert_eq!(live.score, expected_score);
    for (snippet, hit) in live.snippets.iter().zip(&in_live) {
        assert_eq!(snippet.session_id, hit.session_id);
        assert_eq!(snippet.cursor, format!("m{}", base36(hit.src_rowid)));
        assert_eq!(snippet.text, hit.snippet);
        assert_eq!(Some(snippet.at.as_str()), hit.at.as_deref());
    }
    // The strongest passage is the dense prompt of `srch-chat-live`; the title comes with it.
    assert_eq!(live.session_id.as_deref(), Some("srch-chat-live"));
    assert_eq!(live.session_title.as_deref(), Some("Trim the wick"));
    assert_eq!(live.at.as_deref(), Some("2026-09-12T10:00:00.000Z"));
    assert!(live.snippets[0].text.contains("\u{1}lantern\u{2}"));
    assert_eq!(live.snippets[0].role, SearchRole::User);
}

// ------------------------------------------------------------------ the merge

#[test]
fn name_matches_come_first_with_their_chat_evidence_then_the_chat_only_results() {
    let fx = fixture(lantern);
    let response = search(&fx, "lantern", &[], true, 50);
    let order = ids(&response);
    // Name matches in SQL order: no state first (NULL sorts first), then by state, then the
    // archived last.
    assert_eq!(
        order[..3],
        ["srch-unknown", "srch-live", "srch-archived"],
        "{order:?}"
    );
    assert!(response.results[..3].iter().all(|r| r.by_name));
    // Chat-only results after them: the dense chat first, the rest by score.
    assert_eq!(
        sorted(order[3..].to_vec()),
        ["srch-calm", "srch-quiet", "srch-shelved"]
    );
    assert_eq!(order[3], "srch-calm");
    assert!(response.results[3..].iter().all(|r| !r.by_name));
    let scores: Vec<f64> = response.results[3..].iter().map(|r| r.score).collect();
    assert!(
        scores.windows(2).all(|pair| pair[0] >= pair[1]),
        "{scores:?}"
    );
    assert_eq!(response.results.len(), 6);

    // The name match keeps its evidence.
    let unknown = &response.results[0];
    assert_eq!(unknown.hits, 1);
    assert_eq!(unknown.session_id.as_deref(), Some("srch-chat-unknown"));
    assert_eq!(unknown.session_title.as_deref(), Some("Unknown wick talk"));
    assert_eq!(unknown.snippets.len(), 1);
    assert_eq!(response.results[1].hits, 5);
    // The orphan chat resolves to no workspace and shows nowhere.
    assert!(response
        .results
        .iter()
        .flat_map(|r| &r.snippets)
        .all(|s| s.session_id != "srch-chat-orphan"));
}

#[test]
fn a_name_match_without_chat_evidence_is_an_empty_row() {
    let fx = fixture(lantern);
    let response = search(&fx, "beacon", &[], true, 50);
    assert_eq!(ids(&response), ["srch-beacon"]);
    let row = &response.results[0];
    assert!(row.by_name);
    assert_eq!(row.session_id, None);
    assert_eq!(row.hits, 0);
    assert_eq!(row.score, 0.0);
    assert_eq!(row.at, None);
    assert!(row.snippets.is_empty());
    assert_eq!(row.session_title, None);
}

#[test]
fn the_merge_is_cut_to_the_limit_after_the_name_matches() {
    let fx = fixture(lantern);
    assert_eq!(
        ids(&search(&fx, "lantern", &[], true, 4)),
        ["srch-unknown", "srch-live", "srch-archived", "srch-calm"]
    );
    assert_eq!(
        ids(&search(&fx, "lantern", &[], true, 2)),
        ["srch-unknown", "srch-live"]
    );
    assert!(search(&fx, "lantern", &[], true, 0).results.is_empty());
    // The name query itself is limited: with two slots, only two name matches are even asked for.
    let names_only = fixture(nothing);
    assert_eq!(
        ids(&search(&names_only, "lantern", &[], true, 2)),
        ["srch-unknown", "srch-live"]
    );
}

#[test]
fn every_token_must_match_one_of_the_names_fields() {
    let fx = fixture(nothing);
    // "two" is the repo's name, "lantern" the workspace's: no single column holds both.
    let response = search(&fx, "lantern two", &[], true, 50);
    assert_eq!(ids(&response), ["srch-unknown"]);
    assert!(ids(&search(&fx, "lantern shelf", &[], true, 50)).is_empty());
    // A branch and a directory name count too.
    assert_eq!(ids(&search(&fx, "harbour", &[], true, 50)), ["srch-calm"]);
    assert_eq!(ids(&search(&fx, "tower", &[], true, 50)), ["srch-beacon"]);
}

#[test]
fn an_underscore_in_a_token_is_literal_in_the_like_pattern() {
    let fx = fixture(nothing);
    let response = search(&fx, "under_score", &[], true, 50);
    assert_eq!(ids(&response), ["srch-under"]);
    // Without the escape `_` would match any character and find `underXscore` as well; two
    // tokens, as opposed to one with an underscore, find both.
    let both = search(&fx, "under score", &[], true, 50);
    assert_eq!(sorted(ids(&both)), ["srch-under", "srch-underx"]);
    assert_eq!(
        sorted(ids(&search(&fx, "tool", &[], true, 50))),
        ["srch-under", "srch-underx"]
    );
}

// ------------------------------------------------------------------ icons

#[test]
fn a_workspaces_icon_follows_its_repos_icon_rule() {
    let fx = fixture(nothing);
    let icons_dir = seed_search::repo_three_root(fx.test.root());
    std::fs::create_dir_all(&icons_dir).unwrap();
    std::fs::write(icons_dir.join("favicon.svg"), "<svg/>").unwrap();

    let icon = |q: &str| {
        let response = search(&fx, q, &[], true, 50);
        response.results[0].workspace.icon.clone()
    };
    // An explicit emoji, the repo's own file, the owner of a GitHub remote.
    assert_eq!(
        icon("lantern current"),
        Some(RepoIcon::Emoji {
            value: "🔦".to_owned()
        })
    );
    assert_eq!(icon("shelved"), Some(RepoIcon::File));
    assert_eq!(
        icon("beacon"),
        Some(RepoIcon::Github {
            owner: "acme".to_owned()
        })
    );
}

// ------------------------------------------------------------------ the index

#[test]
fn the_answer_without_an_index_is_the_name_matches_and_the_stub_status() {
    let fx = fixture(lantern);
    let response = fx
        .reads
        .search(None, &params("lantern", &[], true, 50))
        .unwrap();
    assert_eq!(
        ids(&response),
        ["srch-unknown", "srch-live", "srch-archived"]
    );
    for row in &response.results {
        assert!(row.by_name);
        assert_eq!(row.hits, 0);
        assert!(row.snippets.is_empty());
        assert_eq!(row.session_id, None);
    }
    assert_eq!(response.index.chunks, 0);
    assert!(!response.index.ready);
    assert_eq!(response.index.progress, 0.0);
    assert_eq!(response.index.error, None);
    assert_eq!(response.query, "lantern");
}

#[test]
fn an_index_that_has_not_caught_up_answers_with_its_status_and_what_it_has() {
    let test = TestDb::new();
    seed_search::seed(&test.conn(), test.root());
    say(
        &test.conn(),
        "m1",
        "srch-chat-quiet",
        "2026-08-03T10:00:00.000Z",
        "a lantern on the shelf",
    );
    let dir = tempfile::tempdir().unwrap();
    let index = SearchIndex::open(&dir.path().join("search.db")).unwrap();
    let reads = Reads::new(test.db(), test.root());

    // Nothing indexed yet: names only, and the status says so.
    let response = reads
        .search(Some(&index), &params("lantern", &[], true, 50))
        .unwrap();
    assert_eq!(
        ids(&response),
        ["srch-unknown", "srch-live", "srch-archived"]
    );
    assert!(response.results.iter().all(|r| r.hits == 0));
    assert_eq!(response.index, index.status());
    assert!(!response.index.ready);
    assert_eq!(response.index.error, None);

    // Once it has indexed, the same search carries the chat as well.
    while index.index_step(&test.db()).unwrap() {}
    let response = reads
        .search(Some(&index), &params("lantern", &[], true, 50))
        .unwrap();
    assert!(ids(&response).contains(&"srch-quiet"));
    assert!(response.index.ready);
    assert_eq!(response.index.chunks, 1);
}

#[test]
fn a_failing_index_gives_no_chat_hits_and_its_error_kind_never_a_failed_answer() {
    let fx = fixture(lantern);
    // Another connection takes the chunk table away under the index.
    {
        let conn = Connection::open(fx.index_dir.path().join("search.db")).unwrap();
        conn.execute_batch("DROP TABLE chunks").unwrap();
    }
    let response = fx
        .reads
        .search(Some(&fx.index), &params("lantern", &[], true, 50))
        .expect("an answer, not an error");
    assert_eq!(
        ids(&response),
        ["srch-unknown", "srch-live", "srch-archived"]
    );
    assert!(response.results.iter().all(|r| r.hits == 0));
    let error = response.index.error.clone().expect("the error kind");
    assert!(error.starts_with("sqlite"), "{error}");
    let dir = fx.index_dir.path().to_string_lossy().into_owned();
    assert!(
        !error.contains(&dir) && !error.contains("chunks"),
        "{error}"
    );
}

#[test]
fn a_query_without_a_term_is_an_empty_answer_with_the_status() {
    let fx = fixture(lantern);
    let response = search(&fx, "  \"\" ?! ", &[], true, 50);
    assert!(response.results.is_empty());
    assert_eq!(response.index, fx.index.status());
    assert_eq!(response.query, "  \"\" ?! ");

    let scoped = search(&fx, "", &["srch-one", "srch-two"], false, 50);
    assert!(scoped.results.is_empty());
    assert_eq!(scoped.repos, ["srch-one", "srch-two"]);
}

#[test]
fn the_answer_serialises_as_the_web_app_reads_it() {
    let fx = fixture(lantern);
    let response = search(&fx, "lantern", &["srch-one"], true, 50);
    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["query"], "lantern");
    assert_eq!(json["repos"], serde_json::json!(["srch-one"]));
    assert_eq!(json["index"]["ready"], true);
    assert!(json["index"].get("error").is_none());
    let first = &json["results"][0];
    assert_eq!(first["workspace"]["id"], "srch-live");
    assert_eq!(first["workspace"]["archived"], false);
    assert_eq!(first["byName"], true);
    assert_eq!(first["sessionId"], "srch-chat-live");
    assert_eq!(first["sessionTitle"], "Trim the wick");
    assert_eq!(first["snippets"][0]["role"], "user");
    assert!(first["snippets"][0]["sessionId"].is_string());
    assert!(first["snippets"][0]["cursor"]
        .as_str()
        .unwrap()
        .starts_with('m'));
}
