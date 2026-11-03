//! The self-development scaffold of `dogfood/` is data that other things rely on: the protected
//! list is read by a shell script and by every contract, the contracts must be accepted by the
//! real parser, the plan by the real loader, the daemon template by the real configuration reader.
//! These tests fail when a part drifts from the others. They read the files of the repository;
//! nothing here runs a process.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fleet_domain::dag::{order_new_tasks, Node};
use fleet_domain::ids::TaskId;
use fleet_domain::limits::MAX_SPEC;
use fleet_domain::value::Value;
use fleet_infra::config_file::{load_config, load_plan};
use fleet_infra::json::parse_value;
use neo_verify::contract::{CheckKind, FileAssertion, VerificationContract};
use neo_verify::glob::Glob;
use neo_verify::judge::{judge, FileObservation, Observation};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap()
}

fn read(rel: &str) -> String {
    let p = root().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// Every file of the repository (relative, `/`-separated), without .git, build output and worktrees.
fn tree() -> Vec<String> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            if dir == base && matches!(name.as_str(), ".git" | "target" | ".fleet-worktrees") {
                continue;
            }
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                walk(&path, base, out);
            } else {
                let rel = path.strip_prefix(base).unwrap();
                out.push(
                    rel.components()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join("/"),
                );
            }
        }
    }
    let base = root();
    let mut out = Vec::new();
    walk(&base, &base, &mut out);
    out.sort();
    out
}

// ---- the protected list ---------------------------------------------------------------------------

struct Entry {
    path: String,
    kind: String,
    glob: Glob,
}

fn protected() -> Vec<Entry> {
    let text = read("dogfood/protected-paths.json");
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(doc["version"], 1);
    assert!(doc["principle"].as_str().unwrap().len() > 40);
    assert!(doc["matching"].as_str().unwrap().contains("case-sensitive"));
    let paths = doc["paths"].as_array().unwrap();
    // the shell checker reads one `"path"` key per line
    let lines_with_path: Vec<&str> = text.lines().filter(|l| l.contains("\"path\"")).collect();
    assert_eq!(lines_with_path.len(), paths.len(), "one entry per line");
    for l in &lines_with_path {
        assert_eq!(l.matches("\"path\"").count(), 1, "{l}");
    }
    paths
        .iter()
        .map(|p| {
            let obj = p.as_object().unwrap();
            let keys: BTreeSet<&str> = obj.keys().map(String::as_str).collect();
            assert_eq!(
                keys,
                BTreeSet::from(["path", "kind", "group", "why"]),
                "{obj:?}"
            );
            let path = obj["path"].as_str().unwrap().to_string();
            let why = obj["why"].as_str().unwrap();
            assert!(
                why.len() >= 20 && why.len() <= 220 && !why.contains('\n'),
                "{path}: one line of justification"
            );
            assert!(!obj["group"].as_str().unwrap().is_empty());
            Entry {
                glob: Glob::new(&path).unwrap_or_else(|e| panic!("{path}: {e}")),
                kind: obj["kind"].as_str().unwrap().to_string(),
                path,
            }
        })
        .collect()
}

fn is_protected(entries: &[Entry], path: &str) -> bool {
    entries.iter().any(|e| e.glob.matches(path))
}

#[test]
fn the_protected_list_is_well_formed_and_every_entry_is_usable_by_the_shell_checker() {
    let entries = protected();
    assert!(entries.len() >= 40, "{} entries", entries.len());
    let mut seen = BTreeSet::new();
    for e in &entries {
        assert!(seen.insert(e.path.clone()), "{} is listed twice", e.path);
        // the checker builds `case` patterns from these characters only
        assert!(
            e.path
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._*?/-".contains(c)),
            "{}",
            e.path
        );
        assert!(!e.path.starts_with('/') && !e.path.ends_with('/') && !e.path.contains(".."));
        let wild = e.path.contains('*') || e.path.contains('?');
        match e.kind.as_str() {
            "file" => assert!(!wild, "{}: a file has no wildcard", e.path),
            "dir" => assert!(
                e.path.ends_with("/**"),
                "{}: a directory ends in /**",
                e.path
            ),
            "pattern" => assert!(wild, "{}: a pattern has a wildcard", e.path),
            "guard" => {}
            other => panic!("{}: unknown kind {other}", e.path),
        }
    }
    // the things the principle names are on the list
    for must in [
        "SECURITY.md",
        "scripts/**",
        "lefthook.yml",
        ".secret-scan-allow",
        ".gitignore",
        ".github/**",
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "dogfood/**",
        "crates/neo-verify/**",
        "crates/neo-goals/**",
        "crates/fleet-app/**",
        "crates/fleet-infra/**",
        "crates/fleet-cli/**",
        "crates/fleet-cli/tests/architecture.rs",
        "crates/fleet-domain/src/approval.rs",
        "crates/fleet-domain/src/supervision.rs",
        "crates/fleet-domain/src/redact.rs",
        "examples/fleet.example.toml",
    ] {
        assert!(seen.contains(must), "{must} must be protected");
    }
}

