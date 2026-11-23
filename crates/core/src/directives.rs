//! Engine directives in SQL script headers (SPEC §3.2).
//!
//! Directives are `-- migrate:<key> [value]` lines in the *header* of a SQL
//! file: the leading run of blank lines and `--` comment lines. Anything after
//! a second `--` on a directive line is a trailing comment.

/// Directives parsed from a script header.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Directives {
    /// `-- migrate:no-transaction`
    pub no_transaction: bool,
    /// `-- migrate:timeout <secs>`
    pub timeout_secs: Option<u64>,
    /// `-- migrate:tag <tag>` (repeatable)
    pub tags: Vec<String>,
    /// `-- migrate:executor <name>`
    pub executor: Option<String>,
}

/// A malformed directive.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DirectiveError {
    /// Unknown `migrate:` key.
    #[error("line {line}: unknown directive `migrate:{key}`")]
    UnknownKey {
        /// 1-based line number.
        line: usize,
        /// The key as written.
        key: String,
    },
    /// The directive needs a value but has none, or the value is invalid.
    #[error("line {line}: invalid value for `migrate:{key}`: {reason}")]
    InvalidValue {
        /// 1-based line number.
        line: usize,
        /// The directive key.
        key: String,
        /// Human-readable reason.
        reason: String,
    },
}

const PREFIX: &str = "migrate:";

/// Parse directives from the header of a (normalized) SQL script.
pub fn parse_sql_header(text: &str) -> Result<Directives, DirectiveError> {
    let mut directives = Directives::default();
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(comment) = trimmed.strip_prefix("--") else {
            break; // first non-comment line ends the header
        };
        let comment = comment.trim_start();
        let Some(body) = comment.strip_prefix(PREFIX) else {
            continue; // an ordinary comment
        };
        // Drop a trailing `-- comment`.
        let body = body.split("--").next().unwrap_or_default().trim();
        let (key, value) = match body.split_once(char::is_whitespace) {
            Some((k, v)) => (k, v.trim()),
            None => (body, ""),
        };
        apply(&mut directives, line, key, value)?;
    }
    Ok(directives)
}

fn apply(d: &mut Directives, line: usize, key: &str, value: &str) -> Result<(), DirectiveError> {
    let invalid = |reason: &str| DirectiveError::InvalidValue {
        line,
        key: key.to_owned(),
        reason: reason.to_owned(),
    };
    match key {
        "no-transaction" => {
            if !value.is_empty() {
                return Err(invalid("takes no value"));
            }
            d.no_transaction = true;
        }
        "timeout" => {
            let secs: u64 = value
                .parse()
                .map_err(|_| invalid("expected a whole number of seconds"))?;
            if secs == 0 {
                return Err(invalid("must be greater than 0"));
            }
            d.timeout_secs = Some(secs);
        }
        "tag" => {
            if value.is_empty() {
                return Err(invalid("expected a tag"));
            }
            d.tags.push(value.to_owned());
        }
        "executor" => {
            if value.is_empty() {
                return Err(invalid("expected an executor name"));
            }
            d.executor = Some(value.to_owned());
        }
        other => {
            return Err(DirectiveError::UnknownKey {
                line,
                key: other.to_owned(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_directives_with_trailing_comments() {
        let sql = "\
-- migrate:no-transaction        -- e.g. CREATE INDEX CONCURRENTLY
-- migrate:timeout 900
-- an ordinary comment
-- migrate:tag hotfix
-- migrate:tag reviewed
-- migrate:executor sqlcmd

CREATE INDEX CONCURRENTLY ix ON t (c);
";
        let d = parse_sql_header(sql).unwrap();
        assert!(d.no_transaction);
        assert_eq!(d.timeout_secs, Some(900));
        assert_eq!(d.tags, vec!["hotfix", "reviewed"]);
        assert_eq!(d.executor.as_deref(), Some("sqlcmd"));
    }

    #[test]
    fn ignores_directives_after_the_header() {
        let sql = "SELECT 1;\n-- migrate:no-transaction\n";
        assert_eq!(parse_sql_header(sql).unwrap(), Directives::default());
    }

    #[test]
    fn rejects_unknown_keys_with_line_number() {
        let err = parse_sql_header("\n-- migrate:no-transation\n").unwrap_err();
        assert_eq!(
            err,
            DirectiveError::UnknownKey {
                line: 2,
                key: "no-transation".into()
            }
        );
    }

    #[test]
    fn rejects_bad_values() {
        assert!(parse_sql_header("-- migrate:timeout soon").is_err());
        assert!(parse_sql_header("-- migrate:timeout 0").is_err());
        assert!(parse_sql_header("-- migrate:no-transaction yes").is_err());
        assert!(parse_sql_header("-- migrate:tag").is_err());
    }

    #[test]
    fn empty_script_has_no_directives() {
        assert_eq!(parse_sql_header("").unwrap(), Directives::default());
    }
}
