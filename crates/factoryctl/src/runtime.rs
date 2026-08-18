//! Shared authority for mutations of the managed runtime.

use std::{
    fs::{File, OpenOptions},
    io,
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

use rustix::fs::FlockOperation;

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
