# migrate-db: Adoption Plan

**Goal:** start using migrate-db on every existing database, whatever deploys its schema today: DacFx, migrations run at pod startup, scripts run by hand, anything else.

**The approach doesn't depend on the old tool.** The baseline is captured from the **live database**, not from the tool that built it. The only step that touches the old mechanism is switching it off. Background: [ADR-0002](decisions/0002-replace-dacfx-with-versioned-migrations.md) (DacFx) and [ADR-0003](decisions/0003-run-migrations-in-pipeline-not-at-startup.md) (no migrations at startup).

```
freeze ─► capture (from reference env) ─► compare (every env) ─► rehearse (scratch) ─► mark (every env) ─► switch on ─► first real migration
                                    nothing is executed against existing databases until here ──────────────────────────────┘
```

---

## Rules

1. **The reference environment is the source of truth.** Normally that's **prod**, because it is what really exists. The baseline describes it exactly.
2. **No drift before marking.** Every environment must match the captured baseline (`baseline compare`). `baseline mark` refuses to run otherwise.
3. **Cutover executes nothing against existing databases.** Capture, compare and mark only read the schema or write ledger rows.
4. **Reversible until the first real migration.** Until the first post-baseline migration is applied, the old mechanism can be switched back on. That migration is the **point of no return**, so keep it small and low-risk.
5. **One database at a time.** Start with the least risky one. Avoid release freezes and weeks with other big changes.
6. **Same commands locally and in the pipeline** (SPEC §10.0). Every step runs as a pipeline job with plan → approval → apply where it writes anything. It can be rehearsed locally against containers.

---

## Step 0: Inventory (no tool needed)

One row per database, in a list or issue tracker:

| Field | Why |
|---|---|
| Project / owner | Who freezes, reviews and approves |
| Technology (SQL Server, Postgres, Mongo, Cosmos NoSQL) | Decides which tool phase it waits for |
| Environments, and which one is the **reference** | Capture and compare targets |
| How the schema is deployed today, in free text | Only so you know what to switch off in steps 2 and 7 |
| Private endpoint? | Yes means a self-hosted agent (SPEC §10.6) |
| Size / hot tables | Planning later online changes |

**Output:** a prioritized list. Start with a small schema, low traffic and no release in the next few weeks.

---

## The process (per database)

### 1. Prepare
- Run `migrate-db init` in the folder that will hold the database's migrations. It writes `migrate-db.toml` (one database per folder, no environments: ADR-0005) and warns if it finds signs of another deploy mechanism. In the pipeline, give each stage its connection (`MIGRATE_DB_CONNECTION`, `MIGRATE_DB_AUTH`), plus a stage for a throwaway rehearsal container or emulator.
- Add the identity the pipeline will use. It needs read access to the schema (for example `VIEW DEFINITION` on SQL Server) in every environment, and DDL rights from step 7 on.

### 2. Freeze
- Announce the cutover window to anyone who deploys to this database, and **switch off whatever changes this schema today**: disable the deploy task, turn off startup migrations through config, stop manual scripts. From now until step 7 this database gets **no schema changes**, so keep the window short (days, not weeks).
- Why the freeze comes first: a mechanism that keeps deploying can invalidate the capture. Some, like dacpac deploys with "drop objects not in source", would even **drop the migrate-db ledger tables** created in step 6.

### 3. Capture
```
# with MIGRATE_DB_CONNECTION pointing at the reference database (usually prod)
migrate-db baseline capture --description baseline
```
This only reads. It writes `V<ts>__baseline.*`, `R__*` files for programmable objects (SQL Server, Postgres) and `V<ts>__baseline.fingerprint.json` (SPEC §6.6). Then review the draft like any PR:
- **Repeatables split:** each procedure, view and function is in its own `R__` file. Schema-bound and indexed views stay in the versioned baseline.
- **Reported but not scripted:** users and roles, grants, database-level settings. Decide where each one lives from now on, either infrastructure as code (preferred) or an explicit migration, and record the decision.
- **Old journals** (from whatever tool was used before) are left in the database untouched. They are dropped later in step 9.

