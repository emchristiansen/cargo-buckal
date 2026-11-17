//! End-to-end generation: the real `cargo-buckal` binary runs `cargo buckal migrate`
//! on a fixture workspace, and the tests read the BUCK files it writes.
//!
//! Buck2 itself is not run. A stand-in `buck2` (configured as `buck2_binary` in a
//! private HOME) answers only what generation asks of it: `--help` and
//! `root --kind project`. Cargo resolves offline; the fixture has path dependencies only.

use std::path::{Path, PathBuf};
use std::process::Command;

struct Fixture {
    _dir: tempfile::TempDir,
    ws: PathBuf,
    home: PathBuf,
}

/// A fixture that keeps another one's directory alive (its stand-in `buck2`).
struct Kept {
    fixture: Fixture,
    _keepalive: Fixture,
}

impl std::ops::Deref for Kept {
    type Target = Fixture;
    fn deref(&self) -> &Fixture {
        &self.fixture
    }
}

impl Fixture {
    fn with_keepalive(self, other: Fixture) -> Kept {
        Kept {
            fixture: self,
            _keepalive: other,
        }
    }
}

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn manifest(name: &str, body: &str) -> String {
    format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{body}")
}

const BUCKAL_TOML: &str = r#"ignore_tests = false

[feature_platforms]
linux-x86_64 = "x86_64-unknown-linux-gnu"
wasm32 = "wasm32-unknown-unknown"

[fixups.extra]
rustc_flags = ["-C", "opt-level=2", "-C", "debuginfo=1"]
"#;

/// A workspace that exercises Cargo's per-context and per-kind activation:
/// - `app` uses `shared` normally with `target-feature`, and its build script uses
///   `shared` with `host-feature`, which alone enables `shared`'s optional `extra`
///   (the review's resolver-2 fixture);
/// - `sdk`'s build script always uses `extra`, while its library uses `extra` and a
///   renamed `extra2` (`renamed`) only through `browser`, which `app` enables on
///   wasm32 only;
/// - `app` has a dependency declared only for `cfg(target_arch = "wasm32")`.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let home = dir.path().join("home");
    write(
        &ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\", \"shared\", \"extra\", \"extra2\", \"sdk\", \"wasmonly\"]\n",
    );
    write(
        &ws.join("app/Cargo.toml"),
        &manifest(
            "app",
            r#"[dependencies]
shared = { path = "../shared", features = ["target-feature"] }
sdk = { path = "../sdk" }
[build-dependencies]
shared = { path = "../shared", features = ["host-feature"] }
[target.'cfg(target_arch = "wasm32")'.dependencies]
sdk = { path = "../sdk", features = ["browser"] }
wasmonly = { path = "../wasmonly" }
"#,
        ),
    );
    write(&ws.join("app/build.rs"), "fn main() {}\n");
    write(
        &ws.join("shared/Cargo.toml"),
        &manifest(
            "shared",
            r#"[features]
target-feature = []
host-feature = ["dep:extra"]
[dependencies]
extra = { path = "../extra", optional = true }
"#,
        ),
    );
    write(
        &ws.join("sdk/Cargo.toml"),
        &manifest(
            "sdk",
            r#"[features]
browser = ["dep:extra", "dep:renamed"]
[dependencies]
extra = { path = "../extra", optional = true }
renamed = { package = "extra2", path = "../extra2", optional = true }
[build-dependencies]
extra = { path = "../extra" }
"#,
        ),
    );
    write(&ws.join("sdk/build.rs"), "fn main() {}\n");
    for name in ["extra", "extra2", "wasmonly"] {
        write(&ws.join(name).join("Cargo.toml"), &manifest(name, ""));
    }
    for name in ["app", "shared", "extra", "extra2", "sdk", "wasmonly"] {
        write(&ws.join(name).join("src/lib.rs"), "");
    }
    write(&ws.join(".buckconfig"), "[cells]\nroot = .\n");
    write(&ws.join("buckal.toml"), BUCKAL_TOML);

    let shim = dir.path().join("bin/buck2");
    write(
        &shim,
        "#!/bin/sh\n\
         case \"$1\" in\n\
         --help|--version) exit 0 ;;\n\
         root) d=$PWD; while [ \"$d\" != / ]; do [ -f \"$d/.buckconfig\" ] && { echo \"$d\"; exit 0; }; d=$(dirname \"$d\"); done; exit 1 ;;\n\
         *) echo \"generation-only buck2 stand-in: $*\" >&2; exit 2 ;;\n\
         esac\n",
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    write(
        &home.join(".config/buckal/config.toml"),
        &format!("buck2_binary = {:?}\n", shim.to_str().unwrap()),
    );
    Fixture {
        _dir: dir,
        ws,
        home,
    }
}

impl Fixture {
    fn migrate(&self, args: &[&str]) {
        let mut all = vec!["migrate"];
        all.extend_from_slice(args);
        self.run(&all);
    }

    fn run(&self, args: &[&str]) {
        let output = self.output(args);
        assert!(
            output.status.success(),
            "cargo buckal {args:?} exited with {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn output(&self, args: &[&str]) -> std::process::Output {
        let cargo_home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cargo")
            });
        Command::new(env!("CARGO_BIN_EXE_cargo-buckal"))
            .arg("buckal")
            .args(args)
            .current_dir(&self.ws)
            .env("HOME", &self.home)
            .env("CARGO_HOME", cargo_home)
            .env("CARGO_NET_OFFLINE", "true")
            .output()
            .expect("run cargo-buckal")
    }

    fn buck(&self, package: &str) -> String {
        std::fs::read_to_string(self.ws.join(package).join("BUCK")).unwrap()
    }
}

/// The text of the rule named `name` in `content`, from its opening line to the
/// closing `)` at the start of a line.
fn rule<'a>(content: &'a str, name: &str) -> &'a str {
    let marker = format!("    name = \"{name}\",\n");
    let at = content
        .find(&marker)
        .unwrap_or_else(|| panic!("no rule `{name}` in:\n{content}"));
    let start = content[..at].rfind("\n").map_or(0, |i| i + 1);
    let end = at + content[at..].find("\n)\n").expect("rule end") + 3;
    &content[start..end]
}

#[test]
fn test_host_and_target_contexts_through_generation() {
    let f = fixture();
    f.migrate(&[]);

    // R1: the target `shared` has only the target feature and no `extra`; the host
    // variant, which app's build script uses, has the host feature and `extra`.
    let shared = f.buck("shared");
    let target = rule(&shared, "shared");
    assert!(target.contains("\"target-feature\""), "{target}");
    assert!(!target.contains("host-feature"), "{target}");
    assert!(!target.contains("//extra:extra"), "{target}");
    let host = rule(&shared, "shared-host");
    assert!(host.contains("\"host-feature\""), "{host}");
    assert!(!host.contains("target-feature"), "{host}");
    assert!(host.contains("\"//extra:extra\""), "{host}");

    let app = f.buck("app");
    let app_lib = rule(&app, "app");
    assert!(app_lib.contains("\"//shared:shared\""), "{app_lib}");
    assert!(!app_lib.contains("shared-host"), "{app_lib}");
    let app_build = rule(&app, "build-script-build");
    assert!(
        app_build.contains("\"//shared:shared-host\""),
        "{app_build}"
    );
    assert!(!app_build.contains("\"//shared:shared\""), "{app_build}");
}

#[test]
fn test_edges_keep_their_kind_and_platform_through_generation() {
    let f = fixture();
    f.migrate(&[]);
    let sdk = f.buck("sdk");

    // R3: natively the library has no `extra` (its edge is wasm32-only), while the
    // build script's edge to the same package is unconditional.
    let lib = rule(&sdk, "sdk");
    assert!(
        lib.contains("\"wasm32\": {\n            \"deps\": [\"//extra:extra\"],"),
        "{lib}"
    );
    assert!(!lib.contains("\"linux-x86_64\""), "{lib}");
    assert!(!lib.contains("    deps = [\"//extra:extra\"]"), "{lib}");
    let build = rule(&sdk, "build-script-build");
    assert!(build.contains("    deps = [\"//extra:extra\"],"), "{build}");
    // (It is compiled with the package's per-platform features, but has no
    // per-platform edges.)
    assert!(!build.contains("\"deps\": ["), "{build}");

    // R2: the renamed optional dependency is wasm32-only, under its alias.
    assert!(
        lib.contains(
            "            \"named_deps\": {\n                \"renamed\": \"//extra2:extra2\",\n            },"
        ),
        "{lib}"
    );
    assert!(!lib.contains("    named_deps"), "{lib}");

    // A dependency declared for `cfg(target_arch = "wasm32")` is kept on wasm32.
    let app = f.buck("app");
    let app_lib = rule(&app, "app");
    assert!(app_lib.contains("\"//wasmonly:wasmonly\""), "{app_lib}");
    assert!(!app_lib.contains("    deps = [\"//wasmonly"), "{app_lib}");
}

#[test]
fn test_cached_migrate_follows_fixups_and_old_caches() {
    let f = fixture();
    f.migrate(&[]);
    let lib = |f: &Fixture| rule(&f.buck("extra"), "extra").to_owned();
    // R6: ordered, with both `-C`.
    assert!(
        lib(&f).contains(
            "        \"-C\",\n        \"opt-level=2\",\n        \"-C\",\n        \"debuginfo=1\",\n"
        ),
        "{}",
        lib(&f)
    );

    // R4: change only the fixup; an ordinary (cached) migrate regenerates `extra`.
    write(
        &f.ws.join("buckal.toml"),
        &BUCKAL_TOML.replace("opt-level=2", "opt-level=3"),
    );
    let shared_before = f.buck("shared");
    f.migrate(&[]);
    assert!(lib(&f).contains("\"opt-level=3\""), "{}", lib(&f));
    assert!(!lib(&f).contains("opt-level=2"), "{}", lib(&f));
    assert_eq!(f.buck("shared"), shared_before);

    // Removing the fixup removes the flags.
    write(
        &f.ws.join("buckal.toml"),
        BUCKAL_TOML.split("[fixups.extra]").next().unwrap(),
    );
    f.migrate(&[]);
    assert!(!lib(&f).contains("opt-level"), "{}", lib(&f));

    // A cache written by an older generator (v0.1.4 wrote version 4; this one writes 6) is stale: the
    // rules are regenerated even though the packages did not change.
    let snap = f.ws.join("buckal.snap");
    let cache = std::fs::read_to_string(&snap).unwrap();
    assert!(cache.contains("version = 14"), "{cache}");
    std::fs::write(&snap, cache.replace("version = 14", "version = 4")).unwrap();
    let stale = "# stale rules from an older generator\n";
    write(&f.ws.join("extra/BUCK"), stale);
    f.migrate(&[]);
    assert_ne!(f.buck("extra"), stale);
    assert!(f.buck("extra").contains("name = \"extra\""));
}

#[test]
fn test_first_snapshot_needs_no_regeneration() {
    let f = fixture();
    f.migrate(&[]);
    let before = f.buck("app");
    assert!(
        rule(&before, "unittest").contains("    resources = glob([\"**/**\"]),"),
        "{before}"
    );
    // R5: the package's first snapshot appears; an ordinary migrate leaves the BUCK
    // file as it is, and the test already declares the file.
    write(&f.ws.join("app/src/snapshots/app__first.snap"), "---\n");
    f.migrate(&[]);
    assert_eq!(f.buck("app"), before);
}

/// A workspace proc-macro with an integration test and a dev-dependency:
/// `app -> pm (proc-macro) -> shared`, where pm really uses shared, and
/// `pm/tests/api.rs` uses `devonly`.
fn proc_macro_fixture(with_integration_test: bool) -> Kept {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let home = dir.path().join("home");
    write(
        &ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\", \"pm\", \"shared\", \"devonly\"]\n",
    );
    write(
        &ws.join("app/Cargo.toml"),
        &manifest("app", "[dependencies]\npm = { path = \"../pm\" }\n"),
    );
    write(&ws.join("app/src/lib.rs"), "pm::sample!();\n");
    write(
        &ws.join("pm/Cargo.toml"),
        &manifest(
            "pm",
            "[lib]\nproc-macro = true\n[dependencies]\nshared = { path = \"../shared\" }\n[dev-dependencies]\ndevonly = { path = \"../devonly\" }\n",
        ),
    );
    write(
        &ws.join("pm/src/lib.rs"),
        "extern crate proc_macro;\n#[proc_macro]\npub fn sample(_: proc_macro::TokenStream) -> proc_macro::TokenStream {\n    let _ = shared::value();\n    proc_macro::TokenStream::new()\n}\n",
    );
    if with_integration_test {
        write(
            &ws.join("pm/tests/api.rs"),
            "#[test]\nfn api() { assert_eq!(devonly::one(), 1); }\n",
        );
    }
    write(&ws.join("shared/Cargo.toml"), &manifest("shared", ""));
    write(
        &ws.join("shared/src/lib.rs"),
        "pub fn value() -> u32 { 1 }\n",
    );
    write(&ws.join("devonly/Cargo.toml"), &manifest("devonly", ""));
    write(
        &ws.join("devonly/src/lib.rs"),
        "pub fn one() -> u32 { 1 }\n",
    );
    write(&ws.join(".buckconfig"), "[cells]\nroot = .\n");
    write(
        &ws.join("buckal.toml"),
        BUCKAL_TOML.split("[fixups.extra]").next().unwrap(),
    );
    let base = fixture();
    // Reuse the stand-in buck2 and HOME of a regular fixture.
    std::fs::create_dir_all(&home).unwrap();
    copy_dir(&base.home, &home);
    Fixture {
        _dir: dir,
        ws,
        home,
    }
    .with_keepalive(base)
}

