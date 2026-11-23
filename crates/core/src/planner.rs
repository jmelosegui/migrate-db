//! The planner: a pure function from (files, ledger, request) to a plan.
//!
//! No I/O, no clock, no async. All reconciliation rules (SPEC §7) live here
//! so they can be tested exhaustively.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::model::{AppliedMigration, Direction, Kind, Status};
use crate::source::{MigrationSet, RepeatableMigration, VersionedMigration};
use crate::version::Version;

/// What the caller asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Apply pending migrations, optionally up to a version or a number of steps.
    Up {
        /// Apply up to and including this version.
        to: Option<Version>,
        /// Apply at most this many migrations.
        steps: Option<usize>,
    },
    /// Revert applied migrations (default: 1 step).
    Down {
        /// Revert everything above this version (`0` = everything).
        to: Option<Version>,
        /// Revert this many migrations.
        steps: Option<usize>,
    },
    /// Move to exactly this version (SPEC §7 rule 9).
    Goto {
        /// The target version (`0` = nothing applied).
        target: Version,
    },
}

/// Per-run policy that affects planning. Both default to **false** (fail
/// closed); the run turns them on (ADR-0005).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Policy {
    /// `--allow-down` / `MIGRATE_DB_ALLOW_DOWN`.
    pub allow_down: bool,
    /// `--allow-out-of-order` / `MIGRATE_DB_ALLOW_OUT_OF_ORDER`.
    pub allow_out_of_order: bool,
}

/// What a step applies: a versioned migration, or a repeatable (SPEC §3.6).
///
/// Its text form is the ledger key: the version text (`20261118093000`) or
/// `R__<name>`. It serializes as that string, so JSON output and the ledger
/// see the same value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StepId {
    /// A versioned migration.
    Version(Version),
    /// A repeatable migration; holds the ledger key `R__<name>`.
    Repeatable(String),
}

impl StepId {
    /// The repeatable's ledger key for `name`.
    pub fn repeatable(name: &str) -> Self {
        Self::Repeatable(format!("R__{name}"))
    }

    /// The ledger key text.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Version(v) => v.as_str(),
            Self::Repeatable(key) => key,
        }
    }

    /// The version, for a versioned step.
    pub fn version(&self) -> Option<&Version> {
        match self {
            Self::Version(v) => Some(v),
            Self::Repeatable(_) => None,
        }
    }

    /// The repeatable's name (without `R__`), for a repeatable step.
    pub fn repeatable_name(&self) -> Option<&str> {
        match self {
            Self::Version(_) => None,
            Self::Repeatable(key) => key.strip_prefix("R__"),
        }
    }
}

impl fmt::Display for StepId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for StepId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl From<Version> for StepId {
    fn from(v: Version) -> Self {
        Self::Version(v)
    }
}

/// One step of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedStep {
    /// Versioned migration version, or the repeatable's `R__<name>`.
    pub version: StepId,
    /// `versioned` or `repeatable` (what the ledger row's `kind` will be);
    /// `baseline` for the marker that `baseline mark` writes.
    pub kind: Kind,
    /// Description.
    pub description: String,
    /// Direction.
    pub direction: Direction,
    /// File that will be executed (relative path).
    pub script: String,
    /// Up file name (what the ledger stores as `script_name`).
    pub script_name: String,
    /// Checksum of the up script (what the ledger stores).
    pub checksum: String,
    /// Runs inside a transaction (no `-- migrate:no-transaction`).
    pub transactional: bool,
}

/// Non-fatal planning findings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PlanWarning {
    /// An applied migration whose file is gone.
    MissingFile {
        /// The version (or `R__<name>`).
        version: String,
    },
    /// New or changed repeatables were not applied, because the run doesn't
    /// end at the latest versioned migration (SPEC §3.6).
    RepeatablesSkipped {
        /// How many would have run.
        count: usize,
    },
}

impl fmt::Display for PlanWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingFile { version } => {
                write!(f, "migration {version} is applied but its file is missing")
            }
            Self::RepeatablesSkipped { count } => write!(
                f,
                "{count} new or changed repeatable(s) skipped: repeatables run only when the run ends at the latest migration"
            ),
        }
    }
}

/// A validated plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Plan {
    /// Database version before the plan.
    pub from_version: Option<Version>,
    /// Database version after the plan.
    pub to_version: Option<Version>,
    /// Ordered steps.
    pub steps: Vec<PlannedStep>,
    /// Non-fatal findings.
    pub warnings: Vec<PlanWarning>,
    /// Fingerprint for `--expect-plan` (SPEC §10.1).
    pub fingerprint: String,
}

impl Plan {
    /// `true` if there is nothing to do.
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

/// Why a plan can't be made. Nothing has been executed when this is returned.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// A ledger row has an unparseable version.
    #[error("ledger row has invalid version `{0}`")]
    InvalidLedgerVersion(String),
    /// A failed / in-progress row blocks further runs (SPEC §6.3).
    #[error(
        "migration {version} is {status} in the ledger; fix the database, then run `migrate-db repair`"
    )]
    Unresolved {
        /// The version.
        version: String,
        /// `failed` or `in_progress`.
        status: String,
    },
    /// Applied migrations whose files changed.
    #[error("checksum mismatch for applied migration(s) {}: the file changed after it was applied", .0.join(", "))]
    ChecksumMismatch(Vec<String>),
    /// Pending migrations older than the database version.
    #[error("pending migration(s) {} are older than the database version; pass --allow-out-of-order or set MIGRATE_DB_ALLOW_OUT_OF_ORDER=1", .0.join(", "))]
    OutOfOrder(Vec<String>),
    /// A down step would need a down file that doesn't exist.
    #[error("migration {0} is irreversible (no down file)")]
    Irreversible(String),
    /// A down step for an applied migration whose file is missing.
    #[error("cannot revert {0}: its migration file is missing")]
    MissingFileForDown(String),
    /// A down step would cross the baseline.
    #[error("cannot go below baseline {0}: a baseline has no down")]
    BelowBaseline(String),
    /// `down` is disabled for this environment.
    #[error(
        "reverting migrations is not allowed in this run; pass --allow-down or set MIGRATE_DB_ALLOW_DOWN=1 where it is safe"
    )]
    DownNotAllowed,
    /// `goto` target isn't a known version.
    #[error("unknown version {0}: it is neither a migration file nor in the ledger")]
    UnknownTarget(String),
    /// Both `--to` and `--steps`.
    #[error("--to and --steps cannot be combined")]
    ConflictingLimits,
    /// `baseline mark` on a ledger that already has rows.
    #[error(
        "baseline mark needs an empty ledger, but it has {0} row(s): this database is already managed by migrate-db"
    )]
    LedgerNotEmpty(usize),
    /// `baseline mark --version` without a matching migration file.
    #[error(
        "no migration file for baseline version {0}: add V{0}__<description>.up.sql (from `baseline capture` or written by hand) first"
    )]
    UnknownBaseline(String),
}

