//! Which Cargo workspace a command generates: the Buck2 project's own, or a
//! standalone workspace inside the project that has a resolution of its own.
//!
//! A standalone workspace is a Cargo workspace below the Buck2 root that is not a
//! member of the root workspace and holds its own `Cargo.lock`. Its graph can need
//! crate versions, or features of shared versions, that the root graph does not
//! have. Generating it into the root workspace's vendor tree would let one graph
//! overwrite the other's BUCK files, so a standalone workspace is generated as its
//! own universe:
//!
//! - it is selected by a `--manifest-path` naming a `Cargo.toml` whose directory
//!   holds a `buckal.toml` of its own and is not the Buck2 root;
//! - its configuration is that `buckal.toml`, and its generation record is the
//!   `buckal.snap` beside it, so generating one universe never reads or replaces
//!   another's record;
//! - its registry and git crates are vendored under
//!   `third-party/rust/standalone/<directory name>/{crates,git}`, a tree no other
//!   universe writes.
//!
//! Every other command line generates the Buck2 project's own workspace, with the
//! root `buckal.toml`, `buckal.snap` and `third-party/rust/{crates,git}`.

use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use cargo_metadata::camino::{Utf8Path, Utf8PathBuf};
use cargo_util_schemas::core::{PackageIdSpec, SourceKind};

use crate::{RUST_ROOT, utils::get_buck2_root};

/// The directory below [`RUST_ROOT`] holding every standalone universe's vendor tree.
pub const STANDALONE_ROOT: &str = "standalone";

/// The configuration file of a universe.
pub const CONFIG_FILE: &str = "buckal.toml";

/// The generation record of a universe.
pub const CACHE_FILE: &str = "buckal.snap";

/// The universe a command generates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Universe {
    /// The Buck2 project's own workspace.
    Root,
    /// The standalone workspace at `dir` (relative to the Buck2 root), named by its
    /// directory's last component.
    Standalone { dir: Utf8PathBuf, name: String },
}

impl Universe {
    /// The directory, relative to the Buck2 root, holding the universe's
    /// `buckal.toml` and `buckal.snap` (empty for the root universe).
    pub fn config_dir(&self) -> &Utf8Path {
        match self {
            Universe::Root => Utf8Path::new(""),
            Universe::Standalone { dir, .. } => dir,
        }
    }

    /// The directory, relative to the Buck2 root, below which the universe's
    /// registry crates (`crates/`) and git crates (`git/`) are vendored.
    pub fn vendor_root(&self) -> String {
        match self {
            Universe::Root => RUST_ROOT.to_owned(),
            Universe::Standalone { name, .. } => format!("{RUST_ROOT}/{STANDALONE_ROOT}/{name}"),
        }
    }

    /// Where the universe's registry crates are vendored.
    pub fn crates_root(&self) -> String {
        format!("{}/crates", self.vendor_root())
    }

    /// Where the universe's git crates are vendored.
    pub fn git_root(&self) -> String {
        format!("{}/git", self.vendor_root())
    }

    /// Where the universe vendors the package whose package-id is `repr`, relative
    /// to the Buck2 root: `<crates root>/<name>/<version>` for a registry package,
    /// `<git root>/<name>/<version>` for a git package, and `None` for any other
    /// source (a path package is first-party, not vendored).
    pub fn vendor_path_relative(&self, repr: &str) -> Result<Option<String>> {
        let spec =
            PackageIdSpec::parse(repr).with_context(|| format!("`{repr}` is not a package-id"))?;
        let root = match spec.kind() {
            Some(SourceKind::Registry) => self.crates_root(),
            Some(SourceKind::Git(_)) => self.git_root(),
            _ => return Ok(None),
        };
        let version = spec
            .version()
            .with_context(|| format!("package-id `{repr}` names no version"))?;
        Ok(Some(format!("{root}/{}/{version}", spec.name())))
    }

