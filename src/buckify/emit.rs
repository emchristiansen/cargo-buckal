use std::{
    borrow::Cow,
    collections::{BTreeMap as Map, BTreeSet as Set},
};

use cargo_metadata::{
    TargetKind,
    camino::{Utf8Path, Utf8PathBuf},
};
use cargo_util_schemas::lockfile::TomlLockfileSourceId;

use crate::{
    buck::{
        BuildscriptRun, CargoManifest, CargoTargetKind, ExportFile, FileGroup, GitFetch, Glob,
        HttpArchive, RustBinary, RustLibrary, RustRule, RustTest, Srcs,
    },
    buckify::actions::is_third_party,
    context::BuckalContext,
    platform::{buck_labels, lookup_platforms},
    platform_features::{PlatformAttrs, platform_list_mut, split_features},
    resolve::{BuckalDepKind, BuckalNode, BuckalTarget, NodeKind},
    utils::{UnwrapOrExit, get_buck2_root, get_cfgs, get_target, get_vendor_path_relative},
};

use super::deps::{
    dep_kind_matches, host_variant_name, set_deps, test_host_variant_name, test_variant_name,
    uses_host_variant,
};

/// Emit `rust_library` rule for the given lib target
pub(super) fn emit_rust_library(
    node: &BuckalNode,
    lib_target: &BuckalTarget,
    manifest_dir: &Utf8PathBuf,
    buckal_name: &str,
    ctx: &BuckalContext,
) -> RustLibrary {
    let vendor = vendor_ref(node);
    let (features, platform) = node_features(node);
    let mut rust_library = RustLibrary {
        name: buckal_name.to_owned(),
        srcs: vendor.srcs.clone(),
        crate_name: lib_target.name.to_owned().replace("-", "_"),
        edition: node.edition.to_string(),
        features,
        platform,
        rustc_flags: vec!["@$(location :manifest[env_flags])".to_owned()],
        visibility: Set::from(["PUBLIC".to_owned()]),
        ..Default::default()
    };

    if lib_target.kind.contains(&TargetKind::ProcMacro) {
        rust_library.proc_macro = Some(true);
    }

    // Set the crate root path
    rust_library.crate_root = vendor.crate_path(&normalize_path_for_buck(
        lib_target
            .src_path
            .to_owned()
            .strip_prefix(manifest_dir)
            .expect("Failed to get library source path")
            .as_str(),
    ));

    // look up platform compatibility
    if let Some(platforms) = lookup_platforms(&node.name) {
        rust_library.compatible_with = buck_labels(&platforms);
    }
    let host = node.platform_features.host_context;
    platform_compatibility(
        node,
        ctx,
        host,
        &mut rust_library.target_compatible_with,
        &mut rust_library.platform,
    );

    // Set dependencies
    set_deps(&mut rust_library, node, CargoTargetKind::Lib, ctx)
        .unwrap_or_exit_ctx(format!("failed to set dependencies for '{}'", buckal_name));

    rust_library
}

/// Emit `rust_binary` rule for the given bin target
pub(super) fn emit_rust_binary(
    node: &BuckalNode,
    bin_target: &BuckalTarget,
    manifest_dir: &Utf8PathBuf,
    buckal_name: &str,
    ctx: &BuckalContext,
) -> RustBinary {
    let vendor = vendor_ref(node);
    let (features, platform) = node_features(node);
    let mut rust_binary = RustBinary {
        name: buckal_name.to_owned(),
        srcs: vendor.srcs.clone(),
        crate_name: bin_target.name.to_owned().replace("-", "_"),
        edition: node.edition.to_string(),
        features,
        platform,
        rustc_flags: vec!["@$(location :manifest[env_flags])".to_owned()],
        visibility: Set::from(["PUBLIC".to_owned()]),
        ..Default::default()
    };

    // Set the crate root path
    rust_binary.crate_root = vendor.crate_path(&normalize_path_for_buck(
        bin_target
            .src_path
            .to_owned()
            .strip_prefix(manifest_dir)
            .expect("Failed to get binary source path")
            .as_str(),
    ));

    // Set dependencies
    set_deps(&mut rust_binary, node, CargoTargetKind::Bin, ctx)
        .unwrap_or_exit_ctx(format!("failed to set dependencies for '{}'", buckal_name));

    if let Some(platforms) = lookup_platforms(&node.name) {
        rust_binary.compatible_with = buck_labels(&platforms);
    }
    let host = node.platform_features.host_context;
    platform_compatibility(
        node,
        ctx,
        host,
        &mut rust_binary.target_compatible_with,
        &mut rust_binary.platform,
    );

    rust_binary
}

