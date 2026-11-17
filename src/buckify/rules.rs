use std::{collections::BTreeSet as Set, vec};

use cargo_metadata::TargetKind;
use cargo_metadata::camino::Utf8PathBuf;
use cargo_util_schemas::core::{PackageIdSpec, SourceKind};
use itertools::Itertools;

use crate::{
    buck::{Load, Rule, RustRule},
    buckal_error, buckal_note,
    context::BuckalContext,
    resolve::{BuckalNode, is_lib_like},
    utils::{UnwrapOrExit, get_vendor_dir},
};

use super::emit::{
    emit_buildscript_build, emit_buildscript_run, emit_cargo_manifest, emit_filegroup,
    emit_git_fetch, emit_http_archive, emit_rust_binary, emit_rust_library, emit_rust_test,
    patch_with_buildscript,
};

/// Buckifies a third-party dependency into a list of BUCK rules.
///
/// This includes generating rules for the library target, and if a build script is present, also generating rules for the build script and patching the library rule accordingly.
pub fn buckify_dep_node(node: &BuckalNode, ctx: &BuckalContext) -> Vec<Rule> {
    // emit buck rules for lib target
    let mut buck_rules: Vec<Rule> = Vec::new();

    let manifest_dir = node.manifest_path.parent().unwrap().to_owned();
    let lib_target = node
        .targets
        .iter()
        .find(|t| t.kind.iter().any(is_lib_like))
        .expect("No library target found");

    // Generate rules to vendor the dependency source code
    let package_id_spec = PackageIdSpec::parse(&node.package_id.repr)
        .unwrap_or_exit_ctx("failed to parse package ID");

    match package_id_spec.kind().unwrap() {
        SourceKind::Registry => {
            let http_archive = emit_http_archive(node);
            buck_rules.push(Rule::HttpArchive(http_archive));
        }
        SourceKind::Path => {
            buckal_error!(
                "Local path ({}) is not supported for third-party packages.",
                package_id_spec.url().unwrap().path()
            );
            buckal_note!(
                "Please consider importing `{}` with registry or git source instead, or if it's a local package, move it to the workspace and it will be treated as a root package.",
                node.name
            );
            std::process::exit(1);
        }
        SourceKind::Git(_) => {
            let git_fetch = emit_git_fetch(node);
            buck_rules.push(Rule::GitFetch(git_fetch));
        }
        _ => {
            buckal_error!("Unsupported source type for package `{}`.", node.name);
            buckal_note!("Only registry and git sources are supported for third-party packages.");
            std::process::exit(1);
        }
    }

    let cargo_manifest = emit_cargo_manifest(node, ctx);
    buck_rules.push(Rule::CargoManifest(cargo_manifest));

    let rust_library = emit_rust_library(node, lib_target, &manifest_dir, &node.name, ctx);

    buck_rules.push(Rule::RustLibrary(rust_library));

    // Check if the package has a build script
    let custom_build_target = node
        .targets
        .iter()
        .find(|t| t.kind.contains(&TargetKind::CustomBuild));

    if let Some(build_target) = custom_build_target {
        // Patch the rust_library rule to support build scripts
        for rule in &mut buck_rules {
            if let Some(rust_rule) = rule.as_rust_rule_mut() {
                patch_with_buildscript(rust_rule, build_target);
            }
        }

        // create the build script rule
        let buildscript_build = emit_buildscript_build(build_target, node, &manifest_dir, ctx);
        buck_rules.push(Rule::RustBinary(buildscript_build));

        // create the build script run rule
        let buildscript_run = emit_buildscript_run(node, build_target, ctx);
        buck_rules.push(Rule::BuildscriptRun(buildscript_run));
    }

    buck_rules
}

