use conductor_remote::notify::{chat_route, device_id, DeviceInfo, PushConfig, PushMessage};
use serde_json::json;

#[test]
fn device_info_is_camel_case_with_null_options() {
    let info = DeviceInfo {
        id: "d1".into(),
        label: "Phone".into(),
        created_at: 10,
        last_ok_at: None,
        last_error: None,
        failures: 0,
    };
    assert_eq!(
        serde_json::to_value(&info).unwrap(),
        json!({
            "id": "d1",
            "label": "Phone",
            "createdAt": 10,
            "lastOkAt": null,
            "lastError": null,
            "failures": 0
        })
    );
    let full = DeviceInfo {
        last_ok_at: Some(20),
        last_error: Some("gone".into()),
        failures: 3,
        ..info
    };
    assert_eq!(
        serde_json::to_value(&full).unwrap(),
        json!({
            "id": "d1",
            "label": "Phone",
            "createdAt": 10,
            "lastOkAt": 20,
            "lastError": "gone",
            "failures": 3
        })
    );
}

#[test]
fn push_config_is_camel_case() {
    let config = PushConfig {
        enabled: true,
        public_key: "BKey".into(),
        devices: vec![DeviceInfo {
            id: "d1".into(),
            label: "Phone".into(),
            created_at: 10,
            last_ok_at: None,
            last_error: None,
            failures: 1,
        }],
    };
    assert_eq!(
        serde_json::to_value(&config).unwrap(),
        json!({
            "enabled": true,
            "publicKey": "BKey",
            "devices": [{
                "id": "d1",
                "label": "Phone",
                "createdAt": 10,
                "lastOkAt": null,
                "lastError": null,
                "failures": 1
            }]
        })
    );
}

#[test]
fn push_message_keeps_its_field_names() {
    let message = PushMessage {
        title: "Done".into(),
        body: "The turn ended".into(),
        tag: "s1".into(),
        url: "/w/w1?session=s1".into(),
        kind: "done".into(),
        ts: 1_700_000_000_000,
    };
    assert_eq!(
        serde_json::to_value(&message).unwrap(),
        json!({
            "title": "Done",
            "body": "The turn ended",
            "tag": "s1",
            "url": "/w/w1?session=s1",
            "kind": "done",
            "ts": 1_700_000_000_000_i64
        })
    );
}

#[test]
fn chat_route_encodes_only_the_session_id() {
    assert_eq!(chat_route("w 1", "s/2?x"), "/w/w 1?session=s%2F2%3Fx");
    assert_eq!(chat_route("w", "!~*'()-_."), "/w/w?session=!~*'()-_.");
    assert_eq!(chat_route("w", "a b&c=d"), "/w/w?session=a%20b%26c%3Dd");
    assert_eq!(chat_route("w", "é"), "/w/w?session=%C3%A9");
}

#[test]
fn device_id_is_sixteen_lowercase_hex_characters() {
    let id = device_id("https://push.example/abc");
    assert_eq!(id.len(), 16);
    assert!(id.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
    assert_eq!(id, device_id("https://push.example/abc"));
    assert_ne!(id, device_id("https://push.example/abd"));
    // printf '%s' 'https://push.example/abc' | shasum -a 256
    // The first 16 hex characters of that digest:
    assert_eq!(id, "f7a263f8786e9f95");
}
