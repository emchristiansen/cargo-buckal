use std::collections::HashMap;

use anyhow::{Result, bail};
use cargo_metadata::{MetadataCommand, PackageId, camino::Utf8PathBuf};
use cargo_util_schemas::{lockfile::TomlLockfile, manifest::TomlManifest};

use crate::{
    buckal_warn,
    config::RepoConfig,
    platform_features::{host_resolutions_differing_by_platform, resolve_platform_features},
    resolve::{BuckalNode, BuckalResolve, NodeKind},
    utils::{UnwrapOrExit, get_buck2_root},
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
}

impl BuckalContext {
    pub fn new(manifest_path: Option<String>) -> Self {
        let repo_config = RepoConfig::load();
        let mut metadata_command = MetadataCommand::new();
        if let Some(manifest) = manifest_path.as_deref() {
            metadata_command.manifest_path(manifest);
        }
        // Packages that only the tests' features enable are part of the resolve.
        if !repo_config.test_features.is_empty() {
            metadata_command.features(cargo_metadata::CargoOpt::SomeFeatures(
                repo_config.test_features.clone(),
            ));
        }
        let cargo_metadata = metadata_command.exec().unwrap();
        let root = cargo_metadata.root_package().map(|p| p.id.clone());
        let packages_map = cargo_metadata
            .packages
            .clone()
            .into_iter()
            .map(|p| (p.id.to_owned(), p))
            .collect::<HashMap<_, _>>();
        let resolve_meta = cargo_metadata.resolve.unwrap();
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

        if !repo_config.feature_platforms.is_empty() {
            let per_platform = resolve_platform_features(
                manifest_path.as_deref(),
                &repo_config.feature_platforms,
                &packages_map,
                &repo_config.test_features,
            )
            .unwrap_or_exit_ctx("failed to resolve per-platform features");
            let differing = host_resolutions_differing_by_platform(&per_platform);
            if !differing.is_empty() {
                let names: Vec<String> = differing
                    .iter()
                    .map(|(id, suffix)| {
                        let name = packages_map
                            .get(id)
                            .map(|p| format!("{} v{}", p.name, p.version))
                            .unwrap_or_else(|| id.repr.clone());
                        format!(
                            "{name} ({}{suffix})",
                            if suffix.is_empty() {
                                "host-only"
                            } else {
                                "variant "
                            }
                        )
                    })
                    .collect();
                buckal_warn!(format!(
                    "{} host compilation(s) resolve differently per configured platform: {}. Buck configures host compilations in the execution platform, so their `platform` attribute selects the execution platform's resolution for every target; the other platforms' host features and dependencies are not built as Cargo plans them",
                    names.len(),
                    names.join(", ")
                ));
            }
            for (pkg_id, features) in per_platform {
                if let Some(&idx) = resolve.index_map.get(&pkg_id) {
                    resolve.dag[idx].platform_features = features;
                }
            }
        }

        Self {
            root,
            resolve,
            workspace_root: cargo_metadata.workspace_root.clone(),
            workspace_inherit,
            no_merge: false,
            repo_config,
        }
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

        let resolve = BuckalResolve { dag, index_map };

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

        let resolve = BuckalResolve { dag, index_map };

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
