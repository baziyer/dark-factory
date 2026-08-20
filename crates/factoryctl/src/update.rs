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
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
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
const MAX_ASSET_URL_BYTES: usize = 4096;
const MAX_ASSET_KEY_BYTES: usize = 128;
/// Every binary in an installed release runtime.
pub const RELEASE_BINARIES: [&str; 4] = ["factoryd", "factory-runner", "factoryctl", "factory-tui"];

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
        self.available_from(&self.current)
    }

    /// The newer version available relative to `current`. The cached
    /// [`Self::current`] is the invoking binary; callers that manage an
    /// installed runtime can compare against that active version instead.
    #[must_use]
    pub fn available_from(&self, current: &str) -> Option<&Manifest> {
        self.latest.as_ref().filter(|manifest| {
            manifest.assets.contains_key(platform_key()) && is_newer(&manifest.version, current)
        })
    }
}

/// Returns the canonical stable release spelling (`MAJOR.MINOR.PATCH`).
/// Release identities become directory names and terminal text, so aliases,
/// prereleases, separators, controls, whitespace, and non-ASCII text are all
/// rejected before a manifest can reach either surface.
pub fn canonical_stable_version(version: &str) -> Result<String, String> {
    let mut parts = version.split('.');
    let mut numbers = [0_u64; 3];
    for number in &mut numbers {
        let part = parts
            .next()
            .filter(|part| !part.is_empty())
            .ok_or_else(|| format!("release version {version:?} is not MAJOR.MINOR.PATCH"))?;
        if !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(format!(
                "release version {version:?} is not a stable ASCII MAJOR.MINOR.PATCH"
            ));
        }
        *number = part
            .parse()
            .map_err(|_| format!("release version {version:?} is out of range"))?;
        if number.to_string() != part {
            return Err(format!(
                "release version {version:?} is not canonically spelled"
            ));
        }
    }
    if parts.next().is_some() {
        return Err(format!(
            "release version {version:?} is not MAJOR.MINOR.PATCH"
        ));
    }
    Ok(format!("{}.{}.{}", numbers[0], numbers[1], numbers[2]))
}

/// Validates every fetched or cached field used as a path, command argument,
/// checksum, or human label. Unknown extra platform assets remain allowed,
/// but their keys and URLs must still be bounded safe ASCII.
pub fn validate_manifest(manifest: &Manifest) -> Result<(), String> {
    let canonical = canonical_stable_version(&manifest.version)?;
    if canonical != manifest.version {
        return Err(format!(
            "release version {:?} is not canonical ({canonical})",
            manifest.version
        ));
    }
    if manifest.assets.is_empty() {
        return Err(format!("release {canonical} has no assets"));
    }
    for (key, asset) in &manifest.assets {
        if key.is_empty()
            || key.len() > MAX_ASSET_KEY_BYTES
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(format!("release {canonical} has an invalid asset key"));
        }
        if asset.url.is_empty()
            || asset.url.len() > MAX_ASSET_URL_BYTES
            || !asset.url.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(format!(
                "release {canonical} asset {key} has an invalid URL"
            ));
        }
        if asset.sha256.len() != 64
            || !asset
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(format!(
                "release {canonical} asset {key} has an invalid SHA-256"
            ));
        }
    }
    Ok(())
}

/// The validated version `$DARK_FACTORY_HOME/bin/current` points at.
///
/// The active runtime is trusted only when `bin`, the version directory,
/// and every release binary are real filesystem entries. Symlinked
/// directories or binaries could otherwise escape the install home while
/// retaining the expected-looking `current -> <version>` link.
pub fn active_version(home: &Path) -> Result<Option<String>, String> {
    let home_metadata = match fs::symlink_metadata(home) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not inspect {}: {error}", home.display())),
    };
    require_real_directory(home, &home_metadata)?;

    let bin = home.join("bin");
    let bin_metadata = match fs::symlink_metadata(&bin) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not inspect {}: {error}", bin.display())),
    };
    require_real_directory(&bin, &bin_metadata)?;

    let link = bin.join("current");
    let metadata = match fs::symlink_metadata(&link) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not inspect {}: {error}", link.display())),
    };
    if !metadata.file_type().is_symlink() {
        return Err(format!("{} is not a symbolic link", link.display()));
    }
    let target = fs::read_link(&link)
        .map_err(|error| format!("could not read {}: {error}", link.display()))?;
    let mut components = target.components();
    let version = match (components.next(), components.next()) {
        (Some(Component::Normal(version)), None) => version
            .to_str()
            .filter(|version| !version.is_empty())
            .ok_or_else(|| format!("{} has an invalid target", link.display()))?,
        _ => return Err(format!("{} has an invalid target", link.display())),
    };
    if !is_valid_version(version) {
        return Err(format!(
            "{} names invalid version {version:?}",
            link.display()
        ));
    }
    validate_runtime(home, version).map_err(|error| {
        format!(
            "{} points at an invalid active runtime: {error}",
            link.display()
        )
    })?;
    Ok(Some(version.to_owned()))
}

