//! Repository operations executed with daemon authority.
//!
//! Agent-controlled worktrees, Git config, attributes, refs and remotes are
//! all untrusted. Commands therefore run with an empty Git configuration and
//! a daemon-created temporary gitdir/index. Mutations finish with compare-and-
//! swap ref updates against the exact HEAD observed at their start.

use std::{
    env,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use factory_core::{AgentId, ProjectId, ProjectSnapshot, SessionId};
use rustix::process::{Pid, Signal, kill_process_group};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::oneshot,
    time::timeout,
};

use crate::store::{RepositoryAuthority, SessionRow};

const MUTATION_TIMEOUT: Duration = Duration::from_secs(60);
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_OUTPUT_BYTES: usize = 512 * 1024;
const MAX_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_PR_TEXT_BYTES: usize = 128 * 1024;
const SAFE_PATH: &str = "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("repository request rejected: {0}")]
    Rejected(String),
    #[error("repository command failed: {0}")]
    Command(String),
    #[error("repository command timed out")]
    Timeout,
}

#[derive(Debug)]
pub struct Target {
    pub project_id: ProjectId,
    pub agent_id: AgentId,
    pub session_id: SessionId,
    pub worktree: PathBuf,
    branch: String,
    common_dir: PathBuf,
    head: String,
    worktree_device: u64,
    worktree_inode: u64,
    authority: RepositoryAuthority,
    github_repo: Option<String>,
    gh_program: PathBuf,
}

pub fn validate_authority(
    remote_url: String,
    base_branch: String,
) -> Result<RepositoryAuthority, Error> {
    validate_ref(&base_branch, "base branch")?;
    let remote_url = canonical_remote(&remote_url)?;
    Ok(RepositoryAuthority {
        remote_url,
        base_branch,
    })
}

impl Target {
    pub async fn validate(
        session: SessionRow,
        project: ProjectSnapshot,
        authority: RepositoryAuthority,
    ) -> Result<Self, Error> {
        let authority = validate_authority(authority.remote_url, authority.base_branch)?;
        let worktree = canonical_directory(Path::new(&session.worktree), "session worktree")?;
        let project_root = canonical_directory(Path::new(&project.root), "project root")?;
        let top = safe_git(
            &worktree,
            None,
            &["rev-parse", "--show-toplevel"],
            None,
            READ_TIMEOUT,
        )
        .await?;
        if canonical_directory(Path::new(top.trim()), "git worktree")? != worktree {
            return Err(Error::Rejected(
                "session path is not the Git worktree root".into(),
            ));
        }
        let common = safe_git(
            &worktree,
            None,
            &["rev-parse", "--git-common-dir"],
            None,
            READ_TIMEOUT,
        )
        .await?;
        let project_common = safe_git(
            &project_root,
            None,
            &["rev-parse", "--git-common-dir"],
            None,
            READ_TIMEOUT,
        )
        .await?;
        let common_dir = resolve_git_path(&worktree, common.trim())?;
        if common_dir != resolve_git_path(&project_root, project_common.trim())? {
            return Err(Error::Rejected(
                "session worktree does not belong to its project repository".into(),
            ));
        }
        let branch = safe_git(
            &worktree,
            None,
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
            None,
            READ_TIMEOUT,
        )
        .await?;
        let branch = branch.trim().to_owned();
        let expected = format!("agent/{}", session.agent_id);
        if branch != expected {
            return Err(Error::Rejected(format!(
                "session must be on its managed branch {expected}"
            )));
        }
        validate_ref(&branch, "head branch")?;
        let head = safe_git(
            &worktree,
            None,
            &["rev-parse", "--verify", "HEAD"],
            None,
            READ_TIMEOUT,
        )
        .await?;
        let metadata = std::fs::metadata(&worktree)
            .map_err(|_| Error::Rejected("session worktree disappeared".into()))?;
        let github_repo = github_slug(&authority.remote_url);
        Ok(Self {
            project_id: session.project_id,
            agent_id: session.agent_id,
            session_id: session.id,
            worktree,
            branch,
            common_dir,
            head: head.trim().to_owned(),
            worktree_device: metadata.dev(),
            worktree_inode: metadata.ino(),
            authority,
            github_repo,
            gh_program: PathBuf::from("gh"),
        })
    }

