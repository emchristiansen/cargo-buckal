use std::{collections::BTreeSet as Set, path::PathBuf};

use anyhow::{Context, Result, bail};

use crate::{
    buck::{CargoTargetKind, RustRule},
    buckal_note, buckal_warn,
    buckify::actions::is_third_party,
    context::BuckalContext,
    platform::{Os, oses_from_platform, platform_is_target_only},
    platform_features::{EdgeRole, platform_dict_mut, platform_list_mut},
    resolve::{BuckalDep, BuckalNode, BuckalTarget, is_lib_like},
    utils::{get_buck2_root, get_vendor_path_relative},
};

use cargo_metadata::{DependencyKind, TargetKind};

/// Check if a dependency kind matches the expected target kind.
pub(super) fn dep_kind_matches(target_kind: CargoTargetKind, dep_kind: DependencyKind) -> bool {
    match target_kind {
        CargoTargetKind::CustomBuild => dep_kind == DependencyKind::Build,
        // Cargo test targets can depend on both dev-deps and regular deps.
        // NOTE: Because dev deps are scoped to test targets here, dev-dependency
        // cycles *could* lower to acyclic Buck2 graphs. However, resolve.rs
        // currently rejects all dev-dependency cycles at DAG construction time.
        // See resolve.rs from_metadata() Pass 2 if relaxing that restriction.
        CargoTargetKind::Test => {
            dep_kind == DependencyKind::Development || dep_kind == DependencyKind::Normal
        }
        _ => dep_kind == DependencyKind::Normal,
    }
}

fn get_lib_targets(node: &BuckalNode) -> Vec<&BuckalTarget> {
    node.targets
        .iter()
        .filter(|t| t.kind.iter().any(is_lib_like))
        .collect()
}

fn resolve_first_party_label(dep_node: &BuckalNode) -> Result<String> {
    let buck2_root = get_buck2_root().context("failed to get buck2 root")?;
    let manifest_path = PathBuf::from(dep_node.manifest_path.as_str());
    let manifest_dir = manifest_path
        .parent()
        .context("manifest_path should always have a parent directory")?;
    let relative_path = manifest_dir
        .strip_prefix(&buck2_root)
        .with_context(|| {
            format!(
                "dependency manifest dir `{}` is not under Buck2 root `{}`",
                manifest_dir.display(),
                buck2_root
            )
        })?
        .to_string_lossy()
        // Normalize path separators for Buck2 (always use forward slashes)
        .replace('\\', "/");

    let dep_bin_targets: Vec<_> = dep_node
        .targets
        .iter()
        .filter(|t| t.kind.contains(&TargetKind::Bin))
        .collect();

    let dep_lib_targets = get_lib_targets(dep_node);

    if dep_lib_targets.len() != 1 {
        bail!(
            "Expected exactly one library target for dependency {}, but found {}",
            dep_node.name,
            dep_lib_targets.len()
        );
    }

    let buckal_name = resolve_buckal_name(&dep_bin_targets, &dep_lib_targets);

    Ok(format!("//{relative_path}:{buckal_name}"))
}

fn resolve_buckal_name(
    dep_bin_targets: &[&BuckalTarget],
    dep_lib_targets: &[&BuckalTarget],
) -> String {
    if dep_bin_targets
        .iter()
        .any(|b| b.name == dep_lib_targets[0].name)
    {
        format!("{}-lib", dep_lib_targets[0].name)
    } else {
        dep_lib_targets[0].name.to_owned()
    }
}

fn resolve_dep_label(
    dep: &BuckalDep,
    dep_node: &BuckalNode,
    ctx: &BuckalContext,
) -> Result<(String, Option<String>)> {
    let dep_node = ctx.patched_node(dep_node)?;
    let dep_package_name = dep_node.name.to_string();
    let is_renamed = dep.name != dep_package_name.replace("-", "_");
    let alias = if is_renamed {
        Some(dep.name.clone())
    } else {
        None
    };

    if !is_third_party(dep_node) {
        let label = resolve_first_party_label(dep_node).with_context(|| {
            format!(
                "failed to resolve first-party label for `{}`",
                dep_node.name
            )
        })?;
        Ok((label, alias))
    } else {
        // third-party dependency
        Ok((
            format!(
                "//{}:{}",
                get_vendor_path_relative(&dep_node.package_id)?,
                dep_node.name
            ),
            alias,
        ))
    }
}

