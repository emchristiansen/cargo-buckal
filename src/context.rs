use std::collections::HashMap;

use anyhow::{Result, bail};
use cargo_metadata::{MetadataCommand, PackageId, camino::Utf8PathBuf};
use cargo_util_schemas::{lockfile::TomlLockfile, manifest::TomlManifest};

use crate::{
    buckal_error, buckal_warn,
    cache::BuckalCache,
    config::RepoConfig,
    platform_features::{
        EdgeRole, PLAIN_HOST_USE, host_resolutions_differing_by_platform, resolve_platform_features,
    },
    resolve::{BuckalNode, BuckalResolve, NodeKind},
    utils::{UnwrapOrExit, get_buck2_root},
    wasm::{
        WASM_TRIPLE, declared_module_features, declares_tests, package_dir, resolve_modules,
        resolve_tests, test_env_declaration,
    },
    workspace_inputs::{WorkspaceInputs, resolve_workspace_inputs},
};

pub struct BuckalContext {
    /// The root package id, if any (None for virtual workspaces)
    pub root: Option<PackageId>,
    pub resolve: BuckalResolve,
    pub workspace_root: Utf8PathBuf,
    /// Whether first-party crates inherit keys from workspace Cargo.toml
    pub workspace_inherit: bool,
    /// Whether to skip merging manual changes in BUCK files
    pub no_merge: bool,
    /// Repository configuration
    pub repo_config: RepoConfig,
    /// The workspace's members, whose binaries and tests `cargo build --workspace`
    /// and `cargo test --workspace` build; `None` takes every first-party package
    /// for one.
    pub workspace_members: Option<std::collections::BTreeSet<PackageId>>,
    /// Host compilations that resolve differently per configured platform, as
    /// `<name> v<version> (<host-only | variant -host | variant -test-host | plain rule used by host compilations>)`,
    /// sorted; `None` without `feature_platforms`. The emission record keeps it,
    /// and a change stops the next generation unless it is accepted.
    pub host_platform_differences: Option<Vec<String>>,
    /// For each entry of `host_platform_differences`, a digest of its resolution
    /// (features and normal and build dependencies, by name and version, per
    /// platform), so that a change in how an entry differs is caught too.
    pub host_platform_difference_digests: Option<std::collections::BTreeMap<String, String>>,
    /// The files outside each first-party package that its build-script runs get
    /// (`[fixups.<pkg>.buildscript.run] inputs`, expanded), by package name.
    pub buildscript_inputs: std::collections::BTreeMap<String, WorkspaceInputs>,
    /// With `[wasm]`: each module's Buck package directory, by package name.
    pub wasm_modules: std::collections::BTreeMap<String, String>,
    /// With `[wasm]`: each first-party package's `[package.metadata.buck2.test-env]`
    /// (`{variable: wasm package}`), where it declares one.
    pub declared_test_env:
        HashMap<PackageId, std::collections::BTreeMap<String, crate::wasm::TestEnvValue>>,
}

