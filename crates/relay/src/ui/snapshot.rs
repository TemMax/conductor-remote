//! A read-only snapshot of an element tree, free of any system binding.

use serde::Serialize;

/// What a snapshot reads of a node. Implemented for [`crate::ui::ax::Element`] (attributes
/// AXRole, AXSubrole, AXTitle, AXDescription, AXIdentifier, AXValue (strings only), AXHelp,
/// AXPlaceholderValue, AXEnabled, AXFocused, AXSelected, actions, AXChildren) and by the tests'
/// fake tree.
pub trait SnapshotSource: Sized {
    /// The node's own fields. A field that cannot be read is left empty.
    fn read(&self) -> NodeFields;
    /// The node's children, in order. Empty when they cannot be read.
    fn children(&self) -> Vec<Self>;
}

/// The fields of one node, as read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeFields {
    pub role: Option<String>,
    pub subrole: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub identifier: Option<String>,
    pub value: Option<String>,
    pub help: Option<String>,
    pub placeholder: Option<String>,
    pub enabled: Option<bool>,
    pub focused: Option<bool>,
    pub selected: Option<bool>,
    pub actions: Vec<String>,
}

/// One node of a snapshot with the nodes below it. In JSON, `None` fields, `false` flags, `0`
/// counts and empty lists are left out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subrole: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focused: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<bool>,
    /// The node had a value, and the limits hid it.
    #[serde(skip_serializing_if = "is_false")]
    pub value_hidden: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<NodeSnapshot>,
    /// How many of this node's children were left out because the node limit was reached.
    #[serde(skip_serializing_if = "is_zero")]
    pub omitted_children: usize,
    /// The node sits at the depth limit: its children were not read.
    #[serde(skip_serializing_if = "is_false")]
    pub depth_limited: bool,
}

/// How much of a tree a snapshot reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotLimits {
    /// The deepest level included; the root is level 0. Nodes at this level get no children.
    pub max_depth: usize,
    /// The most nodes included, the root too.
    pub max_nodes: usize,
    /// `None` hides every value; `Some(n)` keeps the first n chars of each.
    pub value_chars: Option<usize>,
}

/// Walks the tree under `root` depth first, within `limits`. It never fails: a node whose read
/// fails contributes empty fields.
pub fn snapshot<S: SnapshotSource>(root: &S, limits: SnapshotLimits) -> NodeSnapshot {
    let mut included = 0;
    walk(root, 0, limits, &mut included)
}

fn walk<S: SnapshotSource>(
    node: &S,
    depth: usize,
    limits: SnapshotLimits,
    included: &mut usize,
) -> NodeSnapshot {
    *included += 1;
    let mut snap = from_fields(node.read(), limits.value_chars);
    if depth >= limits.max_depth {
        snap.depth_limited = true;
        return snap;
    }
    let children = node.children();
    for (index, child) in children.iter().enumerate() {
        if *included >= limits.max_nodes {
            snap.omitted_children = children.len() - index;
            break;
        }
        snap.children.push(walk(child, depth + 1, limits, included));
    }
    snap
}

fn from_fields(fields: NodeFields, value_chars: Option<usize>) -> NodeSnapshot {
    let (value, value_hidden) = match (fields.value, value_chars) {
        (None, _) => (None, false),
        (Some(_), None) => (None, true),
        (Some(value), Some(max)) => (Some(cut(&value, max)), false),
    };
    NodeSnapshot {
        role: fields.role,
        subrole: fields.subrole,
        title: fields.title,
        description: fields.description,
        identifier: fields.identifier,
        value,
        help: fields.help,
        placeholder: fields.placeholder,
        enabled: fields.enabled,
        focused: fields.focused,
        selected: fields.selected,
        value_hidden,
        actions: fields.actions,
        children: Vec::new(),
        omitted_children: 0,
        depth_limited: false,
    }
}

/// The first `max` chars of `value`, with `…` appended when anything was cut.
fn cut(value: &str, max: usize) -> String {
    match value.char_indices().nth(max) {
        None => value.to_owned(),
        Some((end, _)) => format!("{}…", &value[..end]),
    }
}

fn is_false(flag: &bool) -> bool {
    !*flag
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}
