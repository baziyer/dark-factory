//! Exact-source Rust workspace test preparation and immutable launch.
//!
//! This module owns one deliberately fixed operation. The caller supplies a
//! daemon-owned Change root and a trusted Cargo executable; it does not supply
//! Cargo arguments, build environment, executable paths, or test arguments.
//! The caller must durably revoke Change mutation authority and reap the
//! provider before calling [`RustWorkspaceTest::prepare`]. The before/copy/
//! after manifests below are a fail-closed backstop for external mutation, not
//! a competing source lease.
//!
//! Cargo and test descendants inherit the caller's process group. Process
//! registration and reaping remain the execution kernel's responsibility.

use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;
const BUNDLE_EXECUTABLE_MODE: u32 = 0o500;
const MAX_PATH_BYTES: usize = 4096;
const MAX_SOURCE_ENTRIES: u64 = 2_000_000;
const MAX_SOURCE_BYTES: u64 = 1_099_511_627_776;
const MAX_BUILD_STDOUT: usize = 32 * 1024 * 1024;
const MAX_BUILD_STDERR: usize = 4 * 1024 * 1024;
const MAX_TEST_OUTPUT: usize = 4 * 1024 * 1024;
const MAX_CHECKPOINT_BYTES: usize = 64 * 1024;
const MAX_BUNDLE_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
const MAX_WORKER_INVOCATION_BYTES: usize = 64 * 1024;
const MAX_WORKER_RESULT_BYTES: usize = 512 * 1024;
const MAX_DIAGNOSTIC_BYTES: usize = 4096;
const MAX_TEST_ARTIFACTS: usize = 4096;
const CACHE_POLICY: &str = "rust-workspace-test-v1";
const MANIFEST_VERSION: u32 = 1;
const BUILD_ARGUMENTS: &[&str] = &[
    "test",
    "--locked",
    "--workspace",
    "--all-targets",
    "--no-run",
    "--message-format=json-render-diagnostics",
];
const TEST_ARGUMENTS: &[&str] = &["--test-threads=1"];

/// The one fixed Rust verification operation used by the daemon.
#[derive(Clone, Debug)]
struct RustWorkspaceTest {
    cargo: TrustedExecutable,
    rustc: TrustedExecutable,
    cache_key: String,
    change_root: PathBuf,
    change_identity: FileIdentity,
    cache_root: PathBuf,
    cache_identity: FileIdentity,
    cache_target: PathBuf,
    cargo_home: PathBuf,
    build_home: PathBuf,
    temporary_root: PathBuf,
    temporary_identity: FileIdentity,
    snapshots_root: PathBuf,
    bundle_staging_root: PathBuf,
    checkpoints_root: PathBuf,
}

/// A content-addressed source snapshot and executable bundle ready to launch.
#[derive(Clone, Debug, Eq, PartialEq)]
struct PreparedRustWorkspaceTest {
    snapshot_digest: String,
    snapshot_root: PathBuf,
    bundle_digest: String,
    bundle_root: PathBuf,
}

impl PreparedRustWorkspaceTest {
    #[cfg(test)]
    #[must_use]
    fn snapshot_digest(&self) -> &str {
        &self.snapshot_digest
    }

    #[cfg(test)]
    #[must_use]
    fn snapshot_root(&self) -> &Path {
        &self.snapshot_root
    }

    #[cfg(test)]
    #[must_use]
    fn bundle_digest(&self) -> &str {
        &self.bundle_digest
    }

    #[cfg(test)]
    #[must_use]
    fn bundle_root(&self) -> &Path {
        &self.bundle_root
    }
}

/// Bounded output from one prepared test executable.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct RustTestExecution {
    pub executable_digest: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub diagnostic: String,
}

/// Bounded result from executing every test artifact serially.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct RustWorkspaceTestResult {
    pub snapshot_digest: String,
    pub bundle_digest: String,
    pub cache_target: PathBuf,
    pub success: bool,
    pub executions: Vec<RustTestExecution>,
}

/// Bounded hidden-worker input. All paths are daemon-derived; no arguments or
/// environment can be supplied by the caller.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct WorkerInvocation {
    pub cargo: PathBuf,
    pub changes_root: PathBuf,
    pub change_id: Uuid,
    pub project_incarnation_id: String,
    pub cache_root: PathBuf,
    pub temporary_root: PathBuf,
    pub change_identity: ExactDirectoryIdentity,
    pub cache_identity: ExactDirectoryIdentity,
    pub temporary_identity: ExactDirectoryIdentity,
}

/// Device/inode binding recorded by the daemon before worker launch.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ExactDirectoryIdentity {
    pub device: u64,
    pub inode: u64,
}

/// Exact registered filesystem identity and allocated storage.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ExactTreeMeasurement {
    pub path: PathBuf,
    pub device: u64,
    pub inode: u64,
    pub allocated_bytes: u64,
}

/// Bounded hidden-worker result recovered by the daemon after worker exit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct WorkerResult {
    pub snapshot_digest: Option<String>,
    pub bundle_digest: Option<String>,
    pub success: bool,
    pub diagnostic: String,
    pub bundle_staging: Option<ExactTreeMeasurement>,
}