/// Verifies that one installed version cannot escape `<home>/bin/<version>`
/// through directory or binary symlinks.
pub fn validate_runtime(home: &Path, version: &str) -> Result<(), String> {
    let home_metadata = fs::symlink_metadata(home)
        .map_err(|error| format!("could not inspect {}: {error}", home.display()))?;
    require_real_directory(home, &home_metadata)?;

    let bin = home.join("bin");
    let bin_metadata = fs::symlink_metadata(&bin)
        .map_err(|error| format!("could not inspect {}: {error}", bin.display()))?;
    require_real_directory(&bin, &bin_metadata)?;

    let runtime = bin.join(version);
    let runtime_metadata = fs::symlink_metadata(&runtime)
        .map_err(|error| format!("could not inspect {}: {error}", runtime.display()))?;
    require_real_directory(&runtime, &runtime_metadata)?;
    for name in RELEASE_BINARIES {
        let path = runtime.join(name);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("{} is missing: {error}", path.display()))?;
        if !metadata.file_type().is_file() || metadata.permissions().mode() & 0o111 == 0 {
            return Err(format!(
                "{} is not a direct executable file",
                path.display()
            ));
        }
    }
    Ok(())
}

fn require_real_directory(path: &Path, metadata: &fs::Metadata) -> Result<(), String> {
    if !metadata.file_type().is_dir() {
        return Err(format!("{} is not a direct directory", path.display()));
    }
    Ok(())
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

/// The asset key for the running binary's platform. Release builds cover both
/// Apple silicon and Intel macOS; any other build never sees an available
/// update.
#[must_use]
pub fn platform_key() -> &'static str {
    platform_key_for(std::env::consts::OS, std::env::consts::ARCH)
}

fn platform_key_for(os: &str, arch: &str) -> &'static str {
    match (os, arch) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        _ => "unsupported",
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
    if !force && let Some(previous) = &previous {
        let age_ms = now_ms.saturating_sub(previous.checked_at_ms);
        if previous.current == CURRENT_VERSION
            && (0..=CHECK_INTERVAL.as_millis() as i64).contains(&age_ms)
        {
            return previous.clone();
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
    let check: UpdateCheck = serde_json::from_slice(&bytes).ok()?;
    if let Some(manifest) = &check.latest {
        validate_manifest(manifest).ok()?;
    }
    Some(check)
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
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .map_err(|error| format!("manifest is not valid: {error}"))?;
    validate_manifest(&manifest).map_err(|error| format!("manifest is not valid: {error}"))?;
    Ok(manifest)
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

/// Whether `version` has the release version shape this updater can compare.
#[must_use]
pub fn is_valid_version(version: &str) -> bool {
    parse_version(version).is_some()
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
        assert!(is_valid_version("0.2.0"));
        assert!(is_valid_version("v0.2.0-rc.1"));
        assert!(!is_valid_version("not-a-version"));
    }

    #[test]
    fn release_manifest_rejects_unsafe_or_nonstable_identity_fields() {
        let manifest = |version: &str, sha256: &str, url: &str| Manifest {
            version: version.to_owned(),
            assets: [(
                platform_key().to_owned(),
                Asset {
                    url: url.to_owned(),
                    sha256: sha256.to_owned(),
                },
            )]
            .into(),
        };
        let digest = "ab".repeat(32);
        assert!(
            validate_manifest(&manifest("1.2.3", &digest, "https://example.invalid/a")).is_ok()
        );
        for version in [
            "1.2.3-rc.1",
            "v1.2.3",
            "1.2.03",
            "999.0.0-x/../../outside",
            "1.2.3\u{1b}[2J",
            "1.2.3-é",
        ] {
            assert!(
                validate_manifest(&manifest(version, &digest, "https://example.invalid/a"))
                    .is_err(),
                "accepted {version:?}"
            );
        }
        assert!(validate_manifest(&manifest("1.2.3", "00", "https://example.invalid/a")).is_err());
        assert!(
            validate_manifest(&manifest(
                "1.2.3",
                &"AB".repeat(32),
                "https://example.invalid/a"
            ))
            .is_err()
        );
        assert!(
            validate_manifest(&manifest(
                "1.2.3",
                &digest,
                "https://example.invalid/\nforge"
            ))
            .is_err()
        );
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
        assert!(
            check(Some(manifest(CURRENT_VERSION, platform_key())))
                .available_from("0.1.0")
                .is_some()
        );
        assert!(check(None).available().is_none());
    }

    #[test]
    fn both_released_macos_architectures_have_manifest_keys() {
        assert_eq!(platform_key_for("macos", "aarch64"), "aarch64-apple-darwin");
        assert_eq!(platform_key_for("macos", "x86_64"), "x86_64-apple-darwin");
        assert_eq!(platform_key_for("linux", "x86_64"), "unsupported");
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

    #[test]
    fn unsafe_cached_manifest_is_never_projected_or_installed() {
        let home = tempfile::tempdir().unwrap();
        let hostile = UpdateCheck {
            checked_at_ms: 1,
            current: CURRENT_VERSION.to_owned(),
            latest: Some(Manifest {
                version: "999.0.0-x/../../outside".to_owned(),
                assets: [(
                    platform_key().to_owned(),
                    Asset {
                        url: "https://example.invalid/a".to_owned(),
                        sha256: "00".repeat(32),
                    },
                )]
                .into(),
            }),
            error: None,
        };
        fs::write(
            cache_path(home.path()),
            serde_json::to_vec(&hostile).unwrap(),
        )
        .unwrap();
        assert_eq!(read_cache(home.path()), None);
    }
}