#[test]
fn every_entry_matches_something_that_exists_and_every_guard_matches_nothing_yet() {
    let entries = protected();
    let files = tree();
    for e in &entries {
        let hits = files.iter().filter(|f| e.glob.matches(f)).count();
        if e.kind == "guard" {
            assert_eq!(
                hits, 0,
                "{} exists now: change its kind in dogfood/protected-paths.json (a human decision)",
                e.path
            );
            continue;
        }
        assert!(hits > 0, "{} matches no file of the repository", e.path);
        match e.kind.as_str() {
            // a named file is a regular file of the tree, not merely a path some glob reaches
            "file" => assert!(
                files.binary_search(&e.path).is_ok(),
                "{} is listed as a file and is not one",
                e.path
            ),
            // a directory entry names a directory that exists and has files in it
            "dir" => {
                let dir = e.path.trim_end_matches("**").trim_end_matches('/');
                assert!(
                    root().join(dir).is_dir(),
                    "{} is listed as a directory and is not one",
                    e.path
                );
            }
            _ => {}
        }
    }
    // everything the guardrails are made of is covered: each file of the code that holds an
    // approval, origin, verification, evidence, git or secret rule is on the list
    for f in [
        "crates/fleet-domain/src/approval.rs",
        "crates/fleet-domain/src/events.rs",
        // the canonical JSON every digest, the hash chain, contract ids and pack digests are computed over
        "crates/fleet-domain/src/value.rs",
        "crates/fleet-infra/src/server.rs",
        "crates/fleet-infra/src/daemon.rs",
        "crates/fleet-infra/src/jobs.rs",
        "crates/fleet-infra/src/git.rs",
        "crates/fleet-infra/src/verify.rs",
        "crates/fleet-infra/src/web/auth.rs",
        "crates/fleet-app/src/neo/verify.rs",
        "crates/fleet-app/src/orchestrator/dispatch.rs",
        "crates/neo-goals/src/ledger.rs",
        "crates/neo-verify/src/judge.rs",
        "scripts/secret-scan.sh",
        "lefthook.yml",
        ".secret-scan-allow",
        ".github/workflows/ci.yml",
        ".github/dependabot.yml",
        "dogfood/protected-paths.json",
        "dogfood/contracts/single-file.json",
        "docs/adr/0016-evidence-is-about-the-repository-not-about-what-a-worktree-says.md",
    ] {
        assert!(is_protected(&entries, f), "{f} must be protected");
    }
    // and the files a task is allowed to change are not
    for f in [
        "README.md",
        "docs/domain.md",
        "docs/neo.md",
        "crates/fleet-domain/src/error.rs",
        "crates/fleet-domain/src/dag.rs",
        "crates/fleet-domain/src/status.rs",
        "examples/plan.example.toml",
    ] {
        assert!(!is_protected(&entries, f), "{f} must stay open to tasks");
    }
}

#[test]
fn the_guardrail_files_of_the_later_features_exist_and_are_protected_by_name() {
    let entries = protected();
    let files = tree();
    // Each of these is code or a script that decides something a task must not be able to loosen:
    // SSH host key pinning and the remote host, the forge push and approval rules, the messaging
    // policy and secret shapes, the fan-out pick, the context compiler's secret rules and the check
    // of stored packs, the web admission and the read-only workbench allowlist, and the scripts and
    // CI that run the tests. A file renamed away from here fails the test (the list is then wrong).
    for f in [
        ".github/workflows/ci.yml",
        "docker/Dockerfile.ssh",
        "scripts/ssh-env.sh",
        "scripts/test-layers.sh",
        "scripts/coverage.sh",
        "scripts/secret-scan.sh",
        "examples/fanout-contract.json",
        "examples/github/README.md",
        "crates/fleet-domain/src/remote.rs",
        "crates/fleet-domain/src/msg/channel.rs",
        "crates/fleet-domain/src/msg/secrets.rs",
        "crates/fleet-domain/src/msg/loops.rs",
        "crates/fleet-domain/src/msg/ratelimit.rs",
        "crates/fleet-domain/src/repo/markdown.rs",
        "crates/fleet-domain/src/repo/path.rs",
        "crates/fleet-forge/src/publish.rs",
        "crates/fleet-forge/src/push.rs",
        "crates/fleet-forge/src/rate.rs",
        "crates/fleet-forge/src/untrusted.rs",
        "crates/fleet-fanout/src/group.rs",
        "crates/fleet-fanout/src/rank.rs",
        "crates/fleet-app/src/remote.rs",
        "crates/fleet-app/src/hosts.rs",
        "crates/fleet-app/src/fanout/pick.rs",
        "crates/fleet-app/src/forge/exec.rs",
        "crates/fleet-app/src/orchestrator/messaging.rs",
        "crates/fleet-app/src/orchestrator/forge.rs",
        "crates/fleet-app/src/neo/context.rs",
        "crates/fleet-app/src/neo/context_cmds.rs",
        "crates/fleet-app/src/neo/context_state.rs",
        "crates/fleet-infra/src/ssh/known_hosts.rs",
        "crates/fleet-infra/src/ssh/trust.rs",
        "crates/fleet-infra/src/ssh/argv.rs",
        "crates/fleet-infra/src/ssh/host.rs",
        "crates/fleet-infra/src/hosts_file.rs",
        "crates/fleet-infra/src/git/push.rs",
        "crates/fleet-infra/src/git/remote_sync.rs",
        "crates/fleet-infra/src/forge_gh.rs",
        "crates/fleet-infra/src/msgfs.rs",
        "crates/fleet-infra/src/context_store.rs",
        "crates/fleet-infra/src/web/routes.rs",
        "crates/fleet-infra/src/web/repo.rs",
        "crates/fleet-infra/web/panes.js",
        "crates/fleet-infra/web/workbench.js",
        "crates/fleet-infra/tests/ssh_remote.rs",
        "crates/fleet-infra/tests/dogfood.rs",
        "crates/fleet-cli/tests/e2e_remote.rs",
        "crates/fleet-cli/tests/architecture.rs",
        "crates/neo-context/src/secret.rs",
        "crates/neo-context/src/pack.rs",
        "crates/neo-context/src/compile.rs",
    ] {
        assert!(
            files.iter().any(|x| x == f),
            "{f} does not exist: fix this list"
        );
        assert!(is_protected(&entries, f), "{f} must be protected");
    }
    // the entries that name one of these files (not only a directory above it) carry its reason
    for named in [
        "docker/Dockerfile.ssh",
        "scripts/ssh-env.sh",
        "scripts/test-layers.sh",
        "crates/fleet-infra/src/ssh/known_hosts.rs",
        "crates/fleet-infra/src/ssh/trust.rs",
        "crates/fleet-infra/src/git/push.rs",
        "crates/fleet-fanout/**",
        "crates/fleet-forge/**",
        "crates/neo-context/src/secret.rs",
        ".github/workflows/**",
    ] {
        assert!(
            entries.iter().any(|e| e.path == named),
            "{named} has no entry of its own"
        );
    }
}