impl PlanError {
    /// `true` for CLI usage problems (exit 64) rather than validation failures (exit 2).
    pub fn is_usage(&self) -> bool {
        matches!(self, Self::UnknownTarget(_) | Self::ConflictingLimits)
    }
}

/// State of one migration in `status` (SPEC §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MigrationState {
    /// Applied and unchanged.
    Applied,
    /// Not applied yet.
    Pending,
    /// The baseline marker, or a file covered by it.
    Baseline,
    /// Failed part-way.
    Failed,
    /// In progress (or crashed).
    InProgress,
    /// Applied but the file changed.
    ChecksumMismatch,
    /// Applied but the file is gone.
    MissingFile,
    /// Pending but older than the database version.
    OutOfOrder,
    /// Repeatable whose file changed since it was applied: `up` re-applies it.
    Changed,
    /// Repeatable that has never been applied.
    New,
}

/// One row of a status report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusEntry {
    /// Version text, or `R__<name>` for a repeatable.
    pub version: String,
    /// `versioned`, `baseline` or `repeatable`.
    pub kind: Kind,
    /// Description.
    pub description: String,
    /// State.
    pub state: MigrationState,
    /// When applied, if applied.
    pub applied_at: Option<DateTime<Utc>>,
    /// `true` if there is no down file.
    pub irreversible: bool,
}

/// Result of `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusReport {
    /// Current database version.
    pub database_version: Option<Version>,
    /// All migrations known from files or the ledger, ascending.
    pub entries: Vec<StatusEntry>,
}

impl StatusReport {
    /// Number of migrations `up` would run: pending (incl. out-of-order)
    /// versioned ones, and new or changed repeatables.
    pub fn pending(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| {
                matches!(
                    e.state,
                    MigrationState::Pending
                        | MigrationState::OutOfOrder
                        | MigrationState::Changed
                        | MigrationState::New
                )
            })
            .count()
    }

    /// `true` if any entry needs attention.
    pub fn has_problems(&self) -> bool {
        self.entries.iter().any(|e| {
            matches!(
                e.state,
                MigrationState::Failed
                    | MigrationState::InProgress
                    | MigrationState::ChecksumMismatch
                    | MigrationState::OutOfOrder
            )
        })
    }
}

/// Ledger rows, indexed.
struct LedgerView<'a> {
    applied: BTreeMap<Version, &'a AppliedMigration>,
    baseline: Option<(Version, &'a AppliedMigration)>,
    /// Applied repeatables by name (without `R__`).
    repeatables: BTreeMap<String, &'a AppliedMigration>,
    /// Failed / in-progress rows of any kind; they block every run.
    unresolved: Vec<&'a AppliedMigration>,
}

impl<'a> LedgerView<'a> {
    fn build(ledger: &'a [AppliedMigration]) -> Result<Self, PlanError> {
        let mut view = Self {
            applied: BTreeMap::new(),
            baseline: None,
            repeatables: BTreeMap::new(),
            unresolved: Vec::new(),
        };
        for row in ledger {
            if row.kind == Kind::Repeatable {
                if row.status != Status::Applied {
                    view.unresolved.push(row);
                } else {
                    let name = row.version.strip_prefix("R__").unwrap_or(&row.version);
                    view.repeatables.insert(name.to_owned(), row);
                }
                continue;
            }
            let version: Version = row
                .version
                .parse()
                .map_err(|_| PlanError::InvalidLedgerVersion(row.version.clone()))?;
            if row.status != Status::Applied {
                view.unresolved.push(row);
                continue;
            }
            match row.kind {
                Kind::Baseline => {
                    if view.baseline.as_ref().is_none_or(|(b, _)| &version > b) {
                        view.baseline = Some((version, row));
                    }
                }
                _ => {
                    view.applied.insert(version, row);
                }
            }
        }
        Ok(view)
    }

    fn baseline_version(&self) -> Option<&Version> {
        self.baseline.as_ref().map(|(v, _)| v)
    }

    /// Highest of applied versioned rows and the baseline.
    fn database_version(&self) -> Option<Version> {
        let top = self.applied.keys().next_back();
        match (top, self.baseline_version()) {
            (Some(a), Some(b)) => Some(a.max(b).clone()),
            (a, b) => a.or(b).cloned(),
        }
    }

    fn is_covered(&self, version: &Version) -> bool {
        self.applied.contains_key(version) || self.baseline_version().is_some_and(|b| version <= b)
    }
}

/// Compute `status` (read-only; never fails on drift, only reports it).
pub fn status(set: &MigrationSet, ledger: &[AppliedMigration]) -> Result<StatusReport, PlanError> {
    let view = LedgerView::build(ledger)?;
    let db_version = view.database_version();
    let mut entries: BTreeMap<Version, StatusEntry> = BTreeMap::new();

    for m in &set.versioned {
        let irreversible = m.is_irreversible();
        let (state, applied_at) = if let Some(row) = view.applied.get(&m.version) {
            let state = if row.checksum == m.up.checksum.as_str() {
                MigrationState::Applied
            } else {
                MigrationState::ChecksumMismatch
            };
            (state, Some(row.applied_at))
        } else if view.baseline_version().is_some_and(|b| &m.version <= b) {
            (MigrationState::Baseline, None)
        } else if db_version.as_ref().is_some_and(|d| &m.version < d) {
            (MigrationState::OutOfOrder, None)
        } else {
            (MigrationState::Pending, None)
        };
        entries.insert(
            m.version.clone(),
            StatusEntry {
                version: m.version.to_string(),
                kind: Kind::Versioned,
                description: m.description.clone(),
                state,
                applied_at,
                irreversible,
            },
        );
    }

    for (version, row) in &view.applied {
        entries
            .entry(version.clone())
            .or_insert_with(|| StatusEntry {
                version: row.version.clone(),
                kind: row.kind,
                description: row.description.clone(),
                state: MigrationState::MissingFile,
                applied_at: Some(row.applied_at),
                irreversible: true,
            });
    }
    if let Some((version, row)) = &view.baseline {
        entries.insert(
            version.clone(),
            StatusEntry {
                version: row.version.clone(),
                kind: Kind::Baseline,
                description: row.description.clone(),
                state: MigrationState::Baseline,
                applied_at: Some(row.applied_at),
                irreversible: true,
            },
        );
    }
    let unresolved_state = |row: &AppliedMigration| {
        if row.status == Status::Failed {
            MigrationState::Failed
        } else {
            MigrationState::InProgress
        }
    };
    for row in view
        .unresolved
        .iter()
        .filter(|r| r.kind != Kind::Repeatable)
    {
        if let Ok(version) = row.version.parse::<Version>() {
            let irreversible = set
                .get(&version)
                .is_none_or(VersionedMigration::is_irreversible);
            entries.insert(
                version,
                StatusEntry {
                    version: row.version.clone(),
                    kind: row.kind,
                    description: row.description.clone(),
                    state: unresolved_state(row),
                    applied_at: Some(row.applied_at),
                    irreversible,
                },
            );
        }
    }

    // Repeatables after the versioned migrations, alphabetical (SPEC §3.6).
    let mut repeatables: BTreeMap<String, StatusEntry> = BTreeMap::new();
    for r in &set.repeatables {
        let (state, applied_at) = match view.repeatables.get(&r.name) {
            Some(row) if row.checksum == r.script.checksum.as_str() => {
                (MigrationState::Applied, Some(row.applied_at))
            }
            Some(row) => (MigrationState::Changed, Some(row.applied_at)),
            None => (MigrationState::New, None),
        };
        repeatables.insert(r.name.clone(), repeatable_entry(&r.name, state, applied_at));
    }
    for (name, row) in &view.repeatables {
        repeatables.entry(name.clone()).or_insert_with(|| {
            repeatable_entry(name, MigrationState::MissingFile, Some(row.applied_at))
        });
    }
    for row in view
        .unresolved
        .iter()
        .filter(|r| r.kind == Kind::Repeatable)
    {
        let name = row.version.strip_prefix("R__").unwrap_or(&row.version);
        repeatables.insert(
            name.to_owned(),
            repeatable_entry(name, unresolved_state(row), Some(row.applied_at)),
        );
    }

    Ok(StatusReport {
        database_version: db_version,
        entries: entries
            .into_values()
            .chain(repeatables.into_values())
            .collect(),
    })
}

