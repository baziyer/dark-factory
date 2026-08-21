//! Per-attempt hook authentication token and the small filesystem helpers
//! shared by every provider's generated configuration
//! (`claude-settings.json`, Codex's seeded `config.toml`).
//!
//! A provider's hook subprocess (a `factoryctl hook` invocation spawned by
//! Claude Code or Codex itself, never the daemon) authenticates to the
//! daemon by reading this token file and sending its contents alongside the
//! hook payload; see `crates/factoryctl/src/main.rs`'s `hook` subcommand.
//! The daemon never puts the token on argv or in an environment variable —
//! only a trusted, private file path, matching the existing
//! `runner_process.rs` philosophy of never putting sensitive content on a
//! child's command line.

use std::{
    fs, io,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use uuid::Uuid;

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;
/// 32 random bytes, hex-encoded to 64 characters.
const HOOK_TOKEN_BYTES: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum HookTokenError {
    // `getrandom::Error` does not implement `std::error::Error` (it is a
    // thin `NonZeroRawOsError` wrapper), so it cannot be a `thiserror`
    // `#[source]`/`#[from]` field; its `Display` output is captured instead.
    #[error("cannot generate a random hook token: {0}")]
    Random(String),
    #[error("cannot write hook token file {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("invalid operator token file {path}: {reason}")]
    Invalid { path: PathBuf, reason: String },
}

/// Reads the existing operator credential or creates it exactly once.
/// Existing files must be regular files owned by this process's user, mode
/// `0600`, and contain exactly one 64-character lowercase hexadecimal token.
/// Symlinks and malformed or broadly-readable files fail closed.
///
/// # Errors
///
/// Returns [`HookTokenError::Random`] when token generation fails,
/// [`HookTokenError::Io`] for filesystem errors, or
/// [`HookTokenError::Invalid`] when an existing file is not private and
/// canonical.
pub fn read_or_create_operator_token(path: &Path) -> Result<String, HookTokenError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_private_dir(parent).map_err(|source| HookTokenError::Io {
        path: parent.to_path_buf(),
        source,
    })?;

    match open_operator_token(path) {
        Ok(file) => read_operator_token(path, file),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let token = random_hex().map_err(|error| HookTokenError::Random(error.to_string()))?;
            let created = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(PRIVATE_FILE_MODE)
                .custom_flags(nofollow_flag())
                .open(path);
            match created {
                Ok(mut file) => {
                    file.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))
                        .and_then(|()| file.write_all(token.as_bytes()))
                        .and_then(|()| file.sync_all())
                        .map_err(|source| HookTokenError::Io {
                            path: path.to_path_buf(),
                            source,
                        })?;
                    validate_operator_token_file(path, &file, &token)?;
                    Ok(token)
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let file = open_operator_token(path).map_err(|source| HookTokenError::Io {
                        path: path.to_path_buf(),
                        source,
                    })?;
                    read_operator_token(path, file)
                }
                Err(source) => Err(HookTokenError::Io {
                    path: path.to_path_buf(),
                    source,
                }),
            }
        }
        Err(source) => Err(HookTokenError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn open_operator_token(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(operator_read_flags())
        .open(path)
}

fn read_operator_token(path: &Path, mut file: fs::File) -> Result<String, HookTokenError> {
    validate_operator_token_metadata(path, &file)?;
    let mut token = String::new();
    file.read_to_string(&mut token)
        .map_err(|source| HookTokenError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    validate_operator_token_file(path, &file, &token)?;
    Ok(token)
}

fn validate_operator_token_file(
    path: &Path,
    file: &fs::File,
    token: &str,
) -> Result<(), HookTokenError> {
    validate_operator_token_metadata(path, file)?;
    let valid_token = token.len() == HOOK_TOKEN_BYTES * 2
        && token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    let reason = if !valid_token {
        Some("content is not 64 lowercase hexadecimal characters")
    } else {
        None
    };
    reason.map_or(Ok(()), |reason| {
        Err(HookTokenError::Invalid {
            path: path.to_path_buf(),
            reason: reason.to_owned(),
        })
    })
}

fn validate_operator_token_metadata(path: &Path, file: &fs::File) -> Result<(), HookTokenError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata().map_err(|source| HookTokenError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let reason = if !metadata.is_file() {
        Some("not a regular file")
    } else if metadata.uid() != rustix::process::geteuid().as_raw() {
        Some("not owned by the daemon user")
    } else if metadata.mode() & 0o777 != PRIVATE_FILE_MODE {
        Some("mode is not 0600")
    } else {
        None
    };
    reason.map_or(Ok(()), |reason| {
        Err(HookTokenError::Invalid {
            path: path.to_path_buf(),
            reason: reason.to_owned(),
        })
    })
}

fn random_hex() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; HOOK_TOKEN_BYTES];
    getrandom::fill(&mut bytes)?;
    let mut encoded = String::with_capacity(HOOK_TOKEN_BYTES * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(encoded)
}

/// Atomically overwrites a private file (mode `0600`) through a temp file
/// plus rename, creating its parent directory (mode `0700`) if needed.
/// Shared by the attempt credential, Claude's generated
/// `claude-settings.json`, and Codex's seeded `config.toml`.
///
/// # Errors
///
/// Returns any I/O error from creating the parent directory, writing the
/// temp file, or renaming it into place. The temp file is removed on a
/// write failure; nothing is ever left partially written at `path` itself,
/// since the final step is a single rename.
pub fn write_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_private_dir(parent)?;
    let temp_path = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("provider-config"),
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
        // `OpenOptions::mode` is filtered by the process umask when the
        // temporary file is created. Apply the explicit mode to the open
        // file afterward so callers that are preserving an existing file's
        // permissions are exactly private under any daemon umask.
        file.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
        file.write_all(contents)?;
        file.sync_all()
    })();
    if let Err(source) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(source);
    }
    fs::rename(&temp_path, path)
}

