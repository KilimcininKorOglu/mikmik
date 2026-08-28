//! Reading and writing the accessibility tree.
//!
//! `enigo` and `xcap` move the pointer and capture pixels. Neither can say what
//! is under the pointer, so a script that wants to press a named button has to
//! find it by looking at an image. The accessibility tree already names every
//! control the platform draws, and every desktop platform exposes it: macOS
//! through `AXUIElement`, Windows through `IUIAutomation`, Linux through AT-SPI2
//! over D-Bus.
//!
//! One trait, three implementations, and nothing platform-shaped above this
//! module. A platform reaches its tree through raw pointers or COM interfaces
//! that are neither `Send` nor safe to hand to a script, so the handles stay
//! here and the script sees an opaque id.

mod handles;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

use serde_json::{json, Value};

pub use handles::HandleStore;

/// The ops this module answers, in the order the runner names them.
pub const OPS: &[&str] = &[
    "ax_focused",
    "ax_tree",
    "ax_find",
    "ax_get",
    "ax_set",
    "ax_press",
];

/// The ops that change what is on screen rather than reading it.
pub const WRITING_OPS: &[&str] = &["ax_set", "ax_press"];

/// One node of the tree, as the script reads it.
///
/// The handle is opaque: it names an element the host holds, and there is no
/// way to turn it back into a pointer from the script's side.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub handle: String,
    pub role: String,
    pub title: String,
    pub value: String,
    /// Absolute screen rectangle, when the platform reports one.
    pub bounds: Option<(f64, f64, f64, f64)>,
    pub children: Vec<Node>,
}

impl Node {
    pub fn to_json(&self) -> Value {
        let mut object = json!({
            "handle": self.handle,
            "role": self.role,
            "title": self.title,
            "value": self.value,
        });
        if let Some((x, y, width, height)) = self.bounds {
            object["bounds"] = json!({ "x": x, "y": y, "width": width, "height": height });
        }
        if !self.children.is_empty() {
            let children: Vec<Value> = self.children.iter().map(Node::to_json).collect();
            object["children"] = Value::Array(children);
        }
        object
    }

    /// Every node in this subtree, parents before children.
    pub fn flattened(&self) -> Vec<&Node> {
        let mut found = vec![self];
        for child in &self.children {
            found.extend(child.flattened());
        }
        found
    }
}

/// What `ax.find` matches on. An absent field matches everything.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Query {
    pub role: Option<String>,
    pub title: Option<String>,
    pub value: Option<String>,
    /// Look only inside this application. Absent means the focused one.
    pub pid: Option<i32>,
    pub limit: usize,
}

impl Query {
    /// Whether `node` answers every field this query names.
    ///
    /// Matched on a case-insensitive substring rather than an exact string: a
    /// platform reports "OK" where another reports "OK Button", and a script
    /// that had to spell the whole title would be writing a platform's own
    /// wording into itself.
    pub fn matches(&self, node: &Node) -> bool {
        let holds = |field: &Option<String>, against: &str| match field {
            None => true,
            Some(wanted) => against.to_lowercase().contains(&wanted.to_lowercase()),
        };
        holds(&self.role, &node.role)
            && holds(&self.title, &node.title)
            && holds(&self.value, &node.value)
    }
}

/// Why a platform could not answer.
///
/// Each platform constructs a different subset: macOS raises `PermissionDenied`
/// and never `NotSupported`, while the Linux and Windows stubs raise only
/// `NotSupported`. A single-platform build therefore leaves one or two variants
/// unconstructed, so the enum carries `allow(dead_code)` rather than each build
/// warning on a variant another platform needs.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub enum AxError {
    /// The user has not granted the permission the platform requires.
    ///
    /// Carried as its own case rather than folded into `Failed`, because the
    /// platform answers a refused request with an empty tree and reporting that
    /// as "nothing found" sends the script looking for a control that is there.
    PermissionDenied(String),
    /// The build has no backend for this platform, or the backend has no path
    /// for this call.
    NotSupported(String),
    /// The handle names nothing this session holds.
    UnknownHandle(String),
    Failed(String),
}

impl std::fmt::Display for AxError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PermissionDenied(detail) => {
                write!(formatter, "accessibility is not permitted: {detail}")
            }
            Self::NotSupported(detail) => write!(formatter, "no accessibility backend: {detail}"),
            Self::UnknownHandle(handle) => {
                write!(formatter, "no element is held under the handle {handle}")
            }
            Self::Failed(detail) => write!(formatter, "{detail}"),
        }
    }
}