    async fn revalidate(&self) -> Result<String, Error> {
        let metadata = std::fs::metadata(&self.worktree)
            .map_err(|_| Error::Rejected("session worktree disappeared".into()))?;
        if metadata.dev() != self.worktree_device || metadata.ino() != self.worktree_inode {
            return Err(Error::Rejected("session worktree identity changed".into()));
        }
        let branch = safe_git(
            &self.worktree,
            None,
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
            None,
            READ_TIMEOUT,
        )
        .await?;
        if branch.trim() != self.branch {
            return Err(Error::Rejected(
                "session branch changed during the request".into(),
            ));
        }
        let head = safe_git(
            &self.worktree,
            None,
            &["rev-parse", "--verify", "HEAD"],
            None,
            READ_TIMEOUT,
        )
        .await?;
        Ok(head.trim().to_owned())
    }

    async fn sandbox(&self) -> Result<GitSandbox, Error> {
        let head = self.revalidate().await?;
        GitSandbox::new(&self.worktree, &self.common_dir, &self.branch, &head).await
    }

    pub async fn status(&self) -> Result<String, Error> {
        let sandbox = self.sandbox().await?;
        sandbox
            .git(&["status", "--short", "--branch"], None, READ_TIMEOUT)
            .await
    }

    pub async fn diff(&self, staged: bool) -> Result<String, Error> {
        let sandbox = self.sandbox().await?;
        let args = if staged {
            &["diff", "--cached", "--no-ext-diff", "--"][..]
        } else {
            &["diff", "--no-ext-diff", "--"][..]
        };
        sandbox.git(args, None, READ_TIMEOUT).await
    }

    pub async fn commit(&self, message: &str) -> Result<String, Error> {
        validate_text(message, MAX_MESSAGE_BYTES, "commit message")?;
        let sandbox = self.sandbox().await?;
        sandbox
            .git(&["add", "-A", "--"], None, MUTATION_TIMEOUT)
            .await?;
        let tree = sandbox.git(&["write-tree"], None, MUTATION_TIMEOUT).await?;
        let oid = sandbox
            .git(
                &["commit-tree", tree.trim(), "-p", &self.head, "-F", "-"],
                Some(message.as_bytes()),
                MUTATION_TIMEOUT,
            )
            .await?;
        let current = self.revalidate().await?;
        if current != self.head {
            return Err(Error::Rejected(
                "HEAD changed before commit publication".into(),
            ));
        }
        safe_git(
            &self.worktree,
            None,
            &[
                "update-ref",
                &format!("refs/heads/{}", self.branch),
                oid.trim(),
                &self.head,
            ],
            None,
            MUTATION_TIMEOUT,
        )
        .await?;
        self.revalidate().await?;
        Ok(oid.trim().to_owned())
    }

    pub async fn push(&self) -> Result<String, Error> {
        let head = self.revalidate().await?;
        let sandbox = self.sandbox().await?;
        let refspec = format!("refs/heads/{0}:refs/heads/{0}", self.branch);
        if self.github_repo.is_some() {
            let token = gh(
                &self.gh_program,
                &self.worktree,
                &["auth", "token"],
                READ_TIMEOUT,
            )
            .await?;
            let token = token.trim();
            if token.is_empty() || token.bytes().any(|byte| byte.is_ascii_control()) {
                return Err(Error::Command("gh returned an invalid credential".into()));
            }
            let authorization = format!(
                "Authorization: Basic {}",
                STANDARD.encode(format!("x-access-token:{token}"))
            );
            safe_git_with_header(
                &self.worktree,
                Some(&sandbox),
                &["push", "--porcelain", &self.authority.remote_url, &refspec],
                None,
                MUTATION_TIMEOUT,
                &authorization,
            )
            .await?;
        } else {
            sandbox
                .git(
                    &["push", "--porcelain", &self.authority.remote_url, &refspec],
                    None,
                    MUTATION_TIMEOUT,
                )
                .await?;
        }
        if self.revalidate().await? != head {
            return Err(Error::Rejected(
                "HEAD changed while push was running".into(),
            ));
        }
        Ok(self.branch.clone())
    }

