//! Bounded, identity-safe materialization of daemon-owned Change sources.
//!
//! The caller must run the prepared Git commands inside an already registered
//! attempt process group. It reads exact blob objects from the selected commit,
//! writes them into a private staging tree, and publishes the resulting
//! `.git`-free directory with one atomic rename.

use std::{
    collections::HashSet,
    convert::Infallible,
    ffi::{OsStr, OsString},
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use thiserror::Error;
use uuid::Uuid;

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;
const MAX_PATH_BYTES: usize = 4096;
const MAX_RECORD_BYTES: usize = 8192;
const MAX_INVOCATION_BYTES: usize = 128 * 1024;
const MAX_MANIFEST_OUTPUT_BYTES: usize = 128 * 1024 * 1024;
const MAX_BLOB_HEADER_BYTES: usize = 256;
const STAT_BLOCK_BYTES: u64 = 512;

/// Hard limits checked while extracting and again before publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct SourceLimits {
    pub max_entries: u64,
    pub max_bytes: u64,
}

impl<'de> Deserialize<'de> for SourceLimits {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Fields {
            max_entries: u64,
            max_bytes: u64,
        }

        let fields = Fields::deserialize(deserializer)?;
        Self::new(fields.max_entries, fields.max_bytes).map_err(serde::de::Error::custom)
    }
}

impl SourceLimits {
    /// Creates non-zero source limits.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidLimits`] when either limit is zero.
    pub fn new(max_entries: u64, max_bytes: u64) -> Result<Self, Error> {
        if max_entries == 0 || max_bytes == 0 {
            return Err(Error::InvalidLimits);
        }
        Ok(Self {
            max_entries,
            max_bytes,
        })
    }
}

/// A canonical full Git object ID, never a branch, tag, or abbreviated ref.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct GitOid(String);

impl GitOid {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for GitOid {
    type Error = Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        parse_oid(value.as_bytes())
    }
}

impl<'de> Deserialize<'de> for GitOid {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        parse_oid(value.as_bytes()).map_err(serde::de::Error::custom)
    }
}

/// One closed, argument-vector Git invocation for the registered wrapper.
///
/// `command` clears the ambient environment. There is no shell string and no
/// repository URL or remote operation in either supported invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
struct GitCommand {
    program: PathBuf,
    arguments: Vec<OsString>,
    environment: Vec<(OsString, OsString)>,
}

impl GitCommand {
    #[must_use]
    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(&self.arguments)
            .env_clear()
            .envs(self.environment.iter().cloned());
        command
    }
}

/// Exact filesystem identity recorded before an external effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DirectoryIdentity {
    pub device: u64,
    pub inode: u64,
}

/// The first, read-only checkpoint before any blob or source write.
struct RepositorySelection {
    git_program: PathBuf,
    repository_root: PathBuf,
    repository_identity: DirectoryIdentity,
}

impl RepositorySelection {
    /// Fixes the canonical local repository root and its exact inode before
    /// preparing the HEAD-resolution command.
    ///
    /// # Errors
    ///
    /// Returns an error for an untrusted executable or non-directory root.
    fn open(git_program: &Path, repository_root: &Path) -> Result<Self, Error> {
        let git_program = exact_executable(git_program)?;
        let repository_root = fs::canonicalize(repository_root).map_err(|source| Error::Io {
            path: repository_root.to_owned(),
            source,
        })?;
        validate_canonical_absolute_path(&repository_root)?;
        let repository_identity = directory_identity(&repository_root)?;
        Ok(Self {
            git_program,
            repository_root,
            repository_identity,
        })
    }

    /// Prepares one command that emits the canonical root then the full HEAD
    /// commit. The caller must durably register the already-gated wrapper
    /// before running it.
    #[must_use]
    fn command(&self) -> GitCommand {
        git_command(
            self.git_program.clone(),
            vec![
                OsString::from("-C"),
                self.repository_root.clone().into_os_string(),
                OsString::from("rev-parse"),
                OsString::from("--show-toplevel"),
                OsString::from("--verify"),
                OsString::from("HEAD^{commit}"),
            ],
        )
    }

    fn partial_clone_command(&self) -> GitCommand {
        partial_clone_command(&self.git_program, &self.repository_root)
    }

    /// Accepts exactly the two lines from [`Self::command`] and rechecks the
    /// root identity before producing the durable selection record.
    ///
    /// # Errors
    ///
    /// Ref names, abbreviated IDs, extra output, a different root, and path
    /// replacement all fail closed.
    fn resolve(self, output: &[u8]) -> Result<SelectedRepository, Error> {
        let output = output
            .strip_suffix(b"\n")
            .ok_or(Error::InvalidSelectionOutput)?;
        let mut lines = output.split(|byte| *byte == b'\n');
        let root = lines.next().ok_or(Error::InvalidSelectionOutput)?;
        let oid = lines.next().ok_or(Error::InvalidSelectionOutput)?;
        if lines.next().is_some() || OsStr::from_bytes(root) != self.repository_root.as_os_str() {
            return Err(Error::InvalidSelectionOutput);
        }
        verify_directory_identity(&self.repository_root, self.repository_identity)?;
        Ok(SelectedRepository {
            git_program: self.git_program,
            base_oid: parse_oid(oid)?,
            repository_root: self.repository_root,
            repository_identity: self.repository_identity,
        })
    }
}

/// Durable selection persisted by the daemon before blob activation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SelectionRecord {
    pub base_oid: GitOid,
    pub repository_root: PathBuf,
    pub repository_device: u64,
    pub repository_inode: u64,
    pub staging_path: PathBuf,
    pub staging_device: u64,
    pub staging_inode: u64,
}

/// A persisted exact commit plus the repository inode from which it was read.
struct SelectedRepository {
    git_program: PathBuf,
    base_oid: GitOid,
    repository_root: PathBuf,
    repository_identity: DirectoryIdentity,
}

impl SelectedRepository {
    /// Reconstitutes the post-checkpoint helper and immediately verifies the
    /// exact repository identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the trusted Git executable or repository root is
    /// missing, replaced, or no longer usable.
    fn from_record(git_program: &Path, record: &SelectionRecord) -> Result<Self, Error> {
        validate_canonical_absolute_path(&record.repository_root)?;
        let selected = Self {
            git_program: exact_executable(git_program)?,
            base_oid: record.base_oid.clone(),
            repository_root: record.repository_root.clone(),
            repository_identity: DirectoryIdentity {
                device: record.repository_device,
                inode: record.repository_inode,
            },
        };
        selected.verify_identity()?;
        Ok(selected)
    }

    #[must_use]
    fn base_oid(&self) -> &GitOid {
        &self.base_oid
    }

    /// Builds the first durable checkpoint, including the exact staging root
    /// that the daemon owns if this wrapper dies during extraction.
    #[must_use]
    fn selection_record(&self, staged: &StagedSource) -> SelectionRecord {
        SelectionRecord {
            base_oid: self.base_oid.clone(),
            repository_root: self.repository_root.clone(),
            repository_device: self.repository_identity.device,
            repository_inode: self.repository_identity.inode,
            staging_path: staged.staging_path.clone(),
            staging_device: staged.staging_identity.device,
            staging_inode: staged.staging_identity.inode,
        }
    }

    /// Prepares the exact recursive manifest command that fixes every blob OID.
    ///
    /// # Errors
    ///
    /// Returns an error if the selected repository path was replaced.
    fn manifest_command(&self) -> Result<GitCommand, Error> {
        self.verify_identity()?;
        Ok(git_command(
            self.git_program.clone(),
            vec![
                OsString::from("-C"),
                self.repository_root.clone().into_os_string(),
                OsString::from("ls-tree"),
                OsString::from("-rz"),
                OsString::from("-l"),
                OsString::from("--full-tree"),
                OsString::from(self.base_oid.as_str()),
            ],
        ))
    }

    fn partial_clone_command(&self) -> GitCommand {
        partial_clone_command(&self.git_program, &self.repository_root)
    }

    /// Prepares one streaming exact-object reader. HEAD is never resolved again
    /// after the first daemon checkpoint.
    ///
    /// # Errors
    ///
    /// Returns an error if the selected repository path was replaced.
    fn blob_command(&self) -> Result<GitCommand, Error> {
        self.verify_identity()?;
        Ok(git_command(
            self.git_program.clone(),
            vec![
                OsString::from("-C"),
                self.repository_root.clone().into_os_string(),
                OsString::from("cat-file"),
                OsString::from("--batch"),
            ],
        ))
    }

    /// Rechecks the exact canonical repository directory after each external
    /// Git command and before the provider is allowed to exec.
    ///
    /// # Errors
    ///
    /// Returns an error if the selected repository path was replaced.
    fn verify_identity(&self) -> Result<(), Error> {
        verify_directory_identity(&self.repository_root, self.repository_identity)
    }
}

/// Exact regular-file manifest read from the persisted commit.
struct TreeManifest {
    files: Vec<ManifestFile>,
    bytes: u64,
}

#[derive(Clone)]
struct ManifestFile {
    path: PathBuf,
    oid: GitOid,
    size: u64,
    executable: bool,
}

impl TreeManifest {
    /// Parses `git ls-tree -rz -l --full-tree` output.
    ///
    /// This intentionally rejects symlinks, gitlinks, special modes, and any
    /// case-insensitive `.git` component. Attributes are ordinary committed
    /// blobs because materialization never asks Git to transform exports.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed output, an unsupported entry, duplicate
    /// path, unsafe path, or crossed hard bound.
    fn parse(output: &[u8], limits: SourceLimits) -> Result<Self, Error> {
        if output.is_empty() {
            return Ok(Self {
                files: Vec::new(),
                bytes: 0,
            });
        }
        if !output.ends_with(&[0]) {
            return Err(Error::InvalidTreeManifest);
        }
        let mut files = Vec::new();
        let mut paths = HashSet::new();
        let mut bytes = 0_u64;
        let records = output.strip_suffix(&[0]).unwrap_or(output);
        for raw in records.split(|byte| *byte == 0) {
            let separator = raw
                .iter()
                .position(|byte| *byte == b'\t')
                .ok_or(Error::InvalidTreeManifest)?;
            let (metadata, path) = raw.split_at(separator);
            let path = path.get(1..).ok_or(Error::InvalidTreeManifest)?;
            let fields = metadata
                .split(|byte| *byte == b' ')
                .filter(|field| !field.is_empty())
                .collect::<Vec<_>>();
            if fields.len() != 4 || fields[1] != b"blob" {
                return Err(Error::UnsupportedTreeEntry);
            }
            let executable = match fields[0] {
                b"100644" => false,
                b"100755" => true,
                _ => return Err(Error::UnsupportedTreeEntry),
            };
            let oid = parse_oid(fields[2])?;
            let size = std::str::from_utf8(fields[3])
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or(Error::InvalidTreeManifest)?;
            let path = PathBuf::from(OsString::from_vec(path.to_vec()));
            validate_tree_path(&path)?;
            if files.len() as u64 >= limits.max_entries {
                return Err(Error::SourceTooLarge);
            }
            bytes = bytes.checked_add(size).ok_or(Error::SourceTooLarge)?;
            if bytes > limits.max_bytes || !paths.insert(path.clone()) {
                return if bytes > limits.max_bytes {
                    Err(Error::SourceTooLarge)
                } else {
                    Err(Error::DuplicateTreePath(path))
                };
            }
            files.push(ManifestFile {
                path,
                oid,
                size,
                executable,
            });
        }
        Ok(Self { files, bytes })
    }

