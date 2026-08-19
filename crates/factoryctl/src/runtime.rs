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
        (Some(previous), RollbackOutcome::JobFailed(_)) => {
            format!("bin/current rolled back to {previous}, but launchd recovery failed")
        }
        (Some(previous), RollbackOutcome::RuntimeRestored) => {
            format!("bin/current rolled back to {previous}; launchd plist and job were unchanged")
        }
        (Some(previous), RollbackOutcome::Restored) => {
            format!("bin/current rolled back to {previous}")
        }
        (Some(previous), RollbackOutcome::NotAttempted) => {
            format!("bin/current rollback was not attempted for {previous}")
        }
        (None, _) => "there was no previous managed runtime".to_owned(),
    };
    let recovery = match (error.outcome(), previous_version) {
        (RollbackOutcome::Restored | RollbackOutcome::RuntimeRestored, Some(previous)) => {
            Some(health(previous))
        }
        _ => None,
    };
    match recovery {
        Some(Ok(())) => format!("{error}; {runtime_outcome}; restored runtime is healthy"),
        Some(Err(recovery)) => {
            format!("{error}; {runtime_outcome}; restored runtime health failed: {recovery}")
        }
        None if matches!(
            error.outcome(),
            RollbackOutcome::Restored | RollbackOutcome::RuntimeRestored
        ) =>
        {
            format!(
                "{error}; {runtime_outcome}; no previous managed runtime was available for health checking"
            )
        }
        None => format!("{error}; {runtime_outcome}"),
    }
}

/// The rollback authority captured while [`MutationLock`] is held.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct MutationSnapshot {
    pub active_version: Option<String>,
    pub plist: Option<String>,
}

/// Restores the runtime and launchd job captured by a mutation transaction
/// after the newly loaded daemon fails its health check. The runtime is
/// repointed first; if restoring the old job fails, the new runtime is put
/// back so the active link still matches the job launchd has.
pub fn rollback_after_health_failure(
    home: &Path,
    plist: &Path,
    snapshot: &MutationSnapshot,
    current_version: &str,
    health: impl FnOnce(&str) -> Result<(), String>,
) -> Result<(), String> {
    let previous = snapshot
        .active_version
        .as_deref()
        .ok_or("no previous managed runtime is available")?;
    let previous_plist = snapshot
        .plist
        .as_deref()
        .ok_or("no previous managed launchd job is available")?;
    snapshot.restore_runtime(home)?;
    let rollback_home = home.to_owned();
    let current_version = current_version.to_owned();
    crate::launchd::restore_with_rollback(plist, home, previous_plist, move || {
        crate::install::activate(&rollback_home, &current_version)
    })
    .map_err(|error| error.to_string())?;
    health(previous)
}

impl MutationSnapshot {
    /// Restores the active link captured while the mutation lock was held.
    /// A missing active runtime means the transaction must remove the link,
    /// but never remove an unexpected real directory in its place.
    pub fn restore_runtime(&self, home: &Path) -> Result<(), String> {
        if let Some(version) = &self.active_version {
            return crate::install::activate(home, version);
        }
        let link = crate::install::current_link(home);
        match fs::symlink_metadata(&link) {
            Ok(metadata) if metadata.file_type().is_symlink() || metadata.is_file() => {
                fs::remove_file(&link).map_err(|error| {
                    format!(
                        "could not remove the newly activated runtime {}: {error}",
                        link.display()
                    )
                })?;
                Ok(())
            }
            Ok(_) => Err(format!(
                "cannot remove unexpected non-file active runtime path {}",
                link.display()
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!(
                "could not inspect active runtime path {}: {error}",
                link.display()
            )),
        }
    }
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
}
