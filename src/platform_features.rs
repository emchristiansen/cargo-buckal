//! Per-platform, per-context feature resolution.
//!
//! `cargo metadata` reports, for every package, the union of the features enabled
//! on any platform, even with `--filter-platform`. A feature that a dependency
//! enables only under `[target.'cfg(...)'.dependencies]` (e.g. `browser` for a
//! wasm32 build) therefore ends up in the native build too, which can break it.
//!
//! Cargo's unit graph (`cargo test --workspace --no-run --target <triple>
//! --unit-graph`) is the compilation plan Cargo itself would run for that target:
//! every unit with its resolved features and dependencies. This module reads it
//! once per configured platform. The edges matter as much as the features: an
//! optional dependency that only a platform-specific feature enables (e.g. a derive
//! crate behind a `derive` feature) is in `cargo metadata`'s merged resolve, but
//! Cargo does not build it, or the features it needs from its own dependencies, on
//! the other platforms.
//!
//! Cargo's resolver 2 also resolves the host context separately: build
//! dependencies, proc-macros and everything below them are compiled for the build
//! machine (units without a `platform`) with their own feature set, so a feature a
//! build script's dependency needs (e.g. `serde_json/preserve_order` for a
//! `[build-dependencies]` entry) does not reach the crate the target links. A
//! package built in both contexts with different resolutions gets a separate host
//! variant.
//!
//! `--unit-graph` is an unstable Cargo flag, so the command runs with
//! `RUSTC_BOOTSTRAP=1`; it only plans, it compiles and runs nothing. (`cargo tree`
//! was used before, but it shows some dev-dependency subtrees of workspace members
//! with host-context features, so its output cannot be split into the two
//! contexts reliably.)

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::process::Command;

use anyhow::{Context, Result, bail};
use cargo_metadata::{Package, PackageId};
use serde::{Deserialize, Serialize};

/// One value of the prelude's `platform` attribute: a list (`features`, `deps`) or a
/// dict (`named_deps`, alias to label).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum PlatformValue {
    List(BTreeSet<String>),
    Dict(BTreeMap<String, String>),
}

impl PlatformValue {
    /// The list, if this is one.
    pub fn as_list(&self) -> Option<&BTreeSet<String>> {
        match self {
            PlatformValue::List(list) => Some(list),
            PlatformValue::Dict(_) => None,
        }
    }

    /// The dict, if this is one.
    pub fn as_dict(&self) -> Option<&BTreeMap<String, String>> {
        match self {
            PlatformValue::Dict(dict) => Some(dict),
            PlatformValue::List(_) => None,
        }
    }
}

/// The prelude `platform` attribute: `{platform name: {attribute: value}}`.
pub type PlatformAttrs = BTreeMap<String, BTreeMap<String, PlatformValue>>;

/// The list `attr` of `platform` in `attrs`, created if absent.
pub fn platform_list_mut<'a>(
    attrs: &'a mut PlatformAttrs,
    platform: &str,
    attr: &str,
) -> &'a mut BTreeSet<String> {
    let value = attrs
        .entry(platform.to_owned())
        .or_default()
        .entry(attr.to_owned())
        .or_insert_with(|| PlatformValue::List(BTreeSet::new()));
    match value {
        PlatformValue::List(list) => list,
        PlatformValue::Dict(_) => panic!("platform attribute `{attr}` is a dict, not a list"),
    }
}

/// The dict `attr` of `platform` in `attrs`, created if absent.
pub fn platform_dict_mut<'a>(
    attrs: &'a mut PlatformAttrs,
    platform: &str,
    attr: &str,
) -> &'a mut BTreeMap<String, String> {
    let value = attrs
        .entry(platform.to_owned())
        .or_default()
        .entry(attr.to_owned())
        .or_insert_with(|| PlatformValue::Dict(BTreeMap::new()));
    match value {
        PlatformValue::Dict(dict) => dict,
        PlatformValue::List(_) => panic!("platform attribute `{attr}` is a list, not a dict"),
    }
}

/// Platform name -> enabled features.
pub type FeaturesByPlatform = BTreeMap<String, BTreeSet<String>>;
/// Platform name -> package ids (`PackageId::repr`) of the dependencies there.
pub type DepsByPlatform = BTreeMap<String, BTreeSet<String>>;

/// Which of a package's compilations a dependency edge belongs to. Cargo activates
/// each kind of edge separately: an optional normal dependency can be off where the
/// same package is a build dependency, and the reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeRole {
    /// The library and binaries.
    Normal,
    /// The build script.
    Build,
    /// Tests and examples (normal and dev-dependencies).
    Dev,
}

/// A package's dependency edges per platform, by role.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edges {
    pub normal: DepsByPlatform,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub build: DepsByPlatform,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dev: DepsByPlatform,
}

impl Edges {
    pub fn role(&self, role: EdgeRole) -> &DepsByPlatform {
        match role {
            EdgeRole::Normal => &self.normal,
            EdgeRole::Build => &self.build,
            EdgeRole::Dev => &self.dev,
        }
    }

    fn role_mut(&mut self, role: EdgeRole) -> &mut DepsByPlatform {
        match role {
            EdgeRole::Normal => &mut self.normal,
            EdgeRole::Build => &mut self.build,
            EdgeRole::Dev => &mut self.dev,
        }
    }

    /// Every dependency, of any role on any platform.
    pub fn all(&self) -> impl Iterator<Item = &String> {
        [&self.normal, &self.build, &self.dev]
            .into_iter()
            .flat_map(|by_platform| by_platform.values().flatten())
    }
}

