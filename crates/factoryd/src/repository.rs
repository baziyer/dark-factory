//! Repository operations executed with daemon authority.
//!
//! Agent-controlled worktrees, Git config, attributes, refs and remotes are
//! all untrusted. Commands therefore run with an empty Git configuration and
//! a daemon-created temporary gitdir/index. Mutations finish with compare-and-
//! swap ref updates against the exact HEAD observed at their start.

use std::{
    env,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use factory_core::{AgentId, ProjectId, ProjectSnapshot, SessionId};
use rustix::process::{Pid, Signal, kill_process_group};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::oneshot,
    time::timeout,
};

use crate::store::{ManagedChangeRecord, RepositoryAuthority, SessionRow};

const MUTATION_TIMEOUT: Duration = Duration::from_secs(60);
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_OUTPUT_BYTES: usize = 512 * 1024;
const MAX_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_PR_TEXT_BYTES: usize = 128 * 1024;
const SAFE_PATH: &str = "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin";

#[cfg(test)]
static RECOVERY_REF_RACE: std::sync::Mutex<Option<(PathBuf, String, String)>> =
    std::sync::Mutex::new(None);

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
    git_dir: PathBuf,
    common_dir: PathBuf,
    head: String,
    worktree_device: u64,
    worktree_inode: u64,
    git_dir_device: u64,
    git_dir_inode: u64,
    common_dir_device: u64,
    common_dir_inode: u64,
    authority: RepositoryAuthority,
    github_repo: Option<String>,
    gh_program: PathBuf,
    registered_change: Option<ManagedChangeRecord>,
    #[cfg(test)]
    test_push_race: std::sync::Mutex<Option<(PathBuf, String, String)>>,
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

/// Creates the one daemon-derived issue branch for a task.  The remote, base,
/// repository, and path are all supplied by daemon state; the task id is
/// already validated by the caller as the authenticated session's current
/// run task.
pub async fn create_managed_change(
    project: &ProjectSnapshot,
    authority: RepositoryAuthority,
    task_id: &factory_core::TaskId,
    agent_id: &AgentId,
    worktree_path: &Path,
) -> Result<ManagedChangeRecord, Error> {
    let authority = validate_authority(authority.remote_url, authority.base_branch)?;
    let project_root = canonical_directory(Path::new(&project.root), "project root")?;
    let branch = format!("issue/{}", task_id.as_str());
    validate_ref(&branch, "managed issue branch")?;
    if matches!(authority.base_branch.as_str(), "main" | "master")
        && branch == authority.base_branch
    {
        return Err(Error::Rejected("managed issue branch is protected".into()));
    }
    // A daemon crash can leave the exact derived worktree after the SQLite
    // insert failed. Reuse it only after proving its path and branch are the
    // expected managed identity; unrelated occupants remain a hard collision.
    let mut reuse_existing = match std::fs::symlink_metadata(worktree_path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(Error::Rejected(
                    "managed change worktree leaf must not be a symlink".into(),
                ));
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => {
            return Err(Error::Rejected(
                "managed change worktree is unavailable".into(),
            ));
        }
    };
    let parent = worktree_path
        .parent()
        .ok_or_else(|| Error::Rejected("managed change path has no parent".into()))?;
    if !reuse_existing {
        std::fs::create_dir_all(parent)
            .map_err(|_| Error::Rejected("managed change directory is unavailable".into()))?;
    }
    let daemon_root = worktree_path
        .ancestors()
        .nth(2)
        .ok_or_else(|| Error::Rejected("managed change path is invalid".into()))?;
    let daemon_root = canonical_directory(daemon_root, "managed change root")?;
    let managed_parent = canonical_directory(parent, "managed change parent")?;
    if !managed_parent.starts_with(&daemon_root) {
        return Err(Error::Rejected(
            "managed change path escaped its daemon root".into(),
        ));
    }
    let leaf = worktree_path
        .file_name()
        .ok_or_else(|| Error::Rejected("managed change path has no leaf".into()))?;
    let expected_worktree = managed_parent.join(leaf);
    let base_ref = format!("refs/heads/{}", authority.base_branch);
    let base_output =
        trusted_remote_ls_remote(&project_root, &authority.remote_url, &base_ref).await?;
    let base_sha = parse_remote_head(&base_output, &base_ref)?
        .ok_or_else(|| Error::Rejected("configured base branch is absent remotely".into()))?;
    let local_base = safe_git(
        &project_root,
        None,
        &["rev-parse", "--verify", &format!("{base_sha}^{{commit}}")],
        None,
        READ_TIMEOUT,
    )
    .await?;
    if local_base.trim() != base_sha {
        return Err(Error::Rejected(
            "configured base commit is not present locally".into(),
        ));
    }
    let project_common = safe_git(
        &project_root,
        None,
        &["rev-parse", "--git-common-dir"],
        None,
        READ_TIMEOUT,
    )
    .await?;
    let project_common = resolve_git_path(&project_root, project_common.trim())?;
    let branch_ref = format!("refs/heads/{branch}");
    if reuse_existing {
        let existing_root = canonical_directory(worktree_path, "managed change worktree")?;
        if existing_root != expected_worktree {
            return Err(Error::Rejected(
                "managed change worktree leaf escaped its managed path".into(),
            ));
        }
        let existing_branch = safe_git(
            &existing_root,
            None,
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
            None,
            READ_TIMEOUT,
        )
        .await?;
        if existing_branch.trim() != branch {
            return Err(Error::Rejected(
                "managed change path is occupied by another branch".into(),
            ));
        }
        let existing_common = safe_git(
            &existing_root,
            None,
            &["rev-parse", "--git-common-dir"],
            None,
            READ_TIMEOUT,
        )
        .await?;
        let existing_common = resolve_git_path(&existing_root, existing_common.trim())?;
        if existing_common != project_common {
            return Err(Error::Rejected(
                "managed change path belongs to another repository".into(),
            ));
        }
        let existing_head = safe_git(
            &existing_root,
            None,
            &["rev-parse", "--verify", "HEAD"],
            None,
            READ_TIMEOUT,
        )
        .await?;
        if existing_head.trim() != base_sha {
            if !managed_worktree_is_clean(worktree_path).await? {
                return Err(Error::Rejected(
                    "managed change recovery worktree is dirty at the new base".into(),
                ));
            }
            if safe_git(
                &project_root,
                None,
                &[
                    "merge-base",
                    "--is-ancestor",
                    existing_head.trim(),
                    &base_sha,
                ],
                None,
                READ_TIMEOUT,
            )
            .await
            .is_err()
            {
                return Err(Error::Rejected(
                    "managed change recovery worktree diverged from the new base; preserving it"
                        .into(),
                ));
            }
            safe_git(
                &project_root,
                None,
                &[
                    "worktree",
                    "remove",
                    worktree_path.to_string_lossy().as_ref(),
                ],
                None,
                MUTATION_TIMEOUT,
            )
            .await?;
            #[cfg(test)]
            let recovery_race = { RECOVERY_REF_RACE.lock().unwrap().take() };
            #[cfg(test)]
            if let Some((race_root, race_branch, race_head)) = recovery_race {
                let race_ref = format!("refs/heads/{race_branch}");
                safe_git(
                    &race_root,
                    None,
                    &["update-ref", &race_ref, &race_head],
                    None,
                    MUTATION_TIMEOUT,
                )
                .await?;
            }
            if let Err(error) = safe_git(
                &project_root,
                None,
                &["update-ref", "-d", &branch_ref, existing_head.trim()],
                None,
                MUTATION_TIMEOUT,
            )
            .await
            {
                return Err(Error::Rejected(format!(
                    "managed change branch changed during recovery; preserving it: {error}"
                )));
            }
            reuse_existing = false;
        }
    } else if safe_git(
        &project_root,
        None,
        &["show-ref", "--verify", "--quiet", &branch_ref],
        None,
        READ_TIMEOUT,
    )
    .await
    .is_ok()
    {
        return Err(Error::Rejected(
            "managed issue branch collides locally".into(),
        ));
    }
    let branch_output =
        trusted_remote_ls_remote(&project_root, &authority.remote_url, &branch_ref).await?;
    if parse_remote_head(&branch_output, &branch_ref)?.is_some() {
        return Err(Error::Rejected(
            "managed issue branch collides remotely".into(),
        ));
    }
    let worktree = worktree_path.to_string_lossy().into_owned();
    if !reuse_existing {
        safe_git(
            &project_root,
            None,
            &["worktree", "add", "-b", &branch, &worktree, &base_sha],
            None,
            MUTATION_TIMEOUT,
        )
        .await?;
    }
    let git_dir = safe_git(
        worktree_path,
        None,
        &["rev-parse", "--absolute-git-dir"],
        None,
        READ_TIMEOUT,
    )
    .await?;
    let common_dir = safe_git(
        worktree_path,
        None,
        &["rev-parse", "--git-common-dir"],
        None,
        READ_TIMEOUT,
    )
    .await?;
    let git_dir = canonical_directory(Path::new(git_dir.trim()), "Git directory")?;
    let common_dir = resolve_git_path(worktree_path, common_dir.trim())?;
    let head = safe_git(
        worktree_path,
        None,
        &["rev-parse", "--verify", "HEAD"],
        None,
        READ_TIMEOUT,
    )
    .await?;
    let worktree_metadata = std::fs::metadata(worktree_path)
        .map_err(|_| Error::Rejected("managed worktree disappeared".into()))?;
    let git_dir_metadata = std::fs::metadata(&git_dir)
        .map_err(|_| Error::Rejected("managed Git directory disappeared".into()))?;
    let common_dir_metadata = std::fs::metadata(&common_dir)
        .map_err(|_| Error::Rejected("managed common Git directory disappeared".into()))?;
    Ok(ManagedChangeRecord {
        project_id: project.id.clone(),
        task_id: task_id.clone(),
        agent_id: agent_id.clone(),
        worktree,
        branch,
        git_dir: git_dir.to_string_lossy().into_owned(),
        common_dir: common_dir.to_string_lossy().into_owned(),
        worktree_device: worktree_metadata.dev(),
        worktree_inode: worktree_metadata.ino(),
        git_dir_device: git_dir_metadata.dev(),
        git_dir_inode: git_dir_metadata.ino(),
        common_dir_device: common_dir_metadata.dev(),
        common_dir_inode: common_dir_metadata.ino(),
        base_sha,
        head_sha: head.trim().to_owned(),
        published_head_sha: None,
        state: "active".into(),
    })
}

