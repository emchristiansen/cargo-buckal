use std::collections::{BTreeMap as Map, BTreeSet as Set};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{
    buckal_error,
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
#[serde(default, deny_unknown_fields)]
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
    /// Emit `filegroup(name = "workspace_manifests")` in the workspace-root BUCK
    /// file: the workspace's `Cargo.toml` and `Cargo.lock` and every first-party
    /// package's `Cargo.toml`, each at its project path, for tests that read the
    /// whole workspace's manifests. Each package exports its manifest for it as
    /// `export_file(name = "Cargo.toml")`, visible to the root package (and as its
    /// `export_files` entry for `Cargo.toml` says, if it has one). The filegroup name
    /// is reserved: with this unset, generation removes such a filegroup.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub workspace_manifests: bool,
    /// Features the tests are built with: with `feature_platforms`, the tests are
    /// resolved as `cargo test --workspace --features <these>` resolves them, apart
    /// from the ordinary (production) build, which is resolved exactly as `cargo
    /// build --workspace` resolves it and never gets these features. A library the
    /// tests need with other features gets a `<lib>-test` variant that the tests,
    /// and binaries whose `required-features` only the tests enable, use.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub test_features: Vec<String>,
    /// `[platforms]`: roles of the configured `feature_platforms`.
    #[serde(skip_serializing_if = "PlatformsConfig::is_empty")]
    pub platforms: PlatformsConfig,
    /// `[wasm]`: wasm32 modules built from their own Cargo graph.
    #[serde(skip_serializing_if = "WasmConfig::is_empty")]
    pub wasm: WasmConfig,
    /// `[package_macro]`: one call of a repository macro in each package BUCK
    /// file generation writes, first-party, third-party or both, and in the
    /// generated `platforms/BUCK` beside the first-party ones; not in the
    /// workspace-root BUCK file of a virtual workspace, into which generation
    /// only splices rules. Optionally, a generated Starlark file of the
    /// repository's package tables (`tables`).
    #[serde(skip_serializing_if = "PackageMacroConfig::is_empty")]
    pub package_macro: PackageMacroConfig,
}

/// `[package_macro]` of `buckal.toml`: a macro of the repository's own (`bzl`,
/// `function`) that generation calls once, at package scope, in every BUCK file
/// it writes for a first-party package when `first_party` is set, with
/// `first_party_kwargs` as its keyword arguments, and in every third-party
/// package's BUCK file when `third_party` is set, with `third_party_kwargs`.
/// Third-party BUCK files are generated whole, so there the call is simply part
/// of the output. What the macro declares (for example a `filegroup` of the
/// package's files for a tree that stages every package) is the repository's
/// business: generation only places the call. It places none in the
/// workspace-root BUCK file of a virtual workspace, into which it only splices
/// rules; the root BUCK file of a root package is that package's, and gets the
/// call.
///
/// With `first_party` set, the generator-written `platforms/BUCK` (the
/// `platforms` asset, recognised by its `@generated` first line) gets the
/// first-party load and call too, with `first_party_kwargs`, under the same
/// takeover, marker and removal rules as a first-party BUCK file. Generation
/// keeps the rest of that file byte for byte: the load goes directly below its
/// last `load`, the call at its end. A hand-written statement it takes over
/// goes with its comments, as in a first-party BUCK file, except that the
/// comments above the file's first statement (its header) stay. A
/// `platforms/BUCK` without the `@generated` first line is the repository's own
/// and is left alone.
///
/// In a first-party BUCK file generation emits the macro's `load` and the call
/// and owns both:
/// - A hand-written call of `function` is replaced by the generated one, with a
///   warning if its arguments differ. The call that replaces it is marked like
///   any other generated call, so the takeover is one-way: once the table is
///   unset, generation removes it, and the file no longer calls the macro.
/// - A hand-written `load` of `bzl` that binds only `function` is dropped. One
///   that binds other names too is kept, and generation adds no second load;
///   once the table is unset, it still binds `function`, which nothing calls.
/// - A hand-written `load` that binds `function` to anything but `function` of
///   `bzl`, or binds `function` of `bzl` under another name, is refused: the
///   file is left unchanged and generation exits with an error.
///
/// Generation writes [`crate::buck::PACKAGE_MACRO_MARKER`] on the line directly
/// above the call and above its load. A later generation recognises a statement
/// it wrote by that line, and removes the call and its load once the table is
/// unset, or once the side that called the macro is switched off. A comment line
/// or a blank line written between the marker and the call unmarks the call,
/// which is then kept as hand-written; its marked load is then kept too, with a
/// warning, so the file never calls a name it no longer loads.
///
/// Labels are compared as text: `root//buck2:inputs.bzl` is not recognised as
/// the same file as `//buck2:inputs.bzl`, so a load through another spelling is
/// treated as a load of another file.
///
/// `tables` names a root-relative `.bzl` path at which generation writes the
/// repository's package tables (see `crate::buckify::tables`):
/// `GENERATED_WHOLE_REPOSITORY_PACKAGES`, each generated third-party package's
/// directory mapped to its package (`"<root cell>//<dir>:"`), and
/// `GENERATED_SOURCE_TREE_PACKAGES`, every other directory of the root cell that
/// holds a BUCK file (`"."` for the root). It is written on every generation,
/// cached or not, and `buckal.snap` records its path and a digest of its
/// content. Generation refuses to replace a file there that it did not write.
/// Once `tables` is unset or names another path, generation removes the file it
/// wrote before, with a warning. `tables` needs neither `bzl` nor `function`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "RawPackageMacroConfig")]
pub struct PackageMacroConfig {
    /// The `.bzl` file that defines the macro, as a `load` names it (e.g.
    /// `//buck2:inputs.bzl`).
    pub bzl: String,
    /// The macro's name in `bzl`, a Starlark identifier.
    pub function: String,
    /// Call the macro in first-party packages' BUCK files.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub first_party: bool,
    /// Keyword arguments of the first-party call: literal strings, integers and
    /// booleans.
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub first_party_kwargs: Map<String, MacroArg>,
    /// Call the macro in third-party packages' BUCK files.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub third_party: bool,
    /// Keyword arguments of the third-party call, as `first_party_kwargs`.
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub third_party_kwargs: Map<String, MacroArg>,
    /// The root-relative path of the generated package tables file, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tables: Option<String>,
}