fn repeatable_entry(
    name: &str,
    state: MigrationState,
    applied_at: Option<DateTime<Utc>>,
) -> StatusEntry {
    StatusEntry {
        version: format!("R__{name}"),
        kind: Kind::Repeatable,
        description: name.to_owned(),
        state,
        applied_at,
        // No down file, but `down` never touches repeatables (SPEC §3.6).
        irreversible: false,
    }
}

/// Make a plan. Validates the **whole** plan before returning (SPEC §7 rule 4).
pub fn plan(
    set: &MigrationSet,
    ledger: &[AppliedMigration],
    request: &Request,
    policy: Policy,
) -> Result<Plan, PlanError> {
    let view = LedgerView::build(ledger)?;

    if let Some(row) = view.unresolved.first() {
        return Err(PlanError::Unresolved {
            version: row.version.clone(),
            status: row.status.as_str().to_owned(),
        });
    }

    let mismatched: Vec<String> = view
        .applied
        .iter()
        .filter_map(|(v, row)| {
            set.get(v)
                .filter(|m| m.up.checksum.as_str() != row.checksum)
                .map(|_| v.to_string())
        })
        .collect();
    if !mismatched.is_empty() {
        return Err(PlanError::ChecksumMismatch(mismatched));
    }

    let mut warnings: Vec<PlanWarning> = view
        .applied
        .keys()
        .filter(|v| set.get(v).is_none())
        .map(|v| PlanWarning::MissingFile {
            version: v.to_string(),
        })
        .collect();
    warnings.extend(
        view.repeatables
            .keys()
            .filter(|name| !set.repeatables.iter().any(|r| &r.name == *name))
            .map(|name| PlanWarning::MissingFile {
                version: format!("R__{name}"),
            }),
    );

    let from_version = view.database_version();
    let pending: Vec<&VersionedMigration> = set
        .versioned
        .iter()
        .filter(|m| !view.is_covered(&m.version))
        .collect();

    let mut steps = match request {
        Request::Up { to, steps } => {
            if to.is_some() && steps.is_some() {
                return Err(PlanError::ConflictingLimits);
            }
            check_out_of_order(&pending, from_version.as_ref(), policy)?;
            pending
                .iter()
                .filter(|m| to.as_ref().is_none_or(|t| &m.version <= t))
                .take(steps.unwrap_or(usize::MAX))
                .map(|m| up_step(m))
                .collect::<Vec<_>>()
        }
        Request::Down { to, steps } => {
            if to.is_some() && steps.is_some() {
                return Err(PlanError::ConflictingLimits);
            }
            let applied = applied_descending(&view);
            let selected: Vec<&Version> = match to {
                Some(t) => applied.into_iter().filter(|v| *v > t).collect(),
                None => applied.into_iter().take(steps.unwrap_or(1)).collect(),
            };
            let steps = down_steps(set, &view, &selected)?;
            if !steps.is_empty() && !policy.allow_down {
                return Err(PlanError::DownNotAllowed);
            }
            steps
        }
        Request::Goto { target } => {
            let known = target.is_zero()
                || set.get(target).is_some()
                || view.applied.contains_key(target)
                || view.baseline_version() == Some(target);
            if !known {
                return Err(PlanError::UnknownTarget(target.to_string()));
            }
            let applied = applied_descending(&view);
            let to_revert: Vec<&Version> =
                applied.iter().copied().filter(|v| *v > target).collect();
            let mut steps = down_steps(set, &view, &to_revert)?;
            if !steps.is_empty() && !policy.allow_down {
                return Err(PlanError::DownNotAllowed);
            }
            let remaining_top = applied.iter().copied().find(|v| *v <= target).cloned();
            let ups: Vec<&VersionedMigration> = pending
                .iter()
                .copied()
                .filter(|m| &m.version <= target)
                .collect();
            check_out_of_order(&ups, remaining_top.as_ref(), policy)?;
            steps.extend(ups.into_iter().map(up_step));
            steps
        }
    };

    // Repeatables (SPEC §3.6): after the versioned steps of `up` / `goto`,
    // new or changed ones, alphabetically, and only when the run ends at
    // the latest versioned migration. `down` never touches them.
    if !matches!(request, Request::Down { .. }) {
        let due: Vec<&RepeatableMigration> = set
            .repeatables
            .iter()
            .filter(|r| {
                view.repeatables
                    .get(&r.name)
                    .is_none_or(|row| row.checksum != r.script.checksum.as_str())
            })
            .collect();
        if !due.is_empty() {
            let end = simulate(&view, &steps);
            let at_latest = set
                .latest()
                .is_none_or(|l| end.as_ref().is_some_and(|e| e >= &l.version));
            if at_latest {
                steps.extend(due.into_iter().map(repeatable_step));
            } else {
                warnings.push(PlanWarning::RepeatablesSkipped { count: due.len() });
            }
        }
    }

    let to_version = simulate(&view, &steps);
    let fingerprint = fingerprint(from_version.as_ref(), &steps);
    Ok(Plan {
        from_version,
        to_version,
        steps,
        warnings,
        fingerprint,
    })
}

