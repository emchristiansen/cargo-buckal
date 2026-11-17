use std::collections::BTreeMap;

use anyhow::{Error, Result, anyhow};
use cargo_metadata::{PackageId, camino::Utf8PathBuf};
use serde::{Deserialize, Serialize};

use crate::buckal_error;
use crate::utils::{UnwrapOrExit, get_cache_path};

use crate::config::{PackageFixup, RepoConfig, RepoPatchConfig};
use crate::resolve::BuckalResolve;

// type Fingerprint = [u8; 32];

/// CACHE_VERSION is incremented whenever the cache format or logic changes in a way that is not backward-compatible.
///
/// Version 2: Added multi-platform support to the cache format.
/// Version 3: Switched to BuckalNode-based fingerprinting (DAG refactor).
/// Version 4: Include version patch config in fingerprints.
/// Version 5: Include each package's effective `[fixups]` and its full per-platform,
///            per-context resolution in fingerprints; first-party crates compile from
///            their checkout paths, tests declare their package's files and integration
///            tests name their binaries, host variants and ordered `rustc_flags`. A
///            version 4 cache holds fingerprints of rules generated without these, so it
///            is stale and everything is regenerated.
/// Version 6: Record the fixup `rustc_flags` each generation added, so that `--merge`
///            tells them from manual edits after the fixup changes or is removed.
/// Version 7: `mapped_srcs` destinations are emitted as repository paths. A version 6
///            cache vouches for package-relative destinations, so it is stale.
///
/// Migration strategy:
/// - If found < expected (stale cache from older Buckal): ignore the old cache and rebuild.
/// - If found > expected (cache from newer Buckal): exit immediately and prompt the user to upgrade.
const CACHE_VERSION: u32 = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl Serialize for Fingerprint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&hex::encode(self.0))
    }
}

impl<'de> Deserialize<'de> for Fingerprint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let bytes = hex::decode(s).map_err(serde::de::Error::custom)?;
        if bytes.len() != 32 {
            return Err(serde::de::Error::custom("fingerprint must be 32 bytes"));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(Fingerprint(arr))
    }
}

pub trait BuckalHash {
    fn fingerprint(&self) -> Fingerprint;
}

pub trait PackageIdExt {
    /// ($WORKSPACE) → workspace_root
    fn resolve(&self, workspace_root: &Utf8PathBuf) -> Self;

    /// workspace_root → ($WORKSPACE)
    fn canonicalize(&self, workspace_root: &Utf8PathBuf) -> Self;
}

impl PackageIdExt for PackageId {
    fn resolve(&self, workspace_root: &Utf8PathBuf) -> Self {
        if self.repr.starts_with("path+file://($WORKSPACE)") {
            PackageId {
                repr: self
                    .repr
                    .clone()
                    .replace("($WORKSPACE)", workspace_root.as_str()),
            }
        } else {
            self.clone()
        }
    }

