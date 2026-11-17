use cargo_metadata::camino::Utf8Path;
use regex::Regex;

use crate::{
    buck::{
        FixupFlags, FixupSets, NamedFileGroup, PARTLY_GENERATED_HEADER, Rule, TopLevelKind,
        is_generated_header_line, parse_buck_file, patch_buck_rules, top_level_statements,
    },
    buckal_error, buckal_log,
    buckify::emit::emit_export_file,
    cache::{BuckalChange, ChangeType},
    context::BuckalContext,
    resolve::{BuckalNode, NodeKind},
    utils::{UnwrapOrExit, get_buck2_root, get_vendor_dir},
};

use super::{
    buckify_dep_node, buckify_root_node, cross, gen_buck_content, gen_buck_content_preserving,
    rules::{MANIFEST_EXPORT, build_script_names, fixup_set_contributions},
    vendor_package, windows,
};

impl BuckalChange {
    /// Apply the changes to the BUCK files based on the detected package changes in the cache diff.
    pub fn apply(&self, ctx: &BuckalContext) {
        let re: Regex = Regex::new(r"^([^+#]+)\+([^#]+)#([^@]+)@([^+#]+)(?:\+(.+))?$")
            .expect("error creating regex");
        let skip_pattern = format!("path+file://{}", ctx.workspace_root);

        let mut workspace_emitted = false;

        for (id, change_type) in &self.changes {
            match change_type {
                ChangeType::Added | ChangeType::Changed => {
                    if let Some(node) = ctx.resolve.nodes().find(|n| &n.package_id == id) {
                        buckal_log!(
                            if let ChangeType::Added = change_type {
                                "Adding"
                            } else {
                                "Flushing"
                            },
                            format!("{} v{}", node.name, node.version)
                        );

                        let is_third_party_pkg = is_third_party(node);

                        // Vendor package sources
                        let vendor_dir = if !is_third_party_pkg {
                            node.manifest_path.parent().unwrap().to_owned()
                        } else {
                            vendor_package(node)
                        };

                        // Generate BUCK rules
                        let mut buck_rules = if !is_third_party_pkg {
                            buckify_root_node(node, ctx)
                        } else {
                            buckify_dep_node(node, ctx)
                        };

                        // Export workspace manifest
                        let workspace_manifest_path = ctx.workspace_root.join("Cargo.toml");
                        if ctx.workspace_inherit
                            && !workspace_emitted
                            && node.manifest_path == workspace_manifest_path
                        {
                            buck_rules.push(Rule::ExportFile(emit_export_file()));
                            workspace_emitted = true;
                        }

                        // Patch BUCK Rules
                        let buck_path = vendor_dir.join("BUCK");
                        let fixup_flags = FixupFlags {
                            current: ctx
                                .repo_config
                                .fixups
                                .get(&node.name)
                                .map(|fixup| fixup.rustc_flags.as_slice())
                                .unwrap_or_default(),
                            previous: self.previous_fixup_rustc_flags.as_ref().map(|flags| {
                                flags.get(&node.name).map(Vec::as_slice).unwrap_or_default()
                            }),
                        };
                        // What the previous generation's fixup added to each rule's
                        // sets (known when the cache records its fixups).
                        let previous_sets = self.previous_fixups.as_ref().map(|fixups| {
                            let build_scripts = build_script_names(&buck_rules);
                            let fixup = fixups.get(&node.name).cloned().unwrap_or_default();
                            buck_rules
                                .iter()
                                .filter_map(|rule| {
                                    let name = rule.name()?.to_owned();
                                    Some((
                                        name,
                                        fixup_set_contributions(&fixup, rule, &build_scripts),
                                    ))
                                })
                                .collect::<std::collections::BTreeMap<_, _>>()
                        });
                        merge_rules(
                            &buck_path,
                            &mut buck_rules,
                            ctx,
                            fixup_flags,
                            previous_sets.as_ref(),
                        );

                        // Generate the BUCK file, keeping what a first-party package's
                        // BUCK file has beyond the rules generation owns.
                        let mut buck_content = if is_third_party_pkg {
                            gen_buck_content(&buck_rules)
                        } else {
                            let owned = OwnedNames::of(
                                node,
                                &buck_rules,
                                ctx,
                                self.previous_fixups.as_ref(),
                            );
                            let hand = hand_written(&buck_path, &owned);
                            // The workspace-root rules spliced into this file later.
                            let reserved = if vendor_dir == ctx.workspace_root {
                                root_owned_names(ctx)
                            } else {
                                Vec::new()
                            };
                            if let Err(error) = check_namespace(&buck_rules, &hand.calls, &reserved)
                            {
                                buckal_error!(format!(
                                    "{buck_path}: {error}; it is left unchanged"
                                ));
                                std::process::exit(1);
                            }
                            gen_buck_content_preserving(&buck_rules, &hand.loads, &hand.statements)
                        };
                        if !is_third_party_pkg {
                            buck_content =
                                windows::patch_root_windows_rustc_flags(buck_content, ctx, node);
                        }
                        buck_content = cross::patch_rust_test_target_compatible_with(buck_content);
                        std::fs::write(&buck_path, buck_content)
                            .expect("Failed to write BUCK file");
                    }
                }
                ChangeType::Removed => {
                    // Skip workspace_root package
                    if id.repr.starts_with(skip_pattern.as_str()) {
                        continue;
                    }

                    let caps = re.captures(&id.repr).expect("Failed to parse package ID");
                    let name = &caps[3];
                    let version = &caps[4];

                    buckal_log!("Removing", format!("{} v{}", name, version));
                    let vendor_dir =
                        get_vendor_dir(id).unwrap_or_exit_ctx("failed to get vendor directory");
                    if vendor_dir.exists() {
                        std::fs::remove_dir_all(&vendor_dir)
                            .expect("Failed to remove vendor directory");
                    }
                    if let Some(package_dir) = vendor_dir.parent()
                        && package_dir.exists()
                        && package_dir.read_dir().unwrap().next().is_none()
                    {
                        std::fs::remove_dir_all(package_dir)
                            .expect("Failed to remove empty package directory");
                    }
                }
            }
        }

        // The rules generation owns in the workspace-root BUCK file outside a root
        // package's own rules: the workspace manifest's export (for a virtual
        // workspace, or a root package not regenerated now) and the
        // `workspace_manifests` filegroup. They are replaced in place or appended;
        // everything else in the file is kept byte for byte.
        let mut owned: Vec<(&str, &str, String)> = Vec::new();
        if !workspace_emitted && ctx.workspace_inherit {
            let export =
                serde_starlark::to_string(&emit_export_file()).expect("serialize export_file");
            owned.push(("export_file", "workspace", export));
        }
        if ctx.repo_config.workspace_manifests {
            let group =
                serde_starlark::to_string(&workspace_manifests(ctx)).expect("serialize filegroup");
            owned.push(("filegroup", WORKSPACE_MANIFESTS, group));
        }
        let removed: &[(&str, &str)] = if ctx.repo_config.workspace_manifests {
            &[]
        } else {
            &[("filegroup", WORKSPACE_MANIFESTS)]
        };
        let buck_path = ctx.workspace_root.join("BUCK");
        if buck_path.exists() {
            let existing = std::fs::read_to_string(&buck_path)
                .unwrap_or_exit_ctx(format!("Failed to read {}", buck_path));
            let (statements, _) = top_level_statements(buck_path.as_str(), &existing)
                .unwrap_or_exit_ctx(format!("Failed to parse {}", buck_path));
            for (function, name, _) in &owned {
                if let Some(other) = statements
                    .iter()
                    .find_map(|statement| match &statement.kind {
                        TopLevelKind::Call {
                            function: f,
                            name: Some(n),
                        } if n == name && f != function => Some(f),
                        _ => None,
                    })
                {
                    buckal_error!(format!(
                        "{buck_path}: generation owns the target `{name}` (`{function}`) here, but `{other}(name = \"{name}\")` declares it too; rename it. The file is left unchanged"
                    ));
                    std::process::exit(1);
                }
            }
            let rules: Vec<_> = owned
                .iter()
                .map(|(function, name, text)| (*function, *name, text.trim_end()))
                .collect();
            let buck_content = splice_rules(
                buck_path.as_str(),
                &partly_generated_header(&existing),
                &rules,
                removed,
            )
            .unwrap_or_exit_ctx(format!("Failed to parse {}", buck_path));
            if buck_content != existing {
                std::fs::write(&buck_path, buck_content).expect("Failed to write BUCK file");
            }
        } else if !owned.is_empty() {
            let texts: Vec<&str> = owned.iter().map(|(_, _, text)| text.as_str()).collect();
            let buck_content = format!("{PARTLY_GENERATED_HEADER}\n\n{}", texts.join("\n"));
            std::fs::write(&buck_path, buck_content).expect("Failed to write BUCK file");
        }
    }
}

