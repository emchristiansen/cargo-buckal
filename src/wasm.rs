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

use crate::platform_features::{WasmVariant, resolve_package_graph};

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

/// The rustc flags of Cargo profile `name` defined (or built in) in the workspace
/// manifest `manifest`: `(every crate's, the linked module's own)`. Cargo passes
/// `-C opt-level`, `-C debuginfo`, `-C debug-assertions`, `-C overflow-checks`
/// and `-C codegen-units` to every crate, `-C panic=abort` and `-C strip` when
/// set, and `-C lto` to the final artifact.
pub fn profile_flags(manifest: &toml::Value, name: &str) -> Result<(Vec<String>, Vec<String>)> {
    let profiles = manifest.get("profile").and_then(|p| p.as_table());
    let mut chain: Vec<&toml::value::Table> = Vec::new();
    let mut current = name.to_owned();
    let base = loop {
        let defined = profiles
            .and_then(|p| p.get(&current))
            .and_then(|p| p.as_table());
        match (current.as_str(), defined) {
            ("dev" | "release", defined) => {
                chain.extend(defined);
                break current.clone();
            }
            (_, Some(table)) => {
                chain.push(table);
                let Some(parent) = table.get("inherits").and_then(|i| i.as_str()) else {
                    bail!("Cargo profile `{current}` has no `inherits`");
                };
                if chain.len() > 16 {
                    bail!("Cargo profile `{name}` inherits in a cycle");
                }
                current = parent.to_owned();
            }
            (_, None) => {
                bail!("Cargo profile `{current}` is not defined in the workspace manifest")
            }
        }
    };
    let release = base == "release";
    let mut settings: BTreeMap<&str, toml::Value> = BTreeMap::from([
        (
            "opt-level",
            toml::Value::Integer(if release { 3 } else { 0 }),
        ),
        ("debug", toml::Value::Boolean(!release)),
        ("debug-assertions", toml::Value::Boolean(!release)),
        ("overflow-checks", toml::Value::Boolean(!release)),
        (
            "codegen-units",
            toml::Value::Integer(if release { 16 } else { 256 }),
        ),
        ("lto", toml::Value::Boolean(false)),
        ("panic", toml::Value::String("unwind".to_owned())),
    ]);
    // The base first, then each profile that inherits it.
    for table in chain.iter().rev() {
        for (key, value) in table.iter() {
            settings.insert(key.as_str(), value.clone());
        }
    }
    if let Some(packages) = chain.iter().find_map(|t| t.get("package")) {
        bail!("Cargo profile `{name}`: per-package overrides (`{packages}`) are not supported");
    }
    let text = |v: &toml::Value| match v {
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let on_off = |key: &str| match &settings[key] {
        toml::Value::Boolean(true) => "on",
        _ => "off",
    };
    let debuginfo = match &settings["debug"] {
        toml::Value::Boolean(true) => "2".to_owned(),
        toml::Value::Boolean(false) => "0".to_owned(),
        toml::Value::String(s) => match s.as_str() {
            "none" => "0".to_owned(),
            "limited" => "1".to_owned(),
            "full" => "2".to_owned(),
            other => other.to_owned(),
        },
        other => text(other),
    };
    let mut flags = vec![
        format!("-Copt-level={}", text(&settings["opt-level"])),
        format!("-Cdebuginfo={debuginfo}"),
        format!("-Cdebug-assertions={}", on_off("debug-assertions")),
        format!("-Coverflow-checks={}", on_off("overflow-checks")),
        format!("-Ccodegen-units={}", text(&settings["codegen-units"])),
    ];
    if text(&settings["panic"]) == "abort" {
        flags.push("-Cpanic=abort".to_owned());
    }
    if let Some(strip) = settings.get("strip") {
        let strip = match strip {
            toml::Value::Boolean(true) => "symbols".to_owned(),
            toml::Value::Boolean(false) => "none".to_owned(),
            other => text(other),
        };
        if strip != "none" {
            flags.push(format!("-Cstrip={strip}"));
        }
    }
    let root = match &settings["lto"] {
        toml::Value::Boolean(true) => vec!["-Clto=fat".to_owned()],
        toml::Value::String(s) if s == "fat" || s == "thin" => vec![format!("-Clto={s}")],
        toml::Value::String(s) if s == "off" => vec!["-Clto=off".to_owned()],
        _ => Vec::new(),
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
    workspace_manifest: &toml::Value,
    packages: &HashMap<PackageId, Package>,
    first_party: &BTreeMap<String, PackageId>,
) -> Result<HashMap<PackageId, BTreeMap<String, WasmVariant>>> {
    let mut variants: HashMap<PackageId, BTreeMap<String, WasmVariant>> = HashMap::new();
    let mut seen = BTreeSet::new();
    for module in modules {
        if !seen.insert(module) {
            bail!("`[wasm] packages`: `{module}` is listed twice");
        }
        let Some(id) = first_party.get(module) else {
            bail!("`[wasm] packages`: `{module}` is no first-party package of the workspace");
        };
        let Some(declaration) = module_declaration(&packages[id])? else {
            bail!("`[wasm] packages`: `{module}` declares no `[package.metadata.buck2.wasm]`");
        };
        let (flags, root_flags) = profile_flags(workspace_manifest, &declaration.profile)
            .with_context(|| format!("`[wasm] packages`: `{module}`"))?;
        let graph = resolve_package_graph(
            manifest_path,
            packages,
            module,
            &declaration.features,
            WASM_TRIPLE,
        )?;
        if !graph.contains_key(id) {
            bail!(
                "`[wasm] packages`: `{module}`'s own graph does not compile its library for {WASM_TRIPLE}"
            );
        }
        for (pkg, (features, normal, build)) in graph {
            let root = &pkg == id;
            let variant = WasmVariant {
                platform: platform.to_owned(),
                features,
                normal,
                build,
                rustc_flags: if root {
                    flags.iter().chain(&root_flags).cloned().collect()
                } else {
                    flags.clone()
                },
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

    #[test]
    fn test_profile_flags_follow_inheritance() {
        let manifest: toml::Value = toml::from_str(
            "[profile.release]\ndebug = 0\noverflow-checks = true\nstrip = \"debuginfo\"\n\n\
             [profile.module]\ninherits = \"release\"\nopt-level = \"s\"\nlto = true\ncodegen-units = 1\n",
        )
        .unwrap();
        let (flags, root) = profile_flags(&manifest, "module").unwrap();
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
        let (flags, root) = profile_flags(&manifest, "release").unwrap();
        assert_eq!(flags[0], "-Copt-level=3");
        assert!(root.is_empty());
        assert!(profile_flags(&manifest, "missing").is_err());
    }
}
