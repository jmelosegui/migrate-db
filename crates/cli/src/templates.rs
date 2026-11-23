//! Templates for `migrate-db new`.

use migrate_db_core::config::ProviderKind;
use migrate_db_core::model::Direction;

/// Body of a new migration file.
pub fn body(
    kind: ProviderKind,
    extension: &str,
    direction: Direction,
    version: &str,
    description: &str,
) -> String {
    match (kind, extension, direction) {
        (ProviderKind::Postgres | ProviderKind::Mssql, _, Direction::Up) => format!(
            "-- V{version}: {description}\n\
             -- Runs inside a transaction together with its ledger row.\n\
             -- Add `-- migrate:no-transaction` on its own line at the top for statements that\n\
             -- can't run in a transaction (it must then contain exactly one statement on Postgres).\n\n"
        ),
        (ProviderKind::Postgres | ProviderKind::Mssql, _, Direction::Down) => format!(
            "-- Revert V{version}: {description}\n\
             -- Undo exactly what the up file does, in reverse order.\n\n"
        ),
        (ProviderKind::Mongo, "js", _) => format!(
            "// V{version} ({direction}): {description}\n\
             // Runs with mongosh; `db` is connected to the target database.\n\
             // Write idempotent code: a failed run is retried from the start.\n\n"
        ),
        (ProviderKind::Mongo, _, _) => format!(
            "# V{version} ({direction}): {description}\n\
             # Each command is a MongoDB runCommand document, executed in order.\n\
             # recovery: resume re-runs the failed command, so keep commands idempotent.\n\
             recovery: resume\n\
             transaction: false\n\
             commands: []\n"
        ),
        (ProviderKind::Cosmos, _, _) => format!(
            "# V{version} ({direction}): {description}\n\
             # Operations run in order (no transactions).\n\
             # recovery: resume re-runs the failed operation, so keep operations idempotent.\n\
             recovery: resume\n\
             operations: []\n"
        ),
    }
}

/// Body of a new repeatable (`R__<name>`) file (SPEC §3.6).
pub fn repeatable_body(kind: ProviderKind, name: &str) -> String {
    let (create, example) = match kind {
        ProviderKind::Postgres => (
            "CREATE OR REPLACE",
            format!("-- CREATE OR REPLACE VIEW {name} AS\n-- SELECT ...;\n"),
        ),
        _ => (
            "CREATE OR ALTER",
            format!("-- CREATE OR ALTER PROCEDURE {name}\n-- AS\n-- SELECT ...;\n"),
        ),
    };
    format!(
        "-- Repeatable: {name}\n\
         -- `up` re-applies this file whenever it changes, after the versioned migrations,\n\
         -- in alphabetical order of the R__ names. It has no down: to undo, edit or revert it.\n\
         -- Write it so it can run again: {create} for procedures, views, functions and\n\
         -- triggers; MERGE / upserts for reference data. Keep schema-bound or indexed views\n\
         -- in versioned migrations.\n\n\
         {example}"
    )
}

/// Extension for new files of this provider.
pub fn extension(kind: ProviderKind, new_format: Option<&str>) -> &'static str {
    match kind {
        ProviderKind::Postgres | ProviderKind::Mssql => "sql",
        ProviderKind::Mongo => match new_format {
            Some("yaml") => "yaml",
            _ => "js",
        },
        ProviderKind::Cosmos => "yaml",
    }
}
