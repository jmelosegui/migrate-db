//! `${name}` placeholders in scripts (SPEC §3.3).
//!
//! Off unless the config says `placeholders = { enabled = true }`. Values
//! come from the run (`MIGRATE_DB_PLACEHOLDER_<NAME>`, ADR-0005), so the
//! same file deploys to every stage. Checksums are taken on the **raw**
//! file, so different values per stage are never drift.
//!
//! Syntax: `${name}` where `name` is `[A-Za-z_][A-Za-z0-9_]*`, matched
//! case-insensitively against the variable (`${schema}` →
//! `MIGRATE_DB_PLACEHOLDER_SCHEMA`). `$${` writes a literal `${`.

use std::collections::{BTreeMap, BTreeSet};

/// Prefix of the variables that hold placeholder values.
pub const ENV_PREFIX: &str = "MIGRATE_DB_PLACEHOLDER_";

/// Placeholder values for a run, keyed by upper-case name.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Values(BTreeMap<String, String>);

impl std::fmt::Debug for Values {
    // Values can be sensitive (for example a role password); print names only.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.0.keys()).finish()
    }
}

impl Values {
    /// Collect `MIGRATE_DB_PLACEHOLDER_<NAME>=value` pairs. Earlier sources
    /// win, so pass the process environment before `--env-file` values.
    pub fn from_vars<'a>(vars: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut map = BTreeMap::new();
        for (k, v) in vars {
            if let Some(name) = k.strip_prefix(ENV_PREFIX)
                && valid_name(name)
            {
                map.entry(name.to_ascii_uppercase())
                    .or_insert_with(|| v.to_owned());
            }
        }
        Self(map)
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.0.get(&name.to_ascii_uppercase()).map(String::as_str)
    }
}

/// A placeholder problem in one script.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlaceholderError {
    /// `${` without a valid `name}` after it.
    #[error(
        "line {line}: malformed placeholder (expected ${{name}}; write $${{ for a literal ${{)"
    )]
    Malformed {
        /// 1-based line.
        line: usize,
    },
    /// Placeholders without a value in this run.
    #[error("no value for placeholder(s) {}: set {}", .0.iter().map(|n| format!("${{{n}}}")).collect::<Vec<_>>().join(", "), .0.iter().map(|n| format!("{ENV_PREFIX}{}", n.to_ascii_uppercase())).collect::<Vec<_>>().join(", "))]
    Missing(Vec<String>),
}

fn valid_name(s: &str) -> bool {
    s.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

enum Piece<'a> {
    Text(&'a str),
    Name(&'a str),
}

/// Split `text` into literal text and placeholder names.
fn pieces(text: &str) -> Result<Vec<Piece<'_>>, PlaceholderError> {
    let mut out = Vec::new();
    let mut rest = text;
    let mut consumed = 0usize;
    while let Some(at) = rest.find("${") {
        // `$${` is an escaped literal `${`.
        if at > 0 && rest.as_bytes()[at - 1] == b'$' {
            out.push(Piece::Text(&rest[..at - 1]));
            out.push(Piece::Text("${"));
            consumed += at + 2;
            rest = &rest[at + 2..];
            continue;
        }
        out.push(Piece::Text(&rest[..at]));
        let after = &rest[at + 2..];
        let line = text[..consumed + at].matches('\n').count() + 1;
        let end = after
            .find('}')
            .ok_or(PlaceholderError::Malformed { line })?;
        let name = &after[..end];
        if !valid_name(name) {
            return Err(PlaceholderError::Malformed { line });
        }
        out.push(Piece::Name(name));
        consumed += at + 2 + end + 1;
        rest = &after[end + 1..];
    }
    out.push(Piece::Text(rest));
    Ok(out)
}

/// The names `text` refers to (and a syntax check).
pub fn references(text: &str) -> Result<BTreeSet<String>, PlaceholderError> {
    Ok(pieces(text)?
        .into_iter()
        .filter_map(|p| match p {
            Piece::Name(n) => Some(n.to_owned()),
            Piece::Text(_) => None,
        })
        .collect())
}

/// Substitute every placeholder. Fails, listing them, if any has no value.
pub fn render(text: &str, values: &Values) -> Result<String, PlaceholderError> {
    let mut out = String::with_capacity(text.len());
    let mut missing = BTreeSet::new();
    for p in pieces(text)? {
        match p {
            Piece::Text(t) => out.push_str(t),
            Piece::Name(n) => match values.get(n) {
                Some(v) => out.push_str(v),
                None => {
                    missing.insert(n.to_owned());
                }
            },
        }
    }
    if missing.is_empty() {
        Ok(out)
    } else {
        Err(PlaceholderError::Missing(missing.into_iter().collect()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn values(pairs: &[(&str, &str)]) -> Values {
        Values::from_vars(pairs.iter().copied())
    }

    #[test]
    fn renders_table_of_cases() {
        let v = values(&[
            ("MIGRATE_DB_PLACEHOLDER_SCHEMA", "sales"),
            ("MIGRATE_DB_PLACEHOLDER_Env", "dev"),
            ("UNRELATED", "x"),
        ]);
        for (input, expected) in [
            ("SELECT 1;", "SELECT 1;"),
            (
                "CREATE TABLE ${schema}.t (id int);",
                "CREATE TABLE sales.t (id int);",
            ),
            ("${SCHEMA}.${env}", "sales.dev"),
            ("cost: $5 and $$ and $", "cost: $5 and $$ and $"),
            ("literal $${schema} stays", "literal ${schema} stays"),
            ("$${not_a_placeholder", "${not_a_placeholder"),
        ] {
            assert_eq!(render(input, &v).unwrap(), expected, "{input}");
        }
    }

    #[test]
    fn missing_and_malformed_are_errors() {
        let v = values(&[("MIGRATE_DB_PLACEHOLDER_SCHEMA", "s")]);
        let e = render("${schema}.${role} ${role} ${owner}", &v).unwrap_err();
        assert_eq!(
            e,
            PlaceholderError::Missing(vec!["owner".into(), "role".into()])
        );
        assert!(e.to_string().contains("MIGRATE_DB_PLACEHOLDER_ROLE"), "{e}");
        for (bad, line) in [("a\n${", 2), ("${1x}", 1), ("${}", 1), ("x\ny ${a-b}", 2)] {
            assert_eq!(
                references(bad).unwrap_err(),
                PlaceholderError::Malformed { line },
                "{bad}"
            );
        }
    }

    #[test]
    fn references_and_precedence() {
        let refs = references("${a} ${b} ${a} $${c}").unwrap();
        assert_eq!(refs.into_iter().collect::<Vec<_>>(), ["a", "b"]);
        // First source wins (process env before --env-file).
        let v = Values::from_vars([
            ("MIGRATE_DB_PLACEHOLDER_X", "process"),
            ("MIGRATE_DB_PLACEHOLDER_X", "file"),
        ]);
        assert_eq!(render("${x}", &v).unwrap(), "process");
        assert_eq!(format!("{v:?}"), "{\"X\"}", "Debug shows names, not values");
    }
}