/// `content` of the workspace-root BUCK file, whose header claims only
/// generation's own rules: an earlier `@generated` header (which claims the whole
/// file) becomes [`PARTLY_GENERATED_HEADER`]. Nothing else changes.
fn partly_generated_header(content: &str) -> String {
    match content.split_once('\n') {
        Some((first, rest)) if is_generated_header_line(first) => {
            format!("{PARTLY_GENERATED_HEADER}\n{rest}")
        }
        None if is_generated_header_line(content) => PARTLY_GENERATED_HEADER.to_owned(),
        _ => content.to_owned(),
    }
}

/// The name of the workspace-root filegroup of the workspace's manifests.
const WORKSPACE_MANIFESTS: &str = "workspace_manifests";

/// `filegroup(name = "workspace_manifests")`: the workspace's `Cargo.toml` and
/// `Cargo.lock` and every first-party package's `Cargo.toml`, at their project
/// paths. A package's manifest comes from its `Cargo.toml` export, the root
/// package's files directly.
fn workspace_manifests(ctx: &BuckalContext) -> NamedFileGroup {
    let root = get_buck2_root().unwrap_or_exit_ctx("failed to get Buck2 root");
    let project_path = |path: &Utf8Path| -> String {
        path.strip_prefix(&root)
            .unwrap_or_exit_ctx("a workspace file is outside the Buck2 project")
            .as_str()
            .replace('\\', "/")
    };
    let mut srcs = std::collections::BTreeMap::new();
    for file in ["Cargo.toml", "Cargo.lock"] {
        let path = ctx.workspace_root.join(file);
        if path.exists() {
            srcs.insert(project_path(&path), file.to_owned());
        }
    }
    for node in ctx.resolve.nodes() {
        if !matches!(node.kind, NodeKind::FirstParty { .. }) {
            continue;
        }
        let dir = node.manifest_path.parent().expect("manifest has a parent");
        let source = if dir == ctx.workspace_root {
            "Cargo.toml".to_owned()
        } else {
            format!("//{}:{MANIFEST_EXPORT}", project_path(dir))
        };
        srcs.insert(project_path(&node.manifest_path), source);
    }
    NamedFileGroup {
        name: WORKSPACE_MANIFESTS.to_owned(),
        srcs,
        visibility: ["PUBLIC".to_owned()].into(),
    }
}

