//! Canonical, component-based lane paths. Reparse points are never followed.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// Reject aliases before canonicalization (canonicalization alone hides them).
pub fn canonical_no_reparse(path: &Path) -> io::Result<PathBuf> {
    let absolute = absolute_clean(path)?;
    check_existing_components(&absolute)?;
    fs::canonicalize(absolute)
}

/// Resolve a future path using its nearest existing ancestor, without aliases.
pub fn resolve_no_reparse(path: &Path) -> io::Result<PathBuf> {
    let absolute = absolute_clean(path)?;
    check_existing_components(&absolute)?;
    let mut ancestor = absolute.as_path();
    let mut suffix = Vec::new();
    while !ancestor.try_exists()? {
        suffix.push(
            ancestor
                .file_name()
                .ok_or_else(|| invalid("no existing path ancestor"))?,
        );
        ancestor = ancestor
            .parent()
            .ok_or_else(|| invalid("no existing path ancestor"))?;
    }
    let mut result = fs::canonicalize(ancestor)?;
    for part in suffix.into_iter().rev() {
        result.push(part);
    }
    Ok(result)
}

fn absolute_clean(path: &Path) -> io::Result<PathBuf> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(invalid("empty paths and parent traversal are forbidden"));
    }
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    #[cfg(windows)]
    {
        use std::path::Prefix;
        match absolute.components().next() {
            Some(Component::Prefix(prefix))
                if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) => {}
            _ => return Err(invalid("lane paths must use a local drive")),
        }
        for component in absolute.components() {
            if let Component::Normal(name) = component {
                let name = name.to_string_lossy();
                if name.contains(':') || name.ends_with([' ', '.']) {
                    return Err(invalid(
                        "alternate streams and ambiguous Windows names are forbidden",
                    ));
                }
            }
        }
    }
    Ok(absolute)
}

