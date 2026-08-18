//! Operator-owned live-session capacity.
//!
//! Capacity is a launchd setting rather than a daemon request: changing it
//! restarts only `factoryd`, while detached runner processes and their durable
//! session rows remain in place. Keeping the operation here gives `factoryctl`
//! and `factory-tui` the same validation, launchd, health, and rollback path.

use std::{
    env,
    fs::{self, OpenOptions},
    io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use crate::{launchd, probes};

pub const DEFAULT_MAX_ACTIVE_RUNS: usize = 4;
pub const MAX_MAX_ACTIVE_RUNS: usize = 64;
const HEALTH_WAIT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapacityStatus {
    pub configured: usize,
    pub launchd_loaded: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapacityChange {
    pub previous: usize,
    pub current: usize,
}

pub fn validate(value: usize) -> Result<usize, String> {
    if (1..=MAX_MAX_ACTIVE_RUNS).contains(&value) {
        Ok(value)
    } else {
        Err(format!(
            "capacity must be between 1 and {MAX_MAX_ACTIVE_RUNS} live sessions"
        ))
    }
}

/// The capacity the current operator shell is allowed to change.
pub fn set_from_environment(socket: &Path, requested: usize) -> Result<CapacityChange, String> {
    let home = factory_home()?;
    let user_home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is not set")?;
    set(&home, &user_home, socket, requested)
}

pub fn status_from_environment() -> Result<CapacityStatus, String> {
    let home = factory_home()?;
    let user_home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is not set")?;
    status(&home, &user_home)
}

/// Reads the operator setting without requiring a daemon or a launchd job.
pub fn status(_home: &Path, user_home: &Path) -> Result<CapacityStatus, String> {
    let plist = launchd::plist_path(user_home);
    let configured = launchd::max_active_runs(
        launchd::read_existing(&plist)?
            .as_ref()
            .map_or(&[][..], |job| job.program_arguments.as_slice()),
    )?
    .unwrap_or(DEFAULT_MAX_ACTIVE_RUNS);
    Ok(CapacityStatus {
        configured,
        launchd_loaded: probes::launchd_loaded(),
    })
}

/// Applies one operator capacity change. The setting is deliberately absent
/// from the local daemon protocol: an authenticated provider session is not an
/// operator and must not be able to request it. The provider-session markers
/// are checked here as a second line of defense for both the CLI and TUI.
pub fn set(
    home: &Path,
    user_home: &Path,
    socket: &Path,
    requested: usize,
) -> Result<CapacityChange, String> {
    ensure_operator()?;
    let requested = validate(requested)?;
    let _lock = CapacityLock::acquire(home)?;
    let plist = launchd::plist_path(user_home);
    let existing = launchd::read_existing(&plist)?
        .ok_or("no launchd job is installed; capacity changes require the managed daemon")?;
    launchd::check_home(&existing, home, user_home)?;
    if !probes::launchd_loaded() {
        if probes::daemon_answers(socket) {
            return Err(
                "a daemon answers but its launchd job is not loaded; capacity changes require the managed daemon"
                    .into(),
            );
        }
        return Err("the Dark Factory launchd job is not loaded".into());
    }
    let previous =
        launchd::max_active_runs(&existing.program_arguments)?.unwrap_or(DEFAULT_MAX_ACTIVE_RUNS);
    if previous == requested {
        return Ok(CapacityChange {
            previous,
            current: requested,
        });
    }

    launchd::apply(
        home,
        &plist,
        Some(&existing),
        &probes::provider_directories(),
        &std::collections::BTreeMap::new(),
        Some(requested),
    )?;
    if probes::wait_for_daemon(socket, HEALTH_WAIT, None).is_err() {
        let rollback = launchd::apply(
            home,
            &plist,
            Some(&existing),
            &probes::provider_directories(),
            &std::collections::BTreeMap::new(),
            Some(previous),
        );
        return match rollback {
            Ok(()) => Err(format!(
                "factoryd did not answer after the capacity change; capacity rolled back to {previous}"
            )),
            Err(error) => Err(format!(
                "factoryd did not answer after the capacity change; rollback to {previous} failed: {error}"
            )),
        };
    }
    Ok(CapacityChange {
        previous,
        current: requested,
    })
}

struct CapacityLock {
    path: PathBuf,
}

impl CapacityLock {
    fn acquire(home: &Path) -> Result<Self, String> {
        let path = home.join("capacity.lock");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    "another capacity or runtime update is already in progress".to_owned()
                } else {
                    format!("could not lock capacity setting: {error}")
                }
            })?;
        if let Err(error) = file.set_permissions(fs::Permissions::from_mode(0o600)) {
            let _ = fs::remove_file(&path);
            return Err(format!("could not secure capacity lock: {error}"));
        }
        Ok(Self { path })
    }
}

impl Drop for CapacityLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn ensure_operator() -> Result<(), String> {
    ensure_operator_with(|name| env::var_os(name).is_some_and(|value| !value.is_empty()))
}

fn ensure_operator_with(has_value: impl Fn(&str) -> bool) -> Result<(), String> {
    if [
        "DARK_FACTORY_AGENT",
        "DARK_FACTORY_AGENT_DIR",
        "DARK_FACTORY_SESSION_TOKEN_FILE",
    ]
    .into_iter()
    .any(has_value)
    {
        return Err(
            "capacity changes are operator-only; agent sessions cannot change capacity".into(),
        );
    }
    Ok(())
}

fn factory_home() -> Result<PathBuf, String> {
    factory_core::paths::dark_factory_home().map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_has_a_finite_documented_bound() {
        assert_eq!(validate(1), Ok(1));
        assert_eq!(validate(MAX_MAX_ACTIVE_RUNS), Ok(MAX_MAX_ACTIVE_RUNS));
        assert!(validate(0).is_err());
        assert!(validate(MAX_MAX_ACTIVE_RUNS + 1).is_err());
    }

    #[test]
    fn concurrent_capacity_changes_are_rejected_by_a_private_lock() {
        let home = tempfile::tempdir().unwrap();
        let first = CapacityLock::acquire(home.path()).unwrap();
        assert!(CapacityLock::acquire(home.path()).is_err());
        drop(first);
        assert!(CapacityLock::acquire(home.path()).is_ok());
    }

    #[test]
    fn provider_session_identity_is_not_an_operator_principal() {
        for blocked in [
            "DARK_FACTORY_AGENT",
            "DARK_FACTORY_AGENT_DIR",
            "DARK_FACTORY_SESSION_TOKEN_FILE",
        ] {
            assert!(ensure_operator_with(|name| name == blocked).is_err());
        }
        assert!(ensure_operator_with(|_| false).is_ok());
    }
}