    fn entry_count(&self) -> Result<u64, Error> {
        let mut directories = HashSet::new();
        for file in &self.files {
            let mut parent = file.path.parent();
            while let Some(path) = parent.filter(|path| !path.as_os_str().is_empty()) {
                directories.insert(path.to_owned());
                parent = path.parent();
            }
        }
        u64::try_from(self.files.len())
            .ok()
            .and_then(|files| {
                u64::try_from(directories.len())
                    .ok()
                    .and_then(|directories| files.checked_add(directories))
            })
            .ok_or(Error::SourceTooLarge)
    }
}

/// A `.git`-free source directory published for one Change.
#[derive(Clone, Debug, Eq, PartialEq)]
struct PublishedSource {
    pub path: PathBuf,
    pub identity: DirectoryIdentity,
    pub bytes: u64,
}

/// Small durable handoff written before the wrapper waits for activation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReadyRecord {
    pub base_oid: GitOid,
    pub device: u64,
    pub inode: u64,
    pub size: u64,
}

impl ReadyRecord {
    #[must_use]
    fn from_published(base_oid: GitOid, source: &PublishedSource) -> Self {
        Self {
            base_oid,
            device: source.identity.device,
            inode: source.identity.inode,
            size: source.bytes,
        }
    }
}

/// A private empty staging directory whose exact parent and inode are fixed.
struct StagedSource {
    changes_root: PathBuf,
    changes_root_identity: DirectoryIdentity,
    staging_path: PathBuf,
    staging_identity: DirectoryIdentity,
    published_path: PathBuf,
    limits: SourceLimits,
}

/// Identity-safe state found when restarting a previously checkpointed
/// materialization.
enum ResumedSource {
    Staging(StagedSource),
    Published(PublishedSource),
}

impl StagedSource {
    /// Creates an empty private staging directory beside its final path.
    ///
    /// The changes root must already be an owner-only directory. The fixed
    /// staging and published names must both be absent, making restart state
    /// explicit instead of silently reusing unknown data.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe parent, an existing path, or an I/O
    /// failure. No existing path is removed.
    fn create(changes_root: &Path, change_id: Uuid, limits: SourceLimits) -> Result<Self, Error> {
        let changes_root_identity = private_directory_identity(changes_root)?;
        let suffix = change_id.simple().to_string();
        let staging_path = changes_root.join(format!(".staging-{suffix}"));
        let published_path = changes_root.join(suffix);
        require_absent(&published_path)?;
        let _ = recover_unrecorded_staging_claim(
            changes_root,
            changes_root_identity,
            &staging_path,
            change_id,
        )?;
        let mut builder = DirBuilder::new();
        builder.mode(PRIVATE_DIRECTORY_MODE);
        builder.create(&staging_path).map_err(|source| Error::Io {
            path: staging_path.clone(),
            source,
        })?;
        let staging_identity = private_directory_identity(&staging_path)?;
        sync_directory(changes_root)?;
        Ok(Self {
            changes_root: changes_root.to_owned(),
            changes_root_identity,
            staging_path,
            staging_identity,
            published_path,
            limits,
        })
    }

    /// Reopens exactly the staging or published directory recorded at the
    /// selection checkpoint. It never guesses ownership from a path alone.
    ///
    /// # Errors
    ///
    /// Missing, replaced, simultaneous, or non-canonical paths fail closed.
    fn resume(
        changes_root: &Path,
        change_id: Uuid,
        limits: SourceLimits,
        record: &SelectionRecord,
    ) -> Result<ResumedSource, Error> {
        let changes_root_identity = private_directory_identity(changes_root)?;
        let suffix = change_id.simple().to_string();
        let staging_path = changes_root.join(format!(".staging-{suffix}"));
        let published_path = changes_root.join(suffix);
        if record.staging_path != staging_path {
            return Err(Error::UnsafePath(record.staging_path.clone()));
        }
        let expected = DirectoryIdentity {
            device: record.staging_device,
            inode: record.staging_inode,
        };
        let staging = path_directory_identity(&staging_path)?;
        let published = path_directory_identity(&published_path)?;
        match (staging, published) {
            (Some(identity), None) if identity == expected => {
                private_directory_identity(&staging_path)?;
                Ok(ResumedSource::Staging(Self {
                    changes_root: changes_root.to_owned(),
                    changes_root_identity,
                    staging_path,
                    staging_identity: expected,
                    published_path,
                    limits,
                }))
            }
            (None, Some(identity)) if identity == expected => {
                let (_, bytes) = measure_tree(&published_path, limits)?;
                Ok(ResumedSource::Published(PublishedSource {
                    path: published_path,
                    identity,
                    bytes,
                }))
            }
            (None, None) => Err(Error::MaterializationMissing),
            _ => Err(Error::AmbiguousMaterialization),
        }
    }

    /// Writes the exact blob objects named by the selected tree into the
    /// still-identified staging directory.
    ///
    /// # Errors
    ///
    /// Returns an error on an invalid batch response, crossed bound, unsafe
    /// path, changed blob metadata, or replaced staging root.
    fn materialize_blobs(
        &self,
        requests: &mut impl Write,
        responses: &mut impl BufRead,
        manifest: &TreeManifest,
    ) -> Result<(), Error> {
        self.verify_roots()?;
        require_empty_directory(&self.staging_path)?;
        for file in &manifest.files {
            requests
                .write_all(file.oid.as_str().as_bytes())
                .and_then(|()| requests.write_all(b"\n"))
                .and_then(|()| requests.flush())
                .map_err(Error::GitProcess)?;
            read_blob_header(responses, file)?;
            self.verify_roots()?;
            let destination = self.staging_path.join(&file.path);
            create_private_parents(&self.staging_path, &file.path, self.staging_identity.device)?;
            let mode = if file.executable { 0o755 } else { 0o644 };
            let mut destination_file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&destination)
                .map_err(|source| Error::Io {
                    path: destination.clone(),
                    source,
                })?;
            let copied = io::copy(&mut responses.take(file.size), &mut destination_file)
                .map_err(Error::GitProcess)?;
            if copied != file.size {
                return Err(Error::BlobResponseMismatch(file.path.clone()));
            }
            let mut delimiter = [0_u8; 1];
            responses
                .read_exact(&mut delimiter)
                .map_err(Error::GitProcess)?;
            if delimiter != [b'\n'] {
                return Err(Error::InvalidBlobResponse);
            }
            destination_file.sync_all().map_err(|source| Error::Io {
                path: destination,
                source,
            })?;
        }
        self.verify_roots()?;
        let measured = measure_tree(&self.staging_path, self.limits)?;
        if measured.0 != manifest.entry_count()? || measured.1 != manifest.bytes {
            return Err(Error::BlobResponseMismatch(self.staging_path.clone()));
        }
        for file in &manifest.files {
            let path = self.staging_path.join(&file.path);
            let metadata = fs::symlink_metadata(&path).map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;
            if !metadata.is_file()
                || metadata.file_type().is_symlink()
                || metadata.len() != file.size
                || (metadata.permissions().mode() & 0o111 != 0) != file.executable
            {
                return Err(Error::BlobResponseMismatch(file.path.clone()));
            }
        }
        Ok(())
    }

    /// Atomically publishes the fully extracted source beside its staging
    /// directory and returns the retained inode plus bounded size.
    ///
    /// # Errors
    ///
    /// Returns an error when either recorded directory was replaced, the
    /// destination appeared, the tree is unsafe/oversized, or rename fails.
    fn publish(self) -> Result<PublishedSource, Error> {
        self.verify_roots()?;
        require_absent(&self.published_path)?;
        let (_, bytes) = measure_tree(&self.staging_path, self.limits)?;
        sync_tree(&self.staging_path)?;
        self.verify_roots()?;
        fs::rename(&self.staging_path, &self.published_path).map_err(|source| Error::Io {
            path: self.published_path.clone(),
            source,
        })?;
        let identity = directory_identity(&self.published_path)?;
        if identity != self.staging_identity {
            return Err(Error::IdentityMismatch(self.published_path));
        }
        sync_directory(&self.changes_root)?;
        Ok(PublishedSource {
            path: self.published_path,
            identity,
            bytes,
        })
    }

    /// Clears a partially extracted checkpointed staging tree without changing
    /// its durable root inode, allowing an exact idempotent wrapper retry.
    ///
    /// # Errors
    ///
    /// A replaced root or crossed filesystem boundary fails closed.
    fn reset(self) -> Result<Self, Error> {
        self.verify_roots()?;
        measure_tree_no_follow(&self.staging_path, self.staging_identity.device, None)?;
        clear_directory(&self.staging_path)?;
        sync_directory(&self.staging_path)?;
        self.verify_roots()?;
        Ok(self)
    }

    fn verify_roots(&self) -> Result<(), Error> {
        verify_directory_identity(&self.changes_root, self.changes_root_identity)?;
        verify_directory_identity(&self.staging_path, self.staging_identity)
    }
}

/// Bounded private invocation written by the daemon before it starts the
/// registered hidden wrapper. Provider arguments remain a vector and are never
/// interpreted as a shell command.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MaterializerInvocation {
    pub git_program: PathBuf,
    pub repository_root: PathBuf,
    pub changes_root: PathBuf,
    pub change_id: Uuid,
    pub limits: SourceLimits,
    pub selection_record_path: PathBuf,
    pub selection_activation_path: PathBuf,
    pub ready_record_path: PathBuf,
    pub provider_activation_path: PathBuf,
    pub activation_poll_ms: u64,
    pub provider_program: PathBuf,
    pub provider_arguments: Vec<String>,
}

/// Writes one create-only private materializer invocation and fsyncs it plus
/// its exact private parent.
///
/// # Errors
///
/// Existing, oversized, unsafe, or unserializable invocations are refused.
pub fn write_materializer_invocation(
    path: &Path,
    invocation: &MaterializerInvocation,
) -> Result<(), Error> {
    write_create_only_json(path, invocation, MAX_INVOCATION_BYTES)
}

/// Reads one bounded private materializer invocation.
///
/// # Errors
///
/// Unsafe files, oversized or malformed JSON, zero limits, and invalid UUIDs
/// fail closed.
pub fn read_materializer_invocation(path: &Path) -> Result<MaterializerInvocation, Error> {
    read_json_record(path, MAX_INVOCATION_BYTES)
}

/// Loads a private invocation, resumes its exact selection checkpoint when
/// present, derives the registered runner parent, and runs the wrapper.
///
/// # Errors
///
/// Returns every invocation, checkpoint, materialization, activation, or exec
/// failure from [`run_materializer_wrapper`].
pub fn run_materializer_invocation(path: &Path) -> Result<Infallible, Error> {
    let invocation = read_materializer_invocation(path)?;
    let persisted_selection = match fs::symlink_metadata(&invocation.selection_record_path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Ok(_) => Some(read_selection_record(&invocation.selection_record_path)?),
        Err(source) => {
            return Err(Error::Io {
                path: invocation.selection_record_path,
                source,
            });
        }
    };
    let expected_runner_pid = u32::try_from(
        rustix::process::getppid()
            .ok_or(Error::ParentChanged)?
            .as_raw_nonzero()
            .get(),
    )
    .map_err(|_| Error::InvalidActivationWait)?;
    run_materializer_wrapper(invocation, persisted_selection, expected_runner_pid)
}

