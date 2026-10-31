//! Architecture rules, checked on the source tree itself with plain string parsing (no extra
//! crates): the dependency direction between the crates (the domain crates `fleet-domain`,
//! `neo-verify` and `neo-goals` <- `fleet-app` <- `fleet-infra` <- `fleet-cli`), no terminal
//! library or async runtime where it does not belong, pinned versions, no `unsafe`, and no
//! `unwrap`/`expect`, clock or I/O in the pure layers. The web dashboard stays an adapter: the
//! inner layers know nothing of networking, only the daemon touches the web module, and no HTTP,
//! async or TLS crate is a dependency.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::fs;
use std::path::{Path, PathBuf};

const CRATES: [&str; 6] = [
    "fleet-domain",
    "neo-verify",
    "neo-goals",
    "fleet-app",
    "fleet-infra",
    "fleet-cli",
];

/// The pure domain layer: std only (plus each other, inward), no I/O, no clock, no randomness.
const DOMAIN: [&str; 3] = ["fleet-domain", "neo-verify", "neo-goals"];

/// Every crate that is not the outermost one.
const INNER: [&str; 5] = [
    "fleet-domain",
    "neo-verify",
    "neo-goals",
    "fleet-app",
    "fleet-infra",
];

/// The pure layers: the domain crates and the application layer.
const PURE: [&str; 4] = ["fleet-domain", "neo-verify", "neo-goals", "fleet-app"];

fn root() -> PathBuf {
    fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .expect("the workspace root exists")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

fn crate_dir(name: &str) -> PathBuf {
    root().join("crates").join(name)
}

// ---- manifests ----------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Dep {
    /// `dependencies`, `dev-dependencies`, `build-dependencies`, `workspace.dependencies` or a
    /// `target.*` table.
    section: String,
    name: String,
    /// Everything after the `=` (or the lines of a `[dependencies.name]` table).
    spec: String,
}

fn is_dep_section(s: &str) -> bool {
    matches!(
        s,
        "dependencies" | "dev-dependencies" | "build-dependencies" | "workspace.dependencies"
    ) || (s.starts_with("target.") && s.ends_with("dependencies"))
}

/// `[dependencies.foo]` style headers: (section kind, dependency name).
fn split_dep_table(s: &str) -> Option<(String, String)> {
    for kind in [
        "workspace.dependencies",
        "dev-dependencies",
        "build-dependencies",
        "dependencies",
    ] {
        if let Some(name) = s.strip_prefix(&format!("{kind}.")) {
            return Some((kind.to_string(), name.to_string()));
        }
    }
    None
}

fn parse_deps(text: &str) -> Vec<Dep> {
    let mut out: Vec<Dep> = Vec::new();
    let mut section = String::new();
    let mut table: Option<usize> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            section = header
                .trim_end_matches(']')
                .trim_start_matches('[')
                .trim()
                .to_string();
            table = None;
            if let Some((kind, name)) = split_dep_table(&section) {
                out.push(Dep {
                    section: kind,
                    name,
                    spec: String::new(),
                });
                table = Some(out.len() - 1);
            }
            continue;
        }
        if is_dep_section(&section) {
            if let Some((name, spec)) = line.split_once('=') {
                out.push(Dep {
                    section: section.clone(),
                    name: name.trim().trim_matches('"').to_string(),
                    spec: spec.trim().to_string(),
                });
            }
        } else if let Some(i) = table {
            out[i].spec.push_str(line);
            out[i].spec.push(' ');
        }
    }
    out
}

fn manifest_text(name: &str) -> String {
    read(&crate_dir(name).join("Cargo.toml"))
}

fn deps_of(name: &str) -> Vec<Dep> {
    parse_deps(&manifest_text(name))
}

fn names_in(deps: &[Dep], section: &str) -> Vec<String> {
    let mut v: Vec<String> = deps
        .iter()
        .filter(|d| d.section == section)
        .map(|d| d.name.clone())
        .collect();
    v.sort();
    v
}

fn all_manifests() -> Vec<(String, Vec<Dep>)> {
    let mut v: Vec<(String, Vec<Dep>)> =
        CRATES.iter().map(|c| (c.to_string(), deps_of(c))).collect();
    v.push((
        "workspace root".to_string(),
        parse_deps(&read(&root().join("Cargo.toml"))),
    ));
    v
}

#[test]
fn the_parser_understands_the_manifest_shapes_used_here() {
    // guards the other tests against silently checking nothing
    let text = "[package]\nname = \"x\"\n\n[dependencies]\na = { path = \"../a\" }\nb = \"=1.0.0\"\n\n[dev-dependencies.c]\nversion = \"=2.0.0\"\n\n[features]\ntesting = []\n";
    let deps = parse_deps(text);
    assert_eq!(names_in(&deps, "dependencies"), ["a", "b"]);
    assert_eq!(names_in(&deps, "dev-dependencies"), ["c"]);
    assert!(deps[2].spec.contains("=2.0.0"));
    let infra = deps_of("fleet-infra");
    assert!(infra.len() >= 6, "{infra:?}");
}

