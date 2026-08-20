//! One verified active-runtime update transaction shared by `factoryctl` and
//! `factory-tui`.
//!
//! The TUI owns only presentation and the final process replacement. Every
//! download, checksum, activation, launchd reload, health check, and rollback
//! goes through this module, so the button cannot acquire a second updater.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::install::{self, ReleaseInstallStage};
use crate::update::Manifest;
use crate::{launchd, probes, runtime, update};

const HEALTH_WAIT: Duration = Duration::from_secs(30);
const PENDING_UPDATE_FILE: &str = "pending-update.json";
const MAX_PENDING_UPDATE_BYTES: u64 = 256 * 1024;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReexecRecovery {
    NotNeeded,
    Restored,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum PendingPhase {
    Activating,
    Reloading,
    CheckingHealth,
    AwaitingRelaunch,
}

impl PendingPhase {
    fn for_progress(progress: UpdateProgress) -> Option<Self> {
        match progress {
            UpdateProgress::Activating => Some(Self::Activating),
            UpdateProgress::Reloading => Some(Self::Reloading),
            UpdateProgress::CheckingHealth => Some(Self::CheckingHealth),
            UpdateProgress::Checking
            | UpdateProgress::Downloading
            | UpdateProgress::Verifying
            | UpdateProgress::Unpacking
            | UpdateProgress::RollingBack => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct PendingUpdate {
    version: String,
    archive_sha256: String,
    phase: PendingPhase,
    snapshot: runtime::MutationSnapshot,
    authority: RecoveryAuthority,
}

/// Exact local authority allowed to consume and act on a pending mutation.
/// Socket and plist paths identify replaceable endpoints, while the canonical
/// home plus its inode identifies the state directory that owns the record.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct RecoveryAuthority {
    home: PathBuf,
    home_device: u64,
    home_inode: u64,
    user_home: PathBuf,
    socket: PathBuf,
    plist: PathBuf,
    uid: u32,
    launchd_domain: String,
    launchd_label: String,
}

/// Successful transaction. The rollback authority is retained across the
/// TUI's exec seam; a CLI caller simply drops it after reporting success.
pub struct InstalledUpdate {
    pub version: String,
    pub installed: PathBuf,
    pub daemon: ManagedDaemon,
    pub health_version: Option<String>,
    archive_sha256: String,
    runtime_lock: Option<runtime::MutationLock>,
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
            .field("archive_sha256", &self.archive_sha256)
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
                "active runtime {active} is newer than the selected release; refusing downgrade"
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
    target: launchd::LaunchdTarget,
    installed_version: String,
    snapshot: runtime::MutationSnapshot,
}

impl InstalledUpdate {
    /// The immutable version-directory executable proven against the archive
    /// identity this transaction verified. Never returns `bin/current`, so a
    /// symlink switch cannot retarget the later exec.
    pub fn verified_tui_executable(&self) -> Result<PathBuf, String> {
        install::verify_release_identity(&self.installed, &self.version, &self.archive_sha256)?;
        Ok(self.installed.join("factory-tui"))
    }

    /// Restore the exact runtime/job captured before this update, then require
    /// the previous managed daemon to answer with its exact version. Refuses
    /// to undo a later updater's activation.
    pub fn rollback_after_reexec_failure(
        mut self,
        progress: &mut dyn FnMut(UpdateProgress),
    ) -> Result<ReexecRecovery, String> {
        let Some(plan) = self.rollback.take() else {
            // The only TUI-reachable no-plan outcome is an already-active,
            // exact-version healthy daemon. The transaction changed nothing,
            // so an exec failure has nothing to restore.
            return Ok(ReexecRecovery::NotNeeded);
        };
        progress(UpdateProgress::RollingBack);
        let _lock = self
            .runtime_lock
            .take()
            .ok_or("update rollback lost its mutation lock")?;
        let current = install::active_version(&plan.home)?;
        if current.as_deref() != Some(&plan.installed_version) {
            return Err(format!(
                "active runtime changed after update (expected {}); refusing to roll back another owner",
                plan.installed_version
            ));
        }
        runtime::rollback_after_health_failure_for(
            &plan.target,
            &plan.home,
            &plan.plist,
            &plan.snapshot,
            &plan.installed_version,
            |previous| {
                probes::wait_for_managed_daemon_for(
                    &plan.target,
                    &plan.socket,
                    HEALTH_WAIT,
                    Some(previous),
                    &plan.home,
                )
                .map(|_| ())
            },
        )?;
        remove_pending(&plan.home)?;
        Ok(ReexecRecovery::Restored)
    }
}

fn canonical_endpoint(path: &Path, description: &str) -> Result<PathBuf, String> {
    let name = path
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| format!("{description} has no file name"))?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let parent = fs::canonicalize(parent)
        .map_err(|error| format!("canonicalizing {description} parent: {error}"))?;
    Ok(parent.join(name))
}

fn recovery_authority(
    home: &Path,
    socket: &Path,
    user_home: &Path,
    target: &launchd::LaunchdTarget,
) -> Result<RecoveryAuthority, String> {
    let home = fs::canonicalize(home)
        .map_err(|error| format!("canonicalizing Dark Factory home: {error}"))?;
    let metadata = fs::metadata(&home)
        .map_err(|error| format!("inspecting canonical Dark Factory home: {error}"))?;
    if !metadata.is_dir() {
        return Err("canonical Dark Factory home is not a directory".to_owned());
    }
    let user_home = fs::canonicalize(user_home)
        .map_err(|error| format!("canonicalizing operator home: {error}"))?;
    let plist = launchd::plist_path_for(&user_home, target);
    let plist = if plist.parent().is_some_and(Path::exists) {
        canonical_endpoint(&plist, "launchd plist")?
    } else {
        plist
    };
    Ok(RecoveryAuthority {
        home,
        home_device: metadata.dev(),
        home_inode: metadata.ino(),
        user_home,
        socket: canonical_endpoint(socket, "daemon socket")?,
        plist,
        uid: rustix::process::getuid().as_raw(),
        launchd_domain: target.domain().to_owned(),
        launchd_label: target.label().to_owned(),
    })
}

fn require_recovery_authority(
    pending: &PendingUpdate,
    current: &RecoveryAuthority,
) -> Result<(), String> {
    if &pending.authority != current {
        return Err(
            "interrupted update belongs to a different home, socket, or managed launchd job"
                .to_owned(),
        );
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ExpectedManagedJob {
    Absent,
    Present {
        job: launchd::ExistingJob,
        plist: String,
    },
}

impl ExpectedManagedJob {
    fn capture(plist: &Path, home: &Path, user_home: &Path) -> Result<Self, String> {
        let Some(job) = launchd::read_existing(plist)? else {
            return Ok(Self::Absent);
        };
        launchd::check_home(&job, home, user_home)?;
        let plist = fs::read_to_string(plist)
            .map_err(|error| format!("reading managed launchd job identity: {error}"))?;
        Ok(Self::Present { job, plist })
    }

    fn require_unchanged(
        &self,
        _lock: &runtime::MutationLock,
        plist: &Path,
        home: &Path,
        user_home: &Path,
    ) -> Result<Option<launchd::ExistingJob>, String> {
        let current = Self::capture(plist, home, user_home)?;
        if &current != self {
            return Err(
                "managed launchd job changed after update preflight; refusing stale update"
                    .to_owned(),
            );
        }
        match current {
            Self::Absent => Ok(None),
            Self::Present { job, .. } => Ok(Some(job)),
        }
    }

    fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }
}

struct InstallPreflight {
    authority: RecoveryAuthority,
    target: launchd::LaunchdTarget,
    expected_job: ExpectedManagedJob,
}

fn install_after_preflight(
    preflight: InstallPreflight,
    manifest: &Manifest,
    archive_sha256: String,
    retain_for_reexec: bool,
    progress: &mut dyn FnMut(UpdateProgress),
    log: &mut dyn FnMut(&str),
) -> Result<InstalledUpdate, InstallError> {
    let InstallPreflight {
        authority,
        target,
        expected_job,
    } = preflight;
    let canonical_home = authority.home.clone();
    let canonical_socket = authority.socket.clone();
    let user_home = authority.user_home.clone();
    let home = canonical_home.as_path();
    let socket = canonical_socket.as_path();
    let plist = authority.plist.clone();
    let (runtime_lock, snapshot) = runtime::MutationLock::begin(home, &plist)?;
    let existing = expected_job.require_unchanged(&runtime_lock, &plist, home, &user_home)?;
    let previous_version = snapshot.active_version.clone();

    if snapshot.active_version.as_deref() == Some(manifest.version.as_str()) {
        let installed = install::version_dir(home, &manifest.version);
        let health = if existing.is_some() {
            probes::wait_for_managed_daemon_for(
                &target,
                socket,
                Duration::from_secs(2),
                Some(&manifest.version),
                home,
            )
        } else {
            probes::wait_for_daemon(socket, Duration::from_secs(2), Some(&manifest.version))
        };
        if health.is_ok() {
            install::verify_release_identity(&installed, &manifest.version, &archive_sha256)?;
            log(&format!(
                "{} is already installed and running",
                manifest.version
            ));
            return Ok(InstalledUpdate {
                version: manifest.version.clone(),
                installed,
                daemon: ManagedDaemon::Unchanged,
                health_version: Some(manifest.version.clone()),
                archive_sha256,
                runtime_lock: Some(runtime_lock),
                rollback: None,
            });
        }
    }

    if let Some(active) = snapshot.active_version.as_deref()
        && active != manifest.version
        && !update::is_newer(&manifest.version, active)
    {
        return Err(InstallError::Stale {
            active: active.to_owned(),
        });
    }

    let installed = install::install_release_with_progress(home, manifest, log, &mut |stage| {
        progress(match stage {
            ReleaseInstallStage::Downloading => UpdateProgress::Downloading,
            ReleaseInstallStage::Verifying => UpdateProgress::Verifying,
            ReleaseInstallStage::Unpacking => UpdateProgress::Unpacking,
        });
    })?;

    let mut pending = PendingUpdate {
        version: manifest.version.clone(),
        archive_sha256: archive_sha256.clone(),
        phase: PendingPhase::for_progress(UpdateProgress::Activating).unwrap(),
        snapshot: snapshot.clone(),
        authority,
    };
    write_pending(home, &pending)?;
    progress(UpdateProgress::Activating);
    install::activate(home, &manifest.version)?;
    log(&format!("bin/current -> {}", manifest.version));

    let Some(existing) = existing else {
        log("no launchd job installed; restart the daemon yourself to run the new version");
        remove_pending(home)?;
        return Ok(InstalledUpdate {
            version: manifest.version.clone(),
            installed,
            daemon: ManagedDaemon::NotInstalled,
            health_version: None,
            archive_sha256,
            runtime_lock: Some(runtime_lock),
            rollback: None,
        });
    };

    pending.phase = PendingPhase::for_progress(UpdateProgress::Reloading).unwrap();
    write_pending(home, &pending)?;
    progress(UpdateProgress::Reloading);
    if let Err(error) = launchd::apply_with_rollback_for(
        launchd::ApplyRequest {
            target: &target,
            home,
            plist: &plist,
            existing: Some(&existing),
            provider_directories: &probes::provider_directories(),
            extra_environment: &std::collections::BTreeMap::new(),
            capacity: None,
        },
        || snapshot.restore_runtime(home),
    ) {
        return Err(InstallError::Message(runtime::rollback_report(
            &error,
            previous_version.as_deref(),
            |previous| {
                probes::wait_for_managed_daemon_for(
                    &target,
                    socket,
                    HEALTH_WAIT,
                    Some(previous),
                    home,
                )
                .map(|_| ())
            },
        )));
    }
    log(&format!("rewrote and reloaded {}", plist.display()));
    pending.phase = PendingPhase::for_progress(UpdateProgress::CheckingHealth).unwrap();
    write_pending(home, &pending)?;
    progress(UpdateProgress::CheckingHealth);
    match probes::wait_for_managed_daemon_for(
        &target,
        socket,
        HEALTH_WAIT,
        Some(&manifest.version),
        home,
    ) {
        Ok(version) => {
            if retain_for_reexec {
                pending.phase = PendingPhase::AwaitingRelaunch;
                write_pending(home, &pending)?;
            } else {
                remove_pending(home)?;
            }
            Ok(InstalledUpdate {
                version: manifest.version.clone(),
                installed,
                daemon: ManagedDaemon::Reloaded,
                health_version: Some(version),
                archive_sha256,
                runtime_lock: Some(runtime_lock),
                rollback: retain_for_reexec.then_some(RollbackPlan {
                    home: home.to_owned(),
                    plist,
                    socket: socket.to_owned(),
                    target,
                    installed_version: manifest.version.clone(),
                    snapshot,
                }),
            })
        }
        Err(error) => {
            let rollback = runtime::rollback_after_health_failure_for(
                &target,
                home,
                &plist,
                &snapshot,
                &manifest.version,
                |previous| {
                    probes::wait_for_managed_daemon_for(
                        &target,
                        socket,
                        HEALTH_WAIT,
                        Some(previous),
                        home,
                    )
                    .map(|_| ())
                },
            );
            if rollback.is_ok() {
                remove_pending(home)?;
            }
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

fn pending_path(home: &Path) -> PathBuf {
    home.join(PENDING_UPDATE_FILE)
}

fn write_pending(home: &Path, pending: &PendingUpdate) -> Result<(), String> {
    let bytes = serde_json::to_vec(pending).map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_PENDING_UPDATE_BYTES {
        return Err("managed update recovery record is too large".to_owned());
    }
    let path = pending_path(home);
    let temp = home.join(format!(".{PENDING_UPDATE_FILE}.tmp"));
    let _ = fs::remove_file(&temp);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|error| format!("creating {}: {error}", temp.display()))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("writing {}: {error}", temp.display()))?;
    fs::rename(&temp, &path).map_err(|error| format!("publishing {}: {error}", path.display()))?;
    fs::File::open(home)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("syncing {}: {error}", home.display()))
}

fn read_pending(home: &Path) -> Result<Option<PendingUpdate>, String> {
    let path = pending_path(home);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("inspecting {}: {error}", path.display())),
    };
    if !metadata.file_type().is_file()
        || metadata.permissions().mode() & 0o777 != 0o600
        || metadata.len() > MAX_PENDING_UPDATE_BYTES
    {
        return Err(format!(
            "{} is not a private bounded regular recovery record",
            path.display()
        ));
    }
    let pending: PendingUpdate = serde_json::from_slice(
        &fs::read(&path).map_err(|error| format!("reading {}: {error}", path.display()))?,
    )
    .map_err(|error| format!("parsing {}: {error}", path.display()))?;
    update::canonical_stable_version(&pending.version)?;
    if let Some(previous) = pending.snapshot.active_version.as_deref() {
        update::canonical_stable_version(previous)?;
    }
    if pending.archive_sha256.len() != 64
        || !pending
            .archive_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err("managed update recovery record has an invalid SHA-256".to_owned());
    }
    Ok(Some(pending))
}