#[derive(Clone, Debug)]
struct TrustedExecutable {
    path: PathBuf,
    identity: FileIdentity,
    digest: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    size: u64,
    mode: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TreeManifest {
    root_mode: u32,
    entries: Vec<TreeEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TreeEntry {
    path: PathBuf,
    kind: EntryKind,
    mode: u32,
    size: u64,
    digest: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryKind {
    Directory,
    File,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct BundleManifest {
    version: u32,
    snapshot_digest: String,
    cargo_lock_digest: String,
    cache_key: String,
    cache_target: PathBuf,
    build_arguments: Vec<String>,
    build_environment: Vec<(String, String)>,
    executables: Vec<BundleExecutable>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct BundleExecutable {
    name: String,
    digest: String,
    size: u64,
    mode: u32,
    device: u64,
    inode: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct BundleCheckpoint {
    version: u32,
    snapshot_digest: String,
    staging_name: String,
    staging_device: Option<u64>,
    staging_inode: Option<u64>,
    bundle_digest: Option<String>,
}

#[derive(Deserialize)]
struct CargoMessage {
    reason: String,
    profile: Option<CargoProfile>,
    executable: Option<PathBuf>,
}

#[derive(Deserialize)]
struct CargoProfile {
    test: bool,
}

struct BoundedOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_exceeded: bool,
    stderr_exceeded: bool,
}

/// Exact-source build or launch failure.
#[derive(Debug, Error)]
pub enum RustVerifyError {
    #[error("trusted Cargo is not an absolute executable regular file: {0:?}")]
    InvalidCargo(PathBuf),
    #[error("daemon-owned verifier path is unsafe: {0:?}")]
    UnsafePath(PathBuf),
    #[error("daemon-owned verifier roots overlap: {0:?} and {1:?}")]
    OverlappingRoots(PathBuf, PathBuf),
    #[error("private directory is missing, replaced, or not owner-only: {0:?}")]
    UnsafePrivateDirectory(PathBuf),
    #[error("source tree contains a link, special entry, or unsafe path: {0:?}")]
    UnsafeSourceEntry(PathBuf),
    #[error("source tree crossed its hard entry or byte bound")]
    SourceTooLarge,
    #[error("source changed while its exact snapshot was copied")]
    SourceChanged,
    #[error("Cargo.lock is absent or not a regular file in the selected snapshot")]
    CargoLockMissing,
    #[error("Cargo output exceeded its hard bound")]
    BuildOutputTooLarge,
    #[error("Cargo failed with status {status}: {stderr}")]
    BuildFailed { status: String, stderr: String },
    #[error("Cargo emitted invalid or unsupported JSON artifact output")]
    InvalidCargoOutput,
    #[error("Cargo produced no executable test artifacts")]
    NoTestArtifacts,
    #[error("Cargo named an executable outside the one managed cache: {0:?}")]
    ArtifactOutsideCache(PathBuf),
    #[error("Cargo artifact is missing, replaced, non-regular, or non-executable: {0:?}")]
    InvalidArtifact(PathBuf),
    #[error("bundle checkpoint is invalid or conflicts with this source snapshot")]
    InvalidCheckpoint,
    #[error("bundle manifest is missing, oversized, malformed, or inconsistent")]
    InvalidBundle,
    #[error("prepared executable was replaced or tampered with: {0:?}")]
    ExecutableTampered(PathBuf),
    #[error("test output exceeded its hard bound: {0:?}")]
    TestOutputTooLarge(PathBuf),
    #[error("I/O failed at {path:?}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("JSON encoding or decoding failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("worker output reader panicked")]
    OutputReaderPanicked,
}

type Result<T> = std::result::Result<T, RustVerifyError>;

impl RustWorkspaceTest {
    /// Fixes the trusted Cargo identity and every daemon-derived storage root.
    /// The Change path itself is always derived from `changes_root/change_id`.
    fn new(
        cargo: impl AsRef<Path>,
        changes_root: impl AsRef<Path>,
        change_id: Uuid,
        project_incarnation_id: &str,
        cache_root: impl AsRef<Path>,
        temporary_root: impl AsRef<Path>,
    ) -> Result<Self> {
        let (cargo, rustc) = trusted_toolchain(cargo.as_ref())?;
        let cache_key = cache_key_from_trusted(project_incarnation_id, &cargo, &rustc)?;
        let changes_root = checked_absolute(changes_root.as_ref())?;
        verify_private_directory(&changes_root)?;
        let change_root = changes_root.join(change_id.simple().to_string());
        verify_private_directory(&change_root)?;
        let change_identity = directory_identity(&change_root)?;

        let cache_root = checked_absolute(cache_root.as_ref())?;
        verify_private_directory(&cache_root)?;
        if cache_root.file_name().and_then(OsStr::to_str) != Some(cache_key.as_str()) {
            return Err(RustVerifyError::UnsafePath(cache_root));
        }
        let cache_identity = directory_identity(&cache_root)?;
        let temporary_root = checked_absolute(temporary_root.as_ref())?;
        verify_private_directory(&temporary_root)?;
        let temporary_identity = directory_identity(&temporary_root)?;
        let roots = [
            change_root.clone(),
            cache_root.clone(),
            temporary_root.clone(),
        ];
        ensure_disjoint(&roots)?;

        let cache_target = prepare_private_root(&cache_root.join("workspace-test-target"))?;
        let cargo_home = prepare_private_root(&cache_root.join("cargo-home"))?;
        let build_home = prepare_private_root(&cache_root.join("home"))?;
        let snapshots_root = prepare_private_root(&temporary_root.join("snapshots"))?;
        let bundle_staging_root = prepare_private_root(&temporary_root.join("bundle-staging"))?;
        let checkpoints_root = prepare_private_root(&temporary_root.join("checkpoints"))?;
        Ok(Self {
            cargo,
            rustc,
            cache_key,
            change_root,
            change_identity,
            cache_root,
            cache_identity,
            cache_target,
            cargo_home,
            build_home,
            temporary_root,
            temporary_identity,
            snapshots_root,
            bundle_staging_root,
            checkpoints_root,
        })
    }

    #[cfg(test)]
    #[must_use]
    fn cache_target(&self) -> &Path {
        &self.cache_target
    }

    /// Selects a stable source snapshot, builds into the one project cache,
    /// and stages an immutable content-addressed executable bundle inside the
    /// invocation-owned temporary root.
    fn prepare(&self) -> Result<PreparedRustWorkspaceTest> {
        self.verify_roots()?;
        self.cargo.verify()?;
        self.rustc.verify()?;
        let (snapshot_digest, snapshot_root) = self.prepare_snapshot()?;
        if let Some(prepared) = self.recover_checkpoint(&snapshot_digest, &snapshot_root)? {
            return Ok(prepared);
        }

        let artifacts = self.build(&snapshot_root)?;
        if capture_tree(&snapshot_root)?.digest() != snapshot_digest {
            return Err(RustVerifyError::SourceChanged);
        }
        self.publish_bundle(&snapshot_digest, &snapshot_root, &artifacts)
    }

    /// Reopens and verifies every published executable immediately before its
    /// fixed path launch. The held file remains live until that test exits.
    fn execute(&self, prepared: &PreparedRustWorkspaceTest) -> Result<RustWorkspaceTestResult> {
        self.verify_prepared_paths(prepared)?;
        let manifest = read_bundle_manifest(&prepared.bundle_root)?;
        verify_bundle(
            &prepared.bundle_root,
            &manifest,
            &prepared.snapshot_digest,
            &prepared.bundle_digest,
            &self.cache_key,
        )?;
        let mut executions = Vec::with_capacity(manifest.executables.len());
        for executable in &manifest.executables {
            let path = prepared.bundle_root.join("bin").join(&executable.name);
            let mut held = open_regular_nofollow(&path)
                .map_err(|_| RustVerifyError::ExecutableTampered(path.clone()))?;
            verify_opened_executable(&path, &mut held, executable)?;

            let mut command = Command::new(&path);
            command
                .args(TEST_ARGUMENTS)
                .current_dir(&prepared.snapshot_root)
                .env_clear();
            self.apply_fixed_environment(&mut command);
            let output = run_bounded(command, MAX_TEST_OUTPUT, MAX_TEST_OUTPUT)?;
            // `held` intentionally remains alive across spawn and wait. On
            // macOS a safe descriptor exec is unavailable in stable Rust, so
            // the private exact path is the handoff under the same-UID caveat.
            drop(held);
            if output.stdout_exceeded || output.stderr_exceeded {
                return Err(RustVerifyError::TestOutputTooLarge(path));
            }
            let diagnostic_bytes = if output.stderr.is_empty() && !output.status.success() {
                &output.stdout
            } else {
                &output.stderr
            };
            executions.push(RustTestExecution {
                executable_digest: executable.digest.clone(),
                success: output.status.success(),
                exit_code: output.status.code(),
                diagnostic: bounded_diagnostic(diagnostic_bytes),
            });
        }
        Ok(RustWorkspaceTestResult {
            snapshot_digest: prepared.snapshot_digest.clone(),
            bundle_digest: prepared.bundle_digest.clone(),
            cache_target: self.cache_target.clone(),
            success: executions.iter().all(|execution| execution.success),
            executions,
        })
    }

    fn verify_roots(&self) -> Result<()> {
        self.cargo.verify()?;
        verify_bound_directory(&self.change_root, self.change_identity)?;
        verify_bound_directory(&self.cache_root, self.cache_identity)?;
        verify_bound_directory(&self.temporary_root, self.temporary_identity)?;
        for root in [
            &self.cache_root,
            &self.cache_target,
            &self.cargo_home,
            &self.build_home,
            &self.snapshots_root,
            &self.bundle_staging_root,
            &self.checkpoints_root,
        ] {
            verify_private_directory(root)?;
        }
        Ok(())
    }

    fn prepare_snapshot(&self) -> Result<(String, PathBuf)> {
        let before_identity = directory_identity(&self.change_root)?;
        let before = capture_tree(&self.change_root)?;
        let digest = before.digest();
        let published = self.snapshots_root.join(&digest);
        if path_exists(&published)? {
            let existing = capture_tree(&published)?;
            if existing.digest() != digest {
                return Err(RustVerifyError::SourceChanged);
            }
            verify_directory_identity(&self.change_root, before_identity)?;
            let after = capture_tree(&self.change_root)?;
            if before != after {
                return Err(RustVerifyError::SourceChanged);
            }
            return Ok((digest, published));
        }

        let staging = self
            .snapshots_root
            .join(format!(".staging-{}", Uuid::new_v4().simple()));
        create_private_directory(&staging)?;
        let copy_result = (|| {
            copy_tree(&self.change_root, &staging, &before)?;
            let copied = capture_tree(&staging)?;
            verify_directory_identity(&self.change_root, before_identity)?;
            let after = capture_tree(&self.change_root)?;
            if before != copied || before != after {
                return Err(RustVerifyError::SourceChanged);
            }
            sync_tree(&staging)?;
            fs::rename(&staging, &published).map_err(|source| RustVerifyError::Io {
                path: published.clone(),
                source,
            })?;
            sync_directory(&self.snapshots_root)?;
            Ok(())
        })();
        if copy_result.is_err() {
            let _ = fs::remove_dir_all(&staging);
        }
        copy_result?;
        Ok((digest, published))
    }

    fn build(&self, snapshot: &Path) -> Result<Vec<PathBuf>> {
        let mut command = Command::new(&self.cargo.path);
        command
            .args(BUILD_ARGUMENTS)
            .current_dir(snapshot)
            .env_clear();
        self.apply_fixed_environment(&mut command);
        let output = run_bounded(command, MAX_BUILD_STDOUT, MAX_BUILD_STDERR)?;
        if output.stdout_exceeded || output.stderr_exceeded {
            return Err(RustVerifyError::BuildOutputTooLarge);
        }
        if !output.status.success() {
            return Err(RustVerifyError::BuildFailed {
                status: output.status.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        parse_artifacts(&output.stdout, &self.cache_target)
    }

    fn apply_fixed_environment(&self, command: &mut Command) {
        let path = fixed_path(&self.cargo.path);
        command
            .env("PATH", path)
            .env("HOME", &self.build_home)
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_TARGET_DIR", &self.cache_target)
            .env("CARGO_INCREMENTAL", "0")
            .env("CARGO_TERM_COLOR", "never")
            .env("CARGO_NET_GIT_FETCH_WITH_CLI", "false")
            .env("RUST_BACKTRACE", "0")
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .env("TERM", "dumb")
            .env("NO_COLOR", "1");
    }

    fn build_environment_manifest(&self) -> Vec<(String, String)> {
        vec![
            ("CARGO_HOME".into(), self.cargo_home.display().to_string()),
            ("CARGO_INCREMENTAL".into(), "0".into()),
            ("CARGO_NET_GIT_FETCH_WITH_CLI".into(), "false".into()),
            (
                "CARGO_TARGET_DIR".into(),
                self.cache_target.display().to_string(),
            ),
            ("CARGO_TERM_COLOR".into(), "never".into()),
            ("HOME".into(), self.build_home.display().to_string()),
            ("LANG".into(), "C".into()),
            ("LC_ALL".into(), "C".into()),
            ("NO_COLOR".into(), "1".into()),
            (
                "PATH".into(),
                fixed_path(&self.cargo.path).to_string_lossy().into_owned(),
            ),
            ("RUST_BACKTRACE".into(), "0".into()),
            ("TERM".into(), "dumb".into()),
        ]
    }

    fn checkpoint_path(&self, snapshot_digest: &str) -> PathBuf {
        self.checkpoints_root
            .join(format!("{snapshot_digest}.json"))
    }

    fn recover_checkpoint(
        &self,
        snapshot_digest: &str,
        snapshot_root: &Path,
    ) -> Result<Option<PreparedRustWorkspaceTest>> {
        let path = self.checkpoint_path(snapshot_digest);
        if !path_exists(&path)? {
            return Ok(None);
        }
        let checkpoint: BundleCheckpoint = read_bounded_json(&path, MAX_CHECKPOINT_BYTES)?;
        if checkpoint.version != MANIFEST_VERSION
            || checkpoint.snapshot_digest != snapshot_digest
            || !safe_name(&checkpoint.staging_name)
        {
            return Err(RustVerifyError::InvalidCheckpoint);
        }
        let staging = self.bundle_staging_root.join(&checkpoint.staging_name);
        let Some(bundle_digest) = checkpoint.bundle_digest.as_ref() else {
            self.remove_checkpointed_staging(&staging, &checkpoint)?;
            remove_exact_file(&path)?;
            return Ok(None);
        };
        if !valid_digest(bundle_digest) {
            return Err(RustVerifyError::InvalidCheckpoint);
        }
        verify_checkpointed_directory(&staging, &checkpoint)?;
        let manifest = read_bundle_manifest(&staging)?;
        verify_bundle(
            &staging,
            &manifest,
            snapshot_digest,
            bundle_digest,
            &self.cache_key,
        )?;
        Ok(Some(PreparedRustWorkspaceTest {
            snapshot_digest: snapshot_digest.to_owned(),
            snapshot_root: snapshot_root.to_owned(),
            bundle_digest: bundle_digest.clone(),
            bundle_root: staging,
        }))
    }

    fn remove_checkpointed_staging(
        &self,
        staging: &Path,
        checkpoint: &BundleCheckpoint,
    ) -> Result<()> {
        if !path_exists(staging)? {
            return Ok(());
        }
        if checkpoint.staging_device.is_some() || checkpoint.staging_inode.is_some() {
            verify_checkpointed_directory(staging, checkpoint)?;
        } else {
            verify_private_directory(staging)?;
        }
        fs::remove_dir_all(staging).map_err(|source| RustVerifyError::Io {
            path: staging.to_owned(),
            source,
        })?;
        sync_directory(&self.bundle_staging_root)
    }

    fn publish_bundle(
        &self,
        snapshot_digest: &str,
        snapshot_root: &Path,
        artifacts: &[PathBuf],
    ) -> Result<PreparedRustWorkspaceTest> {
        let staging_name = format!(".staging-{}", Uuid::new_v4().simple());
        let staging = self.bundle_staging_root.join(&staging_name);
        let checkpoint_path = self.checkpoint_path(snapshot_digest);
        let mut checkpoint = BundleCheckpoint {
            version: MANIFEST_VERSION,
            snapshot_digest: snapshot_digest.to_owned(),
            staging_name,
            staging_device: None,
            staging_inode: None,
            bundle_digest: None,
        };
        write_atomic_json(
            &checkpoint_path,
            &checkpoint,
            MAX_CHECKPOINT_BYTES,
            &self.checkpoints_root,
        )?;
        create_private_directory(&staging)?;
        let staging_identity = directory_identity(&staging)?;
        checkpoint.staging_device = Some(staging_identity.device);
        checkpoint.staging_inode = Some(staging_identity.inode);
        write_atomic_json(
            &checkpoint_path,
            &checkpoint,
            MAX_CHECKPOINT_BYTES,
            &self.checkpoints_root,
        )?;

        (|| {
            let bin = staging.join("bin");
            create_private_directory(&bin)?;
            let executables = copy_artifacts(artifacts, &bin)?;
            let cargo_lock_digest = hash_path(&snapshot_root.join("Cargo.lock"))
                .map_err(|_| RustVerifyError::CargoLockMissing)?;
            let manifest = BundleManifest {
                version: MANIFEST_VERSION,
                snapshot_digest: snapshot_digest.to_owned(),
                cargo_lock_digest,
                cache_key: self.cache_key.clone(),
                cache_target: self.cache_target.clone(),
                build_arguments: BUILD_ARGUMENTS
                    .iter()
                    .map(|argument| (*argument).to_owned())
                    .collect(),
                build_environment: self.build_environment_manifest(),
                executables,
            };
            let bundle_digest = bundle_digest(&manifest)?;
            write_create_only_json(
                &staging.join("manifest.json"),
                &manifest,
                MAX_BUNDLE_MANIFEST_BYTES,
            )?;
            sync_tree(&staging)?;
            checkpoint.bundle_digest = Some(bundle_digest.clone());
            write_atomic_json(
                &checkpoint_path,
                &checkpoint,
                MAX_CHECKPOINT_BYTES,
                &self.checkpoints_root,
            )?;
            Ok(PreparedRustWorkspaceTest {
                snapshot_digest: snapshot_digest.to_owned(),
                snapshot_root: snapshot_root.to_owned(),
                bundle_digest,
                bundle_root: staging,
            })
        })()
    }

    fn verify_prepared_paths(&self, prepared: &PreparedRustWorkspaceTest) -> Result<()> {
        if !valid_digest(&prepared.snapshot_digest)
            || !valid_digest(&prepared.bundle_digest)
            || prepared.snapshot_root != self.snapshots_root.join(&prepared.snapshot_digest)
            || prepared.bundle_root.parent() != Some(self.bundle_staging_root.as_path())
        {
            return Err(RustVerifyError::InvalidBundle);
        }
        Ok(())
    }
}

/// Writes one create-only, owner-only, bounded worker invocation.
pub(crate) fn write_worker_invocation(path: &Path, invocation: &WorkerInvocation) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| RustVerifyError::UnsafePath(path.to_owned()))?;
    verify_private_directory(parent)?;
    write_create_only_json(path, invocation, MAX_WORKER_INVOCATION_BYTES)
}

/// Reads one no-follow, owner-only, bounded worker invocation.
fn read_worker_invocation(path: &Path) -> Result<WorkerInvocation> {
    read_bounded_json(path, MAX_WORKER_INVOCATION_BYTES)
}

/// Writes one create-only, owner-only, bounded worker result.
fn write_worker_result(path: &Path, result: &WorkerResult) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| RustVerifyError::UnsafePath(path.to_owned()))?;
    write_atomic_json(path, result, MAX_WORKER_RESULT_BYTES, parent)
}

/// Reads one no-follow, owner-only, bounded worker result.
pub(crate) fn read_worker_result(path: &Path) -> Result<WorkerResult> {
    read_bounded_json(path, MAX_WORKER_RESULT_BYTES)
}

/// Executes a complete hidden-worker invocation without accepting arbitrary
/// Cargo/test arguments or environment.
fn run_worker(invocation: &WorkerInvocation) -> Result<WorkerResult> {
    verify_invocation_roots(invocation)?;
    let operation = RustWorkspaceTest::new(
        &invocation.cargo,
        &invocation.changes_root,
        invocation.change_id,
        &invocation.project_incarnation_id,
        &invocation.cache_root,
        &invocation.temporary_root,
    )?;
    let prepared = operation.prepare()?;
    let result = operation.execute(&prepared)?;
    let diagnostic = result
        .executions
        .iter()
        .filter(|execution| !execution.success && !execution.diagnostic.is_empty())
        .map(|execution| execution.diagnostic.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(WorkerResult {
        snapshot_digest: Some(result.snapshot_digest),
        bundle_digest: Some(result.bundle_digest),
        success: result.success,
        diagnostic: bounded_diagnostic(diagnostic.as_bytes()),
        bundle_staging: Some(measure_exact_tree(&prepared.bundle_root)?),
    })
}

fn verify_invocation_roots(invocation: &WorkerInvocation) -> Result<()> {
    let changes_root = checked_absolute(&invocation.changes_root)?;
    verify_private_directory(&changes_root)?;
    let change_root = changes_root.join(invocation.change_id.simple().to_string());
    let cache_root = checked_absolute(&invocation.cache_root)?;
    let temporary_root = checked_absolute(&invocation.temporary_root)?;
    for (path, expected) in [
        (&change_root, invocation.change_identity),
        (&cache_root, invocation.cache_identity),
        (&temporary_root, invocation.temporary_identity),
    ] {
        let actual = exact_directory_identity(path)?;
        if actual != expected {
            return Err(RustVerifyError::SourceChanged);
        }
    }
    ensure_disjoint(&[change_root, cache_root, temporary_root])
}

/// Runs a hidden-worker invocation and always persists a bounded result when
/// the result path itself remains writable. Operational failures are encoded
/// in the result rather than left to interpretation of process stderr.
pub(crate) fn run_worker_file(invocation_path: &Path, result_path: &Path) -> Result<()> {
    if path_exists(result_path)? {
        let _ = read_worker_result(result_path)?;
        return Ok(());
    }
    let result = match read_worker_invocation(invocation_path).and_then(|value| run_worker(&value))
    {
        Ok(result) => result,
        Err(error) => WorkerResult {
            snapshot_digest: None,
            bundle_digest: None,
            success: false,
            diagnostic: bounded_diagnostic(error.to_string().as_bytes()),
            bundle_staging: None,
        },
    };
    write_worker_result(result_path, &result)
}

/// Re-verifies the exact invocation-owned staging bundle after the worker has
/// exited and before the completion check records its digests.
pub(crate) fn verify_exact_bundle(
    staging: &ExactTreeMeasurement,
    snapshot_digest: &str,
    bundle_digest: &str,
    cache_key: &str,
) -> Result<ExactTreeMeasurement> {
    if !valid_digest(snapshot_digest) || !valid_digest(bundle_digest) {
        return Err(RustVerifyError::InvalidBundle);
    }
    let measured = measure_exact_tree(&staging.path)?;
    if measured != *staging {
        return Err(RustVerifyError::SourceChanged);
    }
    let manifest = read_bundle_manifest(&staging.path)?;
    verify_bundle(
        &staging.path,
        &manifest,
        snapshot_digest,
        bundle_digest,
        cache_key,
    )?;
    if measure_exact_tree(&staging.path)? != *staging {
        return Err(RustVerifyError::SourceChanged);
    }
    Ok(measured)
}

/// Measures one exact tree without following links or double-counting hard
/// links. `allocated_bytes` is `st_blocks * 512`, including directories.
pub(crate) fn measure_exact_tree(path: &Path) -> Result<ExactTreeMeasurement> {
    let path = checked_absolute(path)?;
    let root = directory_identity(&path)?;
    let mut pending = vec![path.clone()];
    let mut seen = BTreeSet::new();
    let mut allocated_bytes = 0_u64;
    let mut entries = 0_u64;
    while let Some(directory) = pending.pop() {
        for child in fs::read_dir(&directory).map_err(|source| RustVerifyError::Io {
            path: directory.clone(),
            source,
        })? {
            let child = child.map_err(|source| RustVerifyError::Io {
                path: directory.clone(),
                source,
            })?;
            let child_path = child.path();
            let metadata =
                fs::symlink_metadata(&child_path).map_err(|source| RustVerifyError::Io {
                    path: child_path.clone(),
                    source,
                })?;
            if metadata.dev() != root.device || metadata.file_type().is_symlink() {
                return Err(RustVerifyError::UnsafeSourceEntry(child_path));
            }
            entries = entries
                .checked_add(1)
                .ok_or(RustVerifyError::SourceTooLarge)?;
            if seen.insert((metadata.dev(), metadata.ino())) {
                allocated_bytes = allocated_bytes
                    .checked_add(
                        metadata
                            .blocks()
                            .checked_mul(512)
                            .ok_or(RustVerifyError::SourceTooLarge)?,
                    )
                    .ok_or(RustVerifyError::SourceTooLarge)?;
            }
            if metadata.is_dir() {
                pending.push(child_path);
            } else if !metadata.is_file() {
                return Err(RustVerifyError::UnsafeSourceEntry(child_path));
            }
            if entries > MAX_SOURCE_ENTRIES {
                return Err(RustVerifyError::SourceTooLarge);
            }
        }
    }
    let root_metadata = fs::symlink_metadata(&path).map_err(|source| RustVerifyError::Io {
        path: path.clone(),
        source,
    })?;
    allocated_bytes = allocated_bytes
        .checked_add(
            root_metadata
                .blocks()
                .checked_mul(512)
                .ok_or(RustVerifyError::SourceTooLarge)?,
        )
        .ok_or(RustVerifyError::SourceTooLarge)?;
    verify_directory_identity(&path, root)?;
    Ok(ExactTreeMeasurement {
        path,
        device: root.device,
        inode: root.inode,
        allocated_bytes,
    })
}

/// Reads the exact direct directory identity without following a final link.
pub(crate) fn exact_directory_identity(path: &Path) -> Result<ExactDirectoryIdentity> {
    let path = checked_absolute(path)?;
    verify_private_directory(&path)?;
    let identity = directory_identity(&path)?;
    Ok(ExactDirectoryIdentity {
        device: identity.device,
        inode: identity.inode,
    })
}

/// Quarantines and removes only an exact daemon-recorded sibling tree, then
/// fsyncs its private parent. Recovery resumes an exact prior quarantine;
/// replacement or ambiguous paths fail closed.
pub(crate) fn remove_exact_tree(
    path: &Path,
    expected_device: u64,
    expected_inode: u64,
    quarantine_name: &str,
) -> Result<()> {
    let path = checked_absolute(path)?;
    if !safe_name(quarantine_name) {
        return Err(RustVerifyError::UnsafePath(PathBuf::from(quarantine_name)));
    }
    let parent = path
        .parent()
        .ok_or_else(|| RustVerifyError::UnsafePath(path.clone()))?;
    verify_private_directory(parent)?;
    let quarantine = parent.join(quarantine_name);
    let original_exists = path_exists(&path)?;
    let quarantine_exists = path_exists(&quarantine)?;
    match (original_exists, quarantine_exists) {
        (false, false) => return sync_directory(parent),
        (true, true) => return Err(RustVerifyError::UnsafePath(quarantine)),
        (true, false) => {
            let current = directory_identity(&path)?;
            if current.device != expected_device || current.inode != expected_inode {
                return Err(RustVerifyError::SourceChanged);
            }
            fs::rename(&path, &quarantine).map_err(|source| RustVerifyError::Io {
                path: quarantine.clone(),
                source,
            })?;
            sync_directory(parent)?;
        }
        (false, true) => {}
    }
    let quarantined = directory_identity(&quarantine)?;
    if quarantined.device != expected_device || quarantined.inode != expected_inode {
        return Err(RustVerifyError::SourceChanged);
    }
    let _ = measure_exact_tree(&quarantine)?;
    fs::remove_dir_all(&quarantine).map_err(|source| RustVerifyError::Io {
        path: quarantine,
        source,
    })?;
    sync_directory(parent)
}

fn bounded_diagnostic(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_DIAGNOSTIC_BYTES)]).into_owned()
}

impl TrustedExecutable {
    fn verify(&self) -> Result<()> {
        let current = file_identity(&self.path)
            .map_err(|_| RustVerifyError::InvalidCargo(self.path.clone()))?;
        let digest =
            hash_path(&self.path).map_err(|_| RustVerifyError::InvalidCargo(self.path.clone()))?;
        if current == self.identity && current.mode & 0o111 != 0 && digest == self.digest {
            Ok(())
        } else {
            Err(RustVerifyError::InvalidCargo(self.path.clone()))
        }
    }
}

impl TreeManifest {
    fn digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"dark-factory-source-snapshot-v1\0");
        hasher.update(self.root_mode.to_be_bytes());
        for entry in &self.entries {
            let path = entry.path.as_os_str().as_bytes();
            hasher.update((path.len() as u64).to_be_bytes());
            hasher.update(path);
            hasher.update([match entry.kind {
                EntryKind::Directory => 0,
                EntryKind::File => 1,
            }]);
            hasher.update(entry.mode.to_be_bytes());
            hasher.update(entry.size.to_be_bytes());
            if let Some(digest) = &entry.digest {
                hasher.update(digest.as_bytes());
            }
        }
        hex_digest(hasher.finalize())
    }
}