fn copy_dir(from: &Path, to: &Path) {
    for entry in walk(from) {
        let rel = entry.strip_prefix(from).unwrap();
        write(&to.join(rel), &std::fs::read_to_string(&entry).unwrap());
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[test]
fn test_proc_macro_with_integration_test_through_generation() {
    for with_integration_test in [true, false] {
        let f = proc_macro_fixture(with_integration_test);
        f.migrate(&[]);
        let pm = f.buck("pm");

        // The proc-macro library is the host compilation, with its real dependency;
        // there is no separate host variant, so every consumer reaches this rule.
        let lib = rule(&pm, "pm");
        assert!(lib.contains("    proc_macro = True,"), "{lib}");
        assert!(lib.contains("\"//shared:shared\""), "{lib}");
        assert!(!pm.contains("name = \"pm-host\""), "{pm}");
        let app = f.buck("app");
        assert!(rule(&app, "app").contains("\"//pm:pm\""), "{app}");

        if with_integration_test {
            // The integration test keeps its own target-context edges (the
            // dev-dependency) and reaches the same proc-macro rule.
            let api = rule(&pm, "api");
            assert!(api.contains("\":pm\""), "{api}");
            assert!(api.contains("\"//devonly:devonly\""), "{api}");
            assert!(api.contains("\"//shared:shared\""), "{api}");
        }
    }
}

#[test]
fn test_update_after_an_old_cache_regenerates_before_certifying() {
    // No feature_platforms: the plain metadata path.
    let f = fixture();
    write(&f.ws.join("buckal.toml"), "ignore_tests = false\n");
    f.migrate(&[]);
    let current = f.buck("extra");

    // Rules and cache from an older generator (cache version 4).
    let snap = f.ws.join("buckal.snap");
    let cache = std::fs::read_to_string(&snap).unwrap();
    std::fs::write(&snap, cache.replace("version = 14", "version = 4")).unwrap();
    // Stale generated rules (hand-written content would be kept).
    let stale = "# @generated by `cargo buckal`\n\nrust_library(\n    name = \"extra\",\n    srcs = [\"stale.rs\"],\n)\n";
    write(&f.ws.join("extra/BUCK"), stale);

    // A no-op update must regenerate the surviving packages before it writes a
    // version 5 cache...
    f.run(&["update", "--workspace"]);
    assert!(
        std::fs::read_to_string(&snap)
            .unwrap()
            .contains("version = 14")
    );
    assert_eq!(f.buck("extra"), current);
    // ...so that an ordinary migrate afterwards finds current rules.
    f.migrate(&[]);
    assert_eq!(f.buck("extra"), current);
}

/// The `rustc_flags` of a rule's text, in order.
fn rustc_flags(rule: &str) -> Vec<String> {
    let start =
        rule.find("    rustc_flags = [").expect("rustc_flags") + "    rustc_flags = [".len();
    let mut flags = Vec::new();
    let mut chars = rule[start..].chars();
    while let Some(c) = chars.next() {
        match c {
            ']' => break,
            '"' => flags.push(chars.by_ref().take_while(|&c| c != '"').collect()),
            _ => {}
        }
    }
    flags
}

#[test]
fn test_merge_keeps_manual_flag_groups_and_drops_old_fixup_flags() {
    let f = fixture();
    let config =
        |fixup: &str| format!("ignore_tests = false\npatch_fields = [\"rustc_flags\"]\n{fixup}");
    let manifest_flag = "@$(location :manifest[env_flags])";

    // A manually written rule with its own `-C` group, merged with a generated fixup
    // `-C` group (no cache yet).
    write(
        &f.ws.join("buckal.toml"),
        &config("[fixups.extra]\nrustc_flags = [\"-C\", \"opt-level=2\"]\n"),
    );
    write(
        &f.ws.join("extra/BUCK"),
        "rust_library(\n    name = \"extra\",\n    srcs = [],\n    crate = \"extra\",\n    crate_root = \"src/lib.rs\",\n    edition = \"2021\",\n    rustc_flags = [\"-C\", \"debuginfo=1\"],\n)\n",
    );
    f.migrate(&["--no-cache", "--merge"]);
    assert_eq!(
        rustc_flags(rule(&f.buck("extra"), "extra")),
        [manifest_flag, "-C", "opt-level=2", "-C", "debuginfo=1"]
    );

    // The fixup changes: the old fixup group is not taken for a manual edit.
    write(
        &f.ws.join("buckal.toml"),
        &config("[fixups.extra]\nrustc_flags = [\"-C\", \"opt-level=3\"]\n"),
    );
    f.migrate(&["--merge"]);
    assert_eq!(
        rustc_flags(rule(&f.buck("extra"), "extra")),
        [manifest_flag, "-C", "opt-level=3", "-C", "debuginfo=1"]
    );

    // The fixup goes away: only the manual group stays.
    write(&f.ws.join("buckal.toml"), &config(""));
    f.migrate(&["--merge"]);
    assert_eq!(
        rustc_flags(rule(&f.buck("extra"), "extra")),
        [manifest_flag, "-C", "debuginfo=1"]
    );
}

#[test]
fn test_merge_keeps_ordered_manual_groups_equal_to_generated_ones() {
    // R6-DUP-1 (A, B, A): the generated fixup `-C opt-level=2` is followed by manual
    // `-C opt-level=3 -C opt-level=2`, so the effective option is opt-level=2.
    let f = fixture();
    let config = |env: &str| {
        format!(
            "ignore_tests = false\npatch_fields = [\"rustc_flags\"]\n[fixups.extra]\nrustc_flags = [\"-C\", \"opt-level=2\"]\n{env}"
        )
    };
    let manifest_flag = "@$(location :manifest[env_flags])";
    write(&f.ws.join("buckal.toml"), &config(""));
    f.migrate(&[]);
    let buck = f.buck("extra");
    assert_eq!(
        rustc_flags(rule(&buck, "extra")),
        [manifest_flag, "-C", "opt-level=2"]
    );
    let edited = buck.replacen(
        "\"opt-level=2\"",
        "\"opt-level=2\", \"-C\", \"opt-level=3\", \"-C\", \"opt-level=2\"",
        1,
    );
    assert_ne!(edited, buck);
    write(&f.ws.join("extra/BUCK"), &edited);

    // Another fixup field changes, so `extra` is regenerated with the same flags.
    write(
        &f.ws.join("buckal.toml"),
        &config("env = { EXTRA = \"1\" }\n"),
    );
    f.migrate(&["--merge"]);
    let extra = f.buck("extra");
    assert!(extra.contains("EXTRA"), "{extra}");
    assert_eq!(
        rustc_flags(rule(&extra, "extra")),
        [
            manifest_flag,
            "-C",
            "opt-level=2",
            "-C",
            "opt-level=3",
            "-C",
            "opt-level=2"
        ]
    );
}

#[test]
fn test_no_cache_keeps_the_record_of_generated_fixup_flags() {
    // R6-NOCACHE-1: `--no-cache` regenerates every package, but a valid cache still
    // records the fixup flags the previous generation emitted.
    let f = fixture();
    let config = |fixup: &str| {
        format!("ignore_tests = false\npatch_fields = [\"rustc_flags\"]\n[fixups.extra]\n{fixup}")
    };
    let flags = |opt: &str| format!("rustc_flags = [\"-C\", \"opt-level={opt}\"]\n");
    let manifest_flag = "@$(location :manifest[env_flags])";
    let extra_flags = || rustc_flags(rule(&f.buck("extra"), "extra"));

    write(&f.ws.join("buckal.toml"), &config(&flags("2")));
    f.migrate(&[]);
    assert_eq!(extra_flags(), [manifest_flag, "-C", "opt-level=2"]);

    // Change under `--no-cache`: only the new group.
    write(&f.ws.join("buckal.toml"), &config(&flags("3")));
    f.migrate(&["--no-cache", "--merge"]);
    assert_eq!(extra_flags(), [manifest_flag, "-C", "opt-level=3"]);

    // Then removal in an ordinary run: neither group.
    write(
        &f.ws.join("buckal.toml"),
        &config("env = { EXTRA = \"1\" }\n"),
    );
    f.migrate(&["--merge"]);
    assert_eq!(extra_flags(), [manifest_flag]);

    // Removal under `--no-cache`, then an ordinary regeneration: neither group.
    write(&f.ws.join("buckal.toml"), &config(&flags("2")));
    f.migrate(&["--merge"]);
    assert_eq!(extra_flags(), [manifest_flag, "-C", "opt-level=2"]);
    write(&f.ws.join("buckal.toml"), &config(""));
    f.migrate(&["--no-cache", "--merge"]);
    assert_eq!(extra_flags(), [manifest_flag]);
    write(
        &f.ws.join("buckal.toml"),
        &config("env = { EXTRA = \"2\" }\n"),
    );
    f.migrate(&["--merge"]);
    assert_eq!(extra_flags(), [manifest_flag]);
}

/// Make `itoa` 0.4.8 and 1.0.0 available offline to the fixture, from a directory
/// source replacing crates.io, and have `consumer` use both.
fn two_itoa_versions(f: &Fixture) {
    let registry = f.ws.parent().unwrap().join("registry");
    // A directory source gives Cargo.lock the `package` checksum its
    // `.cargo-checksum.json` states; generation emits it as the archive's sha256.
    // These are fixture values, not the real crates' checksums: nothing is fetched.
    for (n, version) in ["0.4.8", "1.0.0"].into_iter().enumerate() {
        let dir = registry.join(format!("itoa-{version}"));
        write(
            &dir.join("Cargo.toml"),
            &format!("[package]\nname = \"itoa\"\nversion = \"{version}\"\nedition = \"2015\"\n"),
        );
        write(&dir.join("src/lib.rs"), "");
        write(
            &dir.join(".cargo-checksum.json"),
            &format!("{{\"files\":{{}},\"package\":\"{:064x}\"}}", n + 1),
        );
    }
    write(
        &f.ws.join(".cargo/config.toml"),
        &format!(
            "[source.crates-io]\nreplace-with = \"fixture\"\n[source.fixture]\ndirectory = {:?}\n",
            registry.to_str().unwrap()
        ),
    );
    write(
        &f.ws.join("consumer/Cargo.toml"),
        &manifest(
            "consumer",
            "[dependencies]\nold = { package = \"itoa\", version = \"0.4\" }\nnew = { package = \"itoa\", version = \"1.0\" }\n",
        ),
    );
    write(&f.ws.join("consumer/src/lib.rs"), "");
}

#[test]
fn test_targeted_patch_regenerates_unrelated_dirty_packages() {
    // R4-PATCH-1: `extra`'s fixup changes after the last generation, then a
    // targeted `patch itoa` (for `consumer`) saves a cache that certifies every
    // package. `extra` must be regenerated then, or stay dirty for the next run.
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"extra\", \"consumer\"]\n",
    );
    two_itoa_versions(&f);
    let config = |opt: &str| {
        format!(
            "ignore_tests = false\npatch_fields = [\"rustc_flags\"]\n[fixups.extra]\nrustc_flags = [\"-C\", \"opt-level={opt}\"]\n"
        )
    };
    let manifest_flag = "@$(location :manifest[env_flags])";
    let extra_flags = || rustc_flags(rule(&f.buck("extra"), "extra"));
    write(&f.ws.join("buckal.toml"), &config("2"));
    f.migrate(&[]);
    assert_eq!(extra_flags(), [manifest_flag, "-C", "opt-level=2"]);

    write(&f.ws.join("buckal.toml"), &config("3"));
    f.run(&["patch", "itoa"]);
    let patched = std::fs::read_to_string(f.ws.join("buckal.toml")).unwrap();
    assert!(patched.contains("opt-level=3"), "{patched}");
    assert_eq!(extra_flags(), [manifest_flag, "-C", "opt-level=3"]);

    f.migrate(&["--merge"]);
    assert_eq!(extra_flags(), [manifest_flag, "-C", "opt-level=3"]);
}

#[test]
fn test_fixup_test_execution_through_generation() {
    let f = fixture();
    f.migrate(&[]);
    let app_before = f.buck("app");
    let shared_before = f.buck("shared");
    assert!(!app_before.contains("remote_execution"), "{app_before}");

    // Adding the fixup to an existing configuration: an ordinary (cached) migrate
    // gives every test of `app` the profile and run-time environment.
    let with = |profile: &str| {
        format!(
            "{BUCKAL_TOML}\n[fixups.app]\ntest_remote_execution = \"{profile}\"\n\
             test_run_env = {{ FIXTURE_ROOT = \"/srv/fixtures\" }}\n"
        )
    };
    write(&f.ws.join("buckal.toml"), &with("fixture_fs_profile"));
    f.migrate(&[]);
    let app = f.buck("app");
    let test = rule(&app, "unittest");
    assert!(
        test.contains("    remote_execution = \"fixture_fs_profile\",\n"),
        "{test}"
    );
    assert!(
        test.contains("    run_env = {\n        \"FIXTURE_ROOT\": \"/srv/fixtures\",\n    },\n"),
        "{test}"
    );
    // Only tests: the library and the build script rules are unchanged.
    assert_eq!(rule(&app, "app"), rule(&app_before, "app"));
    assert_eq!(app.matches("remote_execution").count(), 1, "{app}");
    assert_eq!(app.matches("run_env").count(), 1, "{app}");
    // Only this package.
    assert_eq!(f.buck("shared"), shared_before);

    // Changing only the profile regenerates; removing the fixup restores the rules.
    write(&f.ws.join("buckal.toml"), &with("other_profile"));
    f.migrate(&[]);
    let app = f.buck("app");
    assert!(
        app.contains("remote_execution = \"other_profile\""),
        "{app}"
    );
    assert!(!app.contains("fixture_fs_profile"), "{app}");
    write(&f.ws.join("buckal.toml"), BUCKAL_TOML);
    f.migrate(&[]);
    assert_eq!(f.buck("app"), app_before);
}

#[test]
fn test_fixup_test_runtime_tools_through_generation() {
    let f = fixture();
    f.migrate(&[]);
    let app_before = f.buck("app");
    let shared_before = f.buck("shared");
    assert!(!app_before.contains("runtime_tools"), "{app_before}");

    // Adding the fixup: an ordinary (cached) migrate gives every test of `app` the
    // runtime tools dependency, and nothing else changes.
    let with =
        |label: &str| format!("{BUCKAL_TOML}\n[fixups.app]\ntest_runtime_tools = \"{label}\"\n");
    write(&f.ws.join("buckal.toml"), &with("//tools:runtime_tools"));
    f.migrate(&[]);
    let app = f.buck("app");
    let test = rule(&app, "unittest");
    assert!(
        test.contains("    runtime_tools = \"//tools:runtime_tools\",\n"),
        "{test}"
    );
    assert_eq!(app.matches("runtime_tools =").count(), 1, "{app}");
    assert_eq!(rule(&app, "app"), rule(&app_before, "app"));
    assert_eq!(
        app.replace("    runtime_tools = \"//tools:runtime_tools\",\n", ""),
        app_before
    );
    assert_eq!(f.buck("shared"), shared_before);

    // Changing only the label regenerates; removing the fixup restores the rules.
    write(&f.ws.join("buckal.toml"), &with("//tools:other_tools"));
    f.migrate(&[]);
    let app = f.buck("app");
    assert!(
        app.contains("runtime_tools = \"//tools:other_tools\""),
        "{app}"
    );
    assert!(!app.contains("//tools:runtime_tools"), "{app}");
    write(&f.ws.join("buckal.toml"), BUCKAL_TOML);
    f.migrate(&[]);
    assert_eq!(f.buck("app"), app_before);
}

#[test]
fn test_fixup_export_files_through_generation() {
    let f = fixture();
    f.migrate(&[]);
    let app_before = f.buck("app");
    let shared_before = f.buck("shared");
    assert!(!app_before.contains("export_file("), "{app_before}");
    let export = |path: &str| {
        format!(
            "export_file(\n    name = \"{path}\",\n    src = \"{path}\",\n    mode = \"reference\",\n    labels = [\"buckal-export-files\"],\n    visibility = [\"PUBLIC\"],\n)\n"
        )
    };
    let with = |paths: &str| format!("{BUCKAL_TOML}\n[fixups.app]\nexport_files = [{paths}]\n");

    // Adding the fixup: an ordinary (cached) migrate exports the file from `app`'s
    // BUCK file, and nothing else changes.
    write(&f.ws.join("buckal.toml"), &with("\"Cargo.toml\""));
    f.migrate(&[]);
    let app = f.buck("app");
    assert!(app.contains(&export("Cargo.toml")), "{app}");
    assert_eq!(app.matches("export_file(").count(), 1, "{app}");
    assert_eq!(
        app.replace(&export("Cargo.toml"), "").trim_end(),
        app_before.trim_end()
    );
    assert_eq!(f.buck("shared"), shared_before);

    // The exports are read back from the existing BUCK file and survive a migrate
    // that has nothing to regenerate.
    f.migrate(&[]);
    assert_eq!(f.buck("app"), app);

    // Changing the paths regenerates with exactly the new exports.
    write(
        &f.ws.join("buckal.toml"),
        &with("\"build.rs\", \"src/lib.rs\""),
    );
    f.migrate(&[]);
    let app = f.buck("app");
    assert!(app.contains(&export("build.rs")), "{app}");
    assert!(app.contains(&export("src/lib.rs")), "{app}");
    assert!(!app.contains("\"Cargo.toml\""), "{app}");
    assert_eq!(app.matches("export_file(").count(), 2, "{app}");

    // An entry may narrow the visibility; changing only the visibility regenerates.
    let narrowed = |visibility: &str| {
        with(&format!(
            "\"build.rs\", {{ path = \"src/lib.rs\", visibility = [{visibility}] }}"
        ))
    };
    write(&f.ws.join("buckal.toml"), &narrowed("\"//shared:\""));
    f.migrate(&[]);
    let app = f.buck("app");
    assert!(app.contains(&export("build.rs")), "{app}");
    assert!(
        app.contains("export_file(\n    name = \"src/lib.rs\",\n    src = \"src/lib.rs\",\n    mode = \"reference\",\n    labels = [\"buckal-export-files\"],\n    visibility = [\"//shared:\"],\n)\n"),
        "{app}"
    );
    write(
        &f.ws.join("buckal.toml"),
        &narrowed("\"//shared:\", \"//sdk:\""),
    );
    f.migrate(&[]);
    let app = f.buck("app");
    assert!(
        app.contains("    visibility = [\n        \"//sdk:\",\n        \"//shared:\",\n    ],\n"),
        "{app}"
    );
    // A specified empty visibility is private, not `PUBLIC`.
    write(&f.ws.join("buckal.toml"), &narrowed(""));
    f.migrate(&[]);
    let app = f.buck("app");
    assert!(
        app.contains("    name = \"src/lib.rs\",\n    src = \"src/lib.rs\",\n    mode = \"reference\",\n    labels = [\"buckal-export-files\"],\n    visibility = [],\n"),
        "{app}"
    );

    // Removing the fixup restores the original BUCK file.
    write(&f.ws.join("buckal.toml"), BUCKAL_TOML);
    f.migrate(&[]);
    assert_eq!(f.buck("app"), app_before);

    // A path outside the package, a missing file, a file of a nested Buck package
    // and a duplicate are refused, naming the package and the path.
    write(&f.ws.join("app/nested/BUCK"), "");
    write(&f.ws.join("app/nested/data.txt"), "");
    for (paths, path) in [
        ("\"../shared/Cargo.toml\"", "../shared/Cargo.toml"),
        ("\"./Cargo.toml\"", "./Cargo.toml"),
        ("\"/etc/hostname\"", "/etc/hostname"),
        ("\"missing.txt\"", "missing.txt"),
        ("\"src\"", "src"),
        ("\"nested/data.txt\"", "nested/data.txt"),
        ("\"build.rs\", \"build.rs\"", "build.rs"),
    ] {
        write(&f.ws.join("buckal.toml"), &with(paths));
        let output = f.output(&["migrate"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{paths} was accepted:\n{stderr}");
        assert!(
            stderr.contains(&format!("`[fixups.app] export_files`: `{path}`")),
            "{paths}:\n{stderr}"
        );
        assert_eq!(f.buck("app"), app_before, "{paths}");
    }
}

#[test]
fn test_mapped_srcs_repository_destinations_through_generation() {
    let f = fixture();
    // `app` includes a file of its nested Buck package `vocab` and, through `../`, a
    // file of the `shared` package.
    write(
        &f.ws.join("app/vocab/BUCK"),
        "export_file(name = \"words.txt\")\n",
    );
    write(&f.ws.join("app/vocab/words.txt"), "");
    write(&f.ws.join("shared/data.txt"), "");
    write(
        &f.ws.join("app/src/lib.rs"),
        "pub const W: &str = include_str!(\"../vocab/words.txt\");\n\
         pub const D: &str = include_str!(\"../../shared/data.txt\");\n",
    );
    let output = f.output(&["migrate"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    // The scanner suggests the entries that declare both includes.
    assert!(
        stderr.contains("Package `app` includes 2 file(s) that its sources do not contain (1 from nested Buck packages, 1 from outside its directory)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("`\"//app/vocab:words.txt\" = \"vocab/words.txt\"`"),
        "{stderr}"
    );
    assert!(
        stderr.contains("`\"//shared:data.txt\" = \"../shared/data.txt\"`"),
        "{stderr}"
    );
    let app_before = f.buck("app");
    assert!(!app_before.contains("mapped_srcs"), "{app_before}");

    // Declared as written relative to the package, the destinations are emitted as
    // repository paths, and the owning package exports the file.
    let with = |entries: &str| {
        format!(
            "{BUCKAL_TOML}\n[fixups.shared]\nexport_files = [\"data.txt\"]\n\n[fixups.app.mapped_srcs]\n{entries}"
        )
    };
    let declared = "\"//app/vocab:words.txt\" = \"vocab/words.txt\"\n\"//shared:data.txt\" = \"../shared/data.txt\"\n";
    write(&f.ws.join("buckal.toml"), &with(declared));
    let output = f.output(&["migrate"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(!stderr.contains("Package `app` includes"), "{stderr}");
    let app = f.buck("app");
    let mapped = "    mapped_srcs = {\n        \"//app/vocab:words.txt\": \"app/vocab/words.txt\",\n        \"//shared:data.txt\": \"shared/data.txt\",\n    },\n";
    for name in ["app", "unittest"] {
        assert!(rule(&app, name).contains(mapped), "{}", rule(&app, name));
    }
    // Every rule that gets them (the build script's too, as before) has exactly
    // these repository destinations.
    assert_eq!(
        app.matches("mapped_srcs = {").count(),
        app.matches(mapped).count(),
        "{app}"
    );
    assert!(
        f.buck("shared")
            .contains("export_file(\n    name = \"data.txt\","),
        "{}",
        f.buck("shared")
    );

    // Destinations that are absolute, leave the repository, land on a file of the
    // package's own sources or collide are refused, and the BUCK file is kept.
    for (entries, why) in [
        ("\"//x:a\" = \"/etc/hostname\"\n", "is not a relative path"),
        (
            "\"//x:a\" = \"../../outside.txt\"\n",
            "leaves the repository",
        ),
        (
            "\"//x:a\" = \"src/lib.rs\"\n",
            "is a file of the package's own sources",
        ),
        (
            "\"//x:a\" = \"../shared/data.txt\"\n\"//x:b\" = \"./../shared/data.txt\"\n",
            "`shared/data.txt` is also the destination of `//x:a`",
        ),
    ] {
        write(&f.ws.join("buckal.toml"), &with(entries));
        let output = f.output(&["migrate"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "{entries} was accepted:\n{stderr}"
        );
        assert!(
            stderr.contains("`[fixups.app] mapped_srcs`: `//x:") && stderr.contains(why),
            "{entries}:\n{stderr}"
        );
        assert_eq!(f.buck("app"), app, "{entries}");
    }

    // Removing the declarations restores the original BUCK file.
    write(&f.ws.join("buckal.toml"), BUCKAL_TOML);
    f.migrate(&[]);
    assert_eq!(f.buck("app"), app_before);
}

#[test]
fn test_hand_written_buck_content_is_a_fixed_point() {
    let f = fixture();
    // `[workspace.package]` makes generation own the workspace manifest's export in
    // the (virtual) workspace-root BUCK file, which is otherwise hand-written.
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\", \"shared\", \"extra\", \"extra2\", \"sdk\", \"wasmonly\"]\n\n[workspace.package]\nedition = \"2021\"\n",
    );
    let root = "load(\"@prelude//rules.bzl\", \"export_file\")\n\
load(\"//buck2:workspace_inputs.bzl\", \"declare_buildscript_workspace_inputs\")\n\
load(\"//buck2:source_tree_inputs.bzl\", \"declare_source_tree_inputs\")\n\
\n\
declare_buildscript_workspace_inputs()\n\
\n\
# The root package's files of each declared source tree.\n\
declare_source_tree_inputs()\n\
\n\
export_file(\n    name = \"clippy.toml\",\n    src = \"clippy.toml\",\n    visibility = [\"toolchains//...\"],\n)\n";
    write(&f.ws.join("BUCK"), root);
    // A first-party package BUCK file with hand-written content of its own.
    f.migrate(&[]);
    let app_generated = f.buck("app");
    let app_hand = "load(\"//buck2:checks.bzl\", \"declare_checks\")\n";
    let app_call = "# Checks of the package's sources.\ndeclare_checks(name = \"checks\")";
    let app_with_hand =
        app_generated.replacen("\n\nfilegroup(", &format!("\n{app_hand}\nfilegroup("), 1)
            + &format!("\n{app_call}\n");
    assert_ne!(app_with_hand, app_generated);
    write(&f.ws.join("app/BUCK"), &app_with_hand);

    // Two forced re-emissions: the root BUCK file keeps everything and gains only the
    // workspace export, appended once; the package keeps its hand-written load and
    // call; both are unchanged by the second run.
    f.migrate(&["--no-cache"]);
    let root_once = std::fs::read_to_string(f.ws.join("BUCK")).unwrap();
    assert_eq!(
        root_once,
        format!(
            "{root}\nexport_file(\n    name = \"workspace\",\n    src = \"Cargo.toml\",\n    mode = \"reference\",\n    visibility = [\"PUBLIC\"],\n)\n"
        )
    );
    let app_once = f.buck("app");
    assert!(app_once.contains(app_hand), "{app_once}");
    assert!(
        app_once.ends_with(&format!(")\n\n{app_call}\n")),
        "{app_once}"
    );
    f.migrate(&["--no-cache"]);
    assert_eq!(
        std::fs::read_to_string(f.ws.join("BUCK")).unwrap(),
        root_once
    );
    assert_eq!(f.buck("app"), app_once);
    // An ordinary cached migrate leaves them too.
    f.migrate(&[]);
    assert_eq!(
        std::fs::read_to_string(f.ws.join("BUCK")).unwrap(),
        root_once
    );
    assert_eq!(f.buck("app"), app_once);

    // The workspace export is replaced where it stands, not moved.
    let moved = root_once.replacen(
        "declare_buildscript_workspace_inputs()\n",
        "declare_buildscript_workspace_inputs()\n\nexport_file(name = \"workspace\", src = \"Cargo.toml\")\n",
        1,
    );
    let moved = moved[..moved
        .rfind("\nexport_file(\n    name = \"workspace\"")
        .unwrap()]
        .to_owned();
    write(&f.ws.join("BUCK"), &moved);
    f.migrate(&["--no-cache"]);
    let root_twice = std::fs::read_to_string(f.ws.join("BUCK")).unwrap();
    assert_eq!(
        root_twice,
        moved.replacen(
            "export_file(name = \"workspace\", src = \"Cargo.toml\")",
            "export_file(\n    name = \"workspace\",\n    src = \"Cargo.toml\",\n    mode = \"reference\",\n    visibility = [\"PUBLIC\"],\n)",
            1
        )
    );
    f.migrate(&["--no-cache"]);
    assert_eq!(
        std::fs::read_to_string(f.ws.join("BUCK")).unwrap(),
        root_twice
    );
}

#[test]
fn test_workspace_manifests_through_generation() {
    let f = fixture();
    f.migrate(&[]);
    let app_before = f.buck("app");
    let shared_before = f.buck("shared");
    assert!(!f.ws.join("BUCK").exists());

    // Opting in: an ordinary cached migrate exports every package's manifest to
    // the root package and writes the root filegroup of all manifests.
    let enabled = format!("workspace_manifests = true\n{BUCKAL_TOML}");
    write(&f.ws.join("buckal.toml"), &enabled);
    f.migrate(&[]);
    let export = "export_file(\n    name = \"Cargo.toml\",\n    src = \"Cargo.toml\",\n    mode = \"reference\",\n    visibility = [\"//:\"],\n)\n";
    let shared = f.buck("shared");
    assert_eq!(shared, format!("{shared_before}\n{export}"));
    let root = std::fs::read_to_string(f.ws.join("BUCK")).unwrap();
    let mut expected = String::from(
        "# Partly generated by `cargo buckal`: it rewrites every rust_library, rust_binary, rust_test, buildscript_run and cargo_manifest rule, its own filegroup and export_file rules and their loads, and keeps all other content.\n\nfilegroup(\n    name = \"workspace_manifests\",\n    srcs = {\n        \"Cargo.lock\": \"Cargo.lock\",\n        \"Cargo.toml\": \"Cargo.toml\",\n",
    );
    for member in ["app", "extra", "extra2", "sdk", "shared", "wasmonly"] {
        expected.push_str(&format!(
            "        \"{member}/Cargo.toml\": \"//{member}:Cargo.toml\",\n"
        ));
    }
    expected.push_str("    },\n    visibility = [\"PUBLIC\"],\n)\n");
    assert_eq!(root, expected);

    // A fixed point, cached or not.
    f.migrate(&[]);
    f.migrate(&["--no-cache"]);
    assert_eq!(std::fs::read_to_string(f.ws.join("BUCK")).unwrap(), root);
    assert_eq!(f.buck("shared"), shared);

    // An `export_files` entry for the manifest keeps its own visibility and gains
    // the root package's.
    write(
        &f.ws.join("buckal.toml"),
        &format!(
            "{enabled}\n[fixups.app]\nexport_files = [{{ path = \"Cargo.toml\", visibility = [\"//sdk:\"] }}]\n"
        ),
    );
    f.migrate(&[]);
    let app = f.buck("app");
    assert!(
        app.contains("    name = \"Cargo.toml\",\n    src = \"Cargo.toml\",\n    mode = \"reference\",\n    labels = [\"buckal-export-files\"],\n    visibility = [\n        \"//:\",\n        \"//sdk:\",\n    ],\n"),
        "{app}"
    );
    assert_eq!(app.matches("export_file(").count(), 1, "{app}");

    // Opting out removes the exports and the filegroup.
    write(&f.ws.join("buckal.toml"), BUCKAL_TOML);
    f.migrate(&[]);
    assert_eq!(f.buck("app"), app_before);
    assert_eq!(f.buck("shared"), shared_before);
    let root = std::fs::read_to_string(f.ws.join("BUCK")).unwrap();
    assert!(!root.contains("workspace_manifests"), "{root}");
}

#[test]
fn test_fixup_test_labels_and_compatibility_through_generation() {
    let f = fixture();
    f.migrate(&[]);
    let app_before = f.buck("app");
    let shared_before = f.buck("shared");
    let cross = "select({\"//platforms:cross\": [\"config//:none\"], \"DEFAULT\": []})";
    assert!(
        rule(&app_before, "unittest").contains(&format!("    target_compatible_with = {cross},\n")),
        "{app_before}"
    );

    // Package-wide labels, and a single test's labels and compatibility, which the
    // cross-compilation selector is concatenated to.
    let config = format!(
        "{BUCKAL_TOML}\n[fixups.app]\ntest_labels = [\"gate-default-excluded\"]\ntest_args = [\"--declared-skip-substring\", \"slow\"]\n\n[fixups.app.tests.unittest]\nlabels = [\"live-effect\"]\ntarget_compatible_with = [\"//buck2:gate_excluded\"]\nargs = [\"--declared-skip\", \"tests::known_absent\"]\n\n[fixups.shared]\ntest_target_compatible_with = [\"//buck2:gate_excluded\"]\n"
    );
    write(&f.ws.join("buckal.toml"), &config);
    f.migrate(&[]);
    let app = f.buck("app");
    let test = rule(&app, "unittest");
    assert!(
        test.contains(&format!(
            "    target_compatible_with = [\"//buck2:gate_excluded\"] + {cross},\n"
        )),
        "{test}"
    );
    assert!(
        test.contains(
            "    labels = [\n        \"gate-default-excluded\",\n        \"live-effect\",\n    ],\n"
        ),
        "{test}"
    );
    // Case filters as runner arguments: the package's, then the test's, in order.
    assert!(
        test.contains("    args = [\n        \"--declared-skip-substring\",\n        \"slow\",\n        \"--declared-skip\",\n        \"tests::known_absent\",\n    ],\n"),
        "{test}"
    );
    assert_eq!(rule(&app, "app"), rule(&app_before, "app"));
    let shared = f.buck("shared");
    assert!(
        rule(&shared, "unittest").contains(&format!(
            "    target_compatible_with = [\"//buck2:gate_excluded\"] + {cross},\n"
        )),
        "{shared}"
    );
    assert!(!shared.contains("labels ="), "{shared}");
    assert!(!shared.contains("args ="), "{shared}");
    // Read back and re-emitted unchanged.
    f.migrate(&["--no-cache"]);
    assert_eq!(f.buck("app"), app);
    assert_eq!(f.buck("shared"), shared);

    // A test name that is no test of the package is refused.
    write(
        &f.ws.join("buckal.toml"),
        &format!("{config}\n[fixups.app.tests.unitest]\nlabels = [\"x\"]\n"),
    );
    let output = f.output(&["migrate"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains(
            "`[fixups.app.tests.unitest]` names no test of the package (its tests: unittest)"
        ),
        "{stderr}"
    );
    assert_eq!(f.buck("app"), app);

    // Removing the fixups restores the rules.
    write(&f.ws.join("buckal.toml"), BUCKAL_TOML);
    f.migrate(&[]);
    assert_eq!(f.buck("app"), app_before);
    assert_eq!(f.buck("shared"), shared_before);
}

#[test]
fn test_fixup_extra_deps_and_buildscript_through_generation() {
    let f = fixture();
    f.migrate(&[]);
    let sdk_before = f.buck("sdk");
    let app_before = f.buck("app");

    // Reindeer-shaped: library `extra_deps`, `[buildscript.build]` and
    // `[buildscript.run]`, and `buildscript.run = true`, which changes nothing.
    let config = format!(
        "{BUCKAL_TOML}\n[fixups.app]\nbuildscript.run = true\n\n[fixups.sdk]\nextra_deps = [\"toolchains//:alsa\"]\n\n[fixups.sdk.buildscript.build]\nextra_srcs = [\"src/probe/*.rs\"]\nextra_deps = [\"//tools:probe\"]\n\n[fixups.sdk.buildscript.run]\nrustc_link_lib = true\nrustc_link_search = true\n"
    );
    write(&f.ws.join("buckal.toml"), &config);
    f.migrate(&[]);
    assert_eq!(f.buck("app"), app_before);
    let sdk = f.buck("sdk");
    let lib = rule(&sdk, "sdk");
    assert!(lib.contains("\"toolchains//:alsa\""), "{lib}");
    let build = rule(&sdk, "build-script-build");
    assert!(build.contains("\"//tools:probe\""), "{build}");
    assert!(!build.contains("toolchains//:alsa"), "{build}");
    let run = rule(&sdk, "build-script-run");
    assert!(
        run.contains("    rustc_link_lib = True,\n    rustc_link_search = True,\n"),
        "{run}"
    );
    assert!(!rule(&sdk, "unittest").contains("toolchains//:alsa"));
    f.migrate(&["--no-cache"]);
    assert_eq!(f.buck("sdk"), sdk);

    // Not running the build script, and an extra source outside the package, are
    // refused.
    for (fixup, why) in [
        (
            "[fixups.sdk]\nbuildscript.run = false\n",
            "`[fixups.sdk] buildscript.run = false`: generation always runs",
        ),
        (
            "[fixups.sdk.buildscript.build]\nextra_srcs = [\"../app/build.rs\"]\n",
            "`[fixups.sdk.buildscript.build] extra_srcs`: `../app/build.rs` is not a relative pattern inside the package",
        ),
    ] {
        write(
            &f.ws.join("buckal.toml"),
            &format!("{BUCKAL_TOML}\n{fixup}"),
        );
        let output = f.output(&["migrate"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{fixup}:\n{stderr}");
        assert!(stderr.contains(why), "{fixup}:\n{stderr}");
    }

    // Removing the fixups restores the rules.
    write(&f.ws.join("buckal.toml"), BUCKAL_TOML);
    f.migrate(&[]);
    assert_eq!(f.buck("sdk"), sdk_before);
}

/// A workspace whose tests need features the ordinary build does not enable: `selfy`
/// enables its own `test_lib` through a dev-dependency on itself and has a binary
/// that requires it; `gated` has a `test_lib` feature only `--features test_lib`
/// enables; `user` depends on both normally.
fn test_features_fixture(test_features: Option<&str>) -> Fixture {
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"selfy\", \"gated\", \"user\"]\n",
    );
    write(
        &f.ws.join("selfy/Cargo.toml"),
        &manifest(
            "selfy",
            "[features]\ntest_lib = []\nnope = []\n[dev-dependencies]\nselfy = { path = \".\", features = [\"test_lib\"] }\n[[bin]]\nname = \"tool\"\npath = \"src/bin/tool.rs\"\nrequired-features = [\"test_lib\"]\n[[bin]]\nname = \"never\"\npath = \"src/bin/never.rs\"\nrequired-features = [\"nope\"]\n",
        ),
    );
    write(
        &f.ws.join("selfy/src/lib.rs"),
        "#[cfg(feature = \"test_lib\")]\npub mod test_lib {}\n",
    );
    write(&f.ws.join("selfy/src/bin/tool.rs"), "fn main() {}\n");
    write(&f.ws.join("selfy/src/bin/never.rs"), "fn main() {}\n");
    write(
        &f.ws.join("selfy/tests/t.rs"),
        "use selfy::test_lib as _;\n",
    );
    write(
        &f.ws.join("gated/Cargo.toml"),
        &manifest("gated", "[features]\ntest_lib = []\n"),
    );
    write(&f.ws.join("gated/src/lib.rs"), "");
    write(
        &f.ws.join("user/Cargo.toml"),
        &manifest(
            "user",
            "[dependencies]\nselfy = { path = \"../selfy\" }\ngated = { path = \"../gated\" }\n",
        ),
    );
    write(&f.ws.join("user/src/lib.rs"), "");
    let key = test_features
        .map(|features| format!("test_features = [{features}]\n"))
        .unwrap_or_default();
    write(
        &f.ws.join("buckal.toml"),
        &format!("skip_dev_dependency_cycles = true\n{key}{BUCKAL_TOML}"),
    );
    f
}

#[test]
fn test_test_features_resolve_tests_apart() {
    // Without `test_features`, production is still exactly `cargo build`'s: the self
    // dev-dependency's feature reaches only the tests, through the test variant, and
    // `gated`'s `test_lib`, which only `--features` enables, is nowhere.
    let f = test_features_fixture(None);
    f.migrate(&[]);
    let selfy = f.buck("selfy");
    assert!(!rule(&selfy, "selfy").contains("features"), "{selfy}");
    assert!(
        rule(&selfy, "selfy-test").contains("    features = [\"test_lib\"],\n"),
        "{selfy}"
    );
    // A binary whose required feature nothing enables is not built, as under Cargo.
    assert!(!selfy.contains("never"), "{selfy}");
    let gated = f.buck("gated");
    assert!(!gated.contains("test_lib"), "{gated}");
    assert!(!gated.contains("gated-test"), "{gated}");

    // With it, the tests also get the workspace's `--features test_lib`.
    let f = test_features_fixture(Some("\"test_lib\""));
    f.migrate(&[]);
    let selfy = f.buck("selfy");
    let lib = rule(&selfy, "selfy");
    assert!(!lib.contains("features"), "{lib}");
    let variant = rule(&selfy, "selfy-test");
    assert!(
        variant.contains("    features = [\"test_lib\"],\n"),
        "{variant}"
    );
    assert!(variant.contains("    crate = \"selfy\",\n"), "{variant}");
    for test in ["unittest", "t"] {
        let test = rule(&selfy, test);
        assert!(test.contains("    features = [\"test_lib\"],\n"), "{test}");
        assert!(!test.contains("\":selfy\""), "{test}");
    }
    assert!(
        rule(&selfy, "t").contains("    deps = [\":selfy-test\"],\n"),
        "{selfy}"
    );
    // The binary that requires `test_lib` is built as the tests need it.
    let tool = rule(&selfy, "tool");
    assert!(tool.contains("    features = [\"test_lib\"],\n"), "{tool}");
    assert!(tool.contains("    deps = [\":selfy-test\"],\n"), "{tool}");

    // `--features test_lib` reaches `gated`'s tests and its test variant only.
    let gated = f.buck("gated");
    assert!(!rule(&gated, "gated").contains("features"), "{gated}");
    assert!(
        rule(&gated, "gated-test").contains("    features = [\"test_lib\"],\n"),
        "{gated}"
    );
    assert!(
        rule(&gated, "unittest").contains("    features = [\"test_lib\"],\n"),
        "{gated}"
    );

    // A dependent's library uses the ordinary libraries, its tests the variants.
    let user = f.buck("user");
    let lib = rule(&user, "user");
    assert!(lib.contains("\"//selfy:selfy\""), "{lib}");
    assert!(lib.contains("\"//gated:gated\""), "{lib}");
    let test = rule(&user, "unittest");
    assert!(test.contains("\"//selfy:selfy-test\""), "{test}");
    assert!(test.contains("\"//gated:gated-test\""), "{test}");

    // A fixed point.
    f.migrate(&["--no-cache"]);
    assert_eq!(f.buck("selfy"), selfy);
    assert_eq!(f.buck("user"), user);
}

#[test]
fn test_test_host_variants_through_generation() {
    // `dpm`, a proc-macro only the tests use (not a member), gives the host
    // `hostlib` feature `x`; `pm`, a proc-macro of the ordinary build, uses `hostlib`
    // without it. Cargo compiles the tests' `pm` and `hostlib` (and its build script)
    // apart; production stays as `cargo build` resolves it.
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\", \"pm\", \"hostlib\"]\nexclude = [\"dpm\"]\n",
    );
    write(
        &f.ws.join("app/Cargo.toml"),
        &manifest(
            "app",
            "[dependencies]\npm = { path = \"../pm\" }\n[dev-dependencies]\ndpm = { path = \"../dpm\" }\n",
        ),
    );
    write(&f.ws.join("app/src/lib.rs"), "");
    for (name, body) in [
        (
            "pm",
            "[lib]\nproc-macro = true\n[dependencies]\nhostlib = { path = \"../hostlib\" }\n",
        ),
        (
            "dpm",
            "[lib]\nproc-macro = true\n[dependencies]\nhostlib = { path = \"../hostlib\", features = [\"x\"] }\n",
        ),
        ("hostlib", "[features]\nx = []\n"),
    ] {
        write(&f.ws.join(name).join("Cargo.toml"), &manifest(name, body));
        write(&f.ws.join(name).join("src/lib.rs"), "");
    }
    write(&f.ws.join("hostlib/build.rs"), "fn main() {}\n");
    f.migrate(&[]);

    let hostlib = f.buck("hostlib");
    assert!(!rule(&hostlib, "hostlib").contains("features"), "{hostlib}");
    let variant = rule(&hostlib, "hostlib-test-host");
    assert!(variant.contains("    features = [\"x\"],\n"), "{variant}");
    assert!(
        variant.contains("build-script-test-host-run[out_dir]"),
        "{variant}"
    );
    assert!(
        rule(&hostlib, "build-script-test-host-run").contains("    features = [\"x\"],\n"),
        "{hostlib}"
    );
    rule(&hostlib, "build-script-test-host-build");

    let pm = f.buck("pm");
    assert!(rule(&pm, "pm").contains("\"//hostlib:hostlib\""), "{pm}");
    let pm_variant = rule(&pm, "pm-test-host");
    assert!(
        pm_variant.contains("\"//hostlib:hostlib-test-host\""),
        "{pm_variant}"
    );

    // The test-only proc-macro is emitted for the tests, against the tests' host.
    let dpm = f.buck("dpm");
    assert!(
        rule(&dpm, "dpm").contains("\"//hostlib:hostlib-test-host\""),
        "{dpm}"
    );

    // Production `app` uses production `pm`; its tests use the tests' `pm`.
    let app = f.buck("app");
    assert!(rule(&app, "app").contains("\"//pm:pm\""), "{app}");
    for name in ["unittest", "app-test"] {
        let r = rule(&app, name);
        assert!(r.contains("\"//pm:pm-test-host\""), "{r}");
        assert!(!r.contains("\"//pm:pm\""), "{r}");
    }
    assert!(rule(&app, "unittest").contains("\"//dpm:dpm\""), "{app}");

    // A fixed point.
    f.migrate(&["--no-cache"]);
    assert_eq!(f.buck("hostlib"), hostlib);
    assert_eq!(f.buck("app"), app);
}

/// The review's context fixture (t_800b7f5f CTX-UNIT-1, CTX-DEV-1): `app` uses
/// `shared` with `target-f` and the proc-macro `pm`, which uses `shared` with
/// `host-f`. `pm` has inline tests (using `shared`'s host-f-only API and the
/// unix-only dev-dependency `nativeonly`) and an integration test; `wasmonly` is a
/// wasm32-only dev-dependency.
fn pm_contexts_fixture(with_integration_test: bool) -> Kept {
    let f = proc_macro_fixture(with_integration_test);
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\", \"pm\", \"shared\", \"nativeonly\", \"wasmonly\"]\n",
    );
    write(
        &f.ws.join("app/Cargo.toml"),
        &manifest(
            "app",
            "[dependencies]\npm = { path = \"../pm\" }\nshared = { path = \"../shared\", features = [\"target-f\"] }\n",
        ),
    );
    write(
        &f.ws.join("pm/Cargo.toml"),
        &manifest(
            "pm",
            "[lib]\nproc-macro = true\n[dependencies]\nshared = { path = \"../shared\", features = [\"host-f\"] }\n\
             [target.'cfg(unix)'.dev-dependencies]\nnativeonly = { path = \"../nativeonly\" }\n\
             [target.'cfg(target_arch = \"wasm32\")'.dev-dependencies]\nwasmonly = { path = \"../wasmonly\" }\n",
        ),
    );
    write(
        &f.ws.join("pm/src/lib.rs"),
        "extern crate proc_macro;\n#[proc_macro]\npub fn sample(_: proc_macro::TokenStream) -> proc_macro::TokenStream {\n    shared::host();\n    proc_macro::TokenStream::new()\n}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn unit() {\n        shared::host();\n        nativeonly::value();\n    }\n}\n",
    );
    if with_integration_test {
        write(
            &f.ws.join("pm/tests/api.rs"),
            "#[test]\nfn api() {\n    pm::sample!();\n}\n",
        );
    }
    write(
        &f.ws.join("shared/Cargo.toml"),
        &manifest("shared", "[features]\nhost-f = []\ntarget-f = []\n"),
    );
    write(
        &f.ws.join("shared/src/lib.rs"),
        "#[cfg(all(feature = \"host-f\", not(feature = \"target-f\")))]\npub fn host() {}\n",
    );
    for name in ["nativeonly", "wasmonly"] {
        write(&f.ws.join(name).join("Cargo.toml"), &manifest(name, ""));
        write(&f.ws.join(name).join("src/lib.rs"), "pub fn value() {}\n");
    }
    f
}

/// The quoted labels of the list that starts at `list` (`[` ... `]`).
fn labels(list: &str) -> Vec<&str> {
    list[..list.find(']').unwrap()]
        .split('"')
        .skip(1)
        .step_by(2)
        .collect()
}

/// The labels of `platform[<platform>]["deps"]` in a rule.
fn platform_deps<'a>(rule: &'a str, platform: &str) -> Vec<&'a str> {
    let key = format!("\"{platform}\": {{");
    let Some(at) = rule.find(&key) else {
        return Vec::new();
    };
    let section = &rule[at..];
    let section = &section[..section.find('}').unwrap()];
    section
        .find("\"deps\": [")
        .map_or_else(Vec::new, |deps| labels(&section[deps + 9..]))
}

/// The labels of a rule's top-level `deps` list.
fn top_deps(rule: &str) -> Vec<&str> {
    rule.find("\n    deps = [")
        .map_or_else(Vec::new, |at| labels(&rule[at + 13..]))
}

#[test]
fn test_proc_macro_unit_tests_are_host_compilations() {
    for with_integration_test in [true, false] {
        let f = pm_contexts_fixture(with_integration_test);
        f.migrate(&[]);
        let pm = f.buck("pm");
        let shared = f.buck("shared");
        // Ordinary `shared` has both features; `shared-host` only `host-f`.
        assert!(rule(&shared, "shared").contains("target-f"), "{shared}");
        let host = rule(&shared, "shared-host");
        assert!(
            host.contains("host-f") && !host.contains("target-f"),
            "{host}"
        );
        assert!(rule(&pm, "pm").contains("\"//shared:shared-host\""), "{pm}");

        // CTX-UNIT-1: Cargo compiles pm's inline tests for the host, with the host
        // `shared` (`mode=test, kind=proc-macro, platform=null`), whether or not an
        // integration test (on the target) exists.
        let unittest = rule(&pm, "unittest");
        assert!(unittest.contains("\"//shared:shared-host\""), "{unittest}");
        assert!(!unittest.contains("\"//shared:shared\""), "{unittest}");
        // CTX-DEV-1: the host unit tests keep the host's unix-only dev-dependency on
        // every platform (the host is unix), and have no wasm32-only one.
        assert!(
            top_deps(unittest).contains(&"//nativeonly:nativeonly"),
            "{unittest}"
        );
        assert!(!unittest.contains("wasmonly"), "{unittest}");

        if with_integration_test {
            // CTX-DEV-1: the integration test is a target compilation: on wasm32 it
            // has `wasmonly` and not `nativeonly`; on linux the reverse.
            let api = rule(&pm, "api");
            assert!(
                !top_deps(api).iter().any(|d| d.contains("nativeonly")),
                "{api}"
            );
            assert!(
                !top_deps(api).iter().any(|d| d.contains("wasmonly")),
                "{api}"
            );
            let wasm = platform_deps(api, "wasm32");
            assert!(wasm.contains(&"//wasmonly:wasmonly"), "{api}");
            assert!(!wasm.iter().any(|d| d.contains("nativeonly")), "{api}");
            let linux = platform_deps(api, "linux-x86_64");
            assert!(linux.contains(&"//nativeonly:nativeonly"), "{api}");
            assert!(!linux.iter().any(|d| d.contains("wasmonly")), "{api}");
            assert!(api.contains("\":pm\""), "{api}");
        }
    }
}

#[test]
fn test_headers_claim_only_generated_rules() {
    // Generation owns only its own rules in the workspace-root BUCK file and in
    // first-party packages' BUCK files, so their header does not say `@generated`
    // (which claims the whole file). Third-party BUCK files are generated whole.
    let partly = "# Partly generated by `cargo buckal`: it rewrites every rust_library, rust_binary, rust_test, buildscript_run and cargo_manifest rule, its own filegroup and export_file rules and their loads, and keeps all other content.\n";
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"extra\", \"consumer\"]\n",
    );
    two_itoa_versions(&f);
    write(
        &f.ws.join("buckal.toml"),
        &format!("workspace_manifests = true\n{BUCKAL_TOML}"),
    );
    // A root BUCK file from an earlier generation, with hand-written calls.
    let hand = "load(\"//build:inputs.bzl\", \"declare_source_tree_inputs\")\n\n# Inputs of the source tree.\ndeclare_source_tree_inputs(name = \"sources\")\n";
    write(
        &f.ws.join("BUCK"),
        &format!("# @generated by `cargo buckal`\n\n{hand}"),
    );
    f.migrate(&[]);

    let root = std::fs::read_to_string(f.ws.join("BUCK")).unwrap();
    assert!(root.starts_with(&format!("{partly}\n{hand}")), "{root}");
    assert!(!root.contains("@generated"), "{root}");
    rule(&root, "workspace_manifests");

    // A first-party package's BUCK file, with a hand-written call kept.
    let call = "declare_buildscript_workspace_inputs(name = \"buildscript_inputs\")";
    let extra = f.buck("extra");
    assert!(extra.starts_with(partly), "{extra}");
    assert!(!extra.contains("@generated"), "{extra}");
    write(&f.ws.join("extra/BUCK"), &format!("{extra}\n{call}\n"));
    f.migrate(&["--no-cache"]);
    let extra = f.buck("extra");
    assert!(extra.starts_with(partly), "{extra}");
    assert!(!extra.contains("@generated"), "{extra}");
    assert!(extra.contains(call), "{extra}");

    // Third-party packages' BUCK files are generated whole.
    let third_party: Vec<PathBuf> = walk(&f.ws)
        .into_iter()
        .filter(|path| path.ends_with("BUCK") && path.to_str().unwrap().contains("itoa"))
        .collect();
    assert_eq!(third_party.len(), 2, "{third_party:?}");
    for path in third_party {
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.starts_with("# @generated by `cargo buckal`\n"),
            "{content}"
        );
    }

    // A fixed point.
    let root_before = std::fs::read_to_string(f.ws.join("BUCK")).unwrap();
    f.migrate(&["--no-cache"]);
    assert_eq!(
        std::fs::read_to_string(f.ws.join("BUCK")).unwrap(),
        root_before
    );
    assert_eq!(f.buck("extra"), extra);
}

/// A package with binaries: `tool` (same name as the library, with an inline
/// test), `quiet` (`test = false`), `gated` (required feature `off`, which nothing
/// enables) and `enabled` (required feature `on`, a default feature); a
/// dev-dependency `devonly`.
fn bins_fixture() -> Fixture {
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"tool\", \"devonly\"]\n",
    );
    write(
        &f.ws.join("tool/Cargo.toml"),
        &manifest(
            "tool",
            "[features]\ndefault = [\"on\"]\non = []\noff = []\n\
             [dev-dependencies]\ndevonly = { path = \"../devonly\" }\n\
             [[bin]]\nname = \"tool\"\npath = \"src/main.rs\"\n\
             [[bin]]\nname = \"quiet\"\npath = \"src/bin/quiet.rs\"\ntest = false\n\
             [[bin]]\nname = \"gated\"\npath = \"src/bin/gated.rs\"\nrequired-features = [\"off\"]\n\
             [[bin]]\nname = \"enabled\"\npath = \"src/bin/enabled.rs\"\nrequired-features = [\"on\"]\n",
        ),
    );
    write(&f.ws.join("tool/src/lib.rs"), "pub fn one() -> u32 { 1 }\n");
    write(
        &f.ws.join("tool/src/main.rs"),
        "fn main() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn inline() {\n        assert_eq!(tool::one(), devonly::one());\n    }\n}\n",
    );
    for bin in ["quiet", "gated", "enabled"] {
        write(
            &f.ws.join(format!("tool/src/bin/{bin}.rs")),
            "fn main() {}\n",
        );
    }
    write(&f.ws.join("devonly/Cargo.toml"), &manifest("devonly", ""));
    write(
        &f.ws.join("devonly/src/lib.rs"),
        "pub fn one() -> u32 { 1 }\n",
    );
    f
}

#[test]
fn test_binary_unit_tests_through_generation() {
    let f = bins_fixture();
    f.migrate(&[]);
    let tool = f.buck("tool");
    let unittest = rule(&tool, "tool-unittest");
    assert!(
        tool.contains("rust_test(\n    name = \"tool-unittest\",\n"),
        "{tool}"
    );
    assert!(
        unittest.contains("crate_root = \"src/main.rs\""),
        "{unittest}"
    );
    assert!(unittest.contains("crate = \"tool\""), "{unittest}");
    assert!(unittest.contains("\":tool-lib\""), "{unittest}");
    assert!(unittest.contains("\"//devonly:devonly\""), "{unittest}");
    assert!(!unittest.contains("cargo_bin_exes"), "{unittest}");
    rule(&tool, "enabled-unittest");
    for absent in ["quiet-unittest", "gated-unittest"] {
        assert!(
            !tool.contains(&format!("name = \"{absent}\"")),
            "{absent}:\n{tool}"
        );
    }
    // The binaries themselves do not see dev-dependencies.
    assert!(!rule(&tool, "tool").contains("devonly"), "{tool}");
}

#[test]
fn test_binary_unit_tests_match_cargo_unit_graph() {
    // Parity: the binaries whose unit tests Cargo builds for `cargo test
    // --workspace` (units with mode `test` and kind `bin`) are exactly those with a
    // `<bin>-unittest` rule.
    let f = bins_fixture();
    f.migrate(&[]);
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = Command::new(cargo)
        .args([
            "test",
            "--workspace",
            "--no-run",
            "--target",
            "x86_64-unknown-linux-gnu",
            "-Z",
            "unstable-options",
            "--unit-graph",
        ])
        .current_dir(&f.ws)
        .env("RUSTC_BOOTSTRAP", "1")
        .env("CARGO_NET_OFFLINE", "true")
        .output()
        .expect("run cargo");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let graph: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let from_cargo: std::collections::BTreeSet<String> = graph["units"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|u| u["mode"] == "test" && u["target"]["kind"][0] == "bin")
        .map(|u| u["target"]["name"].as_str().unwrap().to_owned())
        .collect();
    let tool = f.buck("tool");
    let emitted: std::collections::BTreeSet<String> = tool
        .lines()
        .filter_map(|line| line.strip_prefix("    name = \""))
        .filter_map(|name| name.strip_suffix("-unittest\","))
        .map(str::to_owned)
        .collect();
    assert_eq!(from_cargo, ["enabled", "tool"].map(str::to_owned).into());
    assert_eq!(emitted, from_cargo, "{tool}");
}

#[test]
fn test_omitted_same_named_binary_keeps_the_library_label() {
    // FEAT-BIN-LABEL-1 (t_7c72255f): `foo` has a library and a same-named binary
    // whose required feature `cli` nothing enables, so the binary is not emitted;
    // `app` depends on `foo`. The library rule and the label `app` uses must agree.
    for cli_for_tests in [false, true] {
        let f = fixture();
        write(
            &f.ws.join("Cargo.toml"),
            "[workspace]\nresolver = \"2\"\nmembers = [\"app\", \"foo\"]\n",
        );
        write(
            &f.ws.join("app/Cargo.toml"),
            &manifest("app", "[dependencies]\nfoo = { path = \"../foo\" }\n"),
        );
        write(&f.ws.join("app/src/lib.rs"), "");
        write(&f.ws.join("app/build.rs"), "fn main() {}\n");
        write(
            &f.ws.join("foo/Cargo.toml"),
            &manifest(
                "foo",
                "[features]\ncli = []\n[[bin]]\nname = \"foo\"\npath = \"src/main.rs\"\nrequired-features = [\"cli\"]\n",
            ),
        );
        write(&f.ws.join("foo/src/lib.rs"), "");
        write(&f.ws.join("foo/src/main.rs"), "fn main() {}\n");
        let config = BUCKAL_TOML.split("[fixups.extra]").next().unwrap();
        let config = if cli_for_tests {
            format!("test_features = [\"foo/cli\"]\n{config}")
        } else {
            config.to_owned()
        };
        write(&f.ws.join("buckal.toml"), &config);
        f.migrate(&[]);
        let foo = f.buck("foo");
        let app = f.buck("app");
        let names: Vec<&str> = foo
            .lines()
            .filter_map(|l| l.strip_prefix("    name = \""))
            .filter_map(|l| l.strip_suffix("\","))
            .collect();
        let labels: Vec<&str> = app
            .split('"')
            .filter_map(|s| s.strip_prefix("//foo:"))
            .collect();
        assert!(!labels.is_empty(), "{app}");
        for label in labels {
            assert!(
                names.contains(&label),
                "{label} not in {names:?}\n{foo}\n{app}"
            );
        }
        assert!(names.contains(&"foo-lib"), "{foo}");
        // Only the tests enable `cli`, and `foo` has no integration test: Cargo
        // builds the binary as its unit tests only (b807 r36 F1).
        assert!(!names.contains(&"foo"), "{foo}");
        assert_eq!(names.contains(&"foo-unittest"), cli_for_tests, "{foo}");
    }
}

#[test]
fn test_proc_macro_integration_test_uses_its_test_host_variant() {
    // FEAT-TEST-HOST-SELF-1 (t_7c72255f): only the test features enable the
    // proc-macro `pm`'s `testflag`, so Cargo compiles the integration test's `pm`
    // (on the host) with it and production `pm` without.
    let f = pm_contexts_fixture(true);
    let manifest_path = f.ws.join("pm/Cargo.toml");
    let pm_manifest = std::fs::read_to_string(&manifest_path).unwrap();
    write(
        &manifest_path,
        &format!("{pm_manifest}[features]\ntestflag = []\n"),
    );
    write(
        &f.ws.join("pm/src/lib.rs"),
        "extern crate proc_macro;\n#[proc_macro]\npub fn sample(_: proc_macro::TokenStream) -> proc_macro::TokenStream {\n    if cfg!(feature = \"testflag\") { \"fn enabled() {}\".parse().unwrap() } else { proc_macro::TokenStream::new() }\n}\n",
    );
    write(
        &f.ws.join("pm/tests/api.rs"),
        "pm::sample!();\n#[test]\nfn api() {\n    enabled();\n}\n",
    );
    let config = BUCKAL_TOML.split("[fixups.extra]").next().unwrap();
    write(
        &f.ws.join("buckal.toml"),
        &format!("test_features = [\"pm/testflag\"]\n{config}"),
    );
    f.migrate(&[]);
    let pm = f.buck("pm");
    assert!(!rule(&pm, "pm").contains("testflag"), "{pm}");
    assert!(rule(&pm, "pm-test-host").contains("testflag"), "{pm}");
    let api = rule(&pm, "api");
    assert!(top_deps(api).contains(&":pm-test-host"), "{api}");
    assert!(!top_deps(api).contains(&":pm"), "{api}");
}

/// Run `migrate`, which must refuse (mentioning `needle`) and leave `files` (paths
/// relative to the workspace) byte-identical.
fn assert_refused_unchanged(f: &Fixture, files: &[&str], needle: &str) {
    let before: Vec<String> = files
        .iter()
        .map(|p| std::fs::read_to_string(f.ws.join(p)).unwrap())
        .collect();
    let output = f.output(&["migrate"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains(needle), "{stderr}");
    for (path, before) in files.iter().zip(before) {
        assert_eq!(
            std::fs::read_to_string(f.ws.join(path)).unwrap(),
            before,
            "{path}"
        );
    }
}

#[test]
fn test_export_files_refuse_names_the_package_already_declares() {
    // EXP-1 (t_7c72255f): a kept hand-written macro call declares `data.txt`.
    let f = fixture();
    f.migrate(&[]);
    let app = f.buck("app");
    write(
        &f.ws.join("app/BUCK"),
        &format!("{app}\nload(\"//:custom.bzl\", \"custom\")\n\ncustom(name = \"data.txt\")\n"),
    );
    write(
        &f.ws.join("custom.bzl"),
        "def custom(name):\n    native.filegroup(name = name, srcs = [])\n",
    );
    write(&f.ws.join("app/data.txt"), "data\n");
    write(
        &f.ws.join("buckal.toml"),
        &format!("{BUCKAL_TOML}\n[fixups.app]\nexport_files = [\"data.txt\"]\n"),
    );
    assert_refused_unchanged(&f, &["app/BUCK"], "data.txt");

    // A file named like the generated library.
    write(&f.ws.join("app/BUCK"), &app);
    write(&f.ws.join("app/app"), "data\n");
    write(
        &f.ws.join("buckal.toml"),
        &format!("{BUCKAL_TOML}\n[fixups.app]\nexport_files = [\"app\"]\n"),
    );
    assert_refused_unchanged(&f, &["app/BUCK"], "app");
}

#[test]
fn test_workspace_manifests_refuses_a_root_target_of_that_name() {
    // WM-1 (t_7c72255f): a root package whose library is `workspace_manifests`.
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[package]\nname = \"workspace_manifests\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\nresolver = \"2\"\nmembers = [\"extra\"]\n",
    );
    write(&f.ws.join("src/lib.rs"), "");
    write(&f.ws.join("BUCK"), "# A hand-written root package file.\n");
    write(
        &f.ws.join("buckal.toml"),
        &format!("workspace_manifests = true\n{BUCKAL_TOML}"),
    );
    assert_refused_unchanged(&f, &["BUCK"], "workspace_manifests");

    // A hand-written macro of that name in a virtual workspace's root BUCK file.
    let f = fixture();
    write(
        &f.ws.join("BUCK"),
        "load(\"//:inputs.bzl\", \"declare_source_tree_inputs\")\n\ndeclare_source_tree_inputs(name = \"workspace_manifests\")\n",
    );
    write(
        &f.ws.join("buckal.toml"),
        &format!("workspace_manifests = true\n{BUCKAL_TOML}"),
    );
    assert_refused_unchanged(&f, &["BUCK"], "workspace_manifests");
}

#[test]
fn test_mapped_srcs_refuse_a_destination_above_the_package() {
    // R7-1 (t_7c72255f): package `b` at `a/b`; a destination `..` (`a`) would have
    // to be a file and the directory of `a/b`'s sources at once.
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"extra\", \"a/b\"]\n",
    );
    write(&f.ws.join("a/b/Cargo.toml"), &manifest("b", ""));
    write(&f.ws.join("a/b/src/lib.rs"), "");
    f.migrate(&[]);
    let config = |entry: &str| format!("{BUCKAL_TOML}\n[fixups.b.mapped_srcs]\n{entry}\n");
    write(
        &f.ws.join("buckal.toml"),
        &config("\"//:data.txt\" = \"..\""),
    );
    assert_refused_unchanged(&f, &["a/b/BUCK"], "is a directory above the package `a/b`");

    // A file beside the package, out of it, is fine.
    write(
        &f.ws.join("buckal.toml"),
        &config("\"//:data.txt\" = \"../data.txt\""),
    );
    f.migrate(&[]);
    let b = f.buck("a/b");
    assert!(b.contains("\"//:data.txt\": \"a/data.txt\""), "{b}");
}

#[test]
fn test_merge_drops_changed_and_removed_set_fixups_and_keeps_manual_values() {
    // CFG-1 (t_7c72255f): with `patch_fields` covering dependencies and
    // compatibility, a changed or removed fixup's earlier labels are not manual
    // edits under an ordinary cached `migrate --merge`; manual labels are.
    let f = fixture();
    write(&f.ws.join("extra/build.rs"), "fn main() {}\n");
    let config = |fixup: &str| {
        format!(
            "ignore_tests = false\npatch_fields = [\"target_compatible_with\", \"deps\"]\n[fixups.extra]\n{fixup}"
        )
    };
    let fixup = |v: &str| {
        format!(
            "test_target_compatible_with = [\"//c:{v}\"]\nextra_deps = [\"//n:{v}\"]\n\
             [fixups.extra.buildscript.build]\nextra_deps = [\"//n:b{v}\"]\n\
             [fixups.extra.tests.unittest]\ntarget_compatible_with = [\"//c:t{v}\"]\n"
        )
    };
    let labels = |rule: &str, of: &[&str]| -> Vec<String> {
        of.iter()
            .filter(|l| rule.contains(&format!("\"{l}\"")))
            .map(|l| l.to_string())
            .collect()
    };
    let all = [
        "//c:old",
        "//c:told",
        "//n:old",
        "//n:bold",
        "//c:new",
        "//c:tnew",
        "//n:new",
        "//n:bnew",
        "//manual:dep",
        "//manual:bdep",
        "//manual:c",
    ];
    let check = |expected: &[(&str, &[&str])]| {
        let buck = f.buck("extra");
        for (name, want) in expected {
            assert_eq!(labels(rule(&buck, name), &all), *want, "{name}:\n{buck}");
        }
    };

    write(&f.ws.join("buckal.toml"), &config(&fixup("old")));
    f.migrate(&["--merge"]);
    check(&[
        ("extra", &["//n:old"]),
        ("build-script-build", &["//n:bold"]),
        ("unittest", &["//c:old", "//c:told"]),
    ]);
    // Manual additions next to the generated labels.
    let buck = f
        .buck("extra")
        .replacen("\"//n:old\"", "\"//n:old\", \"//manual:dep\"", 1)
        .replacen("\"//n:bold\"", "\"//n:bold\", \"//manual:bdep\"", 1)
        .replacen("\"//c:old\"", "\"//c:old\", \"//manual:c\"", 1);
    write(&f.ws.join("extra/BUCK"), &buck);

    write(&f.ws.join("buckal.toml"), &config(&fixup("new")));
    f.migrate(&["--merge"]);
    check(&[
        ("extra", &["//n:new", "//manual:dep"]),
        ("build-script-build", &["//n:bnew", "//manual:bdep"]),
        ("unittest", &["//c:new", "//c:tnew", "//manual:c"]),
    ]);

    write(&f.ws.join("buckal.toml"), &config(""));
    f.migrate(&["--merge"]);
    check(&[
        ("extra", &["//manual:dep"]),
        ("build-script-build", &["//manual:bdep"]),
        ("unittest", &["//manual:c"]),
    ]);
}

#[test]
fn test_tests_consume_their_build_script_output() {
    // BIN-BS-1 (t_7c72255f): Cargo gives every compilation of a package (its
    // library, binaries and all their tests) the build script's output
    // (`cargo:rustc-env`, cfgs, link flags, OUT_DIR). With a test variant, the tests
    // use the test build script, compiled with the tests' features.
    for test_features in [false, true] {
        let f = fixture();
        write(
            &f.ws.join("Cargo.toml"),
            "[workspace]\nresolver = \"2\"\nmembers = [\"tool\"]\n",
        );
        write(
            &f.ws.join("tool/Cargo.toml"),
            &manifest(
                "tool",
                "[features]\nt = []\n[[bin]]\nname = \"app\"\npath = \"src/main.rs\"\n",
            ),
        );
        write(
            &f.ws.join("tool/build.rs"),
            "fn main() {\n    println!(\"cargo:rustc-env=BIN_BUILD_VALUE=correct\");\n}\n",
        );
        write(
            &f.ws.join("tool/src/lib.rs"),
            "#[test]\nfn lib_build_env() {\n    assert_eq!(env!(\"BIN_BUILD_VALUE\"), \"correct\");\n}\n",
        );
        write(
            &f.ws.join("tool/src/main.rs"),
            "fn main() {}\n#[test]\nfn build_env() {\n    assert_eq!(env!(\"BIN_BUILD_VALUE\"), \"correct\");\n}\n",
        );
        write(
            &f.ws.join("tool/tests/it.rs"),
            "#[test]\nfn it_build_env() {\n    assert_eq!(env!(\"BIN_BUILD_VALUE\"), \"correct\");\n}\n",
        );
        let config = BUCKAL_TOML.split("[fixups.extra]").next().unwrap();
        let config = if test_features {
            format!("test_features = [\"tool/t\"]\n{config}")
        } else {
            config.to_owned()
        };
        write(&f.ws.join("buckal.toml"), &config);
        f.migrate(&[]);
        let tool = f.buck("tool");
        let run = if test_features {
            "build-script-test-run"
        } else {
            "build-script-run"
        };
        for name in ["unittest", "app-unittest", "it"] {
            let r = rule(&tool, name);
            let flags = rustc_flags(r);
            let response = format!("@$(location :{run}[rustc_flags])");
            assert_eq!(
                flags.iter().filter(|f| **f == response).count(),
                1,
                "{name}:\n{r}"
            );
            assert!(
                !flags.iter().any(|f| f.contains("build-script")
                    && f.contains("[rustc_flags]")
                    && *f != response),
                "{name}:\n{r}"
            );
            assert!(
                r.contains(&format!("\"OUT_DIR\": \"$(location :{run}[out_dir])\"")),
                "{name}:\n{r}"
            );
        }
        // Production keeps the production build script.
        for name in ["tool", "app"] {
            let r = rule(&tool, name);
            assert!(
                rustc_flags(r).contains(&"@$(location :build-script-run[rustc_flags])".to_owned()),
                "{name}:\n{r}"
            );
        }
    }
}

#[test]
fn test_unit_tests_of_a_build_dependency_library_are_target_compilations() {
    // Control for F2 (Fable review): `hostlib` is a member that the ordinary build
    // compiles only for the host (as `app`'s build dependency). Cargo compiles its
    // unit tests for the target with the test features, here `hostlib/x`.
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\", \"hostlib\"]\n",
    );
    write(
        &f.ws.join("app/Cargo.toml"),
        &manifest(
            "app",
            "[build-dependencies]\nhostlib = { path = \"../hostlib\" }\n",
        ),
    );
    write(&f.ws.join("app/build.rs"), "fn main() {}\n");
    write(&f.ws.join("app/src/lib.rs"), "");
    write(
        &f.ws.join("hostlib/Cargo.toml"),
        &manifest("hostlib", "[features]\nx = []\n"),
    );
    write(&f.ws.join("hostlib/src/lib.rs"), "");
    let config = BUCKAL_TOML.split("[fixups.extra]").next().unwrap();
    write(
        &f.ws.join("buckal.toml"),
        &format!("test_features = [\"hostlib/x\"]\n{config}"),
    );
    f.migrate(&[]);
    let hostlib = f.buck("hostlib");
    assert!(rule(&hostlib, "unittest").contains("\"x\""), "{hostlib}");
    assert!(!rule(&hostlib, "hostlib").contains("\"x\""), "{hostlib}");
}

#[test]
fn test_a_host_only_library_the_tests_build_for_the_target_has_a_test_variant() {
    // F2 (Fable review): `shared` (not a member, so `cargo build --workspace`
    // builds it only where something uses it) is only a build dependency of `app` in
    // the ordinary build (a host compilation without features), and a
    // dev-dependency with feature `t`. Cargo compiles `app`'s tests against a target
    // `shared` with `t`; the tests must not use the production (host) `shared`.
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\"]\nexclude = [\"shared\"]\n",
    );
    write(
        &f.ws.join("app/Cargo.toml"),
        &manifest(
            "app",
            "[build-dependencies]\nshared = { path = \"../shared\" }\n[dev-dependencies]\nshared = { path = \"../shared\", features = [\"t\"] }\n",
        ),
    );
    write(&f.ws.join("app/build.rs"), "fn main() {}\n");
    write(&f.ws.join("app/src/lib.rs"), "");
    write(
        &f.ws.join("shared/Cargo.toml"),
        &manifest("shared", "[features]\nt = []\n"),
    );
    write(&f.ws.join("shared/src/lib.rs"), "");
    write(
        &f.ws.join("buckal.toml"),
        BUCKAL_TOML.split("[fixups.extra]").next().unwrap(),
    );
    f.migrate(&[]);
    let shared = f.buck("shared");
    let app = f.buck("app");
    assert!(!rule(&shared, "shared").contains("\"t\""), "{shared}");
    let unittest = rule(&app, "unittest");
    let label = top_deps(unittest)
        .into_iter()
        .find(|d| d.starts_with("//shared:"))
        .unwrap_or_else(|| panic!("{unittest}"));
    let name = label.strip_prefix("//shared:").unwrap();
    assert!(rule(&shared, name).contains("\"t\""), "{label}:\n{shared}");
    // The build script keeps the production host `shared`.
    assert!(
        rule(&app, "build-script-build").contains("\"//shared:shared\""),
        "{app}"
    );
}