/// A literal keyword argument of a [`PackageMacroConfig`] call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum MacroArg {
    Bool(bool),
    Int(i64),
    Str(String),
}

/// [`PackageMacroConfig`] as written, before it is checked.
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawPackageMacroConfig {
    bzl: String,
    function: String,
    first_party: bool,
    first_party_kwargs: Map<String, MacroArg>,
    third_party: bool,
    third_party_kwargs: Map<String, MacroArg>,
    tables: Option<String>,
}

/// Why `path` cannot be the `[package_macro] tables` file: it must be a
/// root-relative path to a `.bzl` file, written with `/`, with no empty, `.` or
/// `..` component.
fn tables_path_error(path: &str) -> Option<String> {
    let components: Vec<&str> = path.split('/').collect();
    let bad_component = components
        .iter()
        .any(|c| c.is_empty() || *c == "." || *c == "..");
    if bad_component || path.contains(['\\', '"', '\n']) || !path.ends_with(".bzl") {
        return Some(format!(
            "[package_macro] tables must be a root-relative path to a .bzl file (e.g. \"buckal_packages.bzl\"), written with `/` and without `.` or `..` components, not {path:?}"
        ));
    }
    None
}

/// Whether `word` is a Starlark identifier.
fn is_identifier(word: &str) -> bool {
    let mut chars = word.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl TryFrom<RawPackageMacroConfig> for PackageMacroConfig {
    type Error = String;

    fn try_from(raw: RawPackageMacroConfig) -> Result<Self, Self::Error> {
        let config = PackageMacroConfig {
            bzl: raw.bzl,
            function: raw.function,
            first_party: raw.first_party,
            first_party_kwargs: raw.first_party_kwargs,
            third_party: raw.third_party,
            third_party_kwargs: raw.third_party_kwargs,
            tables: raw.tables,
        };
        if let Some(error) = config.tables.as_deref().and_then(tables_path_error) {
            return Err(error);
        }
        // `tables` alone names no macro.
        let macro_named = PackageMacroConfig {
            tables: None,
            ..config.clone()
        };
        if macro_named.is_empty() {
            return Ok(config);
        }
        if !config.bzl.ends_with(".bzl") || config.bzl.contains(['"', '\\', '\n']) {
            return Err(format!(
                "[package_macro] bzl must name a .bzl file as a load does, not {:?}",
                config.bzl
            ));
        }
        if !is_identifier(&config.function) {
            return Err(format!(
                "[package_macro] function must be a Starlark identifier, not {:?}",
                config.function
            ));
        }
        if let Some(key) = config
            .first_party_kwargs
            .keys()
            .chain(config.third_party_kwargs.keys())
            .find(|k| !is_identifier(k))
        {
            return Err(format!(
                "[package_macro] keyword argument {key:?} is not a Starlark identifier"
            ));
        }
        Ok(config)
    }
}

impl PackageMacroConfig {
    /// Whether the table is absent (nothing set).
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The call's keyword arguments in first-party packages, or `None` when the
    /// macro is not called there.
    pub fn first_party_call(&self) -> Option<&Map<String, MacroArg>> {
        self.first_party.then_some(&self.first_party_kwargs)
    }

    /// The call's keyword arguments in third-party packages, or `None` when the
    /// macro is not called there.
    pub fn third_party_call(&self) -> Option<&Map<String, MacroArg>> {
        self.third_party.then_some(&self.third_party_kwargs)
    }
}

/// `[wasm]` of `buckal.toml`: the first-party packages whose
/// `[package.metadata.buck2.wasm]` (`features`, `profile`) generation builds as a
/// wasm32 cdylib, each from its own Cargo graph (`cargo build -p <package>
/// --target wasm32-unknown-unknown --features <features>`) with that Cargo
/// profile's flags. Every library of the graph gets a `<lib>-wasm` variant; the
/// package gets `configured_alias(name = "lib-wasm-wasm32")` configured for
/// `platform`, whose `[cdylib]` is the module. A test whose dependency closure
/// reaches a package declaring `[package.metadata.buck2.test-env]` `VAR = { wasm =
/// "<package>" }` gets `VAR = "$(location <package>:lib-wasm-wasm32[cdylib])"` in
/// its `run_env`.
///
/// A package also builds each `[package.metadata.buck2.wasm-variants.<name>]`
/// (`features`, required and exact; `profile`, the module's when not given) as a
/// module of its own graph: `<lib>-wasm-<package>.<name>` variants and
/// `configured_alias(name = "lib-wasm-wasm32.<name>")`, named by `variant =
/// "<name>"` beside `wasm`. `{ wasm_sha256 = "<package>" }` (with an optional
/// `variant`) names the file holding the module's lowercase hex SHA-256, which the
/// buckal cell's `wasm_sha256` (`@buckal//:wasm_digest.bzl`) computes as
/// `<alias>-sha256`, emitted only for a module a declaration names this way.
///
/// `test_packages` lists the first-party packages whose library unit tests are
/// compiled for wasm32 and run in a browser, each declaring
/// `[package.metadata.buck2.wasm-test]` (and optionally
/// `[package.metadata.buck2.wasm-test-variants.<name>]`): each with exactly its
/// declared `features`, from Cargo's own `cargo test -p <package> --lib --features
/// <features> --target wasm32-unknown-unknown` graph. `--lib` names the library
/// explicitly, so a listed package with `[lib] test = false` still gets its browser
/// test (and no native `unittest`). See `crate::wasm` for what is emitted.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WasmConfig {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<String>,
    /// The wasm32 execution target platform, e.g.
    /// `root//buck2/platforms/wasm:wasm32_unknown_unknown`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// Packages whose library unit tests run in a browser (`wasm-test`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub test_packages: Vec<String>,
    /// The target triple the modules are built for: `wasm32-unknown-unknown`
    /// ([`crate::wasm::WASM_TRIPLE`]) when not given, or another wasm32 triple such
    /// as `wasm32-wasip1` for a standalone universe's guest modules. Browser tests
    /// (`test_packages`) are wasm-bindgen modules and need the default triple.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub triple: Option<String>,
    /// Module keys (`<package>` or `<package>.<variant>`) whose SHA-256 witness
    /// (`<alias>-sha256`) is emitted although no `test-env` declaration of this
    /// universe names it: a standalone universe's module that the root universe's
    /// tests read by label.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub witnessed: Vec<String>,
}

