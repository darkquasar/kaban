//! View names (spec 11): the one memory view `main` and a code view per commit, `code@<sha>`.
//! A [`CommitSha`] is the 40-hex commit id every code view, index writer and snapshot names.
//! [`ViewName`] is open to new kinds (a view that is not a commit, such as an agent's
//! unfinished work over a base): match it with a fallback arm; a form this build does not
//! know is refused by name ([`InvalidViewName`]).

use std::fmt;
use std::str::FromStr;

/// A 40-hex commit id (lowercase), validated once at the boundary.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CommitSha(String);

/// A string that is not 40 lowercase hex digits.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a commit id is 40 lowercase hex digits")]
pub struct InvalidCommitSha;

impl CommitSha {
    /// Accept exactly 40 lowercase hex digits.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let hex = s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        hex.then(|| Self(s.to_owned()))
    }

    /// The hex text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for CommitSha {
    type Err = InvalidCommitSha;
    fn from_str(s: &str) -> Result<Self, InvalidCommitSha> {
        Self::parse(s).ok_or(InvalidCommitSha)
    }
}

impl fmt::Display for CommitSha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The view a read or a write names: the memory view, or the code view of one commit. Later
/// kinds are added as variants, so a match outside this crate needs a fallback arm; its text
/// form round-trips through `Display` and `FromStr`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ViewName {
    /// `main`: the memory graph.
    Main,
    /// `code@<sha>`: the code graph of one commit.
    Code(CommitSha),
}

/// The text before a code view's commit.
const CODE_PREFIX: &str = "code@";

/// A string that names no view this build knows: neither `main` nor `code@<40 lowercase
/// hex>`. The error names the refused text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown view `{name}`: a view is `main` or `code@<40 lowercase hex commit>`")]
pub struct InvalidViewName {
    name: String,
}

impl InvalidViewName {
    /// The refused text.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl FromStr for ViewName {
    type Err = InvalidViewName;
    fn from_str(s: &str) -> Result<Self, InvalidViewName> {
        if s == "main" {
            return Ok(Self::Main);
        }
        s.strip_prefix(CODE_PREFIX)
            .and_then(CommitSha::parse)
            .map(Self::Code)
            .ok_or_else(|| InvalidViewName { name: s.to_owned() })
    }
}

impl fmt::Display for ViewName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Main => f.write_str("main"),
            Self::Code(sha) => write!(f, "{CODE_PREFIX}{sha}"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn a_commit_sha_is_forty_lowercase_hex_digits() {
        let sha: CommitSha = SHA.parse().unwrap();
        assert_eq!(sha.as_str(), SHA);
        assert_eq!(sha.to_string(), SHA);
        assert_eq!(CommitSha::parse(SHA), Some(sha));
    }

    #[test]
    fn a_commit_sha_refuses_uppercase_wrong_length_and_non_hex() {
        for bad in [
            SHA.to_uppercase(),
            SHA[..39].to_owned(),
            format!("{SHA}0"),
            format!("{}g", &SHA[..39]),
            String::new(),
        ] {
            assert_eq!(bad.parse::<CommitSha>(), Err(InvalidCommitSha), "{bad}");
            assert_eq!(CommitSha::parse(&bad), None, "{bad}");
        }
    }

    #[test]
    fn view_names_round_trip_through_their_text() {
        let code = ViewName::Code(SHA.parse().unwrap());
        assert_eq!("main".parse::<ViewName>(), Ok(ViewName::Main));
        assert_eq!(format!("code@{SHA}").parse::<ViewName>(), Ok(code.clone()));
        assert_eq!(ViewName::Main.to_string(), "main");
        assert_eq!(code.to_string(), format!("code@{SHA}"));
    }

    #[test]
    fn an_unknown_view_form_is_refused_by_name_without_a_panic() {
        let long = "x".repeat(4096);
        for bad in [
            "work@agent-1",
            "agent@0123456789abcdef0123456789abcdef01234567/draft",
            "code@é0123456789abcdef0123456789abcdef012345",
            "\u{0}main",
            long.as_str(),
        ] {
            let err = bad
                .parse::<ViewName>()
                .expect_err(&format!("{bad:?} was accepted as a view"));
            assert!(
                err.to_string().contains(bad),
                "the refusal does not name {bad:?}: {err}"
            );
        }
    }

    #[test]
    fn a_view_name_refuses_anything_else() {
        let mut bad = ["", "Main", "main ", "code", "code@", "code@main"]
            .map(str::to_owned)
            .to_vec();
        bad.extend([
            format!("code@{}", SHA.to_uppercase()),
            format!("code:{SHA}"),
            format!("CODE@{SHA}"),
            format!("code@{SHA}/"),
        ]);
        for bad in bad {
            let err = bad.parse::<ViewName>().expect_err(&bad);
            assert_eq!(err.name(), bad);
        }
    }
}
