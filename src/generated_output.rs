//! Optional destination confinement for a caller staging a generation.
//!
//! Cargo's own cache materialization is not generator output and does not use this boundary.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static OUTPUT_ROOT: OnceLock<PathBuf> = OnceLock::new();

pub(crate) fn confine(root: &Path) -> io::Result<()> {
    OUTPUT_ROOT.set(root.canonicalize()?).map_err(|_| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "generation output root already selected",
        )
    })
}

/// Check the resolved write location, including existing symlink ancestors.
pub(crate) fn check_write(path: impl AsRef<Path>) -> io::Result<()> {
    match OUTPUT_ROOT.get() {
        Some(root) => within(root, path.as_ref()),
        None => Ok(()),
    }
}

/// Removal unlinks the leaf itself rather than following a leaf symlink.
pub(crate) fn check_remove(path: impl AsRef<Path>) -> io::Result<()> {
    let path = std::path::absolute(path)?;
    check_write(path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot remove the filesystem root",
        )
    })?)
}

fn within(root: &Path, destination: &Path) -> io::Result<()> {
    let root = root.canonicalize()?;
    let mut ancestor = std::path::absolute(destination)?;
    if ancestor
        .components()
        .any(|part| part == std::path::Component::ParentDir)
    {
        // Existing paths can be resolved exactly; a missing path with `..` must not have its unresolved suffix silently discarded.
        ancestor = ancestor.canonicalize()?;
    }
    loop {
        match ancestor.symlink_metadata() {
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if !ancestor.pop() {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
    // A dangling existing symlink refuses here instead of being mistaken for an absent directory.
    let resolved = ancestor.canonicalize()?;
    if !resolved.starts_with(root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "generation refuses discarded cache output or another external destination: {}",
                destination.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::within;

    #[test]
    fn missing_children_below_internal_parent_are_supported() {
        let root = tempfile::tempdir().unwrap();
        within(root.path(), &root.path().join("new/child/BUCK")).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn nested_external_link_is_refused_without_walking_unrelated_inputs() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        std::os::unix::fs::symlink(cache.path(), outside.path().join("nested")).unwrap();
        assert!(within(root.path(), &root.path().join("link/nested/new/BUCK")).is_err());
        within(root.path(), &root.path().join("BUCK")).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn internal_file_link_preserves_its_destination() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("definition"), "").unwrap();
        std::os::unix::fs::symlink("definition", root.path().join("BUCK")).unwrap();
        within(root.path(), &root.path().join("BUCK")).unwrap();
    }
}
