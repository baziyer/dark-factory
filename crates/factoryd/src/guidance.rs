//! File-backed project and agent guidance under `$DARK_FACTORY_HOME/projects`.
//!
//! This is the sister folder to the SQLite ledger: `PROJECT.md`,
//! `instructions.md`, and `memory.md` are plain, owner-private files an
//! operator or the agent itself can open directly with `$EDITOR`, rather than
//! opaque database columns. SQLite stays the durable ledger for
//! projects/agents/tasks/runs/events/messages; this module only manages the
//! files themselves. Paths are computed by `factory_core::paths`.
//!
//! Directories are created private-by-construction at mode `0700` and files
//! at mode `0600`, matching the daemon's existing state-directory and socket
//! rules (see `lifecycle.rs`). Files are read with a bounded size and written
//! atomically through a temp file plus rename.

use std::{
    fs,
    io::{self, Read as _, Write as _},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use factory_core::{
    AgentId, ProjectId,
    local::{GuidanceHealth, GuidanceHealthState},
    paths,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub use factory_core::local::MAX_GUIDANCE_FILE_BYTES;

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;

/// Dispatch compacts memory before it reaches this high-water mark. The
/// active projection is strictly smaller than this value, leaving room for a
/// later lesson before the next safe dispatch.
pub const MEMORY_COMPACTION_HIGH_WATER_BYTES: usize = 12 * 1024;
const MEMORY_COMPACTION_TARGET_BYTES: usize = MEMORY_COMPACTION_HIGH_WATER_BYTES - 1;
pub const MEMORY_NEAR_LIMIT_BYTES: usize = 14 * 1024;
pub const MAX_MEMORY_ARCHIVES: usize = 8;
const MEMORY_ARCHIVE_DIR_NAME: &str = "memory-archive";

#[derive(Debug, thiserror::Error)]
pub enum GuidanceError {
    #[error("cannot prepare guidance directory {path}: {source}")]
    Directory { path: PathBuf, source: io::Error },
    #[error("cannot remove guidance directory {path}: {source}")]
    Remove { path: PathBuf, source: io::Error },
    #[error("cannot prepare guidance file {path}: {source}")]
    File { path: PathBuf, source: io::Error },
    #[error("guidance file {path} exceeds {MAX_GUIDANCE_FILE_BYTES} bytes")]
    TooLarge { path: PathBuf },
    #[error("guidance file {path} is not valid UTF-8")]
    NotUtf8 { path: PathBuf },
    #[error("guidance text must not contain control characters other than newline and tab")]
    InvalidText,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuidanceInspection {
    pub content: Option<String>,
    pub health: GuidanceHealth,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryCompaction {
    pub archive_path: PathBuf,
    pub bytes_before: usize,
    pub bytes_after: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileFingerprint {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

struct OpenedSource {
    file: fs::File,
    fingerprint: FileFingerprint,
}

struct CapturedSource {
    temporary: PathBuf,
    fingerprint: FileFingerprint,
    bytes: u64,
    digest: [u8; 32],
    projection: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ArchiveEntry {
    path: PathBuf,
    sequence: u64,
    digest: [u8; 32],
    bytes: u64,
}

/// Idempotently creates one project's guidance directory and empty
/// `PROJECT.md`.
pub fn ensure_project(home: &Path, project_id: &ProjectId) -> Result<(), GuidanceError> {
    ensure_file(&paths::project_guidance_path(home, project_id))
}

/// Idempotently creates one agent's guidance directory, empty
/// `instructions.md`, and empty `memory.md`.
pub fn ensure_agent(
    home: &Path,
    project_id: &ProjectId,
    agent_id: &AgentId,
) -> Result<(), GuidanceError> {
    ensure_file(&paths::agent_instructions_path(home, project_id, agent_id))?;
    ensure_file(&paths::agent_memory_path(home, project_id, agent_id))
}

/// Recursively removes one agent's guidance directory (`instructions.md`,
/// `memory.md`, `codex-home/`, `claude-settings.json`, and the outbox
/// under it), if present. A missing directory is not an error. Callers
/// (`local_api::delete_agent_locked`) run this *before* the owning
/// `DeleteAgent` transaction, under `execution::Handle::begin_delete`'s
/// guarantee that no concurrent writer can be recreating files here, and
/// surface a failure as the request's own error rather than logging and
/// proceeding (AGENTS.md rule 3) -- this is no longer best-effort.
pub fn remove_agent(
    home: &Path,
    project_id: &ProjectId,
    agent_id: &AgentId,
) -> Result<(), GuidanceError> {
    remove_dir(&paths::agent_dir(home, project_id, agent_id))
}

/// Recursively removes one project's guidance directory (`PROJECT.md` and
/// every agent directory under it), if present. See [`remove_agent`] for
/// ordering and error-handling: the same applies here, one level up.
pub fn remove_project(home: &Path, project_id: &ProjectId) -> Result<(), GuidanceError> {
    remove_dir(&paths::project_dir(home, project_id))
}

/// The private directory where exact pre-compaction memory snapshots live.
/// It is deliberately outside the active file's prompt path and is bounded by
/// [`MAX_MEMORY_ARCHIVES`].
#[must_use]
pub fn memory_archive_path(memory_path: &Path) -> PathBuf {
    memory_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(MEMORY_ARCHIVE_DIR_NAME)
}

/// Reads one guidance file without ever returning oversized or invalid text.
/// The daemon uses this for mechanical status, where a bad guidance file is a
/// health observation rather than a failed agent lookup.
pub fn inspect(path: &Path) -> GuidanceInspection {
    let max_bytes = u64::try_from(MAX_GUIDANCE_FILE_BYTES).unwrap_or(u64::MAX);
    let path_error = |error: GuidanceError| GuidanceInspection {
        content: None,
        health: GuidanceHealth {
            state: GuidanceHealthState::PathError,
            bytes: 0,
            max_bytes,
            detail: Some(bound_detail(error.to_string())),
        },
    };
    if let Err(error) = ensure_file(path) {
        return path_error(error);
    }
    let source = match open_private(path) {
        Ok(source) => source,
        Err(error) => return path_error(error),
    };
    let (raw, bytes) = match read_opened_bounded(path, source, MAX_GUIDANCE_FILE_BYTES) {
        Ok(BoundedRead::Content { bytes, length }) => (bytes, length),
        Ok(BoundedRead::Oversized { length }) => {
            return GuidanceInspection {
                content: None,
                health: health_u64(GuidanceHealthState::Oversized, length, None),
            };
        }
        Err(error) => return path_error(error),
    };
    match String::from_utf8(raw) {
        Ok(content) => {
            let state = if bytes >= MEMORY_NEAR_LIMIT_BYTES as u64 {
                GuidanceHealthState::NearLimit
            } else {
                GuidanceHealthState::Ok
            };
            GuidanceInspection {
                content: Some(content),
                health: health_u64(state, bytes, None),
            }
        }
        Err(_) => GuidanceInspection {
            content: None,
            health: health_u64(GuidanceHealthState::InvalidUtf8, bytes, None),
        },
    }
}

#[must_use]
pub fn health_for_valid_bytes(bytes: usize) -> GuidanceHealth {
    let state = if bytes >= MEMORY_NEAR_LIMIT_BYTES {
        GuidanceHealthState::NearLimit
    } else {
        GuidanceHealthState::Ok
    };
    health(state, bytes, None)
}

/// Compacts an active memory file only at the safe no-live-session dispatch
/// boundary. The archive is written and synced before the active projection is
/// atomically replaced, so a crash between those steps is repaired by the
/// next call without creating a second copy of the same source bytes.
pub fn compact_memory(path: &Path) -> Result<Option<MemoryCompaction>, GuidanceError> {
    compact_memory_inner(path, None)
}

#[cfg(test)]
fn compact_memory_with_hook(
    path: &Path,
    after_archive: impl FnOnce() -> Result<(), GuidanceError> + 'static,
) -> Result<Option<MemoryCompaction>, GuidanceError> {
    compact_memory_inner(path, Some(Box::new(after_archive)))
}

fn compact_memory_inner(
    path: &Path,
    after_archive: Option<Box<dyn FnOnce() -> Result<(), GuidanceError>>>,
) -> Result<Option<MemoryCompaction>, GuidanceError> {
    validate_guidance_path(path)?;
    regular_metadata(path)?;
    let archive_dir = memory_archive_path(path);
    ensure_private_dir(&archive_dir)?;
    let captured = capture_source(path, &archive_dir)?;
    if captured.bytes <= MEMORY_COMPACTION_HIGH_WATER_BYTES as u64 {
        remove_temporary(&captured.temporary)?;
        return Ok(None);
    }
    let bytes_before = usize::try_from(captured.bytes).unwrap_or(usize::MAX);
    let archives = list_archives(&archive_dir)?;
    let digest = captured.digest;
    let existing_candidate = archives
        .iter()
        .find(|entry| entry.bytes == captured.bytes && entry.digest == digest);
    let existing = match existing_candidate {
        Some(entry) if archive_matches(&entry.path, captured.bytes, digest)? => Some(entry),
        _ => None,
    };
    let (archive_path, archives) = if let Some(existing) = existing {
        remove_temporary(&captured.temporary)?;
        (existing.path.clone(), archives)
    } else {
        let sequence = archives
            .last()
            .map_or(1, |entry| entry.sequence.saturating_add(1));
        let archive_path =
            archive_dir.join(format!("memory-{sequence:020}-{}.bin", hex_digest(&digest)));
        install_archive(
            &captured.temporary,
            &archive_path,
            &archive_dir,
            captured.bytes,
            digest,
        )?;
        let mut archives = archives;
        archives.push(ArchiveEntry {
            path: archive_path.clone(),
            sequence,
            digest,
            bytes: captured.bytes,
        });
        archives.sort_by_key(|entry| entry.sequence);
        (archive_path, archives)
    };
    if let Some(after_archive) = after_archive {
        after_archive()?;
    }
    validate_source_unchanged(path, captured.fingerprint)?;
    rotate_archives(&archives, &archive_path)?;
    replace_memory(path, &captured.projection)?;
    Ok(Some(MemoryCompaction {
        archive_path,
        bytes_before,
        bytes_after: captured.projection.len(),
    }))
}

fn health(state: GuidanceHealthState, bytes: usize, detail: Option<String>) -> GuidanceHealth {
    health_u64(state, u64::try_from(bytes).unwrap_or(u64::MAX), detail)
}

fn health_u64(state: GuidanceHealthState, bytes: u64, detail: Option<String>) -> GuidanceHealth {
    GuidanceHealth {
        state,
        bytes,
        max_bytes: u64::try_from(MAX_GUIDANCE_FILE_BYTES).unwrap_or(u64::MAX),
        detail: detail.map(bound_detail),
    }
}

fn bound_detail(detail: String) -> String {
    detail.chars().take(256).collect()
}

fn regular_metadata(path: &Path) -> Result<std::fs::Metadata, GuidanceError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| GuidanceError::File {
        path: path.to_path_buf(),
        source,
    })?;
    validate_regular_file(path, &metadata)?;
    Ok(metadata)
}

enum BoundedRead {
    Content { bytes: Vec<u8>, length: u64 },
    Oversized { length: u64 },
}

fn open_private(path: &Path) -> Result<OpenedSource, GuidanceError> {
    validate_guidance_path(path)?;
    let before = fs::symlink_metadata(path).map_err(|source| GuidanceError::File {
        path: path.to_path_buf(),
        source,
    })?;
    validate_regular_file(path, &before)?;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nofollow_flag())
        .open(path)
        .map_err(|source| GuidanceError::File {
            path: path.to_path_buf(),
            source,
        })?;
    let opened = file.metadata().map_err(|source| GuidanceError::File {
        path: path.to_path_buf(),
        source,
    })?;
    validate_regular_file(path, &opened)?;
    let before_fingerprint = fingerprint(&before);
    let opened_fingerprint = fingerprint(&opened);
    if before_fingerprint != opened_fingerprint {
        return Err(changed_file(path));
    }
    Ok(OpenedSource {
        file,
        fingerprint: opened_fingerprint,
    })
}

fn validate_guidance_path(path: &Path) -> Result<(), GuidanceError> {
    validate_path_syntax(path)?;
    let depth = managed_parent_depth(path);
    let mut parent = path.parent();
    for _ in 0..depth {
        let parent_path = parent.ok_or_else(|| invalid_path(path, "missing guidance parent"))?;
        let metadata =
            fs::symlink_metadata(parent_path).map_err(|source| GuidanceError::Directory {
                path: parent_path.to_path_buf(),
                source,
            })?;
        validate_private_directory(parent_path, &metadata)?;
        parent = parent_path.parent();
    }
    Ok(())
}

fn validate_existing_guidance_parents(path: &Path) -> Result<(), GuidanceError> {
    validate_path_syntax(path)?;
    let depth = managed_parent_depth(path);
    let mut parent = path.parent();
    for _ in 0..depth {
        let parent_path = parent.ok_or_else(|| invalid_path(path, "missing guidance parent"))?;
        match fs::symlink_metadata(parent_path) {
            Ok(metadata) => validate_private_directory(parent_path, &metadata)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(GuidanceError::Directory {
                    path: parent_path.to_path_buf(),
                    source,
                });
            }
        }
        parent = parent_path.parent();
    }
    Ok(())
}

fn managed_parent_depth(path: &Path) -> usize {
    match path.file_name().and_then(|name| name.to_str()) {
        Some("PROJECT.md") => 3,
        Some("instructions.md" | "memory.md") => 5,
        _ => 1,
    }
}

fn validate_path_syntax(path: &Path) -> Result<(), GuidanceError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(invalid_path(
            path,
            "guidance path must be absolute and contain no ..",
        ));
    }
    Ok(())
}

fn invalid_path(path: &Path, message: &str) -> GuidanceError {
    GuidanceError::File {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidInput, message),
    }
}

