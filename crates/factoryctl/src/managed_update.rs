//! One verified active-runtime update transaction shared by `factoryctl` and
//! `factory-tui`.
//!
//! The TUI owns only presentation and the final process replacement. Every
//! download, checksum, activation, launchd reload, health check, and rollback
//! goes through this module, so the button cannot acquire a second updater.

use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::install::{self, ReleaseInstallStage};
use crate::update::Manifest;
use crate::{launchd, probes, runtime, update};

const HEALTH_WAIT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateProgress {
    Checking,
    Downloading,
    Verifying,
    Unpacking,
    Activating,
    Reloading,
    CheckingHealth,
    RollingBack,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedDaemon {
    NotInstalled,
    Unchanged,
    Reloaded,
}

/// Successful transaction. The rollback authority is retained across the
/// TUI's exec seam; a CLI caller simply drops it after reporting success.
pub struct InstalledUpdate {
    pub version: String,
    pub installed: PathBuf,
    pub daemon: ManagedDaemon,
    pub health_version: Option<String>,
    rollback: Option<RollbackPlan>,
}

impl std::fmt::Debug for InstalledUpdate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InstalledUpdate")
            .field("version", &self.version)
            .field("installed", &self.installed)
            .field("daemon", &self.daemon)
            .field("health_version", &self.health_version)
            .finish_non_exhaustive()
    }
}

/// Typed health failure lets the CLI preserve its existing JSON/error
/// contract while the TUI can show one bounded actionable message.
#[derive(Debug)]
pub enum InstallError {
    Message(String),
    Stale {
        active: String,
    },
    Health {
        version: String,
        installed: PathBuf,
        error: String,
        previous_version: Option<String>,
        rollback: Result<(), String>,
    },
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message(message) => formatter.write_str(message),
            Self::Stale { active } => write!(
                formatter,
                "active runtime changed to {active} while update was downloading; refusing stale update"
            ),
            Self::Health {
                error, rollback, ..
            } => write!(
                formatter,
                "new daemon health failed: {error}; rollback {}",
                if rollback.is_ok() {
                    "restored"
                } else {
                    "failed"
                }
            ),
        }
    }
}

impl From<String> for InstallError {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

struct RollbackPlan {
    home: PathBuf,
    plist: PathBuf,
    socket: PathBuf,
    installed_version: String,
    snapshot: runtime::MutationSnapshot,
}

impl InstalledUpdate {
    /// Restore the exact runtime/job captured before this update, then require
    /// the previous managed daemon to answer with its exact version. Refuses
    /// to undo a later updater's activation.
    pub fn rollback_after_reexec_failure(
        mut self,
        progress: &mut dyn FnMut(UpdateProgress),
    ) -> Result<(), String> {
        let Some(plan) = self.rollback.take() else {
            // The only TUI-reachable no-plan outcome is an already-active,
            // exact-version healthy daemon. The transaction changed nothing,
            // so an exec failure has nothing to restore.
            return Ok(());
        };
        progress(UpdateProgress::RollingBack);
        let (_lock, current) = runtime::MutationLock::begin(&plan.home, &plan.plist)?;
        if current.active_version.as_deref() != Some(&plan.installed_version) {
            return Err(format!(
                "active runtime changed after update (expected {}); refusing to roll back another owner",
                plan.installed_version
            ));
        }
        runtime::rollback_after_health_failure(
            &plan.home,
            &plan.plist,
            &plan.snapshot,
            &plan.installed_version,
            |previous| {
                probes::wait_for_managed_daemon(
                    &plan.socket,
                    HEALTH_WAIT,
                    Some(previous),
                    &plan.home,
                )
                .map(|_| ())
            },
        )
    }
}

/// Install and activate `manifest`. `require_managed_daemon` is true for the
/// TUI: a viewer cannot safely restart an unknown manually managed daemon, so
/// it fails during read-only preflight. The CLI retains its existing
/// install-only behavior when no launchd job is present.
pub fn install(
    home: &Path,
    socket: &Path,
    manifest: &Manifest,
    require_managed_daemon: bool,
    progress: &mut dyn FnMut(UpdateProgress),
    log: &mut dyn FnMut(&str),
) -> Result<InstalledUpdate, InstallError> {
    let user_home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| InstallError::Message("HOME is not set".to_owned()))?;
    let plist = launchd::plist_path(&user_home);
    let existing = launchd::read_existing(&plist)?;
    if let Some(existing) = &existing {
        launchd::check_home(existing, home, &user_home)?;
    } else if require_managed_daemon {
        return Err(InstallError::Message(
            "one-button update requires the managed launchd job; use `factoryctl update --install` for a manually managed daemon"
                .to_owned(),
        ));
    }

