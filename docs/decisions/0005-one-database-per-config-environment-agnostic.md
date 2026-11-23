# ADR-0005: One database per config, and no environments in the config

- **Status:** Accepted
- **Date:** 2026-11-18
- **Applies to:** `migrate-db.toml` (SPEC §5), the global CLI flags (SPEC §4), `init` (SPEC §5.2).

## Context

The first config design had two kinds of table:

- `[targets.<name>]`: several databases in one file, each with its own migrations folder (`migrations/<target>`), selected with `--target`.
- `[environments.<env>]` and `[environments.<env>.targets.<name>]`: per-environment connection strings (`${env:VAR}`), auth and safety switches (`allow_down`, `out_of_order`, `scratch`), selected with `--env`.

Trying `init` on a real project showed two problems:

1. **Environments in the repository duplicate the pipeline.** The pipeline already knows which stage it is deploying (dev, QA, prod). It owns the connection secrets and the approvals. Listing those environments in `migrate-db.toml` makes developers manage environments in the repo, and every new stage needs a config change. The tool runs as a pipeline step (ADR-0003), so the stage should decide.
2. **One folder per database is what teams want.** Each database already lives in its own project folder. `migrations/<target>` sub-folders and `--target` add a layer nobody uses.

## Decision

1. **One `migrate-db.toml` describes one database.** Its settings are flat top-level keys: `provider`, `migrations_dir` (default `migrations`), `versioning`, `strict_filenames`, `ledger`, `placeholders`, `executor`, `runner`, `new_format`. Migration files live directly in `migrations/`. There are no `[targets]` tables and no `--target` flag. For several databases, use several folders, each with its own config.
2. **The config is environment-agnostic.** It holds no connection strings, environment names or secrets. Each run gets its settings from flags or `MIGRATE_DB_*` variables, set per pipeline stage:

   | Setting | Flag | Variable | Default |
   |---|---|---|---|
   | Connection string | `--connection` | `MIGRATE_DB_CONNECTION` | none (required by commands that connect) |
   | Auth mode | `--auth sql\|entra` | `MIGRATE_DB_AUTH` | from the connection string |
   | Allow reverting | `--allow-down` | `MIGRATE_DB_ALLOW_DOWN` | **off** |
   | Allow out-of-order | `--allow-out-of-order` | `MIGRATE_DB_ALLOW_OUT_OF_ORDER` | **off** |

   `--env` is removed. `--env-file <path>` stays: it supplies the same `MIGRATE_DB_*` variables from a file, so a local run reproduces a pipeline stage exactly (SPEC §10.0). Process variables and flags win over the file.
3. **Reverting fails closed per run.** `down`, and `goto` plans that revert, are refused (exit 2) unless the run passes `--allow-down` or sets `MIGRATE_DB_ALLOW_DOWN`. In CI they also still need `--yes`. A pipeline turns reverting on only in stages where it is safe, for example dev.
4. **Old configs fail with directions.** A file that still has `[environments]`, `[targets]` or `[project]` is rejected with a message saying what replaces it. It is never silently half-read.
5. **`init` writes only the database settings.** It asks for the provider, the migrations folder, the ledger and the version scheme. It refuses to run where a config already exists, so extend mode is gone.
6. **Commands that will need a second database or a scratch database get explicit flags** when they are built, instead of environment names. `baseline compare --against-connection`, and `rehearse` with an explicit `--scratch` confirmation plus its own emptiness checks (SPEC §4.1).

## Consequences

- **Good:**
  - The repo holds only what the database *is*. Where and how to deploy stays in the pipeline, with its secrets, approvals and stages.
  - A new stage needs no config change.
  - Simpler config, CLI and `init`. No `${env:}` expansion in the config, because no config value is secret any more.
- **Bad:**
  - The production guard against reverting moves from a committed file (`allow_down = false` under `prod`) to pipeline settings. A pipeline author could set `MIGRATE_DB_ALLOW_DOWN` in the wrong stage. Mitigations: the default is off, CI still needs `--yes`, and pipeline approvals gate production.
  - Per-environment placeholder values (`placeholders = { schema = … }` under an environment) have no home yet. When placeholders are implemented, their values come from `MIGRATE_DB_PLACEHOLDER_<NAME>` variables (SPEC §3.3).
  - A JSON output change: `target` and `env` are no longer reported; `provider` is. There are no external consumers yet (pre-1.0).
- **Migration:** move the target's keys to the top level, delete `[environments]`, and set `MIGRATE_DB_CONNECTION` / `MIGRATE_DB_AUTH` / `MIGRATE_DB_ALLOW_DOWN` per pipeline stage. The error message for old files says exactly this.