/// What Cargo resolves for one package on each configured platform where it is built.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformResolution {
    /// Features of the package's rules, per platform: the target context where the
    /// package is built for the target, else the host context. Its keys are the
    /// platforms the package is built on.
    pub features: FeaturesByPlatform,
    /// Dependency edges of the package's rules, per platform and role, in the same
    /// context.
    pub deps: Edges,
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
    /// The target-context resolution of the package's executables (binaries and
    /// tests), when its library is built for the host only: a proc-macro's
    /// integration test runs on the target, while the proc-macro itself (and so the
    /// package's main resolution above) is a host compilation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_context: Option<HostVariant>,
    /// With a separate test resolution (`test_features`): the library as the tests
    /// need it, where that differs from the ordinary build's (or depends on a
    /// library that does). Emitted as the `<lib>-test` variant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_variant: Option<HostVariant>,
    /// Dependencies (`PackageId::repr`) that have a test variant. Test-context rules
    /// of this package (its tests, test-only executables, its own test variant) use
    /// it.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub test_variant_deps: BTreeSet<String>,
    /// With a separate test resolution: the resolution of the package's tests (and of
    /// binaries only the tests' features enable), where it differs from the main one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests: Option<Box<PlatformResolution>>,
    /// These rules are in the test context: they use their dependencies' test
    /// variants. Set for a package Cargo builds only for tests.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub test_context: bool,
    /// The package as the tests' host compilations (build scripts, proc-macros and
    /// what they use) need it, where that differs from the host compilation of the
    /// ordinary build (or depends on a package that does). Emitted as the
    /// `<lib>-test-host` variant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_host_variant: Option<HostVariant>,
    /// Dependencies (`PackageId::repr`) with a test host variant, which this
    /// package's test-context host compilations (and its proc-macro dependencies of
    /// test-context rules) use.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub test_host_variant_deps: BTreeSet<String>,
    /// Dependencies with an emitted host variant (`<lib>-host`), as the test context
    /// sees them: test-context host compilations use those that have no test host
    /// variant.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub test_context_host_variant_deps: BTreeSet<String>,
    /// The package is also compiled in the host context (by the ordinary build or
    /// the tests) with no host variant of its own, so host compilations, which Buck
    /// configures for the execution platform, use these rules.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub host_uses_plain_rules: bool,
    /// With `host_uses_plain_rules`: what those host compilations need, per
    /// platform on which Cargo plans them (the ordinary build's and the tests'),
    /// for comparison with the plain rules' resolution on the execution platform.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plain_host_needs: Vec<HostVariant>,
    /// The library as each wasm32 module's own Cargo graph compiles it (`[wasm]`),
    /// by module package: emitted as the `<lib>-wasm-<module>` variants.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub wasm_variants: BTreeMap<String, WasmVariant>,
    /// Set on the resolution `<lib>-wasm-<module>` rules are emitted from: the
    /// module, whose variants their target dependencies are too. Never stored.
    #[serde(skip)]
    pub wasm_rules: Option<String>,
    /// The package's library unit tests compiled for wasm32 in each declared
    /// `wasm-test` graph (`[wasm] test_packages`), by test graph key: emitted as a
    /// test-mode module, its alias and a browser test.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub wasm_tests: BTreeMap<String, WasmTest>,
}

/// A library in a wasm32 module's own Cargo graph (target context).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WasmVariant {
    /// The `feature_platforms` name of wasm32-unknown-unknown.
    pub platform: String,
    pub features: BTreeSet<String>,
    /// Normal dependencies (target libraries of the graph, or proc-macros).
    pub normal: BTreeSet<String>,
    /// Build dependencies (host compilations).
    pub build: BTreeSet<String>,
    /// The Cargo profile's rustc flags.
    pub rustc_flags: Vec<String>,
    /// The module itself (the graph's root): built as the cdylib, with the
    /// profile's link-time flags.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub root: bool,
}

/// A package's library unit tests in one of its `wasm-test` graphs: the test-mode
/// compilation of its library root for wasm32.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WasmTest {
    /// The `feature_platforms` name of wasm32-unknown-unknown.
    pub platform: String,
    pub features: BTreeSet<String>,
    /// The test unit's dependencies (`PackageId::repr`): its normal and
    /// dev-dependencies, the package itself where Cargo links its library in.
    pub deps: BTreeSet<String>,
    /// The test unit's rustc flags, from its resolved profile.
    pub rustc_flags: Vec<String>,
    /// The test names the browser test runs (empty: the rule's default).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub labels: BTreeSet<String>,
}

/// The library unit-test unit of a package's own test graph.
#[derive(Debug, Clone, PartialEq)]
pub struct PackageTestUnit {
    pub features: BTreeSet<String>,
    /// Its dependencies (`PackageId::repr`), the package itself included when its
    /// library is linked in.
    pub deps: BTreeSet<String>,
    pub profile: UnitProfile,
}

/// A library of a package's own graph.
#[derive(Debug, Clone, PartialEq)]
pub struct PackageGraphLib {
    pub features: BTreeSet<String>,
    pub normal: BTreeSet<String>,
    pub build: BTreeSet<String>,
    /// The profile Cargo resolved for the library's unit (with the profile's
    /// per-package overrides and Cargo's defaults applied).
    pub profile: UnitProfile,
}

/// The target-context libraries of `cargo build -p <package> --target <triple>
/// --features <features> --profile <profile>`, by package.
pub fn resolve_package_graph(
    manifest_path: Option<&str>,
    packages: &HashMap<PackageId, Package>,
    package: &str,
    features: &[String],
    profile: &str,
    triple: &str,
) -> Result<HashMap<PackageId, PackageGraphLib>> {
    let output = run_unit_graph(
        manifest_path,
        triple,
        GraphShape::Package(package, features, profile),
    )
    .with_context(|| format!("failed to resolve `{package}`'s own graph for `{triple}`"))?;
    let parsed = parse_unit_graph(&output, packages)
        .with_context(|| format!("failed to read Cargo's unit graph of `{package}`"))?;
    let target = parsed.target;
    target
        .features
        .iter()
        .filter(|(id, _)| target.libs.contains(*id))
        .map(|(id, features)| {
            let deps = target.deps.get(id).cloned().unwrap_or_default();
            let profile = target
                .profiles
                .get(id)
                .cloned()
                .with_context(|| format!("Cargo's unit graph gives `{id}` no profile"))?;
            Ok((
                id.clone(),
                PackageGraphLib {
                    features: features.clone(),
                    normal: deps.role(EdgeRole::Normal).clone(),
                    build: deps.role(EdgeRole::Build).clone(),
                    profile,
                },
            ))
        })
        .collect()
}

