//! Path normalisation and component iteration.
//!
//! PMos paths are UTF-8 strings with `/` as the component
//! separator. This module provides:
//!
//! * [`normalize`] — collapse `//`, resolve `.` and `..`, strip
//!   trailing slashes (except for the root), and canonicalise
//!   to an absolute form. Relative inputs are treated as
//!   relative to `/` for v1 (the kernel always has an absolute
//!   starting point; the `cwd`-relative resolution lives in the
//!   syscall dispatch layer, which prepends the process's cwd
//!   before calling into the VFS).
//! * [`components`] — iterate over the path's slash-separated
//!   components, skipping empties. Returned names do NOT include
//!   the separator and never contain `.` or `..` (normalisation
//!   has already resolved those).
//! * [`split_last`] — split an absolute path into
//!   `(parent_absolute, basename)`. Used by create/mkdir/unlink
//!   to find the parent directory for the operation.

use alloc::string::String;
use alloc::vec::Vec;

/// Normalise an absolute or relative path to an absolute,
/// canonical form.
///
/// Rules:
///
/// * If the input is empty or does not start with `/`, it is
///   treated as relative to `/` — so `"foo"` becomes `"/foo"`.
/// * Consecutive `/` are collapsed.
/// * `.` components are dropped.
/// * `..` components pop the previous component, bounded at
///   the root (so `/..` → `/`, matching POSIX).
/// * A trailing `/` is stripped unless the result is `/`.
pub fn normalize(input: &str) -> String {
    let mut stack: Vec<&str> = Vec::new();
    for component in input.split('/') {
        match component {
            "" | "." => continue,
            ".." => {
                stack.pop();
            }
            name => stack.push(name),
        }
    }
    if stack.is_empty() {
        return String::from("/");
    }
    let mut out = String::new();
    for name in &stack {
        out.push('/');
        out.push_str(name);
    }
    out
}

/// Iterate over the components of a slash-separated path,
/// skipping empties. Use this on a *relative* path (e.g. the
/// suffix returned by `MountTable::longest_prefix`).
pub fn components(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|c| !c.is_empty() && *c != ".")
}

/// Split an absolute path into (parent_absolute, final_component).
/// Returns `None` if the input is the root.
///
/// Examples:
/// * `"/foo/bar"`     → `Some(("/foo",  "bar"))`
/// * `"/foo"`         → `Some(("/",     "foo"))`
/// * `"/"`            → `None`
pub fn split_last(path: &str) -> Option<(String, String)> {
    let normalised = normalize(path);
    if normalised == "/" {
        return None;
    }
    // Find the last slash. normalise() guarantees no trailing slash.
    let idx = normalised.rfind('/').unwrap_or(0);
    let (parent, rest) = normalised.split_at(idx);
    let name = &rest[1..]; // skip the leading '/'
    let parent_owned = if parent.is_empty() {
        String::from("/")
    } else {
        String::from(parent)
    };
    Some((parent_owned, String::from(name)))
}

/// Is this path the root?
pub fn is_root(path: &str) -> bool {
    path == "/"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_root() {
        assert_eq!(normalize("/"), "/");
        assert_eq!(normalize(""), "/");
        assert_eq!(normalize("//"), "/");
        assert_eq!(normalize("///"), "/");
    }

    #[test]
    fn normalize_simple_absolute() {
        assert_eq!(normalize("/foo"), "/foo");
        assert_eq!(normalize("/foo/bar"), "/foo/bar");
        assert_eq!(normalize("/foo/bar/baz.txt"), "/foo/bar/baz.txt");
    }

    #[test]
    fn normalize_collapses_double_slashes() {
        assert_eq!(normalize("//foo"), "/foo");
        assert_eq!(normalize("/foo//bar"), "/foo/bar");
        assert_eq!(normalize("/foo///bar////baz"), "/foo/bar/baz");
    }

    #[test]
    fn normalize_drops_dot_components() {
        assert_eq!(normalize("/./foo"), "/foo");
        assert_eq!(normalize("/foo/./bar"), "/foo/bar");
        assert_eq!(normalize("/foo/./././bar"), "/foo/bar");
    }

    #[test]
    fn normalize_resolves_dotdot() {
        assert_eq!(normalize("/foo/../bar"), "/bar");
        assert_eq!(normalize("/foo/bar/.."), "/foo");
        assert_eq!(normalize("/foo/bar/../baz"), "/foo/baz");
    }

    #[test]
    fn normalize_dotdot_bounded_at_root() {
        assert_eq!(normalize("/.."), "/");
        assert_eq!(normalize("/../.."), "/");
        assert_eq!(normalize("/../foo"), "/foo");
    }

    #[test]
    fn normalize_strips_trailing_slash() {
        assert_eq!(normalize("/foo/"), "/foo");
        assert_eq!(normalize("/foo/bar/"), "/foo/bar");
    }

    #[test]
    fn normalize_treats_relative_as_absolute() {
        assert_eq!(normalize("foo"), "/foo");
        assert_eq!(normalize("foo/bar"), "/foo/bar");
    }

    #[test]
    fn components_skip_empty_and_dot() {
        let c: Vec<&str> = components("foo/bar/baz").collect();
        assert_eq!(c, vec!["foo", "bar", "baz"]);
        let c: Vec<&str> = components("").collect();
        assert!(c.is_empty());
        let c: Vec<&str> = components("a//b").collect();
        assert_eq!(c, vec!["a", "b"]);
    }

    #[test]
    fn split_last_normal() {
        assert_eq!(
            split_last("/foo/bar"),
            Some((String::from("/foo"), String::from("bar")))
        );
        assert_eq!(
            split_last("/foo"),
            Some((String::from("/"), String::from("foo")))
        );
    }

    #[test]
    fn split_last_root_returns_none() {
        assert_eq!(split_last("/"), None);
        assert_eq!(split_last(""), None); // normalise to / first
    }

    #[test]
    fn split_last_after_normalisation() {
        assert_eq!(
            split_last("/foo/./bar/../baz"),
            Some((String::from("/foo"), String::from("baz")))
        );
    }
}
