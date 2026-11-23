//! `migrate-db init` in non-interactive mode (no database needed).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;

/// An empty project folder (a git repo, so config discovery stays inside).
fn empty_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    dir
}

fn cmd(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("migrate-db").unwrap();
    cmd.current_dir(dir);
    for var in [
        "MIGRATE_DB_CONFIG",
        "MIGRATE_DB_CONNECTION",
        "MIGRATE_DB_AUTH",
        "MIGRATE_DB_ALLOW_DOWN",
        "MIGRATE_DB_CI",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

fn init(dir: &Path) -> Command {
    let mut c = cmd(dir);
    c.arg("init");
    c
}

fn json(output: &[u8]) -> Value {
    serde_json::from_slice(output).unwrap()
}

#[test]
fn writes_a_flat_environment_agnostic_config_and_the_migrations_folder() {
    let dir = empty_project();
    let out = init(dir.path())
        .args(["--provider", "mssql", "--output", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let doc = json(&out);
    assert_eq!(doc["validate"]["ok"], true);
    assert_eq!(doc["migrations_dir"], "migrations");
    let text = fs::read_to_string(dir.path().join("migrate-db.toml")).unwrap();
    for expected in [
        "provider = \"mssql\"",
        "migrations_dir = \"migrations\"",
        "versioning = \"timestamp\"",
        "ledger = { schema = \"dbo\", table = \"__migrations\" }",
        "MIGRATE_DB_CONNECTION",
    ] {
        assert!(text.contains(expected), "missing {expected}:\n{text}");
    }
    for absent in ["[environments", "[targets", "connection =", "allow_down"] {
        assert!(!text.contains(absent), "unexpected {absent}:\n{text}");
    }
    assert!(dir.path().join("migrations/.gitkeep").exists());
    assert!(
        doc["next_steps"]
            .to_string()
            .contains("MIGRATE_DB_CONNECTION")
    );

    // The project is usable right away; files land directly in migrations/.
    cmd(dir.path())
        .args(["new", "create orders"])
        .assert()
        .success();
    let names: Vec<String> = fs::read_dir(dir.path().join("migrations"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|n| n.ends_with("__create_orders.up.sql")),
        "{names:?}"
    );
    cmd(dir.path())
        .args(["validate", "--offline"])
        .assert()
        .success();
}

#[test]
fn without_a_provider_lists_the_missing_flag() {
    let dir = empty_project();
    init(dir.path())
        .assert()
        .code(64)
        .stderr(predicate::str::contains("--provider").and(predicate::str::contains("--yes")));
    assert!(
        !dir.path().join("migrate-db.toml").exists(),
        "nothing written"
    );
}

#[test]
fn takes_a_project_path_argument_and_custom_answers() {
    let parent = empty_project();
    let project = parent.path().join("ms-sql");
    fs::create_dir(&project).unwrap();
    init(parent.path())
        .arg(project.to_str().unwrap())
        .args([
            "--provider",
            "postgres",
            "--dir",
            "db\\scripts",
            "--ledger",
            "ops.ledger",
            "--versioning",
            "sequential",
        ])
        .assert()
        .success();
    let text = fs::read_to_string(project.join("migrate-db.toml")).unwrap();
    assert!(text.contains("migrations_dir = \"db/scripts\""), "{text}");
    assert!(
        text.contains("schema = \"ops\", table = \"ledger\""),
        "{text}"
    );
    assert!(text.contains("versioning = \"sequential\""), "{text}");
    assert!(
        !text.contains("executor"),
        "postgres has no executor key: {text}"
    );
    assert!(project.join("db/scripts/.gitkeep").exists());
    assert!(!parent.path().join("migrate-db.toml").exists());
}

#[test]
fn refuses_a_folder_that_is_already_set_up() {
    let dir = empty_project();
    init(dir.path())
        .args(["--provider", "mssql"])
        .assert()
        .success();
    let path = dir.path().join("migrate-db.toml");
    let before = fs::read_to_string(&path).unwrap();
    init(dir.path())
        .args(["--provider", "postgres"])
        .assert()
        .code(64)
        .stderr(predicate::str::contains("one database"));
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        before,
        "never overwritten"
    );
}

#[test]
fn detects_existing_tooling_and_suggests_the_provider() {
    let dir = empty_project();
    fs::create_dir(dir.path().join("Db")).unwrap();
    fs::write(dir.path().join("Db/Orders.sqlproj"), "<Project/>").unwrap();
    fs::write(dir.path().join("Db/tables.sql"), "CREATE TABLE t (id int);").unwrap();
    let out = init(dir.path())
        .args(["--yes", "--output", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let doc = json(&out);
    assert_eq!(doc["provider"], "mssql", "suggested from the .sqlproj");
    assert!(
        doc["detected"].to_string().contains("Orders.sqlproj"),
        "{doc}"
    );
    let warnings = doc["warnings"].to_string();
    assert!(
        warnings.contains("baseline") && warnings.contains("don't match"),
        "{warnings}"
    );
    assert!(doc["next_steps"].to_string().contains("ADOPTION"), "{doc}");
    assert_eq!(
        fs::read_to_string(dir.path().join("Db/Orders.sqlproj")).unwrap(),
        "<Project/>",
        "detection modifies nothing"
    );
}

#[test]
fn rejects_unavailable_providers_and_bad_answers() {
    let dir = empty_project();
    for args in [
        &["--provider", "cosmos"][..],
        &["--provider", "mssql", "--dir", "../outside"][..],
        &["--provider", "mssql", "--ledger", "nodot"][..],
        &["--provider", "mssql", "--executor", "sqlcmd"][..],
    ] {
        init(dir.path()).args(args).assert().code(64);
    }
    assert!(!dir.path().join("migrate-db.toml").exists());
}
