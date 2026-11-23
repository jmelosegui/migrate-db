# AGENTS.md

Instructions for AI coding agents (Claude Code, Copilot, Codex, …) and human contributors working in this repository.

## Project status

**Phase 1 in progress.** The core engine, CLI and PostgreSQL provider exist (see the ticked items in [docs/PLAN.md](docs/PLAN.md)). SQL Server is the first production provider and is being built next. Postgres was built first only to prove the engine. The documents under `docs/` are the source of truth for behavior. If an instruction here and a document disagree, the document wins, and you should fix this file.

## How the tool is used

migrate-db is a **CLI that runs as a CI/CD pipeline step** (Azure DevOps / GitHub Actions), separately from application deployment. It does **not** run at app or pod startup. When making design trade-offs, optimize for:
- unattended runs: no prompts, and `--yes` for destructive commands;
- machine-readable output and stable exit codes;
- plan/apply with `--expect-plan`;
- Entra auth through pipeline identities;
- clean behavior when a run is cancelled.

The library API is secondary (SPEC §8).

**Local/pipeline parity is a hard rule (SPEC §10.0).** Never add behavior that exists only in CI or only on a developer machine. Differences may only be presentational or interactive, and each must have a flag (`--ci`, `--log-format`, `--output`) so the pipeline's behavior can be reproduced locally.

## Read before changing anything

| Doc | What it defines |
|---|---|
| [docs/SPEC.md](docs/SPEC.md) | Behavior: CLI, file format, ledger, locking, config, exit codes. **Public contract.** |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Crates, the `Provider` trait, engine pipeline, testing strategy |
| [docs/PROVIDERS.md](docs/PROVIDERS.md) | Per-database drivers, executors, transactions, locks, ledger storage |
| [docs/PLAN.md](docs/PLAN.md) | Phases, exit criteria, open questions |
| [docs/ADOPTION.md](docs/ADOPTION.md) | How existing databases are adopted: one tool-agnostic baseline process (capture → compare → mark) |
| [docs/decisions/](docs/decisions/) | ADRs. Accepted ADRs are binding. Proposed ADRs show intended direction. |

## Workspace layout (target)

```
crates/core               migrate-db-core       engine, planner, model, config, checksum, runner
crates/provider-postgres  migrate-db-postgres
crates/provider-mssql     migrate-db-mssql
crates/provider-mongo     migrate-db-mongo
crates/provider-cosmos    migrate-db-cosmos
crates/provider-memory    in-memory provider for engine tests (not published)
crates/conformance        shared provider test suite (dev-dependency)
crates/cli                migrate-db binary
```

## Commands

```sh
cargo build --workspace
cargo test --workspace                                    # unit, engine and CLI tests; no database needed
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo clippy -p migrate-db --no-default-features --all-targets -- -D warnings   # build without providers
cargo fmt --all -- --check
```

Integration tests need **disposable** databases: SQL Server and PostgreSQL. Podman is the local container runtime:

```powershell
podman run -d --name migrate-db-mssql -e ACCEPT_EULA=Y -e MSSQL_SA_PASSWORD=Migrate-test-1 -p 51433:1433 mcr.microsoft.com/mssql/server:2022-latest
podman exec -e SQLCMDPASSWORD=Migrate-test-1 migrate-db-mssql /opt/mssql-tools18/bin/sqlcmd -C -S localhost -U sa -b -Q "IF DB_ID('migrate_test') IS NULL CREATE DATABASE migrate_test"
$env:MIGRATE_DB_TEST_MSSQL = "Server=tcp:localhost,51433;Database=migrate_test;User ID=sa;Password=Migrate-test-1;TrustServerCertificate=true"

podman run -d --name migrate-db-pg -e POSTGRES_PASSWORD=migrate-test -e POSTGRES_DB=migrate_test -p 55432:5432 docker.io/library/postgres:17
$env:MIGRATE_DB_TEST_POSTGRES = "host=localhost port=55432 user=postgres password=migrate-test dbname=migrate_test"

cargo test --workspace --all-features                     # includes provider + CLI end-to-end tests (needs both)
cargo test -p migrate-db-mssql --features integration      # one provider at a time, including its conformance suite (tests/conformance.rs)
```

Opt-in cloud test (read-only, never run by default): after `az login`, set `MIGRATE_DB_TEST_AZURE_SQL` to an Entra connection string for a dev database you're allowed to read, for example `Server=tcp:<server>.database.windows.net,1433;Database=<db>;Authentication=ActiveDirectoryDefault`.

