//! Discovery of migration files (SPEC §3.1).

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::checksum::{Checksum, normalize};
use crate::directives::{DirectiveError, Directives, parse_sql_header};
use crate::model::Direction;
use crate::version::Version;

/// Suffix of schema-fingerprint files written by `baseline capture`.
/// They live next to migrations but are never migrations themselves.
pub const FINGERPRINT_SUFFIX: &str = ".fingerprint.json";

/// A parsed migration file name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedName {
    /// `V<version>__<name>.<up|down>.<ext>`
    Versioned {
        /// The version.
        version: Version,
        /// The snake_case name part.
        name: String,
        /// `up` or `down`.
        direction: Direction,
        /// Lowercase extension.
        extension: String,
    },
    /// `R__<name>.<ext>`
    Repeatable {
        /// The name part (may contain dots, e.g. `dbo.usp_close`).
        name: String,
        /// Lowercase extension.
        extension: String,
    },
}

/// Parse a file name. Returns `None` if it doesn't follow either pattern.
pub fn parse_file_name(file_name: &str) -> Option<ParsedName> {
    let (stem, extension) = file_name.rsplit_once('.')?;
    if extension.is_empty() {
        return None;
    }
    let extension = extension.to_ascii_lowercase();

    if let Some(name) = stem.strip_prefix("R__") {
        if name.is_empty() {
            return None;
        }
        return Some(ParsedName::Repeatable {
            name: name.to_owned(),
            extension,
        });
    }

    let rest = stem.strip_prefix('V')?;
    let (head, direction) = rest.rsplit_once('.')?;
    let direction = match direction {
        "up" => Direction::Up,
        "down" => Direction::Down,
        _ => return None,
    };
    let (version, name) = head.split_once("__")?;
    if name.is_empty() || name.contains('.') {
        return None;
    }
    let version = version.parse().ok()?;
    Some(ParsedName::Versioned {
        version,
        name: name.to_owned(),
        direction,
        extension,
    })
}

/// A script file loaded from disk.
#[derive(Debug, Clone)]
pub struct ScriptFile {
    /// Absolute (or as-given) path on disk.
    pub path: PathBuf,
    /// Path relative to the migrations directory, with `/` separators.
    pub relative_path: String,
    /// File name only; stored in the ledger as `script_name`.
    pub file_name: String,
    /// Lowercase extension.
    pub extension: String,
    /// Normalized UTF-8 content (BOM stripped, LF line endings).
    pub content: String,
    /// Checksum of the normalized content.
    pub checksum: Checksum,
    /// Header directives (SQL files only; default otherwise).
    pub directives: Directives,
}

/// A versioned migration: an up script and an optional down script.
#[derive(Debug, Clone)]
pub struct VersionedMigration {
    /// The version.
    pub version: Version,
    /// Snake_case name from the file name.
    pub name: String,
    /// Human-readable description (`create_users` → `create users`).
    pub description: String,
    /// The up script.
    pub up: ScriptFile,
    /// The down script; `None` means irreversible.
    pub down: Option<ScriptFile>,
}

impl VersionedMigration {
    /// `true` if there is no down script.
    pub fn is_irreversible(&self) -> bool {
        self.down.is_none()
    }
}

/// A repeatable migration (`R__<name>`).
#[derive(Debug, Clone)]
pub struct RepeatableMigration {
    /// The name part of the file name.
    pub name: String,
    /// The script.
    pub script: ScriptFile,
}

/// A non-fatal discovery finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceWarning {
    /// A file that matches neither naming pattern (or has a foreign extension).
    UnrecognizedFile(String),
}

impl fmt::Display for SourceWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnrecognizedFile(p) => write!(f, "ignored file `{p}` (not a migration name)"),
        }
    }
}

