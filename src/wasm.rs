//! wasm32 modules built from their own Cargo graph (`[wasm]` in `buckal.toml`).
//!
//! A package declaring `[package.metadata.buck2.wasm]` (`features`, `profile`) is
//! built by Cargo as `cargo build -p <package> --target wasm32-unknown-unknown
//! --features <features> --profile <profile>`: its own feature unification, and
//! that profile's code generation for every crate of the graph. Generation
//! resolves that graph, gives every library in it a `<lib>-wasm` variant with the
//! profile's flags, and configures the module's for the wasm32 platform.
//!
//! With `bin = "<name>"`, the module is that binary target instead of the
//! library as a cdylib, as `dx build --platform web` builds a Dioxus app's
//! `main.rs`. The binary (`<name>-wasm-<key>`) takes the graph's dependencies
//! and the package's own library as the graph's variant, and the profile's
//! link-time flags. `bin-wasm-wasm32[.<variant>]` configures it for wasm32, and
//! its default output is the module.
//!
//! A package listed in `[wasm] test_packages` declares
//! `[package.metadata.buck2.wasm-test]` (`features`, optional `tests` and
//! `labels`) and any `[package.metadata.buck2.wasm-test-variants.<name>]`: its
//! library unit tests, compiled for wasm32 from Cargo's own `cargo test -p
//! <package> --lib --target wasm32-unknown-unknown --features <features>` graph
//! (not the workspace's feature unification, which may pull native-only crates into
//! wasm32). Every library of that graph gets a `<lib>-wasm-<key>` variant, keyed
//! [`test_key`]; the package gets the test-mode module `wasm-test-module[.<name>]`
//! (a `rust_binary` compiled with `--test`, so nothing runs it natively), its
//! wasm32 `configured_alias` `wasm-test-module-wasm32[.<name>]`, and the buckal
//! cell's `wasm_browser_test(name = "unittest-browser[.<name>]", wasm = <alias>)`,
//! which runs it in a browser. A graph's `integration-tests` names integration-test
//! targets of the package (`tests/<name>.rs`, distinct from `tests`, which names
//! test functions): each is compiled for wasm32 from Cargo's own `cargo test -p
//! <package> --test <name> --target wasm32-unknown-unknown --features <features>`
//! graph, keyed [`integration_test_key`], as the module `<name>-wasm-test-module[.<variant>]`
//! with the alias `<name>-wasm-test-module-wasm32[.<variant>]` and the browser test
//! `<name>-browser[.<variant>]`. A target with `harness = false` is refused, as are
//! two targets whose keys are equal (names differing only by `-` and `_`). Such a
//! target uses the graph's own library variants, with the package's library added,
//! where its graph is that graph plus the package's library, resolved as every
//! earlier target sharing them resolved it; otherwise it has variants of its own,
//! under its key. Generation owns every `wasm_browser_test` call in a
//! BUCK file it writes, and a declaration has no `timeout_seconds`, so a generated
//! browser test always has the rule's default timeout.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, Result, bail};
use cargo_metadata::{Package, PackageId, camino::Utf8PathBuf};

use crate::platform_features::{
    UnitProfile, WasmBin, WasmTest, WasmVariant, resolve_package_graph, resolve_package_test_graph,
};

/// The target triple of the modules.
pub const WASM_TRIPLE: &str = "wasm32-unknown-unknown";

/// The name of a module's `configured_alias`, whose `[cdylib]` is the module.
pub const MODULE_ALIAS: &str = "lib-wasm-wasm32";

/// The name of a binary module's `configured_alias` (`[package.metadata.buck2.wasm]
/// bin`), whose default output is the module.
pub const BIN_MODULE_ALIAS: &str = "bin-wasm-wasm32";

/// The suffix of the rule holding a module's SHA-256 witness, after its alias.
pub const SHA256_SUFFIX: &str = "-sha256";

/// The name of a package's wasm32 test-mode module (`rust_binary` with `--test`).
pub const TEST_MODULE: &str = "wasm-test-module";

/// The name of the `configured_alias` of a package's test-mode module.
pub const TEST_MODULE_ALIAS: &str = "wasm-test-module-wasm32";

/// The name of a package's browser test (`wasm_browser_test`).
pub const BROWSER_TEST: &str = "unittest-browser";

/// The suffix of an integration-test target's browser test, `<target>-browser`.
pub const INTEGRATION_BROWSER_TEST: &str = "browser";

/// The buckal cell file defining `wasm_browser_test`.
pub const BROWSER_TEST_BZL: &str = "@buckal//:wasm_browser_test.bzl";

/// A module's `[package.metadata.buck2.wasm]`, with its feature variants
/// (`[package.metadata.buck2.wasm-variants.<name>]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleDeclaration {
    pub features: Vec<String>,
    pub profile: String,
    /// The binary target that is the module (`bin`), when the module is a binary
    /// rather than the library as a cdylib. It holds for every variant.
    pub bin: Option<String>,
    /// Each variant's own features (exactly these, in a graph of its own) and
    /// profile (the module's when not given).
    pub variants: BTreeMap<String, (Vec<String>, String)>,
}

impl ModuleDeclaration {
    /// Every module `package` builds, by module key: the featureless module
    /// (`<package>`) and each variant (`<package>.<variant>`), with its features
    /// and profile.
    pub fn modules(&self, package: &str) -> Vec<(String, &[String], &str)> {
        std::iter::once((package.to_owned(), &self.features[..], &self.profile[..]))
            .chain(self.variants.iter().map(|(variant, (features, profile))| {
                (
                    module_key(package, Some(variant)),
                    &features[..],
                    &profile[..],
                )
            }))
            .collect()
    }
}