impl WasmConfig {
    pub fn is_empty(&self) -> bool {
        self.packages.is_empty()
            && self.platform.is_none()
            && self.test_packages.is_empty()
            && self.triple.is_none()
            && self.witnessed.is_empty()
    }

    /// The target triple of the modules.
    pub fn triple(&self) -> &str {
        self.triple.as_deref().unwrap_or(crate::wasm::WASM_TRIPLE)
    }
}

/// `[platforms]` of `buckal.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PlatformsConfig {
    /// The `feature_platforms` name of the execution platform, which Buck
    /// configures host compilations for (e.g. `"linux-x86_64"`). With it, the
    /// recorded host differences include a plain rule that host compilations use
    /// only where its resolution on this platform differs from what they need;
    /// without it, wherever its resolution differs between platforms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<String>,
}

impl PlatformsConfig {
    pub fn is_empty(&self) -> bool {
        self.execution.is_none()
    }
}

impl RepoConfig {
    /// Workspace-level settings that change the rules of every first-party package,
    /// serialized for the cache fingerprints (empty when none is set).
    pub fn first_party_generation_key(&self) -> Vec<u8> {
        let mut key = if self.workspace_manifests {
            b"workspace_manifests".to_vec()
        } else {
            Vec::new()
        };
        // Declared test environments reach tests of other packages.
        if !self.wasm.is_empty() {
            key.extend(b"wasm");
            key.extend(serde_json::to_vec(&self.wasm).expect("wasm config serializes"));
        }
        // The macro call is part of every first-party package's BUCK file.
        if self.package_macro.first_party {
            key.extend(b"package_macro");
            key.extend(
                serde_json::to_vec(&(
                    &self.package_macro.bzl,
                    &self.package_macro.function,
                    &self.package_macro.first_party_kwargs,
                ))
                .expect("package_macro config serializes"),
            );
        }
        key
    }

