//! `migrate-db validate`.

use serde_json::json;

use migrate_db_core::engine::Engine;
use migrate_db_core::error::{Error, exit};
use migrate_db_core::placeholders;
use migrate_db_core::planner::{MigrationState, Policy};

use crate::cli::{ConnectArgs, GlobalArgs};
use crate::context::{Context, identity};
use crate::output::Output;
use crate::providers;

/// Run `validate`.
pub async fn run(
    global: &GlobalArgs,
    out: &Output,
    offline: bool,
    connect: &ConnectArgs,
) -> Result<i32, Error> {
    let ctx = Context::load(global, Some(connect))?;
    let target = ctx.target();

    let set = match ctx.discover(&target) {
        Ok(set) => set,
        Err(Error::Source(errors)) => {
            let messages: Vec<String> = errors.0.iter().map(ToString::to_string).collect();
            out.emit(
                "validate",
                json!({ "offline": offline, "ok": false, "errors": messages, "warnings": [] }),
                || {
                    let mut t = format!(
                        "{} problem(s) in {}:\n",
                        messages.len(),
                        target.migrations_dir.display()
                    );
                    for m in &messages {
                        t.push_str(&format!("  - {m}\n"));
                    }
                    t
                },
            )?;
            return Ok(exit::VALIDATION);
        }
        Err(e) => return Err(e),
    };
    let mut warnings: Vec<String> = set.warnings.iter().map(ToString::to_string).collect();

    // Provider-specific offline checks (e.g. GO lines), for every file, so
    // a bad script is caught in review rather than mid-run.
    let mut problems: Vec<String> = Vec::new();
    let scripts = set
        .versioned
        .iter()
        .flat_map(|m| std::iter::once(&m.up).chain(m.down.as_ref()))
        .chain(set.repeatables.iter().map(|r| &r.script));
    // Placeholders (SPEC §3.3): offline only the syntax can be checked, since
    // a PR build may not have a stage's values; online, every value must be set.
    let values = ctx.placeholders();
    for script in scripts {
        if let Err(p) = providers::prepare(target.provider, script) {
            problems.push(p);
        }
        let Some(values) = &values else { continue };
        let checked = if offline {
            placeholders::references(&script.content).map(|_| ())
        } else {
            placeholders::render(&script.content, values).map(|_| ())
        };
        if let Err(e) = checked {
            problems.push(format!("{}: {e}", script.relative_path));
        }
    }
    for r in &set.repeatables {
        warnings.extend(providers::lint_repeatable(target.provider, &r.script));
    }
    if !offline {
        let resolved = ctx.resolve(connect);
        let mut provider = providers::connect(&resolved).await?;
        let report = Engine::new(provider.as_mut(), &set, Policy::default(), identity())
            .status()
            .await?;
        for e in &report.entries {
            let problem = match e.state {
                MigrationState::ChecksumMismatch => {
                    Some("checksum mismatch (file changed after it was applied)")
                }
                MigrationState::Failed => Some("failed in the ledger"),
                MigrationState::InProgress => Some("in progress in the ledger"),
                MigrationState::OutOfOrder => Some("pending but older than the database version"),
                _ => None,
            };
            if let Some(p) = problem {
                problems.push(format!("{} {}: {p}", e.version, e.description));
            }
        }
    }

    let ok = problems.is_empty();
    out.emit(
        "validate",
        json!({
            "offline": offline,
            "ok": ok,
            "migrations": set.versioned.len(),
            "repeatables": set.repeatables.len(),
            "errors": problems,
            "warnings": warnings,
        }),
        || {
            let mut t = String::new();
            for w in &warnings {
                t.push_str(&format!("warning: {w}\n"));
            }
            for p in &problems {
                t.push_str(&format!("problem: {p}\n"));
            }
            t.push_str(&format!(
                "{}: {} migration(s), {} repeatable(s) in {}\n",
                if ok { "ok" } else { "FAILED" },
                set.versioned.len(),
                set.repeatables.len(),
                target.migrations_dir.display()
            ));
            t
        },
    )?;
    Ok(if ok { exit::OK } else { exit::VALIDATION })
}