/// A discovery error.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// The migrations directory doesn't exist or can't be read.
    #[error("cannot read migrations directory `{path}`: {source}")]
    Directory {
        /// The directory.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A file couldn't be read.
    #[error("cannot read `{path}`: {source}")]
    Read {
        /// The file.
        path: String,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A file isn't valid UTF-8.
    #[error("`{path}` is not valid UTF-8")]
    NotUtf8 {
        /// The file.
        path: String,
    },
    /// Two files claim the same version and direction (or names differ within a pair).
    #[error("duplicate version {version}: `{first}` and `{second}`")]
    DuplicateVersion {
        /// The version.
        version: String,
        /// First file.
        first: String,
        /// Second file.
        second: String,
    },
    /// A down file without a matching up file.
    #[error("`{path}` has no matching up file")]
    OrphanDown {
        /// The down file.
        path: String,
    },
    /// Two repeatables with the same name.
    #[error("duplicate repeatable `R__{name}`: `{first}` and `{second}`")]
    DuplicateRepeatable {
        /// The name.
        name: String,
        /// First file.
        first: String,
        /// Second file.
        second: String,
    },
    /// A malformed directive.
    #[error("`{path}`: {source}")]
    Directive {
        /// The file.
        path: String,
        /// The directive problem.
        #[source]
        source: DirectiveError,
    },
    /// Strict mode: a file that isn't a migration.
    #[error("`{path}` is not a migration file name (strict_filenames is on)")]
    Unrecognized {
        /// The file.
        path: String,
    },
}

/// All problems found during discovery.
#[derive(Debug)]
pub struct SourceErrors(pub Vec<SourceError>);

impl fmt::Display for SourceErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, e) in self.0.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(f, "{e}")?;
        }
        Ok(())
    }
}

impl std::error::Error for SourceErrors {}

/// Options for [`discover`].
#[derive(Debug, Clone)]
pub struct DiscoveryOptions {
    /// Extensions this provider accepts (lowercase, without dot).
    pub extensions: Vec<String>,
    /// Treat unrecognized files as errors instead of warnings.
    pub strict_filenames: bool,
}

/// The migrations found in a directory.
#[derive(Debug, Clone, Default)]
pub struct MigrationSet {
    /// Versioned migrations, ascending by version.
    pub versioned: Vec<VersionedMigration>,
    /// Repeatable migrations, ascending by name.
    pub repeatables: Vec<RepeatableMigration>,
    /// Non-fatal findings.
    pub warnings: Vec<SourceWarning>,
}

impl MigrationSet {
    /// The versioned migration with this version, if any.
    pub fn get(&self, version: &Version) -> Option<&VersionedMigration> {
        self.versioned
            .binary_search_by(|m| m.version.cmp(version))
            .ok()
            .map(|i| &self.versioned[i])
    }

    /// The highest versioned migration, if any.
    pub fn latest(&self) -> Option<&VersionedMigration> {
        self.versioned.last()
    }
}

#[derive(Default)]
struct Pair {
    name: Option<String>,
    up: Option<ScriptFile>,
    down: Option<ScriptFile>,
}

