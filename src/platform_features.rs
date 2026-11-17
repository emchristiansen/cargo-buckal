//! Per-platform feature resolution.
//!
//! `cargo metadata` reports, for every package, the union of the features enabled
//! on any platform, even with `--filter-platform`. A feature that a dependency
//! enables only under `[target.'cfg(...)'.dependencies]` (e.g. `browser` for a
//! wasm32 build) therefore ends up in the native build too, which can break it.
//!
//! `cargo tree --target <triple>` runs Cargo's own feature resolver for that one
//! target, so it reports the features Cargo would actually compile with there, and
//! the dependency edges those features activate. This module asks it once per
//! configured platform. The edges matter as much as the features: an optional
//! dependency that only a platform-specific feature enables (e.g. a derive crate
//! behind a `derive` feature) is in `cargo metadata`'s merged resolve, but Cargo
//! does not build it, or the features it needs from its own dependencies, on the
//! other platforms.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::process::Command;

use anyhow::{Context, Result, bail};
use cargo_metadata::{Package, PackageId};
use serde::{Deserialize, Serialize};

use crate::buckal_warn;

/// The prelude `platform` attribute: `{platform name: {attribute: values}}`.
pub type PlatformAttrs = BTreeMap<String, BTreeMap<String, BTreeSet<String>>>;

/// What Cargo resolves for one package on each configured platform where it is built.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformResolution {
    /// Platform name -> enabled features.
    pub features: BTreeMap<String, BTreeSet<String>>,
    /// Platform name -> package ids (`PackageId::repr`) of its dependencies there.
    pub deps: BTreeMap<String, BTreeSet<String>>,
}

impl PlatformResolution {
    pub fn is_empty(&self) -> bool {
        self.features.is_empty() && self.deps.is_empty()
    }

    /// The configured platforms on which this package depends on `dep`, or `None`
    /// when there is no per-platform data.
    pub fn platforms_with_dep(&self, dep: &PackageId) -> Option<BTreeSet<&str>> {
        if self.deps.is_empty() {
            return None;
        }
        Some(
            self.deps
                .iter()
                .filter(|(_, deps)| deps.contains(&dep.repr))
                .map(|(platform, _)| platform.as_str())
                .collect(),
        )
    }
}

/// Per-platform resolution per package.
pub type PlatformFeatures = HashMap<PackageId, PlatformResolution>;

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
            let deps = parsed.deps.get(&id).cloned().unwrap_or_default();
            let entry = result.entry(id).or_default();
            entry.features.insert(platform.clone(), features);
            entry.deps.insert(platform.clone(), deps);
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
        "depth",
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
    /// Dependencies (`PackageId::repr`) of each package listed, from the tree's edges.
    pub deps: HashMap<PackageId, BTreeSet<String>>,
}

/// Parse `cargo tree --prefix depth --format '{p}|{f}'` output.
///
/// Each line starts with its depth in the tree; a package's parent is the nearest
/// earlier line one level up, and a blank line starts the next workspace member. `{p}` is `name vX.Y.Z`, optionally followed by `(proc-macro)` and by the source in
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
    // Package at each depth along the current path; `None` for an unattributed line.
    let mut path: Vec<Option<PackageId>> = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        let Ok(depth) = line[..digits].parse::<usize>() else {
            path.clear();
            continue;
        };
        path.truncate(depth);
        path.resize(depth, None);
        let line = &line[digits..];
        let line = line.strip_suffix("(*)").map(str::trim_end).unwrap_or(line);
        let Some((p, f)) = line.rsplit_once('|') else {
            path.push(None);
            continue;
        };
        let mut words = p.split_whitespace();
        let (Some(name), Some(version)) = (words.next(), words.next()) else {
            path.push(None);
            continue;
        };
        let Some(version) = version.strip_prefix('v') else {
            path.push(None);
            continue;
        };
        let rest = words.collect::<Vec<_>>().join(" ");
        let rest = rest.as_str();

        let candidates = by_name_version
            .get(&(name, version.to_owned()))
            .map(Vec::as_slice)
            .unwrap_or_default();
        let package = match candidates {
            [] => {
                path.push(None);
                continue;
            }
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
                    path.push(None);
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
        parsed.deps.entry(package.id.clone()).or_default();
        if let Some(Some(parent)) = depth.checked_sub(1).and_then(|up| path.get(up)) {
            parsed
                .deps
                .entry(parent.clone())
                .or_default()
                .insert(package.id.repr.clone());
        }
        path.push(Some(package.id.clone()));
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
            "[workspace]\nresolver = \"2\"\nmembers = [\"app\", \"sdk\", \"fixtures\", \"extra\"]\n",
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
            "[package]\nname = \"sdk\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nextra = { path = \"../extra\", optional = true }\n[features]\ndefault = []\nbrowser = [\"dep:extra\"]\n",
        );
        write(
            root.join("fixtures/Cargo.toml"),
            "[package]\nname = \"fixtures\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[features]\ntest_lib = []\n",
        );
        write(
            root.join("extra/Cargo.toml"),
            "[package]\nname = \"extra\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        for member in ["app", "sdk", "fixtures", "extra"] {
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

        assert_eq!(resolved[&sdk].features["linux-x86_64"], set(&["default"]));
        assert_eq!(
            resolved[&sdk].features["wasm32"],
            set(&["browser", "default"])
        );
        // Dev-dependency features of workspace members are kept, as `cargo test` does.
        assert_eq!(
            resolved[&fixtures].features["linux-x86_64"],
            set(&["test_lib"])
        );

        // The optional dependency that only `browser` enables is an edge on wasm32 only.
        let extra = packages
            .values()
            .find(|p| p.name.as_str() == "extra")
            .unwrap()
            .id
            .clone();
        assert!(resolved[&sdk].deps["linux-x86_64"].is_empty());
        assert_eq!(
            resolved[&sdk].deps["wasm32"],
            BTreeSet::from([extra.repr.clone()])
        );
        assert_eq!(
            resolved[&sdk].platforms_with_dep(&extra),
            Some(BTreeSet::from(["wasm32"]))
        );

        let (common, platform) = split_features(&resolved[&sdk].features);
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
        let extra_dir = dir.path().join("extra");
        let output = format!(
            "0sdk v0.1.0 ({dir})|browser,default\n1unknown v9.9.9|x\n2extra v0.1.0 ({extra})|\n1extra v0.1.0 ({extra})|\n\n0sdk v0.1.0 ({dir}) (*)|default\n",
            dir = sdk_dir.display(),
            extra = extra_dir.display()
        );
        let parsed = parse_cargo_tree(&output, &packages);
        let sdk = packages
            .values()
            .find(|p| p.name.as_str() == "sdk")
            .unwrap()
            .id
            .clone();
        let extra = packages
            .values()
            .find(|p| p.name.as_str() == "extra")
            .unwrap()
            .id
            .clone();
        assert_eq!(parsed.features[&sdk], set(&["browser", "default"]));
        assert_eq!(parsed.features.len(), 2);
        assert!(parsed.ambiguous.is_empty());
        // `extra` under the unattributed `unknown` line is not taken as sdk's dependency;
        // the one directly below sdk is.
        assert_eq!(parsed.deps[&sdk], BTreeSet::from([extra.repr.clone()]));
        assert!(parsed.deps[&extra].is_empty());
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