// ---- dependency direction ----------------------------------------------------------------------------

#[test]
fn the_base_domain_crate_has_no_dependencies_at_all() {
    let deps = deps_of("fleet-domain");
    assert!(deps.is_empty(), "fleet-domain depends on {deps:?}");
    let text = manifest_text("fleet-domain");
    assert!(
        text.contains("[dependencies]"),
        "the empty table documents the rule"
    );
}

#[test]
fn the_neo_domain_crates_depend_only_on_the_domain_crates_below_them() {
    // neo-verify may use fleet-domain; neo-goals may use fleet-domain and neo-verify; nothing
    // else, not even in dev- or build-dependencies (their tests are plain std).
    let allowed: [(&str, &[&str]); 2] = [
        ("neo-verify", &["fleet-domain"]),
        ("neo-goals", &["fleet-domain", "neo-verify"]),
    ];
    for (krate, want) in allowed {
        let deps = deps_of(krate);
        assert_eq!(names_in(&deps, "dependencies"), want, "{krate}: {deps:?}");
        assert!(names_in(&deps, "dev-dependencies").is_empty(), "{krate}");
        assert!(names_in(&deps, "build-dependencies").is_empty(), "{krate}");
        assert_eq!(deps.len(), want.len(), "{krate}: {deps:?}");
    }
}

#[test]
fn the_application_layer_depends_only_on_the_domain_crates() {
    let deps = deps_of("fleet-app");
    assert_eq!(
        names_in(&deps, "dependencies"),
        ["fleet-domain", "neo-goals", "neo-verify"],
        "{deps:?}"
    );
    // the only thing allowed in dev-dependencies is itself (to switch on its testing feature)
    let dev = names_in(&deps, "dev-dependencies");
    assert!(dev.iter().all(|d| d == "fleet-app"), "{dev:?}");
    assert!(names_in(&deps, "build-dependencies").is_empty());
}

#[test]
fn no_domain_crate_depends_on_an_outer_layer() {
    for krate in DOMAIN {
        for d in deps_of(krate) {
            assert!(
                DOMAIN.contains(&d.name.as_str()),
                "{krate} depends on {}, which is not a domain crate",
                d.name
            );
        }
    }
}

#[test]
fn the_infrastructure_layer_depends_inward_never_on_the_cli() {
    let deps = deps_of("fleet-infra");
    let normal = names_in(&deps, "dependencies");
    assert!(normal.contains(&"fleet-app".to_string()), "{normal:?}");
    assert!(normal.contains(&"fleet-domain".to_string()), "{normal:?}");
    assert!(
        normal.iter().any(|n| !n.starts_with("fleet-")),
        "the adapters are expected to use external crates: {normal:?}"
    );
    for d in &deps {
        assert_ne!(
            d.name, "fleet-cli",
            "fleet-infra must not depend on fleet-cli"
        );
    }
}

#[test]
fn only_the_cli_is_an_outermost_crate_and_nothing_depends_on_it() {
    let cli = deps_of("fleet-cli");
    let normal = names_in(&cli, "dependencies");
    for inner in ["fleet-domain", "fleet-app", "fleet-infra"] {
        assert!(normal.contains(&inner.to_string()), "{normal:?}");
    }
    for krate in INNER {
        for d in deps_of(krate) {
            assert_ne!(d.name, "fleet-cli", "{krate} depends on the CLI");
        }
    }
    // every internal dependency of every crate points at a path inside the workspace
    for krate in CRATES {
        for d in deps_of(krate)
            .iter()
            .filter(|d| d.name.starts_with("fleet-") || d.name.starts_with("neo-"))
        {
            assert!(d.spec.contains("path"), "{krate}: {d:?}");
        }
    }
}

#[test]
fn terminal_libraries_are_used_by_the_cli_only() {
    for krate in INNER {
        for d in deps_of(krate) {
            assert!(
                !d.name.starts_with("ratatui") && !d.name.starts_with("crossterm"),
                "{krate} depends on the terminal library {}",
                d.name
            );
        }
    }
    let cli = names_in(&deps_of("fleet-cli"), "dependencies");
    assert!(cli.contains(&"ratatui".to_string()) && cli.contains(&"crossterm".to_string()));
    // and the sources of the inner crates do not name them either
    for krate in INNER {
        for file in rust_sources(krate) {
            let code = code_only(&read(&file));
            assert!(
                !code.contains("ratatui") && !code.contains("crossterm"),
                "{} uses a terminal library",
                file.display()
            );
        }
    }
}