    /// Workspace-level settings that change the rules of every third-party
    /// package, serialized for the cache fingerprints (empty when none is set).
    pub fn third_party_generation_key(&self) -> Vec<u8> {
        let mut key = Vec::new();
        // The macro call is part of every third-party package's BUCK file.
        if self.package_macro.third_party {
            key.extend(b"package_macro");
            key.extend(
                serde_json::to_vec(&(
                    &self.package_macro.bzl,
                    &self.package_macro.function,
                    &self.package_macro.third_party_kwargs,
                ))
                .expect("package_macro config serializes"),
            );
        }
        key
    }
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
            workspace_manifests: false,
            test_features: Vec::new(),
            platforms: PlatformsConfig::default(),
            wasm: WasmConfig::default(),
            package_macro: PackageMacroConfig::default(),
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
    /// that other tests do not, such as a particular fixture filesystem. A single
    /// test's `[fixups.<pkg>.tests.<name>] remote_execution` replaces it for that
    /// test.
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
    /// Labels of every test of the package (its `rust_test` rules), e.g. a label
    /// that keeps a test out of default runs. A single test drops one with
    /// `[fixups.<pkg>.tests.<name>] unset_labels`.
    #[serde(skip_serializing_if = "Set::is_empty")]
    pub test_labels: Set<String>,
    /// `target_compatible_with` constraints (or config settings) of every test of
    /// the package, e.g. one that makes a test incompatible unless a setting is on.
    #[serde(skip_serializing_if = "Set::is_empty")]
    pub test_target_compatible_with: Set<String>,
    /// Constraints (or config settings) of which every test of the package needs
    /// at least one, e.g. either of two switches: any-of, where
    /// `test_target_compatible_with` is all-of. Emitted inside the tests'
    /// `target_compatible_with` as nested `select()`s, since a rule cannot have both
    /// `compatible_with` and `target_compatible_with`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub test_compatible_with: Vec<String>,
    /// Arguments of every test of the package that apply unless one of the named
    /// constraints (or config settings) is present (see [`LiftedArgs`]).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub test_lifted_args: Vec<LiftedArgs>,
    /// `supports_test_execution_caching` of every test of the package: whether Buck
    /// may reuse a cached result of the test's execution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_supports_execution_caching: Option<bool>,
    /// Arguments after the binary of every test of the package, which the test
    /// runner passes on, e.g. the per-test process runner's `--declared-skip NAME`,
    /// `--declared-only-substring WORD` and `--declared-skip-substring WORD`, or
    /// libtest's `--skip NAME`, `--exact` and name filters.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub test_args: Vec<String>,
    /// Additions to single tests of the package (`[fixups.<pkg>.tests.<name>]`),
    /// keyed by the `rust_test` rule name (`unittest` for a library's unit tests,
    /// else the test target's name). A name that is no test of the package is
    /// refused.
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub tests: Map<String, TestFixup>,
    /// `target_compatible_with` constraints (or config settings) of the package's
    /// libraries (every `rust_library`, including its variants), binaries and build
    /// scripts (the build script binaries and their `buildscript_run`), e.g. to keep
    /// a package off platforms where it does not build. Added to what generation
    /// derives from where Cargo builds the package.
    #[serde(skip_serializing_if = "Set::is_empty")]
    pub target_compatible_with: Set<String>,
    /// Extra dependencies of the package's libraries (every `rust_library`,
    /// including a host variant), as in Reindeer's top-level `extra_deps`, e.g. a
    /// toolchain target providing a system library.
    #[serde(skip_serializing_if = "Set::is_empty")]
    pub extra_deps: Set<String>,
    /// The build script, in Reindeer's `[buildscript.build]` and `[buildscript.run]`
    /// shape.
    #[serde(skip_serializing_if = "BuildscriptFixup::is_empty")]
    pub buildscript: BuildscriptFixup,
}

