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

/// A module's `[package.metadata.buck2.wasm]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleDeclaration {
    pub features: Vec<String>,
    pub profile: String,
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
            features.extend(
                declaration
                    .features
                    .iter()
                    .map(|feature| format!("{module}/{feature}")),
            );
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
    Ok(Some(ModuleDeclaration { features, profile }))
}

/// `[package.metadata.buck2.test-env]` of `package`: `{variable: wasm package}`.
pub fn test_env_declaration(package: &Package) -> Result<BTreeMap<String, String>> {
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
            let module = value
                .as_object()
                .filter(|v| v.len() == 1)
                .and_then(|v| v.get("wasm"))
                .and_then(|m| m.as_str())
                .with_context(|| {
                    format!(
                        "`{}`: `[package.metadata.buck2.test-env] {variable}` is not `{{ wasm = \"<package>\" }}`",
                        package.name
                    )
                })?;
            Ok((variable.clone(), module.to_owned()))
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
        let graph = resolve_package_graph(
            manifest_path,
            packages,
            module,
            &declaration.features,
            &declaration.profile,
            WASM_TRIPLE,
        )?;
        if !graph.contains_key(id) {
            bail!(
                "`[wasm] packages`: `{module}`'s own graph does not compile its library for {WASM_TRIPLE}"
            );
        }
        for (pkg, lib) in graph {
            let root = &pkg == id;
            let (mut rustc_flags, root_flags) = profile_flags(&lib.profile)
                .with_context(|| format!("`[wasm] packages`: `{module}`: `{pkg}`"))?;
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
                .insert(module.clone(), variant);
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