// ---- the contracts -------------------------------------------------------------------------------

const COMMON_IDS: [&str; 10] = [
    "protected-diff",
    "protected-script",
    "lock-unchanged",
    "docs-claims",
    "conflict-markers",
    "secret-scan",
    "fmt",
    "clippy",
    "architecture",
    "layers",
];

fn dummies() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        ("TOOLS", "/opt/dogfood/bin"),
        ("CONTAINER_RUN", "/opt/dogfood/container-run"),
        ("REPO", "/opt/dogfood/clone"),
        ("BASE", "0123456789abcdef0123456789abcdef01234567"),
        ("RUN", "neo-roadmap"),
        ("TASK", "a-task"),
        (
            "LOCK_SHA256",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ),
        ("FILE_PATH", "docs/domain.md"),
        ("CRATE", "neo-knowledge"),
        ("PACKAGE", "fleet-infra"),
        ("TEST_NAME", "a_reply_is_delivered"),
        ("TEST_FILTER", "server::tests::a_reply_is_delivered"),
        ("ALSO", ""),
    ])
}

fn rendered(text: &str) -> String {
    let mut out = text.to_string();
    for (k, v) in dummies() {
        out = out.replace(&format!("@{k}@"), v);
    }
    assert!(!out.contains('@'), "an unknown placeholder is left");
    out
}

fn contract_of(text: &str) -> Result<VerificationContract, String> {
    let value = parse_value(text.as_bytes(), 64 * 1024)?;
    VerificationContract::from_value(&value)
}

fn globs_of(list: &[neo_verify::glob::Glob]) -> Vec<String> {
    list.iter().map(|g| g.as_str().to_string()).collect()
}

const TEMPLATES: [&str; 3] = ["single-file", "docs-only", "crate-src"];

