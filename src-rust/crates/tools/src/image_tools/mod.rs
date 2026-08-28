//! Two image tools that reach a model rather than the local screen.
//!
//! `generate_image` asks a provider to draw an image from a structured prompt
//! and writes the result to a file. `inspect_image` sends a local image and a
//! question to a vision-capable model and returns its answer. Both resolve
//! their provider through the same account machinery the rest of the app uses,
//! so no new credential path is opened, and both stay out of the roster when no
//! provider is configured.

mod generate;
mod inspect;

pub use generate::GenerateImageTool;
pub use inspect::InspectImageTool;

use std::path::Path;

/// The image media type an extension implies, for the handful vision models
/// accept. `None` for anything else, so the caller reports an unsupported file
/// rather than sending a wrong media type.
pub(crate) fn media_type_for(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn a_known_extension_maps_to_its_media_type() {
        assert_eq!(
            media_type_for(&PathBuf::from("shot.PNG")),
            Some("image/png")
        );
        assert_eq!(
            media_type_for(&PathBuf::from("a/b/photo.jpeg")),
            Some("image/jpeg")
        );
    }

    #[test]
    fn an_unknown_extension_is_refused_not_guessed() {
        // A wrong media type would be sent to the model, so an unknown one is
        // reported instead of defaulting to something.
        assert!(media_type_for(&PathBuf::from("notes.txt")).is_none());
        assert!(media_type_for(&PathBuf::from("noextension")).is_none());
    }
}
