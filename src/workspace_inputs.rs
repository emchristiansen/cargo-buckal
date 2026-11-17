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
    let start = workspace_root.join(&base);
    if !start.exists() {
        return Err("matches no file".to_owned());
    }
    let mut files: Vec<String> = Vec::new();
    // Symbolic links to directories are followed below the base as at it, so
    // the glob sees the tree a build script reading through them sees (a loop
    // is an error the walk reports).
    for entry in walkdir::WalkDir::new(&start)
        .follow_links(true)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != ".git")
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                let at = error.path().and_then(Utf8Path::from_path);
                let at_relative = at.and_then(|p| relative(p, workspace_root).ok());
                // A link that leads nowhere: refused when the glob matches it,
                // as its literal path is; otherwise it gives no file whatever it
                // was meant to be.
                if let (Some(at), Some(at_relative)) = (at, &at_relative)
                    && at.is_symlink()
                    && !at.exists()
                {
                    if matcher.is_match(at_relative) {
                        return Err(format!("`{at_relative}` is a dangling symbolic link"));
                    }
                    continue;
                }
                // An entry the walk cannot read could hold a matching file:
                // refuse rather than give the build script fewer files than Cargo
                // would.
                let at = at_relative.unwrap_or_else(|| base.to_string());
                return Err(format!("cannot read `{at}`: {error}"));
            }
        };
        let Some(path) = Utf8Path::from_path(entry.path()) else {
            continue;
        };
        let Ok(relative_path) = relative(path, workspace_root) else {
            continue;
        };
        // A symbolic link to a file counts as the file, as a literal path does;
        // where it leads is checked with every match.
        if entry.file_type().is_file() && matcher.is_match(&relative_path) {
            files.push(relative_path);
        }
    }
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

    #[cfg(unix)]
    #[test]
    fn test_a_glob_follows_symlinked_files_and_reports_unreadable_directories() {
        // N9: a glob accepts a symbolic link to a file, as the literal path does,
        // and an entry the walk cannot read is an error, not a silent omission.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
        std::fs::create_dir_all(root.join("conf/locked")).unwrap();
        std::fs::write(root.join("conf/a.toml"), "").unwrap();
        std::os::unix::fs::symlink("a.toml", root.join("conf/link.toml")).unwrap();
        assert_eq!(expand("conf/link.toml", &root).unwrap(), ["conf/link.toml"]);
        assert_eq!(
            expand("conf/*.toml", &root).unwrap(),
            ["conf/a.toml", "conf/link.toml"]
        );
        std::fs::write(root.join("conf/locked/b.toml"), "").unwrap();
        let locked = root.join("conf/locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = expand("conf/**/*.toml", &root);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let error = result.unwrap_err();
        assert!(
            error.contains("cannot read") && error.contains("conf/locked"),
            "{error}"
        );
        assert_eq!(
            expand("missing/*.toml", &root).unwrap_err(),
            "matches no file"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_a_glob_refuses_a_dangling_link_it_matches() {
        // N9 residual (a): a matching dangling link is refused by name, as the
        // literal path is; one the glob does not match gives no file either way.
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
        std::fs::create_dir_all(root.join("conf")).unwrap();
        std::fs::write(root.join("conf/a.toml"), "").unwrap();
        std::os::unix::fs::symlink("gone.toml", root.join("conf/dangling.toml")).unwrap();
        std::os::unix::fs::symlink("gone", root.join("conf/stale")).unwrap();
        assert_eq!(
            expand("conf/dangling.toml", &root).unwrap_err(),
            "matches no file"
        );
        let error = expand("conf/*.toml", &root).unwrap_err();
        assert!(
            error.contains("`conf/dangling.toml` is a dangling symbolic link"),
            "{error}"
        );
        assert_eq!(expand("conf/a*", &root).unwrap(), ["conf/a.toml"]);
    }

    #[cfg(unix)]
    #[test]
    fn test_a_glob_descends_a_symlinked_directory_as_it_follows_a_symlinked_base() {
        // N9 residual (b) (Fable g4, g5): the walk follows a symbolic link to a
        // directory below the glob's base, as it follows one that is the base.
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
        std::fs::create_dir_all(root.join("sdk/real/deep")).unwrap();
        std::fs::write(root.join("sdk/real/deep/d.toml"), "").unwrap();
        std::os::unix::fs::symlink("real", root.join("sdk/linkdir")).unwrap();
        assert_eq!(
            expand("sdk/linkdir/**/*.toml", &root).unwrap(),
            ["sdk/linkdir/deep/d.toml"]
        );
        assert_eq!(
            expand("sdk/**/*.toml", &root).unwrap(),
            ["sdk/linkdir/deep/d.toml", "sdk/real/deep/d.toml"]
        );
        // A link back up is a loop, refused by name rather than walked forever.
        std::os::unix::fs::symlink("..", root.join("sdk/real/up")).unwrap();
        let error = expand("sdk/**/*.toml", &root).unwrap_err();
        assert!(error.contains("cannot read `sdk/"), "{error}");
        std::fs::remove_file(root.join("sdk/real/up")).unwrap();
        // A dangling link the glob matches is still refused; one it does not
        // match is still passed over.
        std::os::unix::fs::symlink("gone", root.join("sdk/real/stale")).unwrap();
        assert_eq!(
            expand("sdk/real/**/*.toml", &root).unwrap(),
            ["sdk/real/deep/d.toml"]
        );
        std::os::unix::fs::symlink("gone.toml", root.join("sdk/real/x.toml")).unwrap();
        let error = expand("sdk/real/**/*.toml", &root).unwrap_err();
        assert!(
            error.contains("`sdk/real/x.toml` is a dangling symbolic link"),
            "{error}"
        );
    }

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
