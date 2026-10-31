//! Architecture rules, checked on the source tree itself with plain string parsing (no extra
//! crates): the dependency direction between the crates (the domain crates `fleet-domain`,
//! `neo-verify` and `neo-goals` <- `fleet-app` <- `fleet-infra` <- `fleet-cli`), no terminal
//! library or async runtime where it does not belong, pinned versions, no `unsafe`, and no
//! `unwrap`/`expect`, clock or I/O in the pure layers.

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