    pub async fn pr_open(&self, title: &str, body: &str) -> Result<String, Error> {
        validate_pr_text(title, body)?;
        let repo = self.github_repo.as_deref().ok_or_else(|| {
            Error::Rejected("pull requests require a configured GitHub HTTPS remote".into())
        })?;
        let head = self.revalidate().await?;
        gh(
            &self.gh_program,
            &self.worktree,
            &[
                "pr",
                "create",
                "--repo",
                repo,
                "--head",
                &self.branch,
                "--base",
                &self.authority.base_branch,
                "--title",
                title,
                "--body",
                body,
            ],
            MUTATION_TIMEOUT,
        )
        .await?;
        let verified = gh(
            &self.gh_program,
            &self.worktree,
            &[
                "pr",
                "view",
                &self.branch,
                "--repo",
                repo,
                "--json",
                "headRefName,baseRefName,url",
                "--jq",
                "[.headRefName,.baseRefName,.url]|@tsv",
            ],
            READ_TIMEOUT,
        )
        .await?;
        let url = verify_pr(&verified, &self.branch, &self.authority.base_branch)?;
        if self.revalidate().await? != head {
            return Err(Error::Rejected("HEAD changed while PR was opened".into()));
        }
        Ok(url)
    }

    pub async fn pr_update(&self, number: u64, title: &str, body: &str) -> Result<String, Error> {
        validate_pr_text(title, body)?;
        if number == 0 {
            return Err(Error::Rejected("PR number must be positive".into()));
        }
        let repo = self.github_repo.as_deref().ok_or_else(|| {
            Error::Rejected("pull requests require a configured GitHub HTTPS remote".into())
        })?;
        let number_text = number.to_string();
        let before = gh(
            &self.gh_program,
            &self.worktree,
            &[
                "pr",
                "view",
                &number_text,
                "--repo",
                repo,
                "--json",
                "headRefName,baseRefName,url",
                "--jq",
                "[.headRefName,.baseRefName,.url]|@tsv",
            ],
            READ_TIMEOUT,
        )
        .await?;
        verify_pr(&before, &self.branch, &self.authority.base_branch)?;
        let head = self.revalidate().await?;
        gh(
            &self.gh_program,
            &self.worktree,
            &[
                "pr",
                "edit",
                &number_text,
                "--repo",
                repo,
                "--title",
                title,
                "--body",
                body,
            ],
            MUTATION_TIMEOUT,
        )
        .await?;
        let after = gh(
            &self.gh_program,
            &self.worktree,
            &[
                "pr",
                "view",
                &number_text,
                "--repo",
                repo,
                "--json",
                "headRefName,baseRefName,url",
                "--jq",
                "[.headRefName,.baseRefName,.url]|@tsv",
            ],
            READ_TIMEOUT,
        )
        .await?;
        verify_pr(&after, &self.branch, &self.authority.base_branch)?;
        if self.revalidate().await? != head {
            return Err(Error::Rejected("HEAD changed while PR was updated".into()));
        }
        Ok(number_text)
    }
}

struct GitSandbox {
    _dir: tempfile::TempDir,
    git_dir: PathBuf,
    index: PathBuf,
    worktree: PathBuf,
    object_dir: PathBuf,
}

impl GitSandbox {
    async fn new(
        worktree: &Path,
        common_dir: &Path,
        branch: &str,
        head: &str,
    ) -> Result<Self, Error> {
        let dir =
            tempfile::tempdir().map_err(|e| Error::Command(format!("temporary gitdir: {e}")))?;
        safe_git(
            dir.path(),
            None,
            &["init", "--bare", "--quiet"],
            None,
            READ_TIMEOUT,
        )
        .await?;
        let sandbox = Self {
            git_dir: dir.path().to_path_buf(),
            index: dir.path().join("index"),
            worktree: worktree.to_path_buf(),
            object_dir: common_dir.join("objects"),
            _dir: dir,
        };
        sandbox
            .git(&["read-tree", head], None, READ_TIMEOUT)
            .await?;
        sandbox
            .git(
                &["update-ref", &format!("refs/heads/{branch}"), head],
                None,
                READ_TIMEOUT,
            )
            .await?;
        Ok(sandbox)
    }
    async fn git(
        &self,
        args: &[&str],
        stdin: Option<&[u8]>,
        deadline: Duration,
    ) -> Result<String, Error> {
        safe_git(&self.worktree, Some(self), args, stdin, deadline).await
    }
}

