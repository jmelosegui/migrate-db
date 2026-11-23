//! Shared command context: config loading, env lookup, CI detection.

use std::collections::BTreeMap;
use std::fs;
use std::io::IsTerminal;
use std::path::Path;

use migrate_db_core::config::{Config, ResolvedTarget, Secret, TargetSettings};
use migrate_db_core::error::Error;
use migrate_db_core::placeholders::Values;
use migrate_db_core::provider::RunIdentity;
use migrate_db_core::source::{DiscoveryOptions, MigrationSet, discover};

use crate::cli::{ConnectArgs, GlobalArgs};
use crate::providers;

/// Loaded config plus helpers.
#[derive(Debug)]
pub struct Context {
    /// The config.
    pub config: Config,
    env_file: BTreeMap<String, String>,
}

impl Context {
    /// Load config from `--config` or by discovery from the current
    /// directory. `connect` is `None` for commands that never connect.
    pub fn load(global: &GlobalArgs, connect: Option<&ConnectArgs>) -> Result<Self, Error> {
        let config = match &global.config {
            Some(path) => Config::load(path)?,
            None => {
                let cwd = std::env::current_dir()
                    .map_err(|e| Error::Usage(format!("cannot read current directory: {e}")))?;
                Config::discover(&cwd)?
            }
        };
        let env_file = match connect.and_then(|c| c.env_file.as_ref()) {
            Some(path) => parse_env_file(path)?,
            None => BTreeMap::new(),
        };
        Ok(Self { config, env_file })
    }

    /// A `--env-file` value. (Flags and process variables are read by clap
    /// and take precedence.)
    fn file_value(&self, name: &str) -> Option<String> {
        self.env_file.get(name).filter(|v| !v.is_empty()).cloned()
    }

    fn file_flag(&self, name: &str) -> bool {
        self.file_value(name).is_some_and(|v| boolish(&v))
    }

    /// The database's settings.
    pub fn target(&self) -> TargetSettings {
        self.config.target()
    }

    /// The database plus this run's connection and auth (ADR-0005).
    pub fn resolve(&self, connect: &ConnectArgs) -> ResolvedTarget {
        let connection = connect
            .connection
            .clone()
            .filter(|c| !c.is_empty())
            .or_else(|| self.file_value("MIGRATE_DB_CONNECTION"))
            .map(Secret::new);
        let auth = connect
            .auth
            .clone()
            .filter(|a| !a.is_empty())
            .or_else(|| self.file_value("MIGRATE_DB_AUTH"));
        self.config.resolve(connection, auth)
    }

    /// Placeholder values (`MIGRATE_DB_PLACEHOLDER_<NAME>`: process
    /// environment first, then `--env-file`), or `None` when the config
    /// doesn't enable placeholders (SPEC §3.3).
    pub fn placeholders(&self) -> Option<Values> {
        if !self.config.target().placeholders_enabled {
            return None;
        }
        let process: Vec<(String, String)> = std::env::vars().collect();
        Some(Values::from_vars(
            process
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .chain(self.env_file.iter().map(|(k, v)| (k.as_str(), v.as_str()))),
        ))
    }

    /// `--allow-down` / `MIGRATE_DB_ALLOW_DOWN` for this run.
    pub fn allow_down(&self, flag: bool) -> bool {
        flag || self.file_flag("MIGRATE_DB_ALLOW_DOWN")
    }

    /// `--allow-out-of-order` / `MIGRATE_DB_ALLOW_OUT_OF_ORDER` for this run.
    pub fn allow_out_of_order(&self, flag: bool) -> bool {
        flag || self.file_flag("MIGRATE_DB_ALLOW_OUT_OF_ORDER")
    }

    /// Discover the target's migration files.
    pub fn discover(&self, target: &TargetSettings) -> Result<MigrationSet, Error> {
        let options = DiscoveryOptions {
            extensions: providers::extensions(target.provider)
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            strict_filenames: target.strict_filenames,
        };
        Ok(discover(&target.migrations_dir, &options)?)
    }
}

/// Parse a `KEY=VALUE` file. `#` starts a comment line; values may be quoted.
fn parse_env_file(path: &Path) -> Result<BTreeMap<String, String>, Error> {
    let text = fs::read_to_string(path)
        .map_err(|e| Error::Usage(format!("cannot read --env-file `{}`: {e}", path.display())))?;
    let mut vars = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (key, value) = line.split_once('=').ok_or_else(|| {
            Error::Usage(format!("{}:{}: expected KEY=VALUE", path.display(), i + 1))
        })?;
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        vars.insert(key.trim().to_owned(), value.to_owned());
    }
    Ok(vars)
}

/// `1` / `true` / `yes` / `on` (as clap's boolish parser).
fn boolish(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "y" | "on"
    )
}

/// `true` when running in CI (or `--ci` / `MIGRATE_DB_CI`).
pub fn is_ci(global: &GlobalArgs) -> bool {
    global.ci
        || ["TF_BUILD", "GITHUB_ACTIONS", "CI"].iter().any(|v| {
            std::env::var(v)
                .is_ok_and(|x| !x.is_empty() && x != "0" && !x.eq_ignore_ascii_case("false"))
        })
}

/// `true` when prompts are allowed.
pub fn is_interactive(global: &GlobalArgs) -> bool {
    !is_ci(global) && std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Who is running, recorded in the ledger.
pub fn identity() -> RunIdentity {
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown".into());
    let host = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".into());
    RunIdentity {
        applied_by: format!("{user}@{host}"),
        host,
        tool_version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

/// CI run reference for lock owners and logs (SPEC §7 rule 10).
pub fn ci_run() -> Option<String> {
    let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    if let Some(id) = get("BUILD_BUILDID") {
        let project = get("SYSTEM_TEAMPROJECT").unwrap_or_default();
        return Some(format!("azure-pipelines:{project}#{id}"));
    }
    if let Some(id) = get("GITHUB_RUN_ID") {
        let repo = get("GITHUB_REPOSITORY").unwrap_or_default();
        return Some(format!("github:{repo}#{id}"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_file_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.env");
        fs::write(
            &path,
            "# comment\nA=1\nexport B = \"two words\"\nC='x=y'\n\n",
        )
        .unwrap();
        let vars = parse_env_file(&path).unwrap();
        assert_eq!(vars["A"], "1");
        assert_eq!(vars["B"], "two words");
        assert_eq!(vars["C"], "x=y");
        fs::write(&path, "NOEQUALS\n").unwrap();
        assert!(parse_env_file(&path).is_err());
    }
}