impl BuckalContext {
    pub fn new(manifest_path: Option<String>) -> Self {
        let mut repo_config = RepoConfig::load();
        let metadata = |features: &[String]| {
            let mut metadata_command = MetadataCommand::new();
            if let Some(manifest) = manifest_path.as_deref() {
                metadata_command.manifest_path(manifest);
            }
            if !features.is_empty() {
                metadata_command
                    .features(cargo_metadata::CargoOpt::SomeFeatures(features.to_vec()));
            }
            metadata_command.exec().unwrap_or_exit_ctx(
                "`cargo metadata` failed (with the configured `test_features` and the wasm modules' features, if any)",
            )
        };
        // Packages that only the tests' features enable are part of the resolve.
        let mut cargo_metadata = metadata(&repo_config.test_features);
        // So are the edges and packages only a wasm module's declared features
        // enable: its own graph has them, and its variants depend on them.
        if !repo_config.wasm.is_empty() {
            let module_features = declared_module_features(
                &repo_config.wasm.packages,
                &repo_config.wasm.test_packages,
                &cargo_metadata.packages,
                &cargo_metadata.workspace_members,
            )
            .unwrap_or_else(|error| {
                buckal_error!(format!("{error:#}"));
                std::process::exit(1);
            });
            if !module_features.is_empty() {
                let mut features = repo_config.test_features.clone();
                features.extend(module_features);
                cargo_metadata = metadata(&features);
            }
        }
        let root = cargo_metadata.root_package().map(|p| p.id.clone());
        let packages_map = cargo_metadata
            .packages
            .clone()
            .into_iter()
            .map(|p| (p.id.to_owned(), p))
            .collect::<HashMap<_, _>>();
        let resolve_meta = cargo_metadata
            .resolve
            .ok_or("`cargo metadata` returned no dependency resolve")
            .unwrap_or_exit();
        let nodes_map = resolve_meta
            .nodes
            .into_iter()
            .map(|n| (n.id.to_owned(), n))
            .collect::<HashMap<_, _>>();
        let lock_path = cargo_metadata.workspace_root.join("Cargo.lock");
        let lock_content =
            std::fs::read_to_string(&lock_path).unwrap_or_exit_ctx("failed to read Cargo.lock");
        let lock_file: TomlLockfile =
            toml::from_str(&lock_content).unwrap_or_exit_ctx("failed to parse Cargo.lock");
        let checksums_map = lock_file
            .package
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| {
                p.checksum
                    .map(|checksum| (format!("{}-{}", p.name, p.version), checksum))
            })
            .collect::<HashMap<_, _>>();
        let workspace_toml = cargo_metadata.workspace_root.join("Cargo.toml");
        let workspace_content = std::fs::read_to_string(&workspace_toml)
            .unwrap_or_exit_ctx("failed to read workspace Cargo.toml");
        let workspace_manifest: TomlManifest = toml::from_str(&workspace_content)
            .unwrap_or_exit_ctx("failed to parse workspace Cargo.toml");
        let workspace_inherit = workspace_manifest.workspace.is_some()
            && workspace_manifest
                .workspace
                .as_ref()
                .unwrap()
                .package
                .is_some();

        let buck2_root = get_buck2_root().unwrap_or_exit_ctx("failed to get Buck2 project root");
        let mut resolve = BuckalResolve::from_metadata(
            &nodes_map,
            &packages_map,
            &checksums_map,
            buck2_root.as_std_path(),
            repo_config.skip_dev_dependency_cycles,
        )
        .unwrap_or_exit_ctx("failed to resolve dependency graph");