/// Emit `rust_test` rule for the given bin target
pub(super) fn emit_rust_test(
    node: &BuckalNode,
    test_target: &BuckalTarget,
    manifest_dir: &Utf8PathBuf,
    buckal_name: &str,
    ctx: &BuckalContext,
) -> RustTest {
    let vendor = vendor_ref(node);
    let (features, platform) = node_features(node);
    let mut rust_test = RustTest {
        name: buckal_name.to_owned(),
        srcs: vendor.srcs.clone(),
        crate_name: test_target.name.to_owned().replace("-", "_"),
        edition: node.edition.to_string(),
        features,
        platform,
        rustc_flags: vec!["@$(location :manifest[env_flags])".to_owned()],
        visibility: Set::from(["PUBLIC".to_owned()]),
        ..Default::default()
    };

    // Set the crate root path
    rust_test.crate_root = vendor.crate_path(&normalize_path_for_buck(
        test_target
            .src_path
            .to_owned()
            .strip_prefix(manifest_dir)
            .expect("Failed to get test source path")
            .as_str(),
    ));

    // Set dependencies
    set_deps(&mut rust_test, node, CargoTargetKind::Test, ctx)
        .unwrap_or_exit_ctx(format!("failed to set dependencies for '{}'", buckal_name));

    // The runtime `CARGO_MANIFEST_DIR` (the package directory that `file!()` paths are
    // relative to) is not emitted here: the Buck2 fork's prelude is the single writer of
    // a test's path-valued runtime environment.
    if matches!(node.kind, NodeKind::FirstParty { .. }) {
        // A Cargo test runs inside the checkout and may read any file of its package
        // at run time: insta snapshots, test data, even its own sources (e.g. a test
        // that lists `src/generated`). Declare the package's files as resources, so
        // they are inputs of the test at their project paths. This does not depend on
        // what exists at generation time, so adding the first snapshot (or data file)
        // needs no regeneration.
        rust_test.resources = Some(Glob {
            include: Set::from([PACKAGE_FILES_GLOB.to_owned()]),
            ..Default::default()
        });
    }

    if let Some(platforms) = lookup_platforms(&node.name) {
        rust_test.compatible_with = buck_labels(&platforms);
    }
    let host = node.platform_features.host_context;
    platform_compatibility(
        node,
        ctx,
        host,
        &mut rust_test.target_compatible_with,
        &mut rust_test.platform,
    );

    rust_test
}

/// The prelude's constraint that no platform satisfies.
pub(super) const INCOMPATIBLE: &str = "prelude//:none";

/// Where Cargo builds a compilation of `node` (its resolution), as the rule's
/// `target_compatible_with`. With configured platforms, a package that none of
/// them builds (a dependency only for other targets, e.g. macOS or Windows) is
/// incompatible everywhere. A target compilation is incompatible on each
/// configured platform its resolution lacks (one built for wasm32 only), through
/// the `platform` attribute. A host compilation (`host`) is configured for the
/// execution platform, while its resolution is keyed by the target it serves, so
/// only the first case applies to it, and to a package host compilations use
/// through these rules (no host variant).
fn platform_compatibility(
    node: &BuckalNode,
    ctx: &BuckalContext,
    host: bool,
    target_compatible_with: &mut Set<String>,
    platform: &mut PlatformAttrs,
) {
    let configured = &ctx.repo_config.feature_platforms;
    if configured.is_empty() {
        return;
    }
    let resolution = &node.platform_features;
    if resolution.is_empty() {
        target_compatible_with.insert(INCOMPATIBLE.to_owned());
        return;
    }
    // Host compilations that use these rules are configured for the execution
    // platform, whatever target they serve: marking a platform would make them
    // incompatible there (Fable N6).
    if host || resolution.host_uses_plain_rules {
        return;
    }
    for name in configured.keys() {
        if !resolution.features.contains_key(name) {
            platform_list_mut(platform, name, "target_compatible_with")
                .insert(INCOMPATIBLE.to_owned());
        }
    }
}

