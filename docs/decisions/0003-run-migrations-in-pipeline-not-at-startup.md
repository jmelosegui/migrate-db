# ADR-0003: Run migrations as a pipeline step, not at pod startup

- **Status:** Proposed
- **Date:** 2026-11-09
- **Applies to:** Services whose databases are migrated by the application itself at startup. Typical examples: EF Core `Database.Migrate()`, DbUp `DeployChanges…PerformUpgrade()` in `Program.cs`, migrate-mongo or Mongock called on boot, and Cosmos `CreateContainerIfNotExistsAsync` during initialization.

## Context

Some databases already use change-based migrations, but **the application runs them when a pod starts**. That causes several problems:
- **Replicas race each other.** Every replica tries to migrate. Correctness depends on the old tool's locking, which some tools don't have; db-migrate, for example, runs MongoDB without a lock.
- **The app identity needs DDL rights** (create, alter, drop) in every environment, prod included. This breaks least privilege, and under Entra data-plane RBAC, Cosmos container creation from the app isn't even allowed.
- **A failed migration becomes a failed rollout.** Pods crash-loop, and the failure surfaces as an availability incident instead of a failed pipeline step with a clear error.
- **There's no plan or approval step.** Nobody sees or approves what will change before it changes.
- **Every technology does it differently,** with its own journal format and tooling.

## Decision

1. **Migrations run only as a migrate-db pipeline step**, before the application deploys, using plan → approval → apply (SPEC §10.1). Applications **never** change schema at startup.
2. **Existing databases are adopted by baseline.** The live schema is captured (`baseline capture`), every environment is proven identical (`baseline compare`), and each existing environment is marked (`baseline mark`). Which tool ran the old migrations doesn't matter. The **old journal table or collection stays untouched** (read-only) for audit during the transition, and is dropped later by a normal migration. Importing old journals into the ledger is **not** part of v1, because a baseline gives the same safety at much lower cost.
3. **The application's database identity loses DDL rights** once the startup migration code is removed. The migration pipeline uses a separate identity that holds DDL rights.
4. **Schema changes must be backward compatible with the app version currently running** (expand → deploy app → contract). This was already necessary with startup migrations, because old pods keep running during a rolling update. The change is that it's now an explicit review rule.
5. The full runbook is in [docs/ADOPTION.md](../ADOPTION.md). It is the same process for every database.

## Consequences

- **Good:**
  - No startup races.
  - Least-privilege application identities.
  - Failures show up in the pipeline, not as crash-looping pods.
  - Changes are reviewed and approved before they run.
  - One ledger and workflow across services.
  - Startup gets faster.
- **Bad:**
  - Developers who relied on "run the app and the database updates itself" locally must now run `migrate-db up` against their local database. This is the same command the pipeline runs (SPEC §10.0).
  - Each service needs a pipeline stage added.
  - For a while, two journals exist side by side: the old tool's and the migrate-db ledger.
- **Reversible until the first real migration.** Re-enabling the startup migration works as long as no post-baseline migrate-db migration has been applied.
