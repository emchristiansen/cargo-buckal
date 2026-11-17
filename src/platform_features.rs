//! Per-platform feature resolution.
//!
//! `cargo metadata` reports, for every package, the union of the features enabled
//! on any platform, even with `--filter-platform`. A feature that a dependency
//! enables only under `[target.'cfg(...)'.dependencies]` (e.g. `browser` for a
//! wasm32 build) therefore ends up in the native build too, which can break it.
//!
//! `cargo tree --target <triple>` runs Cargo's own feature resolver for that one
//! target, so it reports the features Cargo would actually compile with there.
//! This module asks it once per configured platform.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::process::Command;

use anyhow::{Context, Result, bail};
use cargo_metadata::{Package, PackageId};

use crate::buckal_warn;

/// The prelude `platform` attribute: `{platform name: {attribute: values}}`.
pub type PlatformAttrs = BTreeMap<String, BTreeMap<String, BTreeSet<String>>>;

/// Features per package, per platform name.
pub type PlatformFeatures = HashMap<PackageId, BTreeMap<String, BTreeSet<String>>>;

/// Resolve the features of every package for each `(platform name, target triple)`.
///
/// Edges are normal, build and dev (dev only for workspace members), which is the
/// resolution `cargo test --workspace --target <triple>` compiles with.
pub fn resolve_platform_features(
    manifest_path: Option<&str>,
    platforms: &BTreeMap<String, String>,
    packages: &HashMap<PackageId, Package>,
) -> Result<PlatformFeatures> {
    let mut result = PlatformFeatures::new();
    for (platform, triple) in platforms {
        let output = run_cargo_tree(manifest_path, triple)
            .with_context(|| format!("failed to resolve features for platform `{platform}`"))?;
        let parsed = parse_cargo_tree(&output, packages);
        for name in &parsed.ambiguous {
            buckal_warn!(
                "Features of `{}` on platform `{}` could not be attributed to one package; using the union over platforms for it.",
                name,
                platform
            );
        }
        for (id, features) in parsed.features {
            result
                .entry(id)
                .or_default()
                .insert(platform.clone(), features);
        }
        for id in parsed.ambiguous_ids {
            result.remove(&id);
        }
    }
    Ok(result)
}

