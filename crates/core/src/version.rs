//! Migration versions.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Maximum number of digits a version may have (fits in `u128`).
const MAX_DIGITS: usize = 38;

/// A migration version: a run of ASCII digits, compared **numerically**.
///
/// The original text (including leading zeros) is preserved so that it
/// round-trips exactly into file names and ledger rows. Equality, ordering
/// and hashing use the numeric value, so `0042` and `42` are the *same*
/// version (and therefore a duplicate if both exist).
#[derive(Debug, Clone)]
pub struct Version {
    text: String,
    value: u128,
}

impl Version {
    /// The special version `0`, meaning "before any migration".
    pub fn zero() -> Self {
        Self {
            text: "0".to_owned(),
            value: 0,
        }
    }

    /// `true` for the version `0`.
    pub fn is_zero(&self) -> bool {
        self.value == 0
    }

    /// The original text of the version.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The numeric value of the version.
    pub fn value(&self) -> u128 {
        self.value
    }
}

/// Why a string is not a valid [`Version`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VersionError {
    /// The string was empty.
    #[error("version is empty")]
    Empty,
    /// The string contained a non-digit character.
    #[error("version `{0}` must contain only digits 0-9")]
    NotDigits(String),
    /// The string has more digits than supported.
    #[error("version `{0}` has more than {MAX_DIGITS} digits")]
    TooLong(String),
}

impl FromStr for Version {
    type Err = VersionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(VersionError::Empty);
        }
        if !s.bytes().all(|b| b.is_ascii_digit()) {
            return Err(VersionError::NotDigits(s.to_owned()));
        }
        if s.len() > MAX_DIGITS {
            return Err(VersionError::TooLong(s.to_owned()));
        }
        let value = s
            .parse::<u128>()
            .map_err(|_| VersionError::TooLong(s.to_owned()))?;
        Ok(Self {
            text: s.to_owned(),
            value,
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl Eq for Version {}

impl Hash for Version {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.value.cmp(&other.value)
    }
}

impl Serialize for Version {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        s.parse().unwrap()
    }

    #[test]
    fn parses_digits_and_keeps_text() {
        let version = v("0042");
        assert_eq!(version.as_str(), "0042");
        assert_eq!(version.value(), 42);
        assert_eq!(version.to_string(), "0042");
    }

    #[test]
    fn compares_numerically_not_lexically() {
        assert!(v("9") < v("10"));
        assert!(v("20261109142530") > v("20261101000000"));
        assert_eq!(v("0042"), v("42"));
    }

    #[test]
    fn rejects_invalid_input() {
        assert_eq!("".parse::<Version>(), Err(VersionError::Empty));
        assert!(matches!(
            "1.2".parse::<Version>(),
            Err(VersionError::NotDigits(_))
        ));
        assert!(matches!(
            "-1".parse::<Version>(),
            Err(VersionError::NotDigits(_))
        ));
        let long = "9".repeat(39);
        assert!(matches!(
            long.parse::<Version>(),
            Err(VersionError::TooLong(_))
        ));
    }

    #[test]
    fn zero_is_zero() {
        assert!(Version::zero().is_zero());
        assert!(v("000").is_zero());
        assert!(!v("1").is_zero());
    }

    #[test]
    fn serde_round_trips_text() {
        let json = serde_json::to_string(&v("007")).unwrap();
        assert_eq!(json, "\"007\"");
        let back: Version = serde_json::from_str(&json).unwrap();
        assert_eq!(back.as_str(), "007");
    }
}