    let active_version = install::active_version(home)?;
    if active_version.as_deref() == Some(manifest.version.as_str())
        && probes::wait_for_daemon(socket, Duration::from_secs(2), Some(&manifest.version)).is_ok()
    {
        log(&format!(
            "{} is already installed and running",
            manifest.version
        ));
        return Ok(InstalledUpdate {
            version: manifest.version.clone(),
            installed: install::version_dir(home, &manifest.version),
            daemon: ManagedDaemon::Unchanged,
            health_version: Some(manifest.version.clone()),
            rollback: None,
        });
    }

    let installed = install::install_release_with_progress(home, manifest, log, &mut |stage| {
        progress(match stage {
            ReleaseInstallStage::Downloading => UpdateProgress::Downloading,
            ReleaseInstallStage::Verifying => UpdateProgress::Verifying,
            ReleaseInstallStage::Unpacking => UpdateProgress::Unpacking,
        });
    })?;

    let (_runtime_lock, snapshot) = runtime::MutationLock::begin(home, &plist)?;
    let previous_version = snapshot.active_version.clone();
    let existing = launchd::read_existing(&plist)?;
    if let Some(existing) = &existing {
        launchd::check_home(existing, home, &user_home)?;
    } else if require_managed_daemon {
        return Err(InstallError::Message(
            "managed launchd job disappeared while the update was downloading".to_owned(),
        ));
    }
    if let Some(active) = snapshot.active_version.as_deref()
        && active != manifest.version
        && !update::is_newer(&manifest.version, active)
    {
        return Err(InstallError::Stale {
            active: active.to_owned(),
        });
    }

    progress(UpdateProgress::Activating);
    install::activate(home, &manifest.version)?;
    log(&format!("bin/current -> {}", manifest.version));

    let Some(existing) = existing else {
        log("no launchd job installed; restart the daemon yourself to run the new version");
        return Ok(InstalledUpdate {
            version: manifest.version.clone(),
            installed,
            daemon: ManagedDaemon::NotInstalled,
            health_version: None,
            rollback: None,
        });
    };

    progress(UpdateProgress::Reloading);
    if let Err(error) = launchd::apply_with_rollback(
        home,
        &plist,
        Some(&existing),
        &probes::provider_directories(),
        &std::collections::BTreeMap::new(),
        None,
        || snapshot.restore_runtime(home),
    ) {
        return Err(InstallError::Message(runtime::rollback_report(
            &error,
            previous_version.as_deref(),
            |previous| {
                probes::wait_for_managed_daemon(socket, HEALTH_WAIT, Some(previous), home)
                    .map(|_| ())
            },
        )));
    }
    log(&format!("rewrote and reloaded {}", plist.display()));
    progress(UpdateProgress::CheckingHealth);
    match probes::wait_for_daemon(socket, HEALTH_WAIT, Some(&manifest.version)) {
        Ok(version) => Ok(InstalledUpdate {
            version: manifest.version.clone(),
            installed,
            daemon: ManagedDaemon::Reloaded,
            health_version: Some(version),
            rollback: Some(RollbackPlan {
                home: home.to_owned(),
                plist,
                socket: socket.to_owned(),
                installed_version: manifest.version.clone(),
                snapshot,
            }),
        }),
        Err(error) => {
            let rollback = runtime::rollback_after_health_failure(
                home,
                &plist,
                &snapshot,
                &manifest.version,
                |previous| {
                    probes::wait_for_managed_daemon(socket, HEALTH_WAIT, Some(previous), home)
                        .map(|_| ())
                },
            );
            Err(InstallError::Health {
                version: manifest.version.clone(),
                installed,
                error,
                previous_version,
                rollback,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_failure_after_an_unchanged_runtime_needs_no_rollback() {
        let outcome = InstalledUpdate {
            version: "0.2.6".to_owned(),
            installed: PathBuf::from("/unused/bin/0.2.6"),
            daemon: ManagedDaemon::Unchanged,
            health_version: Some("0.2.6".to_owned()),
            rollback: None,
        };
        let mut progress = Vec::new();
        assert!(
            outcome
                .rollback_after_reexec_failure(&mut |stage| progress.push(stage))
                .is_ok()
        );
        assert!(progress.is_empty());
    }
}