fn validate_private_directory(
    path: &Path,
    metadata: &std::fs::Metadata,
) -> Result<(), GuidanceError> {
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o7777 != PRIVATE_DIRECTORY_MODE
    {
        return Err(GuidanceError::Directory {
            path: path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::PermissionDenied,
                "guidance directory must be a private owner-only non-symlink directory",
            ),
        });
    }
    Ok(())
}

fn ensure_private_dir(path: &Path) -> Result<(), GuidanceError> {
    validate_existing_guidance_parents(path)?;
    ensure_dir(path)?;
    let metadata = fs::symlink_metadata(path).map_err(|source| GuidanceError::Directory {
        path: path.to_path_buf(),
        source,
    })?;
    validate_private_directory(path, &metadata)
}

fn sync_directory(path: &Path) -> Result<(), GuidanceError> {
    ensure_private_dir(path)?;
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| GuidanceError::Directory {
            path: path.to_path_buf(),
            source,
        })
}

fn read_opened_bounded(
    path: &Path,
    mut source: OpenedSource,
    maximum: usize,
) -> Result<BoundedRead, GuidanceError> {
    let mut bytes = Vec::with_capacity(maximum.saturating_add(1).min(8192));
    std::io::Read::by_ref(&mut source.file)
        .take(maximum as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| GuidanceError::File {
            path: path.to_path_buf(),
            source,
        })?;
    let after = source
        .file
        .metadata()
        .map_err(|source| GuidanceError::File {
            path: path.to_path_buf(),
            source,
        })?;
    validate_regular_file(path, &after)?;
    let after_fingerprint = fingerprint(&after);
    let current = fs::symlink_metadata(path).map_err(|source| GuidanceError::File {
        path: path.to_path_buf(),
        source,
    })?;
    validate_regular_file(path, &current)?;
    if after_fingerprint != fingerprint(&current) {
        return Err(changed_file(path));
    }
    if after_fingerprint != source.fingerprint || after.len() != bytes.len() as u64 {
        return if after.len() > maximum as u64 {
            Ok(BoundedRead::Oversized {
                length: after.len(),
            })
        } else {
            Err(changed_file(path))
        };
    }
    if bytes.len() > maximum {
        Ok(BoundedRead::Oversized {
            length: after.len(),
        })
    } else {
        Ok(BoundedRead::Content {
            length: after.len(),
            bytes,
        })
    }
}