/// Rule kinds that generation owns in a package BUCK file it writes: their calls
/// are replaced by the generated rules.
const GENERATED_RULE_KINDS: [&str; 9] = [
    "buildscript_run",
    "cargo_manifest",
    "export_file",
    "filegroup",
    "git_fetch",
    "http_archive",
    "rust_binary",
    "rust_library",
    "rust_test",
];

/// The hand-written content of the BUCK file at `buck_path`, which generation
/// keeps when it rewrites the file: loads of other than `@buckal//` files, and
/// every other top-level statement that is not a call of a generated rule kind,
/// each with the comments leading up to it, verbatim. Comments after the last
/// statement are kept as a statement of their own.
/// The names of the `filegroup` and `export_file` rules generation owns in a
/// first-party package's BUCK file, now or in the previous generation, and the
/// names of all the rules it emits now.
#[derive(Debug, Default)]
struct OwnedNames {
    filegroups: std::collections::BTreeSet<String>,
    exports: std::collections::BTreeSet<String>,
    emitted: std::collections::BTreeSet<String>,
}

impl OwnedNames {
    fn of(
        node: &BuckalNode,
        rules: &[Rule],
        ctx: &BuckalContext,
        previous_fixups: Option<&std::collections::BTreeMap<String, crate::config::PackageFixup>>,
    ) -> Self {
        let export_paths = |fixup: Option<&crate::config::PackageFixup>| {
            fixup
                .into_iter()
                .flat_map(|fixup| fixup.export_files.iter().map(|e| e.path().to_owned()))
                .collect::<Vec<_>>()
        };
        let mut exports: std::collections::BTreeSet<String> =
            ["workspace".to_owned(), MANIFEST_EXPORT.to_owned()].into();
        exports.extend(export_paths(ctx.repo_config.fixups.get(&node.name)));
        exports.extend(export_paths(
            previous_fixups.and_then(|fixups| fixups.get(&node.name)),
        ));
        Self {
            filegroups: ["vendor".to_owned(), WORKSPACE_MANIFESTS.to_owned()].into(),
            exports,
            emitted: rules
                .iter()
                .filter_map(Rule::name)
                .map(str::to_owned)
                .collect(),
        }
    }

