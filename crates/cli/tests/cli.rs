//! End-to-end tests of the `migrate-db` binary (no database needed).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;

/// A temp "repo" (with `.git` so discovery can't escape) holding a config.
struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new(config: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join("migrate-db.toml"), config).unwrap();
        Self { dir }
    }

    fn postgres() -> Self {
        Self::new("provider = \"postgres\"\n")
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn migrations(&self) -> PathBuf {
        self.path().join("migrations")
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("migrate-db").unwrap();
        cmd.current_dir(self.path());
        for var in [
            "MIGRATE_DB_CONFIG",
            "MIGRATE_DB_CONNECTION",
            "MIGRATE_DB_AUTH",
            "MIGRATE_DB_ALLOW_DOWN",
            "MIGRATE_DB_ALLOW_OUT_OF_ORDER",
            "MIGRATE_DB_CI",
        ] {
            cmd.env_remove(var);
        }
        cmd
    }

    fn files(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.migrations())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

fn json(output: &[u8]) -> Value {
    serde_json::from_slice(output).unwrap()
}

#[test]
fn new_creates_an_up_down_pair_with_a_timestamp_version() {
    let p = Project::postgres();
    p.cmd()
        .args(["new", "Create orders table"])
        .assert()
        .success()
        .stdout(predicate::str::contains("created"));
    let files = p.files();
    assert_eq!(files.len(), 2, "{files:?}");
    assert!(files[0].starts_with('V') && files[0].ends_with("__create_orders_table.down.sql"));
    assert!(files[1].ends_with("__create_orders_table.up.sql"));
    let version = &files[0][1..15];
    assert!(version.chars().all(|c| c.is_ascii_digit()), "{version}");
}

#[test]
fn new_never_reuses_a_version_even_within_one_second() {
    let p = Project::postgres();
    for d in ["one", "two", "three"] {
        p.cmd().args(["new", d]).assert().success();
    }
    let ups: Vec<String> = p
        .files()
        .into_iter()
        .filter(|f| f.ends_with(".up.sql"))
        .collect();
    let mut versions: Vec<&str> = ups.iter().map(|f| &f[1..f.find("__").unwrap()]).collect();
    versions.dedup();
    assert_eq!(versions.len(), 3, "{ups:?}");
}

#[test]
fn new_irreversible_creates_only_the_up_file_and_json_lists_it() {
    let p = Project::postgres();
    let out = p
        .cmd()
        .args(["new", "drop legacy", "--irreversible", "--output", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let doc = json(&out);
    assert_eq!(doc["schema"], 1);
    assert_eq!(doc["command"], "new");
    assert_eq!(doc["files"].as_array().unwrap().len(), 1);
    assert_eq!(p.files().len(), 1);
}

#[test]
fn new_works_from_a_subdirectory() {
    let p = Project::postgres();
    let sub = p.path().join("deep").join("er");
    fs::create_dir_all(&sub).unwrap();
    p.cmd()
        .current_dir(&sub)
        .args(["new", "from below"])
        .assert()
        .success();
    assert_eq!(
        p.files().len(),
        2,
        "paths resolve relative to the config file"
    );
}

#[test]
fn validate_offline_passes_on_good_files() {
    let p = Project::postgres();
    p.cmd().args(["new", "first"]).assert().success();
    let out = p
        .cmd()
        .args(["validate", "--offline", "--output", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let doc = json(&out);
    assert_eq!(doc["ok"], true);
    assert_eq!(doc["migrations"], 1);
}

#[test]
fn validate_offline_fails_with_exit_2_and_lists_problems() {
    let p = Project::postgres();
    fs::create_dir_all(p.migrations()).unwrap();
    fs::write(p.migrations().join("V1__a.up.sql"), "select 1;").unwrap();
    fs::write(p.migrations().join("V01__b.up.sql"), "select 2;").unwrap();
    fs::write(
        p.migrations().join("V2__c.up.sql"),
        "-- migrate:bogus\nselect 3;",
    )
    .unwrap();
    let out = p
        .cmd()
        .args(["validate", "--offline", "--output", "json"])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let doc = json(&out);
    assert_eq!(doc["ok"], false);
    assert_eq!(doc["errors"].as_array().unwrap().len(), 2);
}

#[test]
fn missing_config_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    Command::cargo_bin("migrate-db")
        .unwrap()
        .current_dir(dir.path())
        .env_remove("MIGRATE_DB_CONFIG")
        .args(["validate", "--offline"])
        .assert()
        .code(64)
        .stderr(predicate::str::contains("migrate-db init"));
}

#[test]
fn bad_arguments_are_usage_errors() {
    let p = Project::postgres();
    p.cmd().args(["up", "--bogus"]).assert().code(64);
    p.cmd().args(["goto", "abc"]).assert().code(64);
    // Environments are gone (ADR-0005): --env is not a flag any more.
    p.cmd().args(["status", "--env", "dev"]).assert().code(64);
}

#[test]
fn commands_that_connect_say_how_to_provide_the_connection() {
    let p = Project::postgres();
    fs::create_dir(p.migrations()).unwrap();
    p.cmd()
        .arg("status")
        .assert()
        .code(64)
        .stderr(predicate::str::contains("MIGRATE_DB_CONNECTION"));
}

#[test]
fn errors_are_json_documents_in_json_mode() {
    let p = Project::postgres();
    fs::create_dir(p.migrations()).unwrap();
    let out = p
        .cmd()
        .args(["status", "--output", "json"])
        .assert()
        .code(64)
        .get_output()
        .stdout
        .clone();
    let doc = json(&out);
    assert_eq!(doc["command"], "status");
    assert_eq!(doc["error"]["exit_code"], 64);
}

#[test]
fn earlier_config_shapes_are_rejected_with_guidance() {
    let p = Project::new(
        "[targets.app]\nprovider = \"postgres\"\nmigrations_dir = \"m\"\n\n\
         [environments.dev.targets.app]\nconnection = \"${env:X}\"\n",
    );
    p.cmd()
        .args(["validate", "--offline"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("ADR-0005"));
}

#[test]
fn env_file_supplies_the_connection() {
    let p = Project::postgres();
    fs::create_dir(p.migrations()).unwrap();
    fs::write(
        p.path().join("local.env"),
        "MIGRATE_DB_CONNECTION=postgres://postgres@127.0.0.1:1/none?connect_timeout=2\n",
    )
    .unwrap();
    // The connection comes from the file; connecting to a closed port then fails (exit 5).
    p.cmd()
        .args(["status", "--env-file", "local.env"])
        .assert()
        .code(5)
        .stderr(predicate::str::contains("cannot connect"));
}

#[test]
fn ci_env_accepts_boolish_values() {
    let p = Project::postgres();
    for v in ["1", "true", "yes", "0"] {
        p.cmd()
            .env("MIGRATE_DB_CI", v)
            .args(["new", &format!("ci {v}")])
            .assert()
            .success();
    }
}

#[cfg(feature = "mssql")]
#[test]
fn validate_offline_catches_a_bad_go_line_on_sql_server() {
    let p = Project::new("provider = \"mssql\"\n");
    fs::create_dir(p.migrations()).unwrap();
    fs::write(
        p.migrations().join("V1__ok.up.sql"),
        "SELECT 1;\nGO\nSELECT 2;\nGO 3\n",
    )
    .unwrap();
    fs::write(p.migrations().join("V2__bad.up.sql"), "SELECT 1;\nGO 0\n").unwrap();
    let out = p
        .cmd()
        .args(["validate", "--offline", "--output", "json"])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let doc = json(&out);
    let errors = doc["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{doc}");
    assert!(
        errors[0]
            .as_str()
            .unwrap()
            .contains("V2__bad.up.sql: line 2"),
        "{doc}"
    );
}

#[cfg(feature = "mssql")]
#[test]
fn validate_warns_about_non_idempotent_repeatables_in_subfolders() {
    let p = Project::new("provider = \"mssql\"\n");
    fs::create_dir_all(p.migrations().join("procs")).unwrap();
    fs::write(
        p.migrations().join("procs").join("R__dbo.get_orders.sql"),
        "CREATE PROCEDURE dbo.get_orders AS SELECT 1;\n",
    )
    .unwrap();
    fs::write(
        p.migrations().join("R__dbo.v_ok.sql"),
        "CREATE OR ALTER VIEW dbo.v_ok AS SELECT 1 AS x;\n",
    )
    .unwrap();
    let out = p
        .cmd()
        .args(["validate", "--offline", "--output", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let doc = json(&out);
    assert_eq!(doc["repeatables"], 2, "{doc}");
    let warnings = doc["warnings"].to_string();
    assert!(
        warnings.contains("R__dbo.get_orders.sql") && warnings.contains("CREATE OR ALTER"),
        "{warnings}"
    );
    assert!(!warnings.contains("v_ok"), "{warnings}");
}

#[cfg(feature = "mssql")]
#[test]
fn validate_rejects_sqlcmd_only_syntax_for_the_native_executor() {
    let p = Project::new("provider = \"mssql\"\n");
    fs::create_dir(p.migrations()).unwrap();
    fs::write(
        p.migrations().join("V1__vars.up.sql"),
        ":setvar Schema dbo\nCREATE TABLE $(Schema).t (id int);\n",
    )
    .unwrap();
    p.cmd()
        .args(["validate", "--offline"])
        .assert()
        .code(2)
        .stdout(
            predicate::str::contains("line 1 (:setvar)")
                .and(predicate::str::contains("line 2 ($(variable))"))
                .and(predicate::str::contains("placeholders")),
        );
}

#[cfg(feature = "mssql")]
#[test]
fn new_creates_repeatables_and_migrations_in_subfolders() {
    let p = Project::new("provider = \"mssql\"\n");
    let out = p
        .cmd()
        .args([
            "new",
            "dbo.get_orders",
            "--repeatable",
            "--folder",
            "procs",
            "--output",
            "json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let doc = json(&out);
    assert_eq!(doc["version"], "R__dbo.get_orders");
    assert_eq!(doc["repeatable"], true);
    let rep = p.migrations().join("procs").join("R__dbo.get_orders.sql");
    let body = fs::read_to_string(&rep).unwrap();
    assert!(
        body.contains("CREATE OR ALTER PROCEDURE dbo.get_orders"),
        "{body}"
    );

    // Versioned pair in a nested folder.
    p.cmd()
        .args(["new", "create orders", "--folder", "2026/orders"])
        .assert()
        .success();
    let nested: Vec<String> = fs::read_dir(p.migrations().join("2026").join("orders"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(nested.len(), 2, "{nested:?}");

    // Both are found; the template-only repeatable passes validation.
    let out = p
        .cmd()
        .args(["validate", "--offline", "--output", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let doc = json(&out);
    assert_eq!(
        (doc["migrations"].as_u64(), doc["repeatables"].as_u64()),
        (Some(1), Some(1))
    );

    // A second repeatable with the same name (any case, any folder) is refused.
    p.cmd()
        .args(["new", "DBO.GET_ORDERS", "--repeatable"])
        .assert()
        .code(64)
        .stderr(predicate::str::contains("already exists"));
    // Paths outside the migrations folder or into dot-folders are refused.
    for bad in ["../outside", ".drafts"] {
        p.cmd()
            .args(["new", "x", "--folder", bad])
            .assert()
            .code(64);
    }
    // --repeatable and --irreversible don't combine.
    p.cmd()
        .args(["new", "x", "--repeatable", "--irreversible"])
        .assert()
        .code(64);
}

#[test]
fn offline_commands_dont_offer_connection_options() {
    let help = |args: &[&str]| -> String {
        let out = Command::cargo_bin("migrate-db")
            .unwrap()
            .args(args)
            .arg("--help")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let new = help(&["new"]);
    for absent in [
        "--connection",
        "--auth",
        "--allow-down",
        "--lock-timeout",
        "--env-file",
    ] {
        assert!(!new.contains(absent), "new --help shows {absent}:\n{new}");
    }
    let status = help(&["status"]);
    assert!(status.contains("--connection") && !status.contains("--allow-down"));
    let up = help(&["up"]);
    assert!(
        up.contains("--lock-timeout")
            && up.contains("--allow-out-of-order")
            && !up.contains("--allow-down")
    );
    let goto = help(&["goto"]);
    assert!(goto.contains("--allow-down") && goto.contains("--allow-out-of-order"));
}
