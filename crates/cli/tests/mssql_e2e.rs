//! End-to-end CLI flow against a real SQL Server (SPEC §10.1 plan → apply).
//! Run: `MIGRATE_DB_TEST_MSSQL=… cargo test -p migrate-db --features integration`
//! (see `crates/provider-mssql/tests/integration.rs` for the container setup).
#![cfg(all(feature = "integration", feature = "mssql"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code

use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;

fn connection() -> String {
    std::env::var("MIGRATE_DB_TEST_MSSQL").expect("set MIGRATE_DB_TEST_MSSQL")
}

#[test]
fn plan_approve_apply_status_and_guarded_down() {
    let schema = format!("e2e_{}", std::process::id());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join(".git")).unwrap();
    fs::write(
        root.join("migrate-db.toml"),
        format!("provider = \"mssql\"\nledger = {{ schema = \"{schema}\" }}\n"),
    )
    .unwrap();
    // Everything per run comes from MIGRATE_DB_* variables, as in a pipeline.
    let cmd = || {
        let mut c = Command::cargo_bin("migrate-db").unwrap();
        c.current_dir(root)
            .env("MIGRATE_DB_CONNECTION", connection())
            .env("MIGRATE_DB_AUTH", "sql")
            .env("MIGRATE_DB_CI", "1")
            .env_remove("MIGRATE_DB_ALLOW_DOWN");
        c
    };

    cmd().args(["new", "create widgets"]).assert().success();
    cmd().args(["new", "add widget proc"]).assert().success();
    let mut files: Vec<_> = fs::read_dir(root.join("migrations"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    for path in &files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        // The ledger schema is created by migrate-db before the first step.
        let sql = match (name.contains("create_widgets"), name.ends_with(".up.sql")) {
            (true, true) => format!("CREATE TABLE [{schema}].widgets (id int);\n"),
            (true, false) => format!("DROP TABLE [{schema}].widgets;\n"),
            (false, true) => format!(
                "ALTER TABLE [{schema}].widgets ADD color nvarchar(20) NULL;\nGO\n\
                 CREATE PROCEDURE [{schema}].list_widgets AS SELECT id, color FROM [{schema}].widgets;\nGO\n"
            ),
            (false, false) => format!(
                "DROP PROCEDURE [{schema}].list_widgets;\nGO\nALTER TABLE [{schema}].widgets DROP COLUMN color;\n"
            ),
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

    cmd()
        .args(["up", "--expect-plan", "plan.json"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("plan changed"));

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

    // Reverting is off unless this run allows it (exit 2), and CI also needs --yes.
    cmd()
        .args(["down", "--yes"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--allow-down"));
    cmd()
        .args(["down", "--allow-down"])
        .assert()
        .code(64)
        .stderr(predicate::str::contains("--yes"));
    cmd()
        .env("MIGRATE_DB_ALLOW_DOWN", "1")
        .args(["down", "--yes"])
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

    // A wrong password is an auth failure and never echoes the secret.
    let bad = connection().replace("Password=", "Password=WRONG-");
    cmd()
        .env("MIGRATE_DB_CONNECTION", &bad)
        .args(["status"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("18456").and(predicate::str::contains("WRONG-").not()));
}

#[test]
fn init_with_test_connection_is_read_only() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    let ledger = format!("init_probe_{}.__migrations", std::process::id());
    let out = Command::cargo_bin("migrate-db")
        .unwrap()
        .current_dir(dir.path())
        .env("MIGRATE_DB_CONNECTION", connection())
        .env_remove("MIGRATE_DB_CI")
        .args([
            "init",
            "--provider",
            "mssql",
            "--ledger",
            &ledger,
            "--test-connection",
            "--output",
            "json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let doc: Value = serde_json::from_slice(&out).unwrap();
    let test = &doc["connection_test"];
    assert_eq!(test["ok"], true, "{doc}");
    assert!(
        test["summary"].as_str().unwrap().contains("lock free"),
        "{doc}"
    );

    // An unreachable server fails with exit 5 in non-interactive mode.
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    Command::cargo_bin("migrate-db")
        .unwrap()
        .current_dir(dir.path())
        .env(
            "MIGRATE_DB_CONNECTION",
            "Server=tcp:127.0.0.1,1;Database=x;User ID=u;Password=p;Connect Timeout=2",
        )
        .env_remove("MIGRATE_DB_CI")
        .args(["init", "--provider", "mssql", "--test-connection"])
        .assert()
        .code(5);
}

#[test]
fn placeholders_come_from_the_run_and_missing_values_stop_it() {
    let schema = format!("ph_{}", std::process::id());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join(".git")).unwrap();
    fs::create_dir(root.join("migrations")).unwrap();
    fs::write(
        root.join("migrate-db.toml"),
        format!(
            "provider = \"mssql\"\nledger = {{ schema = \"{schema}\" }}\nplaceholders = {{ enabled = true }}\n"
        ),
    )
    .unwrap();
    fs::write(
        root.join("migrations").join("V1__t.up.sql"),
        "CREATE TABLE [${schema}].[${table}] (id int);\n",
    )
    .unwrap();
    let cmd = || {
        let mut c = Command::cargo_bin("migrate-db").unwrap();
        c.current_dir(root)
            .env("MIGRATE_DB_CONNECTION", connection())
            .env("MIGRATE_DB_CI", "1")
            .env("MIGRATE_DB_PLACEHOLDER_SCHEMA", &schema)
            .env_remove("MIGRATE_DB_PLACEHOLDER_TABLE");
        c
    };
    // Offline validate only checks syntax: no values needed.
    cmd().args(["validate", "--offline"]).assert().success();
    // A missing value: exit 2, nothing applied, names the variable.
    cmd()
        .arg("up")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("MIGRATE_DB_PLACEHOLDER_TABLE"));
    cmd().args(["validate"]).assert().code(2);
    // With the value (here from an --env-file), it applies.
    fs::write(
        root.join("stage.env"),
        "MIGRATE_DB_PLACEHOLDER_TABLE=orders\n",
    )
    .unwrap();
    cmd()
        .args(["up", "--env-file", "stage.env"])
        .assert()
        .success();
    let out = cmd()
        .args(["status", "--output", "json", "--env-file", "stage.env"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(status["status"]["entries"][0]["state"], "applied");
}

#[test]
fn unlock_force_needs_yes_in_ci_clears_stale_records_and_kills_on_request() {
    use migrate_db_core::provider::{LockOptions, Provider};
    use migrate_db_mssql::MssqlProvider;
    let schema = format!("e2e_unlock_{}", std::process::id());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join(".git")).unwrap();
    fs::create_dir(root.join("migrations")).unwrap();
    fs::write(
        root.join("migrate-db.toml"),
        format!("provider = \"mssql\"\nledger = {{ schema = \"{schema}\" }}\n"),
    )
    .unwrap();
    fs::write(root.join("migrations/V1__one.up.sql"), "SELECT 1;\n").unwrap();
    let cmd = || {
        let mut c = Command::cargo_bin("migrate-db").unwrap();
        c.current_dir(root)
            .env("MIGRATE_DB_CONNECTION", connection())
            .env("MIGRATE_DB_AUTH", "sql")
            .env("MIGRATE_DB_CI", "1");
        c
    };
    let unlock_json = |args: &[&str], code: i32| -> Value {
        let out = cmd()
            .args(["unlock", "--output", "json"])
            .args(args)
            .assert()
            .code(code)
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice(&out).unwrap()
    };
    cmd().arg("up").assert().success();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let ledger: toml::Table = toml::from_str(&format!("schema = \"{schema}\"")).unwrap();
    let connect = |app: &'static str| {
        rt.block_on(MssqlProvider::connect(&connection(), &ledger, app, None))
            .unwrap()
    };
    let lock = LockOptions {
        timeout: std::time::Duration::from_secs(1),
        owner: "e2e-holder".into(),
    };

    // `--force` is required; free lock: nothing to do.
    cmd().arg("unlock").assert().code(64);
    let free = unlock_json(&["--force"], 0);
    assert_eq!(free["result"]["action"], "nothing-to-do");

    // Stale record from a crashed run: CI needs --yes.
    let mut crashed = connect("migrate-db/e2e-crashed");
    rt.block_on(crashed.acquire_lock(&lock)).unwrap();
    drop(crashed);
    std::thread::sleep(std::time::Duration::from_millis(300));
    cmd()
        .args(["unlock", "--force"])
        .assert()
        .code(64)
        .stderr(predicate::str::contains("--yes"));
    let cleared = unlock_json(&["--force", "--yes"], 0);
    assert_eq!(cleared["result"]["action"], "cleared-stale");
    assert_eq!(cleared["lock_before"]["stale_info"], true);

    // Held: exit 3 without --kill-holder, killed with it.
    let mut holder = connect("migrate-db/e2e-holder");
    rt.block_on(holder.acquire_lock(&lock)).unwrap();
    cmd()
        .args(["unlock", "--force", "--yes"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("--kill-holder"));
    let killed = unlock_json(&["--force", "--kill-holder", "--yes"], 0);
    assert_eq!(killed["result"]["action"], "killed-holder");
    assert_eq!(killed["result"]["holder"]["os_user"], "e2e-holder");
    cmd()
        .args(["status", "--output", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"state\":\"free\""));
}