/// `[fixups.<pkg>.buildscript]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct BuildscriptFixup {
    /// Compiling the build script (`build-script-build` and a host variant's).
    #[serde(skip_serializing_if = "BuildscriptBuildFixup::is_empty")]
    pub build: BuildscriptBuildFixup,
    /// Running it: `true` (Reindeer's `buildscript.run = true`; generation always
    /// runs a package's build script) or a table.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<BuildscriptRunEntry>,
}

impl BuildscriptFixup {
    pub fn is_empty(&self) -> bool {
        self.build.is_empty() && self.run.is_none()
    }
}

/// `[fixups.<pkg>.buildscript.build]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct BuildscriptBuildFixup {
    /// Files the build script reads at compile time, as Reindeer's globs relative
    /// to the package. The build script is compiled from the whole package's
    /// sources, so they are present already; each must be a relative pattern inside
    /// the package.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extra_srcs: Vec<String>,
    /// Extra dependencies of the build script binary.
    #[serde(skip_serializing_if = "Set::is_empty")]
    pub extra_deps: Set<String>,
}

impl BuildscriptBuildFixup {
    pub fn is_empty(&self) -> bool {
        self.extra_srcs.is_empty() && self.extra_deps.is_empty()
    }
}

/// `buildscript.run = true` or `[fixups.<pkg>.buildscript.run]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum BuildscriptRunEntry {
    Run(bool),
    Table(BuildscriptRunFixup),
}

/// `[fixups.<pkg>.buildscript.run]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct BuildscriptRunFixup {
    /// Link the crate with the build script's `cargo:rustc-link-lib` libraries.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub rustc_link_lib: bool,
    /// Search the build script's `cargo:rustc-link-search` directories.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub rustc_link_search: bool,
    /// Files outside the package that the build script reads when it runs, as
    /// paths or globs (`*` within a directory, `**` across directories) relative to
    /// the workspace root, e.g. `"../../release-build-env.toml"` read from
    /// `CARGO_MANIFEST_DIR` is listed as `"release-build-env.toml"`. Each run of the
    /// package's build script (its variants' too) gets them at their
    /// workspace-relative positions around the package (`workspace_inputs`, with
    /// `package_path`), so relative paths resolve as under Cargo. A file of a
    /// generated first-party package is exported by that package to this one; any
    /// other file's BUCK file must export it under its name relative to that BUCK
    /// file. A path that is absolute, leaves the workspace, lies in the package
    /// itself or matches no file is refused.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<String>,
}