/// Removes a worktree that was provisioned before its managed row could be
/// committed. The caller invokes this only for that failed registration, so
/// the exact derived branch and path cannot become an unauthenticated live
/// change after a transaction rollback.
pub async fn discard_unregistered_change(
    project_root: &Path,
    record: &ManagedChangeRecord,
) -> Result<(), Error> {
    let project_root = canonical_directory(project_root, "project root")?;
    let worktree = canonical_directory(Path::new(&record.worktree), "managed worktree")?;
    let path = worktree.to_string_lossy().into_owned();
    safe_git(
        &project_root,
        None,
        &["worktree", "remove", &path],
        None,
        MUTATION_TIMEOUT,
    )
    .await?;
    safe_git(
        &project_root,
        None,
        &["branch", "-D", &record.branch],
        None,
        MUTATION_TIMEOUT,
    )
    .await?;
    Ok(())
}

async fn managed_worktree_is_clean(worktree: &Path) -> Result<bool, Error> {
    if safe_git(
        worktree,
        None,
        &["diff", "--quiet", "--no-ext-diff", "--"],
        None,
        READ_TIMEOUT,
    )
    .await
    .is_err()
    {
        return Ok(false);
    }
    Ok(safe_git(
        worktree,
        None,
        &["ls-files", "--others", "--exclude-standard"],
        None,
        READ_TIMEOUT,
    )
    .await?
    .trim()
    .is_empty())
}

fn parse_remote_head(output: &str, reference: &str) -> Result<Option<String>, Error> {
    let Some(line) = output.lines().next() else {
        return Ok(None);
    };
    let mut fields = line.split_ascii_whitespace();
    let head = fields
        .next()
        .ok_or_else(|| Error::Command("malformed remote ref".into()))?;
    if fields.next() != Some(reference) || fields.next().is_some() {
        return Err(Error::Command("malformed remote ref".into()));
    }
    Ok(Some(head.to_owned()))
}

async fn trusted_remote_ls_remote(
    cwd: &Path,
    remote: &str,
    reference: &str,
) -> Result<String, Error> {
    let args = ["ls-remote", remote, reference];
    if github_slug(remote).is_some() {
        let gh = trusted_program("gh")?;
        let helper = format!(
            "credential.https://github.com.helper=!{} auth git-credential",
            gh.display()
        );
        safe_git_with_trusted_helper(cwd, None, &args, None, READ_TIMEOUT, &helper).await
    } else {
        safe_git(cwd, None, &args, None, READ_TIMEOUT).await
    }
}

impl Target {
    pub async fn validate(
        session: SessionRow,
        project: ProjectSnapshot,
        authority: RepositoryAuthority,
    ) -> Result<Self, Error> {
        Self::validate_with_change(session, project, authority, None).await
    }

