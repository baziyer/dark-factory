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
    fs, io,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use factory_core::{AgentId, ProjectId, paths};
use uuid::Uuid;

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;

/// Maximum size of one guidance file (`PROJECT.md`, `instructions.md`, or
/// `memory.md`), enforced on both read and write.
pub const MAX_GUIDANCE_FILE_BYTES: usize = 16 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum GuidanceError {
    #[error("cannot prepare guidance directory {path}: {source}")]
    Directory { path: PathBuf, source: io::Error },
    #[error("cannot prepare guidance file {path}: {source}")]
    File { path: PathBuf, source: io::Error },
    #[error("guidance file {path} exceeds {MAX_GUIDANCE_FILE_BYTES} bytes")]
    TooLarge { path: PathBuf },
    #[error("guidance file {path} is not valid UTF-8")]
    NotUtf8 { path: PathBuf },
    #[error("guidance text must not contain control characters other than newline and tab")]
    InvalidText,
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
/// `memory.md`, and the reserved worktree/codex-home/claude-settings paths
/// under it), if present. A missing directory is not an error: callers use
/// this best-effort after the owning `DeleteAgent` transaction has already
/// committed, so the ledger row is gone either way.
pub fn remove_agent(
    home: &Path,
    project_id: &ProjectId,
    agent_id: &AgentId,
) -> Result<(), GuidanceError> {
    remove_dir(&paths::agent_dir(home, project_id, agent_id))
}

/// Recursively removes one project's guidance directory (`PROJECT.md` and
/// every agent directory under it), if present. Best-effort, for the same
/// reason as [`remove_agent`]: called after `DeleteProject` has already
/// committed.
pub fn remove_project(home: &Path, project_id: &ProjectId) -> Result<(), GuidanceError> {
    remove_dir(&paths::project_dir(home, project_id))
}

fn remove_dir(path: &Path) -> Result<(), GuidanceError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(GuidanceError::Directory {
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
    let bytes = fs::read(path).map_err(|source| GuidanceError::File {
        path: path.to_path_buf(),
        source,
    })?;
    if bytes.len() > MAX_GUIDANCE_FILE_BYTES {
        return Err(GuidanceError::TooLarge {
            path: path.to_path_buf(),
        });
    }
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
    ensure_dir(parent)?;
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
    })
}

fn ensure_dir(path: &Path) -> Result<(), GuidanceError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(GuidanceError::Directory {
                path: path.to_path_buf(),
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "must be a directory, not a symbolic link or file",
                ),
            })
        }
        Ok(_) => Ok(()),
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
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_dir(parent)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(GuidanceError::File {
                path: path.to_path_buf(),
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "must be a regular file, not a symbolic link or directory",
                ),
            })
        }
        Ok(_) => Ok(()),
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
    use std::os::unix::fs::{MetadataExt, symlink};

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
