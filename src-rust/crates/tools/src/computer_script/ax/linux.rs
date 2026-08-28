//! The Linux accessibility backend.
//!
//! Not yet implemented. AT-SPI2 exposes the tree over D-Bus, and a real backend
//! belongs here. It is held back rather than shipped unverified: a backend that
//! drives another machine's desktop must be compiled and run on that machine
//! before anyone trusts it, and this host cannot do either for a live AT-SPI2
//! bus.
//!
//! `focused`, `tree`, `get`, `set` and `press` therefore answer `NotSupported`,
//! which the tool reports as a plain "not on this platform" rather than as an
//! empty desktop.

use serde_json::Value;

use super::{AxBackend, AxError, AxResult, HandleStore, Node};

/// A held element. Empty until a real backend fills the type.
pub type Element = ();

pub struct LinuxBackend;

impl LinuxBackend {
    fn unsupported<T>() -> AxResult<T> {
        Err(AxError::NotSupported(
            "accessibility on Linux is not implemented yet".to_string(),
        ))
    }
}

impl AxBackend for LinuxBackend {
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
