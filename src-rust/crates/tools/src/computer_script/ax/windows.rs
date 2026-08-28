//! The Windows accessibility backend, over UI Automation.
//!
//! UI Automation is a COM API. Every control is an `IUIAutomationElement`,
//! reached through the process-wide `IUIAutomation` object. The `windows`
//! crate wraps each COM call in a safe method that returns a `Result`, so the
//! only `unsafe` here is COM apartment setup and the two `Send`/`Sync` marks
//! that let a held element live in the shared store.
//!
//! Compile-verified on Windows in CI; this repository's development host has no
//! cross toolchain for the target.

use serde_json::{json, Value};
use windows::core::BSTR;
use windows::Win32::Foundation::RECT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationInvokePattern,
    IUIAutomationValuePattern, TreeScope_Children, UIA_InvokePatternId, UIA_ValuePatternId,
};

use super::{AxBackend, AxError, AxResult, HandleStore, Node};

/// A held UI Automation element.
pub struct Element(IUIAutomationElement);

// SAFETY: the element is only ever read under the handle store's lock, so two
// threads never call into the same COM object at once. The automation object
// is created in a multithreaded apartment, which permits calls from any thread.
unsafe impl Send for Element {}
unsafe impl Sync for Element {}

/// The deepest a tree walk goes, whatever a caller asks for.
const MAX_DEPTH: usize = 12;

pub struct WindowsBackend;

impl WindowsBackend {
    /// The process-wide automation object.
    ///
    /// Built per call rather than cached: the cost is a COM activation, well
    /// below the tree walk that follows, and a cached object would pin an
    /// apartment for the life of the session.
    fn automation(&self) -> AxResult<IUIAutomation> {
        // SAFETY: COM allows `CoInitializeEx` once per thread; a second call on
        // an already-initialised thread returns `S_FALSE`, which `.ok()` treats
        // as success. The reserved pointer is null, as the signature allows.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED).ok();
        }
        // SAFETY: `CUIAutomation` is the documented class id for the automation
        // root and `IUIAutomation` is the interface bound to the result.
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
            .map_err(|error| AxError::Failed(format!("no UI Automation: {error}")))
    }

    /// Read one element without descending.
    fn read_node(&self, element: &IUIAutomationElement, handles: &HandleStore) -> Node {
        // SAFETY: `element` is a live automation element; each of these reads a
        // documented current property and returns a `Result` the crate checks.
        let role = unsafe { element.CurrentControlType() }
            .map(|control| control.0.to_string())
            .unwrap_or_default();
        let title = unsafe { element.CurrentName() }
            .map(|name| name.to_string())
            .unwrap_or_default();
        let value = read_value(element).unwrap_or_default();
        let bounds = unsafe { element.CurrentBoundingRectangle() }
            .ok()
            .map(rect_bounds);
        let handle = handles.hold(Element(element.clone()));
        Node {
            handle,
            role,
            title,
            value,
            bounds,
            children: Vec::new(),
        }
    }

    /// Read an element and its children to `depth`.
    fn read_tree(
        &self,
        automation: &IUIAutomation,
        element: &IUIAutomationElement,
        handles: &HandleStore,
        depth: usize,
    ) -> AxResult<Node> {
        let mut node = self.read_node(element, handles);
        if depth == 0 {
            return Ok(node);
        }
        // SAFETY: a true condition matches every child; both calls return a
        // `Result` the crate checks, and `element` is live.
        let condition = unsafe { automation.CreateTrueCondition() }
            .map_err(|error| AxError::Failed(format!("no walk condition: {error}")))?;
        if let Ok(children) = unsafe { element.FindAll(TreeScope_Children, &condition) } {
            let count = unsafe { children.Length() }.unwrap_or(0);
            for index in 0..count {
                if let Ok(child) = unsafe { children.GetElement(index) } {
                    node.children
                        .push(self.read_tree(automation, &child, handles, depth - 1)?);
                }
            }
        }
        Ok(node)
    }

    /// The application root for `pid`, or the focused element when `pid` is
    /// absent.
    fn root_for(
        &self,
        automation: &IUIAutomation,
        pid: Option<i32>,
    ) -> AxResult<IUIAutomationElement> {
        match pid {
            None => unsafe { automation.GetFocusedElement() }
                .map_err(|error| AxError::Failed(format!("no focused element: {error}"))),
            Some(pid) => {
                // SAFETY: the desktop root and a true condition are documented
                // calls; `FindAll` over the root's children returns the top
                // window of every application.
                let root = unsafe { automation.GetRootElement() }
                    .map_err(|error| AxError::Failed(format!("no root element: {error}")))?;
                let condition = unsafe { automation.CreateTrueCondition() }
                    .map_err(|error| AxError::Failed(format!("no walk condition: {error}")))?;
                let windows = unsafe { root.FindAll(TreeScope_Children, &condition) }
                    .map_err(|error| AxError::Failed(format!("no windows: {error}")))?;
                let count = unsafe { windows.Length() }.unwrap_or(0);
                for index in 0..count {
                    if let Ok(window) = unsafe { windows.GetElement(index) } {
                        if unsafe { window.CurrentProcessId() }.unwrap_or(0) == pid {
                            return Ok(window);
                        }
                    }
                }
                Err(AxError::Failed(format!(
                    "no top-level window belongs to pid {pid}"
                )))
            }
        }
    }
}

