//! The macOS accessibility backend, over `AXUIElement`.
//!
//! `AXUIElement` is a C API in the ApplicationServices framework, built on
//! Core Foundation reference types. Every call here is `unsafe` because it
//! crosses into that framework, and each one carries a `// SAFETY:` note. The
//! reference-counting rule is the Core Foundation one: a function with `Copy`
//! or `Create` in its name returns a +1 reference this side must release, and
//! `CfRef` does exactly that on drop.

use core_foundation::base::{CFRelease, CFTypeRef, TCFType};
use core_foundation::string::{CFString, CFStringRef};
use serde_json::{json, Value};

use super::{AxBackend, AxError, AxResult, HandleStore, Node};

/// An owned `AXUIElementRef`, released when it drops.
///
/// The store holds these, so an element the script named stays alive exactly as
/// long as the session names it and no longer.
pub struct Element(AxRef);

// SAFETY: `AXUIElementRef` is a Core Foundation type. Its reference count is
// atomic, so moving one between threads and releasing it from another is sound.
// The store hands the inner pointer to a backend method only under its lock, so
// two threads never touch the same element at once.
unsafe impl Send for Element {}
unsafe impl Sync for Element {}

/// A raw Core Foundation reference this module owns.
struct CfRef(CFTypeRef);

impl Drop for CfRef {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `CfRef` is only ever built from a +1 reference (a `Copy`
            // or `Create` result), so releasing it here balances that one
            // retain and nothing else holds this pointer.
            unsafe { CFRelease(self.0) };
        }
    }
}

/// An owned `AXUIElementRef`, kept apart from `CfRef` only for the clearer type.
struct AxRef(AXUIElementRef);

impl Drop for AxRef {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: as `CfRef::drop`; an `AXUIElementRef` is a Core
            // Foundation type and this balances the +1 it was built from.
            unsafe { CFRelease(self.0 as CFTypeRef) };
        }
    }
}

type AXUIElementRef = *const std::ffi::c_void;
type AXValueRef = *const std::ffi::c_void;
type AXError = i32;

const K_AX_ERROR_SUCCESS: AXError = 0;
const K_AX_ERROR_API_DISABLED: AXError = -25211;
const K_AX_ERROR_NOT_IMPLEMENTED: AXError = -25208;
const K_AX_ERROR_NO_VALUE: AXError = -25212;

/// `AXValueType` for a `CGPoint`, `CGSize` and `CGRect`.
const K_AX_VALUE_CG_POINT: u32 = 1;
const K_AX_VALUE_CG_SIZE: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> AXError;
    fn AXUIElementSetAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> AXError;
    fn AXUIElementPerformAction(element: AXUIElementRef, action: CFStringRef) -> AXError;
    fn AXUIElementSetMessagingTimeout(element: AXUIElementRef, seconds: f32) -> AXError;
    fn AXValueGetValue(value: AXValueRef, the_type: u32, out: *mut std::ffi::c_void) -> bool;
    fn AXValueGetType(value: AXValueRef) -> u32;
}

/// The longest one AX message may wait for the target application to answer.
///
/// An accessibility read is a mach round trip to another process. A target that
/// is busy, or does not vend accessibility at all, would otherwise leave the
/// call blocked until the whole call deadline, and the loop would then blame
/// the script. This caps a single message so an unresponsive target fails fast
/// as `kAXErrorCannotComplete`, which reads as a plain failure the script can
/// catch.
const MESSAGING_TIMEOUT_SECS: f32 = 4.0;

const K_AX_ERROR_CANNOT_COMPLETE: AXError = -25204;

type CFArrayRef = *const std::ffi::c_void;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFArrayGetCount(array: CFArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFArrayRef, index: isize) -> *const std::ffi::c_void;
}

pub struct MacBackend;

impl MacBackend {
    /// The system-wide element, or a permission error when the process is not
    /// trusted.
    ///
    /// The trust check comes first on purpose. Without it every read answers an
    /// empty tree, which the script would read as an empty desktop rather than
    /// as "you have not granted this".
    fn system_wide(&self) -> AxResult<AxRef> {
        // SAFETY: a plain predicate with no arguments; it reads a process flag
        // and returns a bool.
        if !unsafe { AXIsProcessTrusted() } {
            return Err(AxError::PermissionDenied(
                "grant Accessibility to this app in System Settings > Privacy & Security"
                    .to_string(),
            ));
        }
        // SAFETY: takes no argument and returns a +1 reference, wrapped so it is
        // released on drop.
        let element = unsafe { AXUIElementCreateSystemWide() };
        if element.is_null() {
            return Err(AxError::Failed("no system-wide element".to_string()));
        }
        let owned = AxRef(element);
        // SAFETY: `owned.0` is a live element; setting its messaging timeout is
        // a plain configuration call the framework documents on any element.
        unsafe { AXUIElementSetMessagingTimeout(owned.0, MESSAGING_TIMEOUT_SECS) };
        Ok(owned)
    }

