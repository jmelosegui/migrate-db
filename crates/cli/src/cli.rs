//! Command-line definitions.
//!
//! Only options every command uses are global. Connection options belong to
//! the commands that connect, run options to the commands that execute
//! migrations, so `--help` shows each command only what it can use.

use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

/// Database migrations for SQL Server, PostgreSQL, MongoDB and Cosmos DB.
#[derive(Debug, Parser)]
#[command(name = "migrate-db", version, about, propagate_version = true)]
pub struct Cli {
    /// Options shared by every command.
    #[command(flatten)]
    pub global: GlobalArgs,
    /// The command to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Human-readable text.
    Text,
    /// One JSON document on stdout.
    Json,
}

/// Options every command uses.
#[derive(Debug, Args)]
pub struct GlobalArgs {
    /// Config file (default: discover migrate-db.toml up to the repo root).
    #[arg(short = 'c', long, global = true, env = "MIGRATE_DB_CONFIG")]
    pub config: Option<PathBuf>,
    /// Output format.
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Text)]
    pub output: OutputFormat,
    /// Also write the JSON result to this file.
    #[arg(long, global = true)]
    pub output_file: Option<PathBuf>,
    /// Force CI behaviour locally: no prompts, `--yes` required for destructive commands.
    #[arg(
        long,
        global = true,
        env = "MIGRATE_DB_CI",
        action = ArgAction::SetTrue,
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    pub ci: bool,
    /// More output.
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub verbose: u8,
    /// Don't check GitHub for a newer release after the command.
    #[arg(
        long,
        global = true,
        env = "MIGRATE_DB_NO_UPDATE_CHECK",
        action = ArgAction::SetTrue,
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    pub no_update_check: bool,
}

/// How to reach the database (commands that connect).
#[derive(Debug, Args)]
pub struct ConnectArgs {
    /// Connection string for this run. Prefer the environment variable, so
    /// the secret stays out of the command line.
    #[arg(long, env = "MIGRATE_DB_CONNECTION", hide_env_values = true)]
    pub connection: Option<String>,
    /// Authentication: sql (login in the connection string) or entra (Entra ID).
    #[arg(long, env = "MIGRATE_DB_AUTH")]
    pub auth: Option<String>,
    /// Read MIGRATE_DB_* settings from this KEY=VALUE file (process env wins).
    #[arg(long, value_name = "FILE")]
    pub env_file: Option<PathBuf>,
}

/// Options shared by commands that execute migrations.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Show the plan without executing anything (no lock, no writes).
    #[arg(long)]
    pub dry_run: bool,
    /// Refuse to run unless the plan matches this approved plan file.
    #[arg(long, value_name = "FILE")]
    pub expect_plan: Option<PathBuf>,
    /// Seconds to wait for the migration lock.
    #[arg(long, default_value_t = 60)]
    pub lock_timeout: u64,
    /// How to reach the database.
    #[command(flatten)]
    pub connect: ConnectArgs,
}

/// `--allow-down` for commands that may revert.
#[derive(Debug, Args)]
pub struct AllowDown {
    /// Allow reverting migrations in this run. Off by default; set it only
    /// where reverting is safe.
    #[arg(
        id = "allow_down",
        long = "allow-down",
        env = "MIGRATE_DB_ALLOW_DOWN",
        action = ArgAction::SetTrue,
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    pub allowed: bool,
}

/// `--allow-out-of-order` for commands that may apply.
#[derive(Debug, Args)]
pub struct AllowOutOfOrder {
    /// Allow pending migrations older than the database version.
    #[arg(
        id = "allow_out_of_order",
        long = "allow-out-of-order",
        env = "MIGRATE_DB_ALLOW_OUT_OF_ORDER",
        action = ArgAction::SetTrue,
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    pub allowed: bool,
}

/// `init` options (SPEC §5.2).
#[derive(Debug, Args)]
pub struct InitArgs {
    /// Project folder to set up (default: the current folder). One folder holds one database.
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,
    /// Database technology: mssql or postgres.
    #[arg(long)]
    pub provider: Option<String>,
    /// Migrations folder, relative to the project (default: migrations).
    #[arg(long, value_name = "PATH")]
    pub dir: Option<String>,
    /// Ledger location as <schema>.<table> (default: dbo.__migrations / public.__migrations).
    #[arg(long, value_name = "SCHEMA.TABLE")]
    pub ledger: Option<String>,
    /// Version scheme for new migrations: timestamp or sequential.
    #[arg(long)]
    pub versioning: Option<String>,
    /// SQL Server executor: native (sqlcmd comes in a later release).
    #[arg(long)]
    pub executor: Option<String>,
    /// After writing, test the connection (read-only).
    #[arg(long)]
    pub test_connection: bool,
    /// Accept the detected provider in non-interactive mode.
    #[arg(long)]
    pub yes: bool,
    /// The connection for --test-connection.
    #[command(flatten)]
    pub connect: ConnectArgs,
}

/// Commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Set up migrate-db in a project folder (guided; every question has a flag).
    Init(InitArgs),
    /// Create a migration in the migrations folder (offline: never connects).
    New {
        /// What the migration does, e.g. "create orders table". With
        /// --repeatable: the object it defines, e.g. "dbo.get_orders".
        description: String,
        /// Create only the up file (no down).
        #[arg(long, conflicts_with = "repeatable")]
        irreversible: bool,
        /// Create a repeatable migration (R__<name>.sql): re-applied by `up`
        /// whenever it changes; no version and no down file (SPEC §3.6).
        #[arg(long)]
        repeatable: bool,
        /// Subfolder of the migrations folder to create the file(s) in, e.g. "orders" or "procs/reporting".
        #[arg(long, value_name = "PATH")]
        folder: Option<String>,
    },
    /// Show the database version, every migration's state and the lock.
    #[command(alias = "info")]
    Status {
        /// How to reach the database.
        #[command(flatten)]
        connect: ConnectArgs,
    },
    /// Check migration files (and, online, compare them with the ledger).
    Validate {
        /// Don't connect to the database.
        #[arg(long)]
        offline: bool,
        /// How to reach the database (not needed with --offline).
        #[command(flatten)]
        connect: ConnectArgs,
    },
    /// Apply pending migrations.
    Up {
        /// Apply up to and including this version.
        #[arg(long)]
        to: Option<String>,
        /// Apply at most this many migrations.
        #[arg(long)]
        steps: Option<usize>,
        /// `--allow-out-of-order`.
        #[command(flatten)]
        out_of_order: AllowOutOfOrder,
        /// Shared run options.
        #[command(flatten)]
        run: RunArgs,
    },
    /// Revert applied migrations (default: the last one).
    Down {
        /// Revert everything above this version (0 = everything).
        #[arg(long)]
        to: Option<String>,
        /// Revert this many migrations.
        #[arg(long)]
        steps: Option<usize>,
        /// Don't ask for confirmation (required in CI).
        #[arg(long)]
        yes: bool,
        /// `--allow-down`.
        #[command(flatten)]
        allow_down: AllowDown,
        /// Shared run options.
        #[command(flatten)]
        run: RunArgs,
    },
    /// Clear a stale lock-holder record, or (with --kill-holder) end the
    /// session holding the lock. Shows the holder and asks first.
    Unlock {
        /// Required: unlocking bypasses the protection the lock gives.
        #[arg(long, required = true)]
        force: bool,
        /// Also terminate the database session holding the lock (its
        /// current step rolls back). Needs an elevated principal.
        #[arg(long)]
        kill_holder: bool,
        /// Don't ask for confirmation (required in CI).
        #[arg(long)]
        yes: bool,
        /// How to reach the database.
        #[command(flatten)]
        connect: ConnectArgs,
    },
    /// Adopt a database that existed before migrate-db (ADOPTION.md).
    #[command(subcommand)]
    Baseline(BaselineCommand),
    /// Move the database to exactly this version (0 = nothing applied).
    Goto {
        /// Target version.
        #[arg(value_name = "VERSION")]
        target_version: String,
        /// `--allow-out-of-order`.
        #[command(flatten)]
        out_of_order: AllowOutOfOrder,
        /// Don't ask for confirmation when the plan reverts migrations.
        #[arg(long)]
        yes: bool,
        /// `--allow-down` (needed when the plan reverts migrations).
        #[command(flatten)]
        allow_down: AllowDown,
        /// Shared run options.
        #[command(flatten)]
        run: RunArgs,
    },
}

impl Command {
    /// Name used in JSON output.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Init(_) => "init",
            Self::New { .. } => "new",
            Self::Status { .. } => "status",
            Self::Validate { .. } => "validate",
            Self::Up { .. } => "up",
            Self::Down { .. } => "down",
            Self::Goto { .. } => "goto",
            Self::Unlock { .. } => "unlock",
            Self::Baseline(BaselineCommand::Mark { .. }) => "baseline mark",
        }
    }
}

/// `baseline` subcommands (SPEC §4.1).
#[derive(Debug, Subcommand)]
pub enum BaselineCommand {
    /// Record a baseline in an empty ledger: migrations up to VERSION count
    /// as applied. Executes no scripts; it only writes ledger rows.
    // `--version` is the baseline version here (SPEC §4.1), not the tool's.
    #[command(disable_version_flag = true)]
    Mark {
        /// The baseline version: an existing V<version>__... migration file.
        #[arg(long)]
        version: String,
        /// Description for the marker row (default: the file's).
        #[arg(long)]
        description: Option<String>,
        /// Also record every repeatable (R__) at its current checksum.
        #[arg(long)]
        mark_repeatables: bool,
        /// Mark without checking the live schema against a captured
        /// fingerprint. Required until `baseline capture` / `compare` exist;
        /// check the schema yourself first.
        #[arg(long)]
        force_unverified: bool,
        /// Don't ask for confirmation (required in CI).
        #[arg(long)]
        yes: bool,
        /// Shared run options.
        #[command(flatten)]
        run: RunArgs,
    },
}
