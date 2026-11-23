# migrate-db: Architecture

## 1. Workspace layout

```
migrate-db/
  Cargo.toml                      # [workspace]
  crates/
    core/                         # migrate-db-core: engine, model, planner, checksum, discovery, config
    provider-postgres/            # migrate-db-postgres
    provider-mssql/               # migrate-db-mssql
    provider-mongo/               # migrate-db-mongo
    provider-cosmos/              # migrate-db-cosmos
    provider-memory/              # in-memory provider for engine tests (not shipped)
    conformance/                  # shared provider test-suite (dev-dependency)
    cli/                          # migrate-db binary (clap)
  docs/
```

### Modularity model

Rust has **no stable ABI**, so runtime-loaded plugins (`.dll`/`.so`) are fragile and don't fit idiomatic Rust. The tool is modular at **compile time** instead:

- Each provider is a separate crate that depends only on `core`.
- The CLI enables providers with **cargo features**: `postgres`, `mssql`, `mongo`, `cosmos`, all on by default. A team that only needs SQL Server builds `--no-default-features --features mssql` and never compiles the Mongo or Azure SDKs.
- Third parties can write a provider by implementing the `Provider` trait in their own crate and linking it into their own binary or service.
- *Future option (not v1):* an "external provider" protocol, where the CLI launches a provider executable and talks to it over JSON on stdin/stdout. That would allow plugins in any language without ABI issues.

## 2. Core crate modules

| Module | Responsibility |
|---|---|
| `config` | Loads the flat, one-database `migrate-db.toml` (ADR-0005) and combines it with the run's connection and auth. The CLI reads the run settings from flags, `MIGRATE_DB_*` variables and `--env-file`. Built on `serde` and `toml`. |
| `source` | Discovers files from a directory (or an in-memory source in tests), parses file names, pairs up and down files, reads directives. |
| `checksum` | Normalizes content and computes SHA-256 (`sha2`). |
| `model` | `Version`, `Migration`, `AppliedMigration`, `HistoryEvent`, `Direction`, `MigrationState`. |
| `planner` | Pure function from `(files, ledger, request)` to `Plan` (including its `plan_fingerprint`, used by `--expect-plan`; SPEC §10.1) or a reconciliation error. This is where most of the logic lives, and it has **no I/O**, so it is unit-testable without a database. |
| `engine` | Orchestrates connect, lock, ensure_ledger, read ledger, plan, execute, release. Also handles events, progress and cancellation. |
| `provider` | The `Provider` trait and capability flags. |
| `template` | Templates the `new` command uses for each provider. |
| `runner` | Shared **external-runner** support ([ADR-0001](decisions/0001-native-drivers-and-external-runners.md)): finds the binary and checks its version, passes credentials through env or a temp file (never argv), runs the process, enforces the timeout by killing the process tree, and captures and redacts output. Providers build the wrapper script and interpret the outcome. |
| `error` | `thiserror` error enum that maps to exit codes. |

## 3. Provider trait (draft)