/// The key of a module: `<package>`, or `<package>.<variant>` for a variant (a
/// package name holds no `.`).
pub fn module_key(package: &str, variant: Option<&str>) -> String {
    match variant {
        None => package.to_owned(),
        Some(variant) => format!("{package}.{variant}"),
    }
}

/// The name of the `configured_alias` of the module `key`, whose `[cdylib]` is the
/// module: `lib-wasm-wasm32`, or `lib-wasm-wasm32.<variant>`.
pub fn module_alias(key: &str) -> String {
    match key.split_once('.') {
        None => MODULE_ALIAS.to_owned(),
        Some((_, variant)) => format!("{MODULE_ALIAS}.{variant}"),
    }
}

/// The name of the `configured_alias` of a binary module `key`
/// (`[package.metadata.buck2.wasm] bin`), whose default output is the module:
/// `bin-wasm-wasm32`, or `bin-wasm-wasm32.<variant>`.
pub fn bin_module_alias(key: &str) -> String {
    match key.split_once('.') {
        None => BIN_MODULE_ALIAS.to_owned(),
        Some((_, variant)) => format!("{BIN_MODULE_ALIAS}.{variant}"),
    }
}

/// The key of a package's `wasm-test` graph, which names its libraries' variants
/// (`<lib>-wasm-<key>`): `<package>__test`, or `<package>__test__<variant>`. It
/// holds only letters, digits, `_` and `-`, so the crate names derived from it
/// (variant build scripts) are Rust identifiers. Two graphs can share a key
/// (`a` with variant `b__test`, and `a__test__b` without one), and a module key
/// (`<package>`, `<package>.<variant>`) can be spelled like one when a package is
/// named so. [`resolve_tests`] refuses the first, and the generation refuses the
/// second where the two graphs share a library.
pub fn test_key(package: &str, variant: Option<&str>) -> String {
    match variant {
        None => format!("{package}__test"),
        Some(variant) => format!("{package}__test__{variant}"),
    }
}

/// The key of integration-test target `target`'s graph within the `wasm-test` graph
/// `graph_key`: `<graph_key>__it__<target>`, with `-` in the target's name spelled `_`.
pub fn integration_test_key(graph_key: &str, target: &str) -> String {
    format!("{graph_key}__it__{}", target.replace('-', "_"))
}

/// The rule name of integration-test target `target`'s `base` rule (a module, its
/// alias or its browser test) for a `wasm-test` variant: `<target>-<base>[.<variant>]`.
pub fn integration_rule_name(target: &str, base: &str, variant: Option<&str>) -> String {
    test_rule_name(&format!("{target}-{base}"), variant)
}

/// The rule name `base` for a `wasm-test` variant: `base`, or `base.<variant>`.
pub fn test_rule_name(base: &str, variant: Option<&str>) -> String {
    match variant {
        None => base.to_owned(),
        Some(variant) => format!("{base}.{variant}"),
    }
}

/// One `wasm-test` graph of a package: exactly these features, the browser
/// test's declared test names and labels, and the integration-test targets each
/// compiled for wasm32 and run in a browser of their own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestGraph {
    pub features: Vec<String>,
    pub tests: Vec<String>,
    pub labels: Vec<String>,
    pub integration_tests: Vec<String>,
}

/// A package's `[package.metadata.buck2.wasm-test]` and its variants
/// (`[package.metadata.buck2.wasm-test-variants.<name>]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestDeclaration {
    pub default: TestGraph,
    pub variants: BTreeMap<String, TestGraph>,
}

impl TestDeclaration {
    /// Every graph, as `(variant, graph)`: the default (`None`) first.
    pub fn graphs(&self) -> Vec<(Option<&str>, &TestGraph)> {
        std::iter::once((None, &self.default))
            .chain(
                self.variants
                    .iter()
                    .map(|(name, g)| (Some(name.as_str()), g)),
            )
            .collect()
    }
}

/// Whether `package` has any `wasm-test` metadata.
pub fn declares_tests(package: &Package) -> bool {
    package
        .metadata
        .get("buck2")
        .is_some_and(|b| b.get("wasm-test").is_some() || b.get("wasm-test-variants").is_some())
}

/// One `wasm-test` table at `where_`: `features` (required for a variant: its exact
/// features), `tests` and `labels`, nothing else.
fn test_graph(value: &serde_json::Value, where_: &str, variant: bool) -> Result<TestGraph> {
    let table = value
        .as_object()
        .with_context(|| format!("{where_} is not a table"))?;
    if let Some(key) = table
        .keys()
        .find(|k| !["features", "tests", "labels", "integration-tests"].contains(&k.as_str()))
    {
        bail!(
            "{where_}: unknown key `{key}` (it has `features`, `tests`, `labels` and `integration-tests`)"
        );
    }
    let list = |key: &str| -> Result<Vec<String>> {
        let Some(value) = table.get(key) else {
            return Ok(Vec::new());
        };
        let items = string_list(value, &format!("{where_} `{key}`"))?;
        let mut seen = BTreeSet::new();
        for item in &items {
            if item.is_empty() {
                bail!("{where_} `{key}`: an entry is empty");
            }
            if !seen.insert(item) {
                bail!("{where_} `{key}`: `{item}` is listed twice");
            }
        }
        Ok(items)
    };
    if variant && !table.contains_key("features") {
        bail!("{where_}: `features` is required (a variant's exact features)");
    }
    Ok(TestGraph {
        features: list("features")?,
        tests: list("tests")?,
        labels: list("labels")?,
        integration_tests: list("integration-tests")?,
    })
}