/// Buckifies workspace package into a list of BUCK rules, including rules for all targets (bin, lib, test) and handling build scripts if present.
pub fn buckify_root_node(node: &BuckalNode, ctx: &BuckalContext) -> Vec<Rule> {
    let bin_targets: Vec<_> = node
        .targets
        .iter()
        .filter(|t| t.kind.contains(&TargetKind::Bin))
        .collect();

    let lib_targets: Vec<_> = node
        .targets
        .iter()
        .filter(|t| t.kind.iter().any(is_lib_like))
        .collect();

    let test_targets: Vec<_> = node
        .targets
        .iter()
        .filter(|t| t.kind.contains(&TargetKind::Test))
        .collect();

    let mut buck_rules: Vec<Rule> = Vec::new();

    let manifest_dir = node.manifest_path.parent().unwrap().to_owned();

    // Pre-compute the library Buck target name for wiring binary → library deps.
    // In Cargo, every binary in a package implicitly depends on the package's library.
    let lib_buck_name: Option<String> = lib_targets.first().map(|lib_target| {
        if bin_targets.iter().any(|b| b.name == lib_target.name) {
            format!("{}-lib", lib_target.name)
        } else {
            lib_target.name.to_owned()
        }
    });

    // emit filegroup rule for vendor
    let filegroup = emit_filegroup();
    buck_rules.push(Rule::FileGroup(filegroup));

    let cargo_manifest = emit_cargo_manifest(node, ctx);
    buck_rules.push(Rule::CargoManifest(cargo_manifest));

    // emit buck rules for bin targets
    for bin_target in &bin_targets {
        let buckal_name = bin_target.name.to_owned();

        let mut rust_binary = emit_rust_binary(node, bin_target, &manifest_dir, &buckal_name, ctx);

        // Cargo allows `main.rs` to use items from `lib.rs` via the crate's own name by default.
        // The binary must always depend on the sibling library, even when names differ
        // (e.g., binary "my-crate" vs library "my_crate").
        if let Some(lib_name) = &lib_buck_name {
            rust_binary.deps_mut().insert(format!(":{lib_name}"));
        }

        buck_rules.push(Rule::RustBinary(rust_binary));
    }

    // emit buck rules for lib targets
    for lib_target in &lib_targets {
        let buckal_name = if bin_targets.iter().any(|b| b.name == lib_target.name) {
            format!("{}-lib", lib_target.name)
        } else {
            lib_target.name.to_owned()
        };

        let rust_library = emit_rust_library(node, lib_target, &manifest_dir, &buckal_name, ctx);

        buck_rules.push(Rule::RustLibrary(rust_library));

        if !ctx.repo_config.ignore_tests && lib_target.test {
            // If the library target has inline tests, emit a rust_test rule for it
            let rust_test = emit_rust_test(node, lib_target, &manifest_dir, "unittest", ctx);

            buck_rules.push(Rule::RustTest(rust_test));
        }
    }

    // Emit buck rules for integration tests.
    // NOTE: Dev deps are emitted only on these rust_test targets (via
    // dep_kind_matches). This scoping means dev-dependency cycles could
    // theoretically produce acyclic Buck2 graphs. The current policy is to
    // reject them at DAG construction time — see resolve.rs from_metadata()
    // Pass 2. Update that code if relaxing the restriction.
    if !ctx.repo_config.ignore_tests {
        for test_target in &test_targets {
            let buckal_name = test_target.name.to_owned();

            let mut rust_test = emit_rust_test(node, test_target, &manifest_dir, &buckal_name, ctx);

            // Cargo provides `CARGO_BIN_EXE_<name>` to integration tests for every binary
            // target of the package, keyed by the binary's own name (hyphens kept).
            // The prelude's rust_test applies `env` both when compiling and when running
            // the test, so this serves `env!` and `std::env::var` alike.
            for bin_target in &bin_targets {
                rust_test.env_mut().insert(
                    format!("CARGO_BIN_EXE_{}", bin_target.name),
                    format!("$(location :{})", bin_target.name),
                );
            }

            // Integration tests may use the package's library under its crate name.
            if let Some(lib_name) = &lib_buck_name {
                rust_test.deps_mut().insert(format!(":{lib_name}"));
            }

            buck_rules.push(Rule::RustTest(rust_test));
        }
    }

    // Check if the package has a build script
    let custom_build_target = node
        .targets
        .iter()
        .find(|t| t.kind.contains(&TargetKind::CustomBuild));

    if let Some(build_target) = custom_build_target {
        // Patch the rust_library and rust_binary rules to support build scripts
        for rule in &mut buck_rules {
            if let Some(rust_rule) = rule.as_rust_rule_mut() {
                patch_with_buildscript(rust_rule, build_target);
            }
        }

        // create the build script rule
        let buildscript_build = emit_buildscript_build(build_target, node, &manifest_dir, ctx);
        buck_rules.push(Rule::RustBinary(buildscript_build));

        // create the build script run rule
        let buildscript_run = emit_buildscript_run(node, build_target, ctx);
        buck_rules.push(Rule::BuildscriptRun(buildscript_run));
    }

    buck_rules
}

