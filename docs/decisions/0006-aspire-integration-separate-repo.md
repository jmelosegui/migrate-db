# ADR-0006: Aspire integration lives in a separate repository and uses only the public CLI contract

- **Status:** Accepted
- **Date:** 2026-11-22
- **Applies to:** the `migrate-db-aspire` repository (`Aspire.Hosting.MigrateDb`), SPEC §4 (CLI, exit codes, JSON output).

## Context

Teams that develop with .NET Aspire want to run and inspect migrations from the AppHost and its dashboard. The dashboard has no plugin API. The supported extension points are AppHost resources and custom resource commands (`WithCommand`).

This repository is a Rust workspace. An Aspire integration is a .NET package with its own SDK, release cadence (Aspire releases often, and `Aspire.Hosting` 17 targets net10.0 only) and audience.

## Decision

- The integration is the separate repository `migrate-db-aspire`, published as `Aspire.Hosting.MigrateDb`.
- It talks to migrate-db **only through the CLI**: flags, `--output json`, exit codes and `MIGRATE_DB_*` variables. Connection strings are passed as environment variables, never in argv.
- It runs `migrate-db up` as an AppHost resource, and exposes Status, Plan, Validate, Apply and (opt-in, with confirmation) Unlock as dashboard commands. Status reads `status --output json` (`schema: 1`: `status.database_version`, `status.entries[].state`, `lock.state`).
- This is a local development convenience. It does not change [ADR-0003](0003-run-migrations-in-pipeline-not-at-startup.md): production and CI still run the CLI as a pipeline step, with the same commands and flags (SPEC §10.0).

## Consequences

- The JSON output schema and exit codes (architecture rule 7) now have a second consumer. Changing them needs an ADR and a version bump, and the Aspire package must be updated together with it.
- No Rust crate depends on .NET, and `core` is unaffected.
- If the integration needs data the CLI doesn't print, extend the CLI contract here first; don't read the ledger from the package.