const BANNED: [&str; 6] = ["tokio", "async-std", "hyper", "axum", "actix", "warp"];

fn banned(name: &str) -> bool {
    BANNED
        .iter()
        .any(|b| name == *b || name.starts_with(&format!("{b}-")))
}

#[test]
fn no_async_runtime_or_web_framework_is_a_dependency() {
    for (krate, deps) in all_manifests() {
        for d in deps {
            assert!(
                !banned(&d.name),
                "{krate} lists {} (an async runtime or web framework)",
                d.name
            );
        }
    }
    // nor does anything pull one in indirectly
    let lock = read(&root().join("Cargo.lock"));
    for line in lock.lines() {
        if let Some(name) = line
            .strip_prefix("name = \"")
            .and_then(|n| n.strip_suffix('"'))
        {
            assert!(!banned(name), "Cargo.lock contains {name}");
        }
    }
}

// ---- pinned versions --------------------------------------------------------------------------------------

/// `Ok` when the dependency is a path dependency or names an exact version (`=x.y.z`).
fn pinned(spec: &str) -> Result<(), String> {
    let spec = spec.trim();
    if spec.contains("path =") || spec.contains("path=") {
        return Ok(());
    }
    let version = if let Some(rest) = spec.strip_prefix('"') {
        rest.split('"').next().unwrap_or("").to_string()
    } else if let Some(at) = spec.find("version") {
        let after = &spec[at + "version".len()..];
        let after = after
            .trim_start()
            .strip_prefix('=')
            .unwrap_or("")
            .trim_start();
        after
            .strip_prefix('"')
            .map(|r| r.split('"').next().unwrap_or("").to_string())
            .unwrap_or_default()
    } else {
        return Err(format!("no version in `{spec}`"));
    };
    let digits = version.strip_prefix('=').unwrap_or("");
    let looks_exact = !digits.is_empty()
        && digits.split('.').count() >= 3
        && digits
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'));
    if looks_exact {
        Ok(())
    } else {
        Err(format!("`{version}` is not an exact `=x.y.z` requirement"))
    }
}

#[test]
fn the_pin_checker_itself_is_right() {
    for ok in [
        "\"=1.2.3\"",
        "{ version = \"=1.2.3\", default-features = false }",
        "{ path = \"../x\" }",
        "{ version=\"=0.9.0\", features = [\"a\"] }",
    ] {
        assert!(pinned(ok).is_ok(), "{ok}");
    }
    for bad in [
        "\"1.2.3\"",
        "\"^1.2\"",
        "\"*\"",
        "{ version = \"1\" }",
        "{ git = \"x\" }",
    ] {
        assert!(pinned(bad).is_err(), "{bad}");
    }
}

#[test]
fn every_dependency_version_is_pinned_exactly() {
    let mut checked = 0;
    for (krate, deps) in all_manifests() {
        for d in deps {
            if let Err(why) = pinned(&d.spec) {
                panic!(
                    "{krate}: dependency {} ({}) is not pinned: {why}",
                    d.name, d.section
                );
            }
            checked += 1;
        }
    }
    assert!(checked >= 10, "only {checked} dependencies were checked");
}

// ---- unsafe and unwrap -----------------------------------------------------------------------------------------

fn rust_sources(krate: &str) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(&crate_dir(krate).join("src"), &mut out);
    out.sort();
    out
}

/// The text without `//` comments (also doc comments) and `/* */` comments.
fn code_only(text: &str) -> String {
    let mut out = String::new();
    let mut in_block = false;
    for line in text.lines() {
        let mut rest = line;
        let mut kept = String::new();
        loop {
            if in_block {
                match rest.find("*/") {
                    Some(i) => {
                        in_block = false;
                        rest = &rest[i + 2..];
                    }
                    None => break,
                }
            } else {
                let line_comment = rest.find("//");
                let block = rest.find("/*");
                match (line_comment, block) {
                    (Some(l), Some(b)) if b < l => {
                        kept.push_str(&rest[..b]);
                        in_block = true;
                        rest = &rest[b + 2..];
                    }
                    (Some(l), _) => {
                        kept.push_str(&rest[..l]);
                        break;
                    }
                    (None, Some(b)) => {
                        kept.push_str(&rest[..b]);
                        in_block = true;
                        rest = &rest[b + 2..];
                    }
                    (None, None) => {
                        kept.push_str(rest);
                        break;
                    }
                }
            }
        }
        out.push_str(&kept);
        out.push('\n');
    }
    out
}