/// The files of a package, relative to it (a glob does not enter nested Buck packages).
const PACKAGE_FILES_GLOB: &str = "**/**";

/// Emit `buildscript_build` rule for the given build target
pub(super) fn emit_buildscript_build(
    build_target: &BuckalTarget,
    node: &BuckalNode,
    manifest_dir: &Utf8PathBuf,
    ctx: &BuckalContext,
) -> RustBinary {
    let vendor = vendor_ref(node);
    let (features, platform) = node_features(node);
    // create the build script rule
    let mut buildscript_build = RustBinary {
        name: build_target.name.to_owned(),
        srcs: vendor.srcs.clone(),
        crate_name: build_target.name.to_owned().replace("-", "_"),
        edition: node.edition.to_string(),
        features,
        platform,
        rustc_flags: vec!["@$(location :manifest[env_flags])".to_owned()],
        ..Default::default()
    };

    // Set the crate root path for the build script
    buildscript_build.crate_root = vendor.crate_path(&normalize_path_for_buck(
        build_target
            .src_path
            .to_owned()
            .strip_prefix(manifest_dir)
            .expect("Failed to get build script source path")
            .as_str(),
    ));

    // Set dependencies for the build script
    set_deps(
        &mut buildscript_build,
        node,
        CargoTargetKind::CustomBuild,
        ctx,
    )
    .unwrap_or_exit_ctx(format!(
        "failed to set dependencies for '{}'",
        buildscript_build.name
    ));
    // A build script is a host compilation.
    platform_compatibility(
        node,
        ctx,
        true,
        &mut buildscript_build.target_compatible_with,
        &mut buildscript_build.platform,
    );

    buildscript_build
}

/// Emit `buildscript_run` rule for the given build target
pub(super) fn emit_buildscript_run(
    node: &BuckalNode,
    build_target: &BuckalTarget,
    ctx: &BuckalContext,
) -> BuildscriptRun {
    // create the build script run rule
    let build_name = get_build_name(&build_target.name);
    let vendor = vendor_ref(node);
    let (features, platform) = node_features(node);
    let mut buildscript_run = BuildscriptRun {
        name: format!("{}-run", build_name),
        package_name: node.name.to_string(),
        buildscript_rule: format!(":{}", build_target.name),
        env_srcs: Set::from([":manifest[env_dict]".to_owned()]),
        features,
        platform,
        version: node.version.to_string(),
        manifest_dir: vendor.label.clone(),
        visibility: Set::from(["PUBLIC".to_owned()]),
        ..Default::default()
    };

    let host = node.platform_features.host_context;
    platform_compatibility(
        node,
        ctx,
        host,
        &mut buildscript_run.target_compatible_with,
        &mut buildscript_run.platform,
    );
    // The per-platform build script aliases of an incompatible run are as well.
    if buildscript_run
        .target_compatible_with
        .contains(INCOMPATIBLE)
    {
        buildscript_run
            .buildscript_compatible_with
            .insert(INCOMPATIBLE.to_owned());
    }

    let host_target = get_target();
    let host_cfgs = get_cfgs();

    // Set environment variables from dependencies that have the `links` manifest key.
    // See https://doc.rust-lang.org/cargo/reference/build-scripts.html#the-links-manifest-key
    for (dep, dep_node) in ctx.resolve.deps_of(&node.package_id) {
        let dep_node = ctx
            .patched_node(dep_node)
            .unwrap_or_exit_ctx("failed to resolve patched dependency");
        if dep_node.links.is_some()
            && dep.dep_kinds.iter().any(|dk: &BuckalDepKind| {
                dep_kind_matches(CargoTargetKind::Lib, dk.kind)
                    && dk
                        .target
                        .as_ref()
                        .map(|platform| platform.matches(&host_target, &host_cfgs[..]))
                        .unwrap_or(true)
            })
        {
            // Only normal dependencies with the links manifest key for current arch are considered
            let custom_build_target_dep = dep_node
                .targets
                .iter()
                .find(|t| t.kind.contains(&TargetKind::CustomBuild));
            if let Some(build_target_dep) = custom_build_target_dep {
                let mut build_name_dep = get_build_name(&build_target_dep.name).into_owned();
                if node.platform_features.host_context && uses_host_variant(node, dep_node) {
                    build_name_dep = host_variant_name(&build_name_dep);
                }
                buildscript_run.env_srcs.insert(format!(
                    "//{}:{build_name_dep}-run[metadata]",
                    get_vendor_path_relative(&dep_node.package_id).unwrap_or_exit()
                ));
            } else {
                panic!(
                    "Dependency {} has links key but no build script target",
                    dep_node.name
                );
            }
        }
    }

    buildscript_run
}

