use conductor_remote::transcript::{parse_message, StoredMessage};
use serde_json::{json, Value};

fn parsed(frame: Value) -> Value {
    serde_json::to_value(parse_message(
        &StoredMessage {
            rowid: 7,
            id: "sample-frame".into(),
            content: Some(frame.to_string()),
            created_at: None,
            sent_at: None,
            queue_order: None,
        },
        None,
    ))
    .unwrap()
}

#[test]
fn claude_preserves_questions_options_and_multiple_selection() {
    let entries = parsed(json!({"type":"assistant","message":{"content":[{
        "type":"tool_use","id":"question-colour","name":"mcp__conductor__AskUserQuestion",
        "input":{"questions":[{"header":"Colour","question":"Choose colours","multiSelect":true,
            "options":[{"label":"Red","description":"Warm"},{"label":"Blue","description":"Cool"}]}]}
    }]}}));
    assert_eq!(entries[0]["question"]["id"], "question-colour");
    assert_eq!(entries[0]["question"]["provider"], "claude");
    assert_eq!(entries[0]["question"]["questions"][0]["multiSelect"], true);
    assert_eq!(
        entries[0]["question"]["questions"][0]["options"][1]["description"],
        "Cool"
    );
}

#[test]
fn codex_question_has_a_stable_identity_across_duplicate_frames() {
    let entries = parsed(json!({"type":"assistant","codex_async_questions":{
        "id":"call-sample","questions":[{"question":"Choose colour","options":["Red","Blue"]}]
    },"message":{"content":[{"type":"text","text":"Choose colour\nRed\nBlue"}]}}));
    assert_eq!(entries.as_array().unwrap().len(), 1);
    assert_eq!(entries[0]["id"], "question:codex:call-sample");
    assert_eq!(
        entries[0]["question"]["questions"][0]["options"][0]["label"],
        "Red"
    );
}

#[test]
fn malformed_or_unrelated_tool_calls_are_not_answerable() {
    for (name, input) in [
        ("AskUserQuestion", json!({"questions":[]})),
        (
            "OtherTool",
            json!({"questions":[{"question":"Choose","options":["Red"]}]}),
        ),
    ] {
        let entries = parsed(json!({"type":"assistant","message":{"content":[{
            "type":"tool_use","id":"sample","name":name,"input":input
        }]}}));
        assert!(entries[0].get("question").is_none());
    }
}

#[allow(dead_code)]
#[path = "support/seed_messages.rs"]
mod seed_messages;
mod support;
use conductor_remote::reads::Reads;
use support::TestDb;

fn pending_db(frame: Value) -> (TestDb, Reads) {
    let test = TestDb::new();
    let conn = test.conn();
    seed_messages::seed(&conn);
    conn.execute(
        "UPDATE sessions SET status='working' WHERE id=?",
        [seed_messages::CHAT],
    )
    .unwrap();
    conn.execute("INSERT INTO session_messages(id,session_id,content,turn_id,sent_at,role) VALUES ('question-row',?1,?2,'sample-turn','now','assistant')", rusqlite::params![seed_messages::CHAT,frame.to_string()]).unwrap();
    let reads = Reads::new(test.db(), test.root());
    (test, reads)
}

fn codex() -> Value {
    json!({"type":"assistant","codex_async_questions":{"id":"sample-call","questions":[{"question":"Choose colour","options":["Red","Blue"]}]},"message":{"content":[]}})
}

#[test]
fn pending_snapshot_survives_cursor_and_resolves_after_native_codex_answer() {
    let (test, reads) = pending_db(codex());
    assert_eq!(
        serde_json::to_value(reads.get_messages(seed_messages::CHAT, i64::MAX).unwrap()).unwrap()
            ["pendingQuestion"]["id"],
        "sample-call"
    );
    test.conn().execute("INSERT INTO session_messages(id,session_id,content,turn_id,sent_at) VALUES ('answer',?,'Choose colour\nBlue','sample-turn','now')", [seed_messages::CHAT]).unwrap();
    assert!(
        serde_json::to_value(reads.get_messages(seed_messages::CHAT, 0).unwrap())
            .unwrap()
            .get("pendingQuestion")
            .is_none()
    );
}

