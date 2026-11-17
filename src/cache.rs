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
/// Version 8: rules change from an unchanged resolution (headers, the library's own
///            context for proc-macro unit tests, binary unit tests, build-script
///            output on tests, kept dev edges, ...). A version 6 or 7 cache certifies
///            rules this version would not write, so every package is regenerated;
///            its record of the fixups that generation applied stays valid and is
///            kept for `--merge`.
/// Version 9: rules change from an unchanged resolution again: packages that no
///            configured platform builds are incompatible, target compilations are
///            incompatible where their resolution lacks a platform, and binaries
///            only the tests enable are emitted only for integration tests. A
///            version 6 to 8 cache is stale in the same way.
/// Version 10: rules change from an unchanged resolution again: no test variant of
///            a library the tests do not build, binaries and tests link only a
///            Rust library, a path dependency outside the workspace gets its
///            library only, proc-macro unit tests get `proc_macro`, and a
///            host-only library the tests build for the target gets a test
///            variant. A version 9 cache certifies rules without these.
/// Version 11: a package's dev-dependency on itself links its library into its
///            unit tests (a dev edge of the tests' resolution to itself). A version
///            10 cache certifies rules without it.
/// Version 12: a library host compilations use through its plain rule is not
///            marked incompatible on a platform (Fable N6).
/// Version 13: no test variant from a build dependency's target test variant.
/// Version 14: `export_files` rules carry the `buckal-export-files` label.
/// Version 15: a proc-macro package no configured platform builds gets no rules (its
///            prelude alias cannot resolve an execution platform under a wildcard).
/// Version 16: with configured platforms, no `compatible_with` from the static package
///            table (Buck2 refuses it beside `target_compatible_with`).
///
/// Upstream's version 5 (path-source ids canonicalised through their `file://` URL,
/// #102) is adopted here without a version of its own: on a path Cargo does not
/// escape the keys are unchanged, and elsewhere a cache's absolute keys only make
/// that generation regenerate (path sources are never removed).
///
/// Migration strategy:
/// - If found < expected (stale cache from older Buckal): ignore the old cache and rebuild.
/// - If found > expected (cache from newer Buckal): exit immediately and prompt the user to upgrade.
const CACHE_VERSION: u32 = 16;

/// The oldest cache version whose record of the fixups a generation applied
/// (`fixup_rustc_flags`, `fixups`) is still read, from a cache whose rules are
/// outdated.
const OLDEST_RECORD_VERSION: u32 = 6;

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

/// Placeholder written into `buckal.snap` in place of the absolute workspace
/// root, so the cache file is portable across checkouts at different paths.
const WORKSPACE_ID_PLACEHOLDER: &str = "path+file://($WORKSPACE)";

/// The `path+file://…` package-id prefix Cargo emits for crates under
/// `workspace_root`.
///
/// `cargo metadata` reports `workspace_root` as a native filesystem path, but
/// path-source package ids embed it as a `file://` URL — forward slashes, a
/// leading slash before a Windows drive letter, percent-encoding for characters
/// like spaces. Building the prefix via [`url::Url::from_file_path`], the same
/// conversion Cargo itself uses, keeps the two in lockstep on every platform
/// (the previous hand-rolled substring match silently failed on Windows and on
/// any path containing escaped characters).
fn workspace_id_prefix(workspace_root: &Utf8PathBuf) -> String {
    let url = url::Url::from_file_path(workspace_root.as_std_path())
        .expect("workspace_root from `cargo metadata` is always an absolute path");
    format!("path+{url}")
}

/// Whether `rest` — what follows a workspace-root prefix in a package-id repr —
/// is a real boundary (a subpath `/…` or the version tag `#…`) rather than the
/// prefix merely being a substring of a sibling directory's path.
fn is_id_boundary(rest: &str) -> bool {
    rest.starts_with('/') || rest.starts_with('#')
}

pub trait PackageIdExt {
    /// ($WORKSPACE) → workspace_root
    fn resolve(&self, workspace_root: &Utf8PathBuf) -> Self;

    /// workspace_root → ($WORKSPACE)
    fn canonicalize(&self, workspace_root: &Utf8PathBuf) -> Self;
}