/// Patch the given `rust_library` or `rust_binary` rule to support build scripts
pub(super) fn patch_with_buildscript(rust_rule: &mut dyn RustRule, build_target: &BuckalTarget) {
    let build_name = get_build_name(&build_target.name);
    rust_rule.env_mut().insert(
        "OUT_DIR".to_owned(),
        format!("$(location :{build_name}-run[out_dir])").to_owned(),
    );
    // The build script's flags go first, as they always have been emitted.
    let flag = format!("@$(location :{build_name}-run[rustc_flags])");
    let flags = rust_rule.rustc_flags_mut();
    if !flags.contains(&flag) {
        flags.insert(0, flag);
    }
}

/// Emit `http_archive` rule for the given package
pub(super) fn emit_http_archive(node: &BuckalNode) -> HttpArchive {
    let url = format!(
        "https://static.crates.io/crates/{}/{}-{}.crate",
        node.name, node.name, node.version
    );
    let buckal_name = format!("{}-{}", node.name, node.version);
    let checksum = node
        .checksum
        .as_ref()
        .expect("missing checksum for registry package");

    HttpArchive {
        name: get_vendor_name().to_string(),
        urls: Set::from([url]),
        sha256: checksum.to_string(),
        _type: "tar.gz".to_owned(),
        strip_prefix: buckal_name,
        out: None,
    }
}

/// Emit `filegroup` rule for the given package
pub(super) fn emit_filegroup() -> FileGroup {
    FileGroup {
        name: get_vendor_name().to_string(),
        srcs: Glob {
            include: Set::from(["**/**".to_owned()]),
            ..Default::default()
        },
        out: None,
    }
}

/// Emit `git_fetch` rule for the given package
pub(super) fn emit_git_fetch(node: &BuckalNode) -> GitFetch {
    let source_repr = node.source.as_ref().expect("failed to get package source");
    let source_id = TomlLockfileSourceId::new(source_repr.to_owned()).unwrap_or_exit();

    let mut git_repo = source_id.url().to_owned();
    git_repo.set_fragment(None);
    git_repo.set_query(None);

    let mut sub_targets = Set::new();
    if let Some(layout) = git_checkout_layout(manifest_dir_of(node)) {
        sub_targets.extend(layout.subdir);
        sub_targets.extend(layout.workspace_manifest);
    }

    GitFetch {
        name: get_vendor_name().to_string(),
        repo: git_repo.to_string(),
        rev: source_id.url().fragment().unwrap().to_string(),
        sub_targets,
    }
}

/// Emit `cargo_manifest` rule for the given package
pub(super) fn emit_cargo_manifest(node: &BuckalNode, ctx: &BuckalContext) -> CargoManifest {
    if !is_third_party(node) && ctx.workspace_inherit {
        let buck2_root = get_buck2_root().unwrap_or_exit_ctx("failed to get Buck2 root");
        let workspace_root = ctx.workspace_root.to_owned();
        let relative_path = workspace_root
            .strip_prefix(&buck2_root)
            .unwrap_or_exit_ctx("failed to get relative path from Buck2 root to workspace root");
        CargoManifest {
            name: "manifest".to_owned(),
            vendor: get_vendor_target(),
            workspace: format!(
                "//{}:workspace",
                normalize_path_for_buck(relative_path.as_str())
            )
            .into(),
        }
    } else {
        let vendor = vendor_ref(node);
        CargoManifest {
            name: "manifest".to_owned(),
            vendor: vendor.label,
            workspace: vendor.workspace_manifest,
        }
    }
}

