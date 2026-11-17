use std::{collections::BTreeSet as Set, vec};

use cargo_metadata::TargetKind;
use cargo_metadata::camino::{Utf8Path, Utf8PathBuf};
use cargo_util_schemas::core::{PackageIdSpec, SourceKind};
use itertools::Itertools;

use crate::{
    buck::{ExportFile, GENERATED_HEADER, Load, Rule, RustRule},
    buckal_error, buckal_note, buckal_warn,
    context::BuckalContext,
    resolve::{BuckalNode, BuckalTarget, NodeKind, is_lib_like},
    utils::{UnwrapOrExit, get_buck2_root, get_vendor_dir},
};

use super::includes::{
    IncludeGap, UnprovidedInclude, in_nested_package, repository_path, unprovided_includes,
};

use super::deps::{host_variant_name, test_host_variant_name, test_variant_name};
use super::emit::{
    emit_buildscript_build, emit_buildscript_run, emit_cargo_manifest, emit_filegroup,
    emit_git_fetch, emit_http_archive, emit_rust_binary, emit_rust_library, emit_rust_test,
    host_variant_build_target, patch_with_buildscript, test_host_variant_build_target,
    test_variant_build_target,
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

    buck_rules.extend(host_variant_rules(
        node,
        lib_target,
        &node.name,
        &manifest_dir,
        ctx,
    ));
    buck_rules.extend(test_variant_rules(
        node,
        lib_target,
        &node.name,
        &manifest_dir,
        ctx,
    ));
    buck_rules.extend(test_host_variant_rules(
        node,
        lib_target,
        &node.name,
        &manifest_dir,
        ctx,
    ));

    apply_fixups(&mut buck_rules, node, ctx);
    if ctx
        .repo_config
        .fixups
        .get(&node.name)
        .is_some_and(|fixup| !fixup.export_files.is_empty())
    {
        buckal_error!(
            "`[fixups.{}] export_files` is only supported for first-party packages",
            node.name
        );
        std::process::exit(1);
    }

    buck_rules
}