    pub async fn validate_with_change(
        session: SessionRow,
        project: ProjectSnapshot,
        authority: RepositoryAuthority,
        registered_change: Option<ManagedChangeRecord>,
    ) -> Result<Self, Error> {
        let authority = validate_authority(authority.remote_url, authority.base_branch)?;
        if let Some(change) = &registered_change {
            if change.project_id != session.project_id || change.agent_id != session.agent_id {
                return Err(Error::Rejected(
                    "managed change owner identity changed".into(),
                ));
            }
        }
        let worktree_path = registered_change.as_ref().map_or_else(
            || session.worktree.as_str(),
            |change| change.worktree.as_str(),
        );
        let worktree = canonical_directory(Path::new(worktree_path), "managed worktree")?;
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
        let git_dir = safe_git(
            &worktree,
            None,
            &["rev-parse", "--absolute-git-dir"],
            None,
            READ_TIMEOUT,
        )
        .await?;
        let git_dir = canonical_directory(Path::new(git_dir.trim()), "Git directory")?;
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
        if let Some(change) = &registered_change {
            if Path::new(&change.git_dir) != git_dir || Path::new(&change.common_dir) != common_dir
            {
                return Err(Error::Rejected(
                    "managed Git directory identity changed".into(),
                ));
            }
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
        let expected = registered_change.as_ref().map_or_else(
            || format!("agent/{}", session.agent_id),
            |change| change.branch.clone(),
        );
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
        let git_dir_metadata = std::fs::metadata(&git_dir)
            .map_err(|_| Error::Rejected("session Git directory disappeared".into()))?;
        let common_dir_metadata = std::fs::metadata(&common_dir)
            .map_err(|_| Error::Rejected("common Git directory disappeared".into()))?;
        if let Some(change) = &registered_change {
            if metadata.dev() != change.worktree_device
                || metadata.ino() != change.worktree_inode
                || git_dir_metadata.dev() != change.git_dir_device
                || git_dir_metadata.ino() != change.git_dir_inode
                || common_dir_metadata.dev() != change.common_dir_device
                || common_dir_metadata.ino() != change.common_dir_inode
            {
                return Err(Error::Rejected(
                    "managed filesystem identity changed".into(),
                ));
            }
        }
        let github_repo = github_slug(&authority.remote_url);
        Ok(Self {
            project_id: session.project_id,
            agent_id: session.agent_id,
            session_id: session.id,
            worktree,
            branch,
            git_dir,
            common_dir,
            head: head.trim().to_owned(),
            worktree_device: metadata.dev(),
            worktree_inode: metadata.ino(),
            git_dir_device: git_dir_metadata.dev(),
            git_dir_inode: git_dir_metadata.ino(),
            common_dir_device: common_dir_metadata.dev(),
            common_dir_inode: common_dir_metadata.ino(),
            authority,
            github_repo,
            gh_program: trusted_program("gh")?,
            registered_change,
            #[cfg(test)]
            test_push_race: std::sync::Mutex::new(None),
        })
    }

    async fn revalidate(&self) -> Result<String, Error> {
        let metadata = std::fs::metadata(&self.worktree)
            .map_err(|_| Error::Rejected("session worktree disappeared".into()))?;
        if metadata.dev() != self.worktree_device || metadata.ino() != self.worktree_inode {
            return Err(Error::Rejected("session worktree identity changed".into()));
        }
        let actual_git_dir = safe_git(
            &self.worktree,
            None,
            &["rev-parse", "--absolute-git-dir"],
            None,
            READ_TIMEOUT,
        )
        .await?;
        let actual_git_dir =
            canonical_directory(Path::new(actual_git_dir.trim()), "Git directory")?;
        let actual_common = safe_git(
            &self.worktree,
            None,
            &["rev-parse", "--git-common-dir"],
            None,
            READ_TIMEOUT,
        )
        .await?;
        let actual_common = resolve_git_path(&self.worktree, actual_common.trim())?;
        if actual_git_dir != self.git_dir || actual_common != self.common_dir {
            return Err(Error::Rejected(
                "session Git directory identity changed".into(),
            ));
        }
        let git_metadata = std::fs::metadata(&actual_git_dir)
            .map_err(|_| Error::Rejected("session Git directory disappeared".into()))?;
        let common_metadata = std::fs::metadata(&actual_common)
            .map_err(|_| Error::Rejected("common Git directory disappeared".into()))?;
        if git_metadata.dev() != self.git_dir_device
            || git_metadata.ino() != self.git_dir_inode
            || common_metadata.dev() != self.common_dir_device
            || common_metadata.ino() != self.common_dir_inode
        {
            return Err(Error::Rejected(
                "session Git directory identity changed".into(),
            ));
        }
        let git_dir = self.git_dir.to_string_lossy();
        let branch = safe_git(
            &self.worktree,
            None,
            &[
                "--git-dir",
                &git_dir,
                "symbolic-ref",
                "--quiet",
                "--short",
                "HEAD",
            ],
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
            &["--git-dir", &git_dir, "rev-parse", "--verify", "HEAD"],
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
                "--git-dir",
                &self.git_dir.to_string_lossy(),
                "update-ref",
                &format!("refs/heads/{}", self.branch),
                oid.trim(),
                &self.head,
            ],
            None,
            MUTATION_TIMEOUT,
        )
        .await?;
        if self.revalidate().await? != oid.trim() {
            return Err(Error::Rejected(
                "published HEAD did not match the committed object".into(),
            ));
        }
        Ok(oid.trim().to_owned())
    }