/// Ensures `path` is a private (mode `0700`) directory, creating it
/// (recursively) if missing. Errors if `path` exists but is a symlink or a
/// non-directory. Shared by [`write_private_file`]'s parent-directory
/// handling and by providers that need a directory to exist before writing
/// more than one file into it (e.g. an attempt's Codex home).
pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "must be a directory, not a symbolic link or file",
            ))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::DirBuilder::new()
            .recursive(true)
            .mode(PRIVATE_DIRECTORY_MODE)
            .create(path),
        Err(error) => Err(error),
    }
}

/// `O_NOFOLLOW`, so generated provider config is never written through a
/// symlink. The conversion is infallible in practice (the flag bit is far
/// below `i32::MAX` on every supported platform); falling back to `0` would
/// only drop the `NOFOLLOW` protection, never corrupt a write.
fn nofollow_flag() -> i32 {
    i32::try_from(rustix::fs::OFlags::NOFOLLOW.bits()).unwrap_or(0)
}

fn operator_read_flags() -> i32 {
    i32::try_from((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits()).unwrap_or(0)
}

/// Single-quotes `value` for safe inclusion in a POSIX shell command line
/// (both Claude's and Codex's `type = "command"` hook handlers run their
/// `command` string through a shell), escaping any embedded single quote as
/// `'\''`. Defensive: generated paths are daemon-controlled and unlikely to
/// contain shell metacharacters, but a private directory name is not
/// guaranteed to be free of spaces or quotes.
#[must_use]
pub fn shell_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for character in value.chars() {
        if character == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(character);
        }
    }
    quoted.push('\'');
    quoted
}

/// Builds the exact `factoryctl hook --token-file <path> <Event>` command
/// string embedded in generated provider hook configuration, with both
/// paths shell-quoted. Shared by Claude's `claude-settings.json` and
/// Codex's seeded `config.toml`.
#[must_use]
pub fn hook_command(
    factoryctl_path: &Path,
    hook_token_path: &Path,
    event: factory_core::ProviderHookEvent,
) -> String {
    format!(
        "{} hook --token-file {} {}",
        shell_quote(&factoryctl_path.to_string_lossy()),
        shell_quote(&hook_token_path.to_string_lossy()),
        event.provider_event_name(),
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn operator_token_is_stable_across_restarts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("operator.token");
        let first = read_or_create_operator_token(&path).unwrap();
        let second = read_or_create_operator_token(&path).unwrap();

        assert_eq!(first, second);
        assert_eq!(fs::read_to_string(&path).unwrap(), first);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn invalid_existing_operator_token_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("operator.token");
        fs::write(&path, "not-a-token").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        assert!(matches!(
            read_or_create_operator_token(&path),
            Err(HookTokenError::Invalid { .. })
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), "not-a-token");
    }

    #[test]
    fn broadly_readable_operator_token_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("operator.token");
        fs::write(&path, "a".repeat(HOOK_TOKEN_BYTES * 2)).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        assert!(matches!(
            read_or_create_operator_token(&path),
            Err(HookTokenError::Invalid { .. })
        ));
    }

    #[test]
    fn write_private_file_leaves_no_temp_file_behind() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        write_private_file(&path, b"{}").unwrap();
        let leftovers: Vec<_> = fs::read_dir(directory.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp file leaked: {leftovers:?}");
    }

    #[test]
    fn shell_quote_escapes_embedded_single_quotes() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(
            shell_quote("/it's/a path/with space"),
            "'/it'\\''s/a path/with space'"
        );
    }

    #[test]
    fn hook_command_quotes_both_paths_and_uses_the_authority_event_name() {
        let command = hook_command(
            Path::new("/abs/factoryctl"),
            Path::new("/abs/runs/attempt-1/hook.token"),
            factory_core::ProviderHookEvent::PreToolUse,
        );
        assert_eq!(
            command,
            "'/abs/factoryctl' hook --token-file '/abs/runs/attempt-1/hook.token' PreToolUse"
        );
    }
}