/// True when `word` occurs as a whole identifier (`unsafe_code` does not count as `unsafe`).
fn has_word(code: &str, word: &str) -> bool {
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    code.match_indices(word).any(|(i, _)| {
        let before = code[..i].chars().next_back();
        let after = code[i + word.len()..].chars().next();
        !before.is_some_and(is_ident) && !after.is_some_and(is_ident)
    })
}

#[test]
fn unsafe_code_is_forbidden_in_every_crate() {
    let workspace = read(&root().join("Cargo.toml"));
    assert!(
        workspace.contains("[workspace.lints.rust]")
            && workspace.contains("unsafe_code = \"forbid\""),
        "the workspace must forbid unsafe_code"
    );
    for krate in CRATES {
        let manifest = manifest_text(krate);
        let lints = manifest
            .split("[lints]")
            .nth(1)
            .and_then(|rest| rest.split('[').next())
            .unwrap_or("");
        assert!(
            lints.contains("workspace = true"),
            "{krate} does not inherit the workspace lints"
        );
        let lib = read(&crate_dir(krate).join("src/lib.rs"));
        assert!(
            lib.contains("#![forbid(unsafe_code)]"),
            "{krate}/src/lib.rs lacks #![forbid(unsafe_code)]"
        );
    }
    // the binary: either its own attribute or, equivalently, the inherited workspace lint above
    let main = read(&crate_dir("fleet-cli").join("src/main.rs"));
    assert!(
        main.contains("#![forbid(unsafe_code)]") || workspace.contains("unsafe_code = \"forbid\""),
        "fleet-cli/src/main.rs is not covered by a forbid(unsafe_code)"
    );
}

#[test]
fn no_source_file_contains_the_unsafe_keyword() {
    let mut scanned = 0;
    for krate in CRATES {
        for file in rust_sources(krate) {
            let code = code_only(&read(&file));
            assert!(!has_word(&code, "unsafe"), "`unsafe` in {}", file.display());
            scanned += 1;
        }
    }
    assert!(scanned >= 40, "only {scanned} source files were scanned");
    assert!(has_word("unsafe { x }", "unsafe") && !has_word("forbid(unsafe_code)", "unsafe"));
}

#[test]
fn the_pure_layers_do_not_unwrap_or_expect_outside_their_tests() {
    let mut scanned = 0;
    for krate in PURE {
        for file in rust_sources(krate) {
            let text = read(&file);
            // support code behind the `testing` feature opts out explicitly
            let rel = file
                .strip_prefix(crate_dir(krate))
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if rel == "src/testing.rs" || rel.starts_with("src/testing/") {
                let support = read(&crate_dir(krate).join("src/testing.rs"));
                assert!(
                    support.contains("allow(clippy::unwrap_used"),
                    "the testing support module must say that it opts out of the unwrap rule"
                );
                continue;
            }
            let production = match text.find("#[cfg(test)]") {
                Some(i) => &text[..i],
                None => &text,
            };
            let code = code_only(production);
            for needle in [".unwrap()", ".expect("] {
                assert!(
                    !code.contains(needle),
                    "{} calls {needle} outside its tests",
                    file.display()
                );
            }
            scanned += 1;
        }
    }
    assert!(scanned >= 25, "only {scanned} source files were scanned");
}

#[test]
fn the_pure_layers_do_not_touch_the_outside_world() {
    // domain and application code talk to files, processes and sockets only through ports
    let forbidden = [
        "std::fs",
        "std::process",
        "std::net",
        "std::os::",
        "std::thread",
        "std::env::",
        "nix::",
        "serde",
        "fleet_infra",
        "fleet_cli",
    ];
    for krate in PURE {
        for file in rust_sources(krate) {
            let rel = file
                .strip_prefix(crate_dir(krate))
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if rel == "src/testing.rs" || rel.starts_with("src/testing/") {
                continue;
            }
            let text = read(&file);
            let production = match text.find("#[cfg(test)]") {
                Some(i) => &text[..i],
                None => &text,
            };
            let code = code_only(production);
            for needle in forbidden {
                assert!(!code.contains(needle), "{} uses {needle}", file.display());
            }
        }
    }
    // the domain does not even know about the application layer
    for krate in DOMAIN {
        for file in rust_sources(krate) {
            assert!(
                !code_only(&read(&file)).contains("fleet_app"),
                "{}",
                file.display()
            );
        }
    }
}

