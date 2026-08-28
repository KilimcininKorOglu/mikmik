//! The Linux accessibility backend, over AT-SPI2.
//!
//! AT-SPI2 exposes the tree on a private D-Bus, the accessibility bus, whose
//! address the session bus hands out. Every accessible is a `(bus_name,
//! object_path)` pair and every read is a D-Bus call, so a held element is
//! plain owned data and needs no `unsafe` and no `Send` mark.
//!
//! `zbus`'s blocking API is used directly rather than the async `atspi` crate,
//! because the backend trait is synchronous and the bridge already runs it on a
//! blocking thread.
//!
//! Compile-verified on Linux in CI; this repository's development host has no
//! cross toolchain for the target. AT-SPI2 also runs only where a session's
//! accessibility bus is up, so the calls are exercised on a real desktop.

use serde_json::{json, Value};
use zbus::blocking::{connection, Connection, Proxy};
use zvariant::OwnedObjectPath;

use super::{AxBackend, AxError, AxResult, HandleStore, Node};

/// A held accessible: the bus name that serves it and its object path.
///
/// The path is kept as a `String` and reparsed per proxy, because a proxy
/// borrows its path for its own lifetime and a stored owned path would tie
/// every proxy to the store's borrow.
pub type Element = (String, String);

/// The well-known names AT-SPI2 answers on.
const A11Y_BUS_DEST: &str = "org.a11y.Bus";
const A11Y_BUS_PATH: &str = "/org/a11y/bus";
const REGISTRY_DEST: &str = "org.a11y.atspi.Registry";
const ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";
const IFACE_ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const IFACE_COMPONENT: &str = "org.a11y.atspi.Component";
const IFACE_ACTION: &str = "org.a11y.atspi.Action";
const IFACE_EDITABLE: &str = "org.a11y.atspi.EditableText";

/// `ATSPI_COORD_TYPE_SCREEN`, so extents come back in screen coordinates.
const COORD_SCREEN: u32 = 0;

/// The deepest a tree walk goes, whatever a caller asks for.
const MAX_DEPTH: usize = 12;

pub struct LinuxBackend;

impl LinuxBackend {
    /// Open a fresh connection to the accessibility bus.
    ///
    /// The session bus hands out the accessibility bus address; the second
    /// connection is where every accessible lives. A fresh pair per call rather
    /// than a cached one, because a script's calls are far apart and a held
    /// connection would keep a socket open for the session's life.
    fn connect(&self) -> AxResult<Connection> {
        let session = Connection::session()
            .map_err(|error| AxError::Failed(format!("no session bus: {error}")))?;
        let bus = Proxy::new(&session, A11Y_BUS_DEST, A11Y_BUS_PATH, A11Y_BUS_DEST)
            .map_err(|error| AxError::Failed(format!("no a11y bus proxy: {error}")))?;
        let address: String = bus
            .call("GetAddress", &())
            .map_err(|error| AxError::PermissionDenied(format!("no accessibility bus: {error}")))?;
        connection::Builder::address(address.as_str())
            .and_then(connection::Builder::build)
            .map_err(|error| AxError::Failed(format!("could not reach the a11y bus: {error}")))
    }

    /// A proxy for one accessible on one interface.
    fn proxy<'a>(
        &self,
        conn: &'a Connection,
        element: &Element,
        interface: &'static str,
    ) -> AxResult<Proxy<'a>> {
        Proxy::new(conn, element.0.clone(), element.1.clone(), interface)
            .map_err(|error| AxError::Failed(format!("no proxy: {error}")))
    }

    /// One accessible reference, with its object path as a plain string.
    fn element_of(bus: String, path: OwnedObjectPath) -> Element {
        (bus, path.as_str().to_string())
    }

    /// Read one accessible without descending.
    fn read_node(&self, conn: &Connection, element: &Element, handles: &HandleStore) -> Node {
        let accessible = self.proxy(conn, element, IFACE_ACCESSIBLE).ok();
        let role = accessible
            .as_ref()
            .and_then(|proxy| proxy.call::<_, _, String>("GetRoleName", &()).ok())
            .unwrap_or_default();
        let title = accessible
            .as_ref()
            .and_then(|proxy| proxy.get_property::<String>("Name").ok())
            .unwrap_or_default();
        let bounds = self.read_extents(conn, element);
        let handle = handles.hold(element.clone());
        Node {
            handle,
            role,
            title,
            value: String::new(),
            bounds,
            children: Vec::new(),
        }
    }

    /// An accessible's screen rectangle, when it carries the component
    /// interface.
    fn read_extents(&self, conn: &Connection, element: &Element) -> Option<(f64, f64, f64, f64)> {
        let component = self.proxy(conn, element, IFACE_COMPONENT).ok()?;
        let (x, y, width, height): (i32, i32, i32, i32) =
            component.call("GetExtents", &COORD_SCREEN).ok()?;
        Some((x as f64, y as f64, width as f64, height as f64))
    }

    /// Read an accessible and its children to `depth`.
    fn read_tree(
        &self,
        conn: &Connection,
        element: &Element,
        handles: &HandleStore,
        depth: usize,
    ) -> Node {
        let mut node = self.read_node(conn, element, handles);
        if depth == 0 {
            return node;
        }
        for child in self.children(conn, element) {
            node.children
                .push(self.read_tree(conn, &child, handles, depth - 1));
        }
        node
    }

    /// The children of an accessible, each a `(bus_name, path)` pair.
    fn children(&self, conn: &Connection, element: &Element) -> Vec<Element> {
        let Ok(accessible) = self.proxy(conn, element, IFACE_ACCESSIBLE) else {
            return Vec::new();
        };
        let count: i32 = accessible.get_property("ChildCount").unwrap_or(0);
        let mut children = Vec::new();
        for index in 0..count {
            if let Ok((bus, path)) =
                accessible.call::<_, _, (String, OwnedObjectPath)>("GetChildAtIndex", &index)
            {
                children.push(Self::element_of(bus, path));
            }
        }
        children
    }
}

