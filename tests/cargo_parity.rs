//! Production features as BUCKAL resolves them, compared with Cargo's own answer.
//!
//! BUCKAL reads `cargo build --unit-graph`. The independent reference here is `cargo
//! tree -e normal,build --target <triple>`, which resolves the same build through
//! Cargo's tree code: every package it shows below a build-dependency edge or a
//! proc-macro is a host-context compilation, every other one below a workspace
//! member a target-context one. `cargo metadata --filter-platform` is compared too:
//! it reports one feature set per package, merged across platforms, contexts and
//! dev-dependencies, which is what the emitted rules used to carry.
//!
//! `test_production_features_match_cargo` runs on a fixture. Set
//! `BUCKAL_PARITY_MANIFEST=<Cargo.toml>` and run the ignored
//! `test_production_features_match_cargo_on_workspace` for another workspace; it
//! only plans (no build).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::process::Command;

use cargo_buckal::platform_features::{PlatformFeatures, resolve_platform_features};
use cargo_metadata::{MetadataCommand, Package, PackageId};

/// A package's features in one context, if it is built there.
type Features = Option<BTreeSet<String>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Context {
    Target,
    Host,
}

/// One package line of `cargo tree` output.
struct TreeLine {
    depth: usize,
    /// The line is below a `[build-dependencies]` header of its parent.
    build_edge: bool,
    /// The line is below a `[dev-dependencies]` header of its parent: a test's
    /// dependency, compiled for the target.
    dev_edge: bool,
    proc_macro: bool,
    id: Option<PackageId>,
    features: BTreeSet<String>,
}

/// The package lines `cargo tree -e normal,build --target <triple>` shows, in order:
/// the production build. With `tests` (`Some(features)`), `cargo tree -e
/// normal,build,dev --features <features>`: the build `cargo test --features
/// <features>` makes, dev-dependencies (target context) included.
fn cargo_tree(
    manifest: &Path,
    triple: &str,
    tests: Option<&[String]>,
    by_name_version: &HashMap<(String, String), PackageId>,
) -> Vec<TreeLine> {
    let mut command = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned()));
    command.args(["tree", "--workspace", "--target", triple]);
    match tests {
        None => {
            command.args(["-e", "normal,build"]);
        }
        Some(features) => {
            command.args(["-e", "normal,build,dev"]);
            if !features.is_empty() {
                command.args(["--features", &features.join(",")]);
            }
        }
    }
    let output = command
        .args(["-f", "{p}|{f}", "--manifest-path"])
        .arg(manifest)
        .output()
        .expect("run cargo tree");
    assert!(
        output.status.success(),
        "cargo tree: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let mut lines = Vec::new();
    // Per depth: the current section header of the children there.
    let mut build_section: Vec<Option<char>> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            build_section.clear();
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        let mut units = 0;
        while chars.len() >= 4 * (units + 1)
            && chars[4 * units..4 * units + 4]
                .iter()
                .all(|c| matches!(c, '│' | ' ' | '├' | '└' | '─'))
        {
            units += 1;
        }
        let rest: String = chars[4 * units..].iter().collect();
        if rest.starts_with('[') {
            // A section header for the children at depth `units + 1`.
            build_section.resize(units + 2, None);
            build_section[units + 1] = if rest.starts_with("[build-dependencies]") {
                Some('b')
            } else if rest.starts_with("[dev-dependencies]") {
                Some('d')
            } else {
                None
            };
            continue;
        }
        let depth = units;
        build_section.resize(depth + 1, None);
        let build_edge = build_section[depth] == Some('b');
        let dev_edge = build_section[depth] == Some('d');
        // A new node: the sections of its own children start over.
        build_section.truncate(depth + 1);
        let (package, features) = rest.split_once('|').expect("{p}|{f}");
        let mut words = package.split_whitespace();
        let name = words.next().unwrap().to_owned();
        let version = words.next().unwrap().trim_start_matches('v').to_owned();
        lines.push(TreeLine {
            depth,
            build_edge,
            dev_edge,
            proc_macro: package.contains("(proc-macro)"),
            id: by_name_version.get(&(name, version)).cloned(),
            features: features
                .trim_end_matches(" (*)")
                .split(',')
                .filter(|f| !f.is_empty())
                .map(str::to_owned)
                .collect(),
        });
    }
    lines
}