    /// The application element for `pid`, or the focused application's.
    fn application(&self, pid: Option<i32>) -> AxResult<AxRef> {
        let system = self.system_wide()?;
        match pid {
            Some(pid) => {
                // SAFETY: takes a pid and returns a +1 reference; wrapped for
                // release on drop.
                let element = unsafe { AXUIElementCreateApplication(pid) };
                if element.is_null() {
                    return Err(AxError::Failed(format!("no application for pid {pid}")));
                }
                let owned = AxRef(element);
                // SAFETY: a pid-created element does not inherit the system-wide
                // timeout, so it is bounded here too, on a live element.
                unsafe { AXUIElementSetMessagingTimeout(owned.0, MESSAGING_TIMEOUT_SECS) };
                Ok(owned)
            }
            None => copy_element(&system, "AXFocusedApplication"),
        }
    }
}

impl AxBackend for MacBackend {
    fn focused(&self, handles: &HandleStore) -> AxResult<Node> {
        let application = self.application(None)?;
        let focused = copy_element(&application, "AXFocusedUIElement")?;
        Ok(read_node(&focused, handles, 0))
    }

    fn tree(&self, handles: &HandleStore, pid: Option<i32>, depth: usize) -> AxResult<Node> {
        let application = self.application(pid)?;
        Ok(read_tree(&application, handles, depth.min(MAX_DEPTH)))
    }

    fn get(&self, handles: &HandleStore, handle: &str, attribute: &str) -> AxResult<Value> {
        handles.with(handle, |element| read_attribute(&element.0 .0, attribute))?
    }

    fn set(
        &self,
        handles: &HandleStore,
        handle: &str,
        attribute: &str,
        value: &Value,
    ) -> AxResult<()> {
        let text = value
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| value.to_string());
        handles.with(handle, |element| {
            set_string(&element.0 .0, attribute, &text)
        })?
    }

    fn press(&self, handles: &HandleStore, handle: &str) -> AxResult<()> {
        handles.with(handle, |element| perform(&element.0 .0, "AXPress"))?
    }
}

/// The deepest a tree walk goes, whatever a caller asks for.
///
/// A desktop's tree runs to thousands of nodes, and the script pays for every
/// one over the bridge. A caller that wants a single control names a shallow
/// depth; this only caps a runaway.
const MAX_DEPTH: usize = 12;

/// Turn an attribute name into a Core Foundation string once.
fn cfstr(text: &str) -> CFString {
    CFString::new(text)
}

/// Copy the element under `attribute`, wrapped for release.
fn copy_element(element: &AxRef, attribute: &str) -> AxResult<AxRef> {
    let key = cfstr(attribute);
    let mut out: CFTypeRef = std::ptr::null();
    // SAFETY: `element.0` is a live `AXUIElementRef` this module owns, `key`
    // outlives the call, and `out` is a valid slot for the +1 reference the
    // function writes on success.
    let status =
        unsafe { AXUIElementCopyAttributeValue(element.0, key.as_concrete_TypeRef(), &mut out) };
    translate_status(status)?;
    if out.is_null() {
        return Err(AxError::Failed(format!("{attribute} was empty")));
    }
    Ok(AxRef(out as AXUIElementRef))
}

/// Read a string, number or nested value under `attribute`.
fn read_attribute(element: &AXUIElementRef, attribute: &str) -> AxResult<Value> {
    let key = cfstr(attribute);
    let mut out: CFTypeRef = std::ptr::null();
    // SAFETY: `element` is a live reference held by the store, `key` outlives
    // the call, and `out` receives the +1 reference on success.
    let status =
        unsafe { AXUIElementCopyAttributeValue(*element, key.as_concrete_TypeRef(), &mut out) };
    if status == K_AX_ERROR_NO_VALUE {
        return Ok(Value::Null);
    }
    translate_status(status)?;
    let owned = CfRef(out);
    Ok(cf_to_json(owned.0))
}

/// Set a string attribute.
fn set_string(element: &AXUIElementRef, attribute: &str, text: &str) -> AxResult<()> {
    let key = cfstr(attribute);
    let value = cfstr(text);
    // SAFETY: `element` is live, and both `key` and `value` outlive the call.
    // The function retains `value` itself; the local drop releases this side's
    // reference.
    let status = unsafe {
        AXUIElementSetAttributeValue(
            *element,
            key.as_concrete_TypeRef(),
            value.as_concrete_TypeRef() as CFTypeRef,
        )
    };
    translate_status(status)
}

