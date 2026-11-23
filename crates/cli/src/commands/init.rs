//! `migrate-db init`: guided setup (SPEC §5.2).
//!
//! One project folder holds **one database** (ADR-0005): `init` writes a
//! flat `migrate-db.toml` and a `migrations/` folder. The config is
//! environment-agnostic: no connection strings, environment names or
//! secrets. Each run gets those from `MIGRATE_DB_*` variables or flags.
//!
//! 1. **Detection** (read-only): existing tooling and SQL files become
//!    suggestions and warnings.
//! 2. **Questions**, each with a flag. Interactive in a terminal; otherwise
//!    answers come from flags and defaults. The provider is required unless
//!    `--yes` accepts the detected one; a missing answer is exit 64.
//! 3. **Output**: the config and the migrations folder (with `.gitkeep`),
//!    then `validate --offline` and an optional read-only connection test.

use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::json;

use migrate_db_core::config::{
    CONFIG_FILE_NAME, Config, DEFAULT_MIGRATIONS_DIR, ProviderKind, Secret,
};
use migrate_db_core::engine::Engine;
use migrate_db_core::error::{Error, exit};
use migrate_db_core::planner::Policy;
use migrate_db_core::source::{DiscoveryOptions, discover};

use crate::cli::{GlobalArgs, InitArgs};
use crate::context::{identity, is_interactive};
use crate::output::Output;
use crate::providers;

/// Providers `init` can configure in this phase (PLAN.md).
const AVAILABLE: [ProviderKind; 2] = [ProviderKind::Mssql, ProviderKind::Postgres];

/// What `init` will write.
#[derive(Debug)]
struct Answers {
    provider: ProviderKind,
    migrations_dir: String,
    ledger: String,
    versioning: String,
}

/// Run `init`.
pub async fn run(global: &GlobalArgs, out: &Output, args: &InitArgs) -> Result<i32, Error> {
    let root = project_root(args.path.as_deref())?;
    let config_path = root.join(CONFIG_FILE_NAME);
    if config_path.exists() {
        return Err(Error::Usage(format!(
            "{} already exists: this folder is set up. One folder holds one database; \
             edit the file by hand to change it, or run init in another folder for another database",
            config_path.display()
        )));
    }
    let ask = Prompter::new(is_interactive(global) && !out.is_json());
    let found = detect(&root);
    if ask.interactive {
        ask.say(&format!("migrate-db init in {}\n", root.display()));
        for w in &found.warnings {
            ask.say(&format!("  note: {w}\n"));
        }
    }

    let answers = questions(&ask, args, &found)?;
    let toml = render(&answers);

    if ask.interactive {
        ask.say(&format!(
            "\nThis will create {}:\n\n{}\n",
            config_path.display(),
            indent(&toml)
        ));
        if !ask.confirm("Write it?", true)? {
            ask.say("aborted; nothing was written\n");
            return Ok(exit::USAGE);
        }
    }
    write_checked(&config_path, &toml)?;

    let mut created = vec![config_path.display().to_string()];
    let dir = root.join(&answers.migrations_dir);
    if !dir.exists() {
        fs::create_dir_all(&dir)
            .map_err(|e| Error::Usage(format!("cannot create {}: {e}", dir.display())))?;
        created.push(dir.display().to_string());
    }
    if fs::read_dir(&dir)
        .map(|mut d| d.next().is_none())
        .unwrap_or(false)
    {
        let keep = dir.join(".gitkeep");
        fs::write(&keep, "")
            .map_err(|e| Error::Usage(format!("cannot create {}: {e}", keep.display())))?;
        created.push(keep.display().to_string());
    }

    let config = Config::load(&config_path)?;
    let problems = validate_offline(&config)?;

    // Optional, read-only connection test, using this run's settings.
    let connection = args.connect.connection.clone().filter(|c| !c.is_empty());
    let wanted = if args.test_connection {
        true
    } else if ask.interactive && connection.is_some() {
        ask.confirm(
            "MIGRATE_DB_CONNECTION is set. Test the connection now (read-only)?",
            true,
        )?
    } else {
        false
    };
    let test = if wanted {
        Some(test_connection(&config, connection, args.connect.auth.clone()).await)
    } else {
        None
    };

    let mut warnings = found.warnings.clone();
    if args.connect.auth.as_deref() == Some("entra") {
        warnings.push(
            "Entra sign-in for SQL Server isn't implemented yet (PLAN.md Phase 1); use a SQL login for now"
                .into(),
        );
    }
    let next = next_steps(&found);
    let doc = json!({
        "root": root.display().to_string(),
        "provider": answers.provider.as_str(),
        "migrations_dir": answers.migrations_dir,
        "created": created,
        "detected": found.signals,
        "warnings": warnings,
        "validate": { "ok": problems.is_empty(), "errors": problems },
        "connection_test": test.as_ref().map(|r| match r {
            Ok(summary) => json!({ "ok": true, "summary": summary }),
            Err(e) => json!({ "ok": false, "error": e.to_string() }),
        }),
        "next_steps": next,
    });
    out.emit("init", doc, || {
        let mut t = String::new();
        for c in &created {
            t.push_str(&format!("created {c}\n"));
        }
        if problems.is_empty() {
            t.push_str("validate --offline: ok\n");
        } else {
            for p in &problems {
                t.push_str(&format!("validate --offline: problem: {p}\n"));
            }
        }
        match &test {
            Some(Ok(s)) => t.push_str(&format!("connection: ok, {s}\n")),
            Some(Err(e)) => t.push_str(&format!("connection: FAILED: {e}\n")),
            None => {}
        }
        t.push_str("\nnext steps:\n");
        for n in &next {
            t.push_str(&format!("  {n}\n"));
        }
        t
    })?;

    if !problems.is_empty() {
        return Ok(exit::VALIDATION);
    }
    // A failed connection test is a warning interactively, exit 5 otherwise.
    if matches!(test, Some(Err(_))) && !ask.interactive {
        return Ok(exit::CONNECTION);
    }
    Ok(exit::OK)
}

