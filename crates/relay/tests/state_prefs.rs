//! The synced preferences: merge rules, sanitising, the two error answers and persistence over
//! an in-memory store.

use std::sync::Arc;

use conductor_remote::contract::PrefsService;
use conductor_remote::files::attachments::attachment_token;
use conductor_remote::state::prefs::Prefs;
use conductor_remote::state::store::Store;
use serde_json::{json, Value};

const PATH: &str = ".context/attachments/abc123/diagram.png";

fn service() -> (Prefs, Arc<Store>) {
    let store = Arc::new(Store::open_in_memory().expect("in-memory store"));
    (Prefs::new(store.clone()), store)
}

fn attachment() -> Value {
    json!({
        "name": "diagram.png",
        "path": PATH,
        "bytes": 42,
        "token": attachment_token("diagram.png", PATH),
    })
}

fn draft(text: &str, updated_at: i64) -> Value {
    json!({ "text": text, "agent": {}, "attachments": [], "updatedAt": updated_at, "deleted": false })
}

fn tombstone(updated_at: i64) -> Value {
    json!({ "text": "", "agent": {}, "attachments": [], "updatedAt": updated_at, "deleted": true })
}

fn keys(value: &Value) -> Vec<&str> {
    value
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect()
}

#[test]
fn the_token_matches_the_phone_format() {
    assert_eq!(
        attachment()["token"],
        "@⟦diagram.png⟧(.context%2Fattachments%2Fabc123%2Fdiagram.png)"
    );
}

#[test]
fn an_empty_store_gives_an_empty_valid_document() {
    let (prefs, _) = service();
    assert_eq!(prefs.get(), json!({ "readMarks": {}, "drafts": {} }));
}

#[test]
fn a_patch_that_is_not_an_object_is_refused() {
    let (prefs, store) = service();
    for patch in [
        json!(null),
        json!(false),
        json!(0),
        json!(""),
        json!("readMarks"),
        json!([]),
        json!([{ "readMarks": {} }]),
    ] {
        assert_eq!(
            prefs.patch(patch.clone()),
            Err("preferences must be an object".to_owned()),
            "{patch}"
        );
    }
    assert_eq!(store.meta("prefs").expect("meta"), None);
}

#[test]
fn a_patch_without_either_key_has_nothing_to_sync() {
    let (prefs, store) = service();
    for patch in [
        json!({}),
        json!({ "other": 1 }),
        json!({ "readmarks": { "a": "1" } }),
    ] {
        assert_eq!(
            prefs.patch(patch.clone()),
            Err("nothing to sync".to_owned()),
            "{patch}"
        );
    }
    assert_eq!(store.meta("prefs").expect("meta"), None);
}

#[test]
fn a_present_key_with_an_invalid_value_is_sanitised_not_refused() {
    let (prefs, _) = service();
    for patch in [
        json!({ "readMarks": null }),
        json!({ "readMarks": 5 }),
        json!({ "readMarks": ["a"] }),
        json!({ "drafts": "x" }),
        json!({ "drafts": [draft("a", 1)] }),
        json!({ "readMarks": {}, "drafts": {} }),
    ] {
        assert_eq!(
            prefs.patch(patch.clone()),
            Ok(json!({ "readMarks": {}, "drafts": {} })),
            "{patch}"
        );
    }
}

#[test]
fn unknown_keys_next_to_a_valid_one_are_ignored() {
    let (prefs, _) = service();
    let doc = prefs
        .patch(json!({ "readMarks": { "a": "2026-08-01" }, "token": "x", "extra": 1 }))
        .expect("patch");
    assert_eq!(keys(&doc), ["readMarks", "drafts"]);
    assert_eq!(doc["readMarks"], json!({ "a": "2026-08-01" }));
}

#[test]
fn retains_fork_context_when_saving_and_reloading_a_transcript() {
    for bytes in [42u64, 30 * 1024 * 1024] {
        let (prefs, store) = service();
        let context = json!({
            "name": "diagram.png",
            "path": PATH,
            "bytes": bytes,
            "token": attachment_token("diagram.png", PATH),
            "source": "fork",
        });
        prefs
            .patch(json!({ "drafts": { "chat": {
                "text": "", "agent": {}, "attachments": [context.clone()],
                "updatedAt": 10, "deleted": false,
            } } }))
            .expect("patch");
        let reloaded = Prefs::new(store);
        assert_eq!(
            reloaded.get()["drafts"]["chat"]["attachments"],
            json!([context])
        );
    }
}

