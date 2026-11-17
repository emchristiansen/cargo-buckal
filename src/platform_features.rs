//! Per-platform, per-context feature resolution.
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
//!
//! Cargo's resolver 2 also resolves the host context separately: build
//! dependencies, proc-macros and everything below them are compiled for the build
//! machine with their own feature set, so a feature a build script's dependency
//! needs (e.g. `serde_json/preserve_order` for a `[build-dependencies]` entry)
//! does not reach the crate the target links. A package built in both contexts
//! with different resolutions gets a separate host variant.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::process::Command;

use anyhow::{Context, Result, bail};
use cargo_metadata::{Package, PackageId};
use serde::{Deserialize, Serialize};

use crate::buckal_warn;

/// The prelude `platform` attribute: `{platform name: {attribute: values}}`.
pub type PlatformAttrs = BTreeMap<String, BTreeMap<String, BTreeSet<String>>>;

/// Platform name -> enabled features.
pub type FeaturesByPlatform = BTreeMap<String, BTreeSet<String>>;
/// Platform name -> package ids (`PackageId::repr`) of the dependencies there.
pub type DepsByPlatform = BTreeMap<String, BTreeSet<String>>;

/// What Cargo resolves for one package on each configured platform where it is built.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformResolution {
    /// Features of the package's rules, per platform: the target context where the
    /// package is built for the target, else the host context.
    pub features: FeaturesByPlatform,
    /// Dependencies of the package's rules, per platform, in the same context.
    pub deps: DepsByPlatform,
    /// The package is built for the host only (a proc-macro, a build dependency or
    /// below one), so its rules are host-context rules.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub host_context: bool,
    /// The package is built in both contexts and resolves differently in the host
    /// context (or depends on a package that does): the host variant's resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_variant: Option<HostVariant>,
    /// Dependencies (`PackageId::repr`) that have a host variant. Host-context rules of
    /// this package (build scripts, host-only rules, its own host variant) use it.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub host_variant_deps: BTreeSet<String>,
}

/// The host-context resolution of a package that also builds for the target.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostVariant {
    pub features: FeaturesByPlatform,
    pub deps: DepsByPlatform,
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

    /// The resolution of the host variant's rules, if the package has one.
    pub fn host_variant_resolution(&self) -> Option<PlatformResolution> {
        self.host_variant.as_ref().map(|host| PlatformResolution {
            features: host.features.clone(),
            deps: host.deps.clone(),
            host_context: true,
            host_variant: None,
            host_variant_deps: self.host_variant_deps.clone(),
        })
    }
}

/// Per-platform resolution per package.
pub type PlatformFeatures = HashMap<PackageId, PlatformResolution>;

/// One context's resolution of a package, per platform.
#[derive(Debug, Default, Clone, PartialEq)]
struct ContextResolution {
    features: FeaturesByPlatform,
    deps: DepsByPlatform,
}

impl ContextResolution {
    fn is_empty(&self) -> bool {
        self.features.is_empty()
    }
}

/// Resolve the features of every package for each `(platform name, target triple)`.
///
/// Edges are normal, build and dev (dev only for workspace members), which is the
/// resolution `cargo test --workspace --target <triple>` compiles with.
pub fn resolve_platform_features(
    manifest_path: Option<&str>,
    platforms: &BTreeMap<String, String>,
    packages: &HashMap<PackageId, Package>,
) -> Result<PlatformFeatures> {
    let mut target: HashMap<PackageId, ContextResolution> = HashMap::new();
    let mut host: HashMap<PackageId, ContextResolution> = HashMap::new();
    let mut ambiguous_ids = BTreeSet::new();
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
        for (context, into) in [(parsed.target, &mut target), (parsed.host, &mut host)] {
            for (id, features) in context.features {
                let deps = context.deps.get(&id).cloned().unwrap_or_default();
                let entry = into.entry(id).or_default();
                entry.features.insert(platform.clone(), features);
                entry.deps.insert(platform.clone(), deps);
            }
        }
        ambiguous_ids.extend(parsed.ambiguous_ids);
    }
    for id in &ambiguous_ids {
        target.remove(id);
        host.remove(id);
    }
    Ok(combine_contexts(target, host))
}