impl AxBackend for WindowsBackend {
    fn focused(&self, handles: &HandleStore) -> AxResult<Node> {
        let automation = self.automation()?;
        let element = unsafe { automation.GetFocusedElement() }
            .map_err(|error| AxError::Failed(format!("no focused element: {error}")))?;
        Ok(self.read_node(&element, handles))
    }

    fn tree(&self, handles: &HandleStore, pid: Option<i32>, depth: usize) -> AxResult<Node> {
        let automation = self.automation()?;
        let root = self.root_for(&automation, pid)?;
        self.read_tree(&automation, &root, handles, depth.min(MAX_DEPTH))
    }

    fn get(&self, handles: &HandleStore, handle: &str, attribute: &str) -> AxResult<Value> {
        handles.with(handle, |element| read_attribute(&element.0, attribute))?
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
        handles.with(handle, |element| set_value(&element.0, &text))?
    }

    fn press(&self, handles: &HandleStore, handle: &str) -> AxResult<()> {
        handles.with(handle, |element| invoke(&element.0))?
    }
}

/// The current value of an element that carries the value pattern.
fn read_value(element: &IUIAutomationElement) -> Option<String> {
    // SAFETY: the pattern id is the documented one for the value pattern, and
    // the interface is bound to the result; a control without the pattern
    // returns an error the caller treats as "no value".
    let pattern: IUIAutomationValuePattern =
        unsafe { element.GetCurrentPatternAs(UIA_ValuePatternId) }.ok()?;
    // SAFETY: `pattern` is a live value pattern for a real element.
    unsafe { pattern.CurrentValue() }
        .ok()
        .map(|value| value.to_string())
}

/// Read one named attribute.
fn read_attribute(element: &IUIAutomationElement, attribute: &str) -> AxResult<Value> {
    match attribute {
        "name" | "title" | "AXTitle" => {
            // SAFETY: `element` is live; `CurrentName` returns a checked `Result`.
            let name = unsafe { element.CurrentName() }
                .map_err(|error| AxError::Failed(format!("no name: {error}")))?;
            Ok(json!(name.to_string()))
        }
        "value" | "AXValue" => Ok(json!(read_value(element).unwrap_or_default())),
        "role" | "controltype" | "AXRole" => {
            // SAFETY: as above, a checked current-property read.
            let control = unsafe { element.CurrentControlType() }
                .map_err(|error| AxError::Failed(format!("no control type: {error}")))?;
            Ok(json!(control.0))
        }
        other => Err(AxError::NotSupported(format!(
            "attribute {other} is not read on Windows"
        ))),
    }
}

/// Write an element's value through the value pattern.
fn set_value(element: &IUIAutomationElement, text: &str) -> AxResult<()> {
    // SAFETY: the pattern id is the documented value-pattern id; a control
    // without the pattern returns an error rather than an interface.
    let pattern: IUIAutomationValuePattern =
        unsafe { element.GetCurrentPatternAs(UIA_ValuePatternId) }
            .map_err(|error| AxError::Failed(format!("element has no value pattern: {error}")))?;
    // SAFETY: `pattern` is a live value pattern; `SetValue` takes a BSTR that
    // outlives the call.
    unsafe { pattern.SetValue(&BSTR::from(text)) }
        .map_err(|error| AxError::Failed(format!("set failed: {error}")))
}

/// Trigger an element's default action through the invoke pattern.
fn invoke(element: &IUIAutomationElement) -> AxResult<()> {
    // SAFETY: the pattern id is the documented invoke-pattern id; a control
    // without the pattern returns an error rather than an interface.
    let pattern: IUIAutomationInvokePattern =
        unsafe { element.GetCurrentPatternAs(UIA_InvokePatternId) }
            .map_err(|error| AxError::Failed(format!("element has no invoke pattern: {error}")))?;
    // SAFETY: `pattern` is a live invoke pattern for a real element.
    unsafe { pattern.Invoke() }.map_err(|error| AxError::Failed(format!("invoke failed: {error}")))
}

/// Turn a UI Automation rectangle into the module's `(x, y, width, height)`.
fn rect_bounds(rect: RECT) -> (f64, f64, f64, f64) {
    (
        rect.left as f64,
        rect.top as f64,
        (rect.right - rect.left) as f64,
        (rect.bottom - rect.top) as f64,
    )
}