#[test]
fn a_large_attachment_that_is_not_a_fork_is_dropped() {
    let (prefs, _) = service();
    let mut big = attachment();
    big["bytes"] = json!(25 * 1024 * 1024 + 1);
    let mut edge = attachment();
    edge["bytes"] = json!(25 * 1024 * 1024);
    let doc = prefs
        .patch(json!({ "drafts": { "chat": {
            "text": "x", "agent": {}, "attachments": [big], "updatedAt": 1, "deleted": false,
        }, "edge": {
            "text": "x", "agent": {}, "attachments": [edge.clone()], "updatedAt": 1, "deleted": false,
        } } }))
        .expect("patch");
    assert_eq!(doc["drafts"]["chat"]["attachments"], json!([]));
    assert_eq!(doc["drafts"]["edge"]["attachments"], json!([edge]));
}

#[test]
fn keeps_known_effort_values_and_drops_unsupported_values() {
    let (prefs, _) = service();
    let doc = prefs
        .patch(json!({ "drafts": {
            "valid": { "text": "hello", "agent": { "effort": "none", "fast": false }, "updatedAt": 1 },
            "invalid": { "text": "hello", "agent": { "effort": "extreme", "fast": false }, "updatedAt": 1 },
        } }))
        .expect("patch");
    assert_eq!(
        doc["drafts"]["valid"]["agent"],
        json!({ "effort": "none", "fast": false })
    );
    assert_eq!(doc["drafts"]["invalid"]["agent"], json!({ "fast": false }));
}

#[test]
fn every_agent_effort_is_known() {
    let (prefs, _) = service();
    for effort in ["none", "low", "medium", "high", "xhigh", "max", "ultracode"] {
        let doc = prefs
            .patch(json!({ "drafts": { effort: {
                "text": "", "agent": { "effort": effort }, "updatedAt": 1,
            } } }))
            .expect("patch");
        assert_eq!(doc["drafts"][effort]["agent"], json!({ "effort": effort }));
    }
}

#[test]
fn agent_fields_are_checked_one_by_one() {
    let (prefs, _) = service();
    let long_model = "m".repeat(257);
    let doc = prefs
        .patch(json!({ "drafts": {
            "ok": { "text": "", "agent": {
                "auto": true, "model": "Codex", "effort": "high", "plan": true, "fast": false,
            }, "updatedAt": 1 },
            "bad": { "text": "", "agent": {
                "auto": "yes", "model": long_model, "effort": 3, "plan": 1, "fast": null, "other": 1,
            }, "updatedAt": 1 },
            "notobject": { "text": "", "agent": "Codex", "updatedAt": 1 },
        } }))
        .expect("patch");
    assert_eq!(
        doc["drafts"]["ok"]["agent"],
        json!({ "auto": true, "model": "Codex", "effort": "high", "plan": true, "fast": false })
    );
    assert_eq!(doc["drafts"]["bad"]["agent"], json!({}));
    assert_eq!(doc["drafts"]["notobject"]["agent"], json!({}));
}

#[test]
fn merges_read_marks_monotonically() {
    let (prefs, _) = service();
    prefs
        .patch(json!({ "readMarks": { "a": "2026-08-01", "b": "2026-08-03" } }))
        .expect("patch");
    let doc = prefs
        .patch(json!({ "readMarks": { "a": "2026-07-01", "b": "2026-08-04", "c": "2026-08-02" } }))
        .expect("patch");
    assert_eq!(
        doc["readMarks"],
        json!({ "a": "2026-08-01", "b": "2026-08-04", "c": "2026-08-02" })
    );
}

#[test]
fn read_marks_are_validated() {
    let (prefs, _) = service();
    let long_key = "k".repeat(257);
    let long_mark = "m".repeat(129);
    let doc = prefs
        .patch(json!({ "readMarks": {
            "good": "2026-08-01", "empty": "", "bad": 42, "null": null,
            "": "2026-08-01", long_key: "2026-08-01", "longmark": long_mark,
            "edge": "m".repeat(128),
        } }))
        .expect("patch");
    let mut found = keys(&doc["readMarks"]);
    found.sort_unstable();
    assert_eq!(found, ["edge", "good"]);
}

