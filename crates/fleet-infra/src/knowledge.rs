//! The human-written knowledge file (layer 5 of a context pack): facts with a source.
//!
//! ```toml
//! [[fact]]
//! id = "build-tool"
//! claim = "The workspace builds with cargo only."
//! source = "Cargo.toml at 1a2b3c4"
//! freshness = "checked 2026-01-05"      # optional
//! paths = ["crates/**"]                 # optional: the facts is about these paths
//! ```
//!
//! or `{"facts": [{"id": "...", "claim": "...", "source": "..."}]}` in a `.json` file. The schema
//! is strict: unknown fields, a missing source, duplicate ids, control characters and text that
//! looks like a secret are errors. An error names the file and the fact, never the text.

use crate::fsx;
use fleet_app::ports::{KnowledgeEntry, KnowledgeSource};
use fleet_domain::error::{FleetError, Result};
use fleet_domain::sanitize::head_str;
use neo_context::input::Fact;
use neo_context::secret;
use neo_verify::glob::Glob;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_FACTS: usize = 64;
const MAX_ID: usize = 40;
const MAX_CLAIM: usize = 600;
const MAX_SOURCE: usize = 300;
const MAX_FRESHNESS: usize = 300;
const MAX_PATHS: usize = 8;
const MAX_GLOB: usize = 256;
/// Parser messages are shortened: they may quote part of the offending text.
const MAX_PARSER_MESSAGE: usize = 120;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFact {
    id: String,
    claim: String,
    source: String,
    freshness: Option<String>,
    paths: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlFile {
    #[serde(default)]
    fact: Vec<RawFact>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonFile {
    #[serde(default)]
    facts: Vec<RawFact>,
}

pub struct FileKnowledge {
    path: Option<PathBuf>,
}

impl FileKnowledge {
    /// `None`: there is no knowledge file and no entries.
    pub fn new(path: Option<PathBuf>) -> FileKnowledge {
        FileKnowledge { path }
    }
}

impl KnowledgeSource for FileKnowledge {
    fn entries(&self) -> Result<Vec<KnowledgeEntry>> {
        match &self.path {
            None => Ok(Vec::new()),
            Some(path) => load(path),
        }
    }
}

fn file_label(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    head_str(&name, 80).to_string()
}

fn load(path: &Path) -> Result<Vec<KnowledgeEntry>> {
    let label = file_label(path);
    let ext = path.extension().and_then(|e| e.to_str());
    if !matches!(ext, Some("toml" | "json")) {
        return Err(FleetError::invalid(format!(
            "knowledge file {label}: the extension must be .toml or .json"
        )));
    }
    let bytes = fsx::read_regular(path, MAX_FILE_BYTES)
        .map_err(|e| FleetError::invalid(format!("knowledge file {label} {e}")))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| FleetError::invalid(format!("knowledge file {label} is not valid UTF-8")))?;
    let raw = if ext == Some("toml") {
        toml::from_str::<TomlFile>(&text)
            .map(|f| f.fact)
            .map_err(|e| {
                let line = e
                    .span()
                    .map(|s| text.get(..s.start).unwrap_or("").matches('\n').count() + 1);
                let at = line.map(|l| format!(" (line {l})")).unwrap_or_default();
                FleetError::invalid(format!(
                    "knowledge file {label}{at}: {}",
                    head_str(e.message(), MAX_PARSER_MESSAGE)
                ))
            })?
    } else {
        serde_json::from_str::<JsonFile>(&text)
            .map(|f| f.facts)
            .map_err(|e| {
                FleetError::invalid(format!(
                    "knowledge file {label} (line {}): {}",
                    e.line(),
                    head_str(&e.to_string(), MAX_PARSER_MESSAGE)
                ))
            })?
    };
    convert(&label, raw)
}

fn valid_id(id: &str) -> bool {
    let mut bytes = id.bytes();
    let first_ok = bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    first_ok
        && id.len() <= MAX_ID
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn convert(label: &str, raw: Vec<RawFact>) -> Result<Vec<KnowledgeEntry>> {
    if raw.len() > MAX_FACTS {
        return Err(FleetError::invalid(format!(
            "knowledge file {label} has more than {MAX_FACTS} facts"
        )));
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(raw.len());
    for (index, f) in raw.into_iter().enumerate() {
        if !valid_id(&f.id) {
            return Err(FleetError::invalid(format!(
                "knowledge file {label}: fact number {} has an invalid id (lowercase letters, \
                 digits, `-` and `_`, at most {MAX_ID} characters)",
                index + 1
            )));
        }
        let bad = |what: &str| {
            FleetError::invalid(format!("knowledge file {label}: fact {}: {what}", f.id))
        };
        if !seen.insert(f.id.clone()) {
            return Err(bad("the id is used twice"));
        }
        let freshness = f.freshness.clone().unwrap_or_default();
        let paths = f.paths.clone().unwrap_or_default();
        text_field("claim", &f.claim, MAX_CLAIM, true).map_err(|w| bad(&w))?;
        text_field("source", &f.source, MAX_SOURCE, true).map_err(|w| bad(&w))?;
        text_field("freshness", &freshness, MAX_FRESHNESS, false).map_err(|w| bad(&w))?;
        if paths.len() > MAX_PATHS {
            return Err(bad(&format!("more than {MAX_PATHS} paths")));
        }
        for p in &paths {
            text_field("a path", p, MAX_GLOB, true).map_err(|w| bad(&w))?;
            if Glob::new(p).is_err() {
                return Err(bad("a path is not a valid glob"));
            }
        }
        let all = [&f.claim, &f.source, &freshness];
        for text in all.into_iter().chain(paths.iter()) {
            if let Some(rule) = secret::scan_text(text) {
                return Err(bad(&format!(
                    "refused, its text matches the secret rule {rule}"
                )));
            }
        }
        out.push(KnowledgeEntry {
            fact: Fact {
                id: f.id.clone(),
                claim: f.claim.clone(),
                source: f.source.clone(),
                freshness,
            },
            paths,
        });
    }
    Ok(out)
}

fn text_field(
    name: &str,
    text: &str,
    max: usize,
    required: bool,
) -> std::result::Result<(), String> {
    if required && text.trim().is_empty() {
        return Err(format!("{name} is empty"));
    }
    if text.len() > max {
        return Err(format!("{name} is longer than {max} bytes"));
    }
    if text.chars().any(char::is_control) {
        return Err(format!("{name} contains control characters"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::os::unix::fs::symlink;

    fn load_text(name: &str, body: &str) -> Result<Vec<KnowledgeEntry>> {
        let t = TempDir::new("knowledge");
        let path = t.write(name, body);
        FileKnowledge::new(Some(path)).entries()
    }

    fn err_text(name: &str, body: &str) -> String {
        load_text(name, body).unwrap_err().message
    }

    #[test]
    fn no_path_means_no_entries() {
        assert!(FileKnowledge::new(None).entries().unwrap().is_empty());
    }

    #[test]
    fn reads_toml_facts_in_file_order() {
        let e = load_text(
            "k.toml",
            r#"
[[fact]]
id = "build"
claim = "Builds with cargo."
source = "Cargo.toml at 1a2b3c4"
freshness = "checked 2026-01-05"
paths = ["crates/**", "Cargo.toml"]

[[fact]]
id = "a_second-1"
claim = "Second."
source = "Ana"
"#,
        )
        .unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].fact.id, "build");
        assert_eq!(e[0].fact.freshness, "checked 2026-01-05");
        assert_eq!(e[0].paths, vec!["crates/**", "Cargo.toml"]);
        assert_eq!(e[1].fact.id, "a_second-1");
        assert_eq!(e[1].fact.freshness, "");
        assert!(e[1].paths.is_empty());
    }

    #[test]
    fn reads_json_facts_and_empty_files() {
        let e = load_text(
            "k.json",
            r#"{"facts":[{"id":"x","claim":"c","source":"s","paths":["a/*.rs"]}]}"#,
        )
        .unwrap();
        assert_eq!(e[0].paths, vec!["a/*.rs"]);
        assert!(load_text("k.toml", "").unwrap().is_empty());
        assert!(load_text("k.json", "{}").unwrap().is_empty());
    }

    #[test]
    fn a_fact_without_a_source_is_an_error() {
        let m = err_text("k.toml", "[[fact]]\nid = \"x\"\nclaim = \"c\"\n");
        assert!(m.contains("source"), "{m}");
        assert!(m.contains("k.toml"), "{m}");
        let m = err_text("k.json", r#"{"facts":[{"id":"x","claim":"c"}]}"#);
        assert!(m.contains("source"), "{m}");
        let m = err_text(
            "k.toml",
            "[[fact]]\nid = \"x\"\nclaim = \"c\"\nsource = \" \"\n",
        );
        assert!(m.contains("fact x") && m.contains("source is empty"), "{m}");
    }

    #[test]
    fn unknown_fields_and_sections_are_errors() {
        assert!(err_text(
            "k.toml",
            "[[fact]]\nid=\"x\"\nclaim=\"c\"\nsource=\"s\"\nextra=1\n"
        )
        .contains("extra"));
        assert!(err_text("k.toml", "other = 1\n").contains("other"));
        assert!(err_text("k.json", r#"{"facts":[],"x":1}"#).contains("x"));
    }

    #[test]
    fn ids_are_validated_and_unique() {
        for bad in ["", "Upper", "-x", "_x", "a b", "a/b", &"a".repeat(41)] {
            let body = format!("[[fact]]\nid = \"{bad}\"\nclaim = \"c\"\nsource = \"s\"\n");
            assert!(err_text("k.toml", &body).contains("invalid id"), "{bad}");
        }
        let dup = "[[fact]]\nid=\"x\"\nclaim=\"c\"\nsource=\"s\"\n[[fact]]\nid=\"x\"\nclaim=\"d\"\nsource=\"s\"\n";
        assert!(err_text("k.toml", dup).contains("used twice"));
    }

    #[test]
    fn lengths_and_counts_are_bounded() {
        let fact = |claim: &str, source: &str| {
            format!("[[fact]]\nid=\"x\"\nclaim=\"{claim}\"\nsource=\"{source}\"\n")
        };
        assert!(load_text("k.toml", &fact(&"c".repeat(600), "s")).is_ok());
        assert!(err_text("k.toml", &fact(&"c".repeat(601), "s")).contains("claim"));
        assert!(err_text("k.toml", &fact("c", &"s".repeat(301))).contains("source"));
        let many: String = (0..65)
            .map(|i| format!("[[fact]]\nid=\"f{i}\"\nclaim=\"c\"\nsource=\"s\"\n"))
            .collect();
        assert!(err_text("k.toml", &many).contains("more than 64"));
        let paths = format!(
            "[[fact]]\nid=\"x\"\nclaim=\"c\"\nsource=\"s\"\npaths=[{}]\n",
            (0..9)
                .map(|i| format!("\"a{i}\""))
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(err_text("k.toml", &paths).contains("more than 8 paths"));
    }

    #[test]
    fn globs_must_be_valid() {
        let body = "[[fact]]\nid=\"x\"\nclaim=\"c\"\nsource=\"s\"\npaths=[\"../x\"]\n";
        assert!(err_text("k.toml", body).contains("not a valid glob"));
        let body = "[[fact]]\nid=\"x\"\nclaim=\"c\"\nsource=\"s\"\npaths=[\"\"]\n";
        assert!(err_text("k.toml", body).contains("empty"));
    }

    #[test]
    fn control_characters_are_refused() {
        let body = r#"{"facts":[{"id":"x","claim":"line\nbreak","source":"s"}]}"#;
        assert!(err_text("k.json", body).contains("control characters"));
        let body = r#"{"facts":[{"id":"x","claim":"c","source":"s\u001b[31m"}]}"#;
        assert!(err_text("k.json", body).contains("control characters"));
    }

    #[test]
    fn secret_like_text_is_refused_without_being_echoed() {
        let token = format!("ghp_{}", "a1B2".repeat(10));
        let body = format!("[[fact]]\nid=\"leaky\"\nclaim=\"use {token} here\"\nsource=\"s\"\n");
        let m = err_text("k.toml", &body);
        assert!(m.contains("fact leaky") && m.contains("secret rule"), "{m}");
        assert!(!m.contains(&token), "{m}");
        let body = format!("[[fact]]\nid=\"leaky\"\nclaim=\"c\"\nsource=\"{token}\"\n");
        assert!(!err_text("k.toml", &body).contains(&token));
    }

    #[test]
    fn parser_errors_do_not_echo_long_text() {
        let long = "z".repeat(5000);
        let body = format!("[[fact]]\nid = {long}\n");
        let m = err_text("k.toml", &body);
        assert!(m.len() < 400, "{}", m.len());
        let m = err_text("k.json", &format!("{{\"facts\": \"{long}\"}}"));
        assert!(m.len() < 400, "{}", m.len());
    }

    #[test]
    fn the_file_is_read_safely() {
        assert!(err_text("k.yaml", "x").contains(".toml or .json"));
        let t = TempDir::new("knowledge-safe");
        let good = t.write(
            "good.toml",
            "[[fact]]\nid=\"x\"\nclaim=\"c\"\nsource=\"s\"\n",
        );
        symlink(&good, t.join("link.toml")).unwrap();
        let m = FileKnowledge::new(Some(t.join("link.toml")))
            .entries()
            .unwrap_err()
            .message;
        assert!(m.contains("symlink"), "{m}");
        assert!(FileKnowledge::new(Some(t.join("missing.toml")))
            .entries()
            .is_err());
        t.write("big.json", &" ".repeat(300 * 1024));
        let m = FileKnowledge::new(Some(t.join("big.json")))
            .entries()
            .unwrap_err()
            .message;
        assert!(m.contains("larger than"), "{m}");
        std::fs::create_dir(t.join("dir.toml")).unwrap();
        assert!(FileKnowledge::new(Some(t.join("dir.toml")))
            .entries()
            .is_err());
        nix::unistd::mkfifo(&t.join("fifo.json"), nix::sys::stat::Mode::S_IRWXU).unwrap();
        assert!(FileKnowledge::new(Some(t.join("fifo.json")))
            .entries()
            .is_err());
        std::fs::write(t.join("bin.json"), [0xff, 0xfe]).unwrap();
        assert!(FileKnowledge::new(Some(t.join("bin.json")))
            .entries()
            .is_err());
    }
}