/// Insert a dependency label into `rust_rule` in the appropriate attribute.
///
/// `target` is the Buck label we want the rule to depend on. If `alias` is `Some`, the
/// dependency is recorded as a *named* dependency (used for renamed crates); otherwise it is
/// recorded as an unnamed dependency.
///
/// # Platforms
///
/// `platforms` controls whether the dependency is unconditional or platform-specific:
/// - `None` means the dependency applies on all platforms and is inserted into `deps` or
///   `named_deps`.
/// - `Some(set)` means the dependency is conditional and is inserted into `os_deps` or
///   `os_named_deps` for each OS in `set`.
///
/// # Conflict handling
///
/// - For unconditional named dependencies (`named_deps`), if an alias is encountered more than
///   once with different targets, we emit a warning and keep the first value.
/// - For platform-specific named dependencies (`os_named_deps`), an alias may map to only one
///   target per OS. Conflicting targets for the same `(alias, os)` are treated as an error.
fn insert_dep(
    rust_rule: &mut dyn RustRule,
    target: &str,
    alias: Option<&str>,
    platforms: Option<&Set<Os>>,
) -> Result<()> {
    if let Some(platforms) = platforms {
        for os in platforms {
            let os_key = os.key().to_owned();
            if let Some(alias) = alias {
                let entries = rust_rule
                    .os_named_deps_mut()
                    .entry(alias.to_owned())
                    .or_default();

                if let Some(existing) = entries.get(&os_key) {
                    if existing != target {
                        bail!(
                            "os_named_deps alias '{}' had conflicting targets for platform '{}': '{}' vs '{}'",
                            alias,
                            os_key,
                            existing,
                            target
                        );
                    }
                } else {
                    entries.insert(os_key.clone(), target.to_owned());
                }
            } else {
                rust_rule
                    .os_deps_mut()
                    .entry(os_key)
                    .or_default()
                    .insert(target.to_owned());
            }
        }
    } else if let Some(alias) = alias {
        let entry = rust_rule.named_deps_mut().entry(alias.to_owned());
        match entry {
            std::collections::btree_map::Entry::Vacant(v) => {
                v.insert(target.to_owned());
            }
            std::collections::btree_map::Entry::Occupied(o) => {
                if o.get() != target {
                    buckal_warn!(
                        "named_deps alias '{}' had conflicting targets: '{}' vs '{}'",
                        alias,
                        o.get(),
                        target
                    );
                }
            }
        }
    } else {
        rust_rule.deps_mut().insert(target.to_owned());
    }
    Ok(())
}

/// Whether `kind` of `node` is compiled for the host: a build script, or any rule of
/// a package that Cargo builds for the host only (or of its host variant).
pub(super) fn is_host_context(node: &BuckalNode, kind: CargoTargetKind) -> bool {
    kind == CargoTargetKind::CustomBuild || node.platform_features.host_context
}

/// Whether a host-context rule of `node` must use `dep_node`'s host variant.
pub(super) fn uses_host_variant(node: &BuckalNode, dep_node: &BuckalNode) -> bool {
    node.platform_features
        .host_variant_deps
        .contains(&dep_node.package_id.repr)
}

/// The name (or label) of the host variant of the rule `name`.
pub(super) fn host_variant_name(name: &str) -> String {
    format!("{name}-host")
}

/// The name (or label) of the test variant of the rule `name`.
pub(super) fn test_variant_name(name: &str) -> String {
    format!("{name}-test")
}

/// The name (or label) of the test host variant of the rule `name`.
pub(super) fn test_host_variant_name(name: &str) -> String {
    format!("{name}-test-host")
}