#[test]
fn read_marks_come_newest_first() {
    let (prefs, _) = service();
    let doc = prefs
        .patch(json!({ "readMarks": { "a": "2026-08-01", "b": "2026-08-03", "c": "2026-08-02" } }))
        .expect("patch");
    assert_eq!(keys(&doc["readMarks"]), ["b", "c", "a"]);
}

#[test]
fn keeps_a_deletion_tombstone_over_a_stale_or_tied_live_draft() {
    let (prefs, _) = service();
    prefs
        .patch(json!({ "drafts": { "chat": {
            "text": "already sent", "agent": { "model": "Sonnet" },
            "attachments": [attachment()], "updatedAt": 20, "deleted": false,
        } } }))
        .expect("patch");
    prefs
        .patch(json!({ "drafts": { "chat": tombstone(30) } }))
        .expect("patch");
    let mut stale = draft("stale", 29);
    stale["attachments"] = json!([attachment()]);
    prefs
        .patch(json!({ "drafts": { "chat": stale } }))
        .expect("patch");
    let mut tie = draft("tie", 30);
    tie["attachments"] = json!([attachment()]);
    prefs
        .patch(json!({ "drafts": { "chat": tie } }))
        .expect("patch");
    assert_eq!(prefs.get()["drafts"]["chat"], tombstone(30));
}

#[test]
fn a_deletion_wins_an_exact_tie_over_a_live_draft() {
    let (prefs, _) = service();
    prefs
        .patch(json!({ "drafts": { "chat": draft("live", 5) } }))
        .expect("patch");
    prefs
        .patch(json!({ "drafts": { "chat": tombstone(5) } }))
        .expect("patch");
    assert_eq!(prefs.get()["drafts"]["chat"], tombstone(5));
}

#[test]
fn a_tombstone_drops_whatever_the_client_sent_with_it() {
    let (prefs, _) = service();
    let doc = prefs
        .patch(json!({ "drafts": { "chat": {
            "text": "kept?", "agent": { "model": "Codex" },
            "attachments": [attachment()], "updatedAt": 3, "deleted": true,
        } } }))
        .expect("patch");
    assert_eq!(doc["drafts"]["chat"], tombstone(3));
}

#[test]
fn accepts_a_newer_edit_as_one_revision_with_staged_agent_settings() {
    let (prefs, _) = service();
    prefs
        .patch(json!({ "drafts": { "chat": tombstone(10) } }))
        .expect("patch");
    let doc = prefs
        .patch(json!({ "drafts": { "chat": {
            "text": "try another approach",
            "agent": { "model": "Codex", "effort": "high", "plan": true },
            "attachments": [attachment()], "updatedAt": 11, "deleted": false,
        } } }))
        .expect("patch");
    assert_eq!(
        doc["drafts"]["chat"],
        json!({
            "text": "try another approach",
            "agent": { "model": "Codex", "effort": "high", "plan": true },
            "attachments": [attachment()],
            "updatedAt": 11,
            "deleted": false,
        })
    );
}

#[test]
fn preserves_attachments_from_a_cached_client_unless_a_new_client_changes_them() {
    let (prefs, _) = service();
    let mut first = draft("caption", 10);
    first["attachments"] = json!([attachment()]);
    prefs
        .patch(json!({ "drafts": { "chat": first } }))
        .expect("patch");

    // A build from before attachment sync has no `attachments` field.
    let edited =
        json!({ "text": "edited caption", "agent": {}, "updatedAt": 11, "deleted": false });
    let doc = prefs
        .patch(json!({ "drafts": { "chat": edited } }))
        .expect("patch");
    assert_eq!(doc["drafts"]["chat"]["text"], "edited caption");
    assert_eq!(doc["drafts"]["chat"]["attachments"], json!([attachment()]));

    let doc = prefs
        .patch(json!({ "drafts": { "chat": draft("edited caption", 12) } }))
        .expect("patch");
    assert_eq!(doc["drafts"]["chat"]["attachments"], json!([]));
}

