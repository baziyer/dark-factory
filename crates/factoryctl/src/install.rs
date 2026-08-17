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

use std::{
    fs, io,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::Command,
};

use sha2::{Digest, Sha256};

use factoryctl::update::{self, Manifest};

/// Every binary a release ships and an install must contain.
pub const BINARIES: [&str; 4] = ["factoryd", "factory-runner", "factoryctl", "factory-tui"];
/// Downloads larger than this are refused before verification even starts.
const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;

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

/// Downloads this platform's asset from `manifest`, verifies its SHA-256,
/// unpacks it into `bin/<version>` (via a temporary sibling directory that
/// is renamed into place only once everything checked out), and returns
/// that directory. Refuses to overwrite an existing `bin/<version>`.
pub fn install_release(
    home: &Path,
    manifest: &Manifest,
    log: &mut dyn FnMut(&str),
) -> Result<PathBuf, String> {
    let key = update::platform_key();
    let asset = manifest
        .assets
        .get(key)
        .ok_or_else(|| format!("release {} has no asset for {key}", manifest.version))?;
    let destination = version_dir(home, &manifest.version);
    if destination.exists() {
        return Err(format!(
            "{} already exists; activate it with `factoryctl update --install` again after removing it, or leave it",
            destination.display()
        ));
    }
    let staging = bin_dir(home).join(format!(".staging-{}", manifest.version));
    let _ = fs::remove_dir_all(&staging);
    create_private_dir(&bin_dir(home))?;
    create_private_dir(&staging)?;
    let archive = staging.join("release.tar.gz");

    log(&format!("downloading {}", asset.url));
    update::curl_to_file(&asset.url, &archive, MAX_ARCHIVE_BYTES)?;
    let digest = sha256_file(&archive)?;
    if !digest.eq_ignore_ascii_case(asset.sha256.trim()) {
        let _ = fs::remove_dir_all(&staging);
        return Err(format!(
            "checksum mismatch for {}: manifest says {}, download is {digest}",
            asset.url, asset.sha256
        ));
    }
    log(&format!("verified sha256 {digest}"));

    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&staging)
        .status()
        .map_err(|error| format!("could not run tar: {error}"))?;
    if !status.success() {
        let _ = fs::remove_dir_all(&staging);
        return Err(format!("unpacking {} failed ({status})", archive.display()));
    }
    fs::remove_file(&archive).map_err(|error| error.to_string())?;
    if let Err(error) = verify_binaries(&staging) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    fs::rename(&staging, &destination).map_err(|error| {
        format!(
            "could not move {} into place at {}: {error}",
            staging.display(),
            destination.display()
        )
    })?;
    log(&format!("installed {}", destination.display()));
    Ok(destination)
}

/// Repoints `bin/current` at `bin/<version>` atomically (a fresh symlink
/// renamed over the old one), so there is never a moment without a valid
/// `current`. The target must already contain every binary.
pub fn activate(home: &Path, version: &str) -> Result<(), String> {
    verify_binaries(&version_dir(home, version))?;
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
        let metadata = fs::metadata(&path)
            .map_err(|error| format!("{} is missing: {error}", path.display()))?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            return Err(format!("{} is not an executable file", path.display()));
        }
    }
    Ok(())
}

fn create_private_dir(path: &Path) -> Result<(), String> {
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
        activate(home.path(), "0.2.0").unwrap();
        assert_eq!(
            fs::read_link(current_link(home.path())).unwrap(),
            Path::new("0.2.0")
        );
        assert!(current_link(home.path()).join("factoryd").exists());
        activate(home.path(), "0.1.0").unwrap();
        assert_eq!(
            fs::read_link(current_link(home.path())).unwrap(),
            Path::new("0.1.0")
        );
        assert!(activate(home.path(), "9.9.9").is_err());
        assert_eq!(
            fs::read_link(current_link(home.path())).unwrap(),
            Path::new("0.1.0"),
            "failed activate leaves current alone"
        );
    }

    #[test]
    fn install_release_verifies_the_checksum_and_unpacks() {
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
            tag: "v0.3.0".to_owned(),
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
        let mut log = |_: &str| {};
        let error = install_release(home.path(), &manifest("00"), &mut log).unwrap_err();
        assert!(error.contains("checksum mismatch"), "{error}");
        assert!(!version_dir(home.path(), "0.3.0").exists());
        assert!(
            !bin_dir(home.path()).join(".staging-0.3.0").exists(),
            "staging cleaned up"
        );
        let installed = install_release(home.path(), &manifest(&sha), &mut log).unwrap();
        assert_eq!(installed, version_dir(home.path(), "0.3.0"));
        verify_binaries(&installed).unwrap();
        assert!(!installed.join("release.tar.gz").exists());
    }
}