/// Additions to one test (`rust_test` rule) of a package, after the package-wide
/// `test_*` ones.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct TestFixup {
    /// Labels of this test, after the package's `test_labels`.
    #[serde(skip_serializing_if = "Set::is_empty")]
    pub labels: Set<String>,
    /// Labels of the package's `test_labels` this test does not get, e.g. a
    /// live-effect label on a package whose other tests keep it. A label the
    /// package's `test_labels` does not have, or that this test's `labels` adds, is
    /// refused.
    #[serde(skip_serializing_if = "Set::is_empty")]
    pub unset_labels: Set<String>,
    /// How this test executes, instead of the package's `test_remote_execution`:
    /// the name of a profile on the remote test execution toolchain, or
    /// `"disabled"`. Emitted as the test target's `remote_execution`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_execution: Option<String>,
    #[serde(skip_serializing_if = "Set::is_empty")]
    pub target_compatible_with: Set<String>,
    /// Arguments after the package's `test_args`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Any-of constraints of this test, after the package's
    /// `test_compatible_with` (a separate group: the test needs one of each).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub compatible_with: Vec<String>,
    /// Arguments of this test lifted by named constraints, after the package's.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub lifted_args: Vec<LiftedArgs>,
    /// `supports_test_execution_caching` of this test, over the package's
    /// `test_supports_execution_caching`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_test_execution_caching: Option<bool>,
    /// Run-time environment of this test, after the package's `test_run_env`: a key
    /// set here extends it, or overrides the package's value.
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub run_env: Map<String, String>,
    /// Keys of the package's `test_run_env` this test does not get (an empty string
    /// is a value, so removal is a list of its own). A key the package does not set,
    /// or that this test's `run_env` sets, is refused.
    #[serde(skip_serializing_if = "Set::is_empty")]
    pub unset_run_env: Set<String>,
    /// `RuntimeToolsInfo` label of this test, instead of the package's
    /// `test_runtime_tools`; `""` gives this test none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_tools: Option<String>,
}

/// Test arguments (e.g. a narrowing selection such as `--declared-skip-substring
/// WORD`) that apply unless one of `lifted_by`, constraint values or config
/// settings, is present in the test's configuration: emitted as nested
/// `select()`s giving no arguments when one is present and `args` otherwise.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LiftedArgs {
    pub args: Vec<String>,
    pub lifted_by: Vec<String>,
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
#[serde(default, deny_unknown_fields)]
pub struct RepoPatchConfig {
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub version: Map<String, VersionPatch>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VersionPatch {
    pub from: String,
    pub to: String,
}

impl RepoConfig {
    /// The repository's `buckal.toml`, or the defaults when there is none. A file
    /// that cannot be read or parsed, including one with a key generation does not
    /// know, stops the command: falling back to defaults would generate rules
    /// without the configuration the file asks for.
    pub fn load() -> Self {
        let repo_config_path = Self::repo_config_path();

        if !repo_config_path.exists() {
            return Self::default();
        }

        let content = fs::read_to_string(&repo_config_path).unwrap_or_else(|error| {
            buckal_error!(format!(
                "failed to read the repository configuration {}: {error}",
                repo_config_path.display()
            ));
            std::process::exit(1);
        });
        Self::parse(&content).unwrap_or_else(|error| {
            buckal_error!(format!(
                "invalid repository configuration {}: {error}",
                repo_config_path.display()
            ));
            std::process::exit(1);
        })
    }

    /// Parse a `buckal.toml`; an unknown key is an error that names it.
    pub fn parse(content: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(content)
    }