    /// Whether generation owns the call of `function` named `name`: every call of a
    /// Rust, build-script or vendoring rule kind, and the `filegroup` and
    /// `export_file` calls it writes.
    fn owns(&self, function: &str, name: Option<&str>) -> bool {
        match (function, name) {
            ("filegroup", Some(name)) => self.filegroups.contains(name),
            ("export_file", Some(name)) => self.exports.contains(name),
            _ => GENERATED_RULE_KINDS.contains(&function),
        }
    }
}

fn hand_written(buck_path: &Utf8Path, owned: &OwnedNames) -> HandWritten {
    let Ok(content) = std::fs::read_to_string(buck_path) else {
        return Default::default();
    };
    let (statements, trailing) = top_level_statements(buck_path.as_str(), &content)
        .unwrap_or_exit_ctx(format!(
            "Failed to parse {buck_path}, whose hand-written content generation would replace"
        ));
    let mut hand = HandWritten::default();
    for statement in statements {
        let text = content[statement.with_comments.clone()].to_owned();
        match &statement.kind {
            TopLevelKind::Load(bzl) if bzl.starts_with("@buckal//") => {}
            TopLevelKind::Load(_) => hand.loads.push(text),
            TopLevelKind::Call { function, name } if owned.owns(function, name.as_deref()) => {
                // A rule generation owns but no longer emits (a removed target, or
                // one written by hand in a kind generation owns) goes away: say so.
                if let Some(name) = name
                    && !owned.emitted.contains(name)
                {
                    crate::buckal_warn!(format!(
                        "{buck_path}: removing `{function}(name = \"{name}\")`: generation owns every `{function}` rule of a first-party BUCK file{} and no longer emits this one",
                        if matches!(function.as_str(), "filegroup" | "export_file") {
                            " that it writes (its own names and `export_files`)"
                        } else {
                            ""
                        }
                    ));
                }
            }
            kind => {
                if let TopLevelKind::Call {
                    function,
                    name: Some(name),
                } = kind
                {
                    hand.calls.push((function.clone(), name.clone()));
                }
                hand.statements.push(text);
            }
        }
    }
    hand.statements
        .extend(trailing.map(|range| content[range].to_owned()));
    hand
}

/// What [`hand_written`] keeps of a BUCK file.
#[derive(Debug, Default)]
struct HandWritten {
    loads: Vec<String>,
    statements: Vec<String>,
    /// The kept top-level calls with a literal `name`, as `(function, name)`:
    /// targets the file declares beside the generated rules.
    calls: Vec<(String, String)>,
}