fn run_cargo_tree(manifest_path: Option<&str>, triple: &str) -> Result<String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let mut cmd = Command::new(cargo);
    cmd.args([
        "tree",
        "--workspace",
        "--target",
        triple,
        "--edges",
        "normal,build,dev",
        "--prefix",
        "none",
        "--format",
        "{p}|{f}",
    ]);
    if let Some(path) = manifest_path {
        cmd.args(["--manifest-path", path]);
    }
    let output = cmd.output().context("failed to run `cargo tree`")?;
    if !output.status.success() {
        bail!(
            "`cargo tree --target {triple}` exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?)
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct ParsedTree {
    /// Union of the features reported on every line for a package (a package can be
    /// listed for both the host and the target with different features).
    pub features: HashMap<PackageId, BTreeSet<String>>,
    /// Packages whose lines could not be matched to exactly one metadata package.
    pub ambiguous: BTreeSet<String>,
    /// Metadata packages that a line might have meant; their result is unreliable.
    pub ambiguous_ids: BTreeSet<PackageId>,
}

/// Parse `cargo tree --prefix none --format '{p}|{f}'` output.
///
/// `{p}` is `name vX.Y.Z`, optionally followed by `(proc-macro)` and by the source in
/// parentheses (a path for local packages, a URL for git ones); a line repeated
/// because of deduplication ends in ` (*)`. `{f}` is a comma-separated feature list.
pub(crate) fn parse_cargo_tree(output: &str, packages: &HashMap<PackageId, Package>) -> ParsedTree {
    let mut by_name_version: HashMap<(&str, String), Vec<&Package>> = HashMap::new();
    for package in packages.values() {
        by_name_version
            .entry((package.name.as_str(), package.version.to_string()))
            .or_default()
            .push(package);
    }

    let mut parsed = ParsedTree::default();
    for line in output.lines() {
        let line = line.trim();
        let line = line.strip_suffix("(*)").map(str::trim_end).unwrap_or(line);
        let Some((p, f)) = line.rsplit_once('|') else {
            continue;
        };
        let mut words = p.split_whitespace();
        let (Some(name), Some(version)) = (words.next(), words.next()) else {
            continue;
        };
        let Some(version) = version.strip_prefix('v') else {
            continue;
        };
        let rest = words.collect::<Vec<_>>().join(" ");
        let rest = rest.as_str();

        let candidates = by_name_version
            .get(&(name, version.to_owned()))
            .map(Vec::as_slice)
            .unwrap_or_default();
        let package = match candidates {
            [] => continue,
            [only] => *only,
            many => {
                let matching: Vec<_> = many
                    .iter()
                    .filter(|candidate| source_matches(candidate, rest))
                    .collect();
                if let [only] = matching.as_slice() {
                    **only
                } else {
                    parsed.ambiguous.insert(format!("{name} v{version}"));
                    parsed
                        .ambiguous_ids
                        .extend(many.iter().map(|candidate| candidate.id.clone()));
                    continue;
                }
            }
        };

        let features = f
            .split(',')
            .map(str::trim)
            .filter(|feature| !feature.is_empty())
            .map(str::to_owned);
        parsed
            .features
            .entry(package.id.clone())
            .or_default()
            .extend(features);
    }
    parsed
}

/// Whether the source shown by `cargo tree` (`rest`, the text after the version) is
/// the source of `package`.
fn source_matches(package: &Package, rest: &str) -> bool {
    match &package.source {
        // Local package: `cargo tree` shows its directory.
        None => package
            .manifest_path
            .parent()
            .is_some_and(|dir| rest.contains(&format!("({dir})"))),
        Some(source) => {
            let repr = source.repr.as_str();
            if let Some(git) = repr.strip_prefix("git+") {
                // `git+URL#commit`; `cargo tree` shows `(URL#short-commit)`.
                let url = git.split_once('#').map(|(url, _)| url).unwrap_or(git);
                rest.contains(url)
            } else {
                // Registry: shown without a source for crates.io, or as `(registry ...)`.
                !rest.contains("(/") && !rest.contains("://")
            }
        }
    }
}

/// Split per-platform features into the part every platform shares and the
/// per-platform remainder, in the shape of the prelude's `platform` attribute:
/// `{platform: {"features": [...]}}`.
pub fn split_features(
    platform_features: &BTreeMap<String, BTreeSet<String>>,
) -> (BTreeSet<String>, PlatformAttrs) {
    let mut sets = platform_features.values();
    let Some(first) = sets.next() else {
        return (BTreeSet::new(), BTreeMap::new());
    };
    let common: BTreeSet<String> = sets.fold(first.clone(), |acc, set| {
        acc.intersection(set).cloned().collect()
    });
    let platform = platform_features
        .iter()
        .filter_map(|(name, features)| {
            let extra: BTreeSet<String> = features.difference(&common).cloned().collect();
            (!extra.is_empty()).then(|| {
                (
                    name.clone(),
                    BTreeMap::from([("features".to_owned(), extra)]),
                )
            })
        })
        .collect();
    (common, platform)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn metadata_packages(dir: &std::path::Path) -> HashMap<PackageId, Package> {
        let metadata = cargo_metadata::MetadataCommand::new()
            .manifest_path(dir.join("Cargo.toml"))
            .other_options(vec!["--offline".to_owned()])
            .exec()
            .expect("cargo metadata");
        metadata
            .packages
            .into_iter()
            .map(|p| (p.id.clone(), p))
            .collect()
    }

    fn write(path: std::path::PathBuf, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// `app` enables `sdk/browser` only for wasm32 and `fixtures/test_lib` only as a
    /// dev-dependency; `cargo metadata` reports both features unconditionally.
    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root.join("Cargo.toml"),
            "[workspace]\nresolver = \"2\"\nmembers = [\"app\", \"sdk\", \"fixtures\"]\n",
        );
        write(
            root.join("app/Cargo.toml"),
            r#"[package]
name = "app"
version = "0.1.0"
edition = "2021"
[dependencies]
sdk = { path = "../sdk" }
[target.'cfg(target_arch = "wasm32")'.dependencies]
sdk = { path = "../sdk", features = ["browser"] }
[dev-dependencies]
fixtures = { path = "../fixtures", features = ["test_lib"] }
"#,
        );
        write(
            root.join("sdk/Cargo.toml"),
            "[package]\nname = \"sdk\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[features]\ndefault = []\nbrowser = []\n",
        );
        write(
            root.join("fixtures/Cargo.toml"),
            "[package]\nname = \"fixtures\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[features]\ntest_lib = []\n",
        );
        for member in ["app", "sdk", "fixtures"] {
            write(root.join(member).join("src/lib.rs"), "");
        }
        dir
    }

    #[test]
    fn test_cargo_tree_resolves_target_specific_features_per_platform() {
        let dir = workspace();
        let packages = metadata_packages(dir.path());
        let sdk = packages
            .values()
            .find(|p| p.name.as_str() == "sdk")
            .unwrap()
            .id
            .clone();
        let fixtures = packages
            .values()
            .find(|p| p.name.as_str() == "fixtures")
            .unwrap()
            .id
            .clone();

        let platforms = BTreeMap::from([
            (
                "linux-x86_64".to_owned(),
                "x86_64-unknown-linux-gnu".to_owned(),
            ),
            ("wasm32".to_owned(), "wasm32-unknown-unknown".to_owned()),
        ]);
        let manifest = dir.path().join("Cargo.toml");
        let resolved =
            resolve_platform_features(Some(manifest.to_str().unwrap()), &platforms, &packages)
                .expect("resolve");

        assert_eq!(resolved[&sdk]["linux-x86_64"], set(&["default"]));
        assert_eq!(resolved[&sdk]["wasm32"], set(&["browser", "default"]));
        // Dev-dependency features of workspace members are kept, as `cargo test` does.
        assert_eq!(resolved[&fixtures]["linux-x86_64"], set(&["test_lib"]));

        let (common, platform) = split_features(&resolved[&sdk]);
        assert_eq!(common, set(&["default"]));
        assert_eq!(
            platform,
            BTreeMap::from([(
                "wasm32".to_owned(),
                BTreeMap::from([("features".to_owned(), set(&["browser"]))])
            )])
        );
    }

    #[test]
    fn test_parse_cargo_tree_lines() {
        let dir = workspace();
        let packages = metadata_packages(dir.path());
        let sdk_dir = dir.path().join("sdk");
        let output = format!(
            "sdk v0.1.0 ({dir})|browser,default\n\nsdk v0.1.0 ({dir}) (*)|default\nunknown v9.9.9|x\n",
            dir = sdk_dir.display()
        );
        let parsed = parse_cargo_tree(&output, &packages);
        let sdk = packages
            .values()
            .find(|p| p.name.as_str() == "sdk")
            .unwrap()
            .id
            .clone();
        assert_eq!(parsed.features[&sdk], set(&["browser", "default"]));
        assert_eq!(parsed.features.len(), 1);
        assert!(parsed.ambiguous.is_empty());
    }

    #[test]
    fn test_split_features() {
        let per_platform = BTreeMap::from([
            ("linux-x86_64".to_owned(), set(&["default", "std"])),
            ("wasm32".to_owned(), set(&["browser", "default", "std"])),
            ("macos-arm64".to_owned(), set(&["default", "std"])),
        ]);
        let (common, platform) = split_features(&per_platform);
        assert_eq!(common, set(&["default", "std"]));
        assert_eq!(platform.len(), 1);
        assert_eq!(platform["wasm32"]["features"], set(&["browser"]));
    }
}