fn verify_pr(value: &str, head: &str, base: &str) -> Result<String, Error> {
    let mut fields = value.trim().split('\t');
    let actual_head = fields.next().unwrap_or_default();
    let actual_base = fields.next().unwrap_or_default();
    let url = fields.next().unwrap_or_default();
    if actual_head != head || actual_base != base || !url.starts_with("https://github.com/") {
        return Err(Error::Rejected(
            "PR identity changed during the request".into(),
        ));
    }
    Ok(url.to_owned())
}

fn validate_pr_text(title: &str, body: &str) -> Result<(), Error> {
    validate_text(title, 240, "PR title")?;
    validate_text(body, MAX_PR_TEXT_BYTES, "PR body")
}
fn validate_text(value: &str, max: usize, name: &str) -> Result<(), Error> {
    if value.trim().is_empty() || value.len() > max || value.as_bytes().contains(&0) {
        return Err(Error::Rejected(format!(
            "{name} is empty or exceeds its bound"
        )));
    }
    Ok(())
}
fn validate_ref(value: &str, label: &str) -> Result<(), Error> {
    let safe = !value.is_empty()
        && !value.starts_with('-')
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.contains("..")
        && !value.contains("@{")
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b));
    if !safe || matches!(value, "main" | "master") && label == "head branch" {
        return Err(Error::Rejected(format!("invalid {label}")));
    }
    Ok(())
}
fn canonical_remote(value: &str) -> Result<String, Error> {
    if let Some(slug) = github_slug(value) {
        return Ok(format!("https://github.com/{slug}.git"));
    }
    let path = value
        .strip_prefix("file://")
        .map(PathBuf::from)
        .or_else(|| Path::new(value).is_absolute().then(|| PathBuf::from(value)))
        .ok_or_else(|| {
            Error::Rejected("remote must be GitHub HTTPS or an absolute local test path".into())
        })?;
    let canonical = std::fs::canonicalize(path)
        .map_err(|_| Error::Rejected("local remote does not exist".into()))?;
    Ok(format!("file://{}", canonical.display()))
}
fn github_slug(value: &str) -> Option<String> {
    let tail = value.strip_prefix("https://github.com/")?;
    let slug = tail.strip_suffix(".git").unwrap_or(tail);
    let mut parts = slug.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    (parts.next().is_none()
        && !owner.is_empty()
        && !repo.is_empty()
        && owner
            .bytes()
            .chain(repo.bytes())
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)))
    .then(|| format!("{owner}/{repo}"))
}
fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf, Error> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|_| Error::Rejected(format!("{label} is unavailable")))?;
    if !canonical.is_dir() {
        return Err(Error::Rejected(format!("{label} is not a directory")));
    }
    Ok(canonical)
}
fn resolve_git_path(cwd: &Path, value: &str) -> Result<PathBuf, Error> {
    let path = Path::new(value);
    canonical_directory(
        &if path.is_absolute() {
            path.to_owned()
        } else {
            cwd.join(path)
        },
        "Git common directory",
    )
}

async fn safe_git(
    cwd: &Path,
    sandbox: Option<&GitSandbox>,
    args: &[&str],
    stdin: Option<&[u8]>,
    deadline: Duration,
) -> Result<String, Error> {
    safe_git_inner(cwd, sandbox, args, stdin, deadline, None).await
}

async fn safe_git_with_header(
    cwd: &Path,
    sandbox: Option<&GitSandbox>,
    args: &[&str],
    stdin: Option<&[u8]>,
    deadline: Duration,
    authorization: &str,
) -> Result<String, Error> {
    safe_git_inner(cwd, sandbox, args, stdin, deadline, Some(authorization)).await
}