fn questions(ask: &Prompter, args: &InitArgs, found: &Detection) -> Result<Answers, Error> {
    let provider = choose_provider(ask, args, found)?;
    let dir_default = found
        .sql_dirs
        .first()
        .cloned()
        .unwrap_or_else(|| DEFAULT_MIGRATIONS_DIR.to_owned());
    let migrations_dir = answer(
        ask,
        args.dir.clone(),
        "Migrations folder (relative to the project)",
        &dir_default,
    )?;
    let ledger = answer(
        ask,
        args.ledger.clone(),
        "Ledger location (schema.table)",
        default_ledger(provider),
    )?;
    let versioning = answer(
        ask,
        args.versioning.clone(),
        "Version scheme (timestamp | sequential)",
        "timestamp",
    )?;
    if !matches!(versioning.as_str(), "timestamp" | "sequential") {
        return Err(Error::Usage(format!(
            "--versioning must be timestamp or sequential, not `{versioning}`"
        )));
    }
    check_executor(args)?;
    Ok(Answers {
        provider,
        migrations_dir: normalize_dir(&migrations_dir)?,
        ledger: ledger_inline(&ledger)?,
        versioning,
    })
}

fn choose_provider(
    ask: &Prompter,
    args: &InitArgs,
    found: &Detection,
) -> Result<ProviderKind, Error> {
    let suggested = found.provider;
    let text = if let Some(p) = &args.provider {
        p.clone()
    } else if ask.interactive {
        if let Some(p) = suggested {
            ask.say(&format!("Detected: looks like {p}.\n"));
        }
        let default = usize::from(suggested == Some(ProviderKind::Postgres));
        let i = ask.choose(
            "Database technology",
            &["mssql (SQL Server / Azure SQL)", "postgres (PostgreSQL)"],
            default,
        )?;
        AVAILABLE[i].as_str().to_owned()
    } else if let (true, Some(p)) = (args.yes, suggested) {
        p.as_str().to_owned()
    } else {
        return Err(Error::Usage(
            "init needs the database technology in non-interactive mode: pass --provider mssql|postgres \
             (or --yes to accept a detected one)"
                .into(),
        ));
    };
    match text.to_ascii_lowercase().as_str() {
        "mssql" | "sqlserver" | "sql-server" => Ok(ProviderKind::Mssql),
        "postgres" | "postgresql" | "pg" => Ok(ProviderKind::Postgres),
        "mongo" | "mongodb" | "cosmos" => Err(Error::Usage(format!(
            "the `{text}` provider isn't available yet (PLAN.md Phases 3-4); use mssql or postgres"
        ))),
        other => Err(Error::Usage(format!(
            "--provider must be mssql or postgres, not `{other}`"
        ))),
    }
}