/// Emit `export_file` rule for the workspace Cargo.toml.
///
/// Uses `mode = "reference"` so that downstream rules (e.g. `cargo_manifest`)
/// see the real on-disk path instead of a buck-out copy. This is required for
/// manifest_parse to resolve relative readme/license-file paths correctly.
pub(super) fn emit_export_file() -> ExportFile {
    ExportFile {
        name: "workspace".to_owned(),
        src: "Cargo.toml".to_string(),
        mode: Some("reference".to_owned()),
        labels: Default::default(),
        visibility: Set::from(["PUBLIC".to_owned()]),
    }
}

/// The build script target of a package's host variant: `build-script-build` becomes
/// `build-script-host-build`, so its rules are `build-script-host-build` and
/// `build-script-host-run`.
pub(super) fn host_variant_build_target(build_target: &BuckalTarget) -> BuckalTarget {
    BuckalTarget {
        name: format!(
            "{}-build",
            host_variant_name(&get_build_name(&build_target.name))
        ),
        ..build_target.clone()
    }
}

/// The build script target of a package's test variant: `build-script-build`
/// becomes `build-script-test-build`, so its rules are `build-script-test-build` and
/// `build-script-test-run`.
pub(super) fn test_variant_build_target(build_target: &BuckalTarget) -> BuckalTarget {
    BuckalTarget {
        name: format!(
            "{}-build",
            test_variant_name(&get_build_name(&build_target.name))
        ),
        ..build_target.clone()
    }
}

/// The build script target of a package's test host variant: `build-script-build`
/// becomes `build-script-test-host-build`, so its rules are
/// `build-script-test-host-build` and `build-script-test-host-run`.
pub(super) fn test_host_variant_build_target(build_target: &BuckalTarget) -> BuckalTarget {
    BuckalTarget {
        name: format!(
            "{}-build",
            test_host_variant_name(&get_build_name(&build_target.name))
        ),
        ..build_target.clone()
    }
}

fn get_build_name(s: &str) -> Cow<'_, str> {
    if let Some(stripped) = s.strip_suffix("-build") {
        Cow::Owned(stripped.to_string())
    } else {
        Cow::Borrowed(s)
    }
}

/// Get the name of the vendor target
fn get_vendor_name() -> Cow<'static, str> {
    Cow::Borrowed("vendor")
}

/// Get the label of the vendor target
fn get_vendor_target() -> String {
    format!(":{}", get_vendor_name())
}

/// The features to emit for `node`: the features shared by every platform, and the
/// per-platform remainder for the prelude's `platform` attribute. Without per-platform
/// data (no `feature_platforms` configured) this is the merged feature set and no
/// `platform` attribute, exactly as before.
pub(super) fn node_features(node: &BuckalNode) -> (Set<String>, PlatformAttrs) {
    if node.platform_features.is_empty() {
        (node.features.iter().cloned().collect(), Map::new())
    } else {
        split_features(&node.platform_features.features)
    }
}

/// Where a package's sources are found relative to its BUCK file.
#[derive(Debug, PartialEq)]
pub(super) struct VendorRef {
    /// Label of the package directory: `:vendor`, or `:vendor[<subdir>]` for a crate that lives
    /// in a subdirectory of a fetched git repository.
    pub label: String,
    /// The `srcs` of the package's Rust rules.
    pub srcs: Srcs,
    /// Path of the package directory inside the rule's sources, used as the `crate_root`
    /// prefix; empty when the sources are the package's own files at their own paths.
    pub root: String,
    /// Label of the enclosing workspace manifest a git crate inherits keys from, if any.
    pub workspace_manifest: Option<String>,
}