Every change must pass `fmt`, `clippy -D warnings` and the unit tests. If you touch a provider, also run that provider's integration tests, or say clearly that you couldn't (for example because Docker isn't available).

The primary dev platform is Windows (PowerShell). CI runs on Windows and Linux. Don't write scripts that assume a POSIX shell.

## Architecture rules (do not break without an ADR)

1. **Dependency direction:** `core` has **no database drivers**. Providers depend on `core`, never on each other. Only `cli` knows every provider, and it selects them through cargo features.
2. **The planner is pure.** No I/O, no async, no reading the clock: pass `now` in. All reconciliation logic lives there and is unit-tested.
3. **Providers own atomicity.** `execute` runs the script **and** updates the ledger, so the provider can commit both together where the database allows it.
4. **Read-only commands never write.** `status`, `validate` and `--dry-run` take no lock, don't call `ensure_ledger`, and treat a missing ledger as empty.
5. **Native executor by default.** An external runner (`sqlcmd`, `mongosh`) is allowed only under [ADR-0001](docs/decisions/0001-native-drivers-and-external-runners.md):
   - the engine still owns the lock, plan, checksums and ledger;
   - credentials **never** go in argv;
   - the outcome is verified from the ledger where possible, and the design fails closed;
   - the reduced guarantees are reported in `StepGuarantees`.

   Never shell out to a database CLI outside `core::runner`.
6. **No runtime plugin loading** (dylib/`libloading`). Modularity is crates plus cargo features.
7. **Compatibility contracts.** Changing any of these needs an ADR and a migration path:
   - checksum normalization (changing it invalidates every existing ledger);
   - ledger schema, which needs a `ledger_schema_version` bump and a forward upgrade in `ensure_ledger`;
   - exit codes;
   - the JSON output schema;
   - the migration file naming pattern.

## Coding conventions

- Rust 2024 edition. The toolchain is pinned in `rust-toolchain.toml` (Phase 0).
- `#![forbid(unsafe_code)]` in every crate.
- Errors: `thiserror` in libraries, `anyhow` **only** in `cli`. Library errors map to exit codes in one place (`core::error`).
- No `unwrap()`, `expect()` or `panic!` in library code, except in tests or for a documented invariant (`// INVARIANT: …`).
- Async: `tokio`. The `Provider` trait uses `async-trait` because it must be dyn-compatible.
- Logging: `tracing`. **Never log connection strings, tokens, passwords or full runner environments.** Use the redaction helper in `core`.
- Public items get doc comments. Doc examples must compile.
- New dependencies need a reason in the PR or commit message. Prefer maintained crates. `serde_yaml` is unmaintained, so use the replacement chosen in Phase 0.
- Line endings: the repo is LF (`.gitattributes`, added in Phase 0). Anything that reads migration files must be tested with CRLF and BOM input.

## Testing expectations

- **Planner change:** table-driven unit tests, plus a `proptest` property where it fits.
- **Provider change:** must pass `crates/conformance` against a real database container.
- **Executor change:** must assert its declared `StepGuarantees` in the conformance suite.
- **CLI output change:** review `insta` snapshots deliberately (`cargo insta review`). Never accept them blindly.
- **Bug fix:** write the failing regression test first.
- Never weaken a lock, checksum or reconciliation check to make a test pass.

## Git workflow

- **Remote: `origin` = github.com/jmelosegui/migrate-db (private, added 2026-11-22).** Commit directly to `main`. Push only when the user asks. Branches, PRs and branch policies will be revisited later.
- **Commit every unit of progress.** Each coherent change (a doc update, a spec section, a feature, a fix) gets its own commit as soon as it's done. Don't let work pile up uncommitted.
- GPG signing is **not required**. If signing is configured globally, commit with `git -c commit.gpgsign=false commit …`.
- Use Conventional Commit messages (`docs:`, `feat:`, `fix:`, `test:`, `chore:`, `refactor:`), with an optional scope such as `feat(mssql): …`.
- Never rewrite published history, force-push or delete branches without the user's explicit approval.

## Docs are part of the change

- If behavior changes, update `docs/SPEC.md` in the same change.
- For an architectural or hard-to-reverse decision, add `docs/decisions/NNNN-kebab-title.md` (Status, Context, Decision, Consequences). Don't rewrite accepted ADRs; supersede them with a new one.
- When a plan item lands, tick it in `docs/PLAN.md`.

## Safety

- Integration tests run **only** against local containers or emulators. Never point tests or ad-hoc runs at shared, dev, QA or production databases unless the user explicitly asks, and even then never run `down`, `repair` or `unlock --force` without confirmation.
- Cloud tests (a real Cosmos account, Azure SQL) are opt-in via an explicit environment variable and never run by default.
- Never commit secrets or real connection strings. Test credentials are throwaway values read from env.