#[test]
fn the_domain_reads_no_clock_and_draws_no_random_numbers() {
    // time reaches the domain as a number in an argument (`at_ms`), never from the system
    let forbidden = [
        "std::time",
        "SystemTime",
        "Instant",
        "rand::",
        "getrandom",
        "RandomState",
    ];
    let mut scanned = 0;
    for krate in DOMAIN {
        for file in rust_sources(krate) {
            let text = read(&file);
            let production = match text.find("#[cfg(test)]") {
                Some(i) => &text[..i],
                None => &text,
            };
            let code = code_only(production);
            for needle in forbidden {
                assert!(
                    !code.contains(needle),
                    "{} uses {needle}: the domain must not read a clock or draw random numbers",
                    file.display()
                );
            }
            scanned += 1;
        }
    }
    assert!(scanned >= 25, "only {scanned} source files were scanned");
}

#[test]
fn the_neo_crates_are_part_of_the_architecture_tests() {
    // guards the lists above against a crate being added to the workspace and forgotten
    let workspace = read(&root().join("Cargo.toml"));
    let members: Vec<&str> = workspace
        .split("members")
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .unwrap_or("")
        .split('"')
        .filter(|s| s.starts_with("crates/"))
        .collect();
    assert_eq!(members.len(), CRATES.len(), "{members:?}");
    for krate in CRATES {
        assert!(
            members.contains(&format!("crates/{krate}").as_str()),
            "{krate} is not a workspace member"
        );
    }
    for krate in ["neo-verify", "neo-goals"] {
        assert!(DOMAIN.contains(&krate) && PURE.contains(&krate) && INNER.contains(&krate));
    }
}

// ---- the web dashboard is an outer adapter -----------------------------------------------------------

fn rust_files_in(dir: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out.sort();
    out
}

fn relative(file: &Path) -> String {
    file.strip_prefix(root())
        .unwrap_or(file)
        .to_string_lossy()
        .into_owned()
}

/// True when `path` occurs and is not the tail of a longer identifier (`web::` matches
/// `crate::web::x` and `web::x` but not `cobweb::x`).
fn mentions_path(code: &str, path: &str) -> bool {
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    code.match_indices(path)
        .any(|(i, _)| !code[..i].chars().next_back().is_some_and(is_ident))
}

#[test]
fn mentions_path_matches_whole_paths_only() {
    assert!(mentions_path("use crate::web::X;", "web::"));
    assert!(mentions_path("web::x()", "web::"));
    assert!(!mentions_path("cobweb::x()", "web::"));
    assert!(!mentions_path(&code_only("// web::x"), "web::"));
}

#[test]
fn the_inner_layers_know_nothing_of_networking_or_the_web_adapter() {
    // Every source, test and in-memory fake of the domain and the application layer, comments
    // and doc strings excluded (the words may appear there).
    let needles = [
        "fleet_infra",
        "fleet_cli",
        "std::net",
        "TcpListener",
        "TcpStream",
        "UdpSocket",
        "SocketAddr",
        "crate::web",
        "web::",
    ];
    let mut files = rust_sources("fleet-domain");
    files.extend(rust_sources("fleet-app"));
    files.extend(rust_files_in(&crate_dir("fleet-domain").join("tests")));
    files.extend(rust_files_in(&crate_dir("fleet-app").join("tests")));
    let mut saw_fakes = false;
    for file in &files {
        let rel = relative(file);
        saw_fakes |= rel.contains("fleet-app/src/testing");
        let code = code_only(&read(file));
        for needle in needles {
            let found = if needle == "web::" {
                mentions_path(&code, needle)
            } else {
                code.contains(needle)
            };
            assert!(!found, "{rel} mentions {needle}");
        }
    }
    assert!(saw_fakes, "the in-memory fakes were not scanned");
    assert!(files.len() >= 25, "only {} files were scanned", files.len());
}

#[test]
fn the_inner_manifests_list_no_outer_crate_in_any_section() {
    for (krate, forbidden) in [
        (
            "fleet-domain",
            &["fleet-app", "fleet-infra", "fleet-cli"][..],
        ),
        ("fleet-app", &["fleet-infra", "fleet-cli"][..]),
    ] {
        let text = manifest_text(krate);
        for dep in deps_of(krate) {
            assert!(
                !forbidden.contains(&dep.name.as_str()),
                "{krate} depends on {} ({})",
                dep.name,
                dep.section
            );
        }
        for name in forbidden {
            assert!(
                !text.contains(&format!("\"../{name}\"")),
                "{krate}'s manifest points at {name}"
            );
        }
    }
}

