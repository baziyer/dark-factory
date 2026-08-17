//! Update check against the release manifest.
//!
//! GitHub Releases are the source of truth: every tagged release carries a
//! `latest.json` asset (written by `.github/workflows/release.yml`), and
//! `https://github.com/<repo>/releases/latest/download/latest.json` is a
//! stable URL for the newest one. `factoryctl update` and `factory-tui`'s
//! status line both go through [`check`]: the result is cached in
//! `$DARK_FACTORY_HOME/update-check.json` for [`CHECK_INTERVAL`], so a
//! board that runs for days re-checks at most hourly and a fresh
//! `factoryctl update` right after it costs no network call at all. There
//! is no background service; nothing checks unless a client is running.
//!
//! The fetch shells out to `curl` (present on every macOS) rather than
//! adding a TLS stack to a workspace that otherwise has none.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use serde::{Deserialize, Serialize};

/// The version compiled into this `factoryctl` (and, since the workspace
/// shares one version, into every sibling binary from the same build).
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Where the newest release's manifest lives; overridable for tests and
/// mirrors through `DARK_FACTORY_UPDATE_URL`.
pub const MANIFEST_URL: &str =
    "https://github.com/baziyer/dark-factory/releases/latest/download/latest.json";
pub const MANIFEST_URL_ENV: &str = "DARK_FACTORY_UPDATE_URL";
/// How long a cached check stays fresh.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_MANIFEST_BYTES: usize = 64 * 1024;

/// One release, as published by the release workflow (its `latest.json`
/// also carries a `tag`, which nothing here needs).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Manifest {
    pub version: String,
    /// Keyed by Rust target triple, e.g. `aarch64-apple-darwin`.
    pub assets: BTreeMap<String, Asset>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Asset {
    pub url: String,
    pub sha256: String,
}

/// The durable result of the most recent check (also the JSON shape
/// `factoryctl update` prints).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UpdateCheck {
    pub checked_at_ms: i64,
    pub current: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<Manifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl UpdateCheck {
    /// The newer version available for this platform, if the last check
    /// found one.
    #[must_use]
    pub fn available(&self) -> Option<&Manifest> {
        self.latest.as_ref().filter(|manifest| {
            manifest.assets.contains_key(platform_key())
                && is_newer(&manifest.version, &self.current)
        })
    }
}

/// `<home>/update-check.json`.
#[must_use]
fn cache_path(home: &Path) -> PathBuf {
    home.join("update-check.json")
}

/// The manifest URL: `$DARK_FACTORY_UPDATE_URL` if set, else [`MANIFEST_URL`].
#[must_use]
pub fn manifest_url() -> String {
    std::env::var(MANIFEST_URL_ENV)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| MANIFEST_URL.to_owned())
}

/// The asset key for the running binary's platform. Only macOS arm64 is
/// released today (`.github/workflows/release.yml`); any other build
/// simply never sees an available update.
#[must_use]
pub const fn platform_key() -> &'static str {
    if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        "aarch64-apple-darwin"
    } else {
        "unsupported"
    }
}

/// Returns the cached check if it is younger than [`CHECK_INTERVAL`] (and
/// `force` is false), otherwise fetches the manifest at `url` (see
/// [`manifest_url`]), writes the cache, and returns the fresh result. A failed fetch is a result too (`error` set,
/// `latest` carried over from the previous cache when there was one) — and
/// is cached as well, so a machine that is offline doesn't retry on every
/// tick.
#[must_use]
pub fn check(home: &Path, url: &str, now_ms: i64, force: bool) -> UpdateCheck {
    let previous = read_cache(home);
    if !force {
        if let Some(previous) = &previous {
            let age_ms = now_ms.saturating_sub(previous.checked_at_ms);
            if previous.current == CURRENT_VERSION
                && (0..=CHECK_INTERVAL.as_millis() as i64).contains(&age_ms)
            {
                return previous.clone();
            }
        }
    }
    let result = match fetch_manifest(url) {
        Ok(manifest) => UpdateCheck {
            checked_at_ms: now_ms,
            current: CURRENT_VERSION.to_owned(),
            latest: Some(manifest),
            error: None,
        },
        Err(error) => UpdateCheck {
            checked_at_ms: now_ms,
            current: CURRENT_VERSION.to_owned(),
            latest: previous.and_then(|previous| previous.latest),
            error: Some(error),
        },
    };
    // Best effort: a home that doesn't exist yet (no daemon ever ran) just
    // means the next check fetches again.
    let _ = write_cache(home, &result);
    result
}

