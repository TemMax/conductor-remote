mod support;

use std::fs;
use std::path::Path;

use conductor_remote::reads::{HostPaths, Reads};
use serde_json::{json, Value};
use support::TestDb;
use tempfile::TempDir;

struct Setup {
    _db: TestDb,
    home: TempDir,
    state: TempDir,
    reads: Reads,
}

fn setup() -> Setup {
    let db = TestDb::new();
    let home = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let reads = Reads::new(db.db(), db.root()).with_host_paths(HostPaths {
        home: home.path().to_path_buf(),
        state_dir: state.path().to_path_buf(),
    });
    Setup {
        _db: db,
        home,
        state,
        reads,
    }
}

impl Setup {
    fn write_cache(&self, content: &str) {
        fs::write(self.state.path().join("model-cache.json"), content).unwrap();
    }

    fn write_settings(&self, content: &str) {
        let dir = self.home.path().join(".conductor");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("settings.toml"), content).unwrap();
    }

    fn catalog(&self) -> Value {
        serde_json::to_value(self.reads.model_catalog()).unwrap()
    }

    fn defaults(&self) -> Value {
        serde_json::to_value(self.reads.model_defaults().unwrap()).unwrap()
    }
}

fn fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../web/tests/contract/fixtures")
        .join(name);
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

const CACHE: &str = r#"[
  { "agentType": " claude ", "models": ["Fable 5.1 NEW", "5.6 Sol", "5.6 Terra ", "5.6 Sol", 7],
    "defaultModel": "5.6 Sol NEW", "snapshotAt": 1760000000000,
    "snapshotModels": ["Fable 5.1", "5.6 Sol NEW", null],
    "selections": [ { "model": "5.6 Sol NEW", "selectedAt": 1760000000500 },
                    { "model": "x", "selectedAt": "bad" }, { "model": "  ", "selectedAt": 1 }, 3 ],
    "updatedAt": 1760000001000 },
  { "agentType": "codex", "models": ["5.6 Terra"], "defaultModel": "5.6 Terra",
    "snapshotAt": "soon", "updatedAt": 1760000002000 },
  { "agentType": "  ", "models": ["opencode-go/muse-spark"] },
  { "agentType": "empty", "models": [] },
  { "agentType": 5, "models": ["a"] },
  { "models": ["a"] },
  "junk"
]"#;

#[test]
fn normalizes_labels_and_keeps_the_pickers_apart() {
    let s = setup();
    s.write_cache(CACHE);
    let catalog = s.catalog();
    let groups = catalog["groups"].as_array().unwrap();
    let types: Vec<&str> = groups
        .iter()
        .map(|g| g["agentType"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["claude", "codex", "unknown"]);
    assert_eq!(
        groups[0]["models"],
        json!(["5.6 Sol", "5.6 Terra", "Fable 5.1"])
    );
    assert_eq!(groups[0]["snapshotModels"], json!(["5.6 Sol", "Fable 5.1"]));
    assert_eq!(groups[1]["snapshotAt"], Value::Null);
    assert!(groups[2].get("snapshotAt").is_none());
    assert!(groups[2].get("defaultModel").is_none());
    assert_eq!(groups[2]["updatedAt"], json!(0));
}

#[test]
fn the_default_is_that_of_the_newest_entry_and_the_earliest_wins_a_tie() {
    let s = setup();
    s.write_cache(CACHE);
    assert_eq!(s.catalog()["defaultModel"], "5.6 Terra");

    s.write_cache(
        r#"[
        { "agentType": "a", "models": ["m1"], "defaultModel": "m1", "updatedAt": 5 },
        { "agentType": "b", "models": ["m2"], "defaultModel": "m2", "updatedAt": 5 },
        { "agentType": "c", "models": ["m3"], "updatedAt": 9 } ]"#,
    );
    assert_eq!(s.catalog()["defaultModel"], "m1");
}

