//! Shared authority for mutations of the managed runtime.

use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

use rustix::fs::FlockOperation;

/// The rollback authority captured while [`MutationLock`] is held.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct MutationSnapshot {
    pub active_version: Option<String>,
    pub plist: Option<String>,
}

/// Serializes every operation that can rewrite the managed launchd job or
/// repoint its active runtime. The advisory lock is released by the OS when
/// the process exits, so a killed updater cannot strand a stale lock file.
pub struct MutationLock {
    _file: File,
}

impl MutationLock {
    pub fn acquire(home: &Path) -> Result<Self, String> {
        let path = home.join("runtime-mutation.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .map_err(|error| format!("could not open runtime mutation lock: {error}"))?;
        rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive).map_err(|error| {
            let source = io::Error::from_raw_os_error(error.raw_os_error());
            if source.kind() == io::ErrorKind::WouldBlock {
                "another managed runtime mutation is already in progress".to_owned()
            } else {
                format!("could not acquire runtime mutation lock: {source}")
            }
        })?;
        Ok(Self { _file: file })
    }

    /// Captures every value needed to roll back a managed-runtime mutation.
    /// Callers must hold this lock for the whole reread/mutate/health/rollback
    /// transaction; taking these snapshots before acquiring it permits a
    /// competing init or update to be rolled back accidentally.
    pub fn snapshot(&self, home: &Path, plist: &Path) -> Result<MutationSnapshot, String> {
        let plist =
            if plist.exists() {
                Some(fs::read_to_string(plist).map_err(|error| {
                    format!("could not save the existing launchd plist: {error}")
                })?)
            } else {
                None
            };
        Ok(MutationSnapshot {
            active_version: crate::install::active_version(home)?,
            plist,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        sync::mpsc,
        thread,
    };

    use super::MutationLock;

    #[test]
    fn one_recoverable_lock_serializes_capacity_and_update_mutations() {
        let home = tempfile::tempdir().unwrap();
        let first = MutationLock::acquire(home.path()).unwrap();
        assert!(MutationLock::acquire(home.path()).is_err());
        assert!(home.path().join("runtime-mutation.lock").is_file());
        drop(first);
        assert!(MutationLock::acquire(home.path()).is_ok());
    }

    #[test]
    fn concurrent_init_then_update_rollback_uses_the_new_active_snapshot() {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join("bin");
        for version in ["0.1.0", "0.2.0", "0.3.0"] {
            fs::create_dir_all(bin.join(version)).unwrap();
            for name in crate::install::BINARIES {
                let path = bin.join(version).join(name);
                fs::write(&path, "#!/bin/sh\n").unwrap();
                let mut permissions = fs::metadata(&path).unwrap().permissions();
                permissions.set_mode(0o755);
                fs::set_permissions(&path, permissions).unwrap();
            }
        }
        symlink("0.1.0", bin.join("current")).unwrap();

        // A slow update has already downloaded its candidate and observed
        // the old active version, but it has not yet acquired the mutation
        // lock. A competing init wins the lock and activates 0.2.0. The
        // update must snapshot 0.2.0, not stale 0.1.0, so a later failed
        // 0.3.0 activation restores the competing mutation.
        let stale_version = crate::install::active_version(home.path()).unwrap();
        let (finished, wait) = mpsc::channel();
        let init_home = home.path().to_owned();
        let init = thread::spawn(move || {
            let init_lock = MutationLock::acquire(&init_home).unwrap();
            crate::install::activate(&init_home, "0.2.0").unwrap();
            drop(init_lock);
            finished.send(()).unwrap();
        });
        wait.recv().unwrap();
        init.join().unwrap();
        assert_eq!(stale_version, Some("0.1.0".into()));

        let update_lock = MutationLock::acquire(home.path()).unwrap();
        let snapshot = update_lock
            .snapshot(home.path(), &home.path().join("missing.plist"))
            .unwrap();
        crate::install::activate(home.path(), "0.3.0").unwrap();
        crate::install::activate(home.path(), snapshot.active_version.as_deref().unwrap()).unwrap();
        assert_eq!(
            crate::install::active_version(home.path()).unwrap(),
            Some("0.2.0".into())
        );
    }
}
