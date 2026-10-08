//! The native UI layer, without driving any app: the snapshot over a fake tree, the pure
//! tables, and the probes that read only this process's own state.

use conductor_remote::ui::ax::{is_trusted, AxError};
use conductor_remote::ui::identity::bundle_identifier;
use conductor_remote::ui::keys::{flags, key_code, Key, Modifiers};
use conductor_remote::ui::screen::session_state;
use conductor_remote::ui::snapshot::{
    snapshot, NodeFields, NodeSnapshot, SnapshotLimits, SnapshotSource,
};
use serde_json::json;

/// An in-memory tree node.
#[derive(Clone, Default)]
struct Fake {
    fields: NodeFields,
    children: Vec<Fake>,
}

impl SnapshotSource for Fake {
    fn read(&self) -> NodeFields {
        self.fields.clone()
    }

    fn children(&self) -> Vec<Fake> {
        self.children.clone()
    }
}

fn node(role: &str, children: Vec<Fake>) -> Fake {
    Fake {
        fields: NodeFields {
            role: Some(role.to_owned()),
            ..NodeFields::default()
        },
        children,
    }
}

fn with_value(role: &str, value: &str) -> Fake {
    Fake {
        fields: NodeFields {
            role: Some(role.to_owned()),
            value: Some(value.to_owned()),
            ..NodeFields::default()
        },
        children: Vec::new(),
    }
}

fn limits(max_depth: usize, max_nodes: usize, value_chars: Option<usize>) -> SnapshotLimits {
    SnapshotLimits {
        max_depth,
        max_nodes,
        value_chars,
    }
}

fn roles(snap: &NodeSnapshot) -> Vec<String> {
    let mut out = vec![snap.role.clone().unwrap_or_default()];
    for child in &snap.children {
        out.extend(roles(child));
    }
    out
}

/// window ─┬─ group ─┬─ a
///         │         └─ b
///         └─ list ──┬─ c
///                   └─ d
fn tree() -> Fake {
    node(
        "window",
        vec![
            node("group", vec![node("a", vec![]), node("b", vec![])]),
            node("list", vec![node("c", vec![]), node("d", vec![])]),
        ],
    )
}

#[test]
fn snapshot_walks_the_whole_tree_depth_first_within_generous_limits() {
    let snap = snapshot(&tree(), limits(10, 100, Some(100)));
    assert_eq!(
        roles(&snap),
        ["window", "group", "a", "b", "list", "c", "d"]
    );
    assert_eq!(snap.omitted_children, 0);
    assert!(!snap.depth_limited);
    assert!(!snap.children[0].children[0].depth_limited);
}

#[test]
fn snapshot_stops_at_the_depth_limit_and_marks_the_nodes_there() {
    let snap = snapshot(&tree(), limits(1, 100, Some(100)));
    assert_eq!(roles(&snap), ["window", "group", "list"]);
    assert!(!snap.depth_limited);
    for child in &snap.children {
        assert!(child.depth_limited);
        assert!(child.children.is_empty());
        assert_eq!(child.omitted_children, 0);
    }

    let root_only = snapshot(&tree(), limits(0, 100, Some(100)));
    assert_eq!(roles(&root_only), ["window"]);
    assert!(root_only.depth_limited);
}

#[test]
fn snapshot_counts_every_included_node_against_the_node_limit() {
    // window, group, a, b: the budget is spent inside group, so list is left out of window.
    let snap = snapshot(&tree(), limits(10, 4, Some(100)));
    assert_eq!(roles(&snap), ["window", "group", "a", "b"]);
    assert_eq!(snap.omitted_children, 1);
    assert_eq!(snap.children[0].omitted_children, 0);

    // window, group, a: b is left out of group, list out of window.
    let snap = snapshot(&tree(), limits(10, 3, Some(100)));
    assert_eq!(roles(&snap), ["window", "group", "a"]);
    assert_eq!(snap.children[0].omitted_children, 1);
    assert_eq!(snap.omitted_children, 1);

    // The root alone fills a budget of one; its two children are omitted.
    let snap = snapshot(&tree(), limits(10, 1, Some(100)));
    assert_eq!(roles(&snap), ["window"]);
    assert_eq!(snap.omitted_children, 2);

    // Even a budget of zero includes the root.
    let snap = snapshot(&tree(), limits(10, 0, Some(100)));
    assert_eq!(roles(&snap), ["window"]);
    assert_eq!(snap.omitted_children, 2);
}

#[test]
fn snapshot_hides_every_value_when_values_are_off() {
    let root = node(
        "window",
        vec![with_value("field", "secret"), node("button", vec![])],
    );
    let snap = snapshot(&root, limits(5, 50, None));
    let field = &snap.children[0];
    assert_eq!(field.value, None);
    assert!(field.value_hidden);
    // A node without a value has nothing to hide.
    assert!(!snap.children[1].value_hidden);
    assert!(!snap.value_hidden);
}

#[test]
fn snapshot_cuts_long_values_by_chars() {
    let root = node(
        "window",
        vec![
            with_value("long", "héllo wörld"),
            with_value("exact", "héllo"),
            with_value("short", "hé"),
            with_value("empty", ""),
        ],
    );
    let snap = snapshot(&root, limits(5, 50, Some(5)));
    let values: Vec<_> = snap.children.iter().map(|c| c.value.clone()).collect();
    assert_eq!(
        values,
        [
            Some("héllo…".to_owned()),
            Some("héllo".to_owned()),
            Some("hé".to_owned()),
            Some(String::new()),
        ]
    );
    assert!(snap.children.iter().all(|c| !c.value_hidden));

    let zero = snapshot(&root, limits(5, 50, Some(0)));
    assert_eq!(zero.children[0].value.as_deref(), Some("…"));
    assert_eq!(zero.children[3].value.as_deref(), Some(""));
}

