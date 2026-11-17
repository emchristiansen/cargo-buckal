//! Compile-time file includes that a first-party package's own `glob(["**/**"])`
//! does not provide.
//!
//! `include_str!`, `include_bytes!` and `include!` resolve a relative path against
//! the including file. A first-party crate compiles its package's files at their
//! package-relative paths, so an included file is present only if the package glob
//! matches it. It does not when the file
//! - lies in a nested Buck package (a subdirectory with its own BUCK file), which a
//!   glob does not enter; such a file can be declared through `mapped_srcs`, or
//! - lies outside the package directory (`../`), which no package-relative
//!   destination can express.

use std::collections::BTreeSet;

use cargo_metadata::camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use regex::Regex;

/// A literal include found in a package source file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct UnprovidedInclude {
    /// The including file, relative to the package directory.
    pub from: String,
    /// The included path as written.
    pub written: String,
    /// Where it lands, relative to the package directory (may start with `../`).
    pub resolved: String,
    pub kind: IncludeGap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum IncludeGap {
    /// Inside the package directory, but in a nested Buck package; declarable
    /// through `mapped_srcs`.
    NestedPackage,
    /// Outside the package directory.
    OutsidePackage,
}

/// Find literal includes in `package_dir`'s Rust sources that the package glob does
/// not provide.
pub(crate) fn unprovided_includes(package_dir: &Utf8Path) -> BTreeSet<UnprovidedInclude> {
    let pattern = Regex::new(r#"include(?:_str|_bytes)?!\s*\(\s*"([^"\\]+)"\s*\)"#)
        .expect("valid include regex");
    let mut found = BTreeSet::new();
    for entry in package_files(package_dir) {
        if entry.extension() != Some("rs") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&entry) else {
            continue;
        };
        let from = entry
            .strip_prefix(package_dir)
            .expect("walked below the package")
            .to_owned();
        for capture in pattern.captures_iter(&text) {
            let written = capture[1].to_owned();
            if Utf8Path::new(&written).is_absolute() {
                continue;
            }
            let joined = from.parent().unwrap_or(Utf8Path::new("")).join(&written);
            let resolved = normalize(&joined);
            let kind = if resolved.starts_with("..") {
                IncludeGap::OutsidePackage
            } else if in_nested_package(package_dir, &resolved) {
                IncludeGap::NestedPackage
            } else {
                continue;
            };
            found.insert(UnprovidedInclude {
                from: from.to_string(),
                written,
                resolved: resolved.to_string(),
                kind,
            });
        }
    }
    found
}

/// Files below `package_dir` that belong to its Buck package: not in `target/`,
/// hidden directories or nested Buck packages.
fn package_files(package_dir: &Utf8Path) -> Vec<Utf8PathBuf> {
    walkdir::WalkDir::new(package_dir)
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() == 0 || !entry.file_type().is_dir() {
                return true;
            }
            let name = entry.file_name().to_string_lossy();
            !(name == "target" || name.starts_with('.') || entry.path().join("BUCK").exists())
        })
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| Utf8PathBuf::from_path_buf(entry.into_path()).ok())
        .collect()
}

/// Whether `relative` (inside the package) is in a directory below the package that
/// has its own BUCK file.
fn in_nested_package(package_dir: &Utf8Path, relative: &Utf8Path) -> bool {
    relative
        .ancestors()
        .skip(1)
        .filter(|dir| !dir.as_str().is_empty())
        .any(|dir| package_dir.join(dir).join("BUCK").exists())
}

/// Lexically normalize a relative path (`a/b/../c` -> `a/c`), keeping leading `..`.
fn normalize(path: &Utf8Path) -> Utf8PathBuf {
    let mut parts: Vec<&str> = Vec::new();
    for component in path.components() {
        match component {
            Utf8Component::CurDir => {}
            Utf8Component::ParentDir => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else {
                    parts.push("..");
                }
            }
            Utf8Component::Normal(part) => parts.push(part),
            _ => {}
        }
    }
    parts.iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Utf8Path, rel: &str, content: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn test_normalize() {
        assert_eq!(
            normalize(Utf8Path::new("src/x/../../vocab/a.toml")),
            "vocab/a.toml"
        );
        assert_eq!(
            normalize(Utf8Path::new("src/max/../../../lean/p.txt")),
            "../lean/p.txt"
        );
        assert_eq!(normalize(Utf8Path::new("./a/./b")), "a/b");
    }

    #[test]
    fn test_unprovided_includes() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = Utf8PathBuf::from_path_buf(dir.path().join("text/glyph_vocab")).unwrap();
        // Nested Buck package inside the crate directory (as text/glyph_vocab/vocab).
        write(&pkg, "vocab/BUCK", "");
        write(&pkg, "vocab/bpe-52.toml", "");
        write(&pkg, "guidance/note.md", "");
        write(
            &pkg,
            "src/internal/sources.rs",
            r#"
            const A: &str = include_str!("../../vocab/bpe-52.toml");
            const B: &str = include_str!( "../../guidance/note.md" );
            const C: &[u8] = include_bytes!("../../../../lean/generated-index-path.txt");
            const D: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/x"));
            "#,
        );
        // Files of the nested package and of target/ are not the crate's sources.
        write(&pkg, "vocab/ignored.rs", r#"include_str!("../../nowhere")"#);
        write(&pkg, "target/x.rs", r#"include_str!("../../nowhere")"#);

        let found = unprovided_includes(&pkg);
        let expected: BTreeSet<_> = [
            UnprovidedInclude {
                from: "src/internal/sources.rs".to_owned(),
                written: "../../../../lean/generated-index-path.txt".to_owned(),
                resolved: "../../lean/generated-index-path.txt".to_owned(),
                kind: IncludeGap::OutsidePackage,
            },
            UnprovidedInclude {
                from: "src/internal/sources.rs".to_owned(),
                written: "../../vocab/bpe-52.toml".to_owned(),
                resolved: "vocab/bpe-52.toml".to_owned(),
                kind: IncludeGap::NestedPackage,
            },
        ]
        .into();
        assert_eq!(found, expected);
    }
}
