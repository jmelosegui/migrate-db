# migrate-db: Functional Specification

Status: **Draft v0.1** (2026-11-09)

## 1. Goals and non-goals

### Goals
1. Apply pending migrations in order (**up**). Revert applied ones in reverse order (**down**).
2. Scaffold new migration files in a configured folder (**new**).
3. Keep a **ledger** in the target database. The ledger records what was applied, when, by whom, with which checksum, and the resulting database version.
4. Use one engine for every database. Each database type is a pluggable **provider**: PostgreSQL, SQL Server, MongoDB and Cosmos DB.
5. Be safe in CI/CD. Runs are concurrency-safe, report drift, support dry-runs, write machine-readable output and use meaningful exit codes.
6. Ship as a single static binary (Windows, Linux, macOS) that runs as a **CI/CD pipeline step**, separate from application deployment (see §10). It doesn't run at app or pod startup.
7. **Local/pipeline parity.** A developer running the tool locally gets exactly the behavior the pipeline gets (§10.0).

### Non-goals (v1)
- Generating migrations by diffing schemas (state-based migrations, as in SSDT or Atlas).
- An ORM or query builder.
- Moving data *between* different databases.
- A GUI.

## 2. Concepts

| Term | Meaning |
|---|---|
| **Target** | The one database a project folder manages. Its `migrate-db.toml` gives its provider, migrations folder and ledger location ([ADR-0005](decisions/0005-one-database-per-config-environment-agnostic.md)). A system with several databases has several folders. |
| **Run settings** | Where and how one run connects (`MIGRATE_DB_CONNECTION`, `MIGRATE_DB_AUTH`) and its safety switches (`MIGRATE_DB_ALLOW_DOWN`, `MIGRATE_DB_ALLOW_OUT_OF_ORDER`). The pipeline stage sets them; the config never names environments. |
| **Migration** | A versioned change with an **up** script and an optional **down** script. |
| **Version** | A sortable identifier. The default is a UTC timestamp, `YYYYMMDDHHMMSS`. Sequential integers are supported as an alternative. |
| **Database version** | The highest version among ledger rows of kind `versioned` or `baseline` with status `applied`. Repeatable rows (§3.6) never count. |
| **Ledger** | Provider-managed storage in the target database. It holds an *applied* set (current state) and an append-only *history log* (audit). |
| **Checksum** | The SHA-256 of the normalized up-script content. It detects edits to migrations that were already applied. |
| **Baseline** | A ledger marker for a database that existed before the tool. Migrations at or below the baseline version count as applied. |

## 3. Migration files

### 3.1 Layout and naming

```
<migrations_dir>/
  V20261109142530__create_users_table.up.sql
  V20261109142530__create_users_table.down.sql
  V20261110090000__add_email_index.up.sql        # no down file = irreversible
  R__refresh_reporting_views.sql                  # repeatable (§3.6)
  programmability/
    R__dbo.usp_close_order.sql                    # subfolders are allowed for organizing files
```

- Discovery is **recursive** within `migrations_dir`. Subfolders exist only to organize files; they don't change order or versioning. Versions and repeatable names must be unique across the whole tree.

- Pattern: `V<version>__<snake_description>.<up|down>.<ext>`
- `<ext>` depends on the provider: `sql` for Postgres and SQL Server; `js` (mongosh) or `yaml`/`yml`/`json` (runCommand) for Mongo; `yaml`/`yml`/`json` for Cosmos. For Mongo the extension also selects the executor (see §3.5).
- Versions must be unique within a target. Duplicate versions are a validation error.
- A missing `.down` file marks the migration **irreversible**. `down` refuses to plan through it, and the refusal happens before anything runs.
- Files that don't match the pattern are ignored, with a warning. A config flag can make that warning an error.

**Why timestamps by default:** when two branches each add a migration, sequential numbers collide (`0042` and `0042`). Timestamps practically never collide.

### 3.2 Directives

Scripts can carry engine directives. SQL scripts put them in header comments:

```sql
-- migrate:no-transaction        -- run outside a transaction (e.g. CREATE INDEX CONCURRENTLY)
-- migrate:timeout 900           -- statement timeout in seconds for this migration
-- migrate:tag hotfix            -- free-form tag stored in the ledger
-- migrate:executor sqlcmd       -- SQL Server only: run this file with the sqlcmd runner (§3.5)
```

YAML/JSON scripts put the same directives in top-level keys (`transaction:`, `timeout:`, `tags:`). They also accept `recovery: manual | resume | rollback`, which controls recovery after a partial failure (§6.5).

### 3.3 Placeholders

Scripts may reference `${name}` placeholders, for example `${schema}`. Values come from the run, through `MIGRATE_DB_PLACEHOLDER_<NAME>` variables (process environment first, then `--env-file`), because the config is environment-agnostic ([ADR-0005](decisions/0005-one-database-per-config-environment-agnostic.md)). Placeholders are **off by default** and enabled with `placeholders = { enabled = true }`. When they are off, scripts run exactly as written.

- **Names** are `[A-Za-z_][A-Za-z0-9_]*` and match the variable case-insensitively: `${schema}` reads `MIGRATE_DB_PLACEHOLDER_SCHEMA`. `$${` writes a literal `${`.
- **Checksums** are computed on the **raw** file, before substitution, so values that differ between pipeline stages do not register as drift.
- **Missing values fail closed.** Every planned script is rendered before the first step runs (§7 rule 4), so a missing value or a malformed `${…}` stops `up`, `down`, `goto` and `--dry-run` with exit code 2, naming the variable to set. Nothing is executed.
- **`validate --offline`** checks placeholder syntax only, because a PR build may not have a stage's values. Online `validate` also checks that every referenced value is set.
- Values are never logged; diagnostics show placeholder names only.

### 3.4 Checksum normalization

The checksum is `sha256(normalize(up_script_bytes))`, where normalizing means:
1. Strip a UTF-8 BOM.
2. Convert CRLF to LF. This matters on Windows: without it, git `autocrlf` would look like drift.
3. Leave everything else as is. Whitespace and comment edits *do* change the checksum.