fn trusted_executable(path: &Path) -> Result<TrustedExecutable> {
    let path = checked_absolute(path)?;
    let metadata =
        fs::symlink_metadata(&path).map_err(|_| RustVerifyError::InvalidCargo(path.clone()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.permissions().mode() & 0o111 == 0
    {
        return Err(RustVerifyError::InvalidCargo(path));
    }
    let identity = identity(&metadata);
    let digest = hash_path(&path).map_err(|_| RustVerifyError::InvalidCargo(path.clone()))?;
    Ok(TrustedExecutable {
        path,
        identity,
        digest,
    })
}

fn trusted_toolchain(cargo: &Path) -> Result<(TrustedExecutable, TrustedExecutable)> {
    let cargo = trusted_executable(cargo)?;
    let rustc_path = cargo
        .path
        .parent()
        .ok_or_else(|| RustVerifyError::InvalidCargo(cargo.path.clone()))?
        .join("rustc");
    let rustc = trusted_executable(&rustc_path)?;
    Ok((cargo, rustc))
}

/// Stable mutable-cache namespace. It includes project incarnation, the fixed
/// operation policy, and exact trusted Cargo identity/content, but never source
/// revision.
pub(crate) fn cache_key(project_incarnation_id: &str, cargo: &Path) -> Result<String> {
    let (cargo, rustc) = trusted_toolchain(cargo)?;
    cache_key_from_trusted(project_incarnation_id, &cargo, &rustc)
}

fn cache_key_from_trusted(
    project_incarnation_id: &str,
    cargo: &TrustedExecutable,
    rustc: &TrustedExecutable,
) -> Result<String> {
    if project_incarnation_id.is_empty()
        || project_incarnation_id.len() > 255
        || project_incarnation_id.bytes().any(|byte| byte == 0)
    {
        return Err(RustVerifyError::UnsafePath(PathBuf::from(
            project_incarnation_id,
        )));
    }
    let mut hasher = Sha256::new();
    hasher.update(CACHE_POLICY.as_bytes());
    hasher.update([0]);
    hasher.update(project_incarnation_id.as_bytes());
    hasher.update([0]);
    hasher.update(cargo.path.as_os_str().as_bytes());
    hasher.update(cargo.identity.device.to_be_bytes());
    hasher.update(cargo.identity.inode.to_be_bytes());
    hasher.update(cargo.identity.size.to_be_bytes());
    hasher.update(cargo.identity.mode.to_be_bytes());
    hasher.update(cargo.digest.as_bytes());
    hasher.update([0]);
    hasher.update(rustc.path.as_os_str().as_bytes());
    hasher.update(rustc.identity.device.to_be_bytes());
    hasher.update(rustc.identity.inode.to_be_bytes());
    hasher.update(rustc.identity.size.to_be_bytes());
    hasher.update(rustc.identity.mode.to_be_bytes());
    hasher.update(rustc.digest.as_bytes());
    for argument in BUILD_ARGUMENTS {
        hasher.update([0]);
        hasher.update(argument.as_bytes());
    }
    Ok(hex_digest(hasher.finalize()))
}

fn checked_absolute(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute()
        || path.as_os_str().as_bytes().len() > MAX_PATH_BYTES
        || path.components().any(|component| {
            matches!(
                component,
                Component::CurDir | Component::ParentDir | Component::Prefix(_)
            )
        })
    {
        return Err(RustVerifyError::UnsafePath(path.to_owned()));
    }
    Ok(path.to_owned())
}

fn prepare_private_root(path: &Path) -> Result<PathBuf> {
    let path = checked_absolute(path)?;
    match fs::symlink_metadata(&path) {
        Ok(_) => verify_private_directory(&path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => create_private_directory(&path)?,
        Err(source) => {
            return Err(RustVerifyError::Io {
                path: path.clone(),
                source,
            });
        }
    }
    Ok(path)
}

fn create_private_directory(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.mode(PRIVATE_DIRECTORY_MODE);
    builder.create(path).map_err(|source| RustVerifyError::Io {
        path: path.to_owned(),
        source,
    })?;
    verify_private_directory(path)
}

fn verify_private_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| RustVerifyError::UnsafePrivateDirectory(path.to_owned()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o777 != PRIVATE_DIRECTORY_MODE
    {
        return Err(RustVerifyError::UnsafePrivateDirectory(path.to_owned()));
    }
    Ok(())
}

fn ensure_disjoint(roots: &[PathBuf]) -> Result<()> {
    for (index, left) in roots.iter().enumerate() {
        for right in roots.iter().skip(index + 1) {
            if left.starts_with(right) || right.starts_with(left) {
                return Err(RustVerifyError::OverlappingRoots(
                    left.clone(),
                    right.clone(),
                ));
            }
        }
    }
    Ok(())
}

fn safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path.as_os_str().as_bytes().len() <= MAX_PATH_BYTES
        && path.components().all(|component| match component {
            Component::Normal(name) => !name.as_bytes().eq_ignore_ascii_case(b".git"),
            _ => false,
        })
}

fn capture_tree(root: &Path) -> Result<TreeManifest> {
    let root_metadata = fs::symlink_metadata(root).map_err(|source| RustVerifyError::Io {
        path: root.to_owned(),
        source,
    })?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(RustVerifyError::UnsafeSourceEntry(root.to_owned()));
    }
    let root_device = root_metadata.dev();
    let mut pending = vec![root.to_owned()];
    let mut entries = Vec::new();
    let mut bytes = 0_u64;
    while let Some(directory) = pending.pop() {
        let children = fs::read_dir(&directory).map_err(|source| RustVerifyError::Io {
            path: directory.clone(),
            source,
        })?;
        for child in children {
            let path = child
                .map_err(|source| RustVerifyError::Io {
                    path: directory.clone(),
                    source,
                })?
                .path();
            let relative = path
                .strip_prefix(root)
                .map_err(|_| RustVerifyError::UnsafeSourceEntry(path.clone()))?
                .to_owned();
            if !safe_relative(&relative) {
                return Err(RustVerifyError::UnsafeSourceEntry(path));
            }
            let metadata = fs::symlink_metadata(&path).map_err(|source| RustVerifyError::Io {
                path: path.clone(),
                source,
            })?;
            if metadata.dev() != root_device || metadata.mode() & 0o7000 != 0 {
                return Err(RustVerifyError::UnsafeSourceEntry(path));
            }
            let mode = metadata.permissions().mode() & 0o777;
            let entry = if metadata.is_dir() && !metadata.file_type().is_symlink() {
                pending.push(path);
                TreeEntry {
                    path: relative,
                    kind: EntryKind::Directory,
                    mode,
                    size: 0,
                    digest: None,
                }
            } else if metadata.is_file() && !metadata.file_type().is_symlink() {
                bytes = bytes
                    .checked_add(metadata.len())
                    .ok_or(RustVerifyError::SourceTooLarge)?;
                let (digest, opened) = hash_stable_path(&path)?;
                if opened != identity(&metadata) {
                    return Err(RustVerifyError::SourceChanged);
                }
                TreeEntry {
                    path: relative,
                    kind: EntryKind::File,
                    mode,
                    size: metadata.len(),
                    digest: Some(digest),
                }
            } else {
                return Err(RustVerifyError::UnsafeSourceEntry(path));
            };
            entries.push(entry);
            if entries.len() as u64 > MAX_SOURCE_ENTRIES || bytes > MAX_SOURCE_BYTES {
                return Err(RustVerifyError::SourceTooLarge);
            }
        }
    }
    entries.sort_by(|left, right| {
        left.path
            .as_os_str()
            .as_bytes()
            .cmp(right.path.as_os_str().as_bytes())
    });
    Ok(TreeManifest {
        root_mode: root_metadata.permissions().mode() & 0o777,
        entries,
    })
}

fn copy_tree(source: &Path, destination: &Path, manifest: &TreeManifest) -> Result<()> {
    let mut directories = manifest
        .entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Directory)
        .collect::<Vec<_>>();
    directories.sort_by_key(|entry| entry.path.components().count());
    for entry in &directories {
        let path = destination.join(&entry.path);
        create_private_directory(&path)?;
    }
    for entry in manifest
        .entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::File)
    {
        copy_manifest_file(source, destination, entry)?;
    }
    directories.sort_by_key(|entry| std::cmp::Reverse(entry.path.components().count()));
    for entry in directories {
        fs::set_permissions(
            destination.join(&entry.path),
            fs::Permissions::from_mode(entry.mode),
        )
        .map_err(|source| RustVerifyError::Io {
            path: destination.join(&entry.path),
            source,
        })?;
    }
    fs::set_permissions(destination, fs::Permissions::from_mode(manifest.root_mode)).map_err(
        |source| RustVerifyError::Io {
            path: destination.to_owned(),
            source,
        },
    )?;
    Ok(())
}