#[test]
fn sanitises_hand_edited_data_and_stores_the_returned_document() {
    let (prefs, store) = service();
    let unsafe_attachment = json!({
        "name": "..",
        "path": ".context/attachments/unsafe/..",
        "bytes": 1,
        "token": "@⟦..⟧(.context%2Fattachments%2Funsafe%2F..)",
    });
    let mut good = draft("hello", 1);
    good["agent"] = json!({ "model": "Codex", "plan": true });
    good["attachments"] = json!([attachment(), attachment(), unsafe_attachment]);
    let doc = prefs
        .patch(json!({
            "readMarks": { "good": "2026-08-01", "empty": "", "bad": 42 },
            "drafts": { "good": good, "bad": draft("ignored", -1) },
        }))
        .expect("patch");
    assert_eq!(doc["readMarks"], json!({ "good": "2026-08-01" }));
    assert_eq!(keys(&doc["drafts"]), ["good"]);
    assert_eq!(doc["drafts"]["good"]["attachments"], json!([attachment()]));
    let stored: Value =
        serde_json::from_str(&store.meta("prefs").expect("meta").expect("stored")).expect("json");
    assert_eq!(stored, doc);
}

#[test]
fn drafts_are_validated() {
    let (prefs, _) = service();
    let doc = prefs
        .patch(json!({ "drafts": {
            "ok": draft("", 0),
            "string-number": { "text": "x", "updatedAt": "7" },
            "float": { "text": "x", "updatedAt": 1.5 },
            "negative": draft("x", -1),
            "missing-time": { "text": "x" },
            "no-text": { "updatedAt": 1 },
            "text-number": { "text": 5, "updatedAt": 1 },
            "huge": { "text": "x".repeat(1_000_001), "updatedAt": 1 },
            "edge": { "text": "x".repeat(1_000_000), "updatedAt": 1 },
            "not-object": "x",
            "": draft("x", 1),
            "k".repeat(257): draft("x", 1),
        } }))
        .expect("patch");
    let mut found = keys(&doc["drafts"]);
    found.sort_unstable();
    assert_eq!(found, ["edge", "ok", "string-number"]);
    assert_eq!(doc["drafts"]["string-number"]["updatedAt"], 7);
}

#[test]
fn an_attachment_must_match_its_path_name_token_and_stage() {
    let (prefs, _) = service();
    let token = |name: &str, path: &str| attachment_token(name, path);
    let make = |name: &str, path: &str, token: String| json!({ "name": name, "path": path, "bytes": 1, "token": token });
    let good = attachment();
    let mut staged = attachment();
    staged["stageId"] = json!("abc123");
    let mut wrong_stage = attachment();
    wrong_stage["stageId"] = json!("zzz999");
    let mut null_stage = attachment();
    null_stage["stageId"] = json!(null);
    let mut negative = attachment();
    negative["bytes"] = json!(-1);
    let mut fractional = attachment();
    fractional["bytes"] = json!(1.5);
    let mut no_bytes = attachment();
    no_bytes.as_object_mut().expect("object").remove("bytes");
    let mut bad_token = attachment();
    bad_token["token"] = json!("@⟦diagram.png⟧(elsewhere)");
    let other_path = ".context/attachments/abc123/other.png";
    let candidates = vec![
        good.clone(),
        staged.clone(),
        wrong_stage,
        null_stage,
        negative,
        fractional,
        no_bytes,
        bad_token,
        // The name must be the last path segment.
        make("other.png", PATH, token("other.png", PATH)),
        // A stage id is six letters or digits.
        make(
            "diagram.png",
            ".context/attachments/abc12/diagram.png",
            token("diagram.png", ".context/attachments/abc12/diagram.png"),
        ),
        make(
            "diagram.png",
            ".elsewhere/abc123/diagram.png",
            token("diagram.png", ".elsewhere/abc123/diagram.png"),
        ),
        // The name must survive `attachment_name` unchanged.
        make(
            ".hidden",
            ".context/attachments/abc123/.hidden",
            token(".hidden", ".context/attachments/abc123/.hidden"),
        ),
        // A second file in the same stage is fine.
        make("other.png", other_path, token("other.png", other_path)),
        json!("not an object"),
    ];
    let doc = prefs
        .patch(json!({ "drafts": { "chat": {
            "text": "x", "agent": {}, "attachments": candidates, "updatedAt": 1, "deleted": false,
        } } }))
        .expect("patch");
    // `good` and `staged` share a path, so only the first survives.
    assert_eq!(
        doc["drafts"]["chat"]["attachments"],
        json!([
            good,
            make("other.png", other_path, token("other.png", other_path))
        ])
    );

    let doc = prefs
        .patch(json!({ "drafts": { "staged": {
            "text": "x", "agent": {}, "attachments": [staged.clone()], "updatedAt": 1, "deleted": false,
        } } }))
        .expect("patch");
    assert_eq!(doc["drafts"]["staged"]["attachments"], json!([staged]));
}

