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
        let cargo_home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cargo")
            });
        let output = Command::new(env!("CARGO_BIN_EXE_cargo-buckal"))
            .args(["buckal", "migrate"])
            .args(args)
            .current_dir(&self.ws)
            .env("HOME", &self.home)
            .env("CARGO_HOME", cargo_home)
            .env("CARGO_NET_OFFLINE", "true")
            .output()
            .expect("run cargo-buckal");
        assert!(
            output.status.success(),
            "migrate {args:?} exited with {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
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

    // A cache written by an older generator (v0.1.4 wrote version 4) is stale: the
    // rules are regenerated even though the packages did not change.
    let snap = f.ws.join("buckal.snap");
    let cache = std::fs::read_to_string(&snap).unwrap();
    assert!(cache.contains("version = 5"), "{cache}");
    std::fs::write(&snap, cache.replace("version = 5", "version = 4")).unwrap();
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