/// From the flag, the prompt, or the default.
fn answer(
    ask: &Prompter,
    flag: Option<String>,
    question: &str,
    default: &str,
) -> Result<String, Error> {
    match flag {
        Some(v) => Ok(v),
        None if ask.interactive => ask.ask(question, Some(default)),
        None => Ok(default.to_owned()),
    }
}

fn check_executor(args: &InitArgs) -> Result<(), Error> {
    match args.executor.as_deref() {
        None | Some("native") => Ok(()),
        Some("sqlcmd") => Err(Error::Usage(
            "the sqlcmd executor isn't available yet (PLAN.md Phase 3); use native".into(),
        )),
        Some(other) => Err(Error::Usage(format!(
            "--executor must be native or sqlcmd, not `{other}`"
        ))),
    }
}

// ---------------------------------------------------------------------------
// TOML generation

fn default_ledger(p: ProviderKind) -> &'static str {
    match p {
        ProviderKind::Postgres => "public.__migrations",
        _ => "dbo.__migrations",
    }
}

fn ledger_inline(ledger: &str) -> Result<String, Error> {
    let (schema, table) = ledger
        .split_once('.')
        .ok_or_else(|| Error::Usage(format!("--ledger `{ledger}`: expected <schema>.<table>")))?;
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !s.starts_with(|c: char| c.is_ascii_digit())
    };
    if !ok(schema) || !ok(table) {
        return Err(Error::Usage(format!(
            "--ledger `{ledger}`: schema and table must be letters, digits and _"
        )));
    }
    Ok(format!(
        "{{ schema = {}, table = {} }}",
        quote(schema),
        quote(table)
    ))
}

fn render(a: &Answers) -> String {
    let mut t = format!(
        "# migrate-db configuration for one database (SPEC §5), created by `migrate-db init`.\n\
         # It is environment-agnostic: each run gets its connection from MIGRATE_DB_CONNECTION\n\
         # and its auth mode from MIGRATE_DB_AUTH (sql | entra). No secrets live here.\n\n\
         # Database technology.\n\
         provider = {provider}\n\
         # Folder with V<version>__<description>.up.sql / .down.sql files, relative to this file.\n\
         migrations_dir = {dir}\n\
         # Version scheme for `migrate-db new`: timestamp (UTC YYYYMMDDHHMMSS) or sequential.\n\
         versioning = {versioning}\n\
         # Where the ledger tables live. They are created by the first `up`.\n\
         ledger = {ledger}\n",
        provider = quote(a.provider.as_str()),
        dir = quote(&a.migrations_dir),
        versioning = quote(&a.versioning),
        ledger = a.ledger,
    );
    if a.provider == ProviderKind::Mssql {
        t.push_str(
            "# How scripts run: native = the built-in driver (sqlcmd comes in a later release).\n\
             executor = \"native\"\n",
        );
    }
    t
}

/// A TOML string with correct escaping.
fn quote(s: &str) -> String {
    toml::Value::String(s.to_owned()).to_string()
}

fn indent(s: &str) -> String {
    s.lines().map(|l| format!("    {l}\n")).collect()
}

/// Relative, forward slashes, inside the project.
fn normalize_dir(dir: &str) -> Result<String, Error> {
    let d = dir.trim().replace('\\', "/");
    let d = d.trim_end_matches('/').to_owned();
    if d.is_empty()
        || d.starts_with('/')
        || Path::new(&d).is_absolute()
        || d.contains(':')
        || d.split('/').any(|p| p == "..")
    {
        return Err(Error::Usage(format!(
            "--dir `{dir}` must be a relative folder inside the project"
        )));
    }
    Ok(d)
}

/// Write `content`, then load it; remove the file if it doesn't parse.
fn write_checked(path: &Path, content: &str) -> Result<(), Error> {
    fs::write(path, content)
        .map_err(|e| Error::Usage(format!("cannot write {}: {e}", path.display())))?;
    if let Err(e) = Config::load(path) {
        let _ = fs::remove_file(path);
        return Err(Error::Usage(format!(
            "internal: the generated config doesn't load ({e}); nothing was written"
        )));
    }
    Ok(())
}