#[test]
fn queued_followup_does_not_answer_codex_question_but_turn_end_does() {
    let (test, reads) = pending_db(codex());
    let conn = test.conn();
    conn.execute("INSERT INTO session_messages(id,session_id,content,turn_id,queue_order) VALUES ('followup',?,'Please continue','sample-turn',1)", [seed_messages::CHAT]).unwrap();
    assert!(
        serde_json::to_value(reads.get_messages(seed_messages::CHAT, 0).unwrap())
            .unwrap()
            .get("pendingQuestion")
            .is_some()
    );
    conn.execute("INSERT INTO session_messages(id,session_id,content,turn_id) VALUES ('end',?,'{\"type\":\"result\"}','sample-turn')", [seed_messages::CHAT]).unwrap();
    assert!(
        serde_json::to_value(reads.get_messages(seed_messages::CHAT, 0).unwrap())
            .unwrap()
            .get("pendingQuestion")
            .is_none()
    );
}

#[test]
fn claude_matches_tool_result_id_even_when_result_has_no_text() {
    let frame = json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"sample-call","name":"mcp__conductor__AskUserQuestion","input":{"questions":[{"question":"Choose colour","options":[{"label":"Blue","description":"Cool"}]}]}}]}});
    let (test, reads) = pending_db(frame);
    assert!(
        serde_json::to_value(reads.get_messages(seed_messages::CHAT, 0).unwrap())
            .unwrap()
            .get("pendingQuestion")
            .is_some()
    );
    let result = json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"sample-call","content":""}]}});
    test.conn().execute("INSERT INTO session_messages(id,session_id,content,turn_id) VALUES ('answer',?1,?2,'sample-turn')",rusqlite::params![seed_messages::CHAT,result.to_string()]).unwrap();
    assert!(
        serde_json::to_value(reads.get_messages(seed_messages::CHAT, 0).unwrap())
            .unwrap()
            .get("pendingQuestion")
            .is_none()
    );
}

use conductor_remote::transcript::questions::{
    Question, QuestionAnswer, QuestionOption, QuestionProvider, QuestionRequest,
};
use conductor_remote::ui::{
    actions::Driver,
    driver::{Target, UiDriver},
    fake::{conductor_app, main_pane, FakeDesktop, FakeNode, WindowSpec},
};
use std::{cell::Cell, rc::Rc};

fn request(multi: bool) -> QuestionRequest {
    QuestionRequest {
        id: "sample-call".into(),
        provider: QuestionProvider::Claude,
        questions: vec![Question {
            header: Some("Colour".into()),
            question: "Choose colour".into(),
            multi_select: multi,
            options: vec![
                QuestionOption {
                    label: "Red".into(),
                    description: "Warm".into(),
                },
                QuestionOption {
                    label: "Blue".into(),
                    description: "Cool".into(),
                },
            ],
        }],
    }
}

fn native(
    multi: bool,
) -> (
    Driver<FakeDesktop>,
    Target,
    FakeNode,
    Rc<Cell<usize>>,
    Vec<FakeNode>,
) {
    let app = conductor_app(&WindowSpec {
        repo: "sample".into(),
        branch: "sample-branch".into(),
        sidebar: vec![],
        chats: vec!["Sample".into()],
        selected: 1,
        composer_value: Some("Unrelated draft".into()),
    });
    let pane = main_pane(&app);
    let form = FakeNode::new("AXGroup")
        .with_child(FakeNode::new("AXStaticText").with_value("Choose colour"))
        .with_child(FakeNode::new("AXButton").with_label("Cancel agent"))
        .with_child(
            FakeNode::new("AXTextArea")
                .with_label("Other response")
                .with_value(""),
        );
    let options: Vec<_> = ["1 Red Warm", "2 Blue Cool"]
        .iter()
        .map(|label| {
            let node = FakeNode::new(if multi { "AXCheckBox" } else { "AXRadioButton" })
                .with_label(label)
                .with_flag(false);
            node.on_press(|n| n.set_flag(!n.flag_value().unwrap_or(false)));
            form.add_child(node.clone());
            node
        })
        .collect();
    let submitted = Rc::new(Cell::new(0));
    let submit = FakeNode::new("AXButton");
    let count = submitted.clone();
    let owner = pane.clone();
    let f = form.clone();
    submit.on_press(move |_| {
        count.set(count.get() + 1);
        owner.remove_child(&f);
    });
    form.add_child(submit);
    pane.add_child(form.clone());
    (
        Driver::new(FakeDesktop::new(app)),
        Target {
            workspace_id: "sample-ws".into(),
            session_id: Some("sample-chat".into()),
            repo: Some("sample".into()),
            branch: "sample-branch".into(),
            workspace_name: None,
            tab: None,
        },
        form,
        submitted,
        options,
    )
}

