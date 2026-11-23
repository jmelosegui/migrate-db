# migrate-db: Provider Design

Every provider answers the same questions:
1. Which driver does it use?
2. What is a script?
3. How are transactions handled?
4. How does locking work?
5. Where does the ledger live?
6. **Which executors does it offer?**

**Executor rule ([ADR-0001](decisions/0001-native-drivers-and-external-runners.md)):** each provider runs scripts through its **native Rust driver** by default. It uses an **external runner** (the database's own CLI) only when the runner provides something the native path cannot. With either executor, the engine owns the lock, plan, checksums and ledger, and every step reports its `StepGuarantees` (`atomic_ledger`, `previewable`, `external`).

---

## PostgreSQL (`migrate-db-postgres`)

| Aspect | Design |
|---|---|
| Driver | `tokio-postgres` with rustls TLS. Low risk. It is the most mature async Postgres driver. (`sqlx` is an alternative if we want its pool and URL parsing.) |
| Encryption | `sslmode` in the connection string: `disable`, `prefer` (default: TLS when the server offers it) or `require` (never plain text). Whenever TLS is used the certificate chain and host name are verified, against the bundled Mozilla roots plus the operating system store; there is no switch to skip verification. Azure Flexible Server requires TLS, so use `sslmode=require` there. |
| Auth | Password, or an Entra ID access token as the password (Azure Database for PostgreSQL Flexible Server) via `azure_identity`. |
| Script | `.sql`. The whole file is sent as one multi-statement *simple query*, so client-side splitting isn't needed. |
| Transactions | Postgres DDL is transactional. Each migration runs in `BEGIN … COMMIT` **together with the ledger insert or delete**, which makes it atomic. `-- migrate:no-transaction` is required for `CREATE INDEX CONCURRENTLY`, `ALTER TYPE … ADD VALUE` (on older versions), `VACUUM`, and `CREATE DATABASE`. **Caveat:** Postgres runs a multi-statement simple query as an *implicit transaction*, so those statements still fail if the file holds more than one statement. v1 rule: **a no-transaction file must contain exactly one statement**, and `validate` enforces it with a dollar-quote, string and comment aware statement counter. Splitting statements client-side could lift this restriction later. |
| Lock | `pg_advisory_lock(<bigint hash of ledger schema.table>)`, session-level, on a dedicated connection held for the whole run. It is released automatically if the process dies. Read-only held test: `pg_locks WHERE locktype = 'advisory' AND granted` filtered to our key (`classid`/`objid` split of the bigint), visible to all roles. Holder record in `__migrations_meta` includes `pg_backend_pid()` so `unlock --force --kill-holder` can call `pg_terminate_backend(pid, 5000)` (PostgreSQL 14+), after confirming in `pg_locks` that this pid holds the lock. Terminating needs `pg_signal_backend` or the holder's role. |
| Ledger | Tables `<schema>.__migrations` and `<schema>.__migrations_history`, created with `CREATE TABLE IF NOT EXISTS`. |
| Timeout | `SET LOCAL statement_timeout`. |

## Microsoft SQL Server / Azure SQL (`migrate-db-mssql`)

| Aspect | Design |
|---|---|
| Driver | `mssql-tds`, Microsoft's Rust TDS implementation (`microsoft/mssql-rs`). It needs no ODBC driver and is pinned to an exact version behind an internal adapter ([ADR-0004](decisions/0004-mssql-driver-mssql-tds.md)). It was chosen over `tiberius` / `tiberius-ng` mainly because it can cancel a running statement server-side. Entra **access tokens** come from `azure_identity` (CLI, managed identity, workload identity). On Linux it links OpenSSL. |
| Auth | SQL login (`User ID` + `Password`), or an **Entra ID token** from one credential chain, the same locally and in pipelines: workload identity (`AZURE_FEDERATED_TOKEN_FILE`) → client secret (`AZURE_TENANT_ID` + `AZURE_CLIENT_ID` + `AZURE_CLIENT_SECRET`) → Azure CLI / Azure Developer CLI login (also `AzureCLI@2` tasks) → managed identity (`AZURE_CLIENT_ID` picks a user-assigned one; capped at 10 s). `MIGRATE_DB_AZURE_CREDENTIAL = workload-identity\|client-secret\|cli\|managed-identity` pins one source. `Authentication=ActiveDirectoryDefault`, `…Integrated`, `…Interactive` and `…WorkloadIdentity` (as SSMS and ADO.NET write them) all use the chain. `…ManagedIdentity` / `…MSI` uses only managed identity, with `User ID` as the user-assigned client ID. `…Password` / `…ServicePrincipal` are refused, because they put a secret in the string. `MIGRATE_DB_AUTH=entra` forces Entra. One token is fetched per run and reused for the lock connection. Windows-integrated (Kerberos) SQL logins are not supported. |
| Encryption | Connection-string keyword `Encrypt = Mandatory` (default: TDS 7.4 + TLS) \| `Strict` (TDS 8.0) \| `Optional`. `TrustServerCertificate` defaults to `false`. The default is **not** strict, because some Azure SQL servers behind private endpoints reset TDS 8 handshakes ([ADR-0004](decisions/0004-mssql-driver-mssql-tds.md)). |
| Script | `.sql`. **`GO` is not T-SQL.** It is a sqlcmd/SSMS batch separator. The provider splits on lines matching `^\s*GO(\s+\d+)?\s*$` (case-insensitive, ignoring `GO` inside strings and comments) and sends each batch in order. `GO n` repeats the batch n times. This is required because statements like `CREATE PROCEDURE` must be first in their batch. |
| Transactions | `BEGIN TRAN`, then every batch plus the ledger write, then `COMMIT`, all on one connection. Statements not allowed in a user transaction (`ALTER DATABASE`, `CREATE/ALTER FULLTEXT CATALOG/INDEX`, `BACKUP`, …) need `-- migrate:no-transaction`. `SET XACT_ABORT ON` is set so runtime errors doom the transaction. It does **not** cover compile errors (for example Msg 156, "incorrect syntax"), which leave the transaction open, so after any error the provider always sends `IF @@TRANCOUNT > 0 ROLLBACK` (ADR-0004). Cancellation and statement timeouts (`-- migrate:timeout`, applied per batch) send a TDS ATTENTION through the driver. **Scripts must not end the provider's transaction:** see the native executor below. |
| Lock | `sp_getapplock @Resource = 'migrate-db:<schema>.<table>', @LockMode = 'Exclusive', @LockOwner = 'Session'` on a dedicated connection. It is released when the session ends. Read-only held test: `APPLOCK_TEST('public', @Resource, 'Exclusive', 'Session')`, which needs no `VIEW DATABASE STATE`. Holder record in `__migrations_meta` includes `@@SPID` for `unlock --force --kill-holder`. Holder details come from this record, not from `sys.dm_tran_locks`, which needs `VIEW DATABASE PERFORMANCE STATE` (pipeline identities usually lack it). `KILL` needs `KILL DATABASE CONNECTION` on Azure SQL Database (`ALTER ANY CONNECTION` on SQL Server), which ordinary principals don't have. Without it, `--kill-holder` fails with a clear permission error (exit 5). Before `KILL`, the provider confirms through `sys.dm_tran_locks` that the recorded session really holds the lock, which needs `VIEW DATABASE PERFORMANCE STATE`; if it can't confirm, it kills nothing (exit 3). |
| Ledger | `[<schema>].[__migrations]`, `[<schema>].[__migrations_history]` and `[<schema>].[__migrations_meta]` (default schema `dbo`), created with `IF OBJECT_ID(...) IS NULL`. The schema is created if missing. Text columns are `nvarchar`, timestamps `datetime2(3)` in UTC. `applied_by` is `sql:<SUSER_SNAME()> / <os user@host>`. |
| Connection string | ADO.NET keywords: `Server` (`tcp:host,port`, `host,port` or `host`), `Database` (required), `User ID`, `Password`, `Encrypt`, `TrustServerCertificate`, `Connect Timeout`, `Authentication` (`SqlPassword` \| `ActiveDirectoryDefault`). Unknown keywords are errors, so a typo can't silently weaken `Encrypt`. Named instances are not supported. The target's `auth = "sql" \| "entra"` overrides `Authentication`. |
| sqlcmd syntax | The native executor does **not** support `:setvar`, `$(var)`, `:r` or `:on error`. Use `${placeholders}` instead, or switch the script or target to the `sqlcmd` executor. `validate`, `up` and `--dry-run` reject these lines (outside strings and comments) on native-executor scripts before anything runs, and say what to use instead. |

### SQL Server executors

**Native (default): `mssql-tds`, in process.** For each migration:
1. The lock connection holds `sp_getapplock` for the whole run. The server waits up to the lock timeout, so a cancelled run can stop the wait.
2. On the script connection: `SET XACT_ABORT ON; BEGIN TRAN`, and a session temp table `#migrate_db_tx` is created **inside** the transaction as a commit marker.
3. Each `GO`-separated batch is sent in order. After each batch the provider checks `@@TRANCOUNT`.
4. Before the ledger write, `@@TRANCOUNT = 1` and `XACT_STATE() = 1` must hold: the script left exactly the provider's transaction open and committable.
5. The ledger and history rows are inserted.
6. `COMMIT`.

Any error rolls everything back (`IF @@TRANCOUNT > 0 ROLLBACK`), and a `failed` history row is written in a separate transaction. Errors from any statement of a batch are reported, including statements after the first.

**A script that ends the provider's transaction fails closed.** If a script runs `COMMIT` or `ROLLBACK` at the outer level, later statements would run in autocommit mode, so the provider stops at that batch. It also can't know whether the commit kept some of the work. The commit marker shows this: a rollback removes `#migrate_db_tx`, a commit keeps it. When the marker survives a failure, or `@@TRANCOUNT` reaches 0 after a batch, the step is recorded as `failed` (the SPEC §6.3 dirty state), which blocks later runs until `repair`. A script that leaves an extra transaction open, or one that can't commit, is rolled back completely and records nothing. Scripts that manage their own transactions must use `-- migrate:no-transaction`.

Guarantees: `atomic_ledger = true`, `previewable = true`, `external = false`.

**External runner (opt-in): `sqlcmd`.** Use it when scripts depend on sqlcmd features (`:r` includes, `:setvar`). Select it with the target setting `executor = "sqlcmd"` or the per-file directive `-- migrate:executor sqlcmd`. go-sqlcmd is supported first; legacy ODBC `sqlcmd` is deferred.

1. The Rust engine still takes `sp_getapplock` on its own native connection, plans, and verifies checksums.
2. For a **transactional** migration, the provider generates a temporary wrapper file:
   ```sql
   :on error exit
   SET XACT_ABORT ON;
   BEGIN TRAN;
   GO
   :r "<absolute path to migration>.up.sql"
   GO
   INSERT INTO [dbo].[__migrations] (...) VALUES (...);   -- emitted by Rust ledger code
   GO
   COMMIT;
   ```
   The ledger `INSERT` (or `DELETE` for a down) is generated by the same Rust ledger code the native executor uses. It is emitted as text, so the ledger logic lives in one place.
3. The runner is invoked as `sqlcmd -S … -d … -b -I -i <wrapper>`.
   - `-b` exits on the first error. The connection closing then rolls back the open transaction.
   - `-I` forces `QUOTED_IDENTIFIER ON`. sqlcmd defaults it to OFF, unlike drivers and SSMS, and OFF breaks filtered indexes and indexed views.
   - The password goes in `SQLCMDPASSWORD`. Entra uses `--authentication-method ActiveDirectoryDefault`. Credentials never appear on the command line.
4. **The outcome comes from the ledger, not the exit code.** After the process exits, the engine re-reads the ledger on its native connection:
   - **row present:** the migration is applied. A non-zero exit code here only produces a warning. Rust then `UPDATE`s the row with `execution_ms` and `runner_version`, which the wrapper couldn't know.
   - **row absent:** the migration is failed, and the transaction was rolled back. A `failed` history row is written, and the database stays clean.
5. **No-transaction** migrations (`-- migrate:no-transaction`) run without the wrapper's transaction. The ledger is written by Rust after a zero exit code. A non-zero exit leaves a `failed` row (dirty state, see SPEC §6.3).

Guarantees:
- transactional: `atomic_ledger = best-effort`. This holds unless the script issues its own `COMMIT`/`ROLLBACK`, which `validate` warns about.
- `previewable = false` for `:r`-included content. The wrapper is shown in `--dry-run --verbose`.
- `external = true`.
- `executor = "sqlcmd"` and `runner_version` are recorded in the ledger.

## MongoDB (`migrate-db-mongo`)

| Aspect | Design |
|---|---|
| Driver | The official `mongodb` crate (3.x, async) for the engine (lock, ledger) and the native executor. Low risk. |
| Script | Two formats, **selected by file extension**, and both can be mixed in one target:<br>• `.js`: **mongosh JavaScript**, run by the external `mongosh` runner (see below). This is the right choice when the team thinks in `db.users.createIndex(...)`.<br>• `.yaml` / `.json`: an ordered list of **`runCommand` documents** (MongoDB Extended JSON), run by the native executor. These are the server's own command language: `create`, `createIndexes`, `dropIndexes`, `collMod`, `update`, `delete`, `insert`, `renameCollection`, `drop`. No extra dependency, full dry-run.<br>The JavaScript needs a client-side runtime because the server can't execute it (`eval` was removed in MongoDB 4.2) and the Rust driver has none ([ADR-0001](decisions/0001-native-drivers-and-external-runners.md)). The default format generated by `new` is configurable (`new_format = "js" \| "yaml"`). |
| Transactions | Off by default. Multi-document transactions need a replica set or sharded cluster, and many DDL commands are restricted inside them. For native files with `transaction: true` on a server that supports it, commands and the ledger write run in one session transaction. Otherwise each command runs in order and the ledger is written last. A failure leaves a `failed` row (see SPEC §6.3). **Authors should write idempotent migrations**, for example `createIndexes` is already idempotent, and the `new` templates say so. |
| Lock | A lease document `{ _id: "<target>", epoch, owner, host, pid, toolVersion, ciRun, acquiredAt, expiresAt }` in `__migrations_lock`. The document is its own holder record (SPEC §7 rule 10). Each acquisition does `$inc: { epoch: 1 }`. Ledger progress writes filter on `lock_epoch` so a stalled old holder is fenced out (SPEC §6.5). It is acquired with `findOneAndUpdate` (filter: `expiresAt < now` or no doc; upsert), which is atomic thanks to the unique `_id`. A background heartbeat extends `expiresAt`, and a crashed holder's lock expires on its own. |
| Ledger | Collections `__migrations` (`_id` = version) and `__migrations_history`. |
| Cosmos DB for MongoDB | The RU and vCore offerings speak the Mongo wire protocol, so they **use this provider**. Compatibility gaps (for example transaction support on RU) are documented, not engineered around. |

### MongoDB executors

**External runner: `mongosh`, for `.js` files.**
1. The Rust engine takes the lease lock, reads the ledger, plans and verifies checksums. All of this runs on its own native connection.
2. For each `.js` step it runs `mongosh --nodb --quiet --file <wrapper.js>`. The connection string goes in the `MIGRATE_DB_URI` environment variable and never on the command line. The generated wrapper is:
   ```js
   db = connect(process.env.MIGRATE_DB_URI);
   load(<absolute path to migration, JSON-encoded>);
   ```
   The path is emitted with `serde_json::to_string`, never interpolated raw. A Windows path such as `P:\work\migrations\V2026…` contains `\N`, `\m` and `\V`, which a JS string literal would read as escape sequences.
3. A zero exit code makes Rust write the ledger and history rows. A non-zero exit code or timeout writes a `failed` row (dirty state; the operator inspects and runs `repair`). Output is captured, redacted, and stored truncated in history.
4. A script that finished but whose mongosh process died before exiting is treated as **failed**. The design fails closed: it never assumes success.
5. `validate --offline` syntax-checks `.js` files with a Rust JS parser and checksums them. It can't predict their effect.
6. Guarantees: `atomic_ledger = false`, `previewable = false`, `external = true`. A script may use `session.startTransaction()` internally, but the ledger write can't join that transaction.
7. `runner.min_version` is enforced at startup. `executor = "mongosh"` and `runner_version` are recorded in the ledger.

Example `V20261109150000__users_collection.up.js`:

```js
db.createCollection("users", { validator: { $jsonSchema: { bsonType: "object", required: ["email"] } } });
db.users.createIndex({ email: 1 }, { unique: true, name: "ux_users_email" });
db.users.updateMany({ status: { $exists: false } }, { $set: { status: "active" } });
```

**Native executor, for `.yaml` / `.json` files.** Each document is sent with `db.run_command(doc)` in order. Without `transaction: true`, progress is checkpointed in `__migrations` after every command, and `recovery: resume | rollback` is honored (SPEC §6.5). Prefer commands that are naturally idempotent, such as `createIndexes`, or `update` with filters that exclude already-migrated documents. Avoid `$inc`-style updates in resumable files. Guarantees: `atomic_ledger = true` only with `transaction: true` on a replica set, otherwise false. `previewable = true`, `external = false`.

**Out of scope (v1):** migrate-mongo-style Node modules (`module.exports = { async up(db, client) {} }`). These need Node.js and the `mongodb` npm package, not mongosh. They can be added later as another runner under the same rules.

Example `V20261109150000__users_collection.up.yaml` (native equivalent):

```yaml
transaction: false
commands:
  - create: users
    validator: { $jsonSchema: { bsonType: object, required: [email] } }
  - createIndexes: users
    indexes:
      - { key: { email: 1 }, name: ux_users_email, unique: true }
  - update: users
    updates:
      - { q: { status: { $exists: false } }, u: { $set: { status: "active" } }, multi: true }
```

## Azure Cosmos DB, NoSQL API (`migrate-db-cosmos`)

This is the **highest-risk provider**. Cosmos NoSQL has no DDL, no script language and no cross-partition transactions. "Migrations" here are declarative *operations*.

| Aspect | Design |
|---|---|
| Driver | `azure_data_cosmos`, the official Azure SDK for Rust. It has been in **public preview** since Feb 2025, and its 0.x releases still ship breaking changes. Mitigation: wrap it behind a thin internal `CosmosClient` adapter so SDK churn stays in one file, and pin the version. |
| Auth | Entra ID (recommended; uses `azure_identity`) or account key (`key_auth` feature). |
| **Control plane vs data plane** | Entra **data-plane RBAC does not allow creating or deleting databases and containers, or changing throughput**. Those are management operations. So: **key auth** does everything through the data-plane SDK, while **Entra auth** sends container and throughput operations to the **ARM REST API** (`Microsoft.DocumentDB/databaseAccounts/sqlDatabases/containers`) through `reqwest` and `azure_identity`, and needs `subscription`, `resource_group` and `account` in config. *(Confirm in the Phase 0 spike.)* |
| Script | YAML/JSON list of typed operations (below). Large payloads can be referenced as files (`itemsFile`, `bodyFile`). Paths are relative to the migration file and included in the checksum. |
| Transactions | None across operations. Each operation is applied in order, and progress is checkpointed in the ledger container after each one. On failure the ledger gets a `failed` row at that operation, and `recovery: resume \| rollback` lets the next run recover automatically (SPEC §6.5). Operations should be idempotent (`createContainer` → "create if not exists"; `upsertItems`; filtered `patchItems`). |
| Lock | A lease item (`id: "<target>"`) in a **dedicated `__migrations_lock` container**, which `acquire_lock` bootstraps. Because it is separate from the ledger container, the lock exists before `ensure_ledger` runs. It uses **ETag optimistic concurrency**: read, then replace with `If-Match`, or create if absent. It is taken if missing or if `expiresAt < now`. Heartbeat as in Mongo. The lease item carries the same holder fields as Mongo's, including `epoch`, so it is its own holder record. Ledger progress writes use `If-Match` on the row's ETag and check `lock_epoch`, which fences out a stalled old holder (SPEC §6.5). |
| Ledger | Container `__migrations` with partition key `/kind`. Applied items have `id = version`. History items use `kind = "history"`. Created on first run, which requires the control-plane path under Entra auth. |
| Data transforms | `patchItems` and `deleteItems` run a query, then patch or delete each item **per partition**, with bounded concurrency and 429 retry (handled by the SDK; we also back off ourselves). They report the number of items processed. These are not atomic: a re-run must converge, so authors should use filters like `NOT IS_DEFINED(c.status)`. |

Supported operations (v1):

```yaml
operations:
  - createContainer:   { id: orders, partitionKey: { paths: ["/tenantId"], kind: Hash },
                         indexingPolicy: { ... }, defaultTtl: -1, uniqueKeyPolicy: { ... } }
  - replaceContainer:  { id: orders, indexingPolicy: { ... } }     # indexing policy / TTL changes
  - setThroughput:     { container: orders, autoscaleMax: 4000 }
  - deleteContainer:   { id: legacy_orders }
  - upsertItems:       { container: config, itemsFile: seed/config.json }
  - patchItems:        { container: orders,
                         query: "SELECT * FROM c WHERE NOT IS_DEFINED(c.status)",
                         patch: [ { op: set, path: /status, value: open } ] }
  - deleteItems:       { container: orders, query: "SELECT * FROM c WHERE c.type = 'tmp'" }
  - createStoredProcedure: { container: orders, id: bulkClose, bodyFile: sprocs/bulkClose.js }
  - createUdf / createTrigger: { ... }
```

**Out of scope:** partition key changes. Cosmos can't alter a container's partition key, so that needs a new container plus a data copy. The tool can express this as `createContainer` + a future `copyItems` operation, but v1 does not automate it.

---

## Capability summary

| | Postgres | SQL Server | Mongo | Cosmos NoSQL |
|---|---|---|---|---|
| Transactional DDL | ✅ | ✅ (most) | ⚠️ replica set, limited | ❌ |
| Script + ledger atomic | ✅ | ✅ | ⚠️ opt-in | ❌ |
| Native lock | advisory lock | `sp_getapplock` | lease doc | lease doc + ETag |
| Script format | SQL | T-SQL + `GO` | mongosh `.js` or runCommand YAML/JSON | operations YAML/JSON |
| Native executor | `tokio-postgres` | `mssql-tds` | `mongodb` (YAML/JSON) | `azure_data_cosmos` + ARM |
| External runner | none | `sqlcmd` (opt-in) | `mongosh` (`.js`) | none |
| Baseline capture (SPEC §6.6) | `pg_dump` (one-time runner) + native catalog | SqlPackage (one-time runner) + native `sys.sql_modules` | native | native SDK / ARM |
| Schema fingerprint | native catalog | native catalog | native | native |
| Entra ID | ✅ token | ✅ token | n/a (Atlas/OIDC later) | ✅ (+ ARM for DDL-like ops) |
| Driver maturity | High | Medium (verify) | High (official) | Preview (official) |
