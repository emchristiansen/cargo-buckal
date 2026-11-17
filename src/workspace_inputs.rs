//! Files outside a first-party package that its build script reads when it runs
//! (`[fixups.<pkg>.buildscript.run] inputs`).
//!
//! Cargo runs a build script with `CARGO_MANIFEST_DIR` set to the package's checkout
//! directory, so a script may read `../` paths into the rest of the workspace. The
//! cell's `buildscript_run` runs it in a copy of the package instead; given
//! `package_path` (the package's workspace-relative directory) and
//! `workspace_inputs` (`{workspace-relative path: source}`), it places the package
//! at `package_path` and each input at its path inside the run's directory, so those
//! relative paths resolve as under Cargo.
//!
//! The listed paths and globs are expanded here, once per generation, into files;
//! each becomes the label of the Buck package owning it. A generated first-party
//! owner exports the file to the reader through its (effective) `export_files`; any
//! other owner's BUCK file must provide the target, and generation says which.

use std::collections::{BTreeMap, BTreeSet};

use cargo_metadata::camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use globset::GlobBuilder;

use crate::{
    config::{BuildscriptRunEntry, ExportFileEntry, ExportFileSpec, RepoConfig},
    resolve::{BuckalResolve, NodeKind},
};

/// What one package's build-script runs get: the package's workspace-relative
/// directory and the inputs, `{workspace-relative path: label}`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceInputs {
    pub package_path: String,
    pub inputs: BTreeMap<String, String>,
}

/// The result of [`resolve_workspace_inputs`]: the inputs by package name, and a
/// note for each label a hand-written BUCK file must provide.
#[derive(Debug, Default)]
pub struct Resolved {
    pub by_package: BTreeMap<String, WorkspaceInputs>,
    pub notes: Vec<String>,
}

/// Expand every `[fixups.<pkg>.buildscript.run] inputs` of `config` into files and
/// their labels. `config` becomes the effective configuration: each `inputs` lists
/// the files it matched (so a package's fingerprint follows what its runs get), and
/// each generated first-party package owning one exports it to the reader.
pub fn resolve_workspace_inputs(
    config: &mut RepoConfig,
    resolve: &BuckalResolve,
    workspace_root: &Utf8Path,
    buck2_root: &Utf8Path,
    workspace_inherit: bool,
) -> anyhow::Result<Resolved> {
    let first_party: BTreeMap<Utf8PathBuf, String> = resolve
        .nodes()
        .filter(|node| matches!(node.kind, NodeKind::FirstParty { .. }))
        .map(|node| {
            (
                node.manifest_path
                    .parent()
                    .expect("manifest has a parent")
                    .to_owned(),
                node.name.clone(),
            )
        })
        .collect();
    let canonical_root = workspace_root.canonicalize_utf8()?;
    let mut resolved = Resolved::default();
    // `(owner package, path in it, visibility pattern of the reader)`.
    let mut exports: BTreeSet<(String, String, String)> = BTreeSet::new();
    let mut expanded: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, fixup) in &config.fixups {
        let Some(BuildscriptRunEntry::Table(run)) = &fixup.buildscript.run else {
            continue;
        };
        if run.inputs.is_empty() {
            continue;
        }
        let refuse = |input: &str, why: String| {
            anyhow::anyhow!("`[fixups.{name}.buildscript.run] inputs`: `{input}`: {why}")
        };
        let Some(package_dir) = first_party
            .iter()
            .find_map(|(dir, package)| (package == name).then_some(dir))
        else {
            anyhow::bail!(
                "`[fixups.{name}.buildscript.run] inputs`: `{name}` is no first-party package of the workspace"
            );
        };
        let package_path = relative(package_dir, workspace_root)?;
        let reader = format!("//{}:", relative(package_dir, buck2_root)?);
        let mut inputs = BTreeMap::new();
        for input in &run.inputs {
            for file in expand(input, workspace_root).map_err(|why| refuse(input, why))? {
                let absolute = workspace_root.join(&file);
                let canonical = absolute
                    .canonicalize_utf8()
                    .map_err(|error| refuse(input, format!("`{file}`: {error}")))?;
                if !canonical.starts_with(&canonical_root) {
                    return Err(refuse(
                        input,
                        format!("`{file}` leaves the workspace through a symbolic link"),
                    ));
                }
                if Utf8Path::new(&file).starts_with(&package_path) {
                    return Err(refuse(
                        input,
                        format!(
                            "`{file}` is inside the package `{name}`, whose files its runs have already"
                        ),
                    ));
                }
                let owner = absolute
                    .ancestors()
                    .skip(1)
                    .find(|dir| {
                        first_party.contains_key(*dir)
                            || dir.join("BUCK").exists()
                            || *dir == buck2_root
                    })
                    .filter(|dir| dir.starts_with(buck2_root))
                    .ok_or_else(|| {
                        refuse(input, format!("`{file}` is outside the Buck2 project"))
                    })?;
                let owner_path = relative(owner, buck2_root)?;
                let in_owner = relative(&absolute, owner)?;
                let label = if let Some(owner_name) = first_party.get(owner) {
                    exports.insert((owner_name.clone(), in_owner.clone(), reader.clone()));
                    format!("//{owner_path}:{in_owner}")
                } else if workspace_inherit && owner == workspace_root && in_owner == "Cargo.toml" {
                    // Generation's own export of the workspace manifest.
                    format!("//{owner_path}:workspace")
                } else {
                    let label = format!("//{owner_path}:{in_owner}");
                    resolved.notes.push(format!(
                        "`[fixups.{name}.buildscript.run] inputs`: `{file}` belongs to no generated package; its BUCK file must provide `{label}`"
                    ));
                    label
                };
                inputs.insert(file, label);
            }
        }
        expanded.insert(name.clone(), inputs.keys().cloned().collect());
        resolved.by_package.insert(
            name.clone(),
            WorkspaceInputs {
                package_path,
                inputs,
            },
        );
    }
    for (name, files) in expanded {
        if let Some(BuildscriptRunEntry::Table(run)) = config
            .fixups
            .get_mut(&name)
            .and_then(|fixup| fixup.buildscript.run.as_mut())
        {
            run.inputs = files;
        }
    }
    for (owner, path, reader) in exports {
        let export_files = &mut config.fixups.entry(owner).or_default().export_files;
        match export_files.iter_mut().find(|entry| entry.path() == path) {
            Some(ExportFileEntry::Spec(ExportFileSpec {
                visibility: Some(visibility),
                ..
            })) => {
                if !visibility.contains(&reader) && !visibility.iter().any(|v| v == "PUBLIC") {
                    visibility.push(reader);
                }
            }
            // Visible to everything already.
            Some(_) => {}
            None => export_files.push(ExportFileEntry::Spec(ExportFileSpec {
                path,
                visibility: Some(vec![reader]),
            })),
        }
    }
    Ok(resolved)
}