#[test]
fn question_answer_selects_native_option_and_submits_exactly_once() {
    let (mut driver, target, _form, count, options) = native(false);
    driver
        .answer_questions(
            &target,
            &request(false),
            &[QuestionAnswer {
                selected: vec![1],
                other: None,
            }],
            &|| true,
        )
        .unwrap();
    assert_eq!(count.get(), 1);
    assert_eq!(options[1].flag_value(), Some(true));
    assert!(driver
        .answer_questions(
            &target,
            &request(false),
            &[QuestionAnswer {
                selected: vec![0],
                other: None
            }],
            &|| true
        )
        .is_err());
    assert_eq!(count.get(), 1);
}

#[test]
fn stale_question_and_invalid_answer_never_press_submit() {
    let (mut driver, target, _form, count, _) = native(false);
    assert!(driver
        .answer_questions(
            &target,
            &request(false),
            &[QuestionAnswer {
                selected: vec![0],
                other: None
            }],
            &|| false
        )
        .is_err());
    assert!(driver
        .answer_questions(
            &target,
            &request(false),
            &[QuestionAnswer {
                selected: vec![9],
                other: None
            }],
            &|| true
        )
        .is_err());
    assert_eq!(count.get(), 0);
}

#[test]
fn multiple_selection_replaces_existing_checks() {
    let (mut driver, target, _form, count, options) = native(true);
    options[1].set_flag(true);
    driver
        .answer_questions(
            &target,
            &request(true),
            &[QuestionAnswer {
                selected: vec![0],
                other: None,
            }],
            &|| true,
        )
        .unwrap();
    assert_eq!(options[0].flag_value(), Some(true));
    assert_eq!(options[1].flag_value(), Some(false));
    assert_eq!(count.get(), 1);
}