fn project_root(path: Option<&Path>) -> Result<PathBuf, Error> {
    let root = match path {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir()
            .map_err(|e| Error::Usage(format!("cannot read the current directory: {e}")))?,
    };
    if !root.is_dir() {
        return Err(Error::Usage(format!(
            "project folder `{}` doesn't exist",
            root.display()
        )));
    }
    Ok(canonical(&root))
}

/// Canonical path without the `\\?\` prefix Windows adds.
fn canonical(p: &Path) -> PathBuf {
    let c = fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let s = c.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => c,
    }
}

// ---------------------------------------------------------------------------
// After writing

fn discovery_options(config: &Config) -> DiscoveryOptions {
    let t = config.target();
    DiscoveryOptions {
        extensions: providers::extensions(t.provider)
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
        strict_filenames: t.strict_filenames,
    }
}

fn validate_offline(config: &Config) -> Result<Vec<String>, Error> {
    let t = config.target();
    let set = match discover(&t.migrations_dir, &discovery_options(config)) {
        Ok(set) => set,
        Err(migrate_db_core::source::SourceErrors(errors)) => {
            return Ok(errors.iter().map(ToString::to_string).collect());
        }
    };
    let scripts = set
        .versioned
        .iter()
        .flat_map(|m| std::iter::once(&m.up).chain(m.down.as_ref()))
        .chain(set.repeatables.iter().map(|r| &r.script));
    Ok(scripts
        .filter_map(|s| providers::prepare(t.provider, s).err())
        .collect())
}

/// Read-only (SPEC §7 rule 8): connect, read the ledger and the lock.
async fn test_connection(
    config: &Config,
    connection: Option<String>,
    auth: Option<String>,
) -> Result<String, Error> {
    let resolved = config.resolve(connection.map(Secret::new), auth);
    let set = discover(&resolved.target.migrations_dir, &discovery_options(config))?;
    let mut provider = providers::connect(&resolved).await?;
    let mut engine = Engine::new(provider.as_mut(), &set, Policy::default(), identity());
    let status = engine.status().await?;
    let lock = engine.lock_status().await?;
    let version = status
        .database_version
        .as_ref()
        .map_or_else(|| "none".to_owned(), ToString::to_string);
    let applied = status
        .entries
        .iter()
        .filter(|e| e.applied_at.is_some())
        .count();
    Ok(format!(
        "database version {version}, {applied} applied, {} pending, lock {}",
        status.pending(),
        if lock.held { "held" } else { "free" }
    ))
}

fn next_steps(found: &Detection) -> Vec<String> {
    let mut n = vec![
        "set MIGRATE_DB_CONNECTION to the connection string (and MIGRATE_DB_AUTH=entra for Entra ID)"
            .to_owned(),
        "migrate-db new \"<description>\"   create your first migration".to_owned(),
        "migrate-db status                 check the connection and see what's pending".to_owned(),
        "migrate-db up                     apply pending migrations".to_owned(),
    ];
    if found.existing_tooling {
        n.push(
            "existing tooling was detected: adopt the database with the baseline process \
             (docs/ADOPTION.md) before the first `up`"
                .into(),
        );
    }
    n
}

// ---------------------------------------------------------------------------
// Detection (read-only)

#[derive(Debug, Default)]
struct Detection {
    /// Suggested provider, if the signals agree.
    provider: Option<ProviderKind>,
    /// Folders (relative, `/`) holding loose `*.sql` files.
    sql_dirs: Vec<String>,
    /// Another mechanism seems to deploy this schema.
    existing_tooling: bool,
    /// Human-readable findings.
    signals: Vec<String>,
    warnings: Vec<String>,
}

const SKIP: [&str; 6] = [".git", "node_modules", "bin", "obj", "target", ".vs"];
const STARTUP_CALLS: [&str; 4] = [
    "Database.Migrate(",
    "MigrateAsync(",
    "PerformUpgrade(",
    "CreateContainerIfNotExistsAsync(",
];

