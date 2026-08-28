//! The elements a session is holding.
//!
//! A platform names an element with a raw pointer or a COM interface. Neither
//! can travel to a script, and neither stays valid once the window it belongs
//! to closes. The script gets an opaque id instead, and this store is the only
//! place that turns one back into an element.
//!
//! The ids are per session and monotonic, so a stale id from a closed window is
//! rejected by name rather than dereferenced.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use super::{AxError, AxResult};

/// A platform element, held for as long as the session names it.
///
/// The type is the platform's own; the store never looks inside it.
pub struct HandleStore<T = PlatformElement> {
    held: Mutex<HashMap<String, T>>,
    next: AtomicU64,
}

/// What the platform module puts in the store.
///
/// Each backend defines its own; this alias keeps the store's default
/// parameter honest on a platform with no backend.
#[cfg(target_os = "macos")]
pub type PlatformElement = super::macos::Element;
#[cfg(target_os = "windows")]
pub type PlatformElement = super::windows::Element;
#[cfg(target_os = "linux")]
pub type PlatformElement = super::linux::Element;
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
pub type PlatformElement = ();

impl<T> Default for HandleStore<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> HandleStore<T> {
    pub fn new() -> Self {
        Self {
            held: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        }
    }

    /// Hold `element` and name it.
    pub fn hold(&self, element: T) -> String {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let handle = format!("ax-{id}");
        self.held.lock().insert(handle.clone(), element);
        handle
    }

    /// Do something with the element `handle` names.
    ///
    /// The element never leaves the store: a backend reads it under the lock
    /// and returns what it read, so nothing above this module can keep a
    /// pointer past the moment the store drops it.
    pub fn with<R>(&self, handle: &str, act: impl FnOnce(&T) -> R) -> AxResult<R> {
        let held = self.held.lock();
        match held.get(handle) {
            Some(element) => Ok(act(element)),
            None => Err(AxError::UnknownHandle(handle.to_string())),
        }
    }

    /// Let go of everything.
    ///
    /// Called when the scripting session ends: an element outlives the window
    /// it names, and holding one keeps a platform object alive for nothing.
    pub fn clear(&self) {
        self.held.lock().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_element_comes_back_under_its_handle() {
        let store: HandleStore<u32> = HandleStore::new();

        let handle = store.hold(42);
        let read = store.with(&handle, |value| *value);

        assert_eq!(read, Ok(42));
    }

    #[test]
    fn an_unknown_handle_is_refused_rather_than_answered() {
        // The script sends whatever string it likes. A store that answered a
        // handle it does not hold would be answering about another element.
        let store: HandleStore<u32> = HandleStore::new();

        let read = store.with("ax-999", |value| *value);

        assert_eq!(read, Err(AxError::UnknownHandle("ax-999".to_string())));
    }

    #[test]
    fn two_elements_never_share_a_handle() {
        let store: HandleStore<u32> = HandleStore::new();

        let first = store.hold(1);
        let second = store.hold(2);

        assert_ne!(first, second);
        assert_eq!(store.with(&first, |value| *value), Ok(1));
        assert_eq!(store.with(&second, |value| *value), Ok(2));
    }

    #[test]
    fn clearing_the_store_drops_every_handle() {
        let store: HandleStore<u32> = HandleStore::new();
        let handle = store.hold(7);

        store.clear();

        assert!(store.with(&handle, |value| *value).is_err());
    }
}
