//! Editing the `[hosts.<name>]` tables of `fleet.toml` for `fleet host add` and `fleet host remove`.
//!
//! Adding appends one table (and an `env` sub-table) to the end of the file and leaves everything
//! else, comments included, byte for byte. Removing deletes the table and its sub-table by lines;
//! because a line-based edit of a TOML document can be wrong, the result is parsed again and must
//! hold exactly the other hosts and settings the file held before, or nothing is changed.

use crate::config_file::{self, LoadedConfig};
use crate::fsx;
use fleet_app::hosts::{HostDef, DEFAULT_MAX_BUNDLE_BYTES};
use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::{show, HostName};
use fleet_domain::limits::MAX_CONFIG_FILE;
use std::path::Path;

/// A TOML basic string.
fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The text of the tables of one host, with every setting written out.
pub fn render_host(h: &HostDef) -> String {
    let mut out = format!("[hosts.{}]\n", h.name);
    out.push_str(&format!("address = {}\n", quoted(&h.address)));
    out.push_str(&format!("port = {}\n", h.port));
    out.push_str(&format!("user = {}\n", quoted(&h.user)));
    out.push_str(&format!("identity_file = {}\n", quoted(&h.identity_file)));
    out.push_str(&format!("base_dir = {}\n", quoted(&h.base_dir)));
    let workers: Vec<String> = h.allowed_workers.iter().map(|w| quoted(w)).collect();
    out.push_str(&format!("allowed_workers = [{}]\n", workers.join(", ")));
    out.push_str(&format!("max_sessions = {}\n", h.max_sessions));
    if let Some(j) = &h.proxy_jump {
        out.push_str(&format!("proxy_jump = {}\n", quoted(j.as_str())));
    }
    out.push_str(&format!(
        "connect_timeout_secs = {}\n",
        h.connect_timeout_secs
    ));
    out.push_str(&format!("server_alive_secs = {}\n", h.server_alive_secs));
    out.push_str(&format!("server_alive_count = {}\n", h.server_alive_count));
    if h.max_bundle_bytes != DEFAULT_MAX_BUNDLE_BYTES {
        out.push_str(&format!(
            "max_bundle_mib = {}\n",
            h.max_bundle_bytes / (1024 * 1024)
        ));
    }
    if !h.env.is_empty() {
        out.push_str(&format!("\n[hosts.{}.env]\n", h.name));
        for (k, v) in &h.env {
            out.push_str(&format!("{k} = {}\n", quoted(v)));
        }
    }
    out
}

/// `text` with the tables of `host` appended. The result is checked by reading it as a whole
/// configuration; an error means nothing should be written.
pub fn append_host(text: &str, host: &HostDef) -> Result<String> {
    let mut out = text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&render_host(host));
    // It must be a valid configuration, and the host must be in it exactly as written.
    let loaded = config_file::parse_config_text(&out)?;
    match loaded.app.hosts.get(&host.name) {
        Some(read) if read == host => Ok(out),
        _ => Err(FleetError::internal(
            "the host table was not read back as written; nothing was changed",
        )),
    }
}

fn header_of(line: &str) -> Option<String> {
    let t = line.trim_start();
    if !t.starts_with('[') || t.starts_with("[[") {
        return None;
    }
    let end = t.find(']')?;
    Some(
        t.get(1..end)?
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("")
            .replace('"', ""),
    )
}

fn is_any_header(line: &str) -> bool {
    line.trim_start().starts_with('[')
}

/// `text` without the `[hosts.<name>]` table and its `[hosts.<name>.env]` table. Refuses (and
/// changes nothing) when the file does not have the host as a plain table, or when removing the
/// lines changed anything but that host.
pub fn remove_host(text: &str, name: &HostName) -> Result<String> {
    let before: LoadedConfig = config_file::parse_config_text(text)?;
    if !before.app.hosts.contains_key(name) {
        return Err(FleetError::not_found(format!(
            "host {} is not configured",
            show(name.as_str())
        )));
    }
    let main = format!("hosts.{name}");
    let env = format!("hosts.{name}.env");
    let mut kept: Vec<&str> = Vec::new();
    let mut skipping = false;
    for line in text.split_inclusive('\n') {
        if is_any_header(line) {
            skipping = matches!(header_of(line), Some(h) if h == main || h == env);
        }
        if !skipping {
            kept.push(line);
        }
    }
    let result: String = kept.concat();
    let after = config_file::parse_config_text(&result).map_err(|_| refuse(name))?;
    let mut expected = before.app.hosts.clone();
    expected.remove(name);
    if after.app.hosts != expected
        || after.app.workers.len() != before.app.workers.len()
        || after.app.gates.len() != before.app.gates.len()
        || after.remote != before.remote
    {
        return Err(refuse(name));
    }
    Ok(result)
}

