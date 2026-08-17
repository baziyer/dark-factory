//! Per-session hook authentication token, and the small filesystem helpers
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
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use uuid::Uuid;

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;
/// 32 random bytes, hex-encoded to 64 characters — matches `sessions
/// .hook_token`'s `CHECK (length(hook_token) = 64 AND hook_token GLOB
/// '[0-9a-f]*')` constraint.
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
}

/// Generates a fresh 32-byte (64 lowercase hex character) hook token and
/// writes it to `path` at mode `0600`, creating the parent directory (mode
/// `0700`) if needed. Returns the token so the caller can also persist it as
/// `sessions.hook_token` for the daemon's own side of the comparison.
///
/// # Errors
///
/// Returns [`HookTokenError::Random`] if the platform RNG is unavailable, or
/// [`HookTokenError::Io`] if the file cannot be written.
pub fn write_hook_token(path: &Path) -> Result<String, HookTokenError> {
    let token = random_hex().map_err(|error| HookTokenError::Random(error.to_string()))?;
    write_private_file(path, token.as_bytes()).map_err(|source| HookTokenError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(token)
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
/// Shared by the hook token, Claude's generated `claude-settings.json`, and
/// Codex's seeded `config.toml`.
///
/// # Errors
///
/// Returns any I/O error from creating the parent directory, writing the
/// temp file, or renaming it into place. The temp file is removed on a
/// write failure; nothing is ever left partially written at `path` itself,
/// since the final step is a single rename.
pub fn write_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    write_file_with_mode(path, contents, PRIVATE_FILE_MODE)
}

/// Same atomic temp-file-plus-rename write as [`write_private_file`], but
/// with an explicit file `mode` instead of the fixed `0600` private-file
/// default. Used by [`crate::providers::claude::pretrust_worktree`] to
/// rewrite the operator's real `~/.claude.json` in place without silently
/// tightening its existing permissions -- that file is not a Dark Factory
/// generated artifact, unlike every other caller of [`write_private_file`].
///
/// # Errors
///
/// See [`write_private_file`].
pub fn write_file_with_mode(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
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
            .mode(mode)
            .custom_flags(nofollow_flag())
            .open(&temp_path)?;
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
/// more than one file into it (e.g. Codex's per-agent `codex-home`).
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

/// The eight hook events Dark Factory wires into every generated provider
/// configuration, in the order the daemon's state machine reasons about
/// them (see `TRACK5-DESIGN.md` sections 2 and 3): boot, delivery
/// acknowledgement, tool activity (start/finish), idle-wait, turn-complete,
/// subagent-complete, and process shutdown. Codex additionally wires one
/// more, Codex-only event (`PermissionRequest`) on top of this shared set
/// -- see `providers::codex::CODEX_ONLY_HOOK_EVENTS`.
pub const HOOK_EVENTS: [factory_core::ProviderHookEvent; 8] = [
    factory_core::ProviderHookEvent::SessionStart,
    factory_core::ProviderHookEvent::UserPromptSubmit,
    factory_core::ProviderHookEvent::PreToolUse,
    factory_core::ProviderHookEvent::PostToolUse,
    factory_core::ProviderHookEvent::Notification,
    factory_core::ProviderHookEvent::Stop,
    factory_core::ProviderHookEvent::SubagentStop,
    factory_core::ProviderHookEvent::SessionEnd,
];

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use super::*;

    #[test]
    fn write_hook_token_is_64_lowercase_hex_characters_at_mode_0600() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested").join("hook.token");
        let token = write_hook_token(&path).unwrap();

        assert_eq!(token.len(), 64);
        assert!(
            token.bytes().all(|byte| byte.is_ascii_hexdigit()
                && (byte.is_ascii_digit() || byte.is_ascii_lowercase()))
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), token);

        let file_metadata = fs::metadata(&path).unwrap();
        assert_eq!(file_metadata.permissions().mode() & 0o777, 0o600);
        let directory_metadata = fs::metadata(path.parent().unwrap()).unwrap();
        assert_eq!(directory_metadata.permissions().mode() & 0o777, 0o700);
        assert_eq!(file_metadata.uid(), rustix::process::geteuid().as_raw());
    }

    #[test]
    fn write_hook_token_generates_a_different_token_each_call() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hook.token");
        let first = write_hook_token(&path).unwrap();
        let second = write_hook_token(&path).unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read_to_string(&path).unwrap(), second);
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
    fn hook_command_quotes_both_paths_and_uses_the_exact_event_name() {
        let command = hook_command(
            Path::new("/abs/factoryctl"),
            Path::new("/abs/runs/session-1/hook.token"),
            factory_core::ProviderHookEvent::SubagentStop,
        );
        assert_eq!(
            command,
            "'/abs/factoryctl' hook --token-file '/abs/runs/session-1/hook.token' SubagentStop"
        );
    }
}