pub type AxResult<T> = Result<T, AxError>;

/// What every platform has to answer.
///
/// Blocking on purpose: each implementation is a synchronous platform call, and
/// the bridge already runs a host op on a blocking thread.
pub trait AxBackend: Send + Sync {
    /// The element with keyboard focus, and nothing under it.
    fn focused(&self, handles: &HandleStore) -> AxResult<Node>;

    /// The tree under an application, to `depth` levels.
    ///
    /// `pid` absent means the application that holds focus.
    fn tree(&self, handles: &HandleStore, pid: Option<i32>, depth: usize) -> AxResult<Node>;

    /// Read one attribute of a held element.
    fn get(&self, handles: &HandleStore, handle: &str, attribute: &str) -> AxResult<Value>;

    /// Write one attribute of a held element.
    fn set(
        &self,
        handles: &HandleStore,
        handle: &str,
        attribute: &str,
        value: &Value,
    ) -> AxResult<()>;

    /// Trigger a held element's default action.
    fn press(&self, handles: &HandleStore, handle: &str) -> AxResult<()>;
}

/// The backend this build carries.
///
/// A platform with no implementation answers `NotSupported` rather than
/// returning nothing, so a script never reads an unsupported platform as an
/// empty desktop.
pub fn backend() -> Box<dyn AxBackend> {
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacBackend)
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::WindowsBackend)
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::LinuxBackend)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        Box::new(Unsupported)
    }
}

/// The backend for a platform none of the three modules cover.
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
struct Unsupported;

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
impl AxBackend for Unsupported {
    fn focused(&self, _handles: &HandleStore) -> AxResult<Node> {
        Err(AxError::NotSupported(std::env::consts::OS.to_string()))
    }
    fn tree(&self, _handles: &HandleStore, _pid: Option<i32>, _depth: usize) -> AxResult<Node> {
        Err(AxError::NotSupported(std::env::consts::OS.to_string()))
    }
    fn get(&self, _handles: &HandleStore, _handle: &str, _attribute: &str) -> AxResult<Value> {
        Err(AxError::NotSupported(std::env::consts::OS.to_string()))
    }
    fn set(
        &self,
        _handles: &HandleStore,
        _handle: &str,
        _attribute: &str,
        _value: &Value,
    ) -> AxResult<()> {
        Err(AxError::NotSupported(std::env::consts::OS.to_string()))
    }
    fn press(&self, _handles: &HandleStore, _handle: &str) -> AxResult<()> {
        Err(AxError::NotSupported(std::env::consts::OS.to_string()))
    }
}

/// Answer one `ax_*` host op against the session's held elements.
///
/// Blocking: every backend call is a synchronous platform call, so the bridge
/// runs this on a blocking thread. The store is the session's, so a handle one
/// call held is still valid in the next.
pub fn run_blocking(op: &str, args: &Value, handles: &HandleStore) -> AxResult<Value> {
    let backend = backend();
    match op {
        "ax_focused" => backend.focused(handles).map(|node| node.to_json()),
        "ax_tree" => {
            let pid = args.get("pid").and_then(Value::as_i64).map(|n| n as i32);
            let depth = args.get("depth").and_then(Value::as_u64).unwrap_or(1) as usize;
            backend.tree(handles, pid, depth).map(|node| node.to_json())
        }
        "ax_find" => {
            let pid = args.get("pid").and_then(Value::as_i64).map(|n| n as i32);
            let depth = args.get("depth").and_then(Value::as_u64).unwrap_or(8) as usize;
            let query = query_from(args);
            let root = backend.tree(handles, pid, depth)?;
            let found: Vec<Value> = find_in(&root, &query).iter().map(Node::to_json).collect();
            Ok(Value::Array(found))
        }
        "ax_get" => {
            let handle = string_arg(args, "handle")?;
            let attribute = string_arg(args, "attribute")?;
            backend.get(handles, &handle, &attribute)
        }
        "ax_set" => {
            let handle = string_arg(args, "handle")?;
            let attribute = string_arg(args, "attribute")?;
            let value = args.get("value").cloned().unwrap_or(Value::Null);
            backend
                .set(handles, &handle, &attribute, &value)
                .map(|()| json!({ "set": attribute }))
        }
        "ax_press" => {
            let handle = string_arg(args, "handle")?;
            backend
                .press(handles, &handle)
                .map(|()| json!({ "pressed": handle }))
        }
        other => Err(AxError::Failed(format!(
            "unknown accessibility op: {other}"
        ))),
    }
}

