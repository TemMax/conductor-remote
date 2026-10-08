//! The shapes of the search and usage responses: each serialises with exactly the keys of its
//! TypeScript type.

use std::collections::BTreeSet;

use conductor_remote::reads::workspaces::SearchWorkspace;
use conductor_remote::search::index::{IndexStatus, SearchIndex};
use conductor_remote::search::results::{SearchResponse, SearchResult, SearchRole, SearchSnippet};
use conductor_remote::usage::plan::{
    PlanUsageBucket, PlanUsageProviderId, PlanUsageService, PlanUsageSnapshot, PlanUsageStatus,
    PlanUsageWindow, ProviderPlanUsage,
};
use conductor_remote::usage::tools::{
    ToolRange, ToolUsageProvider, ToolUsageRow, ToolUsageService, ToolUsageSnapshot,
};
use serde_json::{json, Value};

fn keys(value: &Value) -> BTreeSet<&str> {
    value
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect()
}

fn set(names: &[&'static str]) -> BTreeSet<&'static str> {
    names.iter().copied().collect()
}

#[test]
fn services_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SearchIndex>();
    assert_send_sync::<PlanUsageService>();
    assert_send_sync::<ToolUsageService>();
}

#[test]
fn index_status_leaves_out_an_absent_error() {
    let mut status = IndexStatus {
        chunks: 3,
        ready: true,
        progress: 1.0,
        error: None,
    };
    let value = serde_json::to_value(&status).unwrap();
    assert_eq!(keys(&value), set(&["chunks", "ready", "progress"]));

    status.error = Some("cannot open".to_string());
    let value = serde_json::to_value(&status).unwrap();
    assert_eq!(keys(&value), set(&["chunks", "ready", "progress", "error"]));
    assert_eq!(value["error"], json!("cannot open"));
}

fn workspace() -> SearchWorkspace {
    SearchWorkspace {
        id: "ws".to_string(),
        workspace_name: None,
        pr_title: None,
        branch: Some("main".to_string()),
        directory_name: None,
        state: None,
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        repo_name: None,
        icon: None,
        archived: false,
    }
}

#[test]
fn search_response_has_the_keys_of_the_web_type() {
    let response = SearchResponse {
        query: "needle".to_string(),
        repos: vec!["repo".to_string()],
        index: IndexStatus {
            chunks: 0,
            ready: false,
            progress: 0.0,
            error: None,
        },
        results: vec![SearchResult {
            workspace: workspace(),
            session_id: None,
            hits: 2,
            score: 1.5,
            at: None,
            snippets: vec![SearchSnippet {
                session_id: "s".to_string(),
                cursor: "7".to_string(),
                role: SearchRole::Thinking,
                at: "2026-01-01T00:00:00Z".to_string(),
                text: "a needle".to_string(),
            }],
            by_name: true,
            session_title: None,
        }],
    };
    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(keys(&value), set(&["query", "repos", "index", "results"]));
    let result = &value["results"][0];
    assert_eq!(
        keys(result),
        set(&[
            "workspace",
            "sessionId",
            "hits",
            "score",
            "at",
            "snippets",
            "byName",
            "sessionTitle"
        ])
    );
    assert_eq!(result["sessionId"], Value::Null);
    assert_eq!(result["at"], Value::Null);
    assert_eq!(result["sessionTitle"], Value::Null);
    assert_eq!(
        keys(&result["workspace"]),
        set(&[
            "id",
            "workspace_name",
            "pr_title",
            "branch",
            "directory_name",
            "state",
            "updated_at",
            "repo_name",
            "icon",
            "archived"
        ])
    );
    let snippet = &result["snippets"][0];
    assert_eq!(
        keys(snippet),
        set(&["sessionId", "cursor", "role", "at", "text"])
    );
    assert_eq!(snippet["role"], json!("thinking"));
}

#[test]
fn search_roles_serialise_in_lower_case() {
    assert_eq!(
        serde_json::to_value(SearchRole::User).unwrap(),
        json!("user")
    );
    assert_eq!(
        serde_json::to_value(SearchRole::Assistant).unwrap(),
        json!("assistant")
    );
}

fn window(duration: Option<Option<i64>>, active: Option<bool>) -> PlanUsageWindow {
    PlanUsageWindow {
        id: "five_hour".to_string(),
        label: "5 hours".to_string(),
        used_percent: 42.0,
        resets_at: None,
        window_duration_mins: duration,
        active,
    }
}

#[test]
fn plan_usage_window_optional_keys() {
    let bare = serde_json::to_value(window(None, None)).unwrap();
    assert_eq!(
        keys(&bare),
        set(&["id", "label", "usedPercent", "resetsAt"])
    );
    assert_eq!(bare["resetsAt"], Value::Null);

    let full = serde_json::to_value(window(Some(Some(300)), Some(true))).unwrap();
    assert_eq!(
        keys(&full),
        set(&[
            "id",
            "label",
            "usedPercent",
            "resetsAt",
            "windowDurationMins",
            "active"
        ])
    );
    assert_eq!(full["windowDurationMins"], json!(300));

    let null_duration = serde_json::to_value(window(Some(None), None)).unwrap();
    assert_eq!(null_duration["windowDurationMins"], Value::Null);
    assert!(null_duration.get("active").is_none());
}

#[test]
fn plan_usage_snapshot_has_the_keys_of_the_web_type() {
    let mut provider = ProviderPlanUsage {
        provider: PlanUsageProviderId::Opencode,
        label: "OpenCode".to_string(),
        status: PlanUsageStatus::Available,
        plan: None,
        buckets: vec![PlanUsageBucket {
            id: "b".to_string(),
            label: "Bucket".to_string(),
            windows: vec![window(None, None)],
        }],
        message: None,
    };
    let snapshot = |provider: &ProviderPlanUsage| PlanUsageSnapshot {
        providers: vec![provider.clone()],
        fetched_at: 1_700_000_000_000,
    };

    let value = serde_json::to_value(snapshot(&provider)).unwrap();
    assert_eq!(keys(&value), set(&["providers", "fetchedAt"]));
    let first = &value["providers"][0];
    assert_eq!(
        keys(first),
        set(&["provider", "label", "status", "plan", "buckets"])
    );
    assert_eq!(first["provider"], json!("opencode"));
    assert_eq!(first["status"], json!("available"));
    assert_eq!(first["plan"], Value::Null);
    assert_eq!(keys(&first["buckets"][0]), set(&["id", "label", "windows"]));

    provider.message = Some("not signed in".to_string());
    provider.status = PlanUsageStatus::Error;
    let value = serde_json::to_value(snapshot(&provider)).unwrap();
    let first = &value["providers"][0];
    assert_eq!(
        keys(first),
        set(&["provider", "label", "status", "plan", "buckets", "message"])
    );
    assert_eq!(first["status"], json!("error"));
}

#[test]
fn provider_ids_serialise_in_lower_case() {
    let ids = [
        PlanUsageProviderId::Claude,
        PlanUsageProviderId::Codex,
        PlanUsageProviderId::Cursor,
        PlanUsageProviderId::Opencode,
    ];
    let values: Vec<Value> = ids
        .iter()
        .map(|id| serde_json::to_value(id).unwrap())
        .collect();
    assert_eq!(
        values,
        vec![
            json!("claude"),
            json!("codex"),
            json!("cursor"),
            json!("opencode")
        ]
    );
}

#[test]
fn tool_usage_snapshot_has_the_keys_of_the_web_type() {
    let snapshot = ToolUsageSnapshot {
        range: ToolRange::Week,
        since: "2026-01-01T00:00:00.000Z".to_string(),
        until: "2026-01-08T00:00:00.000Z".to_string(),
        fetched_at: 1_700_000_000_000,
        providers: vec![ToolUsageProvider {
            provider: "claude".to_string(),
            session_count: 1,
            tools: vec![ToolUsageRow {
                name: None,
                calls: 2,
                input_tokens: 3,
                output_tokens: 4,
                total_tokens: 7,
                largest_call_tokens: 5,
            }],
        }],
    };
    let value = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(
        keys(&value),
        set(&["range", "since", "until", "fetchedAt", "providers"])
    );
    assert_eq!(value["range"], json!("7d"));
    let provider = &value["providers"][0];
    assert_eq!(keys(provider), set(&["provider", "sessionCount", "tools"]));
    let row = &provider["tools"][0];
    assert_eq!(
        keys(row),
        set(&[
            "name",
            "calls",
            "inputTokens",
            "outputTokens",
            "totalTokens",
            "largestCallTokens"
        ])
    );
    assert_eq!(row["name"], Value::Null);
}

#[test]
fn tool_range_serialises_as_the_web_range() {
    assert_eq!(serde_json::to_value(ToolRange::Day).unwrap(), json!("24h"));
    assert_eq!(serde_json::to_value(ToolRange::Week).unwrap(), json!("7d"));
    assert_eq!(
        serde_json::to_value(ToolRange::Month).unwrap(),
        json!("30d")
    );
}

#[test]
fn tool_range_parse() {
    assert_eq!(ToolRange::parse(None), Ok(ToolRange::Day));
    assert_eq!(ToolRange::parse(Some("24h")), Ok(ToolRange::Day));
    assert_eq!(ToolRange::parse(Some("7d")), Ok(ToolRange::Week));
    assert_eq!(ToolRange::parse(Some("30d")), Ok(ToolRange::Month));
    assert_eq!(ToolRange::parse(Some("")), Err(()));
    assert_eq!(ToolRange::parse(Some("90d")), Err(()));
    assert_eq!(ToolRange::parse(Some("7D")), Err(()));
}
