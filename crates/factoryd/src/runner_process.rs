//! Safe process boundary between the daemon and the stable runner.

use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use factory_core::{RunId, RunnerInstanceId, runner::MAX_STARTUP_STDIN_BYTES};
use tokio::{
    io::AsyncWriteExt,
    process::{Child, Command},
};

const SAFE_ENVIRONMENT_NAMES: [&str; 9] = [
    "HOME", "USER", "LOGNAME", "SHELL", "PATH", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE",
];

/// Everything needed to launch one provider command under `factory-runner`.
///
/// This deliberately has no `Debug` or `Clone` implementation because it owns
/// the task bytes.
pub struct LaunchSpec {
    /// Trusted absolute path to the installed stable runner executable.
    pub runner_program: PathBuf,
    /// Provider executable path or name to resolve from the captured `PATH`.
    pub provider_program: PathBuf,
    /// Non-secret provider flags. These are observable in process metadata and
    /// must never contain task content, credentials, or tokens.
    pub provider_arguments: Vec<OsString>,
    pub run_id: RunId,
    pub runner_instance_id: RunnerInstanceId,
    pub runtime_dir: PathBuf,
    pub cwd: PathBuf,
    pub startup_input: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("startup input is {actual_bytes} bytes; maximum is {maximum_bytes}")]
    StartupInputTooLarge {
        actual_bytes: usize,
        maximum_bytes: usize,
    },
    #[error("runner executable path {program:?} must be absolute")]
    RunnerPathNotAbsolute { program: PathBuf },
    #[error("{role} executable {program:?} was not found")]
    ExecutableNotFound {
        role: &'static str,
        program: PathBuf,
    },
    #[error("{role} executable {program:?} is not an executable regular file")]
    NotExecutable {
        role: &'static str,
        program: PathBuf,
    },
    #[error("could not spawn factory-runner: {0}")]
    Spawn(std::io::Error),
    #[error("could not write factory-runner startup input: {0}")]
    StartupInput(std::io::Error),
    #[error("factory-runner did not consume startup input before the deadline")]
    StartupInputTimedOut,
}

struct StartupChild {
    child: Option<Child>,
}

impl StartupChild {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("startup child is present")
    }

    async fn kill_and_reap(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }

    fn into_child(mut self) -> Child {
        self.child.take().expect("startup child is present")
    }
}

impl Drop for StartupChild {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

struct CapturedEnvironment {
    values: Vec<(&'static str, OsString)>,
}

impl CapturedEnvironment {
    fn capture() -> Self {
        Self::from_lookup(|name| env::var_os(name))
    }

    fn from_lookup(mut lookup: impl FnMut(&str) -> Option<OsString>) -> Self {
        let values = SAFE_ENVIRONMENT_NAMES
            .into_iter()
            .filter_map(|name| lookup(name).map(|value| (name, value)))
            .collect();
        Self { values }
    }

