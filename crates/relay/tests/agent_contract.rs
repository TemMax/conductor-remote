//! The shared types and trait seams of agent options.

use conductor_remote::agent::{AgentPatch, Effort};
use conductor_remote::ui::driver::{AgentFailure, Target, UiDriver, UiError, ViewReport};
use serde_json::{json, Map, Value};

fn object(value: Value) -> Map<String, Value> {
    value.as_object().expect("an object").clone()
}

fn patch_of(value: Value) -> Result<AgentPatch, String> {
    AgentPatch::from_object(&object(value))
}

#[test]
fn effort_round_trips_through_its_text() {
    let names = ["none", "low", "medium", "high", "xhigh", "max", "ultracode"];
    assert_eq!(Effort::ALL.len(), names.len());
    for (effort, name) in Effort::ALL.into_iter().zip(names) {
        assert_eq!(effort.as_str(), name);
        assert_eq!(Effort::parse(name), Some(effort));
    }
    assert_eq!(Effort::parse("extra"), None);
}

#[test]
fn effort_menu_labels() {
    let expected: [(Effort, &[&str]); 7] = [
        (Effort::None, &["Off"]),
        (Effort::Low, &["Low"]),
        (Effort::Medium, &["Medium"]),
        (Effort::High, &["High"]),
        (Effort::Xhigh, &["Extra high"]),
        (Effort::Max, &["Max"]),
        (Effort::Ultracode, &["Ultracode", "Ultra"]),
    ];
    for (effort, labels) in expected {
        assert_eq!(effort.menu_labels(), labels);
    }
}

#[test]
fn from_object_reads_nothing_from_an_empty_or_null_object() {
    assert_eq!(patch_of(json!({})), Ok(AgentPatch::default()));
    let nulls = json!({"model": null, "effort": null, "plan": null, "fast": null});
    assert_eq!(patch_of(nulls), Ok(AgentPatch::default()));
}

#[test]
fn from_object_trims_the_model() {
    let patch = patch_of(json!({"model": "  opus  "})).unwrap();
    assert_eq!(patch.model.as_deref(), Some("opus"));
    let patch = patch_of(json!({"model": "   "})).unwrap();
    assert_eq!(patch.model, None);
    assert_eq!(
        patch_of(json!({"model": 3})),
        Err("model: must be a string".to_owned())
    );
    assert_eq!(
        patch_of(json!({"model": true})),
        Err("model: must be a string".to_owned())
    );
}

#[test]
fn from_object_checks_the_effort() {
    let message = "effort: must be one of none, low, medium, high, xhigh, max, ultracode";
    assert_eq!(
        patch_of(json!({"effort": "xhigh"})).unwrap().effort,
        Some(Effort::Xhigh)
    );
    assert_eq!(
        patch_of(json!({"effort": "extra"})),
        Err(message.to_owned())
    );
    assert_eq!(patch_of(json!({"effort": "High"})), Err(message.to_owned()));
    assert_eq!(patch_of(json!({"effort": 2})), Err(message.to_owned()));
}

#[test]
fn from_object_checks_the_booleans() {
    assert_eq!(patch_of(json!({"plan": true})).unwrap().plan, Some(true));
    assert_eq!(patch_of(json!({"fast": false})).unwrap().fast, Some(false));
    assert_eq!(
        patch_of(json!({"plan": "yes"})),
        Err("plan: must be a boolean".to_owned())
    );
    assert_eq!(
        patch_of(json!({"fast": 1})),
        Err("fast: must be a boolean".to_owned())
    );
}

#[test]
fn from_object_checks_the_fields_in_order() {
    let all_bad = json!({"model": 1, "effort": 1, "plan": 1, "fast": 1});
    assert_eq!(patch_of(all_bad), Err("model: must be a string".to_owned()));
    let rest_bad = json!({"effort": 1, "plan": 1, "fast": 1});
    assert!(patch_of(rest_bad).unwrap_err().starts_with("effort:"));
    assert_eq!(
        patch_of(json!({"plan": 1, "fast": 1})),
        Err("plan: must be a boolean".to_owned())
    );
}