/// Trigger an action.
fn perform(element: &AXUIElementRef, action: &str) -> AxResult<()> {
    let key = cfstr(action);
    // SAFETY: `element` is live and `key` outlives the call.
    let status = unsafe { AXUIElementPerformAction(*element, key.as_concrete_TypeRef()) };
    translate_status(status)
}

/// Read one node without descending.
fn read_node(element: &AxRef, handles: &HandleStore, _depth: usize) -> Node {
    let role = string_attribute(&element.0, "AXRole");
    let title = first_present(&element.0, &["AXTitle", "AXDescription", "AXLabel"]);
    let value = string_attribute(&element.0, "AXValue");
    let bounds = read_bounds(&element.0);
    // The store owns the element from here; the walk keeps its own copy alive
    // only for the length of the descent.
    let handle = hold_copy(element, handles);
    Node {
        handle,
        role,
        title,
        value,
        bounds,
        children: Vec::new(),
    }
}

/// Read a node and its children to `depth`.
fn read_tree(element: &AxRef, handles: &HandleStore, depth: usize) -> Node {
    let mut node = read_node(element, handles, depth);
    if depth == 0 {
        return node;
    }
    for child in copy_children(&element.0) {
        node.children.push(read_tree(&child, handles, depth - 1));
    }
    node
}

/// Hold a fresh reference to `element` in the store and name it.
///
/// A retained copy rather than the walk's own reference: the walk drops its
/// references as it unwinds, and the store has to outlive that.
fn hold_copy(element: &AxRef, handles: &HandleStore) -> String {
    // SAFETY: `element.0` is live; `CFRetain` returns the same pointer with its
    // count raised by one, which the stored `Element` releases on drop.
    let retained = unsafe {
        core_foundation::base::CFRetain(element.0 as CFTypeRef);
        element.0
    };
    handles.hold(Element(AxRef(retained)))
}

/// The children of an element, each owned.
fn copy_children(element: &AXUIElementRef) -> Vec<AxRef> {
    let key = cfstr("AXChildren");
    let mut out: CFTypeRef = std::ptr::null();
    // SAFETY: `element` is live, `key` outlives the call, `out` receives the +1
    // array reference on success.
    let status =
        unsafe { AXUIElementCopyAttributeValue(*element, key.as_concrete_TypeRef(), &mut out) };
    if status != K_AX_ERROR_SUCCESS || out.is_null() {
        return Vec::new();
    }
    // The attribute answered a +1 `CFArray` of `AXUIElementRef`. Own it so it is
    // released once, and retain each element the vector keeps.
    let array = CfRef(out);
    // SAFETY: `array.0` is a live `CFArrayRef` this function owns for the
    // length of the loop; the count bounds every index the loop reads.
    let count = unsafe { CFArrayGetCount(array.0 as CFArrayRef) };
    let mut children = Vec::new();
    for index in 0..count {
        // SAFETY: `index` is below the count just read, so it is in bounds. The
        // value comes back under the get rule; retaining it gives the stored
        // `AxRef` its own +1 to release.
        let pointer = unsafe { CFArrayGetValueAtIndex(array.0 as CFArrayRef, index) };
        if pointer.is_null() {
            continue;
        }
        // SAFETY: `pointer` belongs to the array we still own; retaining it
        // raises its count for the stored `AxRef`.
        unsafe { core_foundation::base::CFRetain(pointer as CFTypeRef) };
        children.push(AxRef(pointer as AXUIElementRef));
    }
    children
}

/// A string attribute, or an empty string when absent.
fn string_attribute(element: &AXUIElementRef, attribute: &str) -> String {
    match read_attribute(element, attribute) {
        Ok(Value::String(text)) => text,
        Ok(other) if !other.is_null() => other.to_string(),
        _ => String::new(),
    }
}

/// The first attribute in `names` that answers a non-empty string.
fn first_present(element: &AXUIElementRef, names: &[&str]) -> String {
    for name in names {
        let text = string_attribute(element, name);
        if !text.is_empty() {
            return text;
        }
    }
    String::new()
}

/// The element's screen rectangle, when it reports position and size.
fn read_bounds(element: &AXUIElementRef) -> Option<(f64, f64, f64, f64)> {
    let position = copy_ax_value(element, "AXPosition")?;
    let size = copy_ax_value(element, "AXSize")?;
    let point = value_as_point(&position)?;
    let extent = value_as_size(&size)?;
    Some((point.x, point.y, extent.width, extent.height))
}