#[test]
fn every_template_is_accepted_once_rendered_and_refused_before() {
    let entries = protected();
    let expected_deny: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
    for name in TEMPLATES {
        let raw = read(&format!("dogfood/contracts/{name}.json"));
        assert!(
            contract_of(&raw).is_err(),
            "{name}: an unrendered template must not be accepted by fleet"
        );
        let c = contract_of(&rendered(&raw)).unwrap_or_else(|e| panic!("{name}: {e}"));
        let ids: Vec<&str> = c.checks().iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids[..10], COMMON_IDS, "{name}");
        assert_eq!(ids[10], "scope", "{name}");
        assert_eq!(ids.len(), 11, "{name}");
        assert!(
            c.checks().iter().all(|c| c.mandatory),
            "{name}: every check is mandatory"
        );
        // (a) the protected diff denies exactly the list, and allows everything else
        match &c.checks()[0].kind {
            CheckKind::DiffOnly { allow, deny } => {
                assert!(allow.is_empty(), "{name}");
                assert_eq!(
                    globs_of(deny),
                    expected_deny,
                    "{name}: regenerate with dogfood/gen-contracts.sh"
                );
            }
            other => panic!("{name}: {other:?}"),
        }
        // the scope check allows only what the template says
        match &c.checks()[10].kind {
            CheckKind::DiffOnly { allow, deny } => {
                assert!(deny.is_empty());
                assert!(!allow.is_empty() && allow.len() <= 2, "{name}");
            }
            other => panic!("{name}: {other:?}"),
        }
        // (b) to (e): fmt, clippy, architecture and the layered tests are commands of the wrapper;
        // the scripts are those of the tools directory; none may be a program of the repository
        for (id, wants) in [
            ("fmt", "cargo fmt --all -- --check"),
            ("clippy", "-D warnings"),
            ("architecture", "--test architecture"),
            ("layers", "scripts/test-layers.sh"),
        ] {
            match &c
                .checks()
                .iter()
                .find(|c| c.id.as_str() == id)
                .unwrap()
                .kind
            {
                CheckKind::Command {
                    program,
                    args,
                    expect_exit,
                    ..
                } => {
                    assert_eq!(program, "/opt/dogfood/container-run", "{name}/{id}");
                    assert_eq!(
                        args.len(),
                        1,
                        "{name}/{id}: one shell command for the wrapper"
                    );
                    assert!(args[0].contains(wants), "{name}/{id}: {:?}", args[0]);
                    assert_eq!(*expect_exit, 0);
                }
                other => panic!("{name}/{id}: {other:?}"),
            }
        }
        match &c.checks()[5].kind {
            CheckKind::Command { program, args, .. } => {
                assert_eq!(program, "/opt/dogfood/bin/check-secret-tree.sh");
                assert!(args.is_empty());
            }
            other => panic!("{name}: {other:?}"),
        }
        match &c.checks()[4].kind {
            CheckKind::Command { program, args, .. } => {
                assert_eq!(program, "/opt/dogfood/bin/check-conflict-markers.sh");
                assert_eq!(args, &vec!["--tree".to_string()]);
            }
            other => panic!("{name}: {other:?}"),
        }
        for id in ["protected-script", "docs-claims"] {
            match &c
                .checks()
                .iter()
                .find(|c| c.id.as_str() == id)
                .unwrap()
                .kind
            {
                CheckKind::Command { program, args, .. } => {
                    assert!(program.starts_with("/opt/dogfood/bin/check-"), "{program}");
                    assert_eq!(
                        args,
                        &vec![
                            "--repo".to_string(),
                            "/opt/dogfood/clone".to_string(),
                            "0123456789abcdef0123456789abcdef01234567..fleet/neo-roadmap/a-task"
                                .to_string()
                        ],
                        "{name}/{id}: the range is the task's branch since the run's base"
                    );
                }
                other => panic!("{name}/{id}: {other:?}"),
            }
        }
        // (f) Cargo.lock unchanged: a sha256 assertion on the file, plus the protected diff above
        match &c.checks()[2].kind {
            CheckKind::File { path, assertion } => {
                assert_eq!(path, "Cargo.lock");
                assert!(matches!(assertion, FileAssertion::Sha256(h) if h.len() == 64));
            }
            other => panic!("{name}: {other:?}"),
        }
        assert!(
            c.canonical().len() < 32 * 1024,
            "{name}: below the contract cap"
        );
    }
    // the scope of each template
    let single = contract_of(&rendered(&read("dogfood/contracts/single-file.json"))).unwrap();
    match &single.checks()[10].kind {
        CheckKind::DiffOnly { allow, .. } => assert_eq!(globs_of(allow), ["docs/domain.md"]),
        other => panic!("{other:?}"),
    }
    let docs = contract_of(&rendered(&read("dogfood/contracts/docs-only.json"))).unwrap();
    match &docs.checks()[10].kind {
        CheckKind::DiffOnly { allow, .. } => assert_eq!(globs_of(allow), ["README.md", "docs/**"]),
        other => panic!("{other:?}"),
    }
    let krate = contract_of(&rendered(&read("dogfood/contracts/crate-src.json"))).unwrap();
    match &krate.checks()[10].kind {
        CheckKind::DiffOnly { allow, .. } => {
            assert_eq!(
                globs_of(allow),
                [
                    "crates/neo-knowledge/src/**",
                    "crates/neo-knowledge/tests/**"
                ]
            )
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_templates_share_the_fragment_and_the_fragment_is_a_file_of_its_own() {
    let fragment: serde_json::Value =
        serde_json::from_str(&read("dogfood/contracts/common-checks.json")).unwrap();
    let fragment = fragment.as_array().unwrap();
    assert_eq!(fragment.len(), COMMON_IDS.len());
    for (check, id) in fragment.iter().zip(COMMON_IDS) {
        assert_eq!(check["id"], id);
    }
    for name in TEMPLATES {
        // `@ALSO@` is not JSON until it is rendered (empty or `, "a"`)
        let doc: serde_json::Value = serde_json::from_str(
            &read(&format!("dogfood/contracts/{name}.json")).replace("@ALSO@", ""),
        )
        .unwrap();
        assert_eq!(doc["version"], 1);
        let checks = doc["checks"].as_array().unwrap();
        assert_eq!(
            &checks[..10],
            &fragment[..],
            "{name}: run dogfood/gen-contracts.sh"
        );
    }
}

/// A dependent's branch carries its dependencies' work, so its scope check must allow it too: the
/// `@ALSO@` slot of the three scope templates takes `, "a", "b"` and nothing else changes.
#[test]
fn the_scope_of_a_template_has_a_slot_for_what_its_dependencies_changed() {
    for name in TEMPLATES {
        let raw = read(&format!("dogfood/contracts/{name}.json"));
        assert_eq!(raw.matches("@ALSO@").count(), 1, "{name}");
        let without = contract_of(&rendered(&raw)).unwrap();
        let with = contract_of(&rendered(&raw).replace(
            "\"]\n    }\n  ]",
            "\", \"crates/x/src/a.rs\", \"docs/y.md\"]\n    }\n  ]",
        ))
        .unwrap();
        let scope = |c: &VerificationContract| match &c.checks().last().unwrap().kind {
            CheckKind::DiffOnly { allow, .. } => globs_of(allow),
            other => panic!("{other:?}"),
        };
        let (a, b) = (scope(&without), scope(&with));
        assert_eq!(b[..a.len()], a[..], "{name}: the own scope comes first");
        assert_eq!(b[a.len()..], ["crates/x/src/a.rs", "docs/y.md"], "{name}");
        // and the other nine-plus checks are untouched
        assert_eq!(without.checks().len(), with.checks().len());
    }
    // the flaky-test template is for one file and has no slot
    assert!(!read("dogfood/contracts/flaky-test.json").contains("@ALSO@"));
}

/// The flaky-test template is a reviewed exception for one file: no glob deny check (it could not
/// express "all of the list but this file"), the script check with `--except`, a test-only check, the
/// test still there and a stress loop. Everything else is the shared fragment.
#[test]
fn the_flaky_test_template_is_a_reviewed_exception_for_exactly_one_file() {
    let raw = read("dogfood/contracts/flaky-test.json");
    assert!(
        contract_of(&raw).is_err(),
        "an unrendered template is refused"
    );
    let c = contract_of(&rendered(&raw)).unwrap();
    let ids: Vec<&str> = c.checks().iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "protected-script",
            "lock-unchanged",
            "docs-claims",
            "conflict-markers",
            "secret-scan",
            "fmt",
            "clippy",
            "architecture",
            "layers",
            "scope",
            "kept",
            "test-only",
            "stress"
        ]
    );
    assert!(c.checks().iter().all(|c| c.mandatory));
    let get = |id: &str| c.checks().iter().find(|c| c.id.as_str() == id).unwrap();
    // the exception names the file, once, and nothing else
    match &get("protected-script").kind {
        CheckKind::Command { program, args, .. } => {
            assert_eq!(program, "/opt/dogfood/bin/check-protected.sh");
            assert_eq!(
                args,
                &[
                    "--repo",
                    "/opt/dogfood/clone",
                    "--except",
                    "docs/domain.md",
                    "0123456789abcdef0123456789abcdef01234567..fleet/neo-roadmap/a-task"
                ]
            );
        }
        other => panic!("{other:?}"),
    }
    match &get("scope").kind {
        CheckKind::DiffOnly { allow, deny } => {
            assert_eq!(globs_of(allow), ["docs/domain.md"]);
            assert!(deny.is_empty());
        }
        other => panic!("{other:?}"),
    }
    match &get("kept").kind {
        CheckKind::File { path, assertion } => {
            assert_eq!(path, "docs/domain.md");
            assert!(
                matches!(assertion, FileAssertion::Contains(t) if t == "fn a_reply_is_delivered")
            );
        }
        other => panic!("{other:?}"),
    }
    match &get("test-only").kind {
        CheckKind::Command { program, args, .. } => {
            assert_eq!(program, "/opt/dogfood/bin/check-test-only.sh");
            assert_eq!(args[..2], ["--repo", "/opt/dogfood/clone"]);
            assert_eq!(args[2..4], ["--file", "docs/domain.md"]);
        }
        other => panic!("{other:?}"),
    }
    match &get("stress").kind {
        CheckKind::Command {
            program,
            args,
            timeout_ms,
            ..
        } => {
            assert_eq!(program, "/opt/dogfood/container-run");
            assert_eq!(args.len(), 1);
            let cmd = &args[0];
            assert!(cmd.contains("--no-run"), "the binary is built once");
            assert!(
                cmd.contains("while :; do :; done"),
                "the machine is loaded: {cmd}"
            );
            assert!(
                cmd.contains("cargo test -p fleet-infra --lib server::tests::a_reply_is_delivered --offline --locked"),
                "{cmd}"
            );
            assert!(cmd.contains("-lt 25"), "25 runs in a row: {cmd}");
            assert!(cmd.contains("exit 1"), "one failure fails the check: {cmd}");
            assert!(*timeout_ms <= 900_000);
        }
        other => panic!("{other:?}"),
    }
    // the rest is the fragment: same JSON as in the other templates, bar the protected-script arguments
    let fragment: serde_json::Value =
        serde_json::from_str(&read("dogfood/contracts/common-checks.json")).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let checks = doc["checks"].as_array().unwrap();
    let fragment = fragment.as_array().unwrap();
    for i in 2..10 {
        assert_eq!(checks[i - 1], fragment[i], "{}", fragment[i]["id"]);
    }
    assert_eq!(
        checks[0]["args"],
        serde_json::json!([
            "--repo",
            "@REPO@",
            "--except",
            "@FILE_PATH@",
            "@BASE@..fleet/@RUN@/@TASK@"
        ])
    );
    assert!(
        !raw.contains("\"protected-diff\""),
        "the glob deny check cannot carry an exception"
    );
    assert!(c.canonical().len() < 32 * 1024);
}