/// Vendors the package sources to `third-party` and returns the path.
pub fn vendor_package(node: &BuckalNode) -> Utf8PathBuf {
    let vendor_dir =
        get_vendor_dir(&node.package_id).unwrap_or_exit_ctx("failed to get vendor directory");
    if !vendor_dir.exists() {
        std::fs::create_dir_all(&vendor_dir).expect("Failed to create target directory");
    }

    vendor_dir
}

/// Generate the content of the BUCK file based on the given rules, including conditional load statements for used rule types.
pub fn gen_buck_content(rules: &[Rule]) -> String {
    // Analyze which rule types are present to build conditional load statements
    let mut has_cargo_manifest = false;
    let mut has_rust_library = false;
    let mut has_rust_binary = false;
    let mut has_rust_test = false;
    let mut has_buildscript_run = false;

    for rule in rules {
        match rule {
            Rule::CargoManifest(_) => has_cargo_manifest = true,
            Rule::RustLibrary(_) => has_rust_library = true,
            Rule::RustBinary(_) => has_rust_binary = true,
            Rule::RustTest(_) => has_rust_test = true,
            Rule::BuildscriptRun(_) => has_buildscript_run = true,
            _ => {}
        }
    }

    // Build load statements based on which rule types are present
    let mut loads: Vec<Rule> = vec![];

    if has_cargo_manifest {
        loads.push(Rule::Load(Load {
            bzl: "@buckal//:cargo_manifest.bzl".to_owned(),
            items: Set::from(["cargo_manifest".to_owned()]),
        }));
    }

    // Build wrapper.bzl load items based on which rust rules are present
    let mut wrapper_items: Set<String> = Set::new();

    if has_rust_library {
        wrapper_items.insert("rust_library".to_owned());
    }
    if has_rust_binary {
        wrapper_items.insert("rust_binary".to_owned());
    }
    if has_rust_test {
        wrapper_items.insert("rust_test".to_owned());
    }
    if has_buildscript_run {
        wrapper_items.insert("buildscript_run".to_owned());
    }

    if !wrapper_items.is_empty() {
        loads.push(Rule::Load(Load {
            bzl: "@buckal//:wrapper.bzl".to_owned(),
            items: wrapper_items,
        }));
    }

    let mut content = rules
        .iter()
        .map(serde_starlark::to_string)
        .map(|r| r.unwrap())
        .join("\n");

    if !loads.is_empty() {
        let loads_string = loads
            .iter()
            .map(serde_starlark::to_string)
            .map(|r| r.unwrap())
            .join("");
        content.insert(0, '\n');
        content.insert_str(0, &loads_string);
    }

    content.insert_str(0, "# @generated by `cargo buckal`\n\n");

    content
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RepoConfig;
    use crate::resolve::{BuckalNode, BuckalResolve, BuckalTarget, NodeKind};
    use cargo_metadata::{Edition, PackageId, camino::Utf8PathBuf};
    use daggy::Dag;
    use std::collections::HashMap;

    fn mock_target(name: &str, kind: TargetKind) -> BuckalTarget {
        BuckalTarget {
            name: name.to_string(),
            kind: vec![kind],
            src_path: Utf8PathBuf::from("/tmp/dummy.rs"),
            doctest: true,
            test: true,
        }
    }

    fn mock_node(name: &str, targets: Vec<BuckalTarget>) -> BuckalNode {
        BuckalNode {
            package_id: PackageId {
                repr: format!("{} 0.1.0 (registry+...)", name),
            },
            name: name.to_string(),
            version: "0.1.0".to_string(),
            features: vec![],
            kind: NodeKind::FirstParty {
                relative_path: "".to_string(),
            },
            edition: Edition::E2021,
            manifest_path: Utf8PathBuf::from("/tmp/Cargo.toml"),
            targets,
            source: None,
            links: None,
            checksum: None,
        }
    }

    fn empty_resolve() -> BuckalResolve {
        BuckalResolve {
            dag: Dag::new(),
            index_map: HashMap::new(),
        }
    }

    #[test]
    fn test_buckify_root_node_name_collision() {
        let lib = mock_target("foo", TargetKind::Lib);
        let bin = mock_target("foo", TargetKind::Bin);

        let node = mock_node("foo", vec![lib, bin]);

        let ctx = BuckalContext {
            root: None,
            resolve: empty_resolve(),
            repo_config: RepoConfig {
                ignore_tests: false,
                ..RepoConfig::default()
            },
            workspace_root: Utf8PathBuf::from("/tmp"),
            workspace_inherit: false,
            no_merge: false,
        };

        let rules = buckify_root_node(&node, &ctx);

        let lib_rule = rules.iter().find_map(|r| {
            if let Rule::RustLibrary(l) = r {
                Some(l)
            } else {
                None
            }
        });

        assert!(lib_rule.is_some());
        assert_eq!(lib_rule.unwrap().name, "foo-lib");
    }

    #[test]
    fn test_buckify_root_node_test_deps_lib_alias() {
        let lib = mock_target("foo", TargetKind::Lib);
        let bin = mock_target("foo", TargetKind::Bin);
        let test = mock_target("integration_test", TargetKind::Test);

        let node = mock_node("foo", vec![lib, bin, test]);

        let ctx = BuckalContext {
            root: None,
            resolve: empty_resolve(),
            repo_config: RepoConfig {
                ignore_tests: false,
                ..RepoConfig::default()
            },
            workspace_root: Utf8PathBuf::from("/tmp"),
            workspace_inherit: false,
            no_merge: false,
        };

        let rules = buckify_root_node(&node, &ctx);

        let test_rule = rules.iter().find_map(|r| {
            if let Rule::RustTest(t) = r {
                if t.name == "integration_test" {
                    Some(t)
                } else {
                    None
                }
            } else {
                None
            }
        });

        assert!(test_rule.is_some());
        let test_rule = test_rule.unwrap();
        assert!(test_rule.deps.contains(":foo-lib"));
    }

    fn test_ctx() -> BuckalContext {
        BuckalContext {
            root: None,
            resolve: empty_resolve(),
            repo_config: RepoConfig {
                ignore_tests: false,
                ..RepoConfig::default()
            },
            workspace_root: Utf8PathBuf::from("/tmp"),
            workspace_inherit: false,
            no_merge: false,
        }
    }

    fn find_test<'a>(rules: &'a [Rule], name: &str) -> &'a crate::buck::RustTest {
        rules
            .iter()
            .find_map(|r| match r {
                Rule::RustTest(t) if t.name == name => Some(t),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no rust_test named {name}"))
    }

    #[test]
    fn test_integration_test_gets_cargo_bin_exe_for_every_binary() {
        // Package `my-tool` whose binaries are not named after the package
        // (as in a package `observatory_db_feedback_cli` with binary `observatory-feedback`).
        let lib = mock_target("my_tool", TargetKind::Lib);
        let bin_a = mock_target("my-cli", TargetKind::Bin);
        let bin_b = mock_target("helper", TargetKind::Bin);
        let test = mock_target("cli", TargetKind::Test);

        let node = mock_node("my-tool", vec![lib, bin_a, bin_b, test]);
        let rules = buckify_root_node(&node, &test_ctx());
        let test_rule = find_test(&rules, "cli");

        assert_eq!(
            test_rule
                .env
                .get("CARGO_BIN_EXE_my-cli")
                .map(String::as_str),
            Some("$(location :my-cli)")
        );
        assert_eq!(
            test_rule
                .env
                .get("CARGO_BIN_EXE_helper")
                .map(String::as_str),
            Some("$(location :helper)")
        );
        assert!(test_rule.deps.contains(":my_tool"));
    }

    #[test]
    fn test_integration_test_cargo_bin_exe_keeps_hyphenated_package_binary_name() {
        // The binary takes the package's hyphenated name; Cargo names the variable
        // after the binary, not after the underscored crate name.
        let lib = mock_target("my_tool", TargetKind::Lib);
        let bin = mock_target("my-tool", TargetKind::Bin);
        let test = mock_target("cli", TargetKind::Test);

        let node = mock_node("my-tool", vec![lib, bin, test]);
        let rules = buckify_root_node(&node, &test_ctx());
        let test_rule = find_test(&rules, "cli");

        assert_eq!(
            test_rule
                .env
                .get("CARGO_BIN_EXE_my-tool")
                .map(String::as_str),
            Some("$(location :my-tool)")
        );
        assert!(!test_rule.env.contains_key("CARGO_BIN_EXE_my_tool"));
        assert!(test_rule.deps.contains(":my_tool"));
    }

    #[test]
    fn test_unit_test_gets_no_cargo_bin_exe() {
        // Cargo sets CARGO_BIN_EXE_* for integration tests and benches only.
        let lib = mock_target("my_tool", TargetKind::Lib);
        let bin = mock_target("my-tool", TargetKind::Bin);

        let node = mock_node("my-tool", vec![lib, bin]);
        let rules = buckify_root_node(&node, &test_ctx());
        let unit = find_test(&rules, "unittest");

        assert!(!unit.env.keys().any(|k| k.starts_with("CARGO_BIN_EXE_")));
    }

    #[test]
    fn test_buckify_git_dep_in_repository_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let write = |rel: &str, content: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        };
        write(".cargo-ok", "");
        write("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
        write(
            "crates/rig-core/Cargo.toml",
            "[package]\nname = \"rig-core\"\n",
        );
        write("crates/rig-core/src/lib.rs", "");
        write("crates/rig-core/build.rs", "fn main() {}");

        let rev = "6871da91c889479c679951aad12d5f367152b01a";
        let manifest_dir = root.join("crates/rig-core");
        let target = |name: &str, kind: TargetKind, file: &str| BuckalTarget {
            name: name.to_owned(),
            kind: vec![kind],
            src_path: manifest_dir.join(file),
            doctest: true,
            test: true,
        };
        let node = BuckalNode {
            package_id: PackageId {
                repr: format!(
                    "git+https://github.com/emchristiansen/rig?rev={rev}#rig-core@0.42.0"
                ),
            },
            name: "rig-core".to_owned(),
            version: "0.42.0".to_owned(),
            features: vec![],
            kind: NodeKind::ThirdParty,
            edition: Edition::E2021,
            manifest_path: manifest_dir.join("Cargo.toml"),
            targets: vec![
                target("rig_core", TargetKind::Lib, "src/lib.rs"),
                target("build-script-build", TargetKind::CustomBuild, "build.rs"),
            ],
            source: Some(format!(
                "git+https://github.com/emchristiansen/rig?rev={rev}#{rev}"
            )),
            links: None,
            checksum: None,
        };

        let rules = buckify_dep_node(&node, &test_ctx());

        let git_fetch = rules
            .iter()
            .find_map(|r| match r {
                Rule::GitFetch(g) => Some(g),
                _ => None,
            })
            .expect("git_fetch");
        assert_eq!(git_fetch.name, "vendor");
        assert_eq!(git_fetch.repo, "https://github.com/emchristiansen/rig");
        assert_eq!(git_fetch.rev, rev);
        assert_eq!(
            git_fetch.sub_targets,
            Set::from(["Cargo.toml".to_owned(), "crates/rig-core".to_owned()])
        );

        for rule in &rules {
            match rule {
                Rule::CargoManifest(m) => {
                    assert_eq!(m.vendor, ":vendor[crates/rig-core]");
                    assert_eq!(m.workspace.as_deref(), Some(":vendor[Cargo.toml]"));
                }
                Rule::RustLibrary(l) => {
                    assert_eq!(l.srcs, Set::from([":vendor[crates/rig-core]".to_owned()]));
                    assert_eq!(l.crate_root, "vendor/crates/rig-core/src/lib.rs");
                }
                Rule::RustBinary(b) => {
                    assert_eq!(b.srcs, Set::from([":vendor[crates/rig-core]".to_owned()]));
                    assert_eq!(b.crate_root, "vendor/crates/rig-core/build.rs");
                }
                Rule::BuildscriptRun(r) => {
                    assert_eq!(r.manifest_dir, ":vendor[crates/rig-core]");
                }
                _ => {}
            }
        }
    }
}
