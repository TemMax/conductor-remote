//! The dev server's contract: the JSON the phone reads, the new `UiError` texts and the
//! `UiDriver::run_task` default.

use conductor_remote::contract::Services;
use conductor_remote::dev::{DevRunConfig, DevServerForward, DevServerResult, DevServerState};
use conductor_remote::ui::driver::{Target, UiDriver, UiError, ViewReport};
use serde_json::json;

fn idle() -> DevServerState {
    DevServerState {
        available: true,
        running: false,
        forwarded: false,
        port: None,
        url: None,
        forwards: Vec::new(),
        run_configs: Vec::new(),
        task: None,
        error: None,
    }
}

#[test]
fn a_state_serialises_in_camel_case_with_null_port_and_url() {
    let state = DevServerState {
        running: true,
        forwards: vec![DevServerForward {
            name: "web".to_owned(),
            port: 5173,
            running: true,
            forwarded: false,
            url: None,
        }],
        run_configs: vec![DevRunConfig {
            id: "dev".to_owned(),
            name: "Dev".to_owned(),
            command: "npm run dev".to_owned(),
        }],
        ..idle()
    };
    assert_eq!(
        serde_json::to_value(&state).unwrap(),
        json!({
            "available": true,
            "running": true,
            "forwarded": false,
            "port": null,
            "url": null,
            "forwards": [
                { "name": "web", "port": 5173, "running": true, "forwarded": false, "url": null }
            ],
            "runConfigs": [{ "id": "dev", "name": "Dev", "command": "npm run dev" }],
        })
    );
}

#[test]
fn task_and_error_appear_only_when_set() {
    let state = DevServerState {
        task: Some("Dev".to_owned()),
        error: Some("no Run strip".to_owned()),
        port: Some(5173),
        url: Some("https://mac.tail.ts.net:5173".to_owned()),
        ..idle()
    };
    let value = serde_json::to_value(&state).unwrap();
    assert_eq!(value["task"], "Dev");
    assert_eq!(value["error"], "no Run strip");
    assert_eq!(value["port"], 5173);
    assert_eq!(value["url"], "https://mac.tail.ts.net:5173");
}

#[test]
fn a_result_is_flat_and_leaves_changed_out_when_none() {
    let result = DevServerResult {
        ok: true,
        state: idle(),
        changed: None,
    };
    let value = serde_json::to_value(&result).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["available"], true);
    assert!(value.get("state").is_none());
    assert!(value.get("changed").is_none());

    let changed = DevServerResult {
        changed: Some(false),
        ..result
    };
    assert_eq!(
        serde_json::to_value(&changed).unwrap()["changed"],
        json!(false)
    );
}

#[test]
fn the_run_errors_read_as_specified() {
    let cases = [
        (
            UiError::NoRunStrip,
            "couldn't find the Run controls of this workspace",
        ),
        (
            UiError::NoRunTask("Dev".to_owned()),
            "Conductor has no Run task named Dev",
        ),
        (
            UiError::RunNotChanged,
            "Conductor did not start or stop the Run task",
        ),
    ];
    for (error, text) in cases {
        assert_eq!(error.to_string(), text);
    }
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
fn a_driver_without_a_run_strip_says_there_is_no_window() {
    let target = Target {
        workspace_id: "w1".to_owned(),
        session_id: None,
        repo: None,
        branch: "main".to_owned(),
        workspace_name: None,
        tab: None,
    };
    let mut driver = Minimal;
    assert!(matches!(
        driver.run_task(&target, None, true),
        Err(UiError::NoWindow)
    ));
    assert!(matches!(
        driver.run_task(&target, Some("Dev"), false),
        Err(UiError::NoWindow)
    ));
}

#[test]
fn services_default_has_no_dev() {
    assert!(Services::default().dev.is_none());
}