#[test]
fn only_the_daemon_uses_the_web_module_inside_the_infrastructure_crate() {
    let src = crate_dir("fleet-infra").join("src");
    let mut users = Vec::new();
    for file in rust_files_in(&src) {
        let rel = file
            .strip_prefix(&src)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        if rel.starts_with("web/") || rel == "web.rs" {
            continue;
        }
        let code = code_only(&read(&file));
        if mentions_path(&code, "web::") || code.contains("crate::web") {
            users.push(rel.clone());
        }
        // a brace import such as `use crate::{web, git}` is a way around the path check
        for line in code.lines().filter(|l| l.contains("crate::{")) {
            assert!(!has_word(line, "web"), "{rel}: {line}");
        }
    }
    assert_eq!(
        users,
        ["daemon.rs"],
        "modules that reach into web: {users:?}"
    );
    // lib.rs only declares the module
    let lib = code_only(&read(&src.join("lib.rs")));
    let lines: Vec<&str> = lib
        .lines()
        .filter(|l| has_word(l, "web"))
        .map(str::trim)
        .collect();
    assert_eq!(lines, ["pub mod web;"], "{lines:?}");
}

#[test]
fn the_web_module_imports_only_what_it_needs_from_the_rest_of_the_crate() {
    let dir = crate_dir("fleet-infra").join("src/web");
    let allowed_modules = [
        "web", "eventlog", "json", "statedir", "server", "protocol", "daemon", "toolrun",
    ];
    let allowed_paths = [
        "crate::eventlog::VerifyReport",
        "crate::server::PeerInfo",
        "crate::protocol::MAX_RESPONSE_BYTES",
        "crate::daemon::VERSION",
        "crate::toolrun::output_within",
        "crate::toolrun::TOOL_TIMEOUT",
    ];
    let forbidden = [
        "git",
        "pty",
        "jobs",
        "undo",
        "contract_fs",
        "logs",
        "config_file",
        "client",
        "doctor",
        "fsx",
        "procid",
        "memory",
        "clock",
        "digest",
    ];
    let files = rust_files_in(&dir);
    assert!(files.len() >= 6, "{files:?}");
    let mut seen_paths: Vec<String> = Vec::new();
    for file in &files {
        let rel = relative(file);
        let code = code_only(&read(file));
        assert!(
            !code.contains("crate::{"),
            "{rel} uses a brace import of crate modules"
        );
        for (i, _) in code.match_indices("crate::") {
            let rest = &code[i + "crate::".len()..];
            let module: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            assert!(
                !forbidden.contains(&module.as_str()),
                "{rel} imports crate::{module}"
            );
            assert!(
                allowed_modules.contains(&module.as_str()),
                "{rel} imports crate::{module}, which is not on the list"
            );
            let path: String = format!("crate::{rest}")
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == ':')
                .collect();
            match module.as_str() {
                "eventlog" | "server" | "protocol" | "daemon" | "toolrun" => {
                    assert!(
                        allowed_paths.contains(&path.as_str()),
                        "{rel} imports {path}"
                    );
                    seen_paths.push(path);
                }
                _ => {}
            }
        }
        // the sibling adapters are not reachable through `super::super` or `fleet_infra::` either
        assert!(!code.contains("fleet_infra::"), "{rel}");
        assert!(!code.contains("super::super"), "{rel}");
    }
    // the check is not vacuous: the module does use the paths on the list
    seen_paths.sort();
    seen_paths.dedup();
    assert_eq!(seen_paths.len(), allowed_paths.len(), "{seen_paths:?}");
}

#[test]
fn the_cli_reaches_the_web_adapter_only_through_its_option_parser_and_options() {
    let mut uses = Vec::new();
    for file in rust_sources("fleet-cli") {
        let rel = relative(&file);
        let code = code_only(&read(&file));
        for (i, _) in code.match_indices("fleet_infra::web") {
            let rest = &code[i + "fleet_infra::web".len()..];
            assert!(
                rest.starts_with("::{parse_bind, WebOptions};"),
                "{rel}: unexpected web import `{}`",
                rest.lines().next().unwrap_or("")
            );
            uses.push(rel.clone());
        }
        for line in code.lines().filter(|l| l.contains("fleet_infra::{")) {
            assert!(!has_word(line, "web"), "{rel}: {line}");
        }
        // no other path into the module, even through a re-import
        for (i, _) in code.match_indices("web::") {
            let before = &code[..i];
            assert!(
                before.ends_with("fleet_infra::"),
                "{rel}: web:: used outside the single import"
            );
        }
    }
    assert_eq!(uses.len(), 1, "{uses:?}");
    assert!(uses[0].ends_with("fleet-cli/src/commands.rs"), "{uses:?}");
}

#[test]
fn the_web_module_has_no_unsafe_and_no_unwrap_or_expect_outside_its_tests() {
    let files = rust_files_in(&crate_dir("fleet-infra").join("src/web"));
    assert!(files.len() >= 6, "{files:?}");
    for file in &files {
        let rel = relative(file);
        let text = read(file);
        assert!(
            !has_word(&code_only(&text), "unsafe"),
            "{rel} contains the unsafe keyword"
        );
        // production code is everything above the file's top-level `#[cfg(test)]` module
        let mut production = String::new();
        for line in text.lines() {
            if line == "#[cfg(test)]" {
                break;
            }
            production.push_str(line);
            production.push('\n');
        }
        let code = code_only(&production);
        for needle in [
            ".unwrap(",
            ".expect(",
            "unwrap_unchecked",
            "panic!(",
            "unreachable!(",
            "todo!(",
            "unimplemented!(",
        ] {
            assert!(
                !code.contains(needle),
                "{rel} uses {needle} outside its tests"
            );
        }
    }
}