fn check_out_of_order(
    pending: &[&VersionedMigration],
    db_version: Option<&Version>,
    policy: Policy,
) -> Result<(), PlanError> {
    if policy.allow_out_of_order {
        return Ok(());
    }
    let Some(db) = db_version else {
        return Ok(());
    };
    let older: Vec<String> = pending
        .iter()
        .filter(|m| &m.version < db)
        .map(|m| m.version.to_string())
        .collect();
    if older.is_empty() {
        Ok(())
    } else {
        Err(PlanError::OutOfOrder(older))
    }
}

/// Applied versioned rows plus the baseline, highest first.
fn applied_descending<'v>(view: &'v LedgerView<'_>) -> Vec<&'v Version> {
    let mut all: Vec<&Version> = view.applied.keys().collect();
    if let Some(b) = view.baseline_version() {
        all.push(b);
    }
    all.sort_by(|a, b| b.cmp(a));
    all
}

fn down_steps(
    set: &MigrationSet,
    view: &LedgerView<'_>,
    versions: &[&Version],
) -> Result<Vec<PlannedStep>, PlanError> {
    let mut steps = Vec::with_capacity(versions.len());
    for &version in versions {
        if view.baseline_version() == Some(version) {
            return Err(PlanError::BelowBaseline(version.to_string()));
        }
        let m = set
            .get(version)
            .ok_or_else(|| PlanError::MissingFileForDown(version.to_string()))?;
        let down = m
            .down
            .as_ref()
            .ok_or_else(|| PlanError::Irreversible(version.to_string()))?;
        steps.push(PlannedStep {
            version: StepId::Version(m.version.clone()),
            kind: Kind::Versioned,
            description: m.description.clone(),
            direction: Direction::Down,
            script: down.relative_path.clone(),
            script_name: m.up.file_name.clone(),
            checksum: m.up.checksum.to_string(),
            transactional: !down.directives.no_transaction,
        });
    }
    Ok(steps)
}

fn up_step(m: &VersionedMigration) -> PlannedStep {
    PlannedStep {
        version: StepId::Version(m.version.clone()),
        kind: Kind::Versioned,
        description: m.description.clone(),
        direction: Direction::Up,
        script: m.up.relative_path.clone(),
        script_name: m.up.file_name.clone(),
        checksum: m.up.checksum.to_string(),
        transactional: !m.up.directives.no_transaction,
    }
}

fn repeatable_step(r: &RepeatableMigration) -> PlannedStep {
    PlannedStep {
        version: StepId::repeatable(&r.name),
        kind: Kind::Repeatable,
        description: r.name.clone(),
        direction: Direction::Up,
        script: r.script.relative_path.clone(),
        script_name: r.script.file_name.clone(),
        checksum: r.script.checksum.to_string(),
        transactional: !r.script.directives.no_transaction,
    }
}

/// Database version after applying `steps`. Repeatables don't count.
fn simulate(view: &LedgerView<'_>, steps: &[PlannedStep]) -> Option<Version> {
    let mut applied: BTreeSet<Version> = view.applied.keys().cloned().collect();
    if let Some(b) = view.baseline_version() {
        applied.insert(b.clone());
    }
    for s in steps {
        let Some(v) = s.version.version() else {
            continue;
        };
        match s.direction {
            Direction::Up => applied.insert(v.clone()),
            Direction::Down => applied.remove(v),
        };
    }
    applied.into_iter().next_back()
}

/// Plan for `baseline mark` (SPEC §4.1, ADOPTION.md step 6): one
/// `baseline` row for `version`, so every migration at or below it counts
/// as applied, plus, with `mark_repeatables`, one row per repeatable at its
/// current checksum. **Nothing in this plan is ever executed**; the steps
/// only describe the ledger rows to write.
///
/// The ledger must be empty, and `version` must be a versioned migration
/// file (the captured or hand-written baseline script), whose checksum the
/// marker records. `description` overrides the file's description.
pub fn baseline_plan(
    set: &MigrationSet,
    ledger: &[AppliedMigration],
    version: &Version,
    description: Option<&str>,
    mark_repeatables: bool,
) -> Result<Plan, PlanError> {
    if !ledger.is_empty() {
        return Err(PlanError::LedgerNotEmpty(ledger.len()));
    }
    let m = set
        .get(version)
        .ok_or_else(|| PlanError::UnknownBaseline(version.to_string()))?;
    let mut steps = vec![PlannedStep {
        kind: Kind::Baseline,
        description: description.map_or_else(|| m.description.clone(), str::to_owned),
        ..up_step(m)
    }];
    if mark_repeatables {
        steps.extend(set.repeatables.iter().map(repeatable_step));
    }
    // Not interchangeable with an `up` plan of the same files: approving
    // one must never authorize the other.
    let mut h = Sha256::new();
    h.update(b"migrate-db-baseline-v1\n");
    h.update(fingerprint(None, &steps).as_bytes());
    for st in &steps {
        h.update(format!("{} {}\n", st.kind.as_str(), st.description).as_bytes());
    }
    Ok(Plan {
        from_version: None,
        to_version: Some(version.clone()),
        steps,
        warnings: Vec::new(),
        fingerprint: hex::encode(h.finalize()),
    })
}

