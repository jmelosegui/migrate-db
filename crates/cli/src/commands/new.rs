//! `migrate-db new`.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::json;

use migrate_db_core::config::{ProviderKind, Versioning};
use migrate_db_core::error::{Error, exit};
use migrate_db_core::model::Direction;
use migrate_db_core::source::MigrationSet;
use migrate_db_core::version::Version;

use crate::cli::GlobalArgs;
use crate::context::Context;
use crate::output::Output;
use crate::templates;

/// What `new` was asked for.
#[derive(Debug)]
pub struct NewArgs<'a> {
    /// The description (versioned) or object name (repeatable).
    pub description: &'a str,
    /// Up file only.
    pub irreversible: bool,
    /// An `R__<name>` repeatable instead of a versioned pair.
    pub repeatable: bool,
    /// Subfolder of the migrations folder.
    pub folder: Option<&'a str>,
}

/// Run `new`.
pub fn run(global: &GlobalArgs, out: &Output, args: &NewArgs<'_>) -> Result<i32, Error> {
    let ctx = Context::load(global, None)?;
    let target = ctx.target();
    let dir = match args.folder {
        Some(f) => target.migrations_dir.join(subfolder(f)?),
        None => target.migrations_dir.clone(),
    };
    fs::create_dir_all(&dir)
        .map_err(|e| Error::Usage(format!("cannot create `{}`: {e}", dir.display())))?;
    // The whole tree: versions and repeatable names are unique across it.
    let existing = ctx.discover(&target)?;
    let extension = templates::extension(target.provider, target.new_format.as_deref());

    let (id, created) = if args.repeatable {
        let name = repeatable_name(args.description)?;
        if !matches!(
            target.provider,
            ProviderKind::Mssql | ProviderKind::Postgres
        ) {
            return Err(Error::Usage(format!(
                "repeatable migrations are for SQL providers; this project uses `{}`",
                target.provider
            )));
        }
        if let Some(r) = existing
            .repeatables
            .iter()
            .find(|r| r.name.eq_ignore_ascii_case(&name))
        {
            return Err(Error::Usage(format!(
                "a repeatable named `{}` already exists: {}; edit that file instead (it re-applies when it changes)",
                r.name, r.script.relative_path
            )));
        }
        let path = dir.join(format!("R__{name}.{extension}"));
        create_file(&path, &templates::repeatable_body(target.provider, &name))?;
        (format!("R__{name}"), vec![path])
    } else {
        let name = snake_case(args.description)?;
        let version = next_version(target.versioning, latest(&existing), Utc::now());
        let description = name.replace('_', " ");
        let mut directions = vec![Direction::Up];
        if !args.irreversible {
            directions.push(Direction::Down);
        }
        let mut created: Vec<PathBuf> = Vec::new();
        for direction in directions {
            let path = dir.join(format!("V{version}__{name}.{direction}.{extension}"));
            let body = templates::body(
                target.provider,
                extension,
                direction,
                &version,
                &description,
            );
            create_file(&path, &body)?;
            created.push(path);
        }
        (version, created)
    };

    let files: Vec<String> = created.iter().map(|p| p.display().to_string()).collect();
    out.emit(
        "new",
        json!({ "version": id, "repeatable": args.repeatable, "files": files }),
        || {
            let mut t = String::new();
            for f in &files {
                t.push_str(&format!("created {f}\n"));
            }
            t
        },
    )?;
    Ok(exit::OK)
}

/// Create a new file; never overwrite.
fn create_file(path: &Path, body: &str) -> Result<(), Error> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| Error::Usage(format!("cannot create `{}`: {e}", path.display())))?;
    file.write_all(body.as_bytes())
        .map_err(|e| Error::Usage(format!("cannot write `{}`: {e}", path.display())))
}

/// `--folder`: a relative path inside the migrations folder. Dot-folders are
/// refused, because discovery skips them and the files would never run.
pub fn subfolder(folder: &str) -> Result<PathBuf, Error> {
    let cleaned = folder.trim().replace('\\', "/");
    let cleaned = cleaned.trim_matches('/');
    let bad = |why: &str| {
        Error::Usage(format!(
            "--folder `{folder}` {why}; use a relative path inside the migrations folder, e.g. `orders` or `procs/reporting`"
        ))
    };
    if cleaned.is_empty() {
        return Err(bad("is empty"));
    }
    if folder.trim().starts_with(['/', '\\']) || cleaned.contains(':') {
        return Err(bad("is absolute"));
    }
    let mut path = PathBuf::new();
    for part in cleaned.split('/') {
        match part {
            "" => {}
            ".." => return Err(bad("leaves the migrations folder")),
            p if p.starts_with('.') => {
                return Err(bad(
                    "names a dot-folder, which discovery skips (its files would never run)",
                ));
            }
            p => path.push(p),
        }
    }
    Ok(path)
}