/// Recursively discover migrations under `dir`.
///
/// Hidden files and directories (`.` prefix) and `*.fingerprint.json` files
/// are skipped. Every problem is collected, so `validate` can report all of
/// them at once.
pub fn discover(dir: &Path, options: &DiscoveryOptions) -> Result<MigrationSet, SourceErrors> {
    if let Err(source) = fs::read_dir(dir) {
        return Err(SourceErrors(vec![SourceError::Directory {
            path: dir.to_path_buf(),
            source,
        }]));
    }

    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut pairs: BTreeMap<Version, Pair> = BTreeMap::new();
    let mut repeatables: BTreeMap<String, ScriptFile> = BTreeMap::new();

    let walker = walkdir::WalkDir::new(dir)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.'));

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                let path = e
                    .path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                errors.push(SourceError::Read {
                    path,
                    source: e.into(),
                });
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy().into_owned();
        let relative = relative_path(dir, entry.path());
        if file_name.ends_with(FINGERPRINT_SUFFIX) {
            continue;
        }

        let parsed = parse_file_name(&file_name)
            .filter(|p| options.extensions.iter().any(|e| e == extension_of(p)));
        let Some(parsed) = parsed else {
            if options.strict_filenames {
                errors.push(SourceError::Unrecognized { path: relative });
            } else {
                warnings.push(SourceWarning::UnrecognizedFile(relative));
            }
            continue;
        };

        let script = match load_script(entry.path(), &relative, &file_name, extension_of(&parsed)) {
            Ok(s) => s,
            Err(e) => {
                errors.push(e);
                continue;
            }
        };

        match parsed {
            ParsedName::Versioned {
                version,
                name,
                direction,
                ..
            } => {
                let pair = pairs.entry(version.clone()).or_default();
                if let Some(existing) = &pair.name
                    && existing != &name
                {
                    let first = pair
                        .up
                        .as_ref()
                        .or(pair.down.as_ref())
                        .map(|s| s.relative_path.clone())
                        .unwrap_or_default();
                    errors.push(SourceError::DuplicateVersion {
                        version: version.to_string(),
                        first,
                        second: script.relative_path,
                    });
                    continue;
                }
                pair.name = Some(name);
                let slot = match direction {
                    Direction::Up => &mut pair.up,
                    Direction::Down => &mut pair.down,
                };
                if let Some(existing) = slot {
                    errors.push(SourceError::DuplicateVersion {
                        version: version.to_string(),
                        first: existing.relative_path.clone(),
                        second: script.relative_path,
                    });
                } else {
                    *slot = Some(script);
                }
            }
            ParsedName::Repeatable { name, .. } => {
                if let Some(existing) = repeatables.get(&name) {
                    errors.push(SourceError::DuplicateRepeatable {
                        name,
                        first: existing.relative_path.clone(),
                        second: script.relative_path,
                    });
                } else {
                    repeatables.insert(name, script);
                }
            }
        }
    }

    let mut versioned = Vec::with_capacity(pairs.len());
    for (version, pair) in pairs {
        match (pair.up, pair.name) {
            (Some(up), Some(name)) => versioned.push(VersionedMigration {
                version,
                description: name.replace('_', " "),
                name,
                up,
                down: pair.down,
            }),
            _ => {
                if let Some(down) = pair.down {
                    errors.push(SourceError::OrphanDown {
                        path: down.relative_path,
                    });
                }
            }
        }
    }

    if !errors.is_empty() {
        return Err(SourceErrors(errors));
    }
    Ok(MigrationSet {
        versioned,
        repeatables: repeatables
            .into_iter()
            .map(|(name, script)| RepeatableMigration { name, script })
            .collect(),
        warnings,
    })
}

