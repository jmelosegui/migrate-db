//! `GO` batch splitting (PROVIDERS.md § SQL Server).
//!
//! `GO` is not T-SQL: sqlcmd and SSMS treat a line holding only `GO` (or
//! `GO <n>`) as the end of a batch. Statements such as `CREATE PROCEDURE`
//! must be first in their batch, so the provider sends each batch on its own.
//!
//! A line is a separator only when it starts outside any string literal,
//! quoted identifier or block comment, and matches
//! `^\s*GO(\s+<n>)?\s*(--.*)?$` (case-insensitive). T-SQL block comments
//! nest, so the scanner tracks their depth.

/// One batch to send to the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    /// Batch text, without the `GO` line.
    pub sql: String,
    /// 1-based line in the script where the batch starts. SQL Server reports
    /// error lines relative to the batch; add `start_line - 1` to map them
    /// back to the file.
    pub start_line: usize,
    /// How many times to run it (`GO <n>`); at least 1.
    pub repeat: u32,
}

/// A malformed `GO` line.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SplitError {
    /// `GO 0` or a count that doesn't fit in a `u32`.
    #[error("line {line}: invalid batch count `{count}` after GO (must be 1 or more)")]
    InvalidCount {
        /// 1-based line number.
        line: usize,
        /// The count as written.
        count: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Code,
    /// Inside `'...'`.
    String,
    /// Inside `[...]`.
    Bracket,
    /// Inside `"..."`.
    Quoted,
    /// Inside `/* ... */`, with nesting depth.
    Block(u32),
}

/// Split a script into batches. Empty or whitespace-only batches are dropped.
pub fn split(script: &str) -> Result<Vec<Batch>, SplitError> {
    let mut batches = Vec::new();
    let mut state = State::Code;
    let mut current = String::new();
    let mut start_line = 1;

    for (index, line) in script.split_inclusive('\n').enumerate() {
        let line_no = index + 1;
        if state == State::Code
            && let Some(repeat) = go_line(line, line_no)?
        {
            push(&mut batches, &mut current, start_line, repeat);
            start_line = line_no + 1;
            continue;
        }
        if current.is_empty() {
            start_line = line_no;
        }
        current.push_str(line);
        state = scan(line, state);
    }
    push(&mut batches, &mut current, start_line, 1);
    Ok(batches)
}

fn push(batches: &mut Vec<Batch>, current: &mut String, start_line: usize, repeat: u32) {
    let sql = std::mem::take(current);
    if !sql.trim().is_empty() {
        batches.push(Batch {
            sql,
            start_line,
            repeat,
        });
    }
}

/// `Some(repeat)` when `line` is a `GO` separator.
fn go_line(line: &str, line_no: usize) -> Result<Option<u32>, SplitError> {
    let text = line.trim();
    let text = match text.find("--") {
        Some(at) => text[..at].trim_end(),
        None => text,
    };
    let Some(prefix) = text.get(..2) else {
        return Ok(None);
    };
    if !prefix.eq_ignore_ascii_case("go") {
        return Ok(None);
    }
    let rest = &text[2..];
    if rest.is_empty() {
        return Ok(Some(1));
    }
    // `GO` must be followed by whitespace and a count (`GOTO`, `GO;` are not separators).
    if !rest.starts_with(char::is_whitespace) {
        return Ok(None);
    }
    let count = rest.trim();
    if !count.chars().all(|c| c.is_ascii_digit()) {
        return Ok(None);
    }
    match count.parse::<u32>() {
        Ok(n) if n >= 1 => Ok(Some(n)),
        _ => Err(SplitError::InvalidCount {
            line: line_no,
            count: count.to_owned(),
        }),
    }
}

/// A sqlcmd-only construct the native executor can't run (PROVIDERS.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlcmdUse {
    /// 1-based line.
    pub line: usize,
    /// What was found, e.g. `:setvar` or `$(var)`.
    pub what: String,
}