#[test]
fn a_cache_without_a_default_has_no_default_model_key() {
    let s = setup();
    s.write_cache(r#"[{ "agentType": "codex", "models": ["5.6 Sol"], "updatedAt": 1 }]"#);
    let catalog = s.catalog();
    assert!(catalog.get("defaultModel").is_none());
    assert_eq!(catalog["groups"].as_array().unwrap().len(), 1);
}

#[test]
fn labels_sort_by_lowercase_with_the_lowercase_spelling_first() {
    let s = setup();
    s.write_cache(
        r#"[{ "agentType": "x", "models": ["b", "Abc", "abc", "B", "a NEW"], "updatedAt": 1 }]"#,
    );
    assert_eq!(
        s.catalog()["groups"][0]["models"],
        json!(["a", "abc", "Abc", "b", "B"])
    );
}

#[test]
fn a_missing_or_broken_catalogue_is_empty() {
    let s = setup();
    assert_eq!(s.catalog(), json!({ "groups": [] }));
    for broken in ["not json", "{}", "", r#"{"groups": []}"#] {
        s.write_cache(broken);
        assert_eq!(s.catalog(), json!({ "groups": [] }), "{broken:?}");
    }
}

#[test]
fn the_catalogue_is_read_on_every_call() {
    let s = setup();
    assert_eq!(s.catalog(), json!({ "groups": [] }));
    s.write_cache(r#"[{ "agentType": "codex", "models": ["m"], "updatedAt": 1 }]"#);
    assert_eq!(s.catalog()["groups"].as_array().unwrap().len(), 1);
}

#[test]
fn reads_provider_specific_values_from_ordinary_and_quoted_tables() {
    let s = setup();
    s.write_settings(
        "[\"models\"].x = 1\n[models]\ndefault_effort_level = \"low\"\n\n[\"models\".\"claude_code\"]\ndefault_effort_level = \"max\" # keep this note\n\n[models.codex]\ndefault_thinking_level = 'xhigh'\n",
    );
    assert_eq!(
        s.defaults(),
        json!({ "defaultEfforts": { "claude": "max", "codex": "xhigh" } })
    );
}

#[test]
fn only_a_quoted_string_on_one_line_counts() {
    let s = setup();
    s.write_settings(
        "[models.claude_code]\ndefault_effort_level = high\n[models.codex]\ndefault_thinking_level = \"open\n",
    );
    assert_eq!(
        s.defaults(),
        json!({ "defaultEfforts": { "claude": null, "codex": null } })
    );
}

#[test]
fn an_absent_effort_is_null_and_a_missing_file_gives_both_null() {
    let s = setup();
    let none = json!({ "defaultEfforts": { "claude": null, "codex": null } });
    assert_eq!(s.defaults(), none);
    s.write_settings("[models.codex]\ndefault_thinking_level = \"high\"\n");
    assert_eq!(
        s.defaults(),
        json!({ "defaultEfforts": { "claude": null, "codex": "high" } })
    );
}

#[test]
fn another_read_error_is_returned() {
    let s = setup();
    // A directory where the file should be.
    fs::create_dir_all(s.home.path().join(".conductor").join("settings.toml")).unwrap();
    assert!(s.reads.model_defaults().is_err());
}

#[test]
fn without_host_paths_both_reads_are_empty() {
    let db = TestDb::new();
    let reads = Reads::new(db.db(), db.root());
    assert_eq!(
        serde_json::to_value(reads.model_catalog()).unwrap(),
        json!({ "groups": [] })
    );
    assert_eq!(
        serde_json::to_value(reads.model_defaults().unwrap()).unwrap(),
        json!({ "defaultEfforts": { "claude": null, "codex": null } })
    );
}

#[test]
fn golden_catalog() {
    let s = setup();
    s.write_cache(CACHE);
    assert_eq!(s.catalog(), fixture("models-catalog.json"));
}

#[test]
fn golden_defaults() {
    let s = setup();
    s.write_settings(
        "[models.claude_code]\ndefault_effort_level = \"max\"\n[models.codex]\ndefault_thinking_level = \"xhigh\"\n",
    );
    assert_eq!(s.defaults(), fixture("models-defaults.json"));
}

fn cache_value(s: &Setup) -> Value {
    serde_json::from_str(&fs::read_to_string(s.state.path().join("model-cache.json")).unwrap())
        .unwrap()
}

fn names(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

#[test]
fn a_first_record_creates_the_file_and_the_catalogue_serves_it() {
    use std::os::unix::fs::PermissionsExt;

    let s = setup();
    s.reads
        .record_models(" claude ", &names(&["b", "a"]), Some("a"), 100)
        .unwrap();
    let path = s.state.path().join("model-cache.json");
    let mode = fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(
        cache_value(&s),
        json!([{ "agentType": "claude", "models": ["b", "a"], "defaultModel": "a", "updatedAt": 100 }])
    );
    assert!(!s.state.path().join("model-cache.json.tmp").exists());
    let catalog = s.catalog();
    assert_eq!(catalog["groups"][0]["agentType"], "claude");
    assert_eq!(catalog["groups"][0]["models"], json!(["a", "b"]));
    assert_eq!(catalog["groups"][0]["defaultModel"], "a");
    assert_eq!(catalog["defaultModel"], "a");
}

#[test]
fn a_second_record_replaces_the_picker_and_keeps_the_other_keys() {
    let s = setup();
    s.write_cache(
        r#"[{ "agentType": "claude", "models": ["old"], "defaultModel": "old", "snapshotAt": 5,
        "selections": [{ "model": "old", "selectedAt": 7 }], "updatedAt": 1 }]"#,
    );
    s.reads
        .record_models("claude", &names(&["z", "y"]), Some("y"), 200)
        .unwrap();
    let value = cache_value(&s);
    let entries = value.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["models"], json!(["z", "y"]));
    assert_eq!(entries[0]["defaultModel"], "y");
    assert_eq!(entries[0]["updatedAt"], 200);
    assert_eq!(entries[0]["snapshotAt"], 5);
    assert_eq!(
        entries[0]["selections"],
        json!([{ "model": "old", "selectedAt": 7 }])
    );
}

