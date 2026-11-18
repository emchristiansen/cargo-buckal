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

/// The `rust_filegroup` the root generation writes in a package of the project's
/// own workspace whose sources a standalone universe compiles.
pub const STANDALONE_SRCS: &str = "standalone-srcs";

/// The `alias` of that package's `cargo_manifest` the root generation writes
/// beside it, through which a universe's copy of the package reads its manifest.
pub const STANDALONE_MANIFEST: &str = "standalone-manifest";

/// The prelude file defining `rust_filegroup`.
pub const RUST_FILEGROUP_BZL: &str = "@prelude//rust:sources.bzl";

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
        // A path package's package-id may not parse as a spec: Cargo writes a
        // package named after its directory without the name
        // (`path+file:///…/guest#0.1.0`).
        if repr.starts_with("path+") {
            return Ok(self.path_package_dir(repr));
        }
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

    /// Where the universe generates the path package whose canonical package-id
    /// is `repr`: a standalone universe generates a path package of the project
    /// outside its own workspace (`path+file://($PROJECT)/<dir>#…`, see
    /// [`crate::cache::PROJECT_ID_PLACEHOLDER`]) into
    /// `<vendor root>/path/<name>/<version>`, so that it never writes the owning
    /// package's BUCK file. Every other path package is generated in place:
    /// `None`.
    pub fn path_package_dir(&self, repr: &str) -> Option<String> {
        let Universe::Standalone { .. } = self else {
            return None;
        };
        let rest = repr.strip_prefix(crate::cache::PROJECT_ID_PLACEHOLDER)?;
        let (dir, fragment) = rest.split_once('#')?;
        let (name, version) = match fragment.split_once('@') {
            Some((name, version)) => (name, version),
            // Named after its directory: Cargo leaves the name out. The project's
            // own root package (an empty `dir`) is named after the project root.
            None => match dir.rsplit('/').next().filter(|name| !name.is_empty()) {
                Some(name) => (name, fragment),
                None => (
                    project_root()
                        .and_then(|root| root.file_name())
                        .expect("generation selects the universe, and with it the project root, before it names a vendored path package"),
                    fragment,
                ),
            },
        };
        Some(format!("{}/path/{name}/{version}", self.vendor_root()))
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
        if dir == root || !holds_file(&dir, CONFIG_FILE)? {
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

/// The standalone workspace enclosing `cwd`, relative to the Buck2 root
/// `buck2_root`: the nearest directory from `cwd` up to, but not including, the
/// root that holds both a `Cargo.toml` and a `buckal.toml`.
fn enclosing_standalone(cwd: &Utf8Path, buck2_root: &Utf8Path) -> Result<Option<Utf8PathBuf>> {
    let cwd = cwd
        .canonicalize_utf8()
        .with_context(|| format!("failed to resolve the current directory {cwd}"))?;
    let root = buck2_root
        .canonicalize_utf8()
        .with_context(|| format!("failed to resolve the Buck2 root {buck2_root}"))?;
    for dir in cwd.ancestors() {
        let Ok(relative) = dir.strip_prefix(&root) else {
            return Ok(None);
        };
        if relative.as_str().is_empty() {
            return Ok(None);
        }
        if holds_file(dir, "Cargo.toml")? && holds_file(dir, CONFIG_FILE)? {
            return Ok(Some(relative.to_owned()));
        }
    }
    Ok(None)
}

static SELECTED: OnceLock<Universe> = OnceLock::new();

static PROJECT_ROOT: OnceLock<Utf8PathBuf> = OnceLock::new();

/// The Buck2 project root, once [`select`] has run: generation records a path
/// package outside its workspace but inside the project relative to it.
pub fn project_root() -> Option<&'static Utf8PathBuf> {
    PROJECT_ROOT.get()
}

/// Select the universe `manifest_path` names for the rest of the process. A command
/// taking `--manifest-path` calls this before it reads a configuration, a
/// generation record or a vendor path; selecting a second, different universe is an
/// error.
///
/// Refused, before anything is written:
/// - no `--manifest-path` inside a standalone workspace, where Cargo would resolve
///   the standalone workspace while generation took the root universe and
///   rewrote the root record;
/// - a standalone universe sharing its vendor tree with another
///   ([`crate::buckify::tables::check_universe_names`]).
pub fn select(manifest_path: Option<&str>) -> Result<&'static Universe> {
    let buck2_root = get_buck2_root().context("failed to get the Buck2 project root")?;
    if manifest_path.is_none() {
        let cwd = std::env::current_dir().context("failed to read the current directory")?;
        let cwd = Utf8PathBuf::from_path_buf(cwd).map_err(|cwd| {
            anyhow::anyhow!("the current directory {} is not UTF-8", cwd.display())
        })?;
        if let Some(dir) = enclosing_standalone(&cwd, &buck2_root)? {
            bail!(
                "the current directory is inside the standalone workspace `{dir}`; generate it with `--manifest-path {dir}/Cargo.toml`, or run from outside it to generate the Buck2 project's own workspace. Nothing is written"
            );
        }
    }
    let universe = Universe::of_manifest(manifest_path, &buck2_root)?;
    let _ = PROJECT_ROOT.set(buck2_root.clone());
    if matches!(universe, Universe::Standalone { .. }) {
        use crate::buckify::tables::{CellLayout, check_universe_names, standalone_universes};
        let layout = CellLayout::of_project(&buck2_root).map_err(anyhow::Error::msg)?;
        let found = standalone_universes(&buck2_root, &layout).map_err(anyhow::Error::msg)?;
        check_universe_names(&found, Some(&universe)).map_err(anyhow::Error::msg)?;
    }
    let selected = SELECTED.get_or_init(|| universe.clone());
    if *selected != universe {
        bail!("this command already generates {selected:?}; it cannot also generate {universe:?}");
    }
    Ok(selected)
}