#[test]
fn test_host_compilations_differing_by_platform_are_reported() {
    // F4 (Fable review): Cargo plans the host proc-macro `pm` with feature `f` for
    // linux and `w` for wasm32. Buck configures it in the execution platform, so it
    // cannot build the wasm32 resolution; generation reports it.
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\", \"pm\"]\n",
    );
    write(
        &f.ws.join("app/Cargo.toml"),
        &manifest(
            "app",
            "[target.'cfg(not(target_arch = \"wasm32\"))'.dependencies]\npm = { path = \"../pm\", features = [\"f\"] }\n\
             [target.'cfg(target_arch = \"wasm32\")'.dependencies]\npm = { path = \"../pm\", features = [\"w\"] }\n",
        ),
    );
    write(&f.ws.join("app/src/lib.rs"), "");
    write(
        &f.ws.join("pm/Cargo.toml"),
        &manifest(
            "pm",
            "[lib]\nproc-macro = true\n[features]\nf = []\nw = []\n",
        ),
    );
    write(&f.ws.join("pm/src/lib.rs"), "");
    write(
        &f.ws.join("buckal.toml"),
        BUCKAL_TOML.split("[fixups.extra]").next().unwrap(),
    );
    let output = f.output(&["migrate"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("resolve differently per configured platform: pm v0.1.0 (host-only)"),
        "{stderr}"
    );
    // The data stays keyed by the platform Cargo planned for.
    let pm = f.buck("pm");
    assert!(
        pm.contains("\"linux-x86_64\": {\n            \"features\": [\"f\"]"),
        "{pm}"
    );

    // A host compilation that resolves alike on every platform is not reported
    // (the list changes, which the record makes an explicit acceptance).
    write(
        &f.ws.join("app/Cargo.toml"),
        &manifest(
            "app",
            "[dependencies]\npm = { path = \"../pm\", features = [\"f\"] }\n",
        ),
    );
    let output = f.output(&["migrate", "--accept-host-differences"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(!stderr.contains("resolve differently"), "{stderr}");
}

#[test]
fn test_binary_unit_tests_get_cargo_bin_name() {
    // F1 (Fable review): Cargo sets `CARGO_BIN_NAME` (the binary's name, hyphens
    // kept) for a binary's unit-test compilation; `env!("CARGO_BIN_NAME")` in its
    // crate root needs it.
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"cli\"]\n",
    );
    write(
        &f.ws.join("cli/Cargo.toml"),
        &manifest(
            "cli",
            "[[bin]]\nname = \"my-cli\"\npath = \"src/main.rs\"\n",
        ),
    );
    write(
        &f.ws.join("cli/src/main.rs"),
        "const NAME: &str = env!(\"CARGO_BIN_NAME\");\nfn main() {\n    println!(\"{NAME}\");\n}\n#[test]\nfn name() {\n    assert_eq!(NAME, \"my-cli\");\n}\n",
    );
    write(
        &f.ws.join("buckal.toml"),
        BUCKAL_TOML.split("[fixups.extra]").next().unwrap(),
    );
    f.migrate(&[]);
    let cli = f.buck("cli");
    let unittest = rule(&cli, "my-cli-unittest");
    assert!(
        unittest.contains("\"CARGO_BIN_NAME\": \"my-cli\""),
        "{unittest}"
    );
    assert!(unittest.contains("crate = \"my_cli\""), "{unittest}");
    assert!(!unittest.contains("CARGO_BIN_EXE"), "{unittest}");
}

/// The packages (`//<pkg>:` labels' packages) a rule depends on, platform-specific
/// ones included.
fn dep_packages(rule: &str) -> std::collections::BTreeSet<String> {
    rule.split('"')
        .filter_map(|s| s.strip_prefix("//"))
        .filter_map(|s| s.split_once(':').map(|(p, _)| p.to_owned()))
        .filter(|p| !p.starts_with("platforms"))
        .collect()
}

#[test]
fn test_dev_edges_closing_package_cycles_stay_on_the_tests() {
    // b807 r30/r31 (Muninn): with `skip_dev_dependency_cycles`, a dev edge that
    // closes a cycle among packages was dropped, so the dependent's tests lost it.
    // A test is a rule apart from its package's library, so in Buck no rule depends
    // on itself through such an edge, and Cargo builds the tests with it.
    // - true back-edge: `metrics` dev-depends on `init`, which depends on `metrics`;
    // - longer back-edge: `covered` dev-depends on `policy`, which depends on
    //   `gen`, which depends on `covered`;
    // - dev-only back-edge: `testkit` and `usage` dev-depend on each other.
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"metrics\", \"init\", \"covered\", \"policy\", \"gen\", \"testkit\", \"usage\"]\n",
    );
    let path = |n: &str| format!("{n} = {{ path = \"../{n}\" }}\n");
    for (name, deps, dev_deps) in [
        ("metrics", String::new(), path("init")),
        ("init", path("metrics"), String::new()),
        ("covered", String::new(), path("policy")),
        ("policy", path("gen"), String::new()),
        ("gen", path("covered"), String::new()),
        ("testkit", String::new(), path("usage")),
        ("usage", String::new(), path("testkit")),
    ] {
        write(
            &f.ws.join(name).join("Cargo.toml"),
            &manifest(
                name,
                &format!("[dependencies]\n{deps}[dev-dependencies]\n{dev_deps}"),
            ),
        );
        write(&f.ws.join(name).join("src/lib.rs"), "");
    }
    let config = BUCKAL_TOML.split("[fixups.extra]").next().unwrap();
    write(
        &f.ws.join("buckal.toml"),
        &format!("skip_dev_dependency_cycles = true\n{config}"),
    );
    let output = f.output(&["migrate"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(!stderr.contains("Skipping dev-dependency"), "{stderr}");

    for (name, dev_dep) in [
        ("metrics", "init"),
        ("covered", "policy"),
        ("testkit", "usage"),
        ("usage", "testkit"),
    ] {
        let buck = f.buck(name);
        let unittest = rule(&buck, "unittest");
        assert!(
            dep_packages(unittest).contains(dev_dep),
            "{name} tests lack {dev_dep}:\n{unittest}"
        );
        // The library has no dev-dependency.
        assert!(!dep_packages(rule(&buck, name)).contains(dev_dep), "{buck}");
    }

    // Parity: each unit test depends on exactly the packages Cargo's unit graph
    // gives its unit-test unit (on linux).
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = Command::new(cargo)
        .args([
            "test",
            "--workspace",
            "--no-run",
            "--target",
            "x86_64-unknown-linux-gnu",
            "-Z",
            "unstable-options",
            "--unit-graph",
        ])
        .current_dir(&f.ws)
        .env("RUSTC_BOOTSTRAP", "1")
        .env("CARGO_NET_OFFLINE", "true")
        .output()
        .expect("run cargo");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let graph: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let units = graph["units"].as_array().unwrap();
    let package = |u: &serde_json::Value| {
        let id = u["pkg_id"].as_str().unwrap();
        id.rsplit('/')
            .next()
            .unwrap()
            .split('#')
            .next()
            .unwrap()
            .to_owned()
    };
    for name in [
        "metrics", "init", "covered", "policy", "gen", "testkit", "usage",
    ] {
        let unit = units
            .iter()
            .find(|u| u["mode"] == "test" && u["target"]["kind"][0] == "lib" && package(u) == name)
            .unwrap_or_else(|| panic!("no unit test unit for {name}"));
        let cargo_deps: std::collections::BTreeSet<String> = unit["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| package(&units[d["index"].as_u64().unwrap() as usize]))
            .filter(|p| p != name)
            .collect();
        let buck = f.buck(name);
        assert_eq!(
            dep_packages(rule(&buck, "unittest")),
            cargo_deps,
            "{name}:\n{buck}"
        );
    }
}

#[test]
fn test_hand_written_filegroups_and_exports_are_kept() {
    // F5 (Fable review): hand-written `filegroup` and `export_file` rules whose names
    // generation does not own were deleted without notice.
    let f = fixture();
    f.migrate(&[]);
    let app = f.buck("app");
    let hand = "# Fixtures for the tests.\nfilegroup(\n    name = \"handmade_fixtures\",\n    srcs = [\"a.txt\"],\n)\n\nexport_file(\n    name = \"a.txt\",\n)\n\ngenrule(\n    name = \"gen\",\n    out = \"out.txt\",\n    cmd = \"touch $OUT\",\n)\n";
    // A stale generated rule, which goes away with a notice.
    let stale = "rust_test(\n    name = \"removed_it\",\n    srcs = [],\n    crate = \"removed_it\",\n    crate_root = \"tests/removed_it.rs\",\n    edition = \"2021\",\n)\n";
    write(&f.ws.join("app/a.txt"), "a\n");
    write(&f.ws.join("app/BUCK"), &format!("{app}\n{hand}\n{stale}"));
    let output = f.output(&["migrate", "--no-cache"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let regenerated = f.buck("app");
    for kept in [
        "# Fixtures for the tests.\nfilegroup(\n    name = \"handmade_fixtures\",",
        "export_file(\n    name = \"a.txt\",\n)",
        "genrule(\n    name = \"gen\",",
    ] {
        assert!(regenerated.contains(kept), "{kept}:\n{regenerated}");
    }
    assert!(!regenerated.contains("removed_it"), "{regenerated}");
    assert!(
        stderr.contains("removing `rust_test(name = \"removed_it\")`"),
        "{stderr}"
    );
    // A fixed point, with nothing more removed.
    let output = f.output(&["migrate", "--no-cache"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(!stderr.contains("removing"), "{stderr}");
    assert_eq!(f.buck("app"), regenerated);
}

#[test]
fn test_an_older_cache_regenerates_rules_and_keeps_its_fixup_record() {
    // F6 (Fable review): a cache written by an older version (7) certified rules this
    // version would not write. An ordinary migrate now regenerates every package,
    // and still uses that cache's record of the fixup flags for `--merge`.
    let f = fixture();
    let config = |opt: &str| {
        format!(
            "ignore_tests = false\npatch_fields = [\"rustc_flags\"]\n[fixups.extra]\nrustc_flags = [\"-C\", \"opt-level={opt}\"]\n"
        )
    };
    write(&f.ws.join("buckal.toml"), &config("2"));
    f.migrate(&[]);
    let current = f.buck("shared");
    let snap = f.ws.join("buckal.snap");
    let cache = std::fs::read_to_string(&snap).unwrap();
    assert!(cache.contains("version = 14"), "{cache}");
    std::fs::write(&snap, cache.replace("version = 14", "version = 7")).unwrap();
    // Rules as an older version wrote them.
    let old = current.replacen(
        "# Partly generated",
        "# @generated by `cargo buckal`\n# Partly generated",
        1,
    );
    let old = old.replace("# Partly generated", "# old rules");
    write(&f.ws.join("shared/BUCK"), &old);

    write(&f.ws.join("buckal.toml"), &config("3"));
    f.migrate(&["--merge"]);
    assert_eq!(f.buck("shared"), current);
    assert_eq!(
        rustc_flags(rule(&f.buck("extra"), "extra")),
        ["@$(location :manifest[env_flags])", "-C", "opt-level=3"]
    );
    assert!(
        std::fs::read_to_string(&snap)
            .unwrap()
            .contains("version = 14")
    );
}

#[test]
fn test_test_only_host_compilations_use_host_flavours() {
    // F3 (Fable review): the ordinary build compiles `e` for the target only, and
    // `f` in both contexts (`f` with `t` for the target, `f-host` with `h`). The
    // proc-macro `dpm`, which only the tests build, uses `e`: Cargo compiles a host
    // `e` against the host `f` with `h`. The tests' host `e` must not be the
    // production target rule, which depends on the target `f`.
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\", \"e\", \"f\"]\nexclude = [\"dpm\"]\n",
    );
    write(
        &f.ws.join("app/Cargo.toml"),
        &manifest(
            "app",
            "[dependencies]\ne = { path = \"../e\" }\nf = { path = \"../f\", features = [\"t\"] }\n\
             [build-dependencies]\nf = { path = \"../f\", features = [\"h\"] }\n\
             [dev-dependencies]\ndpm = { path = \"../dpm\" }\n",
        ),
    );
    write(&f.ws.join("app/build.rs"), "fn main() {}\n");
    write(&f.ws.join("app/src/lib.rs"), "");
    write(
        &f.ws.join("e/Cargo.toml"),
        &manifest("e", "[dependencies]\nf = { path = \"../f\" }\n"),
    );
    write(
        &f.ws.join("f/Cargo.toml"),
        &manifest("f", "[features]\nt = []\nh = []\n"),
    );
    write(
        &f.ws.join("dpm/Cargo.toml"),
        &manifest(
            "dpm",
            "[lib]\nproc-macro = true\n[dependencies]\ne = { path = \"../e\" }\n",
        ),
    );
    for name in ["e", "f", "dpm"] {
        write(&f.ws.join(name).join("src/lib.rs"), "");
    }
    write(
        &f.ws.join("buckal.toml"),
        BUCKAL_TOML.split("[fixups.extra]").next().unwrap(),
    );
    f.migrate(&[]);
    let fbuck = f.buck("f");
    assert!(rule(&fbuck, "f").contains("\"t\""), "{fbuck}");
    assert!(rule(&fbuck, "f-host").contains("\"h\""), "{fbuck}");
    // Production `e` is a target rule on the target `f`.
    let ebuck = f.buck("e");
    assert!(rule(&ebuck, "e").contains("\"//f:f\""), "{ebuck}");

    // The test-only proc-macro's `e` is a host compilation on the host `f`.
    let dpm = f.buck("dpm");
    let e_label = top_deps(rule(&dpm, "dpm"))
        .into_iter()
        .find(|d| d.starts_with("//e:"))
        .unwrap_or_else(|| panic!("{dpm}"))
        .to_owned();
    let e_name = e_label.strip_prefix("//e:").unwrap();
    assert_ne!(e_name, "e", "{dpm}");
    let host_e = rule(&ebuck, e_name);
    assert!(host_e.contains("\"//f:f-host\""), "{e_label}:\n{ebuck}");
    assert!(!host_e.contains("\"//f:f\""), "{e_label}:\n{ebuck}");
}

#[test]
fn test_integration_tests_honour_required_features() {
    // F7 (Fable review): Cargo builds an integration test only where its
    // `required-features` are on in the tests' resolution.
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\"]\n",
    );
    write(
        &f.ws.join("app/Cargo.toml"),
        &manifest(
            "app",
            "[features]\noff = []\ntest_lib = []\n\
             [[test]]\nname = \"gated_it\"\npath = \"tests/gated_it.rs\"\nrequired-features = [\"off\"]\n\
             [[test]]\nname = \"lib_it\"\npath = \"tests/lib_it.rs\"\nrequired-features = [\"test_lib\"]\n\
             [[test]]\nname = \"plain_it\"\npath = \"tests/plain_it.rs\"\n",
        ),
    );
    write(&f.ws.join("app/src/lib.rs"), "");
    for name in ["gated_it", "lib_it", "plain_it"] {
        write(&f.ws.join(format!("app/tests/{name}.rs")), "");
    }
    let config = BUCKAL_TOML.split("[fixups.extra]").next().unwrap();
    write(
        &f.ws.join("buckal.toml"),
        &format!("test_features = [\"app/test_lib\"]\n{config}"),
    );
    f.migrate(&[]);
    let app = f.buck("app");
    rule(&app, "lib_it");
    rule(&app, "plain_it");
    assert!(!app.contains("name = \"gated_it\""), "{app}");
}

#[test]
fn test_no_test_variant_for_a_target_dependency_with_a_test_host_variant() {
    // `x` gets a test host variant (the test-only proc-macro `dpm` compiles it for
    // the host with `h`), while its target compilation is the same for the tests.
    // `user` only depends on `x` for the target, so its tests' compilation equals
    // production: no `user-test` variant (a target rule takes a host compilation
    // only of a proc-macro or a build dependency).
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\", \"user\", \"x\"]\nexclude = [\"dpm\"]\n",
    );
    write(
        &f.ws.join("app/Cargo.toml"),
        &manifest(
            "app",
            "[dependencies]\nuser = { path = \"../user\" }\n[dev-dependencies]\ndpm = { path = \"../dpm\" }\n",
        ),
    );
    write(
        &f.ws.join("user/Cargo.toml"),
        &manifest("user", "[dependencies]\nx = { path = \"../x\" }\n"),
    );
    write(
        &f.ws.join("x/Cargo.toml"),
        &manifest("x", "[features]\nh = []\n"),
    );
    write(
        &f.ws.join("dpm/Cargo.toml"),
        &manifest(
            "dpm",
            "[lib]\nproc-macro = true\n[dependencies]\nx = { path = \"../x\", features = [\"h\"] }\n",
        ),
    );
    for name in ["app", "user", "x", "dpm"] {
        write(&f.ws.join(name).join("src/lib.rs"), "");
    }
    write(
        &f.ws.join("buckal.toml"),
        BUCKAL_TOML.split("[fixups.extra]").next().unwrap(),
    );
    f.migrate(&[]);
    let x = f.buck("x");
    assert!(rule(&x, "x-test-host").contains("\"h\""), "{x}");
    let user = f.buck("user");
    assert!(!user.contains("name = \"user-test\""), "{user}");
    let app = f.buck("app");
    assert!(rule(&app, "unittest").contains("\"//user:user\""), "{app}");
}

#[test]
fn test_cdylib_only_library_unit_tests_are_emitted() {
    // b807 r28 C: Cargo runs the unit tests of a library whose only crate type is
    // cdylib (unit mode=test, kind cdylib), as for any library.
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"wasmlib\"]\n",
    );
    write(
        &f.ws.join("wasmlib/Cargo.toml"),
        &manifest("wasmlib", "[lib]\ncrate-type = [\"cdylib\"]\n"),
    );
    write(
        &f.ws.join("wasmlib/src/lib.rs"),
        "#[cfg(test)]\nmod tests {\n    #[test]\n    fn inline() {}\n}\n",
    );
    write(
        &f.ws.join("buckal.toml"),
        BUCKAL_TOML.split("[fixups.extra]").next().unwrap(),
    );
    f.migrate(&[]);
    let buck = f.buck("wasmlib");
    let unittest = rule(&buck, "unittest");
    assert!(
        unittest.contains("crate_root = \"src/lib.rs\""),
        "{unittest}"
    );
    assert!(
        buck.contains("rust_test(\n    name = \"unittest\",\n"),
        "{buck}"
    );
}

#[test]
fn test_binaries_only_the_tests_enable_are_emitted_where_cargo_builds_them() {
    // b807 r36 F1: Cargo builds a binary whose `required-features` only the tests
    // enable as its unit tests, and as a binary only for an integration test of
    // its package. Without one, no binary rule is emitted (Cargo skips it); with
    // one, the binary has its normal dependencies.
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"diff\", \"withit\", \"shared\"]\n",
    );
    for name in ["diff", "withit"] {
        write(
            &f.ws.join(name).join("Cargo.toml"),
            &manifest(
                name,
                &format!(
                    "[features]\ntest_lib = []\n\
                     [dependencies]\nshared = {{ path = \"../shared\" }}\n\
                     [[bin]]\nname = \"{name}\"\npath = \"src/main.rs\"\nrequired-features = [\"test_lib\"]\n"
                ),
            ),
        );
        write(
            &f.ws.join(name).join("src/main.rs"),
            "fn main() {\n    shared::one();\n}\n",
        );
    }
    write(&f.ws.join("withit/tests/it.rs"), "");
    write(&f.ws.join("shared/Cargo.toml"), &manifest("shared", ""));
    write(&f.ws.join("shared/src/lib.rs"), "pub fn one() {}\n");
    let config = BUCKAL_TOML.split("[fixups.extra]").next().unwrap();
    write(
        &f.ws.join("buckal.toml"),
        &format!("test_features = [\"diff/test_lib\", \"withit/test_lib\"]\n{config}"),
    );
    f.migrate(&[]);
    let diff = f.buck("diff");
    assert!(!diff.contains("rust_binary("), "{diff}");
    assert_eq!(
        top_deps(rule(&diff, "diff-unittest")),
        ["//shared:shared"],
        "{diff}"
    );
    let withit = f.buck("withit");
    assert_eq!(
        top_deps(rule(&withit, "withit")),
        ["//shared:shared"],
        "{withit}"
    );
    rule(&withit, "it");
}

#[test]
fn test_packages_built_on_no_or_some_configured_platforms_are_incompatible_elsewhere() {
    // b807 r36 F4, Fable N5: `maconly` is a dependency for macOS only, which no
    // configured platform builds, so its rules (build script included) are
    // incompatible everywhere; `wasmdep` is built for wasm32 only, so its target
    // rules are incompatible on linux-x86_64. A package-level
    // `target_compatible_with` fixup adds to the libraries and build scripts.
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\"]\nexclude = [\"maconly\", \"wasmdep\"]\n",
    );
    write(
        &f.ws.join("app/Cargo.toml"),
        &manifest(
            "app",
            "[target.'cfg(target_os = \"macos\")'.dependencies]\nmaconly = { path = \"../maconly\" }\n\
             [target.'cfg(target_arch = \"wasm32\")'.dependencies]\nwasmdep = { path = \"../wasmdep\" }\n",
        ),
    );
    write(&f.ws.join("app/src/lib.rs"), "");
    write(&f.ws.join("app/build.rs"), "fn main() {}\n");
    for name in ["maconly", "wasmdep"] {
        write(&f.ws.join(name).join("Cargo.toml"), &manifest(name, ""));
        write(&f.ws.join(name).join("src/lib.rs"), "");
        write(&f.ws.join(name).join("build.rs"), "fn main() {}\n");
    }
    let config = BUCKAL_TOML.split("[fixups.extra]").next().unwrap();
    write(
        &f.ws.join("buckal.toml"),
        &format!("{config}\n[fixups.app]\ntarget_compatible_with = [\"//cfg:app\"]\n"),
    );
    f.migrate(&[]);
    let none = "target_compatible_with = [\"prelude//:none\"]";
    let maconly = f.buck("maconly");
    assert!(rule(&maconly, "maconly").contains(none), "{maconly}");
    assert!(
        rule(&maconly, "build-script-build").contains(none),
        "{maconly}"
    );
    let run = rule(&maconly, "build-script-run");
    assert!(run.contains(none), "{maconly}");
    assert!(
        run.contains("buildscript_compatible_with = [\"prelude//:none\"]"),
        "{maconly}"
    );
    let wasmdep = f.buck("wasmdep");
    let lib = rule(&wasmdep, "wasmdep");
    assert!(!lib.contains(none), "{wasmdep}");
    assert!(
        lib.contains(
            "\"linux-x86_64\": {\n            \"target_compatible_with\": [\"prelude//:none\"],"
        ),
        "{wasmdep}"
    );
    assert!(!lib.contains("\"wasm32\": {"), "{wasmdep}");
    let app = f.buck("app");
    assert!(
        rule(&app, "app").contains("target_compatible_with = [\"//cfg:app\"]"),
        "{app}"
    );
    assert!(
        rule(&app, "build-script-run").contains("target_compatible_with = [\"//cfg:app\"]"),
        "{app}"
    );
    assert!(!app.contains("prelude//:none"), "{app}");
}

#[test]
fn test_no_test_variant_for_a_library_the_tests_do_not_build() {
    // b807 r36 F1, Fable N3: Cargo runs a cdylib-only library's unit tests but
    // never builds the library itself for `cargo test` (no doc-tests, nothing can
    // link it), so the tests' resolution has no library to emit as a `-test`
    // variant, and its integration test does not depend on it; the unit tests
    // keep their dependencies.
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"wasmlib\", \"shared\"]\n",
    );
    write(
        &f.ws.join("wasmlib/Cargo.toml"),
        &manifest(
            "wasmlib",
            "[lib]\ncrate-type = [\"cdylib\"]\n\
             [features]\ntest_lib = [\"shared/extra\"]\n\
             [dependencies]\nshared = { path = \"../shared\" }\n",
        ),
    );
    write(
        &f.ws.join("wasmlib/src/lib.rs"),
        "pub fn f() {\n    shared::one();\n}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn inline() {}\n}\n",
    );
    write(&f.ws.join("wasmlib/tests/it.rs"), "");
    write(
        &f.ws.join("shared/Cargo.toml"),
        &manifest("shared", "[features]\nextra = []\n"),
    );
    write(&f.ws.join("shared/src/lib.rs"), "pub fn one() {}\n");
    let config = BUCKAL_TOML.split("[fixups.extra]").next().unwrap();
    write(
        &f.ws.join("buckal.toml"),
        &format!("test_features = [\"wasmlib/test_lib\"]\n{config}"),
    );
    f.migrate(&[]);
    let wasmlib = f.buck("wasmlib");
    assert!(!wasmlib.contains("name = \"wasmlib-test\""), "{wasmlib}");
    assert_eq!(
        top_deps(rule(&wasmlib, "wasmlib")),
        ["//shared:shared"],
        "{wasmlib}"
    );
    assert_eq!(
        top_deps(rule(&wasmlib, "unittest")),
        ["//shared:shared-test"],
        "{wasmlib}"
    );
    assert_eq!(
        top_deps(rule(&wasmlib, "it")),
        ["//shared:shared-test"],
        "{wasmlib}"
    );
}