/// `[package.metadata.buck2.wasm-test]` of `package` with its variants, if declared.
/// Variants without the default declaration are refused.
pub fn test_declaration(package: &Package) -> Result<Option<TestDeclaration>> {
    let buck2 = package.metadata.get("buck2");
    let variants_table = buck2.and_then(|b| b.get("wasm-test-variants"));
    let Some(default) = buck2.and_then(|b| b.get("wasm-test")) else {
        if variants_table.is_some() {
            bail!(
                "`{}`: `[package.metadata.buck2.wasm-test-variants]` needs `[package.metadata.buck2.wasm-test]`",
                package.name
            );
        }
        return Ok(None);
    };
    let default = test_graph(
        default,
        &format!("`{}`: `[package.metadata.buck2.wasm-test]`", package.name),
        false,
    )?;
    let mut variants = BTreeMap::new();
    if let Some(table) = variants_table {
        let table = table.as_object().with_context(|| {
            format!(
                "`{}`: `[package.metadata.buck2.wasm-test-variants]` is not a table",
                package.name
            )
        })?;
        for (name, variant) in table {
            let where_ = format!(
                "`{}`: `[package.metadata.buck2.wasm-test-variants.{name}]`",
                package.name
            );
            if !valid_variant_name(name) {
                bail!("{where_}: a variant name holds only letters, digits, `_` and `-`");
            }
            variants.insert(name.clone(), test_graph(variant, &where_, true)?);
        }
    }
    Ok(Some(TestDeclaration { default, variants }))
}

/// What a `[package.metadata.buck2.test-env]` variable gets: the module `module`
/// (a module key), or the file holding its lowercase SHA-256 (`sha256`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TestEnvValue {
    pub module: String,
    pub sha256: bool,
}

impl serde::Serialize for TestEnvValue {
    /// The featureless module's path as its key alone, as before variants and
    /// witnesses existed (so recorded fingerprints keep their values).
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.sha256 {
            use serde::ser::SerializeMap;
            let mut map = serializer.serialize_map(Some(1))?;
            map.serialize_entry("wasm_sha256", &self.module)?;
            map.end()
        } else {
            serializer.serialize_str(&self.module)
        }
    }
}

/// A list of strings at `value`, or why not (`what` names it).
fn string_list(value: &serde_json::Value, what: &str) -> Result<Vec<String>> {
    value
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .map(|f| f.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
        })
        .with_context(|| format!("{what} is not a list of strings"))
}

/// Whether `name` can name a variant: letters, digits, `_` and `-`.
fn valid_variant_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The features the modules in `modules` (package names) declare, as
/// `<package>/<feature>`: what `cargo metadata` needs for the workspace's resolve to
/// contain every edge and package of the modules' own graphs. Modules are looked
/// up among the workspace `members` only (a package outside the workspace may
/// share a member's name); one that is no member is left to [`resolve_modules`]
/// to refuse.
pub fn declared_module_features(
    modules: &[String],
    test_packages: &[String],
    packages: &[Package],
    members: &[PackageId],
) -> Result<Vec<String>> {
    let mut features = Vec::new();
    // The `wasm-test` graphs' features, likewise (a listed package that is no member
    // or declares nothing is left to `resolve_tests`).
    for tested in test_packages {
        let Some(package) = packages
            .iter()
            .find(|p| p.name.as_str() == tested && members.contains(&p.id))
        else {
            continue;
        };
        if let Some(declaration) = test_declaration(package)? {
            for (_, graph) in declaration.graphs() {
                features.extend(graph.features.iter().map(|f| format!("{tested}/{f}")));
            }
        }
    }
    for module in modules {
        let Some(package) = packages
            .iter()
            .find(|p| p.name.as_str() == module && members.contains(&p.id))
        else {
            continue;
        };
        if let Some(declaration) = module_declaration(package)? {
            for (_, module_features, _) in declaration.modules(module) {
                features.extend(
                    module_features
                        .iter()
                        .map(|feature| format!("{module}/{feature}")),
                );
            }
        }
    }
    Ok(features)
}