/// Combine the target- and host-context resolutions into each package's rules: the
/// target context where the package is built for the target, the host context
/// otherwise, and a host variant where a package built in both contexts resolves
/// differently in the host context, or depends there on a package with a host
/// variant (whose host-context dependents must then use the variant too).
fn combine_contexts(
    mut target: HashMap<PackageId, ContextResolution>,
    mut host: HashMap<PackageId, ContextResolution>,
) -> PlatformFeatures {
    let differs = |t: &ContextResolution, h: &ContextResolution| {
        h.features.iter().any(|(platform, features)| {
            t.features.get(platform) != Some(features)
                || t.deps.get(platform) != h.deps.get(platform)
        })
    };
    let mut variants: BTreeSet<String> = target
        .iter()
        .filter_map(|(id, t)| {
            let h = host.get(id)?;
            (!t.is_empty() && !h.is_empty() && differs(t, h)).then(|| id.repr.clone())
        })
        .collect();
    loop {
        let more: Vec<String> = target
            .keys()
            .filter(|id| !variants.contains(&id.repr))
            .filter(|id| {
                host.get(*id)
                    .is_some_and(|h| h.deps.values().flatten().any(|dep| variants.contains(dep)))
            })
            .map(|id| id.repr.clone())
            .collect();
        if more.is_empty() {
            break;
        }
        variants.extend(more);
    }

    let ids: BTreeSet<PackageId> = target.keys().chain(host.keys()).cloned().collect();
    let mut result = PlatformFeatures::new();
    for id in ids {
        let t = target.remove(&id).filter(|t| !t.is_empty());
        let h = host.remove(&id).filter(|h| !h.is_empty());
        let all_deps = t
            .iter()
            .chain(h.iter())
            .flat_map(|c| c.deps.values().flatten());
        let host_variant_deps = all_deps
            .filter(|dep| variants.contains(*dep))
            .cloned()
            .collect();
        let resolution = match (t, h) {
            (Some(t), h) => PlatformResolution {
                host_variant: h
                    .filter(|_| variants.contains(&id.repr))
                    .map(|h| HostVariant {
                        features: h.features,
                        deps: h.deps,
                    }),
                features: t.features,
                deps: t.deps,
                host_context: false,
                host_variant_deps,
            },
            (None, Some(h)) => PlatformResolution {
                features: h.features,
                deps: h.deps,
                host_context: true,
                host_variant: None,
                host_variant_deps,
            },
            (None, None) => continue,
        };
        result.insert(id, resolution);
    }
    result
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
        "--charset",
        "ascii",
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

/// Features and dependencies listed for packages in one context.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct ContextTree {
    /// Union of the features reported on every line for a package in this context.
    pub features: HashMap<PackageId, BTreeSet<String>>,
    /// Dependencies (`PackageId::repr`) of each package listed, from the tree's edges.
    pub deps: HashMap<PackageId, BTreeSet<String>>,
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct ParsedTree {
    /// Packages as compiled for the target.
    pub target: ContextTree,
    /// Packages as compiled for the host: build dependencies, proc-macros and all
    /// their dependencies.
    pub host: ContextTree,
    /// Packages whose lines could not be matched to exactly one metadata package.
    pub ambiguous: BTreeSet<String>,
    /// Metadata packages that a line might have meant; their result is unreliable.
    pub ambiguous_ids: BTreeSet<PackageId>,
}

/// Parse `cargo tree --charset ascii --format '{p}|{f}'` output.
///
/// Each line is indented by four characters per level (`|-- `, `` `-- ``, `|   `,
/// four spaces); a package's parent is the nearest earlier line one level up, and a
/// blank line starts the next workspace member. A `[build-dependencies]` line marks
/// the following children of the same parent as build dependencies, until a
/// `[dev-dependencies]` line. A package is in the host context when it is a build
/// dependency, a proc-macro, or below either. `{p}` is `name vX.Y.Z`, optionally
/// followed by `(proc-macro)` and by the source in parentheses (a path for local
/// packages, a URL for git ones); a line repeated because of deduplication ends in
/// ` (*)`. `{f}` is a comma-separated feature list.
pub(crate) fn parse_cargo_tree(output: &str, packages: &HashMap<PackageId, Package>) -> ParsedTree {
    let mut by_name_version: HashMap<(&str, String), Vec<&Package>> = HashMap::new();
    for package in packages.values() {
        by_name_version
            .entry((package.name.as_str(), package.version.to_string()))
            .or_default()
            .push(package);
    }

    let mut parsed = ParsedTree::default();
    // Package and its context (host or not) at each depth along the current path;
    // `None` for an unattributed line.
    let mut path: Vec<(Option<PackageId>, bool)> = Vec::new();
    // Whether the children at each depth currently are build dependencies.
    let mut build_edges: Vec<bool> = Vec::new();
    for line in output.lines() {
        let indent = line.len() - line.trim_start_matches(['|', '`', '-', ' ']).len();
        let content = line[indent..].trim_end();
        if content.is_empty() {
            path.clear();
            build_edges.clear();
            continue;
        }
        let depth = indent / 4;
        if content.starts_with('[') {
            // `[build-dependencies]` / `[dev-dependencies]` for children at depth + 1.
            build_edges.resize(build_edges.len().max(depth + 2), false);
            build_edges[depth + 1] = content == "[build-dependencies]";
            continue;
        }
        path.truncate(depth);
        path.resize(depth, (None, false));
        build_edges.resize(build_edges.len().max(depth + 2), false);
        let parent_host = depth
            .checked_sub(1)
            .and_then(|up| path.get(up))
            .is_some_and(|p| p.1);
        let host =
            parent_host || (depth > 0 && build_edges[depth]) || content.contains(" (proc-macro)");
        // The children of this line start as normal dependencies.
        build_edges.truncate(depth + 1);
        build_edges.push(false);

        let content = content
            .strip_suffix("(*)")
            .map(str::trim_end)
            .unwrap_or(content);
        let Some((p, f)) = content.rsplit_once('|') else {
            path.push((None, host));
            continue;
        };
        let mut words = p.split_whitespace();
        let (Some(name), Some(version)) = (words.next(), words.next()) else {
            path.push((None, host));
            continue;
        };
        let Some(version) = version.strip_prefix('v') else {
            path.push((None, host));
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
                path.push((None, host));
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
                    path.push((None, host));
                    continue;
                }
            }
        };

        let features = f
            .split(',')
            .map(str::trim)
            .filter(|feature| !feature.is_empty())
            .map(str::to_owned);
        let context = if host {
            &mut parsed.host
        } else {
            &mut parsed.target
        };
        context
            .features
            .entry(package.id.clone())
            .or_default()
            .extend(features);
        context.deps.entry(package.id.clone()).or_default();
        if let Some((Some(parent), parent_host)) = depth.checked_sub(1).and_then(|up| path.get(up))
        {
            let parent_context = if *parent_host {
                &mut parsed.host
            } else {
                &mut parsed.target
            };
            parent_context
                .deps
                .entry(parent.clone())
                .or_default()
                .insert(package.id.repr.clone());
        }
        path.push((Some(package.id.clone()), host));
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
        let fixtures_dir = dir.path().join("fixtures");
        let output = format!(
            "sdk v0.1.0 ({dir})|browser,default\n\
             |-- unknown v9.9.9|x\n\
             |   `-- extra v0.1.0 ({extra})|\n\
             `-- extra v0.1.0 ({extra})|\n\
             [build-dependencies]\n\
             `-- fixtures v0.1.0 ({fixtures})|test_lib\n\
             \n\
             sdk v0.1.0 ({dir}) (*)|default\n",
            dir = sdk_dir.display(),
            extra = extra_dir.display(),
            fixtures = fixtures_dir.display(),
        );
        let parsed = parse_cargo_tree(&output, &packages);
        let id = |name: &str| {
            packages
                .values()
                .find(|p| p.name.as_str() == name)
                .unwrap()
                .id
                .clone()
        };
        let (sdk, extra, fixtures) = (id("sdk"), id("extra"), id("fixtures"));
        assert_eq!(parsed.target.features[&sdk], set(&["browser", "default"]));
        assert_eq!(parsed.target.features.len(), 2);
        assert!(parsed.ambiguous.is_empty());
        // `extra` under the unattributed `unknown` line is not taken as sdk's dependency;
        // the one directly below sdk is. The build dependency is sdk's dependency too,
        // but in the host context.
        assert_eq!(
            parsed.target.deps[&sdk],
            BTreeSet::from([extra.repr.clone(), fixtures.repr.clone()])
        );
        assert!(parsed.target.deps[&extra].is_empty());
        assert_eq!(parsed.host.features[&fixtures], set(&["test_lib"]));
        assert!(!parsed.target.features.contains_key(&fixtures));
    }

    fn package_manifest(name: &str, body: &str) -> String {
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{body}")
    }

    /// `app` uses `json` plainly, and its build script uses `gen`, which needs
    /// `json/preserve_order`: Cargo compiles `json` twice, and only the host copy has
    /// `preserve_order`. `leaf` (below `json`) resolves identically in both contexts.
    /// Only `app` is a workspace member (members are all built for the target); the
    /// others are path dependencies outside the workspace, like registry crates.
    fn host_workspace() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        let crates = dir.path().join("crates");
        write(
            ws.join("Cargo.toml"),
            "[workspace]\nresolver = \"2\"\nmembers = [\"app\"]\n",
        );
        write(
            ws.join("app/Cargo.toml"),
            &package_manifest(
                "app",
                "[dependencies]\njson = { path = \"../../crates/json\" }\npm = { path = \"../../crates/pm\" }\n[build-dependencies]\ngen = { path = \"../../crates/gen\" }\n",
            ),
        );
        write(ws.join("app/src/lib.rs"), "");
        write(ws.join("app/build.rs"), "fn main() {}\n");
        write(
            crates.join("gen/Cargo.toml"),
            &package_manifest(
                "gen",
                "[dependencies]\njson = { path = \"../json\", features = [\"preserve_order\"] }\n",
            ),
        );
        write(
            crates.join("json/Cargo.toml"),
            &package_manifest(
                "json",
                "[dependencies]\nleaf = { path = \"../leaf\" }\n[features]\npreserve_order = []\n",
            ),
        );
        write(
            crates.join("leaf/Cargo.toml"),
            &package_manifest("leaf", ""),
        );
        write(
            crates.join("pm/Cargo.toml"),
            &package_manifest(
                "pm",
                "[lib]\nproc-macro = true\n[dependencies]\njson = { path = \"../json\" }\n",
            ),
        );
        for name in ["gen", "json", "leaf", "pm"] {
            write(crates.join(name).join("src/lib.rs"), "");
        }
        (dir, ws)
    }

    #[test]
    fn test_host_context_resolves_separately() {
        let (_dir, ws) = host_workspace();
        let packages = metadata_packages(&ws);
        let id = |name: &str| {
            packages
                .values()
                .find(|p| p.name.as_str() == name)
                .unwrap()
                .id
                .clone()
        };
        let platforms = BTreeMap::from([(
            "linux-x86_64".to_owned(),
            "x86_64-unknown-linux-gnu".to_owned(),
        )]);
        let manifest = ws.join("Cargo.toml");
        let resolved =
            resolve_platform_features(Some(manifest.to_str().unwrap()), &platforms, &packages)
                .expect("resolve");

        // The target copy of `json` does not get the build dependency's feature.
        let json = &resolved[&id("json")];
        assert_eq!(json.features["linux-x86_64"], set(&[]));
        assert!(!json.host_context);
        let host = json.host_variant.as_ref().expect("json has a host variant");
        assert_eq!(host.features["linux-x86_64"], set(&["preserve_order"]));

        // `leaf` resolves the same in both contexts: one shared rule.
        assert!(resolved[&id("leaf")].host_variant.is_none());
        // `gen` and the proc-macro `pm` are built for the host only, and use json's variant.
        for name in ["gen", "pm"] {
            let r = &resolved[&id(name)];
            assert!(r.host_context, "{name}");
            assert!(r.host_variant_deps.contains(&id("json").repr), "{name}");
        }
        // `app` is target-only. It records json's variant; only its host-context rules
        // (the build script, whose only dependency is `gen`) would use it.
        let app = &resolved[&id("app")];
        assert!(!app.host_context && app.host_variant.is_none());
        assert_eq!(app.features["linux-x86_64"], set(&[]));
    }

    #[test]
    fn test_host_variant_propagates_to_shared_dependents() {
        let pid = |name: &str| PackageId {
            repr: format!("registry+https://github.com/rust-lang/crates.io-index#{name}@1.0.0"),
        };
        let res = |features: &[&str], deps: &[&str]| ContextResolution {
            features: BTreeMap::from([("linux".to_owned(), set(features))]),
            deps: BTreeMap::from([(
                "linux".to_owned(),
                deps.iter().map(|d| pid(d).repr).collect(),
            )]),
        };
        // `mid` resolves the same in both contexts, but depends on `json`, which does not.
        let target = HashMap::from([
            (pid("json"), res(&["std"], &[])),
            (pid("mid"), res(&["default"], &["json"])),
            (pid("top"), res(&[], &["mid"])),
        ]);
        let host = HashMap::from([
            (pid("json"), res(&["preserve_order", "std"], &[])),
            (pid("mid"), res(&["default"], &["json"])),
        ]);
        let combined = combine_contexts(target, host);
        assert!(combined[&pid("json")].host_variant.is_some());
        let mid = &combined[&pid("mid")];
        assert_eq!(
            mid.host_variant,
            Some(HostVariant {
                features: BTreeMap::from([("linux".to_owned(), set(&["default"]))]),
                deps: BTreeMap::from([("linux".to_owned(), BTreeSet::from([pid("json").repr]))]),
            })
        );
        assert!(mid.host_variant_deps.contains(&pid("json").repr));
        // `top` is target-only: no variant, but it records that `mid` has one.
        assert!(combined[&pid("top")].host_variant.is_none());
        assert_eq!(
            combined[&pid("top")].host_variant_deps,
            BTreeSet::from([pid("mid").repr])
        );
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