/// `"dbo.get_orders"` → `dbo.get_orders`; `"[dbo].[get orders]"` →
/// `dbo.get_orders`. Letters, digits, `_`, `.` and `-`.
pub fn repeatable_name(object: &str) -> Result<String, Error> {
    let mut out = String::with_capacity(object.len());
    for c in object.trim().chars() {
        match c {
            '[' | ']' | '"' | '`' => {}
            c if c.is_whitespace() => out.push('_'),
            c if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') => out.push(c),
            other => {
                return Err(Error::Usage(format!(
                    "a repeatable name may only contain letters, digits, `_`, `.` and `-`, not `{other}` (e.g. dbo.get_orders)"
                )));
            }
        }
    }
    let out = out.trim_matches(['.', '_', '-']).to_owned();
    if out.is_empty() {
        return Err(Error::Usage(
            "the repeatable name must contain a letter or digit, e.g. dbo.get_orders".into(),
        ));
    }
    Ok(out)
}

fn latest(set: &MigrationSet) -> Option<&Version> {
    set.latest().map(|m| &m.version)
}

/// `"Create Orders table!"` → `create_orders_table`.
pub fn snake_case(description: &str) -> Result<String, Error> {
    let mut out = String::with_capacity(description.len());
    let mut pending_sep = false;
    for c in description.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_sep && !out.is_empty() {
                out.push('_');
            }
            pending_sep = false;
            out.push(c.to_ascii_lowercase());
        } else {
            pending_sep = true;
        }
    }
    if out.is_empty() {
        return Err(Error::Usage(
            "the description must contain at least one letter or digit".into(),
        ));
    }
    Ok(out)
}

/// The next version: a UTC timestamp (bumped past the latest if needed) or
/// the next sequential number.
pub fn next_version(scheme: Versioning, latest: Option<&Version>, now: DateTime<Utc>) -> String {
    match scheme {
        Versioning::Timestamp => {
            let candidate = now.format("%Y%m%d%H%M%S").to_string();
            match latest {
                Some(l) if l.value() >= candidate.parse::<u128>().unwrap_or(0) => {
                    (l.value() + 1).to_string()
                }
                _ => candidate,
            }
        }
        Versioning::Sequential => match latest {
            Some(l) => {
                let width = l.as_str().len().max(4);
                format!("{:0width$}", l.value() + 1)
            }
            None => "0001".to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn snake_cases_descriptions() {
        assert_eq!(
            snake_case("Create Orders table!").unwrap(),
            "create_orders_table"
        );
        assert_eq!(
            snake_case("  add  index (email) ").unwrap(),
            "add_index_email"
        );
        assert!(snake_case("!!!").is_err());
    }

    #[test]
    fn subfolders_stay_inside_and_avoid_dot_folders() {
        assert_eq!(subfolder("orders").unwrap(), PathBuf::from("orders"));
        assert_eq!(
            subfolder("procs\\reporting\\").unwrap(),
            PathBuf::from("procs").join("reporting")
        );
        for bad in [
            "",
            "/abs",
            "\\abs",
            "C:\\x",
            "../out",
            "a/../../b",
            ".drafts",
            "a/.hidden",
        ] {
            assert!(subfolder(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn repeatable_names() {
        assert_eq!(repeatable_name("dbo.get_orders").unwrap(), "dbo.get_orders");
        assert_eq!(
            repeatable_name("[dbo].[get orders]").unwrap(),
            "dbo.get_orders"
        );
        assert_eq!(
            repeatable_name(" 010_views-core ").unwrap(),
            "010_views-core"
        );
        for bad in ["", "...", "dbo/x", "a;b", "x*"] {
            assert!(repeatable_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn timestamp_versions_never_go_backwards() {
        let now = Utc.with_ymd_and_hms(2026, 11, 9, 14, 25, 30).unwrap();
        assert_eq!(
            next_version(Versioning::Timestamp, None, now),
            "20261109142530"
        );
        let later: Version = "20261109142530".parse().unwrap();
        assert_eq!(
            next_version(Versioning::Timestamp, Some(&later), now),
            "20261109142531"
        );
    }

    #[test]
    fn sequential_versions_keep_width() {
        let now = Utc::now();
        assert_eq!(next_version(Versioning::Sequential, None, now), "0001");
        let v: Version = "0041".parse().unwrap();
        assert_eq!(next_version(Versioning::Sequential, Some(&v), now), "0042");
        let v: Version = "99999".parse().unwrap();
        assert_eq!(
            next_version(Versioning::Sequential, Some(&v), now),
            "100000"
        );
    }
}