```rust
#[async_trait::async_trait]          // dyn-compatible async; see note below
pub trait Provider: Send + Sync {
    /// "postgres", "mssql", ...
    fn kind(&self) -> &'static str;

    /// File extensions this provider accepts, e.g. ["sql"] or ["yaml","yml","json"].
    fn extensions(&self) -> &'static [&'static str];

    fn capabilities(&self) -> Capabilities;   // transactional_ddl, supports_lock, ...

    /// Offline, provider-specific parse/validation (GO splitting, YAML schema).
    /// Called by `validate --offline` and before execution.
    fn prepare(&self, script: &RawScript) -> Result<PreparedScript, ProviderError>;

    /// Create ledger + history storage if missing. Must be idempotent and race-safe.
    async fn ensure_ledger(&mut self) -> Result<(), ProviderError>;

    /// Acquire the exclusive target-level lock; bootstraps its own storage if needed.
    async fn acquire_lock(&mut self, opts: &LockOptions) -> Result<LockHandle, ProviderError>;
    async fn release_lock(&mut self, handle: LockHandle) -> Result<(), ProviderError>;
    async fn force_unlock(&mut self, kill_holder: bool, who: &RunIdentity) -> Result<UnlockOutcome, ProviderError>;

    /// Read-only: normalized schema model of the live database (SPEC §6.6).
    /// Core hashes and diffs it (pure code), so fingerprint and compare logic is shared.
    async fn schema_model(&mut self) -> Result<SchemaModel, ProviderError>;

    /// Read-only: script the live schema as baseline files (versioned + repeatables)
    /// plus a report of what was not scripted (principals, grants, db settings).
    async fn capture_schema(&mut self, opts: &CaptureOptions) -> Result<CapturedBaseline, ProviderError>;

    /// Read-only: is the native lock held, and by whom (SPEC §7 rule 10)?
    /// Must not acquire the lock or create any storage.
    async fn lock_status(&mut self) -> Result<LockStatus, ProviderError>;

    /// Returns an empty Vec (never an error, never creates anything) when the
    /// ledger does not exist yet, so read-only commands stay side-effect free.
    async fn read_ledger(&mut self) -> Result<Vec<AppliedMigration>, ProviderError>;

    /// Execute one migration step AND update the ledger. The provider makes this
    /// atomic when the step is transactional (script + ledger in one transaction).
    async fn execute(
        &mut self,
        step: &PlannedStep,           // migration, direction, prepared script, tx mode
    ) -> Result<StepOutcome, ProviderError>;

    /// Ledger-only operations used by baseline / repair.
    async fn write_ledger(&mut self, op: LedgerOp) -> Result<(), ProviderError>;
    async fn append_history(&mut self, ev: &HistoryEvent) -> Result<(), ProviderError>;
}

pub struct Capabilities {
    pub transactional_ddl: bool,      // Postgres: yes, MSSQL: mostly, Mongo/Cosmos: no
    pub transactional_ledger: bool,   // can script + ledger commit atomically
    pub native_lock: bool,            // advisory/app lock vs. lease document
    pub op_checkpoints: bool,         // can record per-operation progress (Mongo/Cosmos native files)
}

// PlannedStep carries `start_op` (0 normally; > 0 when resuming per SPEC §6.5)
// and `recovery: Option<RecoveryAction>` (Resume | RollbackThenReapply), both
// decided by the pure planner from the ledger row and the file's `recovery` key.
```