Files a migration references are included in its checksum: Cosmos `itemsFile` / `bodyFile`, sqlcmd `:r <path>` and mongosh `load("<path>")` with a **literal** path. They are resolved relative to the migration file and included recursively, so editing a shared procedure pulled in with `:r` counts as drift. Some references can't be resolved statically, such as a path containing `$(var)` or a computed `load()` argument. `validate` warns about those, because edits to such files can't be detected.

### 3.5 Executors

Each step runs on one **executor** ([ADR-0001](decisions/0001-native-drivers-and-external-runners.md)):

- **Native (default):** the provider's Rust driver runs inside the process.
- **External runner (opt-in):** the database's CLI runs as a child process. These exist only where the CLI provides something the native path cannot:
  - **`sqlcmd`** for SQL Server scripts that need `:r` or `:setvar`. Selected by target setting or `-- migrate:executor sqlcmd`.
  - **`mongosh`** for Mongo `.js` files. Selected by extension.

Whichever executor runs a step, the engine owns the lock, plan, checksums and ledger. Every planned step carries **guarantees**, and `status` and `--dry-run` print them:

| Guarantee | Meaning |
|---|---|
| `atomic_ledger` | `yes` / `best-effort` / `no`: whether the script's effect and its ledger row commit together |
| `previewable` | Whether `--dry-run --verbose` can show exactly what will execute |
| `external` | The step runs in an external process |

An external runner must be found on `PATH` (or at `runner.path`) and must meet `runner.min_version`. If not, the engine exits with code 6 **before** taking the lock. The runner gets credentials only through environment variables or a short-lived, permission-restricted file, never through command-line arguments.

### 3.6 Repeatable migrations

Repeatables keep **one file per object** for code that changes often: procedures, views, functions, triggers, reference data. This replaces the main convenience of DacFx ([ADR-0002](decisions/0002-replace-dacfx-with-versioned-migrations.md)).

