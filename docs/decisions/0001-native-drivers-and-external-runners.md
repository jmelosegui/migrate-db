# ADR-0001: Native drivers by default, external runners by exception

- **Status:** Accepted
- **Date:** 2026-11-09
- **Applies to:** every provider. It is most visible in SQL Server (`sqlcmd`) and MongoDB (`mongosh`).

## Context

A provider has to execute migration scripts against its database. There are two ways to do that:

1. **Native executor:** a Rust driver linked into the binary talks the database's wire protocol directly (`tiberius`, `tokio-postgres`, `mongodb`, `azure_data_cosmos`).
2. **External runner:** the engine launches the database's own CLI (`sqlcmd`, `mongosh`) as a child process for each script.

In early design discussions the external runner was rejected for SQL Server but proposed for MongoDB. Mechanically these are the same choice, and that inconsistency was pointed out. This ADR replaces the ad-hoc judgement with a rule.

What each choice costs and buys differs by database:

| | SQL Server → `sqlcmd` | MongoDB → `mongosh` |
|---|---|---|
| What the external runner **costs** | The native path gets atomic *script + ledger* in one transaction for free, because SQL Server has transactional DDL. A runner can only get that with a generated wrapper (see below) and only on a best-effort basis. It also adds a CLI dependency on every agent. | Very little atomicity is lost, because Mongo DDL is mostly non-transactional and the native path also writes the ledger after the commands. It adds a CLI dependency. |
| What the external runner **buys** | Nothing for ordinary T-SQL, because the native driver runs the same `.sql` files. It only adds sqlcmd-specific features: `:r` includes, `:setvar` / `$(var)`, `:on error`. | **The authoring language itself.** The server can't execute JavaScript (`eval` was removed in 4.2) and the Rust driver has no JS runtime. Without mongosh, there are no JavaScript migrations. |

## Decision

1. **Native executor is the default** for every provider.
2. **An external runner is allowed only when it provides something the native executor cannot.** For example, a scripting language (mongosh JavaScript) or client-side script features that existing scripts depend on (sqlcmd `:r` / `:setvar`). Convenience and familiarity alone are not enough.
3. **The engine stays in charge.** With an external runner, the Rust engine still owns the lock, planning, checksums, the ledger and the history log. The runner only executes the body of one script.
4. **Reduced guarantees are visible, never implied.** Every step carries `StepGuarantees` (`atomic_ledger`, `previewable`, `external`). `status`, `--dry-run` and the JSON output show them, and the ledger records which `executor` and `runner_version` applied each migration.
5. **External runners follow one shared implementation** in `migrate-db-core::runner`. That module covers:
   - finding the binary;
   - checking the minimum version;
   - passing credentials **only through environment variables or a short-lived, permission-restricted file, never through argv**;
   - enforcing timeouts by killing the process tree;
   - capturing and redacting output.

   Exit codes are interpreted by the provider.
6. **Truth comes from the ledger, not the exit code.** After a runner exits, the engine re-reads the ledger to decide the outcome where that's possible. The details are in each provider's section of PROVIDERS.md.

### Resulting executors

| Provider | Native executor (default) | External runner (opt-in) | How it is selected |
|---|---|---|---|
| PostgreSQL | `tokio-postgres` | none planned (`psql` adds nothing v1 needs) | n/a |
| SQL Server | `tiberius` | `sqlcmd` (go-sqlcmd first) | Target setting `executor = "sqlcmd"`, or a per-file `-- migrate:executor sqlcmd` directive |
| MongoDB | `mongodb` crate (`.yaml` / `.json` runCommand files) | `mongosh` (`.js` files) | **File extension.** Both kinds can be mixed in one target. |
| Cosmos DB NoSQL | `azure_data_cosmos` + ARM REST | none (no Cosmos CLI with equivalent semantics) | n/a |

## Consequences

- **Good:** one rule explains every executor choice. Teams can author Mongo migrations in JavaScript from day one, and adopt existing sqlcmd-dependent SQL scripts without rewriting them. The engine, ledger and lock behavior are identical whichever executor runs a script.
- **Bad:** targets that use external runners need the CLI installed and version-pinned on every agent. `validate` and `--dry-run` are weaker for those scripts (syntax and checksum checks only). The runner adds process-startup overhead (about a second per migration), which is acceptable at migration volumes.
- **Follow-ups:**
  - Phase 0 spikes confirm mongosh exit-code behavior and `connect()` from an environment variable.
  - They also confirm go-sqlcmd flags, `SQLCMDPASSWORD`, and Entra (`--authentication-method ActiveDirectoryDefault`) support.
  - Legacy ODBC `sqlcmd` support is deferred.