/// The target-context libraries of `cargo test -p <package> --lib --no-run --target
/// <triple> --features <features>`, by package, and the package's library unit-test
/// unit (`None` when Cargo compiles none, as for a package without a library; `--lib`
/// names the library explicitly, so `[lib] test = false` still compiles its unit tests).
pub fn resolve_package_test_graph(
    manifest_path: Option<&str>,
    packages: &HashMap<PackageId, Package>,
    package: &PackageId,
    name: &str,
    features: &[String],
    triple: &str,
) -> Result<(HashMap<PackageId, PackageGraphLib>, Option<PackageTestUnit>)> {
    let output = run_unit_graph(
        manifest_path,
        triple,
        GraphShape::PackageTest(name, features),
    )
    .with_context(|| format!("failed to resolve `{name}`'s own test graph for `{triple}`"))?;
    let parsed = parse_unit_graph(&output, packages)
        .with_context(|| format!("failed to read Cargo's unit test graph of `{name}`"))?;
    let target = parsed.target;
    let test = match target.test_profiles.get(package) {
        None => None,
        Some(profile) => {
            let deps = target.deps.get(package).cloned().unwrap_or_default();
            Some(PackageTestUnit {
                features: target.features.get(package).cloned().unwrap_or_default(),
                deps: deps.role(EdgeRole::Dev).clone(),
                profile: profile.clone(),
            })
        }
    };
    let libs = target
        .features
        .iter()
        .filter(|(id, _)| target.libs.contains(*id))
        .map(|(id, features)| {
            let deps = target.deps.get(id).cloned().unwrap_or_default();
            let profile = target
                .profiles
                .get(id)
                .cloned()
                .with_context(|| format!("Cargo's unit graph gives `{id}` no profile"))?;
            Ok((
                id.clone(),
                PackageGraphLib {
                    features: features.clone(),
                    normal: deps.role(EdgeRole::Normal).clone(),
                    build: deps.role(EdgeRole::Build).clone(),
                    profile,
                },
            ))
        })
        .collect::<Result<_>>()?;
    Ok((libs, test))
}

/// The resolution of one more compilation context of a package: the host variant of
/// a library also built for the target, or the target context of the executables of
/// a package whose library is host-only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostVariant {
    pub features: FeaturesByPlatform,
    pub deps: Edges,
}

impl PlatformResolution {
    pub fn is_empty(&self) -> bool {
        self.features.is_empty()
    }

    /// The configured platforms on which this package is built.
    pub fn platforms(&self) -> BTreeSet<&str> {
        self.features.keys().map(String::as_str).collect()
    }

    /// The configured platforms on which this package's `role` compilation depends on
    /// `dep`, or `None` when there is no per-platform data.
    pub fn platforms_with_dep(&self, dep: &PackageId, role: EdgeRole) -> Option<BTreeSet<&str>> {
        if self.is_empty() {
            return None;
        }
        Some(
            self.deps
                .role(role)
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
            host_variant_deps: self.host_variant_deps.clone(),
            // A package only the tests build: its host variant is in the test context.
            test_context: self.test_context,
            test_variant_deps: self.test_variant_deps.clone(),
            test_host_variant_deps: self.test_host_variant_deps.clone(),
            ..Default::default()
        })
    }

    /// The labelling of a test-context resolution of this package: host variants as
    /// the test context sees them, and test and test host variants.
    fn in_test_context(&self, resolution: PlatformResolution) -> PlatformResolution {
        PlatformResolution {
            host_variant_deps: self.test_context_host_variant_deps.clone(),
            test_variant_deps: self.test_variant_deps.clone(),
            test_host_variant_deps: self.test_host_variant_deps.clone(),
            test_context: true,
            ..resolution
        }
    }

    /// The resolution of the test host variant, if the package has one.
    pub fn test_host_variant_resolution(&self) -> Option<PlatformResolution> {
        self.test_host_variant.as_ref().map(|host| {
            self.in_test_context(PlatformResolution {
                features: host.features.clone(),
                deps: host.deps.clone(),
                host_context: true,
                ..Default::default()
            })
        })
    }

    /// The resolution of the test variant's library, if the package has one.
    pub fn test_variant_resolution(&self) -> Option<PlatformResolution> {
        self.test_variant.as_ref().map(|test| {
            self.in_test_context(PlatformResolution {
                features: test.features.clone(),
                deps: test.deps.clone(),
                ..Default::default()
            })
        })
    }

    /// The resolution that the package's tests are compiled with: the test
    /// resolution where there is a separate one, else [`Self::exec_resolution`].
    pub fn test_resolution(&self) -> PlatformResolution {
        match &self.tests {
            None => self.exec_resolution(),
            Some(tests) => self.in_test_context(tests.exec_resolution()),
        }
    }

    /// The resolution that the library's inline unit tests are compiled with: the
    /// library's own context (the host for a host-only library, whose unit tests
    /// Cargo compiles for the host), from the test resolution where there is a
    /// separate one. Integration tests and binaries use [`Self::test_resolution`]
    /// and [`Self::exec_resolution`] instead.
    pub fn unit_test_resolution(&self) -> PlatformResolution {
        let own_context = |r: &PlatformResolution| PlatformResolution {
            exec_context: None,
            ..r.clone()
        };
        match &self.tests {
            None => own_context(self),
            Some(tests) => self.in_test_context(own_context(tests)),
        }
    }

    /// The resolution that the package's binaries and tests are compiled with: the
    /// target context when the library itself is host-only and the executables are
    /// not, else the package's main resolution.
    pub fn exec_resolution(&self) -> PlatformResolution {
        match &self.exec_context {
            None => self.clone(),
            Some(exec) => PlatformResolution {
                features: exec.features.clone(),
                deps: exec.deps.clone(),
                host_context: false,
                host_variant_deps: self.host_variant_deps.clone(),
                test_variant_deps: self.test_variant_deps.clone(),
                test_host_variant_deps: self.test_host_variant_deps.clone(),
                test_context: self.test_context,
                ..Default::default()
            },
        }
    }
}

/// What the plain rules of `r` build on `platform` (its features, and its normal and
/// build edges): that platform's entry, or for a platform the package has none
/// for, what every platform it has shares, which emission hoists out of
/// `platform` (Fable N10).
fn plain_rule_on(
    r: &PlatformResolution,
    platform: &str,
) -> (BTreeSet<String>, [BTreeSet<String>; 2]) {
    let edges_on = |on: &str| {
        [EdgeRole::Normal, EdgeRole::Build]
            .map(|role| r.deps.role(role).get(on).cloned().unwrap_or_default())
    };
    if let Some(features) = r.features.get(platform) {
        return (features.clone(), edges_on(platform));
    }
    let mut platforms = r.features.keys();
    let Some(first) = platforms.next() else {
        return Default::default();
    };
    platforms.fold(
        (r.features[first].clone(), edges_on(first)),
        |(features, edges), on| {
            let other = edges_on(on);
            (
                features.intersection(&r.features[on]).cloned().collect(),
                [0, 1].map(|i| edges[i].intersection(&other[i]).cloned().collect()),
            )
        },
    )
}

/// The kind of [`host_resolutions_differing_by_platform`] entry for a library
/// whose plain rules host compilations use.
pub const PLAIN_HOST_USE: &str = "plain";

/// Per-platform resolution per package.
pub type PlatformFeatures = HashMap<PackageId, PlatformResolution>;