#[derive(Debug, Default)]
struct Report {
    /// Package lines compared (with repeats).
    compared: usize,
    mismatches: Vec<String>,
    /// `(platform, package)` whose target-context features `cargo metadata
    /// --filter-platform` reports otherwise.
    metadata_differs: BTreeSet<(String, String)>,
    /// Lines whose package or context could not be told.
    unattributed: usize,
    /// Lines of packages `cargo build` builds nothing of (`required-features`).
    not_built: usize,
    /// With tests: `(platform, package)` with a dev-dependency on itself in `cargo
    /// tree`, each also compared with BUCKAL's dev edge of the package to itself.
    self_dev_edges: usize,
    /// Host-context lines whose rule was checked for compatibility with every
    /// configured platform.
    host_rules_checked: usize,
}

/// Compare BUCKAL's resolution with `cargo tree`'s: production, or with `tests`
/// (`Some(test_features)`) the test contexts (test variants and test host variants,
/// else the production rules the tests share).
fn compare(
    manifest: &Path,
    platforms: &BTreeMap<String, String>,
    tests: Option<&[String]>,
) -> (Report, PlatformFeatures) {
    let metadata = MetadataCommand::new()
        .manifest_path(manifest)
        .exec()
        .unwrap();
    let packages: HashMap<PackageId, Package> = metadata
        .packages
        .iter()
        .map(|p| (p.id.clone(), p.clone()))
        .collect();
    let mut by_name_version: HashMap<(String, String), PackageId> = HashMap::new();
    let mut ambiguous = BTreeSet::new();
    for p in &metadata.packages {
        let key = (p.name.to_string(), p.version.to_string());
        if by_name_version.insert(key.clone(), p.id.clone()).is_some() {
            ambiguous.insert(key);
        }
    }
    for key in ambiguous {
        by_name_version.remove(&key);
    }
    let resolved = resolve_platform_features(
        Some(manifest.to_str().unwrap()),
        platforms,
        &packages,
        tests.unwrap_or(&[]),
    )
    .expect("resolve");
    let mut report = Report::default();
    for (platform, triple) in platforms {
        let union: HashMap<String, BTreeSet<String>> = MetadataCommand::new()
            .manifest_path(manifest)
            .other_options(vec!["--filter-platform".to_owned(), triple.clone()])
            .exec()
            .unwrap()
            .resolve
            .unwrap()
            .nodes
            .into_iter()
            .map(|n| {
                (
                    n.id.repr,
                    n.features.into_iter().map(|f| f.to_string()).collect(),
                )
            })
            .collect();
        // BUCKAL's feature sets of a package for this platform, per context.
        let expected = |id: &PackageId| -> Option<(Features, Features)> {
            let resolution = resolved.get(id)?;
            let main = resolution.features.get(platform).cloned();
            let mut target = (!resolution.host_context).then(|| main.clone()).flatten();
            let mut host = if resolution.host_context {
                main
            } else {
                resolution
                    .host_variant
                    .as_ref()
                    .and_then(|h| h.features.get(platform).cloned())
                    .or(main)
            };
            if tests.is_some() {
                if let Some(test) = &resolution.test_variant {
                    target = test.features.get(platform).cloned();
                } else if let Some(t) = resolution.tests.as_ref().filter(|t| !t.host_context) {
                    // No library the tests build (a binary-only or cdylib-only
                    // member): its tests' own compilations have the tests'
                    // resolution.
                    target = t.features.get(platform).cloned();
                }
                if let Some(test_host) = &resolution.test_host_variant {
                    host = test_host.features.get(platform).cloned();
                }
            }
            Some((target, host))
        };
        // The context of each depth's current node; `None` below a root whose
        // context cannot be told.
        let mut contexts: Vec<Option<Context>> = Vec::new();
        for line in cargo_tree(manifest, triple, tests, &by_name_version) {
            contexts.truncate(line.depth);
            let Some(id) = line.id.clone() else {
                report.unattributed += 1;
                contexts.push(None);
                continue;
            };
            let Some((target, host)) = expected(&id) else {
                // `cargo tree` lists a package whose every target needs features
                // that are off; `cargo build` builds nothing of it.
                if packages[&id]
                    .targets
                    .iter()
                    .all(|t| !t.required_features.is_empty())
                {
                    report.not_built += 1;
                } else {
                    report
                        .mismatches
                        .push(format!("{platform}: {} not resolved", id.repr));
                }
                contexts.push(None);
                continue;
            };
            let context = if line.depth == 0 {
                // A workspace member as a root, in a context `cargo tree` does not
                // show: the one whose features it has, if any. Its dependencies are
                // then compared in that context. A root with the features of both
                // contexts of a package built differently in each (a host variant)
                // cannot be placed, and neither can its dependencies.
                let in_target = target == Some(line.features.clone());
                let in_host = host == Some(line.features.clone());
                let has_variant = resolved.get(&id).is_some_and(|r| {
                    r.host_variant.is_some() || tests.is_some() && r.test_host_variant.is_some()
                });
                if line.proc_macro || in_host && !in_target {
                    Some(Context::Host)
                } else if in_target && in_host && has_variant {
                    None
                } else if in_target {
                    Some(Context::Target)
                } else {
                    report.mismatches.push(format!(
                        "{platform}: root {} {:?}: BUCKAL target {target:?} host {host:?}",
                        id.repr, line.features
                    ));
                    None
                }
            } else {
                match contexts[line.depth - 1] {
                    None => None,
                    // A test's dependency of a host compilation (a proc-macro or a
                    // host-only member): its unit tests are compiled for the host and
                    // its integration tests for the target, so the context of the
                    // dependency cannot be told.
                    Some(Context::Host) if line.dev_edge => None,
                    // A test's dependency of a target compilation (proc-macros aside).
                    Some(_) if line.dev_edge && !line.proc_macro => Some(Context::Target),
                    Some(Context::Host) => Some(Context::Host),
                    Some(Context::Target) if line.build_edge || line.proc_macro => {
                        Some(Context::Host)
                    }
                    Some(Context::Target) => Some(Context::Target),
                }
            };
            // A proc-macro's own dependencies are host compilations.
            contexts.push(if line.proc_macro {
                context.map(|_| Context::Host)
            } else {
                context
            });
            let Some(context) = context else {
                report.unattributed += 1;
                continue;
            };
            report.compared += 1;
            // A host compilation is configured for the execution platform: the rule
            // it uses must not be marked incompatible on any configured platform.
            // That is the package's own host-context rule, its host (or, for the
            // tests, test host) variant, or else its plain rule, which generation
            // marks on each platform its target resolution lacks unless host
            // compilations use it (`host_uses_plain_rules`).
            if context == Context::Host {
                let r = &resolved[&id];
                let variant =
                    r.host_variant.is_some() || tests.is_some() && r.test_host_variant.is_some();
                let marked = platforms.keys().any(|p| !r.features.contains_key(p));
                if !r.host_context && !variant && marked && !r.host_uses_plain_rules {
                    report.mismatches.push(format!(
                        "{platform}: {}: a host compilation uses a rule marked incompatible on a configured platform",
                        id.repr
                    ));
                }
                report.host_rules_checked += 1;
            }
            let ok = match context {
                Context::Target => target.as_ref() == Some(&line.features),
                Context::Host => host.as_ref() == Some(&line.features),
            };
            if !ok {
                report.mismatches.push(format!(
                    "{platform}: {} ({context:?}): cargo tree {:?}, BUCKAL target {target:?} host {host:?}",
                    id.repr, line.features
                ));
            }
            if tests.is_none()
                && context == Context::Target
                && union.get(&id.repr) != Some(&line.features)
            {
                report
                    .metadata_differs
                    .insert((platform.clone(), id.repr.clone()));
            }
        }
        if let Some(features) = tests {
            // The members whose library Cargo links into their own unit tests (a
            // dev-dependency on themselves), from its unit graph, and BUCKAL's dev
            // edges of the tests' resolution to the package itself. `cargo tree`
            // cannot show them: a member printed before as a dependency is shown
            // again as a root without its children.
            let cargo_self = self_linked_unit_tests(manifest, triple, features);
            let buckal_self: BTreeSet<String> = resolved
                .iter()
                .filter(|(id, r)| {
                    r.tests
                        .as_deref()
                        .unwrap_or(r)
                        .deps
                        .dev
                        .get(platform)
                        .is_some_and(|dev| dev.contains(&id.repr))
                })
                .map(|(id, _)| id.repr.clone())
                .collect();
            report.self_dev_edges += cargo_self.len();
            for id in cargo_self.symmetric_difference(&buckal_self) {
                report.mismatches.push(format!(
                    "{platform}: {id}: library linked into its own unit tests: cargo {}, BUCKAL {}",
                    cargo_self.contains(id),
                    buckal_self.contains(id)
                ));
            }
        }
    }
    (report, resolved)
}