fn copy_manifest_file(source: &Path, destination: &Path, entry: &TreeEntry) -> Result<()> {
    let source_path = source.join(&entry.path);
    let destination_path = destination.join(&entry.path);
    let mut input =
        open_regular_nofollow(&source_path).map_err(|_| RustVerifyError::SourceChanged)?;
    let opened_before = file_identity_from(&input).map_err(|source| RustVerifyError::Io {
        path: source_path.clone(),
        source,
    })?;
    if opened_before.size != entry.size || opened_before.mode != entry.mode {
        return Err(RustVerifyError::SourceChanged);
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(entry.mode)
        .open(&destination_path)
        .map_err(|source| RustVerifyError::Io {
            path: destination_path.clone(),
            source,
        })?;
    let copied = io::copy(&mut input, &mut output).map_err(|source| RustVerifyError::Io {
        path: destination_path.clone(),
        source,
    })?;
    output.sync_all().map_err(|source| RustVerifyError::Io {
        path: destination_path.clone(),
        source,
    })?;
    fs::set_permissions(&destination_path, fs::Permissions::from_mode(entry.mode)).map_err(
        |source| RustVerifyError::Io {
            path: destination_path.clone(),
            source,
        },
    )?;
    let opened_after = file_identity_from(&input).map_err(|source| RustVerifyError::Io {
        path: source_path.clone(),
        source,
    })?;
    let path_after = file_identity(&source_path).map_err(|_| RustVerifyError::SourceChanged)?;
    if copied != entry.size || opened_before != opened_after || opened_before != path_after {
        return Err(RustVerifyError::SourceChanged);
    }
    Ok(())
}

fn parse_artifacts(stdout: &[u8], cache_target: &Path) -> Result<Vec<PathBuf>> {
    let canonical_cache = fs::canonicalize(cache_target).map_err(|source| RustVerifyError::Io {
        path: cache_target.to_owned(),
        source,
    })?;
    let mut artifacts = BTreeSet::new();
    for line in stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let message: CargoMessage =
            serde_json::from_slice(line).map_err(|_| RustVerifyError::InvalidCargoOutput)?;
        if message.reason != "compiler-artifact"
            || !message.profile.is_some_and(|profile| profile.test)
        {
            continue;
        }
        let Some(path) = message.executable else {
            continue;
        };
        let absolute = if path.is_absolute() {
            path
        } else {
            return Err(RustVerifyError::ArtifactOutsideCache(path));
        };
        let canonical = fs::canonicalize(&absolute)
            .map_err(|_| RustVerifyError::InvalidArtifact(absolute.clone()))?;
        if !canonical.starts_with(&canonical_cache) {
            return Err(RustVerifyError::ArtifactOutsideCache(canonical));
        }
        let metadata = fs::symlink_metadata(&absolute)
            .map_err(|_| RustVerifyError::InvalidArtifact(absolute.clone()))?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.permissions().mode() & 0o111 == 0
        {
            return Err(RustVerifyError::InvalidArtifact(absolute));
        }
        artifacts.insert(absolute);
        if artifacts.len() > MAX_TEST_ARTIFACTS {
            return Err(RustVerifyError::InvalidCargoOutput);
        }
    }
    if artifacts.is_empty() {
        return Err(RustVerifyError::NoTestArtifacts);
    }
    Ok(artifacts.into_iter().collect())
}