/// The stress check is the pattern of the trials that worked: one named test, many times, under load.
#[test]
fn the_stress_check_runs_one_named_test_and_only_through_the_wrapper() {
    let cmd = read("dogfood/contracts/flaky-test.json");
    for placeholder in ["@PACKAGE@", "@TEST_FILTER@", "@TEST_NAME@", "@FILE_PATH@"] {
        assert!(cmd.contains(placeholder), "{placeholder}");
    }
    // the same pattern as the trials that worked (dogfood/trials): eight busy loops, 25 runs
    let trial = read("dogfood/trials/contract.flaky-jobs-test.json");
    for needle in [
        "timeout 280 sh -c",
        "while :; do :; done",
        "-lt 25",
        "--no-run",
        "\\\"1 passed\\\"",
    ] {
        assert!(trial.contains(needle), "the trial has {needle}");
        assert!(cmd.contains(needle), "the template has {needle}");
    }
}

#[test]
fn the_trial_files_are_kept_as_loadable_plans_and_parseable_contracts() {
    for name in ["flaky-server-test", "flaky-jobs-test"] {
        let plan = load_plan(&root().join(format!("dogfood/trials/plan.{name}.toml"))).unwrap();
        assert_eq!(
            plan.get("tasks").and_then(Value::as_array).unwrap().len(),
            1
        );
        let contract = read(&format!("dogfood/trials/contract.{name}.json"));
        assert!(!contract.contains("/Users/"), "no personal path");
        let c = contract_of(&contract).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(c.checks().iter().any(|c| c.id.as_str() == "stress"));
        assert!(c.checks().iter().any(|c| c.id.as_str() == "kept"));
    }
    let text = read("dogfood/trials/README.md");
    assert!(text.contains("could not run shell commands"));
}

#[test]
fn the_human_only_contract_can_never_pass() {
    let text = read("dogfood/contracts/human-only.json");
    let c = contract_of(&text).unwrap();
    assert_eq!(c.checks().len(), 2);
    assert!(c.checks().iter().all(|c| c.mandatory));
    let (diff, file) = (&c.checks()[0], &c.checks()[1]);
    let marker = match &file.kind {
        CheckKind::File { path, assertion } => {
            assert!(matches!(assertion, FileAssertion::Exists));
            path.clone()
        }
        other => panic!("{other:?}"),
    };
    match &diff.kind {
        CheckKind::DiffOnly { allow, deny } => {
            assert!(allow.is_empty());
            assert_eq!(globs_of(deny), ["**"]);
        }
        other => panic!("{other:?}"),
    }
    assert!(
        !tree().contains(&marker),
        "{marker} must never exist: the contract could pass"
    );
    let changed = |paths: &[&str]| Observation::Changed {
        paths: paths.iter().map(|p| p.to_string()).collect(),
        truncated: false,
    };
    let diff_passes = |paths: &[&str]| judge(diff, &changed(paths)).verdict.as_str() == "passed";
    let file_passes =
        |obs: FileObservation| judge(file, &Observation::File(obs)).verdict.as_str() == "passed";
    // no change at all: the file is missing, so the file check fails
    assert!(diff_passes(&[]));
    assert!(!file_passes(FileObservation::Missing));
    // the file exists: it was created by a change, which the diff check refuses, whatever else changed
    assert!(file_passes(FileObservation::Regular));
    assert!(!diff_passes(&[&marker]));
    assert!(!diff_passes(&["README.md"]));
    assert!(!diff_passes(&["a/b/c/d.rs", &marker]));
    // a diff that was cut, or too large, is an error and not a pass
    let cut = Observation::Changed {
        paths: vec![],
        truncated: true,
    };
    assert_ne!(judge(diff, &cut).verdict.as_str(), "passed");
    // so no pair of observations passes both checks: either the file is missing (check 2 fails)
    // or the file exists and, to exist, was added by the commits (check 1 fails)
}

