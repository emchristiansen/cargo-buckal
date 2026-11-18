use cargo_metadata::camino::{Utf8Path, Utf8PathBuf};
use regex::Regex;

use crate::{
    buck::{
        FixupFlags, FixupSets, NamedFileGroup, PACKAGE_MACRO_MARKER, PARTLY_GENERATED_HEADER, Rule,
        TopLevelKind, TopLevelStatement, canonical_call, is_generated_header_line, parse_buck_file,
        patch_buck_rules, top_level_statements,
    },
    buckal_error, buckal_log,
    buckify::emit::emit_export_file,
    cache::{BuckalChange, ChangeType},
    context::BuckalContext,
    resolve::{BuckalNode, NodeKind},
    utils::{UnwrapOrExit, get_buck2_root, get_vendor_dir, is_path_source},
};

use super::{
    buckify_dep_node, buckify_root_node, cross, gen_buck_content, gen_buck_content_preserving,
    rules::{MANIFEST_EXPORT, build_script_names, fixup_set_contributions},
    windows,
};

impl BuckalChange {
    /// Apply the changes to the BUCK files based on the detected package changes in the cache diff.
    pub fn apply(&self, ctx: &BuckalContext) {
        let re: Regex = Regex::new(r"^([^+#]+)\+([^#]+)#([^@]+)@([^+#]+)(?:\+(.+))?$")
            .expect("error creating regex");

        let mut workspace_emitted = false;
        // Nothing is written until every package's rules are known: a refusal
        // (which exits) leaves every file as it was.
        let mut writes: Vec<(Utf8PathBuf, String)> = Vec::new();
        let mut removals: Vec<Utf8PathBuf> = Vec::new();

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
                            get_vendor_dir(&node.package_id)
                                .unwrap_or_exit_ctx("failed to get vendor directory")
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
                            if let Err(error) = check_namespace(&buck_rules, &[], &[]) {
                                buckal_error!(format!(
                                    "{buck_path}: {error}; it is left unchanged"
                                ));
                                std::process::exit(1);
                            }
                            gen_buck_content(&buck_rules)
                        } else {
                            let owned = OwnedNames::of(
                                node,
                                &buck_rules,
                                ctx,
                                self.previous_fixups.as_ref(),
                            );
                            let hand = match hand_written(&buck_path, &owned) {
                                Ok(hand) => hand,
                                Err(error) => {
                                    buckal_error!(error);
                                    std::process::exit(1);
                                }
                            };
                            // A kept hand-written load already binds the macro.
                            if hand.binds_macro {
                                for rule in &mut buck_rules {
                                    if let Rule::PackageMacro(m) = rule {
                                        m.load = false;
                                    }
                                }
                            }
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
                        writes.push((buck_path, buck_content));
                    }
                }
                ChangeType::Removed => {
                    // Path-source packages (the workspace root, its members, and any
                    // local `path = "..."` dep) live in the user's source tree, not in
                    // a vendor directory, so there is nothing for us to remove. Skip
                    // them; get_vendor_dir would otherwise abort on the unsupported
                    // source kind.
                    if is_path_source(id).unwrap_or_exit_ctx("failed to classify package source") {
                        continue;
                    }

                    let caps = re.captures(&id.repr).expect("Failed to parse package ID");
                    let name = &caps[3];
                    let version = &caps[4];

                    buckal_log!("Removing", format!("{} v{}", name, version));
                    removals.push(
                        get_vendor_dir(id).unwrap_or_exit_ctx("failed to get vendor directory"),
                    );
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
        // The root package's BUCK file of this run, if it was generated above.
        let pending_root = writes.iter().rposition(|(path, _)| *path == buck_path);
        if pending_root.is_some() || buck_path.exists() {
            let existing = match pending_root {
                Some(at) => writes[at].1.clone(),
                None => std::fs::read_to_string(&buck_path)
                    .unwrap_or_exit_ctx(format!("Failed to read {}", buck_path)),
            };
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
            match pending_root {
                Some(at) => writes[at].1 = buck_content,
                None if buck_content != existing => writes.push((buck_path, buck_content)),
                None => {}
            }
        } else if !owned.is_empty() {
            let texts: Vec<&str> = owned.iter().map(|(_, _, text)| text.as_str()).collect();
            let buck_content = format!("{PARTLY_GENERATED_HEADER}\n\n{}", texts.join("\n"));
            writes.push((buck_path, buck_content));
        }

        for (path, content) in writes {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).expect("Failed to create the package directory");
            }
            std::fs::write(&path, content).expect("Failed to write BUCK file");
        }
        for vendor_dir in removals {
            if vendor_dir.exists() {
                std::fs::remove_dir_all(&vendor_dir).expect("Failed to remove vendor directory");
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
const GENERATED_RULE_KINDS: [&str; 11] = [
    "buildscript_run",
    "cargo_manifest",
    "export_file",
    "filegroup",
    "git_fetch",
    "http_archive",
    "rust_binary",
    "rust_library",
    "rust_test",
    // The buckal cell's rules, loaded from `@buckal//`, whose loads generation
    // also owns: a hand-written call could not keep its load.
    "wasm_browser_test",
    "wasm_sha256",
];

/// The names of the `filegroup` and `export_file` rules generation owns in a
/// first-party package's BUCK file, now or in the previous generation, and the
/// names of all the rules it emits now.
#[derive(Debug, Default)]
struct OwnedNames {
    filegroups: std::collections::BTreeSet<String>,
    exports: std::collections::BTreeSet<String>,
    emitted: std::collections::BTreeSet<String>,
    /// The `[package_macro]` call generation writes in this file.
    package_macro: Option<OwnedMacro>,
}

/// The `[package_macro]` call generation writes in a first-party BUCK file.
#[derive(Debug)]
struct OwnedMacro {
    bzl: String,
    function: String,
    /// The call as generation writes it, without its marker.
    call: String,
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
            package_macro: rules.iter().find_map(|rule| match rule {
                Rule::PackageMacro(m) => Some(OwnedMacro {
                    bzl: m.bzl.clone(),
                    function: m.function.clone(),
                    call: serde_starlark::to_string(rule).expect("serialize package macro call"),
                }),
                _ => None,
            }),
        }
    }

    /// Whether generation owns the call of `function` named `name`: every call of a
    /// Rust, build-script, vendoring, `wasm_sha256` or `wasm_browser_test` rule
    /// kind, the `filegroup` and `export_file` calls it writes, and the modules' and
    /// test-mode modules' `configured_alias`es.
    /// An owned call generation does not emit is removed, with a warning.
    fn owns(&self, function: &str, name: Option<&str>) -> bool {
        if self
            .package_macro
            .as_ref()
            .is_some_and(|owned| owned.function == function)
        {
            return true;
        }
        match (function, name) {
            ("filegroup", Some(name)) => self.filegroups.contains(name),
            ("export_file", Some(name)) => self.exports.contains(name),
            ("configured_alias", Some(name)) => {
                [crate::wasm::MODULE_ALIAS, crate::wasm::TEST_MODULE_ALIAS]
                    .iter()
                    .any(|alias| name == *alias || name.starts_with(&format!("{alias}.")))
            }
            _ => GENERATED_RULE_KINDS.contains(&function),
        }
    }
}