impl VendorRef {
    /// `path` (relative to the package directory) as a path inside the rule's sources.
    pub fn crate_path(&self, path: &str) -> String {
        if self.root.is_empty() {
            path.to_owned()
        } else {
            format!("{}/{}", self.root, path)
        }
    }
}

/// The vendored location of `node`'s package directory.
///
/// A first-party package compiles its own files (`srcs = glob(["**/**"])`,
/// `crate_root = "src/lib.rs"`). The prelude places them at their repository paths
/// (`<package>/<path>`) in the crate's sources directory, next to `mapped_srcs` at
/// their repository-path destinations, and remaps that directory away, so `file!()`
/// is the path relative to the repository checkout (`<package>/src/...`), as Cargo
/// gives it for a member of a workspace at the repository root. Includes that climb
/// out of the package (`../`) then resolve to the mapped files. Tools that locate
/// files from `file!()` at run time, such as insta snapshots, find them where they
/// are checked in. The
/// `vendor` filegroup remains the input of `cargo_manifest` and `buildscript_run`.
///
/// A registry archive is the `vendor` target itself. A git
/// dependency may live in a subdirectory of its repository (e.g. `crates/foo` of a
/// multi-crate workspace); its `vendor` git_fetch then exposes that subdirectory and the
/// enclosing workspace manifest as sub-targets, and every rule refers to those.
pub(super) fn vendor_ref(node: &BuckalNode) -> VendorRef {
    if matches!(node.kind, NodeKind::FirstParty { .. }) {
        return VendorRef {
            label: get_vendor_target(),
            srcs: Srcs::Glob(Glob {
                include: Set::from(["**/**".to_owned()]),
                ..Default::default()
            }),
            root: String::new(),
            workspace_manifest: None,
        };
    }
    let is_git = node
        .source
        .as_deref()
        .is_some_and(|source| source.starts_with("git+"));
    let layout = if is_git {
        git_checkout_layout(manifest_dir_of(node))
    } else {
        None
    };
    vendor_ref_for_layout(layout)
}

fn vendor_ref_for_layout(layout: Option<GitLayout>) -> VendorRef {
    let name = get_vendor_name();
    let layout = layout.unwrap_or_default();
    let workspace_manifest = layout
        .workspace_manifest
        .map(|path| format!(":{name}[{path}]"));
    match layout.subdir {
        Some(subdir) => VendorRef {
            label: format!(":{name}[{subdir}]"),
            srcs: Srcs::Labels(Set::from([format!(":{name}[{subdir}]")])),
            root: format!("{name}/{subdir}"),
            workspace_manifest,
        },
        None => VendorRef {
            label: get_vendor_target(),
            srcs: Srcs::Labels(Set::from([get_vendor_target()])),
            root: name.to_string(),
            workspace_manifest,
        },
    }
}

fn manifest_dir_of(node: &BuckalNode) -> &Utf8Path {
    node.manifest_path
        .parent()
        .expect("manifest path has a parent directory")
}

/// Position of a git dependency inside its Cargo checkout.
#[derive(Debug, Default, PartialEq)]
pub(super) struct GitLayout {
    /// Package directory relative to the repository root; `None` at the root.
    pub subdir: Option<String>,
    /// Workspace manifest (relative to the repository root) that encloses the package and
    /// that it may inherit `workspace = true` keys from; `None` if there is none or the
    /// package is its own workspace root.
    pub workspace_manifest: Option<String>,
}

/// Locate `manifest_dir` inside the Cargo git checkout that contains it.
///
/// Cargo marks the root of every git checkout with `.cargo-ok` (and keeps the `.git`
/// directory there). Returns `None` when no checkout root is found above `manifest_dir`.
pub(super) fn git_checkout_layout(manifest_dir: &Utf8Path) -> Option<GitLayout> {
    let checkout_root = manifest_dir
        .ancestors()
        .find(|dir| dir.join(".cargo-ok").exists() || dir.join(".git").exists())?;
    let relative = |dir: &Utf8Path| -> String {
        normalize_path_for_buck(
            dir.strip_prefix(checkout_root)
                .expect("ancestor of the manifest directory")
                .as_str(),
        )
    };

    let subdir = Some(relative(manifest_dir)).filter(|path| !path.is_empty());

    let workspace_manifest = if is_workspace_root(&manifest_dir.join("Cargo.toml")) {
        None
    } else {
        manifest_dir
            .ancestors()
            .skip(1)
            .take_while(|dir| dir.starts_with(checkout_root))
            .map(|dir| dir.join("Cargo.toml"))
            .find(|manifest| is_workspace_root(manifest))
            .map(|manifest| relative(&manifest))
    };

    Some(GitLayout {
        subdir,
        workspace_manifest,
    })
}

