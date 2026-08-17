//! Small local probes shared by `update --install` (and, later, `init`/
//! `doctor`): where the provider CLIs live, and whether a daemon answers.

use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use factory_core::local::{LocalRequest, LocalResponse, ServerFrame};
use factoryctl::Client;

/// The provider CLIs a factory can run, and `git`, which worktrees need.
pub const PROBED_PROGRAMS: [&str; 3] = ["claude", "codex", "git"];

/// The first executable regular file named `program` on this process's `PATH`.
#[must_use]
pub fn locate_on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|directory| {
        let candidate = directory.join(program);
        let metadata = fs::metadata(&candidate).ok()?;
        (metadata.is_file() && metadata.mode() & 0o111 != 0).then_some(candidate)
    })
}

/// The directories [`PROBED_PROGRAMS`] resolve from on this process's `PATH`,
/// deduplicated, in probe order — what a launchd job's `PATH` must contain
/// for sessions to find them.
#[must_use]
pub fn provider_directories() -> Vec<PathBuf> {
    let mut directories = Vec::new();
    for program in PROBED_PROGRAMS {
        if let Some(directory) =
            locate_on_path(program).and_then(|path| path.parent().map(Path::to_path_buf))
        {
            if !directories.contains(&directory) {
                directories.push(directory);
            }
        }
    }
    directories
}

/// Polls `health` at `socket` until a daemon answers — with `version`
/// equal to `expected_version` when one is given, so a just-restarted job
/// is not mistaken for the daemon it replaced — or `timeout` elapses.
/// Returns the version reported, or the last error.
pub fn wait_for_daemon(
    socket: &Path,
    timeout: Duration,
    expected_version: Option<&str>,
) -> Result<String, String> {
    let client = Client::new(socket);
    let deadline = Instant::now() + timeout;
    loop {
        let last_error =
            match client.request_with_timeout(LocalRequest::Health, Duration::from_secs(2)) {
                Ok(ServerFrame::Response {
                    response: LocalResponse::Health { version, .. },
                    ..
                }) => match expected_version {
                    Some(expected) if version != expected => format!(
                        "a daemon answers, but it is {} rather than {expected}",
                        if version.is_empty() {
                            "an older version"
                        } else {
                            &version
                        }
                    ),
                    _ => return Ok(version),
                },
                Ok(_) => "unexpected reply to health".to_owned(),
                Err(error) => error.to_string(),
            };
        if Instant::now() >= deadline {
            return Err(last_error);
        }
        thread::sleep(Duration::from_millis(500));
    }
}
