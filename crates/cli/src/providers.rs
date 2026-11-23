//! Provider registry: which providers this build contains.

use migrate_db_core::config::{ProviderKind, ResolvedTarget};
use migrate_db_core::error::Error;
use migrate_db_core::provider::Provider;
use migrate_db_core::source::ScriptFile;

/// File extensions a provider executes (known even without connecting).
pub fn extensions(kind: ProviderKind) -> &'static [&'static str] {
    match kind {
        ProviderKind::Postgres | ProviderKind::Mssql => &["sql"],
        ProviderKind::Mongo => &["js", "yaml", "yml", "json"],
        ProviderKind::Cosmos => &["yaml", "yml", "json"],
    }
}

/// The provider's offline checks for one script (`validate --offline`).
/// Providers not compiled into this build have none.
pub fn prepare(kind: ProviderKind, script: &ScriptFile) -> Result<(), String> {
    match kind {
        #[cfg(feature = "mssql")]
        ProviderKind::Mssql => migrate_db_mssql::prepare(script),
        _ => {
            let _ = script;
            Ok(())
        }
    }
}

/// The provider's warnings for a repeatable (`R__`) script (SPEC §3.6).
pub fn lint_repeatable(kind: ProviderKind, script: &ScriptFile) -> Vec<String> {
    match kind {
        #[cfg(feature = "mssql")]
        ProviderKind::Mssql => migrate_db_mssql::lint_repeatable(script),
        #[cfg(feature = "postgres")]
        ProviderKind::Postgres => migrate_db_postgres::lint_repeatable(script),
        _ => {
            let _ = script;
            Vec::new()
        }
    }
}

/// `application_name` / lock-holder label for this run.
#[cfg_attr(not(any(feature = "postgres", feature = "mssql")), allow(dead_code))]
fn application_name() -> String {
    let mut name = format!("migrate-db/{}", env!("CARGO_PKG_VERSION"));
    if let Some(run) = crate::context::ci_run() {
        name.push(' ');
        name.push_str(&run);
    }
    name
}

/// Connect to the database.
pub async fn connect(target: &ResolvedTarget) -> Result<Box<dyn Provider>, Error> {
    let kind = target.target.provider;
    let connection = || {
        target.connection.as_ref().ok_or_else(|| {
            Error::Usage(
                "no connection for this run: set MIGRATE_DB_CONNECTION (or pass --connection, \
                 or put it in an --env-file)"
                    .into(),
            )
        })
    };
    match kind {
        #[cfg(feature = "postgres")]
        ProviderKind::Postgres => {
            let provider = migrate_db_postgres::PostgresProvider::connect(
                connection()?.expose(),
                &target.target.ledger,
                &application_name(),
            )
            .await?;
            Ok(Box::new(provider))
        }
        #[cfg(feature = "mssql")]
        ProviderKind::Mssql => {
            let provider = migrate_db_mssql::MssqlProvider::connect(
                connection()?.expose(),
                &target.target.ledger,
                &application_name(),
                target.auth.as_deref(),
            )
            .await?;
            Ok(Box::new(provider))
        }
        _ => {
            let _ = connection;
            Err(Error::Usage(format!(
                "the `{kind}` provider is not available in this build yet"
            )))
        }
    }
}
