//! Configuration (SPEC §5): loading, discovery and resolution.
//!
//! One `migrate-db.toml` describes **one database** (ADR-0005): its provider,
//! its migrations folder and its ledger, as flat top-level keys. It is
//! environment-agnostic: where to connect, how to authenticate and the
//! safety switches for a run come from that run's flags and environment
//! variables, so the pipeline stage, not the repository, decides them.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Default config file name.
pub const CONFIG_FILE_NAME: &str = "migrate-db.toml";

/// Default migrations folder, relative to the config file.
pub const DEFAULT_MIGRATIONS_DIR: &str = "migrations";

/// Database technology.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// PostgreSQL.
    Postgres,
    /// SQL Server / Azure SQL.
    Mssql,
    /// MongoDB.
    Mongo,
    /// Cosmos DB (NoSQL API).
    Cosmos,
}

impl ProviderKind {
    /// Config text value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Mssql => "mssql",
            Self::Mongo => "mongo",
            Self::Cosmos => "cosmos",
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Version scheme for new migrations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Versioning {
    /// UTC `YYYYMMDDHHMMSS` (default).
    #[default]
    Timestamp,
    /// Increasing integers.
    Sequential,
}

/// `placeholders = { enabled = … }`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaceholderSettings {
    /// Whether `${name}` substitution is on.
    #[serde(default)]
    pub enabled: bool,
}

/// `runner = { … }` for external runners.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerConfig {
    /// Binary name or path.
    pub path: Option<String>,
    /// Minimum required version.
    pub min_version: Option<String>,
}

/// The whole `migrate-db.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    /// Database technology.
    pub provider: ProviderKind,
    /// Migrations folder, relative to the config file (default `migrations`).
    #[serde(default = "default_migrations_dir")]
    pub migrations_dir: PathBuf,
    /// Version scheme for `new`.
    #[serde(default)]
    pub versioning: Versioning,
    /// Treat non-migration files in the migrations folder as errors.
    #[serde(default)]
    pub strict_filenames: bool,
    /// Provider-specific ledger location.
    #[serde(default)]
    pub ledger: toml::Table,
    /// Placeholder settings.
    #[serde(default)]
    pub placeholders: PlaceholderSettings,
    /// Default executor.
    pub executor: Option<String>,
    /// External runner settings.
    pub runner: Option<RunnerConfig>,
    /// Default format for `new` (Mongo).
    pub new_format: Option<String>,
}

fn default_migrations_dir() -> PathBuf {
    PathBuf::from(DEFAULT_MIGRATIONS_DIR)
}

/// A loaded config file.
#[derive(Debug, Clone)]
pub struct Config {
    /// Path of the file.
    pub path: PathBuf,
    /// Directory containing the file; relative paths resolve against it.
    pub root: PathBuf,
    /// Parsed content.
    pub file: ConfigFile,
}

/// A secret value; never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wrap a secret.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Access the value. Never log the result.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// The database's settings from the config file.
#[derive(Debug, Clone)]
pub struct TargetSettings {
    /// Provider.
    pub provider: ProviderKind,
    /// Absolute migrations directory.
    pub migrations_dir: PathBuf,
    /// Ledger location table.
    pub ledger: toml::Table,
    /// Version scheme.
    pub versioning: Versioning,
    /// Strict file names.
    pub strict_filenames: bool,
    /// Placeholders enabled.
    pub placeholders_enabled: bool,
    /// Default executor.
    pub executor: Option<String>,
    /// Runner settings.
    pub runner: Option<RunnerConfig>,
    /// Default `new` format.
    pub new_format: Option<String>,
}

/// The database plus the connection for one run.
#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    /// The database's settings.
    pub target: TargetSettings,
    /// Connection string (`--connection` / `MIGRATE_DB_CONNECTION`).
    pub connection: Option<Secret>,
    /// Auth mode (`--auth` / `MIGRATE_DB_AUTH`): `sql` or `entra`.
    pub auth: Option<String>,
}

/// Configuration problems.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// No config file found.
    #[error(
        "no {CONFIG_FILE_NAME} found in `{start}` or its parents (up to the repository root); run `migrate-db init`"
    )]
    NotFound {
        /// Where the search started.
        start: PathBuf,
    },
    /// The file couldn't be read.
    #[error("cannot read `{path}`: {source}")]
    Read {
        /// The file.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The file isn't valid TOML or doesn't match the schema.
    #[error("invalid config `{path}`: {message}")]
    Parse {
        /// The file.
        path: PathBuf,
        /// Parser message.
        message: String,
    },
}

impl ConfigError {
    /// `true` when the config is missing (usage error) rather than invalid.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound { .. })
    }
}

/// Find `migrate-db.toml` in `start` or its ancestors, stopping at the
/// repository root (the first directory that contains `.git`).
pub fn find_config(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let candidate = dir.join(CONFIG_FILE_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        if dir.join(".git").exists() {
            return None;
        }
    }
    None
}

/// Why a key from the earlier config shape fails (ADR-0005).
fn removed_key(key: &str) -> Option<&'static str> {
    match key {
        "environments" => Some(
            "`[environments]` is no longer supported: the config is environment-agnostic \
             (ADR-0005). Delete those tables and give each run its settings instead: the \
             connection in MIGRATE_DB_CONNECTION (or --connection), the auth mode in \
             MIGRATE_DB_AUTH (or --auth), and --allow-down / MIGRATE_DB_ALLOW_DOWN=1 where \
             reverting is allowed",
        ),
        "targets" => Some(
            "`[targets.<name>]` is no longer supported: one config describes one database \
             (ADR-0005). Move that target's keys (provider, migrations_dir, ledger, …) to the \
             top level of the file; use one folder with its own migrate-db.toml per database",
        ),
        "project" => Some(
            "`[project]` is no longer supported (ADR-0005): move `versioning` and \
             `strict_filenames` to the top level of the file",
        ),
        _ => None,
    }
}