impl PackageIdExt for PackageId {
    fn resolve(&self, workspace_root: &Utf8PathBuf) -> Self {
        match self.repr.strip_prefix(WORKSPACE_ID_PLACEHOLDER) {
            Some(rest) if is_id_boundary(rest) => PackageId {
                repr: format!("{}{rest}", workspace_id_prefix(workspace_root)),
            },
            _ => self.clone(),
        }
    }

    fn canonicalize(&self, workspace_root: &Utf8PathBuf) -> Self {
        let prefix = workspace_id_prefix(workspace_root);
        match self.repr.strip_prefix(prefix.as_str()) {
            Some(rest) if is_id_boundary(rest) => PackageId {
                repr: format!("{WORKSPACE_ID_PLACEHOLDER}{rest}"),
            },
            _ => self.clone(),
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
    /// Loaded from disk: a record of a previous generation, whose fixup flags
    /// (`fixup_rustc_flags`) are known.
    #[serde(skip)]
    recorded: bool,
    /// Loaded from an older cache version: it records what that generation applied,
    /// but its fingerprints do not certify rules this version writes, so every
    /// package is regenerated.
    #[serde(skip)]
    rules_outdated: bool,
    /// The fixup `rustc_flags` this generation added, by package name, so that a
    /// later `--merge` can tell them from manual edits once they change or go away.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    fixup_rustc_flags: BTreeMap<String, Vec<String>>,
    /// The fixups this generation applied, by package name, so that a later
    /// `--merge` can tell their set-valued contributions (dependencies,
    /// compatibility, environment) from manual edits once they change or go away.
    /// `None` in a cache written before this was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fixups: Option<BTreeMap<String, PackageFixup>>,
    /// The host compilations that resolved differently per configured platform in
    /// this generation (see `BuckalContext::host_platform_differences`). `None`
    /// without `feature_platforms`, and in a cache written before it was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host_platform_differences: Option<Vec<String>>,
    /// A digest of each recorded entry's resolution (see
    /// `BuckalContext::host_platform_difference_digests`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host_platform_difference_digests: Option<BTreeMap<String, String>>,
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
    /// change first-party packages' rules, with `workspace_inputs`: what else of
    /// the workspace reaches every first-party package's rules (the declared test
    /// environments), serialized; empty when nothing does.
    pub fn from_resolve_with_config(
        resolve: &BuckalResolve,
        workspace_root: &Utf8PathBuf,
        config: &RepoConfig,
        workspace_inputs: &[u8],
    ) -> Self {
        let mut key = config.first_party_generation_key();
        if !workspace_inputs.is_empty() {
            key.extend(b"workspace_inputs");
            key.extend(workspace_inputs);
        }
        Self::from_resolve_with_options(
            resolve,
            workspace_root,
            &config.patch,
            &config.fixups,
            &key,
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
            recorded: false,
            rules_outdated: false,
            fixup_rustc_flags,
            fixups: Some(fixups.clone()),
            host_platform_differences: None,
            host_platform_difference_digests: None,
        }
    }

    pub fn new_empty() -> Self {
        Self {
            fingerprints: BTreeMap::new(),
            version: CACHE_VERSION,
            regenerate_all: false,
            recorded: false,
            rules_outdated: false,
            fixup_rustc_flags: BTreeMap::new(),
            fixups: None,
            host_platform_differences: None,
            host_platform_difference_digests: None,
        }
    }

    /// Record the host compilations that resolve differently per platform.
    pub fn with_host_platform_differences(
        mut self,
        differences: Option<Vec<String>>,
        digests: Option<BTreeMap<String, String>>,
    ) -> Self {
        self.host_platform_differences = differences;
        self.host_platform_difference_digests = digests;
        self
    }

    /// The recorded digests of the entries' resolutions, if recorded.
    pub fn host_platform_difference_digests(&self) -> Option<&BTreeMap<String, String>> {
        self.recorded
            .then_some(self.host_platform_difference_digests.as_ref())
            .flatten()
    }

    /// The recorded host compilations that resolve differently per platform, if
    /// this is a record of a generation that kept them.
    pub fn host_platform_differences(&self) -> Option<&[String]> {
        self.recorded
            .then_some(self.host_platform_differences.as_deref())
            .flatten()
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
        let mut cache = toml::from_str::<BuckalCache>(&content)
            .map_err(|e| anyhow!("Failed to parse cache file: {}", e))?;
        if cache.version > CACHE_VERSION {
            buckal_error!(
                "Cache was written by a newer version of Buckal (found v{}, expected v{}). Please upgrade Buckal.",
                cache.version,
                CACHE_VERSION
            );
            std::process::exit(1);
        }
        if cache.version < OLDEST_RECORD_VERSION {
            return Err(anyhow!(
                "Cache version is stale (found {}, expected {})",
                cache.version,
                CACHE_VERSION
            ));
        }
        cache.rules_outdated = cache.version < CACHE_VERSION;
        cache.recorded = true;
        Ok(cache)
    }

    pub fn save(&self) {
        let cache_path = get_cache_path().unwrap_or_exit();
        let content = toml::to_string_pretty(self).unwrap_or_exit();
        let comment = "# @generated by `cargo buckal`\n# Not intended for manual editing.";
        std::fs::write(cache_path, format!("{}\n{}", comment, content)).unwrap_or_exit();
    }

    /// The fixup `rustc_flags` of the generation this cache records, if it records
    /// one (an inventory or an empty cache does not).
    pub fn previous_fixup_record(&self) -> Option<BTreeMap<String, Vec<String>>> {
        (self.recorded && !self.regenerate_all).then(|| self.fixup_rustc_flags.clone())
    }

    /// The fixups of the generation this cache records, if it records them.
    pub fn previous_fixups_record(&self) -> Option<BTreeMap<String, PackageFixup>> {
        (self.recorded && !self.regenerate_all)
            .then(|| self.fixups.clone())
            .flatten()
    }

    pub fn diff(&self, other: &BuckalCache, workspace_root: &Utf8PathBuf) -> BuckalChange {
        let mut _diff = BuckalChange {
            previous_fixup_rustc_flags: other.previous_fixup_record(),
            previous_fixups: other.previous_fixups_record(),
            ..Default::default()
        };
        for (id, fp) in &self.fingerprints {
            if let Some(other_fp) = other.fingerprints.get(id) {
                if fp != other_fp || other.regenerate_all || other.rules_outdated {
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
    /// The fixup `rustc_flags` the previous generation added, by package name, when
    /// a cache records them (`None`: unknown, so `--merge` cannot tell them from
    /// manual edits).
    pub previous_fixup_rustc_flags: Option<BTreeMap<String, Vec<String>>>,
    /// The fixups the previous generation applied, by package name, when a cache
    /// records them (`None`: unknown, so `--merge` cannot tell their set-valued
    /// contributions from manual edits).
    pub previous_fixups: Option<BTreeMap<String, PackageFixup>>,
}

impl BuckalChange {
    /// Add `other`'s changes for packages this one does not change already.
    pub fn merge(&mut self, other: BuckalChange) {
        for (id, change) in other.changes {
            self.changes.entry(id).or_insert(change);
        }
        if self.previous_fixup_rustc_flags.is_none() {
            self.previous_fixup_rustc_flags = other.previous_fixup_rustc_flags;
        }
        if self.previous_fixups.is_none() {
            self.previous_fixups = other.previous_fixups;
        }
    }
}

#[derive(Debug)]
pub enum ChangeType {
    Added,
    Removed,
    Changed,
}

#[cfg(test)]
mod tests {
    use super::PackageIdExt;
    use cargo_metadata::{PackageId, camino::Utf8PathBuf};

    // `cargo metadata` reports `workspace_root` as a *native* filesystem path,
    // but a path-source package id embeds it as the body of a `file://` *URL*:
    // forward slashes throughout, a leading slash before a Windows drive letter,
    // and percent-encoding for characters like spaces. `canonicalize` / `resolve`
    // must bridge the two so `buckal.snap` stores the path-independent
    // `($WORKSPACE)` placeholder. On Unix a simple absolute path happens to equal
    // its URL body, so the bug only surfaces on Windows — except for paths
    // containing characters cargo percent-encodes (`C:\Users\Jane Doe\…`), which
    // break the bare-substring approach on every platform.
    //
    // The shapes below are picked per host because `url::Url::from_file_path`
    // (used by the fix, and by Cargo itself) only accepts a path that is
    // absolute *on the current OS*.

    fn ws() -> Utf8PathBuf {
        if cfg!(windows) {
            Utf8PathBuf::from(r"D:\proj")
        } else {
            Utf8PathBuf::from("/work/proj")
        }
    }

    /// The `path+file://…` prefix Cargo emits for path crates under [`ws`].
    fn ws_prefix() -> &'static str {
        if cfg!(windows) {
            "path+file:///D:/proj"
        } else {
            "path+file:///work/proj"
        }
    }

    fn ws_spaced() -> Utf8PathBuf {
        if cfg!(windows) {
            Utf8PathBuf::from(r"D:\Code\Jane Doe\proj")
        } else {
            Utf8PathBuf::from("/work/Jane Doe/proj")
        }
    }

    fn ws_spaced_prefix() -> &'static str {
        if cfg!(windows) {
            "path+file:///D:/Code/Jane%20Doe/proj"
        } else {
            "path+file:///work/Jane%20Doe/proj"
        }
    }

    #[test]
    fn canonicalize_workspace_internal_path_crate() {
        let id = PackageId {
            repr: format!("{}/crates/foo#0.1.0", ws_prefix()),
        };
        assert_eq!(
            id.canonicalize(&ws()).repr,
            "path+file://($WORKSPACE)/crates/foo#0.1.0",
            "a path crate under the workspace root must be rewritten to ($WORKSPACE)"
        );
    }

    #[test]
    fn resolve_reproduces_cargo_metadata_repr() {
        let canon = PackageId {
            repr: "path+file://($WORKSPACE)/crates/foo#0.1.0".to_string(),
        };
        assert_eq!(
            canon.resolve(&ws()).repr,
            format!("{}/crates/foo#0.1.0", ws_prefix()),
            "resolve must reproduce the exact repr `cargo metadata` emits"
        );
    }

    #[test]
    fn canonicalize_handles_workspace_root_package_itself() {
        let id = PackageId {
            repr: format!("{}#0.7.0", ws_prefix()),
        };
        assert_eq!(
            id.canonicalize(&ws()).repr,
            "path+file://($WORKSPACE)#0.7.0"
        );
    }

    #[test]
    fn canonicalize_handles_percent_encoded_workspace_path() {
        // A workspace under a directory whose name contains a space: Cargo
        // percent-encodes it in the package id, so a literal substring match on
        // the native path fails on every platform.
        let id = PackageId {
            repr: format!("{}/crates/foo#0.1.0", ws_spaced_prefix()),
        };
        assert_eq!(
            id.canonicalize(&ws_spaced()).repr,
            "path+file://($WORKSPACE)/crates/foo#0.1.0"
        );
        let back = PackageId {
            repr: "path+file://($WORKSPACE)/crates/foo#0.1.0".to_string(),
        };
        assert_eq!(
            back.resolve(&ws_spaced()).repr,
            format!("{}/crates/foo#0.1.0", ws_spaced_prefix())
        );
    }

    #[test]
    fn canonicalize_leaves_non_workspace_path_crate_untouched() {
        // A path dep that lives *outside* the workspace must be left alone — the
        // ($WORKSPACE) placeholder can't express it. Guards against over-reach.
        let outside = if cfg!(windows) {
            "path+file:///D:/other/lib/bar#0.1.0"
        } else {
            "path+file:///work/other/lib/bar#0.1.0"
        };
        let id = PackageId {
            repr: outside.to_string(),
        };
        assert_eq!(id.canonicalize(&ws()).repr, id.repr);
    }

    #[test]
    fn canonicalize_leaves_registry_id_untouched() {
        let id = PackageId {
            repr: "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0".to_string(),
        };
        assert_eq!(id.canonicalize(&ws()).repr, id.repr);
    }

    #[test]
    fn canonicalize_does_not_match_sibling_directory() {
        // `<root>` must not be matched as a bare substring of a sibling directory
        // that merely shares a name prefix (`proj` vs `proj_sidecar`).
        let id = PackageId {
            repr: format!("{}_sidecar/lib/bar#0.1.0", ws_prefix()),
        };
        assert_eq!(id.canonicalize(&ws()).repr, id.repr);
    }
}
