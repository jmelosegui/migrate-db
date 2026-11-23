//! The provider contract, as one test suite every provider must pass
//! (ARCHITECTURE testing strategy, AGENTS.md "Testing expectations").
//!
//! A provider's test crate implements [`Harness`] for its database (how to
//! open a connection to an isolated target, the few SQL snippets the cases
//! need, and an independent assertion connection) and expands
//! [`conformance_tests!`], which generates one `#[tokio::test]` per case in
//! [`cases`]:
//!
//! ```ignore
//! migrate_db_conformance::conformance_tests!(MyHarness::from_env());
//! ```
//!
//! Every case creates its own target, so cases run in parallel. They assert
//! only what SPEC and the `Provider` docs promise for every database;
//! dialect details (`GO` batches, Postgres `CONCURRENTLY`) stay in each
//! provider's own integration tests.

#![forbid(unsafe_code)]
// Test-support crate: failing an assertion is how a case reports.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod cases;

use async_trait::async_trait;
use migrate_db_core::provider::Provider;

/// One isolated target: a namespace (schema) that holds the case's objects
/// and its ledger, so it has its own lock too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Schema name, unique per case.
    pub namespace: String,
}

/// A history row, as the cases inspect it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRow {
    /// `version` column (`R__name` for repeatables, empty for unlock events).
    pub version: String,
    /// `event` column, e.g. `up`, `force-unlock`.
    pub event: String,
    /// `succeeded` or `failed`.
    pub status: String,
}

/// What a provider's test crate supplies to run the suite.
#[async_trait]
pub trait Harness: Send + Sync {
    /// A new target that doesn't exist in the database yet.
    fn new_target(&self) -> Target;

    /// A new provider connection to `target`, labelled `application`
    /// (`application_name` / `APP_NAME`).
    async fn connect(&self, target: &Target, application: &str) -> Box<dyn Provider>;

    // --- Assertions, through a connection independent of the provider ---

    /// Does table `name` exist in the target's namespace?
    async fn table_exists(&self, target: &Target, name: &str) -> bool;

    /// Does any ledger storage exist (applied, history or meta table)?
    async fn ledger_storage_exists(&self, target: &Target) -> bool;

    /// History rows, oldest first.
    async fn history(&self, target: &Target) -> Vec<HistoryRow>;

    /// Rewrite the stored lock-holder record so it names a live session
    /// that does **not** hold the lock (the assertion connection's own).
    async fn point_holder_at_another_session(&self, target: &Target);

    // --- Dialect: the SQL each case runs ---

    /// Create table `name` (one int column) in the namespace.
    fn create_table(&self, target: &Target, name: &str) -> String;

    /// Drop table `name`.
    fn drop_table(&self, target: &Target, name: &str) -> String;

    /// Create table `name`, then fail with a runtime error in a later
    /// statement of the same script.
    fn create_then_fail(&self, target: &Target, name: &str) -> String;

    /// Create table `name`, then run for `secs` seconds.
    fn create_then_sleep(&self, target: &Target, name: &str, secs: u32) -> String;

    /// An idempotent definition of view `name` returning `marker` (for
    /// repeatables): `CREATE OR ALTER` / `CREATE OR REPLACE`.
    fn define_view(&self, target: &Target, name: &str, marker: i32) -> String;

    /// Does view `name` exist and return `marker`?
    async fn view_returns(&self, target: &Target, name: &str, marker: i32) -> bool;
}

/// Generate one `#[tokio::test]` per contract case, each calling
/// [`cases`]`::<case>(&harness)`. `$harness` is evaluated once per test.
#[macro_export]
macro_rules! conformance_tests {
    ($harness:expr) => {
        $crate::conformance_tests!(@cases $harness;
            read_only_operations_create_nothing,
            ensure_ledger_is_idempotent,
            up_and_down_record_ledger_and_history,
            failing_transactional_step_rolls_back_completely,
            failing_non_transactional_step_leaves_a_blocking_failed_row,
            lock_is_exclusive_and_dies_with_its_session,
            holder_record_is_visible_and_stale_after_a_crash,
            waiting_run_reports_the_holder,
            force_unlock_follows_the_contract,
            repeatables_reapply_only_when_changed,
            cancel_interrupts_the_running_step_and_rolls_back,
            baseline_mark_writes_rows_and_executes_nothing,
        );
    };
    (@cases $harness:expr; $($case:ident),* $(,)?) => {
        $(
            #[tokio::test]
            async fn $case() {
                $crate::cases::$case(&$harness).await;
            }
        )*
    };
}