fn validate_regular_file(path: &Path, metadata: &std::fs::Metadata) -> Result<(), GuidanceError> {
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o7777 != PRIVATE_FILE_MODE
    {
        return Err(GuidanceError::File {
            path: path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::PermissionDenied,
                "guidance file must be a private owner-only regular file",
            ),
        });
    }
    Ok(())
}

fn fingerprint(metadata: &std::fs::Metadata) -> FileFingerprint {
    FileFingerprint {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        modified: (metadata.mtime(), metadata.mtime_nsec()),
        changed: (metadata.ctime(), metadata.ctime_nsec()),
    }
}

fn changed_file(path: &Path) -> GuidanceError {
    GuidanceError::File {
        path: path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::InvalidData,
            "guidance file changed during read",
        ),
    }
}

fn capture_source(path: &Path, archive_dir: &Path) -> Result<CapturedSource, GuidanceError> {
    let source = open_private(path)?;
    let source_fingerprint = source.fingerprint;
    let temporary = archive_dir.join(format!(".capture-{}.tmp", Uuid::new_v4()));
    let result = (|| -> io::Result<CapturedSource> {
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(PRIVATE_FILE_MODE)
            .custom_flags(nofollow_flag())
            .open(&temporary)?;
        let mut input = source.file;
        let mut digest = Sha256::new();
        let mut tail =
            std::collections::VecDeque::with_capacity(MEMORY_COMPACTION_TARGET_BYTES + 1);
        let mut buffer = [0u8; 8192];
        let mut bytes = 0u64;
        loop {
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            output.write_all(&buffer[..count])?;
            digest.update(&buffer[..count]);
            bytes = bytes.saturating_add(count as u64);
            for byte in &buffer[..count] {
                if tail.len() == MEMORY_COMPACTION_TARGET_BYTES + 1 {
                    tail.pop_front();
                }
                tail.push_back(*byte);
            }
        }
        output.sync_all()?;
        let after = input.metadata()?;
        let current = fs::symlink_metadata(path).map_err(io::Error::other)?;
        validate_regular_file(path, &current).map_err(io::Error::other)?;
        if fingerprint(&after) != source_fingerprint
            || fingerprint(&current) != source_fingerprint
            || bytes != after.len()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "guidance file changed during compaction",
            ));
        }
        let raw: Vec<u8> = tail.into_iter().collect();
        let projection = complete_tail_projection(&raw);
        Ok(CapturedSource {
            temporary: temporary.clone(),
            fingerprint: source_fingerprint,
            bytes,
            digest: digest.finalize().into(),
            projection,
        })
    })();
    match result {
        Ok(captured) => Ok(captured),
        Err(source) => {
            let _ = fs::remove_file(&temporary);
            Err(GuidanceError::File {
                path: path.to_path_buf(),
                source,
            })
        }
    }
}