#[test]
fn at_most_a_hundred_attachments_are_kept() {
    let (prefs, _) = service();
    let many: Vec<Value> = (0..120)
        .map(|n| {
            let name = format!("f{n}.png");
            let path = format!(".context/attachments/abc123/{name}");
            json!({ "name": name, "path": path, "bytes": 1, "token": attachment_token(&name, &path) })
        })
        .collect();
    let doc = prefs
        .patch(json!({ "drafts": { "chat": {
            "text": "x", "agent": {}, "attachments": many, "updatedAt": 1, "deleted": false,
        } } }))
        .expect("patch");
    assert_eq!(
        doc["drafts"]["chat"]["attachments"]
            .as_array()
            .expect("array")
            .len(),
        100
    );
}

#[test]
fn a_patch_that_changes_nothing_keeps_the_document() {
    let (prefs, store) = service();
    let first = prefs
        .patch(json!({ "readMarks": { "a": "2026-08-01" }, "drafts": { "chat": draft("x", 1) } }))
        .expect("patch");
    let stored = store.meta("prefs").expect("meta");
    let again = prefs
        .patch(json!({ "readMarks": { "a": "2026-07-01" }, "drafts": { "chat": draft("x", 1) } }))
        .expect("patch");
    assert_eq!(again, first);
    assert_eq!(store.meta("prefs").expect("meta"), stored);
}

#[test]
fn a_patch_with_only_one_key_leaves_the_other_alone() {
    let (prefs, _) = service();
    prefs
        .patch(json!({ "readMarks": { "a": "2026-08-01" } }))
        .expect("patch");
    prefs
        .patch(json!({ "drafts": { "chat": draft("x", 1) } }))
        .expect("patch");
    let doc = prefs.get();
    assert_eq!(doc["readMarks"], json!({ "a": "2026-08-01" }));
    assert_eq!(doc["drafts"]["chat"]["text"], "x");
}

#[test]
fn the_document_persists_across_a_new_service_over_the_same_store() {
    let (prefs, store) = service();
    let patched = prefs
        .patch(json!({
            "readMarks": { "a": "2026-08-01" },
            "drafts": { "chat": {
                "text": "hello", "agent": { "effort": "max" },
                "attachments": [attachment()], "updatedAt": 4, "deleted": false,
            } },
        }))
        .expect("patch");

    let reopened = Prefs::new(store.clone());
    assert_eq!(reopened.get(), patched);

    // The merge works against what was stored, not against an empty document.
    let merged = reopened
        .patch(json!({ "readMarks": { "a": "2026-07-01", "b": "2026-08-02" } }))
        .expect("patch");
    assert_eq!(
        merged["readMarks"],
        json!({ "a": "2026-08-01", "b": "2026-08-02" })
    );
    assert_eq!(merged["drafts"], patched["drafts"]);
    assert_eq!(Prefs::new(store).get(), merged);
}

#[test]
fn a_malformed_stored_document_reads_as_empty() {
    let (_, store) = service();
    store.set_meta("prefs", "{not json").expect("set");
    assert_eq!(
        Prefs::new(store.clone()).get(),
        json!({ "readMarks": {}, "drafts": {} })
    );
    store
        .set_meta(
            "prefs",
            r#"{"readMarks":{"a":"1","b":5},"drafts":{"x":{"text":1,"updatedAt":1}}}"#,
        )
        .expect("set");
    assert_eq!(
        Prefs::new(store).get(),
        json!({ "readMarks": { "a": "1" }, "drafts": {} })
    );
}