async fn safe_git_inner(
    cwd: &Path,
    sandbox: Option<&GitSandbox>,
    args: &[&str],
    stdin: Option<&[u8]>,
    deadline: Duration,
    authorization: Option<&str>,
) -> Result<String, Error> {
    let mut fixed = vec![
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "credential.helper=",
        "-c",
        "core.sshCommand=/usr/bin/false",
        "-c",
        "diff.external=",
        "-c",
        "protocol.allow=never",
        "-c",
        "protocol.file.allow=always",
        "-c",
        "protocol.https.allow=always",
    ];
    fixed.extend_from_slice(args);
    let mut envs = vec![
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_SYSTEM", "/dev/null"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_ATTR_NOSYSTEM", "1"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_ASKPASS", "/usr/bin/false"),
        ("GIT_SSH_COMMAND", "/usr/bin/false"),
    ];
    if let Some(value) = authorization {
        // The daemon-sourced credential stays out of argv. Repository and
        // global config, helpers, askpass, SSH, filters, and hooks remain off.
        envs.extend([
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "http.extraHeader"),
            ("GIT_CONFIG_VALUE_0", value),
        ]);
    }
    let owned;
    if let Some(s) = sandbox {
        owned = vec![
            ("GIT_DIR", s.git_dir.to_string_lossy().into_owned()),
            ("GIT_INDEX_FILE", s.index.to_string_lossy().into_owned()),
            ("GIT_WORK_TREE", s.worktree.to_string_lossy().into_owned()),
            (
                "GIT_OBJECT_DIRECTORY",
                s.object_dir.to_string_lossy().into_owned(),
            ),
            ("GIT_AUTHOR_NAME", "Dark Factory".into()),
            ("GIT_AUTHOR_EMAIL", "factory@localhost".into()),
            ("GIT_COMMITTER_NAME", "Dark Factory".into()),
            ("GIT_COMMITTER_EMAIL", "factory@localhost".into()),
        ];
        for (k, v) in &owned {
            envs.push((k, v));
        }
    }
    run_command(Path::new("git"), cwd, &fixed, &envs, stdin, deadline).await
}

async fn gh(
    program: &Path,
    cwd: &Path,
    args: &[&str],
    deadline: Duration,
) -> Result<String, Error> {
    let home = env::var("HOME").unwrap_or_else(|_| "/var/empty".into());
    let config = format!("{home}/.config/gh");
    run_command(
        program,
        cwd,
        args,
        &[("HOME", home.as_str()), ("GH_CONFIG_DIR", config.as_str())],
        None,
        deadline,
    )
    .await
}

async fn bounded_read(
    mut reader: impl AsyncRead + Unpin,
    overflow: oneshot::Sender<()>,
) -> Vec<u8> {
    let mut output = Vec::new();
    let mut chunk = [0u8; 8192];
    let overflow = overflow;
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) if output.len() + n <= MAX_OUTPUT_BYTES => output.extend_from_slice(&chunk[..n]),
            Ok(_) => {
                let _ = overflow.send(());
                break;
            }
        }
    }
    output
}

async fn wait_for_overflow(receiver: &mut oneshot::Receiver<()>) {
    if receiver.await.is_err() {
        std::future::pending::<()>().await;
    }
}

