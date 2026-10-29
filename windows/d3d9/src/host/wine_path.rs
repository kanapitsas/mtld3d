//! Paths on a native host build, which are already the host's own.

use std::path::Path;

/// The path the native runtime opens for `path`: the same one, when it is UTF-8.
#[must_use]
pub fn unix_path(path: &Path) -> Option<String> {
    path.to_str().map(str::to_owned)
}