/// The hand-written content of the BUCK file at `buck_path`, which generation
/// keeps when it rewrites the file: loads of other than `@buckal//` files, and
/// every other top-level statement that is not a call of a generated rule kind,
/// each with the comments leading up to it, verbatim. Comments after the last
/// statement are kept as a statement of their own.
///
/// An `Err` is a refusal: a hand-written load that generation cannot reconcile
/// with the `[package_macro]` call it writes. The caller leaves the file as it is.
fn hand_written(buck_path: &Utf8Path, owned: &OwnedNames) -> Result<HandWritten, String> {
    let Ok(content) = std::fs::read_to_string(buck_path) else {
        return Ok(Default::default());
    };
    let (statements, trailing) = top_level_statements(buck_path.as_str(), &content)
        .unwrap_or_exit_ctx(format!(
            "Failed to parse {buck_path}, whose hand-written content generation would replace"
        ));
    let marks: Vec<bool> = statements
        .iter()
        .map(|statement| is_package_macro_marked(&content, statement))
        .collect();
    // The top-level calls this generation keeps, as `(function, call)`: a marked
    // load an earlier generation wrote stays while one of them uses a name it binds.
    let kept_calls: Vec<(&str, &str)> = statements
        .iter()
        .zip(&marks)
        .filter_map(|(statement, &marked)| match &statement.kind {
            TopLevelKind::Call { function, name }
                if !removes_call(
                    owned,
                    function,
                    name.as_deref(),
                    marked,
                    &content[statement.with_comments.clone()],
                ) =>
            {
                Some((
                    function.as_str(),
                    content[statement.span.clone()].trim_end(),
                ))
            }
            _ => None,
        })
        .collect();
    let mut hand = HandWritten::default();
    for (statement, &marked) in statements.iter().zip(&marks) {
        let text = content[statement.with_comments.clone()].to_owned();
        // A marked load that a kept call still uses: dropping it would leave the
        // call calling a name nothing binds, so it is kept like a hand-written load.
        let used_by = match &statement.kind {
            TopLevelKind::Load(bzl, bindings) if marked && !bzl.starts_with("@buckal//") => {
                kept_calls.iter().find_map(|&(function, call)| {
                    bindings
                        .iter()
                        .any(|(local, _)| local == function)
                        .then_some((bzl, function, call))
                })
            }
            _ => None,
        };
        if let Some((bzl, function, call)) = used_by {
            crate::buckal_warn!(format!(
                "{buck_path}: keeping the load of `{function}` from `{bzl}` that an earlier generation wrote from `[package_macro]`, because the kept call `{call}` still calls it: generation removes a call only when the marker is on the line directly above it. Remove the call and the load by hand if the call should go"
            ));
        }
        match &statement.kind {
            TopLevelKind::Load(bzl, _) if bzl.starts_with("@buckal//") => {}
            // The `[package_macro]` load an earlier generation wrote, which no kept
            // call uses.
            TopLevelKind::Load(..) if marked && used_by.is_none() => {}
            TopLevelKind::Load(bzl, bindings) => match &owned.package_macro {
                // The macro bound under another local name: its calls are calls of
                // the macro that generation does not recognise as such, so it
                // would call the macro a second time.
                Some(OwnedMacro {
                    bzl: macro_bzl,
                    function,
                    ..
                }) if bzl == macro_bzl
                    && let Some((local, _)) = bindings
                        .iter()
                        .find(|(local, their)| their == function && local != function) =>
                {
                    return Err(format!(
                        "{buck_path}: a hand-written load binds `{function}` of `{macro_bzl}`, the `[package_macro]` function generation calls here, under another name, `{local}`, so generation would call it a second time; load it as `{function}`, whose hand-written call generation then takes over, or remove the load and its calls. The file is left unchanged"
                    ));
                }
                Some(OwnedMacro {
                    bzl: macro_bzl,
                    function,
                    ..
                }) if bindings.iter().any(|(local, _)| local == function) => {
                    let only = bindings.len() == 1;
                    let same = bzl == macro_bzl
                        && bindings
                            .iter()
                            .any(|(local, their)| local == function && their == function);
                    if !same {
                        return Err(format!(
                            "{buck_path}: a hand-written load binds `{function}`, the `[package_macro]` function generation calls here, to something other than `{function}` of `{macro_bzl}`; rename the binding. The file is left unchanged"
                        ));
                    }
                    if !only {
                        // It binds other names of the file too: keep it, and
                        // generation does not load the macro again.
                        hand.binds_macro = true;
                        hand.loads.push(text);
                    }
                }
                _ => hand.loads.push(text),
            },
            // The `[package_macro]` call an earlier generation wrote, which this one
            // no longer configures.
            TopLevelKind::Call { function, .. } if marked && !owned.owns(function, None) => {
                crate::buckal_warn!(format!(
                    "{buck_path}: removing the `{function}()` call an earlier generation wrote from `[package_macro]`, which no longer configures it here"
                ));
            }
            TopLevelKind::Call { function, name }
                if removes_call(owned, function, name.as_deref(), marked, &text) =>
            {
                // A hand-written call of the macro that generation takes over.
                if !marked
                    && let Some(warning) = owned.package_macro.as_ref().and_then(|m| {
                        replaced_arguments_warning(
                            buck_path,
                            m,
                            function,
                            &content[statement.span.clone()],
                        )
                    })
                {
                    crate::buckal_warn!(warning);
                }
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
    Ok(hand)
}

/// Whether generation removes (or replaces with its own) the top-level call of
/// `function` named `name`, whose text with its leading comments is `text` and
/// which is `marked` as written from `[package_macro]`: a marked call the table no
/// longer configures, and every call generation owns.
fn removes_call(
    owned: &OwnedNames,
    function: &str,
    name: Option<&str>,
    marked: bool,
    text: &str,
) -> bool {
    marked && !owned.owns(function, None)
        || owned.owns(function, name)
        // An `export_files` rule generation wrote, recognised by its label even
        // without a record of the earlier fixups.
        || function == "export_file"
            && text.contains(&format!("\"{}\"", crate::buck::EXPORT_FILES_LABEL))
}

/// The warning for a hand-written call `hand` of `function` (the statement alone,
/// without its comments) in `buck_path` that generation replaces with its own
/// `[package_macro]` call `owned`, when their arguments differ; `None` when
/// `function` is not the macro or the two calls are the same up to layout and
/// keyword order.
fn replaced_arguments_warning(
    buck_path: &Utf8Path,
    owned: &OwnedMacro,
    function: &str,
    hand: &str,
) -> Option<String> {
    if function != owned.function {
        return None;
    }
    let generated = owned.call.trim_end();
    let hand = hand.trim_end();
    match (canonical_call(hand), canonical_call(generated)) {
        (Some(a), Some(b)) if a == b => None,
        _ => Some(format!(
            "{buck_path}: replacing the hand-written `{hand}` with the `[package_macro]` call `{generated}`: the arguments change"
        )),
    }
}

/// Whether `statement` of BUCK file text `content` is one generation wrote from
/// `[package_macro]`: the comment line directly above it, with no blank line
/// between, is [`PACKAGE_MACRO_MARKER`], as generation writes it. Other comment
/// lines and blank lines further up its comment block do not matter, and a
/// marker separated from the statement by a blank line or another comment line
/// marks nothing. [`hand_written`] keeps a marked load while a kept call still
/// uses a name it binds.
fn is_package_macro_marked(content: &str, statement: &TopLevelStatement) -> bool {
    content[statement.with_comments.start..statement.span.start]
        .strip_suffix('\n')
        .and_then(|above| above.rsplit('\n').next())
        .is_some_and(|line| line.trim() == PACKAGE_MACRO_MARKER)
}

/// What [`hand_written`] keeps of a BUCK file.
#[derive(Debug, Default)]
struct HandWritten {
    /// A kept load binds the `[package_macro]` function, so generation does not
    /// load it again.
    binds_macro: bool,
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

/// Refuse a BUCK file that would declare a target name twice: two generated
/// rules, a generated rule and a kept hand-written call, or either and a rule
/// spliced in later (`reserved`, as `(function, name)`).
fn check_namespace(
    rules: &[Rule],
    calls: &[(String, String)],
    reserved: &[(&str, &str)],
) -> Result<(), String> {
    let mut seen: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
    for rule in rules {
        let Some(name) = rule.name() else { continue };
        if let Some(first) = seen.insert(name, rule.function()) {
            return Err(format!(
                "generation emits two rules named `{name}` (`{first}` and `{}`); rename what generates one of them",
                rule.function()
            ));
        }
    }
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
    // A package without a BUCK file has nothing to merge; the file is written
    // with the others once every package's rules are known.
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
            host_platform_differences: None,
            host_platform_difference_digests: None,
            buildscript_inputs: Default::default(),
            wasm_modules: Default::default(),
            declared_test_env: Default::default(),
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

    /// The BUCK file `apply` writes for a library package at the workspace root of
    /// a fresh directory, whose BUCK file is `existing` before, under `config`.
    fn apply_to_root_package(existing: Option<&str>, config: RepoConfig) -> String {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let tmp_path =
            Utf8PathBuf::try_from(tmp.path().to_path_buf()).expect("temp dir is not valid UTF-8");
        let manifest_path = tmp_path.join("Cargo.toml");
        std::fs::write(
            &manifest_path,
            "[package]\nname = \"myroot\"\nversion = \"0.1.0\"\n",
        )
        .expect("write Cargo.toml");
        std::fs::create_dir_all(tmp_path.join("src")).expect("create src dir");
        std::fs::write(tmp_path.join("src/lib.rs"), "").expect("write lib.rs");
        if let Some(existing) = existing {
            std::fs::write(tmp_path.join("BUCK"), existing).expect("write BUCK");
        }

        let lib_target = mock_target("myroot", TargetKind::Lib, tmp_path.join("src/lib.rs"));
        let node = mock_first_party_node("myroot", manifest_path, vec![lib_target]);
        let package_id = node.package_id.clone();
        let mut dag = Dag::new();
        let idx = dag.add_node(node);
        let mut index_map = HashMap::new();
        index_map.insert(package_id.clone(), idx);
        let resolve = BuckalResolve {
            dag,
            index_map,
            test_only_edges: Default::default(),
        };
        let mut changes = BTreeMap::new();
        changes.insert(package_id.clone(), ChangeType::Changed);
        let change = BuckalChange {
            changes,
            ..Default::default()
        };
        let ctx = BuckalContext {
            workspace_members: None,
            host_platform_differences: None,
            host_platform_difference_digests: None,
            buildscript_inputs: Default::default(),
            wasm_modules: Default::default(),
            declared_test_env: Default::default(),
            root: Some(package_id),
            resolve,
            workspace_root: tmp_path.clone(),
            workspace_inherit: false,
            no_merge: false,
            repo_config: config,
        };
        change.apply(&ctx);
        std::fs::read_to_string(tmp_path.join("BUCK")).expect("read BUCK file")
    }

    fn package_macro_config() -> RepoConfig {
        RepoConfig {
            package_macro: crate::config::PackageMacroConfig {
                bzl: "//buck2:source_tree_inputs.bzl".to_owned(),
                function: "declare_source_tree_inputs".to_owned(),
                first_party: true,
                ..Default::default()
            },
            ..RepoConfig::default()
        }
    }

    const MACRO_LOAD: &str =
        "load(\"//buck2:source_tree_inputs.bzl\", \"declare_source_tree_inputs\")";

    #[test]
    fn test_package_macro_takes_over_a_hand_written_call_and_load() {
        let existing = format!(
            "{MACRO_LOAD}\nload(\"//other:defs.bzl\", \"kept_rule\")\n\n# The package's exports, by hand.\ndeclare_source_tree_inputs()\n\nkept_rule(name = \"kept\")\n"
        );
        let content = apply_to_root_package(Some(&existing), package_macro_config());
        assert_eq!(
            content.matches("declare_source_tree_inputs()").count(),
            1,
            "{content}"
        );
        assert_eq!(content.matches(MACRO_LOAD).count(), 1, "{content}");
        assert_eq!(
            content.matches(crate::buck::PACKAGE_MACRO_MARKER).count(),
            2,
            "{content}"
        );
        assert!(
            content.contains(&format!(
                "{}\ndeclare_source_tree_inputs()\n",
                crate::buck::PACKAGE_MACRO_MARKER
            )),
            "{content}"
        );
        assert!(!content.contains("by hand"), "{content}");
        assert!(
            content.contains("load(\"//other:defs.bzl\", \"kept_rule\")"),
            "{content}"
        );
        assert!(content.contains("kept_rule(name = \"kept\")"), "{content}");

        // Regenerating its own output changes nothing.
        let again = apply_to_root_package(Some(&content), package_macro_config());
        assert_eq!(again, content);
    }

    #[test]
    fn test_package_macro_keeps_a_hand_written_load_that_binds_more() {
        let load = "load(\"//buck2:source_tree_inputs.bzl\", \"SOURCE_TREES\", \"declare_source_tree_inputs\")";
        let existing = format!("{load}\n\ndeclare_source_tree_inputs()\n\nprint(SOURCE_TREES)\n");
        let content = apply_to_root_package(Some(&existing), package_macro_config());
        assert!(content.contains(load), "{content}");
        assert!(!content.contains(MACRO_LOAD), "{content}");
        assert_eq!(
            content
                .matches("load(\"//buck2:source_tree_inputs.bzl\"")
                .count(),
            1
        );
        assert_eq!(
            content.matches("declare_source_tree_inputs()").count(),
            1,
            "{content}"
        );
        assert!(content.contains("print(SOURCE_TREES)"), "{content}");
    }

    #[test]
    fn test_package_macro_call_is_removed_once_unconfigured() {
        let hand = "load(\"//other:defs.bzl\", \"kept_rule\")\n\nkept_rule(name = \"kept\")\n";
        let generated = apply_to_root_package(Some(hand), package_macro_config());
        assert!(
            generated.contains("declare_source_tree_inputs()"),
            "{generated}"
        );
        let content = apply_to_root_package(Some(&generated), RepoConfig::default());
        assert!(!content.contains("declare_source_tree_inputs"), "{content}");
        assert!(
            !content.contains(crate::buck::PACKAGE_MACRO_MARKER),
            "{content}"
        );
        assert!(content.contains("kept_rule(name = \"kept\")"), "{content}");
        assert_eq!(
            content,
            apply_to_root_package(Some(hand), RepoConfig::default())
        );
    }

    #[test]
    fn test_unconfigured_generation_keeps_a_hand_written_call() {
        let existing = format!("{MACRO_LOAD}\n\ndeclare_source_tree_inputs()\n");
        let content = apply_to_root_package(Some(&existing), RepoConfig::default());
        assert!(content.contains(MACRO_LOAD), "{content}");
        assert!(
            content.contains("declare_source_tree_inputs()"),
            "{content}"
        );
        assert!(
            !content.contains(crate::buck::PACKAGE_MACRO_MARKER),
            "{content}"
        );
    }

    // PM-1 (Fable, review 36, probe 7): a hand-written call whose arguments differ
    // from the generated call is replaced, with a warning naming the file and both
    // calls.
    #[test]
    fn test_package_macro_warns_when_takeover_changes_a_calls_arguments() {
        let owned = super::OwnedMacro {
            bzl: "//buck2:source_tree_inputs.bzl".to_owned(),
            function: "declare_source_tree_inputs".to_owned(),
            call: "declare_source_tree_inputs(\n    cell_path = \"x\",\n    whole = True,\n)\n"
                .to_owned(),
        };
        let path = cargo_metadata::camino::Utf8Path::new("pkg/BUCK");
        let warning = super::replaced_arguments_warning(
            path,
            &owned,
            "declare_source_tree_inputs",
            "declare_source_tree_inputs(whole = True, cell_path = \"mine\")",
        )
        .expect("a warning");
        assert!(warning.starts_with("pkg/BUCK: "), "{warning}");
        assert!(
            warning.contains("`declare_source_tree_inputs(whole = True, cell_path = \"mine\")`"),
            "{warning}"
        );
        assert!(
            warning.contains(
                "`declare_source_tree_inputs(\n    cell_path = \"x\",\n    whole = True,\n)`"
            ),
            "{warning}"
        );
        assert!(warning.contains("the arguments change"), "{warning}");
        // A missing argument, and an extra one, are changes too.
        for hand in [
            "declare_source_tree_inputs(whole = True)",
            "declare_source_tree_inputs(cell_path = \"x\", whole = True, depth = 1)",
            "declare_source_tree_inputs(\"x\", whole = True)",
        ] {
            assert!(
                super::replaced_arguments_warning(path, &owned, "declare_source_tree_inputs", hand)
                    .is_some(),
                "{hand}"
            );
        }

        // The same call in another layout, keyword order or with a trailing comma
        // is no change.
        for hand in [
            "declare_source_tree_inputs(whole = True, cell_path = \"x\")",
            "declare_source_tree_inputs(cell_path=\"x\", whole=True,)",
            "declare_source_tree_inputs(\n    whole = True,\n    cell_path = \"x\",\n)",
        ] {
            assert_eq!(
                super::replaced_arguments_warning(path, &owned, "declare_source_tree_inputs", hand),
                None,
                "{hand}"
            );
        }
        // Nor is a call of another function this warning's business.
        assert_eq!(
            super::replaced_arguments_warning(
                path,
                &owned,
                "rust_library",
                "rust_library(name = \"x\")"
            ),
            None
        );
    }

    #[test]
    fn test_package_macro_takeover_of_a_call_with_arguments_writes_the_configured_call() {
        let existing = format!(
            "{MACRO_LOAD}\n\n# by hand\ndeclare_source_tree_inputs(whole = True, cell_path = \"mine\")\n"
        );
        let content = apply_to_root_package(Some(&existing), package_macro_config());
        assert_eq!(
            content.matches("declare_source_tree_inputs(").count(),
            1,
            "{content}"
        );
        assert!(
            content.contains(&format!(
                "{}\ndeclare_source_tree_inputs()\n",
                crate::buck::PACKAGE_MACRO_MARKER
            )),
            "{content}"
        );
        assert!(!content.contains("mine"), "{content}");
        assert!(!content.contains("by hand"), "{content}");
        assert_eq!(content.matches(MACRO_LOAD).count(), 1, "{content}");
    }

    /// `generated` with `above` inserted before the marker of its macro call.
    fn with_comments_above_the_call(generated: &str, above: &str) -> String {
        let call = format!(
            "{}\ndeclare_source_tree_inputs()\n",
            crate::buck::PACKAGE_MACRO_MARKER
        );
        assert_eq!(generated.matches(&call).count(), 1, "{generated}");
        generated.replace(&call, &format!("{above}{call}"))
    }

    // PM-2 (Fable, review 36, probe 11): the marker directly above the call is
    // recognised whatever comment lines and blank lines precede it, so removing
    // the table removes both the call and its load.
    #[test]
    fn test_package_macro_marker_is_recognised_below_other_comments() {
        let hand = "load(\"//other:defs.bzl\", \"kept_rule\")\n\nkept_rule(name = \"kept\")\n";
        let generated = apply_to_root_package(Some(hand), package_macro_config());
        for above in [
            "# why this package exports its inputs\n\n",
            "# why this package exports its inputs\n# and more\n",
            "# one\n\n# two\n",
        ] {
            let edited = with_comments_above_the_call(&generated, above);
            // Configured, the call stays the one generated call.
            let again = apply_to_root_package(Some(&edited), package_macro_config());
            assert_eq!(
                again.matches("declare_source_tree_inputs()").count(),
                1,
                "{again}"
            );
            // Unconfigured, the call and its load both go.
            let content = apply_to_root_package(Some(&edited), RepoConfig::default());
            assert!(
                !content.contains("declare_source_tree_inputs"),
                "{above:?}:\n{content}"
            );
            assert!(
                !content.contains(crate::buck::PACKAGE_MACRO_MARKER),
                "{above:?}:\n{content}"
            );
            assert!(content.contains("kept_rule(name = \"kept\")"), "{content}");
        }
    }

    // The choice behind PM-2: only the line directly above a statement marks it.
    // A marker separated from a hand-written call by a blank line, or by another
    // comment line, does not make it generated.
    #[test]
    fn test_package_macro_marker_marks_only_the_statement_directly_below_it() {
        let marker = crate::buck::PACKAGE_MACRO_MARKER;
        for between in ["\n", "# by hand\n"] {
            let existing =
                format!("{MACRO_LOAD}\n\n{marker}\n{between}declare_source_tree_inputs()\n");
            let content = apply_to_root_package(Some(&existing), RepoConfig::default());
            assert!(content.contains(MACRO_LOAD), "{between:?}:\n{content}");
            assert!(
                content.contains("declare_source_tree_inputs()\n"),
                "{between:?}:\n{content}"
            );
        }
    }

    // PM-5 (Fable, review 42, probes e1, e1s and e2): a comment line or a blank
    // line between the marker and the generated call unmarks the call, which is
    // kept as hand-written. Its marked load is kept with it, so the file does not
    // call a name it no longer loads.
    #[test]
    fn test_package_macro_keeps_a_marked_load_while_a_kept_call_uses_it() {
        let marker = crate::buck::PACKAGE_MACRO_MARKER;
        let hand = "load(\"//other:defs.bzl\", \"kept_rule\")\n\nkept_rule(name = \"kept\")\n";
        let generated = apply_to_root_package(Some(hand), package_macro_config());
        let call = format!("{marker}\ndeclare_source_tree_inputs()\n");
        let off = RepoConfig {
            package_macro: crate::config::PackageMacroConfig {
                first_party: false,
                ..package_macro_config().package_macro
            },
            ..RepoConfig::default()
        };
        for between in ["# note: why this package exports\n", "\n"] {
            assert_eq!(generated.matches(&call).count(), 1, "{generated}");
            let edited = generated.replace(
                &call,
                &format!("{marker}\n{between}declare_source_tree_inputs()\n"),
            );
            // The table removed (e1, e2), or its first-party side switched off (e1s).
            for (case, config) in [("removed", RepoConfig::default()), ("off", off.clone())] {
                let content = apply_to_root_package(Some(&edited), config.clone());
                assert!(
                    content.contains(&format!("{marker}\n{MACRO_LOAD}\n")),
                    "{between:?} {case}:\n{content}"
                );
                assert!(
                    content.contains(&format!(
                        "{marker}\n{between}declare_source_tree_inputs()\n"
                    )),
                    "{between:?} {case}:\n{content}"
                );
                assert!(content.contains("kept_rule(name = \"kept\")"), "{content}");
                // A later generation keeps the pair as it is.
                assert_eq!(
                    apply_to_root_package(Some(&content), config),
                    content,
                    "{between:?} {case}"
                );
            }
            // Configured again, the call is taken over and the file is the
            // generated one.
            assert_eq!(
                apply_to_root_package(Some(&edited), package_macro_config()),
                generated,
                "{between:?}"
            );
        }
    }

    // PM-5's mirror (Fable, review 42, probe e3): a comment line between the marker
    // and the generated load unmarks the load. Once the table is removed the
    // marked call goes and the load stays, binding a name nothing calls.
    #[test]
    fn test_package_macro_unmarked_load_stays_when_its_marked_call_goes() {
        let marker = crate::buck::PACKAGE_MACRO_MARKER;
        let generated = apply_to_root_package(None, package_macro_config());
        let load = format!("{marker}\n{MACRO_LOAD}\n");
        assert_eq!(generated.matches(&load).count(), 1, "{generated}");
        let edited = generated.replace(
            &load,
            &format!("{marker}\n# note: the inputs macro\n{MACRO_LOAD}\n"),
        );
        let content = apply_to_root_package(Some(&edited), RepoConfig::default());
        assert!(
            content.contains(&format!(
                "{marker}\n# note: the inputs macro\n{MACRO_LOAD}\n"
            )),
            "{content}"
        );
        assert!(
            !content.contains("declare_source_tree_inputs()"),
            "{content}"
        );
    }

    // PM-4 (Fable, review 36, probe 8): a load that binds the macro under another
    // local name is refused, rather than calling the macro a second time.
    #[test]
    fn test_package_macro_refuses_a_load_of_the_macro_under_another_name() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let dir =
            Utf8PathBuf::try_from(tmp.path().to_path_buf()).expect("temp dir is not valid UTF-8");
        let owned = super::OwnedNames {
            package_macro: Some(super::OwnedMacro {
                bzl: "//buck2:source_tree_inputs.bzl".to_owned(),
                function: "declare_source_tree_inputs".to_owned(),
                call: "declare_source_tree_inputs()\n".to_owned(),
            }),
            ..Default::default()
        };
        for load in [
            "load(\"//buck2:source_tree_inputs.bzl\", di = \"declare_source_tree_inputs\")",
            "load(\"//buck2:source_tree_inputs.bzl\", \"SOURCE_TREES\", di = \"declare_source_tree_inputs\")",
            "load(\"//buck2:source_tree_inputs.bzl\", \"declare_source_tree_inputs\", di = \"declare_source_tree_inputs\")",
        ] {
            let path = dir.join("BUCK");
            std::fs::write(&path, format!("{load}\n\ndi()\n")).expect("write BUCK");
            let error = super::hand_written(&path, &owned).expect_err(load);
            assert!(error.starts_with(&format!("{path}: ")), "{error}");
            assert!(error.contains("under another name, `di`"), "{error}");
            assert!(error.contains("The file is left unchanged"), "{error}");
        }

        // The same alias of a function of another file is no binding of the macro.
        let path = dir.join("BUCK");
        let other = "load(\"//other:x.bzl\", di = \"declare_source_tree_inputs\")";
        std::fs::write(&path, format!("{other}\n\ndi()\n")).expect("write BUCK");
        let hand = super::hand_written(&path, &owned).expect("kept");
        assert_eq!(hand.loads, vec![other.to_owned()]);
        assert_eq!(hand.statements, vec!["di()".to_owned()]);
    }

    // Unconfigured, an aliased load and its calls are hand-written content like any
    // other, and kept.
    #[test]
    fn test_unconfigured_generation_keeps_an_aliased_load() {
        let load = "load(\"//buck2:source_tree_inputs.bzl\", di = \"declare_source_tree_inputs\")";
        let content =
            apply_to_root_package(Some(&format!("{load}\n\ndi()\n")), RepoConfig::default());
        assert!(content.contains(load), "{content}");
        assert!(content.contains("\ndi()\n"), "{content}");
        assert!(
            !content.contains("declare_source_tree_inputs()"),
            "{content}"
        );
    }
}