/// Runs the hidden materializer inside the already registered provider PID
/// and process group, then replaces that same PID with the real provider.
///
/// The wrapper has two daemon checkpoints. First it writes the selected commit,
/// repository inode, and staging inode, then waits. Only after activation does
/// it read the exact manifest blobs and atomically publish. It then writes the
/// source identity and waits again before final identity checks, `chdir`, and
/// `exec`. A restarted wrapper consumes the persisted selection and never
/// resolves HEAD again.
///
/// # Errors
///
/// Every invalid checkpoint, identity change, command failure, unsafe tree,
/// crossed bound, activation failure, and `exec` failure is returned. Success
/// cannot return because the provider replaces this process.
fn run_materializer_wrapper(
    invocation: MaterializerInvocation,
    persisted_selection: Option<SelectionRecord>,
    expected_runner_pid: u32,
) -> Result<Infallible, Error> {
    let activation_poll_interval = Duration::from_millis(invocation.activation_poll_ms);
    if expected_runner_pid == 0 || activation_poll_interval.is_zero() {
        return Err(Error::InvalidActivationWait);
    }
    let provider_program = exact_executable(&invocation.provider_program)?;
    let (selected, staged, already_published) = match persisted_selection.as_ref() {
        Some(record) => {
            let selected = SelectedRepository::from_record(&invocation.git_program, record)?;
            match StagedSource::resume(
                &invocation.changes_root,
                invocation.change_id,
                invocation.limits,
                record,
            )? {
                ResumedSource::Staging(staged) => {
                    let staged = staged.reset()?;
                    (selected, Some(staged), None)
                }
                ResumedSource::Published(source) => (selected, None, Some(source)),
            }
        }
        None => {
            let selection =
                RepositorySelection::open(&invocation.git_program, &invocation.repository_root)?;
            reject_partial_clone(&selection.partial_clone_command())?;
            let output = run_git_output(&selection.command(), MAX_RECORD_BYTES)?;
            let selected = selection.resolve(&output)?;
            let staged = StagedSource::create(
                &invocation.changes_root,
                invocation.change_id,
                invocation.limits,
            )?;
            (selected, Some(staged), None)
        }
    };
    reject_partial_clone(&selected.partial_clone_command())?;

    let checkpoint = match staged.as_ref() {
        Some(staged) => selected.selection_record(staged),
        None => persisted_selection
            .clone()
            .ok_or(Error::MaterializationMissing)?,
    };
    write_selection_record(&invocation.selection_record_path, &checkpoint)?;
    wait_for_activation(
        &invocation.selection_activation_path,
        expected_runner_pid,
        activation_poll_interval,
    )?;
    selected.verify_identity()?;

    let published = match (staged, already_published) {
        (Some(staged), None) => {
            let manifest_bytes = run_git_output(
                &selected.manifest_command()?,
                manifest_output_limit(invocation.limits),
            )?;
            selected.verify_identity()?;
            let manifest = TreeManifest::parse(&manifest_bytes, invocation.limits)?;
            materialize_blobs_command(&selected.blob_command()?, &staged, &manifest)?;
            selected.verify_identity()?;
            staged.publish()?
        }
        (None, Some(source)) => source,
        _ => return Err(Error::AmbiguousMaterialization),
    };
    let ready = ReadyRecord::from_published(selected.base_oid().clone(), &published);
    write_ready_record(&invocation.ready_record_path, &ready)?;
    wait_for_activation(
        &invocation.provider_activation_path,
        expected_runner_pid,
        activation_poll_interval,
    )?;
    verify_for_provider_exec(&selected, &published)?;

    let error = Command::new(provider_program)
        .args(invocation.provider_arguments)
        .current_dir(&published.path)
        .env("GIT_CEILING_DIRECTORIES", &published.path)
        .exec();
    Err(Error::ProviderExec(error))
}

/// Writes one exact-idempotent, owner-only, bounded JSON ready record and fsyncs
/// both the file and its existing private parent directory.
///
/// # Errors
///
/// An existing record is accepted only when it safely decodes to the same value.
fn write_ready_record(path: &Path, record: &ReadyRecord) -> Result<(), Error> {
    write_json_record(path, record)
}

/// Writes the first exact-idempotent selection checkpoint before blob or source
/// effects are allowed.
///
/// # Errors
///
/// An existing record is accepted only when it safely decodes to the same value.
fn write_selection_record(path: &Path, record: &SelectionRecord) -> Result<(), Error> {
    write_json_record(path, record)
}

/// Reads one bounded, owner-only selection checkpoint.
///
/// # Errors
///
/// Unsafe files, oversized or malformed JSON, and invalid typed fields fail closed.
pub fn read_selection_record(path: &Path) -> Result<SelectionRecord, Error> {
    read_json_record(path, MAX_RECORD_BYTES)
}

/// Removes only the exact persisted selection checkpoint after the Store has
/// durably marked its Change available.
///
/// Missing is idempotent success. An existing file must be an owner-only
/// bounded record that decodes exactly to `record`; replacement and mismatch
/// fail closed before unlink. The private checkpoints parent is fsynced.
///
/// # Errors
///
/// Unsafe, replaced, malformed, or conflicting checkpoints are never removed.
pub fn remove_selection_record(path: &Path, record: &SelectionRecord) -> Result<(), Error> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::UnsafePath(path.to_owned()))?;
    let parent_identity = private_directory_identity(parent)?;
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Ok(_) => {}
        Err(source) => {
            return Err(Error::Io {
                path: path.to_owned(),
                source,
            });
        }
    }
    let expected = private_file_identity(path)?;
    if read_selection_record(path)? != *record {
        return Err(Error::RecordMismatch);
    }
    let current = private_file_identity(path)?;
    if current.identity != expected.identity || current.length != expected.length {
        return Err(Error::IdentityMismatch(path.to_owned()));
    }
    verify_directory_identity(parent, parent_identity)?;
    fs::remove_file(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    require_absent(path)?;
    sync_directory(parent)
}

/// Reads one bounded, owner-only source-ready checkpoint.
///
/// # Errors
///
/// Unsafe files, oversized or malformed JSON, and invalid typed fields fail closed.
pub fn read_ready_record(path: &Path) -> Result<ReadyRecord, Error> {
    read_json_record(path, MAX_RECORD_BYTES)
}

fn write_create_only_json(
    path: &Path,
    value: &impl Serialize,
    maximum_bytes: usize,
) -> Result<(), Error> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::UnsafePath(path.to_owned()))?;
    let parent_identity = private_directory_identity(parent)?;
    let mut bytes = serde_json::to_vec(value).map_err(Error::ReadyRecord)?;
    bytes.push(b'\n');
    if bytes.len() > maximum_bytes {
        return Err(Error::ReadyRecordTooLarge);
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PRIVATE_FILE_MODE)
        .open(path)
        .map_err(|source| {
            if source.kind() == io::ErrorKind::AlreadyExists {
                Error::PathExists(path.to_owned())
            } else {
                Error::Io {
                    path: path.to_owned(),
                    source,
                }
            }
        })?;
    file.write_all(&bytes).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    file.sync_all().map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    let opened = file.metadata().map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    let current = private_file_identity(path)?;
    if current.identity != identity(&opened) || current.length != opened.len() {
        return Err(Error::IdentityMismatch(path.to_owned()));
    }
    verify_directory_identity(parent, parent_identity)?;
    sync_directory(parent)
}