/// Copy an `AXValue` attribute, owned.
fn copy_ax_value(element: &AXUIElementRef, attribute: &str) -> Option<CfRef> {
    let key = cfstr(attribute);
    let mut out: CFTypeRef = std::ptr::null();
    // SAFETY: `element` is live, `key` outlives the call, `out` receives a +1
    // reference on success.
    let status =
        unsafe { AXUIElementCopyAttributeValue(*element, key.as_concrete_TypeRef(), &mut out) };
    if status != K_AX_ERROR_SUCCESS || out.is_null() {
        return None;
    }
    Some(CfRef(out))
}

fn value_as_point(value: &CfRef) -> Option<CGPoint> {
    let mut point = CGPoint { x: 0.0, y: 0.0 };
    // SAFETY: `value.0` is a live `AXValueRef`; the type check guards the cast,
    // and `point` is the right shape for a `CGPoint` payload.
    let ok = unsafe {
        AXValueGetType(value.0 as AXValueRef) == K_AX_VALUE_CG_POINT
            && AXValueGetValue(
                value.0 as AXValueRef,
                K_AX_VALUE_CG_POINT,
                &mut point as *mut CGPoint as *mut std::ffi::c_void,
            )
    };
    ok.then_some(point)
}

fn value_as_size(value: &CfRef) -> Option<CGSize> {
    let mut size = CGSize {
        width: 0.0,
        height: 0.0,
    };
    // SAFETY: as `value_as_point`, for a `CGSize` payload.
    let ok = unsafe {
        AXValueGetType(value.0 as AXValueRef) == K_AX_VALUE_CG_SIZE
            && AXValueGetValue(
                value.0 as AXValueRef,
                K_AX_VALUE_CG_SIZE,
                &mut size as *mut CGSize as *mut std::ffi::c_void,
            )
    };
    ok.then_some(size)
}

/// Turn a Core Foundation value into JSON, for the attributes a script reads.
///
/// Only the shapes a control carries are handled: a string, and anything else
/// as its description. A deeper conversion would pull in every Core Foundation
/// type for values a script never asks for.
fn cf_to_json(value: CFTypeRef) -> Value {
    if value.is_null() {
        return Value::Null;
    }
    // SAFETY: `value` is a live Core Foundation reference. `CFGetTypeID` reads
    // its type without taking ownership, and the string branch borrows it under
    // the get rule, leaving the caller's reference intact.
    unsafe {
        let type_id = core_foundation::base::CFGetTypeID(value);
        if type_id == CFString::type_id() {
            let string = CFString::wrap_under_get_rule(value as CFStringRef);
            return json!(string.to_string());
        }
    }
    // A number, boolean or nested element: name that it is present without
    // decoding a type the script did not ask for.
    json!({ "present": true })
}

/// Map an `AXError` onto the module's error, keeping permission apart.
fn translate_status(status: AXError) -> AxResult<()> {
    match status {
        K_AX_ERROR_SUCCESS => Ok(()),
        K_AX_ERROR_API_DISABLED => Err(AxError::PermissionDenied(
            "the accessibility API is disabled for this process".to_string(),
        )),
        K_AX_ERROR_NOT_IMPLEMENTED => Err(AxError::Failed(
            "the element does not implement that attribute".to_string(),
        )),
        K_AX_ERROR_CANNOT_COMPLETE => Err(AxError::Failed(
            "the application did not answer in time".to_string(),
        )),
        other => Err(AxError::Failed(format!("AXError {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disabled_api_reads_as_a_permission_error() {
        // The status the framework returns when the process is not trusted has
        // to reach the script as a permission error, not a generic failure, so
        // the message tells the user what to grant.
        let error = translate_status(K_AX_ERROR_API_DISABLED).expect_err("disabled is an error");

        assert!(matches!(error, AxError::PermissionDenied(_)), "{error}");
    }

    #[test]
    fn a_missing_value_is_not_an_error_at_the_attribute_layer() {
        // `AXValue` is absent on a great many controls. Read through
        // `read_attribute` it is `Null`, not a failure, so a tree walk does not
        // stop at the first control without a value.
        assert_eq!(translate_status(K_AX_ERROR_SUCCESS), Ok(()));
        assert_eq!(K_AX_ERROR_NO_VALUE, -25212);
    }

    #[test]
    fn a_string_value_decodes_to_a_json_string() {
        let value = CFString::new("hello");
        // SAFETY: the local `value` outlives the borrow `cf_to_json` takes under
        // the get rule.
        let json = cf_to_json(value.as_concrete_TypeRef() as CFTypeRef);

        assert_eq!(json, json!("hello"));
    }
}