/// `path` relative to `base`, with `/` separators.
fn relative(path: &Utf8Path, base: &Utf8Path) -> anyhow::Result<String> {
    Ok(path
        .strip_prefix(base)
        .map_err(|_| anyhow::anyhow!("`{path}` is outside `{base}`"))?
        .as_str()
        .replace('\\', "/"))
}

/// The workspace-relative files `input` names: the file itself, or every file a
/// glob matches, sorted. The reason when it is refused.
fn expand(input: &str, workspace_root: &Utf8Path) -> Result<Vec<String>, String> {
    let written = Utf8Path::new(input);
    if input.is_empty() || written.has_root() || written.is_absolute() {
        return Err("is not a relative path".to_owned());
    }
    let mut normal: Vec<&str> = Vec::new();
    for component in written.components() {
        match component {
            Utf8Component::Normal(part) => normal.push(part),
            Utf8Component::CurDir => {}
            Utf8Component::ParentDir => {
                if normal.pop().is_none() {
                    return Err("leaves the workspace".to_owned());
                }
            }
            Utf8Component::RootDir | Utf8Component::Prefix(_) => {
                return Err("is not a relative path".to_owned());
            }
        }
    }
    if normal.is_empty() {
        return Err("names the workspace root, not a file".to_owned());
    }
    let is_glob = |part: &str| part.contains(['*', '?', '[', '{']);
    if !normal.iter().any(|part| is_glob(part)) {
        let path = normal.join("/");
        let file = workspace_root.join(&path);
        if file.is_dir() {
            return Err(format!(
                "`{path}` is a directory; name its files or a glob such as `{path}/**`"
            ));
        }
        if !file.is_file() {
            return Err("matches no file".to_owned());
        }
        return Ok(vec![path]);
    }
    let pattern = normal.join("/");
    let matcher = GlobBuilder::new(&pattern)
        .literal_separator(true)
        .build()
        .map_err(|error| format!("is not a valid glob: {error}"))?
        .compile_matcher();
    // Walk only below the pattern's literal directory.
    let base: Utf8PathBuf = normal.iter().take_while(|part| !is_glob(part)).collect();
    let mut files: Vec<String> = walkdir::WalkDir::new(workspace_root.join(&base))
        .into_iter()
        .filter_entry(|entry| entry.file_name() != ".git")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| {
            let path = Utf8Path::from_path(entry.path())?;
            let path = relative(path, workspace_root).ok()?;
            matcher.is_match(&path).then_some(path)
        })
        .collect();
    files.sort();
    if files.is_empty() {
        return Err("matches no file".to_owned());
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::expand;
    use cargo_metadata::camino::Utf8PathBuf;

    #[test]
    fn test_expand_refuses_and_matches() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/x.toml"), "").unwrap();
        std::fs::write(root.join("a/b/y.toml"), "").unwrap();
        std::fs::write(root.join("a/b/z.txt"), "").unwrap();
        assert_eq!(expand("a/x.toml", &root).unwrap(), ["a/x.toml"]);
        assert_eq!(expand("./a/b/../x.toml", &root).unwrap(), ["a/x.toml"]);
        assert_eq!(expand("a/*.toml", &root).unwrap(), ["a/x.toml"]);
        assert_eq!(
            expand("a/**/*.toml", &root).unwrap(),
            ["a/b/y.toml", "a/x.toml"]
        );
        for (input, why) in [
            ("", "is not a relative path"),
            ("/a/x.toml", "is not a relative path"),
            ("../a/x.toml", "leaves the workspace"),
            ("a/../../x.toml", "leaves the workspace"),
            ("a/..", "names the workspace root"),
            ("a/b", "is a directory"),
            ("a/missing.toml", "matches no file"),
            ("a/*.json", "matches no file"),
        ] {
            let error = expand(input, &root).unwrap_err();
            assert!(error.contains(why), "{input}: {error}");
        }
    }
}
