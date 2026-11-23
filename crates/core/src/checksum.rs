//! Checksum normalization (SPEC §3.4).
//!
//! **Compatibility contract:** changing [`normalize`] invalidates every
//! checksum stored in every ledger. Do not change it without an ADR and a
//! ledger migration path.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// Normalize script bytes before hashing:
/// 1. strip a leading UTF-8 BOM;
/// 2. convert CRLF to LF (lone CR is kept);
/// 3. leave everything else untouched.
pub fn normalize(bytes: &[u8]) -> Vec<u8> {
    let bytes = bytes.strip_prefix(UTF8_BOM).unwrap_or(bytes);
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            out.push(b'\n');
            i += 2;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// SHA-256 checksum of a script, as lowercase hex.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Checksum(String);

impl Checksum {
    /// Checksum of already-normalized bytes.
    pub fn of_normalized(bytes: &[u8]) -> Self {
        Self(hex::encode(Sha256::digest(bytes)))
    }

    /// Normalize `bytes`, then hash them.
    pub fn of(bytes: &[u8]) -> Self {
        Self::of_normalized(&normalize(bytes))
    }

    /// Wrap a checksum read from a ledger.
    pub fn from_hex(hex: impl Into<String>) -> Self {
        Self(hex.into())
    }

    /// The lowercase hex string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Checksum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_bom() {
        assert_eq!(normalize(b"\xEF\xBB\xBFselect 1;"), b"select 1;");
    }

    #[test]
    fn converts_crlf_but_keeps_lone_cr() {
        assert_eq!(normalize(b"a\r\nb\rc\n"), b"a\nb\rc\n");
    }

    #[test]
    fn bom_and_crlf_hash_like_plain_lf() {
        let windows = b"\xEF\xBB\xBFcreate table t (id int);\r\n";
        let unix = b"create table t (id int);\n";
        assert_eq!(Checksum::of(windows), Checksum::of(unix));
    }

    #[test]
    fn whitespace_changes_the_checksum() {
        assert_ne!(Checksum::of(b"select 1;"), Checksum::of(b"select  1;"));
    }

    #[test]
    fn known_value() {
        // sha256("") — guards against accidental algorithm changes.
        assert_eq!(
            Checksum::of(b"").as_str(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