fn copy_artifacts(artifacts: &[PathBuf], bin: &Path) -> Result<Vec<BundleExecutable>> {
    let mut executables = Vec::with_capacity(artifacts.len());
    for (index, artifact) in artifacts.iter().enumerate() {
        let mut input = open_regular_nofollow(artifact)
            .map_err(|_| RustVerifyError::InvalidArtifact(artifact.clone()))?;
        let before = file_identity_from(&input).map_err(|source| RustVerifyError::Io {
            path: artifact.clone(),
            source,
        })?;
        if before.mode & 0o111 == 0 {
            return Err(RustVerifyError::InvalidArtifact(artifact.clone()));
        }
        let digest_before = hash_open_file(&mut input)?;
        input.rewind().map_err(|source| RustVerifyError::Io {
            path: artifact.clone(),
            source,
        })?;
        let name = format!("test-{index:04}");
        let destination = bin.join(&name);
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(BUNDLE_EXECUTABLE_MODE)
            .open(&destination)
            .map_err(|source| RustVerifyError::Io {
                path: destination.clone(),
                source,
            })?;
        let copied = io::copy(&mut input, &mut output).map_err(|source| RustVerifyError::Io {
            path: destination.clone(),
            source,
        })?;
        output.sync_all().map_err(|source| RustVerifyError::Io {
            path: destination.clone(),
            source,
        })?;
        fs::set_permissions(
            &destination,
            fs::Permissions::from_mode(BUNDLE_EXECUTABLE_MODE),
        )
        .map_err(|source| RustVerifyError::Io {
            path: destination.clone(),
            source,
        })?;
        let after = file_identity_from(&input).map_err(|source| RustVerifyError::Io {
            path: artifact.clone(),
            source,
        })?;
        let current = file_identity(artifact)
            .map_err(|_| RustVerifyError::InvalidArtifact(artifact.clone()))?;
        input.rewind().map_err(|source| RustVerifyError::Io {
            path: artifact.clone(),
            source,
        })?;
        let digest_after = hash_open_file(&mut input)?;
        if before != after
            || before != current
            || copied != before.size
            || digest_before != digest_after
        {
            return Err(RustVerifyError::InvalidArtifact(artifact.clone()));
        }
        let mut published =
            open_regular_nofollow(&destination).map_err(|source| RustVerifyError::Io {
                path: destination.clone(),
                source,
            })?;
        let published_identity =
            file_identity_from(&published).map_err(|source| RustVerifyError::Io {
                path: destination.clone(),
                source,
            })?;
        let published_digest = hash_open_file(&mut published)?;
        if published_digest != digest_before || published_identity.size != before.size {
            return Err(RustVerifyError::InvalidArtifact(artifact.clone()));
        }
        executables.push(BundleExecutable {
            name,
            digest: digest_before,
            size: before.size,
            mode: BUNDLE_EXECUTABLE_MODE,
            device: published_identity.device,
            inode: published_identity.inode,
        });
    }
    Ok(executables)
}