#[test]
fn no_http_async_or_tls_crate_is_a_dependency_of_any_crate() {
    // everything the four crates may depend on besides each other
    let allowed = [
        "serde",
        "serde_json",
        "sha2",
        "toml",
        "portable-pty",
        "nix",
        "ratatui",
        "crossterm",
    ];
    let mut checked = 0;
    for (krate, deps) in all_manifests() {
        for dep in deps {
            checked += 1;
            let is_path = dep.spec.contains("path");
            assert!(
                is_path || allowed.contains(&dep.name.as_str()),
                "{krate} ({}) depends on {}, which is not on the list of {allowed:?}",
                dep.section,
                dep.name
            );
        }
    }
    assert!(checked >= 15, "only {checked} dependencies were checked");
}

/// Crates that serve or speak HTTP, run an async executor, or provide TLS. `mio` belongs here too:
/// it is allowed only where the terminal library brings it in (see the lock file test).
const NETWORK_CRATES: [&str; 31] = [
    "hyper",
    "hyper-util",
    "tokio",
    "tokio-util",
    "axum",
    "warp",
    "tiny_http",
    "tiny-http",
    "rustls",
    "openssl",
    "native-tls",
    "reqwest",
    "ureq",
    "actix",
    "actix-web",
    "actix-rt",
    "rocket",
    "async-std",
    "smol",
    "mio",
    "h2",
    "http",
    "tower",
    "tungstenite",
    "tokio-tungstenite",
    "isahc",
    "surf",
    "curl",
    "futures",
    "async-io",
    "socket2",
];

struct LockPackage {
    name: String,
    dependencies: Vec<String>,
}

fn lock_packages() -> Vec<LockPackage> {
    let text = read(&root().join("Cargo.lock"));
    let mut out: Vec<LockPackage> = Vec::new();
    let mut in_deps = false;
    for line in text.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            out.push(LockPackage {
                name: String::new(),
                dependencies: Vec::new(),
            });
            in_deps = false;
        } else if let Some(name) = line.strip_prefix("name = \"") {
            if let Some(p) = out.last_mut() {
                p.name = name.trim_end_matches('"').to_string();
            }
        } else if line == "dependencies = [" {
            in_deps = true;
        } else if line == "]" {
            in_deps = false;
        } else if in_deps {
            if let (Some(p), Some(dep)) = (
                out.last_mut(),
                line.trim_matches(|c| c == ',' || c == '"')
                    .split(' ')
                    .next(),
            ) {
                p.dependencies.push(dep.to_string());
            }
        }
    }
    out
}

#[test]
fn the_lock_file_holds_no_http_async_or_tls_crate() {
    let packages = lock_packages();
    assert!(
        packages.len() >= 60 && packages.iter().any(|p| p.name == "fleet-infra"),
        "the lock file was not parsed: {} packages",
        packages.len()
    );
    for p in &packages {
        if p.name == "mio" {
            continue;
        }
        let name = p.name.as_str();
        assert!(
            !NETWORK_CRATES.contains(&name),
            "Cargo.lock contains the network crate {name}"
        );
        for prefix in [
            "tokio-", "hyper-", "actix-", "rustls-", "openssl-", "async-", "axum-",
        ] {
            assert!(!name.starts_with(prefix), "Cargo.lock contains {name}");
        }
    }
    // `mio` comes with the terminal library's event polling and nothing else may use it
    let users: Vec<&str> = packages
        .iter()
        .filter(|p| p.dependencies.iter().any(|d| d == "mio"))
        .map(|p| p.name.as_str())
        .collect();
    for user in &users {
        assert!(
            matches!(*user, "crossterm" | "signal-hook-mio"),
            "{user} depends on mio"
        );
    }
    // and the fleet crates themselves never name any of these crates
    for p in packages.iter().filter(|p| p.name.starts_with("fleet-")) {
        for d in &p.dependencies {
            assert!(
                !NETWORK_CRATES.contains(&d.as_str()),
                "{} depends on {d}",
                p.name
            );
        }
    }
}

// ---- messaging between agents ---------------------------------------------------------------------

