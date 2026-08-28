//! The Windows accessibility backend.
//!
//! Not yet implemented. UI Automation is a COM API reached through the
//! `windows` crate's `Win32_UI_Accessibility` bindings, and a real backend
//! belongs here. It is held back rather than shipped unverified: a backend that
//! drives another machine's desktop must be compiled and run on that machine
//! before anyone trusts it, and this host cannot do either for Windows.
//!
//! `focused`, `tree`, `get`, `set` and `press` therefore answer `NotSupported`,
//! which the tool reports as a plain "not on this platform" rather than as an
//! empty desktop.

use serde_json::Value;

use super::{AxBackend, AxError, AxResult, HandleStore, Node};

/// A held element. Empty until a real backend fills the type.
pub type Element = ();

pub struct WindowsBackend;

impl WindowsBackend {
    fn unsupported<T>() -> AxResult<T> {
        Err(AxError::NotSupported(
            "accessibility on Windows is not implemented yet".to_string(),
        ))
    }
}

impl AxBackend for WindowsBackend {
    fn focused(&self, _handles: &HandleStore) -> AxResult<Node> {
        Self::unsupported()
    }
    fn tree(&self, _handles: &HandleStore, _pid: Option<i32>, _depth: usize) -> AxResult<Node> {
        Self::unsupported()
    }
    fn get(&self, _handles: &HandleStore, _handle: &str, _attribute: &str) -> AxResult<Value> {
        Self::unsupported()
    }
    fn set(
        &self,
        _handles: &HandleStore,
        _handle: &str,
        _attribute: &str,
        _value: &Value,
    ) -> AxResult<()> {
        Self::unsupported()
    }
    fn press(&self, _handles: &HandleStore, _handle: &str) -> AxResult<()> {
        Self::unsupported()
    }
}