    fn canonicalize(&self, workspace_root: &Utf8PathBuf) -> Self {
        if self
            .repr
            .starts_with(format!("path+file://{}", workspace_root.as_str()).as_str())
        {
            PackageId {
                repr: self
                    .repr
                    .clone()
                    .replace(workspace_root.as_str(), "($WORKSPACE)"),
            }
        } else {
            self.clone()
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct BuckalCache {
    fingerprints: BTreeMap<PackageId, Fingerprint>,
    version: u32,
    /// This is not a record of generated rules, only an inventory of the packages
    /// (rebuilt from current metadata because no compatible cache exists). It still
    /// detects removals, but every surviving package must be regenerated: their BUCK
    /// files may come from another generator version, or from nothing at all.
    #[serde(skip)]
    regenerate_all: bool,
    /// The fixup `rustc_flags` this generation added, by package name, so that a
    /// later `--merge` can tell them from manual edits once they change or go away.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    fixup_rustc_flags: BTreeMap<String, Vec<String>>,
}

impl BuckalCache {
    pub fn from_resolve(
        resolve: &BuckalResolve,
        workspace_root: &Utf8PathBuf,
        patch_config: &RepoPatchConfig,
    ) -> Self {
        Self::from_resolve_with_fixups(resolve, workspace_root, patch_config, &BTreeMap::new())
    }

    /// Fingerprints that also cover each package's effective `[fixups.<package>]`
    /// (none, or the configured one), so that adding, changing or removing a fixup
    /// regenerates exactly that package's rules in an ordinary cached migration.
    pub fn from_resolve_with_fixups(
        resolve: &BuckalResolve,
        workspace_root: &Utf8PathBuf,
        patch_config: &RepoPatchConfig,
        fixups: &BTreeMap<String, PackageFixup>,
    ) -> Self {
        Self::from_resolve_with_options(resolve, workspace_root, patch_config, fixups, &[])
    }

    /// Fingerprints for the repository configuration `config`: its fixups (see
    /// [`Self::from_resolve_with_fixups`]) and the workspace-level settings that
    /// change first-party packages' rules.
    pub fn from_resolve_with_config(
        resolve: &BuckalResolve,
        workspace_root: &Utf8PathBuf,
        config: &RepoConfig,
    ) -> Self {
        Self::from_resolve_with_options(
            resolve,
            workspace_root,
            &config.patch,
            &config.fixups,
            &config.first_party_generation_key(),
        )
    }

    fn from_resolve_with_options(
        resolve: &BuckalResolve,
        workspace_root: &Utf8PathBuf,
        patch_config: &RepoPatchConfig,
        fixups: &BTreeMap<String, PackageFixup>,
        first_party_key: &[u8],
    ) -> Self {
        let fingerprints = resolve
            .nodes()
            .map(|node| {
                let mut base =
                    resolve.fingerprint_of(&node.package_id, workspace_root, patch_config);
                if !first_party_key.is_empty()
                    && matches!(node.kind, crate::resolve::NodeKind::FirstParty { .. })
                {
                    let mut hasher = blake3::Hasher::new();
                    hasher.update(&base.0);
                    hasher.update(b"workspace");
                    hasher.update(first_party_key);
                    base = Fingerprint(*hasher.finalize().as_bytes());
                }
                let fingerprint = match fixups.get(&node.name) {
                    None => base,
                    Some(fixup) => {
                        let mut hasher = blake3::Hasher::new();
                        hasher.update(&base.0);
                        hasher.update(b"fixup");
                        hasher.update(
                            &serde_json::to_vec(fixup).expect("a fixup serializes to JSON"),
                        );
                        Fingerprint(*hasher.finalize().as_bytes())
                    }
                };
                (node.package_id.canonicalize(workspace_root), fingerprint)
            })
            .collect();
        let fixup_rustc_flags = fixups
            .iter()
            .filter(|(_, fixup)| !fixup.rustc_flags.is_empty())
            .map(|(name, fixup)| (name.clone(), fixup.rustc_flags.clone()))
            .collect();
        Self {
            fingerprints,
            version: CACHE_VERSION,
            regenerate_all: false,
            fixup_rustc_flags,
        }
    }

    pub fn new_empty() -> Self {
        Self {
            fingerprints: BTreeMap::new(),
            version: CACHE_VERSION,
            regenerate_all: false,
            fixup_rustc_flags: BTreeMap::new(),
        }
    }

    /// Mark this cache as an inventory rebuilt from metadata, not a record of
    /// generated rules: diffing against it regenerates every surviving package.
    pub fn into_unverified_inventory(mut self) -> Self {
        self.regenerate_all = true;
        self
    }

    /// Whether this is an inventory whose packages must all be regenerated.
    pub fn is_unverified_inventory(&self) -> bool {
        self.regenerate_all
    }

    pub fn load() -> Result<Self, Error> {
        let cache_path = get_cache_path().unwrap_or_exit_ctx("failed to get cache path");
        if !cache_path.exists() {
            return Err(anyhow!("Cache file does not exist"));
        }
        let content = std::fs::read_to_string(&cache_path)?;
        let cache = toml::from_str::<BuckalCache>(&content)
            .map_err(|e| anyhow!("Failed to parse cache file: {}", e))?;
        if cache.version > CACHE_VERSION {
            buckal_error!(
                "Cache was written by a newer version of Buckal (found v{}, expected v{}). Please upgrade Buckal.",
                cache.version,
                CACHE_VERSION
            );
            std::process::exit(1);
        }
        if cache.version < CACHE_VERSION {
            return Err(anyhow!(
                "Cache version is stale (found {}, expected {})",
                cache.version,
                CACHE_VERSION
            ));
        }
        Ok(cache)
    }

    pub fn save(&self) {
        let cache_path = get_cache_path().unwrap_or_exit();
        let content = toml::to_string_pretty(self).unwrap_or_exit();
        let comment = "# @generated by `cargo buckal`\n# Not intended for manual editing.";
        std::fs::write(cache_path, format!("{}\n{}", comment, content)).unwrap_or_exit();
    }

    pub fn diff(&self, other: &BuckalCache, workspace_root: &Utf8PathBuf) -> BuckalChange {
        let mut _diff = BuckalChange {
            previous_fixup_rustc_flags: other.fixup_rustc_flags.clone(),
            ..Default::default()
        };
        for (id, fp) in &self.fingerprints {
            if let Some(other_fp) = other.fingerprints.get(id) {
                if fp != other_fp || other.regenerate_all {
                    _diff
                        .changes
                        .insert(id.resolve(workspace_root), ChangeType::Changed);
                }
            } else {
                // new package added in self
                _diff
                    .changes
                    .insert(id.resolve(workspace_root), ChangeType::Added);
            }
        }
        for id in other.fingerprints.keys() {
            if !self.fingerprints.contains_key(id) {
                // redundant package removed in self
                _diff
                    .changes
                    .insert(id.resolve(workspace_root), ChangeType::Removed);
            }
        }
        _diff
    }
}

#[derive(Debug, Default)]
pub struct BuckalChange {
    pub changes: BTreeMap<PackageId, ChangeType>,
    /// The fixup `rustc_flags` the previous generation added, by package name.
    pub previous_fixup_rustc_flags: BTreeMap<String, Vec<String>>,
}

impl BuckalChange {
    /// Add `other`'s changes for packages this one does not change already.
    pub fn merge(&mut self, other: BuckalChange) {
        for (id, change) in other.changes {
            self.changes.entry(id).or_insert(change);
        }
        for (name, flags) in other.previous_fixup_rustc_flags {
            self.previous_fixup_rustc_flags.entry(name).or_insert(flags);
        }
    }
}

#[derive(Debug)]
pub enum ChangeType {
    Added,
    Removed,
    Changed,
}