/// Buckifies workspace package into a list of BUCK rules, including rules for all targets (bin, lib, test) and handling build scripts if present.
pub fn buckify_root_node(node: &BuckalNode, ctx: &BuckalContext) -> Vec<Rule> {
    // Cargo builds a binary only where its `required-features` are on; one that
    // neither the production nor the test resolution enables is not built at all.
    let production = node.platform_features.exec_resolution();
    let tests = node.platform_features.test_resolution();
    let bin_targets: Vec<_> = node
        .targets
        .iter()
        .filter(|t| t.kind.contains(&TargetKind::Bin))
        .filter(|t| {
            let per_platform = !ctx.repo_config.feature_platforms.is_empty();
            features_enable(&production, &t.required_features, per_platform)
                || features_enable(&tests, &t.required_features, per_platform)
        })
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

    report_unprovided_includes(node, ctx);

    // Binaries and tests are compiled in their own context, which differs from the
    // library's when the library is host-only (a proc-macro with an integration test).
    let exec_node = BuckalNode {
        platform_features: node.platform_features.exec_resolution(),
        ..node.clone()
    };
    // Tests are compiled as Cargo compiles them for `cargo test`: with a separate
    // test resolution, in the test context.
    let test_node = BuckalNode {
        platform_features: node.platform_features.test_resolution(),
        ..node.clone()
    };
    let has_test_variant = node.platform_features.test_variant.is_some();
    // Rules compiled in the test context: they use the package's own test variant.
    let mut test_context_rules: Set<String> = Set::new();

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

        // A binary whose `required-features` only the tests' resolution enables is
        // built for the tests.
        let for_tests = !has_required_features(&exec_node, bin_target)
            && has_required_features(&test_node, bin_target);
        let bin_node = if for_tests { &test_node } else { &exec_node };
        let mut rust_binary =
            emit_rust_binary(bin_node, bin_target, &manifest_dir, &buckal_name, ctx);

        // Cargo allows `main.rs` to use items from `lib.rs` via the crate's own name by default.
        // The binary must always depend on the sibling library, even when names differ
        // (e.g., binary "my-crate" vs library "my_crate").
        if let Some(lib_name) = &lib_buck_name {
            let lib = if for_tests && has_test_variant {
                test_variant_name(lib_name)
            } else {
                lib_name.clone()
            };
            rust_binary.deps_mut().insert(format!(":{lib}"));
        }
        if for_tests {
            test_context_rules.insert(buckal_name.clone());
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
            let rust_test = emit_rust_test(&test_node, lib_target, &manifest_dir, "unittest", ctx);
            test_context_rules.insert("unittest".to_owned());

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

            let mut rust_test =
                emit_rust_test(&test_node, test_target, &manifest_dir, &buckal_name, ctx);
            test_context_rules.insert(buckal_name.clone());

            // Cargo gives integration tests `CARGO_BIN_EXE_<name>` for every binary of the
            // package, named after the binary (hyphens kept). The Buck2 fork's prelude is
            // the single writer of that environment (compile time and run time); this
            // only names the binaries.
            for bin_target in &bin_targets {
                rust_test
                    .cargo_bin_exes
                    .insert(bin_target.name.clone(), format!(":{}", bin_target.name));
            }

            // Integration tests may use the package's library under its crate name
            // (as the tests need it).
            if let Some(lib_name) = &lib_buck_name {
                let lib = if has_test_variant {
                    test_variant_name(lib_name)
                } else {
                    lib_name.clone()
                };
                rust_test.deps_mut().insert(format!(":{lib}"));
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
        // Patch the rust_library and rust_binary rules to support build scripts; rules
        // in the test context use the test variant's, whose features are the tests'.
        let test_build = test_variant_build_target(build_target);
        for rule in &mut buck_rules {
            let for_tests = has_test_variant
                && rule
                    .name()
                    .is_some_and(|name| test_context_rules.contains(name));
            if let Some(rust_rule) = rule.as_rust_rule_mut() {
                if for_tests {
                    patch_with_buildscript(rust_rule, &test_build);
                } else {
                    patch_with_buildscript(rust_rule, build_target);
                }
            }
        }

        // create the build script rule
        let buildscript_build = emit_buildscript_build(build_target, node, &manifest_dir, ctx);
        buck_rules.push(Rule::RustBinary(buildscript_build));

        // create the build script run rule
        let buildscript_run = emit_buildscript_run(node, build_target, ctx);
        buck_rules.push(Rule::BuildscriptRun(buildscript_run));
    }

    if let (Some(lib_target), Some(lib_name)) = (lib_targets.first(), &lib_buck_name) {
        buck_rules.extend(host_variant_rules(
            node,
            lib_target,
            lib_name,
            &manifest_dir,
            ctx,
        ));
        buck_rules.extend(test_variant_rules(
            node,
            lib_target,
            lib_name,
            &manifest_dir,
            ctx,
        ));
        buck_rules.extend(test_host_variant_rules(
            node,
            lib_target,
            lib_name,
            &manifest_dir,
            ctx,
        ));
    }

    apply_fixups(&mut buck_rules, node, ctx);
    match fixup_export_files(node, &buck_rules, ctx) {
        Ok(exports) => buck_rules.extend(exports.into_iter().map(Rule::ExportFile)),
        Err(error) => {
            buckal_error!("{:#}", error);
            std::process::exit(1);
        }
    }
    if ctx.repo_config.workspace_manifests {
        export_manifest_to_workspace_root(node, &mut buck_rules, ctx);
    }

    buck_rules
}

/// The name under which a first-party package exports its `Cargo.toml` for the
/// workspace-root `workspace_manifests` filegroup.
pub(super) const MANIFEST_EXPORT: &str = "Cargo.toml";

/// The visibility pattern of the workspace-root package.
pub(super) fn workspace_root_package(ctx: &BuckalContext) -> String {
    let root = get_buck2_root().unwrap_or_exit_ctx("failed to get Buck2 root");
    let package = ctx
        .workspace_root
        .strip_prefix(&root)
        .unwrap_or_exit_ctx("the workspace root is outside the Buck2 project");
    format!("//{package}:")
}

/// Export `node`'s `Cargo.toml` to the workspace-root package (for
/// `workspace_manifests`), unless the package is the root package itself, widening
/// an `export_files` export of it if there is one.
fn export_manifest_to_workspace_root(
    node: &BuckalNode,
    rules: &mut Vec<Rule>,
    ctx: &BuckalContext,
) {
    if node.manifest_path.parent() == Some(ctx.workspace_root.as_path()) {
        return;
    }
    let root_package = workspace_root_package(ctx);
    let existing = rules.iter_mut().find_map(|rule| match rule {
        Rule::ExportFile(export) if export.name == MANIFEST_EXPORT => Some(export),
        _ => None,
    });
    match existing {
        Some(export) => {
            if !export.visibility.contains("PUBLIC") {
                export.visibility.insert(root_package);
            }
        }
        None => rules.push(Rule::ExportFile(ExportFile {
            name: MANIFEST_EXPORT.to_owned(),
            src: "Cargo.toml".to_owned(),
            mode: Some("reference".to_owned()),
            visibility: Set::from([root_package]),
        })),
    }
}

/// The `export_file` rules of `[fixups.<package>] export_files` for a first-party
/// package, refusing a path that is not an existing file of the package itself or
/// whose rule name another rule of the package already has.
fn fixup_export_files(
    node: &BuckalNode,
    rules: &[Rule],
    ctx: &BuckalContext,
) -> anyhow::Result<Vec<ExportFile>> {
    use cargo_metadata::camino::{Utf8Component, Utf8Path};

    let Some(fixup) = ctx.repo_config.fixups.get(&node.name) else {
        return Ok(Vec::new());
    };
    let package_dir = node.manifest_path.parent().expect("manifest has a parent");
    let mut taken: Set<&str> = rules.iter().filter_map(Rule::name).collect();
    if ctx.workspace_inherit && node.manifest_path == ctx.workspace_root.join("Cargo.toml") {
        // The workspace manifest's own `export_file`, added to this package's BUCK.
        taken.insert("workspace");
    }
    let package = &node.name;
    fixup
        .export_files
        .iter()
        .map(|entry| {
            let path = entry.path();
            let relative = Utf8Path::new(path);
            let plain = !path.is_empty()
                && relative
                    .components()
                    .all(|c| matches!(c, Utf8Component::Normal(_)));
            if !plain {
                anyhow::bail!(
                    "`[fixups.{package}] export_files`: `{path}` is not a relative path inside the package (no `.`, `..` or leading `/`)"
                );
            }
            let file = package_dir.join(relative);
            if !file.is_file() {
                anyhow::bail!(
                    "`[fixups.{package}] export_files`: `{path}` is not an existing file of the package (`{file}`)"
                );
            }
            if let Some(nested) = relative
                .ancestors()
                .skip(1)
                .filter(|dir| !dir.as_str().is_empty())
                .find(|dir| package_dir.join(dir).join("BUCK").exists())
            {
                anyhow::bail!(
                    "`[fixups.{package}] export_files`: `{path}` is inside the nested Buck package `{nested}`"
                );
            }
            if !taken.insert(path) {
                anyhow::bail!(
                    "`[fixups.{package}] export_files`: `{path}` is listed twice or is already the name of a rule in the package's BUCK file"
                );
            }
            Ok(ExportFile {
                name: path.to_owned(),
                src: path.to_owned(),
                mode: Some("reference".to_owned()),
                visibility: entry.visibility(),
            })
        })
        .collect()
}

/// The rules of `node`'s host variant, if it has one: its library `<lib_name>-host`
/// and, with a build script, `build-script-host-build`/`build-script-host-run`,
/// compiled with Cargo's host-context resolution. Host-context rules of other
/// packages (build scripts, proc-macros and what they use) depend on it instead of
/// the target-context library.
fn host_variant_rules(
    node: &BuckalNode,
    lib_target: &BuckalTarget,
    lib_name: &str,
    manifest_dir: &Utf8PathBuf,
    ctx: &BuckalContext,
) -> Vec<Rule> {
    let Some(resolution) = node.platform_features.host_variant_resolution() else {
        return Vec::new();
    };
    let host_node = BuckalNode {
        platform_features: resolution,
        ..node.clone()
    };
    let mut rules = vec![Rule::RustLibrary(emit_rust_library(
        &host_node,
        lib_target,
        manifest_dir,
        &host_variant_name(lib_name),
        ctx,
    ))];
    let custom_build_target = node
        .targets
        .iter()
        .find(|t| t.kind.contains(&TargetKind::CustomBuild));
    if let Some(build_target) = custom_build_target {
        let host_build = host_variant_build_target(build_target);
        for rule in &mut rules {
            if let Some(rust_rule) = rule.as_rust_rule_mut() {
                patch_with_buildscript(rust_rule, &host_build);
            }
        }
        rules.push(Rule::RustBinary(emit_buildscript_build(
            &host_build,
            &host_node,
            manifest_dir,
            ctx,
        )));
        rules.push(Rule::BuildscriptRun(emit_buildscript_run(
            &host_node,
            &host_build,
            ctx,
        )));
    }
    rules
}

/// Whether `target`'s `required-features` are on in `node`'s resolution (on some
/// platform; always without per-platform resolution).
fn has_required_features(node: &BuckalNode, target: &BuckalTarget) -> bool {
    features_enable(&node.platform_features, &target.required_features, false)
}

/// Whether `required` features are all on in `resolution` (on some platform).
/// Without per-platform data they are taken to be, unless `per_platform`
/// resolution is configured: then Cargo builds nothing of the package.
fn features_enable(
    resolution: &crate::platform_features::PlatformResolution,
    required: &[String],
    per_platform: bool,
) -> bool {
    required.is_empty()
        || resolution.features.is_empty() && !per_platform
        || resolution
            .features
            .values()
            .any(|on| required.iter().all(|f| on.contains(f)))
}

/// The rules of `node`'s test variant, if it has one: its library `<lib_name>-test`
/// and, with a build script, `build-script-test-build`/`build-script-test-run`,
/// compiled with the tests' resolution. Test-context rules of the package and of
/// other packages depend on it instead of the library.
fn test_variant_rules(
    node: &BuckalNode,
    lib_target: &BuckalTarget,
    lib_name: &str,
    manifest_dir: &Utf8PathBuf,
    ctx: &BuckalContext,
) -> Vec<Rule> {
    let Some(resolution) = node.platform_features.test_variant_resolution() else {
        return Vec::new();
    };
    variant_rules(
        node,
        resolution,
        lib_target,
        &test_variant_name(lib_name),
        test_variant_build_target,
        manifest_dir,
        ctx,
    )
}

/// The rules of `node`'s test host variant, if it has one: its library
/// `<lib_name>-test-host` and, with a build script,
/// `build-script-test-host-build`/`build-script-test-host-run`, compiled with the
/// tests' host-context resolution. The tests' build scripts and proc-macros (and
/// what those use) depend on it.
fn test_host_variant_rules(
    node: &BuckalNode,
    lib_target: &BuckalTarget,
    lib_name: &str,
    manifest_dir: &Utf8PathBuf,
    ctx: &BuckalContext,
) -> Vec<Rule> {
    let Some(resolution) = node.platform_features.test_host_variant_resolution() else {
        return Vec::new();
    };
    variant_rules(
        node,
        resolution,
        lib_target,
        &test_host_variant_name(lib_name),
        test_host_variant_build_target,
        manifest_dir,
        ctx,
    )
}

/// A variant `name` of `node`'s library compiled with `resolution`, with its own
/// build script rules (`build_target_of` the package's) if it has a build script.
fn variant_rules(
    node: &BuckalNode,
    resolution: crate::platform_features::PlatformResolution,
    lib_target: &BuckalTarget,
    name: &str,
    build_target_of: fn(&BuckalTarget) -> BuckalTarget,
    manifest_dir: &Utf8PathBuf,
    ctx: &BuckalContext,
) -> Vec<Rule> {
    let variant_node = BuckalNode {
        platform_features: resolution,
        ..node.clone()
    };
    let mut rules = vec![Rule::RustLibrary(emit_rust_library(
        &variant_node,
        lib_target,
        manifest_dir,
        name,
        ctx,
    ))];
    let custom_build_target = node
        .targets
        .iter()
        .find(|t| t.kind.contains(&TargetKind::CustomBuild));
    if let Some(build_target) = custom_build_target {
        let variant_build = build_target_of(build_target);
        for rule in &mut rules {
            if let Some(rust_rule) = rule.as_rust_rule_mut() {
                patch_with_buildscript(rust_rule, &variant_build);
            }
        }
        rules.push(Rule::RustBinary(emit_buildscript_build(
            &variant_build,
            &variant_node,
            manifest_dir,
            ctx,
        )));
        rules.push(Rule::BuildscriptRun(emit_buildscript_run(
            &variant_node,
            &variant_build,
            ctx,
        )));
    }
    rules
}

/// Add the repository's `[fixups.<package>]` configuration to the package's rules.
fn apply_fixups(rules: &mut [Rule], node: &BuckalNode, ctx: &BuckalContext) {
    let Some(fixup) = ctx.repo_config.fixups.get(&node.name) else {
        return;
    };
    let mapped = fixup_mapped_srcs(node, fixup).unwrap_or_else(|error| {
        buckal_error!("{:#}", error);
        std::process::exit(1);
    });
    // A per-test fixup that names no test would silently not apply (with tests
    // generated at all).
    let tests: Set<&str> = rules
        .iter()
        .filter_map(|rule| match rule {
            Rule::RustTest(r) => Some(r.name.as_str()),
            _ => None,
        })
        .collect();
    if let Some(unknown) = fixup
        .tests
        .keys()
        .find(|name| !ctx.repo_config.ignore_tests && !tests.contains(name.as_str()))
    {
        buckal_error!(
            "`[fixups.{}.tests.{unknown}]` names no test of the package (its tests: {})",
            node.name,
            tests.iter().join(", ")
        );
        std::process::exit(1);
    }
    if let Err(error) = check_buildscript_fixup(&node.name, &fixup.buildscript) {
        buckal_error!("{:#}", error);
        std::process::exit(1);
    }
    let run = match &fixup.buildscript.run {
        Some(crate::config::BuildscriptRunEntry::Table(run)) => run.clone(),
        _ => Default::default(),
    };
    let build_scripts: Set<String> = rules
        .iter()
        .filter_map(|rule| match rule {
            Rule::BuildscriptRun(r) => r.buildscript_rule.strip_prefix(':').map(str::to_owned),
            _ => None,
        })
        .collect();
    for rule in rules {
        let (env, rustc_flags, mapped_srcs) = match rule {
            Rule::RustLibrary(r) => {
                r.deps.extend(fixup.extra_deps.iter().cloned());
                (&mut r.env, &mut r.rustc_flags, &mut r.mapped_srcs)
            }
            Rule::RustBinary(r) => {
                if build_scripts.contains(&r.name) {
                    r.deps
                        .extend(fixup.buildscript.build.extra_deps.iter().cloned());
                }
                (&mut r.env, &mut r.rustc_flags, &mut r.mapped_srcs)
            }
            Rule::RustTest(r) => {
                r.extra_resources
                    .extend(fixup.test_resources.iter().cloned());
                r.run_env.extend(fixup.test_run_env.clone());
                if let Some(remote_execution) = &fixup.test_remote_execution {
                    r.remote_execution = Some(remote_execution.clone());
                }
                if let Some(runtime_tools) = &fixup.test_runtime_tools {
                    r.runtime_tools = Some(runtime_tools.clone());
                }
                r.labels.extend(fixup.test_labels.iter().cloned());
                r.target_compatible_with
                    .extend(fixup.test_target_compatible_with.iter().cloned());
                r.args.extend(fixup.test_args.iter().cloned());
                if let Some(test) = fixup.tests.get(&r.name) {
                    r.labels.extend(test.labels.iter().cloned());
                    r.target_compatible_with
                        .extend(test.target_compatible_with.iter().cloned());
                    r.args.extend(test.args.iter().cloned());
                }
                (&mut r.env, &mut r.rustc_flags, &mut r.mapped_srcs)
            }
            Rule::BuildscriptRun(r) => {
                r.env.extend(fixup.buildscript_env.clone());
                r.rustc_link_lib |= run.rustc_link_lib;
                r.rustc_link_search |= run.rustc_link_search;
                continue;
            }
            _ => continue,
        };
        env.extend(fixup.env.clone());
        rustc_flags.extend(fixup.rustc_flags.iter().cloned());
        mapped_srcs.extend(mapped.clone());
    }
}

/// Refuse what generation cannot honour in `[fixups.<package>.buildscript]`: not
/// running the build script, and an `extra_srcs` pattern outside the package.
fn check_buildscript_fixup(
    package: &str,
    buildscript: &crate::config::BuildscriptFixup,
) -> anyhow::Result<()> {
    if buildscript.run == Some(crate::config::BuildscriptRunEntry::Run(false)) {
        anyhow::bail!(
            "`[fixups.{package}] buildscript.run = false`: generation always runs a package's build script"
        );
    }
    for pattern in &buildscript.build.extra_srcs {
        let path = Utf8Path::new(pattern);
        let inside = !pattern.is_empty()
            && path
                .components()
                .all(|c| matches!(c, cargo_metadata::camino::Utf8Component::Normal(_)));
        if !inside {
            anyhow::bail!(
                "`[fixups.{package}.buildscript.build] extra_srcs`: `{pattern}` is not a relative pattern inside the package"
            );
        }
    }
    Ok(())
}

/// Report compile-time includes of a first-party package that its sources do not
/// provide and that no `mapped_srcs` fixup declares, suggesting the entries that
/// would declare them.
fn report_unprovided_includes(node: &BuckalNode, ctx: &BuckalContext) {
    let package_dir = node.manifest_path.parent().unwrap();
    let includes = unprovided_includes(package_dir);
    if includes.is_empty() {
        return;
    }
    let (_, package) = package_location(node).unwrap_or_exit();
    let root = get_buck2_root().unwrap_or_exit_ctx("failed to get Buck2 root");
    // A refused fixup is reported when the fixups are applied.
    let declared: Set<String> = ctx
        .repo_config
        .fixups
        .get(&node.name)
        .and_then(|fixup| fixup_mapped_srcs(node, fixup).ok())
        .map(|mapped| mapped.into_values().collect())
        .unwrap_or_default();
    let mut suggestions = Vec::new();
    let mut unreachable = Vec::new();
    for include in &includes {
        match repository_path(&package, &include.resolved) {
            Ok(path) if declared.contains(path.as_str()) => {}
            Ok(path) => suggestions.push((include, path)),
            Err(_) => unreachable.push(include),
        }
    }
    let at = |include: &UnprovidedInclude| format!("`{}` in {}", include.written, include.from);
    if !suggestions.is_empty() {
        let nested = suggestions
            .iter()
            .filter(|(include, _)| include.kind == IncludeGap::NestedPackage)
            .count();
        let examples = suggestions
            .iter()
            .take(3)
            .map(|(include, path)| {
                format!(
                    "{} as `\"{}\" = \"{}\"`",
                    at(include),
                    export_label_hint(&root, path),
                    include.resolved
                )
            })
            .join(", ");
        buckal_warn!(
            "Package `{}` includes {} file(s) that its sources do not contain ({} from nested Buck packages, {} from outside its directory). Declare each with `mapped_srcs` under `[fixups.{}]` in buckal.toml, as a label exporting the file (e.g. through `export_files` of its package) mapped to the path as included, e.g. {}.",
            node.name,
            suggestions.len(),
            nested,
            suggestions.len() - nested,
            node.name,
            examples
        );
    }
    if !unreachable.is_empty() {
        buckal_warn!(
            "Package `{}` includes {} file(s) from outside the repository (e.g. {}), which no build input can provide.",
            node.name,
            unreachable.len(),
            unreachable
                .iter()
                .take(3)
                .map(|include| at(include))
                .join(", ")
        );
    }
}

/// A label that would export the file at repository path `path`: its name relative
/// to the nearest enclosing Buck package, a directory with a BUCK file or a Cargo
/// package (whose BUCK file generation writes).
fn export_label_hint(root: &Utf8Path, path: &Utf8Path) -> String {
    let owner = path
        .ancestors()
        .skip(1)
        .find(|dir| {
            let dir = root.join(dir);
            dir.join("BUCK").exists() || dir.join("Cargo.toml").exists()
        })
        .unwrap_or(Utf8Path::new(""));
    let name = path.strip_prefix(owner).expect("ancestor of the path");
    format!("//{owner}:{name}")
}

/// The package's directory and its repository path: the first-party package
/// directory, or the vendor directory holding a third-party package's BUCK file.
fn package_location(node: &BuckalNode) -> anyhow::Result<(Utf8PathBuf, Utf8PathBuf)> {
    let root = get_buck2_root()?;
    let dir = if matches!(node.kind, NodeKind::FirstParty { .. }) {
        node.manifest_path
            .parent()
            .expect("manifest has a parent")
            .to_owned()
    } else {
        get_vendor_dir(&node.package_id)?
    };
    let package = dir
        .strip_prefix(&root)
        .map_err(|_| {
            anyhow::anyhow!(
                "package `{}` at `{dir}` is outside the repository `{root}`",
                node.name
            )
        })?
        .to_owned();
    Ok((dir, package))
}

/// The package's `[fixups] mapped_srcs` with destinations as repository paths,
/// refusing a destination that is absolute, leaves the repository, is the package
/// directory, repeats, or clashes with the package's own sources or another
/// destination (the same path, or one a directory prefix of the other).
fn fixup_mapped_srcs(
    node: &BuckalNode,
    fixup: &crate::config::PackageFixup,
) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    if fixup.mapped_srcs.is_empty() {
        return Ok(Default::default());
    }
    let (package_dir, package) = package_location(node)?;
    repository_mapped_srcs(&node.name, &fixup.mapped_srcs, &package_dir, &package)
}