/// The variant of `dep_node` that the test-context rule `kind` of `node` uses: for
/// a host compilation (a build script or host-only rule) or a host-only dependency
/// (a proc-macro), its test host variant, else its host variant (in a host
/// compilation); for a target compilation, its test variant.
fn test_context_variant(
    node: &BuckalNode,
    kind: CargoTargetKind,
    dep_node: &BuckalNode,
) -> Option<fn(&str) -> String> {
    let resolution = &node.platform_features;
    let dep = &dep_node.package_id.repr;
    let host_rule = is_host_context(node, kind);
    // A target compilation loads only a proc-macro dependency on the host; any
    // other dependency (even one the ordinary build only uses on the host) it
    // compiles for the target, through the dependency's test variant.
    let host_dep = dep_node.platform_features.host_context
        && dep_node
            .targets
            .iter()
            .any(|t| t.kind.contains(&TargetKind::ProcMacro));
    if host_rule || host_dep {
        if resolution.test_host_variant_deps.contains(dep) {
            Some(test_host_variant_name)
        } else if host_rule && resolution.host_variant_deps.contains(dep) {
            Some(host_variant_name)
        } else {
            None
        }
    } else if resolution.test_variant_deps.contains(dep) {
        Some(test_variant_name)
    } else {
        None
    }
}

/// The role of `kind`'s dependency edges in Cargo's resolution.
fn edge_role(kind: CargoTargetKind) -> EdgeRole {
    match kind {
        CargoTargetKind::CustomBuild => EdgeRole::Build,
        CargoTargetKind::Test => EdgeRole::Dev,
        CargoTargetKind::Lib | CargoTargetKind::Bin => EdgeRole::Normal,
    }
}

pub(super) fn set_deps(
    rust_rule: &mut dyn RustRule,
    node: &BuckalNode,
    kind: CargoTargetKind,
    ctx: &BuckalContext,
) -> Result<()> {
    let node_platforms = node.platform_features.platforms();
    for (dep, dep_node) in ctx.resolve.deps_of(&node.package_id) {
        if !dep
            .dep_kinds
            .iter()
            .any(|dk| dep_kind_matches(kind, dk.kind))
        {
            continue;
        }

        // With per-platform resolution, Cargo's own activated edges of this kind decide
        // where the dependency exists, on exactly the configured platforms: an optional
        // dependency enabled by a feature that is on for no configured platform is not
        // built at all, one enabled on some platforms only is a dependency there only
        // (renamed or not), and a dependency declared for a target the OS mapping below
        // does not know (e.g. `cfg(target_arch = "wasm32")`) is kept where Cargo builds it.
        if let Some(on) = node
            .platform_features
            .platforms_with_dep(&dep_node.package_id, edge_role(kind))
        {
            if on.is_empty() {
                continue;
            }
            let (target_label, alias) = dep_label(node, kind, dep, dep_node, ctx)?;
            if on == node_platforms {
                insert_dep(rust_rule, &target_label, alias.as_deref(), None)?;
            } else {
                for platform in on {
                    match &alias {
                        Some(alias) => {
                            platform_dict_mut(rust_rule.platform_mut(), platform, "named_deps")
                                .insert(alias.clone(), target_label.clone());
                        }
                        None => {
                            platform_list_mut(rust_rule.platform_mut(), platform, "deps")
                                .insert(target_label.clone());
                        }
                    }
                }
            }
            continue;
        }

        let mut unconditional = false;
        let mut platforms = Set::<Os>::new();
        let mut has_unsupported_platform = false;

        for dk in dep
            .dep_kinds
            .iter()
            .filter(|dk| dep_kind_matches(kind, dk.kind))
        {
            match &dk.target {
                None => unconditional = true,
                Some(platform) => {
                    let oses = oses_from_platform(platform);
                    if oses.is_empty() {
                        if platform_is_target_only(platform) {
                            has_unsupported_platform = true;
                            continue;
                        }
                        unconditional = true;
                        continue;
                    }
                    platforms.extend(oses);
                }
            }
        }

        if !unconditional && platforms.is_empty() {
            if has_unsupported_platform {
                buckal_note!(
                    "Dependency '{}' (package '{}') targets only unsupported platforms and will be omitted.",
                    dep.name,
                    dep_node.name
                );
            }
            continue;
        }

        let (target_label, alias) = dep_label(node, kind, dep, dep_node, ctx)?;
        if unconditional {
            insert_dep(rust_rule, &target_label, alias.as_deref(), None)?;
        } else {
            insert_dep(rust_rule, &target_label, alias.as_deref(), Some(&platforms))?;
        }
    }
    Ok(())
}