/// The packages whose library unit-test unit depends on their own library in
/// `cargo test --workspace --no-run --target <triple> --features <features>`'s unit
/// graph.
fn self_linked_unit_tests(manifest: &Path, triple: &str, features: &[String]) -> BTreeSet<String> {
    let mut command = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned()));
    command.env("RUSTC_BOOTSTRAP", "1").args([
        "test",
        "--workspace",
        "--no-run",
        "--target",
        triple,
    ]);
    if !features.is_empty() {
        command.args(["--features", &features.join(",")]);
    }
    let output = command
        .args(["-Z", "unstable-options", "--unit-graph", "--manifest-path"])
        .arg(manifest)
        .output()
        .expect("run cargo test --unit-graph");
    assert!(
        output.status.success(),
        "cargo test --unit-graph: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let graph: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let units = graph["units"].as_array().unwrap();
    let lib_kind = |u: &serde_json::Value| {
        u["target"]["kind"].as_array().unwrap().iter().any(|k| {
            matches!(
                k.as_str().unwrap(),
                "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro"
            )
        })
    };
    units
        .iter()
        .filter(|u| u["mode"] == "test" && lib_kind(u))
        .filter(|u| {
            u["dependencies"].as_array().unwrap().iter().any(|d| {
                let v = &units[d["index"].as_u64().unwrap() as usize];
                v["pkg_id"] == u["pkg_id"] && v["mode"] == "build" && lib_kind(v)
            })
        })
        .map(|u| u["pkg_id"].as_str().unwrap().to_owned())
        .collect()
}

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// Feature unification that differs per platform and per context: `common` gets `a`
/// from `app` everywhere, `b` and the optional `helper` from `web` on wasm32 only,
/// `c` from `app`'s build script (host only), `d` from the proc-macro `pm`, and `h`
/// on `helper` from a dev-dependency (tests only). Under `cargo test --features
/// test_lib`: the proc-macro `pm` (a member) gets `test_lib`, but the build
/// dependency `gen` does not (command-line features reach members' target
/// compilations and host-only members); the dev-dependency proc-macro `dpm`, which
/// only the tests build (not a member, like a third-party crate), gives the host
/// `hostlib` that `pm` also uses feature `x`.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\", \"web\", \"common\", \"pm\", \"helper\", \"gen\", \"hostlib\"]\nexclude = [\"dpm\"]\n",
    );
    let package = |name: &str, body: &str| {
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{body}")
    };
    write(
        &root.join("common/Cargo.toml"),
        &package(
            "common",
            "[features]\na = []\nb = []\nc = []\nd = []\n[dependencies]\nhelper = { path = \"../helper\", optional = true }\n",
        ),
    );
    write(
        &root.join("helper/Cargo.toml"),
        &package("helper", "[features]\nh = []\n"),
    );
    write(
        &root.join("pm/Cargo.toml"),
        &package(
            "pm",
            "[lib]\nproc-macro = true\n[features]\ntest_lib = []\n[dependencies]\ncommon = { path = \"../common\", features = [\"d\"] }\nhostlib = { path = \"../hostlib\" }\n",
        ),
    );
    write(
        &root.join("app/Cargo.toml"),
        &package(
            "app",
            "[dependencies]\ncommon = { path = \"../common\", features = [\"a\"] }\npm = { path = \"../pm\" }\n[build-dependencies]\ncommon = { path = \"../common\", features = [\"c\"] }\ngen = { path = \"../gen\" }\n[dev-dependencies]\nhelper = { path = \"../helper\", features = [\"h\"] }\ndpm = { path = \"../dpm\" }\n",
        ),
    );
    write(&root.join("app/build.rs"), "fn main() {}\n");
    write(
        &root.join("gen/Cargo.toml"),
        &package("gen", "[features]\ntest_lib = []\n"),
    );
    write(
        &root.join("hostlib/Cargo.toml"),
        &package("hostlib", "[features]\nx = []\n"),
    );
    write(
        &root.join("dpm/Cargo.toml"),
        &package(
            "dpm",
            "[lib]\nproc-macro = true\n[dependencies]\nhostlib = { path = \"../hostlib\", features = [\"x\"] }\n",
        ),
    );
    write(
        &root.join("web/Cargo.toml"),
        &package(
            "web",
            "[target.'cfg(target_arch = \"wasm32\")'.dependencies]\ncommon = { path = \"../common\", features = [\"b\", \"helper\"] }\n",
        ),
    );
    for member in [
        "app", "web", "common", "pm", "helper", "gen", "hostlib", "dpm",
    ] {
        write(&root.join(member).join("src/lib.rs"), "");
    }
    dir
}