fn extension_of(parsed: &ParsedName) -> &str {
    match parsed {
        ParsedName::Versioned { extension, .. } | ParsedName::Repeatable { extension, .. } => {
            extension
        }
    }
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn load_script(
    path: &Path,
    relative: &str,
    file_name: &str,
    extension: &str,
) -> Result<ScriptFile, SourceError> {
    let bytes = fs::read(path).map_err(|source| SourceError::Read {
        path: relative.to_owned(),
        source,
    })?;
    let normalized = normalize(&bytes);
    let checksum = Checksum::of_normalized(&normalized);
    let content = String::from_utf8(normalized).map_err(|_| SourceError::NotUtf8 {
        path: relative.to_owned(),
    })?;
    let directives = if extension == "sql" {
        parse_sql_header(&content).map_err(|source| SourceError::Directive {
            path: relative.to_owned(),
            source,
        })?
    } else {
        Directives::default()
    };
    Ok(ScriptFile {
        path: path.to_path_buf(),
        relative_path: relative.to_owned(),
        file_name: file_name.to_owned(),
        extension: extension.to_owned(),
        content,
        checksum,
        directives,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sql_options() -> DiscoveryOptions {
        DiscoveryOptions {
            extensions: vec!["sql".into()],
            strict_filenames: false,
        }
    }

    fn write(dir: &Path, rel: &str, content: &[u8]) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn parses_versioned_and_repeatable_names() {
        assert_eq!(
            parse_file_name("V20261109142530__create_users.up.sql"),
            Some(ParsedName::Versioned {
                version: "20261109142530".parse().unwrap(),
                name: "create_users".into(),
                direction: Direction::Up,
                extension: "sql".into(),
            })
        );
        assert!(matches!(
            parse_file_name("V1__x.down.SQL"),
            Some(ParsedName::Versioned { direction: Direction::Down, ref extension, .. }) if extension == "sql"
        ));
        assert_eq!(
            parse_file_name("R__dbo.usp_close_order.sql"),
            Some(ParsedName::Repeatable {
                name: "dbo.usp_close_order".into(),
                extension: "sql".into(),
            })
        );
    }

    #[test]
    fn rejects_malformed_names() {
        for name in [
            "V1__x.sql",       // no direction
            "V1_x.up.sql",     // single underscore
            "Vx__name.up.sql", // non-numeric version
            "V1__.up.sql",     // empty name
            "V1__a.b.up.sql",  // dot in versioned name
            "R__.sql",         // empty repeatable name
            "readme.md",
            "V1__x.sideways.sql",
        ] {
            assert_eq!(parse_file_name(name), None, "{name}");
        }
    }

    #[test]
    fn discovers_pairs_recursively_in_version_order() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        write(d, "V2__add_index.up.sql", b"create index i on t(c);\n");
        write(d, "V1__create_t.up.sql", b"create table t(c int);\r\n");
        write(d, "V1__create_t.down.sql", b"drop table t;\n");
        write(
            d,
            "programmability/R__dbo.v_t.sql",
            b"create or alter view v as select 1;\n",
        );
        write(d, "V2__add_index.fingerprint.json", b"{}");
        write(d, ".gitkeep", b"");
        write(d, "notes.txt", b"hello");

        let set = discover(d, &sql_options()).unwrap();
        let versions: Vec<_> = set
            .versioned
            .iter()
            .map(|m| m.version.to_string())
            .collect();
        assert_eq!(versions, ["1", "2"]);
        assert_eq!(set.versioned[0].description, "create t");
        assert!(!set.versioned[0].is_irreversible());
        assert!(set.versioned[1].is_irreversible());
        assert_eq!(set.versioned[0].up.content, "create table t(c int);\n");
        assert_eq!(set.repeatables.len(), 1);
        assert_eq!(
            set.repeatables[0].script.relative_path,
            "programmability/R__dbo.v_t.sql"
        );
        assert_eq!(
            set.warnings,
            vec![SourceWarning::UnrecognizedFile("notes.txt".into())]
        );
    }

    #[test]
    fn reports_every_problem() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        write(d, "V1__a.up.sql", b"select 1;");
        write(d, "V0001__b.up.sql", b"select 2;"); // same version, different name
        write(d, "V3__orphan.down.sql", b"select 3;");
        write(d, "V4__bad.up.sql", b"-- migrate:bogus\nselect 4;");
        write(d, "V5__latin1.up.sql", b"select '\xE9';");
        write(d, "R__x.sql", b"select 5;");
        write(d, "sub/R__x.sql", b"select 6;");

        let errors = discover(d, &sql_options()).unwrap_err().0;
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, SourceError::DuplicateVersion { .. }))
        );
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, SourceError::OrphanDown { .. }))
        );
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, SourceError::Directive { .. }))
        );
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, SourceError::NotUtf8 { .. }))
        );
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, SourceError::DuplicateRepeatable { .. }))
        );
    }

    #[test]
    fn strict_mode_turns_unknown_files_into_errors() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "notes.txt", b"hello");
        let options = DiscoveryOptions {
            strict_filenames: true,
            ..sql_options()
        };
        let errors = discover(tmp.path(), &options).unwrap_err().0;
        assert!(matches!(errors[0], SourceError::Unrecognized { .. }));
    }

    #[test]
    fn foreign_extensions_are_not_migrations() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "V1__a.up.yaml", b"commands: []");
        let set = discover(tmp.path(), &sql_options()).unwrap();
        assert!(set.versioned.is_empty());
        assert_eq!(set.warnings.len(), 1);
    }

    #[test]
    fn missing_directory_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let errors = discover(&tmp.path().join("nope"), &sql_options())
            .unwrap_err()
            .0;
        assert!(matches!(errors[0], SourceError::Directory { .. }));
    }

    #[test]
    fn directives_are_read_from_sql_headers() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "V1__idx.up.sql",
            b"-- migrate:no-transaction\nCREATE INDEX CONCURRENTLY i ON t(c);",
        );
        let set = discover(tmp.path(), &sql_options()).unwrap();
        assert!(set.versioned[0].up.directives.no_transaction);
    }
}
