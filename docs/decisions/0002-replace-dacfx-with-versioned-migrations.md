# ADR-0002: Replace DacFx (SQL Database Projects) with versioned migrations

- **Status:** Proposed
- **Date:** 2026-11-09
- **Applies to:** SQL Server / Azure SQL databases that are currently deployed from `.sqlproj` / dacpac with SqlPackage.

## Context

Many teams develop SQL Server databases with SQL Database Projects (DacFx). DacFx is **state-based**: the project declares the desired schema, and at deploy time SqlPackage compares the dacpac with the target and generates the change script on the fly. The other database technologies (Postgres, MongoDB, Cosmos DB) have no DacFx equivalent, and they already use, or will use, **change-based** migrations. migrate-db provides a single change-based workflow for all four.

**What DacFx gives us today:**
- build-time reference validation;
- one file per object;
- easy maintenance of procedures, views and functions;
- automatic dependency ordering;
- drift reconciliation;
- mature Visual Studio / VS Code tooling.

**What it costs us:**
- **Non-deterministic deploys:** the script that runs in prod is generated against prod and was never reviewed or tested as-is.
- **Unplanned table rebuilds:** sometimes it decides to rebuild a table, with long locks and large transactions on big tables.
- **Awkward data changes:** pre- and post-deployment scripts that keep growing.
- **No real rollback, and no ledger in the database.**
- **Surprises:** objects can be dropped unexpectedly, depending on deploy settings.
- **Heavy deploy dependency:** SqlPackage needs .NET on the agent.
- **A second, different workflow** next to every other database technology.

## Decision

1. **New schema changes to SQL Server databases are written as migrate-db versioned migrations.** For each database, from its cutover date, it is deployed only by migrate-db in the pipeline.
2. **Existing databases are adopted by baseline, not by history import.** DacFx has no per-change history to import. The **live** reference schema (normally prod) is captured with `migrate-db baseline capture` into `V<ts>__baseline.sql` plus `R__` files. Every environment is proven identical with `baseline compare`. Existing environments are marked with `baseline mark` so they skip the baseline, while new environments (local, ephemeral, new regions) run it from scratch. The `.sqlproj` is not used as the baseline source.
3. **Programmable objects keep the one-file-per-object experience** through **repeatable migrations** (`R__*.sql`, SPEC §3.6). These are split out of the baseline: one file per procedure, view or function, using `CREATE OR ALTER`.
4. **DacFx's build-time validation is replaced by `migrate-db rehearse`** (SPEC §4.1). In PR builds it applies every migration from zero to a disposable SQL Server container, optionally round-trips `down` and `up` again, and fails the PR on any error.
5. The full runbook is in [docs/ADOPTION.md](../ADOPTION.md). It is the same process for every database, whatever deployed it.
6. **The `.sqlproj` is retired.** It is kept read-only during the transition window as a reference model, then deleted. Nobody deploys it after cutover.

## Consequences

- **Good:**
  - Prod runs exactly the script that was reviewed and rehearsed.
  - Authors control *how* each change is applied: online operations, batching, expand/contract.
  - Data changes are first-class.
  - Every database has a ledger with an audit trail.
  - One workflow, CLI and pipeline pattern across SQL Server, Postgres, Mongo and Cosmos.
  - Agents no longer need SqlPackage.
- **Bad:**
  - Developers lose the "current state of a table in one file" view; table definitions now live across migration history.
  - Dependency ordering inside migrations becomes the author's job.
  - Schema-compare and refactor-log tooling goes away. The team needs a short ramp-up (`migrate-db new`, writing idempotent repeatables, writing downs).
- **Requires from the tool:**
  - the SQL Server provider (Phase 1, moved up from Phase 2 on 2026-11-17);
  - repeatable migrations and `rehearse`, both pulled into Phase 2;
  - `baseline capture` / `compare` / `mark` with schema fingerprints (SPEC §6.6). Capture uses SqlPackage once, as an external runner, and strips the `USE`, sqlcmd and `ALTER DATABASE` lines.
- **Reversible until the first real migration.** The cutover itself only writes ledger rows, so a database can go back to DacFx deploys until its first post-baseline migrate-db migration is applied. That migration is the point of no return (see ADOPTION.md).