fn write_json_record<T>(path: &Path, record: &T) -> Result<(), Error>
where
    T: Serialize + DeserializeOwned + Eq,
{
    let parent = path
        .parent()
        .ok_or_else(|| Error::UnsafePath(path.to_owned()))?;
    let parent_identity = private_directory_identity(parent)?;
    let mut bytes = serde_json::to_vec(record).map_err(Error::ReadyRecord)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(Error::ReadyRecordTooLarge);
    }
    let opened = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PRIVATE_FILE_MODE)
        .open(path);
    let mut file = match opened {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let existing: T = read_json_record(path, MAX_RECORD_BYTES)?;
            if &existing == record {
                return Ok(());
            }
            return Err(Error::RecordMismatch);
        }
        Err(source) => {
            return Err(Error::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    file.write_all(&bytes).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    file.sync_all().map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    let opened = file.metadata().map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    let current = private_file_identity(path)?;
    if current.identity != identity(&opened) || current.length != opened.len() {
        return Err(Error::IdentityMismatch(path.to_owned()));
    }
    verify_directory_identity(parent, parent_identity)?;
    sync_directory(parent)
}

fn read_json_record<T: DeserializeOwned>(path: &Path, maximum_bytes: usize) -> Result<T, Error> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::UnsafePath(path.to_owned()))?;
    let parent_identity = private_directory_identity(parent)?;
    let expected = private_file_identity(path)?;
    if expected.length > maximum_bytes as u64 {
        return Err(Error::ReadyRecordTooLarge);
    }
    let mut file = File::open(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    let opened = file.metadata().map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    if opened.dev() != expected.identity.device
        || opened.ino() != expected.identity.inode
        || opened.len() != expected.length
    {
        return Err(Error::IdentityMismatch(path.to_owned()));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((maximum_bytes + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?;
    if bytes.len() > maximum_bytes || bytes.len() as u64 != expected.length {
        return Err(Error::ReadyRecordTooLarge);
    }
    let current = private_file_identity(path)?;
    if current.identity != expected.identity || current.length != expected.length {
        return Err(Error::IdentityMismatch(path.to_owned()));
    }
    verify_directory_identity(parent, parent_identity)?;
    serde_json::from_slice(&bytes).map_err(Error::ReadyRecord)
}

/// Rechecks a published source's exact inode before provider exec.
///
/// # Errors
///
/// Returns an error if the source path was replaced or is no longer a
/// directory.
fn verify_published_source(source: &PublishedSource) -> Result<(), Error> {
    verify_directory_identity(&source.path, source.identity)
}

/// Final fail-closed check after provider activation and immediately before
/// `chdir` plus `exec`: neither the selected repository nor published source
/// may have been replaced.
///
/// # Errors
///
/// Returns an error when either exact directory identity changed.
fn verify_for_provider_exec(
    selected: &SelectedRepository,
    source: &PublishedSource,
) -> Result<(), Error> {
    selected.verify_identity()?;
    verify_published_source(source)
}

/// Bounded source size measured only after the caller has proved that no live
/// attempt leases the Change.
///
/// `logical_bytes` counts each non-directory path's apparent length, including
/// every hard-link name, and is used for selected-tree/ready integrity. Storage
/// reporting uses `allocated_bytes`: `st_blocks * 512` counted once per
/// `(device, inode)`, including the root and child directories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceMeasurement {
    pub entries: u64,
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
}

/// Measures one exact quiescent Change without following links or crossing a
/// filesystem boundary.
///
/// # Errors
///
/// Replacement, special directory mounts, I/O errors, and crossed bounds fail
/// closed. The caller, not this filesystem helper, must first prove quiescence
/// from durable run state.
pub fn measure_quiescent_source(
    changes_root: &Path,
    change_id: Uuid,
    expected: DirectoryIdentity,
    limits: SourceLimits,
) -> Result<SourceMeasurement, Error> {
    let parent_identity = private_directory_identity(changes_root)?;
    let source = changes_root.join(change_id.simple().to_string());
    verify_directory_identity(&source, expected)?;
    let measurement = measure_tree_no_follow(&source, expected.device, Some(limits))?;
    verify_directory_identity(&source, expected)?;
    verify_directory_identity(changes_root, parent_identity)?;
    Ok(measurement)
}

/// Measures the one exact staging or already-published inode recorded for a
/// quiescent Provisioning Change. The durable selection record is the sole
/// ownership proof; missing, ambiguous, or replaced paths remain unknown.
///
/// # Errors
///
/// This fails closed unless exactly one deterministic path still has the
/// checkpointed directory identity.
pub fn measure_quiescent_provisioning_source(
    changes_root: &Path,
    change_id: Uuid,
    selection: &SelectionRecord,
    limits: SourceLimits,
) -> Result<SourceMeasurement, Error> {
    let parent_identity = private_directory_identity(changes_root)?;
    let suffix = change_id.simple().to_string();
    let staging = changes_root.join(format!(".staging-{suffix}"));
    let published = changes_root.join(&suffix);
    if selection.staging_path != staging {
        return Err(Error::UnsafePath(selection.staging_path.clone()));
    }
    let expected = DirectoryIdentity {
        device: selection.staging_device,
        inode: selection.staging_inode,
    };
    if expected.inode == 0 || expected.device != parent_identity.device {
        return Err(Error::IdentityMismatch(staging));
    }
    let target = match (
        private_claim_identity(&staging, parent_identity.device)?,
        private_claim_identity(&published, parent_identity.device)?,
    ) {
        (Some(identity), None) if identity == expected => staging,
        (None, Some(identity)) if identity == expected => published,
        _ => return Err(Error::IdentityMismatch(selection.staging_path.clone())),
    };
    let measurement = measure_tree_no_follow(&target, expected.device, Some(limits))?;
    verify_directory_identity(&target, expected)?;
    verify_directory_identity(changes_root, parent_identity)?;
    Ok(measurement)
}

/// Result of exact Change source reclamation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemovalOutcome {
    Removed,
    AlreadyAbsent,
}

/// Atomically quarantines and removes one exact quiescent Change source.
///
/// The caller supplies the durable inode identity and must have already proved
/// that no nonterminal run leases the Change. Restart recovery accepts either
/// the exact source or the exact deterministic quarantine. Missing both is
/// idempotent success; replacement or simultaneous paths fail closed.
///
/// # Errors
///
/// This never follows a symlink or removes an unverified path.
pub fn remove_quiescent_source(
    changes_root: &Path,
    change_id: Uuid,
    expected: DirectoryIdentity,
) -> Result<RemovalOutcome, Error> {
    let parent_identity = private_directory_identity(changes_root)?;
    let suffix = change_id.simple().to_string();
    let source = changes_root.join(&suffix);
    let quarantine = changes_root.join(format!(".removing-{suffix}"));
    remove_exact_directories(
        changes_root,
        parent_identity,
        &[source],
        &quarantine,
        expected,
    )
}

/// Removes only filesystem artifacts owned by one durably abandoned
/// Provisioning Change.
///
/// With a selection record, staging or already-published source ownership is
/// proven by the checkpointed inode. Without one, only the deterministic
/// owner-only staging claim may be quarantined; a final source path is never
/// inferred to be owned. The selection checkpoint remains caller-owned and
/// must be removed only after this helper proves the source artifacts absent.
///
/// The caller must first durably fence the Change from admission and prove no
/// nonterminal run leases it.
///
/// # Errors
///
/// Replacement, ambiguity, unsafe private paths, and unowned published paths
/// fail closed without recursively removing them.
pub fn remove_provisioning_source(
    changes_root: &Path,
    change_id: Uuid,
    selection: Option<&SelectionRecord>,
) -> Result<RemovalOutcome, Error> {
    let parent_identity = private_directory_identity(changes_root)?;
    let suffix = change_id.simple().to_string();
    let staging = changes_root.join(format!(".staging-{suffix}"));
    let published = changes_root.join(&suffix);
    let quarantine = changes_root.join(format!(".removing-{suffix}"));
    let unclaimed = changes_root.join(format!(".unclaimed-{suffix}"));

    let Some(selection) = selection else {
        require_absent(&published)?;
        require_absent(&quarantine)?;
        return recover_unrecorded_staging_claim(
            changes_root,
            parent_identity,
            &staging,
            change_id,
        );
    };
    if selection.staging_path != staging {
        return Err(Error::UnsafePath(selection.staging_path.clone()));
    }
    let expected = DirectoryIdentity {
        device: selection.staging_device,
        inode: selection.staging_inode,
    };
    if expected.inode == 0 || expected.device != parent_identity.device {
        return Err(Error::IdentityMismatch(staging));
    }
    require_absent(&unclaimed)?;
    for path in [&staging, &published, &quarantine] {
        let _ = private_claim_identity(path, parent_identity.device)?;
    }
    remove_exact_directories(
        changes_root,
        parent_identity,
        &[staging, published],
        &quarantine,
        expected,
    )
}

/// Releases one wrapper checkpoint with an exact-idempotent empty owner-only
/// marker and fsyncs its existing private parent.
///
/// # Errors
///
/// An existing marker is accepted only when it is the same safe marker shape;
/// symlinks, content, unsafe modes, and parent replacement fail closed.
pub fn activate_checkpoint(path: &Path) -> Result<(), Error> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::UnsafePath(path.to_owned()))?;
    let parent_identity = private_directory_identity(parent)?;
    let opened = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PRIVATE_FILE_MODE)
        .open(path);
    match opened {
        Ok(file) => {
            file.sync_all().map_err(|source| Error::Io {
                path: path.to_owned(),
                source,
            })?;
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if private_file_identity(path)?.length != 0 {
                return Err(Error::UnsafeActivationMarker);
            }
        }
        Err(source) => {
            return Err(Error::Io {
                path: path.to_owned(),
                source,
            });
        }
    }
    verify_directory_identity(parent, parent_identity)?;
    if private_file_identity(path)?.length != 0 {
        return Err(Error::UnsafeActivationMarker);
    }
    sync_directory(parent)
}

/// Waits until the daemon creates one owner-only activation marker.
///
/// The expected runner must remain this process's parent throughout the wait;
/// orphaning fails closed. The returned marker is never deleted by the
/// wrapper, so a daemon restart cannot turn an observed activation back into
/// an unactivated state.
///
/// # Errors
///
/// Returns an error for a changed parent, unsafe marker, or metadata failure.
fn wait_for_activation(
    marker: &Path,
    expected_parent_pid: u32,
    poll_interval: Duration,
) -> Result<(), Error> {
    if expected_parent_pid == 0 || poll_interval.is_zero() {
        return Err(Error::InvalidActivationWait);
    }
    let expected_parent_pid =
        i32::try_from(expected_parent_pid).map_err(|_| Error::InvalidActivationWait)?;
    let parent_path = marker
        .parent()
        .ok_or_else(|| Error::UnsafePath(marker.to_owned()))?;
    let parent_identity = private_directory_identity(parent_path)?;
    loop {
        verify_directory_identity(parent_path, parent_identity)?;
        let parent = rustix::process::getppid().map(|pid| pid.as_raw_nonzero().get());
        if parent != Some(expected_parent_pid) {
            return Err(Error::ParentChanged);
        }
        match fs::symlink_metadata(marker) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink()
                    || !metadata.is_file()
                    || metadata.uid() != rustix::process::geteuid().as_raw()
                    || metadata.permissions().mode() & 0o777 != PRIVATE_FILE_MODE
                    || metadata.len() != 0
                {
                    return Err(Error::UnsafeActivationMarker);
                }
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                thread::sleep(poll_interval);
            }
            Err(source) => {
                return Err(Error::Io {
                    path: marker.to_owned(),
                    source,
                });
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("source limits must be non-zero")]
    InvalidLimits,
    #[error("Git emitted a non-canonical object ID")]
    InvalidOid,
    #[error("trusted executable is not an absolute executable regular file: {0:?}")]
    InvalidExecutable(PathBuf),
    #[error("directory is not an exact non-symlink directory: {0:?}")]
    InvalidDirectory(PathBuf),
    #[error("private directory is not owner-only: {0:?}")]
    UnsafePrivateDirectory(PathBuf),
    #[error("path already exists: {0:?}")]
    PathExists(PathBuf),
    #[error("path is unsafe: {0:?}")]
    UnsafePath(PathBuf),
    #[error("source crosses a filesystem boundary: {0:?}")]
    FilesystemBoundary(PathBuf),
    #[error("directory identity changed: {0:?}")]
    IdentityMismatch(PathBuf),
    #[error("checkpointed materialization path is missing")]
    MaterializationMissing,
    #[error("checkpointed materialization paths are replaced or ambiguous")]
    AmbiguousMaterialization,
    #[error("staging directory is not empty: {0:?}")]
    StagingNotEmpty(PathBuf),
    #[error("source exceeds its hard entry or byte bound")]
    SourceTooLarge,
    #[error("allocated storage measurement overflowed")]
    StorageMeasurementOverflow,
    #[error("git tree path is unsafe: {0:?}")]
    UnsafeTreePath(PathBuf),
    #[error("git tree contains a duplicate path: {0:?}")]
    DuplicateTreePath(PathBuf),
    #[error("git tree manifest is malformed")]
    InvalidTreeManifest,
    #[error("git tree contains a symlink, gitlink, or unsupported mode")]
    UnsupportedTreeEntry,
    #[error("Git returned an invalid blob batch response")]
    InvalidBlobResponse,
    #[error("Git blob response did not match the selected tree: {0:?}")]
    BlobResponseMismatch(PathBuf),
    #[error("Git selection output is not the exact canonical root and full HEAD commit")]
    InvalidSelectionOutput,
    #[error("partial-clone repositories are not accepted as local source authority")]
    PartialCloneUnsupported,
    #[error("ready record encoding failed: {0}")]
    ReadyRecord(serde_json::Error),
    #[error("ready record is unexpectedly large")]
    ReadyRecordTooLarge,
    #[error("existing checkpoint does not match the requested record")]
    RecordMismatch,
    #[error("activation wait has an invalid parent or poll interval")]
    InvalidActivationWait,
    #[error("registered runner is no longer the wrapper parent")]
    ParentChanged,
    #[error("activation marker is not an empty owner-only regular file")]
    UnsafeActivationMarker,
    #[error("Git command failed with status {0}")]
    GitFailed(std::process::ExitStatus),
    #[error("Git command output exceeded its hard bound")]
    GitOutputTooLarge,
    #[error("could not spawn or supervise Git: {0}")]
    GitProcess(io::Error),
    #[error("provider exec failed: {0}")]
    ProviderExec(io::Error),
    #[error("I/O failed at {path:?}: {source}")]
    Io { path: PathBuf, source: io::Error },
}