// ---- the plan and the goals ------------------------------------------------------------------------

struct Goal {
    id: String,
    parent: String,
    task: String,
    template: String,
    params: BTreeMap<String, String>,
}

fn goals() -> Vec<Goal> {
    read("dogfood/goals.neo-roadmap.tsv")
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let cols: Vec<&str> = l.split('\t').collect();
            assert_eq!(cols.len(), 7, "{l}");
            for (i, c) in cols.iter().enumerate() {
                assert!(!c.is_empty() && !c.contains('|'), "column {i} of {l}");
            }
            let params = if cols[4] == "-" {
                BTreeMap::new()
            } else {
                cols[4]
                    .split(',')
                    .map(|kv| {
                        let (k, v) = kv.split_once('=').unwrap();
                        (k.to_string(), v.to_string())
                    })
                    .collect()
            };
            Goal {
                id: cols[0].into(),
                parent: cols[1].into(),
                task: cols[2].into(),
                template: cols[3].into(),
                params,
            }
        })
        .collect()
}

struct PlanTask {
    id: String,
    title: String,
    spec: String,
    worker: String,
    after: Vec<String>,
}

fn plan() -> (Value, Vec<PlanTask>) {
    let doc = load_plan(&root().join("dogfood/plan.neo-roadmap.toml")).unwrap();
    let tasks = doc
        .get("tasks")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .map(|t| {
            let s = |k: &str| t.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            PlanTask {
                id: s("id"),
                title: s("title"),
                spec: s("spec"),
                worker: s("worker"),
                after: t
                    .get("after")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect())
                    .unwrap_or_default(),
            }
        })
        .collect();
    (doc, tasks)
}

#[test]
fn the_plan_is_a_valid_dag_whose_every_task_names_its_worker_and_says_when_it_is_human_only() {
    let (doc, tasks) = plan();
    let run = doc.get("new_run").unwrap();
    assert_eq!(run.get("id").and_then(Value::as_str), Some("neo-roadmap"));
    assert_eq!(run.get("max_parallel").and_then(Value::as_u64), Some(1));
    assert!(tasks.len() >= 30, "{} tasks", tasks.len());
    let nodes: Vec<Node> = tasks
        .iter()
        .map(|t| Node {
            id: TaskId::parse(&t.id).unwrap(),
            deps: t.after.iter().map(|d| TaskId::parse(d).unwrap()).collect(),
        })
        .collect();
    order_new_tasks(&BTreeSet::new(), &nodes)
        .expect("no cycle, no unknown dependency, no duplicate");
    let mut human = 0;
    for t in &tasks {
        assert!(
            t.spec.len() > 100 && t.spec.len() <= MAX_SPEC,
            "{}: spec size",
            t.id
        );
        assert!(t.title.len() <= 200 && !t.title.is_empty());
        match t.worker.as_str() {
            "human-only" => {
                human += 1;
                assert!(t.title.contains("HUMAN-ONLY"), "{}: title", t.id);
                assert!(
                    t.spec.starts_with("\nHUMAN-ONLY") || t.spec.starts_with("HUMAN-ONLY"),
                    "{}: spec",
                    t.id
                );
            }
            "agent" => {
                assert!(
                    !t.title.contains("HUMAN-ONLY") && !t.spec.contains("HUMAN-ONLY"),
                    "{}",
                    t.id
                );
                // an agent task is told which files it may change and that protected paths are off limits
                assert!(
                    t.spec.contains("Change ONLY") || t.spec.contains("Change only"),
                    "{}",
                    t.id
                );
                // the agent runs in write mode: it cannot run commands, fleet commits and checks
                assert!(
                    t.spec.contains("You cannot run commands")
                        && t.spec.contains("Commit nothing yourself"),
                    "{}: the spec must say that the agent edits blind",
                    t.id
                );
            }
            other => panic!("{}: worker {other}", t.id),
        }
    }
    assert!(human >= 8, "{human} human-only tasks");
    // the starters come first: every task but themselves waits behind all three
    let starters: BTreeSet<&str> = tasks
        .iter()
        .filter(|t| t.id.starts_with("starter-"))
        .map(|t| t.id.as_str())
        .collect();
    assert_eq!(starters.len(), 3);
    for t in tasks.iter().filter(|t| !t.id.starts_with("starter-")) {
        let mut reach: BTreeSet<&str> = BTreeSet::new();
        let mut stack: Vec<&str> = t.after.iter().map(String::as_str).collect();
        while let Some(id) = stack.pop() {
            if reach.insert(id) {
                let dep = tasks.iter().find(|x| x.id == id).unwrap();
                stack.extend(dep.after.iter().map(String::as_str));
            }
        }
        assert!(
            starters.is_subset(&reach),
            "{} does not wait for the starters",
            t.id
        );
    }
}