/// The label (and alias, for a renamed dependency) that `kind` of `node` uses for
/// `dep_node`: its host variant in a host-context rule, where it has one.
fn dep_label(
    node: &BuckalNode,
    kind: CargoTargetKind,
    dep: &BuckalDep,
    dep_node: &BuckalNode,
    ctx: &BuckalContext,
) -> Result<(String, Option<String>)> {
    let (mut target_label, alias) = resolve_dep_label(dep, dep_node, ctx).with_context(|| {
        format!(
            "failed to resolve dependency label for '{}' (package '{}')",
            dep.name, dep_node.name
        )
    })?;
    if node.platform_features.test_context {
        if let Some(variant) = test_context_variant(node, kind, dep_node) {
            target_label = variant(&target_label);
        }
    } else if is_host_context(node, kind) && uses_host_variant(node, dep_node) {
        target_label = host_variant_name(&target_label);
    }
    Ok((target_label, alias))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use cargo_metadata::{DependencyKind, Edition, PackageId, camino::Utf8PathBuf};
    use daggy::Dag;

    use crate::{
        buck::{CargoTargetKind, RustLibrary},
        config::{RepoConfig, RepoPatchConfig, VersionPatch},
        context::BuckalContext,
        resolve::{BuckalDep, BuckalDepKind, BuckalResolve, NodeKind},
    };

    fn mock_target(name: &str, kind: TargetKind) -> BuckalTarget {
        BuckalTarget {
            name: name.to_string(),
            kind: vec![kind],
            src_path: cargo_metadata::camino::Utf8PathBuf::from("/tmp/dummy.rs"),
            doctest: true,
            required_features: Vec::new(),
            test: true,
        }
    }

    #[test]
    fn test_resolve_buckal_name_with_collision() {
        let lib = mock_target("foo", TargetKind::Lib);
        let bin = mock_target("foo", TargetKind::Bin);

        let lib_targets = vec![&lib];
        let bin_targets = vec![&bin];

        let name = resolve_buckal_name(&bin_targets, &lib_targets);
        assert_eq!(name, "foo-lib");
    }

    #[test]
    fn test_resolve_buckal_name_without_collision() {
        let lib = mock_target("foo", TargetKind::Lib);
        let bin = mock_target("bar", TargetKind::Bin);

        let lib_targets = vec![&lib];
        let bin_targets = vec![&bin];

        let name = resolve_buckal_name(&bin_targets, &lib_targets);
        assert_eq!(name, "foo");
    }

    #[test]
    fn test_set_deps_redirects_to_patched_version() {
        let root_id = PackageId {
            repr: "path+file:///tmp/root#root@0.1.0".to_string(),
        };
        let old_id = PackageId {
            repr: "registry+https://github.com/rust-lang/crates.io-index#foo@0.1.0".to_string(),
        };
        let new_id = PackageId {
            repr: "registry+https://github.com/rust-lang/crates.io-index#foo@0.2.0".to_string(),
        };

        let root_node = BuckalNode {
            package_id: root_id.clone(),
            name: "root".to_string(),
            version: "0.1.0".to_string(),
            features: vec![],
            platform_features: Default::default(),
            kind: NodeKind::FirstParty {
                relative_path: "".to_string(),
            },
            edition: Edition::E2021,
            manifest_path: Utf8PathBuf::from("/tmp/root/Cargo.toml"),
            targets: vec![mock_target("root", TargetKind::Lib)],
            source: None,
            links: None,
            checksum: None,
        };

        let old_node = BuckalNode {
            package_id: old_id.clone(),
            name: "foo".to_string(),
            version: "0.1.0".to_string(),
            features: vec![],
            platform_features: Default::default(),
            kind: NodeKind::ThirdParty,
            edition: Edition::E2021,
            manifest_path: Utf8PathBuf::from("/tmp/vendor/foo/0.1.0/Cargo.toml"),
            targets: vec![mock_target("foo", TargetKind::Lib)],
            source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
            links: None,
            checksum: None,
        };

        let new_node = BuckalNode {
            package_id: new_id.clone(),
            name: "foo".to_string(),
            version: "0.2.0".to_string(),
            features: vec![],
            platform_features: Default::default(),
            kind: NodeKind::ThirdParty,
            edition: Edition::E2021,
            manifest_path: Utf8PathBuf::from("/tmp/vendor/foo/0.2.0/Cargo.toml"),
            targets: vec![mock_target("foo", TargetKind::Lib)],
            source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
            links: None,
            checksum: None,
        };

        let mut dag = Dag::new();
        let root_idx = dag.add_node(root_node);
        let old_idx = dag.add_node(old_node);
        let new_idx = dag.add_node(new_node);

        dag.add_edge(
            root_idx,
            old_idx,
            BuckalDep {
                name: "foo".to_string(),
                dep_kinds: vec![BuckalDepKind {
                    kind: DependencyKind::Normal,
                    target: None,
                }],
            },
        )
        .expect("failed to add edge");

        let mut index_map = HashMap::new();
        index_map.insert(root_id.clone(), root_idx);
        index_map.insert(old_id, old_idx);
        index_map.insert(new_id, new_idx);

        let ctx = BuckalContext {
            workspace_members: None,
            host_platform_differences: None,
            root: Some(root_id.clone()),
            resolve: BuckalResolve {
                dag,
                index_map,
                test_only_edges: Default::default(),
            },
            workspace_root: Utf8PathBuf::from("/tmp/root"),
            workspace_inherit: false,
            no_merge: false,
            repo_config: RepoConfig {
                patch: RepoPatchConfig {
                    version: [(
                        "foo".to_string(),
                        VersionPatch {
                            from: "0.1.0".to_string(),
                            to: "0.2.0".to_string(),
                        },
                    )]
                    .into_iter()
                    .collect(),
                },
                ..RepoConfig::default()
            },
        };

        let root = ctx.resolve.get(&root_id).expect("missing root node");
        let mut rule = RustLibrary::default();

        set_deps(&mut rule, root, CargoTargetKind::Lib, &ctx).expect("failed to set deps");

        assert!(
            rule.deps
                .contains("//third-party/rust/crates/foo/0.2.0:foo")
        );
        assert!(
            !rule
                .deps
                .contains("//third-party/rust/crates/foo/0.1.0:foo")
        );
    }

    #[test]
    fn test_set_deps_follows_per_platform_edges() {
        let id = |name: &str| PackageId {
            repr: format!("registry+https://github.com/rust-lang/crates.io-index#{name}@1.0.0"),
        };
        let node = |name: &str| BuckalNode {
            package_id: id(name),
            name: name.to_string(),
            version: "1.0.0".to_string(),
            features: vec![],
            platform_features: Default::default(),
            kind: NodeKind::ThirdParty,
            edition: Edition::E2021,
            manifest_path: Utf8PathBuf::from(format!("/registry/{name}/Cargo.toml")),
            targets: vec![mock_target(name, TargetKind::Lib)],
            source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
            links: None,
            checksum: None,
        };
        let normal = || BuckalDepKind {
            kind: DependencyKind::Normal,
            target: None,
        };

        let mut parent = node("minicbor");
        // Cargo's per-platform edges: `everywhere` on both platforms, `wasm_only` on
        // wasm32 only, `nowhere` (an optional dependency whose feature no configured
        // platform enables) on neither.
        let everywhere = id("everywhere").repr;
        let wasm_only = id("wasm_only").repr;
        parent.platform_features.deps.normal = [
            ("linux-x86_64".to_owned(), Set::from([everywhere.clone()])),
            (
                "wasm32".to_owned(),
                Set::from([everywhere.clone(), wasm_only.clone()]),
            ),
        ]
        .into();
        parent.platform_features.features = [
            ("linux-x86_64".to_owned(), Set::new()),
            ("wasm32".to_owned(), Set::new()),
        ]
        .into();

        let mut dag = Dag::new();
        let mut index_map = HashMap::new();
        let parent_idx = dag.add_node(parent.clone());
        index_map.insert(parent.package_id.clone(), parent_idx);
        for name in ["everywhere", "wasm_only", "nowhere"] {
            let child = node(name);
            let idx = dag.add_node(child.clone());
            index_map.insert(child.package_id.clone(), idx);
            dag.add_edge(
                parent_idx,
                idx,
                BuckalDep {
                    name: name.to_string(),
                    dep_kinds: vec![normal()],
                },
            )
            .unwrap();
        }
        let ctx = BuckalContext {
            workspace_members: None,
            host_platform_differences: None,
            root: None,
            resolve: BuckalResolve {
                dag,
                index_map,
                test_only_edges: Default::default(),
            },
            workspace_root: Utf8PathBuf::from("/registry"),
            workspace_inherit: false,
            no_merge: false,
            repo_config: RepoConfig::default(),
        };

        let mut rule = RustLibrary::default();
        set_deps(&mut rule, &parent, CargoTargetKind::Lib, &ctx).expect("set deps");

        assert_eq!(
            rule.deps,
            Set::from(["//third-party/rust/crates/everywhere/1.0.0:everywhere".to_owned()])
        );
        assert_eq!(
            rule.platform["wasm32"]["deps"].as_list(),
            Some(&Set::from([
                "//third-party/rust/crates/wasm_only/1.0.0:wasm_only".to_owned()
            ]))
        );
        assert!(!rule.platform.contains_key("linux-x86_64"));
        let all: Vec<_> = rule
            .deps
            .iter()
            .chain(
                rule.platform
                    .values()
                    .flat_map(|a| a.values().filter_map(|v| v.as_list()).flatten()),
            )
            .collect();
        assert!(!all.iter().any(|label| label.contains("nowhere")));
    }

    #[test]
    fn test_host_context_rules_use_host_variants() {
        let id = |name: &str| PackageId {
            repr: format!("registry+https://github.com/rust-lang/crates.io-index#{name}@1.0.0"),
        };
        let node = |name: &str| BuckalNode {
            package_id: id(name),
            name: name.to_string(),
            version: "1.0.0".to_string(),
            features: vec![],
            platform_features: Default::default(),
            kind: NodeKind::ThirdParty,
            edition: Edition::E2021,
            manifest_path: Utf8PathBuf::from(format!("/registry/{name}/Cargo.toml")),
            targets: vec![mock_target(name, TargetKind::Lib)],
            source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
            links: None,
            checksum: None,
        };
        // `app` uses `json` as a normal and as a build dependency; `json` has a host variant.
        let mut app = node("app");
        app.platform_features.host_variant_deps = Set::from([id("json").repr]);
        let mut json = node("json");
        json.platform_features.host_variant = Some(Default::default());

        let mut dag = Dag::new();
        let app_idx = dag.add_node(app.clone());
        let json_idx = dag.add_node(json.clone());
        dag.add_edge(
            app_idx,
            json_idx,
            BuckalDep {
                name: "json".to_string(),
                dep_kinds: vec![
                    BuckalDepKind {
                        kind: DependencyKind::Normal,
                        target: None,
                    },
                    BuckalDepKind {
                        kind: DependencyKind::Build,
                        target: None,
                    },
                ],
            },
        )
        .unwrap();
        let ctx = BuckalContext {
            workspace_members: None,
            host_platform_differences: None,
            root: None,
            resolve: BuckalResolve {
                dag,
                index_map: HashMap::from([(id("app"), app_idx), (id("json"), json_idx)]),
                test_only_edges: Default::default(),
            },
            workspace_root: Utf8PathBuf::from("/registry"),
            workspace_inherit: false,
            no_merge: false,
            repo_config: RepoConfig::default(),
        };
        let label = "//third-party/rust/crates/json/1.0.0:json";

        let mut lib = RustLibrary::default();
        set_deps(&mut lib, &app, CargoTargetKind::Lib, &ctx).unwrap();
        assert_eq!(lib.deps, Set::from([label.to_owned()]));

        let mut build = RustLibrary::default();
        set_deps(&mut build, &app, CargoTargetKind::CustomBuild, &ctx).unwrap();
        assert_eq!(build.deps, Set::from([format!("{label}-host")]));

        // A host-only package (e.g. a proc-macro) uses the variant from its library too.
        let mut host_only = app.clone();
        host_only.platform_features.host_context = true;
        let mut pm = RustLibrary::default();
        set_deps(&mut pm, &host_only, CargoTargetKind::Lib, &ctx).unwrap();
        assert_eq!(pm.deps, Set::from([format!("{label}-host")]));
    }
}