#[test]
fn a_record_for_another_type_appends_and_keeps_the_first() {
    let s = setup();
    s.reads
        .record_models("claude", &names(&["a"]), None, 1)
        .unwrap();
    s.reads
        .record_models("", &names(&["b"]), Some("b"), 2)
        .unwrap();
    assert_eq!(
        cache_value(&s),
        json!([
            { "agentType": "claude", "models": ["a"], "updatedAt": 1 },
            { "agentType": "unknown", "models": ["b"], "defaultModel": "b", "updatedAt": 2 }
        ])
    );
}

#[test]
fn a_record_without_a_default_removes_the_key() {
    let s = setup();
    s.reads
        .record_models("claude", &names(&["a"]), Some("a"), 1)
        .unwrap();
    s.reads
        .record_models("claude", &names(&["a", "b"]), None, 2)
        .unwrap();
    let value = cache_value(&s);
    assert!(value[0].get("defaultModel").is_none());
    assert_eq!(value[0]["models"], json!(["a", "b"]));
    assert_eq!(value[0]["updatedAt"], 2);
}

#[test]
fn recording_no_models_leaves_the_file_byte_identical() {
    let s = setup();
    s.write_cache(CACHE);
    s.reads.record_models("claude", &[], Some("x"), 9).unwrap();
    assert_eq!(
        fs::read_to_string(s.state.path().join("model-cache.json")).unwrap(),
        CACHE
    );
    let fresh = setup();
    fresh.reads.record_models("claude", &[], None, 9).unwrap();
    assert!(!fresh.state.path().join("model-cache.json").exists());
}

#[test]
fn a_broken_file_is_replaced_by_a_one_entry_array() {
    let s = setup();
    for broken in ["not json", "{}", ""] {
        s.write_cache(broken);
        s.reads
            .record_models("codex", &names(&["m"]), None, 3)
            .unwrap();
        assert_eq!(
            cache_value(&s),
            json!([{ "agentType": "codex", "models": ["m"], "updatedAt": 3 }]),
            "{broken:?}"
        );
    }
}

#[test]
fn recording_without_host_paths_writes_nothing() {
    let db = TestDb::new();
    let reads = Reads::new(db.db(), db.root());
    reads
        .record_models("claude", &names(&["a"]), Some("a"), 1)
        .unwrap();
    assert_eq!(
        serde_json::to_value(reads.model_catalog()).unwrap(),
        json!({ "groups": [] })
    );
}