/// The rules generation splices into the workspace-root BUCK file, as
/// `(function, name)`.
fn root_owned_names(ctx: &BuckalContext) -> Vec<(&'static str, &'static str)> {
    if ctx.repo_config.workspace_manifests {
        vec![("filegroup", WORKSPACE_MANIFESTS)]
    } else {
        Vec::new()
    }
}

/// Refuse a BUCK file that would declare a target name twice: a generated rule
/// and a kept hand-written call, or either and a rule spliced in later
/// (`reserved`, as `(function, name)`).
fn check_namespace(
    rules: &[Rule],
    calls: &[(String, String)],
    reserved: &[(&str, &str)],
) -> Result<(), String> {
    let generated: Vec<&str> = rules.iter().filter_map(Rule::name).collect();
    for (function, name) in calls {
        if generated.contains(&name.as_str()) {
            return Err(format!(
                "the hand-written `{function}(name = \"{name}\")` declares the target `{name}`, which generation also emits; rename it (or the generating fixup)"
            ));
        }
    }
    for (function, name) in reserved {
        if generated.contains(name) {
            return Err(format!(
                "generation emits a rule named `{name}` here and also owns `{function}(name = \"{name}\")` in this file"
            ));
        }
        if let Some((other, _)) = calls.iter().find(|(f, n)| n == name && f != function) {
            return Err(format!(
                "generation owns `{function}(name = \"{name}\")` in this file, but the hand-written `{other}(name = \"{name}\")` declares it too; rename it"
            ));
        }
    }
    Ok(())
}

/// `content` of a BUCK file that generation does not own as a whole, without the
/// top-level calls `removed` (`(function, name)`), and with each
/// `(function, name, text)` rule replacing the top-level call of `function` with
/// that `name` in place, or appended if there is none. Everything else is kept
/// byte for byte.
fn splice_rules(
    path: &str,
    content: &str,
    rules: &[(&str, &str, &str)],
    removed: &[(&str, &str)],
) -> anyhow::Result<String> {
    let mut spliced = content.to_owned();
    for (function, name) in removed {
        let (statements, _) = top_level_statements(path, &spliced)?;
        if let Some(statement) = statements.iter().find(|statement| {
            matches!(
                &statement.kind,
                TopLevelKind::Call { function: f, name: Some(n) } if f == function && n == name
            )
        }) {
            let mut start = statement.span.start;
            let mut end = statement.span.end;
            if spliced[end..].starts_with('\n') {
                end += 1;
            }
            if spliced[..start].ends_with("\n\n") {
                start -= 1;
            }
            spliced.replace_range(start..end, "");
        }
    }
    for (function, name, text) in rules {
        let (statements, _) = top_level_statements(path, &spliced)?;
        let existing = statements.iter().find(|statement| {
            matches!(
                &statement.kind,
                TopLevelKind::Call { function: f, name: Some(n) } if f == function && n == name
            )
        });
        spliced = match existing {
            Some(statement) => format!(
                "{}{text}{}",
                &spliced[..statement.span.start],
                &spliced[statement.span.end..]
            ),
            None => {
                if !spliced.is_empty() && !spliced.ends_with('\n') {
                    spliced.push('\n');
                }
                if !spliced.is_empty() {
                    spliced.push('\n');
                }
                spliced.push_str(text);
                spliced.push('\n');
                spliced
            }
        };
    }
    Ok(spliced)
}

/// Check if a node represents a third-party dependency
pub(super) fn is_third_party(node: &BuckalNode) -> bool {
    matches!(node.kind, NodeKind::ThirdParty)
}

