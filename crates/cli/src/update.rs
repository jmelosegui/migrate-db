//! "A new version is available" notice, printed to stderr after every command.
//!
//! Rules (SPEC §10.5): it never changes stdout, the exit code or the JSON result; it
//! checks GitHub at most once a day (the answer is cached); a short timeout and every
//! failure are silent; and `--no-update-check` / `MIGRATE_DB_NO_UPDATE_CHECK` turn it off.

use std::path::PathBuf;
use std::time::Duration;

use semver::Version;
use serde_json::{Value, json};

const REPO: &str = "jmelosegui/migrate-db";
const CHECK_INTERVAL_SECS: i64 = 24 * 60 * 60;
const FETCH_TIMEOUT: Duration = Duration::from_secs(2);

/// Cached result of the last check.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cache {
    checked_at: i64,
    latest: Option<String>,
}

impl Cache {
    fn parse(text: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(text).ok()?;
        Some(Self {
            checked_at: v.get("checked_at")?.as_i64()?,
            latest: v.get("latest").and_then(Value::as_str).map(str::to_owned),
        })
    }

    fn to_json(&self) -> String {
        json!({ "checked_at": self.checked_at, "latest": self.latest }).to_string()
    }
}

/// Returns the notice text if a newer release exists. Never fails.
pub async fn notice(disabled: bool) -> Option<String> {
    if disabled {
        return None;
    }
    let path = cache_path();
    let cached = path
        .as_deref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| Cache::parse(&t));
    let now = chrono::Utc::now().timestamp();

    let needs_fetch = cached
        .as_ref()
        .is_none_or(|c| now - c.checked_at >= CHECK_INTERVAL_SECS || c.checked_at > now);
    let fetched = if needs_fetch {
        fetch_latest().await
    } else {
        None
    };

    let (cache, latest) = decide(now, cached, needs_fetch, fetched);
    if needs_fetch && let Some(p) = path {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, cache.to_json());
    }
    message(env!("CARGO_PKG_VERSION"), latest.as_deref()?)
}

/// Pure decision: what to store and which version is the latest known one.
/// A failed fetch keeps the old answer but still records the attempt, so an
/// offline machine retries once a day, not on every command.
fn decide(
    now: i64,
    cached: Option<Cache>,
    fetched_now: bool,
    fetched: Option<String>,
) -> (Cache, Option<String>) {
    let previous = cached.and_then(|c| c.latest);
    let latest = if fetched_now {
        fetched.or(previous)
    } else {
        previous
    };
    (
        Cache {
            checked_at: now,
            latest: latest.clone(),
        },
        latest,
    )
}

/// The notice, if `latest` (a tag such as `v0.2.0`) is newer than `current`.
fn message(current: &str, latest: &str) -> Option<String> {
    let latest_v = Version::parse(latest.trim_start_matches('v')).ok()?;
    let current_v = Version::parse(current).ok()?;
    if latest_v <= current_v {
        return None;
    }
    let update = if cfg!(windows) {
        format!("irm https://raw.githubusercontent.com/{REPO}/main/scripts/install.ps1 | iex")
    } else {
        format!(
            "curl -fsSL https://raw.githubusercontent.com/{REPO}/main/scripts/install.sh | bash"
        )
    };
    Some(format!(
        "notice: migrate-db {latest_v} is available (you have {current_v}). Update with:\n  {update}\nRelease notes: https://github.com/{REPO}/releases/tag/v{latest_v}\nSilence this with --no-update-check or MIGRATE_DB_NO_UPDATE_CHECK=1."
    ))
}

async fn fetch_latest() -> Option<String> {
    let task = tokio::task::spawn_blocking(|| {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(FETCH_TIMEOUT))
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .provider(ureq::tls::TlsProvider::NativeTls)
                    .build(),
            )
            .build();
        let agent: ureq::Agent = config.into();
        let body = agent
            .get(&format!(
                "https://api.github.com/repos/{REPO}/releases/latest"
            ))
            .header(
                "User-Agent",
                concat!("migrate-db/", env!("CARGO_PKG_VERSION")),
            )
            .header("Accept", "application/vnd.github+json")
            .call()
            .ok()?
            .body_mut()
            .read_to_string()
            .ok()?;
        let v: Value = serde_json::from_str(&body).ok()?;
        v.get("tag_name")?.as_str().map(str::to_owned)
    });
    // The agent's own timeout bounds the request; this bounds DNS and connect stalls.
    tokio::time::timeout(FETCH_TIMEOUT + Duration::from_secs(1), task)
        .await
        .ok()?
        .ok()?
}

/// Per-user cache file. Only environment variables, no extra crates.
fn cache_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
    } else if let Some(x) = std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        PathBuf::from(x)
    } else {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        if cfg!(target_os = "macos") {
            home.join("Library").join("Caches")
        } else {
            home.join(".cache")
        }
    };
    Some(base.join("migrate-db").join("update-check.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_release_produces_a_notice() {
        let m = message("0.1.0", "v0.2.0").unwrap();
        assert!(m.contains("0.2.0 is available (you have 0.1.0)"));
        assert!(m.contains("--no-update-check"));
    }

    #[test]
    fn same_older_or_unparseable_versions_stay_silent() {
        assert!(message("0.2.0", "v0.2.0").is_none());
        assert!(message("0.3.0", "v0.2.9").is_none());
        assert!(message("0.1.0", "nightly").is_none());
        assert!(message("0.1.0", "").is_none());
    }

    #[test]
    fn prerelease_is_older_than_its_release() {
        assert!(message("0.2.0-rc.1", "v0.2.0").is_some());
        assert!(message("0.2.0", "v0.2.0-rc.1").is_none());
    }

    #[test]
    fn failed_fetch_keeps_the_previous_answer_and_records_the_attempt() {
        let old = Cache {
            checked_at: 10,
            latest: Some("v0.2.0".into()),
        };
        let (cache, latest) = decide(100_000, Some(old), true, None);
        assert_eq!(latest.as_deref(), Some("v0.2.0"));
        assert_eq!(cache.checked_at, 100_000);
    }

    #[test]
    fn fresh_cache_is_used_without_a_new_answer() {
        let c = Cache {
            checked_at: 50,
            latest: Some("v0.3.0".into()),
        };
        let (_, latest) = decide(60, Some(c), false, None);
        assert_eq!(latest.as_deref(), Some("v0.3.0"));
    }

    #[test]
    fn fetched_answer_replaces_the_cached_one() {
        let c = Cache {
            checked_at: 1,
            latest: Some("v0.2.0".into()),
        };
        let (cache, latest) = decide(999_999, Some(c), true, Some("v0.4.0".into()));
        assert_eq!(latest.as_deref(), Some("v0.4.0"));
        assert_eq!(Cache::parse(&cache.to_json()), Some(cache));
    }

    #[test]
    fn corrupt_cache_is_ignored() {
        assert!(Cache::parse("not json").is_none());
        assert!(Cache::parse("{}").is_none());
    }
}