fn platforms() -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "linux-x86_64".to_owned(),
            "x86_64-unknown-linux-gnu".to_owned(),
        ),
        ("wasm32".to_owned(), "wasm32-unknown-unknown".to_owned()),
    ])
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn test_production_features_match_cargo() {
    let dir = fixture();
    let manifest = dir.path().join("Cargo.toml");
    let (report, resolved) = compare(&manifest, &platforms(), None);
    assert!(report.mismatches.is_empty(), "{report:#?}");
    assert!(report.compared >= 10, "{report:#?}");
    // `cargo metadata` merges what differs; the old rules carried that merge.
    assert!(!report.metadata_differs.is_empty(), "{report:#?}");
    assert_eq!(report.unattributed, 0, "{report:#?}");

    let common = resolved
        .iter()
        .find(|(id, _)| id.repr.contains("/common#"))
        .map(|(_, r)| r)
        .unwrap();
    // Target context: per platform, `cargo build`'s own unification.
    assert_eq!(common.features["linux-x86_64"], set(&["a", "d"]));
    assert_eq!(common.features["wasm32"], set(&["a", "b", "d", "helper"]));
    // Host context: the build script's and the proc-macro's features only.
    let host = common.host_variant.as_ref().expect("host variant");
    assert_eq!(host.features["linux-x86_64"], set(&["c", "d"]));
    assert_eq!(host.features["wasm32"], set(&["c", "d"]));
    // The dev-dependency's feature stays out of production, and is the tests'.
    let helper = resolved
        .iter()
        .find(|(id, _)| id.repr.contains("/helper#"))
        .map(|(_, r)| r)
        .unwrap();
    assert_eq!(helper.features["linux-x86_64"], set(&[]));
    assert_eq!(
        helper.test_variant.as_ref().unwrap().features["linux-x86_64"],
        set(&["h"])
    );
}

