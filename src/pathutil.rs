//! Small path string helpers shared across the crate.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

/// Resolve `path` against `cwd` (absolute paths are kept as-is) and lexically
/// clean the result via [`clean_path`].
pub(crate) fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        clean_path(path)
    } else {
        clean_path(cwd.join(path))
    }
}

/// Lexically normalize a path: drop `.` components and resolve `..` without
/// touching the filesystem (so symlinks are not followed). An empty result
/// becomes `.`.
pub(crate) fn clean_path(path: impl AsRef<Path>) -> PathBuf {
    let mut cleaned = PathBuf::new();

    for component in path.as_ref().components() {
        match component {
            Component::Prefix(prefix) => cleaned.push(prefix.as_os_str()),
            Component::RootDir => cleaned.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                if !cleaned.pop() && !cleaned.has_root() {
                    cleaned.push("..");
                }
            }
            Component::Normal(value) => cleaned.push(value),
        }
    }

    if cleaned.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        cleaned
    }
}

/// Final path component of `path`, or the whole string when it has none.
pub(crate) fn basename(path: &str) -> String {
    Path::new(path).file_name().map_or_else(
        || path.to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Normalize each path to POSIX form without a leading `./`, returning the
/// distinct, non-empty results in first-seen order.
pub(crate) fn unique_paths(paths: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();

    for path in paths {
        let normalized = strip_dot_slash(&to_posix(&path));
        if !normalized.is_empty() && seen.insert(normalized.clone()) {
            unique.push(normalized);
        }
    }

    unique
}

/// Drop a single leading `./` from a path string.
pub(crate) fn strip_dot_slash(value: &str) -> String {
    value.strip_prefix("./").unwrap_or(value).to_owned()
}

/// Convert Windows-style backslash separators to forward slashes.
pub(crate) fn to_posix(value: &str) -> String {
    value.replace('\\', "/")
}

/// A path rendered as a POSIX-style (forward-slash) string.
pub(crate) fn path_to_posix(path: &Path) -> String {
    to_posix(&path_to_string(path))
}

/// A path rendered as a string, lossily decoding any non-UTF-8 components.
pub(crate) fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_path_drops_current_dir_components() {
        assert_eq!(clean_path("a/./b"), Path::new("a/b"));
    }

    #[test]
    fn clean_path_resolves_parent_dir_components() {
        assert_eq!(clean_path("a/b/../c"), Path::new("a/c"));
    }

    #[test]
    fn clean_path_keeps_an_unrooted_leading_parent_dir() {
        assert_eq!(clean_path("../a"), Path::new("../a"));
    }

    #[test]
    fn clean_path_maps_an_empty_path_to_dot() {
        assert_eq!(clean_path(""), Path::new("."));
    }

    #[test]
    fn unique_paths_dedup_preserving_first_seen_order() {
        let input = vec!["b".to_owned(), "a".to_owned(), "b".to_owned()];
        assert_eq!(unique_paths(input), ["b", "a"]);
    }

    #[test]
    fn unique_paths_normalize_separators_and_strip_dot_slash() {
        assert_eq!(
            unique_paths(vec!["./src\\app.ts".to_owned()]),
            ["src/app.ts"]
        );
    }

    #[test]
    fn unique_paths_drop_empty_entries() {
        assert_eq!(unique_paths(vec![String::new(), "a".to_owned()]), ["a"]);
    }
}