impl Config {
    /// Load a config file from an explicit path.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(path, &text)
    }

    /// Discover and load the config, starting at `start`.
    pub fn discover(start: &Path) -> Result<Self, ConfigError> {
        let path = find_config(start).ok_or_else(|| ConfigError::NotFound {
            start: start.to_path_buf(),
        })?;
        Self::load(&path)
    }

    /// Parse config text that came from `path`.
    pub fn parse(path: &Path, text: &str) -> Result<Self, ConfigError> {
        let parse_err = |message: String| ConfigError::Parse {
            path: path.to_path_buf(),
            message,
        };
        // Say why a former key fails, instead of a bare "unknown field".
        if let Ok(raw) = text.parse::<toml::Table>()
            && let Some(message) = raw.keys().find_map(|k| removed_key(k))
        {
            return Err(parse_err(message.to_owned()));
        }
        let file: ConfigFile = toml::from_str(text).map_err(|e| parse_err(e.to_string()))?;
        let root = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(Self {
            path: path.to_path_buf(),
            root,
            file,
        })
    }

    /// The database's settings.
    pub fn target(&self) -> TargetSettings {
        let f = &self.file;
        TargetSettings {
            migrations_dir: self.root.join(&f.migrations_dir),
            provider: f.provider,
            ledger: f.ledger.clone(),
            versioning: f.versioning,
            strict_filenames: f.strict_filenames,
            placeholders_enabled: f.placeholders.enabled,
            executor: f.executor.clone(),
            runner: f.runner.clone(),
            new_format: f.new_format.clone(),
        }
    }

    /// The database together with this run's connection settings.
    pub fn resolve(&self, connection: Option<Secret>, auth: Option<String>) -> ResolvedTarget {
        ResolvedTarget {
            target: self.target(),
            connection,
            auth,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
provider = "mssql"
migrations_dir = "sql"
versioning = "sequential"
ledger = { schema = "dbo", table = "__migrations" }
placeholders = { enabled = true }
executor = "native"
runner = { path = "sqlcmd", min_version = "1.8.0" }
"#;

    fn sample() -> Config {
        Config::parse(Path::new("/repo/db/migrate-db.toml"), SAMPLE).unwrap()
    }

    #[test]
    fn resolves_relative_paths_against_the_config_file() {
        let s = sample().target();
        assert_eq!(s.migrations_dir, Path::new("/repo/db").join("sql"));
        assert_eq!(s.provider, ProviderKind::Mssql);
        assert_eq!(s.versioning, Versioning::Sequential);
        assert!(s.placeholders_enabled);
    }

    #[test]
    fn minimal_config_defaults_to_the_migrations_folder() {
        let c =
            Config::parse(Path::new("/p/migrate-db.toml"), "provider = \"postgres\"\n").unwrap();
        let s = c.target();
        assert_eq!(s.migrations_dir, Path::new("/p").join("migrations"));
        assert_eq!(s.versioning, Versioning::Timestamp);
    }

    #[test]
    fn resolve_carries_the_run_connection_and_hides_it() {
        let r = sample().resolve(
            Some(Secret::new("Server=x;Password=s3cret")),
            Some("sql".into()),
        );
        let conn = r.connection.unwrap();
        assert_eq!(conn.expose(), "Server=x;Password=s3cret");
        assert_eq!(format!("{conn:?}"), "Secret(***)");
        assert_eq!(r.auth.as_deref(), Some("sql"));
    }

    #[test]
    fn earlier_shapes_are_rejected_with_the_replacement() {
        for (text, expect) in [
            (
                "provider = \"mssql\"\n[environments.dev]\nallow_down = true\n",
                "MIGRATE_DB_CONNECTION",
            ),
            (
                "[targets.app]\nprovider = \"mssql\"\nmigrations_dir = \"m\"\n",
                "one database",
            ),
            (
                "provider = \"mssql\"\n[project]\nversioning = \"timestamp\"\n",
                "top level",
            ),
        ] {
            let err = Config::parse(Path::new("migrate-db.toml"), text)
                .unwrap_err()
                .to_string();
            assert!(err.contains("ADR-0005") && err.contains(expect), "{err}");
        }
    }

    #[test]
    fn unknown_keys_and_a_missing_provider_are_rejected() {
        for text in [
            "provider = \"postgres\"\nmigrations_folder = \"x\"\n",
            "migrations_dir = \"m\"\n",
        ] {
            assert!(matches!(
                Config::parse(Path::new("migrate-db.toml"), text),
                Err(ConfigError::Parse { .. })
            ));
        }
    }

    #[test]
    fn discovery_walks_up_but_stops_at_the_repo_root() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path();
        fs::write(outer.join(CONFIG_FILE_NAME), "").unwrap(); // outside the repo
        let repo = outer.join("repo");
        let nested = repo.join("db").join("sub");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(repo.join(".git")).unwrap();

        assert_eq!(find_config(&nested), None, "must not escape the repo");

        fs::write(repo.join("db").join(CONFIG_FILE_NAME), "").unwrap();
        assert_eq!(
            find_config(&nested),
            Some(repo.join("db").join(CONFIG_FILE_NAME))
        );
    }
}