impl AxBackend for LinuxBackend {
    fn focused(&self, _handles: &HandleStore) -> AxResult<Node> {
        // AT-SPI2 has no call that returns the focused element: focus is an
        // event a listener receives, not a property the tree exposes. Rather
        // than guess, this says so and points the script at `tree` and `find`.
        Err(AxError::NotSupported(
            "AT-SPI2 exposes no focused-element call; walk the tree with ax.tree and ax.find"
                .to_string(),
        ))
    }

    fn tree(&self, handles: &HandleStore, _pid: Option<i32>, depth: usize) -> AxResult<Node> {
        // AT-SPI2 keys the tree on bus names, not OS pids, so `pid` is ignored
        // here and the whole desktop is returned; a caller narrows it with
        // `ax.find`. The root desktop is served by the registry.
        let conn = self.connect()?;
        let root: Element = (REGISTRY_DEST.to_string(), ROOT_PATH.to_string());
        Ok(self.read_tree(&conn, &root, handles, depth.min(MAX_DEPTH)))
    }

    fn get(&self, handles: &HandleStore, handle: &str, attribute: &str) -> AxResult<Value> {
        let element = handles.with(handle, Clone::clone)?;
        let conn = self.connect()?;
        let accessible = self.proxy(&conn, &element, IFACE_ACCESSIBLE)?;
        match attribute {
            "name" | "title" | "AXTitle" => accessible
                .get_property::<String>("Name")
                .map(|name| json!(name))
                .map_err(|error| AxError::Failed(format!("no name: {error}"))),
            "role" | "AXRole" => accessible
                .call::<_, _, String>("GetRoleName", &())
                .map(|role| json!(role))
                .map_err(|error| AxError::Failed(format!("no role: {error}"))),
            other => Err(AxError::NotSupported(format!(
                "attribute {other} is not read on Linux"
            ))),
        }
    }

    fn set(
        &self,
        handles: &HandleStore,
        handle: &str,
        _attribute: &str,
        value: &Value,
    ) -> AxResult<()> {
        let text = value
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| value.to_string());
        let element = handles.with(handle, Clone::clone)?;
        let conn = self.connect()?;
        let editable = self.proxy(&conn, &element, IFACE_EDITABLE)?;
        // `SetTextContents` replaces the whole value, which is what a script
        // that names a field and a new value expects.
        let ok: bool = editable
            .call("SetTextContents", &text)
            .map_err(|error| AxError::Failed(format!("set failed: {error}")))?;
        if ok {
            Ok(())
        } else {
            Err(AxError::Failed(
                "the element refused the new text".to_string(),
            ))
        }
    }

    fn press(&self, handles: &HandleStore, handle: &str) -> AxResult<()> {
        let element = handles.with(handle, Clone::clone)?;
        let conn = self.connect()?;
        let action = self.proxy(&conn, &element, IFACE_ACTION)?;
        // Action zero is the default action, the one a click would trigger.
        let ok: bool = action
            .call("DoAction", &0i32)
            .map_err(|error| AxError::Failed(format!("press failed: {error}")))?;
        if ok {
            Ok(())
        } else {
            Err(AxError::Failed(
                "the element has no default action".to_string(),
            ))
        }
    }
}