#[test]
fn test_a_path_dependency_outside_the_workspace_gets_its_library_only() {
    // b807 r36/r37 F1, Fable N4: `ext` is a path dependency in the workspace's
    // `exclude`. `cargo build --workspace` and `cargo test --workspace` build its
    // library for `app` and nothing else of it: no binary, no unit or integration
    // test, no binary unit test.
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\", \"shared\"]\nexclude = [\"ext\"]\n",
    );
    write(
        &f.ws.join("app/Cargo.toml"),
        &manifest("app", "[dependencies]\next = { path = \"../ext\" }\n"),
    );
    write(&f.ws.join("app/src/lib.rs"), "");
    write(
        &f.ws.join("ext/Cargo.toml"),
        &manifest(
            "ext",
            "[workspace]\n[dependencies]\nshared = { path = \"../shared\" }\n\
             [dev-dependencies]\nshared = { path = \"../shared\" }\n\
             [[bin]]\nname = \"ext-tool\"\npath = \"src/main.rs\"\n",
        ),
    );
    write(
        &f.ws.join("ext/src/lib.rs"),
        "pub fn f() {\n    shared::one();\n}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn inline() {}\n}\n",
    );
    write(&f.ws.join("ext/src/main.rs"), "fn main() {}\n");
    write(&f.ws.join("ext/tests/it.rs"), "");
    write(&f.ws.join("shared/Cargo.toml"), &manifest("shared", ""));
    write(&f.ws.join("shared/src/lib.rs"), "pub fn one() {}\n");
    write(
        &f.ws.join("buckal.toml"),
        BUCKAL_TOML.split("[fixups.extra]").next().unwrap(),
    );
    f.migrate(&[]);
    let ext = f.buck("ext");
    assert_eq!(top_deps(rule(&ext, "ext")), ["//shared:shared"], "{ext}");
    assert!(!ext.contains("rust_test("), "{ext}");
    assert!(!ext.contains("rust_binary("), "{ext}");
    let app = f.buck("app");
    rule(&app, "unittest");
}

