//! `status`, `up`, `down`, `goto`.

use std::fs;
use std::io::{BufRead, Write};
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use migrate_db_core::cancel::CancelToken;
use migrate_db_core::engine::{Engine, Observer, RunReport};
use migrate_db_core::error::{Error, exit};
use migrate_db_core::model::{Direction, Kind};
use migrate_db_core::planner::{MigrationState, Plan, PlannedStep, Policy, Request, StatusReport};
use migrate_db_core::provider::{LockOptions, LockOwner, LockStatus, ProviderError, StepOutcome};
use migrate_db_core::version::Version;

use crate::cli::{ConnectArgs, GlobalArgs, RunArgs};
use crate::context::{Context, ci_run, identity, is_interactive};
use crate::output::{Output, to_value};
use crate::providers;

/// Parse a version argument.
pub fn parse_version(text: &str) -> Result<Version, Error> {
    text.parse()
        .map_err(|e| Error::Usage(format!("invalid version `{text}`: {e}")))
}

/// Run `status`.
pub async fn status(
    global: &GlobalArgs,
    out: &Output,
    connect: &ConnectArgs,
) -> Result<i32, Error> {
    let ctx = Context::load(global, Some(connect))?;
    let resolved = ctx.resolve(connect);
    let set = ctx.discover(&resolved.target)?;
    let mut provider = providers::connect(&resolved).await?;
    let mut engine = Engine::new(provider.as_mut(), &set, Policy::default(), identity());
    let report = engine.status().await?;
    let lock = engine.lock_status().await?;
    let now = chrono::Utc::now();
    out.emit(
        "status",
        json!({
            "provider": resolved.target.provider.as_str(),
            "status": to_value(&report),
            "lock": lock_json(&lock, now),
        }),
        || {
            let mut t = status_text(&report);
            t.push_str(&lock_text(&lock, now));
            t
        },
    )?;
    Ok(exit::OK)
}

/// The `lock` object of SPEC §7 rule 10.
pub(crate) fn lock_json(lock: &LockStatus, now: chrono::DateTime<chrono::Utc>) -> Value {
    let since = lock.holder.as_ref().map(|h| h.acquired_at);
    json!({
        "state": if lock.held { "held" } else { "free" },
        "holder": lock.holder.as_ref().map(to_value),
        "since": since,
        "age_s": since.map(|s| (now - s).num_seconds()),
        "lease_expires_at": lock.lease_expires_at,
        "stale_info": lock.stale_info,
    })
}

pub(crate) fn lock_text(lock: &LockStatus, now: chrono::DateTime<chrono::Utc>) -> String {
    match (&lock.holder, lock.held) {
        (Some(h), true) => format!(
            "lock: held by {} for {}s\n",
            h.describe(),
            (now - h.acquired_at).num_seconds()
        ),
        (None, true) => "lock: held (holder unknown)\n".to_owned(),
        (Some(h), false) => format!("lock: free (stale holder info from {})\n", h.describe()),
        (None, false) => "lock: free\n".to_owned(),
    }
}

fn status_text(report: &StatusReport) -> String {
    let version = report
        .database_version
        .as_ref()
        .map_or_else(|| "none".to_owned(), ToString::to_string);
    let mut t = format!("database version {version}, {} pending\n", report.pending());
    for e in &report.entries {
        let state = match e.state {
            MigrationState::Changed => "changed (will re-apply)".to_owned(),
            other => serde_json::to_value(other)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default(),
        };
        let applied = e
            .applied_at
            .map(|d| d.format("%Y-%m-%d %H:%M:%SZ").to_string())
            .unwrap_or_default();
        // Repeatables show their file name stem; the description repeats it.
        let description = if e.kind == Kind::Repeatable {
            ""
        } else {
            e.description.as_str()
        };
        t.push_str(&format!(
            "  {:<24} {:<24} {:<20} {}\n",
            e.version, state, applied, description
        ));
    }
    t
}

/// What a mutating command asks for.
#[derive(Debug)]
pub struct Invocation<'a> {
    /// Command name.
    pub command: &'static str,
    /// The request.
    pub request: Request,
    /// `--dry-run` / `--expect-plan`.
    pub run: &'a RunArgs,
    /// `--allow-out-of-order`.
    pub allow_out_of_order: bool,
    /// `--allow-down`.
    pub allow_down: bool,
    /// `--yes`.
    pub yes: bool,
}