fn complete_tail_projection(raw: &[u8]) -> Vec<u8> {
    let complete = if raw.len() > MEMORY_COMPACTION_TARGET_BYTES {
        if raw[0] == b'\n' {
            &raw[1..]
        } else {
            let Some(first_newline) = raw[1..].iter().position(|byte| *byte == b'\n') else {
                return Vec::new();
            };
            &raw[first_newline + 2..]
        }
    } else {
        raw
    };
    let Some(last_newline) = complete.iter().rposition(|byte| *byte == b'\n') else {
        return Vec::new();
    };
    String::from_utf8_lossy(&complete[..last_newline + 1])
        .into_owned()
        .into_bytes()
}

fn validate_source_unchanged(path: &Path, expected: FileFingerprint) -> Result<(), GuidanceError> {
    let current = fs::symlink_metadata(path).map_err(|source| GuidanceError::File {
        path: path.to_path_buf(),
        source,
    })?;
    validate_regular_file(path, &current)?;
    if fingerprint(&current) != expected {
        return Err(changed_file(path));
    }
    Ok(())
}

fn hex_digest(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn remove_temporary(path: &Path) -> Result<(), GuidanceError> {
    fs::remove_file(path).map_err(|source| GuidanceError::File {
        path: path.to_path_buf(),
        source,
    })
}

fn install_archive(
    temporary: &Path,
    archive: &Path,
    directory: &Path,
    expected_bytes: u64,
    expected_digest: [u8; 32],
) -> Result<(), GuidanceError> {
    match fs::hard_link(temporary, archive) {
        Ok(()) => {
            sync_directory(directory)?;
            remove_temporary(temporary)
        }
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
            if archive_matches(archive, expected_bytes, expected_digest)? {
                remove_temporary(temporary)
            } else {
                let _ = fs::remove_file(temporary);
                Err(GuidanceError::File {
                    path: archive.to_path_buf(),
                    source,
                })
            }
        }
        Err(source) => {
            let _ = fs::remove_file(temporary);
            Err(GuidanceError::File {
                path: archive.to_path_buf(),
                source,
            })
        }
    }
}

fn archive_matches(
    path: &Path,
    expected_bytes: u64,
    expected_digest: [u8; 32],
) -> Result<bool, GuidanceError> {
    let source = open_private(path)?;
    let expected_fingerprint = source.fingerprint;
    let mut file = source.file;
    let mut digest = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 8192];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|source| GuidanceError::File {
                path: path.to_path_buf(),
                source,
            })?;
        if count == 0 {
            break;
        }
        bytes = bytes.saturating_add(count as u64);
        digest.update(&buffer[..count]);
    }
    let after = file.metadata().map_err(|source| GuidanceError::File {
        path: path.to_path_buf(),
        source,
    })?;
    let current = fs::symlink_metadata(path).map_err(|source| GuidanceError::File {
        path: path.to_path_buf(),
        source,
    })?;
    validate_regular_file(path, &current)?;
    if fingerprint(&after) != expected_fingerprint || fingerprint(&current) != expected_fingerprint
    {
        return Err(changed_file(path));
    }
    let actual_digest: [u8; 32] = digest.finalize().into();
    Ok(bytes == expected_bytes && actual_digest == expected_digest)
}

fn list_archives(directory: &Path) -> Result<Vec<ArchiveEntry>, GuidanceError> {
    let mut archives = Vec::new();
    for entry in fs::read_dir(directory).map_err(|source| GuidanceError::Directory {
        path: directory.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| GuidanceError::Directory {
            path: directory.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("memory-") || !name.ends_with(".bin") {
            continue;
        }
        let metadata = validate_archive_file(&path)?;
        let Some((sequence, digest)) = parse_archive_name(&name) else {
            continue;
        };
        archives.push(ArchiveEntry {
            path,
            sequence,
            digest,
            bytes: metadata.len(),
        });
    }
    archives.sort_by_key(|entry| entry.sequence);
    Ok(archives)
}

fn parse_archive_name(name: &str) -> Option<(u64, [u8; 32])> {
    let body = name.strip_prefix("memory-")?.strip_suffix(".bin")?;
    let (sequence, digest) = body.split_once('-')?;
    if sequence.is_empty() || digest.len() != 64 {
        return None;
    }
    let mut parsed = [0u8; 32];
    for (index, chunk) in digest.as_bytes().chunks_exact(2).enumerate() {
        parsed[index] = u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
    }
    Some((sequence.parse().ok()?, parsed))
}

fn validate_archive_file(path: &Path) -> Result<std::fs::Metadata, GuidanceError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| GuidanceError::File {
        path: path.to_path_buf(),
        source,
    })?;
    validate_regular_file(path, &metadata)?;
    Ok(metadata)
}