#[test]
fn test_proc_macro_unit_tests_have_the_proc_macro_crate() {
    // Fable R18, b807 r36 F3: rustc puts `proc_macro` in a crate's extern prelude
    // only when compiling a proc-macro; Cargo passes `--extern proc_macro` to a
    // proc-macro's test harness too. Its integration test is an ordinary crate.
    let f = pm_contexts_fixture(true);
    f.migrate(&[]);
    let pm = f.buck("pm");
    assert!(
        rustc_flags(rule(&pm, "unittest")).contains(&"--extern=proc_macro".to_owned()),
        "{pm}"
    );
    assert!(!rule(&pm, "pm").contains("--extern"), "{pm}");
    assert!(!rule(&pm, "api").contains("--extern"), "{pm}");
    let app = f.buck("app");
    assert!(!app.contains("--extern"), "{app}");
}

#[test]
fn test_a_host_only_library_the_tests_build_for_the_target_uses_target_flavours() {
    // Fable N1 (f13): production uses `x` only in `app`'s build script, so its
    // rule is a host compilation, against `f-host`. The tests use `x` for the
    // target, where Cargo compiles it against the target `f` (feature `t`): the
    // tests need an `x-test` variant on target flavours, even though `x`'s own
    // features and dependency names are the same in both.
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\"]\nexclude = [\"x\", \"f\"]\n",
    );
    write(
        &f.ws.join("app/Cargo.toml"),
        &manifest(
            "app",
            "[dependencies]\nf = { path = \"../f\", features = [\"t\"] }\n\
             [build-dependencies]\nx = { path = \"../x\" }\nf = { path = \"../f\", features = [\"h\"] }\n\
             [dev-dependencies]\nx = { path = \"../x\" }\n",
        ),
    );
    write(&f.ws.join("app/src/lib.rs"), "");
    write(&f.ws.join("app/build.rs"), "fn main() {}\n");
    write(&f.ws.join("app/tests/it.rs"), "");
    write(
        &f.ws.join("x/Cargo.toml"),
        &manifest("x", "[dependencies]\nf = { path = \"../f\" }\n"),
    );
    write(&f.ws.join("x/src/lib.rs"), "");
    write(
        &f.ws.join("f/Cargo.toml"),
        &manifest("f", "[features]\nt = []\nh = []\n"),
    );
    write(&f.ws.join("f/src/lib.rs"), "");
    write(
        &f.ws.join("buckal.toml"),
        BUCKAL_TOML.split("[fixups.extra]").next().unwrap(),
    );
    f.migrate(&[]);
    let fb = f.buck("f");
    assert!(rule(&fb, "f").contains("\"t\""), "{fb}");
    assert!(rule(&fb, "f-host").contains("\"h\""), "{fb}");
    let x = f.buck("x");
    assert_eq!(top_deps(rule(&x, "x")), ["//f:f-host"], "{x}");
    assert_eq!(top_deps(rule(&x, "x-test")), ["//f:f"], "{x}");
    let app = f.buck("app");
    assert!(top_deps(rule(&app, "it")).contains(&"//x:x-test"), "{app}");
    assert!(
        top_deps(rule(&app, "build-script-build")).contains(&"//x:x"),
        "{app}"
    );
}