fn bundle_digest(manifest: &BundleManifest) -> Result<String> {
    let mut content = manifest.clone();
    for executable in &mut content.executables {
        executable.device = 0;
        executable.inode = 0;
    }
    let bytes = serde_json::to_vec(&content)?;
    Ok(hex_digest(Sha256::digest(bytes)))
}

fn read_bundle_manifest(root: &Path) -> Result<BundleManifest> {
    read_bounded_json(&root.join("manifest.json"), MAX_BUNDLE_MANIFEST_BYTES)
        .map_err(|_| RustVerifyError::InvalidBundle)
}

fn verify_bundle(
    root: &Path,
    manifest: &BundleManifest,
    snapshot_digest: &str,
    expected_bundle_digest: &str,
    expected_cache_key: &str,
) -> Result<()> {
    if manifest.version != MANIFEST_VERSION
        || manifest.snapshot_digest != snapshot_digest
        || manifest.cache_key != expected_cache_key
        || manifest.build_arguments.len() != BUILD_ARGUMENTS.len()
        || !manifest
            .build_arguments
            .iter()
            .zip(BUILD_ARGUMENTS)
            .all(|(actual, expected)| actual == expected)
        || manifest.executables.is_empty()
        || manifest.executables.len() > MAX_TEST_ARTIFACTS
        || bundle_digest(manifest)? != expected_bundle_digest
    {
        return Err(RustVerifyError::InvalidBundle);
    }
    verify_private_directory(root)?;
    verify_private_directory(&root.join("bin"))?;
    let entries = fs::read_dir(root)
        .map_err(|source| RustVerifyError::Io {
            path: root.to_owned(),
            source,
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|source| RustVerifyError::Io {
            path: root.to_owned(),
            source,
        })?;
    if entries.len() != 2 {
        return Err(RustVerifyError::InvalidBundle);
    }
    let bin_entries = fs::read_dir(root.join("bin"))
        .map_err(|source| RustVerifyError::Io {
            path: root.join("bin"),
            source,
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|source| RustVerifyError::Io {
            path: root.join("bin"),
            source,
        })?;
    if bin_entries.len() != manifest.executables.len() {
        return Err(RustVerifyError::InvalidBundle);
    }
    for executable in &manifest.executables {
        let path = root.join("bin").join(&executable.name);
        let mut file = open_regular_nofollow(&path).map_err(|_| RustVerifyError::InvalidBundle)?;
        verify_opened_executable(&path, &mut file, executable)
            .map_err(|_| RustVerifyError::InvalidBundle)?;
    }
    Ok(())
}

fn verify_opened_executable(
    path: &Path,
    file: &mut File,
    expected: &BundleExecutable,
) -> Result<()> {
    let opened = file_identity_from(file).map_err(|source| RustVerifyError::Io {
        path: path.to_owned(),
        source,
    })?;
    let current =
        file_identity(path).map_err(|_| RustVerifyError::ExecutableTampered(path.to_owned()))?;
    if opened.device != expected.device
        || opened.inode != expected.inode
        || opened.size != expected.size
        || opened.mode != expected.mode
        || current != opened
    {
        return Err(RustVerifyError::ExecutableTampered(path.to_owned()));
    }
    let digest = hash_open_file(file)?;
    file.rewind().map_err(|source| RustVerifyError::Io {
        path: path.to_owned(),
        source,
    })?;
    if digest != expected.digest {
        return Err(RustVerifyError::ExecutableTampered(path.to_owned()));
    }
    Ok(())
}

fn verify_checkpointed_directory(path: &Path, checkpoint: &BundleCheckpoint) -> Result<()> {
    let expected = match (checkpoint.staging_device, checkpoint.staging_inode) {
        (Some(device), Some(inode)) => (device, inode),
        _ => return Err(RustVerifyError::InvalidCheckpoint),
    };
    verify_private_directory(path)?;
    let identity = directory_identity(path)?;
    if (identity.device, identity.inode) == expected {
        Ok(())
    } else {
        Err(RustVerifyError::InvalidCheckpoint)
    }
}

fn write_create_only_json<T: Serialize>(path: &Path, value: &T, maximum: usize) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    if bytes.len() > maximum {
        return Err(RustVerifyError::InvalidBundle);
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PRIVATE_FILE_MODE)
        .open(path)
        .map_err(|source| RustVerifyError::Io {
            path: path.to_owned(),
            source,
        })?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|source| RustVerifyError::Io {
            path: path.to_owned(),
            source,
        })?;
    Ok(())
}

fn write_atomic_json<T: Serialize>(
    path: &Path,
    value: &T,
    maximum: usize,
    parent: &Path,
) -> Result<()> {
    verify_private_directory(parent)?;
    if path.parent() != Some(parent) {
        return Err(RustVerifyError::UnsafePath(path.to_owned()));
    }
    let temporary = parent.join(format!(".checkpoint-{}.tmp", Uuid::new_v4().simple()));
    write_create_only_json(&temporary, value, maximum)?;
    fs::rename(&temporary, path).map_err(|source| RustVerifyError::Io {
        path: path.to_owned(),
        source,
    })?;
    sync_directory(parent)
}