/// sqlcmd commands (a `:` at the start of a code line) and `$(var)`
/// references outside strings and comments.
pub fn sqlcmd_syntax(script: &str) -> Vec<SqlcmdUse> {
    const COMMANDS: [&str; 15] = [
        "setvar",
        "r",
        "on",
        "error",
        "connect",
        "exit",
        "out",
        "quit",
        "reset",
        "listvar",
        "ed",
        "serverlist",
        "xml",
        "help",
        "perftrace",
    ];
    let mut found = Vec::new();
    let mut state = State::Code;
    for (index, line) in script.split_inclusive('\n').enumerate() {
        let line_no = index + 1;
        if state == State::Code {
            let t = line.trim_start();
            if let Some(rest) = t.strip_prefix(':') {
                let word: String = rest
                    .chars()
                    .take_while(char::is_ascii_alphabetic)
                    .collect::<String>()
                    .to_ascii_lowercase();
                if COMMANDS.contains(&word.as_str()) {
                    found.push(SqlcmdUse {
                        line: line_no,
                        what: format!(":{word}"),
                    });
                }
            } else if t.starts_with("!!") {
                found.push(SqlcmdUse {
                    line: line_no,
                    what: "!! (shell command)".into(),
                });
            }
        }
        let (next, saw_var) = scan_line(line, state);
        if saw_var {
            found.push(SqlcmdUse {
                line: line_no,
                what: "$(variable)".into(),
            });
        }
        state = next;
    }
    found
}

/// Advance the lexical state across one line.
fn scan(line: &str, state: State) -> State {
    scan_line(line, state).0
}

