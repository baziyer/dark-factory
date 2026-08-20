//! Installing a set of Dark Factory binaries under `$DARK_FACTORY_HOME/bin`.
//!
//! Layout (see `docs/development/WORKFLOW.md`, "Release and update"):
//!
//! ```text
//! $DARK_FACTORY_HOME/bin/<version>/{factoryd,factory-runner,factoryctl,factory-tui}
//! $DARK_FACTORY_HOME/bin/current -> <version>          (relative symlink)
//! ```
//!
//! `current` is what the launchd job runs and what an operator puts on
//! `PATH`; repointing it is the whole "activate" step, so a rollback is
//! `activate(previous)` and nothing is ever deleted on install. Every
//! already-running session's hooks reference `bin/current/factoryctl` by
//! that symlinked path (the daemon hands its own sibling path to the
//! providers uncanonicalized), so they follow the repoint too.
//!
//! A version directory only ever appears complete: everything is staged
//! under `bin/.staging-<version>` and renamed into place once all four
//! binaries checked out; any failure removes the staging directory.

use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::Command,
};

pub use crate::update::RELEASE_BINARIES as BINARIES;
use crate::update::{self, Manifest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Observable boundaries of the archive install. Emitted immediately before
/// the named work starts, so a caller never labels checksum or unpack time as
/// download time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReleaseInstallStage {
    Downloading,
    Verifying,
    Unpacking,
}

/// Downloads larger than this are refused before verification even starts.
const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;
const RELEASE_IDENTITY_FILE: &str = ".release-identity.json";
const MAX_RELEASE_IDENTITY_BYTES: u64 = 16 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReleaseIdentity {
    version: String,
    archive_sha256: String,
    binaries: BTreeMap<String, String>,
}

/// `<home>/bin`.
#[must_use]
pub fn bin_dir(home: &Path) -> PathBuf {
    home.join("bin")
}

/// `<home>/bin/<version>`.
#[must_use]
pub fn version_dir(home: &Path, version: &str) -> PathBuf {
    bin_dir(home).join(version)
}

/// `<home>/bin/current`.
#[must_use]
pub fn current_link(home: &Path) -> PathBuf {
    bin_dir(home).join("current")
}

/// The validated version `bin/current` points at, or `None` when no active
/// runtime has been installed. The link must be the one-component relative
/// shape [`activate`] writes, and its target must contain all four binaries.
pub fn active_version(home: &Path) -> Result<Option<String>, String> {
    update::active_version(home)
}

/// Downloads this platform's asset from `manifest`, verifies its SHA-256,
/// unpacks it into `bin/<version>`, and returns that directory. A complete
/// `bin/<version>` already on disk is reused as is (nothing is downloaded);
/// an incomplete one is an error naming it, never silently overwritten.
pub fn install_release(
    home: &Path,
    manifest: &Manifest,
    log: &mut dyn FnMut(&str),
) -> Result<PathBuf, String> {
    install_release_with_progress(home, manifest, log, &mut |_| {})
}

/// [`install_release`] with typed, truthful progress boundaries for operator
/// clients. The original wrapper remains for callers that only need logs.
pub fn install_release_with_progress(
    home: &Path,
    manifest: &Manifest,
    log: &mut dyn FnMut(&str),
    progress: &mut dyn FnMut(ReleaseInstallStage),
) -> Result<PathBuf, String> {
    update::validate_manifest(manifest)?;
    let key = update::platform_key();
    let asset = manifest
        .assets
        .get(key)
        .ok_or_else(|| format!("release {} has no asset for {key}", manifest.version))?;
    let destination = version_dir(home, &manifest.version);
    if destination.exists() {
        update::validate_runtime(home, &manifest.version).map_err(|error| {
            format!(
                "{error}; remove {} to download it again",
                destination.display()
            )
        })?;
        verify_release_identity(&destination, &manifest.version, &asset.sha256).map_err(
            |error| {
                format!(
                    "{error}; remove {} to download it again",
                    destination.display()
                )
            },
        )?;
        log(&format!("{} already present", destination.display()));
        return Ok(destination);
    }
    stage(home, &manifest.version, |staging| {
        let archive = staging.join("release.tar.gz");
        progress(ReleaseInstallStage::Downloading);
        log(&format!("downloading {}", asset.url));
        update::curl_to_file(&asset.url, &archive, MAX_ARCHIVE_BYTES)?;
        let size = fs::metadata(&archive)
            .map_err(|error| error.to_string())?
            .len();
        if size > MAX_ARCHIVE_BYTES {
            return Err(format!(
                "{} is {size} bytes, over the {MAX_ARCHIVE_BYTES} limit",
                asset.url
            ));
        }
        progress(ReleaseInstallStage::Verifying);
        let digest = sha256_file(&archive)?;
        if !digest.eq_ignore_ascii_case(asset.sha256.trim()) {
            return Err(format!(
                "checksum mismatch for {}: manifest says {}, download is {digest}",
                asset.url, asset.sha256
            ));
        }
        log(&format!("verified sha256 {digest}"));
        progress(ReleaseInstallStage::Unpacking);
        let status = Command::new("tar")
            .arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(staging)
            .status()
            .map_err(|error| format!("could not run tar: {error}"))?;
        if !status.success() {
            return Err(format!("unpacking {} failed ({status})", archive.display()));
        }
        verify_binaries(staging)?;
        write_release_identity(staging, &manifest.version, &digest)?;
        fs::remove_file(&archive).map_err(|error| error.to_string())
    })
    .inspect(|installed| log(&format!("installed {}", installed.display())))
}