#[test]
fn from_object_ignores_other_keys_and_reads_all_four() {
    assert_eq!(
        patch_of(json!({"text": "hi", "agent": 5})),
        Ok(AgentPatch::default())
    );
    let patch = patch_of(json!({
        "model": " sonnet ", "effort": "max", "plan": true, "fast": false, "other": 1
    }))
    .unwrap();
    assert_eq!(
        patch,
        AgentPatch {
            model: Some("sonnet".to_owned()),
            effort: Some(Effort::Max),
            plan: Some(true),
            fast: Some(false),
        }
    );
    assert!(!patch.is_empty());
}

#[test]
fn json_round_trips() {
    let patch = AgentPatch {
        model: Some("opus".to_owned()),
        effort: Some(Effort::Ultracode),
        plan: Some(false),
        fast: Some(true),
    };
    assert_eq!(
        patch.to_json(),
        r#"{"model":"opus","effort":"ultracode","plan":false,"fast":true}"#
    );
    assert_eq!(AgentPatch::from_json(&patch.to_json()), Some(patch));
    assert_eq!(AgentPatch::default().to_json(), "{}");
    assert_eq!(AgentPatch::from_json("{}"), Some(AgentPatch::default()));
    assert_eq!(AgentPatch::from_json("nope"), None);
}

#[test]
fn the_default_patch_is_empty() {
    assert!(AgentPatch::default().is_empty());
    let fast = AgentPatch {
        fast: Some(false),
        ..AgentPatch::default()
    };
    assert!(!fast.is_empty());
}

#[test]
fn new_errors_have_their_texts() {
    let cases = [
        (
            UiError::NoModelPicker,
            "couldn't find the model picker in Conductor's composer",
        ),
        (
            UiError::MenuNotOpened("Effort".to_owned()),
            "Conductor did not open its Effort menu",
        ),
        (
            UiError::MenuStuck,
            "couldn't close Conductor's menu - close it on your Mac and try again.",
        ),
        (
            UiError::NoModel("x".to_owned()),
            "Conductor's model list has no model named x",
        ),
        (
            UiError::SeveralModels("son".to_owned()),
            "several models match son - use the full name",
        ),
        (
            UiError::ModelNotApplied("opus".to_owned()),
            "Conductor did not switch to opus",
        ),
        (
            UiError::NoEffort("max".to_owned()),
            "this model has no max effort",
        ),
        (
            UiError::EffortNotApplied("max".to_owned()),
            "Conductor did not set the effort to max",
        ),
        (UiError::NoFast, "this model has no Fast mode"),
        (
            UiError::FastNotApplied,
            "Conductor did not change Fast mode",
        ),
        (UiError::NoPlan, "this chat has no Plan mode"),
        (
            UiError::PlanNotApplied,
            "Conductor did not change Plan mode",
        ),
    ];
    for (error, text) in cases {
        assert_eq!(error.to_string(), text);
        assert!(error.sent_nothing(), "{error:?}");
        assert!(!error.is_lock(), "{error:?}");
        assert!(!error.retry_wont_help(), "{error:?}");
    }
}

#[test]
fn failure_converts_from_an_error() {
    let failure = AgentFailure::from(UiError::NoFast);
    assert_eq!(failure.error, UiError::NoFast);
    assert!(!failure.new_chat);
}

struct Minimal;

impl UiDriver for Minimal {
    fn trusted(&self) -> bool {
        true
    }
    fn send_prompt(&mut self, _: &Target, _: &str, _: bool) -> Result<u32, UiError> {
        Ok(1)
    }
    fn stop_turn(&mut self, _: &Target) -> Result<(), UiError> {
        Ok(())
    }
    fn new_chat(&mut self, _: &Target) -> Result<(), UiError> {
        Ok(())
    }
    fn locate(&mut self) -> Result<ViewReport, UiError> {
        Ok(ViewReport::default())
    }
    fn open_link(&mut self, _: &str) -> Result<(), UiError> {
        Ok(())
    }
}

#[test]
fn a_driver_without_agent_support_says_so() {
    let target = Target {
        workspace_id: "w1".to_owned(),
        session_id: Some("s1".to_owned()),
        repo: None,
        branch: "main".to_owned(),
        workspace_name: None,
        tab: None,
    };
    let mut driver = Minimal;
    assert_eq!(driver.list_models(&target), Err(UiError::NoModelPicker));
    assert_eq!(
        driver.set_agent(&target, &AgentPatch::default()),
        Err(AgentFailure {
            error: UiError::NoModelPicker,
            new_chat: false
        })
    );
}
