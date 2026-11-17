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
    assert!(cache.contains("version = 7"), "{cache}");
    std::fs::write(&snap, cache.replace("version = 7", "version = 4")).unwrap();
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
    std::fs::write(&snap, cache.replace("version = 7", "version = 4")).unwrap();
    // Stale generated rules (hand-written content would be kept).
    let stale = "# @generated by `cargo buckal`\n\nrust_library(\n    name = \"extra\",\n    srcs = [\"stale.rs\"],\n)\n";
    write(&f.ws.join("extra/BUCK"), stale);

    // A no-op update must regenerate the surviving packages before it writes a
    // version 5 cache...
    f.run(&["update", "--workspace"]);
    assert!(
        std::fs::read_to_string(&snap)
            .unwrap()
            .contains("version = 7")
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
            "export_file(\n    name = \"{path}\",\n    src = \"{path}\",\n    mode = \"reference\",\n    visibility = [\"PUBLIC\"],\n)\n"
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
        app.contains("export_file(\n    name = \"src/lib.rs\",\n    src = \"src/lib.rs\",\n    mode = \"reference\",\n    visibility = [\"//shared:\"],\n)\n"),
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
        app.contains("    name = \"src/lib.rs\",\n    src = \"src/lib.rs\",\n    mode = \"reference\",\n    visibility = [],\n"),
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
        "# @generated by `cargo buckal`\n\nfilegroup(\n    name = \"workspace_manifests\",\n    srcs = {\n        \"Cargo.lock\": \"Cargo.lock\",\n        \"Cargo.toml\": \"Cargo.toml\",\n",
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
        app.contains("    name = \"Cargo.toml\",\n    src = \"Cargo.toml\",\n    mode = \"reference\",\n    visibility = [\n        \"//:\",\n        \"//sdk:\",\n    ],\n"),
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