/// `[package.metadata.buck2.wasm]` of `package`, if declared.
pub fn module_declaration(package: &Package) -> Result<Option<ModuleDeclaration>> {
    let Some(wasm) = package.metadata.get("buck2").and_then(|b| b.get("wasm")) else {
        return Ok(None);
    };
    let features = match wasm.get("features") {
        None => Vec::new(),
        Some(list) => list
            .as_array()
            .and_then(|items| {
                items
                    .iter()
                    .map(|f| f.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
            })
            .with_context(|| {
                format!(
                    "`{}`: `[package.metadata.buck2.wasm] features` is not a list of strings",
                    package.name
                )
            })?,
    };
    let profile = match wasm.get("profile") {
        None => "release".to_owned(),
        Some(profile) => profile
            .as_str()
            .with_context(|| {
                format!(
                    "`{}`: `[package.metadata.buck2.wasm] profile` is not a string",
                    package.name
                )
            })?
            .to_owned(),
    };
    let bin = match wasm.get("bin") {
        None => None,
        Some(bin) => {
            let bin = bin.as_str().with_context(|| {
                format!(
                    "`{}`: `[package.metadata.buck2.wasm] bin` is not a string",
                    package.name
                )
            })?;
            if !package
                .targets
                .iter()
                .any(|t| t.name == bin && t.kind.contains(&cargo_metadata::TargetKind::Bin))
            {
                bail!(
                    "`{}`: `[package.metadata.buck2.wasm] bin` names `{bin}`, which is no binary target of the package",
                    package.name
                );
            }
            Some(bin.to_owned())
        }
    };
    let mut variants = BTreeMap::new();
    if let Some(table) = package
        .metadata
        .get("buck2")
        .and_then(|b| b.get("wasm-variants"))
    {
        let where_ = format!(
            "`{}`: `[package.metadata.buck2.wasm-variants]`",
            package.name
        );
        let table = table
            .as_object()
            .with_context(|| format!("{where_} is not a table"))?;
        for (name, variant) in table {
            let where_ = format!(
                "`{}`: `[package.metadata.buck2.wasm-variants.{name}]`",
                package.name
            );
            if !valid_variant_name(name) {
                bail!("{where_}: a variant name holds only letters, digits, `_` and `-`");
            }
            let variant = variant
                .as_object()
                .with_context(|| format!("{where_} is not a table"))?;
            if let Some(key) = variant
                .keys()
                .find(|k| !["features", "profile"].contains(&k.as_str()))
            {
                bail!("{where_}: unknown key `{key}` (a variant has `features` and `profile`)");
            }
            let variant_features = match variant.get("features") {
                None => bail!("{where_}: `features` is required (a variant's exact features)"),
                Some(list) => string_list(list, &format!("{where_} `features`"))?,
            };
            let variant_profile = match variant.get("profile") {
                None => profile.clone(),
                Some(p) => p
                    .as_str()
                    .with_context(|| format!("{where_} `profile` is not a string"))?
                    .to_owned(),
            };
            variants.insert(name.clone(), (variant_features, variant_profile));
        }
    }
    Ok(Some(ModuleDeclaration {
        features,
        profile,
        bin,
        variants,
    }))
}

/// `[package.metadata.buck2.test-env]` of `package`: each variable's module path
/// (`{ wasm = "<package>" }`) or SHA-256 witness file (`{ wasm_sha256 = "<package>"
/// }`), of the package's featureless module or, with `variant = "<name>"`, of that
/// variant.
pub fn test_env_declaration(package: &Package) -> Result<BTreeMap<String, TestEnvValue>> {
    let Some(table) = package
        .metadata
        .get("buck2")
        .and_then(|b| b.get("test-env"))
        .and_then(|t| t.as_object())
    else {
        return Ok(BTreeMap::new());
    };
    table
        .iter()
        .map(|(variable, value)| {
            let refuse = || {
                anyhow::anyhow!(
                    "`{}`: `[package.metadata.buck2.test-env] {variable}` is not `{{ wasm = \"<package>\" }}` or `{{ wasm_sha256 = \"<package>\" }}`, with an optional `variant = \"<name>\"`",
                    package.name
                )
            };
            let entry = value.as_object().ok_or_else(refuse)?;
            if entry
                .keys()
                .any(|k| !["wasm", "wasm_sha256", "variant"].contains(&k.as_str()))
            {
                return Err(refuse());
            }
            let (module, sha256) = match (entry.get("wasm"), entry.get("wasm_sha256")) {
                (Some(module), None) => (module, false),
                (None, Some(module)) => (module, true),
                _ => return Err(refuse()),
            };
            let module = module.as_str().ok_or_else(refuse)?;
            // A package name has no `.`: `<package>.<variant>` is a module key,
            // which a declaration spells only with `variant`.
            if let Some((name, variant)) = module.split_once('.') {
                bail!(
                    "`{}`: `[package.metadata.buck2.test-env] {variable}` names the package `{module}`; a variant is named by its own key, `{{ {} = \"{name}\", variant = \"{variant}\" }}`",
                    package.name,
                    if sha256 { "wasm_sha256" } else { "wasm" }
                );
            }
            let variant = match entry.get("variant") {
                None => None,
                Some(v) => Some(v.as_str().filter(|v| valid_variant_name(v)).ok_or_else(refuse)?),
            };
            Ok((
                variable.clone(),
                TestEnvValue {
                    module: module_key(module, variant),
                    sha256,
                },
            ))
        })
        .collect()
}

/// The rustc flags of a unit compiled with `profile`, as Cargo resolved it:
/// `(the unit's, the linked module's own)`. Cargo passes `-C opt-level`, `-C
/// debuginfo`, `-C debug-assertions`, `-C overflow-checks` and `-C codegen-units`
/// (its default when unset: 256 for an incremental profile, else 16) to every
/// unit, `-C panic=abort` and `-C strip` when not off, and `-C lto` to the final
/// artifact. A strip Cargo left deferred is resolved as Cargo does: `debuginfo`
/// when the unit has no debuginfo.
pub fn profile_flags(profile: &UnitProfile) -> Result<(Vec<String>, Vec<String>)> {
    let debuginfo = match &profile.debuginfo {
        serde_json::Value::Number(level) => level.to_string(),
        serde_json::Value::String(name) => name.clone(),
        other => bail!("unknown debuginfo `{other}` in Cargo's unit profile"),
    };
    let codegen_units = profile
        .codegen_units
        .unwrap_or(if profile.incremental { 256 } else { 16 });
    let on_off = |on: bool| if on { "on" } else { "off" };
    let mut flags = vec![
        format!("-Copt-level={}", profile.opt_level),
        format!("-Cdebuginfo={debuginfo}"),
        format!("-Cdebug-assertions={}", on_off(profile.debug_assertions)),
        format!("-Coverflow-checks={}", on_off(profile.overflow_checks)),
        format!("-Ccodegen-units={codegen_units}"),
    ];
    if profile.panic == "abort" {
        flags.push("-Cpanic=abort".to_owned());
    }
    let strip_level = |inner: &serde_json::Value| match inner {
        serde_json::Value::String(none) if none == "None" => Some(None),
        serde_json::Value::Object(named) => named
            .get("Named")
            .and_then(|n| n.as_str())
            .map(|n| (n != "none").then(|| n.to_owned())),
        _ => None,
    };
    let strip = match &profile.strip {
        serde_json::Value::Object(strip) => match (strip.get("resolved"), strip.get("deferred")) {
            (Some(inner), None) => strip_level(inner),
            (None, Some(inner)) if debuginfo == "0" => {
                strip_level(inner).map(|level| level.or_else(|| Some("debuginfo".to_owned())))
            }
            (None, Some(inner)) => strip_level(inner),
            _ => None,
        },
        _ => None,
    };
    let Some(strip) = strip else {
        bail!("unknown strip `{}` in Cargo's unit profile", profile.strip);
    };
    if let Some(strip) = strip {
        flags.push(format!("-Cstrip={strip}"));
    }
    let root = match profile.lto.as_str() {
        "true" | "fat" => vec!["-Clto=fat".to_owned()],
        "thin" => vec!["-Clto=thin".to_owned()],
        "off" => vec!["-Clto=off".to_owned()],
        "false" => Vec::new(),
        other => bail!("unknown lto `{other}` in Cargo's unit profile"),
    };
    Ok((flags, root))
}

/// The `<lib>-wasm-<module>` variants of every module in `modules` (package
/// names), by package and module: each module's own graph, with its profile's
/// flags. Two modules' graphs may compile a library differently, so each module
/// has its own variants.
pub fn resolve_modules(
    modules: &[String],
    triple: &str,
    platform: &str,
    manifest_path: Option<&str>,
    packages: &HashMap<PackageId, Package>,
    members: &BTreeMap<String, PackageId>,
) -> Result<HashMap<PackageId, BTreeMap<String, WasmVariant>>> {
    let mut variants: HashMap<PackageId, BTreeMap<String, WasmVariant>> = HashMap::new();
    let mut seen = BTreeSet::new();
    for module in modules {
        if !seen.insert(module) {
            bail!("`[wasm] packages`: `{module}` is listed twice");
        }
        let Some(id) = members.get(module) else {
            bail!("`[wasm] packages`: `{module}` is no member of the workspace");
        };
        let Some(declaration) = module_declaration(&packages[id])? else {
            bail!("`[wasm] packages`: `{module}` declares no `[package.metadata.buck2.wasm]`");
        };
        for (key, features, profile) in declaration.modules(module) {
            let graph =
                resolve_package_graph(manifest_path, packages, module, features, profile, triple)?;
            if !graph.contains_key(id) {
                bail!(
                    "`[wasm] packages`: `{key}`'s own graph does not compile its library for {triple}"
                );
            }
            for (pkg, lib) in graph {
                let root = &pkg == id;
                let (mut rustc_flags, root_flags) = profile_flags(&lib.profile)
                    .with_context(|| format!("`[wasm] packages`: `{key}`: `{pkg}`"))?;
                // The module's link-time flags go to what links it: the binary that
                // `bin` names, or else the library itself as the cdylib.
                let bin = match (&declaration.bin, root) {
                    (Some(name), true) => Some(WasmBin {
                        name: name.clone(),
                        rustc_flags: rustc_flags.iter().cloned().chain(root_flags).collect(),
                    }),
                    _ => {
                        if root {
                            rustc_flags.extend(root_flags);
                        }
                        None
                    }
                };
                let variant = WasmVariant {
                    platform: platform.to_owned(),
                    features: lib.features,
                    normal: lib.normal,
                    build: lib.build,
                    rustc_flags,
                    root,
                    bin,
                };
                variants
                    .entry(pkg)
                    .or_default()
                    .insert(key.clone(), variant);
            }
        }
    }
    Ok(variants)
}

/// What `[wasm] test_packages` (package names) adds: the `<lib>-wasm-<key>`
/// variants of every library of each declared `wasm-test` graph (by package and
/// [`test_key`]), and each package's test units (by package and key). A listed
/// package that is listed twice, is no member, declares nothing, has no Rust
/// library or a build script, or compiles no library unit tests for wasm32 is
/// refused, and so is a member declaring `wasm-test` that is not listed.
pub fn resolve_tests(
    test_packages: &[String],
    platform: &str,
    manifest_path: Option<&str>,
    packages: &HashMap<PackageId, Package>,
    members: &BTreeMap<String, PackageId>,
) -> Result<TestVariants> {
    let mut resolved = TestVariants::default();
    let mut seen = BTreeSet::new();
    let mut keys = BTreeMap::new();
    for tested in test_packages {
        if !seen.insert(tested) {
            bail!("`[wasm] test_packages`: `{tested}` is listed twice");
        }
        let Some(id) = members.get(tested) else {
            bail!("`[wasm] test_packages`: `{tested}` is no member of the workspace");
        };
        let package = &packages[id];
        let Some(declaration) = test_declaration(package)? else {
            bail!(
                "`[wasm] test_packages`: `{tested}` declares no `[package.metadata.buck2.wasm-test]`"
            );
        };
        if !package.targets.iter().any(|t| {
            t.kind.iter().any(|k| {
                matches!(
                    k,
                    cargo_metadata::TargetKind::Lib | cargo_metadata::TargetKind::RLib
                )
            })
        }) {
            bail!(
                "`[wasm] test_packages`: `{tested}` has no Rust library whose unit tests could run"
            );
        }
        if package
            .targets
            .iter()
            .any(|t| t.kind.contains(&cargo_metadata::TargetKind::CustomBuild))
        {
            bail!(
                "`[wasm] test_packages`: `{tested}` has a build script, which a `wasm-test` module does not support"
            );
        }
        for (variant, graph) in declaration.graphs() {
            let key = test_key(tested, variant);
            claim_test_key(&mut keys, &key, tested, variant, None)?;
            let (libs, test) = resolve_package_test_graph(
                manifest_path,
                packages,
                id,
                tested,
                &graph.features,
                None,
                WASM_TRIPLE,
            )?;
            let Some(test) = test else {
                bail!(
                    "`[wasm] test_packages`: `cargo test -p {tested} --lib` compiles no library unit tests of `{tested}` for {WASM_TRIPLE} (`{key}`)"
                );
            };
            let (mut rustc_flags, root_flags) = profile_flags(&test.profile)
                .with_context(|| format!("`[wasm] test_packages`: `{key}`: `{tested}`"))?;
            rustc_flags.extend(root_flags);
            resolved.tests.entry(id.clone()).or_default().insert(
                key.clone(),
                WasmTest {
                    platform: platform.to_owned(),
                    features: test.features,
                    deps: test.deps,
                    rustc_flags,
                    tests: graph.tests.clone(),
                    labels: graph.labels.iter().cloned().collect(),
                    integration: None,
                    variants_key: None,
                },
            );
            let graph_libs = libs.clone();
            // The package's library as the first integration target sharing `key`
            // resolved it.
            let mut shared: Option<crate::platform_features::PackageGraphLib> = None;
            insert_test_variants(&mut resolved, libs, &key, platform)?;
            for target in &graph.integration_tests {
                let declared = package.targets.iter().any(|t| {
                    t.name == *target && t.kind.contains(&cargo_metadata::TargetKind::Test)
                });
                if !declared {
                    bail!(
                        "`[wasm] test_packages`: `{key}` names `{target}` in `integration-tests`, which is no integration-test target of `{tested}`"
                    );
                }
                if !has_test_harness(&package.manifest_path, target)? {
                    bail!(
                        "`[wasm] test_packages`: `{key}` names `{target}` in `integration-tests`, which has `harness = false`; its module would be compiled with `--test`, which such a target is not"
                    );
                }
                let it_key = integration_test_key(&key, target);
                claim_test_key(&mut keys, &it_key, tested, variant, Some(target))?;
                let (libs, test) = resolve_package_test_graph(
                    manifest_path,
                    packages,
                    id,
                    tested,
                    &graph.features,
                    Some(target),
                    WASM_TRIPLE,
                )?;
                let Some(test) = test else {
                    bail!(
                        "`[wasm] test_packages`: `cargo test -p {tested} --test {target}` compiles no test harness for {WASM_TRIPLE} (`{it_key}`)"
                    );
                };
                let (mut rustc_flags, root_flags) = profile_flags(&test.profile)
                    .with_context(|| format!("`[wasm] test_packages`: `{it_key}`: `{tested}`"))?;
                rustc_flags.extend(root_flags);
                // Cargo resolves features for the package and its dev-dependencies,
                // not per test target, so an integration test's graph is normally the
                // library unit tests' graph plus the package's own library. Then its
                // module uses that graph's variants (`key`), with the package's
                // library added to them, rather than a copy of every library under
                // its own key. Any difference keeps the target's own key.
                // A later target sharing the key must resolve the package's library as
                // the first did; otherwise it would replace that variant unchecked
                // (Fable review 128).
                let variants_key = if shares_graph(&libs, &graph_libs, id, shared.as_ref()) {
                    if shared.is_none() {
                        shared = libs.get(id).cloned();
                    }
                    let own: HashMap<_, _> = libs
                        .into_iter()
                        .filter(|(pkg, _)| pkg == id && !graph_libs.contains_key(pkg))
                        .collect();
                    insert_test_variants(&mut resolved, own, &key, platform)?;
                    Some(key.clone())
                } else {
                    insert_test_variants(&mut resolved, libs, &it_key, platform)?;
                    None
                };
                resolved.tests.entry(id.clone()).or_default().insert(
                    it_key.clone(),
                    WasmTest {
                        platform: platform.to_owned(),
                        features: test.features,
                        deps: test.deps,
                        rustc_flags,
                        tests: Vec::new(),
                        labels: graph.labels.iter().cloned().collect(),
                        integration: Some(target.clone()),
                        variants_key,
                    },
                );
            }
        }
    }
    for (name, id) in members {
        if !seen.contains(name) && declares_tests(&packages[id]) {
            bail!(
                "`{name}` declares `[package.metadata.buck2.wasm-test]`, but `[wasm] test_packages` does not list it"
            );
        }
    }
    Ok(resolved)
}

/// Whether an integration test's graph `libs` is the library unit tests' graph
/// `graph_libs` plus the package `id`'s own library: every other library equal, and
/// `id`'s equal to the unit tests' graph's where that builds it too, and to
/// `earlier`, the library as an earlier target sharing the graph resolved it.
fn shares_graph(
    libs: &HashMap<PackageId, crate::platform_features::PackageGraphLib>,
    graph_libs: &HashMap<PackageId, crate::platform_features::PackageGraphLib>,
    id: &PackageId,
    earlier: Option<&crate::platform_features::PackageGraphLib>,
) -> bool {
    let others = |m: &HashMap<PackageId, crate::platform_features::PackageGraphLib>| {
        m.iter()
            .filter(|(pkg, _)| *pkg != id)
            .map(|(pkg, lib)| (pkg.clone(), lib.clone()))
            .collect::<HashMap<_, _>>()
    };
    libs.contains_key(id)
        && others(libs) == others(graph_libs)
        && graph_libs
            .get(id)
            .is_none_or(|lib| Some(lib) == libs.get(id))
        && earlier.is_none_or(|lib| Some(lib) == libs.get(id))
}

/// Whether integration-test target `target` of the package at `manifest` has
/// libtest's harness: false only where a `[[test]]` entry of that name sets
/// `harness = false`. `cargo metadata` and the unit graph do not say.
fn has_test_harness(manifest: &Utf8PathBuf, target: &str) -> Result<bool> {
    let text =
        std::fs::read_to_string(manifest).with_context(|| format!("reading `{manifest}`"))?;
    let doc: toml::Table =
        toml::from_str(&text).with_context(|| format!("parsing `{manifest}`"))?;
    let Some(tests) = doc.get("test").and_then(toml::Value::as_array) else {
        return Ok(true);
    };
    Ok(!tests.iter().any(|t| {
        t.get("name").and_then(toml::Value::as_str) == Some(target)
            && t.get("harness").and_then(toml::Value::as_bool) == Some(false)
    }))
}

/// Give every library of a resolved `wasm-test` graph its `<lib>-wasm-<key>` variant.
fn insert_test_variants(
    resolved: &mut TestVariants,
    libs: HashMap<PackageId, crate::platform_features::PackageGraphLib>,
    key: &str,
    platform: &str,
) -> Result<()> {
    for (pkg, lib) in libs {
        let (rustc_flags, _) = profile_flags(&lib.profile)
            .with_context(|| format!("`[wasm] test_packages`: `{key}`: `{pkg}`"))?;
        resolved.variants.entry(pkg).or_default().insert(
            key.to_owned(),
            WasmVariant {
                platform: platform.to_owned(),
                features: lib.features,
                normal: lib.normal,
                build: lib.build,
                rustc_flags,
                root: false,
                bin: None,
            },
        );
    }
    Ok(())
}

/// Records that `package`'s graph `variant`, or its integration-test `target` in that
/// graph, uses `key`, refusing a key that an earlier one already uses: the second's
/// library variants would otherwise replace the first's without a word.
fn claim_test_key(
    keys: &mut BTreeMap<String, String>,
    key: &str,
    package: &str,
    variant: Option<&str>,
    target: Option<&str>,
) -> Result<()> {
    let graph = match variant {
        None => format!("`{package}`'s default `wasm-test` graph"),
        Some(variant) => format!("`{package}`'s `wasm-test` variant `{variant}`"),
    };
    let user = match target {
        None => graph,
        Some(target) => format!("integration-test target `{target}` of {graph}"),
    };
    if let Some(earlier) = keys.insert(key.to_owned(), user.clone()) {
        let rename = if target.is_some() || earlier.starts_with("integration-test target") {
            "the package, the variant or the integration-test target"
        } else {
            "the package or the variant"
        };
        bail!(
            "`[wasm] test_packages`: {user} and {earlier} both have the test graph key `{key}`; rename {rename}"
        );
    }
    Ok(())
}

/// What [`resolve_tests`] resolves.
#[derive(Debug, Default)]
pub struct TestVariants {
    /// The libraries' variants, by package and test graph key.
    pub variants: HashMap<PackageId, BTreeMap<String, WasmVariant>>,
    /// The tested packages' test units, by package and test graph key.
    pub tests: HashMap<PackageId, BTreeMap<String, WasmTest>>,
}

/// The directory of `manifest`'s package relative to `root`, with `/`.
pub fn package_dir(manifest: &Utf8PathBuf, root: &Utf8PathBuf) -> Option<String> {
    manifest
        .parent()?
        .strip_prefix(root)
        .ok()
        .map(|p| p.as_str().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::{
        claim_test_key, profile_flags, shares_graph, test_graph, test_key, test_rule_name,
    };
    use crate::platform_features::UnitProfile;
    use std::collections::BTreeMap;

    #[test]
    fn test_wasm_test_keys_and_names() {
        // Keys name variant libraries and, through them, build-script crates: only
        // letters, digits, `_` and `-` (rustc refuses `.` in a crate name).
        assert_eq!(test_key("observatory_site", None), "observatory_site__test");
        assert_eq!(
            test_key("observatory_site", Some("voice")),
            "observatory_site__test__voice"
        );
        assert_eq!(test_rule_name("unittest-browser", None), "unittest-browser");
        assert_eq!(
            test_rule_name("unittest-browser", Some("voice")),
            "unittest-browser.voice"
        );
    }

    #[test]
    fn test_wasm_test_key_collision_is_refused() {
        // `a` with variant `b__test` and `a__test__b` without one share `a__test__b__test`.
        assert_eq!(test_key("a", Some("b__test")), test_key("a__test__b", None));
        let mut keys = BTreeMap::new();
        claim_test_key(
            &mut keys,
            &test_key("a", Some("b__test")),
            "a",
            Some("b__test"),
            None,
        )
        .unwrap();
        claim_test_key(&mut keys, &test_key("a", None), "a", None, None).unwrap();
        let error = claim_test_key(
            &mut keys,
            &test_key("a__test__b", None),
            "a__test__b",
            None,
            None,
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            error,
            "`[wasm] test_packages`: `a__test__b`'s default `wasm-test` graph and `a`'s `wasm-test` variant `b__test` both have the test graph key `a__test__b__test`; rename the package or the variant"
        );
    }

    #[test]
    fn test_wasm_test_tables() {
        let graph = test_graph(
            &serde_json::json!({"features": ["test_lib"], "tests": ["a::b"], "labels": ["l"]}),
            "here",
            true,
        )
        .unwrap();
        assert_eq!(graph.features, ["test_lib"]);
        assert_eq!(graph.tests, ["a::b"]);
        assert_eq!(graph.labels, ["l"]);
        // The default declaration may omit everything; a variant needs `features`.
        let empty = test_graph(&serde_json::json!({}), "here", false).unwrap();
        assert!(empty.features.is_empty() && empty.tests.is_empty());
        for (value, variant, why) in [
            (serde_json::json!({}), true, "here: `features` is required"),
            (serde_json::json!([]), false, "here is not a table"),
            (
                serde_json::json!({"features": "x"}),
                false,
                "here `features` is not a list of strings",
            ),
            (
                serde_json::json!({"tests": [""]}),
                false,
                "here `tests`: an entry is empty",
            ),
            (
                serde_json::json!({"profile": "dev"}),
                false,
                "here: unknown key `profile`",
            ),
        ] {
            let error = format!("{:#}", test_graph(&value, "here", variant).unwrap_err());
            assert!(error.contains(why), "{value}: {error}");
        }
    }

    fn unit_profile(json: &str) -> UnitProfile {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn test_profile_flags_from_a_units_resolved_profile() {
        // As `cargo build --profile <p> --unit-graph` (cargo 1.100) gives it.
        let module = unit_profile(
            r#"{"name": "module", "opt_level": "s", "lto": "true", "codegen_backend": null, "codegen_units": 1, "debuginfo": 0, "split_debuginfo": null, "debug_assertions": false, "overflow_checks": true, "rpath": false, "incremental": false, "panic": "unwind", "strip": {"resolved": {"Named": "debuginfo"}}, "trim_paths": "none"}"#,
        );
        let (flags, root) = profile_flags(&module).unwrap();
        assert_eq!(
            flags,
            [
                "-Copt-level=s",
                "-Cdebuginfo=0",
                "-Cdebug-assertions=off",
                "-Coverflow-checks=on",
                "-Ccodegen-units=1",
                "-Cstrip=debuginfo",
            ]
        );
        assert_eq!(root, ["-Clto=fat"]);
        // The dev profile: debuginfo, no strip, incremental's default codegen units.
        let dev = unit_profile(
            r#"{"opt_level": "0", "lto": "false", "codegen_units": null, "debuginfo": 2, "debug_assertions": true, "overflow_checks": true, "incremental": true, "panic": "abort", "strip": {"resolved": "None"}}"#,
        );
        let (flags, root) = profile_flags(&dev).unwrap();
        assert_eq!(
            flags,
            [
                "-Copt-level=0",
                "-Cdebuginfo=2",
                "-Cdebug-assertions=on",
                "-Coverflow-checks=on",
                "-Ccodegen-units=256",
                "-Cpanic=abort",
            ]
        );
        assert!(root.is_empty());
        // A deferred strip without debuginfo is `debuginfo`, as Cargo resolves it.
        let deferred = unit_profile(
            r#"{"opt_level": "3", "lto": "thin", "codegen_units": null, "debuginfo": 0, "debug_assertions": false, "overflow_checks": false, "incremental": false, "panic": "unwind", "strip": {"deferred": "None"}}"#,
        );
        let (flags, root) = profile_flags(&deferred).unwrap();
        assert_eq!(flags.last().unwrap(), "-Cstrip=debuginfo");
        assert_eq!(root, ["-Clto=thin"]);
        let unknown = unit_profile(
            r#"{"opt_level": "3", "lto": "maybe", "codegen_units": null, "debuginfo": 0, "debug_assertions": false, "overflow_checks": false, "incremental": false, "panic": "unwind", "strip": {"resolved": "None"}}"#,
        );
        assert!(profile_flags(&unknown).is_err());
    }
    #[test]
    fn test_an_integration_target_shares_the_unit_tests_graph_only_when_equal() {
        use crate::platform_features::PackageGraphLib;
        use cargo_metadata::PackageId;
        let lib = |features: &[&str]| PackageGraphLib {
            features: features.iter().map(|f| (*f).to_owned()).collect(),
            normal: Default::default(),
            build: Default::default(),
            profile: unit_profile(
                r#"{"opt_level": "0", "lto": "false", "codegen_units": null, "debuginfo": 2, "debug_assertions": true, "overflow_checks": true, "incremental": false, "panic": "unwind", "strip": {"resolved": "None"}}"#,
            ),
        };
        let id = |repr: &str| PackageId {
            repr: repr.to_owned(),
        };
        let (site, dep) = (id("site"), id("dep"));
        let graph = std::collections::HashMap::from([(dep.clone(), lib(&["m"]))]);
        let with_site = |site_lib: PackageGraphLib, dep_lib: PackageGraphLib| {
            std::collections::HashMap::from([(site.clone(), site_lib), (dep.clone(), dep_lib)])
        };
        let target = with_site(lib(&["test_lib"]), lib(&["m"]));
        // The unit tests' graph plus the package's library: shared.
        assert!(shares_graph(&target, &graph, &site, None));
        // An earlier sharing target resolved the library the same: shared.
        assert!(shares_graph(
            &target,
            &graph,
            &site,
            Some(&lib(&["test_lib"]))
        ));
        // ...or differently: the target's own key (the fallback).
        assert!(!shares_graph(&target, &graph, &site, Some(&lib(&[]))));
        // Another library differs: the fallback.
        assert!(!shares_graph(
            &with_site(lib(&["test_lib"]), lib(&[])),
            &graph,
            &site,
            None
        ));
        // The package's library is absent: the fallback.
        assert!(!shares_graph(&graph, &graph, &site, None));
        // The unit tests' graph builds the library too, differently: the fallback.
        let cyclic = with_site(lib(&["extra"]), lib(&["m"]));
        assert!(!shares_graph(&target, &cyclic, &site, None));
        assert!(shares_graph(&cyclic, &cyclic, &site, None));
    }
}
