//! wasm32 modules built from their own Cargo graph (`[wasm]` in `buckal.toml`).
//!
//! A package declaring `[package.metadata.buck2.wasm]` (`features`, `profile`) is
//! built by Cargo as `cargo build -p <package> --target wasm32-unknown-unknown
//! --features <features> --profile <profile>`: its own feature unification, and
//! that profile's code generation for every crate of the graph. Generation
//! resolves that graph, gives every library in it a `<lib>-wasm` variant with the
//! profile's flags, and configures the module's for the wasm32 platform.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, Result, bail};
use cargo_metadata::{Package, PackageId, camino::Utf8PathBuf};

use crate::platform_features::{UnitProfile, WasmVariant, resolve_package_graph};

/// The target triple of the modules.
pub const WASM_TRIPLE: &str = "wasm32-unknown-unknown";

/// The name of a module's `configured_alias`, whose `[cdylib]` is the module.
pub const MODULE_ALIAS: &str = "lib-wasm-wasm32";

/// The suffix of the rule holding a module's SHA-256 witness, after its alias.
pub const SHA256_SUFFIX: &str = "-sha256";

/// A module's `[package.metadata.buck2.wasm]`, with its feature variants
/// (`[package.metadata.buck2.wasm-variants.<name>]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleDeclaration {
    pub features: Vec<String>,
    pub profile: String,
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
    packages: &[Package],
    members: &[PackageId],
) -> Result<Vec<String>> {
    let mut features = Vec::new();
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
            let graph = resolve_package_graph(
                manifest_path,
                packages,
                module,
                features,
                profile,
                WASM_TRIPLE,
            )?;
            if !graph.contains_key(id) {
                bail!(
                    "`[wasm] packages`: `{key}`'s own graph does not compile its library for {WASM_TRIPLE}"
                );
            }
            for (pkg, lib) in graph {
                let root = &pkg == id;
                let (mut rustc_flags, root_flags) = profile_flags(&lib.profile)
                    .with_context(|| format!("`[wasm] packages`: `{key}`: `{pkg}`"))?;
                if root {
                    rustc_flags.extend(root_flags);
                }
                let variant = WasmVariant {
                    platform: platform.to_owned(),
                    features: lib.features,
                    normal: lib.normal,
                    build: lib.build,
                    rustc_flags,
                    root,
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
    use super::profile_flags;
    use crate::platform_features::UnitProfile;

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
}
