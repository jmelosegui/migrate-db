# migrate-db

A database migration tool written in Rust. It versions database changes as script files, applies them **up** or rolls them **down**, and keeps a **ledger** inside each target database. The ledger records which scripts ran and what version the database is at.

There is one engine, and each database type plugs in as a provider:

| Provider | Script format | Executors | Status |
|---|---|---|---|
| Microsoft SQL Server / Azure SQL | `.sql` (with `GO` batches) | native; `sqlcmd` opt-in | Working: SQL logins and Entra ID (az login, managed / workload identity); sqlcmd in Phase 3 |
| PostgreSQL | `.sql` | native | Working (built first to prove the engine); TLS and Entra in Phase 2 |
| MongoDB | `.js` (mongosh) or `.yaml` / `.json` (runCommand) | `mongosh` for `.js`; native for YAML/JSON | Planned for Phase 3 |
| Azure Cosmos DB (NoSQL API) | `.yaml` / `.json` (declarative operations) | native | Planned for Phase 4 |

Scripts run through each database's **native Rust driver** by default. A database's own CLI is used only when it provides something the driver can't (see [ADR-0001](docs/decisions/0001-native-drivers-and-external-runners.md)).

## Documents

- [docs/SPEC.md](docs/SPEC.md): functional spec. Covers CLI commands, migration files, the ledger, locking, config and exit codes.
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): crate layout, the `Provider` trait and the engine pipeline.
- [docs/PROVIDERS.md](docs/PROVIDERS.md): per-provider design. Covers drivers, script semantics, transactions, locks and ledger storage.
- [docs/PLAN.md](docs/PLAN.md): phased delivery plan with exit criteria, plus open questions.
- [docs/ADOPTION.md](docs/ADOPTION.md): how an existing database starts using migrate-db, whatever deploys them today: freeze, capture a baseline from the live database, compare every environment, mark, switch on.
- [docs/decisions/](docs/decisions/): architecture decision records (ADRs).
- [AGENTS.md](AGENTS.md): instructions for AI coding agents and contributors.

## Quick taste (target UX)

One folder holds one database. Its `migrate-db.toml` says what the database is; each run says where to connect, normally through `MIGRATE_DB_*` variables set by the pipeline stage ([ADR-0005](docs/decisions/0005-one-database-per-config-environment-agnostic.md)).

```powershell
migrate-db init                          # guided setup; detects existing tooling, writes migrate-db.toml
migrate-db new "create users table"      # -> migrations/V20261109142530__create_users_table.{up,down}.sql
$env:MIGRATE_DB_CONNECTION = "Server=tcp:localhost,1433;Database=app;User ID=…;Password=…"
migrate-db status
migrate-db up --dry-run
migrate-db up
migrate-db down --steps 1 --allow-down   # reverting is off unless the run allows it
```