    pub async fn push(&self) -> Result<String, Error> {
        let head = self.revalidate().await?;
        let sandbox = self.sandbox().await?;
        let refspec = format!("refs/heads/{0}:refs/heads/{0}", self.branch);
        if let Some(change) = &self.registered_change {
            if !self.worktree_is_clean(&sandbox).await? {
                return Err(Error::Rejected(
                    "managed change worktree is dirty; commit before publishing".into(),
                ));
            }
            if sandbox
                .git(
                    &["merge-base", "--is-ancestor", &change.base_sha, &head],
                    None,
                    READ_TIMEOUT,
                )
                .await
                .is_err()
            {
                return Err(Error::Rejected(
                    "managed change HEAD is not based on its registered base".into(),
                ));
            }
            if let Some(published) = &change.published_head_sha {
                if sandbox
                    .git(
                        &["merge-base", "--is-ancestor", published, &head],
                        None,
                        READ_TIMEOUT,
                    )
                    .await
                    .is_err()
                {
                    return Err(Error::Rejected(
                        "managed change HEAD rewrites its published history".into(),
                    ));
                }
            }
            let base = self
                .remote_head(
                    &sandbox,
                    &format!("refs/heads/{}", self.authority.base_branch),
                )
                .await?;
            if base.as_deref() != Some(change.base_sha.as_str()) {
                return Err(Error::Rejected(
                    "managed change base is stale; re-register from the current base".into(),
                ));
            }
            let remote = self
                .remote_head(&sandbox, &format!("refs/heads/{}", self.branch))
                .await?;
            if remote.as_deref() == Some(head.as_str()) {
                return Ok(self.branch.clone());
            }
            if remote.as_deref() != change.published_head_sha.as_deref() {
                return Err(Error::Rejected(
                    "managed change remote branch is stale or collides with an existing branch"
                        .into(),
                ));
            }
        }
        let managed_lease = self.registered_change.as_ref().map(|change| {
            format!(
                "--force-with-lease=refs/heads/{}:{}",
                self.branch,
                change.published_head_sha.as_deref().unwrap_or_default()
            )
        });
        #[cfg(test)]
        let test_push_race = { self.test_push_race.lock().unwrap().take() };
        #[cfg(test)]
        if let Some((remote, branch, sha)) = test_push_race {
            let reference = format!("refs/heads/{branch}");
            safe_git(
                &remote,
                None,
                &["update-ref", &reference, &sha],
                None,
                MUTATION_TIMEOUT,
            )
            .await?;
        }
        if self.github_repo.is_some() {
            let helper = format!(
                "credential.https://github.com.helper=!{} auth git-credential",
                self.gh_program.display()
            );
            let mut args = vec!["push", "--porcelain"];
            if let Some(lease) = managed_lease.as_deref() {
                args.push(lease);
            }
            args.push(&self.authority.remote_url);
            args.push(&refspec);
            safe_git_with_trusted_helper(
                &self.worktree,
                Some(&sandbox),
                &args,
                None,
                MUTATION_TIMEOUT,
                &helper,
            )
            .await?;
        } else {
            let mut args = vec!["push", "--porcelain"];
            if let Some(lease) = managed_lease.as_deref() {
                args.push(lease);
            }
            args.push(&self.authority.remote_url);
            args.push(&refspec);
            sandbox.git(&args, None, MUTATION_TIMEOUT).await?;
        }
        if self.revalidate().await? != head {
            return Err(Error::Rejected(
                "HEAD changed while push was running".into(),
            ));
        }
        let remote = self
            .remote_head(&sandbox, &format!("refs/heads/{}", self.branch))
            .await?;
        if remote.as_deref() != Some(head.as_str()) {
            return Err(Error::Rejected(
                "remote branch did not match the published managed commit".into(),
            ));
        }
        Ok(self.branch.clone())
    }

    async fn remote_head(
        &self,
        sandbox: &GitSandbox,
        reference: &str,
    ) -> Result<Option<String>, Error> {
        let args = ["ls-remote", self.authority.remote_url.as_str(), reference];
        let output = if self.github_repo.is_some() {
            let helper = format!(
                "credential.https://github.com.helper=!{} auth git-credential",
                self.gh_program.display()
            );
            safe_git_with_trusted_helper(
                &self.worktree,
                Some(sandbox),
                &args,
                None,
                READ_TIMEOUT,
                &helper,
            )
            .await?
        } else {
            sandbox.git(&args, None, READ_TIMEOUT).await?
        };
        let Some(line) = output.lines().next() else {
            return Ok(None);
        };
        let mut fields = line.split_ascii_whitespace();
        let head = fields
            .next()
            .ok_or_else(|| Error::Command("malformed remote ref".into()))?;
        if fields.next() != Some(reference) || fields.next().is_some() {
            return Err(Error::Command("malformed remote ref".into()));
        }
        Ok(Some(head.to_owned()))
    }

    pub fn registered_change(&self) -> Option<&ManagedChangeRecord> {
        self.registered_change.as_ref()
    }

    #[cfg(test)]
    fn with_push_race(mut self, remote: PathBuf, branch: &str, sha: &str) -> Self {
        *self.test_push_race.get_mut().unwrap() = Some((remote, branch.to_owned(), sha.to_owned()));
        self
    }

    #[cfg(test)]
    fn with_test_github(
        mut self,
        authority: RepositoryAuthority,
        program: PathBuf,
        repository: &str,
    ) -> Self {
        self.authority = authority;
        self.gh_program = program;
        self.github_repo = Some(repository.to_owned());
        self
    }

    pub async fn revalidate_head_for_audit(&self) -> Result<String, Error> {
        self.revalidate().await
    }

    pub async fn ensure_abandonable(&self) -> Result<(), Error> {
        let Some(change) = &self.registered_change else {
            return Err(Error::Rejected("no managed change is registered".into()));
        };
        let head = self.revalidate().await?;
        let sandbox = self.sandbox().await?;
        if !self.worktree_is_clean(&sandbox).await? {
            return Err(Error::Rejected(
                "cannot abandon a dirty managed change worktree".into(),
            ));
        }
        let remote = self
            .remote_head(&sandbox, &format!("refs/heads/{}", self.branch))
            .await?;
        if head != change.base_sha && change.published_head_sha.as_deref() != Some(head.as_str()) {
            return Err(Error::Rejected(
                "cannot abandon unpublished managed commits".into(),
            ));
        }
        if remote != change.published_head_sha {
            return Err(Error::Rejected(
                "cannot abandon with a stale remote branch".into(),
            ));
        }
        Ok(())
    }

    async fn worktree_is_clean(&self, sandbox: &GitSandbox) -> Result<bool, Error> {
        if sandbox
            .git(
                &["diff", "--quiet", "--no-ext-diff", "--"],
                None,
                READ_TIMEOUT,
            )
            .await
            .is_err()
        {
            return Ok(false);
        }
        Ok(sandbox
            .git(
                &["ls-files", "--others", "--exclude-standard"],
                None,
                READ_TIMEOUT,
            )
            .await?
            .trim()
            .is_empty())
    }