fn check_existing_components(path: &Path) -> io::Result<()> {
    let mut current = PathBuf::new();
    let mut missing = false;
    for component in path.components() {
        current.push(component.as_os_str());
        if matches!(component, Component::Prefix(_) | Component::RootDir) {
            continue;
        }
        if missing {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if is_reparse(&metadata) => {
                return Err(invalid("reparse path aliases are forbidden"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => missing = true,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(crate) fn is_reparse(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// Compare canonical components, not textual prefixes (Mirror-vm is distinct).
pub fn contains(root: &Path, candidate: &Path) -> bool {
    let mut candidate = candidate.components();
    root.components().all(|part| {
        candidate
            .next()
            .is_some_and(|other| same_component(part.as_os_str(), other.as_os_str()))
    })
}

fn same_component(left: &std::ffi::OsStr, right: &std::ffi::OsStr) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn CompareStringOrdinal(
                left: *const u16,
                left_len: i32,
                right: *const u16,
                right_len: i32,
                ignore_case: i32,
            ) -> i32;
        }
        let left: Vec<u16> = left.encode_wide().collect();
        let right: Vec<u16> = right.encode_wide().collect();
        let (Ok(left_len), Ok(right_len)) = (i32::try_from(left.len()), i32::try_from(right.len()))
        else {
            return false;
        };
        // SAFETY: Both counted UTF-16 buffers remain live for this synchronous comparison.
        unsafe { CompareStringOrdinal(left.as_ptr(), left_len, right.as_ptr(), right_len, 1) == 2 }
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

/// Validate against every caller-enumerated Git worktree and discover ancestor .git files.
pub fn validate_lane_root(path: &Path, worktrees: &[PathBuf]) -> io::Result<PathBuf> {
    let canonical = resolve_no_reparse(path)?;
    for worktree in worktrees {
        if contains(&canonical_no_reparse(worktree)?, &canonical) {
            return Err(invalid("LaneRoot is inside a Git worktree"));
        }
    }
    #[cfg(windows)]
    {
        let cameron = resolve_no_reparse(Path::new(r"C:\Users\engin\Desktop\Mirror"))?;
        if contains(&cameron, &canonical) {
            return Err(invalid("LaneRoot is inside Cameron's checkout"));
        }
    }
    for ancestor in canonical.ancestors() {
        if ancestor.join(".git").try_exists()? {
            return Err(invalid("LaneRoot is inside a Git worktree"));
        }
    }
    Ok(canonical)
}

/// Return a canonical/future child, rejecting the root itself and all escapes.
pub fn validate_child(root: &Path, child: &Path) -> io::Result<PathBuf> {
    let root = canonical_no_reparse(root)?;
    let child = resolve_no_reparse(child)?;
    if !contains(&root, &child) || root.components().count() == child.components().count() {
        return Err(invalid("path is not a strict child of its bound root"));
    }
    Ok(child)
}

/// Remove only the exact package scratch shape `<root>/packages/<id>/source`.
/// Reparse entries, excessive trees and any changed path fail without traversal.
pub fn remove_scratch(root: &Path, scratch: &Path) -> io::Result<()> {
    remove_scratch_until(root, scratch, Instant::now() + Duration::from_secs(120))
}

/// As `remove_scratch`, additionally respecting the controller's absolute deadline.
/// Progress and subsequent cleanup entries never restart the deadline.
pub fn remove_scratch_until(root: &Path, scratch: &Path, deadline: Instant) -> io::Result<()> {
    check_deadline(deadline)?;
    let root = canonical_no_reparse(root)?;
    let scratch = validate_child(&root, scratch)?;
    check_deadline(deadline)?;
    let suffix: Vec<_> = scratch
        .components()
        .skip(root.components().count())
        .collect();
    if suffix.len() != 3
        || suffix[0].as_os_str() != "packages"
        || suffix[2].as_os_str() != "source"
        || !safe_id(&suffix[1].as_os_str().to_string_lossy())
    {
        return Err(invalid(
            "scratch must be the bound package source directory",
        ));
    }
    if !scratch.try_exists()? {
        return Ok(());
    }
    let mut entries = Vec::new();
    collect_tree(&scratch, 0, &mut entries, deadline)?;
    for (path, directory) in entries.into_iter().rev() {
        check_deadline(deadline)?;
        canonical_no_reparse(&path)?;
        let metadata = fs::symlink_metadata(&path)?;
        if is_reparse(&metadata) || metadata.is_dir() != directory {
            return Err(invalid("scratch changed during cleanup"));
        }
        if directory {
            fs::remove_dir(path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    check_deadline(deadline)?;
    Ok(())
}

fn check_deadline(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "scratch cleanup deadline expired",
        ))
    } else {
        Ok(())
    }
}

fn collect_tree(
    path: &Path,
    depth: usize,
    entries: &mut Vec<(PathBuf, bool)>,
    deadline: Instant,
) -> io::Result<()> {
    check_deadline(deadline)?;
    if depth > 128 || entries.len() >= 100_000 {
        return Err(invalid("scratch tree exceeds cleanup bounds"));
    }
    let metadata = fs::symlink_metadata(path)?;
    if is_reparse(&metadata) {
        return Err(invalid("scratch contains a reparse entry"));
    }
    entries.push((path.to_owned(), metadata.is_dir()));
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            collect_tree(&entry?.path(), depth + 1, entries, deadline)?;
        }
    } else if !metadata.is_file() {
        return Err(invalid("scratch contains a non-file entry"));
    }
    Ok(())
}

pub(crate) fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn root() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "mirror-paths-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        path
    }
    #[test]
    fn components_reject_checkout_but_allow_similar_sibling() {
        let root = root();
        let checkout = root.join("Mirror");
        fs::create_dir(&checkout).unwrap();
        fs::write(checkout.join(".git"), "gitdir: elsewhere").unwrap();
        assert!(validate_lane_root(&checkout.join("lane"), &[]).is_err());
        assert!(validate_lane_root(&root.join("Mirror-vm"), &[checkout]).is_ok());
        assert!(validate_child(&root, &root.join("../other")).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cleanup_is_exactly_bound_and_preserves_siblings() {
        let root = root();
        let source = root.join("packages/one/source");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("nested/file"), "scratch").unwrap();
        fs::write(root.join("keep"), "evidence").unwrap();
        assert!(remove_scratch(&root, &root).is_err());
        assert!(remove_scratch(&root, &root.join("packages/one")).is_err());
        remove_scratch(&root, &source).unwrap();
        assert!(!source.exists());
        assert!(root.join("keep").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn expired_cleanup_deadline_cannot_delete_the_first_entry() {
        let root = root();
        let source = root.join("packages/one/source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("retained"), "evidence").unwrap();
        let error = remove_scratch_until(&root, &source, Instant::now()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(source.join("retained").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn junction_aliases_are_rejected_before_canonicalization_and_cleanup() {
        use std::os::windows::process::CommandExt;
        let root = root();
        let outside = root.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), "must survive").unwrap();
        let source = root.join("packages/one/source");
        fs::create_dir_all(&source).unwrap();
        let link = source.join("alias");
        let output = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", "$ErrorActionPreference = 'Stop'; New-Item -ItemType Junction -Path $env:MIRROR_FIXTURE_LINK -Target $env:MIRROR_FIXTURE_TARGET | Out-Null"])
            .env("MIRROR_FIXTURE_LINK", &link).env("MIRROR_FIXTURE_TARGET", &outside)
            .creation_flags(0x0800_0000).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(canonical_no_reparse(&link).is_err());
        assert!(resolve_no_reparse(&link.join("not-created")).is_err());
        assert!(validate_lane_root(&link, &[]).is_err());
        assert!(remove_scratch(&root, &source).is_err());
        assert!(outside.join("keep").exists());
        // Only remove the known fixture junction itself, without following its target.
        fs::remove_dir(&link).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
