//! Host key pinning: one `known_hosts` file per host inside the state directory, written only by
//! the explicit operator command `fleet host trust`, holding exactly the key the operator
//! confirmed. `ssh` is told to use that file and nothing else and to refuse every other key.

use crate::fsx;
use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::{show, HostName};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

/// Largest known_hosts file read back (one key is a few hundred bytes).
const MAX_KNOWN_HOSTS: u64 = 16 * 1024;
/// The key types fleet pins. Everything else (certificates, `sk-` keys, DSA) is refused: a pin is a
/// plain public key that a person compared by fingerprint.
const KEY_TYPES: [&str; 4] = [
    "ssh-ed25519",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
];

/// One host key as ssh writes it in a known_hosts file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKey {
    pub key_type: String,
    /// The base64 text of the key blob, as in the file.
    pub base64: String,
}

impl HostKey {
    /// `SHA256:<unpadded base64>` of the key blob: the form `ssh-keygen -l` prints.
    pub fn fingerprint(&self) -> String {
        match decode_base64(&self.base64) {
            Some(blob) => format!("SHA256:{}", encode_base64_unpadded(&Sha256::digest(&blob))),
            None => "SHA256:(unreadable)".to_string(),
        }
    }

    pub fn line(&self, host_field: &str) -> String {
        format!("{host_field} {} {}\n", self.key_type, self.base64)
    }
}

/// The host field of a known_hosts line: the address, or `[address]:port` for another port.
pub fn host_field(address: &str, port: u16) -> String {
    if port == 22 {
        address.to_string()
    } else {
        format!("[{address}]:{port}")
    }
}

/// Where the pin of a host lives: `<state>/hosts/<name>.known_hosts`.
pub fn pin_path(state_root: &Path, host: &HostName) -> PathBuf {
    state_root
        .join("hosts")
        .join(format!("{}.known_hosts", host.as_str()))
}

/// Reads and validates one known_hosts line for `expected_field`. Marker lines (`@cert-authority`,
/// `@revoked`), hashed hosts, patterns, comments and options are refused: a pin names one host and
/// one plain key.
pub fn parse_pin_line(line: &str, expected_field: &str) -> Result<HostKey> {
    let parts: Vec<&str> = line.split(' ').collect();
    let [field, key_type, base64] = parts[..] else {
        return Err(FleetError::invalid(
            "a pinned key line must be `<host> <type> <key>`",
        ));
    };
    if field != expected_field {
        return Err(FleetError::invalid(format!(
            "the pinned key is for {} but this host is {}",
            show(field),
            show(expected_field)
        )));
    }
    key_from_parts(key_type, base64)
}

fn key_from_parts(key_type: &str, base64: &str) -> Result<HostKey> {
    if !KEY_TYPES.contains(&key_type) {
        return Err(FleetError::invalid(format!(
            "host key type {} is not supported (ed25519 and ECDSA keys are)",
            show(key_type)
        )));
    }
    let blob = decode_base64(base64)
        .filter(|b| !b.is_empty() && b.len() < 2048)
        .ok_or_else(|| FleetError::invalid("the host key is not valid base64"))?;
    // The blob starts with the key type as a length-prefixed string.
    let announced = blob
        .get(0..4)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map(|b| u32::from_be_bytes(b) as usize)
        .and_then(|n| blob.get(4..4usize.checked_add(n)?));
    if announced != Some(key_type.as_bytes()) {
        return Err(FleetError::invalid(
            "the host key blob does not say the type it was labelled with",
        ));
    }
    Ok(HostKey {
        key_type: key_type.to_string(),
        base64: base64.to_string(),
    })
}

/// The one key ssh recorded in a scratch known_hosts file during a scan: exactly one line, for
/// `expected_field` and nothing else.
pub fn parse_scan_result(text: &str, expected_field: &str) -> Result<HostKey> {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    match lines[..] {
        [line] => parse_pin_line(line.trim_end(), expected_field),
        [] => Err(FleetError::invalid(
            "the host presented no key (is it reachable on this address and port?)",
        )),
        _ => Err(FleetError::invalid(
            "the scan recorded more than one key; refusing to pick one",
        )),
    }
}

/// What is pinned for a host now, if anything.
pub fn read_pin(path: &Path, expected_field: &str) -> Result<Option<HostKey>> {
    match fsx::lstat_kind(path) {
        None => return Ok(None),
        Some(fsx::FileKind::File) => {}
        Some(_) => {
            return Err(FleetError::forbidden(format!(
                "{} is not a regular file",
                path.display()
            )))
        }
    }
    let bytes = fsx::read_regular(path, MAX_KNOWN_HOSTS)
        .map_err(|e| FleetError::invalid(format!("cannot read {}: {e}", path.display())))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| FleetError::invalid("the pinned key file is not valid UTF-8"))?;
    let line = text.trim_end_matches('\n');
    if line.contains('\n') {
        return Err(FleetError::invalid(
            "the pinned key file has more than one line",
        ));
    }
    parse_pin_line(line, expected_field).map(Some)
}

/// Writes the pin, private (0600) and atomically, replacing an earlier one.
pub fn write_pin(path: &Path, expected_field: &str, key: &HostKey) -> Result<()> {
    if let Some(dir) = path.parent() {
        fsx::ensure_private_dir(dir)?;
    }
    fsx::write_private_atomic(path, key.line(expected_field).as_bytes())
}

/// Removes the pin (idempotent).
pub fn remove_pin(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(FleetError::io("cannot remove the pinned key", &e)),
    }
}