fn read_bounded_json<T: for<'de> Deserialize<'de>>(path: &Path, maximum: usize) -> Result<T> {
    let metadata = fs::symlink_metadata(path).map_err(|source| RustVerifyError::Io {
        path: path.to_owned(),
        source,
    })?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o777 != PRIVATE_FILE_MODE
        || metadata.len() > maximum as u64
    {
        return Err(RustVerifyError::InvalidCheckpoint);
    }
    let mut file = open_regular_nofollow(path).map_err(|source| RustVerifyError::Io {
        path: path.to_owned(),
        source,
    })?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((maximum as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| RustVerifyError::Io {
            path: path.to_owned(),
            source,
        })?;
    if bytes.len() > maximum {
        return Err(RustVerifyError::InvalidCheckpoint);
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn remove_exact_file(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| RustVerifyError::UnsafePath(path.to_owned()))?;
    let _ = read_bounded_json::<BundleCheckpoint>(path, MAX_CHECKPOINT_BYTES)?;
    fs::remove_file(path).map_err(|source| RustVerifyError::Io {
        path: path.to_owned(),
        source,
    })?;
    sync_directory(parent)
}

fn run_bounded(
    mut command: Command,
    stdout_limit: usize,
    stderr_limit: usize,
) -> Result<BoundedOutput> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|source| RustVerifyError::Io {
        path: PathBuf::from("process spawn"),
        source,
    })?;
    let stdout = child.stdout.take().ok_or_else(|| RustVerifyError::Io {
        path: PathBuf::from("process stdout"),
        source: io::Error::other("stdout pipe is absent"),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| RustVerifyError::Io {
        path: PathBuf::from("process stderr"),
        source: io::Error::other("stderr pipe is absent"),
    })?;
    let stdout_reader = thread::spawn(move || read_bounded(stdout, stdout_limit));
    let stderr_reader = thread::spawn(move || read_bounded(stderr, stderr_limit));
    let status = child.wait().map_err(|source| RustVerifyError::Io {
        path: PathBuf::from("process wait"),
        source,
    })?;
    let (stdout, stdout_exceeded) = stdout_reader
        .join()
        .map_err(|_| RustVerifyError::OutputReaderPanicked)?
        .map_err(|source| RustVerifyError::Io {
            path: PathBuf::from("process stdout"),
            source,
        })?;
    let (stderr, stderr_exceeded) = stderr_reader
        .join()
        .map_err(|_| RustVerifyError::OutputReaderPanicked)?
        .map_err(|source| RustVerifyError::Io {
            path: PathBuf::from("process stderr"),
            source,
        })?;
    Ok(BoundedOutput {
        status,
        stdout,
        stderr,
        stdout_exceeded,
        stderr_exceeded,
    })
}

fn read_bounded(mut reader: impl Read, limit: usize) -> io::Result<(Vec<u8>, bool)> {
    let mut retained = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut exceeded = false;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok((retained, exceeded));
        }
        let available = limit.saturating_sub(retained.len());
        retained.extend_from_slice(&buffer[..read.min(available)]);
        exceeded |= read > available;
    }
}

fn open_regular_nofollow(path: &Path) -> io::Result<File> {
    let fd = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
    let file: File = fd.into();
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    Ok(file)
}

fn file_identity(path: &Path) -> io::Result<FileIdentity> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::Error::other("not a direct regular file"));
    }
    Ok(identity(&metadata))
}

fn file_identity_from(file: &File) -> io::Result<FileIdentity> {
    file.metadata().map(|metadata| identity(&metadata))
}

fn identity(metadata: &fs::Metadata) -> FileIdentity {
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        mode: metadata.permissions().mode() & 0o777,
    }
}