#[test]
fn snapshot_copies_every_field() {
    let fields = NodeFields {
        role: Some("AXButton".into()),
        subrole: Some("AXCloseButton".into()),
        title: Some("Close".into()),
        description: Some("close chat".into()),
        identifier: Some("close".into()),
        value: Some("v".into()),
        help: Some("Closes".into()),
        placeholder: Some("Type".into()),
        enabled: Some(true),
        focused: Some(false),
        selected: Some(true),
        actions: vec!["AXPress".into()],
    };
    let root = Fake {
        fields: fields.clone(),
        children: Vec::new(),
    };
    let snap = snapshot(&root, limits(3, 10, Some(10)));
    assert_eq!(
        serde_json::to_value(&snap).unwrap(),
        json!({
            "role": "AXButton",
            "subrole": "AXCloseButton",
            "title": "Close",
            "description": "close chat",
            "identifier": "close",
            "value": "v",
            "help": "Closes",
            "placeholder": "Type",
            "enabled": true,
            "focused": false,
            "selected": true,
            "actions": ["AXPress"],
        })
    );
}

#[test]
fn snapshot_json_leaves_out_absent_fields_false_flags_and_zero_counts() {
    let root = node(
        "window",
        vec![with_value("field", "x"), node("leaf", vec![])],
    );

    // Everything empty or default is left out.
    let bare = snapshot(&node("window", vec![]), limits(5, 50, Some(10)));
    assert_eq!(
        serde_json::to_value(&bare).unwrap(),
        json!({ "role": "window" })
    );

    let hidden = snapshot(&root, limits(5, 2, None));
    assert_eq!(
        serde_json::to_value(&hidden).unwrap(),
        json!({
            "role": "window",
            "children": [{ "role": "field", "valueHidden": true }],
            "omittedChildren": 1,
        })
    );

    let limited = snapshot(&root, limits(0, 50, Some(10)));
    assert_eq!(
        serde_json::to_value(&limited).unwrap(),
        json!({ "role": "window", "depthLimited": true })
    );
}

#[test]
fn snapshot_of_a_node_whose_reads_fail_has_empty_fields() {
    // A fake whose reads all failed reports default fields and no children.
    let snap = snapshot(&Fake::default(), limits(5, 50, Some(10)));
    assert_eq!(serde_json::to_value(&snap).unwrap(), json!({}));
}

#[test]
fn ax_error_from_code_covers_every_code_of_the_header() {
    let table = [
        (-25200, AxError::Failure),
        (-25201, AxError::IllegalArgument),
        (-25202, AxError::InvalidUiElement),
        (-25203, AxError::InvalidUiElementObserver),
        (-25204, AxError::CannotComplete),
        (-25205, AxError::AttributeUnsupported),
        (-25206, AxError::ActionUnsupported),
        (-25207, AxError::NotificationUnsupported),
        (-25208, AxError::NotImplemented),
        (-25209, AxError::NotificationAlreadyRegistered),
        (-25210, AxError::NotificationNotRegistered),
        (-25211, AxError::ApiDisabled),
        (-25212, AxError::NoValue),
        (-25213, AxError::ParameterizedAttributeUnsupported),
        (-25214, AxError::NotEnoughPrecision),
    ];
    for (code, error) in table {
        assert_eq!(AxError::from_code(code), Some(error), "code {code}");
    }
    assert_eq!(AxError::from_code(0), None);
    assert_eq!(AxError::from_code(-25215), Some(AxError::Other(-25215)));
    assert_eq!(AxError::from_code(-25199), Some(AxError::Other(-25199)));
    assert_eq!(AxError::from_code(7), Some(AxError::Other(7)));
}

#[test]
fn key_codes_match_hitoolbox() {
    let table = [
        (Key::Return, 0x24),
        (Key::Escape, 0x35),
        (Key::Space, 0x31),
        (Key::Delete, 0x33),
        (Key::L, 0x25),
        (Key::T, 0x11),
        (Key::V, 0x09),
        (Key::K, 0x28),
    ];
    for (key, code) in table {
        assert_eq!(key_code(key), code, "{key:?}");
    }
}

#[test]
fn modifier_flags_match_iollevent() {
    let none = Modifiers::default();
    assert_eq!(flags(none), 0);
    assert_eq!(
        flags(Modifiers {
            shift: true,
            ..none
        }),
        0x20000
    );
    assert_eq!(
        flags(Modifiers {
            control: true,
            ..none
        }),
        0x40000
    );
    assert_eq!(
        flags(Modifiers {
            option: true,
            ..none
        }),
        0x80000
    );
    assert_eq!(
        flags(Modifiers {
            command: true,
            ..none
        }),
        0x100000
    );
    assert_eq!(
        flags(Modifiers {
            command: true,
            shift: true,
            option: true,
            control: true,
        }),
        0x1E0000
    );
}

#[test]
fn is_trusted_without_prompt_returns_a_bool() {
    // NULL options: reads this process's own grant and never shows the dialog.
    let trusted: bool = is_trusted(false);
    assert_eq!(trusted, is_trusted(false));
}

#[test]
fn session_state_returns_without_panicking() {
    // `None` outside a GUI session (CI, ssh); otherwise the current session's state.
    let _ = session_state();
}

#[test]
fn bundle_identifier_returns_without_panicking() {
    // A test binary does not run from a bundle, but whatever it reports must not panic.
    let _ = bundle_identifier();
}