    /// The selected universe's `buckal.toml` ([`crate::universe`]).
    pub fn repo_config_path() -> PathBuf {
        let buck2_root = get_buck2_root().unwrap_or_exit();
        buck2_root
            .join(crate::universe::selected().config_dir())
            .join(crate::universe::CONFIG_FILE)
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::{ExportFileEntry, MacroArg, PackageMacroConfig, RepoConfig, Set};

    #[test]
    fn test_wasm_modules_default_to_wasm32_unknown_unknown() {
        let config = RepoConfig::parse("[wasm]\npackages = [\"m\"]\n").expect("parses");
        assert_eq!(config.wasm.triple(), crate::wasm::WASM_TRIPLE);
        assert!(config.wasm.witnessed.is_empty());
    }

    #[test]
    fn test_wasm_modules_name_their_triple_and_witnesses() {
        let config = RepoConfig::parse(
            "[wasm]\npackages = [\"guest\"]\ntriple = \"wasm32-wasip1\"\nwitnessed = [\"guest\"]\n",
        )
        .expect("parses");
        assert_eq!(config.wasm.triple(), "wasm32-wasip1");
        assert_eq!(config.wasm.witnessed, vec!["guest".to_owned()]);
        assert!(!config.wasm.is_empty());
    }

    #[test]
    fn test_repo_config_deserializes_package_macro() {
        let config = RepoConfig::parse(
            r#"
                [package_macro]
                bzl = "//buck2:source_tree_inputs.bzl"
                function = "declare_source_tree_inputs"
                first_party = true
                first_party_kwargs = { whole_repository_only = true, cell_path = "x", depth = 2 }
            "#,
        )
        .expect("failed to deserialize repo config");
        let package_macro = &config.package_macro;
        assert_eq!(package_macro.bzl, "//buck2:source_tree_inputs.bzl");
        assert_eq!(package_macro.function, "declare_source_tree_inputs");
        let kwargs = package_macro.first_party_call().expect("first-party call");
        assert_eq!(
            kwargs.get("whole_repository_only"),
            Some(&MacroArg::Bool(true))
        );
        assert_eq!(
            kwargs.get("cell_path"),
            Some(&MacroArg::Str("x".to_owned()))
        );
        assert_eq!(kwargs.get("depth"), Some(&MacroArg::Int(2)));
        assert!(
            RepoConfig::default()
                .package_macro
                .first_party_call()
                .is_none()
        );
    }

    #[test]
    fn test_repo_config_refuses_a_malformed_package_macro() {
        for (table, expected) in [
            (
                "bzl = \"//a:b.bzl\"\nfunction = \"not-an-identifier\"\nfirst_party = true",
                "function",
            ),
            (
                "bzl = \"//a:b\"\nfunction = \"f\"\nfirst_party = true",
                "bzl",
            ),
            (
                "bzl = \"//a:b.bzl\"\nfunction = \"f\"\nfirst_party = true\nfirst_party_kwargs = { \"a b\" = 1 }",
                "keyword",
            ),
            (
                "bzl = \"//a:b.bzl\"\nfunction = \"f\"\nunknown = true",
                "unknown",
            ),
        ] {
            let error = RepoConfig::parse(&format!("[package_macro]\n{table}\n"))
                .expect_err(table)
                .to_string();
            assert!(error.contains(expected), "{table}: {error}");
        }
    }

    #[test]
    fn test_package_macro_reaches_the_first_party_generation_key() {
        let base = RepoConfig::default();
        assert!(base.first_party_generation_key().is_empty());
        let configured = RepoConfig {
            package_macro: PackageMacroConfig {
                bzl: "//buck2:inputs.bzl".to_owned(),
                function: "declare_inputs".to_owned(),
                first_party: true,
                ..PackageMacroConfig::default()
            },
            ..RepoConfig::default()
        };
        let key = configured.first_party_generation_key();
        assert!(!key.is_empty());
        let mut other = configured.clone();
        other
            .package_macro
            .first_party_kwargs
            .insert("whole_repository_only".to_owned(), MacroArg::Bool(true));
        assert_ne!(other.first_party_generation_key(), key);
        let mut renamed = configured.clone();
        renamed.package_macro.function = "declare_other".to_owned();
        assert_ne!(renamed.first_party_generation_key(), key);
        // Off for first-party packages: no key at all.
        let mut off = configured.clone();
        off.package_macro.first_party = false;
        assert!(off.first_party_generation_key().is_empty());
        // The first-party call does not reach third-party packages.
        assert!(configured.third_party_generation_key().is_empty());
    }

    #[test]
    fn test_package_macro_reaches_the_third_party_generation_key() {
        assert!(
            RepoConfig::default()
                .third_party_generation_key()
                .is_empty()
        );
        let configured = RepoConfig {
            package_macro: PackageMacroConfig {
                bzl: "//buck2:inputs.bzl".to_owned(),
                function: "declare_inputs".to_owned(),
                third_party: true,
                ..PackageMacroConfig::default()
            },
            ..RepoConfig::default()
        };
        let key = configured.third_party_generation_key();
        assert!(!key.is_empty());
        assert!(configured.first_party_generation_key().is_empty());
        let mut other = configured.clone();
        other
            .package_macro
            .third_party_kwargs
            .insert("whole_repository_only".to_owned(), MacroArg::Bool(true));
        assert_ne!(other.third_party_generation_key(), key);
        // A first-party argument changes only the first-party key.
        let mut first = configured.clone();
        first
            .package_macro
            .first_party_kwargs
            .insert("whole_repository_only".to_owned(), MacroArg::Bool(true));
        assert_eq!(first.third_party_generation_key(), key);
    }

    #[test]
    fn test_repo_config_deserializes_third_party_package_macro() {
        let config = RepoConfig::parse(
            r#"
                [package_macro]
                bzl = "//buck2:source_tree_inputs.bzl"
                function = "declare_source_tree_inputs"
                third_party = true
                third_party_kwargs = { whole_repository_only = true }
            "#,
        )
        .expect("failed to deserialize repo config");
        assert!(config.package_macro.first_party_call().is_none());
        let kwargs = config
            .package_macro
            .third_party_call()
            .expect("third-party call");
        assert_eq!(
            kwargs.get("whole_repository_only"),
            Some(&MacroArg::Bool(true))
        );
        assert!(
            RepoConfig::parse(
                "[package_macro]\nbzl = \"//a:b.bzl\"\nfunction = \"f\"\nthird_party = true\nthird_party_kwargs = { \"a-b\" = true }\n"
            )
            .is_err()
        );
    }

    #[test]
    fn test_repo_config_deserializes_package_tables() {
        // `tables` alone: no macro needed.
        let config =
            RepoConfig::parse("[package_macro]\ntables = \"buckal_packages.bzl\"\n").unwrap();
        assert_eq!(
            config.package_macro.tables.as_deref(),
            Some("buckal_packages.bzl")
        );
        assert!(config.package_macro.first_party_call().is_none());
        assert!(!config.package_macro.is_empty());
        // It reaches neither side's generation key: no BUCK file depends on it.
        assert!(config.first_party_generation_key().is_empty());
        assert!(config.third_party_generation_key().is_empty());

        let config = RepoConfig::parse(
            "[package_macro]\nbzl = \"//a:b.bzl\"\nfunction = \"f\"\nfirst_party = true\ntables = \"buck2/generated/packages.bzl\"\n",
        )
        .unwrap();
        assert_eq!(
            config.package_macro.tables.as_deref(),
            Some("buck2/generated/packages.bzl")
        );
        assert!(RepoConfig::default().package_macro.tables.is_none());
    }

    #[test]
    fn test_repo_config_refuses_a_malformed_tables_path() {
        for path in [
            "",
            "/abs/packages.bzl",
            "./packages.bzl",
            "a/../packages.bzl",
            "a//packages.bzl",
            "packages.txt",
            "a\\\\packages.bzl",
        ] {
            let error = RepoConfig::parse(&format!("[package_macro]\ntables = {path:?}\n"))
                .expect_err(path)
                .to_string();
            assert!(
                error.contains("[package_macro] tables must"),
                "{path}: {error}"
            );
        }
        // A macro still needs its name beside `tables`.
        let error = RepoConfig::parse("[package_macro]\nfirst_party = true\ntables = \"t.bzl\"\n")
            .expect_err("no bzl")
            .to_string();
        assert!(error.contains("bzl"), "{error}");
    }

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
    fn test_repo_config_deserializes_per_test_execution_and_unset_labels() {
        let config: RepoConfig = toml::from_str(
            r#"
                [fixups.client]
                test_remote_execution = "disabled"
                test_labels = ["unqualified-live-effect"]

                [fixups.client.tests.pure]
                remote_execution = "nativelink_test_result_reuse"
                unset_labels = ["unqualified-live-effect"]
            "#,
        )
        .expect("failed to deserialize repo config");
        let fixup = config.fixups.get("client").expect("missing fixup");
        let test = fixup.tests.get("pure").expect("missing test fixup");
        assert_eq!(
            test.remote_execution.as_deref(),
            Some("nativelink_test_result_reuse")
        );
        assert_eq!(
            test.unset_labels,
            Set::from(["unqualified-live-effect".to_owned()])
        );
        // The package-wide setting is untouched by the per-test one.
        assert_eq!(fixup.test_remote_execution.as_deref(), Some("disabled"));

        // Unset, they are absent from the serialization (which the cache
        // fingerprints), so existing per-test fixups keep their fingerprints.
        let other: RepoConfig =
            toml::from_str("[fixups.other.tests.it]\nlabels = [\"x\"]\n").expect("deserialize");
        let it = other.fixups.get("other").unwrap().tests.get("it").unwrap();
        assert_eq!(it.remote_execution, None);
        assert!(it.unset_labels.is_empty());
        assert_eq!(serde_json::to_string(it).unwrap(), r#"{"labels":["x"]}"#);

        // A misspelt key is refused, not ignored.
        assert!(
            toml::from_str::<RepoConfig>("[fixups.c.tests.it]\nremote_executon = \"p\"\n").is_err()
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