#[test]
fn every_task_has_exactly_one_goal_with_the_contract_its_worker_requires() {
    let (_, tasks) = plan();
    let goals = goals();
    let mut ids = BTreeSet::new();
    for g in &goals {
        assert!(ids.insert(g.id.clone()), "{} twice", g.id);
        assert!(g.id.len() <= 40 && g.id.chars().next().unwrap().is_ascii_alphanumeric());
        assert!(g
            .id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'));
    }
    let milestones: BTreeSet<&str> = goals
        .iter()
        .filter(|g| g.task == "-")
        .map(|g| g.id.as_str())
        .collect();
    assert!(milestones.len() >= 5);
    for m in goals.iter().filter(|g| g.task == "-") {
        assert_eq!(m.template, "-");
        assert_eq!(m.parent, "-", "milestones are roots");
    }
    let entries = protected();
    let mut covered = BTreeSet::new();
    for g in goals.iter().filter(|g| g.task != "-") {
        assert!(
            milestones.contains(g.parent.as_str()),
            "{}: parent {}",
            g.id,
            g.parent
        );
        assert!(
            covered.insert(g.task.clone()),
            "{}: a second goal for the task",
            g.task
        );
        let t = tasks
            .iter()
            .find(|t| t.id == g.task)
            .unwrap_or_else(|| panic!("{}: no such task", g.task));
        let template_file = format!("dogfood/contracts/{}.json", g.template);
        assert!(root().join(&template_file).exists(), "{template_file}");
        if t.worker == "human-only" {
            assert_eq!(
                g.template, "human-only",
                "{}: a human-only task must have the contract that cannot pass",
                g.task
            );
            assert!(g.params.is_empty());
        } else {
            assert_ne!(g.template, "human-only", "{}", g.task);
            match g.template.as_str() {
                "single-file" => {
                    assert_eq!(g.params.len(), 1, "{}", g.task);
                    let file = &g.params["FILE_PATH"];
                    assert!(
                        !is_protected(&entries, file),
                        "{}: {file} is protected",
                        g.task
                    );
                    assert!(
                        t.spec.contains(file.as_str()),
                        "{}: the spec does not name {file}",
                        g.task
                    );
                }
                "crate-src" => {
                    let c = &g.params["CRATE"];
                    assert_eq!(g.params.len(), 1);
                    for sample in [
                        format!("crates/{c}/src/x.rs"),
                        format!("crates/{c}/tests/x.rs"),
                    ] {
                        assert!(!is_protected(&entries, &sample), "{sample} is protected");
                    }
                    assert!(t.spec.contains(&format!("crates/{c}/")), "{}", g.task);
                }
                "docs-only" => assert!(g.params.is_empty()),
                other => panic!("{}: {other}", g.task),
            }
        }
    }
    let all: BTreeSet<String> = tasks.iter().map(|t| t.id.clone()).collect();
    assert_eq!(
        covered, all,
        "the goals cover exactly the tasks of the plan"
    );
}

// ---- the daemon configuration ----------------------------------------------------------------------

#[test]
fn the_daemon_template_is_accepted_by_the_real_reader_and_is_strict() {
    let secretish = |n: &str| {
        let up = n.to_ascii_uppercase();
        ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"]
            .iter()
            .any(|w| up.contains(w))
    };
    let cfg = load_config(
        &root().join("dogfood/fleet.dogfood.example.toml"),
        vec![("PATH".into(), "/usr/bin".into())],
    )
    .unwrap_or_else(|e| panic!("{e:?}"));
    let app = &cfg.app;
    assert_eq!(app.max_sessions, 1);
    assert_eq!(app.max_retries, 1);
    assert!(app.merge_needs_approval, "merge stays a human approval");
    assert!(
        app.external_commands,
        "approved commands stay a human decision"
    );
    assert!(app.approval_ttl_ms <= 15 * 60 * 1000);
    assert_eq!(
        app.default_worker.as_ref().map(|w| w.as_str()),
        Some("agent")
    );
    assert!(
        app.env.allowed().iter().all(|n| !secretish(n)),
        "{:?}",
        app.env.allowed()
    );
    let agent = app
        .workers
        .iter()
        .find(|(n, _)| n.as_str() == "agent")
        .unwrap()
        .1;
    assert_eq!(agent.command, "/Users/kienpt/.local/bin/omp");
    assert!(
        agent.env_pass.is_empty(),
        "no secret variable is passed through"
    );
    assert!(agent.env_set.iter().all(|(k, _)| !secretish(k)));
    assert!(agent.timeout_ms.unwrap() <= 30 * 60 * 1000);
    assert!(agent.idle_timeout_ms.unwrap() <= agent.timeout_ms.unwrap());
    let args = &agent.args;
    for flag in [
        "-p",
        "--no-session",
        "--no-extensions",
        "--no-skills",
        "--no-rules",
    ] {
        assert!(args.iter().any(|a| a == flag), "{flag}");
    }
    let value_of = |flag: &str| {
        let i = args
            .iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("{flag}"));
        args[i + 1].clone()
    };
    // the arguments that were run for real: no profile (an isolated profile is an optional extra, never another name)
    if args.iter().any(|a| a == "--profile") {
        assert_eq!(value_of("--profile"), "fleet-dogfood");
    }
    assert_eq!(value_of("--approval-mode"), "write");
    assert_eq!(value_of("--max-time"), "25m");
    assert_eq!(value_of("--cwd"), "{worktree}");
    assert_eq!(args.last().unwrap(), "@{brief_file}");
    for banned in [
        "--auto-approve",
        "--yolo",
        "yolo",
        "--api-key",
        "--approval-mode=yolo",
    ] {
        assert!(!args.iter().any(|a| a == banned), "{banned}");
    }
    assert!(
        !args.iter().any(|a| a.contains("{brief}")),
        "the brief goes through its file, not the command line"
    );
    assert_eq!(
        (app.author.name.as_str(), app.author.email.as_str()),
        ("fleet", "fleet@localhost"),
        "the commits of fleet keep the default identity the runbook describes"
    );
    let human = app
        .workers
        .iter()
        .find(|(n, _)| n.as_str() == "human-only")
        .unwrap()
        .1;
    assert_eq!(human.command, "/bin/sh");
    assert!(human.env_pass.is_empty());
    assert!(human.timeout_ms.unwrap() <= 60_000);
    // gates are programs outside the repository
    assert!(!app.gates.is_empty());
    for g in &app.gates {
        assert!(g.command.starts_with('/'), "{}", g.name);
        assert!(g.env_pass.is_empty());
    }
    // the template has no secret-looking assignment and no key-shaped text
    let text = read("dogfood/fleet.dogfood.example.toml");
    for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
        assert!(!line.to_ascii_lowercase().contains("api_key"), "{line}");
    }
}

