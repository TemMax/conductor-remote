use conductor_remote::contract::{
    Assets, ConductorControl, ConductorStatus, LaunchResponse, StateResponse, Token,
};
use conductor_remote::testing::{FakeConductor, MemoryAssets};
use serde_json::json;

#[test]
fn token_accepts_exact_token() {
    assert!(Token::new("secret-token").matches("secret-token"));
}

#[test]
fn token_rejects_different_token_of_same_length() {
    assert!(!Token::new("secret-token").matches("secret-tokeN"));
}

#[test]
fn token_rejects_shorter_candidate() {
    assert!(!Token::new("secret-token").matches("secret-toke"));
}

#[test]
fn token_rejects_longer_candidate() {
    assert!(!Token::new("secret-token").matches("secret-token!"));
}

#[test]
fn token_rejects_empty_candidate() {
    assert!(!Token::new("secret-token").matches(""));
}

#[test]
fn token_debug_does_not_leak_the_token() {
    let token = Token::new("secret-token");
    assert!(!format!("{:?}", token).contains("secret-token"));
}

#[test]
fn skeleton_running_serialises_to_the_wire_shape() {
    let value = serde_json::to_value(StateResponse::skeleton(ConductorStatus::Running)).unwrap();
    assert_eq!(
        value,
        json!({
            "workspaces": [],
            "actuator": {"name": "accessibility", "caveat": "", "precise": true, "available": false},
            "version": env!("CARGO_PKG_VERSION"),
            "conductor": {"running": true}
        })
    );
}

#[test]
fn skeleton_not_running_reports_running_false() {
    let value = serde_json::to_value(StateResponse::skeleton(ConductorStatus::NotRunning)).unwrap();
    assert_eq!(value["conductor"], json!({"running": false}));
}

#[test]
fn launch_response_without_error_omits_the_field() {
    let value = serde_json::to_value(LaunchResponse {
        ok: true,
        error: None,
    })
    .unwrap();
    assert_eq!(value, json!({"ok": true}));
}

#[test]
fn fake_conductor_launch_counts_calls() {
    let fake = FakeConductor::new(ConductorStatus::NotRunning);
    fake.launch().unwrap();
    fake.launch().unwrap();
    assert_eq!(fake.launches(), 2);
}

#[test]
fn fake_conductor_launch_flips_status_to_running() {
    let fake = FakeConductor::new(ConductorStatus::NotRunning);
    fake.launch().unwrap();
    assert_eq!(fake.status(), ConductorStatus::Running);
}

#[test]
fn fake_conductor_launch_notifies_a_subscriber() {
    let fake = FakeConductor::new(ConductorStatus::NotRunning);
    let mut rx = fake.subscribe();
    fake.launch().unwrap();
    assert!(rx.has_changed().unwrap());
    assert_eq!(*rx.borrow_and_update(), ConductorStatus::Running);
}

#[test]
fn fake_conductor_returns_configured_error_and_keeps_status() {
    let fake = FakeConductor::new(ConductorStatus::NotRunning);
    fake.fail_launch_with("boom");
    let err = fake.launch().unwrap_err();
    assert_eq!(err.0, "boom");
    assert_eq!(fake.status(), ConductorStatus::NotRunning);
    assert_eq!(fake.launches(), 1);
}

#[test]
fn memory_assets_returns_what_was_stored() {
    let assets = MemoryAssets::default().with("index.html", "text/html", b"<p>hi</p>");
    let asset = assets.get("index.html").unwrap();
    assert_eq!(asset.bytes.as_ref(), b"<p>hi</p>");
    assert_eq!(asset.content_type, "text/html");
}

#[test]
fn memory_assets_returns_none_for_unknown_path() {
    let assets = MemoryAssets::default().with("index.html", "text/html", b"x");
    assert!(assets.get("missing.js").is_none());
}