/// Whether `op` is one this module answers.
///
/// Matched against `OPS` rather than an `ax_` prefix, so an `ax_` op this
/// module does not define falls through to `host_ops`, which names it as
/// unknown rather than this module swallowing it into a generic failure.
pub fn owns(op: &str) -> bool {
    OPS.contains(&op)
}

/// Whether `op` changes what is on screen.
pub fn writes(op: &str) -> bool {
    WRITING_OPS.contains(&op)
}

fn query_from(args: &Value) -> Query {
    Query {
        role: string_field(args, "role"),
        title: string_field(args, "title"),
        value: string_field(args, "value"),
        pid: args.get("pid").and_then(Value::as_i64).map(|n| n as i32),
        limit: args.get("limit").and_then(Value::as_u64).unwrap_or(0) as usize,
    }
}

fn string_field(args: &Value, name: &str) -> Option<String> {
    args.get(name)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn string_arg(args: &Value, name: &str) -> AxResult<String> {
    string_field(args, name)
        .ok_or_else(|| AxError::Failed(format!("`{name}` is required and must be a string")))
}

/// Search a tree for the nodes a query names.
///
/// Shared rather than written three times: every backend can build a tree, and
/// the matching rule has to read the same on all of them.
pub fn find_in(root: &Node, query: &Query) -> Vec<Node> {
    let limit = if query.limit == 0 { 20 } else { query.limit };
    root.flattened()
        .into_iter()
        .filter(|node| query.matches(node))
        .take(limit)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(role: &str, title: &str) -> Node {
        Node {
            handle: format!("h-{title}"),
            role: role.to_string(),
            title: title.to_string(),
            value: String::new(),
            bounds: None,
            children: Vec::new(),
        }
    }

    fn tree() -> Node {
        Node {
            children: vec![node("AXButton", "Save"), node("AXTextField", "Name")],
            ..node("AXWindow", "Editor")
        }
    }

    #[test]
    fn a_query_with_no_field_matches_every_node() {
        let found = find_in(&tree(), &Query::default());

        assert_eq!(found.len(), 3);
    }

    #[test]
    fn a_role_narrows_the_search_to_that_role() {
        let query = Query {
            role: Some("AXButton".to_string()),
            ..Default::default()
        };

        let found = find_in(&tree(), &query);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "Save");
    }

    #[test]
    fn a_title_matches_without_its_exact_case_or_length() {
        // One platform reports "Save", another "Save Button". A script that had
        // to spell the whole title would carry a platform's wording.
        let query = Query {
            title: Some("sav".to_string()),
            ..Default::default()
        };

        let found = find_in(&tree(), &query);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "Save");
    }

    #[test]
    fn the_limit_caps_what_comes_back() {
        let query = Query {
            limit: 2,
            ..Default::default()
        };

        assert_eq!(find_in(&tree(), &query).len(), 2);
    }

    #[test]
    fn a_refused_permission_does_not_read_as_an_empty_desktop() {
        // The platform answers a refused request with nothing. Reporting that
        // as "no elements" sends the script looking for a control that is
        // there, so the refusal carries its own case.
        let denied = AxError::PermissionDenied("Accessibility".to_string());

        assert!(denied.to_string().contains("not permitted"), "{denied}");
        assert_ne!(denied, AxError::Failed("Accessibility".to_string()));
    }

    #[test]
    fn a_node_reports_its_bounds_only_when_the_platform_gave_some() {
        let without = node("AXButton", "Save").to_json();
        let with = Node {
            bounds: Some((1.0, 2.0, 3.0, 4.0)),
            ..node("AXButton", "Save")
        }
        .to_json();

        assert!(without.get("bounds").is_none());
        assert_eq!(with["bounds"]["width"], 3.0);
    }

    #[test]
    fn every_writing_op_is_one_of_the_ops() {
        for op in WRITING_OPS {
            assert!(OPS.contains(op), "{op}");
        }
    }
}