fn msg_files() -> Vec<PathBuf> {
    let dir = crate_dir("fleet-domain").join("src/msg");
    let mut files: Vec<PathBuf> = rust_files_in(&dir)
        .into_iter()
        .filter(|f| f.file_name().is_some_and(|n| n != "mod.rs"))
        .collect();
    files.sort();
    files
}

#[test]
fn the_messaging_domain_is_pure_declared_and_tested() {
    let files = msg_files();
    assert!(files.len() >= 8, "only {} messaging files", files.len());
    let module = read(&crate_dir("fleet-domain").join("src/msg/mod.rs"));
    for file in &files {
        let stem = file.file_stem().unwrap().to_string_lossy().into_owned();
        assert!(
            module.contains(&format!("pub mod {stem};")),
            "{stem} is not declared in msg/mod.rs"
        );
        let text = read(file);
        assert!(
            text.contains("#[cfg(test)]"),
            "{} has no unit tests",
            file.display()
        );
        let production = match text.find("#[cfg(test)]") {
            Some(i) => &text[..i],
            None => &text,
        };
        let code = code_only(production);
        // pure: no clock, no randomness, no files, no processes, no threads
        for needle in [
            "std::time",
            "SystemTime",
            "Instant",
            "std::fs",
            "std::process",
            "std::thread",
            "std::env::",
            "std::net",
            "rand::",
            ".unwrap()",
            ".expect(",
        ] {
            assert!(!code.contains(needle), "{} uses {needle}", file.display());
        }
    }
    let declared = module.matches("pub mod ").count();
    assert_eq!(
        declared,
        files.len(),
        "msg/mod.rs declares a module without a file"
    );
}

#[test]
fn only_the_message_file_adapter_and_its_tests_write_inboxes() {
    // the inbox belongs to the daemon: no inner layer touches the directory, and in the
    // infrastructure crate only msgfs.rs (and the pty's fixed notice) knows its layout
    for krate in PURE {
        for file in rust_sources(krate) {
            let rel = relative(&file);
            if rel.contains("/testing") {
                continue;
            }
            let text = read(&file);
            let production = match text.find("#[cfg(test)]") {
                Some(i) => &text[..i],
                None => &text,
            };
            for needle in ["openat(", "renameat(", "unlinkat(", "\".fleet/inbox"] {
                assert!(
                    !code_only(production).contains(needle),
                    "{rel} touches the message directories directly"
                );
            }
        }
    }
    for file in rust_sources("fleet-infra") {
        let rel = relative(&file);
        if rel.ends_with("/msgfs.rs") || rel.contains("/web/") {
            continue;
        }
        let text = read(&file);
        let production = match text.find("#[cfg(test)]") {
            Some(i) => &text[..i],
            None => &text,
        };
        let code = code_only(production);
        assert!(
            !code.contains("INBOX_DIR")
                && !code.contains("OUTBOX_DIR")
                && !code.contains("\"inbox\""),
            "{rel} knows the layout of the message directories"
        );
    }
}

#[test]
fn the_web_adapter_never_names_the_operators_message_commands() {
    let mut scanned = 0;
    let mut files = rust_files_in(&crate_dir("fleet-infra").join("src/web"));
    for asset in ["app.js", "index.html", "app.css"] {
        files.push(crate_dir("fleet-infra").join("web").join(asset));
    }
    for file in files {
        let full = read(&file);
        // the tests of the web module name the commands to prove they are refused
        let text = match full.find("#[cfg(test)]") {
            Some(i) if file.extension().is_some_and(|e| e == "rs") => full[..i].to_string(),
            _ => full,
        };
        for needle in ["msg.send", "msg.mute", "msg.unmute"] {
            assert!(
                !code_only(&text).contains(needle),
                "{} reaches for {needle}",
                file.display()
            );
        }
        scanned += 1;
    }
    assert!(scanned >= 8, "only {scanned} web files were scanned");
}

#[test]
fn message_text_has_no_path_into_an_approval_a_contract_or_a_goal() {
    // peer text may not become part of anything the daemon treats as true: the approval
    // action for a message is built from ids and a digest, and the verification crates do not
    // know the message types at all
    for krate in ["neo-verify", "neo-goals"] {
        for file in rust_sources(krate) {
            let code = code_only(&read(&file));
            assert!(
                !code.contains("msg::") && !code.contains("OutboxMessage"),
                "{} knows messages",
                file.display()
            );
        }
    }
    let approval = read(&crate_dir("fleet-domain").join("src/approval.rs"));
    let action = approval
        .split("pub fn message(")
        .nth(1)
        .and_then(|rest| rest.split("pub fn validate").next())
        .unwrap_or("");
    assert!(!action.is_empty(), "Action::message was not found");
    assert!(
        !action.contains("subject") && !action.contains("body"),
        "the approval action must name the message, never its words"
    );
}