/// Run `up` / `down` / `goto`.
pub async fn execute(global: &GlobalArgs, out: &Output, inv: Invocation<'_>) -> Result<i32, Error> {
    let ctx = Context::load(global, Some(&inv.run.connect))?;
    let resolved = ctx.resolve(&inv.run.connect);
    let set = ctx.discover(&resolved.target)?;
    let mut provider = providers::connect(&resolved).await?;
    let policy = Policy {
        allow_down: ctx.allow_down(inv.allow_down),
        allow_out_of_order: ctx.allow_out_of_order(inv.allow_out_of_order),
    };
    let who = identity();
    let mut engine = Engine::new(provider.as_mut(), &set, policy, who.clone())
        .with_placeholders(ctx.placeholders());
    let base = json!({ "provider": resolved.target.provider.as_str() });

    let preview = engine.dry_run(&inv.request).await?;
    if inv.run.dry_run {
        out.emit(
            inv.command,
            merge(base, json!({ "dry_run": true, "plan": to_value(&preview) })),
            || plan_text(&preview),
        )?;
        return Ok(exit::OK);
    }

    let mut expected = match &inv.run.expect_plan {
        Some(path) => Some(read_fingerprint(path)?),
        None => None,
    };
    let reverts = preview.steps.iter().any(|s| s.direction == Direction::Down);
    if reverts && !inv.yes {
        if !is_interactive(global) {
            return Err(Error::Usage(
                "this plan reverts migrations; pass --yes to confirm in non-interactive mode"
                    .into(),
            ));
        }
        eprint!("{}", plan_text(&preview));
        if !confirm("This plan reverts migrations. Proceed?") {
            eprintln!("aborted; nothing was executed");
            return Ok(exit::USAGE);
        }
        // Execute exactly what was confirmed.
        expected.get_or_insert(preview.fingerprint.clone());
    }

    let lock = LockOptions {
        timeout: Duration::from_secs(inv.run.lock_timeout),
        owner: LockOwner::current(&who, ci_run()),
    };
    // Steps print as text only; lock waits go to stderr in both modes.
    let observer = CliObserver {
        steps: !out.is_json(),
    };
    // Ctrl-C / SIGTERM: finish or roll back the current step, release the
    // lock, exit 4 (SPEC §7 rule 7). A second signal exits at once, which is
    // still safe: the server rolls back and drops the lock with the session.
    let cancel = CancelToken::new();
    let _signals = crate::signals::on_cancel(cancel.clone());
    let mut engine = engine.with_cancel(cancel);
    let report = engine
        .run(&inv.request, &lock, expected.as_deref(), &observer)
        .await?;

    out.emit(
        inv.command,
        merge(
            base,
            json!({ "dry_run": false, "report": to_value(&report) }),
        ),
        || report_text(&report),
    )?;
    Ok(report.exit_code())
}

fn merge(mut a: Value, b: Value) -> Value {
    if let (Value::Object(a), Value::Object(b)) = (&mut a, b) {
        a.extend(b);
    }
    a
}

pub(crate) fn read_fingerprint(path: &Path) -> Result<String, Error> {
    let text = fs::read_to_string(path)
        .map_err(|e| Error::Usage(format!("cannot read plan file `{}`: {e}", path.display())))?;
    let doc: Value = serde_json::from_str(&text)
        .map_err(|e| Error::Usage(format!("plan file `{}` is not JSON: {e}", path.display())))?;
    doc.pointer("/plan/fingerprint")
        .or_else(|| doc.pointer("/fingerprint"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            Error::Usage(format!(
                "plan file `{}` has no fingerprint (create it with --dry-run --output json --output-file)",
                path.display()
            ))
        })
}

pub(crate) fn confirm(question: &str) -> bool {
    eprint!("{question} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    if std::io::stdin().lock().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim(), "y" | "Y" | "yes" | "YES")
}

fn version_text(v: Option<&Version>) -> String {
    v.map_or_else(|| "none".to_owned(), ToString::to_string)
}

fn plan_text(plan: &Plan) -> String {
    let mut t = format!(
        "plan: database version {} -> {}, {} step(s)\n",
        version_text(plan.from_version.as_ref()),
        version_text(plan.to_version.as_ref()),
        plan.steps.len()
    );
    for w in &plan.warnings {
        t.push_str(&format!("  warning: {w}\n"));
    }
    for s in &plan.steps {
        t.push_str(&format!(
            "  {:<4} {:<16} {:<30} {}{}\n",
            s.direction,
            s.version,
            s.description,
            s.script,
            if s.transactional {
                ""
            } else {
                "  (no transaction)"
            }
        ));
    }
    t.push_str(&format!("fingerprint: {}\n", plan.fingerprint));
    t
}

fn report_text(report: &RunReport) -> String {
    let mut t = String::new();
    if report.plan.is_empty() {
        t.push_str(&format!(
            "nothing to do (database version {})\n",
            version_text(report.plan.from_version.as_ref())
        ));
    }
    if let Some(f) = &report.failure {
        t.push_str(&format!(
            "FAILED {} {} {}: {}\n",
            f.direction, f.version, f.description, f.error
        ));
        if report.completed.is_empty() {
            t.push_str("no step completed before the failure\n");
        } else {
            let done: Vec<String> = report
                .completed
                .iter()
                .map(|s| format!("{} {}", s.direction, s.version))
                .collect();
            t.push_str(&format!(
                "completed before the failure ({}): {}\n",
                done.len(),
                done.join(", ")
            ));
        }
    } else if !report.plan.is_empty() {
        t.push_str(&format!(
            "done: {} step(s), database version {}\n",
            report.completed.len(),
            version_text(report.plan.to_version.as_ref())
        ));
    }
    if let Some(e) = &report.lock_release_error {
        t.push_str(&format!("warning: could not release the lock: {e}\n"));
    }
    t
}

struct CliObserver {
    steps: bool,
}

impl Observer for CliObserver {
    fn lock_waiting(&self, waited: Duration, lock: Option<&LockStatus>) {
        let holder = match lock {
            Some(LockStatus {
                holder: Some(h), ..
            }) => h.describe(),
            Some(LockStatus { held: false, .. }) => "nobody (it was just released)".to_owned(),
            _ => "an unknown holder".to_owned(),
        };
        eprintln!(
            "waiting for the migration lock ({}s so far), held by {holder}",
            waited.as_secs()
        );
    }

    fn step_finished(&self, step: &PlannedStep, result: &Result<StepOutcome, ProviderError>) {
        if !self.steps {
            return;
        }
        match result {
            Ok(o) => eprintln!(
                "ok   {} {} {} ({} ms)",
                step.direction, step.version, step.description, o.duration_ms
            ),
            Err(e) => eprintln!(
                "FAIL {} {} {}: {e}",
                step.direction, step.version, step.description
            ),
        }
    }
}
