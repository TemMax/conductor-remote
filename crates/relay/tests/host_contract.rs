use conductor_remote::contract::Services;
use conductor_remote::host::service::Host;
use conductor_remote::host::HostService;
use serde_json::json;

#[test]
fn services_default_has_no_host() {
    assert!(Services::default().host.is_none());
}

#[tokio::test]
async fn the_stub_answers_501_everywhere() {
    let host = Host;
    let answers = [
        host.logs(None, None),
        host.logs(Some("relay".into()), Some(10)),
        host.settings(),
        host.nosleep(),
        host.arm_nosleep(60),
        host.disarm_nosleep(),
        host.restart_conductor(false).await,
        host.restart_conductor(true).await,
    ];
    for answer in answers {
        assert_eq!(answer.status, 501);
        assert_eq!(answer.body, json!({ "error": "not implemented" }));
    }
}