/// Stable fingerprint of a plan (SPEC §10.1).
pub fn fingerprint(from: Option<&Version>, steps: &[PlannedStep]) -> String {
    let mut h = Sha256::new();
    h.update(b"migrate-db-plan-v1\n");
    h.update(
        format!(
            "from:{}\n",
            from.map_or_else(|| "none".to_owned(), Version::to_string)
        )
        .as_bytes(),
    );
    for s in steps {
        h.update(format!("{} {} {}\n", s.direction, s.version, s.checksum).as_bytes());
    }
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checksum::Checksum;
    use crate::directives::Directives;
    use crate::source::ScriptFile;
    use std::path::PathBuf;

    fn script(name: &str, body: &str) -> ScriptFile {
        ScriptFile {
            path: PathBuf::from(name),
            relative_path: name.to_owned(),
            file_name: name.to_owned(),
            extension: "sql".into(),
            content: body.to_owned(),
            checksum: Checksum::of(body.as_bytes()),
            directives: Directives::default(),
        }
    }

    /// `(version, reversible)` pairs → a migration set.
    fn set(specs: &[(u32, bool)]) -> MigrationSet {
        let mut versioned: Vec<_> = specs
            .iter()
            .map(|&(v, reversible)| VersionedMigration {
                version: v.to_string().parse().unwrap(),
                name: format!("m{v}"),
                description: format!("m{v}"),
                up: script(&format!("V{v}__m{v}.up.sql"), &format!("up {v}")),
                down: reversible
                    .then(|| script(&format!("V{v}__m{v}.down.sql"), &format!("down {v}"))),
            })
            .collect();
        versioned.sort_by(|a, b| a.version.cmp(&b.version));
        MigrationSet {
            versioned,
            ..MigrationSet::default()
        }
    }

    fn row(version: u32, kind: Kind, status: Status, checksum: &str) -> AppliedMigration {
        AppliedMigration {
            version: version.to_string(),
            description: format!("m{version}"),
            script_name: format!("V{version}__m{version}.up.sql"),
            checksum: checksum.to_owned(),
            kind,
            status,
            direction: None,
            applied_at: DateTime::<Utc>::UNIX_EPOCH,
            applied_by: "test".into(),
            execution_ms: 0,
            tool_version: "test".into(),
            executor: "native".into(),
            runner_version: None,
            op_count: None,
            completed_ops: None,
            failed_op: None,
            recovery: None,
            lock_epoch: None,
        }
    }

    fn applied(set: &MigrationSet, versions: &[u32]) -> Vec<AppliedMigration> {
        versions
            .iter()
            .map(|&v| {
                let m = set.get(&v.to_string().parse().unwrap()).unwrap();
                row(v, Kind::Versioned, Status::Applied, m.up.checksum.as_str())
            })
            .collect()
    }

    fn v(n: u32) -> Version {
        n.to_string().parse().unwrap()
    }

    fn up() -> Request {
        Request::Up {
            to: None,
            steps: None,
        }
    }

    fn down(steps: Option<usize>, to: Option<u32>) -> Request {
        Request::Down {
            to: to.map(v),
            steps,
        }
    }

    const ALLOW_ALL: Policy = Policy {
        allow_down: true,
        allow_out_of_order: true,
    };

    fn versions(plan: &Plan) -> Vec<(Direction, String)> {
        plan.steps
            .iter()
            .map(|s| (s.direction, s.version.to_string()))
            .collect()
    }

    use Direction::{Down as D, Up as U};

    fn vs(items: &[(Direction, u32)]) -> Vec<(Direction, String)> {
        items.iter().map(|&(d, n)| (d, n.to_string())).collect()
    }

    #[test]
    fn up_applies_pending_in_order_with_limits() {
        let s = set(&[(3, true), (1, true), (2, true)]);
        let p = plan(&s, &[], &up(), Policy::default()).unwrap();
        assert_eq!(versions(&p), vs(&[(U, 1), (U, 2), (U, 3)]));
        assert_eq!(p.from_version, None);
        assert_eq!(p.to_version, Some(v(3)));

        let p = plan(
            &s,
            &[],
            &Request::Up {
                to: Some(v(2)),
                steps: None,
            },
            Policy::default(),
        )
        .unwrap();
        assert_eq!(versions(&p), vs(&[(U, 1), (U, 2)]));
        let p = plan(
            &s,
            &[],
            &Request::Up {
                to: None,
                steps: Some(1),
            },
            Policy::default(),
        )
        .unwrap();
        assert_eq!(versions(&p), vs(&[(U, 1)]));
        assert_eq!(
            plan(
                &s,
                &[],
                &Request::Up {
                    to: Some(v(2)),
                    steps: Some(1)
                },
                Policy::default()
            ),
            Err(PlanError::ConflictingLimits)
        );
    }

    #[test]
    fn up_with_nothing_pending_is_empty() {
        let s = set(&[(1, true)]);
        let p = plan(&s, &applied(&s, &[1]), &up(), Policy::default()).unwrap();
        assert!(p.is_empty());
        assert_eq!(p.from_version, Some(v(1)));
        assert_eq!(p.to_version, Some(v(1)));
    }

    #[test]
    fn out_of_order_is_rejected_unless_allowed() {
        let s = set(&[(1, true), (2, true), (3, true)]);
        let ledger = applied(&s, &[1, 3]);
        assert_eq!(
            plan(&s, &ledger, &up(), Policy::default()),
            Err(PlanError::OutOfOrder(vec!["2".into()]))
        );
        let p = plan(&s, &ledger, &up(), ALLOW_ALL).unwrap();
        assert_eq!(versions(&p), vs(&[(U, 2)]));
        // down is never blocked by out-of-order pending files
        let p = plan(&s, &ledger, &down(None, None), ALLOW_ALL).unwrap();
        assert_eq!(versions(&p), vs(&[(D, 3)]));
        let p = plan(
            &s,
            &ledger,
            &down(None, None),
            Policy {
                allow_down: true,
                allow_out_of_order: false,
            },
        )
        .unwrap();
        assert_eq!(versions(&p), vs(&[(D, 3)]));
    }

    #[test]
    fn checksum_mismatch_blocks_every_run() {
        let s = set(&[(1, true), (2, true)]);
        let mut ledger = applied(&s, &[1]);
        ledger[0].checksum = "edited".into();
        for req in [up(), down(None, None), Request::Goto { target: v(2) }] {
            assert_eq!(
                plan(&s, &ledger, &req, ALLOW_ALL),
                Err(PlanError::ChecksumMismatch(vec!["1".into()])),
                "{req:?}"
            );
        }
    }

    #[test]
    fn failed_rows_block_runs() {
        let s = set(&[(1, true)]);
        let ledger = vec![row(1, Kind::Versioned, Status::Failed, "x")];
        assert!(matches!(
            plan(&s, &ledger, &up(), ALLOW_ALL),
            Err(PlanError::Unresolved { .. })
        ));
    }

    #[test]
    fn missing_file_only_warns_for_up_but_blocks_down() {
        let s = set(&[(2, true)]);
        let mut ledger = vec![row(1, Kind::Versioned, Status::Applied, "gone")];
        ledger.extend(applied(&s, &[2]));
        let p = plan(&s, &ledger, &up(), ALLOW_ALL).unwrap();
        assert_eq!(
            p.warnings,
            vec![PlanWarning::MissingFile {
                version: "1".into()
            }]
        );
        assert_eq!(
            plan(&s, &ledger, &down(None, Some(0)), ALLOW_ALL),
            Err(PlanError::MissingFileForDown("1".into()))
        );
    }

    #[test]
    fn down_defaults_to_one_step_and_respects_limits() {
        let s = set(&[(1, true), (2, true), (3, true)]);
        let ledger = applied(&s, &[1, 2, 3]);
        let p = plan(&s, &ledger, &down(None, None), ALLOW_ALL).unwrap();
        assert_eq!(versions(&p), vs(&[(D, 3)]));
        assert_eq!(p.to_version, Some(v(2)));
        let p = plan(&s, &ledger, &down(Some(2), None), ALLOW_ALL).unwrap();
        assert_eq!(versions(&p), vs(&[(D, 3), (D, 2)]));
        let p = plan(&s, &ledger, &down(None, Some(1)), ALLOW_ALL).unwrap();
        assert_eq!(versions(&p), vs(&[(D, 3), (D, 2)]));
        let p = plan(&s, &ledger, &down(None, Some(0)), ALLOW_ALL).unwrap();
        assert_eq!(versions(&p), vs(&[(D, 3), (D, 2), (D, 1)]));
        assert_eq!(p.to_version, None);
        assert_eq!(p.steps[0].script, "V3__m3.down.sql");
        assert_eq!(p.steps[0].script_name, "V3__m3.up.sql");
    }

    #[test]
    fn down_is_validated_whole_before_running() {
        let s = set(&[(1, true), (2, false), (3, true)]);
        let ledger = applied(&s, &[1, 2, 3]);
        assert_eq!(
            plan(&s, &ledger, &down(Some(3), None), ALLOW_ALL),
            Err(PlanError::Irreversible("2".into()))
        );
    }

    #[test]
    fn down_requires_allow_down() {
        let s = set(&[(1, true)]);
        let ledger = applied(&s, &[1]);
        assert_eq!(
            plan(&s, &ledger, &down(None, None), Policy::default()),
            Err(PlanError::DownNotAllowed)
        );
        // nothing to revert is not an error
        assert!(
            plan(&s, &[], &down(None, None), Policy::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn baseline_covers_older_files_and_is_a_floor() {
        let s = set(&[(1, false), (5, true), (7, true)]);
        let mut ledger = vec![row(5, Kind::Baseline, Status::Applied, "")];
        let p = plan(&s, &ledger, &up(), Policy::default()).unwrap();
        assert_eq!(
            versions(&p),
            vs(&[(U, 7)]),
            "1 and 5 are covered by the baseline"
        );
        assert_eq!(p.from_version, Some(v(5)));

        ledger.extend(applied(&s, &[7]));
        assert_eq!(
            versions(&plan(&s, &ledger, &down(None, None), ALLOW_ALL).unwrap()),
            vs(&[(D, 7)])
        );
        assert_eq!(
            plan(&s, &ledger, &down(Some(2), None), ALLOW_ALL),
            Err(PlanError::BelowBaseline("5".into()))
        );
        assert_eq!(
            plan(&s, &ledger, &Request::Goto { target: v(1) }, ALLOW_ALL),
            Err(PlanError::BelowBaseline("5".into()))
        );
    }

    #[test]
    fn goto_moves_in_both_directions() {
        let s = set(&[(1, true), (2, true), (3, true), (4, true)]);
        let ledger = applied(&s, &[1, 2]);
        let p = plan(
            &s,
            &ledger,
            &Request::Goto { target: v(4) },
            Policy::default(),
        )
        .unwrap();
        assert_eq!(versions(&p), vs(&[(U, 3), (U, 4)]));
        let p = plan(&s, &ledger, &Request::Goto { target: v(1) }, ALLOW_ALL).unwrap();
        assert_eq!(versions(&p), vs(&[(D, 2)]));
        let p = plan(&s, &ledger, &Request::Goto { target: v(2) }, ALLOW_ALL).unwrap();
        assert!(p.is_empty());
        let p = plan(
            &s,
            &ledger,
            &Request::Goto {
                target: Version::zero(),
            },
            ALLOW_ALL,
        )
        .unwrap();
        assert_eq!(versions(&p), vs(&[(D, 2), (D, 1)]));
        assert_eq!(
            plan(&s, &ledger, &Request::Goto { target: v(9) }, ALLOW_ALL),
            Err(PlanError::UnknownTarget("9".into()))
        );
        assert_eq!(
            plan(
                &s,
                &ledger,
                &Request::Goto { target: v(1) },
                Policy::default()
            ),
            Err(PlanError::DownNotAllowed)
        );
    }

    #[test]
    fn goto_can_mix_downs_and_out_of_order_ups() {
        let s = set(&[(1, true), (2, true), (3, true)]);
        let ledger = applied(&s, &[1, 3]); // 2 is pending and out of order
        let p = plan(&s, &ledger, &Request::Goto { target: v(2) }, ALLOW_ALL).unwrap();
        assert_eq!(versions(&p), vs(&[(D, 3), (U, 2)]));
        // after reverting 3, applying 2 is in order relative to the new top (1)
        let strict = Policy {
            allow_down: true,
            allow_out_of_order: false,
        };
        assert_eq!(
            versions(&plan(&s, &ledger, &Request::Goto { target: v(2) }, strict).unwrap()),
            vs(&[(D, 3), (U, 2)])
        );
    }

    #[test]
    fn fingerprint_changes_with_plan_content() {
        let s = set(&[(1, true), (2, true)]);
        let a = plan(&s, &[], &up(), Policy::default()).unwrap();
        let b = plan(
            &s,
            &[],
            &Request::Up {
                to: Some(v(1)),
                steps: None,
            },
            Policy::default(),
        )
        .unwrap();
        let again = plan(&s, &[], &up(), Policy::default()).unwrap();
        assert_ne!(a.fingerprint, b.fingerprint);
        assert_eq!(a.fingerprint, again.fingerprint);
        let after_one = plan(&s, &applied(&s, &[1]), &up(), Policy::default()).unwrap();
        assert_ne!(after_one.fingerprint, a.fingerprint);
    }

    #[test]
    fn status_reports_every_state() {
        let s = set(&[(1, true), (2, true), (3, true), (5, true), (6, false)]);
        let mut ledger = applied(&s, &[1, 3]);
        ledger[1].checksum = "edited".into(); // 3 mismatched
        ledger.push(row(4, Kind::Versioned, Status::Applied, "gone")); // missing file
        ledger.push(row(5, Kind::Versioned, Status::Failed, "x"));
        let report = status(&s, &ledger).unwrap();
        let states: Vec<_> = report
            .entries
            .iter()
            .map(|e| (e.version.as_str(), e.state))
            .collect();
        assert_eq!(
            states,
            vec![
                ("1", MigrationState::Applied),
                ("2", MigrationState::OutOfOrder),
                ("3", MigrationState::ChecksumMismatch),
                ("4", MigrationState::MissingFile),
                ("5", MigrationState::Failed),
                ("6", MigrationState::Pending),
            ]
        );
        assert_eq!(report.database_version, Some(v(4)));
        assert_eq!(report.pending(), 2);
        assert!(report.has_problems());
        assert!(report.entries[5].irreversible);
    }

    #[test]
    fn repeatable_rows_never_count_as_database_version() {
        let s = set(&[(1, true)]);
        let mut ledger = applied(&s, &[1]);
        let mut r = row(0, Kind::Repeatable, Status::Applied, "c");
        r.version = "R__dbo.v".into();
        ledger.push(r);
        assert_eq!(status(&s, &ledger).unwrap().database_version, Some(v(1)));
        assert!(
            plan(&s, &ledger, &up(), Policy::default())
                .unwrap()
                .is_empty()
        );
    }

    // ---------- properties ----------

    /// Apply a plan to a ledger the way a provider would.
    fn execute(set: &MigrationSet, ledger: &mut Vec<AppliedMigration>, plan: &Plan) {
        for step in &plan.steps {
            match step.direction {
                Direction::Up => {
                    let version = step.version.version().unwrap();
                    let m = set.get(version).unwrap();
                    ledger.push(row(
                        version.value() as u32,
                        Kind::Versioned,
                        Status::Applied,
                        m.up.checksum.as_str(),
                    ));
                }
                Direction::Down => ledger.retain(|r| r.version != step.version.to_string()),
            }
        }
    }

    fn applied_set(ledger: &[AppliedMigration]) -> BTreeSet<u32> {
        ledger.iter().map(|r| r.version.parse().unwrap()).collect()
    }

    proptest::proptest! {
        #[test]
        fn up_then_down_to_zero_empties_the_ledger(versions in proptest::collection::btree_set(1u32..500, 0..20)) {
            let specs: Vec<_> = versions.iter().map(|&v| (v, true)).collect();
            let s = set(&specs);
            let mut ledger = Vec::new();
            let p = plan(&s, &ledger, &up(), Policy::default()).unwrap();
            execute(&s, &mut ledger, &p);
            proptest::prop_assert_eq!(applied_set(&ledger), versions.clone());
            let p = plan(&s, &ledger, &down(None, Some(0)), ALLOW_ALL).unwrap();
            execute(&s, &mut ledger, &p);
            proptest::prop_assert!(ledger.is_empty());
        }

        #[test]
        fn goto_reaches_exactly_the_target_state(
            versions in proptest::collection::btree_set(1u32..200, 1..15),
            start_idx in 0usize..15,
            target_idx in 0usize..16,
        ) {
            let specs: Vec<_> = versions.iter().map(|&v| (v, true)).collect();
            let s = set(&specs);
            let all: Vec<u32> = versions.iter().copied().collect();
            let start: Vec<u32> = all.iter().copied().take(start_idx.min(all.len())).collect();
            let mut ledger = applied(&s, &start);
            let target = if target_idx >= all.len() { 0 } else { all[target_idx] };
            let p = plan(&s, &ledger, &Request::Goto { target: v(target) }, ALLOW_ALL).unwrap();
            execute(&s, &mut ledger, &p);
            let expected: BTreeSet<u32> = all.iter().copied().filter(|&x| x <= target).collect();
            proptest::prop_assert_eq!(applied_set(&ledger), expected);
            // and a second goto is a no-op
            let again = plan(&s, &ledger, &Request::Goto { target: v(target) }, ALLOW_ALL).unwrap();
            proptest::prop_assert!(again.is_empty());
        }
    }

    // ---------- repeatables (SPEC §3.6) ----------

    fn with_repeatables(mut s: MigrationSet, specs: &[(&str, &str)]) -> MigrationSet {
        s.repeatables = specs
            .iter()
            .map(|(name, body)| crate::source::RepeatableMigration {
                name: (*name).to_owned(),
                script: script(&format!("R__{name}.sql"), body),
            })
            .collect();
        s.repeatables.sort_by(|a, b| a.name.cmp(&b.name));
        s
    }

    fn rep_row(name: &str, checksum_of: &str, status: Status) -> AppliedMigration {
        AppliedMigration {
            version: format!("R__{name}"),
            kind: Kind::Repeatable,
            ..row(
                0,
                Kind::Repeatable,
                status,
                Checksum::of(checksum_of.as_bytes()).as_str(),
            )
        }
    }

    fn ids(p: &Plan) -> Vec<String> {
        p.steps.iter().map(|s| s.version.to_string()).collect()
    }

    #[test]
    fn up_runs_new_repeatables_after_versioned_ones_alphabetically() {
        let s = with_repeatables(
            set(&[(1, true), (2, true)]),
            &[("b_view", "v1"), ("a_proc", "p1")],
        );
        let p = plan(&s, &[], &up(), ALLOW_ALL).unwrap();
        assert_eq!(ids(&p), ["1", "2", "R__a_proc", "R__b_view"]);
        let rep = &p.steps[2];
        assert_eq!((rep.kind, rep.direction), (Kind::Repeatable, Direction::Up));
        assert_eq!(rep.script, "R__a_proc.sql");
        assert_eq!(
            p.to_version,
            Some(v(2)),
            "repeatables don't change the database version"
        );
    }

    #[test]
    fn only_new_or_changed_repeatables_run() {
        let s = with_repeatables(set(&[(1, true)]), &[("same", "x"), ("changed", "new body")]);
        let mut ledger = applied(&s, &[1]);
        ledger.push(rep_row("same", "x", Status::Applied));
        ledger.push(rep_row("changed", "old body", Status::Applied));
        let p = plan(&s, &ledger, &up(), ALLOW_ALL).unwrap();
        assert_eq!(
            ids(&p),
            ["R__changed"],
            "a changed repeatable is not drift: it re-applies"
        );
        ledger.retain(|r| r.version != "R__changed");
        ledger.push(rep_row("changed", "new body", Status::Applied));
        assert!(plan(&s, &ledger, &up(), ALLOW_ALL).unwrap().is_empty());
    }

    #[test]
    fn repeatables_are_skipped_unless_the_run_ends_at_latest() {
        let s = with_repeatables(set(&[(1, true), (2, true), (3, true)]), &[("v", "x")]);
        // (request, ledger, expected ids, expect skipped warning)
        let cases: Vec<(Request, Vec<u32>, Vec<&str>, bool)> = vec![
            (
                Request::Up {
                    to: Some(v(2)),
                    steps: None,
                },
                vec![],
                vec!["1", "2"],
                true,
            ),
            (
                Request::Up {
                    to: None,
                    steps: Some(1),
                },
                vec![],
                vec!["1"],
                true,
            ),
            (Request::Goto { target: v(2) }, vec![], vec!["1", "2"], true),
            (
                Request::Goto { target: v(3) },
                vec![1],
                vec!["2", "3", "R__v"],
                false,
            ),
            (up(), vec![1, 2, 3], vec!["R__v"], false),
            (down(Some(1), None), vec![1, 2, 3], vec!["3"], false),
        ];
        for (request, start, expected, skipped) in cases {
            let p = plan(&s, &applied(&s, &start), &request, ALLOW_ALL).unwrap();
            assert_eq!(ids(&p), expected, "{request:?}");
            let warned = p
                .warnings
                .iter()
                .any(|w| matches!(w, PlanWarning::RepeatablesSkipped { count: 1 }));
            assert_eq!(warned, skipped, "{request:?}: {:?}", p.warnings);
        }
    }

    #[test]
    fn repeatables_alone_run_and_failed_rows_block() {
        let s = with_repeatables(MigrationSet::default(), &[("only", "x")]);
        assert_eq!(ids(&plan(&s, &[], &up(), ALLOW_ALL).unwrap()), ["R__only"]);
        let ledger = vec![rep_row("only", "x", Status::Failed)];
        assert!(matches!(
            plan(&s, &ledger, &up(), ALLOW_ALL),
            Err(PlanError::Unresolved { ref version, .. }) if version == "R__only"
        ));
    }

    #[test]
    fn status_shows_repeatable_states_after_versioned_entries() {
        let s = with_repeatables(
            set(&[(1, true)]),
            &[("a_same", "x"), ("b_changed", "new"), ("c_new", "n")],
        );
        let mut ledger = applied(&s, &[1]);
        ledger.push(rep_row("a_same", "x", Status::Applied));
        ledger.push(rep_row("b_changed", "old", Status::Applied));
        ledger.push(rep_row("z_deleted", "gone", Status::Applied));
        let r = status(&s, &ledger).unwrap();
        let got: Vec<(&str, MigrationState)> = r
            .entries
            .iter()
            .map(|e| (e.version.as_str(), e.state))
            .collect();
        assert_eq!(
            got,
            [
                ("1", MigrationState::Applied),
                ("R__a_same", MigrationState::Applied),
                ("R__b_changed", MigrationState::Changed),
                ("R__c_new", MigrationState::New),
                ("R__z_deleted", MigrationState::MissingFile),
            ]
        );
        assert_eq!(r.pending(), 2, "changed + new will run");
        assert!(!r.has_problems(), "changed repeatables are not drift");
        assert_eq!(r.database_version, Some(v(1)));
        let p = plan(&s, &ledger, &up(), ALLOW_ALL).unwrap();
        assert!(p.warnings.contains(&PlanWarning::MissingFile {
            version: "R__z_deleted".into()
        }));
    }

    #[test]
    fn fingerprint_covers_repeatable_content() {
        let a = with_repeatables(set(&[(1, true)]), &[("v", "one")]);
        let b = with_repeatables(set(&[(1, true)]), &[("v", "two")]);
        let pa = plan(&a, &[], &up(), ALLOW_ALL).unwrap();
        let pb = plan(&b, &[], &up(), ALLOW_ALL).unwrap();
        assert_ne!(pa.fingerprint, pb.fingerprint);
    }

    proptest::proptest! {
        #[test]
        fn up_runs_exactly_the_new_or_changed_repeatables(
            // name index -> (current body, ledger state: 0 none, 1 same, 2 old body)
            reps in proptest::collection::btree_map(0u8..30, (0u8..3, 0u8..3), 0..12),
        ) {
            let specs: Vec<(String, String)> =
                reps.iter().map(|(n, (b, _))| (format!("r{n:02}"), format!("body{b}"))).collect();
            let spec_refs: Vec<(&str, &str)> = specs.iter().map(|(n, b)| (n.as_str(), b.as_str())).collect();
            let s = with_repeatables(MigrationSet::default(), &spec_refs);
            let mut ledger: Vec<AppliedMigration> = Vec::new();
            let mut expected: Vec<String> = Vec::new();
            for ((name, body), (_, (_, state))) in specs.iter().zip(reps.iter()) {
                match state {
                    0 => expected.push(format!("R__{name}")),
                    1 => ledger.push(rep_row(name, body, Status::Applied)),
                    _ => {
                        ledger.push(rep_row(name, "stale", Status::Applied));
                        expected.push(format!("R__{name}"));
                    }
                }
            }
            let p = plan(&s, &ledger, &up(), ALLOW_ALL).unwrap();
            proptest::prop_assert_eq!(ids(&p), expected);
            // Apply: record each step's checksum; then nothing is left to do.
            for step in &p.steps {
                ledger.retain(|r| r.version != step.version.as_str());
                let mut r = rep_row(step.version.repeatable_name().unwrap(), "", Status::Applied);
                r.checksum = step.checksum.clone();
                ledger.push(r);
            }
            proptest::prop_assert!(plan(&s, &ledger, &up(), ALLOW_ALL).unwrap().is_empty());
        }
    }

    #[test]
    fn baseline_plan_marks_one_version_and_nothing_runs() {
        let set = set(&[(1, true), (2, true), (3, true)]);
        let mark = baseline_plan(&set, &[], &v(2), None, false).unwrap();
        assert_eq!(mark.steps.len(), 1);
        let st = &mark.steps[0];
        assert_eq!(st.kind, Kind::Baseline);
        assert_eq!(st.version.to_string(), "2");
        assert_eq!(st.checksum, set.get(&v(2)).unwrap().up.checksum.as_str());
        assert_eq!(mark.to_version, Some(v(2)));

        // After marking, versions up to the baseline count as applied and
        // `up` plans only the newer one.
        let marker = row(2, Kind::Baseline, Status::Applied, &st.checksum);
        let up = plan(&set, &[marker], &up(), Policy::default()).unwrap();
        let versions: Vec<String> = up.steps.iter().map(|s| s.version.to_string()).collect();
        assert_eq!(versions, ["3"]);
    }

    #[test]
    fn baseline_plan_refuses_a_used_ledger_or_a_missing_file() {
        let set = set(&[(1, true), (2, true)]);
        let rows = applied(&set, &[1]);
        assert_eq!(
            baseline_plan(&set, &rows, &v(2), None, false),
            Err(PlanError::LedgerNotEmpty(1))
        );
        assert_eq!(
            baseline_plan(&set, &[], &v(9), None, false),
            Err(PlanError::UnknownBaseline("9".into()))
        );
    }

    #[test]
    fn baseline_fingerprint_differs_from_the_up_plan() {
        let set = set(&[(1, true)]);
        let mark = baseline_plan(&set, &[], &v(1), None, false).unwrap();
        let up = plan(&set, &[], &up(), Policy::default()).unwrap();
        assert_ne!(mark.fingerprint, up.fingerprint);
        let renamed = baseline_plan(&set, &[], &v(1), Some("baseline"), false).unwrap();
        assert_ne!(mark.fingerprint, renamed.fingerprint);
    }
}