fn replace_memory(path: &Path, content: &[u8]) -> Result<(), GuidanceError> {
    validate_guidance_path(path)?;
    regular_metadata(path)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_dir(parent)?;
    let temporary = parent.join(format!(".memory.tmp-{}", Uuid::new_v4()));
    let result = (|| -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(PRIVATE_FILE_MODE)
            .custom_flags(nofollow_flag())
            .open(&temporary)?;
        file.write_all(content)?;
        file.sync_all()
    })();
    if let Err(source) = result {
        let _ = fs::remove_file(&temporary);
        return Err(GuidanceError::File {
            path: path.to_path_buf(),
            source,
        });
    }
    fs::rename(&temporary, path).map_err(|source| GuidanceError::File {
        path: path.to_path_buf(),
        source,
    })?;
    sync_directory(parent).map_err(|error| match error {
        GuidanceError::Directory { source, .. } => GuidanceError::File {
            path: path.to_path_buf(),
            source,
        },
        other => other,
    })
}

fn rotate_archives(archives: &[ArchiveEntry], preserve: &Path) -> Result<(), GuidanceError> {
    let remove_count = archives.len().saturating_sub(MAX_MEMORY_ARCHIVES);
    let removable = archives
        .iter()
        .filter(|entry| entry.path != preserve)
        .take(remove_count);
    for entry in removable {
        fs::remove_file(&entry.path).map_err(|source| GuidanceError::File {
            path: entry.path.clone(),
            source,
        })?;
    }
    if remove_count > 0 {
        sync_directory(
            preserve
                .parent()
                .ok_or_else(|| invalid_path(preserve, "archive has no parent"))?,
        )?;
    }
    Ok(())
}

fn remove_dir(path: &Path) -> Result<(), GuidanceError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(GuidanceError::Remove {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Reads a guidance file, lazily creating an empty private file (and its
/// parent directories) if it does not exist yet. Bounded to
/// [`MAX_GUIDANCE_FILE_BYTES`].
pub fn read_or_create(path: &Path) -> Result<String, GuidanceError> {
    ensure_file(path)?;
    let source = open_private(path)?;
    let bytes = match read_opened_bounded(path, source, MAX_GUIDANCE_FILE_BYTES)? {
        BoundedRead::Content { bytes, .. } => bytes,
        BoundedRead::Oversized { .. } => {
            return Err(GuidanceError::TooLarge {
                path: path.to_path_buf(),
            });
        }
    };
    String::from_utf8(bytes).map_err(|_| GuidanceError::NotUtf8 {
        path: path.to_path_buf(),
    })
}

/// Atomically overwrites a guidance file through a temp file plus rename,
/// creating parent directories if needed. Bounded to
/// [`MAX_GUIDANCE_FILE_BYTES`]; rejects control characters other than
/// newline and tab.
pub fn write(path: &Path, text: &str) -> Result<(), GuidanceError> {
    if text.len() > MAX_GUIDANCE_FILE_BYTES {
        return Err(GuidanceError::TooLarge {
            path: path.to_path_buf(),
        });
    }
    if text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        return Err(GuidanceError::InvalidText);
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    validate_existing_guidance_parents(path)?;
    ensure_dir(parent)?;
    validate_guidance_path(path)?;
    let temp_path = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("guidance"),
        Uuid::new_v4()
    ));
    let write_result = (|| -> io::Result<()> {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(PRIVATE_FILE_MODE)
            .custom_flags(nofollow_flag())
            .open(&temp_path)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()
    })();
    if let Err(source) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(GuidanceError::File {
            path: temp_path,
            source,
        });
    }
    fs::rename(&temp_path, path).map_err(|source| GuidanceError::File {
        path: path.to_path_buf(),
        source,
    })?;
    sync_directory(parent).map_err(|error| match error {
        GuidanceError::Directory { source, .. } => GuidanceError::File {
            path: path.to_path_buf(),
            source,
        },
        other => other,
    })
}