fn detect(root: &Path) -> Detection {
    let mut d = Detection::default();
    let mut votes: Vec<ProviderKind> = Vec::new();
    let mut bad_names = 0usize;
    let mut sql_files = 0usize;
    let mut startup_hits: Vec<String> = Vec::new();
    walk(root, root, 0, &mut |rel, path, depth| {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let lower = name.to_ascii_lowercase();
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if depth <= 3 {
            if ext == "sql" {
                sql_files += 1;
                let dir = Path::new(rel)
                    .parent()
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                if !dir.is_empty() && !d.sql_dirs.contains(&dir) {
                    d.sql_dirs.push(dir);
                }
                if !migration_like(&name) {
                    bad_names += 1;
                }
            }
            if ext == "sqlproj" {
                d.existing_tooling = true;
                votes.push(ProviderKind::Mssql);
                d.signals
                    .push(format!("SQL Database Project (DacFx): {rel}"));
            }
            if lower == "flyway.conf" || lower == "flyway.toml" {
                d.signals.push(format!("Flyway config: {rel}"));
                d.existing_tooling = true;
                let text = read_small(path);
                if text.contains("jdbc:sqlserver") {
                    votes.push(ProviderKind::Mssql);
                } else if text.contains("jdbc:postgresql") {
                    votes.push(ProviderKind::Postgres);
                }
                d.warnings.push(
                    "Flyway V<n>__name.sql files need renaming to .up.sql (and down files)".into(),
                );
            }
            if lower == "migrate-mongo-config.js" {
                d.existing_tooling = true;
                d.signals.push(format!("migrate-mongo config: {rel}"));
                d.warnings.push(
                    "migrate-mongo (Node-style) migrations aren't supported in v1; the Mongo provider comes in Phase 3"
                        .into(),
                );
            }
            if lower == "database.json" {
                let text = read_small(path);
                if text.contains("\"driver\"") {
                    d.existing_tooling = true;
                    d.signals.push(format!("db-migrate config: {rel}"));
                    if text.contains("\"mssql\"") {
                        votes.push(ProviderKind::Mssql);
                    } else if text.contains("\"pg\"") {
                        votes.push(ProviderKind::Postgres);
                    }
                }
            }
            if ext == "csproj" {
                let text = read_small(path);
                if text.contains("dbup-sqlserver") {
                    d.existing_tooling = true;
                    votes.push(ProviderKind::Mssql);
                    d.signals.push(format!("DbUp (SQL Server): {rel}"));
                } else if text.contains("dbup-postgresql") {
                    d.existing_tooling = true;
                    votes.push(ProviderKind::Postgres);
                    d.signals.push(format!("DbUp (PostgreSQL): {rel}"));
                }
                if text.contains("Microsoft.EntityFrameworkCore.SqlServer") {
                    votes.push(ProviderKind::Mssql);
                } else if text.contains("Npgsql.EntityFrameworkCore.PostgreSQL") {
                    votes.push(ProviderKind::Postgres);
                }
            }
            if name.ends_with("ModelSnapshot.cs") {
                d.existing_tooling = true;
                d.signals.push(format!("EF Core migrations: {rel}"));
            }
        }
        if depth <= 5 && (ext == "cs" || ext == "js") {
            let text = read_small(path);
            for call in STARTUP_CALLS {
                if text.contains(call) {
                    d.existing_tooling = true;
                    startup_hits.push(format!("{call}… in {rel}"));
                }
            }
        }
    });
    if sql_files > 0 {
        let wher = if d.sql_dirs.is_empty() {
            "the project folder".to_owned()
        } else {
            d.sql_dirs.join(", ")
        };
        d.signals
            .push(format!("{sql_files} .sql file(s) in {wher}"));
        if bad_names > 0 {
            d.warnings.push(format!(
                "{bad_names} .sql file(s) don't match V<version>__<description>.up.sql / .down.sql / R__<name>.sql"
            ));
        }
    }
    d.signals.extend(startup_hits.into_iter().take(10));
    if d.existing_tooling {
        d.warnings.push(
            "another mechanism already deploys this schema: adopt the database with the baseline \
             process (capture → compare → mark, docs/ADOPTION.md) and switch that mechanism off at \
             cutover. Nothing has been modified."
                .into(),
        );
    }
    votes.dedup();
    if !votes.is_empty() && votes.windows(2).all(|w| w[0] == w[1]) {
        d.provider = votes.first().copied();
    }
    d
}

fn migration_like(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("r__") {
        return true;
    }
    let Some(rest) = lower.strip_prefix('v') else {
        return false;
    };
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    digits > 0
        && rest[digits..].starts_with("__")
        && (lower.ends_with(".up.sql") || lower.ends_with(".down.sql"))
}