fn find<'a>(
    resolved: &'a PlatformFeatures,
    name: &str,
) -> &'a cargo_buckal::platform_features::PlatformResolution {
    resolved
        .iter()
        .find(|(id, _)| id.repr.contains(&format!("/{name}#")))
        .map(|(_, r)| r)
        .unwrap()
}

#[test]
fn test_test_contexts_match_cargo() {
    let dir = fixture();
    let manifest = dir.path().join("Cargo.toml");
    let features = ["test_lib".to_owned()];
    let (report, resolved) = compare(&manifest, &platforms(), Some(&features));
    assert!(report.mismatches.is_empty(), "{report:#?}");
    assert!(report.compared >= 10, "{report:#?}");
    assert_eq!(report.unattributed, 0, "{report:#?}");

    // Under the tests, the host `hostlib` has `x` (from `dpm`, a dev-dependency) and
    // the proc-macro `pm` has `test_lib` and uses that `hostlib`; production has
    // neither.
    let hostlib = find(&resolved, "hostlib");
    assert_eq!(hostlib.features["linux-x86_64"], set(&[]));
    assert_eq!(
        hostlib.test_host_variant.as_ref().unwrap().features["linux-x86_64"],
        set(&["x"])
    );
    let pm = find(&resolved, "pm");
    assert_eq!(pm.features["linux-x86_64"], set(&[]));
    assert_eq!(
        pm.test_host_variant.as_ref().unwrap().features["linux-x86_64"],
        set(&["test_lib"])
    );
    // `gen` is a build dependency, which `--features` does not reach on the host.
    assert!(find(&resolved, "gen").test_host_variant.is_none());
    // `dpm` is built for the tests only: emitted from the tests' resolution.
    let dpm = find(&resolved, "dpm");
    assert!(dpm.test_context && dpm.host_context);
    // `app` uses `pm`, so its tests compile it apart (a test variant) against
    // `pm`'s test host variant.
    let app = find(&resolved, "app");
    assert!(app.test_variant.is_some());
    let pm_id = resolved
        .keys()
        .find(|id| id.repr.contains("/pm#"))
        .unwrap()
        .repr
        .clone();
    assert!(app.test_host_variant_deps.contains(&pm_id));
    // Production is the same as without test features.
    let (production, _) = compare(&manifest, &platforms(), None);
    assert!(production.mismatches.is_empty(), "{production:#?}");
}