// ---- base64 (standard alphabet) -------------------------------------------------------------------

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn encode_base64_unpadded(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 4 / 3 + 2);
    for chunk in data.chunks(3) {
        let b = [
            chunk.first().copied().unwrap_or(0),
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = [(n >> 18) & 63, (n >> 12) & 63, (n >> 6) & 63, n & 63];
        for c in chars.iter().take(chunk.len() + 1) {
            if let Some(&a) = ALPHABET.get(*c as usize) {
                out.push(char::from(a));
            }
        }
    }
    out
}

/// Strict decoding: the alphabet only, padding only at the end and only as needed.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let trimmed = text.trim_end_matches('=');
    if text.len() - trimmed.len() > 2 || text.is_empty() {
        return None;
    }
    if !text.len().is_multiple_of(4) && text.len() != trimmed.len() {
        return None;
    }
    let mut out = Vec::with_capacity(trimmed.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for c in trimmed.bytes() {
        let v = ALPHABET.iter().position(|a| *a == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    // Leftover bits must be zero (canonical encoding).
    if bits > 0 && (acc & ((1 << bits) - 1)) != 0 {
        return None;
    }
    if trimmed.len() % 4 == 1 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// A syntactically valid ed25519 key blob (not a real key): type string + 32 bytes.
    fn sample_key() -> HostKey {
        let mut blob = Vec::new();
        blob.extend_from_slice(&11u32.to_be_bytes());
        blob.extend_from_slice(b"ssh-ed25519");
        blob.extend_from_slice(&32u32.to_be_bytes());
        blob.extend_from_slice(&[7u8; 32]);
        HostKey {
            key_type: "ssh-ed25519".into(),
            base64: encode_padded(&blob),
        }
    }

    fn encode_padded(data: &[u8]) -> String {
        let mut s = encode_base64_unpadded(data);
        while !s.len().is_multiple_of(4) {
            s.push('=');
        }
        s
    }

    #[test]
    fn base64_round_trips_every_length() {
        for n in 0..40usize {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            let enc = encode_padded(&data);
            if n == 0 {
                assert_eq!(decode_base64(&enc), None);
            } else {
                assert_eq!(decode_base64(&enc).unwrap(), data, "{n}");
            }
        }
        assert_eq!(encode_base64_unpadded(b"Man"), "TWFu");
        assert_eq!(encode_base64_unpadded(b"Ma"), "TWE");
        assert_eq!(decode_base64("TWE=").unwrap(), b"Ma");
    }

    #[test]
    fn base64_rejects_what_is_not_canonical() {
        for bad in [
            "TWF", "TW=u", "TWE==", "T===", "TW Fu", "TWFu\n", "TWF!", "A", "", "TWF=u", "TWG=",
        ] {
            assert!(decode_base64(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn the_fingerprint_is_sha256_of_the_blob() {
        let key = sample_key();
        let blob = decode_base64(&key.base64).unwrap();
        let expect = format!("SHA256:{}", encode_base64_unpadded(&Sha256::digest(&blob)));
        assert_eq!(key.fingerprint(), expect);
        assert!(key.fingerprint().len() == 7 + 43);
    }

    #[test]
    fn lines_are_parsed_strictly() {
        let key = sample_key();
        let line = key.line("example.org");
        let parsed = parse_pin_line(line.trim_end(), "example.org").unwrap();
        assert_eq!(parsed, key);
        assert!(parse_pin_line(line.trim_end(), "other.org").is_err());
        for bad in [
            "example.org ssh-ed25519".to_string(),
            format!("@cert-authority example.org ssh-ed25519 {}", key.base64),
            format!("example.org,other ssh-ed25519 {}", key.base64),
            format!("example.org ssh-rsa {}", key.base64),
            format!("example.org ssh-ed25519 {} comment", key.base64),
            "example.org ssh-ed25519 !!!".to_string(),
            format!("example.org ecdsa-sha2-nistp256 {}", key.base64),
            format!("|1|abc|def ssh-ed25519 {}", key.base64),
            format!("example.org  ssh-ed25519 {}", key.base64),
        ] {
            assert!(parse_pin_line(&bad, "example.org").is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_scan_must_record_exactly_one_key() {
        let key = sample_key();
        let one = key.line("h");
        assert_eq!(parse_scan_result(&one, "h").unwrap(), key);
        assert!(parse_scan_result("", "h").is_err());
        assert!(parse_scan_result(&format!("{one}{one}"), "h").is_err());
        assert!(parse_scan_result(&key.line("jump"), "h").is_err());
    }

    #[test]
    fn host_fields_name_the_port_when_it_is_not_22() {
        assert_eq!(host_field("a.example", 22), "a.example");
        assert_eq!(host_field("a.example", 2222), "[a.example]:2222");
        assert_eq!(host_field("::1", 2222), "[::1]:2222");
    }

    #[test]
    fn the_pin_file_is_private_replaced_whole_and_read_back() {
        let dir = TempDir::new("pin");
        let host = HostName::parse("build").unwrap();
        let path = pin_path(dir.path(), &host);
        assert!(read_pin(&path, "h").unwrap().is_none());
        let key = sample_key();
        write_pin(&path, "h", &key).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(read_pin(&path, "h").unwrap().unwrap(), key);
        remove_pin(&path).unwrap();
        remove_pin(&path).unwrap();
        assert!(read_pin(&path, "h").unwrap().is_none());
    }

    #[test]
    fn a_pin_that_is_not_a_plain_file_or_has_extra_lines_is_refused() {
        let dir = TempDir::new("pin-bad");
        let host = HostName::parse("build").unwrap();
        let path = pin_path(dir.path(), &host);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let key = sample_key();
        fs::write(&path, format!("{}{}", key.line("h"), key.line("h"))).unwrap();
        assert!(read_pin(&path, "h").is_err());
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", &path).unwrap();
        assert!(read_pin(&path, "h").is_err());
    }
}
