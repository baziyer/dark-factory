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

/// Each of [`PROBED_PROGRAMS`] this process's `PATH` resolves, with the
/// directory it resolves from — what a launchd job's `PATH` must be able to
/// find for sessions to run them (see `launchd::merged_path`).
#[must_use]
pub fn provider_directories() -> Vec<(&'static str, PathBuf)> {
    PROBED_PROGRAMS
        .iter()
        .filter_map(|program| {
            locate_on_path(program)
                .and_then(|path| path.parent().map(Path::to_path_buf))
                .map(|directory| (*program, directory))
        })
        .collect()
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