/// The selected universe: the root universe unless [`select`] chose another.
/// Whether `dir` holds the file `name`, deciding for selection, the walk of the
/// project and configuration loading alike whether a directory is a universe. A
/// symlink counts as the file it resolves to. A dangling symlink, or an entry that
/// is not a file, is refused: the walk sees the entry while a check that follows
/// it sees nothing, so counting it either way would let the two disagree.
pub fn holds_file(dir: &Utf8Path, name: &str) -> Result<bool> {
    let path = dir.join(name);
    match path.symlink_metadata() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).with_context(|| format!("failed to inspect {path}")),
        Ok(_) => {}
    }
    let metadata = path
        .metadata()
        .with_context(|| format!("{path} is a symlink that resolves to no file"))?;
    if !metadata.is_file() {
        bail!("{path} is not a file");
    }
    Ok(true)
}

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
    fn test_a_path_package_is_never_vendored() {
        let universe = Universe::Standalone {
            dir: "standalone_workspaces/guest".into(),
            name: "guest".to_owned(),
        };
        // A package named after its directory: Cargo leaves the name out.
        assert_eq!(
            universe
                .vendor_path_relative("path+file://($WORKSPACE)#0.1.0")
                .unwrap(),
            None
        );
        assert_eq!(
            universe
                .vendor_path_relative("path+file://($WORKSPACE)#guest-pkg@0.1.0")
                .unwrap(),
            None
        );
        assert_eq!(
            universe
                .vendor_path_relative(
                    "registry+https://github.com/rust-lang/crates.io-index#sdep@2.0.0"
                )
                .unwrap()
                .as_deref(),
            Some("third-party/rust/standalone/guest/crates/sdep/2.0.0")
        );
    }

    #[test]
    fn test_a_project_path_package_is_generated_into_the_universe_tree() {
        let universe = Universe::Standalone {
            dir: "standalone_workspaces/guest".into(),
            name: "guest".to_owned(),
        };
        assert_eq!(
            universe
                .vendor_path_relative("path+file://($PROJECT)/libs/shared_lib#shared-lib@0.2.0")
                .unwrap()
                .as_deref(),
            Some("third-party/rust/standalone/guest/path/shared-lib/0.2.0")
        );
        // Named after its directory: the name is the directory's.
        assert_eq!(
            universe
                .vendor_path_relative("path+file://($PROJECT)/libs/prelude#0.1.0")
                .unwrap()
                .as_deref(),
            Some("third-party/rust/standalone/guest/path/prelude/0.1.0")
        );
        // The root universe generates its path packages in place.
        assert_eq!(
            Universe::Root
                .vendor_path_relative("path+file://($PROJECT)/libs/prelude#0.1.0")
                .unwrap(),
            None
        );
    }

    #[test]
    fn test_inside_a_standalone_workspace_is_found() {
        let dir = project();
        let root = root_of(&dir);
        let guest = root.join("standalone_workspaces/guest");
        std::fs::create_dir_all(guest.join("src")).unwrap();
        // Without its own configuration the directory is the root workspace's.
        assert_eq!(
            enclosing_standalone(&guest.join("src"), &root).unwrap(),
            None
        );
        std::fs::write(guest.join(CONFIG_FILE), "").unwrap();
        assert_eq!(
            enclosing_standalone(&guest.join("src"), &root).unwrap(),
            Some("standalone_workspaces/guest".into())
        );
        assert_eq!(
            enclosing_standalone(&guest, &root).unwrap(),
            Some("standalone_workspaces/guest".into())
        );
        // The root itself, with its own buckal.toml, is not standalone.
        assert_eq!(enclosing_standalone(&root, &root).unwrap(), None);
    }

    #[test]
    #[cfg(unix)]
    fn test_selection_follows_a_symlinked_configuration_and_refuses_a_dangling_one() {
        let dir = project();
        let root = root_of(&dir);
        let guest = root.join("standalone_workspaces/guest");
        let manifest = guest.join("Cargo.toml");
        std::os::unix::fs::symlink("missing.toml", guest.join(CONFIG_FILE)).unwrap();
        let error = Universe::of_manifest(Some(manifest.as_str()), &root).unwrap_err();
        assert!(
            format!("{error:#}").contains("is a symlink that resolves to no file"),
            "{error:#}"
        );
        assert!(enclosing_standalone(&guest, &root).is_err());
        std::fs::write(guest.join("missing.toml"), "").unwrap();
        assert_eq!(
            Universe::of_manifest(Some(manifest.as_str()), &root).unwrap(),
            Universe::Standalone {
                dir: "standalone_workspaces/guest".into(),
                name: "guest".to_owned()
            }
        );
        assert_eq!(
            enclosing_standalone(&guest, &root).unwrap(),
            Some("standalone_workspaces/guest".into())
        );
    }

    #[test]
    fn test_the_root_universe_vendors_into_the_shared_tree() {
        assert_eq!(Universe::Root.crates_root(), crate::RUST_CRATES_ROOT);
        assert_eq!(Universe::Root.git_root(), crate::RUST_GIT_ROOT);
        assert_eq!(Universe::Root.config_dir(), "");
    }
}