/// Requires an installed release to be the exact archive identity previously
/// verified for this version, and requires every still-executable binary to
/// retain the digest captured immediately after unpacking.
pub fn verify_release_identity(
    directory: &Path,
    version: &str,
    archive_sha256: &str,
) -> Result<(), String> {
    let directory_metadata = fs::symlink_metadata(directory)
        .map_err(|error| format!("could not inspect {}: {error}", directory.display()))?;
    if !directory_metadata.file_type().is_dir() {
        return Err(format!("{} is not a direct directory", directory.display()));
    }
    verify_binaries(directory)?;
    let path = directory.join(RELEASE_IDENTITY_FILE);
    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        format!(
            "{} has no verified release identity: {error}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_file()
        || metadata.permissions().mode() & 0o777 != 0o600
        || metadata.len() > MAX_RELEASE_IDENTITY_BYTES
    {
        return Err(format!(
            "{} is not a private bounded regular identity file",
            path.display()
        ));
    }
    let identity: ReleaseIdentity = serde_json::from_slice(
        &fs::read(&path).map_err(|error| format!("reading {}: {error}", path.display()))?,
    )
    .map_err(|error| format!("{} is invalid: {error}", path.display()))?;
    if identity.version != version || identity.archive_sha256 != archive_sha256 {
        return Err(format!(
            "{} does not match release {version} archive {archive_sha256}",
            path.display()
        ));
    }
    if identity.binaries.len() != BINARIES.len() {
        return Err(format!(
            "{} has an incomplete binary identity",
            path.display()
        ));
    }
    for name in BINARIES {
        let expected = identity
            .binaries
            .get(name)
            .ok_or_else(|| format!("{} omits {name}", path.display()))?;
        let actual = sha256_file(&directory.join(name))?;
        if &actual != expected {
            return Err(format!(
                "{} no longer matches its verified release identity",
                directory.join(name).display()
            ));
        }
    }
    Ok(())
}

fn write_release_identity(
    directory: &Path,
    version: &str,
    archive_sha256: &str,
) -> Result<(), String> {
    let binaries = BINARIES
        .into_iter()
        .map(|name| sha256_file(&directory.join(name)).map(|digest| (name.to_owned(), digest)))
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let bytes = serde_json::to_vec(&ReleaseIdentity {
        version: version.to_owned(),
        archive_sha256: archive_sha256.to_owned(),
        binaries,
    })
    .map_err(|error| error.to_string())?;
    let path = directory.join(RELEASE_IDENTITY_FILE);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|error| format!("creating {}: {error}", path.display()))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("writing {}: {error}", path.display()))
}

/// Copies the four binaries from `source` (typically the directory the
/// running `factoryctl` lives in — a `cargo build --release` target dir or
/// an unpacked release) into `bin/<version>`. Refuses to overwrite.
pub fn install_from_dir(home: &Path, source: &Path, version: &str) -> Result<PathBuf, String> {
    verify_binaries(source)?;
    if version_dir(home, version).exists() {
        return Err(format!(
            "{} already exists",
            version_dir(home, version).display()
        ));
    }
    stage(home, version, |staging| {
        for name in BINARIES {
            fs::copy(source.join(name), staging.join(name))
                .map_err(|error| format!("copying {name} from {}: {error}", source.display()))?;
        }
        Ok(())
    })
}