**Async-trait note:** native `async fn` in traits is stable, but traits that use it are not dyn-compatible (you can't write `Box<dyn Provider>`). Because the CLI picks the provider at runtime from config, we use the `async-trait` crate. The alternative is an `enum AnyProvider` with `match` dispatch, which would be fine too since the provider set is closed. Decision: `async-trait` for v1, revisit if the boxing cost ever matters. It won't matter for a migration tool.

**Executors live inside the provider.** `prepare` decides which executor each script uses: native, or an external runner chosen by extension, directive or target config. It stores that choice in `PreparedScript` together with its guarantees:

```rust
pub enum Executor { Native, External { runner: RunnerKind } }   // RunnerKind: Sqlcmd | Mongosh
pub enum Atomicity { Yes, BestEffort, No }
pub struct StepGuarantees { pub atomic_ledger: Atomicity, pub previewable: bool, pub external: bool }
```

The planner copies `StepGuarantees` into each `PlannedStep`, so `status` and `--dry-run` can show them without any I/O. `execute` routes to the native driver or to `core::runner`. The lock, the ledger reads and the outcome verification always use the provider's **native connection**, even when the script itself runs in an external process.

**Why `execute` includes the ledger write:** only the provider knows whether the script and the ledger row can share one transaction. Postgres and SQL Server can. Mongo can only on a replica set, and only for some commands. Cosmos cannot. Putting both writes behind one call keeps those rules inside the provider.

## 4. Engine pipeline

```
load config ─► discover files ─► provider.prepare() each (offline validation)
     │
     ▼
connect
     │
     ├── read-only (status | validate | --dry-run):
     │      read_ledger (missing ⇒ empty) ─► planner::plan ─► print ─► exit 0 / 2
     │      (no lock, no ensure_ledger: never writes to the database)
     ▼
acquire_lock ─► ensure_ledger ─► read_ledger
     │
     ▼
planner::plan(files, ledger, request)  ──(error)──► release, exit 2
     │
     ▼
for step in plan:  provider.execute(step)  ──(error)──► record, release, exit 4
     │
     ▼
release_lock ─► report (text | json)
```

Notes:
- The engine takes the lock *before* `ensure_ledger`, so two first-time runs can't race when creating the ledger. For document databases, the lock document lives in its own small collection or container (`__migrations_lock`), separate from the ledger, and `acquire_lock` bootstraps it.
- Dry-run output can differ from the real run only if another process migrates between the two. The real run re-plans under the lock.
- Each step's progress is reported through an `EngineObserver` trait. The CLI renders it with `indicatif` and tracing. Embedders can hook their own logging.

## 5. Key dependencies (candidates; versions pinned during Phase 0)

| Concern | Crate |
|---|---|
| Async runtime | `tokio` |
| CLI | `clap` (derive) |
| `init` prompts / config edits | `inquire` (or `dialoguer`); `toml_edit` to extend configs while preserving comments |
| Errors | `thiserror` (libs), `anyhow` (CLI only) |
| Config | `serde`, `toml`, `figment` |
| Logging | `tracing`, `tracing-subscriber` |
| Hashing | `sha2`, `hex` |
| Time | `time` or `chrono` |
| YAML/JSON | `serde_yaml_ng` or `serde_norway` (the original `serde_yaml` is unmaintained), `serde_json` |
| Entra ID | `azure_identity` |
| Postgres | `tokio-postgres` (+ `tokio-postgres-rustls`), or `sqlx` |
| SQL Server | `mssql-tds` ([ADR-0004](decisions/0004-mssql-driver-mssql-tds.md)) |
| MongoDB | `mongodb` (official) |
| Cosmos DB | `azure_data_cosmos` (official, preview) + ARM REST via `reqwest` for control plane |
| Process runner | `tokio::process` (+ job objects on Windows / process groups on Unix to kill the process tree) |
| JS syntax check (mongosh files) | `oxc_parser` or `swc_ecma_parser` (syntax only; nothing is executed) |
| Tests | `testcontainers`, `insta` (snapshot tests for plans and output), `assert_cmd` |

## 6. Testing strategy

1. **Planner unit tests** (pure, fast): every reconciliation rule and every up/down/to/steps combination. Use table-driven and property tests (`proptest`), for example checking that `up` followed by `down --to 0` always gives back an empty ledger.
2. **Engine tests** run against `provider-memory`, including injected failures, lock contention and cancellation.
3. **Provider conformance suite** (`crates/conformance`): one generic test suite that every provider must pass. It covers the ledger lifecycle, read-only operations creating nothing, lock exclusivity across two connections, the holder record and stale detection, lock-wait reporting, `force_unlock` (including never killing an unverified session), atomic rollback, failed-state handling, repeatables and cancellation. A provider test crate implements `Harness` (connect to an isolated target, a handful of dialect snippets, an independent assertion connection) and expands `conformance_tests!`, which generates one test per case. Each provider crate runs it against a real database in a container (`tests/conformance.rs`, `--features integration`). Dialect specifics stay in the provider's own integration tests.
4. **Integration containers:** `postgres:17`, `mcr.microsoft.com/mssql/server:2022-latest`, `mongo:8` as a single-node replica set (needed for transactions), and the Cosmos DB Linux emulator (vNext). The emulator has gaps, so Cosmos tests also have an opt-in job against a real dev account.
5. **CLI tests:** `assert_cmd` plus `insta` snapshots of text and JSON output.
6. **External-runner tests:**
   - `core::runner` is tested against a fake runner binary built in the workspace. It covers exit codes, timeouts, stdout floods and secret redaction.
   - The real `sqlcmd` and `mongosh` executors run the conformance suite in CI images where those CLIs are installed and version-pinned.
   - The suite asserts each executor's declared `StepGuarantees`. For example, a sqlcmd transactional step that fails must leave no ledger row.