#[test]
#[ignore = "set BUCKAL_PARITY_MANIFEST (and BUCKAL_PARITY_TEST_FEATURES) for a workspace"]
fn test_test_contexts_match_cargo_on_workspace() {
    let manifest = std::env::var("BUCKAL_PARITY_MANIFEST").expect("BUCKAL_PARITY_MANIFEST");
    let features: Vec<String> = std::env::var("BUCKAL_PARITY_TEST_FEATURES")
        .unwrap_or_default()
        .split(',')
        .filter(|f| !f.is_empty())
        .map(str::to_owned)
        .collect();
    let (report, resolved) = compare(Path::new(&manifest), &platforms(), Some(&features));
    let test_variants = resolved
        .values()
        .filter(|r| r.test_variant.is_some())
        .count();
    let test_host_variants = resolved
        .values()
        .filter(|r| r.test_host_variant.is_some())
        .count();
    eprintln!(
        "tests --features {features:?}: compared {} cargo tree package lines ({} host, checked for compatibility) and {} self dev-dependency edges; {} mismatches; {} lines unattributed; {} lines of packages never built; {test_variants} test variants, {test_host_variants} test host variants",
        report.compared,
        report.host_rules_checked,
        report.self_dev_edges,
        report.mismatches.len(),
        report.unattributed,
        report.not_built,
    );
    for mismatch in &report.mismatches {
        eprintln!("MISMATCH {mismatch}");
    }
    for (id, r) in &resolved {
        if r.test_host_variant.is_some() {
            eprintln!("TEST-HOST-VARIANT {}", id.repr);
        }
        // Built only for the tests, and for the host there.
        if r.test_context && (r.host_context || r.host_variant.is_some()) {
            eprintln!("TEST-ONLY-HOST {}", id.repr);
        }
    }
    assert!(report.mismatches.is_empty());
}

#[test]
#[ignore = "set BUCKAL_PARITY_MANIFEST to a workspace's Cargo.toml"]
fn test_production_features_match_cargo_on_workspace() {
    let manifest = std::env::var("BUCKAL_PARITY_MANIFEST").expect("BUCKAL_PARITY_MANIFEST");
    let (report, _) = compare(Path::new(&manifest), &platforms(), None);
    eprintln!(
        "compared {} cargo tree package lines ({} host, checked for compatibility); {} mismatches; {} lines unattributed; {} lines of packages cargo build builds nothing of; cargo metadata --filter-platform differs from cargo's target-context features for {} (platform, package) pairs",
        report.compared,
        report.host_rules_checked,
        report.mismatches.len(),
        report.unattributed,
        report.not_built,
        report.metadata_differs.len()
    );
    for mismatch in &report.mismatches {
        eprintln!("MISMATCH {mismatch}");
    }
    assert!(report.mismatches.is_empty());
}