fn git_command(program: PathBuf, arguments: Vec<OsString>) -> GitCommand {
    GitCommand {
        program,
        arguments,
        environment: vec![
            (OsString::from("LC_ALL"), OsString::from("C")),
            (OsString::from("GIT_CONFIG_NOSYSTEM"), OsString::from("1")),
            (
                OsString::from("GIT_CONFIG_GLOBAL"),
                OsString::from("/dev/null"),
            ),
            (
                OsString::from("GIT_NO_REPLACE_OBJECTS"),
                OsString::from("1"),
            ),
            (OsString::from("GIT_NO_LAZY_FETCH"), OsString::from("1")),
            (OsString::from("GIT_OPTIONAL_LOCKS"), OsString::from("0")),
            (OsString::from("GIT_TERMINAL_PROMPT"), OsString::from("0")),
        ],
    }
}

fn partial_clone_command(program: &Path, repository_root: &Path) -> GitCommand {
    git_command(
        program.to_owned(),
        vec![
            OsString::from("-C"),
            repository_root.as_os_str().to_owned(),
            OsString::from("config"),
            OsString::from("--local"),
            OsString::from("--null"),
            OsString::from("--get-regexp"),
            OsString::from("^(extensions\\.partialclone|remote\\..*\\.promisor)$"),
        ],
    )
}

fn reject_partial_clone(command: &GitCommand) -> Result<(), Error> {
    let (status, output) = run_git_output_status(command, MAX_RECORD_BYTES)?;
    match (status.code(), output.is_empty()) {
        (Some(1), true) => Ok(()),
        (_, false) => Err(Error::PartialCloneUnsupported),
        _ => Err(Error::GitFailed(status)),
    }
}

fn run_git_output(command: &GitCommand, maximum_bytes: usize) -> Result<Vec<u8>, Error> {
    let (status, output) = run_git_output_status(command, maximum_bytes)?;
    if !status.success() {
        return Err(Error::GitFailed(status));
    }
    Ok(output)
}

fn run_git_output_status(
    command: &GitCommand,
    maximum_bytes: usize,
) -> Result<(std::process::ExitStatus, Vec<u8>), Error> {
    let mut child = command
        .command()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(Error::GitProcess)?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::GitProcess(io::Error::other("Git stdout pipe is absent")))?;
    let read_limit = u64::try_from(maximum_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut output = Vec::new();
    let read = stdout
        .by_ref()
        .take(read_limit)
        .read_to_end(&mut output)
        .map_err(Error::GitProcess);
    if read.is_err() || output.len() > maximum_bytes {
        let _ = child.kill();
    }
    let status = child.wait().map_err(Error::GitProcess)?;
    read?;
    if output.len() > maximum_bytes {
        return Err(Error::GitOutputTooLarge);
    }
    Ok((status, output))
}

fn materialize_blobs_command(
    command: &GitCommand,
    staged: &StagedSource,
    manifest: &TreeManifest,
) -> Result<(), Error> {
    let mut child = command
        .command()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(Error::GitProcess)?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| Error::GitProcess(io::Error::other("Git stdin pipe is absent")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::GitProcess(io::Error::other("Git stdout pipe is absent")))?;
    let mut stdout = BufReader::new(stdout);
    let materialization = staged.materialize_blobs(&mut stdin, &mut stdout, manifest);
    drop(stdin);
    if materialization.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().map_err(Error::GitProcess)?;
    materialization?;
    if !status.success() {
        return Err(Error::GitFailed(status));
    }
    Ok(())
}

fn manifest_output_limit(limits: SourceLimits) -> usize {
    usize::try_from(limits.max_entries)
        .unwrap_or(usize::MAX)
        .saturating_mul(MAX_PATH_BYTES + 160)
        .min(MAX_MANIFEST_OUTPUT_BYTES)
}

fn parse_oid(oid: &[u8]) -> Result<GitOid, Error> {
    if !matches!(oid.len(), 40 | 64)
        || !oid.iter().all(u8::is_ascii_hexdigit)
        || oid.iter().any(u8::is_ascii_uppercase)
    {
        return Err(Error::InvalidOid);
    }
    let value = std::str::from_utf8(oid).map_err(|_| Error::InvalidOid)?;
    Ok(GitOid(value.to_owned()))
}

fn read_blob_header(reader: &mut impl BufRead, expected: &ManifestFile) -> Result<(), Error> {
    let mut header = Vec::with_capacity(MAX_BLOB_HEADER_BYTES);
    let mut terminated = false;
    for _ in 0..MAX_BLOB_HEADER_BYTES {
        let mut byte = [0_u8; 1];
        reader.read_exact(&mut byte).map_err(Error::GitProcess)?;
        if byte == [b'\n'] {
            terminated = true;
            break;
        }
        header.push(byte[0]);
    }
    if !terminated {
        return Err(Error::InvalidBlobResponse);
    }
    let fields = header.split(|byte| *byte == b' ').collect::<Vec<_>>();
    let size = fields
        .get(2)
        .and_then(|size| std::str::from_utf8(size).ok())
        .and_then(|size| size.parse::<u64>().ok());
    if fields.len() != 3
        || fields[0] != expected.oid.as_str().as_bytes()
        || fields[1] != b"blob"
        || size != Some(expected.size)
    {
        return Err(Error::BlobResponseMismatch(expected.path.clone()));
    }
    Ok(())
}

fn create_private_parents(
    staging_root: &Path,
    file_path: &Path,
    expected_device: u64,
) -> Result<(), Error> {
    let Some(parent) = file_path.parent() else {
        return Err(Error::UnsafeTreePath(file_path.to_owned()));
    };
    let mut current = staging_root.to_owned();
    for component in parent.components() {
        let Component::Normal(name) = component else {
            return Err(Error::UnsafeTreePath(file_path.to_owned()));
        };
        current.push(name);
        let mut builder = DirBuilder::new();
        builder.mode(PRIVATE_DIRECTORY_MODE);
        match builder.create(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(source) => {
                return Err(Error::Io {
                    path: current,
                    source,
                });
            }
        }
        let identity = private_directory_identity(&current)?;
        if identity.device != expected_device {
            return Err(Error::FilesystemBoundary(current));
        }
    }
    Ok(())
}