        let mut host_platform_differences = None;
        let mut host_platform_difference_digests = None;
        if !repo_config.feature_platforms.is_empty() {
            let per_platform = resolve_platform_features(
                manifest_path.as_deref(),
                &repo_config.feature_platforms,
                &packages_map,
                &repo_config.test_features,
            )
            .unwrap_or_exit_ctx("failed to resolve per-platform features");
            let execution = repo_config.platforms.execution.as_deref();
            if let Some(execution) = execution
                && !repo_config.feature_platforms.contains_key(execution)
            {
                buckal_error!(format!(
                    "`[platforms] execution = \"{execution}\"` names no platform of `feature_platforms` ({})",
                    repo_config
                        .feature_platforms
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                std::process::exit(1);
            }
            let differing = host_resolutions_differing_by_platform(&per_platform, execution);
            let entries: Vec<(String, String)> = differing
                .iter()
                .map(|(id, suffix)| {
                    let name = packages_map
                        .get(id)
                        .map(|p| format!("{} v{}", p.name, p.version))
                        .unwrap_or_else(|| id.repr.clone());
                    let entry = match *suffix {
                        "" => format!("{name} (host-only)"),
                        PLAIN_HOST_USE => format!("{name} (plain rule used by host compilations)"),
                        _ => format!("{name} (variant {suffix})"),
                    };
                    let resolution = &per_platform[id];
                    let (features, deps) = match *suffix {
                        "-host" => resolution
                            .host_variant
                            .as_ref()
                            .map(|h| (&h.features, &h.deps)),
                        "-test-host" => resolution
                            .test_host_variant
                            .as_ref()
                            .map(|h| (&h.features, &h.deps)),
                        _ => Some((&resolution.features, &resolution.deps)),
                    }
                    .expect("a differing host compilation has a resolution");
                    let named = |dep: &String| {
                        packages_map
                            .get(&PackageId { repr: dep.clone() })
                            .map(|p| format!("{}@{}", p.name, p.version))
                            .unwrap_or_else(|| dep.clone())
                    };
                    let shape: std::collections::BTreeMap<&String, _> = features
                        .iter()
                        .map(|(platform, on)| {
                            let edges = |role: EdgeRole| -> std::collections::BTreeSet<String> {
                                deps.role(role)
                                    .get(platform)
                                    .map(|d| d.iter().map(named).collect())
                                    .unwrap_or_default()
                            };
                            (
                                platform,
                                (on, edges(EdgeRole::Normal), edges(EdgeRole::Build)),
                            )
                        })
                        .collect();
                    let mut hashed = serde_json::to_vec(&shape).expect("a resolution serializes");
                    // A plain-rule entry is compared with what its host compilations
                    // need, per platform: those are part of what it records (N11).
                    // Other kinds hash their resolution alone, as before.
                    if *suffix == PLAIN_HOST_USE {
                        let needs: Vec<std::collections::BTreeMap<&String, _>> = resolution
                            .plain_host_needs
                            .iter()
                            .map(|need| {
                                need.features
                                    .iter()
                                    .map(|(platform, on)| {
                                        let edges =
                                            |role: EdgeRole| -> std::collections::BTreeSet<String> {
                                                need.deps
                                                    .role(role)
                                                    .get(platform)
                                                    .map(|d| d.iter().map(named).collect())
                                                    .unwrap_or_default()
                                            };
                                        (
                                            platform,
                                            (on, edges(EdgeRole::Normal), edges(EdgeRole::Build)),
                                        )
                                    })
                                    .collect()
                            })
                            .collect();
                        hashed.extend(b"plain_host_needs");
                        hashed.extend(serde_json::to_vec(&needs).expect("host needs serialize"));
                    }
                    let digest = blake3::hash(&hashed);
                    (entry, digest.to_hex().to_string())
                })
                .collect();
            let mut names: Vec<String> = entries.iter().map(|(name, _)| name.clone()).collect();
            names.sort();
            host_platform_difference_digests = Some(entries.into_iter().collect());
            if !names.is_empty() {
                buckal_warn!(format!(
                    "{} host compilation(s) resolve differently per configured platform: {}. Buck configures host compilations in the execution platform, so their `platform` attribute selects the execution platform's resolution for every target; the other platforms' host features and dependencies are not built as Cargo plans them",
                    names.len(),
                    names.join(", ")
                ));
            }
            host_platform_differences = Some(names);
            for (pkg_id, features) in per_platform {
                if let Some(&idx) = resolve.index_map.get(&pkg_id) {
                    resolve.dag[idx].platform_features = features;
                }
            }
        }

        let workspace_members = Some(cargo_metadata.workspace_members.iter().cloned().collect());

        // Without `[wasm]`, nothing builds a `wasm-test` declaration: refuse it rather
        // than ignore it (with `[wasm]`, `resolve_tests` refuses an unlisted one).
        if repo_config.wasm.is_empty()
            && let Some(id) = cargo_metadata
                .workspace_members
                .iter()
                .find(|id| declares_tests(&packages_map[id]))
        {
            buckal_error!(format!(
                "`{}` declares `[package.metadata.buck2.wasm-test]`, but `[wasm] test_packages` does not list it",
                packages_map[id].name
            ));
            std::process::exit(1);
        }

        let mut wasm_modules = std::collections::BTreeMap::new();
        let mut declared_test_env = HashMap::new();
        if !repo_config.wasm.is_empty() {
            let wasm = &repo_config.wasm;
            let Some(platform) = repo_config
                .feature_platforms
                .iter()
                .find_map(|(name, triple)| (triple == WASM_TRIPLE).then_some(name.clone()))
            else {
                buckal_error!(format!(
                    "`[wasm]` needs a `feature_platforms` entry for {WASM_TRIPLE}"
                ));
                std::process::exit(1);
            };
            if wasm.platform.is_none() {
                buckal_error!(
                    "`[wasm] platform` is required: the wasm32 target platform the modules are configured for"
                );
                std::process::exit(1);
            }
            // Modules are workspace members, looked up by name among them only: a
            // path package outside the workspace may share a member's name.
            let members: std::collections::BTreeMap<String, PackageId> = cargo_metadata
                .workspace_members
                .iter()
                .map(|id| (packages_map[id].name.to_string(), id.clone()))
                .collect();
            let first_party: Vec<PackageId> = resolve
                .nodes()
                .filter(|n| matches!(n.kind, NodeKind::FirstParty { .. }))
                .map(|n| n.package_id.clone())
                .collect();
            let variants = resolve_modules(
                &wasm.packages,
                &platform,
                manifest_path.as_deref(),
                &packages_map,
                &members,
            )
            .unwrap_or_else(|error| {
                buckal_error!(format!("{error:#}"));
                std::process::exit(1);
            });
            for (id, by_module) in variants {
                let Some(&idx) = resolve.index_map.get(&id) else {
                    let modules = by_module.keys().cloned().collect::<Vec<_>>().join("`, `");
                    buckal_error!(format!(
                        "`[wasm] packages`: `{modules}`'s own graph compiles `{id}`, which the workspace's resolve does not contain"
                    ));
                    std::process::exit(1);
                };
                resolve.dag[idx].platform_features.wasm_variants = by_module;
            }
            let tests = resolve_tests(
                &wasm.test_packages,
                &platform,
                manifest_path.as_deref(),
                &packages_map,
                &members,
            )
            .unwrap_or_else(|error| {
                buckal_error!(format!("{error:#}"));
                std::process::exit(1);
            });
            for (id, by_key) in tests.variants {
                let Some(&idx) = resolve.index_map.get(&id) else {
                    let keys = by_key.keys().cloned().collect::<Vec<_>>().join("`, `");
                    buckal_error!(format!(
                        "`[wasm] test_packages`: `{keys}`'s own graph compiles `{id}`, which the workspace's resolve does not contain"
                    ));
                    std::process::exit(1);
                };
                let variants = &mut resolve.dag[idx].platform_features.wasm_variants;
                for (key, variant) in by_key {
                    // A module key spelled like a test graph key (a package named so).
                    if variants.insert(key.clone(), variant).is_some() {
                        buckal_error!(format!(
                            "`[wasm] test_packages`: the test graph `{key}` has the key of a `[wasm] packages` module; rename that package"
                        ));
                        std::process::exit(1);
                    }
                }
            }
            for (id, by_key) in tests.tests {
                let idx = resolve.index_map[&id];
                resolve.dag[idx].platform_features.wasm_tests = by_key;
            }
            for module in &wasm.packages {
                let package = &packages_map[&members[module]];
                let dir = package_dir(&package.manifest_path, &buck2_root).unwrap_or_default();
                // resolve_modules has read every declaration.
                let declaration = crate::wasm::module_declaration(package)
                    .ok()
                    .flatten()
                    .expect("a module's declaration was read");
                for (key, _, _) in declaration.modules(module) {
                    wasm_modules.insert(key, dir.clone());
                }
            }
            for id in &first_party {
                let declared = test_env_declaration(&packages_map[id]).unwrap_or_else(|error| {
                    buckal_error!(format!("{error:#}"));
                    std::process::exit(1);
                });
                if !declared.is_empty() {
                    declared_test_env.insert(id.clone(), declared);
                }
            }
        }

        let inputs = resolve_workspace_inputs(
            &mut repo_config,
            &resolve,
            &cargo_metadata.workspace_root,
            &buck2_root,
            workspace_inherit,
        )
        .unwrap_or_else(|error| {
            buckal_error!(format!("{error:#}"));
            std::process::exit(1);
        });
        for note in &inputs.notes {
            buckal_warn!(note.clone());
        }

        Self {
            root,
            resolve,
            workspace_members,
            host_platform_differences,
            host_platform_difference_digests,
            buildscript_inputs: inputs.by_package,
            wasm_modules,
            declared_test_env,
            workspace_root: cargo_metadata.workspace_root.clone(),
            workspace_inherit,
            no_merge: false,
            repo_config,
        }
    }

    /// The cache recording this generation: the packages' fingerprints, the
    /// fixups it applies and the host compilations that differ per platform.
    pub fn new_cache(&self) -> BuckalCache {
        BuckalCache::from_resolve_with_config(
            &self.resolve,
            &self.workspace_root,
            &self.repo_config,
            &self.declared_test_env_key(),
        )
        .with_host_platform_differences(
            self.host_platform_differences.clone(),
            self.host_platform_difference_digests.clone(),
        )
    }

    /// The declared test environments, by package name, serialized (empty when
    /// none is declared): a declaration reaches the tests of every package whose
    /// dependency closure contains its package, so a change to one regenerates
    /// every first-party package (Fable N15).
    fn declared_test_env_key(&self) -> Vec<u8> {
        if self.declared_test_env.is_empty() {
            return Vec::new();
        }
        let by_name: std::collections::BTreeMap<
            &str,
            &std::collections::BTreeMap<String, crate::wasm::TestEnvValue>,
        > = self
            .declared_test_env
            .iter()
            .map(|(id, declared)| {
                (
                    self.resolve.dag[self.resolve.index_map[id]].name.as_str(),
                    declared,
                )
            })
            .collect();
        serde_json::to_vec(&by_name).expect("declared test environments serialize")
    }

    /// Stop, writing nothing, when the host compilations that resolve differently
    /// per platform are not those the last generation recorded (R17 is deferred:
    /// Buck builds each of them in the execution platform's resolution only), unless
    /// `accept`. A record without the list, or no record, accepts any.
    pub fn check_host_platform_differences(&self, accept: bool) {
        let Some(current) = &self.host_platform_differences else {
            return;
        };
        let Ok(recorded) = BuckalCache::load() else {
            return;
        };
        let Some(previous) = recorded.host_platform_differences() else {
            return;
        };
        // Entries on both lists whose resolution changed (when both record it).
        let changed: Vec<&String> = match (
            recorded.host_platform_difference_digests(),
            &self.host_platform_difference_digests,
        ) {
            (Some(before), Some(now)) => current
                .iter()
                .filter(|name| previous.contains(name))
                .filter(|name| before.get(*name) != now.get(*name))
                .collect(),
            _ => Vec::new(),
        };
        if previous == current.as_slice() && changed.is_empty() || accept {
            return;
        }
        let mut lines: Vec<String> = previous
            .iter()
            .filter(|name| !current.contains(name))
            .map(|name| format!("  - {name}"))
            .collect();
        lines.extend(
            current
                .iter()
                .filter(|name| !previous.contains(name))
                .map(|name| format!("  + {name}")),
        );
        lines.extend(
            changed
                .iter()
                .map(|name| format!("  ~ {name} (its resolution changed)")),
        );
        buckal_error!(format!(
            "the host compilations that resolve differently per configured platform changed since the recorded generation (Buck builds each in the execution platform's resolution only):\n{}\nReview them, then run `cargo buckal migrate --accept-host-differences` to accept and record the new list.",
            lines.join("\n")
        ));
        std::process::exit(1);
    }

    /// Whether `id` is a member of the workspace (with no member list, any package).
    pub fn is_workspace_member(&self, id: &PackageId) -> bool {
        self.workspace_members
            .as_ref()
            .is_none_or(|members| members.contains(id))
    }

    pub fn patched_node<'a>(&'a self, node: &'a BuckalNode) -> Result<&'a BuckalNode> {
        if !matches!(node.kind, NodeKind::ThirdParty) {
            return Ok(node);
        }

        let Some(version_patch) = self.repo_config.patch.version.get(&node.name) else {
            return Ok(node);
        };

        if version_patch.from != node.version {
            return Ok(node);
        }

        let candidates: Vec<&BuckalNode> = self
            .resolve
            .nodes()
            .filter(|candidate| {
                matches!(candidate.kind, NodeKind::ThirdParty)
                    && candidate.name == node.name
                    && candidate.version == version_patch.to
                    && candidate.source == node.source
            })
            .collect();

        match candidates.as_slice() {
            [patched] => Ok(*patched),
            [] => bail!(
                "version patch for '{}' points to missing dependency version '{}'",
                node.name,
                version_patch.to
            ),
            _ => bail!(
                "version patch for '{}' is ambiguous for version '{}'",
                node.name,
                version_patch.to
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RepoPatchConfig, VersionPatch};
    use crate::resolve::{BuckalDep, BuckalNode, BuckalResolve, NodeKind};
    use cargo_metadata::Edition;
    use daggy::Dag;

    fn make_node_with_source(name: &str, version: &str, source: Option<&str>) -> BuckalNode {
        BuckalNode {
            package_id: PackageId {
                repr: format!(
                    "{}#{}@{}",
                    source.unwrap_or("path+file:///local"),
                    name,
                    version
                ),
            },
            name: name.to_string(),
            version: version.to_string(),
            features: vec![],
            platform_features: Default::default(),
            kind: NodeKind::ThirdParty,
            edition: Edition::E2021,
            manifest_path: Utf8PathBuf::from(format!("/tmp/{}/Cargo.toml", name)),
            targets: vec![],
            source: source.map(String::from),
            links: None,
            checksum: None,
        }
    }

    fn make_context(resolve: BuckalResolve, patch: RepoPatchConfig) -> BuckalContext {
        BuckalContext {
            workspace_members: None,
            host_platform_differences: None,
            host_platform_difference_digests: None,
            buildscript_inputs: Default::default(),
            wasm_modules: Default::default(),
            declared_test_env: Default::default(),
            root: None,
            resolve,
            workspace_root: Utf8PathBuf::from("/tmp"),
            workspace_inherit: false,
            no_merge: false,
            repo_config: RepoConfig {
                patch,
                ..RepoConfig::default()
            },
        }
    }

    /// When the same crate name/version exists from two different sources (e.g.
    /// crates.io and git), `patched_node` must select the candidate that matches
    /// the original node's source, not bail with "ambiguous".
    #[test]
    fn test_patched_node_disambiguates_by_source() {
        let registry = "registry+https://github.com/rust-lang/crates.io-index";
        let git = "git+https://github.com/example/foo.git";

        let from_registry = make_node_with_source("foo", "1.0.0", Some(registry));
        let to_registry = make_node_with_source("foo", "2.0.0", Some(registry));
        let to_git = make_node_with_source("foo", "2.0.0", Some(git));

        let mut dag = Dag::<BuckalNode, BuckalDep, u32>::new();
        let mut index_map = HashMap::new();

        let idx_from = dag.add_node(from_registry.clone());
        let idx_to_reg = dag.add_node(to_registry.clone());
        let idx_to_git = dag.add_node(to_git.clone());

        index_map.insert(from_registry.package_id.clone(), idx_from);
        index_map.insert(to_registry.package_id.clone(), idx_to_reg);
        index_map.insert(to_git.package_id.clone(), idx_to_git);

        let resolve = BuckalResolve {
            dag,
            index_map,
            test_only_edges: Default::default(),
        };

        let patch = RepoPatchConfig {
            version: [(
                "foo".to_string(),
                VersionPatch {
                    from: "1.0.0".to_string(),
                    to: "2.0.0".to_string(),
                },
            )]
            .into_iter()
            .collect(),
        };

        let ctx = make_context(resolve, patch);
        let result = ctx.patched_node(&from_registry).unwrap();

        // Must select the registry candidate, not the git one
        assert_eq!(result.source.as_deref(), Some(registry));
        assert_eq!(result.version, "2.0.0");
    }

    /// When the target version does not exist for the same source,
    /// `patched_node` should return an error even if another source has it.
    #[test]
    fn test_patched_node_missing_when_wrong_source() {
        let registry = "registry+https://github.com/rust-lang/crates.io-index";
        let git = "git+https://github.com/example/foo.git";

        let from_registry = make_node_with_source("foo", "1.0.0", Some(registry));
        // Only the git source has version 2.0.0
        let to_git = make_node_with_source("foo", "2.0.0", Some(git));

        let mut dag = Dag::<BuckalNode, BuckalDep, u32>::new();
        let mut index_map = HashMap::new();

        let idx_from = dag.add_node(from_registry.clone());
        let idx_to_git = dag.add_node(to_git.clone());

        index_map.insert(from_registry.package_id.clone(), idx_from);
        index_map.insert(to_git.package_id.clone(), idx_to_git);

        let resolve = BuckalResolve {
            dag,
            index_map,
            test_only_edges: Default::default(),
        };

        let patch = RepoPatchConfig {
            version: [(
                "foo".to_string(),
                VersionPatch {
                    from: "1.0.0".to_string(),
                    to: "2.0.0".to_string(),
                },
            )]
            .into_iter()
            .collect(),
        };

        let ctx = make_context(resolve, patch);
        let result = ctx.patched_node(&from_registry);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("missing dependency version")
        );
    }
}