#[tokio::test]
async fn concurrent_repeated_answers_submit_once_and_wrong_workspace_is_refused() {
    use conductor_remote::{
        contract::Priority,
        delivery::{
            parked::{ParkedQueue, ParkedTimings},
            questions::AnswerQuestionsRequest,
            service::{WriteTimings, Writes},
            WriteService,
        },
        state::store::Store,
        ui::{actor::UiActor, node::UiNode},
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    for uncertain in [false, true] {
        let test = TestDb::new();
        let conn = test.conn();
        conn.execute_batch("INSERT INTO repos(id,name) VALUES ('sample-repo','sample'); INSERT INTO workspaces(local_id,id,repository_id,branch,state) VALUES ('sample-ws','sample-ws','sample-repo','sample-branch','ready'); INSERT INTO sessions(id,workspace_id,title,status,is_hidden) VALUES ('sample-chat','sample-ws','Sample','needs_user_input',0);").unwrap();
        let frame = json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"sample-call","name":"mcp__conductor__AskUserQuestion","input":{"questions":[{"question":"Choose colour","options":[{"label":"Red","description":"Warm"},{"label":"Blue","description":"Cool"}]}]}}]}});
        conn.execute("INSERT INTO session_messages(id,session_id,content,turn_id,role) VALUES ('sample-question','sample-chat',?,'sample-turn','assistant')",[frame.to_string()]).unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let submitted = count.clone();
        let ui = UiActor::spawn(move || {
            let (driver, _, form, _, _) = native(false);
            let mut submit = form.children().unwrap().last().unwrap().clone();
            if uncertain {
                form.remove_child(&submit);
                submit = FakeNode::new("AXButton");
                form.add_child(submit.clone());
            }
            submit.on_press(move |_| {
                submitted.fetch_add(1, Ordering::SeqCst);
            });
            Box::new(driver)
        });
        let reads = Arc::new(Reads::new(test.db(), test.root()));
        let parked = ParkedQueue::new(
            Arc::new(Store::open_in_memory().unwrap()),
            Arc::new(|| Some(false)),
            ParkedTimings::default(),
        );
        let writes = Writes::new(
            reads,
            ui,
            Arc::new(|| true),
            WriteTimings::default(),
            parked,
        );
        let request = AnswerQuestionsRequest {
            workspace_id: "sample-ws".into(),
            request_id: "sample-call".into(),
            answers: vec![QuestionAnswer {
                selected: vec![0],
                other: None,
            }],
        };
        let wrong = AnswerQuestionsRequest {
            workspace_id: "other-ws".into(),
            ..request.clone()
        };
        assert_ne!(
            writes
                .answer_questions("sample-chat".into(), wrong.clone(), Priority::Interactive)
                .await
                .status,
            200
        );
        assert_eq!(count.load(Ordering::SeqCst), 0);
        let (a, b) = tokio::join!(
            writes.answer_questions("sample-chat".into(), request.clone(), Priority::Interactive),
            writes.answer_questions("sample-chat".into(), request.clone(), Priority::Interactive)
        );
        assert_eq!(a.status, if uncertain { 502 } else { 200 }, "{:?}", a.body);
        assert_eq!(a, b);
        assert_eq!(a.body["submitted"], true);
        assert_ne!(
            writes
                .answer_questions("sample-chat".into(), wrong, Priority::Interactive)
                .await
                .status,
            200
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn question_form_accepts_other_field_inside_accessibility_wrapper() {
    use conductor_remote::ui::node::UiNode;
    let (mut driver, target, form, count, _) = native(false);
    let area = form
        .children()
        .unwrap()
        .into_iter()
        .find(|n| n.label().as_deref() == Some("Other response"))
        .unwrap();
    let text = form
        .children()
        .unwrap()
        .into_iter()
        .find(|n| n.role().as_deref() == Some("AXStaticText"))
        .unwrap();
    form.remove_child(&text);
    form.add_child(FakeNode::new("AXGroup").with_child(FakeNode::new("AXGroup").with_child(text)));
    form.remove_child(&area);
    form.add_child(FakeNode::new("AXGroup").with_child(area));
    driver
        .answer_questions(
            &target,
            &request(false),
            &[QuestionAnswer {
                selected: vec![0],
                other: None,
            }],
            &|| true,
        )
        .unwrap();
    assert_eq!(count.get(), 1);
}

#[test]
fn replay_after_answer_does_not_resurrect_question() {
    let (test, reads) = pending_db(codex());
    let c = test.conn();
    c.execute("INSERT INTO session_messages(id,session_id,content,turn_id,sent_at) VALUES ('answer',?,'Choose colour\nBlue','sample-turn','now')",[seed_messages::CHAT]).unwrap();
    c.execute("INSERT INTO session_messages(id,session_id,content,turn_id,role) VALUES ('replay',?1,?2,'sample-turn','assistant')",rusqlite::params![seed_messages::CHAT,codex().to_string()]).unwrap();
    assert!(reads
        .pending_question(seed_messages::CHAT)
        .unwrap()
        .is_none());
}

#[test]
fn unrelated_queued_text_and_subagent_questions_do_not_hide_root_question() {
    let (test, reads) = pending_db(codex());
    let c = test.conn();
    c.execute("INSERT INTO session_messages(id,session_id,content,queue_order) VALUES ('queued',?,'Explain AskUserQuestion',1)",[seed_messages::CHAT]).unwrap();
    assert!(reads
        .pending_question(seed_messages::CHAT)
        .unwrap()
        .is_some());
    let mut frame = codex();
    frame["parent_tool_use_id"] = json!("sample-child");
    c.execute("INSERT INTO session_messages(id,session_id,content,turn_id,role) VALUES ('child',?1,?2,'sample-turn','assistant')",rusqlite::params![seed_messages::CHAT,frame.to_string()]).unwrap();
    assert!(reads
        .pending_question(seed_messages::CHAT)
        .unwrap()
        .is_some());
}

#[test]
fn quoting_a_question_in_an_ordinary_followup_is_not_a_codex_answer() {
    let (test, reads) = pending_db(codex());
    test.conn().execute("INSERT INTO session_messages(id,session_id,content,turn_id,sent_at) VALUES ('followup',?,'Please reconsider:\nChoose colour\nbefore changing the code','sample-turn','now')",[seed_messages::CHAT]).unwrap();
    assert!(reads
        .pending_question(seed_messages::CHAT)
        .unwrap()
        .is_some());
}

#[test]
fn custom_multiselect_and_multiline_answer_preserve_the_text() {
    use conductor_remote::ui::node::UiNode;
    let (mut driver, target, form, count, _) = native(true);
    let check = FakeNode::new("AXCheckBox")
        .with_label("Select other response")
        .with_flag(false);
    check.on_press(|n| n.set_flag(!n.flag_value().unwrap()));
    form.add_child(check.clone());
    let other = "Extra sample\nsecond line";
    driver
        .answer_questions(
            &target,
            &request(true),
            &[QuestionAnswer {
                selected: vec![0],
                other: Some(other.into()),
            }],
            &|| true,
        )
        .unwrap();
    assert_eq!(check.flag_value(), Some(true));
    assert_eq!(count.get(), 1);
    assert_eq!(
        form.find_label("Other response")
            .unwrap()
            .value()
            .unwrap()
            .as_deref(),
        Some(other)
    );
}

#[test]
fn a_submit_without_confirmation_is_uncertain_and_is_not_pressed_twice() {
    use conductor_remote::ui::{driver::UiError, node::UiNode};
    let (mut driver, target, form, _count, _) = native(false);
    let submit = form
        .children()
        .unwrap()
        .into_iter()
        .find(|n| n.role().as_deref() == Some("AXButton") && n.label().is_none())
        .unwrap();
    form.remove_child(&submit);
    let presses = Rc::new(Cell::new(0));
    let count = presses.clone();
    let submit = FakeNode::new("AXButton");
    submit.on_press(move |_| count.set(count.get() + 1));
    form.add_child(submit);
    assert_eq!(
        driver.answer_questions(
            &target,
            &request(false),
            &[QuestionAnswer {
                selected: vec![0],
                other: None
            }],
            &|| true
        ),
        Err(UiError::QuestionSubmissionUnknown)
    );
    assert_eq!(presses.get(), 1);
}

#[test]
fn changing_or_answering_question_during_selection_stops_before_submit() {
    let (mut driver, target, _form, count, options) = native(false);
    let active = Rc::new(Cell::new(true));
    let flag = active.clone();
    options[0].on_press(move |_| flag.set(false));
    assert!(driver
        .answer_questions(
            &target,
            &request(false),
            &[QuestionAnswer {
                selected: vec![0],
                other: None
            }],
            &|| active.get()
        )
        .is_err());
    assert_eq!(count.get(), 0);
}

#[test]
fn replay_of_an_old_question_keeps_a_newer_request_pending() {
    let (test, reads) = pending_db(codex());
    let c = test.conn();
    c.execute("INSERT INTO session_messages(id,session_id,content,turn_id,sent_at) VALUES ('answer',?,'Choose colour\nBlue','sample-turn','now')",[seed_messages::CHAT]).unwrap();
    let mut second = codex();
    second["codex_async_questions"]["id"] = json!("second-call");
    second["codex_async_questions"]["questions"][0]["question"] = json!("Choose shape");
    for (id, frame) in [("second", second), ("old-replay", codex())] {
        c.execute("INSERT INTO session_messages(id,session_id,content,turn_id,role) VALUES (?1,?2,?3,'sample-turn','assistant')",rusqlite::params![id,seed_messages::CHAT,frame.to_string()]).unwrap();
    }
    assert_eq!(
        reads
            .pending_question(seed_messages::CHAT)
            .unwrap()
            .unwrap()
            .id,
        "second-call"
    );
}

#[test]
fn accessibility_failure_after_submit_is_unconfirmed_not_success() {
    use conductor_remote::ui::{ax::AxError, desktop::Desktop, driver::UiError, node::UiNode};
    let (mut driver, target, form, count, _) = native(false);
    let pane = main_pane(&driver.desktop().application(4242));
    form.children()
        .unwrap()
        .last()
        .unwrap()
        .on_press(move |_| pane.fail_children(AxError::CannotComplete));
    assert_eq!(
        driver.answer_questions(
            &target,
            &request(false),
            &[QuestionAnswer {
                selected: vec![0],
                other: None
            }],
            &|| true
        ),
        Err(UiError::QuestionSubmissionUnknown)
    );
    assert_eq!(count.get(), 1);
}

#[test]
fn an_unreadable_textarea_label_after_submit_is_unconfirmed() {
    use conductor_remote::ui::{desktop::Desktop, driver::UiError, node::UiNode};
    let (mut driver, target, form, count, _) = native(false);
    let pane = main_pane(&driver.desktop().application(4242));
    let area = form.find_label("Other response").unwrap();
    let retained_form = form.clone();
    form.children().unwrap().last().unwrap().on_press(move |_| {
        area.set_label("");
        pane.add_child(retained_form.clone());
    });
    assert_eq!(
        driver.answer_questions(
            &target,
            &request(false),
            &[QuestionAnswer {
                selected: vec![0],
                other: None
            }],
            &|| true
        ),
        Err(UiError::QuestionSubmissionUnknown)
    );
    assert_eq!(count.get(), 1);
}

#[test]
fn a_user_json_prompt_cannot_impersonate_a_native_question() {
    let (test, reads) = pending_db(codex());
    let mut fake = codex();
    fake["codex_async_questions"]["id"] = json!("user-json");
    test.conn().execute("INSERT INTO session_messages(id,session_id,role,content,turn_id,sent_at) VALUES ('user-json',?1,'user',?2,'sample-turn','now')",rusqlite::params![seed_messages::CHAT,fake.to_string()]).unwrap();
    assert_eq!(
        reads
            .pending_question(seed_messages::CHAT)
            .unwrap()
            .unwrap()
            .id,
        "sample-call"
    );
}