/// `mapped_srcs` written relative to the package at `package_dir` (repository path
/// `package`), with repository-path destinations; see [`fixup_mapped_srcs`].
fn repository_mapped_srcs(
    name: &str,
    written_srcs: &std::collections::BTreeMap<String, String>,
    package_dir: &Utf8Path,
    package: &Utf8Path,
) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    let mut mapped = std::collections::BTreeMap::new();
    let mut destinations: std::collections::BTreeMap<Utf8PathBuf, &str> = Default::default();
    for (label, written) in written_srcs {
        let refuse = |why: String| {
            anyhow::anyhow!("`[fixups.{name}] mapped_srcs`: `{label}` = `{written}`: {why}")
        };
        let destination = repository_path(package, written).map_err(refuse)?;
        if let Ok(own) = destination.strip_prefix(package) {
            if own.as_str().is_empty() {
                return Err(refuse(
                    "the destination is the package directory".to_owned(),
                ));
            }
            // The package glob provides every file outside nested Buck packages:
            // the destination may be neither such a file, nor a directory holding
            // them, nor below such a file.
            let on_disk = package_dir.join(own);
            let globbed = !in_nested_package(package_dir, own)
                && !(on_disk.is_dir() && on_disk.join("BUCK").exists());
            if globbed && on_disk.is_file() {
                return Err(refuse(format!(
                    "`{destination}` is a file of the package's own sources"
                )));
            }
            if globbed && on_disk.is_dir() {
                return Err(refuse(format!(
                    "`{destination}` is a directory of the package's own sources"
                )));
            }
            if let Some(file) = own
                .ancestors()
                .skip(1)
                .filter(|dir| !dir.as_str().is_empty())
                .find(|dir| package_dir.join(dir).is_file())
            {
                return Err(refuse(format!(
                    "`{destination}` is below `{package}/{file}`, a file of the package's own sources"
                )));
            }
        }
        if let Some(other) = destinations.insert(destination.clone(), label) {
            return Err(refuse(format!(
                "`{destination}` is also the destination of `{other}`"
            )));
        }
        mapped.insert(label.clone(), destination.to_string());
    }
    // A destination that is a directory prefix of another would need to be both a
    // file and a directory.
    for (destination, label) in &destinations {
        if let Some((prefix, prefix_label)) = destination
            .ancestors()
            .skip(1)
            .find_map(|dir| destinations.get_key_value(dir))
        {
            anyhow::bail!(
                "`[fixups.{name}] mapped_srcs`: `{label}` = `{}`: `{destination}` is below `{prefix}`, the destination of `{prefix_label}`",
                written_srcs[*label]
            );
        }
    }
    Ok(mapped)
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
    gen_buck_content_preserving(rules, &[], &[])
}