/// Runs `fill` against a fresh `bin/.staging-<version>`, verifies the four
/// binaries are there and executable, and renames it to `bin/<version>`.
/// Any error (or a missing binary) removes the staging directory.
fn stage(
    home: &Path,
    version: &str,
    fill: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<PathBuf, String> {
    let destination = version_dir(home, version);
    let staging = bin_dir(home).join(format!(".staging-{version}"));
    let _ = fs::remove_dir_all(&staging);
    create_private_dir(&bin_dir(home))?;
    create_private_dir(&staging)?;
    let result = fill(&staging)
        .and_then(|()| verify_binaries(&staging))
        .and_then(|()| {
            fs::rename(&staging, &destination).map_err(|error| {
                format!(
                    "could not move {} into place at {}: {error}",
                    staging.display(),
                    destination.display()
                )
            })
        });
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result.map(|()| destination)
}

/// Repoints `bin/current` at `bin/<version>` atomically (a fresh symlink
/// renamed over the old one), so there is never a moment without a valid
/// `current`. The target must already contain every binary.
pub fn activate(home: &Path, version: &str) -> Result<(), String> {
    update::validate_runtime(home, version)?;
    let link = current_link(home);
    let temp = bin_dir(home).join(".current.tmp");
    let _ = fs::remove_file(&temp);
    symlink(version, &temp)
        .map_err(|error| format!("could not create {}: {error}", temp.display()))?;
    fs::rename(&temp, &link)
        .map_err(|error| format!("could not repoint {}: {error}", link.display()))
}

/// Every binary present, a regular file, and executable.
pub fn verify_binaries(dir: &Path) -> Result<(), String> {
    for name in BINARIES {
        let path = dir.join(name);
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

/// Creates `path` (and parents) as a `0700` directory.
pub fn create_private_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("could not create {}: {error}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("could not set permissions on {}: {error}", path.display()))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|error| error.to_string())?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher).map_err(|error| error.to_string())?;
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_binaries(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        for name in BINARIES {
            let path = dir.join(name);
            fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn activate_repoints_current_atomically_and_refuses_incomplete_versions() {
        let home = tempfile::tempdir().unwrap();
        fake_binaries(&version_dir(home.path(), "0.1.0"));
        fake_binaries(&version_dir(home.path(), "0.2.0"));
        assert_eq!(active_version(home.path()).unwrap(), None);
        activate(home.path(), "0.2.0").unwrap();
        assert_eq!(
            active_version(home.path()).unwrap().as_deref(),
            Some("0.2.0")
        );
        assert!(current_link(home.path()).join("factoryd").exists());
        activate(home.path(), "0.1.0").unwrap();
        assert_eq!(
            active_version(home.path()).unwrap().as_deref(),
            Some("0.1.0")
        );
        assert!(activate(home.path(), "9.9.9").is_err());
        assert_eq!(
            active_version(home.path()).unwrap().as_deref(),
            Some("0.1.0"),
            "failed activate leaves current alone"
        );

        fs::remove_file(current_link(home.path())).unwrap();
        symlink("../0.1.0", current_link(home.path())).unwrap();
        assert!(active_version(home.path()).is_err());

        fs::remove_file(current_link(home.path())).unwrap();
        fake_binaries(&version_dir(home.path(), "not-a-version"));
        symlink("not-a-version", current_link(home.path())).unwrap();
        assert!(active_version(home.path()).is_err());
    }

    #[test]
    fn active_runtime_rejects_directory_and_binary_symlink_indirection() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let external = root.path().join("external");
        fake_binaries(&external);
        fs::create_dir_all(bin_dir(&home)).unwrap();

        symlink(&external, version_dir(&home, "0.3.0")).unwrap();
        symlink("0.3.0", current_link(&home)).unwrap();
        assert!(
            active_version(&home)
                .unwrap_err()
                .contains("direct directory")
        );
        assert!(activate(&home, "0.3.0").is_err());

        fs::remove_file(current_link(&home)).unwrap();
        fs::remove_file(version_dir(&home, "0.3.0")).unwrap();
        fake_binaries(&version_dir(&home, "0.3.0"));
        fs::remove_file(version_dir(&home, "0.3.0/factoryd")).unwrap();
        symlink(
            external.join("factoryd"),
            version_dir(&home, "0.3.0/factoryd"),
        )
        .unwrap();
        symlink("0.3.0", current_link(&home)).unwrap();
        assert!(
            active_version(&home)
                .unwrap_err()
                .contains("direct executable file")
        );
        assert!(activate(&home, "0.3.0").is_err());

        let indirect_home = root.path().join("indirect-home");
        let external_bin = root.path().join("external-bin");
        fake_binaries(&external_bin.join("0.3.0"));
        symlink("0.3.0", external_bin.join("current")).unwrap();
        fs::create_dir_all(&indirect_home).unwrap();
        symlink(&external_bin, indirect_home.join("bin")).unwrap();
        assert!(
            active_version(&indirect_home)
                .unwrap_err()
                .contains("direct directory")
        );
    }

    #[test]
    fn install_from_dir_stages_verifies_and_refuses_overwrite() {
        let home = tempfile::tempdir().unwrap();
        let source = home.path().join("src");
        fake_binaries(&source);
        let installed = install_from_dir(home.path(), &source, "0.3.0").unwrap();
        assert_eq!(installed, version_dir(home.path(), "0.3.0"));
        verify_binaries(&installed).unwrap();
        assert!(install_from_dir(home.path(), &source, "0.3.0").is_err());
        // An incomplete source never produces a version directory or leaves staging behind.
        fs::remove_file(source.join("factory-tui")).unwrap();
        assert!(install_from_dir(home.path(), &source, "0.4.0").is_err());
        assert!(!version_dir(home.path(), "0.4.0").exists());
        assert!(!bin_dir(home.path()).join(".staging-0.4.0").exists());
    }

    #[test]
    fn install_release_verifies_the_checksum_unpacks_and_reuses_a_complete_dir() {
        let home = tempfile::tempdir().unwrap();
        let source = home.path().join("src");
        fake_binaries(&source);
        let archive = home.path().join("release.tar.gz");
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&source)
            .args(BINARIES)
            .status()
            .unwrap();
        assert!(status.success());
        let sha = sha256_file(&archive).unwrap();
        let manifest = |sha256: &str| Manifest {
            version: "0.3.0".to_owned(),
            assets: [(
                update::platform_key().to_owned(),
                update::Asset {
                    url: format!("file://{}", archive.display()),
                    sha256: sha256.to_owned(),
                },
            )]
            .into_iter()
            .collect(),
        };
        let lines = std::cell::RefCell::new(Vec::new());
        let mut log = |line: &str| lines.borrow_mut().push(line.to_owned());
        let error =
            install_release(home.path(), &manifest(&"00".repeat(32)), &mut log).unwrap_err();
        assert!(error.contains("checksum mismatch"), "{error}");
        assert!(!version_dir(home.path(), "0.3.0").exists());
        assert!(
            !bin_dir(home.path()).join(".staging-0.3.0").exists(),
            "staging cleaned up"
        );
        // A download that fails outright cleans up too.
        let mut unreachable = manifest(&sha);
        unreachable
            .assets
            .get_mut(update::platform_key())
            .unwrap()
            .url = "http://127.0.0.1:9/never.tar.gz".to_owned();
        assert!(install_release(home.path(), &unreachable, &mut log).is_err());
        assert!(!bin_dir(home.path()).join(".staging-0.3.0").exists());

        let stages = std::cell::RefCell::new(Vec::new());
        let installed =
            install_release_with_progress(home.path(), &manifest(&sha), &mut log, &mut |stage| {
                stages.borrow_mut().push(stage)
            })
            .unwrap();
        assert_eq!(
            *stages.borrow(),
            [
                ReleaseInstallStage::Downloading,
                ReleaseInstallStage::Verifying,
                ReleaseInstallStage::Unpacking,
            ]
        );
        assert_eq!(installed, version_dir(home.path(), "0.3.0"));
        verify_binaries(&installed).unwrap();
        assert!(!installed.join("release.tar.gz").exists());
        // Second time: reused, not downloaded (a now-unreachable URL proves it).
        assert_eq!(
            install_release(home.path(), &unreachable, &mut log).unwrap(),
            installed
        );
        assert!(
            lines
                .borrow()
                .iter()
                .any(|line| line.contains("already present"))
        );
        let error =
            install_release(home.path(), &manifest(&"00".repeat(32)), &mut log).unwrap_err();
        assert!(error.contains("does not match release"), "{error}");
        // Keeping all four files executable is insufficient: modified bytes
        // must not be reused under the release's verified identity.
        fs::write(installed.join("factory-tui"), b"#!/bin/sh\necho tampered\n").unwrap();
        fs::set_permissions(
            installed.join("factory-tui"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let error = install_release(home.path(), &manifest(&sha), &mut log).unwrap_err();
        assert!(error.contains("no longer matches"), "{error}");
        // A tampered version directory is refused, not "reused".
        fs::remove_file(installed.join("factory-tui")).unwrap();
        assert!(
            install_release(home.path(), &manifest(&sha), &mut log)
                .unwrap_err()
                .contains("remove")
        );
    }
}