fn exact_executable(path: &Path) -> Result<PathBuf, Error> {
    if !path.is_absolute() {
        return Err(Error::InvalidExecutable(path.to_owned()));
    }
    let metadata =
        fs::symlink_metadata(path).map_err(|_| Error::InvalidExecutable(path.to_owned()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.permissions().mode() & 0o111 == 0
    {
        return Err(Error::InvalidExecutable(path.to_owned()));
    }
    Ok(path.to_owned())
}

fn private_directory_identity(path: &Path) -> Result<DirectoryIdentity, Error> {
    let metadata = fs::symlink_metadata(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o777 != PRIVATE_DIRECTORY_MODE
    {
        return Err(Error::UnsafePrivateDirectory(path.to_owned()));
    }
    Ok(identity(&metadata))
}

struct PrivateFileIdentity {
    identity: DirectoryIdentity,
    length: u64,
}

fn private_file_identity(path: &Path) -> Result<PrivateFileIdentity, Error> {
    let metadata = fs::symlink_metadata(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o777 != PRIVATE_FILE_MODE
    {
        return Err(Error::UnsafePath(path.to_owned()));
    }
    Ok(PrivateFileIdentity {
        identity: identity(&metadata),
        length: metadata.len(),
    })
}

fn directory_identity(path: &Path) -> Result<DirectoryIdentity, Error> {
    let metadata = fs::symlink_metadata(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::InvalidDirectory(path.to_owned()));
    }
    Ok(identity(&metadata))
}

fn path_directory_identity(path: &Path) -> Result<Option<DirectoryIdentity>, Error> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => {
            Ok(Some(identity(&metadata)))
        }
        Ok(_) => Err(Error::InvalidDirectory(path.to_owned())),
        Err(source) => Err(Error::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

fn identity(metadata: &fs::Metadata) -> DirectoryIdentity {
    DirectoryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

fn verify_directory_identity(path: &Path, expected: DirectoryIdentity) -> Result<(), Error> {
    if directory_identity(path)? == expected {
        Ok(())
    } else {
        Err(Error::IdentityMismatch(path.to_owned()))
    }
}

fn require_absent(path: &Path) -> Result<(), Error> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(Error::PathExists(path.to_owned())),
        Err(source) => Err(Error::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

fn remove_exact_directories(
    changes_root: &Path,
    parent_identity: DirectoryIdentity,
    sources: &[PathBuf],
    quarantine: &Path,
    expected: DirectoryIdentity,
) -> Result<RemovalOutcome, Error> {
    let mut found = None;
    for path in sources
        .iter()
        .map(PathBuf::as_path)
        .chain(std::iter::once(quarantine))
    {
        if let Some(identity) = path_directory_identity(path)? {
            if identity != expected || found.is_some() {
                return Err(Error::AmbiguousMaterialization);
            }
            found = Some(path.to_owned());
        }
    }
    let Some(source) = found else {
        return Ok(RemovalOutcome::AlreadyAbsent);
    };
    let target = if source == quarantine {
        source
    } else {
        verify_directory_identity(changes_root, parent_identity)?;
        fs::rename(&source, quarantine).map_err(|source| Error::Io {
            path: quarantine.to_owned(),
            source,
        })?;
        sync_directory(changes_root)?;
        quarantine.to_owned()
    };
    remove_identified_directory(changes_root, parent_identity, &target, expected)?;
    Ok(RemovalOutcome::Removed)
}

fn remove_identified_directory(
    changes_root: &Path,
    parent_identity: DirectoryIdentity,
    target: &Path,
    expected: DirectoryIdentity,
) -> Result<(), Error> {
    verify_directory_identity(changes_root, parent_identity)?;
    verify_directory_identity(target, expected)?;
    measure_tree_no_follow(target, expected.device, None)?;
    verify_directory_identity(target, expected)?;
    verify_directory_identity(changes_root, parent_identity)?;
    fs::remove_dir_all(target).map_err(|source| Error::Io {
        path: target.to_owned(),
        source,
    })?;
    require_absent(target)?;
    sync_directory(changes_root)
}

fn private_claim_identity(
    path: &Path,
    expected_device: u64,
) -> Result<Option<DirectoryIdentity>, Error> {
    let metadata = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Ok(metadata) => metadata,
        Err(source) => {
            return Err(Error::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o777 != PRIVATE_DIRECTORY_MODE
        || metadata.dev() != expected_device
    {
        return Err(Error::UnsafePrivateDirectory(path.to_owned()));
    }
    Ok(Some(identity(&metadata)))
}

fn recover_unrecorded_staging_claim(
    changes_root: &Path,
    parent_identity: DirectoryIdentity,
    staging_path: &Path,
    change_id: Uuid,
) -> Result<RemovalOutcome, Error> {
    let quarantine = changes_root.join(format!(".unclaimed-{}", change_id.simple()));
    let staging = private_claim_identity(staging_path, parent_identity.device)?;
    let quarantined = private_claim_identity(&quarantine, parent_identity.device)?;
    let (claimed, target) = match (staging, quarantined) {
        (None, None) => return Ok(RemovalOutcome::AlreadyAbsent),
        (Some(claimed), None) => {
            verify_directory_identity(changes_root, parent_identity)?;
            fs::rename(staging_path, &quarantine).map_err(|source| Error::Io {
                path: quarantine.clone(),
                source,
            })?;
            sync_directory(changes_root)?;
            (claimed, quarantine)
        }
        (None, Some(claimed)) => (claimed, quarantine),
        (Some(_), Some(_)) => return Err(Error::AmbiguousMaterialization),
    };
    remove_identified_directory(changes_root, parent_identity, &target, claimed)?;
    Ok(RemovalOutcome::Removed)
}

fn require_empty_directory(path: &Path) -> Result<(), Error> {
    let mut entries = fs::read_dir(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;
    if entries
        .next()
        .transpose()
        .map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?
        .is_some()
    {
        return Err(Error::StagingNotEmpty(path.to_owned()));
    }
    Ok(())
}

fn validate_tree_path(path: &Path) -> Result<(), Error> {
    if path.as_os_str().as_encoded_bytes().len() > MAX_PATH_BYTES || path.as_os_str().is_empty() {
        return Err(Error::UnsafeTreePath(path.to_owned()));
    }
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err(Error::UnsafeTreePath(path.to_owned()));
        };
        if ascii_name_eq(name, b".git") {
            return Err(Error::UnsafeTreePath(path.to_owned()));
        }
    }
    Ok(())
}

fn validate_canonical_absolute_path(path: &Path) -> Result<(), Error> {
    if !path.is_absolute()
        || path.as_os_str().as_encoded_bytes().len() > MAX_PATH_BYTES
        || path.components().any(|component| {
            matches!(
                component,
                Component::CurDir | Component::ParentDir | Component::Prefix(_)
            )
        })
    {
        return Err(Error::UnsafePath(path.to_owned()));
    }
    Ok(())
}

fn ascii_name_eq(name: &OsStr, expected: &[u8]) -> bool {
    name.as_encoded_bytes().eq_ignore_ascii_case(expected)
}

fn measure_tree(root: &Path, limits: SourceLimits) -> Result<(u64, u64), Error> {
    let mut pending = vec![root.to_owned()];
    let mut entries = 0_u64;
    let mut bytes = 0_u64;
    while let Some(directory) = pending.pop() {
        let children = fs::read_dir(&directory).map_err(|source| Error::Io {
            path: directory.clone(),
            source,
        })?;
        for child in children {
            let child = child.map_err(|source| Error::Io {
                path: directory.clone(),
                source,
            })?;
            let path = child.path();
            validate_tree_path(
                path.strip_prefix(root)
                    .map_err(|_| Error::UnsafePath(path.clone()))?,
            )?;
            let metadata = fs::symlink_metadata(&path).map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;
            if metadata.file_type().is_symlink() || !(metadata.is_dir() || metadata.is_file()) {
                return Err(Error::UnsafePath(path));
            }
            entries = entries.checked_add(1).ok_or(Error::SourceTooLarge)?;
            if metadata.is_file() {
                bytes = bytes
                    .checked_add(metadata.len())
                    .ok_or(Error::SourceTooLarge)?;
            } else {
                pending.push(path);
            }
            if entries > limits.max_entries || bytes > limits.max_bytes {
                return Err(Error::SourceTooLarge);
            }
        }
    }
    Ok((entries, bytes))
}

fn measure_tree_no_follow(
    root: &Path,
    expected_device: u64,
    limits: Option<SourceLimits>,
) -> Result<SourceMeasurement, Error> {
    let root_metadata = fs::symlink_metadata(root).map_err(|source| Error::Io {
        path: root.to_owned(),
        source,
    })?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(Error::InvalidDirectory(root.to_owned()));
    }
    if root_metadata.dev() != expected_device {
        return Err(Error::FilesystemBoundary(root.to_owned()));
    }
    let mut pending = vec![root.to_owned()];
    let mut entries = 0_u64;
    let mut logical_bytes = 0_u64;
    let mut allocated_bytes = allocated_bytes_from_blocks(root_metadata.blocks())?;
    let mut allocated_inodes = HashSet::from([(root_metadata.dev(), root_metadata.ino())]);
    while let Some(directory) = pending.pop() {
        let children = fs::read_dir(&directory).map_err(|source| Error::Io {
            path: directory.clone(),
            source,
        })?;
        for child in children {
            let path = child
                .map_err(|source| Error::Io {
                    path: directory.clone(),
                    source,
                })?
                .path();
            let metadata = fs::symlink_metadata(&path).map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;
            entries = entries.checked_add(1).ok_or(Error::SourceTooLarge)?;
            if metadata.dev() != expected_device {
                return Err(Error::FilesystemBoundary(path));
            }
            if allocated_inodes.insert((metadata.dev(), metadata.ino())) {
                allocated_bytes = allocated_bytes
                    .checked_add(allocated_bytes_from_blocks(metadata.blocks())?)
                    .ok_or(Error::StorageMeasurementOverflow)?;
            }
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                pending.push(path);
            } else {
                logical_bytes = logical_bytes
                    .checked_add(metadata.len())
                    .ok_or(Error::SourceTooLarge)?;
            }
            if limits.is_some_and(|limits| {
                entries > limits.max_entries || logical_bytes > limits.max_bytes
            }) {
                return Err(Error::SourceTooLarge);
            }
        }
    }
    Ok(SourceMeasurement {
        entries,
        logical_bytes,
        allocated_bytes,
    })
}

fn allocated_bytes_from_blocks(blocks: u64) -> Result<u64, Error> {
    blocks
        .checked_mul(STAT_BLOCK_BYTES)
        .ok_or(Error::StorageMeasurementOverflow)
}

fn clear_directory(root: &Path) -> Result<(), Error> {
    let children = fs::read_dir(root).map_err(|source| Error::Io {
        path: root.to_owned(),
        source,
    })?;
    for child in children {
        let path = child
            .map_err(|source| Error::Io {
                path: root.to_owned(),
                source,
            })?
            .path();
        let metadata = fs::symlink_metadata(&path).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        let removed = if metadata.is_dir() && !metadata.file_type().is_symlink() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        removed.map_err(|source| Error::Io { path, source })?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), Error> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })
}

fn sync_tree(root: &Path) -> Result<(), Error> {
    let mut pending = vec![root.to_owned()];
    let mut directories = Vec::new();
    while let Some(directory) = pending.pop() {
        let children = fs::read_dir(&directory).map_err(|source| Error::Io {
            path: directory.clone(),
            source,
        })?;
        for child in children {
            let path = child
                .map_err(|source| Error::Io {
                    path: directory.clone(),
                    source,
                })?
                .path();
            let metadata = fs::symlink_metadata(&path).map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() {
                File::open(&path)
                    .and_then(|file| file.sync_all())
                    .map_err(|source| Error::Io { path, source })?;
            } else {
                return Err(Error::UnsafePath(path));
            }
        }
        directories.push(directory);
    }
    for directory in directories.into_iter().rev() {
        sync_directory(&directory)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt, symlink},
        process::Stdio,
    };

    use super::*;

    fn private_dir(path: &Path) {
        let mut builder = fs::DirBuilder::new();
        builder.mode(PRIVATE_DIRECTORY_MODE);
        builder.create(path).unwrap();
    }

    fn find_git() -> PathBuf {
        ["/usr/bin/git", "/opt/homebrew/bin/git"]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())
            .expect("git executable")
    }

    fn run(command: &GitCommand) -> std::process::Output {
        command
            .command()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap()
    }

    fn change_id_from_record(record: &SelectionRecord) -> Uuid {
        let name = record.staging_path.file_name().unwrap().to_str().unwrap();
        Uuid::parse_str(name.strip_prefix(".staging-").unwrap()).unwrap()
    }

    fn selection_for(
        changes: &Path,
        change_id: Uuid,
        staging_identity: DirectoryIdentity,
    ) -> SelectionRecord {
        SelectionRecord {
            base_oid: GitOid::try_from("a".repeat(40)).unwrap(),
            repository_root: changes.to_owned(),
            repository_device: directory_identity(changes).unwrap().device,
            repository_inode: directory_identity(changes).unwrap().inode,
            staging_path: changes.join(format!(".staging-{}", change_id.simple())),
            staging_device: staging_identity.device,
            staging_inode: staging_identity.inode,
        }
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, SelectedRepository) {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let repository = root.path().join("repository");
        private_dir(&repository);
        let git = find_git();
        let init = Command::new(&git)
            .arg("init")
            .arg("--quiet")
            .arg(&repository)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap();
        assert!(init.success());
        fs::write(repository.join("tracked.txt"), "base\n").unwrap();
        private_dir(&repository.join("nested"));
        fs::write(repository.join("nested/executable.sh"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(
            repository.join("nested/executable.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let add = Command::new(&git)
            .current_dir(&repository)
            .args(["add", "."])
            .status()
            .unwrap();
        assert!(add.success());
        let commit = Command::new(&git)
            .current_dir(&repository)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "base",
            ])
            .status()
            .unwrap();
        assert!(commit.success());
        let selection = RepositorySelection::open(&git, &repository).unwrap();
        let resolved = run(&selection.command());
        assert!(resolved.status.success(), "{:?}", resolved.stderr);
        let selected = selection.resolve(&resolved.stdout).unwrap();
        (root, git, repository, selected)
    }

    fn materialized_source() -> (tempfile::TempDir, StagedSource, SelectionRecord) {
        let (root, git, repository, selected) = fixture();
        fs::write(root.path().join("repository/tracked.txt"), "dirty\n").unwrap();
        fs::write(root.path().join("repository/untracked.txt"), "ignored\n").unwrap();
        let commit = Command::new(&git)
            .current_dir(&repository)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "add",
                ".",
            ])
            .status()
            .unwrap();
        assert!(commit.success());
        let commit = Command::new(&git)
            .current_dir(&repository)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "later",
            ])
            .status()
            .unwrap();
        assert!(commit.success());
        let limits = SourceLimits::new(20, 4096).unwrap();
        let manifest = run(&selected.manifest_command().unwrap());
        assert!(manifest.status.success(), "{:?}", manifest.stderr);
        let manifest = TreeManifest::parse(&manifest.stdout, limits).unwrap();
        selected.verify_identity().unwrap();
        let changes = root.path().join("changes");
        private_dir(&changes);
        let staged = StagedSource::create(&changes, Uuid::new_v4(), limits).unwrap();
        let record = selected.selection_record(&staged);
        materialize_blobs_command(&selected.blob_command().unwrap(), &staged, &manifest).unwrap();
        (root, staged, record)
    }

    #[test]
    fn exact_commit_is_published_without_git_metadata() {
        let (_root, staged, selection) = materialized_source();
        let published = staged.publish().unwrap();
        assert_eq!(
            fs::read_to_string(published.path.join("tracked.txt")).unwrap(),
            "base\n"
        );
        assert!(!published.path.join("untracked.txt").exists());
        assert!(fs::symlink_metadata(published.path.join(".git")).is_err());
        assert_eq!(
            fs::symlink_metadata(published.path.join("nested/executable.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0o111
        );
        let ready = ReadyRecord::from_published(selection.base_oid, &published);
        assert_eq!(ready.device, published.identity.device);
        assert_eq!(ready.inode, published.identity.inode);
        assert_eq!(ready.size, 15);
    }

    #[test]
    fn repository_info_attributes_cannot_transform_committed_blob_bytes() {
        let (root, git, repository, _old_selection) = fixture();
        const COMMITTED: &str = "$Format:%H$\n";
        fs::write(repository.join("tracked.txt"), COMMITTED).unwrap();
        let commit = Command::new(&git)
            .current_dir(&repository)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--quiet",
                "-am",
                "export substitution fixture",
            ])
            .status()
            .unwrap();
        assert!(commit.success());
        fs::write(
            repository.join(".git/info/attributes"),
            "tracked.txt export-subst\n",
        )
        .unwrap();

        let selection = RepositorySelection::open(&git, &repository).unwrap();
        let resolved = run(&selection.command());
        assert!(resolved.status.success(), "{:?}", resolved.stderr);
        let selected = selection.resolve(&resolved.stdout).unwrap();
        let limits = SourceLimits::new(20, 4096).unwrap();
        let output = run(&selected.manifest_command().unwrap());
        assert!(output.status.success(), "{:?}", output.stderr);
        let manifest = TreeManifest::parse(&output.stdout, limits).unwrap();
        let changes = root.path().join("changes");
        private_dir(&changes);
        let staged = StagedSource::create(&changes, Uuid::new_v4(), limits).unwrap();
        materialize_blobs_command(&selected.blob_command().unwrap(), &staged, &manifest).unwrap();
        let published = staged.publish().unwrap();

        assert_eq!(
            fs::read_to_string(published.path.join("tracked.txt")).unwrap(),
            COMMITTED
        );
    }

    #[test]
    fn unrecorded_staging_contents_are_never_adopted_after_crash() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let changes = root.path().join("changes");
        private_dir(&changes);
        let change_id = Uuid::new_v4();
        let abandoned =
            StagedSource::create(&changes, change_id, SourceLimits::new(20, 4096).unwrap())
                .unwrap();
        fs::write(abandoned.staging_path.join("partial"), "never publish").unwrap();
        drop(abandoned);

        let replacement =
            StagedSource::create(&changes, change_id, SourceLimits::new(20, 4096).unwrap())
                .unwrap();
        assert!(
            fs::read_dir(&replacement.staging_path)
                .unwrap()
                .next()
                .is_none()
        );
        assert!(
            fs::symlink_metadata(changes.join(format!(".unclaimed-{}", change_id.simple())))
                .is_err()
        );
    }

    #[test]
    fn persisted_staging_and_published_source_resume_by_exact_inode() {
        let (root, _staged, record) = materialized_source();
        let change_id = change_id_from_record(&record);
        let changes = root.path().join("changes");
        let resumed = StagedSource::resume(
            &changes,
            change_id,
            SourceLimits::new(20, 4096).unwrap(),
            &record,
        )
        .unwrap();
        let ResumedSource::Staging(resumed) = resumed else {
            panic!("expected staging");
        };
        let published = resumed.publish().unwrap();
        let resumed = StagedSource::resume(
            &changes,
            change_id,
            SourceLimits::new(20, 4096).unwrap(),
            &record,
        )
        .unwrap();
        let ResumedSource::Published(resumed) = resumed else {
            panic!("expected published source");
        };
        assert_eq!(resumed.identity, published.identity);
        assert_eq!(resumed.bytes, published.bytes);
    }

    #[test]
    fn replaced_staging_path_is_never_published_or_removed() {
        let (root, staged, _selection) = materialized_source();
        let path = staged.staging_path.clone();
        fs::remove_dir_all(&path).unwrap();
        let replacement = root.path().join("replacement");
        private_dir(&replacement);
        fs::write(replacement.join("keep"), "present").unwrap();
        symlink(&replacement, &path).unwrap();
        assert!(matches!(staged.reset(), Err(Error::InvalidDirectory(_))));
        assert_eq!(
            fs::read_to_string(replacement.join("keep")).unwrap(),
            "present"
        );

        let changes = root.path().join("changes");
        let second = StagedSource::create(
            &changes,
            Uuid::new_v4(),
            SourceLimits::new(20, 4096).unwrap(),
        )
        .unwrap();
        let second_path = second.staging_path.clone();
        fs::remove_dir(&second_path).unwrap();
        symlink(&replacement, &second_path).unwrap();
        assert!(matches!(second.publish(), Err(Error::InvalidDirectory(_))));
        assert_eq!(
            fs::read_to_string(replacement.join("keep")).unwrap(),
            "present"
        );
    }

    #[test]
    fn ready_record_is_create_only_and_private() {
        let (root, staged, selection) = materialized_source();
        let published = staged.publish().unwrap();
        let runtime = root.path().join("runtime");
        private_dir(&runtime);
        let path = runtime.join("change.ready");
        let record = ReadyRecord::from_published(selection.base_oid, &published);
        write_ready_record(&path, &record).unwrap();
        assert_eq!(
            fs::symlink_metadata(&path).unwrap().permissions().mode() & 0o777,
            PRIVATE_FILE_MODE
        );
        write_ready_record(&path, &record).unwrap();
        assert_eq!(read_ready_record(&path).unwrap(), record);
        let mut conflicting = record.clone();
        conflicting.size += 1;
        assert!(matches!(
            write_ready_record(&path, &conflicting),
            Err(Error::RecordMismatch)
        ));
    }

    #[test]
    fn invocation_is_bounded_private_and_create_only() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().join("materialize.json");
        let invocation = MaterializerInvocation {
            git_program: PathBuf::from("/usr/bin/git"),
            repository_root: root.path().join("repository"),
            changes_root: root.path().join("changes"),
            change_id: Uuid::new_v4(),
            limits: SourceLimits::new(20, 4096).unwrap(),
            selection_record_path: root.path().join("selection.ready"),
            selection_activation_path: root.path().join("selection.activate"),
            ready_record_path: root.path().join("source.ready"),
            provider_activation_path: root.path().join("provider.activate"),
            activation_poll_ms: 10,
            provider_program: PathBuf::from("/bin/sh"),
            provider_arguments: vec!["-c".into(), "exit 0".into()],
        };
        write_materializer_invocation(&path, &invocation).unwrap();
        assert_eq!(read_materializer_invocation(&path).unwrap(), invocation);
        assert_eq!(
            fs::symlink_metadata(&path).unwrap().permissions().mode() & 0o777,
            PRIVATE_FILE_MODE
        );
        assert!(matches!(
            write_materializer_invocation(&path, &invocation),
            Err(Error::PathExists(_))
        ));
    }

    #[test]
    fn selection_is_durable_before_blobs_and_never_reresolves_head() {
        let (root, _git, _repository, selected) = fixture();
        let changes = root.path().join("changes");
        private_dir(&changes);
        let staged = StagedSource::create(
            &changes,
            Uuid::new_v4(),
            SourceLimits::new(20, 4096).unwrap(),
        )
        .unwrap();
        let record = selected.selection_record(&staged);
        let runtime = root.path().join("runtime");
        private_dir(&runtime);
        let selection_path = runtime.join("selection.ready");
        write_selection_record(&selection_path, &record).unwrap();
        let decoded = read_selection_record(&selection_path).unwrap();
        assert_eq!(decoded, record);
        write_selection_record(&selection_path, &record).unwrap();

        let activation = runtime.join("selection.activate");
        activate_checkpoint(&activation).unwrap();
        activate_checkpoint(&activation).unwrap();
        let parent = rustix::process::getppid().unwrap();
        wait_for_activation(
            &activation,
            parent.as_raw_nonzero().get() as u32,
            Duration::from_millis(1),
        )
        .unwrap();

        let mut conflicting = record.clone();
        conflicting.staging_inode = conflicting.staging_inode.saturating_add(1);
        assert!(matches!(
            remove_selection_record(&selection_path, &conflicting),
            Err(Error::RecordMismatch)
        ));
        assert!(selection_path.is_file());
        remove_selection_record(&selection_path, &record).unwrap();
        remove_selection_record(&selection_path, &record).unwrap();
        assert!(fs::symlink_metadata(selection_path).is_err());
    }

    #[test]
    fn selected_repository_replacement_is_refused() {
        let (root, _git, repository, selected) = fixture();
        let original = root.path().join("original-repository");
        fs::rename(&repository, &original).unwrap();
        private_dir(&repository);
        assert!(matches!(
            selected.manifest_command(),
            Err(Error::IdentityMismatch(_))
        ));
        assert!(original.join(".git").is_dir());
    }

    #[test]
    fn every_git_read_disables_promisor_lazy_fetch() {
        let (_root, git, repository, selected) = fixture();
        for command in [
            RepositorySelection::open(&git, &repository)
                .unwrap()
                .command(),
            selected.manifest_command().unwrap(),
            selected.blob_command().unwrap(),
        ] {
            assert!(
                command
                    .environment
                    .iter()
                    .any(|(name, value)| { name == "GIT_NO_LAZY_FETCH" && value == "1" })
            );
        }
    }

    #[test]
    fn partial_clone_is_refused_before_any_object_read() {
        let (_root, git, repository, _selected) = fixture();
        assert!(
            Command::new(&git)
                .args([
                    "-C",
                    repository.to_str().unwrap(),
                    "config",
                    "core.repositoryformatversion",
                    "1",
                ])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new(&git)
                .args([
                    "-C",
                    repository.to_str().unwrap(),
                    "config",
                    "extensions.partialClone",
                    "origin",
                ])
                .status()
                .unwrap()
                .success()
        );
        let selection = RepositorySelection::open(&git, &repository).unwrap();
        assert!(matches!(
            reject_partial_clone(&selection.partial_clone_command()),
            Err(Error::PartialCloneUnsupported)
        ));
        assert!(
            Command::new(&git)
                .args([
                    "-C",
                    repository.to_str().unwrap(),
                    "config",
                    "extensions.partialClone",
                    "",
                ])
                .status()
                .unwrap()
                .success()
        );
        assert!(matches!(
            reject_partial_clone(&selection.partial_clone_command()),
            Err(Error::PartialCloneUnsupported)
        ));
        assert!(
            Command::new(&git)
                .args([
                    "-C",
                    repository.to_str().unwrap(),
                    "config",
                    "--unset",
                    "extensions.partialClone",
                ])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new(&git)
                .args([
                    "-C",
                    repository.to_str().unwrap(),
                    "config",
                    "remote.origin.promisor",
                    "false",
                ])
                .status()
                .unwrap()
                .success()
        );
        assert!(matches!(
            reject_partial_clone(&selection.partial_clone_command()),
            Err(Error::PartialCloneUnsupported)
        ));
    }

    #[test]
    fn manifest_rejects_gitlinks_links_and_git_paths_but_accepts_attributes() {
        let limits = SourceLimits::new(20, 4096).unwrap();
        let oid = "a".repeat(40);
        let link = format!("120000 blob {oid} 7\tlink\0");
        assert!(matches!(
            TreeManifest::parse(link.as_bytes(), limits),
            Err(Error::UnsupportedTreeEntry)
        ));
        let gitlink = format!("160000 commit {oid} -\tsubmodule\0");
        assert!(matches!(
            TreeManifest::parse(gitlink.as_bytes(), limits),
            Err(Error::UnsupportedTreeEntry)
        ));
        let attributes = format!("100644 blob {oid} 1\tsrc/.GITATTRIBUTES\0");
        assert!(TreeManifest::parse(attributes.as_bytes(), limits).is_ok());
        let git_path = format!("100644 blob {oid} 1\t.GIT/config\0");
        assert!(matches!(
            TreeManifest::parse(git_path.as_bytes(), limits),
            Err(Error::UnsafeTreePath(_))
        ));
    }

    #[test]
    fn quiescent_measurement_and_removal_are_exact_and_restart_safe() {
        let (root, staged, selection) = materialized_source();
        let change_id = change_id_from_record(&selection);
        let changes = root.path().join("changes");
        let staging_measurement = measure_quiescent_provisioning_source(
            &changes,
            change_id,
            &selection,
            SourceLimits::new(30, 8192).unwrap(),
        )
        .unwrap();
        let published = staged.publish().unwrap();
        assert_eq!(
            measure_quiescent_provisioning_source(
                &changes,
                change_id,
                &selection,
                SourceLimits::new(30, 8192).unwrap(),
            )
            .unwrap(),
            staging_measurement
        );
        symlink("tracked.txt", published.path.join("provider-link")).unwrap();
        let measured = measure_quiescent_source(
            &changes,
            change_id,
            published.identity,
            SourceLimits::new(30, 8192).unwrap(),
        )
        .unwrap();
        assert!(measured.entries > staging_measurement.entries);
        assert!(measured.logical_bytes > published.bytes);

        let quarantine = changes.join(format!(".removing-{}", change_id.simple()));
        fs::rename(&published.path, &quarantine).unwrap();
        assert_eq!(
            remove_quiescent_source(&changes, change_id, published.identity).unwrap(),
            RemovalOutcome::Removed
        );
        assert_eq!(
            remove_quiescent_source(&changes, change_id, published.identity).unwrap(),
            RemovalOutcome::AlreadyAbsent
        );
    }

    #[test]
    fn storage_measurement_uses_allocated_unique_inodes_and_logical_path_lengths() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let changes = root.path().join("changes");
        private_dir(&changes);
        let change_id = Uuid::new_v4();
        let source = changes.join(change_id.simple().to_string());
        private_dir(&source);

        const SPARSE_LENGTH: u64 = 16 * 1024 * 1024;
        const DATA_LENGTH: usize = 8192;
        let sparse = source.join("sparse");
        fs::File::create(&sparse)
            .unwrap()
            .set_len(SPARSE_LENGTH)
            .unwrap();
        let data = source.join("data");
        fs::write(&data, vec![b'x'; DATA_LENGTH]).unwrap();
        let duplicate = source.join("data-hardlink");
        fs::hard_link(&data, &duplicate).unwrap();
        let link = source.join("data-symlink");
        symlink("data", &link).unwrap();

        let source_metadata = fs::symlink_metadata(&source).unwrap();
        let sparse_metadata = fs::symlink_metadata(&sparse).unwrap();
        let data_metadata = fs::symlink_metadata(&data).unwrap();
        let duplicate_metadata = fs::symlink_metadata(&duplicate).unwrap();
        let link_metadata = fs::symlink_metadata(&link).unwrap();
        assert_eq!(data_metadata.ino(), duplicate_metadata.ino());
        assert_eq!(data_metadata.dev(), duplicate_metadata.dev());
        let expected_allocated = [
            source_metadata.blocks(),
            sparse_metadata.blocks(),
            data_metadata.blocks(),
            link_metadata.blocks(),
        ]
        .into_iter()
        .try_fold(0_u64, |bytes, blocks| {
            bytes.checked_add(blocks.checked_mul(STAT_BLOCK_BYTES)?)
        })
        .unwrap();
        let expected_logical =
            SPARSE_LENGTH + u64::try_from(DATA_LENGTH).unwrap() * 2 + link_metadata.len();
        let identity = directory_identity(&source).unwrap();
        let measured = measure_quiescent_source(
            &changes,
            change_id,
            identity,
            SourceLimits::new(10, 32 * 1024 * 1024).unwrap(),
        )
        .unwrap();

        assert_eq!(measured.entries, 4);
        assert_eq!(measured.logical_bytes, expected_logical);
        assert_eq!(measured.allocated_bytes, expected_allocated);
        if sparse_metadata.blocks() * STAT_BLOCK_BYTES < SPARSE_LENGTH {
            assert!(measured.allocated_bytes < measured.logical_bytes);
        }
        assert!(matches!(
            allocated_bytes_from_blocks(u64::MAX),
            Err(Error::StorageMeasurementOverflow)
        ));
    }

    #[test]
    fn removal_refuses_a_replaced_source_path() {
        let (root, staged, selection) = materialized_source();
        let change_id = change_id_from_record(&selection);
        let published = staged.publish().unwrap();
        let original = root.path().join("original-source");
        fs::rename(&published.path, &original).unwrap();
        let replacement = root.path().join("replacement-source");
        private_dir(&replacement);
        fs::write(replacement.join("keep"), "present").unwrap();
        symlink(&replacement, &published.path).unwrap();
        assert!(matches!(
            remove_quiescent_source(&root.path().join("changes"), change_id, published.identity,),
            Err(Error::InvalidDirectory(_))
        ));
        assert_eq!(
            fs::read_to_string(replacement.join("keep")).unwrap(),
            "present"
        );
        assert!(original.is_dir());
    }

    #[test]
    fn provisioning_removal_recovers_unrecorded_staging_and_claim_quarantine() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let changes = root.path().join("changes");
        private_dir(&changes);
        let change_id = Uuid::new_v4();
        assert_eq!(
            remove_provisioning_source(&changes, change_id, None).unwrap(),
            RemovalOutcome::AlreadyAbsent
        );

        let staging = changes.join(format!(".staging-{}", change_id.simple()));
        private_dir(&staging);
        fs::write(staging.join("partial"), "discard").unwrap();
        assert_eq!(
            remove_provisioning_source(&changes, change_id, None).unwrap(),
            RemovalOutcome::Removed
        );
        assert!(fs::symlink_metadata(&staging).is_err());

        let unclaimed = changes.join(format!(".unclaimed-{}", change_id.simple()));
        private_dir(&unclaimed);
        fs::write(unclaimed.join("partial"), "discard after rename").unwrap();
        assert_eq!(
            remove_provisioning_source(&changes, change_id, None).unwrap(),
            RemovalOutcome::Removed
        );
        assert!(fs::symlink_metadata(unclaimed).is_err());
    }

    #[test]
    fn unrecorded_removal_never_claims_a_final_or_unsafe_staging_path() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let changes = root.path().join("changes");
        private_dir(&changes);
        let change_id = Uuid::new_v4();
        let published = changes.join(change_id.simple().to_string());
        private_dir(&published);
        fs::write(published.join("keep"), "present").unwrap();
        assert!(matches!(
            remove_provisioning_source(&changes, change_id, None),
            Err(Error::PathExists(path)) if path == published
        ));
        assert_eq!(
            fs::read_to_string(published.join("keep")).unwrap(),
            "present"
        );

        fs::remove_dir_all(&published).unwrap();
        let staging = changes.join(format!(".staging-{}", change_id.simple()));
        private_dir(&staging);
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            remove_provisioning_source(&changes, change_id, None),
            Err(Error::UnsafePrivateDirectory(path)) if path == staging
        ));
        assert!(staging.is_dir());

        fs::set_permissions(&staging, fs::Permissions::from_mode(0o700)).unwrap();
        let unclaimed = changes.join(format!(".unclaimed-{}", change_id.simple()));
        private_dir(&unclaimed);
        assert!(matches!(
            remove_provisioning_source(&changes, change_id, None),
            Err(Error::AmbiguousMaterialization)
        ));
        assert!(staging.is_dir());
        assert!(unclaimed.is_dir());
    }

    #[test]
    fn checkpointed_provisioning_removal_handles_staging_publish_and_restart() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let changes = root.path().join("changes");
        private_dir(&changes);
        let checkpoints = changes.join(".checkpoints");
        private_dir(&checkpoints);

        let staging_id = Uuid::new_v4();
        let staging = changes.join(format!(".staging-{}", staging_id.simple()));
        private_dir(&staging);
        fs::write(staging.join("partial"), "discard").unwrap();
        let record = selection_for(&changes, staging_id, directory_identity(&staging).unwrap());
        let checkpoint = checkpoints.join("selection.json");
        write_selection_record(&checkpoint, &record).unwrap();
        assert_eq!(
            remove_provisioning_source(&changes, staging_id, Some(&record)).unwrap(),
            RemovalOutcome::Removed
        );
        assert_eq!(read_selection_record(&checkpoint).unwrap(), record);
        assert_eq!(
            remove_provisioning_source(&changes, staging_id, Some(&record)).unwrap(),
            RemovalOutcome::AlreadyAbsent
        );

        let published_id = Uuid::new_v4();
        let published_staging = changes.join(format!(".staging-{}", published_id.simple()));
        private_dir(&published_staging);
        let published_record = selection_for(
            &changes,
            published_id,
            directory_identity(&published_staging).unwrap(),
        );
        fs::rename(
            &published_staging,
            changes.join(published_id.simple().to_string()),
        )
        .unwrap();
        assert_eq!(
            remove_provisioning_source(&changes, published_id, Some(&published_record)).unwrap(),
            RemovalOutcome::Removed
        );

        let quarantined_id = Uuid::new_v4();
        let quarantined_staging = changes.join(format!(".staging-{}", quarantined_id.simple()));
        private_dir(&quarantined_staging);
        let quarantined_record = selection_for(
            &changes,
            quarantined_id,
            directory_identity(&quarantined_staging).unwrap(),
        );
        fs::rename(
            &quarantined_staging,
            changes.join(format!(".removing-{}", quarantined_id.simple())),
        )
        .unwrap();
        assert_eq!(
            remove_provisioning_source(&changes, quarantined_id, Some(&quarantined_record))
                .unwrap(),
            RemovalOutcome::Removed
        );
    }

    #[test]
    fn checkpointed_provisioning_removal_refuses_ambiguity_and_replacement() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let changes = root.path().join("changes");
        private_dir(&changes);
        let change_id = Uuid::new_v4();
        let staging = changes.join(format!(".staging-{}", change_id.simple()));
        private_dir(&staging);
        let record = selection_for(&changes, change_id, directory_identity(&staging).unwrap());
        let published = changes.join(change_id.simple().to_string());
        private_dir(&published);
        assert!(matches!(
            remove_provisioning_source(&changes, change_id, Some(&record)),
            Err(Error::AmbiguousMaterialization)
        ));
        assert!(staging.is_dir());
        assert!(published.is_dir());

        fs::remove_dir_all(&published).unwrap();
        let original = root.path().join("original-staging");
        fs::rename(&staging, &original).unwrap();
        private_dir(&staging);
        fs::write(staging.join("keep"), "replacement").unwrap();
        assert!(matches!(
            remove_provisioning_source(&changes, change_id, Some(&record)),
            Err(Error::AmbiguousMaterialization)
        ));
        assert_eq!(
            fs::read_to_string(staging.join("keep")).unwrap(),
            "replacement"
        );
        assert!(original.is_dir());

        fs::set_permissions(&staging, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            remove_provisioning_source(&changes, change_id, Some(&record)),
            Err(Error::UnsafePrivateDirectory(path)) if path == staging
        ));

        let mut wrong_path = record;
        wrong_path.staging_path = root.path().join("outside");
        assert!(matches!(
            remove_provisioning_source(&changes, change_id, Some(&wrong_path)),
            Err(Error::UnsafePath(_))
        ));
    }

    #[test]
    fn oid_parser_rejects_ref_names_and_extra_output() {
        assert!(serde_json::from_str::<GitOid>("\"main\"").is_err());
        assert!(serde_json::from_str::<GitOid>(&format!("\"{}\"", "A".repeat(40))).is_err());
        assert!(serde_json::from_str::<GitOid>(&format!("\"{}extra\"", "a".repeat(40))).is_err());
    }
}