    /// The universe `manifest_path` selects, `buck2_root` being the Buck2 project
    /// root: a standalone universe when the manifest's directory holds a
    /// `buckal.toml` and is not the Buck2 root, the root universe otherwise.
    pub fn of_manifest(manifest_path: Option<&str>, buck2_root: &Utf8Path) -> Result<Self> {
        let Some(manifest_path) = manifest_path else {
            return Ok(Universe::Root);
        };
        let manifest = Utf8Path::new(manifest_path);
        let manifest = if manifest.is_absolute() {
            manifest.to_owned()
        } else {
            let cwd = std::env::current_dir().context("failed to read the current directory")?;
            Utf8PathBuf::from_path_buf(cwd)
                .map_err(|cwd| {
                    anyhow::anyhow!("the current directory {} is not UTF-8", cwd.display())
                })?
                .join(manifest)
        };
        let dir = manifest.parent().with_context(|| {
            format!("`--manifest-path {manifest_path}` names no file in a directory")
        })?;
        let dir = dir.canonicalize_utf8().with_context(|| {
            format!("failed to resolve the directory of `--manifest-path {manifest_path}`")
        })?;
        let root = buck2_root
            .canonicalize_utf8()
            .with_context(|| format!("failed to resolve the Buck2 root {buck2_root}"))?;
        if dir == root || !dir.join(CONFIG_FILE).is_file() {
            return Ok(Universe::Root);
        }
        let Ok(relative) = dir.strip_prefix(&root) else {
            bail!(
                "`--manifest-path {manifest_path}` has a `{CONFIG_FILE}` beside it but lies outside the Buck2 project {root}"
            );
        };
        let name = relative
            .file_name()
            .with_context(|| format!("the standalone workspace {relative} has no directory name"))?
            .to_owned();
        Ok(Universe::Standalone {
            dir: relative.to_owned(),
            name,
        })
    }
}

static SELECTED: OnceLock<Universe> = OnceLock::new();

/// Select the universe `manifest_path` names for the rest of the process. A command
/// taking `--manifest-path` calls this before it reads a configuration, a
/// generation record or a vendor path; selecting a second, different universe is an
/// error.
pub fn select(manifest_path: Option<&str>) -> Result<&'static Universe> {
    let buck2_root = get_buck2_root().context("failed to get the Buck2 project root")?;
    let universe = Universe::of_manifest(manifest_path, &buck2_root)?;
    let selected = SELECTED.get_or_init(|| universe.clone());
    if *selected != universe {
        bail!("this command already generates {selected:?}; it cannot also generate {universe:?}");
    }
    Ok(selected)
}

/// The selected universe: the root universe unless [`select`] chose another.
pub fn selected() -> &'static Universe {
    SELECTED.get_or_init(|| Universe::Root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("standalone_workspaces/guest")).expect("mkdir");
        std::fs::write(dir.path().join("Cargo.toml"), "").expect("write");
        std::fs::write(dir.path().join(CONFIG_FILE), "").expect("write");
        std::fs::write(
            dir.path().join("standalone_workspaces/guest/Cargo.toml"),
            "",
        )
        .expect("write");
        dir
    }

    fn root_of(dir: &tempfile::TempDir) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(dir.path().to_owned()).expect("utf8")
    }

    #[test]
    fn test_no_manifest_path_is_the_root_universe() {
        let dir = project();
        assert_eq!(
            Universe::of_manifest(None, &root_of(&dir)).unwrap(),
            Universe::Root
        );
    }

    #[test]
    fn test_the_root_manifest_is_the_root_universe() {
        let dir = project();
        let root = root_of(&dir);
        let manifest = root.join("Cargo.toml");
        assert_eq!(
            Universe::of_manifest(Some(manifest.as_str()), &root).unwrap(),
            Universe::Root
        );
    }

    #[test]
    fn test_a_workspace_without_its_own_config_is_the_root_universe() {
        let dir = project();
        let root = root_of(&dir);
        let manifest = root.join("standalone_workspaces/guest/Cargo.toml");
        assert_eq!(
            Universe::of_manifest(Some(manifest.as_str()), &root).unwrap(),
            Universe::Root
        );
    }

    #[test]
    fn test_a_workspace_with_its_own_config_is_a_standalone_universe() {
        let dir = project();
        let root = root_of(&dir);
        std::fs::write(
            root.join("standalone_workspaces/guest").join(CONFIG_FILE),
            "",
        )
        .unwrap();
        let manifest = root.join("standalone_workspaces/guest/Cargo.toml");
        let universe = Universe::of_manifest(Some(manifest.as_str()), &root).unwrap();
        assert_eq!(
            universe,
            Universe::Standalone {
                dir: "standalone_workspaces/guest".into(),
                name: "guest".to_owned()
            }
        );
        assert_eq!(universe.config_dir(), "standalone_workspaces/guest");
        assert_eq!(
            universe.crates_root(),
            "third-party/rust/standalone/guest/crates"
        );
        assert_eq!(universe.git_root(), "third-party/rust/standalone/guest/git");
    }

    #[test]
    fn test_the_root_universe_vendors_into_the_shared_tree() {
        assert_eq!(Universe::Root.crates_root(), crate::RUST_CRATES_ROOT);
        assert_eq!(Universe::Root.git_root(), crate::RUST_GIT_ROOT);
        assert_eq!(Universe::Root.config_dir(), "");
    }
}
