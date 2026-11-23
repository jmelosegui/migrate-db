# ADR-0004: Use Microsoft's `mssql-tds` as the SQL Server driver

- **Status:** Accepted (2026-11-17), after the container spike and an Azure SQL + Entra check (see "Azure SQL results").
- **Date:** 2026-11-17
- **Applies to:** `providers/mssql` (native executor).

## Context

PLAN.md Phase 0 asked for a spike comparing `tiberius` and `tiberius-ng`. While doing it we found a third candidate, Microsoft's own `mssql-tds` (`microsoft/mssql-rs`). The spike ran the same scenarios against all three on a `mssql/server:2022` container (SQL login, TLS with a self-signed certificate):

| Scenario | `tiberius` 0.13.0 | `tiberius-ng` 0.13.1 | `mssql-tds` 0.4.0 |
|---|---|---|---|
| `BEGIN TRAN` → several batches → `COMMIT`, visible from a second session | pass | pass | pass |
| Runtime error (245) under `XACT_ABORT ON` → rolled back, `@@TRANCOUNT = 0` | pass | pass | pass |
| Parse error (102) inside the transaction | transaction **stays open** (`@@TRANCOUNT = 1`) | same | same |
| `sp_getapplock` exclusive/session: second session gets `-1` after its 1 s timeout | pass | pass | pass |
| Lock holder visible in `sys.dm_tran_locks` ⨝ `sys.dm_exec_sessions` by `program_name` | pass | pass | pass |
| Lock released when the holder disconnects | pass | pass | pass |
| **Cancel a running statement** (`WAITFOR DELAY '00:00:15'` cancelled after 1 s) | no cancel API. Dropping the future doesn't reach the server: the next query blocks about 14 s and the transaction is still open. | same | `CancelHandle` sends ATTENTION: the call returns in 1.0 s, the transaction is rolled back and the connection can be reused. |
| Fallback: drop the whole connection mid-statement | rolled back, seen by another session in about 3 ms | same | n/a |
| Entra access token API | `AuthMethod::aad_token` (static token, redacted in `Debug`) | same | `ClientContext::access_token`, plus a token-factory hook (`auth_method_map`) for refresh |

Other factors:

| | `tiberius` | `tiberius-ng` | `mssql-tds` |
|---|---|---|---|
| Maintainer | Prisma | one community maintainer | Microsoft (`microsoft/mssql-rs`) |
| Release activity | 0.12.3 (2024-07), then 0.13.0 (2026-09) | three releases on 2026-08-29/30, none since | 0.2 → 0.3 → 0.4 between 2026-10-06 and 2026-11-07 |
| API stability | stable, small | stable, small (same API as tiberius) | pre-1.0, large, and each minor release may break it |
| TLS | rustls or native-tls | rustls or native-tls | native-tls only: Schannel on Windows, **OpenSSL on Linux** |
| Defaults | TDS 7.x | TDS 7.x / 8.0 | TDS 8.0 strict encryption by default |

Cancellation matters to us. SPEC §7 rule 7 requires clean behavior when a pipeline run is cancelled, and statement timeouts need the same mechanism. With tiberius, the only way to stop a running statement is to drop the connection. That still rolls back safely, but it can't tell "cancelled" apart from "connection lost", and it can't recover the session.

## Decision

1. **`providers/mssql` uses `mssql-tds`**, pinned to an exact version (`=0.4.x`). It's hidden behind a small internal client adapter inside the provider crate (connect, execute batch, scalar query, cancel, close), so API changes from upstream releases stay in one file.
2. **Cancellation and statement timeouts use `CancelHandle` / `ExecuteOptions::timeout`.** If cancel settlement fails, the driver retires the connection, and the provider treats that as a failed step (fail closed).
3. **After any error in a transactional migration, the provider always sends `IF @@TRANCOUNT > 0 ROLLBACK`** before writing the `failed` history row. The spike shows that `XACT_ABORT ON` does **not** roll back on compile errors such as 102, so the transaction would otherwise stay open on the session. This applies whichever driver we use.
4. **No fallback driver.** `tiberius` / `tiberius-ng` are not kept as an alternative (decided 2026-11-17). If verification finds a problem with `mssql-tds` (Entra on Azure SQL, Linux packaging), we fix it with or upstream to `mssql-tds`, rather than switching drivers. Switching would need a new ADR.

## Consequences

- **Good:** real server-side cancellation and timeouts; a driver maintained by Microsoft, with TDS 8.0 strict encryption and Entra token refresh built in; no ODBC.
- **Bad:**
  - Pre-1.0 churn. We pin exact versions and upgrade deliberately, through the adapter.
  - Linux builds link OpenSSL. CI agents need `libssl-dev`, the Docker image needs `libssl`, and a fully static (musl) binary would need the `openssl` crate's `vendored` feature. That decision is deferred to Phase 5 distribution.
  - A bigger dependency tree and API than tiberius.
- **Neutral:** `sp_getapplock` locking and the lock-holder query work the same on every driver, so lock visibility (SPEC §7 rule 10) can be built independently of this choice.

## Azure SQL results (2026-11-17)

These ran against a development Azure SQL Database reached through a private endpoint, with a user Entra token from the Azure CLI after MFA. The token was passed through `ClientContext::access_token` with `TdsAuthenticationMethod::AccessToken`. Everything was read-only: a `#temp` table, a session-scoped applock, permission lookups, and a cancelled `WAITFOR`.

| Check | Result |
|---|---|
| Login, **TDS 8 strict** | **Fails**: the server resets the TLS handshake (os error 10054), on both `master` and the user database. |
| Login, TDS 7.4 `Encrypt=On`, certificate validated | Pass |
| Multi-batch transaction on a `#temp` table | Pass |
| `sp_getapplock` contended: waiter gets `-1` after about 1.08 s | Pass |
| `APPLOCK_TEST` held test: `0` while held, `1` after release, with no extra permission | Pass |
| Holder lookup via `sys.dm_tran_locks` | Fails without `VIEW DATABASE PERFORMANCE STATE` (error 262/297) |
| Cancel (ATTENTION) over the network: returns in about 1.1 s, connection reusable | Pass |
| `KILL DATABASE CONNECTION` / `ALTER ANY CONNECTION` for a developer principal | Not granted (checked with `HAS_PERMS_BY_NAME`; `KILL` was never run) |

What this means for the provider:

1. **The default encryption mode is `Encrypt=Mandatory` (TDS 7.4 + TLS, certificate validated), not the driver's default of strict.** Strict is available as an explicit opt-in.
2. **Lock holder information comes from our own holder record** in the ledger meta table, not from DMVs, because pipeline identities usually lack `VIEW DATABASE PERFORMANCE STATE`. `APPLOCK_TEST` is the read-only held test, as PROVIDERS.md planned.
3. **`unlock --kill-holder` needs an elevated principal** on Azure SQL Database. Without the permission, the command must fail with a clear message, never silently skip.

## Still to verify

- Managed identity and workload identity tokens from `azure_identity` in a pipeline. They use the same wire path as the CLI token, so the risk is low.
- A Linux build in CI: OpenSSL linking and the native-tls ALPN feature.

The spike code was a throwaway crate outside the workspace. Its scenarios move into the `conformance` suite and the `providers/mssql` integration tests.

## Erratum (2026-11-18)

The spike's compile-error statement, `SELECT FROM WHERE`, raises **Msg 156** ("Incorrect syntax near the keyword 'FROM'"), not 102 as the table and decision 3 say. The finding doesn't change: a compile error (level 15) leaves the transaction open under `XACT_ABORT ON`. `providers/mssql`'s integration tests now assert this.
