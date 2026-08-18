//! Daemon-owned, session-authenticated Git and GitHub operations.
//!
//! The wire requests contain no path, project, agent, branch, remote, or
//! credential selector. Those all come from the live session and durable
//! project state, then are checked again against Git before every command.

use std::{
    path::{Path, PathBuf},
    process::Output,
    time::Duration,
};

use factory_core::{AgentId, ProjectId, ProjectSnapshot, SessionId};
use tokio::{process::Command, time::timeout};

use crate::store::SessionRow;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_OUTPUT_BYTES: usize = 512 * 1024;
const MAX_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_PR_TEXT_BYTES: usize = 128 * 1024;

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
    pub branch: String,
    base: String,
    gh_program: PathBuf,
}

impl Target {
    pub async fn validate(session: SessionRow, project: ProjectSnapshot) -> Result<Self, Error> {
        let worktree = canonical_directory(Path::new(&session.worktree), "session worktree")?;
        let project_root = canonical_directory(Path::new(&project.root), "project root")?;
        let top = git(&worktree, &["rev-parse", "--show-toplevel"]).await?;
        if canonical_directory(Path::new(top.trim()), "git worktree")? != worktree {
            return Err(Error::Rejected(
                "session path is not the Git worktree root".into(),
            ));
        }
        let worktree_common = git(&worktree, &["rev-parse", "--git-common-dir"]).await?;
        let project_common = git(&project_root, &["rev-parse", "--git-common-dir"]).await?;
        if resolve_git_path(&worktree, worktree_common.trim())?
            != resolve_git_path(&project_root, project_common.trim())?
        {
            return Err(Error::Rejected(
                "session worktree does not belong to its project repository".into(),
            ));
        }
        let branch = git(&worktree, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await?;
        let branch = branch.trim().to_owned();
        let expected = format!("agent/{}", session.agent_id);
        if branch != expected {
            return Err(Error::Rejected(format!(
                "session must be on its managed branch {expected}"
            )));
        }
        if matches!(branch.as_str(), "main" | "master") || branch.is_empty() {
            return Err(Error::Rejected("protected or detached ref".into()));
        }
        let base = remote_default_branch(&worktree)
            .await
            .unwrap_or_else(|| "main".into());
        if base == branch {
            return Err(Error::Rejected("head branch cannot be the PR base".into()));
        }
        Ok(Self {
            project_id: session.project_id,
            agent_id: session.agent_id,
            session_id: session.id,
            worktree,
            branch,
            base,
            gh_program: PathBuf::from("gh"),
        })
    }

    pub async fn status(&self) -> Result<String, Error> {
        git(&self.worktree, &["status", "--short", "--branch"]).await
    }

    pub async fn diff(&self, staged: bool) -> Result<String, Error> {
        let args = if staged {
            vec!["diff", "--cached", "--no-ext-diff", "--"]
        } else {
            vec!["diff", "--no-ext-diff", "--"]
        };
        git(&self.worktree, &args).await
    }

    pub async fn commit(&self, message: &str) -> Result<String, Error> {
        validate_text(message, MAX_MESSAGE_BYTES, "commit message")?;
        git(&self.worktree, &["add", "-A", "--"]).await?;
        git(&self.worktree, &["commit", "--no-gpg-sign", "-m", message]).await?;
        let oid = git(&self.worktree, &["rev-parse", "HEAD"]).await?;
        Ok(oid.trim().to_owned())
    }

    pub async fn push(&self) -> Result<String, Error> {
        let refspec = format!("refs/heads/{0}:refs/heads/{0}", self.branch);
        git(&self.worktree, &["push", "--porcelain", "origin", &refspec]).await?;
        Ok(self.branch.clone())
    }

    pub async fn pr_open(&self, title: &str, body: &str) -> Result<String, Error> {
        validate_pr_text(title, body)?;
        gh(
            &self.gh_program,
            &self.worktree,
            &[
                "pr",
                "create",
                "--head",
                &self.branch,
                "--base",
                &self.base,
                "--title",
                title,
                "--body",
                body,
            ],
        )
        .await
        .map(|s| s.trim().to_owned())
    }

    pub async fn pr_update(&self, number: u64, title: &str, body: &str) -> Result<String, Error> {
        validate_pr_text(title, body)?;
        if number == 0 {
            return Err(Error::Rejected("PR number must be positive".into()));
        }
        let number_text = number.to_string();
        let head = gh(
            &self.gh_program,
            &self.worktree,
            &[
                "pr",
                "view",
                &number_text,
                "--json",
                "headRefName",
                "--jq",
                ".headRefName",
            ],
        )
        .await?;
        if head.trim() != self.branch {
            return Err(Error::Rejected("PR head is not the session branch".into()));
        }
        gh(
            &self.gh_program,
            &self.worktree,
            &["pr", "edit", &number_text, "--title", title, "--body", body],
        )
        .await?;
        Ok(number_text)
    }
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
    let resolved = if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    };
    canonical_directory(&resolved, "Git common directory")
}

