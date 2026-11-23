# migrate-db: Delivery Plan

The phases are ordered by **risk and value**. The SQL providers prove the engine, and the NoSQL providers come last because their drivers and semantics are the least certain. Each phase ends with a usable, tested binary.

## Phase 0: Decisions and spikes

Goal: answer the open questions and remove driver risk before writing the engine.

- [ ] Resolve the [open questions](#open-questions) below.
- [x] **MSSQL spike:** container and Azure SQL + Entra (2026-11-17). `mssql-tds` was picked over `tiberius` / `tiberius-ng` ([ADR-0004](decisions/0004-mssql-driver-mssql-tds.md), Accepted). *Still open: managed and workload identity in a pipeline, a Linux OpenSSL build (both need CI).*
- [ ] **Cosmos spike:** with `azure_data_cosmos` (latest), create/read/replace an item with ETag and run a cross-partition query plus patch. Confirm that container creation is rejected under Entra data-plane RBAC, then make it work through ARM REST. Check how well the Linux emulator works in CI.
- [ ] **External-runner spike** ([ADR-0001](decisions/0001-native-drivers-and-external-runners.md)):
  - mongosh: confirm `--nodb --file` with `connect(process.env.MIGRATE_DB_URI)`, and that an uncaught error exits non-zero.
  - go-sqlcmd: confirm `-b -I -i`, `SQLCMDPASSWORD`, `--authentication-method ActiveDirectoryDefault`, and that `:r` works inside a wrapper transaction.
  - Both: confirm that killing the process tree works on Windows and Linux.
- [x] Pin the toolchain (`rust-toolchain.toml`, 1.99.1, edition 2024). *Still open: crate naming. Licensed MIT.*
- [ ] Set up the repo: ~~cargo workspace skeleton, `rustfmt`/`clippy` config~~ (done), CI pipeline (Azure DevOps or GitHub Actions) with build, test and lint on Windows and Linux (*deferred: the repo is local-only*).

**Exit:** a short decision record (`docs/decisions/*.md`) for each spike, and a green CI run on an empty workspace.

## Phase 1: Core engine, CLI and SQL Server (MVP)

**Reordered (2026-11-17).** SQL Server is the first production provider, because most databases that will adopt the tool are SQL Server / Azure SQL ([ADR-0002](decisions/0002-replace-dacfx-with-versioned-migrations.md)). PostgreSQL was built first to prove the engine. It stays working and tested, but the rest of its work moves to Phase 2. The Phase 0 MSSQL spike blocks `providers/mssql`.

- [x] `core`: model, file discovery and naming, directives, checksum normalization, config loading with `${env:}`, config discovery up to the repo root (SPEC §5.1). *Still open: `MIGRATE_DB__…` env-var overrides of config keys.*
- [x] `core::planner`: pure planning and reconciliation, with full unit and property tests (versioned migrations; repeatables and §6.5 recovery come in their phases).
- [x] `core::engine`: pipeline, observer events, error-to-exit-code mapping. Ctrl-C / SIGTERM cancellation (§7 rule 7) interrupts the running statement on SQL Server and Postgres and exits 4; a connection error mid-run is reported like a script error, listing the steps that committed (exit 5).
- [x] `providers/memory` with engine tests.
- [x] `providers/mssql`: `GO` batch splitter (with tests covering strings, comments, `GO n`), transactional execute with `XACT_ABORT`, `sp_getapplock` lock, §6.1 ledger DDL (`ledger_schema_version = 1`), no-transaction directive, timeouts. Also fails closed when a script ends the provider's transaction (commit marker, PROVIDERS.md). `GO` lines are checked offline by `prepare` (`validate --offline`, and before any step of `up`/`--dry-run`). sqlcmd-only syntax (`:setvar`, `:r`, `:on error`, `$(var)`, `!!`) is rejected the same way.
- [x] MSSQL auth: SQL login and Entra (CLI, managed identity, workload identity, client secret) through one credential chain on `azure_identity` 1.0. Manual and opt-in (`MIGRATE_DB_TEST_AZURE_SQL`) checks pass against a dev Azure SQL with `az login` (2026-11-18). *Still open: verify managed and workload identity on a real pipeline agent.*
- [x] Shared `conformance` crate, run against the `mssql/server:2022` container and the existing Postgres provider. *Landed 2026-11-19: 11 contract cases, passing on both (`cargo test -p migrate-db-mssql --features integration --test conformance`, same for Postgres).*
- [x] Lock visibility (SPEC §7 rule 10): `LockHolder` record with CI run reference, `lock_status()` held test, stale-record detection, logging the holder while waiting, `unlock --force [--kill-holder]`. The conformance suite asserts these for every provider. *Done (2026-11-18) for SQL Server and Postgres: the holder record in `__migrations_meta` (written after acquiring, removed on release), `lock_status()` with stale detection (`APPLOCK_TEST` / `pg_locks`), the `lock` object in `status`, and the holder named on lock timeout. Done (2026-11-19): the engine waits in 15 s slices and logs the holder between them (provider-independent); `unlock --force [--kill-holder]` with verify-before-kill, exact-record deletes and a `force-unlock` history event. The conformance suite asserts all of it for both providers (2026-11-19).*
- [x] `cli`: `new`, `status`, `up`, `down`, `goto`, `validate` (offline and online), `--dry-run`, `--output json`. The `mssql` provider is a default cargo feature. `init` landed (below). *Still open: `rehearse`.*
- [x] Guided `init` (SPEC §5.2): prompts with a flag for each one, the non-interactive rules, read-only detection of existing tooling, extend mode (append-only, verified by parsing), `validate --offline` at the end, and an optional read-only connection test. SQL Server and Postgres only. Questions for other providers are added in their phases. The pipeline-template option arrives in Phase 5. *Interactive prompts are tested by hand only (no TTY in CI tests).*
- [x] Pipeline support (SPEC §10): `plan_fingerprint` + `--expect-plan`, `--output-file`, CI detection with non-interactive `--yes` rules, `--ci` to force CI mode locally, explicit `--env-file` (§10.0). *Still open: `--log-format auto|azure-pipelines|github`.*
- [x] Integration tests against a real SQL Server (`mssql/server:2022` container, `--features integration`), including the CLI end-to-end plan → approve → apply flow. *Still open: `insta` snapshot tests of text output.*
- [x] Engine proven on PostgreSQL first: `providers/postgres` with the full §6.1 ledger, advisory lock with holder on timeout, transactional execute, no-transaction directive, timeouts, and integration tests (Podman) including the CLI end-to-end flow. The remaining Postgres work is in Phase 2.

**Exit:** on a real SQL Server (the container, plus a manual check on Azure SQL with Entra), `new → up → status → down → up` works. Two concurrent `up` runs serialize. An edited applied script fails `validate` with exit code 2. A failing migration rolls back cleanly.

## Phase 2: Adoption features and PostgreSQL completion

- [ ] `baseline`, `repair` and `unlock` commands, which are now needed for adopting existing databases.
- [x] **Repeatable migrations** (SPEC §3.6), moved up from Phase 5 for the DacFx replacement ([ADR-0002](decisions/0002-replace-dacfx-with-versioned-migrations.md)). This covers `CREATE OR ALTER` validation warnings and recursive discovery. *Landed 2026-11-18 for SQL Server and Postgres: planner (new or changed, alphabetical, only at latest, never on down, failed rows block), `status` states, `validate` warnings (`CREATE OR ALTER` / `OR REPLACE`, `WITH SCHEMABINDING`). `rehearse --round-trip` force-apply comes with `rehearse`.*
- [ ] **`rehearse`** (SPEC §4.1) against a `mssql/server` container, including `--round-trip` and the empty-target and `scratch` safety checks.
- [ ] **`baseline capture` / `compare` / `mark`** (SPEC §6.6), the core of [ADOPTION.md](ADOPTION.md):
  - fingerprint model, hashing and diffing in `core`;
  - SQL Server capture through SqlPackage (one-time runner) plus native `sys.sql_modules` repeatables;
  - Postgres capture through `pg_dump` plus native catalog reads;
  - native fingerprints for both;
  - `mark` verifies the fingerprint before writing.
  - *`mark` landed 2026-11-22 without the fingerprint check: empty-ledger rule, baseline row with the file's checksum, `--mark-repeatables`, `--dry-run` / `--expect-plan`, `--yes` in CI, a `baseline` history event per row, all rows in one transaction (`Provider::mark`, conformance case `baseline_mark_writes_rows_and_executes_nothing`, passing on SQL Server and Postgres). Until capture/compare exist it requires `--force-unverified` (exit 2 otherwise).*
- [ ] `providers/postgres` completion: ~~TLS (rustls)~~ (done 2026-11-22: rustls with certificate and host name verification against the Mozilla roots plus the OS store, for connections and cancel requests; `sslmode` keeps its driver meaning; checked against an Azure Flexible Server, which refuses plain text), Entra tokens for Azure Flexible Server (*an access token already works as the password; the automatic credential chain is still open*), the one-statement check for no-transaction files, and passing the conformance suite (*three cancel and lock-holder cases fail locally with or without TLS; to investigate*).

**Exit:**
- The tool can baseline an existing SQL database and apply new migrations to it from a pipeline using a managed identity.
- **The adoption pilots are ready to start:** for a real SQL Server database, `baseline capture` → `compare` (in every environment) → `rehearse --scratch` → `compare` on the rehearsal database is clean.

## Phase 3: External runners and MongoDB

- [ ] `core::runner`: binary discovery and version check, credential passing through env or temp file, timeout with process-tree kill, output capture and redaction. Tested against a fake runner binary.
- [ ] `StepGuarantees` reported in `status`, `--dry-run` and the JSON output. `executor` and `runner_version` columns in the ledger.
- [ ] `providers/mongo`:
  - `.js` files through the **mongosh runner**, with `.js` syntax checks in `validate`.
  - `.yaml` / `.json` runCommand files through the **native executor**, with schema validation in `prepare`.
- [ ] Lease lock with heartbeat, ledger collections, optional transaction mode for native files.
- [ ] Mongo `baseline capture` / fingerprint (SPEC §6.6): collections, validators and indexes as native YAML.
- [ ] Operation-level progress and recovery (SPEC §6.5) for native YAML/JSON files: per-command checkpoints, the `recovery` policies, the checksum and direction guards, and resume in `--dry-run` and `plan_fingerprint`. The planner's recovery logic is unit-tested in `core`, and the conformance suite kills a run mid-file and asserts the resume.
- [ ] **sqlcmd executor** for `providers/mssql`: generated transactional wrapper, outcome verified from the ledger, the `-- migrate:executor` directive. It reuses `core::runner`, which is why it lands here and not in Phase 2.
- [ ] Conformance suite against a `mongo:8` single-node replica set, for both Mongo executors and the sqlcmd executor.
- [ ] Document the Cosmos DB for MongoDB (RU/vCore) compatibility notes.

**Exit:** using both `.js` and `.yaml` migrations, create a collection, add a validator and index, backfill a field, roll it all back, and see the ledger correct throughout. A failing transactional sqlcmd migration leaves no ledger row.

## Phase 4: Cosmos DB (NoSQL API)

- [ ] `providers/cosmos`: internal client adapter, operation set from PROVIDERS.md, ETag lease lock, ledger container.
- [ ] Control-plane routing (data plane under key auth, ARM under Entra).
- [ ] Data-transform operations with per-partition batching, 429 back-off and progress reporting.
- [ ] Operation-level checkpoints and `recovery` policies (SPEC §6.5), reusing the Phase 3 engine support.
- [ ] Cosmos `baseline capture` / fingerprint (SPEC §6.6): containers, policies, throughput, stored procedures, UDFs and triggers as native YAML.
- [ ] Conformance suite against the emulator, plus an opt-in job against a real dev account.

**Exit:** a container lifecycle (create, change indexing policy, seed, patch, roll back) works against a real account with Entra auth.

## Phase 5: Hardening and distribution

- [ ] Hooks (`before_up`, `after_each`, …) as scripts in the same folder.
- [ ] Release artifacts: signed binaries for win-x64, linux-x64/arm64 and macOS; a Docker image (optionally with `sqlcmd` and `mongosh` preinstalled at pinned versions).
- [ ] Pipeline templates: an Azure DevOps YAML template (validate → plan → approval → apply) and a GitHub Actions equivalent.
- [ ] Docs: user guide, provider guides, "writing a provider".
- [ ] Optional: external-provider protocol (JSON over stdio).

---

## Prior-art review: node-db-migrate (2026-11-09)

Reviewed [db-migrate/node-db-migrate](https://github.com/db-migrate/node-db-migrate) and [its docs](https://db-migrate.readthedocs.io/en/latest/). It is actively maintained (1.0 shipped Oct 2026; latest 1.8.1). **Official** drivers: Postgres, MySQL/MariaDB, SQLite, CockroachDB, MongoDB.
- **SQL Server:** only through the community `db-migrate-mssql` package (and a fork, `@devotis/db-migrate-mssql`, that adds transaction support). Quality is unverified.
- **Cosmos DB NoSQL:** no driver found.
- **Drift detection:** none based on checksums in the docs reviewed.
- **Locking:** none for MongoDB; it runs without a lock and prints a warning.

These items are **proposed**. They go into SPEC.md once accepted.

| # | Feature in db-migrate | Do we have it? | Proposal |
|---|---|---|---|
| P1 | **Step-level progress and resumable recovery** (v2 migrations record each step before running it; on rerun `recovery: skip` resumes and `rollback` reverts) | No. A failed non-transactional migration leaves a "dirty" state that needs a manual `repair`. | **Adopted → SPEC §6.5 (Phases 3–4).** Operation-level checkpoints with `recovery: manual \| resume \| rollback`, defaulting to `manual`. Doesn't apply to opaque `.js`/`.sql` scripts. |
| P2 | **Lock fast-path**: processes with nothing pending never touch the lock; waiters re-check what's pending after acquiring it | Partly. We re-plan under the lock, but we always take it. | **Deprioritized.** The tool runs as a pipeline step, not at pod startup, so there's no crowd of processes contending for the lock. The re-plan-under-lock part is already in place. Revisit only if the tool is ever embedded in services. |
| P3 | `status` shows lock state | No | **Adopted → SPEC §7 rule 10, `status`, `unlock --kill-holder` (Phase 1 for SQL Server and Postgres, then each provider in its own phase).** |
| P4 | `sync <version>`: go up or down to a version, choosing the direction automatically | No. Users pick `up --to` or `down --to`. | **Adopted → SPEC §4.1 `goto`, §7 rule 9 (Phase 1).** |
| P5 | `reset` | Yes, as `down --to 0` | Add a `reset` alias, guarded like `down`. |
| P6 | **Scopes**: sub-folders with their own migration sets, optionally their own DB/schema; `up:all` | Yes, as **one folder per database** (ADR-0005) | **Rejected:** one config manages one database; a pipeline runs migrate-db once per database folder. |
| P7 | **Seeds**: re-runnable data sets, separate from migrations, removable | No. Only *repeatable migrations* are planned (Phase 5). | **Consider (Phase 5):** environment-scoped seeds (for example `local`/`dev` only) for test and reference data. |
| P8 | `db:create` / `db:drop` | No | **Consider (Phase 5):** limited to environments flagged `allow_db_admin`; useful for local/CI bootstrap. |
| P9 | **Background or batched data migrations** (`work --parallel --pause --batch`), resumable | No | **Consider after v1.** This is the right home for large Cosmos/Mongo backfills that shouldn't block a deploy. |
| P10 | **Expand/contract with releases** (`deprecateColumn` … `dropDeprecated`) | No | Defer. Worth an ADR if zero-downtime deploys become a requirement. |
| P11 | `.env` loading, rc files, `DATABASE_URL` convention | Partly. We have TOML, env overrides and `--connection`. | **Adopted differently → SPEC §10.0:** explicit `--env-file <path>` only, behaving the same locally and in CI. No automatic `.env` pickup and no rc files, for parity. |
| P12 | Customizable `create` templates via plugin | Built-in templates only | Adopt: a `templates/` folder in the project overrides the built-in templates. |
| P13 | Cross-database schema DSL (`createTable`, `addColumn`, …) | No | **Reject.** It stays a non-goal. Native SQL / commands keep full fidelity, and db-migrate's own support matrix shows how uneven a DSL is across drivers. |
| P14 | Dry-run that still executes `all()` reads | n/a | Our dry-run executes nothing, and we keep it that way. |

**Where we're already ahead:** checksums and drift detection, `baseline`/`repair`, locking for Mongo (db-migrate runs Mongo without a lock), first-class SQL Server and Cosmos support, Entra ID auth, JSON output and stable exit codes, and a single binary with no runtime prerequisites.

---

## Open questions

These need a decision from the team. The recommended default is in bold.

1. **Cosmos API:** which Cosmos API do you mean? NoSQL (**assumed**), MongoDB (covered by the Mongo provider), or PostgreSQL (covered by the Postgres provider)?
2. **Mongo JavaScript style:** both formats are now supported ([ADR-0001](decisions/0001-native-drivers-and-external-runners.md)). Which one should `new` generate by default: mongosh `.js` or runCommand `.yaml`? And is anyone using **migrate-mongo / Node-driver-style** files (`async up(db)`)? Those would need a Node runner, which is out of scope for v1.
2a. **sqlcmd features:** do existing SQL Server scripts use `:r` includes or `:setvar`? If not, the sqlcmd executor can move to Phase 5.
3. **Version scheme:** **timestamp** or sequential integers?
4. **Up/down layout:** **separate `.up`/`.down` files**, or one file with `-- migrate:up` / `-- migrate:down` sections?
5. **Out-of-order migrations:** forbid by default and allow with a flag (**recommended**), or always allow?
6. ~~**Down in production:** blocked by default, or allowed with confirmation?~~ **Resolved (2026-11-18, ADR-0005):** reverting is off unless the run passes `--allow-down` / `MIGRATE_DB_ALLOW_DOWN`, plus `--yes` in CI. Pipelines turn it on only in stages where it is safe.
7. ~~**Distribution:** CLI only, or embedded in services?~~ **Resolved (2026-11-09):** CLI only. It runs as a pipeline step, independent of pod startup. The library API is secondary and has no v1 stability promise (SPEC §8).
7a. **Pipeline identity:** Azure DevOps service connections with workload identity federation? Do the target databases sit behind private endpoints, which would mean self-hosted agents?
8. ~~**Existing tooling:** import the ledger, or baseline?~~ **Resolved (2026-11-09):** **baseline, no import.** SQL Server databases often use DacFx, and other databases run migrations at pod startup. Both are adopted by baseline per [ADOPTION.md](ADOPTION.md), [ADR-0002](decisions/0002-replace-dacfx-with-versioned-migrations.md) and [ADR-0003](decisions/0003-run-migrations-in-pipeline-not-at-startup.md). **Which tool** each database used before **doesn't matter**: the baseline is captured from the live database (SPEC §6.6), and you only have to switch the old mechanism off.
9. **CI platform** for the tool's own builds and releases: Azure DevOps or GitHub Actions?
10. **Name and licensing:** is `migrate-db` final? Internal only, or open source?
