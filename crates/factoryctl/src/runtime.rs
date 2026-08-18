//! Shared authority for mutations of the managed runtime.

use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

use rustix::fs::FlockOperation;

use crate::launchd::{MutationError, RollbackOutcome};

/// Formats the common init/update recovery report and performs the managed
/// health check only when the typed launchd transaction says both plist and
/// runtime were restored.
pub fn rollback_report(
    error: &MutationError,
    previous_version: Option<&str>,
    health: impl FnOnce(&str) -> Result<(), String>,
) -> String {
    let runtime_outcome = match (previous_version, error.outcome()) {
        (Some(previous), RollbackOutcome::RuntimeFailed(_)) => {
            format!("bin/current could NOT be rolled back to {previous}")
        }
        (Some(previous), _) => format!("bin/current rolled back to {previous}"),
        (None, _) => "there was no previous managed runtime".to_owned(),
    };
    let recovery = match (error.outcome(), previous_version) {
        (RollbackOutcome::Restored, Some(previous)) => Some(health(previous)),
        _ => None,
    };
    match recovery {
        Some(Ok(())) => format!("{error}; {runtime_outcome}; restored runtime is healthy"),
        Some(Err(recovery)) => {
            format!("{error}; {runtime_outcome}; restored runtime health failed: {recovery}")
        }
        None if matches!(error.outcome(), RollbackOutcome::Restored) => format!(
            "{error}; {runtime_outcome}; no previous managed runtime was available for health checking"
        ),
        None => format!("{error}; {runtime_outcome}"),
    }
}

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

    /// Acquires the production mutation lock and captures rollback authority
    /// as one operation. Callers must keep the returned lock until their
    /// mutation, health check, and any rollback are complete.
    pub fn begin(home: &Path, plist: &Path) -> Result<(Self, MutationSnapshot), String> {
        let lock = Self::acquire(home)?;
        let snapshot = lock.snapshot(home, plist)?;
        Ok((lock, snapshot))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        sync::{Arc, Barrier, mpsc},
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
    fn production_update_transaction_snapshots_after_a_competing_init() {
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

        // The update has finished its download work and is waiting to enter
        // the same production transaction used by init/update. A competing
        // init wins first; the update must take its snapshot after waiting
        // for the lock, or its failed activation would restore stale 0.1.0.
        let barrier = Arc::new(Barrier::new(2));
        let (finished, wait) = mpsc::channel();
        let init_home = home.path().to_owned();
        let init_barrier = Arc::clone(&barrier);
        let init = thread::spawn(move || {
            let (init_lock, _) =
                MutationLock::begin(&init_home, &init_home.join("missing.plist")).unwrap();
            init_barrier.wait();
            crate::install::activate(&init_home, "0.2.0").unwrap();
            drop(init_lock);
            finished.send(()).unwrap();
        });
        barrier.wait();
        wait.recv().unwrap();
        init.join().unwrap();

        let (update_lock, snapshot) =
            MutationLock::begin(home.path(), &home.path().join("missing.plist")).unwrap();
        crate::install::activate(home.path(), "0.3.0").unwrap();
        crate::install::activate(home.path(), snapshot.active_version.as_deref().unwrap()).unwrap();
        drop(update_lock);
        assert_eq!(
            crate::install::active_version(home.path()).unwrap(),
            Some("0.2.0".into())
        );
    }
}
