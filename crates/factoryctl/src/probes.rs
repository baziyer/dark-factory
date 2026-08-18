//! Small local probes shared by `init`, `doctor`, and `update --install`:
//! where the provider CLIs live, their versions, whether the launchd job is
//! loaded, and whether a daemon answers.

use std::{
    fs,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::Client;
use factory_core::local::{LocalRequest, LocalResponse, ServerFrame};

use crate::launchd;

const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

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

/// `<program> --version`, first line, within [`VERSION_PROBE_TIMEOUT`];
/// `None` if it fails, hangs, or prints nothing.
#[must_use]
pub fn probe_version(program: &Path) -> Option<String> {
    let mut child = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.take(4096).read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + VERSION_PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let text = reader.join().ok()?;
    let line = text.lines().next()?.trim();
    (status.is_some_and(|status| status.success()) && !line.is_empty()).then(|| line.to_owned())
}

/// Whether launchd has the job loaded for this user.
#[must_use]
pub fn launchd_loaded() -> bool {
    Command::new("launchctl")
        .args([
            "print",
            &format!(
                "gui/{}/{}",
                rustix::process::getuid().as_raw(),
                launchd::LABEL
            ),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Whether anything answers `health` at `socket` right now (one request,
/// no waiting).
#[must_use]
pub fn daemon_answers(socket: &Path) -> bool {
    Client::new(socket)
        .request_with_timeout(LocalRequest::Health, Duration::from_secs(2))
        .is_ok()
}

/// The Codex home agents seed from, given a launchd job's environment (or
/// none): `CODEX_HOME` from the job, else from this process, else
/// `<user_home>/.codex` — mirroring `CodexProvider::new`.
#[must_use]
pub fn codex_seed_home(
    job_environment: Option<&std::collections::BTreeMap<String, String>>,
    user_home: &Path,
) -> PathBuf {
    job_environment
        .and_then(|environment| environment.get("CODEX_HOME").cloned())
        .or_else(|| std::env::var("CODEX_HOME").ok())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| user_home.join(".codex"))
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
