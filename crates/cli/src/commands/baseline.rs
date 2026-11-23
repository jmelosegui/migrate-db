//! `baseline mark` (SPEC §4.1, ADOPTION.md step 6).

use std::time::Duration;

use serde_json::json;

use migrate_db_core::cancel::CancelToken;
use migrate_db_core::engine::{BaselineRequest, Engine, MarkReport, NoopObserver};
use migrate_db_core::error::{Error, exit};
use migrate_db_core::planner::{Plan, Policy};
use migrate_db_core::provider::{LockOptions, LockOwner};

use crate::cli::{GlobalArgs, RunArgs};
use crate::commands::migrate::{confirm, parse_version, read_fingerprint};
use crate::context::{Context, ci_run, identity, is_interactive};
use crate::output::{Output, to_value};
use crate::providers;

/// What `baseline mark` was asked to do.
#[derive(Debug)]
pub struct MarkArgs<'a> {
    /// `--version`.
    pub version: &'a str,
    /// `--description`.
    pub description: Option<&'a str>,
    /// `--mark-repeatables`.
    pub mark_repeatables: bool,
    /// `--force-unverified`.
    pub force_unverified: bool,
    /// `--yes`.
    pub yes: bool,
    /// `--dry-run`, `--expect-plan`, lock and connection.
    pub run: &'a RunArgs,
}

/// Run `baseline mark`.
pub async fn mark(global: &GlobalArgs, out: &Output, args: MarkArgs<'_>) -> Result<i32, Error> {
    let version = parse_version(args.version)?;
    let request = BaselineRequest {
        version: &version,
        description: args.description,
        mark_repeatables: args.mark_repeatables,
    };
    let ctx = Context::load(global, Some(&args.run.connect))?;
    let resolved = ctx.resolve(&args.run.connect);
    let set = ctx.discover(&resolved.target)?;
    let mut provider = providers::connect(&resolved).await?;
    let who = identity();
    let mut engine = Engine::new(provider.as_mut(), &set, Policy::default(), who.clone());
    let base = json!({ "provider": resolved.target.provider.as_str() });

    let preview = engine.dry_run_baseline(&request).await?;
    if args.run.dry_run {
        out.emit(
            "baseline mark",
            merge(base, json!({ "dry_run": true, "plan": to_value(&preview) })),
            || plan_text(&preview),
        )?;
        return Ok(exit::OK);
    }

    // SPEC §4.1: mark first proves this database matches the captured
    // schema. Fingerprints come with `baseline capture`; until then the run
    // has to say it checked the schema some other way.
    if !args.force_unverified {
        return Err(Error::BaselineUnverified(
            "baseline mark checks the live schema against the captured fingerprint first, \
             and `baseline capture` / `compare` are not available yet: compare the schema \
             yourself, then pass --force-unverified"
                .into(),
        ));
    }

    let mut expected = match &args.run.expect_plan {
        Some(path) => Some(read_fingerprint(path)?),
        None => None,
    };
    if !args.yes {
        if !is_interactive(global) {
            return Err(Error::Usage(
                "baseline mark needs --yes in non-interactive mode".into(),
            ));
        }
        eprint!("{}", plan_text(&preview));
        if !confirm("Record this baseline? No script runs; only ledger rows are written.") {
            eprintln!("aborted; nothing was written");
            return Ok(exit::USAGE);
        }
        // Write exactly what was confirmed.
        expected.get_or_insert(preview.fingerprint.clone());
    }

    let lock = LockOptions {
        timeout: Duration::from_secs(args.run.lock_timeout),
        owner: LockOwner::current(&who, ci_run()),
    };
    let cancel = CancelToken::new();
    let _signals = crate::signals::on_cancel(cancel.clone());
    let report = engine
        .with_cancel(cancel)
        .mark_baseline(&request, &lock, expected.as_deref(), &NoopObserver)
        .await?;
    out.emit(
        "baseline mark",
        merge(
            base,
            json!({ "dry_run": false, "report": to_value(&report) }),
        ),
        || report_text(&report),
    )?;
    Ok(exit::OK)
}

fn merge(mut a: serde_json::Value, b: serde_json::Value) -> serde_json::Value {
    if let (serde_json::Value::Object(a), serde_json::Value::Object(b)) = (&mut a, b) {
        a.extend(b);
    }
    a
}

fn plan_text(plan: &Plan) -> String {
    let mut t = String::from("baseline: ledger rows to write (no script runs)\n");
    for s in &plan.steps {
        t.push_str(&format!(
            "  {:<10} {:<20} {:<30} {}\n",
            s.kind.as_str(),
            s.version,
            s.description,
            s.script_name
        ));
    }
    t.push_str(&format!("fingerprint: {}\n", plan.fingerprint));
    t
}

fn report_text(report: &MarkReport) -> String {
    let version = report
        .plan
        .to_version
        .as_ref()
        .map_or_else(|| "none".to_owned(), ToString::to_string);
    let mut t = format!(
        "baseline recorded: database version {version}, {} ledger row(s) written, nothing executed\n",
        report.plan.steps.len()
    );
    if let Some(e) = &report.lock_release_error {
        t.push_str(&format!("warning: could not release the lock: {e}\n"));
    }
    t
}