fn walk(root: &Path, dir: &Path, depth: usize, f: &mut dyn FnMut(&str, &Path, usize)) {
    if depth > 5 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| name.clone());
        match entry.file_type() {
            Ok(t) if t.is_dir() => {
                if !SKIP.contains(&name.as_str()) {
                    walk(root, &path, depth + 1, f);
                }
            }
            Ok(t) if t.is_file() => f(&rel, &path, depth + 1),
            _ => {}
        }
    }
}

/// File text, or empty if it's large or unreadable.
fn read_small(path: &Path) -> String {
    match fs::metadata(path) {
        Ok(m) if m.len() <= 1_000_000 => fs::read_to_string(path).unwrap_or_default(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Prompts (stderr, so stdout stays clean)

struct Prompter {
    interactive: bool,
}

impl Prompter {
    fn new(interactive: bool) -> Self {
        Self { interactive }
    }

    fn say(&self, text: &str) {
        eprint!("{text}");
        let _ = std::io::stderr().flush();
    }

    fn read_line(&self) -> Result<String, Error> {
        let mut line = String::new();
        let n = std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|e| Error::Usage(format!("cannot read input: {e}")))?;
        if n == 0 {
            return Err(Error::Usage("input closed; nothing was written".into()));
        }
        Ok(line.trim().to_owned())
    }

    fn ask(&self, question: &str, default: Option<&str>) -> Result<String, Error> {
        loop {
            match default {
                Some(d) => self.say(&format!("{question} [{d}]: ")),
                None => self.say(&format!("{question}: ")),
            }
            let line = self.read_line()?;
            if !line.is_empty() {
                return Ok(line);
            }
            if let Some(d) = default {
                return Ok(d.to_owned());
            }
        }
    }

    fn confirm(&self, question: &str, default: bool) -> Result<bool, Error> {
        loop {
            self.say(&format!(
                "{question} [{}]: ",
                if default { "Y/n" } else { "y/N" }
            ));
            match self.read_line()?.to_ascii_lowercase().as_str() {
                "" => return Ok(default),
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => self.say("  please answer y or n\n"),
            }
        }
    }

    fn choose(&self, question: &str, options: &[&str], default: usize) -> Result<usize, Error> {
        self.say(&format!("{question}:\n"));
        for (i, o) in options.iter().enumerate() {
            self.say(&format!("  {}) {o}\n", i + 1));
        }
        loop {
            self.say(&format!("Choose 1-{} [{}]: ", options.len(), default + 1));
            let line = self.read_line()?;
            if line.is_empty() {
                return Ok(default);
            }
            match line.parse::<usize>() {
                Ok(n) if (1..=options.len()).contains(&n) => return Ok(n - 1),
                _ => self.say("  please enter one of the numbers\n"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(quote("x"), "\"x\"");
        for s in [r"a\b", "it's \"quoted\"", "x\ny"] {
            let doc: toml::Table = toml::from_str(&format!("k = {}", quote(s))).unwrap();
            assert_eq!(doc["k"].as_str(), Some(s), "round trip {s:?}");
        }
        assert_eq!(normalize_dir("db\\migrations\\").unwrap(), "db/migrations");
        for bad in ["", "../x", "C:/x", "/abs"] {
            assert!(normalize_dir(bad).is_err(), "{bad}");
        }
        assert!(migration_like("V20261118__create_orders.up.sql"));
        assert!(migration_like("R__dbo.view.sql"));
        assert!(!migration_like("create_orders.sql"));
        assert!(ledger_inline("dbo").is_err());
        assert!(ledger_inline("dbo.x;drop").is_err());
    }

    #[test]
    fn rendered_config_loads() {
        let toml = render(&Answers {
            provider: ProviderKind::Mssql,
            migrations_dir: "migrations".into(),
            ledger: ledger_inline("dbo.__migrations").unwrap(),
            versioning: "timestamp".into(),
        });
        let c = Config::parse(Path::new("/p/migrate-db.toml"), &toml).unwrap();
        assert_eq!(c.target().provider, ProviderKind::Mssql);
        assert_eq!(
            c.target().migrations_dir,
            Path::new("/p").join("migrations")
        );
        assert!(!toml.contains("[environments"), "{toml}");
    }
}