/// [`gen_buck_content`], keeping hand-written `loads` (after the generated ones) and
/// `statements` (after the generated rules) of the file it replaces, verbatim.
pub fn gen_buck_content_preserving(
    rules: &[Rule],
    preserved_loads: &[String],
    preserved_statements: &[String],
) -> String {
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

    for statement in preserved_statements {
        content.push('\n');
        content.push_str(statement);
        content.push('\n');
    }

    if !loads.is_empty() || !preserved_loads.is_empty() {
        let mut loads_string = loads
            .iter()
            .map(serde_starlark::to_string)
            .map(|r| r.unwrap())
            .join("");
        for load in preserved_loads {
            loads_string.push_str(load);
            loads_string.push('\n');
        }
        content.insert(0, '\n');
        content.insert_str(0, &loads_string);
    }

    content.insert_str(0, "\n\n");
    content.insert_str(0, GENERATED_HEADER);

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
            src_path: Utf8PathBuf::from("/nonexistent-buckal-test-package/dummy.rs"),
            doctest: true,
            required_features: Vec::new(),
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
            platform_features: Default::default(),
            kind: NodeKind::FirstParty {
                relative_path: "".to_string(),
            },
            edition: Edition::E2021,
            manifest_path: Utf8PathBuf::from("/nonexistent-buckal-test-package/Cargo.toml"),
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
    fn test_integration_test_names_binaries_and_gets_no_path_env() {
        // Package `my-tool` whose binaries are not named after the package
        // (as in a package `observatory_db_feedback_cli` with binary `observatory-feedback`).
        let lib = mock_target("my_tool", TargetKind::Lib);
        let bin_a = mock_target("my-cli", TargetKind::Bin);
        let bin_b = mock_target("helper", TargetKind::Bin);
        let test = mock_target("cli", TargetKind::Test);

        let node = mock_node("my-tool", vec![lib, bin_a, bin_b, test]);
        let rules = buckify_root_node(&node, &test_ctx());

        let test_rule = find_test(&rules, "cli");
        assert!(test_rule.deps.contains(":my_tool"));
        // The binaries are named for the prelude, which writes CARGO_BIN_EXE_*.
        assert_eq!(
            test_rule.cargo_bin_exes,
            [
                ("helper".to_owned(), ":helper".to_owned()),
                ("my-cli".to_owned(), ":my-cli".to_owned()),
            ]
            .into()
        );
        assert!(
            !test_rule
                .env
                .keys()
                .any(|k| k.starts_with("CARGO_BIN_EXE_"))
        );
        assert!(test_rule.run_env.is_empty());
        let content = gen_buck_content(&rules);
        assert!(
            content.contains("    cargo_bin_exes = {\n        \"helper\": \":helper\",\n        \"my-cli\": \":my-cli\",\n    },\n"),
            "{content}"
        );

        // Cargo sets CARGO_BIN_EXE_* for integration tests (and benches) only.
        let unit = find_test(&rules, "unittest");
        assert!(unit.cargo_bin_exes.is_empty());
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
            required_features: Vec::new(),
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
            platform_features: Default::default(),
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
                    assert_eq!(
                        l.srcs,
                        Set::from([":vendor[crates/rig-core]".to_owned()]).into()
                    );
                    assert_eq!(l.crate_root, "vendor/crates/rig-core/src/lib.rs");
                }
                Rule::RustBinary(b) => {
                    assert_eq!(
                        b.srcs,
                        Set::from([":vendor[crates/rig-core]".to_owned()]).into()
                    );
                    assert_eq!(b.crate_root, "vendor/crates/rig-core/build.rs");
                }
                Rule::BuildscriptRun(r) => {
                    assert_eq!(r.manifest_dir, ":vendor[crates/rig-core]");
                }
                _ => {}
            }
        }
    }

    #[test]
    fn test_fixups_reach_buildscript_run_and_crate_rules() {
        let manifest_dir = Utf8PathBuf::from("/registry/openssl-sys-0.9.117");
        let target = |name: &str, kind: TargetKind, file: &str| BuckalTarget {
            name: name.to_owned(),
            kind: vec![kind],
            src_path: manifest_dir.join(file),
            doctest: true,
            required_features: Vec::new(),
            test: true,
        };
        let node = BuckalNode {
            package_id: PackageId {
                repr: "registry+https://github.com/rust-lang/crates.io-index#openssl-sys@0.9.117"
                    .to_owned(),
            },
            name: "openssl-sys".to_owned(),
            version: "0.9.117".to_owned(),
            features: vec![],
            platform_features: Default::default(),
            kind: NodeKind::ThirdParty,
            edition: Edition::E2021,
            manifest_path: manifest_dir.join("Cargo.toml"),
            targets: vec![
                target("openssl_sys", TargetKind::Lib, "src/lib.rs"),
                target(
                    "build-script-main",
                    TargetKind::CustomBuild,
                    "build/main.rs",
                ),
            ],
            source: Some("registry+https://github.com/rust-lang/crates.io-index".to_owned()),
            links: Some("openssl".to_owned()),
            checksum: Some("0".repeat(64)),
        };

        let mut ctx = test_ctx();
        ctx.repo_config.fixups.insert(
            "openssl-sys".to_owned(),
            crate::config::PackageFixup {
                buildscript_env: [
                    ("OPENSSL_NO_VENDOR".to_owned(), "1".to_owned()),
                    ("OPENSSL_LIB_DIR".to_owned(), "/opt/openssl/lib".to_owned()),
                ]
                .into(),
                env: Default::default(),
                rustc_flags: vec!["-Lnative=/opt/openssl/lib".to_owned()],
                mapped_srcs: Default::default(),
                test_resources: Default::default(),
                test_remote_execution: None,
                test_run_env: Default::default(),
                test_runtime_tools: None,
                export_files: Default::default(),
                test_labels: Default::default(),
                test_target_compatible_with: Default::default(),
                test_args: Default::default(),
                tests: Default::default(),
                extra_deps: Default::default(),
                buildscript: Default::default(),
            },
        );

        let rules = buckify_dep_node(&node, &ctx);

        let run = rules
            .iter()
            .find_map(|r| match r {
                Rule::BuildscriptRun(r) => Some(r),
                _ => None,
            })
            .expect("buildscript_run");
        assert_eq!(
            run.env.get("OPENSSL_NO_VENDOR").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            run.env.get("OPENSSL_LIB_DIR").map(String::as_str),
            Some("/opt/openssl/lib")
        );

        let lib = rules
            .iter()
            .find_map(|r| match r {
                Rule::RustLibrary(l) => Some(l),
                _ => None,
            })
            .expect("rust_library");
        assert!(
            lib.rustc_flags
                .contains(&"-Lnative=/opt/openssl/lib".to_owned())
        );
        // The generated flags are kept.
        assert!(
            lib.rustc_flags
                .contains(&"@$(location :manifest[env_flags])".to_owned())
        );

        // Other packages are untouched.
        let mut other = node.clone();
        other.name = "libc".to_owned();
        let other_rules = buckify_dep_node(&other, &ctx);
        for rule in &other_rules {
            if let Rule::BuildscriptRun(r) = rule {
                assert!(r.env.is_empty());
            }
        }
    }

    #[test]
    fn test_fixup_rustc_flags_keep_order_and_repeats() {
        let lib = mock_target("sdk", TargetKind::Lib);
        let build = mock_target("build-script-build", TargetKind::CustomBuild);
        let node = mock_node("sdk", vec![lib, build]);
        let flags: Vec<String> = ["-C", "opt-level=2", "-C", "debuginfo=1"]
            .map(str::to_owned)
            .into();
        let mut ctx = test_ctx();
        ctx.repo_config.fixups.insert(
            "sdk".to_owned(),
            crate::config::PackageFixup {
                rustc_flags: flags.clone(),
                ..Default::default()
            },
        );

        let rules = buckify_root_node(&node, &ctx);
        let lib = rules
            .iter()
            .find_map(|r| match r {
                Rule::RustLibrary(l) => Some(l),
                _ => None,
            })
            .expect("rust_library");
        let mut expected = vec![
            "@$(location :build-script-run[rustc_flags])".to_owned(),
            "@$(location :manifest[env_flags])".to_owned(),
        ];
        expected.extend(flags.iter().cloned());
        assert_eq!(lib.rustc_flags, expected);

        // Serialized in that order, with both `-C`.
        let content = gen_buck_content(&rules);
        assert!(
            content.contains(
                "        \"-C\",\n        \"opt-level=2\",\n        \"-C\",\n        \"debuginfo=1\",\n"
            ),
            "{content}"
        );

        // Read back from the BUCK file in the same order, and a merge with the existing
        // file neither drops nor duplicates arguments.
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("BUCK")).unwrap();
        std::fs::write(&path, &content).unwrap();
        let existing = crate::buck::parse_buck_file(&path).unwrap();
        let parsed = existing
            .values()
            .find_map(|r| match r {
                Rule::RustLibrary(l) if l.name == "sdk" => Some(l),
                _ => None,
            })
            .expect("parsed rust_library");
        assert_eq!(parsed.rustc_flags, expected);
        let mut regenerated = rules.clone();
        crate::buck::patch_buck_rules(
            &existing,
            &mut regenerated,
            &Set::from(["rustc_flags".to_owned()]),
            &[],
        );
        assert_eq!(gen_buck_content(&regenerated), content);
    }

    #[test]
    fn test_fixup_test_resources_reach_tests_only() {
        let (_dir, node) = first_party_package("conta/wire", &["src/lib.rs", "tests/wire.rs"]);
        let mut ctx = test_ctx();
        let label = "//conta/wire/src/generated:files".to_owned();
        ctx.repo_config.fixups.insert(
            node.name.clone(),
            crate::config::PackageFixup {
                test_resources: vec![label.clone()],
                ..Default::default()
            },
        );
        let rules = buckify_root_node(&node, &ctx);
        for test in ["unittest", "wire"] {
            let t = find_test(&rules, test);
            assert_eq!(t.extra_resources, Set::from([label.clone()]));
            // The package's own files stay declared.
            assert!(t.resources.is_some());
        }
        let content = gen_buck_content(&rules);
        assert!(
            content.contains("    extra_resources = [\"//conta/wire/src/generated:files\"],\n"),
            "{content}"
        );
        assert_eq!(content.matches("extra_resources").count(), 2, "{content}");
    }

    #[test]
    fn test_fixup_test_execution_reaches_every_test_and_nothing_else() {
        let (_dir, mut node) = first_party_package("pkg", &["src/lib.rs", "tests/wire.rs"]);
        node.targets.push(BuckalTarget {
            name: "tool".to_owned(),
            kind: vec![TargetKind::Bin],
            src_path: node.manifest_path.parent().unwrap().join("src/main.rs"),
            doctest: false,
            required_features: Vec::new(),
            test: false,
        });
        let mut ctx = test_ctx();
        let plain = gen_buck_content(&buckify_root_node(&node, &ctx));
        assert!(!plain.contains("remote_execution"), "{plain}");
        assert!(!plain.contains("run_env"), "{plain}");

        ctx.repo_config.fixups.insert(
            node.name.clone(),
            crate::config::PackageFixup {
                test_remote_execution: Some("fixture_fs_profile".to_owned()),
                test_run_env: [("FIXTURE_ROOT".to_owned(), "/srv/fixtures".to_owned())].into(),
                ..Default::default()
            },
        );
        let rules = buckify_root_node(&node, &ctx);
        for test in ["unittest", "wire"] {
            let t = find_test(&rules, test);
            assert_eq!(t.remote_execution.as_deref(), Some("fixture_fs_profile"));
            assert_eq!(
                t.run_env,
                [("FIXTURE_ROOT".to_owned(), "/srv/fixtures".to_owned())].into()
            );
            // Compile-time env is untouched: run_env is for running only.
            assert!(!t.env.contains_key("FIXTURE_ROOT"));
        }
        let content = gen_buck_content(&rules);
        assert!(
            content.contains("    remote_execution = \"fixture_fs_profile\",\n"),
            "{content}"
        );
        assert!(
            content.contains(
                "    run_env = {\n        \"FIXTURE_ROOT\": \"/srv/fixtures\",\n    },\n"
            ),
            "{content}"
        );
        // Only the two tests carry them; the library and the binary do not.
        assert_eq!(content.matches("remote_execution").count(), 2, "{content}");
        assert_eq!(content.matches("run_env").count(), 2, "{content}");
        // Everything else is as without the fixup.
        let stripped = content
            .replace("    remote_execution = \"fixture_fs_profile\",\n", "")
            .replace(
                "    run_env = {\n        \"FIXTURE_ROOT\": \"/srv/fixtures\",\n    },\n",
                "",
            );
        assert_eq!(stripped, plain);
    }

    #[test]
    fn test_per_platform_features_emit_platform_attribute() {
        let lib = mock_target("sdk", TargetKind::Lib);
        let mut node = mock_node("sdk", vec![lib]);
        // What `cargo metadata` reports: the union, including the wasm-only feature.
        node.features = vec!["browser".to_owned(), "default".to_owned()];
        node.platform_features = crate::platform_features::PlatformResolution {
            features: [
                ("linux-x86_64".to_owned(), Set::from(["default".to_owned()])),
                (
                    "wasm32".to_owned(),
                    Set::from(["browser".to_owned(), "default".to_owned()]),
                ),
            ]
            .into(),
            ..Default::default()
        };

        let rules = buckify_root_node(&node, &test_ctx());
        let lib_rule = rules
            .iter()
            .find_map(|r| match r {
                Rule::RustLibrary(l) => Some(l),
                _ => None,
            })
            .expect("rust_library");
        assert_eq!(lib_rule.features, Set::from(["default".to_owned()]));
        assert_eq!(
            lib_rule.platform["wasm32"]["features"].as_list(),
            Some(&Set::from(["browser".to_owned()]))
        );
        assert!(!lib_rule.platform.contains_key("linux-x86_64"));

        let unit = find_test(&rules, "unittest");
        assert_eq!(unit.features, Set::from(["default".to_owned()]));

        let content = gen_buck_content(&rules);
        assert!(
            content.contains("    platform = {\n        \"wasm32\": {\n            \"features\": [\"browser\"],\n        },\n    },\n"),
            "{content}"
        );
    }

    #[test]
    fn test_without_per_platform_features_output_is_unchanged() {
        let lib = mock_target("sdk", TargetKind::Lib);
        let mut node = mock_node("sdk", vec![lib]);
        node.features = vec!["browser".to_owned(), "default".to_owned()];

        let rules = buckify_root_node(&node, &test_ctx());
        let content = gen_buck_content(&rules);
        assert!(!content.contains("platform = "), "{content}");
        assert!(content.contains("\"browser\""), "{content}");
    }

    fn first_party_package(relative_path: &str, files: &[&str]) -> (tempfile::TempDir, BuckalNode) {
        let dir = tempfile::tempdir().unwrap();
        let pkg = Utf8PathBuf::from_path_buf(dir.path().join(relative_path)).unwrap();
        for file in files {
            let path = pkg.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let target = |name: &str, kind: TargetKind, file: &str| BuckalTarget {
            name: name.to_owned(),
            kind: vec![kind],
            src_path: pkg.join(file),
            doctest: true,
            required_features: Vec::new(),
            test: true,
        };
        let mut node = mock_node(
            "conta-wire",
            vec![
                target("conta_wire", TargetKind::Lib, "src/lib.rs"),
                target("wire", TargetKind::Test, "tests/wire.rs"),
            ],
        );
        node.kind = NodeKind::FirstParty {
            relative_path: relative_path.to_owned(),
        };
        node.manifest_path = pkg.join("Cargo.toml");
        (dir, node)
    }

    #[test]
    fn test_first_party_sources_compile_at_their_checkout_paths() {
        let (_dir, node) = first_party_package(
            "conta/wire",
            &[
                "src/lib.rs",
                "tests/wire.rs",
                "testdata/snapshots/a__b.snap",
            ],
        );
        let rules = buckify_root_node(&node, &test_ctx());
        let content = gen_buck_content(&rules);

        let lib = rules
            .iter()
            .find_map(|r| match r {
                Rule::RustLibrary(l) => Some(l),
                _ => None,
            })
            .expect("rust_library");
        // file!() becomes `conta/wire/src/lib.rs`: the prelude places the sources at
        // their repository paths and remaps the sources directory away.
        assert_eq!(lib.crate_root, "src/lib.rs");
        assert!(
            content.contains("    srcs = glob([\"**/**\"]),\n"),
            "{content}"
        );
        assert!(!content.contains("crate_root = \"vendor/"), "{content}");

        // cargo_manifest still reads the vendor filegroup.
        assert!(content.contains("    vendor = \":vendor\",\n"), "{content}");

        for test in ["unittest", "wire"] {
            let t = find_test(&rules, test);
            // The runtime CARGO_MANIFEST_DIR is the prelude's to write.
            assert!(t.run_env.is_empty());
            assert_eq!(
                t.resources.as_ref().map(|g| g.include.clone()),
                Some(Set::from(["**/**".to_owned()]))
            );
        }
        assert_eq!(find_test(&rules, "wire").crate_root, "tests/wire.rs");
        assert!(
            content.contains("    resources = glob([\"**/**\"]),\n"),
            "{content}"
        );
        assert!(!content.contains("run_env"), "{content}");
    }

    #[test]
    fn test_test_resources_do_not_depend_on_existing_files() {
        // No snapshot or data file yet: the tests still declare the package's files,
        // so the first snapshot (or a source file a test reads, e.g. `src/generated`)
        // is an input without regenerating the BUCK file.
        let (_dir, node) =
            first_party_package("conta/inspect/render", &["src/lib.rs", "tests/wire.rs"]);
        let rules = buckify_root_node(&node, &test_ctx());
        let before = gen_buck_content(&rules);
        for test in ["unittest", "wire"] {
            assert_eq!(
                find_test(&rules, test)
                    .resources
                    .as_ref()
                    .map(|g| g.include.clone()),
                Some(Set::from(["**/**".to_owned()]))
            );
        }
        let pkg = node.manifest_path.parent().unwrap().to_owned();
        std::fs::create_dir_all(pkg.join("src/snapshots")).unwrap();
        std::fs::write(pkg.join("src/snapshots/x.snap"), "").unwrap();
        assert_eq!(
            gen_buck_content(&buckify_root_node(&node, &test_ctx())),
            before
        );
    }

    #[test]
    fn test_includes_declared_through_repository_mapped_srcs() {
        // As text/glyph_vocab: `src/internal.rs` includes `../vocab/bpe-52.toml`, and
        // `vocab/` is its own Buck package that exports the file; `src/types.rs`
        // includes a file of another package through `../`.
        let (_dir, node) = first_party_package(
            "text/glyph_vocab",
            &[
                "src/lib.rs",
                "tests/wire.rs",
                "vocab/BUCK",
                "vocab/bpe-52.toml",
            ],
        );
        let pkg = node.manifest_path.parent().unwrap().to_owned();
        std::fs::write(
            pkg.join("src/internal.rs"),
            "pub const A: &str = include_str!(\"../vocab/bpe-52.toml\");",
        )
        .unwrap();
        std::fs::write(
            pkg.join("src/types.rs"),
            "include!(\"../../../callosum/client/src/types/e.rs\");",
        )
        .unwrap();
        assert_eq!(
            super::super::includes::unprovided_includes(&pkg)
                .into_iter()
                .map(|i| (i.resolved, i.kind))
                .collect::<Vec<_>>(),
            vec![
                ("vocab/bpe-52.toml".to_owned(), IncludeGap::NestedPackage),
                (
                    "../../callosum/client/src/types/e.rs".to_owned(),
                    IncludeGap::OutsidePackage
                ),
            ]
        );

        let package = Utf8Path::new("text/glyph_vocab");
        let written = |entries: &[(&str, &str)]| -> std::collections::BTreeMap<String, String> {
            entries
                .iter()
                .map(|(label, path)| (label.to_string(), path.to_string()))
                .collect()
        };
        let declared = written(&[
            (
                "//text/glyph_vocab/vocab:compiler_input/bpe-52.toml",
                "vocab/bpe-52.toml",
            ),
            (
                "//callosum/client:src/types/e.rs",
                "../../callosum/client/src/types/e.rs",
            ),
        ]);
        assert_eq!(
            repository_mapped_srcs("text_glyph_vocab", &declared, &pkg, package).unwrap(),
            written(&[
                (
                    "//text/glyph_vocab/vocab:compiler_input/bpe-52.toml",
                    "text/glyph_vocab/vocab/bpe-52.toml",
                ),
                (
                    "//callosum/client:src/types/e.rs",
                    "callosum/client/src/types/e.rs",
                ),
            ])
        );

        for (refused, why) in [
            (written(&[("//x:a", "/abs/a.rs")]), "is not a relative path"),
            (
                written(&[("//x:a", "../../../a.rs")]),
                "leaves the repository",
            ),
            (written(&[("//x:a", ".")]), "is the package directory"),
            (written(&[("//x:a", "src/lib.rs")]), "own sources"),
            (
                written(&[("//x:a", "src")]),
                "is a directory of the package's own sources",
            ),
            (
                written(&[("//x:a", "src/lib.rs/x.rs")]),
                "a file of the package's own sources",
            ),
            (
                written(&[("//x:a", "../x/a.rs"), ("//x:b", "../x/./a.rs")]),
                "is also the destination of `//x:a`",
            ),
            (
                written(&[("//x:a", "../x/a"), ("//x:b", "../x/a/b.rs")]),
                "`text/x/a/b.rs` is below `text/x/a`, the destination of `//x:a`",
            ),
        ] {
            let error = repository_mapped_srcs("text_glyph_vocab", &refused, &pkg, package)
                .unwrap_err()
                .to_string();
            assert!(
                error.starts_with("`[fixups.text_glyph_vocab] mapped_srcs`: `//x:")
                    && error.contains(why),
                "{error}"
            );
        }
    }

    #[test]
    fn test_host_variant_emits_host_rules() {
        let lib = mock_target("json", TargetKind::Lib);
        let build = mock_target("build-script-build", TargetKind::CustomBuild);
        let mut node = mock_node("json", vec![lib, build]);
        let linux = |features: &[&str]| {
            std::collections::BTreeMap::from([(
                "linux-x86_64".to_owned(),
                features.iter().map(|f| f.to_string()).collect::<Set<_>>(),
            )])
        };
        node.platform_features = crate::platform_features::PlatformResolution {
            features: linux(&["std"]),
            deps: crate::platform_features::Edges {
                normal: linux(&[]),
                ..Default::default()
            },
            host_variant: Some(crate::platform_features::HostVariant {
                features: linux(&["preserve_order", "std"]),
                deps: crate::platform_features::Edges {
                    normal: linux(&[]),
                    ..Default::default()
                },
            }),
            ..Default::default()
        };

        let rules = buckify_root_node(&node, &test_ctx());
        let library = |name: &str| {
            rules
                .iter()
                .find_map(|r| match r {
                    Rule::RustLibrary(l) if l.name == name => Some(l),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("rust_library {name}"))
        };
        assert_eq!(library("json").features, Set::from(["std".to_owned()]));
        assert_eq!(
            library("json").env["OUT_DIR"],
            "$(location :build-script-run[out_dir])"
        );
        let host = library("json-host");
        assert_eq!(
            host.features,
            Set::from(["preserve_order".to_owned(), "std".to_owned()])
        );
        assert_eq!(
            host.env["OUT_DIR"],
            "$(location :build-script-host-run[out_dir])"
        );
        assert!(
            host.rustc_flags
                .contains(&"@$(location :build-script-host-run[rustc_flags])".to_owned())
        );
        assert!(
            !host
                .rustc_flags
                .contains(&"@$(location :build-script-run[rustc_flags])".to_owned())
        );

        let run = rules
            .iter()
            .find_map(|r| match r {
                Rule::BuildscriptRun(b) if b.name == "build-script-host-run" => Some(b),
                _ => None,
            })
            .expect("host build script run");
        assert_eq!(run.buildscript_rule, ":build-script-host-build");
        assert_eq!(
            run.features,
            Set::from(["preserve_order".to_owned(), "std".to_owned()])
        );
        assert!(
            rules
                .iter()
                .any(|r| matches!(r, Rule::RustBinary(b) if b.name == "build-script-host-build"))
        );

        // Without a host variant nothing extra is emitted.
        node.platform_features.host_variant = None;
        let rules = buckify_root_node(&node, &test_ctx());
        assert!(!gen_buck_content(&rules).contains("-host"));
    }
}
