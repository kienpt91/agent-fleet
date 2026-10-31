//! SHA-256 adapter for the domain's `Digest` port.

use fleet_domain::approval::Digest;
use sha2::{Digest as _, Sha256};
use std::fmt::Write as _;

pub struct Sha256Digest;

impl Digest for Sha256Digest {
    fn sha256_hex(&self, data: &[u8]) -> String {
        let hash = Sha256::digest(data);
        let mut out = String::with_capacity(64);
        for byte in hash.iter() {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_nist_vectors() {
        assert_eq!(
            Sha256Digest.sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            Sha256Digest.sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn output_is_lowercase_hex_of_fixed_length() {
        let h = Sha256Digest.sha256_hex(&[0xff; 1000]);
        assert_eq!(h.len(), 64);
        assert!(h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    }
}
