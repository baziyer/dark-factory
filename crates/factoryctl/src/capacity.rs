//! Operator-owned live-session capacity.
//!
//! Capacity is a launchd setting rather than a daemon request: changing it
//! restarts only `factoryd`, while detached runner processes and their durable
//! session rows remain in place. Keeping the operation here gives `factoryctl`
//! and `factory-tui` the same validation, launchd, health, and rollback path.

use std::{
    env,
    path::{Path, PathBuf},
    time::Duration,
};

use crate::{launchd, probes, runtime};

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

/// Applies one capacity change. The setting is deliberately absent from the
/// local daemon protocol; the daemon's explicit PreToolUse policy denies
/// provider-session shell mutations before this operator-side service runs.
pub fn set(
    home: &Path,
    user_home: &Path,
    socket: &Path,
    requested: usize,
) -> Result<CapacityChange, String> {
    let requested = validate(requested)?;
    let _lock = runtime::MutationLock::acquire(home)?;
    let plist = launchd::plist_path(user_home);
    let existing = launchd::read_existing(&plist)?
        .ok_or("no launchd job is installed; capacity changes require the managed daemon")?;
    let previous_plist = std::fs::read_to_string(&plist)
        .map_err(|error| format!("could not save the existing launchd plist: {error}"))?;
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
    probes::wait_for_managed_daemon(socket, Duration::from_secs(2), None, home)
        .map_err(|error| format!("managed launchd health check failed: {error}"))?;
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
    if let Err(error) = probes::wait_for_managed_daemon(socket, HEALTH_WAIT, None, home) {
        let rollback = launchd::restore(&plist, home, &previous_plist);
        return match rollback {
            Ok(()) => match probes::wait_for_managed_daemon(socket, HEALTH_WAIT, None, home) {
                Ok(_) => Err(format!(
                    "managed daemon health failed after the capacity change ({error}); capacity rolled back to {previous}"
                )),
                Err(recovery) => Err(format!(
                    "managed daemon health failed after the capacity change ({error}); capacity plist restored but rollback health failed: {recovery}"
                )),
            },
            Err(rollback) => Err(format!(
                "managed daemon health failed after the capacity change ({error}); rollback to {previous} failed: {rollback}"
            )),
        };
    }
    Ok(CapacityChange {
        previous,
        current: requested,
    })
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
}