Commit the files.

### 4. Compare every environment
```
# once per environment, each with that environment's MIGRATE_DB_CONNECTION
migrate-db baseline compare
```
Any difference must be fixed **in that environment**, with a reviewed one-off script, until `compare` is clean. If the difference shows prod is the odd one out, decide deliberately: fix prod, or re-capture. `compare` also warns about system-generated constraint names. Those aren't drift, but they're worth naming explicitly in a later migration.

### 5. Rehearse
```
# MIGRATE_DB_CONNECTION pointing at the empty scratch database
migrate-db rehearse --scratch
migrate-db baseline compare
```
This applies the baseline and repeatables to an empty scratch database, then proves the result matches the captured fingerprint. Add both commands to the PR pipeline from now on.

### 6. Mark every environment
Through the pipeline, in each existing environment's stage (its own `MIGRATE_DB_CONNECTION`):
```
migrate-db baseline mark --version <ts> --description baseline --mark-repeatables --dry-run --output-file plan.json
# approval
migrate-db baseline mark --version <ts> --description baseline --mark-repeatables --expect-plan plan.json
migrate-db status      # expect: baseline + repeatables applied, nothing pending
```
`mark` re-checks the fingerprint before writing, so an environment that drifted since step 4 is refused. It executes **no** scripts; it only writes ledger rows.

New environments (local, new regions, ephemeral) never get marked. They just run `migrate-db up`, which applies the baseline from scratch.

### 7. Switch on
- Add the migrate-db stages to the pipeline:
  - PR: `validate --offline`, `rehearse`;
  - per environment: plan → approval → apply (SPEC §10.1), before the application deploy.
- **Remove the old mechanism for good**: delete the disabled deploy task, remove startup migration code and packages, retire the old project files.
- If the application identity had DDL rights only because of startup migrations, **revoke them**. Only the pipeline identity keeps DDL rights.
- Tell developers: after pulling, run `migrate-db up` with their local `MIGRATE_DB_CONNECTION` (or `--env-file local.env`). New changes start with `migrate-db new "<description>"`.

### 8. Point of no return
Merge the **first small post-baseline migration** and deploy it through every environment. Ideal candidates are harmless, such as naming a system-generated constraint or adding a comment or extended property.

### 9. Clean up (one or two release cycles later)
- Drop the old tool's journal tables or collections with a normal migrate-db migration.
- Delete leftover project files from the repo.

---

## Rollback (before step 8 only)
1. Switch the old mechanism back on. If DDL rights were revoked in step 7, re-grant them first.
2. The ledger tables can stay, since nothing reads them, or be dropped with a manual script. A mechanism that drops unknown objects will remove them by itself.

After step 8 there is no rollback. Fix forward with migrations.

---

## Order of adoption

| Stage | Scope | Waits for |
|---|---|---|
| 0 | Inventory and cutover-window policy | nothing; start now |
| 1 | One SQL Server database and one Postgres database first | Phase 2 (capture, compare, mark, repeatables, rehearse) |
| 2 | Remaining SQL Server and Postgres databases | Phase 2 |
| 3 | Mongo databases | Phase 3 |
| 4 | Cosmos NoSQL databases | Phase 4 |
| 5 | Cleanup: old journals and project files gone; SqlPackage and other old tools removed from agent images | — |

After the first database of each technology, update this runbook with what you learned.

## Done, per database

- [ ] `baseline compare` is clean for every environment, and `status` shows `baseline` with nothing pending.
- [ ] The pipeline runs `validate` + `rehearse` on PRs and plan → approval → apply per environment.
- [ ] The old mechanism is removed, and the application identity has no DDL rights.
- [ ] The first post-baseline migration is in prod (point of no return passed).
- [ ] Old journals and project files are scheduled for deletion.
