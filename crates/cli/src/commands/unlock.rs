//! `unlock --force [--kill-holder]` (SPEC §4.1, §7 rule 10).

use serde_json::json;

use migrate_db_core::engine::Engine;
use migrate_db_core::error::{Error, exit};
use migrate_db_core::planner::Policy;
use migrate_db_core::provider::{LockStatus, UnlockOutcome};

use crate::cli::{ConnectArgs, GlobalArgs};
use crate::commands::migrate::{confirm, lock_json, lock_text};
use crate::context::{Context, identity, is_interactive};
use crate::output::{Output, to_value};
use crate::providers;

/// Run `unlock --force`.
pub async fn run(
    global: &GlobalArgs,
    out: &Output,
    kill_holder: bool,
    yes: bool,
    connect: &ConnectArgs,
) -> Result<i32, Error> {
    let ctx = Context::load(global, Some(connect))?;
    let resolved = ctx.resolve(connect);
    let set = ctx.discover(&resolved.target)?;
    let mut provider = providers::connect(&resolved).await?;
    let mut engine = Engine::new(provider.as_mut(), &set, Policy::default(), identity());
    let now = chrono::Utc::now();
    let before = engine.lock_status().await?;

    // Ask only when there is something to do; the provider re-checks the
    // lock itself and refuses a held lock without --kill-holder.
    if let Some(question) = question(&before, kill_holder)
        && !yes
    {
        if !is_interactive(global) {
            return Err(Error::Usage(
                "unlock --force needs --yes in non-interactive mode".into(),
            ));
        }
        eprint!("{}", lock_text(&before, now));
        if !confirm(&question) {
            eprintln!("aborted; nothing was changed");
            return Ok(exit::USAGE);
        }
    }

    let outcome = engine.force_unlock(kill_holder).await?;
    out.emit(
        "unlock",
        json!({
            "provider": resolved.target.provider.as_str(),
            "lock_before": lock_json(&before, now),
            "result": to_value(&outcome),
        }),
        || outcome_text(&outcome),
    )?;
    Ok(exit::OK)
}

/// The confirmation question, or `None` when nothing will change (the lock
/// is free with no record, or held without --kill-holder, which fails).
fn question(lock: &LockStatus, kill_holder: bool) -> Option<String> {
    match (lock.held, &lock.holder) {
        (false, Some(_)) => Some("Remove this stale lock-holder record?".into()),
        (true, Some(h)) if kill_holder => Some(format!(
            "Terminate session {} of this run? Its current step rolls back.",
            h.session_id
                .map_or_else(|| "?".to_owned(), |id| id.to_string())
        )),
        _ => None,
    }
}

fn outcome_text(outcome: &UnlockOutcome) -> String {
    match outcome {
        UnlockOutcome::NothingToDo => "lock: free; nothing to do\n".to_owned(),
        UnlockOutcome::ClearedStale(Some(h)) => {
            format!("removed the stale holder record of {}\n", h.describe())
        }
        UnlockOutcome::ClearedStale(None) => {
            "removed an unreadable stale holder record\n".to_owned()
        }
        UnlockOutcome::KilledHolder(h) => format!(
            "terminated the session of {}; the lock is released\n",
            h.describe()
        ),
    }
}