// ---- the documents -----------------------------------------------------------------------------------

#[test]
fn no_contract_runs_the_remote_layer_and_the_documents_say_why() {
    // the tests against a real sshd need the fleet-ssh image: a contract runs in fleet-dev, where
    // scripts/test-layers.sh skips that layer, and none of them may ask for the sshd tests by name
    for name in [
        "single-file",
        "docs-only",
        "crate-src",
        "flaky-test",
        "human-only",
        "common-checks",
    ] {
        let doc: serde_json::Value =
            serde_json::from_str(&rendered(&read(&format!("dogfood/contracts/{name}.json"))))
                .unwrap();
        let checks = match &doc {
            serde_json::Value::Array(a) => a.clone(),
            other => other["checks"].as_array().unwrap().clone(),
        };
        for c in checks.iter().filter(|c| c["kind"] == "command") {
            let line = format!("{} {}", c["program"], c["args"]);
            for banned in [
                "ssh-env",
                "fleet-ssh",
                "ssh_remote",
                "e2e_remote",
                "FLEET_SSH_TESTS",
            ] {
                assert!(
                    !line.contains(banned),
                    "{name}: {line} runs the remote layer"
                );
            }
        }
    }
    let layers = read("scripts/test-layers.sh");
    assert!(
        layers.contains("layer remote"),
        "the remote layer moved: update the documents"
    );
    assert!(
        layers.contains("/usr/sbin/sshd"),
        "the remote layer no longer depends on sshd"
    );
    // the wrapper's image has no sshd, so the layer is skipped inside a check
    let wrapper = "dogfood/container-run.example.sh";
    let text = read(wrapper);
    assert!(text.contains("image=fleet-dev"), "{wrapper}");
    assert!(!text.contains("image=fleet-ssh"), "{wrapper}");
    let readme = read("dogfood/contracts/README.md");
    assert!(
        readme.contains("The `remote` layer is not part of the contract"),
        "contracts/README.md must document the remote layer"
    );
    let runbook = read("dogfood/RUNBOOK.md");
    assert!(
        runbook.contains("`remote` layer"),
        "RUNBOOK.md must say that the remote layer is not part of a contract"
    );
}

#[test]
fn the_documents_name_every_part_and_every_protected_path() {
    let entries = protected();
    let doc = read("dogfood/PROTECTED_PATHS.md");
    for e in &entries {
        assert!(
            doc.contains(&format!("`{}`", e.path)),
            "PROTECTED_PATHS.md lacks `{}`",
            e.path
        );
    }
    let (_, tasks) = plan();
    let human_md = read("dogfood/human-only.md");
    for t in tasks.iter().filter(|t| t.worker == "human-only") {
        assert!(
            human_md.contains(&format!("`{}`", t.id)),
            "human-only.md lacks {}",
            t.id
        );
    }
    let runbook = read("dogfood/RUNBOOK.md");
    for part in [
        "check-protected.sh",
        "check-docs-claims.sh",
        "check-secret-tree.sh",
        "check-conflict-markers.sh",
        "check-test-only.sh",
        "install-tools.sh",
        "render-contract.sh",
        "apply-goals.sh",
        "plan-without.sh",
        "deps-refresh.sh",
        "gen-contracts.sh",
        "container-run.example.sh",
        "container-run.inside.sh",
        "fleet.dogfood.example.toml",
        "plan.neo-roadmap.toml",
        "goals.neo-roadmap.tsv",
        "human-only.md",
        "PROTECTED_PATHS.md",
        "contracts/README.md",
        "trials/README.md",
        "fake-agent.sh",
        "git cherry-pick -n",
        "fleet <fleet@localhost>",
    ] {
        assert!(runbook.contains(part), "RUNBOOK.md never mentions {part}");
    }
    let contracts_readme = read("dogfood/contracts/README.md");
    for name in [
        "single-file",
        "docs-only",
        "crate-src",
        "flaky-test",
        "human-only",
        "common-checks",
    ] {
        assert!(
            contracts_readme.contains(&format!("{name}.json")),
            "contracts/README.md lacks {name}"
        );
    }
}

#[test]
fn the_scripts_are_posix_sh_and_the_ones_that_are_run_are_executable() {
    use std::os::unix::fs::PermissionsExt;
    let files = tree();
    let scripts: Vec<&String> = files
        .iter()
        .filter(|f| f.starts_with("dogfood/") && f.ends_with(".sh"))
        .collect();
    assert!(scripts.len() >= 18, "{scripts:?}");
    for s in scripts {
        let text = read(s);
        assert_eq!(text.lines().next(), Some("#!/bin/sh"), "{s}");
        for banned in ["#!/bin/bash", "[[ ", "$(( ", " <<< ", "declare -", "local "] {
            // arithmetic with spaces is fine in sh; only bashisms that sh rejects are listed
            if banned == "$(( " {
                continue;
            }
            assert!(!text.contains(banned), "{s} uses {banned}");
        }
        let mode = fs::metadata(root().join(s)).unwrap().permissions().mode();
        let is_test_lib = s.ends_with("/lib.sh");
        assert!(is_test_lib || mode & 0o100 != 0, "{s} must be executable");
    }
}