/// Merge existing BUCK rules with new ones, preserving manual changes in specified fields.
fn merge_rules(
    buck_path: &Utf8Path,
    buck_rules: &mut [Rule],
    ctx: &BuckalContext,
    fixup_flags: FixupFlags,
    previous_sets: Option<&std::collections::BTreeMap<String, FixupSets>>,
) {
    if buck_path.exists() {
        // Skip merging manual changes if `--no-merge` is set
        if !ctx.no_merge && !ctx.repo_config.patch_fields.is_empty() {
            let existing_rules = parse_buck_file(buck_path)
                .unwrap_or_exit_ctx(format!("Failed to parse {}", buck_path));
            patch_buck_rules(
                &existing_rules,
                buck_rules,
                &ctx.repo_config.patch_fields,
                fixup_flags,
                previous_sets,
            );
        }
    } else {
        std::fs::File::create(buck_path)
            .unwrap_or_exit_ctx(format!("Failed to create {}", buck_path));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use cargo_metadata::{PackageId, camino::Utf8PathBuf};
    use daggy::Dag;

    use crate::{
        cache::{BuckalChange, ChangeType},
        config::RepoConfig,
        context::BuckalContext,
        resolve::{BuckalNode, BuckalResolve, BuckalTarget, NodeKind},
    };

    use cargo_metadata::Edition;
    use cargo_metadata::TargetKind;

    fn mock_target(name: &str, kind: TargetKind, src_path: Utf8PathBuf) -> BuckalTarget {
        BuckalTarget {
            name: name.to_string(),
            kind: vec![kind],
            src_path,
            doctest: true,
            required_features: Vec::new(),
            test: true,
        }
    }

    fn mock_first_party_node(
        name: &str,
        manifest_path: Utf8PathBuf,
        targets: Vec<BuckalTarget>,
    ) -> BuckalNode {
        BuckalNode {
            package_id: PackageId {
                repr: format!("path+file://{name}#0.1.0"),
            },
            name: name.to_string(),
            version: "0.1.0".to_string(),
            features: vec![],
            platform_features: Default::default(),
            kind: NodeKind::FirstParty {
                relative_path: "".to_string(),
            },
            edition: Edition::E2021,
            manifest_path,
            targets,
            source: None,
            links: None,
            checksum: None,
        }
    }

    #[test]
    fn test_apply_generates_root_buck_file() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let tmp_path =
            Utf8PathBuf::try_from(tmp.path().to_path_buf()).expect("temp dir is not valid UTF-8");

        let manifest_path = tmp_path.join("Cargo.toml");
        std::fs::write(
            &manifest_path,
            "[package]\nname = \"myroot\"\nversion = \"0.1.0\"\n",
        )
        .expect("write Cargo.toml");

        // Create a src/lib.rs so the target src_path exists
        let src_dir = tmp_path.join("src");
        std::fs::create_dir_all(&src_dir).expect("create src dir");
        std::fs::write(src_dir.join("lib.rs"), "").expect("write lib.rs");

        let lib_target = mock_target("myroot", TargetKind::Lib, tmp_path.join("src/lib.rs"));
        let node = mock_first_party_node("myroot", manifest_path, vec![lib_target]);
        let package_id = node.package_id.clone();

        // Build a BuckalResolve with the node in the graph
        let mut dag = Dag::new();
        let idx = dag.add_node(node);
        let mut index_map = HashMap::new();
        index_map.insert(package_id.clone(), idx);
        let resolve = BuckalResolve {
            dag,
            index_map,
            test_only_edges: Default::default(),
        };

        // BuckalChange with Added for our root package
        let mut changes = BTreeMap::new();
        changes.insert(package_id.clone(), ChangeType::Added);
        let change = BuckalChange {
            changes,
            ..Default::default()
        };

        // BuckalContext with root set to our package (this is the key scenario)
        let ctx = BuckalContext {
            workspace_members: None,
            root: Some(package_id),
            resolve,
            workspace_root: tmp_path.clone(),
            workspace_inherit: false,
            no_merge: false,
            repo_config: RepoConfig::default(),
        };

        change.apply(&ctx);

        let buck_path = tmp_path.join("BUCK");
        assert!(
            buck_path.exists(),
            "BUCK file should be generated for root package"
        );

        let content = std::fs::read_to_string(&buck_path).expect("read BUCK file");
        assert!(
            content.contains("rust_library"),
            "BUCK file should contain a rust_library rule, got:\n{content}"
        );
        assert!(
            content.contains("load("),
            "BUCK file should contain load statements, got:\n{content}"
        );
    }
}