/// Advance the lexical state across one line; also report a sqlcmd
/// `$(` variable reference in code.
fn scan_line(line: &str, mut state: State) -> (State, bool) {
    let bytes = line.as_bytes();
    let mut saw_var = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let next = bytes.get(i + 1).copied();
        match state {
            State::Code => match (c, next) {
                (b'-', Some(b'-')) => return (State::Code, saw_var), // line comment
                (b'$', Some(b'(')) => saw_var = true,
                (b'/', Some(b'*')) => {
                    state = State::Block(1);
                    i += 1;
                }
                (b'\'', _) => state = State::String,
                (b'[', _) => state = State::Bracket,
                (b'"', _) => state = State::Quoted,
                _ => {}
            },
            State::String => {
                if c == b'\'' {
                    if next == Some(b'\'') {
                        i += 1; // escaped quote
                    } else {
                        state = State::Code;
                    }
                }
            }
            State::Bracket => {
                if c == b']' {
                    if next == Some(b']') {
                        i += 1;
                    } else {
                        state = State::Code;
                    }
                }
            }
            State::Quoted => {
                if c == b'"' {
                    if next == Some(b'"') {
                        i += 1;
                    } else {
                        state = State::Code;
                    }
                }
            }
            State::Block(depth) => match (c, next) {
                (b'/', Some(b'*')) => {
                    state = State::Block(depth + 1);
                    i += 1;
                }
                (b'*', Some(b'/')) => {
                    state = if depth == 1 {
                        State::Code
                    } else {
                        State::Block(depth - 1)
                    };
                    i += 1;
                }
                _ => {}
            },
        }
        i += 1;
    }
    (state, saw_var)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// (sql, start_line, repeat) per batch.
    fn parts(script: &str) -> Vec<(String, usize, u32)> {
        split(script)
            .unwrap()
            .into_iter()
            .map(|b| (b.sql, b.start_line, b.repeat))
            .collect()
    }

    /// (name, script, expected (sql, start_line, repeat) per batch).
    type Case = (
        &'static str,
        &'static str,
        &'static [(&'static str, usize, u32)],
    );

    #[test]
    fn table_of_cases() {
        let cases: &[Case] = &[
            ("no GO", "SELECT 1;\n", &[("SELECT 1;\n", 1, 1)]),
            (
                "basic",
                "SELECT 1;\nGO\nSELECT 2;\n",
                &[("SELECT 1;\n", 1, 1), ("SELECT 2;\n", 3, 1)],
            ),
            (
                "case, spaces, trailing comment, no final newline",
                "A\n  go  -- end of a\nB",
                &[("A\n", 1, 1), ("B", 3, 1)],
            ),
            (
                "GO n repeats",
                "INSERT t DEFAULT VALUES;\nGO 3\n",
                &[("INSERT t DEFAULT VALUES;\n", 1, 3)],
            ),
            (
                "empty batches dropped",
                "GO\n\nGO\nSELECT 1;\nGO\n  \n",
                &[("SELECT 1;\n", 4, 1)],
            ),
            (
                "GO inside a string is text",
                "SELECT 'a\nGO\nb';\nGO\nSELECT 2;\n",
                &[("SELECT 'a\nGO\nb';\n", 1, 1), ("SELECT 2;\n", 5, 1)],
            ),
            (
                "escaped quote keeps the string open",
                "SELECT 'it''s\nGO\n';\n",
                &[("SELECT 'it''s\nGO\n';\n", 1, 1)],
            ),
            (
                "GO inside a block comment is text",
                "/*\nGO\n*/\nSELECT 1;\n",
                &[("/*\nGO\n*/\nSELECT 1;\n", 1, 1)],
            ),
            (
                "block comments nest",
                "/* outer /* inner */\nGO\n*/\nSELECT 1;\nGO\nSELECT 2;\n",
                &[
                    ("/* outer /* inner */\nGO\n*/\nSELECT 1;\n", 1, 1),
                    ("SELECT 2;\n", 6, 1),
                ],
            ),
            (
                "a quote in a line comment doesn't open a string",
                "-- don't\nSELECT 1;\nGO\nSELECT 2;\n",
                &[("-- don't\nSELECT 1;\n", 1, 1), ("SELECT 2;\n", 4, 1)],
            ),
            (
                "bracket and double-quoted identifiers",
                "SELECT 1 AS [x\nGO\n], 2 AS \"y\nGO\n\";\n",
                &[("SELECT 1 AS [x\nGO\n], 2 AS \"y\nGO\n\";\n", 1, 1)],
            ),
            (
                "GOTO, GO; and GOx are not separators",
                "GOTO done;\nGO;\nGOx\n",
                &[("GOTO done;\nGO;\nGOx\n", 1, 1)],
            ),
            (
                "CREATE PROCEDURE stays first in its batch",
                "CREATE TABLE t (id int);\nGO\nCREATE PROCEDURE p AS SELECT 1;\nGO\n",
                &[
                    ("CREATE TABLE t (id int);\n", 1, 1),
                    ("CREATE PROCEDURE p AS SELECT 1;\n", 3, 1),
                ],
            ),
        ];
        for (name, script, expected) in cases {
            let expected: Vec<_> = expected
                .iter()
                .map(|(s, l, r)| ((*s).to_owned(), *l, *r))
                .collect();
            assert_eq!(parts(script), expected, "{name}");
        }
    }

    #[test]
    fn invalid_counts_are_errors() {
        assert_eq!(
            split("SELECT 1;\nGO 0\n"),
            Err(SplitError::InvalidCount {
                line: 2,
                count: "0".into()
            })
        );
        assert!(split("GO 99999999999\n").is_err());
    }

    #[test]
    fn finds_sqlcmd_only_syntax_outside_strings_and_comments() {
        let script = ":setvar Schema dbo\n\
                      SELECT * FROM $(Schema).t;\n\
                      :r .\\other.sql\n\
                      :ON ERROR EXIT\n\
                      SELECT ':r not a command', '$(nope)';\n\
                      -- :setvar in a comment\n\
                      /* $(also) in a block */\n\
                      SELECT 1 AS [a:r], x FROM t WHERE y = 'z'; :setvar inline_is_not_a_command\n\
                      !!dir\n";
        let found: Vec<(usize, String)> = sqlcmd_syntax(script)
            .into_iter()
            .map(|u| (u.line, u.what))
            .collect();
        assert_eq!(
            found,
            [
                (1, ":setvar".to_owned()),
                (2, "$(variable)".to_owned()),
                (3, ":r".to_owned()),
                (4, ":on".to_owned()),
                (9, "!! (shell command)".to_owned()),
            ]
        );
        assert!(sqlcmd_syntax("SELECT 1;\nGO\n").is_empty());
    }

    #[test]
    fn batches_reassemble_the_non_separator_lines() {
        // Every non-GO line ends up in exactly one batch, in order.
        let script = "A\nGO\n'x\nGO\n'\nGO 2\n/*\nGO\n*/B\n";
        let joined: String = split(script).unwrap().into_iter().map(|b| b.sql).collect();
        assert_eq!(joined, "A\n'x\nGO\n'\n/*\nGO\n*/B\n");
    }
}