async fn run_command(
    program: &Path,
    cwd: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    stdin: Option<&[u8]>,
    deadline: Duration,
) -> Result<String, Error> {
    let mut command = Command::new(program);
    command
        .current_dir(cwd)
        .args(args)
        .env_clear()
        .env("PATH", SAFE_PATH)
        .envs(envs.iter().copied())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command
        .spawn()
        .map_err(|e| Error::Command(format!("could not start {}: {e}", program.display())))?;
    let pid = child
        .id()
        .and_then(|id| Pid::from_raw(id as i32))
        .ok_or_else(|| Error::Command("child has no pid".into()))?;
    if let (Some(bytes), Some(mut writer)) = (stdin, child.stdin.take()) {
        writer
            .write_all(bytes)
            .await
            .map_err(|_| Error::Command("command stdin failed".into()))?;
    }
    let (overflow_tx, mut overflow_rx) = oneshot::channel();
    let stdout = tokio::spawn(bounded_read(
        child.stdout.take().expect("piped stdout"),
        overflow_tx,
    ));
    let (stderr_overflow_tx, mut stderr_overflow_rx) = oneshot::channel();
    let stderr = tokio::spawn(bounded_read(
        child.stderr.take().expect("piped stderr"),
        stderr_overflow_tx,
    ));
    let outcome = timeout(deadline, async {
        tokio::select! {
            status = child.wait() => status.map(Some),
            () = wait_for_overflow(&mut overflow_rx) => Ok(None),
            () = wait_for_overflow(&mut stderr_overflow_rx) => Ok(None),
        }
    })
    .await;
    let status = match outcome {
        Ok(Ok(Some(status))) => status,
        Ok(Ok(None)) => {
            let _ = kill_process_group(pid, Signal::KILL);
            let _ = child.wait().await;
            return Err(Error::Command("command output exceeded its bound".into()));
        }
        Ok(Err(e)) => return Err(Error::Command(e.to_string())),
        Err(_) => {
            let _ = kill_process_group(pid, Signal::KILL);
            let _ = child.wait().await;
            return Err(Error::Timeout);
        }
    };
    let stdout = stdout.await.unwrap_or_default();
    let stderr = stderr.await.unwrap_or_default();
    if !status.success() {
        let summary = String::from_utf8_lossy(&stderr);
        return Err(Error::Command(
            summary
                .lines()
                .next()
                .unwrap_or("command failed")
                .to_owned(),
        ));
    }
    String::from_utf8(stdout).map_err(|_| Error::Command("command output was not UTF-8".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use factory_core::{ObserverHealth, Provider, RunnerInstanceId, SessionState};
    use std::{fs, os::unix::fs::PermissionsExt, process::Command as StdCommand};

    fn run(cwd: &Path, args: &[&str]) {
        assert!(
            StdCommand::new("git")
                .current_dir(cwd)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }

    async fn fixture() -> (tempfile::TempDir, Target, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let remote = temp.path().join("remote.git");
        let worktree = temp.path().join("worker");
        run(temp.path(), &["init", "--bare", remote.to_str().unwrap()]);
        run(temp.path(), &["init", "-b", "main", repo.to_str().unwrap()]);
        run(&repo, &["config", "user.name", "Test"]);
        run(&repo, &["config", "user.email", "test@example.invalid"]);
        fs::write(repo.join("README"), "initial\n").unwrap();
        run(&repo, &["add", "README"]);
        run(&repo, &["commit", "-m", "initial"]);
        run(
            &repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        run(&repo, &["push", "origin", "main"]);
        run(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "agent/worker",
                worktree.to_str().unwrap(),
            ],
        );
        let target_stub = Target {
            project_id: "project".to_owned().try_into().unwrap(),
            agent_id: "worker".to_owned().try_into().unwrap(),
            session_id: "session".to_owned().try_into().unwrap(),
            worktree: worktree.clone(),
            branch: "agent/worker".into(),
            common_dir: repo.join(".git"),
            head: String::new(),
            worktree_device: 0,
            worktree_inode: 0,
            authority: validate_authority(remote.to_string_lossy().into_owned(), "main".into())
                .unwrap(),
            github_repo: None,
            gh_program: "gh".into(),
        };
        let session = SessionRow {
            id: target_stub.session_id.clone(),
            project_id: target_stub.project_id.clone(),
            agent_id: target_stub.agent_id.clone(),
            provider: Provider::Shell,
            provider_session_id: None,
            worktree: worktree.to_string_lossy().into_owned(),
            codex_home: None,
            hook_token: "a".repeat(64),
            state: SessionState::Idle,
            state_since_ms: 1,
            activity: None,
            activity_inferred: false,
            wait_reason: None,
            observer_health: ObserverHealth::Healthy,
            observer_health_since_ms: 1,
            runner_instance_id: RunnerInstanceId::try_from("runner".to_owned()).unwrap(),
            runner_runtime: "/tmp/runner".into(),
            runner_protocol_version: 1,
            last_hook_event: None,
            last_hook_at_ms: None,
            started_at_ms: 1,
            updated_at_ms: 1,
            ended_at_ms: None,
            exit_code: None,
            exit_signal: None,
            stop_requested_at_ms: None,
            current_run_id: None,
        };
        let project = ProjectSnapshot {
            id: target_stub.project_id,
            name: "Test".into(),
            root: repo.to_string_lossy().into_owned(),
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        let target = Target::validate(session, project, target_stub.authority)
            .await
            .unwrap();
        (temp, target, remote)
    }

    #[tokio::test]
    async fn agent_git_config_hooks_filters_helpers_and_fsmonitor_never_execute() {
        let (temp, target, _) = fixture().await;
        let marker = temp.path().join("marker");
        let attack = temp.path().join("attack");
        fs::write(
            &attack,
            format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&attack, fs::Permissions::from_mode(0o700)).unwrap();
        run(
            &target.worktree,
            &["config", "core.hooksPath", temp.path().to_str().unwrap()],
        );
        run(
            &target.worktree,
            &["config", "core.fsmonitor", attack.to_str().unwrap()],
        );
        run(
            &target.worktree,
            &["config", "filter.evil.clean", attack.to_str().unwrap()],
        );
        run(
            &target.worktree,
            &["config", "credential.helper", attack.to_str().unwrap()],
        );
        fs::write(
            target.worktree.join(".gitattributes"),
            "victim filter=evil\n",
        )
        .unwrap();
        fs::write(target.worktree.join("victim"), "safe\n").unwrap();
        fs::write(
            temp.path().join("pre-commit"),
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(
            temp.path().join("pre-commit"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        target.commit("safe commit").await.unwrap();
        assert!(
            !marker.exists(),
            "agent-controlled executable ran with daemon authority"
        );
    }

    #[tokio::test]
    async fn configured_remote_is_immutable_and_branch_changes_fail_closed() {
        let (temp, target, remote) = fixture().await;
        let attacker = temp.path().join("attacker.git");
        run(temp.path(), &["init", "--bare", attacker.to_str().unwrap()]);
        run(
            &target.worktree,
            &["remote", "set-url", "origin", attacker.to_str().unwrap()],
        );
        fs::write(target.worktree.join("change"), "x").unwrap();
        let oid = target.commit("change").await.unwrap();
        target.push().await.unwrap();
        assert_eq!(
            safe_git(
                &remote,
                None,
                &["rev-parse", "refs/heads/agent/worker"],
                None,
                READ_TIMEOUT
            )
            .await
            .unwrap()
            .trim(),
            oid
        );
        assert!(
            safe_git(
                &attacker,
                None,
                &["rev-parse", "refs/heads/agent/worker"],
                None,
                READ_TIMEOUT
            )
            .await
            .is_err()
        );
        run(&target.worktree, &["branch", "-m", "agent/other"]);
        assert!(
            target
                .push()
                .await
                .unwrap_err()
                .to_string()
                .contains("branch changed")
        );
    }

    #[tokio::test]
    async fn bounded_runner_kills_an_output_flood_process_group() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("flood");
        let child_pid = temp.path().join("pid");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\n(sh -c 'echo $$ > {}; while :; do printf x; done') &\nwait\n",
                child_pid.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let error = run_command(&script, temp.path(), &[], &[], None, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("output exceeded"));
        let pid: i32 = fs::read_to_string(child_pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(rustix::process::test_kill_process(Pid::from_raw(pid).unwrap()).is_err());
    }

    #[tokio::test]
    async fn gh_is_pinned_to_configured_repo_base_and_reverified_after_mutation() {
        let (temp, mut target, _) = fixture().await;
        target.authority =
            validate_authority("https://github.com/owner/repo.git".into(), "main".into()).unwrap();
        target.github_repo = Some("owner/repo".into());
        let fake = temp.path().join("gh");
        let log = temp.path().join("gh.log");
        fs::write(&fake, format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$1 $2\" in\n 'pr view') printf 'agent/worker\\tmain\\thttps://github.com/owner/repo/pull/7\\n' ;;\nesac\n", log.display())).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
        target.gh_program = fake;
        assert_eq!(
            target.pr_open("Title", "Body").await.unwrap(),
            "https://github.com/owner/repo/pull/7"
        );
        assert_eq!(target.pr_update(7, "Title 2", "Body 2").await.unwrap(), "7");
        let calls = fs::read_to_string(log).unwrap();
        assert!(calls.contains("pr create --repo owner/repo --head agent/worker --base main"));
        assert_eq!(
            calls.matches("pr view").count(),
            3,
            "open and both sides of update must verify"
        );
        assert!(calls.contains("pr edit 7 --repo owner/repo"));
    }
}