fn directory_identity(path: &Path) -> Result<FileIdentity> {
    let metadata = fs::symlink_metadata(path).map_err(|source| RustVerifyError::Io {
        path: path.to_owned(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(RustVerifyError::UnsafePrivateDirectory(path.to_owned()));
    }
    Ok(identity(&metadata))
}

fn verify_directory_identity(path: &Path, expected: FileIdentity) -> Result<()> {
    verify_private_directory(path)?;
    if directory_identity(path)? == expected {
        Ok(())
    } else {
        Err(RustVerifyError::SourceChanged)
    }
}

fn verify_bound_directory(path: &Path, expected: FileIdentity) -> Result<()> {
    verify_private_directory(path)?;
    let actual = directory_identity(path)?;
    if actual.device == expected.device && actual.inode == expected.inode {
        Ok(())
    } else {
        Err(RustVerifyError::SourceChanged)
    }
}

fn hash_stable_path(path: &Path) -> Result<(String, FileIdentity)> {
    let mut file = open_regular_nofollow(path).map_err(|source| RustVerifyError::Io {
        path: path.to_owned(),
        source,
    })?;
    let before = file_identity_from(&file).map_err(|source| RustVerifyError::Io {
        path: path.to_owned(),
        source,
    })?;
    let digest = hash_open_file(&mut file)?;
    let after = file_identity_from(&file).map_err(|source| RustVerifyError::Io {
        path: path.to_owned(),
        source,
    })?;
    let current = file_identity(path).map_err(|source| RustVerifyError::Io {
        path: path.to_owned(),
        source,
    })?;
    if before != after || before != current {
        return Err(RustVerifyError::SourceChanged);
    }
    Ok((digest, before))
}

fn hash_path(path: &Path) -> Result<String> {
    hash_stable_path(path).map(|(digest, _)| digest)
}

fn hash_open_file(file: &mut File) -> Result<String> {
    file.rewind().map_err(|source| RustVerifyError::Io {
        path: PathBuf::from("open file"),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| RustVerifyError::Io {
                path: PathBuf::from("open file"),
                source,
            })?;
        if read == 0 {
            return Ok(hex_digest(hasher.finalize()));
        }
        hasher.update(&buffer[..read]);
    }
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn safe_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        && value != "."
        && value != ".."
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(RustVerifyError::UnsafePath(path.to_owned())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(RustVerifyError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

fn fixed_path(cargo: &Path) -> OsString {
    let mut paths = Vec::new();
    if let Some(parent) = cargo.parent() {
        paths.push(parent.to_owned());
    }
    paths.extend([
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
        PathBuf::from("/usr/sbin"),
        PathBuf::from("/sbin"),
    ]);
    std::env::join_paths(paths).unwrap_or_else(|_| OsString::from("/usr/bin:/bin"))
}

fn sync_tree(root: &Path) -> Result<()> {
    let mut pending = vec![root.to_owned()];
    let mut directories = Vec::new();
    while let Some(directory) = pending.pop() {
        for child in fs::read_dir(&directory).map_err(|source| RustVerifyError::Io {
            path: directory.clone(),
            source,
        })? {
            let path = child
                .map_err(|source| RustVerifyError::Io {
                    path: directory.clone(),
                    source,
                })?
                .path();
            let metadata = fs::symlink_metadata(&path).map_err(|source| RustVerifyError::Io {
                path: path.clone(),
                source,
            })?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                pending.push(path);
            } else if metadata.is_file() && !metadata.file_type().is_symlink() {
                File::open(&path)
                    .and_then(|file| file.sync_all())
                    .map_err(|source| RustVerifyError::Io { path, source })?;
            } else {
                return Err(RustVerifyError::UnsafePath(path));
            }
        }
        directories.push(directory);
    }
    for directory in directories.into_iter().rev() {
        sync_directory(&directory)?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| RustVerifyError::Io {
            path: path.to_owned(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _temporary: tempfile::TempDir,
        cargo: PathBuf,
        changes: PathBuf,
        change_id: Uuid,
        cache: PathBuf,
        worker_temporary: PathBuf,
        change: PathBuf,
        artifact: PathBuf,
        invocation_log: PathBuf,
        run_marker: PathBuf,
    }

    impl Fixture {
        fn new(mutate_live_source: bool) -> Self {
            let temporary = tempfile::Builder::new()
                .prefix("df-rust-verify-")
                .tempdir()
                .unwrap();
            let root = temporary.path();
            let cargo = root.join("cargo");
            let changes = root.join("changes");
            let change_id = Uuid::new_v4();
            let change = changes.join(change_id.simple().to_string());
            let cache_parent = root.join("caches");
            let worker_temporary = root.join("worker-temporary");
            let invocation_log = root.join("invocations.log");
            let run_marker = root.join("ran.txt");
            for directory in [&changes, &change, &cache_parent, &worker_temporary] {
                private_directory(directory);
            }
            fs::write(change.join("Cargo.toml"), "[workspace]\n").unwrap();
            fs::write(change.join("Cargo.lock"), "# exact lock\n").unwrap();
            fs::write(change.join("revision.txt"), "A\n").unwrap();

            let mutation = if mutate_live_source {
                format!(
                    "printf 'B\\n' > {}\n",
                    shell_literal(&change.join("revision.txt"))
                )
            } else {
                String::new()
            };
            let script = format!(
                "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$CARGO_TARGET_DIR\" >> {log}\nmkdir -p \"$CARGO_TARGET_DIR/debug/deps\"\nrevision=$(sed -n '1p' \"$PWD/revision.txt\")\nartifact=\"$CARGO_TARGET_DIR/debug/deps/fake-test\"\nprintf '#!/bin/sh\\nprintf %s > {marker}\\n' \"$revision\" > \"$artifact\"\nchmod 755 \"$artifact\"\n{mutation}printf '{{\"reason\":\"compiler-artifact\",\"profile\":{{\"test\":true}},\"executable\":\"%s\"}}\\n' \"$artifact\"\n",
                log = shell_literal(&invocation_log),
                marker = shell_literal(&run_marker),
            );
            fs::write(&cargo, script).unwrap();
            fs::set_permissions(&cargo, fs::Permissions::from_mode(0o700)).unwrap();
            let rustc = root.join("rustc");
            fs::write(&rustc, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&rustc, fs::Permissions::from_mode(0o700)).unwrap();
            let key = cache_key("project-incarnation-1", &cargo).unwrap();
            let cache = cache_parent.join(key);
            private_directory(&cache);
            let artifact = cache.join("workspace-test-target/debug/deps/fake-test");
            Self {
                _temporary: temporary,
                cargo,
                changes,
                change_id,
                cache,
                worker_temporary,
                change,
                artifact,
                invocation_log,
                run_marker,
            }
        }

        fn operation(&self) -> RustWorkspaceTest {
            RustWorkspaceTest::new(
                &self.cargo,
                &self.changes,
                self.change_id,
                "project-incarnation-1",
                &self.cache,
                &self.worker_temporary,
            )
            .unwrap()
        }
    }

    fn private_directory(path: &Path) {
        let mut builder = fs::DirBuilder::new();
        builder.mode(PRIVATE_DIRECTORY_MODE);
        builder.create(path).unwrap();
    }

    fn shell_literal(path: &Path) -> String {
        format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
    }

    #[test]
    fn live_source_mutation_during_build_cannot_change_selected_snapshot() {
        let fixture = Fixture::new(true);
        let operation = fixture.operation();
        let prepared = operation.prepare().unwrap();
        assert_eq!(
            fs::read_to_string(fixture.change.join("revision.txt")).unwrap(),
            "B\n"
        );
        assert_eq!(
            fs::read_to_string(prepared.snapshot_root().join("revision.txt")).unwrap(),
            "A\n"
        );
        let result = operation.execute(&prepared).unwrap();
        assert!(result.success);
        assert_eq!(fs::read_to_string(&fixture.run_marker).unwrap(), "A");
    }

    #[test]
    fn two_source_revisions_reuse_one_cache_and_publish_distinct_bundles() {
        let fixture = Fixture::new(false);
        let operation = fixture.operation();
        let first = operation.prepare().unwrap();
        fs::write(fixture.change.join("revision.txt"), "B\n").unwrap();
        let second = operation.prepare().unwrap();
        assert_ne!(first.snapshot_digest(), second.snapshot_digest());
        assert_ne!(first.bundle_digest(), second.bundle_digest());
        let invocations = fs::read_to_string(&fixture.invocation_log).unwrap();
        let paths = invocations.lines().collect::<Vec<_>>();
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], paths[1]);
        assert_eq!(Path::new(paths[0]), operation.cache_target());
    }

    #[test]
    fn replacing_mutable_cargo_output_after_prepare_still_launches_bundle_a() {
        let fixture = Fixture::new(false);
        let operation = fixture.operation();
        let prepared = operation.prepare().unwrap();
        fs::write(
            &fixture.artifact,
            format!(
                "#!/bin/sh\nprintf B > {}\n",
                shell_literal(&fixture.run_marker)
            ),
        )
        .unwrap();
        fs::set_permissions(&fixture.artifact, fs::Permissions::from_mode(0o700)).unwrap();
        let result = operation.execute(&prepared).unwrap();
        assert!(result.success);
        assert_eq!(fs::read_to_string(&fixture.run_marker).unwrap(), "A");
    }

    #[test]
    fn tampered_bundle_fails_before_launch() {
        let fixture = Fixture::new(false);
        let operation = fixture.operation();
        let prepared = operation.prepare().unwrap();
        let executable = prepared.bundle_root().join("bin/test-0000");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        assert!(matches!(
            operation.execute(&prepared),
            Err(RustVerifyError::InvalidBundle | RustVerifyError::ExecutableTampered(_))
        ));
        assert!(!fixture.run_marker.exists());
    }

    #[test]
    fn ready_checkpoint_recovers_staging_after_restart_without_rebuilding() {
        let fixture = Fixture::new(false);
        let operation = fixture.operation();
        let prepared = operation.prepare().unwrap();
        let restarted = fixture.operation().prepare().unwrap();
        assert_eq!(restarted.bundle_digest(), prepared.bundle_digest());
        assert_eq!(restarted.bundle_root(), prepared.bundle_root());
        assert!(restarted.bundle_root().is_dir());
        assert_eq!(
            fs::read_to_string(&fixture.invocation_log)
                .unwrap()
                .lines()
                .count(),
            1
        );
    }

    #[test]
    fn exact_tree_reclamation_refuses_replacement_identity() {
        let fixture = Fixture::new(false);
        let operation = fixture.operation();
        let prepared = operation.prepare().unwrap();
        let measured = measure_exact_tree(prepared.bundle_root()).unwrap();
        assert!(measured.allocated_bytes > 0);
        assert!(
            remove_exact_tree(
                prepared.bundle_root(),
                measured.device,
                measured.inode.saturating_add(1),
                ".reaping-wrong"
            )
            .is_err()
        );
        remove_exact_tree(
            prepared.bundle_root(),
            measured.device,
            measured.inode,
            ".reaping-exact",
        )
        .unwrap();
        assert!(!prepared.bundle_root().exists());
    }

    #[test]
    fn exact_tree_reclamation_recovers_after_quarantine_rename() {
        let fixture = Fixture::new(false);
        let operation = fixture.operation();
        let prepared = operation.prepare().unwrap();
        let measured = measure_exact_tree(prepared.bundle_root()).unwrap();
        let quarantine_name = ".reaping-restart";
        let quarantine = prepared
            .bundle_root()
            .parent()
            .unwrap()
            .join(quarantine_name);
        fs::rename(prepared.bundle_root(), &quarantine).unwrap();
        sync_directory(prepared.bundle_root().parent().unwrap()).unwrap();

        remove_exact_tree(
            prepared.bundle_root(),
            measured.device,
            measured.inode,
            quarantine_name,
        )
        .unwrap();
        assert!(!quarantine.exists());
        remove_exact_tree(
            prepared.bundle_root(),
            measured.device,
            measured.inode,
            quarantine_name,
        )
        .unwrap();
    }

    #[test]
    fn exact_staging_bundle_is_reverified_before_completion() {
        let fixture = Fixture::new(false);
        let operation = fixture.operation();
        let prepared = operation.prepare().unwrap();
        let staging = measure_exact_tree(prepared.bundle_root()).unwrap();
        let verified = verify_exact_bundle(
            &staging,
            prepared.snapshot_digest(),
            prepared.bundle_digest(),
            &operation.cache_key,
        )
        .unwrap();
        assert_eq!(verified, staging);
        assert!(prepared.bundle_root().is_dir());

        fs::write(prepared.bundle_root().join("manifest.json"), b"{}").unwrap();
        assert!(
            verify_exact_bundle(
                &staging,
                prepared.snapshot_digest(),
                prepared.bundle_digest(),
                &operation.cache_key,
            )
            .is_err()
        );
    }

    #[test]
    fn worker_file_persists_bounded_failure_without_stdio_contract() {
        let fixture = Fixture::new(false);
        let invocation_path = fixture.worker_temporary.join("invocation.json");
        let result_path = fixture.worker_temporary.join("result.json");
        let mut change_identity = exact_directory_identity(&fixture.change).unwrap();
        change_identity.inode = change_identity.inode.saturating_add(1);
        let invocation = WorkerInvocation {
            cargo: fixture.cargo.clone(),
            changes_root: fixture.changes.clone(),
            change_id: fixture.change_id,
            project_incarnation_id: "project-incarnation-1".into(),
            cache_root: fixture.cache.clone(),
            temporary_root: fixture.worker_temporary.clone(),
            change_identity,
            cache_identity: exact_directory_identity(&fixture.cache).unwrap(),
            temporary_identity: exact_directory_identity(&fixture.worker_temporary).unwrap(),
        };
        write_worker_invocation(&invocation_path, &invocation).unwrap();
        run_worker_file(&invocation_path, &result_path).unwrap();
        let result = read_worker_result(&result_path).unwrap();
        assert!(!result.success);
        assert!(result.snapshot_digest.is_none());
        assert!(result.bundle_staging.is_none());
        assert!(!result.diagnostic.is_empty());
        assert!(result.diagnostic.len() <= MAX_DIAGNOSTIC_BYTES);
        assert!(!fixture.cache.join("workspace-test-target").exists());
    }
}
