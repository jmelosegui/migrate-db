//! End-to-end CLI flow against a real PostgreSQL (SPEC §10.1 plan → apply).
//! Run: `MIGRATE_DB_TEST_POSTGRES=… cargo test -p migrate-db --features integration`
#![cfg(feature = "integration")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code

use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;

fn connection() -> String {
    std::env::var("MIGRATE_DB_TEST_POSTGRES").expect("set MIGRATE_DB_TEST_POSTGRES")
}

#[test]
fn plan_approve_apply_status_and_guarded_down() {
    let schema = format!("e2e_{}", std::process::id());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join(".git")).unwrap();
    fs::write(
        root.join("migrate-db.toml"),
        format!("provider = \"postgres\"\nledger = {{ schema = \"{schema}\" }}\n"),
    )
    .unwrap();
    // Everything per run comes from MIGRATE_DB_* variables, as in a pipeline.
    let cmd = || {
        let mut c = Command::cargo_bin("migrate-db").unwrap();
        c.current_dir(root)
            .env("MIGRATE_DB_CONNECTION", connection())
            .env("MIGRATE_DB_CI", "1")
            .env_remove("MIGRATE_DB_ALLOW_DOWN");
        c
    };

    // Author two migrations.
    cmd().args(["new", "create widgets"]).assert().success();
    cmd().args(["new", "add widget color"]).assert().success();
    let mut ups: Vec<_> = fs::read_dir(root.join("migrations"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    ups.sort();
    for path in &ups {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let sql = match (name.contains("create_widgets"), name.ends_with(".up.sql")) {
            (true, true) => format!(
                "CREATE SCHEMA IF NOT EXISTS {schema}; CREATE TABLE {schema}.widgets (id int);"
            ),
            (true, false) => format!("DROP TABLE {schema}.widgets;"),
            (false, true) => format!("ALTER TABLE {schema}.widgets ADD COLUMN color text;"),
            (false, false) => format!("ALTER TABLE {schema}.widgets DROP COLUMN color;"),
        };
        fs::write(path, sql).unwrap();
    }

    // Plan → approve → apply.
    cmd()
        .args([
            "up",
            "--dry-run",
            "--output",
            "json",
            "--output-file",
            "plan.json",
        ])
        .assert()
        .success();
    let plan: Value =
        serde_json::from_str(&fs::read_to_string(root.join("plan.json")).unwrap()).unwrap();
    assert_eq!(plan["plan"]["steps"].as_array().unwrap().len(), 2);
    let out = cmd()
        .args(["up", "--expect-plan", "plan.json", "--output", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let doc: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(doc["report"]["completed"].as_array().unwrap().len(), 2);

    // The approved plan is now stale: applying it again must refuse (exit 2).
    cmd()
        .args(["up", "--expect-plan", "plan.json"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("plan changed"));

    // Status shows both applied.
    let out = cmd()
        .args(["status", "--output", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status: Value = serde_json::from_slice(&out).unwrap();
    let states: Vec<&str> = status["status"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["state"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["applied", "applied"]);
    cmd().args(["validate"]).assert().success();

    // Down in CI needs --yes, on top of --allow-down.
    cmd()
        .args(["down", "--allow-down"])
        .assert()
        .code(64)
        .stderr(predicate::str::contains("--yes"));
    cmd()
        .args(["down", "--yes"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--allow-down"));
    cmd()
        .args(["down", "--yes", "--allow-down"])
        .assert()
        .success();
    cmd()
        .args(["goto", "0", "--yes", "--allow-down"])
        .assert()
        .success();
    let out = cmd()
        .args(["status", "--output", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status: Value = serde_json::from_slice(&out).unwrap();
    assert!(status["status"]["database_version"].is_null());
}

#[test]
fn baseline_mark_records_without_executing_then_up_continues() {
    let schema = format!("e2e_baseline_{}", std::process::id());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join(".git")).unwrap();
    fs::write(
        root.join("migrate-db.toml"),
        format!("provider = \"postgres\"\nledger = {{ schema = \"{schema}\" }}\n"),
    )
    .unwrap();
    fs::create_dir(root.join("migrations")).unwrap();
    // The baseline stands for a schema the database already has. It is not
    // valid SQL, so the test fails if anything ever executes it.
    fs::write(
        root.join("migrations/V1__baseline.up.sql"),
        "this would fail if baseline mark executed it;\n",
    )
    .unwrap();
    fs::write(
        root.join("migrations/V2__widgets.up.sql"),
        format!("CREATE SCHEMA IF NOT EXISTS {schema}; CREATE TABLE {schema}.widgets (id int);"),
    )
    .unwrap();
    let cmd = || {
        let mut c = Command::cargo_bin("migrate-db").unwrap();
        c.current_dir(root)
            .env("MIGRATE_DB_CONNECTION", connection())
            .env("MIGRATE_DB_CI", "1");
        c
    };

    // Unverified marking must be asked for explicitly (exit 2), and CI needs --yes (exit 64).
    cmd()
        .args(["baseline", "mark", "--version", "1"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--force-unverified"));
    cmd()
        .args(["baseline", "mark", "--version", "1", "--force-unverified"])
        .assert()
        .code(64);

    // Plan → approve → mark.
    cmd()
        .args([
            "baseline",
            "mark",
            "--version",
            "1",
            "--dry-run",
            "--output",
            "json",
            "--output-file",
            "plan.json",
        ])
        .assert()
        .success();
    let out = cmd()
        .args([
            "baseline",
            "mark",
            "--version",
            "1",
            "--force-unverified",
            "--yes",
            "--expect-plan",
            "plan.json",
            "--output",
            "json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let doc: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(doc["report"]["plan"]["steps"][0]["kind"], "baseline");

    let out = cmd()
        .args(["status", "--output", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status: Value = serde_json::from_slice(&out).unwrap();
    let entries = status["status"]["entries"].as_array().unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e["version"] == "1" && e["state"] == "baseline"),
        "{entries:?}"
    );

    // `up` continues after the baseline: only V2 runs.
    let out = cmd()
        .args(["up", "--output", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let up: Value = serde_json::from_slice(&out).unwrap();
    let done = up["report"]["completed"].as_array().unwrap();
    assert_eq!(done.len(), 1, "{done:?}");

    // A managed database is never re-baselined.
    cmd()
        .args([
            "baseline",
            "mark",
            "--version",
            "1",
            "--force-unverified",
            "--yes",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("empty ledger"));
}