/// Host compilations whose resolution differs between the configured platforms
/// they are built for: `(package, kind)`, the kind naming the host compilation
/// (`""` for a host-only package's own rules, `-host`, `-test-host`, or
/// [`PLAIN_HOST_USE`] for a library's plain rules that host compilations use for
/// want of a host variant, Fable N7).
///
/// Cargo plans a separate host compilation per target (each target's unit graph
/// unifies the host's features apart), and BUCKAL keys them by platform. Buck
/// configures a host compilation in the execution platform, though, so its
/// `platform` attribute selects the execution platform's entry, whatever the
/// target: the other platforms' host resolutions are not what Buck builds.
///
/// A plain rule that host compilations use is recorded, with `execution` (the
/// configured name of the execution platform), when what a host compilation needs
/// on some platform differs from the plain rule's resolution on the execution
/// platform, which Buck builds for it (Fable N7); without `execution`, whenever
/// the plain rule's resolution differs between platforms.
pub fn host_resolutions_differing_by_platform(
    resolutions: &PlatformFeatures,
    execution: Option<&str>,
) -> Vec<(PackageId, &'static str)> {
    let differs = |features: &FeaturesByPlatform, deps: &Edges| {
        let mut platforms = features.keys();
        let Some(first) = platforms.next() else {
            return false;
        };
        let deps_on = |platform: &String| {
            [EdgeRole::Normal, EdgeRole::Build]
                .map(|role| deps.role(role).get(platform).cloned().unwrap_or_default())
        };
        platforms.any(|platform| {
            features.get(platform) != features.get(first) || deps_on(platform) != deps_on(first)
        })
    };
    let mut found: Vec<(PackageId, &'static str)> = resolutions
        .iter()
        .flat_map(|(id, r)| {
            let mut hosts = Vec::new();
            if r.host_context && differs(&r.features, &r.deps) {
                hosts.push((id.clone(), ""));
            }
            // Host compilations use the plain rules, which Buck configures for the
            // execution platform whatever the target (no per-platform marking
            // either, N6).
            if !r.host_context && r.host_uses_plain_rules {
                let recorded = match execution {
                    Some(execution) => {
                        let (features, edges) = plain_rule_on(r, execution);
                        r.plain_host_needs.iter().any(|need| {
                            need.features.iter().any(|(platform, needed)| {
                                let needed_edges =
                                    [EdgeRole::Normal, EdgeRole::Build].map(|role| {
                                        need.deps
                                            .role(role)
                                            .get(platform)
                                            .cloned()
                                            .unwrap_or_default()
                                    });
                                *needed != features || needed_edges != edges
                            })
                        })
                    }
                    None => differs(&r.features, &r.deps),
                };
                if recorded {
                    hosts.push((id.clone(), PLAIN_HOST_USE));
                }
            }
            if let Some(host) = &r.host_variant
                && differs(&host.features, &host.deps)
            {
                hosts.push((id.clone(), "-host"));
            }
            if let Some(host) = &r.test_host_variant
                && differs(&host.features, &host.deps)
            {
                hosts.push((id.clone(), "-test-host"));
            }
            hosts
        })
        .collect();
    found.sort_by(|a, b| (&a.0.repr, a.1).cmp(&(&b.0.repr, b.1)));
    found
}

/// One context's resolution of a package, per platform.
#[derive(Debug, Default, Clone, PartialEq)]
struct ContextResolution {
    features: FeaturesByPlatform,
    deps: Edges,
    /// The package's library is compiled in this context (on some platform).
    has_lib: bool,
}

impl ContextResolution {
    fn is_empty(&self) -> bool {
        self.features.is_empty()
    }
}

/// Resolve the features of every package for each `(platform name, target triple)`,
/// as Cargo does, from its unit graphs.
///
/// The ordinary (production) build and the tests are resolved apart, as Cargo builds
/// them: the main resolution is exactly that of `cargo build --workspace --target
/// <triple>` (target and host contexts; no feature that only a dev-dependency or
/// the test features enable), and the tests' that of `cargo test --workspace
/// --no-run --target <triple> --features <test_features>`. A library the tests need
/// with other features or dependencies gets a test variant.
pub fn resolve_platform_features(
    manifest_path: Option<&str>,
    platforms: &BTreeMap<String, String>,
    packages: &HashMap<PackageId, Package>,
    test_features: &[String],
) -> Result<PlatformFeatures> {
    let (main, _) = resolve_shape(manifest_path, platforms, packages, GraphShape::Build)?;
    let (tests, tests_built) = resolve_shape(
        manifest_path,
        platforms,
        packages,
        GraphShape::Test(test_features),
    )?;
    Ok(with_test_resolution(main, tests, &tests_built))
}

/// Which compilation Cargo plans in a unit graph.
#[derive(Debug, Clone, Copy)]
enum GraphShape<'a> {
    /// `cargo build --workspace`: libraries and binaries.
    Build,
    /// `cargo test --workspace --no-run`, with these `--features`.
    Test(&'a [String]),
    /// `cargo build -p <package>`, with these `--features` and this `--profile`:
    /// the package's own graph.
    Package(&'a str, &'a [String], &'a str),
    /// `cargo test -p <package> --lib --no-run`, with these `--features`: the
    /// package's own graph of its library unit tests.
    PackageTest(&'a str, &'a [String]),
}

/// What a unit graph compiles besides each package's resolution.
#[derive(Debug, Default)]
struct Built {
    /// Packages compiled in the host context.
    host: BTreeSet<PackageId>,
    /// Packages whose library is compiled for the target.
    target_libs: BTreeSet<PackageId>,
}

fn resolve_shape(
    manifest_path: Option<&str>,
    platforms: &BTreeMap<String, String>,
    packages: &HashMap<PackageId, Package>,
    shape: GraphShape,
) -> Result<(PlatformFeatures, Built)> {
    let mut target: HashMap<PackageId, ContextResolution> = HashMap::new();
    let mut host: HashMap<PackageId, ContextResolution> = HashMap::new();
    for (platform, triple) in platforms {
        let output = run_unit_graph(manifest_path, triple, shape)
            .with_context(|| format!("failed to resolve features for platform `{platform}`"))?;
        let parsed = parse_unit_graph(&output, packages)
            .with_context(|| format!("failed to read Cargo's unit graph for `{triple}`"))?;
        for (context, into) in [(parsed.target, &mut target), (parsed.host, &mut host)] {
            for (id, features) in context.features {
                let deps = context.deps.get(&id).cloned().unwrap_or_default();
                let has_lib = context.libs.contains(&id);
                let entry = into.entry(id).or_default();
                entry.has_lib |= has_lib;
                entry.features.insert(platform.clone(), features);
                for role in [EdgeRole::Normal, EdgeRole::Build, EdgeRole::Dev] {
                    let on = deps.role(role);
                    if !on.is_empty() || role == EdgeRole::Normal {
                        entry
                            .deps
                            .role_mut(role)
                            .insert(platform.clone(), on.clone());
                    }
                }
            }
        }
    }
    let built = Built {
        host: host.keys().cloned().collect(),
        target_libs: target
            .iter()
            .filter(|(_, t)| t.has_lib)
            .map(|(id, _)| id.clone())
            .collect(),
    };
    Ok((combine_contexts(target, host), built))
}

/// Combine the ordinary build's resolution (`main`) with the tests' (`tests`).
///
/// A package that only the tests build takes the test resolution, in the test
/// context. A target-context library that the tests build with other features or
/// library dependencies, or whose library dependencies there include a test
/// variant, gets a test variant. A package whose tests resolve differently from its
/// main resolution, or use a test variant, records its tests' resolution.
fn with_test_resolution(
    mut main: PlatformFeatures,
    mut tests: PlatformFeatures,
    tests_built: &Built,
) -> PlatformFeatures {
    let tests_host_built = &tests_built.host;
    // The host compilation of a package: its main resolution when it is host-only,
    // else its host variant, else (built for the target alike) the target one.
    let host_side = |r: &PlatformResolution| -> HostVariant {
        match (&r.host_variant, r.host_context) {
            (Some(host), false) => host.clone(),
            _ => HostVariant {
                features: r.features.clone(),
                deps: r.deps.clone(),
            },
        }
    };
    let host_edges = |h: &HostVariant| (h.deps.normal.clone(), h.deps.build.clone());
    // Packages the tests compile for the host differently from the ordinary build
    // (one that only the tests build is emitted from the tests' resolution and needs
    // no variant), or whose host dependencies there do.
    let in_both_on_host = |id: &PackageId| {
        tests_host_built.contains(id) && main.contains_key(id) && tests.contains_key(id)
    };
    // Packages the ordinary build compiles in both contexts apart: a target rule
    // and a host variant, which a host compilation must use.
    let host_variant_in_main: BTreeSet<String> = main
        .iter()
        .filter(|(_, m)| m.host_variant.is_some())
        .map(|(id, _)| id.repr.clone())
        .collect();
    let host_compiled_in_main: BTreeSet<String> = main
        .iter()
        .filter(|(_, m)| m.host_context || m.host_variant.is_some())
        .map(|(id, _)| id.repr.clone())
        .collect();
    let mut host_variants: BTreeSet<String> = tests
        .iter()
        .filter(|(id, t)| {
            in_both_on_host(id) && {
                let (m, t) = (host_side(&main[*id]), host_side(t));
                m.features != t.features
                    || host_edges(&m) != host_edges(&t)
                    // The ordinary build compiles it for the target only, so its rule
                    // is a target compilation: one whose dependencies have a host
                    // variant would reach their target flavours.
                    || !host_compiled_in_main.contains(&id.repr)
                        && t.deps
                            .normal
                            .values()
                            .chain(t.deps.build.values())
                            .flatten()
                            .any(|dep| host_variant_in_main.contains(dep))
            }
        })
        .map(|(id, _)| id.repr.clone())
        .collect();
    loop {
        let more: Vec<String> = tests
            .iter()
            .filter(|(id, _)| in_both_on_host(id) && !host_variants.contains(&id.repr))
            .filter(|(_, t)| {
                let h = host_side(t);
                h.deps
                    .normal
                    .values()
                    .chain(h.deps.build.values())
                    .flatten()
                    .any(|dep| host_variants.contains(dep))
            })
            .map(|(id, _)| id.repr.clone())
            .collect();
        if more.is_empty() {
            break;
        }
        host_variants.extend(more);
    }
    let lib_edges = |r: &PlatformResolution| (r.deps.normal.clone(), r.deps.build.clone());
    let differs = |m: &PlatformResolution, t: &PlatformResolution| {
        m.features != t.features || lib_edges(m) != lib_edges(t)
    };
    // Packages the tests compile for the target that the ordinary build also has:
    // for the target, or (a library the ordinary build only uses on the host) for
    // the host only, whose production rule then has the host's resolution.
    // Only a library the tests compile for the target can have a test variant: a
    // cdylib-only library's unit tests are no build of the library itself.
    let in_both_on_target = |id: &PackageId| {
        main.contains_key(id)
            && tests_built.target_libs.contains(id)
            && tests.get(id).is_some_and(|t| !t.host_context)
    };
    let host_only_in_tests: BTreeSet<String> = tests
        .iter()
        .filter(|(_, t)| t.host_context)
        .map(|(id, _)| id.repr.clone())
        .collect();
    let mut variants: BTreeSet<String> = tests
        .iter()
        .filter(|(id, t)| {
            in_both_on_target(id)
                && (differs(&main[*id], t)
                    // The ordinary build compiles it for the host only, so its rule
                    // is a host compilation, against its dependencies' host
                    // variants: the tests' target compilation needs their target
                    // flavours.
                    || main[*id].host_context
                        && t.deps
                            .normal
                            .values()
                            .chain(t.deps.build.values())
                            .flatten()
                            .any(|dep| host_variant_in_main.contains(dep))
                    // Its build script or a proc-macro it uses is compiled otherwise
                    // for the tests. (A normal dependency is a host compilation of
                    // this target rule only when it is host-only: a proc-macro.)
                    || t.deps
                        .build
                        .values()
                        .flatten()
                        .any(|dep| host_variants.contains(dep))
                    || t.deps.normal.values().flatten().any(|dep| {
                        host_variants.contains(dep) && host_only_in_tests.contains(dep)
                    }))
        })
        .map(|(id, _)| id.repr.clone())
        .collect();
    loop {
        // A target test variant reaches its dependents' target compilations only:
        // a build script is a host compilation, which uses a dependency's host or
        // test host flavour (seeded above through `host_variants`), never its
        // target test variant.
        let more: Vec<String> = tests
            .iter()
            .filter(|(id, _)| in_both_on_target(id) && !variants.contains(&id.repr))
            .filter(|(_, t)| {
                t.deps
                    .normal
                    .values()
                    .flatten()
                    .any(|dep| variants.contains(dep))
            })
            .map(|(id, _)| id.repr.clone())
            .collect();
        if more.is_empty() {
            break;
        }
        variants.extend(more);
    }

    // Packages whose `<lib>-host` rule is emitted: host variants of the ordinary
    // build, and those of packages only the tests build.
    let emitted_host_variants: BTreeSet<String> = main
        .iter()
        .filter(|(_, m)| m.host_variant.is_some())
        .map(|(id, _)| id.repr.clone())
        .chain(
            tests
                .iter()
                .filter(|(id, t)| !main.contains_key(*id) && t.host_variant.is_some())
                .map(|(id, _)| id.repr.clone()),
        )
        .collect();

    let ids: BTreeSet<PackageId> = main.keys().chain(tests.keys()).cloned().collect();
    for id in ids {
        let Some(t) = tests.remove(&id) else {
            continue;
        };
        let all_deps: BTreeSet<String> = t
            .deps
            .all()
            .chain(t.exec_context.iter().flat_map(|e| e.deps.all()))
            .chain(t.host_variant.iter().flat_map(|h| h.deps.all()))
            .cloned()
            .collect();
        let test_variant_deps: BTreeSet<String> = all_deps
            .iter()
            .filter(|dep| variants.contains(*dep))
            .cloned()
            .collect();
        let test_host_variant_deps: BTreeSet<String> = all_deps
            .iter()
            .filter(|dep| host_variants.contains(*dep))
            .cloned()
            .collect();
        let test_context_host_variant_deps: BTreeSet<String> = all_deps
            .iter()
            .filter(|dep| emitted_host_variants.contains(*dep))
            .cloned()
            .collect();
        match main.get_mut(&id) {
            None => {
                // Built for the tests only: its rules are in the test context.
                let mut only = t;
                only.test_variant_deps = test_variant_deps;
                only.test_host_variant_deps = test_host_variant_deps;
                only.host_variant_deps = test_context_host_variant_deps.clone();
                only.test_context_host_variant_deps = test_context_host_variant_deps;
                only.test_context = true;
                main.insert(id, only);
            }
            Some(m) => {
                // The tests compile it for the host too: without a host variant of
                // the ordinary build, their host compilations use its plain rules.
                if !m.host_context && m.host_variant.is_none() && tests_host_built.contains(&id) {
                    m.host_uses_plain_rules = true;
                    // Unless they get a test host variant, which they then use.
                    if !host_variants.contains(&id.repr) {
                        m.plain_host_needs
                            .extend(t.plain_host_needs.iter().cloned());
                        m.plain_host_needs.extend(t.host_variant.iter().cloned());
                    }
                }
                if variants.contains(&id.repr) {
                    m.test_variant = Some(HostVariant {
                        features: t.features.clone(),
                        deps: t.deps.clone(),
                    });
                }
                if host_variants.contains(&id.repr) {
                    m.test_host_variant = Some(host_side(&t));
                }
                let same = m.features == t.features
                    && m.deps == t.deps
                    && m.exec_context == t.exec_context;
                if !same || !test_variant_deps.is_empty() || !test_host_variant_deps.is_empty() {
                    m.tests = Some(Box::new(t));
                }
                m.test_variant_deps = test_variant_deps;
                m.test_host_variant_deps = test_host_variant_deps;
                m.test_context_host_variant_deps = test_context_host_variant_deps;
            }
        }
    }
    main
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
                || [EdgeRole::Normal, EdgeRole::Build, EdgeRole::Dev]
                    .into_iter()
                    .any(|role| t.deps.role(role).get(platform) != h.deps.role(role).get(platform))
        })
    };
    let mut variants: BTreeSet<String> = target
        .iter()
        .filter_map(|(id, t)| {
            let h = host.get(id)?;
            (t.has_lib && h.has_lib && differs(t, h)).then(|| id.repr.clone())
        })
        .collect();
    loop {
        let more: Vec<String> = target
            .iter()
            .filter(|(id, t)| t.has_lib && !variants.contains(&id.repr))
            .map(|(id, _)| id)
            .filter(|id| {
                host.get(*id)
                    .filter(|h| h.has_lib)
                    .is_some_and(|h| h.deps.all().any(|dep| variants.contains(dep)))
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
        let host_variant_deps = t
            .iter()
            .chain(h.iter())
            .flat_map(|c| c.deps.all())
            .filter(|dep| variants.contains(*dep))
            .cloned()
            .collect();
        // The library's own context decides the package's main resolution: the target
        // where the library is built for the target, the host where it is built for the
        // host only (a proc-macro, or a library used only by build scripts and
        // proc-macros). Executables compiled in the other context (a proc-macro's
        // integration test, on the target) keep that context's resolution apart.
        let lib_on_target = t.as_ref().is_some_and(|t| t.has_lib);
        let lib_on_host = h.as_ref().is_some_and(|h| h.has_lib);
        let resolution = match (t, h) {
            (Some(t), h) if lib_on_target || !lib_on_host => PlatformResolution {
                host_uses_plain_rules: h.is_some() && !variants.contains(&id.repr),
                plain_host_needs: h
                    .iter()
                    .filter(|_| !variants.contains(&id.repr))
                    .map(|h| HostVariant {
                        features: h.features.clone(),
                        deps: h.deps.clone(),
                    })
                    .collect(),
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
                exec_context: None,
                ..Default::default()
            },
            (t, Some(h)) => {
                // A host-only library's unit tests are host compilations, with the
                // host context's dev edges (in `h`); its integration tests and
                // binaries are target compilations, with the target's alone.
                let exec_context = t.map(|t| HostVariant {
                    features: t.features,
                    deps: t.deps,
                });
                PlatformResolution {
                    features: h.features,
                    deps: h.deps,
                    host_context: true,
                    host_variant: None,
                    host_variant_deps,
                    exec_context,
                    ..Default::default()
                }
            }
            (None, None) => continue,
            (Some(_), None) => unreachable!("handled by the first arm"),
        };
        result.insert(id, resolution);
    }
    result
}

fn run_unit_graph(manifest_path: Option<&str>, triple: &str, shape: GraphShape) -> Result<String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let mut cmd = Command::new(cargo);
    cmd.env("RUSTC_BOOTSTRAP", "1");
    let command = match shape {
        GraphShape::Build => {
            cmd.args(["build", "--workspace"]);
            "cargo build"
        }
        GraphShape::Test(features) => {
            cmd.args(["test", "--workspace", "--no-run"]);
            if !features.is_empty() {
                cmd.args(["--features", &features.join(",")]);
            }
            "cargo test"
        }
        GraphShape::Package(package, features, profile) => {
            cmd.args(["build", "-p", package, "--profile", profile]);
            if !features.is_empty() {
                cmd.args(["--features", &features.join(",")]);
            }
            "cargo build -p"
        }
        GraphShape::PackageTest(package, features) => {
            cmd.args(["test", "-p", package, "--lib", "--no-run"]);
            if !features.is_empty() {
                cmd.args(["--features", &features.join(",")]);
            }
            "cargo test -p"
        }
    };
    cmd.args(["--target", triple, "-Z", "unstable-options", "--unit-graph"]);
    if let Some(path) = manifest_path {
        cmd.args(["--manifest-path", path]);
    }
    let output = cmd
        .output()
        .with_context(|| format!("failed to run `{command} --unit-graph`"))?;
    if !output.status.success() {
        bail!(
            "`{command} --unit-graph --target {triple}` exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?)
}

/// A package's dependencies in one context, by role (platform-independent: one
/// unit graph is one platform).
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct RoleDeps {
    normal: BTreeSet<String>,
    build: BTreeSet<String>,
    dev: BTreeSet<String>,
}

impl RoleDeps {
    pub(crate) fn role(&self, role: EdgeRole) -> &BTreeSet<String> {
        match role {
            EdgeRole::Normal => &self.normal,
            EdgeRole::Build => &self.build,
            EdgeRole::Dev => &self.dev,
        }
    }

    fn role_mut(&mut self, role: EdgeRole) -> &mut BTreeSet<String> {
        match role {
            EdgeRole::Normal => &mut self.normal,
            EdgeRole::Build => &mut self.build,
            EdgeRole::Dev => &mut self.dev,
        }
    }
}

/// Features and dependencies of packages in one context.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct ContextTree {
    /// Union of the features of the package's units in this context.
    pub features: HashMap<PackageId, BTreeSet<String>>,
    /// Dependencies (`PackageId::repr`) of the package's units in this context, by
    /// role. Build dependencies are host-context packages.
    pub deps: HashMap<PackageId, RoleDeps>,
    /// Packages whose library (or proc-macro) is compiled in this context.
    pub libs: HashSet<PackageId>,
    /// The profile of each package's library unit in this context.
    pub profiles: HashMap<PackageId, UnitProfile>,
    /// The profile of each package's library unit-test unit in this context.
    pub test_profiles: HashMap<PackageId, UnitProfile>,
}

/// The profile Cargo resolved for one unit (`--unit-graph`'s `profile`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct UnitProfile {
    pub opt_level: String,
    /// `"false"`, `"true"`, `"thin"`, `"fat"` or `"off"`.
    pub lto: String,
    pub codegen_units: Option<u32>,
    /// A level (0, 1, 2) or a name (`"line-tables-only"`, ...).
    pub debuginfo: serde_json::Value,
    pub debug_assertions: bool,
    pub overflow_checks: bool,
    pub incremental: bool,
    pub panic: String,
    /// `{"resolved": "None" | {"Named": <level>}}` (or `"deferred"`).
    pub strip: serde_json::Value,
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct ParsedGraph {
    /// Packages as compiled for the target.
    pub target: ContextTree,
    /// Packages as compiled for the host: build dependencies, proc-macros and all
    /// their dependencies.
    pub host: ContextTree,
}

#[derive(Deserialize)]
struct UnitGraph {
    units: Vec<Unit>,
}

#[derive(Deserialize)]
struct Unit {
    pkg_id: String,
    target: UnitTarget,
    /// The target triple; `None` for units compiled for the host.
    platform: Option<String>,
    mode: String,
    features: Vec<String>,
    dependencies: Vec<UnitDep>,
    profile: Option<UnitProfile>,
}

#[derive(Deserialize)]
struct UnitTarget {
    kind: Vec<String>,
}

#[derive(Deserialize)]
struct UnitDep {
    index: usize,
}

impl Unit {
    /// Compiling a build script. Such a unit has no `platform` whatever the
    /// package's context is; it is reached through the package's
    /// `run-custom-build` unit, which does carry the context.
    fn is_build_script_compile(&self) -> bool {
        self.mode != "run-custom-build" && self.target.kind.iter().any(|k| k == "custom-build")
    }

    /// Compiling the package's library or proc-macro (not its tests or executables).
    fn is_lib_build(&self) -> bool {
        self.mode == "build"
            && self.target.kind.iter().any(|k| {
                matches!(
                    k.as_str(),
                    "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro"
                )
            })
    }

    /// A unit of the package's library target (in any mode).
    fn is_lib_target(&self) -> bool {
        self.target.kind.iter().any(|k| {
            matches!(
                k.as_str(),
                "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro"
            )
        })
    }

    /// The role of this unit's dependencies: tests and examples see dev-dependencies,
    /// the library and binaries normal ones.
    fn edge_role(&self) -> EdgeRole {
        if self.mode == "test"
            || self
                .target
                .kind
                .iter()
                .any(|k| k == "test" || k == "example" || k == "bench")
        {
            EdgeRole::Dev
        } else {
            EdgeRole::Normal
        }
    }
}

/// Parse `cargo --unit-graph` output into each package's features and dependencies
/// per context and role. A unit is in the host context when it has no `platform`.
/// Doc-test units are skipped. Units of packages that are not in `packages` (the
/// `cargo metadata` resolve) are ignored.
pub(crate) fn parse_unit_graph(
    output: &str,
    packages: &HashMap<PackageId, Package>,
) -> Result<ParsedGraph> {
    let graph: UnitGraph = serde_json::from_str(output)?;
    let unit = |index: usize| {
        graph
            .units
            .get(index)
            .with_context(|| format!("unit graph refers to missing unit {index}"))
    };
    let mut parsed = ParsedGraph::default();
    for u in &graph.units {
        if u.mode == "doctest" || u.is_build_script_compile() {
            continue;
        }
        let id = PackageId {
            repr: u.pkg_id.clone(),
        };
        if !packages.contains_key(&id) {
            continue;
        }
        let context = if u.platform.is_some() {
            &mut parsed.target
        } else {
            &mut parsed.host
        };
        context
            .features
            .entry(id.clone())
            .or_default()
            .extend(u.features.iter().cloned());
        if u.is_lib_build() {
            context.libs.insert(id.clone());
            if let Some(profile) = &u.profile {
                context.profiles.insert(id.clone(), profile.clone());
            }
        }
        if u.mode == "test"
            && u.is_lib_target()
            && let Some(profile) = &u.profile
        {
            context.test_profiles.insert(id.clone(), profile.clone());
        }
        let deps = context.deps.entry(id).or_default();
        let role = u.edge_role();
        for dep in &u.dependencies {
            let v = unit(dep.index)?;
            // A dev-dependency of the package on itself: Cargo links the library
            // (built as the tests resolve it) into the library's own unit-test
            // harness, under its crate name. Recorded as a dev edge to itself.
            if v.pkg_id == u.pkg_id && u.mode == "test" && u.is_lib_target() && v.is_lib_build() {
                deps.dev.insert(u.pkg_id.clone());
                continue;
            }
            if v.pkg_id != u.pkg_id {
                // Another package's `run-custom-build` (its `links` metadata) comes
                // with a dependency on that package's library, which is recorded.
                if v.mode != "run-custom-build" {
                    deps.role_mut(role).insert(v.pkg_id.clone());
                }
                continue;
            }
            // The package's own build script: its dependencies are the package's
            // build dependencies.
            if v.mode == "run-custom-build" {
                for compile in &v.dependencies {
                    let w = unit(compile.index)?;
                    if w.pkg_id == u.pkg_id && w.is_build_script_compile() {
                        for build_dep in &w.dependencies {
                            deps.build.insert(unit(build_dep.index)?.pkg_id.clone());
                        }
                    }
                }
            }
        }
    }
    Ok(parsed)
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
                    BTreeMap::from([("features".to_owned(), PlatformValue::List(extra))]),
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
    fn test_unit_graph_resolves_target_specific_features_per_platform() {
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
            resolve_platform_features(Some(manifest.to_str().unwrap()), &platforms, &packages, &[])
                .expect("resolve");

        assert_eq!(resolved[&sdk].features["linux-x86_64"], set(&["default"]));
        assert_eq!(
            resolved[&sdk].features["wasm32"],
            set(&["browser", "default"])
        );
        // A dev-dependency's feature is not in the production build, as under `cargo
        // build`; the tests get it from the test variant, as under `cargo test`.
        assert_eq!(resolved[&fixtures].features["linux-x86_64"], set(&[]));
        assert_eq!(
            resolved[&fixtures].test_variant.as_ref().unwrap().features["linux-x86_64"],
            set(&["test_lib"])
        );

        // The optional dependency that only `browser` enables is an edge on wasm32 only.
        let extra = packages
            .values()
            .find(|p| p.name.as_str() == "extra")
            .unwrap()
            .id
            .clone();
        assert!(resolved[&sdk].deps.normal["linux-x86_64"].is_empty());
        assert_eq!(
            resolved[&sdk].deps.normal["wasm32"],
            BTreeSet::from([extra.repr.clone()])
        );
        assert_eq!(
            resolved[&sdk].platforms_with_dep(&extra, EdgeRole::Normal),
            Some(BTreeSet::from(["wasm32"]))
        );

        let (common, platform) = split_features(&resolved[&sdk].features);
        assert_eq!(common, set(&["default"]));
        assert_eq!(
            platform,
            BTreeMap::from([(
                "wasm32".to_owned(),
                BTreeMap::from([(
                    "features".to_owned(),
                    PlatformValue::List(set(&["browser"]))
                )])
            )])
        );
    }

    #[test]
    fn test_parse_unit_graph() {
        let dir = workspace();
        let packages = metadata_packages(dir.path());
        let id = |name: &str| {
            packages
                .values()
                .find(|p| p.name.as_str() == name)
                .unwrap()
                .id
                .clone()
        };
        let (sdk, extra, fixtures) = (id("sdk"), id("extra"), id("fixtures"));
        let unit = |pkg: &PackageId,
                    kind: &str,
                    platform: bool,
                    mode: &str,
                    features: &[&str],
                    deps: &[usize]| {
            serde_json::json!({
                "pkg_id": pkg.repr,
                "target": {"kind": [kind]},
                "platform": if platform { serde_json::json!("x86_64-unknown-linux-gnu") } else { serde_json::Value::Null },
                "mode": mode,
                "features": features,
                "dependencies": deps.iter().map(|i| serde_json::json!({"index": i})).collect::<Vec<_>>(),
            })
        };
        // 0: sdk lib (target) -> extra lib (1) and sdk's build script run (2).
        // 2: run -> compile (3), whose build dependency is fixtures, built for the host (4).
        // 5: a doc-test unit of sdk with a feature nothing else has, which is ignored.
        let graph = serde_json::json!({"version": 1, "units": [
            unit(&sdk, "lib", true, "build", &["browser", "default"], &[1, 2]),
            unit(&extra, "lib", true, "build", &[], &[]),
            unit(&sdk, "custom-build", true, "run-custom-build", &["browser", "default"], &[3]),
            unit(&sdk, "custom-build", false, "build", &["browser", "default"], &[4]),
            unit(&fixtures, "lib", false, "build", &["test_lib"], &[]),
            unit(&sdk, "lib", true, "doctest", &["doc-only"], &[0]),
        ], "roots": [0]});
        let parsed = parse_unit_graph(&graph.to_string(), &packages).expect("parse");

        assert_eq!(parsed.target.features[&sdk], set(&["browser", "default"]));
        // The library's edge is a normal one; the build script's dependency is a build
        // edge of sdk (a host-context package).
        assert_eq!(
            parsed.target.deps[&sdk].role(EdgeRole::Normal),
            &BTreeSet::from([extra.repr.clone()])
        );
        assert_eq!(
            parsed.target.deps[&sdk].role(EdgeRole::Build),
            &BTreeSet::from([fixtures.repr.clone()])
        );
        assert_eq!(parsed.target.deps[&extra], RoleDeps::default());
        assert_eq!(parsed.host.features[&fixtures], set(&["test_lib"]));
        assert!(!parsed.target.features.contains_key(&fixtures));
        // The build script compile unit has no platform but is not a host package.
        assert!(!parsed.host.features.contains_key(&sdk));
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
            resolve_platform_features(Some(manifest.to_str().unwrap()), &platforms, &packages, &[])
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
            deps: Edges {
                normal: BTreeMap::from([(
                    "linux".to_owned(),
                    deps.iter().map(|d| pid(d).repr).collect(),
                )]),
                ..Default::default()
            },
            has_lib: true,
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
                deps: Edges {
                    normal: BTreeMap::from([(
                        "linux".to_owned(),
                        BTreeSet::from([pid("json").repr])
                    )]),
                    ..Default::default()
                },
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
        assert_eq!(
            platform["wasm32"]["features"].as_list(),
            Some(&set(&["browser"]))
        );
    }
}