fn remove_pending(home: &Path) -> Result<(), String> {
    let path = pending_path(home);
    match fs::remove_file(&path) {
        Ok(()) => fs::File::open(home)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("syncing {}: {error}", home.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("removing {}: {error}", path.display())),
    }
}

fn rollback_pending(
    home: &Path,
    plist: &Path,
    socket: &Path,
    target: &launchd::LaunchdTarget,
    pending: &PendingUpdate,
) -> Result<(), String> {
    match (
        pending.snapshot.active_version.as_deref(),
        pending.snapshot.plist.as_deref(),
    ) {
        (Some(_), Some(_)) => runtime::rollback_after_health_failure_for(
            target,
            home,
            plist,
            &pending.snapshot,
            &pending.version,
            |previous| {
                probes::wait_for_managed_daemon_for(
                    target,
                    socket,
                    HEALTH_WAIT,
                    Some(previous),
                    home,
                )
                .map(|_| ())
            },
        ),
        (_, None) => pending.snapshot.restore_runtime(home),
        (None, Some(_)) => Err(
            "cannot recover a managed launchd job without a previous runtime version".to_owned(),
        ),
    }
}

fn recover_pending_locked(
    home: &Path,
    plist: &Path,
    socket: &Path,
    target: &launchd::LaunchdTarget,
    pending: &PendingUpdate,
) -> Result<(), String> {
    let active = install::active_version(home)?;
    if active.as_deref() != Some(&pending.version)
        && active.as_deref() != pending.snapshot.active_version.as_deref()
    {
        return Err(format!(
            "active runtime changed during recovery (pending {}, active {}); refusing to overwrite another owner",
            pending.version,
            active.as_deref().unwrap_or("none")
        ));
    }
    if pending.phase == PendingPhase::AwaitingRelaunch
        && active.as_deref() == Some(&pending.version)
        && install::verify_release_identity(
            &install::version_dir(home, &pending.version),
            &pending.version,
            &pending.archive_sha256,
        )
        .is_ok()
        && probes::wait_for_managed_daemon_for(
            target,
            socket,
            HEALTH_WAIT,
            Some(&pending.version),
            home,
        )
        .is_ok()
    {
        return remove_pending(home);
    }
    rollback_pending(home, plist, socket, target, pending)?;
    remove_pending(home)
}