    fn value(&self, name: &str) -> Option<&OsStr> {
        self.values
            .iter()
            .find_map(|(candidate, value)| (*candidate == name).then_some(value.as_os_str()))
    }
}

/// Starts one stable runner after checking its trusted absolute path and
/// resolving the provider to a canonical executable file.
///
/// Only `HOME`, `USER`, `LOGNAME`, `SHELL`, `PATH`, `TMPDIR`, `LANG`, `LC_ALL`,
/// and `LC_CTYPE` cross the daemon boundary when present. The child also gets
/// fixed non-interactive values for `NO_COLOR`, `TERM`, and
/// `GIT_TERMINAL_PROMPT`. Task bytes are bounded by
/// [`MAX_STARTUP_STDIN_BYTES`], written only to the runner's piped stdin, and
/// the pipe is closed before this function returns.
///
/// The returned child retains Tokio's default no-kill-on-drop behavior. Its
/// caller owns subsequent observation and reaping; dropping it must not stop an
/// independently running agent.
///
/// # Errors
///
/// Returns an error before spawning when task bytes are oversized or either
/// executable is missing or unusable. A spawn or startup-input write failure is
/// also returned; a spawned child is explicitly killed and reaped after a write
/// failure or timeout. Cancellation synchronously kills a child that has not
/// yet received its complete input; the stable runner cannot have launched the
/// provider at that point.
pub async fn spawn_runner(spec: LaunchSpec, startup_timeout: Duration) -> Result<Child, Error> {
    spawn_runner_with_environment_and_timeout(
        spec,
        CapturedEnvironment::capture(),
        Some(startup_timeout),
    )
    .await
}

#[cfg(test)]
async fn spawn_runner_with_environment(
    spec: LaunchSpec,
    environment: CapturedEnvironment,
) -> Result<Child, Error> {
    spawn_runner_with_environment_and_timeout(spec, environment, None).await
}

async fn spawn_runner_with_environment_and_timeout(
    spec: LaunchSpec,
    environment: CapturedEnvironment,
    startup_timeout: Option<Duration>,
) -> Result<Child, Error> {
    if spec.startup_input.len() > MAX_STARTUP_STDIN_BYTES {
        return Err(Error::StartupInputTooLarge {
            actual_bytes: spec.startup_input.len(),
            maximum_bytes: MAX_STARTUP_STDIN_BYTES,
        });
    }

    if !spec.runner_program.is_absolute() {
        return Err(Error::RunnerPathNotAbsolute {
            program: spec.runner_program,
        });
    }
    let runner = checked_executable(&spec.runner_program, "runner")?;
    let provider = resolve_executable(&spec.provider_program, &environment, "provider")?;
    let input_length = spec.startup_input.len().to_string();
    let mut command = Command::new(runner);
    command
        .arg("--run-id")
        .arg(spec.run_id.as_str())
        .arg("--runner-instance-id")
        .arg(spec.runner_instance_id.as_str())
        .arg("--runtime-dir")
        .arg(spec.runtime_dir)
        .arg("--cwd")
        .arg(spec.cwd)
        .arg("--stdin-bytes")
        .arg(input_length)
        .arg("--")
        .arg(provider)
        .args(spec.provider_arguments)
        .stdin(Stdio::piped());
    apply_runner_environment(&mut command, &environment);

    let mut child = StartupChild::new(command.spawn().map_err(Error::Spawn)?);
    let mut stdin = child
        .child_mut()
        .stdin
        .take()
        .expect("factory-runner was configured with piped stdin");
    let write_result = match startup_timeout {
        Some(limit) => {
            match tokio::time::timeout(limit, stdin.write_all(&spec.startup_input)).await {
                Ok(result) => result,
                Err(_) => {
                    drop(stdin);
                    child.kill_and_reap().await;
                    return Err(Error::StartupInputTimedOut);
                }
            }
        }
        None => stdin.write_all(&spec.startup_input).await,
    };
    if let Err(error) = write_result {
        drop(stdin);
        child.kill_and_reap().await;
        return Err(Error::StartupInput(error));
    }
    drop(stdin);
    Ok(child.into_child())
}

fn apply_runner_environment(command: &mut Command, environment: &CapturedEnvironment) {
    command.env_clear();
    for (name, value) in &environment.values {
        command.env(name, value);
    }
    command
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("GIT_TERMINAL_PROMPT", "0");
}

fn resolve_executable(
    program: &Path,
    environment: &CapturedEnvironment,
    role: &'static str,
) -> Result<PathBuf, Error> {
    if program.as_os_str().as_bytes().contains(&b'/') {
        return checked_executable(program, role);
    }

    let Some(path) = environment.value("PATH") else {
        return Err(Error::ExecutableNotFound {
            role,
            program: program.to_owned(),
        });
    };
    let mut unusable = None;
    for directory in env::split_paths(path) {
        let candidate = directory.join(program);
        match checked_executable(&candidate, role) {
            Ok(executable) => return Ok(executable),
            Err(Error::NotExecutable { .. }) => unusable = Some(candidate),
            Err(Error::ExecutableNotFound { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    if let Some(program) = unusable {
        Err(Error::NotExecutable { role, program })
    } else {
        Err(Error::ExecutableNotFound {
            role,
            program: program.to_owned(),
        })
    }
}

fn checked_executable(program: &Path, role: &'static str) -> Result<PathBuf, Error> {
    let canonical = match fs::canonicalize(program) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::ExecutableNotFound {
                role,
                program: program.to_owned(),
            });
        }
        Err(_) => {
            return Err(Error::NotExecutable {
                role,
                program: program.to_owned(),
            });
        }
    };
    let metadata = fs::metadata(&canonical).map_err(|_| Error::NotExecutable {
        role,
        program: program.to_owned(),
    })?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(Error::NotExecutable {
            role,
            program: program.to_owned(),
        });
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use std::{
        env,
        ffi::OsString,
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::Stdio,
        time::{Duration, Instant},
    };

    use factory_core::{RunId, RunnerInstanceId, runner::MAX_STARTUP_STDIN_BYTES};
    use rustix::process::{Pid, test_kill_process};
    use tokio::process::Command;

    use super::{
        CapturedEnvironment, LaunchSpec, SAFE_ENVIRONMENT_NAMES, apply_runner_environment,
        resolve_executable, spawn_runner_with_environment,
        spawn_runner_with_environment_and_timeout,
    };

    fn id<T>(value: &str) -> T
    where
        T: TryFrom<String>,
        T::Error: std::fmt::Debug,
    {
        T::try_from(value.to_owned()).unwrap()
    }

    fn environment(path: OsString, temporary: &Path) -> CapturedEnvironment {
        CapturedEnvironment::from_lookup(|name| match name {
            "HOME" => Some(OsString::from("/safe/home")),
            "USER" => Some(OsString::from("safe-user")),
            "SHELL" => Some(OsString::from("/bin/sh")),
            "PATH" => Some(path.clone()),
            "TMPDIR" => Some(temporary.as_os_str().to_owned()),
            "LANG" => Some(OsString::from("C")),
            "LC_ALL" => Some(OsString::from("C")),
            "LC_CTYPE" => Some(OsString::from("C")),
            "OPENAI_API_KEY" => Some(OsString::from("openai-secret-sentinel")),
            "ANTHROPIC_API_KEY" => Some(OsString::from("anthropic-secret-sentinel")),
            "CODEX_ACCESS_TOKEN" => Some(OsString::from("codex-secret-sentinel")),
            "CLAUDE_CODE_OAUTH_TOKEN" => Some(OsString::from("claude-secret-sentinel")),
            "GOOGLE_API_KEY" => Some(OsString::from("google-secret-sentinel")),
            "VERCEL_TOKEN" => Some(OsString::from("vercel-secret-sentinel")),
            "DARK_FACTORY_WEBHOOK_SECRET" => Some(OsString::from("webhook-secret-sentinel")),
            "DARK_FACTORY_TASK" => Some(OsString::from("private task sentinel")),
            "SSH_AUTH_SOCK" => Some(OsString::from("/secret/agent.sock")),
            "HTTPS_PROXY" => Some(OsString::from("https://secret-proxy.example")),
            "DYLD_INSERT_LIBRARIES" => Some(OsString::from("/secret/library.dylib")),
            "LD_PRELOAD" => Some(OsString::from("/secret/library.so")),
            _ => None,
        })
    }

    fn probe_environment(bin: &Path, temporary: &Path) -> CapturedEnvironment {
        let path = env::join_paths([bin, Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
        environment(path, temporary)
    }

    fn executable(path: &Path, source: &str) {
        fs::write(path, source).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn scripts(directory: &Path) {
        executable(
            &directory.join("runner-probe"),
            r#"#!/bin/sh
set -eu
: > "$TMPDIR/runner-argv"
for item in "$@"; do
    printf '%s\n' "$item" >> "$TMPDIR/runner-argv"
done
while [ "$#" -gt 0 ]; do
    case "$1" in
        --run-id|--runner-instance-id|--runtime-dir|--cwd|--stdin-bytes) shift 2 ;;
        --) shift; break ;;
        *) exit 64 ;;
    esac
done
exec "$@"
"#,
        );
        executable(
            &directory.join("provider-probe"),
            r#"#!/bin/sh
set -eu
cat > "$TMPDIR/provider-stdin"
env > "$TMPDIR/provider-env"
printf '%s\n' "$@" > "$TMPDIR/provider-argv"
"#,
        );
    }

    fn spec(directory: &Path, task: Vec<u8>) -> LaunchSpec {
        LaunchSpec {
            runner_program: directory.join("runner-probe"),
            provider_program: PathBuf::from("provider-probe"),
            provider_arguments: vec![OsString::from("--safe-provider-flag")],
            run_id: id::<RunId>("run-safe-launch"),
            runner_instance_id: id::<RunnerInstanceId>("runner-safe-launch"),
            runtime_dir: directory.join("runtime"),
            cwd: directory.to_owned(),
            startup_input: task,
        }
    }

    #[tokio::test]
    async fn environment_is_default_deny_with_fixed_overrides() {
        assert_eq!(
            SAFE_ENVIRONMENT_NAMES,
            [
                "HOME", "USER", "LOGNAME", "SHELL", "PATH", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE",
            ]
        );
        let directory = tempfile::tempdir().unwrap();
        let captured = environment(OsString::from("/usr/bin:/bin"), directory.path());
        let env_program = resolve_executable(Path::new("env"), &captured, "test").unwrap();
        let mut command = Command::new(env_program);
        command
            .env("OPENAI_API_KEY", "openai-secret-sentinel")
            .env("ANTHROPIC_API_KEY", "anthropic-secret-sentinel")
            .env("CODEX_ACCESS_TOKEN", "codex-secret-sentinel")
            .env("CLAUDE_CODE_OAUTH_TOKEN", "claude-secret-sentinel")
            .env("GOOGLE_API_KEY", "google-secret-sentinel")
            .env("VERCEL_TOKEN", "vercel-secret-sentinel")
            .env("DARK_FACTORY_WEBHOOK_SECRET", "webhook-secret-sentinel")
            .env("DARK_FACTORY_TASK", "private task sentinel")
            .env("SSH_AUTH_SOCK", "/secret/agent.sock")
            .env("HTTPS_PROXY", "https://secret-proxy.example")
            .env("DYLD_INSERT_LIBRARIES", "/secret/library.dylib")
            .env("LD_PRELOAD", "/secret/library.so")
            .stdout(Stdio::piped());

        apply_runner_environment(&mut command, &captured);
        let output = command.output().await.unwrap();
        assert!(output.status.success());
        let output = String::from_utf8(output.stdout).unwrap();

        assert!(output.lines().any(|line| line == "HOME=/safe/home"));
        assert!(output.lines().any(|line| line == "USER=safe-user"));
        assert!(output.lines().any(|line| line == "SHELL=/bin/sh"));
        assert!(output.lines().any(|line| line == "PATH=/usr/bin:/bin"));
        assert!(
            output
                .lines()
                .any(|line| line == format!("TMPDIR={}", directory.path().display()))
        );
        assert!(output.lines().any(|line| line == "LANG=C"));
        assert!(output.lines().any(|line| line == "LC_ALL=C"));
        assert!(output.lines().any(|line| line == "LC_CTYPE=C"));
        assert!(output.lines().any(|line| line == "NO_COLOR=1"));
        assert!(output.lines().any(|line| line == "TERM=dumb"));
        assert!(output.lines().any(|line| line == "GIT_TERMINAL_PROMPT=0"));
        assert!(!output.contains("GOOGLE_API_KEY"));
        assert!(!output.contains("OPENAI_API_KEY"));
        assert!(!output.contains("ANTHROPIC_API_KEY"));
        assert!(!output.contains("CODEX_ACCESS_TOKEN"));
        assert!(!output.contains("CLAUDE_CODE_OAUTH_TOKEN"));
        assert!(!output.contains("openai-secret-sentinel"));
        assert!(!output.contains("anthropic-secret-sentinel"));
        assert!(!output.contains("codex-secret-sentinel"));
        assert!(!output.contains("claude-secret-sentinel"));
        assert!(!output.contains("VERCEL_TOKEN"));
        assert!(!output.contains("DARK_FACTORY_WEBHOOK_SECRET"));
        assert!(!output.contains("DARK_FACTORY_TASK"));
        assert!(!output.contains("private task sentinel"));
        assert!(!output.contains("SSH_AUTH_SOCK"));
        assert!(!output.contains("HTTPS_PROXY"));
        assert!(!output.contains("DYLD_INSERT_LIBRARIES"));
        assert!(!output.contains("LD_PRELOAD"));
        assert!(
            !output.contains("LOGNAME="),
            "missing safe names stay absent"
        );
    }

    #[tokio::test]
    async fn prompt_is_exact_stdin_and_never_runner_argv_or_environment() {
        let directory = tempfile::tempdir().unwrap();
        scripts(directory.path());
        let captured = probe_environment(directory.path(), directory.path());
        let task = b"private task \xf0\x9f\x8f\xad\nwith spaces\0and bytes".to_vec();

        let mut child =
            spawn_runner_with_environment(spec(directory.path(), task.clone()), captured)
                .await
                .unwrap();
        assert!(child.wait().await.unwrap().success());

        assert_eq!(
            fs::read(directory.path().join("provider-stdin")).unwrap(),
            task
        );
        let runner_argv = fs::read(directory.path().join("runner-argv")).unwrap();
        let provider_argv = fs::read(directory.path().join("provider-argv")).unwrap();
        let provider_env = fs::read(directory.path().join("provider-env")).unwrap();
        for observed in [&runner_argv, &provider_argv, &provider_env] {
            assert!(!observed.windows(12).any(|bytes| bytes == b"private task"));
        }
        let runner_argv = String::from_utf8(runner_argv).unwrap();
        assert!(runner_argv.contains("--stdin-bytes\n39\n"));
        assert!(runner_argv.contains("--run-id\nrun-safe-launch\n"));
        assert!(runner_argv.contains("--runner-instance-id\nrunner-safe-launch\n"));
        assert!(runner_argv.contains("--safe-provider-flag"));
        assert!(runner_argv.contains(directory.path().join("provider-probe").to_str().unwrap()));
    }

    #[tokio::test]
    async fn oversized_multibyte_prompt_is_rejected_before_spawn_without_echoing_it() {
        let directory = tempfile::tempdir().unwrap();
        scripts(directory.path());
        let captured = probe_environment(directory.path(), directory.path());
        let mut task = "£".repeat(MAX_STARTUP_STDIN_BYTES / 2);
        task.push('£');

        let error = spawn_runner_with_environment(
            spec(directory.path(), task.as_bytes().to_vec()),
            captured,
        )
        .await
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("1048576"));
        assert!(!message.contains('£'));
        assert!(!directory.path().join("runner-argv").exists());
    }

    #[test]
    fn path_resolution_canonicalizes_and_rejects_unusable_files() {
        let directory = tempfile::tempdir().unwrap();
        let bin = directory.path().join("bin with spaces");
        fs::create_dir(&bin).unwrap();
        let executable_path = bin.join("works target");
        executable(&executable_path, "#!/bin/sh\nexit 0\n");
        let executable_link = bin.join("works");
        std::os::unix::fs::symlink(&executable_path, &executable_link).unwrap();
        let non_executable = bin.join("not-executable");
        fs::write(&non_executable, "not executable").unwrap();
        fs::set_permissions(&non_executable, fs::Permissions::from_mode(0o600)).unwrap();
        let captured = environment(OsString::from(bin.as_os_str()), directory.path());

        assert_eq!(
            resolve_executable(Path::new("works"), &captured, "provider").unwrap(),
            fs::canonicalize(executable_path).unwrap()
        );
        assert!(
            resolve_executable(Path::new("not-executable"), &captured, "provider")
                .unwrap_err()
                .to_string()
                .contains("executable regular file")
        );
        assert!(
            resolve_executable(Path::new("missing"), &captured, "provider")
                .unwrap_err()
                .to_string()
                .contains("not found")
        );
    }

    #[tokio::test]
    async fn both_executables_resolve_before_the_runner_is_spawned() {
        let directory = tempfile::tempdir().unwrap();
        executable(
            &directory.path().join("runner-probe"),
            "#!/bin/sh\nprintf spawned > \"$TMPDIR/spawned\"\n",
        );
        let captured = probe_environment(directory.path(), directory.path());
        let mut launch = spec(directory.path(), b"secret prompt sentinel".to_vec());
        launch.provider_program = PathBuf::from("missing-provider");

        let error = spawn_runner_with_environment(launch, captured)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not found"));
        assert!(!error.to_string().contains("secret prompt sentinel"));
        assert!(!directory.path().join("spawned").exists());
    }

    #[tokio::test]
    async fn runner_path_must_be_absolute_and_is_never_resolved_from_path() {
        let directory = tempfile::tempdir().unwrap();
        executable(
            &directory.path().join("runner-probe"),
            "#!/bin/sh\nprintf counterfeit > \"$TMPDIR/counterfeit\"\n",
        );
        executable(
            &directory.path().join("provider-probe"),
            "#!/bin/sh\nexit 0\n",
        );

        for runner in ["runner-probe", "./runner-probe"] {
            let captured = probe_environment(directory.path(), directory.path());
            let mut launch = spec(directory.path(), b"runner path secret".to_vec());
            launch.runner_program = PathBuf::from(runner);
            let error = spawn_runner_with_environment(launch, captured)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("absolute"));
            assert!(!error.to_string().contains("runner path secret"));
        }
        assert!(!directory.path().join("counterfeit").exists());
    }