fn is_workspace_root(manifest: &Utf8Path) -> bool {
    std::fs::read_to_string(manifest)
        .ok()
        .and_then(|content| content.parse::<toml::Table>().ok())
        .is_some_and(|table| table.contains_key("workspace"))
}

/// Normalize a path for Buck by converting backslashes to forward slashes.
/// This normalization is critical on Windows, where paths use backslashes,
/// as Buck2 requires forward slashes in all generated BUCK files regardless of the host platform.
fn normalize_path_for_buck(path: &str) -> String {
    path.replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Utf8Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// A Cargo git checkout: `.cargo-ok` marker, workspace manifest at the root,
    /// member crate in `crates/foo`.
    fn checkout() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        write(&root.join(".cargo-ok"), "");
        write(
            &root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n[workspace.package]\nversion = \"1.2.3\"\n",
        );
        write(
            &root.join("crates/foo/Cargo.toml"),
            "[package]\nname = \"foo\"\nversion.workspace = true\n",
        );
        (dir, root)
    }

    #[test]
    fn test_git_layout_crate_in_subdirectory() {
        let (_dir, root) = checkout();
        let layout = git_checkout_layout(&root.join("crates/foo")).unwrap();
        assert_eq!(
            layout,
            GitLayout {
                subdir: Some("crates/foo".to_owned()),
                workspace_manifest: Some("Cargo.toml".to_owned()),
            }
        );
    }

    #[test]
    fn test_git_layout_crate_at_repository_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        write(&root.join(".cargo-ok"), "");
        write(&root.join("Cargo.toml"), "[package]\nname = \"bar\"\n");
        assert_eq!(git_checkout_layout(&root), Some(GitLayout::default()));
    }

    #[test]
    fn test_git_layout_crate_that_is_its_own_workspace() {
        let (_dir, root) = checkout();
        write(
            &root.join("crates/solo/Cargo.toml"),
            "[package]\nname = \"solo\"\n[workspace]\n",
        );
        let layout = git_checkout_layout(&root.join("crates/solo")).unwrap();
        assert_eq!(layout.subdir.as_deref(), Some("crates/solo"));
        assert_eq!(layout.workspace_manifest, None);
    }

    #[test]
    fn test_git_layout_without_checkout_marker() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        write(
            &root.join("crates/foo/Cargo.toml"),
            "[package]\nname = \"foo\"\n",
        );
        assert_eq!(git_checkout_layout(&root.join("crates/foo")), None);
    }

    #[test]
    fn test_vendor_ref_for_subdirectory_crate() {
        let vendor = vendor_ref_for_layout(Some(GitLayout {
            subdir: Some("crates/foo".to_owned()),
            workspace_manifest: Some("Cargo.toml".to_owned()),
        }));
        assert_eq!(
            vendor,
            VendorRef {
                label: ":vendor[crates/foo]".to_owned(),
                srcs: Srcs::Labels(Set::from([":vendor[crates/foo]".to_owned()])),
                root: "vendor/crates/foo".to_owned(),
                workspace_manifest: Some(":vendor[Cargo.toml]".to_owned()),
            }
        );
    }

    #[test]
    fn test_vendor_ref_default() {
        assert_eq!(
            vendor_ref_for_layout(None),
            VendorRef {
                label: ":vendor".to_owned(),
                srcs: Srcs::Labels(Set::from([":vendor".to_owned()])),
                root: "vendor".to_owned(),
                workspace_manifest: None,
            }
        );
    }
}