fn ensure_dir(path: &Path) -> Result<(), GuidanceError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_private_directory(path, &metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::DirBuilder::new()
            .recursive(true)
            .mode(PRIVATE_DIRECTORY_MODE)
            .create(path)
            .map_err(|source| GuidanceError::Directory {
                path: path.to_path_buf(),
                source,
            }),
        Err(source) => Err(GuidanceError::Directory {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn ensure_file(path: &Path) -> Result<(), GuidanceError> {
    validate_existing_guidance_parents(path)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_dir(parent)?;
    validate_guidance_path(path)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_regular_file(path, &metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(PRIVATE_FILE_MODE)
                .custom_flags(nofollow_flag())
                .open(path)
            {
                Ok(_) => Ok(()),
                // Another connection lazily created it between our check and
                // our create; that is fine, the file now exists either way.
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
                Err(source) => Err(GuidanceError::File {
                    path: path.to_path_buf(),
                    source,
                }),
            }
        }
        Err(source) => Err(GuidanceError::File {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// `O_NOFOLLOW`, so guidance files are never opened through a symlink.
///
/// The conversion is infallible in practice (the flag bit is far below
/// `i32::MAX` on every supported platform); falling back to `0` would only
/// drop the `NOFOLLOW` protection, never corrupt a read or write.
fn nofollow_flag() -> i32 {
    i32::try_from(rustix::fs::OFlags::NOFOLLOW.bits()).unwrap_or(0)
}

/// The current process's effective owner, used only in tests to assert the
/// daemon never creates guidance files owned by anyone else.
#[cfg(test)]
fn effective_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    use factory_core::paths;

    use super::*;

    fn ids() -> (ProjectId, AgentId) {
        (
            ProjectId::try_from("factory").unwrap(),
            AgentId::try_from("god").unwrap(),
        )
    }

    #[test]
    fn ensure_project_creates_a_private_empty_file_idempotently() {
        let home = tempfile::tempdir().unwrap();
        let (project, _) = ids();
        ensure_project(home.path(), &project).unwrap();
        let path = paths::project_guidance_path(home.path(), &project);
        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(metadata.mode() & 0o777, PRIVATE_FILE_MODE);
        assert_eq!(metadata.uid(), effective_uid());
        assert_eq!(fs::read_to_string(&path).unwrap(), "");

        let directory_metadata = fs::metadata(path.parent().unwrap()).unwrap();
        assert_eq!(directory_metadata.mode() & 0o777, PRIVATE_DIRECTORY_MODE);

        // Idempotent: a second call does not fail or truncate real content.
        write(&path, "keep me").unwrap();
        ensure_project(home.path(), &project).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "keep me");
    }

    #[test]
    fn ensure_agent_creates_instructions_and_memory() {
        let home = tempfile::tempdir().unwrap();
        let (project, agent) = ids();
        ensure_agent(home.path(), &project, &agent).unwrap();
        assert_eq!(
            fs::read_to_string(paths::agent_instructions_path(
                home.path(),
                &project,
                &agent
            ))
            .unwrap(),
            ""
        );
        assert_eq!(
            fs::read_to_string(paths::agent_memory_path(home.path(), &project, &agent)).unwrap(),
            ""
        );
    }

    #[test]
    fn read_or_create_lazily_creates_missing_files() {
        let home = tempfile::tempdir().unwrap();
        let (project, _) = ids();
        let path = paths::project_guidance_path(home.path(), &project);
        assert!(fs::symlink_metadata(&path).is_err());
        let content = read_or_create(&path).unwrap();
        assert_eq!(content, "");
        assert!(fs::symlink_metadata(&path).is_ok());
    }

    #[test]
    fn write_then_read_round_trips_and_is_atomic() {
        let home = tempfile::tempdir().unwrap();
        let (project, _) = ids();
        let path = paths::project_guidance_path(home.path(), &project);
        write(&path, "# Project\n\nBuild the thing.\n").unwrap();
        assert_eq!(
            read_or_create(&path).unwrap(),
            "# Project\n\nBuild the thing.\n"
        );
        // No leftover temp files.
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp file leaked: {leftovers:?}");
    }

    #[test]
    fn write_rejects_oversized_text() {
        let home = tempfile::tempdir().unwrap();
        let (project, _) = ids();
        let path = paths::project_guidance_path(home.path(), &project);
        let oversized = "x".repeat(MAX_GUIDANCE_FILE_BYTES + 1);
        assert!(matches!(
            write(&path, &oversized),
            Err(GuidanceError::TooLarge { .. })
        ));
    }

    #[test]
    fn write_rejects_control_characters() {
        let home = tempfile::tempdir().unwrap();
        let (project, _) = ids();
        let path = paths::project_guidance_path(home.path(), &project);
        assert!(matches!(
            write(&path, "hello\u{0007}world"),
            Err(GuidanceError::InvalidText)
        ));
    }

    #[test]
    fn read_or_create_rejects_a_file_that_exceeds_the_bound() {
        let home = tempfile::tempdir().unwrap();
        let (project, _) = ids();
        let path = paths::project_guidance_path(home.path(), &project);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "x".repeat(MAX_GUIDANCE_FILE_BYTES + 1)).unwrap();
        assert!(matches!(
            read_or_create(&path),
            Err(GuidanceError::TooLarge { .. })
        ));
    }

    #[test]
    fn inspect_reports_exact_and_one_byte_overflow_without_returning_content() {
        let home = tempfile::tempdir().unwrap();
        let (project, agent) = ids();
        let path = paths::agent_memory_path(home.path(), &project, &agent);
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        fs::write(&path, vec![b'x'; MAX_GUIDANCE_FILE_BYTES]).unwrap();
        let exact = inspect(&path);
        assert_eq!(exact.health.state, GuidanceHealthState::NearLimit);
        assert_eq!(exact.health.bytes, MAX_GUIDANCE_FILE_BYTES as u64);
        assert!(exact.content.is_some());

        fs::write(&path, vec![b'x'; MAX_GUIDANCE_FILE_BYTES + 1]).unwrap();
        let overflow = inspect(&path);
        assert_eq!(overflow.health.state, GuidanceHealthState::Oversized);
        assert_eq!(overflow.health.bytes, (MAX_GUIDANCE_FILE_BYTES + 1) as u64);
        assert!(overflow.content.is_none());
    }

    #[test]
    fn inspect_reports_near_limit_and_invalid_utf8_as_bounded_health() {
        let home = tempfile::tempdir().unwrap();
        let (project, agent) = ids();
        let path = paths::agent_memory_path(home.path(), &project, &agent);
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        fs::write(&path, vec![b'x'; MEMORY_NEAR_LIMIT_BYTES]).unwrap();
        let near = inspect(&path);
        assert_eq!(near.health.state, GuidanceHealthState::NearLimit);
        assert_eq!(near.health.bytes, MEMORY_NEAR_LIMIT_BYTES as u64);
        assert!(near.content.is_some());

        fs::write(&path, [b'a', 0xff, b'\n']).unwrap();
        let invalid = inspect(&path);
        assert_eq!(invalid.health.state, GuidanceHealthState::InvalidUtf8);
        assert_eq!(invalid.health.bytes, 3);
        assert!(invalid.content.is_none());
    }

    #[test]
    fn compaction_has_explicit_complete_line_tail_semantics() {
        let home = tempfile::tempdir().unwrap();
        let (project, agent) = ids();
        let path = paths::agent_memory_path(home.path(), &project, &agent);
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        fs::write(&path, "x".repeat(MAX_GUIDANCE_FILE_BYTES + 1)).unwrap();
        compact_memory(&path).unwrap().unwrap();
        assert!(fs::read(&path).unwrap().is_empty());

        fs::write(&path, format!("head\n{}\nunterminated", "é".repeat(8_000))).unwrap();
        compact_memory(&path).unwrap().unwrap();
        assert!(fs::read(&path).unwrap().is_empty());

        fs::write(&path, format!("head\n{}\nlast\n", "é".repeat(8_000))).unwrap();
        compact_memory(&path).unwrap().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "last\n");
    }

    #[test]
    fn exact_compaction_high_water_is_left_untouched() {
        let home = tempfile::tempdir().unwrap();
        let (project, agent) = ids();
        let path = paths::agent_memory_path(home.path(), &project, &agent);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let source = "x".repeat(MEMORY_COMPACTION_HIGH_WATER_BYTES);
        fs::write(&path, &source).unwrap();

        assert!(compact_memory(&path).unwrap().is_none());
        assert_eq!(fs::read_to_string(&path).unwrap(), source);
    }

    #[test]
    fn existing_parent_and_file_modes_must_remain_private() {
        let home = tempfile::tempdir().unwrap();
        let (project, agent) = ids();
        let path = paths::agent_memory_path(home.path(), &project, &agent);
        ensure_agent(home.path(), &project, &agent).unwrap();

        fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            inspect(&path).health.state,
            GuidanceHealthState::PathError
        ));
        fs::set_permissions(
            path.parent().unwrap(),
            fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE),
        )
        .unwrap();

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(inspect(&path).health.state, GuidanceHealthState::PathError);
    }

    #[test]
    fn managed_parent_symlink_substitution_is_rejected() {
        let home = tempfile::tempdir().unwrap();
        let (project, agent) = ids();
        let project_dir = paths::project_dir(home.path(), &project);
        let target = home.path().join("target-project");
        fs::create_dir_all(project_dir.parent().unwrap()).unwrap();
        fs::create_dir_all(&target).unwrap();
        symlink(&target, &project_dir).unwrap();
        let path = paths::agent_memory_path(home.path(), &project, &agent);

        assert_eq!(inspect(&path).health.state, GuidanceHealthState::PathError);
    }

    #[test]
    fn opened_inode_reads_bound_append_overflow_and_reject_replacement() {
        let home = tempfile::tempdir().unwrap();
        let (project, agent) = ids();
        let path = paths::agent_memory_path(home.path(), &project, &agent);
        ensure_agent(home.path(), &project, &agent).unwrap();
        fs::write(&path, "x".repeat(MAX_GUIDANCE_FILE_BYTES)).unwrap();

        let source = open_private(&path).unwrap();
        let mut append = fs::OpenOptions::new().append(true).open(&path).unwrap();
        append.write_all(b"!").unwrap();
        let result = read_opened_bounded(&path, source, MAX_GUIDANCE_FILE_BYTES).unwrap();
        assert!(matches!(result, BoundedRead::Oversized { .. }));

        let source = open_private(&path).unwrap();
        let replacement = path.with_extension("replacement");
        fs::write(&replacement, "replacement").unwrap();
        fs::rename(&replacement, &path).unwrap();
        assert!(matches!(
            read_opened_bounded(&path, source, MAX_GUIDANCE_FILE_BYTES),
            Err(GuidanceError::File { .. })
        ));
    }

    #[test]
    fn compaction_preserves_recent_utf8_lines_and_private_exact_archive() {
        let home = tempfile::tempdir().unwrap();
        let (project, agent) = ids();
        let path = paths::agent_memory_path(home.path(), &project, &agent);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let source = format!("old lesson\n{}\nlatest lesson\n", "é".repeat(8_500));
        assert!(source.len() > MAX_GUIDANCE_FILE_BYTES);
        fs::write(&path, &source).unwrap();

        let result = compact_memory(&path).unwrap().unwrap();
        let active = fs::read(&path).unwrap();
        assert!(active.len() <= MEMORY_COMPACTION_TARGET_BYTES);
        assert!(std::str::from_utf8(&active).is_ok());
        assert!(active.ends_with(b"\n"));
        assert_eq!(fs::read(&result.archive_path).unwrap(), source.as_bytes());
        assert_eq!(
            fs::metadata(&result.archive_path).unwrap().mode() & 0o777,
            PRIVATE_FILE_MODE
        );
        assert_eq!(
            fs::metadata(result.archive_path.parent().unwrap())
                .unwrap()
                .mode()
                & 0o777,
            PRIVATE_DIRECTORY_MODE
        );
    }

    #[test]
    fn compaction_is_crash_idempotent_and_does_not_duplicate_archives() {
        let home = tempfile::tempdir().unwrap();
        let (project, agent) = ids();
        let path = paths::agent_memory_path(home.path(), &project, &agent);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let source = format!("lesson\n{}", "x".repeat(MAX_GUIDANCE_FILE_BYTES));
        fs::write(&path, &source).unwrap();

        let failure = compact_memory_with_hook(&path, || {
            Err(GuidanceError::File {
                path: PathBuf::from("test crash"),
                source: io::Error::other("simulated crash"),
            })
        });
        assert!(failure.is_err());
        assert_eq!(fs::read(&path).unwrap(), source.as_bytes());
        let archive_dir = memory_archive_path(&path);
        let archives: Vec<_> = fs::read_dir(&archive_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(archives.len(), 1);
        assert_eq!(fs::read(&archives[0]).unwrap(), source.as_bytes());

        let repaired = compact_memory(&path).unwrap().unwrap();
        assert!(repaired.bytes_after <= MEMORY_COMPACTION_TARGET_BYTES);
        assert_eq!(fs::read(&repaired.archive_path).unwrap(), source.as_bytes());
        assert!(compact_memory(&path).unwrap().is_none());
        assert_eq!(fs::read_dir(&archive_dir).unwrap().count(), 1);
    }

    #[test]
    fn archive_digest_collision_never_overwrites_or_reuses_wrong_bytes() {
        let home = tempfile::tempdir().unwrap();
        let (project, agent) = ids();
        let path = paths::agent_memory_path(home.path(), &project, &agent);
        ensure_agent(home.path(), &project, &agent).unwrap();
        let source = format!("lesson\n{}", "x".repeat(MAX_GUIDANCE_FILE_BYTES));
        fs::write(&path, &source).unwrap();

        let archive_dir = memory_archive_path(&path);
        ensure_private_dir(&archive_dir).unwrap();
        let digest: [u8; 32] = Sha256::digest(source.as_bytes()).into();
        let collision = archive_dir.join(format!("memory-{0:020}-{1}.bin", 1, hex_digest(&digest)));
        let wrong = vec![b'z'; source.len()];
        fs::write(&collision, &wrong).unwrap();
        fs::set_permissions(&collision, fs::Permissions::from_mode(PRIVATE_FILE_MODE)).unwrap();

        let result = compact_memory(&path).unwrap().unwrap();
        assert_ne!(result.archive_path, collision);
        assert_eq!(fs::read(&collision).unwrap(), wrong);
        assert_eq!(fs::read(&result.archive_path).unwrap(), source.as_bytes());
    }

    #[test]
    fn archive_rotation_stays_bounded() {
        let home = tempfile::tempdir().unwrap();
        let (project, agent) = ids();
        let path = paths::agent_memory_path(home.path(), &project, &agent);
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        for index in 0..(MAX_MEMORY_ARCHIVES + 3) {
            fs::write(
                &path,
                format!("iteration {index}\n{}", "x".repeat(MAX_GUIDANCE_FILE_BYTES)),
            )
            .unwrap();
            compact_memory(&path).unwrap().unwrap();
            assert!(
                fs::read_dir(memory_archive_path(&path)).unwrap().count() <= MAX_MEMORY_ARCHIVES
            );
        }
        assert_eq!(
            fs::read_dir(memory_archive_path(&path)).unwrap().count(),
            MAX_MEMORY_ARCHIVES
        );
    }

    #[test]
    fn compaction_rejects_active_archive_and_archive_entry_symlinks() {
        let home = tempfile::tempdir().unwrap();
        let (project, agent) = ids();
        let path = paths::agent_memory_path(home.path(), &project, &agent);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, vec![b'x'; MAX_GUIDANCE_FILE_BYTES]).unwrap();

        let active_target = home.path().join("active-target");
        fs::write(&active_target, vec![b'x'; MAX_GUIDANCE_FILE_BYTES]).unwrap();
        fs::remove_file(&path).unwrap();
        symlink(&active_target, &path).unwrap();
        assert!(matches!(
            compact_memory(&path),
            Err(GuidanceError::File { .. })
        ));

        fs::remove_file(&path).unwrap();
        fs::write(&path, vec![b'x'; MAX_GUIDANCE_FILE_BYTES]).unwrap();
        let archive_dir = memory_archive_path(&path);
        let archive_target = home.path().join("archive-target");
        fs::create_dir(&archive_target).unwrap();
        symlink(&archive_target, &archive_dir).unwrap();
        assert!(matches!(
            compact_memory(&path),
            Err(GuidanceError::Directory { .. })
        ));

        fs::remove_file(&archive_dir).unwrap();
        fs::create_dir(&archive_dir).unwrap();
        fs::set_permissions(&archive_dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            compact_memory(&path),
            Err(GuidanceError::Directory { .. })
        ));
        fs::set_permissions(
            &archive_dir,
            fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE),
        )
        .unwrap();
        let entry_target = home.path().join("entry-target");
        fs::write(&entry_target, "not an archive").unwrap();
        symlink(&entry_target, archive_dir.join("memory-old.bin")).unwrap();
        assert!(matches!(
            compact_memory(&path),
            Err(GuidanceError::File { .. })
        ));
    }

    #[test]
    fn remove_agent_deletes_the_agent_directory_and_is_idempotent() {
        let home = tempfile::tempdir().unwrap();
        let (project, agent) = ids();
        ensure_agent(home.path(), &project, &agent).unwrap();
        let agent_dir = paths::agent_dir(home.path(), &project, &agent);
        assert!(agent_dir.is_dir());

        remove_agent(home.path(), &project, &agent).unwrap();
        assert!(!agent_dir.exists());
        // Missing directory is not an error (best-effort cleanup).
        remove_agent(home.path(), &project, &agent).unwrap();
    }

    #[test]
    fn remove_project_deletes_the_project_directory_including_agents() {
        let home = tempfile::tempdir().unwrap();
        let (project, agent) = ids();
        ensure_project(home.path(), &project).unwrap();
        ensure_agent(home.path(), &project, &agent).unwrap();
        let project_dir = paths::project_dir(home.path(), &project);
        assert!(project_dir.is_dir());

        remove_project(home.path(), &project).unwrap();
        assert!(!project_dir.exists());
        // Missing directory is not an error (best-effort cleanup).
        remove_project(home.path(), &project).unwrap();
    }

    #[test]
    fn ensure_file_refuses_to_follow_a_symlink() {
        let home = tempfile::tempdir().unwrap();
        let (project, _) = ids();
        let path = paths::project_guidance_path(home.path(), &project);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let target = home.path().join("elsewhere.md");
        fs::write(&target, "not private").unwrap();
        symlink(&target, &path).unwrap();
        assert!(matches!(
            read_or_create(&path),
            Err(GuidanceError::File { .. })
        ));
    }
}