#[test]
fn test_unknown_buckal_toml_keys_are_refused() {
    // A key generation does not know must not be ignored, nor make the whole
    // file fall back to defaults: migration stops, naming the key and the file,
    // and writes nothing.
    let config = BUCKAL_TOML.split("[fixups.extra]").next().unwrap();
    for (extra, key) in [
        ("test_featurs = [\"x\"]\n", "test_featurs"),
        (
            "\n[fixups.app]\ntarget_compatible = [\"//cfg:x\"]\n",
            "target_compatible",
        ),
    ] {
        let f = fixture();
        let toml = if extra.starts_with('\n') {
            format!("{config}{extra}")
        } else {
            format!("{extra}{config}")
        };
        write(&f.ws.join("buckal.toml"), &toml);
        let output = f.output(&["migrate"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{key}: {stderr}");
        assert!(stderr.contains(key), "{key}: {stderr}");
        assert!(stderr.contains("buckal.toml"), "{key}: {stderr}");
        assert!(!f.ws.join("app/BUCK").exists(), "{key}: {stderr}");
    }
}

#[test]
fn test_host_compilations_differing_by_platform_are_recorded_and_gated() {
    // F4/R17 (deferral accepted): the emission record keeps the list of host
    // compilations that resolve differently per platform. A migration whose list
    // differs from the recorded one stops, naming what changed and writing
    // nothing, unless `--accept-host-differences` accepts the new list.
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\", \"pm\"]\n",
    );
    let differing = manifest(
        "app",
        "[target.'cfg(not(target_arch = \"wasm32\"))'.dependencies]\npm = { path = \"../pm\", features = [\"f\"] }\n\
         [target.'cfg(target_arch = \"wasm32\")'.dependencies]\npm = { path = \"../pm\", features = [\"w\"] }\n",
    );
    let alike = manifest(
        "app",
        "[dependencies]\npm = { path = \"../pm\", features = [\"f\"] }\n",
    );
    write(&f.ws.join("app/Cargo.toml"), &alike);
    write(&f.ws.join("app/src/lib.rs"), "");
    write(
        &f.ws.join("pm/Cargo.toml"),
        &manifest(
            "pm",
            "[lib]\nproc-macro = true\n[features]\nf = []\nw = []\n",
        ),
    );
    write(&f.ws.join("pm/src/lib.rs"), "");
    write(
        &f.ws.join("buckal.toml"),
        BUCKAL_TOML.split("[fixups.extra]").next().unwrap(),
    );
    let snap = f.ws.join("buckal.snap");
    // First generation: an empty list is recorded.
    f.migrate(&[]);
    let cache = std::fs::read_to_string(&snap).unwrap();
    assert!(cache.contains("host_platform_differences = []"), "{cache}");

    // The list changes: refused, nothing written.
    write(&f.ws.join("app/Cargo.toml"), &differing);
    let pm_before = f.buck("pm");
    let output = f.output(&["migrate"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("+ pm v0.1.0 (host-only)"), "{stderr}");
    assert!(stderr.contains("--accept-host-differences"), "{stderr}");
    assert_eq!(f.buck("pm"), pm_before);
    assert_eq!(std::fs::read_to_string(&snap).unwrap(), cache);

    // Accepted: generated and recorded; the same list again needs no flag.
    f.migrate(&["--accept-host-differences"]);
    let cache = std::fs::read_to_string(&snap).unwrap();
    assert!(
        cache.contains("host_platform_differences = [\"pm v0.1.0 (host-only)\"]"),
        "{cache}"
    );
    assert!(f.buck("pm").contains("\"w\""), "{}", f.buck("pm"));
    f.migrate(&[]);

    // An entry going away is a change too.
    write(&f.ws.join("app/Cargo.toml"), &alike);
    let output = f.output(&["migrate"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("- pm v0.1.0 (host-only)"), "{stderr}");
}

#[test]
fn test_a_self_dev_dependency_links_the_library_into_its_unit_tests() {
    // b807 r43 G4: `selfie` is a dev-dependency of itself with `test_lib`. Cargo's
    // unit graph links the library, built with `test_lib`, into the library's
    // unit-test harness (`--extern selfie=...`), so test code can name its own
    // crate. The unit tests depend on the library as the tests build it.
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"selfie\", \"plain\"]\n",
    );
    write(
        &f.ws.join("selfie/Cargo.toml"),
        &manifest(
            "selfie",
            "[features]\ntest_lib = []\n\
             [dev-dependencies]\nselfie = { path = \".\", features = [\"test_lib\"] }\n",
        ),
    );
    write(
        &f.ws.join("selfie/src/lib.rs"),
        "#[cfg(feature = \"test_lib\")]\npub fn helper() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {\n        selfie::helper();\n    }\n}\n",
    );
    write(&f.ws.join("selfie/tests/it.rs"), "");
    // Without a self dev-dependency, Cargo does not link the library into its
    // own unit tests.
    write(&f.ws.join("plain/Cargo.toml"), &manifest("plain", ""));
    write(
        &f.ws.join("plain/src/lib.rs"),
        "#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n",
    );
    let config = BUCKAL_TOML.split("[fixups.extra]").next().unwrap();
    write(
        &f.ws.join("buckal.toml"),
        &format!("skip_dev_dependency_cycles = true\n{config}"),
    );
    f.migrate(&[]);
    let selfie = f.buck("selfie");
    assert!(
        rule(&selfie, "selfie-test").contains("\"test_lib\""),
        "{selfie}"
    );
    assert!(!rule(&selfie, "selfie").contains("test_lib"), "{selfie}");
    assert_eq!(
        top_deps(rule(&selfie, "unittest")),
        [":selfie-test"],
        "{selfie}"
    );
    assert_eq!(top_deps(rule(&selfie, "it")), [":selfie-test"], "{selfie}");
    assert!(top_deps(rule(&selfie, "selfie")).is_empty(), "{selfie}");
    let plain = f.buck("plain");
    assert!(top_deps(rule(&plain, "unittest")).is_empty(), "{plain}");
}

#[test]
fn test_a_library_host_compilations_use_is_not_marked_incompatible_on_a_platform() {
    // Fable N6 (f19): `app` uses `w` and the proc-macro `pm` on wasm32 only, and
    // `pm` uses `w`. Cargo builds `w` for the wasm32 target and for the host alike,
    // so there is no `w-host`: `pm`, a host compilation configured for the
    // execution platform (linux), uses the plain `w`. Marking `w` incompatible on
    // linux-x86_64 would make `pm`, and so the wasm32 `app`, incompatible. A
    // library that host compilations use through its plain rule is not marked.
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\"]\nexclude = [\"w\", \"pm\", \"wonly\"]\n",
    );
    write(
        &f.ws.join("app/Cargo.toml"),
        &manifest(
            "app",
            "[target.'cfg(target_arch = \"wasm32\")'.dependencies]\n\
             w = { path = \"../w\" }\npm = { path = \"../pm\" }\nwonly = { path = \"../wonly\" }\n",
        ),
    );
    write(&f.ws.join("app/src/lib.rs"), "");
    write(&f.ws.join("w/Cargo.toml"), &manifest("w", ""));
    write(&f.ws.join("w/src/lib.rs"), "");
    write(&f.ws.join("w/build.rs"), "fn main() {}\n");
    write(&f.ws.join("wonly/Cargo.toml"), &manifest("wonly", ""));
    write(&f.ws.join("wonly/src/lib.rs"), "");
    write(
        &f.ws.join("pm/Cargo.toml"),
        &manifest(
            "pm",
            "[lib]\nproc-macro = true\n[dependencies]\nw = { path = \"../w\" }\n",
        ),
    );
    write(&f.ws.join("pm/src/lib.rs"), "");
    write(
        &f.ws.join("buckal.toml"),
        BUCKAL_TOML.split("[fixups.extra]").next().unwrap(),
    );
    f.migrate(&[]);
    let w = f.buck("w");
    assert!(!w.contains("name = \"w-host\""), "{w}");
    assert!(!w.contains("prelude//:none"), "{w}");
    let pm = f.buck("pm");
    assert!(top_deps(rule(&pm, "pm")).contains(&"//w:w"), "{pm}");
    // A wasm32-only library no host compilation uses is still marked.
    let wonly = f.buck("wonly");
    assert!(
        rule(&wonly, "wonly").contains(
            "\"linux-x86_64\": {\n            \"target_compatible_with\": [\"prelude//:none\"],"
        ),
        "{wonly}"
    );
}

#[test]
fn test_a_cargo_metadata_failure_is_an_error_not_a_panic() {
    // Fable 193510c4 P3: a `test_features` entry no package has makes `cargo
    // metadata` fail; generation reports it and exits 1, without a panic.
    let f = fixture();
    let config = BUCKAL_TOML.split("[fixups.extra]").next().unwrap();
    write(
        &f.ws.join("buckal.toml"),
        &format!("test_features = [\"no_such_feature\"]\n{config}"),
    );
    let output = f.output(&["migrate"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(stderr.contains("cargo metadata"), "{stderr}");
    assert!(stderr.contains("no_such_feature"), "{stderr}");
}

#[test]
fn test_host_differences_guard_compares_resolutions() {
    // Fable 193510c4 P3: the guard compared names only. `pm` stays on the list,
    // but how it differs changes (wasm32 now enables `v` instead of `w`): the
    // record keeps each entry's resolution, so the change stops generation.
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\", \"pm\"]\n",
    );
    let app = |wasm_feature: &str| {
        manifest(
            "app",
            &format!(
                "[target.'cfg(not(target_arch = \"wasm32\"))'.dependencies]\npm = {{ path = \"../pm\", features = [\"f\"] }}\n\
                 [target.'cfg(target_arch = \"wasm32\")'.dependencies]\npm = {{ path = \"../pm\", features = [\"{wasm_feature}\"] }}\n"
            ),
        )
    };
    write(&f.ws.join("app/Cargo.toml"), &app("w"));
    write(&f.ws.join("app/src/lib.rs"), "");
    write(
        &f.ws.join("pm/Cargo.toml"),
        &manifest(
            "pm",
            "[lib]\nproc-macro = true\n[features]\nf = []\nw = []\nv = []\n",
        ),
    );
    write(&f.ws.join("pm/src/lib.rs"), "");
    write(
        &f.ws.join("buckal.toml"),
        BUCKAL_TOML.split("[fixups.extra]").next().unwrap(),
    );
    f.migrate(&[]);
    let snap = f.ws.join("buckal.snap");
    let cache = std::fs::read_to_string(&snap).unwrap();
    write(&f.ws.join("app/Cargo.toml"), &app("v"));
    let pm_before = f.buck("pm");
    let output = f.output(&["migrate"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("~ pm v0.1.0 (host-only)"), "{stderr}");
    assert_eq!(f.buck("pm"), pm_before);
    assert_eq!(std::fs::read_to_string(&snap).unwrap(), cache);
    f.migrate(&["--accept-host-differences"]);
    assert!(f.buck("pm").contains("\"v\""), "{}", f.buck("pm"));
    f.migrate(&[]);
}

#[test]
fn test_a_refusal_writes_no_file_of_the_run() {
    // Fable P3: a refusal for one package exited after the BUCK files of the
    // packages generated before it were written. Generation now writes nothing
    // until every package's rules are known.
    let f = fixture();
    write(&f.ws.join("wasmonly/wasmonly"), "data\n");
    write(
        &f.ws.join("buckal.toml"),
        &format!("{BUCKAL_TOML}\n[fixups.wasmonly]\nexport_files = [\"wasmonly\"]\n"),
    );
    let output = f.output(&["migrate"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("wasmonly"), "{stderr}");
    for package in ["app", "shared", "extra", "extra2", "sdk", "wasmonly"] {
        assert!(
            !f.ws.join(package).join("BUCK").exists(),
            "{package}/BUCK written: {stderr}"
        );
    }
    assert!(!f.ws.join("BUCK").exists(), "{stderr}");
    assert!(!f.ws.join("buckal.snap").exists(), "{stderr}");
}

#[test]
fn test_a_build_dependency_with_a_target_test_variant_makes_no_variant() {
    // Fable P3 (f1): `app`'s build script uses `hl` (feature `x`), and its tests use
    // `hl` for the target (feature `y`), so `hl` gets a test variant. `app`'s build
    // script is a host compilation, which never uses a target test variant, and
    // its host `hl` is the same for the tests: `app` needs no test variant and no
    // second build-script run.
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\"]\nexclude = [\"hl\"]\n",
    );
    write(
        &f.ws.join("app/Cargo.toml"),
        &manifest(
            "app",
            "[build-dependencies]\nhl = { path = \"../hl\", features = [\"x\"] }\n\
             [dev-dependencies]\nhl = { path = \"../hl\", features = [\"y\"] }\n",
        ),
    );
    write(&f.ws.join("app/src/lib.rs"), "");
    write(&f.ws.join("app/build.rs"), "fn main() {}\n");
    write(&f.ws.join("app/tests/it.rs"), "");
    write(
        &f.ws.join("hl/Cargo.toml"),
        &manifest("hl", "[features]\nx = []\ny = []\n"),
    );
    write(&f.ws.join("hl/src/lib.rs"), "");
    write(
        &f.ws.join("buckal.toml"),
        BUCKAL_TOML.split("[fixups.extra]").next().unwrap(),
    );
    f.migrate(&[]);
    let hl = f.buck("hl");
    assert!(rule(&hl, "hl-test").contains("\"y\""), "{hl}");
    let app = f.buck("app");
    assert!(!app.contains("name = \"app-test\""), "{app}");
    assert!(!app.contains("build-script-test-"), "{app}");
    assert!(
        top_deps(rule(&app, "it")).contains(&"//hl:hl-test"),
        "{app}"
    );
    assert!(
        top_deps(rule(&app, "build-script-build")).contains(&"//hl:hl"),
        "{app}"
    );
}

#[test]
fn test_a_generated_export_file_is_removed_without_a_cache_record() {
    // Fable P3: an `export_files` entry is removed while the cache keeps no record
    // of the earlier fixups (here: no cache at all). The `export_file` generation
    // wrote carries its own mark, so it is still recognised and removed.
    let f = fixture();
    write(&f.ws.join("app/data.txt"), "data\n");
    write(
        &f.ws.join("buckal.toml"),
        &format!("{BUCKAL_TOML}\n[fixups.app]\nexport_files = [\"data.txt\"]\n"),
    );
    f.migrate(&[]);
    assert!(rule(&f.buck("app"), "data.txt").contains("src = \"data.txt\""));
    std::fs::remove_file(f.ws.join("buckal.snap")).unwrap();
    write(&f.ws.join("buckal.toml"), BUCKAL_TOML);
    let output = f.output(&["migrate"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let app = f.buck("app");
    assert!(!app.contains("name = \"data.txt\""), "{app}");
    assert!(
        stderr.contains("removing `export_file(name = \"data.txt\")`"),
        "{stderr}"
    );
}

#[test]
fn test_buildscript_run_inputs_through_generation() {
    // G3: `shared`'s build script reads files outside its package relative to
    // `CARGO_MANIFEST_DIR`, as Cargo runs it from the checkout: `../shared.toml` at
    // the workspace root (owned by no generated package) and `../sdk/conf/*.toml`
    // (in the `sdk` package). Its runs (target and host variant) must place them at
    // their workspace-relative positions around the package.
    let f = fixture();
    write(
        &f.ws.join("shared/build.rs"),
        "fn main() {\n    let dir = std::env::var(\"CARGO_MANIFEST_DIR\").unwrap();\n    std::fs::read_to_string(format!(\"{dir}/../shared.toml\")).unwrap();\n    std::fs::read_to_string(format!(\"{dir}/../sdk/conf/a.toml\")).unwrap();\n}\n",
    );
    write(&f.ws.join("shared.toml"), "");
    write(&f.ws.join("sdk/conf/a.toml"), "");
    write(&f.ws.join("sdk/conf/b.toml"), "");
    write(&f.ws.join("sdk/conf/notes.txt"), "");
    f.migrate(&[]);
    let shared_before = f.buck("shared");
    let sdk_before = f.buck("sdk");
    // Without the key the runs place nothing outside the package: the build script
    // would not find `../shared.toml`.
    let runs = shared_before.matches("buildscript_run(").count();
    assert!(runs >= 2, "{shared_before}");
    assert!(
        !shared_before.contains("workspace_inputs"),
        "{shared_before}"
    );
    assert!(!shared_before.contains("package_path"), "{shared_before}");

    let with = |inputs: &str| {
        format!("{BUCKAL_TOML}\n[fixups.shared.buildscript.run]\ninputs = [{inputs}]\n")
    };
    write(
        &f.ws.join("buckal.toml"),
        &with("\"shared.toml\", \"sdk/conf/*.toml\""),
    );
    let output = f.output(&["migrate"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let shared = f.buck("shared");
    let expected = "    package_path = \"shared\",\n    workspace_inputs = {\n        \"sdk/conf/a.toml\": \"//sdk:conf/a.toml\",\n        \"sdk/conf/b.toml\": \"//sdk:conf/b.toml\",\n        \"shared.toml\": \"//:shared.toml\",\n    },\n";
    assert_eq!(shared.matches(expected).count(), runs, "{shared}");
    // The owning generated package exports its files to the reader only; a file no
    // generated package owns is named, for its BUCK file to provide.
    let sdk = f.buck("sdk");
    for name in ["conf/a.toml", "conf/b.toml"] {
        assert!(
            sdk.contains(&format!("export_file(\n    name = \"{name}\",")),
            "{sdk}"
        );
        let export = rule(&sdk, name);
        assert!(export.contains("\"//shared:\""), "{export}");
        assert!(export.contains("\"buckal-export-files\""), "{export}");
    }
    assert!(!sdk.contains("notes.txt"), "{sdk}");
    assert!(
        stderr.contains("`[fixups.shared.buildscript.run] inputs`: `shared.toml` belongs to no generated package; its BUCK file must provide `//:shared.toml`"),
        "{stderr}"
    );
    f.migrate(&["--no-cache"]);
    assert_eq!(f.buck("shared"), shared);
    assert_eq!(f.buck("sdk"), sdk);

    // Paths that are absolute, leave the workspace, lie in the package itself or
    // match nothing are refused, and no BUCK file changes.
    for (input, why) in [
        ("/etc/hostname", "is not a relative path"),
        ("../outside.toml", "leaves the workspace"),
        ("sdk/../../outside.toml", "leaves the workspace"),
        ("shared/src/lib.rs", "is inside the package `shared`"),
        ("missing.toml", "matches no file"),
        ("sdk/conf/*.json", "matches no file"),
    ] {
        write(&f.ws.join("buckal.toml"), &with(&format!("{input:?}")));
        let output = f.output(&["migrate"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{input} was accepted:\n{stderr}");
        assert!(
            stderr.contains(&format!(
                "`[fixups.shared.buildscript.run] inputs`: `{input}`"
            )) && stderr.contains(why),
            "{input}:\n{stderr}"
        );
        assert_eq!(f.buck("shared"), shared, "{input}");
        assert_eq!(f.buck("sdk"), sdk, "{input}");
    }

    // Removing the key restores both packages.
    write(&f.ws.join("buckal.toml"), BUCKAL_TOML);
    f.migrate(&[]);
    assert_eq!(f.buck("shared"), shared_before);
    assert_eq!(f.buck("sdk"), sdk_before);
}

#[test]
fn test_a_plain_rule_host_compilations_use_is_recorded_when_it_differs_by_platform() {
    // Fable N7 (fixture f28): `app` uses `w` everywhere and, on wasm32, `w` with `f`
    // and the proc-macro `pm`, which needs `w` with `f`. On wasm32 Cargo's host `w`
    // equals its target `w`, so no `-host` variant is emitted and `pm` uses plain
    // `:w`, whose resolution differs between linux and wasm32. Buck builds `pm`'s
    // `w` with the execution platform's resolution (no `f`), so the guard must
    // record it. `pm` and `w` are excluded path dependencies, like f28's registry
    // packages: as members, `cargo build --workspace` would build `pm` (and `w`
    // with `f`) on linux too.
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\"]\nexclude = [\"pm\", \"w\"]\n",
    );
    write(
        &f.ws.join("app/Cargo.toml"),
        &manifest(
            "app",
            "[dependencies]\nw = { path = \"../w\" }\n\
             [target.'cfg(target_arch = \"wasm32\")'.dependencies]\nw = { path = \"../w\", features = [\"f\"] }\npm = { path = \"../pm\" }\n",
        ),
    );
    write(&f.ws.join("app/src/lib.rs"), "");
    write(
        &f.ws.join("pm/Cargo.toml"),
        &manifest(
            "pm",
            "[workspace]\n[lib]\nproc-macro = true\n[dependencies]\nw = { path = \"../w\", features = [\"f\"] }\n",
        ),
    );
    write(&f.ws.join("pm/src/lib.rs"), "");
    write(
        &f.ws.join("w/Cargo.toml"),
        &manifest("w", "[workspace]\n[features]\nf = []\n"),
    );
    write(&f.ws.join("w/src/lib.rs"), "");
    write(
        &f.ws.join("buckal.toml"),
        BUCKAL_TOML.split("[fixups.extra]").next().unwrap(),
    );
    let output = f.output(&["migrate"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    // The shape is f28's: plain `:w` for `pm`, `f` on wasm32 only.
    let w = f.buck("w");
    assert!(!w.contains("name = \"w-host\""), "{w}");
    assert!(
        w.contains("\"wasm32\": {\n            \"features\": [\"f\"]"),
        "{w}"
    );
    assert!(
        stderr.contains("resolve differently per configured platform: w v0.1.0 (plain rule used by host compilations)"),
        "{stderr}"
    );
    let snap = std::fs::read_to_string(f.ws.join("buckal.snap")).unwrap();
    assert!(
        snap.contains("w v0.1.0 (plain rule used by host compilations)"),
        "{snap}"
    );
}

/// Fable N7 with `[platforms] execution`: `buckal.toml` with both configured
/// platforms and linux as the execution platform.
fn execution_platform_toml() -> String {
    format!(
        "{}\n[platforms]\nexecution = \"linux-x86_64\"\n",
        BUCKAL_TOML.split("[fixups.extra]").next().unwrap()
    )
}

#[test]
fn test_with_the_execution_platform_a_plain_rule_host_use_differing_from_it_is_recorded() {
    // f28's shape: `pm`'s host compilation of `w` is planned on wasm32 with `f`,
    // while Buck builds plain `:w` with the execution platform's (linux) resolution,
    // which lacks `f`.
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\"]\nexclude = [\"pm\", \"w\"]\n",
    );
    write(
        &f.ws.join("app/Cargo.toml"),
        &manifest(
            "app",
            "[dependencies]\nw = { path = \"../w\" }\n\
             [target.'cfg(target_arch = \"wasm32\")'.dependencies]\nw = { path = \"../w\", features = [\"f\"] }\npm = { path = \"../pm\" }\n",
        ),
    );
    write(&f.ws.join("app/src/lib.rs"), "");
    write(
        &f.ws.join("pm/Cargo.toml"),
        &manifest(
            "pm",
            "[workspace]\n[lib]\nproc-macro = true\n[dependencies]\nw = { path = \"../w\", features = [\"f\"] }\n",
        ),
    );
    write(&f.ws.join("pm/src/lib.rs"), "");
    write(
        &f.ws.join("w/Cargo.toml"),
        &manifest("w", "[workspace]\n[features]\nf = []\n"),
    );
    write(&f.ws.join("w/src/lib.rs"), "");
    write(&f.ws.join("buckal.toml"), &execution_platform_toml());
    let output = f.output(&["migrate"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        !f.buck("w").contains("name = \"w-host\""),
        "{}",
        f.buck("w")
    );
    assert!(
        stderr.contains("resolve differently per configured platform: w v0.1.0 (plain rule used by host compilations)"),
        "{stderr}"
    );
}

#[test]
fn test_with_the_execution_platform_a_plain_rule_differing_only_elsewhere_is_not_recorded() {
    // rustls-native-certs' shape: `r` has a `cfg(unix)` dependency `probe`, so its
    // plain rule differs between linux and wasm32. Its only host compilation, for
    // the proc-macro `pm` that `app` uses on linux, needs exactly its linux
    // resolution, which is the execution platform's: nothing to record. Without
    // `[platforms] execution` the guard stays conservative and records it.
    let f = fixture();
    write(
        &f.ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"app\"]\nexclude = [\"r\", \"probe\", \"pm\"]\n",
    );
    write(
        &f.ws.join("app/Cargo.toml"),
        &manifest(
            "app",
            "[dependencies]\nr = { path = \"../r\" }\n\
             [target.'cfg(not(target_arch = \"wasm32\"))'.dependencies]\npm = { path = \"../pm\" }\n",
        ),
    );
    write(&f.ws.join("app/src/lib.rs"), "");
    write(
        &f.ws.join("pm/Cargo.toml"),
        &manifest(
            "pm",
            "[workspace]\n[lib]\nproc-macro = true\n[dependencies]\nr = { path = \"../r\" }\n",
        ),
    );
    write(&f.ws.join("pm/src/lib.rs"), "");
    write(
        &f.ws.join("r/Cargo.toml"),
        &manifest(
            "r",
            "[workspace]\n[target.'cfg(unix)'.dependencies]\nprobe = { path = \"../probe\" }\n",
        ),
    );
    write(&f.ws.join("r/src/lib.rs"), "");
    write(
        &f.ws.join("probe/Cargo.toml"),
        &manifest("probe", "[workspace]\n"),
    );
    write(&f.ws.join("probe/src/lib.rs"), "");
    write(&f.ws.join("buckal.toml"), &execution_platform_toml());
    let output = f.output(&["migrate"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    // The shape: no `-host` variant, `pm` uses plain `:r`, linux-only `probe`.
    let r = f.buck("r");
    assert!(
        top_deps(rule(&f.buck("pm"), "pm")).contains(&"//r:r"),
        "{}",
        f.buck("pm")
    );
    assert!(!r.contains("name = \"r-host\""), "{r}");
    assert!(
        r.contains("\"linux-x86_64\": {\n            \"deps\": [\"//probe:probe\"]"),
        "{r}"
    );
    assert!(!stderr.contains("resolve differently"), "{stderr}");
    let snap = std::fs::read_to_string(f.ws.join("buckal.snap")).unwrap();
    assert!(snap.contains("host_platform_differences = []"), "{snap}");

    // Without the execution platform: the conservative record.
    write(
        &f.ws.join("buckal.toml"),
        BUCKAL_TOML.split("[fixups.extra]").next().unwrap(),
    );
    let output = f.output(&["migrate", "--accept-host-differences"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("r v0.1.0 (plain rule used by host compilations)"),
        "{stderr}"
    );

    // An execution platform that is not configured is refused.
    write(
        &f.ws.join("buckal.toml"),
        &execution_platform_toml().replace("execution = \"linux-x86_64\"", "execution = \"linux\""),
    );
    let output = f.output(&["migrate"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains(
            "`[platforms] execution = \"linux\"` names no platform of `feature_platforms`"
        ),
        "{stderr}"
    );
}