/// Resolves a crash-interrupted TUI update before the board offers another
/// mutation. Pre-health phases roll back; a post-health relaunch handoff is
/// committed only when the exact target daemon is still healthy.
pub fn recover_pending(home: &Path, socket: &Path) -> Result<(), String> {
    let user_home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is not set")?;
    let target = launchd::LaunchdTarget::for_user(rustix::process::getuid().as_raw());
    recover_pending_for(home, socket, &user_home, &target)
}

fn recover_pending_for(
    home: &Path,
    socket: &Path,
    user_home: &Path,
    target: &launchd::LaunchdTarget,
) -> Result<(), String> {
    let Some(pending) = read_pending(home)? else {
        return Ok(());
    };
    let authority = recovery_authority(home, socket, user_home, target)?;
    require_recovery_authority(&pending, &authority)?;
    let (_lock, _) = runtime::MutationLock::begin(&authority.home, &authority.plist)?;
    recover_pending_locked(
        &authority.home,
        &authority.plist,
        &authority.socket,
        target,
        &pending,
    )
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
    retain_for_reexec: bool,
    progress: &mut dyn FnMut(UpdateProgress),
    log: &mut dyn FnMut(&str),
) -> Result<InstalledUpdate, InstallError> {
    update::validate_manifest(manifest)?;
    let archive_sha256 = manifest
        .assets
        .get(update::platform_key())
        .ok_or_else(|| {
            InstallError::Message(format!(
                "release {} has no asset for {}",
                manifest.version,
                update::platform_key()
            ))
        })?
        .sha256
        .clone();
    recover_pending(home, socket)?;
    let user_home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| InstallError::Message("HOME is not set".to_owned()))?;
    let target = launchd::LaunchdTarget::for_user(rustix::process::getuid().as_raw());
    let authority = recovery_authority(home, socket, &user_home, &target)?;
    let expected_job =
        ExpectedManagedJob::capture(&authority.plist, &authority.home, &authority.user_home)?;
    if expected_job.is_absent() && require_managed_daemon {
        return Err(InstallError::Message(
            "one-button update requires the managed launchd job; use `factoryctl update --install` for a manually managed daemon"
                .to_owned(),
        ));
    }
    install_after_preflight(
        InstallPreflight {
            authority,
            target,
            expected_job,
        },
        manifest,
        archive_sha256,
        retain_for_reexec,
        progress,
        log,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::process::Command;

    fn test_authority(home: &Path) -> (launchd::LaunchdTarget, RecoveryAuthority) {
        let target = launchd::LaunchdTarget::for_user(rustix::process::getuid().as_raw());
        let authority = recovery_authority(home, &home.join("f.sock"), home, &target).unwrap();
        (target, authority)
    }

    #[test]
    fn exec_failure_after_an_unchanged_runtime_needs_no_rollback() {
        let outcome = InstalledUpdate {
            version: "0.2.6".to_owned(),
            installed: PathBuf::from("/unused/bin/0.2.6"),
            daemon: ManagedDaemon::Unchanged,
            health_version: Some("0.2.6".to_owned()),
            archive_sha256: "00".repeat(32),
            runtime_lock: None,
            rollback: None,
        };
        let mut progress = Vec::new();
        assert_eq!(
            outcome
                .rollback_after_reexec_failure(&mut |stage| progress.push(stage))
                .unwrap(),
            ReexecRecovery::NotNeeded
        );
        assert!(progress.is_empty());
    }

    #[test]
    fn crash_boundaries_record_every_runtime_mutation_but_not_downloads() {
        for progress in [
            UpdateProgress::Checking,
            UpdateProgress::Downloading,
            UpdateProgress::Verifying,
            UpdateProgress::Unpacking,
        ] {
            assert_eq!(PendingPhase::for_progress(progress), None);
        }
        assert_eq!(
            PendingPhase::for_progress(UpdateProgress::Activating),
            Some(PendingPhase::Activating)
        );
        assert_eq!(
            PendingPhase::for_progress(UpdateProgress::Reloading),
            Some(PendingPhase::Reloading)
        );
        assert_eq!(
            PendingPhase::for_progress(UpdateProgress::CheckingHealth),
            Some(PendingPhase::CheckingHealth)
        );
        assert_ne!(PendingPhase::Activating, PendingPhase::AwaitingRelaunch);
        assert_ne!(PendingPhase::Reloading, PendingPhase::AwaitingRelaunch);
        assert_ne!(PendingPhase::CheckingHealth, PendingPhase::AwaitingRelaunch);
    }

    #[test]
    fn interrupted_mutations_and_an_unverified_handoff_are_durably_rolled_back() {
        let home = tempfile::tempdir().unwrap();
        let source = home.path().join("source");
        fs::create_dir_all(&source).unwrap();
        for name in install::BINARIES {
            let path = source.join(name);
            fs::write(&path, format!("#!/bin/sh\necho {name}\n")).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        install::install_from_dir(home.path(), &source, "0.2.5").unwrap();
        install::install_from_dir(home.path(), &source, "0.2.6").unwrap();
        let (target, authority) = test_authority(home.path());
        for phase in [
            PendingPhase::Activating,
            PendingPhase::Reloading,
            PendingPhase::CheckingHealth,
            PendingPhase::AwaitingRelaunch,
        ] {
            install::activate(home.path(), "0.2.5").unwrap();
            let pending = PendingUpdate {
                version: "0.2.6".to_owned(),
                archive_sha256: "00".repeat(32),
                phase,
                snapshot: runtime::MutationSnapshot {
                    active_version: Some("0.2.5".to_owned()),
                    plist: None,
                },
                authority: authority.clone(),
            };
            write_pending(home.path(), &pending).unwrap();
            install::activate(home.path(), "0.2.6").unwrap();
            recover_pending_locked(
                home.path(),
                &home.path().join("unused.plist"),
                &home.path().join("unused.sock"),
                &target,
                &pending,
            )
            .unwrap();
            assert_eq!(
                install::active_version(home.path()).unwrap().as_deref(),
                Some("0.2.5")
            );
            assert!(!pending_path(home.path()).exists());
        }
    }

    #[test]
    fn recovery_record_rejects_an_unsafe_previous_runtime_path() {
        let home = tempfile::tempdir().unwrap();
        let (_, authority) = test_authority(home.path());
        let pending = PendingUpdate {
            version: "0.2.6".to_owned(),
            archive_sha256: "00".repeat(32),
            phase: PendingPhase::Activating,
            snapshot: runtime::MutationSnapshot {
                active_version: Some("../../outside".to_owned()),
                plist: None,
            },
            authority,
        };
        write_pending(home.path(), &pending).unwrap();
        let error = read_pending(home.path()).unwrap_err();
        assert!(error.contains("release version"), "{error}");
    }

    #[test]
    fn recovery_refuses_socket_or_operator_home_substitution_before_mutation() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("factory-home");
        let user_a = root.path().join("user-a");
        let user_b = root.path().join("user-b");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&user_a).unwrap();
        fs::create_dir_all(&user_b).unwrap();
        let target = launchd::LaunchdTarget::for_user(rustix::process::getuid().as_raw());
        let plist_a = launchd::plist_path_for(&user_a, &target);
        fs::create_dir_all(plist_a.parent().unwrap()).unwrap();
        fs::write(&plist_a, "saved managed job").unwrap();
        let plist_b = launchd::plist_path_for(&user_b, &target);
        fs::create_dir_all(plist_b.parent().unwrap()).unwrap();
        fs::write(&plist_b, "different managed job").unwrap();
        let socket_a = home.join("a.sock");
        let socket_b = home.join("b.sock");
        let _listener_a = UnixListener::bind(&socket_a).unwrap();
        let _listener_b = UnixListener::bind(&socket_b).unwrap();

        let source = root.path().join("source");
        fs::create_dir_all(&source).unwrap();
        for name in install::BINARIES {
            let path = source.join(name);
            fs::write(&path, format!("#!/bin/sh\necho {name}\n")).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        install::install_from_dir(&home, &source, "0.2.5").unwrap();
        install::install_from_dir(&home, &source, "0.2.6").unwrap();
        install::activate(&home, "0.2.5").unwrap();
        let (_lock, snapshot) = runtime::MutationLock::begin(&home, &plist_a).unwrap();
        let pending = PendingUpdate {
            version: "0.2.6".to_owned(),
            archive_sha256: "00".repeat(32),
            phase: PendingPhase::Reloading,
            snapshot,
            authority: recovery_authority(&home, &socket_a, &user_a, &target).unwrap(),
        };
        write_pending(&home, &pending).unwrap();
        install::activate(&home, "0.2.6").unwrap();

        for (socket, user_home) in [(&socket_b, &user_a), (&socket_a, &user_b)] {
            let error = recover_pending_for(&home, socket, user_home, &target).unwrap_err();
            assert!(error.contains("different home, socket, or managed launchd job"));
            assert_eq!(
                install::active_version(&home).unwrap().as_deref(),
                Some("0.2.6")
            );
            assert_eq!(fs::read_to_string(&plist_a).unwrap(), "saved managed job");
            assert_eq!(
                fs::read_to_string(&plist_b).unwrap(),
                "different managed job"
            );
            assert!(pending_path(&home).exists());
        }
    }

    #[cfg(target_os = "macos")]
    fn fake_manifest(version: &str) -> Manifest {
        Manifest {
            version: version.to_owned(),
            assets: [(
                update::platform_key().to_owned(),
                update::Asset {
                    url: "file:///must-not-be-downloaded".to_owned(),
                    sha256: "00".repeat(32),
                },
            )]
            .into(),
        }
    }

    #[cfg(target_os = "macos")]
    fn install_fake_runtime(home: &Path, version: &str) {
        let source = home.join("source");
        fs::create_dir_all(&source).unwrap();
        for name in install::BINARIES {
            let path = source.join(name);
            fs::write(&path, format!("#!/bin/sh\necho {name}\n")).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        install::install_from_dir(home, &source, version).unwrap();
        install::activate(home, version).unwrap();
    }

    #[cfg(target_os = "macos")]
    fn managed_plist(home: &Path) -> String {
        format!(
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>ProgramArguments</key><array><string>{}/bin/current/factoryd</string></array><key>EnvironmentVariables</key><dict><key>DARK_FACTORY_HOME</key><string>{}</string></dict></dict></plist>",
            home.display(),
            home.display()
        )
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn install_refuses_a_managed_job_appearing_after_preflight_before_any_mutation() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("factory-home");
        let user_home = root.path().join("user-home");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&user_home).unwrap();
        install_fake_runtime(&home, "0.2.5");
        let target = launchd::LaunchdTarget::for_user(rustix::process::getuid().as_raw());
        let authority =
            recovery_authority(&home, &home.join("f.sock"), &user_home, &target).unwrap();
        let plist = launchd::plist_path_for(&user_home, &target);
        let expected = ExpectedManagedJob::capture(&plist, &home, &user_home).unwrap();
        assert!(expected.is_absent());

        fs::create_dir_all(plist.parent().unwrap()).unwrap();
        let appeared = managed_plist(&home);
        fs::write(&plist, &appeared).unwrap();
        let mut progress = Vec::new();
        let error = install_after_preflight(
            InstallPreflight {
                authority,
                target,
                expected_job: expected,
            },
            &fake_manifest("0.2.6"),
            "00".repeat(32),
            false,
            &mut |stage| progress.push(stage),
            &mut |_| {},
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("managed launchd job changed after update preflight"),
            "{error}"
        );
        assert!(progress.is_empty(), "the archive must not be downloaded");
        assert_eq!(
            install::active_version(&home).unwrap().as_deref(),
            Some("0.2.5")
        );
        assert!(!install::version_dir(&home, "0.2.6").exists());
        assert!(!pending_path(&home).exists());
        assert_eq!(fs::read_to_string(&plist).unwrap(), appeared);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn install_refuses_a_managed_job_disappearing_after_preflight_before_any_mutation() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("factory-home");
        let user_home = root.path().join("user-home");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&user_home).unwrap();
        install_fake_runtime(&home, "0.2.5");
        let target = launchd::LaunchdTarget::for_user(rustix::process::getuid().as_raw());
        let plist = launchd::plist_path_for(&user_home, &target);
        fs::create_dir_all(plist.parent().unwrap()).unwrap();
        fs::write(&plist, managed_plist(&home)).unwrap();
        let authority =
            recovery_authority(&home, &home.join("f.sock"), &user_home, &target).unwrap();
        let expected = ExpectedManagedJob::capture(&plist, &home, &user_home).unwrap();
        assert!(!expected.is_absent());

        fs::remove_file(&plist).unwrap();
        let mut progress = Vec::new();
        let error = install_after_preflight(
            InstallPreflight {
                authority,
                target,
                expected_job: expected,
            },
            &fake_manifest("0.2.6"),
            "00".repeat(32),
            false,
            &mut |stage| progress.push(stage),
            &mut |_| {},
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("managed launchd job changed after update preflight"),
            "{error}"
        );
        assert!(progress.is_empty(), "the archive must not be downloaded");
        assert_eq!(
            install::active_version(&home).unwrap().as_deref(),
            Some("0.2.5")
        );
        assert!(!install::version_dir(&home, "0.2.6").exists());
        assert!(!pending_path(&home).exists());
        assert!(!plist.exists());
    }

    #[test]
    fn verified_exec_path_is_not_retargeted_by_a_current_link_swap() {
        let home = tempfile::tempdir().unwrap();
        let source = home.path().join("source");
        fs::create_dir_all(&source).unwrap();
        for name in install::BINARIES {
            let path = source.join(name);
            fs::write(&path, format!("#!/bin/sh\necho {name} exact\n")).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let archive = home.path().join("release.tar.gz");
        assert!(
            Command::new("tar")
                .arg("-czf")
                .arg(&archive)
                .arg("-C")
                .arg(&source)
                .args(install::BINARIES)
                .status()
                .unwrap()
                .success()
        );
        let sha = format!("{:x}", Sha256::digest(fs::read(&archive).unwrap()));
        let manifest = Manifest {
            version: "0.2.6".to_owned(),
            assets: [(
                update::platform_key().to_owned(),
                update::Asset {
                    url: format!("file://{}", archive.display()),
                    sha256: sha.clone(),
                },
            )]
            .into(),
        };
        let installed = install::install_release(home.path(), &manifest, &mut |_| {}).unwrap();
        install::activate(home.path(), "0.2.6").unwrap();
        let outcome = InstalledUpdate {
            version: "0.2.6".to_owned(),
            installed: installed.clone(),
            daemon: ManagedDaemon::Reloaded,
            health_version: Some("0.2.6".to_owned()),
            archive_sha256: sha,
            runtime_lock: None,
            rollback: None,
        };
        let exact = outcome.verified_tui_executable().unwrap();

        install::install_from_dir(home.path(), &source, "0.2.7").unwrap();
        install::activate(home.path(), "0.2.7").unwrap();
        assert_eq!(exact, installed.join("factory-tui"));
        assert_ne!(
            exact,
            install::current_link(home.path()).join("factory-tui")
        );
        assert!(
            String::from_utf8(fs::read(exact).unwrap())
                .unwrap()
                .contains("exact")
        );
    }
}