    pub async fn pr_open(&self, title: &str, body: &str) -> Result<String, Error> {
        validate_pr_text(title, body)?;
        let repo = self.github_repo.as_deref().ok_or_else(|| {
            Error::Rejected("pull requests require a configured GitHub HTTPS remote".into())
        })?;
        let head = self.ensure_published_pr_head().await?;
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
                "headRefName,baseRefName,headRefOid,url",
                "--jq",
                "[.headRefName,.baseRefName,.headRefOid,.url]|@tsv",
            ],
            READ_TIMEOUT,
        )
        .await?;
        let url = verify_pr_sha(&verified, &self.branch, &self.authority.base_branch, &head)?;
        self.ensure_published_pr_head().await?;
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
        let head = self.ensure_published_pr_head().await?;
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
                "headRefName,baseRefName,headRefOid,url",
                "--jq",
                "[.headRefName,.baseRefName,.headRefOid,.url]|@tsv",
            ],
            READ_TIMEOUT,
        )
        .await?;
        verify_pr_sha(&before, &self.branch, &self.authority.base_branch, &head)?;
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
                "headRefName,baseRefName,headRefOid,url",
                "--jq",
                "[.headRefName,.baseRefName,.headRefOid,.url]|@tsv",
            ],
            READ_TIMEOUT,
        )
        .await?;
        verify_pr_sha(&after, &self.branch, &self.authority.base_branch, &head)?;
        self.ensure_published_pr_head().await?;
        Ok(number_text)
    }

    async fn ensure_published_pr_head(&self) -> Result<String, Error> {
        let head = self.revalidate().await?;
        let Some(change) = &self.registered_change else {
            return Ok(head);
        };
        if change.published_head_sha.as_deref() != Some(head.as_str()) {
            return Err(Error::Rejected(
                "publish the exact local HEAD before changing its PR".into(),
            ));
        }
        let sandbox = self.sandbox().await?;
        let remote = self
            .remote_head(&sandbox, &format!("refs/heads/{}", self.branch))
            .await?;
        if remote.as_deref() != Some(head.as_str()) {
            return Err(Error::Rejected(
                "PR target branch is not at the exact published HEAD".into(),
            ));
        }
        Ok(head)
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

fn verify_pr_sha(value: &str, head: &str, base: &str, expected_sha: &str) -> Result<String, Error> {
    let mut fields = value.trim().split('\t');
    let actual_head = fields.next().unwrap_or_default();
    let actual_base = fields.next().unwrap_or_default();
    let actual_sha = fields.next().unwrap_or_default();
    let url = fields.next().unwrap_or_default();
    if actual_head != head
        || actual_base != base
        || actual_sha != expected_sha
        || !url.starts_with("https://github.com/")
    {
        return Err(Error::Rejected(
            "PR identity or published commit changed during the request".into(),
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

fn trusted_program(name: &str) -> Result<PathBuf, Error> {
    for directory in SAFE_PATH.split(':') {
        let candidate = Path::new(directory).join(name);
        let Ok(path) = std::fs::canonicalize(candidate) else {
            continue;
        };
        let metadata = std::fs::metadata(&path)
            .map_err(|_| Error::Rejected(format!("{name} executable disappeared")))?;
        let safe_path = path
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_./-".contains(byte));
        if safe_path && metadata.is_file() && !writable_by_daemon(&metadata) {
            return Ok(path);
        }
    }
    Err(Error::Rejected(format!(
        "{name} must resolve to a non-writable executable in the daemon allowlist"
    )))
}

fn writable_by_daemon(metadata: &std::fs::Metadata) -> bool {
    let mode = metadata.permissions().mode();
    mode & 0o002 != 0
        || (metadata.uid() == rustix::process::getuid().as_raw() && mode & 0o200 != 0)
        || (metadata.gid() == rustix::process::getgid().as_raw() && mode & 0o020 != 0)
}

fn validate_program(path: &Path) -> Result<(), Error> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|_| Error::Rejected("trusted executable disappeared".into()))?;
    let metadata = std::fs::metadata(&canonical)
        .map_err(|_| Error::Rejected("trusted executable disappeared".into()))?;
    if canonical != path || !metadata.is_file() || writable_by_daemon(&metadata) {
        return Err(Error::Rejected(
            "trusted executable identity changed".into(),
        ));
    }
    Ok(())
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

async fn safe_git_with_trusted_helper(
    cwd: &Path,
    sandbox: Option<&GitSandbox>,
    args: &[&str],
    stdin: Option<&[u8]>,
    deadline: Duration,
    helper: &str,
) -> Result<String, Error> {
    safe_git_inner(cwd, sandbox, args, stdin, deadline, Some(helper)).await
}

async fn safe_git_inner(
    cwd: &Path,
    sandbox: Option<&GitSandbox>,
    args: &[&str],
    stdin: Option<&[u8]>,
    deadline: Duration,
    trusted_helper: Option<&str>,
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
    if let Some(helper) = trusted_helper {
        // This fixed daemon-selected helper exchanges credentials over its
        // pipe. No token is placed in argv or a child environment.
        fixed.extend_from_slice(&["-c", helper]);
    }
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
    validate_program(program)?;
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
) -> (Vec<u8>, bool) {
    let mut output = Vec::new();
    let mut chunk = [0u8; 8192];
    let overflow = overflow;
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) if output.len() + n <= MAX_OUTPUT_BYTES => output.extend_from_slice(&chunk[..n]),
            Ok(_) => {
                let _ = overflow.send(());
                return (output, true);
            }
        }
    }
    (output, false)
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
    let mut stdout = tokio::spawn(bounded_read(
        child.stdout.take().expect("piped stdout"),
        overflow_tx,
    ));
    let (stderr_overflow_tx, mut stderr_overflow_rx) = oneshot::channel();
    let mut stderr = tokio::spawn(bounded_read(
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
            stdout.abort();
            stderr.abort();
            return Err(Error::Command("command output exceeded its bound".into()));
        }
        Ok(Err(e)) => return Err(Error::Command(e.to_string())),
        Err(_) => {
            let _ = kill_process_group(pid, Signal::KILL);
            let _ = child.wait().await;
            stdout.abort();
            stderr.abort();
            return Err(Error::Timeout);
        }
    };
    // A successful direct child may leave a hostile descendant holding the
    // pipes open. Terminate the now-orphaned process group, then bound reader
    // completion independently instead of holding daemon work forever.
    let _ = kill_process_group(pid, Signal::KILL);
    let readers = timeout(Duration::from_secs(1), async {
        (
            (&mut stdout).await.unwrap_or_default(),
            (&mut stderr).await.unwrap_or_default(),
        )
    })
    .await;
    let (stdout_result, stderr_result) = match readers {
        Ok(output) => output,
        Err(_) => {
            stdout.abort();
            stderr.abort();
            return Err(Error::Command("command pipes did not close".into()));
        }
    };
    let (stdout, stdout_overflowed) = stdout_result;
    let (stderr, stderr_overflowed) = stderr_result;
    if stdout_overflowed || stderr_overflowed {
        return Err(Error::Command("command output exceeded its bound".into()));
    }
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

    fn session_for_target(target: &Target) -> SessionRow {
        SessionRow {
            id: target.session_id.clone(),
            project_id: target.project_id.clone(),
            agent_id: target.agent_id.clone(),
            provider: Provider::Shell,
            runtime_model: None,
            runtime_reasoning_effort: None,
            runtime_permission_mode: None,
            runtime_control_mode: None,
            provider_session_id: None,
            worktree: target.worktree.to_string_lossy().into_owned(),
            codex_home: None,
            hook_token: "a".repeat(64),
            state: SessionState::Idle,
            state_since_ms: 1,
            activity: None,
            activity_inferred: false,
            wait_reason: None,
            observer_reason: None,
            observer_health: ObserverHealth::Healthy,
            observer_health_since_ms: 1,
            notification_kind: None,
            runner_instance_id: RunnerInstanceId::try_from("runner").unwrap(),
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
            delivery_recovery_stop_requested_at_ms: None,
            current_run_id: None,
        }
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
        let session = SessionRow {
            id: "session".to_owned().try_into().unwrap(),
            project_id: "project".to_owned().try_into().unwrap(),
            agent_id: "worker".to_owned().try_into().unwrap(),
            provider: Provider::Shell,
            runtime_model: None,
            runtime_reasoning_effort: None,
            runtime_permission_mode: None,
            runtime_control_mode: None,
            provider_session_id: None,
            worktree: worktree.to_string_lossy().into_owned(),
            codex_home: None,
            hook_token: "a".repeat(64),
            state: SessionState::Idle,
            state_since_ms: 1,
            activity: None,
            activity_inferred: false,
            wait_reason: None,
            observer_reason: None,
            observer_health: ObserverHealth::Healthy,
            observer_health_since_ms: 1,
            runner_instance_id: RunnerInstanceId::try_from("runner".to_owned()).unwrap(),
            runner_runtime: "/tmp/runner".into(),
            runner_protocol_version: 1,
            last_hook_event: None,
            notification_kind: None,
            last_hook_at_ms: None,
            started_at_ms: 1,
            updated_at_ms: 1,
            ended_at_ms: None,
            exit_code: None,
            exit_signal: None,
            stop_requested_at_ms: None,
            delivery_recovery_stop_requested_at_ms: None,
            current_run_id: None,
        };
        let project = ProjectSnapshot {
            id: "project".to_owned().try_into().unwrap(),
            name: "Test".into(),
            root: repo.to_string_lossy().into_owned(),
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        let authority =
            validate_authority(remote.to_string_lossy().into_owned(), "main".into()).unwrap();
        let target = Target::validate(session, project, authority).await.unwrap();
        (temp, target, remote)
    }

    #[tokio::test]
    async fn managed_change_is_derived_and_recovers_an_exact_db_gap_retry() {
        let (temp, target, remote) = fixture().await;
        let project = ProjectSnapshot {
            id: target.project_id.clone(),
            name: "Project".into(),
            root: temp.path().join("repo").to_string_lossy().into_owned(),
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        let authority =
            validate_authority(remote.to_string_lossy().into_owned(), "main".into()).unwrap();
        let task_id = factory_core::TaskId::try_from("issue-175".to_owned()).unwrap();
        let path = temp.path().join("changes").join("issue-175");
        let record = create_managed_change(
            &project,
            authority.clone(),
            &task_id,
            &target.agent_id,
            &path,
        )
        .await
        .unwrap();
        assert_eq!(record.branch, "issue/issue-175");
        assert_eq!(record.head_sha, record.base_sha);
        assert_eq!(record.worktree, path.to_string_lossy());
        let retry = create_managed_change(&project, authority, &task_id, &target.agent_id, &path)
            .await
            .unwrap();
        assert_eq!(
            retry, record,
            "a DB-gap retry must recover the exact worktree"
        );

        run(&temp.path().join("repo"), &["checkout", "main"]);
        fs::write(temp.path().join("repo").join("README"), "advanced\n").unwrap();
        run(&temp.path().join("repo"), &["commit", "-am", "advance"]);
        run(&temp.path().join("repo"), &["push", "origin", "main"]);
        let advanced = create_managed_change(
            &project,
            validate_authority(remote.to_string_lossy().into_owned(), "main".into()).unwrap(),
            &task_id,
            &target.agent_id,
            &path,
        )
        .await
        .unwrap();
        assert_ne!(advanced.base_sha, record.base_sha);
        assert_eq!(advanced.base_sha, advanced.head_sha);
    }

    #[tokio::test]
    async fn managed_change_rejects_an_exact_path_from_another_repository() {
        let (temp, target, remote) = fixture().await;
        let project = ProjectSnapshot {
            id: target.project_id.clone(),
            name: "Project".into(),
            root: temp.path().join("repo").to_string_lossy().into_owned(),
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        let authority =
            validate_authority(remote.to_string_lossy().into_owned(), "main".into()).unwrap();
        let task_id = factory_core::TaskId::try_from("issue-175-foreign".to_owned()).unwrap();
        let path = temp.path().join("changes").join("issue-175-foreign");
        let foreign = temp.path().join("foreign");
        run(
            temp.path(),
            &["init", "-b", "main", foreign.to_str().unwrap()],
        );
        run(&foreign, &["config", "user.name", "Foreign"]);
        run(
            &foreign,
            &["config", "user.email", "foreign@example.invalid"],
        );
        fs::write(foreign.join("README"), "foreign\n").unwrap();
        run(&foreign, &["add", "README"]);
        run(&foreign, &["commit", "-m", "foreign"]);
        run(
            &foreign,
            &[
                "worktree",
                "add",
                "-b",
                "issue/issue-175-foreign",
                path.to_str().unwrap(),
            ],
        );

        let error = create_managed_change(
            &project,
            authority.clone(),
            &task_id,
            &target.agent_id,
            &path,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("another repository"));

        run(
            &foreign,
            &["worktree", "remove", "--force", path.to_str().unwrap()],
        );
        let recovered =
            create_managed_change(&project, authority, &task_id, &target.agent_id, &path)
                .await
                .unwrap();
        assert_eq!(recovered.base_sha, recovered.head_sha);
    }

    #[tokio::test]
    async fn managed_change_preserves_a_clean_divergent_recovery_branch() {
        let (temp, target, remote) = fixture().await;
        let project = ProjectSnapshot {
            id: target.project_id.clone(),
            name: "Project".into(),
            root: temp.path().join("repo").to_string_lossy().into_owned(),
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        let authority =
            validate_authority(remote.to_string_lossy().into_owned(), "main".into()).unwrap();
        let task_id = factory_core::TaskId::try_from("issue-175-divergent".to_owned()).unwrap();
        let path = temp.path().join("changes").join("issue-175-divergent");
        let record = create_managed_change(
            &project,
            authority.clone(),
            &task_id,
            &target.agent_id,
            &path,
        )
        .await
        .unwrap();
        fs::write(path.join("unique"), "must survive\n").unwrap();
        run(&path, &["add", "unique"]);
        run(&path, &["commit", "-m", "unique managed work"]);
        let unique_head = String::from_utf8(
            StdCommand::new("git")
                .current_dir(&path)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let unique_head = unique_head.trim();

        fs::write(temp.path().join("repo").join("README"), "different base\n").unwrap();
        run(
            &temp.path().join("repo"),
            &["commit", "-am", "different base"],
        );
        run(&temp.path().join("repo"), &["push", "origin", "main"]);
        let error = create_managed_change(&project, authority, &task_id, &target.agent_id, &path)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("diverged"));
        assert!(path.is_dir(), "divergent recovery worktree was removed");
        let branch_head = String::from_utf8(
            StdCommand::new("git")
                .current_dir(temp.path().join("repo"))
                .args(["rev-parse", "refs/heads/issue/issue-175-divergent"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert_eq!(branch_head.trim(), unique_head);
        assert_ne!(record.base_sha, unique_head);
    }

    #[tokio::test]
    async fn managed_change_recovery_ref_race_preserves_a_new_unique_commit() {
        let (temp, target, remote) = fixture().await;
        let project = ProjectSnapshot {
            id: target.project_id.clone(),
            name: "Project".into(),
            root: temp.path().join("repo").to_string_lossy().into_owned(),
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        let authority =
            validate_authority(remote.to_string_lossy().into_owned(), "main".into()).unwrap();
        let task_id = factory_core::TaskId::try_from("issue-175-recovery-race".to_owned()).unwrap();
        let path = temp.path().join("changes").join("issue-175-recovery-race");
        let record = create_managed_change(
            &project,
            authority.clone(),
            &task_id,
            &target.agent_id,
            &path,
        )
        .await
        .unwrap();

        fs::write(path.join("old"), "old recovery state\n").unwrap();
        run(&path, &["add", "old"]);
        run(&path, &["commit", "-m", "old recovery state"]);
        let old_head = String::from_utf8(
            StdCommand::new("git")
                .current_dir(&path)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let old_head = old_head.trim();

        run(&temp.path().join("repo"), &["checkout", "main"]);
        run(&temp.path().join("repo"), &["merge", "--ff-only", old_head]);
        fs::write(temp.path().join("repo").join("README"), "advanced\n").unwrap();
        run(
            &temp.path().join("repo"),
            &["commit", "-am", "advance base"],
        );
        run(&temp.path().join("repo"), &["push", "origin", "main"]);

        let tree = String::from_utf8(
            StdCommand::new("git")
                .current_dir(temp.path().join("repo"))
                .args(["rev-parse", &format!("{old_head}^{{tree}}")])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let unique_head = String::from_utf8(
            StdCommand::new("git")
                .current_dir(temp.path().join("repo"))
                .args([
                    "commit-tree",
                    tree.trim(),
                    "-p",
                    old_head,
                    "-m",
                    "concurrent unique",
                ])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let unique_head = unique_head.trim().to_owned();
        *RECOVERY_REF_RACE.lock().unwrap() = Some((
            temp.path().join("repo"),
            record.branch.clone(),
            unique_head.clone(),
        ));

        let error = create_managed_change(&project, authority, &task_id, &target.agent_id, &path)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("changed during recovery"),
            "{error}"
        );
        assert!(!path.exists(), "recovery removed the old worktree");

        let branch_head = String::from_utf8(
            StdCommand::new("git")
                .current_dir(temp.path().join("repo"))
                .args(["rev-parse", &record.branch])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert_eq!(branch_head.trim(), unique_head);
        assert!(
            StdCommand::new("git")
                .current_dir(temp.path().join("repo"))
                .args(["cat-file", "-e", &format!("{unique_head}^{{commit}}")])
                .status()
                .unwrap()
                .success()
        );
    }

    #[tokio::test]
    async fn managed_change_rejects_a_same_repository_leaf_symlink() {
        let (temp, target, remote) = fixture().await;
        let project = ProjectSnapshot {
            id: target.project_id.clone(),
            name: "Project".into(),
            root: temp.path().join("repo").to_string_lossy().into_owned(),
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        let authority =
            validate_authority(remote.to_string_lossy().into_owned(), "main".into()).unwrap();
        let task_id = factory_core::TaskId::try_from("issue-175-symlink".to_owned()).unwrap();
        let path = temp.path().join("changes").join("issue-175-symlink");
        let outside = temp.path().join("outside");
        run(
            &temp.path().join("repo"),
            &[
                "worktree",
                "add",
                "-b",
                "issue/issue-175-symlink",
                outside.to_str().unwrap(),
            ],
        );
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        let error = create_managed_change(
            &project,
            authority.clone(),
            &task_id,
            &target.agent_id,
            &path,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("leaf"));

        fs::remove_file(&path).unwrap();
        run(
            &temp.path().join("repo"),
            &["worktree", "remove", "--force", outside.to_str().unwrap()],
        );
        run(
            &temp.path().join("repo"),
            &["branch", "-D", "issue/issue-175-symlink"],
        );
        let recovered =
            create_managed_change(&project, authority, &task_id, &target.agent_id, &path)
                .await
                .unwrap();
        assert_eq!(recovered.base_sha, recovered.head_sha);
    }

    #[tokio::test]
    async fn managed_change_publish_is_clean_idempotent_and_non_rewriting() {
        let (temp, target, remote) = fixture().await;
        let project = ProjectSnapshot {
            id: target.project_id.clone(),
            name: "Project".into(),
            root: temp.path().join("repo").to_string_lossy().into_owned(),
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        let authority =
            validate_authority(remote.to_string_lossy().into_owned(), "main".into()).unwrap();
        let task_id = factory_core::TaskId::try_from("issue-175-publish".to_owned()).unwrap();
        let path = temp.path().join("changes").join("issue-175-publish");
        let record = create_managed_change(
            &project,
            authority.clone(),
            &task_id,
            &target.agent_id,
            &path,
        )
        .await
        .unwrap();
        let session = SessionRow {
            id: target.session_id.clone(),
            project_id: target.project_id.clone(),
            agent_id: target.agent_id.clone(),
            provider: Provider::Shell,
            runtime_model: None,
            runtime_reasoning_effort: None,
            runtime_permission_mode: None,
            runtime_control_mode: None,
            provider_session_id: None,
            worktree: target.worktree.to_string_lossy().into_owned(),
            codex_home: None,
            hook_token: "a".repeat(64),
            state: SessionState::Idle,
            state_since_ms: 1,
            activity: None,
            activity_inferred: false,
            wait_reason: None,
            observer_reason: None,
            observer_health: ObserverHealth::Healthy,
            observer_health_since_ms: 1,
            notification_kind: None,
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
            delivery_recovery_stop_requested_at_ms: None,
            current_run_id: None,
        };
        let registered = Target::validate_with_change(
            session,
            project.clone(),
            authority.clone(),
            Some(record.clone()),
        )
        .await
        .unwrap();
        registered.commit("first managed commit").await.unwrap();
        let registered =
            registered.with_push_race(remote.clone(), "issue/issue-175-publish", &record.base_sha);
        let raced = registered.push().await.unwrap_err();
        assert!(!raced.to_string().is_empty());
        let remote_after_race = StdCommand::new("git")
            .args([
                "--git-dir",
                remote.to_str().unwrap(),
                "show-ref",
                "--hash",
                "--verify",
                "refs/heads/issue/issue-175-publish",
            ])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&remote_after_race.stdout).trim(),
            record.base_sha
        );
        run(
            &remote,
            &[
                "update-ref",
                "refs/heads/issue/issue-175-publish",
                &record.base_sha,
            ],
        );
        let stale = registered.push().await.unwrap_err();
        assert!(stale.to_string().contains("stale or collides"));
        run(
            &remote,
            &["update-ref", "-d", "refs/heads/issue/issue-175-publish"],
        );
        registered.push().await.unwrap();
        registered.push().await.unwrap();
        std::fs::write(path.join("dirty"), "must not publish\n").unwrap();
        let dirty = Target::validate_with_change(
            SessionRow {
                id: target.session_id.clone(),
                project_id: target.project_id.clone(),
                agent_id: target.agent_id.clone(),
                provider: Provider::Shell,
                runtime_model: None,
                runtime_reasoning_effort: None,
                runtime_permission_mode: None,
                runtime_control_mode: None,
                provider_session_id: None,
                worktree: target.worktree.to_string_lossy().into_owned(),
                codex_home: None,
                hook_token: "a".repeat(64),
                state: SessionState::Idle,
                state_since_ms: 1,
                activity: None,
                activity_inferred: false,
                wait_reason: None,
                observer_reason: None,
                observer_health: ObserverHealth::Healthy,
                observer_health_since_ms: 1,
                notification_kind: None,
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
                delivery_recovery_stop_requested_at_ms: None,
                current_run_id: None,
            },
            project,
            authority,
            Some(record),
        )
        .await
        .unwrap();
        let error = dirty.push().await.unwrap_err();
        assert!(error.to_string().contains("dirty"));
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
    async fn mutation_rejects_replaced_linked_worktree_gitdir() {
        let (temp, target, _) = fixture().await;
        let attacker = temp.path().join("attacker.git");
        run(temp.path(), &["init", "--bare", attacker.to_str().unwrap()]);
        fs::create_dir_all(attacker.join("objects/info")).unwrap();
        fs::write(
            attacker.join("objects/info/alternates"),
            format!("{}\n", target.common_dir.join("objects").display()),
        )
        .unwrap();
        run(
            temp.path(),
            &[
                "--git-dir",
                attacker.to_str().unwrap(),
                "update-ref",
                "refs/heads/agent/worker",
                &target.head,
            ],
        );
        fs::write(attacker.join("HEAD"), "ref: refs/heads/agent/worker\n").unwrap();
        fs::write(
            target.worktree.join(".git"),
            format!("gitdir: {}\n", attacker.display()),
        )
        .unwrap();
        fs::write(target.worktree.join("README"), "attacker\n").unwrap();

        let error = target.commit("must fail closed").await.unwrap_err();
        assert!(error.to_string().contains("Git directory identity changed"));
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
    async fn bounded_runner_closes_pipes_retained_by_a_descendant() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("retained-pipe");
        fs::write(&script, "#!/bin/sh\n(sleep 30) &\nexit 0\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let started = std::time::Instant::now();
        run_command(
            &script,
            temp.path(),
            &[],
            &[],
            None,
            Duration::from_secs(15),
        )
        .await
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn gh_is_pinned_to_configured_repo_base_and_reverified_after_mutation() {
        let (temp, target, _) = fixture().await;
        let authority =
            validate_authority("https://github.com/owner/repo.git".into(), "main".into()).unwrap();
        let fake = temp.path().join("gh");
        let log = temp.path().join("gh.log");
        fs::write(&fake, format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$1 $2\" in\n 'pr view') printf 'agent/worker\\tmain\\t{}\\thttps://github.com/owner/repo/pull/7\\n' ;;\nesac\n", log.display(), target.head)).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o500)).unwrap();
        let target = target.with_test_github(
            authority,
            std::fs::canonicalize(fake).unwrap(),
            "owner/repo",
        );
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

    #[tokio::test]
    async fn managed_change_pr_uses_the_registered_published_head() {
        let (temp, target, remote) = fixture().await;
        let project = ProjectSnapshot {
            id: target.project_id.clone(),
            name: "Project".into(),
            root: temp.path().join("repo").to_string_lossy().into_owned(),
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        let authority =
            validate_authority(remote.to_string_lossy().into_owned(), "main".into()).unwrap();
        let task_id = factory_core::TaskId::try_from("issue-175-pr".to_owned()).unwrap();
        let path = temp.path().join("changes").join("issue-175-pr");
        let record = create_managed_change(
            &project,
            authority.clone(),
            &task_id,
            &target.agent_id,
            &path,
        )
        .await
        .unwrap();
        let publishing_target = Target::validate_with_change(
            session_for_target(&target),
            project.clone(),
            authority.clone(),
            Some(record.clone()),
        )
        .await
        .unwrap();
        let head = publishing_target.commit("managed PR commit").await.unwrap();
        publishing_target.push().await.unwrap();

        let mut published = record;
        published.head_sha = head.clone();
        published.published_head_sha = Some(head.clone());
        let fake = temp.path().join("gh-managed");
        let log = temp.path().join("gh-managed.log");
        fs::write(
            &fake,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$1 $2\" in\n 'pr view') printf 'issue/issue-175-pr\\tmain\\t{}\\thttps://github.com/owner/repo/pull/8\\n' ;;\nesac\n",
                log.display(), head
            ),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o500)).unwrap();
        let target = Target::validate_with_change(
            session_for_target(&target),
            project,
            authority.clone(),
            Some(published),
        )
        .await
        .unwrap()
        .with_test_github(authority, fs::canonicalize(fake).unwrap(), "owner/repo");
        assert_eq!(
            target
                .pr_open("Managed title", "Managed body")
                .await
                .unwrap(),
            "https://github.com/owner/repo/pull/8"
        );
        let calls = fs::read_to_string(log).unwrap();
        assert!(calls.contains("pr create --repo owner/repo --head issue/issue-175-pr"));
        assert!(calls.contains("pr view issue/issue-175-pr --repo owner/repo"));
    }
}
