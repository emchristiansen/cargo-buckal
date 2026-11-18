use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use cargo_metadata::MetadataCommand;
use clap::Parser;
use walkdir::WalkDir;

use crate::{
    buckal_log, buckal_note,
    utils::{UnwrapOrExit, ensure_prerequisites, get_buck2_root},
};

#[derive(Parser, Debug)]
pub struct AutoremoveArgs {
    /// Don’t actually remove the dependencies
    #[arg(name = "dry-run", long, default_value = "false")]
    pub dry_run: bool,
    /// Path to Cargo.toml
    #[arg(long)]
    pub manifest_path: Option<String>,
}

pub fn execute(args: &AutoremoveArgs) {
    ensure_prerequisites().unwrap_or_exit();
    let universe = crate::universe::select(args.manifest_path.as_deref()).unwrap_or_exit();

    let buck2_root = get_buck2_root().unwrap_or_exit();
    let cargo_metadata = if let Some(manifest) = &args.manifest_path {
        MetadataCommand::new()
            .manifest_path(manifest)
            .exec()
            .unwrap()
    } else {
        MetadataCommand::new().exec().unwrap()
    };
    let used_packages = cargo_metadata
        .packages
        .into_iter()
        .map(|p| {
            format!(
                "{}/{}/{}",
                extract_kind_str(&p.id.repr).unwrap_or_exit(),
                p.name,
                p.version
            )
        })
        .collect::<BTreeSet<_>>();

    if args.dry_run {
        buckal_note!("The following packages would be removed:");
    }

    let vendor_root = buck2_root.join(universe.vendor_root());
    for entry_path in
        unused_package_dirs(vendor_root.as_std_path(), &used_packages).unwrap_or_exit()
    {
        let entry_display = entry_path
            .strip_prefix(&buck2_root)
            .ok()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        if args.dry_run {
            println!("  {}", entry_display);
        } else {
            buckal_log!("Removing", entry_display.to_string());
            std::fs::remove_dir_all(&entry_path).unwrap_or_exit();
        }
    }

    if !args.dry_run {
        for entry_path in empty_name_dirs(vendor_root.as_std_path()).unwrap_or_exit() {
            std::fs::remove_dir_all(&entry_path).unwrap_or_exit();
        }
    }
}

/// The source-kind directories of a universe's vendor tree: registry crates
/// (`crates/`), git crates (`git/`) and, in a standalone universe, the project's
/// path packages it generates into its own tree (`path/`). Generation writes only
/// these, so autoremove walks only these. The root universe's vendor tree also
/// holds `standalone/`, the standalone universes' trees, which a root autoremove
/// must never touch.
const VENDOR_KINDS: [&str; 3] = ["crates", "git", "path"];

/// The `<kind>/<name>/<version>` directories of the vendor tree at `vendor_root`
/// whose label is not in `used`, walking only [`VENDOR_KINDS`].
fn unused_package_dirs(vendor_root: &Path, used: &BTreeSet<String>) -> Result<Vec<PathBuf>> {
    let mut unused = Vec::new();
    for kind in VENDOR_KINDS {
        let kind_dir = vendor_root.join(kind);
        if !kind_dir.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&kind_dir)
            .min_depth(2)
            .max_depth(2)
            .sort_by_file_name()
        {
            let entry = entry?;
            let label = entry
                .path()
                .strip_prefix(vendor_root)
                .expect("walked below the vendor root")
                .to_string_lossy()
                .replace('\\', "/");
            if !used.contains(&label) {
                unused.push(entry.into_path());
            }
        }
    }
    Ok(unused)
}

/// The `<kind>/<name>` directories of the vendor tree at `vendor_root` left empty,
/// walking only [`VENDOR_KINDS`].
fn empty_name_dirs(vendor_root: &Path) -> Result<Vec<PathBuf>> {
    let mut empty = Vec::new();
    for kind in VENDOR_KINDS {
        let kind_dir = vendor_root.join(kind);
        if !kind_dir.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&kind_dir)
            .min_depth(1)
            .max_depth(1)
            .sort_by_file_name()
        {
            let entry = entry?;
            if entry.file_type().is_dir() && entry.path().read_dir()?.next().is_none() {
                empty.push(entry.into_path());
            }
        }
    }
    Ok(empty)
}

/// Extract the source kind (registry or git) from a package ID string
fn extract_kind_str(string: &str) -> Result<&str> {
    let (kind, _) = string
        .split_once('+')
        .ok_or_else(|| anyhow::format_err!("invalid source `{}`", string))?;
    match kind {
        "registry" => Ok("crates"),
        // `path+file://…`: a standalone universe's `path/<name>/<version>`.
        _ => Ok(kind),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mkdir(root: &Path, path: &str) {
        std::fs::create_dir_all(root.join(path)).unwrap();
    }

    #[test]
    fn test_root_autoremove_never_reaches_a_standalone_universe() {
        let dir = tempfile::tempdir().unwrap();
        let vendor = dir.path();
        for path in [
            "crates/used/1.0.0",
            "crates/unused/0.1.0",
            "crates/empty",
            "git/gdep/0.1.0",
            "standalone/guest/crates/sdep/2.0.0",
            "standalone/guest/git/gsdep/0.2.0",
            "standalone/guest/path/shared/0.1.0",
        ] {
            mkdir(vendor, path);
        }
        std::fs::write(vendor.join("BUCK"), "").unwrap();
        let used: BTreeSet<String> = ["crates/used/1.0.0"].map(str::to_owned).into();
        let unused = unused_package_dirs(vendor, &used).unwrap();
        assert_eq!(
            unused,
            [
                vendor.join("crates/unused/0.1.0"),
                vendor.join("git/gdep/0.1.0")
            ]
        );
        assert_eq!(
            empty_name_dirs(vendor).unwrap(),
            [vendor.join("crates/empty")]
        );
    }

    #[test]
    fn test_standalone_autoremove_walks_its_own_tree() {
        let dir = tempfile::tempdir().unwrap();
        let vendor = dir.path().join("standalone/guest");
        for path in [
            "crates/sdep/2.0.0",
            "crates/stale/0.1.0",
            "path/shared/0.1.0",
            "path/gone/0.1.0",
        ] {
            mkdir(&vendor, path);
        }
        let used: BTreeSet<String> = ["crates/sdep/2.0.0", "path/shared/0.1.0"]
            .map(str::to_owned)
            .into();
        assert_eq!(
            unused_package_dirs(&vendor, &used).unwrap(),
            [
                vendor.join("crates/stale/0.1.0"),
                vendor.join("path/gone/0.1.0")
            ]
        );
    }
}
