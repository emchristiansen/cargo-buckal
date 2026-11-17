use std::collections::{BTreeMap as Map, BTreeSet as Set};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{
    buckal_warn,
    utils::{UnwrapOrExit, get_buck2_root},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(
        default = "default_buck2_binary",
        skip_serializing_if = "is_default_buck2_binary"
    )]
    pub buck2_binary: String,
    #[serde(default, skip_serializing_if = "RegistryDefault::if_skip")]
    pub registry: RegistryDefault,
    #[serde(default = "default_registries", skip_serializing_if = "Map::is_empty")]
    pub registries: Map<String, RegistryEntry>,
}

fn is_default_buck2_binary(value: &str) -> bool {
    value == "buck2"
}

fn default_buck2_binary() -> String {
    "buck2".to_string()
}

fn default_registries() -> Map<String, RegistryEntry> {
    let mut registries = Map::new();
    registries.insert(
        "buck2hub".to_string(),
        RegistryEntry {
            base: "https://app.buck2hub.com".to_string(),
            api: "https://git.buck2hub.com".to_string(),
            token: None,
        },
    );
    registries
}

impl Default for Config {
    fn default() -> Self {
        Self {
            buck2_binary: default_buck2_binary(),
            registry: RegistryDefault::default(),
            registries: default_registries(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RegistryDefault {
    pub default: Option<String>,
}

impl RegistryDefault {
    pub fn if_skip(&self) -> bool {
        self.default.is_none()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub base: String,
    pub api: String,
    pub token: Option<String>,
}

impl Config {
    /// Load configuration from ~/.config/buckal/config.toml
    pub fn load() -> Self {
        let config_path = Self::config_path();

        if !config_path.exists() {
            return Self::default();
        }

        match fs::read_to_string(&config_path) {
            Ok(content) => match toml::from_str::<Config>(&content) {
                Ok(config) => config,
                Err(_) => {
                    eprintln!(
                        "Warning: Failed to parse config file at {}, using defaults",
                        config_path.display()
                    );
                    Self::default()
                }
            },
            Err(_) => {
                eprintln!(
                    "Warning: Failed to read config file at {}, using defaults",
                    config_path.display()
                );
                Self::default()
            }
        }
    }

    /// Save configuration to ~/.config/buckal/config.toml
    pub fn save(&self) -> Result<()> {
        let config_path = Self::config_path();
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;

        // Write with owner-only permissions (0600 on Unix)
        // Following Cargo's approach: Unix gets 0600, other platforms use default permissions
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&config_path)?;

        file.write_all(content.as_bytes())?;

        // Set permissions after writing (Unix only)
        set_permissions(&file)?;

        Ok(())
    }

    /// Get the configuration file path
    pub fn config_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home)
            .join(".config")
            .join("buckal")
            .join("config.toml")
    }

    /// Get the default registry name, or "buck2hub" if not set
    pub fn default_registry(&self) -> &str {
        self.registry.default.as_deref().unwrap_or("buck2hub")
    }
}

/// Set file permissions to owner-only (Unix only, following Cargo's approach)
#[cfg(unix)]
fn set_permissions(file: &File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut perms = file.metadata()?.permissions();
    perms.set_mode(0o600);
    file.set_permissions(perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_permissions(_file: &File) -> Result<()> {
    // On non-Unix platforms, rely on default file system permissions
    // This is the same approach used by Cargo
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RepoConfig {
    pub align_cells: bool,
    pub ignore_tests: bool,
    pub patch_fields: Set<String>,
    pub patch: RepoPatchConfig,
    /// Per-package additions to the generated rules, keyed by package name
    /// (`[fixups.<package>]`). Applies to every version of that package.
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub fixups: Map<String, PackageFixup>,
    /// Platforms to resolve features for separately, as Reindeer platform name
    /// (the keys of the prelude's `platform` attribute, e.g. `linux-x86_64`) to Rust
    /// target triple. When set, each package's features are resolved per platform with
    /// Cargo's own resolver and emitted through `platform = {...}`, instead of the union
    /// over all platforms that `cargo metadata` reports.
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub feature_platforms: Map<String, String>,
    /// Drop dev-dependency edges that would close a cycle (Cargo allows such cycles;
    /// a strictly acyclic graph is required here) instead of refusing the workspace.
    /// Each dropped edge is reported; the dependent's tests lose that dev-dependency.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub skip_dev_dependency_cycles: bool,
}

impl Default for RepoConfig {
    fn default() -> Self {
        Self {
            align_cells: false,
            ignore_tests: true,
            patch_fields: Set::new(),
            patch: RepoPatchConfig::default(),
            fixups: Map::new(),
            feature_platforms: Map::new(),
            skip_dev_dependency_cycles: false,
        }
    }
}

/// Knowledge about a package that Cargo metadata does not carry, such as how its
/// build script finds a system library (e.g. a pinned OpenSSL instead of a vendored
/// copy). Added to the generated rules on every regeneration, so no generated BUCK
/// file needs a hand edit.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PackageFixup {
    /// Environment for running the package's build script (`buildscript_run`).
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub buildscript_env: Map<String, String>,
    /// Environment for compiling the package's crates (library, binaries, tests).
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub env: Map<String, String>,
    /// Extra rustc flags for the package's crates (library, binaries, tests).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rustc_flags: Vec<String>,
    /// Extra compile inputs for the package's crates, as the prelude's `mapped_srcs`:
    /// a label (e.g. an `export_file` of a nested package, or of another package
    /// through its `export_files` fixup) to the file's path, written relative to the
    /// package directory and possibly climbing out of it with `..`, as an include
    /// resolves it. For files that `include!`/`include_str!`/`include_bytes!` reads
    /// but the package glob does not provide. Emitted as the repository path; a
    /// destination that is absolute, leaves the repository, repeats or lands on a
    /// file of the package's own sources is refused.
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub mapped_srcs: Map<String, String>,
    /// Extra run-time inputs of the package's tests, as labels (e.g. an `export_file`
    /// or `filegroup` of files a test reads outside its own package). The package's
    /// own files are always test resources; these add files from elsewhere. Emitted
    /// as the tests' `extra_resources`, which the prelude adds to `resources` at
    /// their project paths.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub test_resources: Vec<String>,
    /// How the package's tests execute: the name of a profile on the remote test
    /// execution toolchain, or `"disabled"`. Emitted as the `remote_execution` of
    /// every test target of the package. For tests that need a worker capability
    /// that other tests do not, such as a particular fixture filesystem.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_remote_execution: Option<String>,
    /// Environment for running the package's tests, not for compiling them. Added to
    /// the `run_env` of every test target of the package. For values that the
    /// execution profile promises the test, such as the path of a worker-provided
    /// fixture root.
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub test_run_env: Map<String, String>,
    /// A label providing the prelude's `RuntimeToolsInfo` (a pinned tools directory)
    /// for the package's tests that run tools at run time. Emitted as the
    /// `runtime_tools` of every test target of the package.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_runtime_tools: Option<String>,
    /// Files of a first-party package to export to other packages, as paths relative
    /// to the package directory (e.g. a manifest another package's test reads). Each
    /// becomes `export_file(name = <path>, src = <path>, mode = "reference")` in the
    /// package's BUCK file, which generation would otherwise rewrite without it. A
    /// path must name an existing file inside the package (not in a nested Buck
    /// package). An entry is the path, visible to everything (`PUBLIC`), or
    /// `{ path = "<path>", visibility = ["<pattern>", ...] }`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub export_files: Vec<ExportFileEntry>,
}

/// An entry of `[fixups.<package>] export_files`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum ExportFileEntry {
    /// The path alone, exported with `visibility = ["PUBLIC"]`.
    Path(String),
    /// The path with an explicit visibility.
    Spec(ExportFileSpec),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExportFileSpec {
    pub path: String,
    /// The `export_file`'s visibility patterns; `PUBLIC` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<Vec<String>>,
}

impl ExportFileEntry {
    pub fn path(&self) -> &str {
        match self {
            ExportFileEntry::Path(path) => path,
            ExportFileEntry::Spec(spec) => &spec.path,
        }
    }

    /// The visibility to emit: as specified, else `PUBLIC`.
    pub fn visibility(&self) -> Set<String> {
        match self {
            ExportFileEntry::Spec(ExportFileSpec {
                visibility: Some(visibility),
                ..
            }) => visibility.iter().cloned().collect(),
            _ => Set::from(["PUBLIC".to_owned()]),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RepoPatchConfig {
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub version: Map<String, VersionPatch>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VersionPatch {
    pub from: String,
    pub to: String,
}

impl RepoConfig {
    pub fn load() -> Self {
        let repo_config_path = Self::repo_config_path();

        if !repo_config_path.exists() {
            return Self::default();
        }

        match fs::read_to_string(&repo_config_path) {
            Ok(content) => match toml::from_str::<RepoConfig>(&content) {
                Ok(config) => config,
                Err(_) => {
                    buckal_warn!(
                        "Failed to parse repo config file at {}, using defaults",
                        repo_config_path.display()
                    );
                    Self::default()
                }
            },
            Err(_) => {
                buckal_warn!(
                    "Failed to read repo config file at {}, using defaults",
                    repo_config_path.display()
                );
                Self::default()
            }
        }
    }

    pub fn repo_config_path() -> PathBuf {
        let buck2_root = get_buck2_root().unwrap_or_exit();
        buck2_root.join("buckal.toml").into()
    }
}

#[cfg(test)]
mod tests {
    use super::{ExportFileEntry, RepoConfig, Set};

    #[test]
    fn test_repo_config_deserializes_version_patch() {
        let config: RepoConfig = toml::from_str(
            r#"
                [patch.version]
                pyo3 = { from = "0.26.0", to = "0.27.2" }
            "#,
        )
        .expect("failed to deserialize repo config");

        let patch = config
            .patch
            .version
            .get("pyo3")
            .expect("missing pyo3 patch");

        assert_eq!(patch.from, "0.26.0");
        assert_eq!(patch.to, "0.27.2");
    }

    #[test]
    fn test_repo_config_deserializes_fixups() {
        let config: RepoConfig = toml::from_str(
            r#"
                [fixups.openssl-sys]
                buildscript_env = { OPENSSL_NO_VENDOR = "1", OPENSSL_LIB_DIR = "/opt/openssl/lib" }
                rustc_flags = ["-Lnative=/opt/openssl/lib"]
            "#,
        )
        .expect("failed to deserialize repo config");

        let fixup = config.fixups.get("openssl-sys").expect("missing fixup");
        assert_eq!(
            fixup
                .buildscript_env
                .get("OPENSSL_NO_VENDOR")
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(
            fixup
                .buildscript_env
                .get("OPENSSL_LIB_DIR")
                .map(String::as_str),
            Some("/opt/openssl/lib")
        );
        assert!(fixup.env.is_empty());
        assert_eq!(fixup.rustc_flags, ["-Lnative=/opt/openssl/lib"]);
    }

    #[test]
    fn test_repo_config_deserializes_fixup_mapped_srcs() {
        // Destinations are written relative to the package, as an include resolves
        // them (possibly with `../`); generation emits them as repository paths
        // (`text/glyph_vocab/vocab/bpe-52.toml`), see `buckify::rules`.
        let config: RepoConfig = toml::from_str(
            r#"
                [fixups.text_glyph_vocab.mapped_srcs]
                "//text/glyph_vocab/vocab:compiler_input/bpe-52.toml" = "vocab/bpe-52.toml"
                "//text/lexicon:words.txt" = "../lexicon/words.txt"
            "#,
        )
        .expect("failed to deserialize repo config");
        let fixup = config
            .fixups
            .get("text_glyph_vocab")
            .expect("missing fixup");
        assert_eq!(
            fixup
                .mapped_srcs
                .get("//text/glyph_vocab/vocab:compiler_input/bpe-52.toml")
                .map(String::as_str),
            Some("vocab/bpe-52.toml")
        );
        assert_eq!(
            fixup
                .mapped_srcs
                .get("//text/lexicon:words.txt")
                .map(String::as_str),
            Some("../lexicon/words.txt")
        );
    }

    #[test]
    fn test_repo_config_deserializes_fixup_export_files() {
        let config: RepoConfig = toml::from_str(
            r#"
                [fixups.ccc]
                export_files = [
                    "Cargo.toml",
                    { path = "data/a.json", visibility = ["//codegen/ccc:", "//conta/durable_turn:"] },
                    { path = "data/b.json" },
                ]
            "#,
        )
        .expect("failed to deserialize repo config");
        let entries = &config.fixups.get("ccc").unwrap().export_files;
        assert_eq!(
            entries
                .iter()
                .map(ExportFileEntry::path)
                .collect::<Vec<_>>(),
            ["Cargo.toml", "data/a.json", "data/b.json"]
        );
        let public = Set::from(["PUBLIC".to_owned()]);
        assert_eq!(entries[0].visibility(), public);
        assert_eq!(
            entries[1].visibility(),
            Set::from([
                "//codegen/ccc:".to_owned(),
                "//conta/durable_turn:".to_owned()
            ])
        );
        assert_eq!(entries[2].visibility(), public);
        // The visibility is part of the fixup's serialization, which the cache
        // fingerprints.
        assert_eq!(
            serde_json::to_string(&entries).unwrap(),
            r#"["Cargo.toml",{"path":"data/a.json","visibility":["//codegen/ccc:","//conta/durable_turn:"]},{"path":"data/b.json"}]"#
        );
        // An unknown key in an entry is refused.
        assert!(
            toml::from_str::<RepoConfig>(
                "[fixups.ccc]\nexport_files = [{ path = \"a\", visibilty = [] }]\n"
            )
            .is_err()
        );
    }

    #[test]
    fn test_repo_config_deserializes_fixup_test_execution() {
        let config: RepoConfig = toml::from_str(
            r#"
                [fixups.fixture_user]
                test_remote_execution = "fixture_fs_profile"
                test_run_env = { FIXTURE_ROOT = "/srv/fixtures" }
                test_runtime_tools = "//tools:runtime_tools"
            "#,
        )
        .expect("failed to deserialize repo config");

        let fixup = config.fixups.get("fixture_user").expect("missing fixup");
        assert_eq!(
            fixup.test_remote_execution.as_deref(),
            Some("fixture_fs_profile")
        );
        assert_eq!(
            fixup.test_run_env.get("FIXTURE_ROOT").map(String::as_str),
            Some("/srv/fixtures")
        );
        assert_eq!(
            fixup.test_runtime_tools.as_deref(),
            Some("//tools:runtime_tools")
        );
        // Run-time environment is not compile-time environment.
        assert!(fixup.env.is_empty());

        // Unset, they are absent from the fixup and from its serialization (which the
        // cache fingerprints), so existing fixups keep their fingerprints.
        let other: RepoConfig =
            toml::from_str("[fixups.other]\nenv = { A = \"1\" }\n").expect("deserialize");
        let other = other.fixups.get("other").unwrap();
        assert_eq!(other.test_remote_execution, None);
        assert!(other.test_run_env.is_empty());
        assert_eq!(other.test_runtime_tools, None);
        assert_eq!(
            serde_json::to_string(other).unwrap(),
            r#"{"env":{"A":"1"}}"#
        );
    }

    #[test]
    fn test_repo_config_skip_dev_dependency_cycles() {
        assert!(!RepoConfig::default().skip_dev_dependency_cycles);
        let config: RepoConfig =
            toml::from_str("skip_dev_dependency_cycles = true").expect("deserialize");
        assert!(config.skip_dev_dependency_cycles);
    }

    #[test]
    fn test_repo_config_rejects_unknown_fixup_key() {
        let result = toml::from_str::<RepoConfig>(
            r#"
                [fixups.openssl-sys]
                build_env = { OPENSSL_NO_VENDOR = "1" }
            "#,
        );
        assert!(result.is_err(), "a misspelt fixup key must not be ignored");
    }
}
