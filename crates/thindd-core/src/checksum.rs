//! Digest algorithms understood by the bmap format.

use crate::error::{Error, Result};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};
use std::fmt;

/// A digest algorithm that may appear in a bmap file.
///
/// Format version 1.3 hard-coded SHA-1; version 2.0 introduced the
/// `<ChecksumType>` element and with it SHA-256 (the default) and SHA-512.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ChecksumKind {
    /// SHA-1 — only for reading legacy 1.3 bmap files.
    Sha1,
    /// SHA-256 — what upstream `bmaptool` writes, and what we write.
    #[default]
    Sha256,
    /// SHA-512.
    Sha512,
}

impl ChecksumKind {
    /// Parse the algorithm name as it appears in `<ChecksumType>`.
    ///
    /// ```
    /// # use thindd_core::ChecksumKind;
    /// # fn main() -> Result<(), thindd_core::Error> {
    /// assert_eq!(ChecksumKind::from_name("SHA256")?, ChecksumKind::Sha256);
    /// # Ok(())
    /// # }
    /// ```
    pub fn from_name(name: &str) -> Result<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "sha1" => Ok(Self::Sha1),
            "sha256" => Ok(Self::Sha256),
            "sha512" => Ok(Self::Sha512),
            other => Err(Error::UnsupportedChecksum { name: other.to_owned() }),
        }
    }

    /// Canonical lower-case name, as written into `<ChecksumType>`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
            Self::Sha512 => "sha512",
        }
    }

    /// Number of characters in the hex form of this digest.
    #[must_use]
    pub const fn hex_len(self) -> usize {
        match self {
            Self::Sha1 => 40,
            Self::Sha256 => 64,
            Self::Sha512 => 128,
        }
    }

    /// Start an incremental digest.
    #[must_use]
    pub fn hasher(self) -> Hasher {
        match self {
            Self::Sha1 => Hasher::Sha1(Box::new(Sha1::new())),
            Self::Sha256 => Hasher::Sha256(Box::new(Sha256::new())),
            Self::Sha512 => Hasher::Sha512(Box::new(Sha512::new())),
        }
    }

    /// One-shot digest of `data`, hex-encoded.
    #[must_use]
    pub fn digest(self, data: &[u8]) -> String {
        let mut h = self.hasher();
        h.update(data);
        h.finish()
    }
}

impl fmt::Display for ChecksumKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// An in-progress digest over a block range or over a whole bmap file.
///
/// The variants are boxed so that the enum stays small — SHA-512 state is
/// considerably larger than SHA-1 state, and this value is moved around per
/// range.
pub enum Hasher {
    /// SHA-1 state.
    Sha1(Box<Sha1>),
    /// SHA-256 state.
    Sha256(Box<Sha256>),
    /// SHA-512 state.
    Sha512(Box<Sha512>),
}

impl Hasher {
    /// Feed more bytes into the digest.
    pub fn update(&mut self, data: &[u8]) {
        match self {
            Self::Sha1(h) => h.update(data),
            Self::Sha256(h) => h.update(data),
            Self::Sha512(h) => h.update(data),
        }
    }

    /// Finish the digest and return it hex-encoded in lower case.
    #[must_use]
    pub fn finish(self) -> String {
        match self {
            Self::Sha1(h) => hex::encode(h.finalize()),
            Self::Sha256(h) => hex::encode(h.finalize()),
            Self::Sha512(h) => hex::encode(h.finalize()),
        }
    }

    /// Which algorithm this hasher implements.
    #[must_use]
    pub const fn kind(&self) -> ChecksumKind {
        match self {
            Self::Sha1(_) => ChecksumKind::Sha1,
            Self::Sha256(_) => ChecksumKind::Sha256,
            Self::Sha512(_) => ChecksumKind::Sha512,
        }
    }
}

impl fmt::Debug for Hasher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Hasher").field(&self.kind()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_answer_sha256() {
        assert_eq!(
            ChecksumKind::Sha256.digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn known_answer_sha1() {
        assert_eq!(ChecksumKind::Sha1.digest(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn hex_len_matches_digest_len() {
        for k in [ChecksumKind::Sha1, ChecksumKind::Sha256, ChecksumKind::Sha512] {
            assert_eq!(k.digest(b"").len(), k.hex_len(), "{k}");
        }
    }

    #[test]
    fn incremental_matches_one_shot() {
        let mut h = ChecksumKind::Sha512.hasher();
        h.update(b"hello ");
        h.update(b"world");
        assert_eq!(h.finish(), ChecksumKind::Sha512.digest(b"hello world"));
    }

    #[test]
    fn unknown_algorithm_is_rejected() {
        assert!(matches!(ChecksumKind::from_name("md5"), Err(Error::UnsupportedChecksum { .. })));
    }
}