/// Reads and parses the cache; anything unreadable or malformed is `None`.
fn read_cache(home: &Path) -> Option<UpdateCheck> {
    let bytes = fs::read(cache_path(home)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_cache(home: &Path, check: &UpdateCheck) -> io::Result<()> {
    let path = cache_path(home);
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, serde_json::to_vec(check).map_err(io::Error::other)?)?;
    fs::rename(temp, path)
}

/// Downloads and parses the manifest at `url` with `curl`.
fn fetch_manifest(url: &str) -> Result<Manifest, String> {
    let bytes = curl(url, MAX_MANIFEST_BYTES)?;
    serde_json::from_slice(&bytes).map_err(|error| format!("manifest is not valid: {error}"))
}

/// Fetches `url` to memory with `curl`, bounded to `max_bytes`. Follows
/// redirects (GitHub's `releases/latest/download/...` is one), fails on
/// HTTP errors, and never prompts.
fn curl(url: &str, max_bytes: usize) -> Result<Vec<u8>, String> {
    let output = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--max-time",
            &FETCH_TIMEOUT.as_secs().to_string(),
            "--max-filesize",
            &max_bytes.to_string(),
            "--",
            url,
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("could not run curl: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "fetching {url} failed: {}",
            stderr.trim().lines().last().unwrap_or("curl failed")
        ));
    }
    if output.stdout.len() > max_bytes {
        return Err(format!("{url} exceeds {max_bytes} bytes"));
    }
    Ok(output.stdout)
}

/// Downloads `url` to `destination` with `curl` (streaming, no size cap
/// beyond `max_bytes`; the caller verifies the checksum afterwards).
pub fn curl_to_file(url: &str, destination: &Path, max_bytes: u64) -> Result<(), String> {
    let status = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--max-time",
            "600",
            "--max-filesize",
            &max_bytes.to_string(),
            "--output",
        ])
        .arg(destination)
        .arg("--")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .map_err(|error| format!("could not run curl: {error}"))?;
    if !status.success() {
        return Err(format!("downloading {url} failed ({status})"));
    }
    Ok(())
}

/// Milliseconds since the Unix epoch, for cache stamps and status frames.
#[must_use]
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

/// Semantic-version comparison: `candidate` is newer than `current` when its
/// `MAJOR.MINOR.PATCH` is greater, or equal with `current` a pre-release and
/// `candidate` not (`0.2.0` > `0.2.0-rc.1`). Anything unparseable is never
/// newer. Pre-release identifiers are compared as plain strings — enough
/// for `-rc.1`/`-rc.2`, and this project doesn't tag anything fancier.
#[must_use]
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

/// `(core, is_release, pre_release)`: tuple ordering gives core first, then
/// a release above any pre-release of the same core, then pre-releases in
/// string order.
fn parse_version(text: &str) -> Option<([u64; 3], bool, String)> {
    let text = text.strip_prefix('v').unwrap_or(text);
    let (core, pre) = match text.split_once('-') {
        Some((core, pre)) => (core, pre),
        None => (text, ""),
    };
    let mut parts = core.split('.');
    let mut numbers = [0u64; 3];
    for slot in &mut numbers {
        *slot = parts.next()?.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some((numbers, pre.is_empty(), pre.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_by_core_and_prerelease() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.0"));
        assert!(!is_newer("0.1.0.1", "0.1.0"));
        assert!(is_newer("0.2.0", "0.2.0-rc.1"));
        assert!(!is_newer("0.2.0-rc.1", "0.2.0"));
        assert!(is_newer("0.2.0-rc.2", "0.2.0-rc.1"));
    }

    #[test]
    fn available_requires_a_newer_version_for_this_platform() {
        let manifest = |version: &str, key: &str| Manifest {
            version: version.to_owned(),
            assets: [(
                key.to_owned(),
                Asset {
                    url: "https://example.invalid/x.tar.gz".to_owned(),
                    sha256: "00".to_owned(),
                },
            )]
            .into_iter()
            .collect(),
        };
        let check = |latest: Option<Manifest>| UpdateCheck {
            checked_at_ms: 0,
            current: CURRENT_VERSION.to_owned(),
            latest,
            error: None,
        };
        assert!(
            check(Some(manifest("999.0.0", platform_key())))
                .available()
                .is_some()
        );
        assert!(
            check(Some(manifest("999.0.0", "riscv64gc-unknown-none")))
                .available()
                .is_none()
        );
        assert!(
            check(Some(manifest("0.0.1", platform_key())))
                .available()
                .is_none()
        );
        assert!(check(None).available().is_none());
    }

    #[test]
    fn cache_round_trips_and_is_reused_while_fresh() {
        let home = tempfile::tempdir().expect("tempdir");
        let cached = UpdateCheck {
            checked_at_ms: 1_000_000,
            current: CURRENT_VERSION.to_owned(),
            latest: None,
            error: Some("offline".to_owned()),
        };
        write_cache(home.path(), &cached).expect("write cache");
        assert_eq!(read_cache(home.path()), Some(cached.clone()));
        // Fresh: returned verbatim, no fetch attempted (an unreachable
        // manifest URL would otherwise produce a different error).
        let unreachable = "http://127.0.0.1:9/never";
        assert_eq!(
            check(home.path(), unreachable, 1_000_000 + 60_000, false),
            cached
        );
        // Stale: refetched (and the fetch fails, so `error` changes).
        let stale_at = 1_000_000 + CHECK_INTERVAL.as_millis() as i64 + 1;
        let refreshed = check(home.path(), unreachable, stale_at, false);
        assert_ne!(refreshed.error, cached.error);
        assert!(refreshed.error.is_some());
        assert_eq!(read_cache(home.path()), Some(refreshed));
    }
}