fn refuse(name: &HostName) -> FleetError {
    FleetError::conflict(format!(
        "host {name} could not be removed automatically (it is not written as plain [hosts.{name}] tables); edit the file by hand"
    ))
}

/// Reads the configuration file as text, bounded; `None` when it does not exist.
pub fn read_text(path: &Path) -> Result<Option<String>> {
    match fsx::lstat_kind(path) {
        None => Ok(None),
        Some(_) => {
            let bytes = fsx::read_regular(path, MAX_CONFIG_FILE)
                .map_err(|e| FleetError::invalid(format!("{}: {e}", path.display())))?;
            String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| FleetError::invalid(format!("{} is not valid UTF-8", path.display())))
        }
    }
}

/// Writes the configuration file atomically (private, 0600).
pub fn write_text(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() && !dir.exists() {
            fsx::ensure_private_dir(dir)?;
        }
    }
    fsx::write_private_atomic(path, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str) -> HostDef {
        let mut h = HostDef::new(
            HostName::parse(name).unwrap(),
            "build.example.org",
            "fleet",
            "/home/me/.ssh/key",
            "/srv/fleet",
        );
        h.allowed_workers = vec!["agent".into(), "/opt/x/agent".into()];
        h.env = vec![("API_BASE".into(), "https://a.invalid/\"q\"".into())];
        h.port = 2222;
        h
    }

    const BASE: &str =
        "# my config\n[daemon]\nmax_sessions = 2 # keep\n\n[workers.agent]\ncommand = \"agent\"\n";

    #[test]
    fn a_host_is_appended_after_everything_else_and_read_back_as_written() {
        let h = host("build");
        let out = append_host(BASE, &h).unwrap();
        assert!(out.starts_with(BASE), "the existing text is untouched");
        assert!(out.contains("[hosts.build]") && out.contains("[hosts.build.env]"));
        let again = append_host(&out, &host("other")).unwrap();
        assert!(again.starts_with(&out));
        assert!(append_host("", &h).unwrap().starts_with("[hosts.build]"));
        // The same name twice is refused by the configuration reader.
        assert!(append_host(&out, &h).is_err());
    }

    #[test]
    fn a_host_is_removed_with_its_env_table_and_nothing_else() {
        let h = host("build");
        let mut text = append_host(BASE, &h).unwrap();
        text = append_host(&text, &host("other")).unwrap();
        let without = remove_host(&text, &h.name).unwrap();
        assert!(!without.contains("hosts.build"));
        assert!(without.contains("[hosts.other]") && without.contains("[hosts.other.env]"));
        assert!(without.starts_with(BASE));
        let nothing = remove_host(&without, &HostName::parse("other").unwrap()).unwrap();
        assert_eq!(nothing.trim_end(), BASE.trim_end());
        assert!(remove_host(&nothing, &h.name).is_err());
    }

    #[test]
    fn a_host_written_another_way_is_refused_instead_of_damaged() {
        let text = "[hosts]\nbuild = { address = \"a.example\", user = \"u\", identity_file = \"/k\", base_dir = \"/b\" }\n[workers.agent]\ncommand = \"agent\"\n";
        let err = remove_host(text, &HostName::parse("build").unwrap()).unwrap_err();
        assert!(err.message.contains("by hand"), "{}", err.message);
        // A table with a multi-line array is removed whole.
        let multi = "[hosts.build]\naddress = \"a.example\"\nuser = \"u\"\nidentity_file = \"/k\"\nbase_dir = \"/b\"\nallowed_workers = [\n  \"agent\",\n]\n\n[daemon]\nmax_sessions = 3\n";
        let out = remove_host(multi, &HostName::parse("build").unwrap()).unwrap();
        assert_eq!(out, "[daemon]\nmax_sessions = 3\n");
    }

    #[test]
    fn values_are_written_as_valid_toml_whatever_they_contain() {
        for v in [
            "plain",
            "with \"quotes\"",
            "back\\slash",
            "tab\there",
            "é日本",
        ] {
            let mut h = host("h");
            h.env = vec![("V".into(), v.into())];
            let out = append_host("", &h).unwrap();
            let read = config_file::parse_config_text(&out).unwrap();
            assert_eq!(read.app.hosts[&h.name].env[0].1, v);
        }
    }
}