    #[tokio::test]
    async fn exact_startup_limit_is_accepted() {
        let directory = tempfile::tempdir().unwrap();
        scripts(directory.path());
        let captured = probe_environment(directory.path(), directory.path());
        let task = vec![b'x'; MAX_STARTUP_STDIN_BYTES];

        let mut child = spawn_runner_with_environment(spec(directory.path(), task), captured)
            .await
            .unwrap();
        assert!(child.wait().await.unwrap().success());
        assert_eq!(
            fs::metadata(directory.path().join("provider-stdin"))
                .unwrap()
                .len(),
            u64::try_from(MAX_STARTUP_STDIN_BYTES).unwrap()
        );
    }

    #[tokio::test]
    async fn startup_write_failure_is_reaped_and_never_echoes_prompt() {
        let directory = tempfile::tempdir().unwrap();
        executable(
            &directory.path().join("runner-probe"),
            "#!/bin/sh\nexit 0\n",
        );
        executable(
            &directory.path().join("provider-probe"),
            "#!/bin/sh\nexit 0\n",
        );
        let captured = probe_environment(directory.path(), directory.path());
        let task = "write-failure-secret".repeat(MAX_STARTUP_STDIN_BYTES / 20);

        let error =
            spawn_runner_with_environment(spec(directory.path(), task.into_bytes()), captured)
                .await
                .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("startup input"));
        assert!(!message.contains("write-failure-secret"));
    }