async fn remote_default_branch(cwd: &Path) -> Option<String> {
    git(
        cwd,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .await
    .ok()
    .and_then(|value| value.trim().strip_prefix("origin/").map(str::to_owned))
}

async fn git(cwd: &Path, args: &[&str]) -> Result<String, Error> {
    command(cwd, "git", args).await
}

async fn gh(program: &Path, cwd: &Path, args: &[&str]) -> Result<String, Error> {
    command(cwd, program, args).await
}

async fn command(cwd: &Path, program: impl AsRef<Path>, args: &[&str]) -> Result<String, Error> {
    let program = program.as_ref();
    let mut command = Command::new(program);
    command.current_dir(cwd).args(args).kill_on_drop(true);
    let output = timeout(COMMAND_TIMEOUT, command.output())
        .await
        .map_err(|_| Error::Timeout)?
        .map_err(|error| {
            Error::Command(format!("could not start {}: {error}", program.display()))
        })?;
    output_text(program, output)
}

fn output_text(program: &Path, output: Output) -> Result<String, Error> {
    if output.stdout.len().saturating_add(output.stderr.len()) > MAX_OUTPUT_BYTES {
        return Err(Error::Command(format!(
            "{} output exceeded its bound",
            program.display()
        )));
    }
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let summary = stderr.lines().next().unwrap_or("command failed");
        return Err(Error::Command(format!("{}: {summary}", program.display())));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| Error::Command(format!("{} output was not UTF-8", program.display())))
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt, process::Command as StdCommand};

    use tempfile::TempDir;

    use super::*;
    use factory_core::{ObserverHealth, Provider, RunnerInstanceId, SessionState};

    fn run(cwd: &Path, args: &[&str]) {
        let status = StdCommand::new("git")
            .current_dir(cwd)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn fixture() -> (TempDir, Target, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let remote = temp.path().join("remote.git");
        let worktree = temp.path().join("worker");
        run(temp.path(), &["init", "--bare", remote.to_str().unwrap()]);
        run(temp.path(), &["init", "-b", "main", repo.to_str().unwrap()]);
        run(&repo, &["config", "user.name", "Factory Test"]);
        run(&repo, &["config", "user.email", "factory@example.invalid"]);
        fs::write(repo.join("README"), "initial\n").unwrap();
        run(&repo, &["add", "README"]);
        run(&repo, &["commit", "-m", "initial"]);
        run(
            &repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        run(&repo, &["push", "-u", "origin", "main"]);
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
        run(&worktree, &["config", "user.name", "Factory Test"]);
        run(
            &worktree,
            &["config", "user.email", "factory@example.invalid"],
        );
        let target = Target {
            project_id: "project".to_owned().try_into().unwrap(),
            agent_id: "worker".to_owned().try_into().unwrap(),
            session_id: "session".to_owned().try_into().unwrap(),
            worktree,
            branch: "agent/worker".into(),
            base: "main".into(),
            gh_program: PathBuf::from("gh"),
        };
        (temp, target, remote)
    }

    #[tokio::test]
    async fn daemon_commit_and_non_force_push_only_the_managed_branch() {
        let (_temp, target, remote) = fixture();
        fs::write(target.worktree.join("change.txt"), "bounded\n").unwrap();
        let oid = target.commit("managed change").await.unwrap();
        assert_eq!(oid.len(), 40);
        assert_eq!(target.push().await.unwrap(), "agent/worker");
        let pushed = git(&remote, &["rev-parse", "refs/heads/agent/worker"])
            .await
            .unwrap();
        assert_eq!(pushed.trim(), oid);
        assert!(
            git(&remote, &["rev-parse", "refs/heads/main"])
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn fake_gh_proves_open_and_update_are_bounded_to_the_session_head() {
        let (temp, mut target, _remote) = fixture();
        let fake = temp.path().join("fake-gh");
        let log = temp.path().join("gh.log");
        fs::write(
            &fake,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$1 $2\" in\n  'pr view') printf '%s\\n' 'agent/worker' ;;\n  'pr create') printf '%s\\n' 'https://example.invalid/pull/7' ;;\nesac\n",
                log.display()
            ),
        ).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
        target.gh_program = fake;
        assert_eq!(
            target.pr_open("Small change", "Body").await.unwrap(),
            "https://example.invalid/pull/7"
        );
        assert_eq!(target.pr_update(7, "Revised", "Body 2").await.unwrap(), "7");
        let calls = fs::read_to_string(log).unwrap();
        assert!(calls.contains("pr create --head agent/worker --base main"));
        assert!(calls.contains("pr view 7 --json headRefName"));
        assert!(calls.contains("pr edit 7 --title Revised --body Body 2"));
        assert!(!calls.contains("--force"));
        assert!(!calls.contains("--delete"));
    }

    fn session(target: &Target) -> SessionRow {
        SessionRow {
            id: target.session_id.clone(),
            project_id: target.project_id.clone(),
            agent_id: target.agent_id.clone(),
            provider: Provider::Shell,
            provider_session_id: None,
            worktree: target.worktree.to_string_lossy().into_owned(),
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
        }
    }

    #[tokio::test]
    async fn validation_derives_the_exact_project_worktree_and_agent_branch() {
        let (_temp, target, _remote) = fixture();
        let common = git(&target.worktree, &["rev-parse", "--git-common-dir"])
            .await
            .unwrap();
        let project_root = canonical_directory(
            &target.worktree.join(common.trim()).join(".."),
            "project root",
        )
        .unwrap();
        let project = ProjectSnapshot {
            id: target.project_id.clone(),
            name: "Fixture".into(),
            root: project_root.to_string_lossy().into_owned(),
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        let validated = Target::validate(session(&target), project.clone())
            .await
            .unwrap();
        assert_eq!(validated.branch, "agent/worker");

        run(&target.worktree, &["branch", "-m", "other"]);
        let error = Target::validate(session(&target), project)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("managed branch agent/worker"));
    }
}