- **Naming:** `R__<name>.<ext>`, anywhere in the tree, for example `R__dbo.usp_close_order.sql`. `<name>` must be unique within the target.
- **When they run:** after all pending versioned migrations in an `up` or `goto` run, **re-applied whenever their checksum differs from the last applied checksum** (or when they've never been applied).
- **Order:** alphabetical by `<name>`. Use prefixes such as `R__010_views_…` / `R__020_procs_…` when one repeatable depends on another.
- **Only at the latest version:** repeatables run only when the run ends at the **latest** versioned migration. With `--to`, `--steps`, or a `goto` below latest, they are skipped and the output says so. That prevents applying a view that references a column a later versioned migration adds.
- **Must be idempotent:** `CREATE OR ALTER` (SQL Server), `CREATE OR REPLACE` (Postgres), upserts for reference data. `validate` warns when a SQL repeatable lacks `CREATE OR ALTER` / `OR REPLACE`.
- **Not for schema-bound or indexed views.** On SQL Server, `ALTER VIEW` (which is what `CREATE OR ALTER` does on an existing view) **drops every index on an indexed view**, and schema-bound objects block changes to the tables they reference. Keep such objects in **versioned** migrations that drop and recreate the indexes explicitly. `validate` warns on `WITH SCHEMABINDING` in an `R__` file.
- **No down file.** `down` never touches repeatables. To roll back a repeatable, change the file (or revert it in git) and run `up`. Dropping the object belongs in a versioned migration; deleting the `R__` file alone only produces a `missing-file` warning.
- **Ledger:** `kind = repeatable`, `version = "R__<name>"`, plus the checksum of the last applied content. Each repeatable is its own step with the same transaction and executor rules as versioned migrations.
- **Failure:** stops the run like any other step (§7 rule 5). Versioned migrations already applied in the same run stay applied.

## 4. CLI

Binary name: `migrate-db`. One config describes one database, and it holds no environments ([ADR-0005](decisions/0005-one-database-per-config-environment-agnostic.md)): **each run supplies its own connection and safety switches**, normally as `MIGRATE_DB_*` variables set by the pipeline stage. Each command offers only the options it uses.

Global options (every command):

| Flag | Variable | Description |
|---|---|---|
| `-c, --config <path>` | `MIGRATE_DB_CONFIG` | Config file. Default: discovered (§5.1). |
| `--output <text\|json>` | | Output format. Default: `text`. |
| `--output-file <path>` | | Also write the JSON result to a file. |
| `--ci` | `MIGRATE_DB_CI` | Force CI behavior locally (§10.0). |

Connection options (`status`, `validate`, `up`, `down`, `goto`, `unlock`, and `init` for its connection test; `new` never connects):

| Flag | Variable | Description |
|---|---|---|
| `--connection <str>` | `MIGRATE_DB_CONNECTION` | Connection string. Prefer the variable, so the secret stays out of the command line. |
| `--auth <sql\|entra>` | `MIGRATE_DB_AUTH` | Authentication mode. `sql`: a login in the connection string. `entra`: Entra ID through the default credential chain. |
| `--env-file <path>` | | Read `MIGRATE_DB_*` values from a `KEY=VALUE` file. Flags and process variables win (§10.0). |

Run options (`up`, `down`, `goto`): `--dry-run`, `--expect-plan <file>`, `--lock-timeout <secs>` (default 60). `--allow-down` (`MIGRATE_DB_ALLOW_DOWN`) is on `down` and `goto` and is **off by default**. `--allow-out-of-order` (`MIGRATE_DB_ALLOW_OUT_OF_ORDER`) is on `up` and `goto`.

### 4.1 Commands

| Command | Behavior |
|---|---|
| `init [PATH] [flags…]` | **Guided setup** (§5.2). Run it in the folder that holds the database project, or pass that folder. It detects existing tooling, asks for the database technology and settings, then writes `migrate-db.toml` and the migrations folder. Every prompt has a flag, so it also runs without prompts. It refuses a folder that already has a config; it never overwrites. |
| `new <description> [--irreversible] [--repeatable] [--folder <path>]` | Creates an `up`/`down` file pair from the provider template in `migrations_dir`, versioned with the current UTC timestamp. `--repeatable` instead creates one `R__<name>.sql` file (§3.6), where `<name>` is the object it defines (`dbo.get_orders`; brackets are dropped, spaces become `_`); it refuses a name that already exists anywhere in the tree, ignoring case. `--folder` creates the file(s) in a subfolder of `migrations_dir` (`orders`, `procs/reporting`); it refuses paths that leave the folder, and dot-folders, which discovery skips. Prints the paths it created. (This is the "create migration script" requirement. It also works offline.) |
| `status` (alias `info`) | Shows the database version and every migration with its state: `applied`, `pending`, `failed` (with `at op i/n, recovery: …` for operation-list files, §6.5), `in-progress`, `checksum-mismatch` (versioned only), `missing-file`, `out-of-order`, `baseline`. Repeatables show `applied`, `changed (will re-apply)` or `new`. It also shows the executor and guarantees (§3.5) for each migration, and the **lock state** (§7 rule 10): free or held, holder, CI run, since when, and lease expiry. It reads the lock without acquiring it. |
| `up [--to <version>] [--steps <n>] [--dry-run] [--allow-out-of-order]` | Applies pending migrations in ascending order, up to and including `--to`, or `n` steps. `--allow-out-of-order` can also be set with `MIGRATE_DB_ALLOW_OUT_OF_ORDER`. |
| `down [--to <version>] [--steps <n>] [--dry-run] [--yes]` | Reverts applied migrations in descending order. The default is `--steps 1`. `--to <v>` reverts everything *above* v, and `--to 0` reverts everything. **Refused (exit 2) unless the run allows reverting** (`--allow-down` / `MIGRATE_DB_ALLOW_DOWN`). Also requires `--yes` when not in a TTY. |
| `goto <version> [--dry-run] [--expect-plan <file>] [--allow-out-of-order] [--yes]` | Moves the database to exactly `<version>`, choosing direction automatically (see §7 rule 9). `goto 0` reverts everything. If the plan contains any down step, every `down` safeguard applies: `--allow-down`, `--yes` when non-interactive, and irreversible checks. |
| `validate [--offline]` | Offline checks: naming, duplicate versions, parse/syntax checks done by the provider (for example YAML schema or `GO` splitting), and placeholders that resolve. Online checks additionally compare the files against the ledger: checksums of **versioned** migrations, and missing files. A changed repeatable is reported as "will re-apply", not as an error. Exits with code 2 on a problem. |
| `rehearse --scratch [--round-trip]` | **Executes** every versioned and repeatable migration from zero against a **disposable** database, and fails on any error (exit 4). It replaces DacFx's build-time validation and runs in PR builds against a container or emulator ([ADR-0002](decisions/0002-replace-dacfx-with-versioned-migrations.md)).<br>• `--round-trip`: after reaching latest, runs every `down` back as far as possible, then `up` again. The final up leg **force-applies every repeatable**, even if its checksum is unchanged, because `down` never touches repeatable rows. That proves the downs work and the repeatables still apply against the rebuilt schema. Irreversible migrations end the down leg early, with a notice. For adopted databases the down leg therefore stops at `V…__baseline` (which has no down), so round-trip exercises only post-baseline downs.<br>• **Safety:** the run must confirm the database is disposable with `--scratch` (or `MIGRATE_DB_SCRATCH=1`), and the database must be **empty**: no ledger, and no user objects (SQL), collections (Mongo) or containers (Cosmos). Otherwise it exits with code 2 before touching anything.<br>• It doesn't create or drop the database; the pipeline owns the container's lifecycle. |
| `baseline capture [--version <ts>] [--description <d>]` | **Read-only** on the database. Point the run's connection at the reference database. It scripts that database's current schema into the migrations folder: `V<ts>__<d>` (default `baseline`), `R__` files for programmable objects (SQL), and a **schema fingerprint** `V<ts>__<d>.fingerprint.json`. It doesn't matter how the schema got there (DacFx, startup migrations, hand changes). See §6.6. Refuses if files for that version already exist. |
| `baseline compare [--against <fingerprint-file>] [--against-connection <var>]` | **Read-only.** Computes the live schema fingerprint of the run's database and diffs it against the captured fingerprint (default: the latest `*.fingerprint.json`), or against a second database whose connection string is in the named variable. It prints the differing objects and exits with code 2 if there are any. Run it once per environment (pipeline stage) to prove every environment matches before marking, and to prove a rehearsed baseline reproduces the reference. |
| `baseline mark --version <v> [--description <d>] [--mark-repeatables] [--force-unverified] [--dry-run] [--expect-plan <file>]` | Writes a baseline marker into an empty ledger. Fails if the ledger already holds rows. **Executes no scripts.** Unless `--force-unverified` is given, it first runs the same check as `baseline compare` against the captured fingerprint and refuses (exit 2) on any difference.<br>• `--mark-repeatables`: also records every `R__` file's current checksum as applied, still without executing anything.<br>• `--dry-run` / `--expect-plan` work as for `up`, so marking goes through plan → approval → apply like any other change. A `baseline mark` plan file is never valid for `up`, or the other way round.<br>• The version must be an existing `V<v>__...` file; the marker records its checksum. It needs `--yes` when non-interactive.<br>• *Until `baseline capture` / `compare` exist there is no fingerprint to check, so `mark` requires `--force-unverified` and exits with code 2 without it.* |
| `repair [--accept-checksums] [--remove-failed] [--resolve <version> applied\|reverted]` | Fixes the ledger **without running scripts**. It runs under the lock, so it can't touch a row whose run is still alive. Every action records a history event, and it needs `--yes` in CI.<br>• `--accept-checksums`: rewrites stored checksums to match the current files.<br>• `--remove-failed`: deletes `failed` / `in_progress` rows from **up** runs, declaring "not applied". It refuses rows from **down** runs, because there the real state is ambiguous.<br>• `--resolve <v> applied\|reverted`: the operator states the real outcome after fixing the database by hand. `applied` marks the row applied; `reverted` deletes it. It works on `failed` and `in_progress` rows in either direction. |
| `unlock --force [--kill-holder]` | Prints the current holder and asks for confirmation (`--yes` in CI, otherwise exit 64). `--force` is required. What it does depends on the lock type:<br>• **Lease locks** (Mongo, Cosmos): deletes the lease.<br>• **Session locks** (Postgres, SQL Server): these can't go stale, because they die with their session. If the lock is **free**, the command removes a stale holder record (only the record it read, and only while the lock is still free). If the lock is **held**, it exits with code 3 and names the holder, unless `--kill-holder` is given: then it terminates the holder's database session (`pg_terminate_backend` / `KILL`), whose current step rolls back. It kills only a session it has **verified** holds the lock (`pg_locks`; on SQL Server `sys.dm_tran_locks`, which needs `VIEW DATABASE PERFORMANCE STATE`). An unknown or unverifiable holder is never killed (exit 3). Missing permission to kill is exit 5.<br>Anything it removes or kills is recorded as a `force-unlock` history event when the ledger exists; it never creates the ledger. JSON: `lock_before` (the `lock` object) and `result` (`{"action": "nothing-to-do" \| "cleared-stale" \| "killed-holder", "holder": {…}}`). |

`--dry-run` connects, reads the ledger and prints the exact plan: ordered migrations, direction, and the transaction mode, executor and guarantees for each one. External runners are never launched during a dry run. Nothing is executed and the lock is not taken. `--dry-run --verbose` also prints the rendered scripts.

### 4.2 Exit codes

| Code | Meaning |
|---|---|
| 0 | Success. This includes "nothing to do". |
| 1 | Unexpected or internal error. |
| 2 | Validation failed: drift, checksum mismatch, bad files, out-of-order without the allow flag. |
| 3 | Timed out acquiring the lock. |
| 4 | A migration failed during execution. |
| 5 | Connection or authentication failure. |
| 6 | External runner (`sqlcmd`, `mongosh`) is missing or below `runner.min_version`. |
| 64 | CLI usage error. |

When a run stops at a failing step, the report still lists every step that completed before it, and the failure records its `kind`. That decides the exit code: `execution` (the script failed) is 4; `connection` / `auth` is 5; `runner` is 6; anything else, such as a ledger write, is 1. After a lost connection the step's outcome is **unknown**, because its commit may or may not have reached the server. The message says so and points to `status`. A non-transactional step's `in_progress` row then blocks later runs until it is resolved (§6.3).

### 4.3 JSON output

When `--output json` is set, every command prints one JSON document to stdout. Logs go to stderr. The schema is versioned (`"schema": 1`). Example for `up`:

```json
{
  "schema": 1, "command": "up", "target": "orders", "env": "dev",
  "from_version": "20261101000000", "to_version": "20261109142530",
  "results": [
    {"version": "20261109142530", "description": "create users table",
     "direction": "up", "status": "applied", "duration_ms": 184}
  ]
}
```

## 5. Configuration

The file is `migrate-db.toml`, and **one file describes one database** ([ADR-0005](decisions/0005-one-database-per-config-environment-agnostic.md)). For several databases, use one folder per database, each with its own config and `migrations/` folder.

The file says **what the database is**. It is environment-agnostic: no connection strings, environment names or secrets. Each run says **where and how**, through flags or `MIGRATE_DB_*` variables (§4) that the pipeline sets per stage: `MIGRATE_DB_CONNECTION`, `MIGRATE_DB_AUTH`, `MIGRATE_DB_ALLOW_DOWN`, `MIGRATE_DB_ALLOW_OUT_OF_ORDER`. Where the provider supports it, `auth = entra` uses Entra ID (Azure AD) through the default credential chain: Azure CLI, managed identity, workload identity, environment.

```toml
provider = "mssql"                # postgres | mssql | mongo | cosmos   (required)
migrations_dir = "migrations"     # relative to this file; this is the default
versioning = "timestamp"          # or "sequential"
strict_filenames = false
ledger = { schema = "dbo", table = "__migrations" }
placeholders = { enabled = true } # values come from MIGRATE_DB_PLACEHOLDER_<NAME> (§3.3)
executor = "native"               # native | sqlcmd  (files can override with -- migrate:executor)
runner = { path = "sqlcmd", min_version = "1.8.0" }   # only used by the sqlcmd executor
```

Other providers use the same flat shape, for example `provider = "mongo"` with `new_format = "js"`, or `provider = "cosmos"` with `ledger = { database = "orders", container = "__migrations" }`. Settings that differ per stage, such as the Cosmos endpoint and ARM control plane, will come from `MIGRATE_DB_*` variables when those providers are built (Phase 4).

**Fail-closed defaults:** a run that sets nothing may not revert or apply out of order. Unknown keys are errors, so a typo can't silently weaken a setting. A file that still uses the earlier `[targets]`, `[environments]` or `[project]` tables is rejected with a message that says what replaces them.

### 5.1 Config discovery

- Every command except `init` needs a config file. Without `--config` / `MIGRATE_DB_CONFIG`, the tool looks for `migrate-db.toml` in the current directory, then in each parent directory, like git.
- The search **stops at the repository root**, the first directory containing `.git`, so a stray config in a parent folder outside the repo is never picked up. The first file found wins.
- **All relative paths in the file resolve relative to the config file's directory**, not the current directory. So `migrate-db up` gives the same result from any subfolder of the project.
- If no config is found, the tool exits with code 64 and suggests `migrate-db init`.

### 5.2 Guided setup (`init`)

`init` is the only way the tool creates a config. There are no separate `configure` or `config` commands; any later change is a hand edit of the TOML.

**Usage:** `migrate-db init [PATH] [flags…]`. `PATH` is the project folder to set up; the default is the current folder. The config and migrations folder are written there. If the folder already has a `migrate-db.toml`, `init` exits with code 64 and changes nothing: one folder holds one database.

**Modes**
- **Interactive** when stdin and stdout are terminals and no CI is detected (§10.2). It asks questions, suggesting defaults and detection results, shows the TOML it will write, and asks for confirmation.
- **Non-interactive** otherwise. Every answer comes from flags; answers that have a default use it. Only the **provider** is required: pass `--provider`, or `--yes` to accept a detected one. Otherwise `init` exits with code 64 and says which flag is missing. It never hangs waiting for input.
- **Never asks for secrets or environments.** The config it writes is environment-agnostic (§5), so it is always safe to commit.

**Step 1: Detection** (read-only). It scans the current directory up to depth 3 (for code patterns, `*.cs` and `*.js` files up to depth 5), skipping `.git`, `node_modules`, `bin`, `obj` and `target`. Findings become suggestions and warnings; they never change anything:

| Signal | Suggestion / warning |
|---|---|
| Loose `*.sql` files | A SQL provider (asks which one). Offers the folder as `migrations_dir` and warns if file names don't match the `V<version>__…` pattern. |
| Signs that **another mechanism deploys this schema**: `*.sqlproj`, a DbUp project (`dbup-*`), EF Core `Migrations/` with a model snapshot, or startup calls such as `Database.Migrate(`, `MigrateAsync(`, `PerformUpgrade(`, `CreateContainerIfNotExistsAsync(` | Suggests a provider where it can. Prints one generic warning: the database has an existing schema and an existing deploy mechanism, so adopt it with the baseline process (`baseline capture` → `compare` → `mark`, [ADOPTION.md](ADOPTION.md)), and switch that mechanism off at cutover. The hits are shown only to help find what to switch off. Nothing is modified. |
| `flyway.conf` / `flyway.toml` | Provider from the JDBC URL. Notes that Flyway's `V<n>__name.sql` files need renaming to `.up.sql` (and need down files). |
| `migrate-mongo-config.js` | Mongo. Warns that Node-style migration files aren't supported in v1 and need rewriting as mongosh `.js` or YAML. |
| `database.json` with a `driver` key (db-migrate) | Provider from the driver. |

**Step 2: Questions**, each with its flag:

| Question | Flag | Default |
|---|---|---|
| Database technology | `--provider postgres\|mssql\|mongo\|cosmos` | from detection |
| Migrations folder | `--dir <path>` | `migrations` (or the folder of detected `.sql` files) |
| Ledger location | `--ledger <schema.table>` (SQL) / `--ledger <database/container>` (Cosmos) / `--ledger <database>` (Mongo) | `dbo.__migrations` / `public.__migrations` / `<db>/__migrations` |
| Version scheme | `--versioning timestamp\|sequential` | `timestamp` |
| SQL Server executor | `--executor native\|sqlcmd` | `native` |
| Mongo default `new` format | `--new-format js\|yaml` | `js` |
| Mongo transaction mode | `--mongo-transactions` | off |
| Test connection | `--test-connection` | off; offered interactively only if `MIGRATE_DB_CONNECTION` is set |
| Pipeline template *(Phase 5)* | `--pipeline none\|azure-devops\|github` | `none` |

Questions for the Mongo and Cosmos providers are added in their phases; until then `init` offers only `mssql` and `postgres`.

**Step 3: Output**
- `migrate-db.toml`, with a short comment above each generated key.
- The migrations folder, with a `.gitkeep`.
- A pipeline template, if one was chosen.

`init` then runs `validate --offline` and prints next steps: set `MIGRATE_DB_CONNECTION`, `migrate-db new "<description>"`, `status`, `up`, plus a `baseline` hint if existing tooling was detected.

**Connection test** (optional). It uses the run's connection (`MIGRATE_DB_CONNECTION` / `--connection`, `MIGRATE_DB_AUTH` / `--auth`) and the read-only path (§7 rule 8): connect, then report the database version, ledger presence and lock state. Nothing is written. A failure is a **warning** in interactive mode and exit code 5 in non-interactive mode.

## 6. Ledger

The ledger is created automatically and idempotently on first contact (`ensure_ledger`). Each provider stores it natively (see PROVIDERS.md) but follows the same logical schema.

### 6.1 Applied set (current state)

One row or document per migration that is currently applied.

| Field | Type | Notes |
|---|---|---|
| `version` | string (PK) | Stored as text so timestamps and integers sort the same way, by fixed width or numeric parse. |
| `description` | string | |
| `script_name` | string | The up file name. |
| `checksum` | string | SHA-256 hex. |
| `kind` | enum | `versioned` \| `baseline` \| `repeatable` |
| `status` | enum | `applied` \| `in_progress` \| `failed`. `in_progress` and `failed` occur only for non-transactional migrations (§6.3, §6.5). |
| `direction` | enum, nullable | `up` \| `down`. The direction of the in-progress or failed run. Null when `applied`. |
| `op_count`, `completed_ops`, `failed_op` | int, nullable | Progress for operation-list files (§6.5). Null otherwise. |
| `recovery` | enum, nullable | `manual` \| `resume` \| `rollback`, copied from the file when the run started. |
| `lock_epoch` | int, nullable | Lease epoch of the run that owns an `in_progress` or `failed` row. All its writes are fenced on it (§6.5). Lease-lock providers only. |
| `applied_at` | UTC timestamp | |
| `applied_by` | string | DB principal and OS user/host, for example `sql:deploy_user@build-agent-07`. |
| `execution_ms` | int | |
| `tool_version` | string | migrate-db version. |
| `executor` | enum | `native` \| `sqlcmd` \| `mongosh` |
| `runner_version` | string, nullable | Version reported by the external runner. Null for native. |

### 6.2 History log (append-only audit)

One row per *event*: up, down, baseline, repair, unlock.

| Field | Notes |
|---|---|
| `id` | Monotonic or UUID. |
| `version`, `direction`, `event` | `up` / `down` / `baseline` / `repair` / `force-unlock` / `resume` / `recovery-rollback` |
| `from_op`, `to_op` | For operation-list files: the range of operations this event executed. |
| `status` | `succeeded` / `failed` |
| `checksum`, `started_at`, `finished_at`, `applied_by`, `host`, `tool_version` | |
| `error` | Truncated error message on failure. |

### 6.3 Ledger semantics

- **Up success:** the applied row is inserted and a history row added. For transactional migrations, both writes happen **in the same transaction as the script**, so the change is atomic.
- **Down success:** the applied row is deleted and a history row added. These writes are also atomic for transactional migrations.
- **Failure (transactional):** the transaction rolls back. Only a `failed` history row is written, in a separate transaction. The database stays clean and a re-run is safe.
- **Failure (non-transactional):** the database may be partially changed. The engine writes an applied row with `status = failed`, and every later `up`/`down` refuses to run until an operator fixes the database and runs `repair --remove-failed` or `repair --resolve`. This works like Flyway's "dirty" state. **Exception:** operation-list files whose `recovery` is `resume` or `rollback` can recover automatically (§6.5).
- **External runners:** where possible, the outcome comes from **re-reading the ledger after the process exits**, not from the exit code. For example, the sqlcmd wrapper writes the ledger row inside its own transaction. Where that isn't possible (mongosh), a non-zero exit, a timeout, or an unknown result is treated as **failed** and writes a `failed` row. The tool never assumes success. Per-provider details are in PROVIDERS.md.

### 6.4 Ledger schema versioning

The ledger's own structure will change between tool releases. A `__migrations_meta` record (table row or document) stores `ledger_schema_version`.
- `ensure_ledger` upgrades older ledger schemas forward, idempotently and under the lock.
- A tool that finds a ledger **newer** than it understands refuses to write, exits with code 2 and asks for an upgrade.
- Ledger upgrades must stay backward-readable for at least one minor release.

### 6.5 Operation-level progress and recovery

**Applies to** native-executor YAML/JSON migrations that run without a transaction: Mongo runCommand files without `transaction: true`, and all Cosmos files. These files are ordered lists of operations, so the engine can checkpoint between operations. It does **not** apply to `.sql` or mongosh `.js` files, which are opaque to the engine, or to transactional files, which roll back as a whole.

**Progress tracking**, under the lock:
1. Before operation 0, the ledger row is written with `status = in_progress`, `direction`, `op_count = n`, `completed_ops = 0` and `recovery`. For a down, the existing applied row switches to `in_progress`.
2. After each operation succeeds, `completed_ops` is set to `i + 1`. This costs one small ledger write per operation.
3. After the last operation, the row becomes `applied` (up) or is deleted (down), and a history row is added.
4. If operation `i` fails, the row becomes `failed` with `failed_op = i`, and the error goes to the history log.
5. If the process is killed, the row stays `in_progress`. The next run acquires the lease, takes over the row (see **Fencing** below), and treats it as `failed` at operation `completed_ops`.

**Fencing (lease locks).** §6.5 only applies to Mongo and Cosmos, which use leases. A lease can expire while its holder is still alive, for example after a stall, a network blip or a throttled heartbeat. To prevent two writers on one migration:
- **Epoch per acquisition.** Every lease acquisition increments a monotonic `epoch` stored in the lease document.
- **Conditional writes.** The ledger row stores the epoch of the run that owns it (`lock_epoch`). Every progress or status write is conditional on `lock_epoch` matching the writer's epoch: a filter on `lock_epoch` in Mongo, and `If-Match` on the row's ETag (plus an epoch check) in Cosmos.
- **Takeover.** A new holder taking over an `in_progress` or `failed` row first rewrites `lock_epoch` to its own epoch. The old holder's next write then fails, and it aborts immediately (exit 4) without starting another operation.
- **Failed renewal.** A holder whose heartbeat fails to renew before half of `lease_ttl` remains stops before starting its next operation.
- **Residual risk.** An operation the old holder had *already started* can still finish while the new holder re-runs it. That is one more reason resumable operations must be idempotent.

**Manual override.** Under any policy, an operator can settle a row with `repair --resolve <version> applied|reverted` (§4.1).

**Recovery policy.** The file's `recovery:` key decides what the next run does:

| Policy | Behavior |
|---|---|
| `manual` (**default**) | As in §6.3: blocked until an operator runs `repair --resolve` (or `--remove-failed` for up runs). |
| `resume` | The next run in the same direction continues from operation `completed_ops`. It **re-runs the operation that failed**, because that operation may have partly applied. Every operation from that point on must therefore be idempotent. |
| `rollback` | **Up failures only.** The next `up` first runs the migration's down operations, which must tolerate a partially applied up, then re-applies the up from operation 0. It needs a down file, and `validate` errors otherwise. A failed **down** with `rollback` is treated as `manual`. |

`manual` is the default so the tool fails closed. The `new` template for Mongo YAML and Cosmos files writes `recovery: resume` **explicitly**, with a comment about idempotency. That way the choice is visible in code review.

**Guards:**
- **Checksum.** Recovery is refused (exit 2) if the file's checksum differs from the one stored when the failed run started. The partial state belongs to the old content. The operator decides, using `repair`.
- **Direction.** A failed **up** can only be recovered by `up` (or `goto` to a version at or above it). Trying to `down` through it exits with code 2. A failed **down** can only be resumed by `down` or `goto` below it.
- **Visibility.**
  - `status` shows `failed at op 3/5 (recovery: resume)`.
  - `--dry-run` shows `resume V… from op 3/5` or `rollback V…, then re-apply`.
  - The `plan_fingerprint` (§10.1) includes the start operation, so an approved plan can't silently become a different recovery.
- **History.** Recovery runs record `resume` or `recovery-rollback` events, with `from_op` and `to_op`.
- **Within one operation.** v1 doesn't checkpoint inside a single operation, such as a Cosmos `patchItems` over many items. Its query filter must make re-runs converge, for example `NOT IS_DEFINED(c.status)`.

### 6.6 Baseline capture and schema fingerprints

A baseline is derived from a **live database**, never from the tool that built it. That makes adoption the same for every starting point (see [ADOPTION.md](ADOPTION.md)).

**Capture** (`baseline capture`, read-only). Each provider scripts its schema:

| Provider | Versioned baseline contains | Repeatables (`R__`) | How |
|---|---|---|---|
| SQL Server | schemas, types, tables, constraints, indexes, sequences, schema-bound/indexed views, synonyms | procedures, functions, non-schema-bound views and triggers, as `CREATE OR ALTER`, one file each | modules natively from `sys.sql_modules`; tables etc. through **SqlPackage** (Extract + Script) as a one-time external runner (ADR-0001: it provides what the native path can't), with `USE`, sqlcmd lines and `ALTER DATABASE` stripped |
| Postgres | everything from `pg_dump --schema-only --no-owner --no-privileges`, excluding functions, procedures and views | functions, procedures and views as `CREATE OR REPLACE` | `pg_dump` as a one-time external runner, plus native catalog reads |
| Mongo | collections, validators, indexes (native YAML: `create`, `createIndexes`) | none | native (`listCollections`, `listIndexes`) |
| Cosmos NoSQL | databases, containers (partition key, indexing policy, TTL, unique keys, throughput), stored procedures, UDFs, triggers (native YAML operations) | none | native SDK / ARM reads |

- **Never captured:** the migrate-db ledger, other migration tools' journal tables (for example `__EFMigrationsHistory`, `SchemaVersions`, `changelog`), and data. Journal tables stay in the database untouched; dropping them later is a normal migration. Security principals, grants and database-level settings are **reported but not scripted**. The output lists them so the team can move them to infrastructure as code or to an explicit migration.
- **Review:** the capture is a draft for review. The output summarizes object counts and anything it couldn't script.

**Fingerprint.** A normalized model of the schema, with one hash per object and an overall hash, computed natively from the catalog. `capture` writes it, and `compare` and `mark` recompute it live. Normalization ignores:
- the ledger and journal tables;
- object ids and creation dates;
- whitespace in module definitions;
- **system-generated constraint names** (for example `DF__Orders__Statu__3B75D760`). These differ between environments for the same schema. They're compared by definition, and `compare` warns about them, suggesting they be named explicitly in a later migration.

## 7. Engine rules

1. **Order:** up applies in ascending version order. Down reverts in descending order.
2. **Lock:** every mutating command holds a target-level exclusive lock for the whole run. Read-only commands (`status`, `validate`, `--dry-run`) don't take it.
3. **Pre-flight reconciliation** runs before anything executes. The run aborts with exit code 2 if any of these hold:
   - an applied **versioned** migration's checksum differs from its file (unless `repair --accept-checksums` has been run). **Repeatables are exempt:** for them a checksum difference is the trigger to re-apply (§3.6), not drift;
   - a `failed` or `in_progress` row exists that its `recovery` policy can't recover in this direction (§6.5);
   - for `up` and `goto` (the commands that apply pending migrations): there is a pending migration with version lower than the database version, and out-of-order is not allowed. `down` isn't blocked by out-of-order pending files, so an emergency rollback is never held up by an unrelated merge.

   An applied migration with no file (`missing-file`) **only warns**. Old files are sometimes squashed. A `down` past such a version is rejected when the plan is validated (rule 4).
4. **Plan is validated whole before step 1 runs.** For example, `down --steps 3` across an irreversible migration fails up front. It does not stop halfway.
5. **Stop on first failure.** The steps that already completed stay applied. Each step is its own unit; the tool does not attempt a cross-migration transaction.
6. **Timeouts:** a per-migration timeout comes from a directive or config. The default is none.
7. **Signals:** Ctrl-C finishes or rolls back the current step, releases the lock and exits with code 4.
   - **Which signals:** Ctrl-C and Ctrl-Break on Windows; SIGINT and SIGTERM on Linux (what pipeline agents send on cancel).
   - **First signal:** no further step starts. The running statement is interrupted: SQL Server gets a TDS ATTENTION, Postgres a protocol cancel request. The step then rolls back through its normal failure path, and its failure `kind` is `cancelled`. A wait for the lock is abandoned. The report lists the steps that completed.
   - **Second signal:** exits immediately with code 4. This is still safe, because the server rolls back the open transaction and drops the session lock when the connection closes. A non-transactional step cut off this way leaves its `in_progress` row (§6.3).
8. **Read-only commands never write.** `status`, `validate` and `--dry-run` skip both the lock and `ensure_ledger`. A missing ledger is treated as empty, so pointing `status` at a fresh database creates nothing.
9. **`goto <v>` is declarative.** Its target state is: *every known migration with version ≤ v is applied, and none above v is.*
   - **Valid targets:** `v` must be `0` or a version that exists as a file or in the ledger. Anything else is a usage error (exit 64), so a typo can't silently pick a different target.
   - **The plan has two parts:**
     1. **down** every applied migration with version > v, in descending order;
     2. **up** every pending migration with version ≤ v, in ascending order.
   - **Out-of-order pending migrations:** if any pending migration below the current database version is included, out-of-order must be allowed (`--allow-out-of-order` or `MIGRATE_DB_ALLOW_OUT_OF_ORDER`). Otherwise the run exits with code 2.
   - **Already there:** if the database is already at v, `goto` is a no-op and exits with code 0.
   - **Baseline floor:** a baseline has no down. So `goto` (or `down`) to a version below the baseline is rejected up front with exit code 2.
   - **Same checks as `up`/`down`:** the plan is validated whole before anything runs (rule 4), and it supports `--dry-run` and `--expect-plan` like `up` and `down`.
   - **Scope:** `up --to` and `down --to` remain, for callers that want to restrict the direction explicitly.
10. **Lock visibility.** Whoever holds the lock must be identifiable without special database permissions.
    - **Holder record.** After acquiring the lock, the holder writes a `LockHolder` record:
      - principal, OS user, host, pid, tool version;
      - **CI run reference**, taken from `BUILD_BUILDID` / `SYSTEM_TEAMFOUNDATIONCOLLECTIONURI` / `SYSTEM_TEAMPROJECT` or `GITHUB_RUN_ID` / `GITHUB_REPOSITORY`;
      - the database session id;
      - `acquired_at`.

      For lease locks this data lives in the lease document itself. For session locks it is a row in `__migrations_meta`, removed on release.
    - **Stale records.** A holder record is trusted only if the **native lock is actually held**. Each provider has a read-only test: `pg_locks` for Postgres, `APPLOCK_TEST` for SQL Server, `expiresAt > now` for leases. If the native lock is free, `status` reports `free (stale holder info from …)`. If the native lock is held but no record exists yet, it reports `held (holder unknown)`. That happens in the moment between acquiring and writing, or with an older tool version.
    - **Waiting.** While waiting for the lock, the engine logs the current holder every 15 s (on stderr, in every output mode). On `--lock-timeout` it exits with code 3 and names the holder in the message and in the JSON output.
    - **JSON.** `status --output json` includes:
      `"lock": {"state": "free|held", "holder": {...} | null, "since": "...", "age_s": n, "lease_expires_at": "..." | null, "stale_info": bool}`.
    - **DBA visibility.** Connections also set `application_name` (Postgres) / `APP_NAME` (SQL Server) to `migrate-db/<version> <ci-run>`.

## 8. Library API (secondary, not a v1 deliverable)

The tool is a pipeline CLI, and services do **not** migrate themselves on startup. The crate split still makes the engine usable from Rust code (tests, custom tooling), but v1 makes **no stability promise** for the library API and doesn't ship embedded migrations. A sketch for reference:

```rust
let provider = migrate_db_postgres::PostgresProvider::connect(&conn_str).await?;
let source = MigrationSource::from_dir("migrations")?;
let report = Engine::new(provider, source, EngineOptions::default())
    .up(Target::Latest)
    .await?;
```

## 9. Security

- No secrets in config files or logs. Connection strings are redacted in all output.
- Entra ID is supported for SQL Server, Postgres (Azure Flexible Server tokens) and Cosmos.
- Reverting is off unless the run turns it on (`--allow-down` / `MIGRATE_DB_ALLOW_DOWN`). Pipelines set it only in stages where reverting is safe; production stages leave it unset.
- The tool runs scripts with the privileges of the connecting principal. It does no privilege escalation.

## 10. Running locally and in pipelines

migrate-db runs as a step in Azure DevOps or GitHub Actions pipelines, separately from application deployment. It also runs on developer machines.

### 10.0 Local/pipeline parity

**The tool behaves the same on a laptop as on a pipeline agent.** It uses the same binary, config file, commands, plans, ledger writes, locks, exit codes and JSON output.
- **Allowed differences are presentational or interactive only:** prompts, confirmation, and the log format. Each one has a flag, so a developer can reproduce pipeline behavior exactly:
  - **`--ci`** (or `MIGRATE_DB_CI=1`) forces CI mode locally: no prompts, `--yes` required for destructive commands, and pipeline-style logs unless `--log-format` says otherwise.
  - `--log-format` and `--output` override any auto-detection.
- **No implicit environment loading.** Run settings come from flags and the process environment, plus an optional **`--env-file <path>`** that is loaded only when passed explicitly, in either place. A developer reproduces a pipeline stage by putting that stage's `MIGRATE_DB_*` values in a file. There is no automatic `.env` pickup, so a stray file can't change behavior on one machine but not another.
- **Same authentication path.** `MIGRATE_DB_AUTH=entra` uses one credential chain everywhere: the Azure CLI login locally (`az login`), and the service connection, workload identity or managed identity on agents. Config never contains "local-only" auth branches.
- **Same runner versions.** `runner.min_version` is enforced identically on laptops and agents. For exact parity, the release Docker image (Phase 5) can be run locally with the same tag the pipeline uses.
- Plan/apply (`--dry-run --output-file` → `--expect-plan`) works locally too.

### 10.1 Recommended stages

```
PR build ──► validate --offline                      (no DB connection needed)
         ──► rehearse --scratch --round-trip         (throwaway container / emulator)
   │
per stage (dev, qa, prod…; the stage sets MIGRATE_DB_CONNECTION / MIGRATE_DB_AUTH):
   plan   ──► up --dry-run --output json --output-file plan.json   (published as a pipeline artifact)
   gate   ──► stage approval (reviewers read plan.json / the rendered summary)
   apply  ──► up --expect-plan plan.json
```

- **`validate --offline` in PR builds** catches naming errors, duplicate versions, parse errors (`GO` splitting, YAML schema, JS syntax) and unresolvable placeholders before merge.
- **Plan and apply.** The JSON plan includes a `plan_fingerprint`: a SHA-256 over the current database version plus the ordered `(version, direction, checksum, executor)` steps. `up` / `down --expect-plan <file>` re-plans **under the lock**. If the fingerprint differs, it executes nothing and exits with code 2. That happens when someone else migrated in between, or the files changed. So what was approved is exactly what runs.

### 10.2 Non-interactive behavior

- The tool never prompts when there's no TTY or when it detects a CI environment (`TF_BUILD`, `GITHUB_ACTIONS`, `CI`).
- `down`, `repair`, `baseline` and `unlock --force` require `--yes` in that case, or they exit with code 64.
- Without `--allow-down` / `MIGRATE_DB_ALLOW_DOWN`, `down` is refused (exit 2) even with `--yes`.

### 10.3 Authentication in pipelines

- **Preferred:** Entra ID through the pipeline identity. This is an Azure DevOps service connection using workload identity federation, or GitHub OIDC. Run migrate-db inside an `AzureCLI@2` task (or after `azure/login` on GitHub) so the CLI login is available.
- **One credential chain everywhere** (§10.0), tried in order: workload identity (`AZURE_FEDERATED_TOKEN_FILE`), client secret (`AZURE_TENANT_ID` / `AZURE_CLIENT_ID` / `AZURE_CLIENT_SECRET`), Azure CLI / Azure Developer CLI login, then managed identity (self-hosted agents, Azure VMs; capped at 10 s). Locally that means `az login`. `MIGRATE_DB_AZURE_CREDENTIAL` pins one source when a machine has several. The failure message lists each source tried and why it failed.
- The identity needs a database user, for example `CREATE USER [<name>] FROM EXTERNAL PROVIDER`, with rights to match the commands it runs.
- SQL passwords and Cosmos keys come from variable groups or Key Vault as **environment variables**, never as arguments.

### 10.4 Cancellation and crashed runs

- When a pipeline run is cancelled, the engine handles the signal: the current transactional step is rolled back, the lock is released, and it exits with code 4.
- If the agent is killed hard:
  - SQL Server and Postgres release session locks (`sp_getapplock`, advisory locks) as soon as the connection drops, and roll back the open transaction.
  - Mongo and Cosmos lease locks expire after `lock.lease_ttl` (default 120 s, heartbeat every 30 s). The next run may wait up to that long. If the old holder was only stalled and is still alive, epoch fencing (§6.5) stops it from writing after the takeover.
  - A non-transactional step that was cut off mid-way leaves a `failed` row (§6.3).

### 10.5 Logs and machine output

- `--log-format auto|plain|azure-pipelines|github`. The `auto` default detects the CI system and emits collapsible groups per migration and error annotations: `##vso[task.logissue type=error]` / `::error::`.
- `--output-file <path>` writes the JSON result to a file so it doesn't mix with logs. It can be published as an artifact.

### 10.6 Agents and environment

- The binary comes from a versioned release artifact or a container image. Pipelines pin the version.
- If a target uses an external runner, `sqlcmd` or `mongosh` must be on the agent image at the configured `min_version` (§3.5).
- Databases behind private endpoints need **self-hosted agents** with network access to the VNet.

### 10.7 Concurrency

Overlapping pipeline runs can target the same database: re-runs, two releases, or parallel stages. **First line of defence:** an exclusive-lock check on the pipeline environment (the Azure DevOps "Exclusive lock" check, or GitHub `concurrency` groups). **Safety net:** the tool's own target lock (§7.2), which always applies.

## 11. Open questions

See [PLAN.md § Open questions](PLAN.md#open-questions).