    #[tokio::test]
    async fn cancelling_startup_input_kills_the_not_yet_ready_runner() {
        let directory = tempfile::tempdir().unwrap();
        executable(
            &directory.path().join("runner-probe"),
            "#!/bin/sh\necho $$ > \"$TMPDIR/runner-pid\"\nwhile :; do :; done\n",
        );
        executable(
            &directory.path().join("provider-probe"),
            "#!/bin/sh\nexit 0\n",
        );
        let captured = probe_environment(directory.path(), directory.path());
        let launch = tokio::spawn(spawn_runner_with_environment_and_timeout(
            spec(directory.path(), vec![b'x'; MAX_STARTUP_STDIN_BYTES]),
            captured,
            None,
        ));
        let marker = directory.path().join("runner-pid");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !marker.exists() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let raw_pid = fs::read_to_string(marker).unwrap().trim().parse().unwrap();
        let pid = Pid::from_raw(raw_pid).unwrap();

        launch.abort();
        assert!(launch.await.unwrap_err().is_cancelled());
        let deadline = Instant::now() + Duration::from_secs(2);
        while test_kill_process(pid).is_ok() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(test_kill_process(pid).is_err());
    }

    #[tokio::test]
    async fn dropping_returned_child_does_not_kill_it() {
        let directory = tempfile::tempdir().unwrap();
        scripts(directory.path());
        executable(
            &directory.path().join("provider-probe"),
            "#!/bin/sh\nsleep 0.2\nprintf survived > \"$TMPDIR/survived\"\ncat >/dev/null\n",
        );
        let captured = probe_environment(directory.path(), directory.path());
        let child = spawn_runner_with_environment(spec(directory.path(), Vec::new()), captured)
            .await
            .unwrap();
        drop(child);

        let marker = directory.path().join("survived");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !marker.exists() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(fs::read_to_string(marker).unwrap(), "survived");
    }
}
