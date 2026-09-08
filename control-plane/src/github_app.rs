#![cfg_attr(test, allow(dead_code))]

use std::{collections::BTreeMap, sync::Arc};

use base64::{Engine as _, engine::general_purpose};
use serde::{Deserialize, Serialize};
#[cfg(any(target_arch = "wasm32", test))]
use sha2::{Digest as _, Sha256};
use zeroize::{Zeroize as _, Zeroizing};

#[cfg(target_arch = "wasm32")]
use crate::journal::{DeliveryJournal, Operation, OperationRecord, OperationTransition};
use crate::maintainer::MAX_EXACT_INTEGER;

pub(crate) const PRIVATE_KEY_BINDING: &str = "DARK_FACTORY_MAINTAINER_PRIVATE_KEY_PKCS8";
pub(crate) const PERMISSION_REVISION_BINDING: &str = "DARK_FACTORY_MAINTAINER_PERMISSION_REVISION";
pub(crate) const PERMISSION_REVISION: &str = "maintainer-operations-v6";
const GITHUB_API_VERSION: &str = "2026-03-10";
// GitHub list endpoints below request at most 100 records. Issue comments and
// review bodies can each be 65,536 characters, so a webhook-sized 64 KiB cap
// rejected valid bounded pages before their typed count checks could run.
const MAX_GITHUB_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Publication bounds. A commit is a bounded, reviewable unit of work, not a
/// bulk upload channel, and the Worker must hold every blob in memory.
const MAX_COMMIT_FILES: usize = 50;
const MAX_COMMIT_FILE_BYTES: usize = 1_000_000;
const MAX_ISSUE_COMMENT_PAGES: usize = 10;
const MAX_ISSUE_COMMENTS_PER_PAGE: usize = 100;
const MAX_WORKFLOW_RUNS: usize = 20;
const MAX_WORKFLOW_JOBS: usize = 100;
const MAX_WORKFLOW_STEPS: usize = 100;
const MAX_JOB_LOG_BYTES: usize = 64 * 1024;
const MAX_LOG_REDIRECT_BYTES: usize = 4_096;
#[derive(Clone, Copy)]
struct WorkflowRef<'a> {
    api_id: &'a str,
    response_path: &'a str,
}

impl<'a> WorkflowRef<'a> {
    /// A caller-named workflow. `valid_workflow_path` has already proven the
    /// shape, so the API id is exactly the file name the path ends with, and
    /// the two halves cannot disagree because both are read from one string.
    fn requested(path: &'a str) -> Result<Self, OperationError> {
        let api_id = path
            .strip_prefix(".github/workflows/")
            .ok_or(OperationError::InvalidInput)?;
        Ok(Self {
            api_id,
            response_path: path,
        })
    }
}

const RELEASE_WORKFLOW: WorkflowRef<'static> = WorkflowRef {
    api_id: "release.yml",
    response_path: ".github/workflows/release.yml",
};
const DEPLOY_WORKFLOW: WorkflowRef<'static> = WorkflowRef {
    api_id: "deploy-control-plane.yml",
    response_path: ".github/workflows/deploy-control-plane.yml",
};

#[derive(Clone)]
pub(crate) struct AppAuthority(Arc<Authority>);

struct Authority {
    app_id: i64,
    private_key: PrivateKey,
}

struct PrivateKey(Vec<u8>);

impl Drop for PrivateKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum Error {
    #[error("maintainer App authority configuration is invalid")]
    Configuration,
    #[error("maintainer App authority is unavailable")]
    Unavailable,
    /// GitHub answered, and its status is the answer. Endpoints report *why*
    /// they refused, and collapsing that into "unavailable" turns a
    /// determinate refusal into a reconciliation the caller cannot resolve.
    /// Which statuses a given endpoint uses to refuse is the caller's to
    /// classify, not this type's.
    #[error("github rejected the request with status {0}")]
    Rejected(u16),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum OperationError {
    #[error("maintainer operation input is invalid")]
    InvalidInput,
    #[error("operation ID is already bound to a different request")]
    Conflict,
    /// The request was refused determinately. Distinct from `Indeterminate`,
    /// which means the outcome is genuinely unknown. Most refusals leave the
    /// operation ID retryable once the precondition the reason names actually
    /// holds — but not all: `TreeTruncated` names a fact about the commit that
    /// will be just as true next time, and reads reach this variant too, so it
    /// is not only about mutations. The reason carries which of those it is,
    /// which is why the wrapper text states the refusal and promises nothing. The reason rides along because an
    /// untyped outcome has now cost two diagnosis cycles (#371): "the token
    /// lacks a permission", "the queue rejected the entry", and "there is
    /// no queue" are three different retries, and without the reason the
    /// only way to tell them apart is reading this crate's source.
    #[error("the request was refused: {0}")]
    Refused(RefusalReason),
    #[error("operation outcome requires reconciliation")]
    Indeterminate,
    #[error("maintainer operation authority is unavailable")]
    Unavailable,
}

/// Why GitHub refused, said with typed classifications only. GitHub's
/// error text can quote caller input, so the text never rides along -- the
/// same discipline `github_graphql` applies to its logging.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum RefusalReason {
    /// The mutation was rejected before execution, and these are the typed
    /// error classes GitHub returned at the mutation root.
    #[error("rejected before execution as {0}")]
    Rejected(RejectionKinds),
    /// The mutation answered with neither an effect nor an error.
    #[error("answered with neither an effect nor an error")]
    NoEffect,
    /// The queue read answered, and its answer carried no merge queue for
    /// the base branch -- stated as the observation, because `entries:
    /// None` is also the shape of a null repository. The decision doc
    /// calls a queueless branch unsupported and fails closed rather than
    /// falling back to a merge.
    #[error("the queue read found no merge queue on the base branch")]
    NoMergeQueue,
    #[error("the workflow run is not a completed failure")]
    RunNotFailed,
    #[error("the workflow job is not a completed failure")]
    JobNotFailed,
    #[error("the pull request was already queued before this operation claimed it")]
    AlreadyQueued,
    /// The App is not installed on the named repository, or the installation
    /// cannot see it. Distinguished from a mutation's own `NOT_FOUND` because
    /// on a surface where the caller names the repository this is the likeliest
    /// mistake, and reporting it as a mutation refusal sends the caller looking
    /// at the issue or pull request instead of at the installation.
    #[error("the App is not installed on the named repository")]
    RepositoryNotInstalled,
    /// The installation exists but does not carry the authority this revision
    /// mints, and the value names the first field that failed. Readiness cannot
    /// report this: it names no repository, so no installation is audited until
    /// one is used, which makes the using caller the only one who can be told.
    #[error("the installation is not usable: {0}")]
    InstallationRejected(&'static str),
    /// GitHub truncated the tree it returned, so the listing is incomplete.
    /// Determinate: the same commit truncates every time.
    #[error("the commit's tree is too large for GitHub to return whole")]
    TreeTruncated,
    #[error("the direct merge preconditions are not satisfied")]
    MergePreconditions,
    #[error("the pull request checks are not complete and successful")]
    MergeChecks,
    #[error("the required review did not allow this pull request head")]
    MergeReview,
    #[error("the pull request head conflicted with the merge request")]
    MergeHeadConflict,
    #[error("github refused the merge with status {0}")]
    MergeRejected(u16),
}

/// Which pre-execution rejection classes appeared. More than one can:
/// GitHub reports one error per problem, so the set is carried whole
/// rather than collapsed to whichever arrived first.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RejectionKinds {
    not_found: bool,
    forbidden: bool,
    unprocessable: bool,
    rate_limited: bool,
}

impl std::fmt::Display for RejectionKinds {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut separate = false;
        for (present, name) in [
            (self.not_found, "NOT_FOUND"),
            (self.forbidden, "FORBIDDEN"),
            (self.unprocessable, "UNPROCESSABLE"),
            (self.rate_limited, "RATE_LIMITED"),
        ] {
            if present {
                if separate {
                    formatter.write_str("+")?;
                }
                formatter.write_str(name)?;
                separate = true;
            }
        }
        if !separate {
            // No real path constructs the empty set: `classify_graphql_errors`
            // records a class for every error it accepts and refuses an empty
            // error array earlier. But the derived `Default` is
            // crate-visible, so the impossible value must at least read as
            // what it is rather than trailing off mid-sentence.
            formatter.write_str("no recorded class")?;
        }
        Ok(())
    }
}

impl From<Error> for OperationError {
    fn from(error: Error) -> Self {
        match error {
            Error::Rejected(status) => rejection_for_status(status)
                .map(RefusalReason::Rejected)
                .map(Self::Refused)
                .unwrap_or(Self::Unavailable),
            Error::Configuration | Error::Unavailable => Self::Unavailable,
        }
    }
}

fn rejection_for_status(status: u16) -> Option<RejectionKinds> {
    let mut kinds = RejectionKinds::default();
    match status {
        404 => kinds.not_found = true,
        401 | 403 => kinds.forbidden = true,
        409 | 422 => kinds.unprocessable = true,
        429 => kinds.rate_limited = true,
        _ => return None,
    }
    Some(kinds)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreatePullRequest {
    pub(crate) repository: String,
    pub(crate) operation_id: String,
    pub(crate) issue_number: i64,
    pub(crate) head: String,
    pub(crate) head_sha: String,
    pub(crate) base: String,
    pub(crate) base_sha: String,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) draft: bool,
}

/// Replace an open pull request's body only while it still names the exact
/// head the caller reviewed. The App adds its own operation marker so a lost
/// response can be reconciled without adopting a different edit.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdatePullRequestBody {
    pub(crate) repository: String,
    pub(crate) operation_id: String,
    pub(crate) pull_number: i64,
    pub(crate) head_sha: String,
    pub(crate) body: String,
}

/// Close one pull request only while it still names the head the caller
/// reviewed. GitHub's close endpoint has no caller-selected state or generic
/// update fields; this surface exposes only the one terminal transition.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClosePullRequest {
    pub(crate) repository: String,
    pub(crate) operation_id: String,
    pub(crate) pull_number: i64,
    pub(crate) head_sha: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubmitPullRequestReview {
    pub(crate) repository: String,
    pub(crate) operation_id: String,
    pub(crate) pull_number: i64,
    pub(crate) head_sha: String,
    pub(crate) event: ReviewEvent,
    pub(crate) body: String,
}

/// What the reviewer concluded, which is not the same thing as which GitHub
/// review state carries it.
///
/// The review contract has three outcomes -- the reviewer is satisfied, the
/// reviewer found a blocking defect, or the reviewer left a note that decides
/// nothing -- and GitHub offers this App exactly one state to say all three in.
///
/// The constraint is one constraint, and it applies to both ends. This App
/// opens the pull requests it reviews, and GitHub refuses a self-review that
/// takes a side: not `APPROVE`, and equally not `REQUEST_CHANGES`. The
/// approval half was already understood -- it is the whole reason the review
/// became a status check instead of an approval -- but the blocking half was
/// not, and `RequestChanges` was mapped onto GitHub's real `REQUEST_CHANGES`
/// event anyway. GitHub refused every such post, reconciliation found no
/// review to adopt, and the operation journalled indeterminate, so a blocking
/// verdict was unrecordable on precisely the pull requests the gate exists to
/// gate (#377).
///
/// So all three verdicts post `COMMENT` and are recorded `COMMENTED`, and the
/// verdict itself rides in a line the App renders: `verdict` supplies the
/// word and `SubmitPullRequestReview::marked_body` writes the line. Caller
/// text carrying that prefix is refused by `free_of_review_verdict`, so a
/// caller cannot state a verdict it did not ask for -- the same discipline as
/// the operation marker, for the same reason.
///
/// `ALLOW`, `COMMENT`, and `REQUEST_CHANGES` are therefore all this App's own
/// words for its own outcomes. None of them is a GitHub review event any
/// more, and none of them reaches the wire.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum ReviewEvent {
    Allow,
    Comment,
    RequestChanges,
}

/// The `event` every verdict is submitted as, and the state GitHub records it
/// in. Constants and not a function of the verdict: which outcome a review
/// carries is readable only from the line the App writes, never from the
/// GitHub state, which is why the required `review` check reads that line.
const REVIEW_EVENT: &str = "COMMENT";
const REVIEW_STATE: &str = "COMMENTED";

impl ReviewEvent {
    /// The verdict word the required `review` check reads.
    const fn verdict(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Comment => "note",
            Self::RequestChanges => "block",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObserveRepository {
    pub(crate) repository: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObservePullRequestChecks {
    pub(crate) repository: String,
    pub(crate) pull_number: i64,
    pub(crate) head_sha: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct RepositoryResult {
    pub(crate) repository: String,
    pub(crate) repository_id: i64,
    pub(crate) default_branch: String,
    pub(crate) default_sha: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateIssue {
    pub(crate) repository: String,
    pub(crate) operation_id: String,
    pub(crate) title: String,
    pub(crate) body: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct IssueResult {
    pub(crate) number: i64,
    pub(crate) url: String,
}

/// One file's exact bytes at one exact commit.
///
/// Reading another agent's work needed a `git fetch`, which needed the
/// operator's credential in an agent process. Content at a commit is what that
/// fetch was actually for: with this and `publish_commit`, integrating someone
/// else's branch never touches git history at all.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObserveFile {
    pub(crate) repository: String,
    pub(crate) commit_sha: String,
    pub(crate) path: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct FileObservationResult {
    pub(crate) path: String,
    pub(crate) commit_sha: String,
    /// Absent when the path does not exist at that commit, which is an answer.
    /// A caller integrating a deletion needs to tell that from a failure.
    pub(crate) content_base64: Option<String>,
}

/// One commit's tree, exactly as git records it.
///
/// This deliberately does not diff. An earlier version answered "which paths
/// differ", which meant deciding what counts as added, modified, removed or
/// renamed, whether a submodule change is a change, and which file modes are
/// legal. Every one of those judgements was wrong at least once, and none of
/// them is this service's to make: a caller comparing two trees knows what it
/// wants a rename to mean. Reporting what git says, and nothing more, is both
/// smaller and harder to be wrong about.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObserveTree {
    pub(crate) repository: String,
    pub(crate) commit_sha: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct TreeEntryResult {
    pub(crate) path: String,
    /// `blob`, `tree` or `commit`, as git names them.
    pub(crate) kind: String,
    /// Git's own mode string, not an enumeration this service invents. This,
    /// not `kind`, is what decides whether an entry round-trips through
    /// `observe_file` and `publish_commit`: only `100644` and `100755` do. A
    /// symlink is a `blob` with mode `120000`, so a caller keying on `kind`
    /// would try to read one and be refused.
    pub(crate) mode: String,
    pub(crate) sha: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct TreeObservationResult {
    pub(crate) commit_sha: String,
    pub(crate) tree_sha: String,
    /// This commit's parents. Comparing two trees answers "how do these two
    /// snapshots differ", which is only the same question as "what did this
    /// branch change" when one commit is an ancestor of the other. Without a
    /// way to walk ancestry a caller cannot tell those apart, and replaying the
    /// difference onto a moved default branch silently reverts everything
    /// landed since the fork. The commit is already fetched to reach its tree,
    /// so this costs nothing.
    pub(crate) parents: Vec<String>,
    pub(crate) entries: Vec<TreeEntryResult>,
}

/// Which commit a branch points at right now.
///
/// Every other observation takes a `head_sha` as input, and `maintainer_status`
/// answers only for the default branch, so an agent working on a topic branch
/// had no way to learn its head through this App at all -- it had to ask a
/// human or reach for ambient git credentials, which is the one thing this
/// surface exists to avoid.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObserveRef {
    pub(crate) repository: String,
    pub(crate) branch: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct RefObservationResult {
    pub(crate) branch: String,
    /// Absent when the branch does not exist, which is an answer rather than a
    /// failure: it is how a caller tells "not published yet" from "moved".
    pub(crate) head_sha: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObserveIssue {
    pub(crate) repository: String,
    pub(crate) issue_number: i64,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct IssueObservationResult {
    pub(crate) number: i64,
    pub(crate) url: String,
    pub(crate) state: String,
    pub(crate) state_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IssueResolutionReason {
    Completed,
    NotPlanned,
}

impl IssueResolutionReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::NotPlanned => "not_planned",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResolveIssue {
    pub(crate) repository: String,
    pub(crate) operation_id: String,
    pub(crate) issue_number: i64,
    pub(crate) body: String,
    pub(crate) state_reason: IssueResolutionReason,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ResolveIssueResult {
    pub(crate) number: i64,
    pub(crate) url: String,
    pub(crate) comment_url: String,
    pub(crate) state: String,
    pub(crate) state_reason: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObservePullRequestMerge {
    pub(crate) repository: String,
    pub(crate) enqueue_operation_id: String,
    pub(crate) pull_number: i64,
    pub(crate) head_sha: String,
    pub(crate) base: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct PullRequestMergeResult {
    pub(crate) pull_number: i64,
    pub(crate) head_sha: String,
    pub(crate) base: String,
    pub(crate) pull_state: String,
    pub(crate) state: MergeObservationState,
    pub(crate) entry_id: String,
    pub(crate) queue_state: Option<String>,
    pub(crate) merge_commit_sha: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum MergeObservationState {
    ActiveQueue,
    MergedAfterEnqueueAttempt,
    NotQueued,
}

/// One file's content at a path, or its removal when `content_base64` is absent.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileChange {
    pub(crate) path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) content_base64: Option<String>,
    /// `100644` or `100755`. Absent keeps an existing path's mode and creates a
    /// new one non-executable, which is the old behaviour. Stating it is what
    /// lets an integrating caller reproduce an executable file it observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) mode: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublishCommit {
    pub(crate) repository: String,
    pub(crate) operation_id: String,
    pub(crate) branch: String,
    /// The commit the branch must currently point at. Two agents racing the
    /// same branch means the second one's expectation no longer holds and it
    /// fails closed instead of clobbering the first.
    pub(crate) expected_head_sha: String,
    pub(crate) message: String,
    pub(crate) changes: Vec<FileChange>,
}

/// Add one pull request to the merge queue for its base branch.
///
/// This is the queue path for repositories whose active repository ruleset
/// requires a merge queue. Repositories with an active strict ruleset and no
/// queue use the separate exact-head `merge_pull_request_at_head` operation
/// instead.
///
/// There is no merge method here. The queue's ruleset decides it, and a
/// caller-supplied method would either be ignored or contradict the ruleset.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnqueuePullRequest {
    pub(crate) repository: String,
    pub(crate) operation_id: String,
    pub(crate) pull_number: i64,
    /// `enqueuePullRequest` takes `expectedHeadOid`, so exact-head binding is
    /// enforced by the platform on the write itself rather than only by the
    /// re-read below.
    pub(crate) head_sha: String,
    /// The branch whose queue this is. The decision doc requires the bound
    /// base as well as the head: without it, a pull request that targets a
    /// different branch than the caller believes would be enqueued onto that
    /// branch's queue instead.
    pub(crate) base: String,
}

/// Merge one pull request directly, but only after proving every repository,
/// branch-rule, check, and review condition at the exact head. The operation
/// deliberately has no caller-selected URL, method, or merge mode: this is
/// one fixed `PUT /pulls/{n}/merge` squash path.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MergePullRequestAtHead {
    pub(crate) repository: String,
    pub(crate) operation_id: String,
    pub(crate) review_operation_id: String,
    pub(crate) pull_number: i64,
    pub(crate) head_sha: String,
    pub(crate) base: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct MergePullRequestAtHeadResult {
    pub(crate) pull_number: i64,
    pub(crate) head_sha: String,
    pub(crate) base: String,
    pub(crate) merge_commit_sha: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct CommitResult {
    pub(crate) branch: String,
    pub(crate) commit_sha: String,
    pub(crate) parent_sha: String,
}

/// Deliberately carries no `position`: it changes while the entry waits, and
/// this is a durable result an idempotent replay returns verbatim, so a stored
/// "position 3" is a lie the moment anything ahead merges.
///
/// `state_when_recorded` is named for exactly what it is, and deliberately not
/// "at enqueue": reconciliation builds this result too, and it observes an
/// entry that may have moved `QUEUED -> AWAITING_CHECKS -> UNMERGEABLE` since.
/// It is worth carrying because an entry can be `UNMERGEABLE` the moment it is
/// created -- a moved base, a conflict -- and a caller told only "queued"
/// waits for a merge that is never coming. It is a durable observation, not a
/// live status; `observe_pull_request_merge` performs the later live read.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct EnqueueResult {
    pub(crate) pull_number: i64,
    pub(crate) head_sha: String,
    pub(crate) entry_id: String,
    pub(crate) state_when_recorded: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct PullRequestResult {
    pub(crate) number: i64,
    pub(crate) url: String,
    pub(crate) head_sha: String,
    pub(crate) base_sha: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ClosePullRequestResult {
    pub(crate) pull_number: i64,
    pub(crate) head_sha: String,
    pub(crate) url: String,
    pub(crate) state: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ReviewResult {
    pub(crate) review_id: i64,
    pub(crate) url: String,
    pub(crate) head_sha: String,
    pub(crate) state: String,
    /// All three verdicts are `COMMENTED` on the wire, so `state` cannot tell
    /// a reviewer which verdict it just recorded -- on the one field that now
    /// gates merges. This echoes it back.
    pub(crate) verdict: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ChecksResult {
    pub(crate) pull_number: i64,
    pub(crate) head_sha: String,
    pub(crate) checks: Vec<CheckResult>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct CheckResult {
    pub(crate) name: String,
    pub(crate) status: String,
    pub(crate) conclusion: Option<String>,
    pub(crate) url: String,
    #[serde(skip)]
    app_id: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObservePullRequestWorkflows {
    pub(crate) repository: String,
    pub(crate) workflow_path: String,
    pub(crate) pull_number: i64,
    pub(crate) head_sha: String,
    pub(crate) base: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct PullRequestWorkflowsResult {
    pub(crate) pull_number: i64,
    pub(crate) head_sha: String,
    pub(crate) base: String,
    pub(crate) runs: Vec<WorkflowRunResult>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct WorkflowRunResult {
    pub(crate) run_id: i64,
    pub(crate) run_attempt: i64,
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) event: String,
    pub(crate) status: String,
    pub(crate) conclusion: Option<String>,
    pub(crate) url: String,
    pub(crate) jobs: Vec<WorkflowJobResult>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct WorkflowJobResult {
    pub(crate) job_id: i64,
    pub(crate) name: String,
    pub(crate) status: String,
    pub(crate) conclusion: Option<String>,
    pub(crate) url: String,
    pub(crate) steps: Vec<WorkflowStepResult>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct WorkflowStepResult {
    pub(crate) name: String,
    pub(crate) status: String,
    pub(crate) conclusion: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadPullRequestJobLog {
    pub(crate) repository: String,
    pub(crate) workflow_path: String,
    pub(crate) pull_number: i64,
    pub(crate) head_sha: String,
    pub(crate) base: String,
    pub(crate) run_id: i64,
    pub(crate) run_attempt: i64,
    pub(crate) job_id: i64,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct JobLogResult {
    pub(crate) pull_number: i64,
    pub(crate) head_sha: String,
    pub(crate) base: String,
    pub(crate) run_id: i64,
    pub(crate) run_attempt: i64,
    pub(crate) job_id: i64,
    pub(crate) text: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RerunFailedPullRequestJobs {
    pub(crate) repository: String,
    pub(crate) workflow_path: String,
    pub(crate) operation_id: String,
    pub(crate) pull_number: i64,
    pub(crate) head_sha: String,
    pub(crate) base: String,
    pub(crate) run_id: i64,
    pub(crate) run_attempt: i64,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct RerunFailedPullRequestJobsResult {
    pub(crate) pull_number: i64,
    pub(crate) head_sha: String,
    pub(crate) base: String,
    pub(crate) run_id: i64,
    pub(crate) run_attempt: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublishReleaseTag {
    pub(crate) repository: String,
    pub(crate) operation_id: String,
    pub(crate) tag: String,
    pub(crate) commit_sha: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ReleaseTagResult {
    pub(crate) tag: String,
    pub(crate) commit_sha: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecoverRelease {
    pub(crate) repository: String,
    pub(crate) operation_id: String,
    pub(crate) tag: String,
    pub(crate) commit_sha: String,
    pub(crate) workflow_sha: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DispatchControlPlaneDeploy {
    pub(crate) repository: String,
    pub(crate) operation_id: String,
    pub(crate) commit_sha: String,
    pub(crate) reviewed_tree: String,
    pub(crate) promote: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct WorkflowDispatchResult {
    pub(crate) operation_id: String,
    pub(crate) workflow: String,
    pub(crate) commit_sha: String,
    pub(crate) run_id: i64,
    pub(crate) run_attempt: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObserveRelease {
    pub(crate) repository: String,
    pub(crate) tag: String,
    pub(crate) commit_sha: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObserveReleaseWorkflow {
    pub(crate) repository: String,
    pub(crate) operation_id: String,
    pub(crate) tag: String,
    pub(crate) tag_sha: String,
    pub(crate) workflow_sha: String,
    pub(crate) run_id: i64,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ReleaseWorkflowObservationResult {
    pub(crate) operation_id: String,
    pub(crate) tag: String,
    pub(crate) tag_sha: String,
    pub(crate) workflow_sha: String,
    pub(crate) workflow_run: WorkflowRunResult,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ReleaseObservationResult {
    pub(crate) tag: String,
    pub(crate) commit_sha: String,
    pub(crate) release: Option<ReleaseResult>,
    pub(crate) workflow_run: WorkflowRunResult,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ReleaseResult {
    pub(crate) tag: String,
    pub(crate) url: String,
    pub(crate) draft: bool,
    pub(crate) prerelease: bool,
    pub(crate) assets: Vec<ReleaseAssetResult>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ReleaseAssetResult {
    pub(crate) name: String,
    pub(crate) size: i64,
    pub(crate) digest: String,
    pub(crate) url: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObserveControlPlaneDeploy {
    pub(crate) repository: String,
    pub(crate) operation_id: String,
    pub(crate) commit_sha: String,
    pub(crate) reviewed_tree: String,
    pub(crate) promote: bool,
    pub(crate) run_id: i64,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ControlPlaneDeployObservationResult {
    pub(crate) operation_id: String,
    pub(crate) commit_sha: String,
    pub(crate) reviewed_tree: String,
    pub(crate) promote: bool,
    pub(crate) workflow_run: WorkflowRunResult,
}

impl AppAuthority {
    pub(crate) fn new(
        app_id: i64,
        private_key: String,
        permission_revision: String,
    ) -> Result<Self, Error> {
        if permission_revision != PERMISSION_REVISION {
            return Err(Error::Configuration);
        }
        let private_key = Zeroizing::new(private_key);
        let private_key = general_purpose::STANDARD
            .decode(private_key.as_bytes())
            .map_err(|_| Error::Configuration)?;
        if !(1_000..=16_384).contains(&private_key.len()) {
            return Err(Error::Configuration);
        }
        Ok(Self(Arc::new(Authority {
            app_id,
            private_key: PrivateKey(private_key),
        })))
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn verify(&self) -> Result<(), Error> {
        self.0.verify().await
    }

    pub(crate) const fn permission_revision(&self) -> &'static str {
        PERMISSION_REVISION
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn observe_repository(
        &self,
        mut request: ObserveRepository,
    ) -> Result<RepositoryResult, OperationError> {
        let repository = RepositoryName::requested(&mut request.repository)?;
        let token = self
            .0
            .installation_token(
                repository,
                BTreeMap::from([("contents", "read"), ("metadata", "read")]),
            )
            .await?;
        self.0.repository_snapshot(&token).await
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn create_issue(
        &self,
        journal: &DeliveryJournal,
        mut request: CreateIssue,
    ) -> Result<IssueResult, OperationError> {
        request.validate()?;
        let repository = RepositoryName::requested(&mut request.repository)?;
        let operation = request.operation("create_issue")?;
        let state = journal
            .begin_operation(&operation)
            .await
            .map_err(|_| OperationError::Unavailable)?;
        if let Some(result) = completed_or_conflict::<IssueResult>(&state)? {
            return Ok(result);
        }
        let token = self
            .0
            .installation_token(
                repository,
                BTreeMap::from([("issues", "write"), ("metadata", "read")]),
            )
            .await?;
        if let Some(result) = self.0.reconcile_issue(&token, &request).await? {
            return complete(journal, &operation, result).await;
        }
        if matches!(
            state,
            OperationRecord::Executing | OperationRecord::Indeterminate
        ) {
            journal
                .mark_operation(&operation, OperationTransition::Indeterminate)
                .await
                .map_err(|_| OperationError::Unavailable)?;
            return Err(OperationError::Indeterminate);
        }
        match journal
            .mark_operation(&operation, OperationTransition::Executing)
            .await
            .map_err(|_| OperationError::Unavailable)?
        {
            OperationRecord::Claimed => {}
            OperationRecord::Completed(result) => {
                return serde_json::from_str(&result).map_err(|_| OperationError::Unavailable);
            }
            OperationRecord::Conflict => return Err(OperationError::Conflict),
            OperationRecord::Executing | OperationRecord::Indeterminate => {
                if let Some(result) = self.0.reconcile_issue(&token, &request).await? {
                    return complete(journal, &operation, result).await;
                }
                return Err(OperationError::Indeterminate);
            }
            OperationRecord::New | OperationRecord::Planned => {
                return Err(OperationError::Unavailable);
            }
        }
        match self.0.post_issue(&token, &request).await {
            Ok(result) => complete(journal, &operation, result).await,
            Err(OperationError::Refused(reason)) => refuse(journal, &operation, reason).await,
            Err(_) => {
                if let Some(result) = self.0.reconcile_issue(&token, &request).await? {
                    return complete(journal, &operation, result).await;
                }
                let _ = journal
                    .mark_operation(&operation, OperationTransition::Indeterminate)
                    .await;
                Err(OperationError::Indeterminate)
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn observe_file(
        &self,
        mut request: ObserveFile,
    ) -> Result<FileObservationResult, OperationError> {
        request.validate()?;
        let repository = RepositoryName::requested(&mut request.repository)?;
        let token = self
            .0
            .installation_token(
                repository,
                BTreeMap::from([("contents", "read"), ("metadata", "read")]),
            )
            .await?;
        let content = self
            .0
            .read_file(&token, &request.commit_sha, &request.path)
            .await?;
        Ok(FileObservationResult {
            path: request.path,
            commit_sha: request.commit_sha,
            content_base64: content,
        })
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn observe_tree(
        &self,
        mut request: ObserveTree,
    ) -> Result<TreeObservationResult, OperationError> {
        request.validate()?;
        let repository = RepositoryName::requested(&mut request.repository)?;
        let token = self
            .0
            .installation_token(
                repository,
                BTreeMap::from([("contents", "read"), ("metadata", "read")]),
            )
            .await?;
        self.0.read_tree(&token, request.commit_sha).await
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn observe_ref(
        &self,
        mut request: ObserveRef,
    ) -> Result<RefObservationResult, OperationError> {
        request.validate()?;
        let repository = RepositoryName::requested(&mut request.repository)?;
        let token = self
            .0
            .installation_token(
                repository,
                BTreeMap::from([("contents", "read"), ("metadata", "read")]),
            )
            .await?;
        // `read_ref_optional` is the whole check: it proves the ref is exactly
        // `refs/heads/{branch}`, that it points at a commit, and that the SHA
        // is well formed, and refuses anything else. `None` is the one thing it
        // reports as an answer rather than a failure -- the branch is absent.
        let reference = self.0.read_ref_optional(&token, &request.branch).await?;
        Ok(RefObservationResult {
            branch: request.branch,
            head_sha: reference.map(|reference| reference.object.sha),
        })
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn observe_issue(
        &self,
        mut request: ObserveIssue,
    ) -> Result<IssueObservationResult, OperationError> {
        request.validate()?;
        let repository = RepositoryName::requested(&mut request.repository)?;
        let token = self
            .0
            .installation_token(
                repository,
                BTreeMap::from([("issues", "read"), ("metadata", "read")]),
            )
            .await?;
        self.0
            .read_issue(&token, request.issue_number)
            .await?
            .into_observation()
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn resolve_issue(
        &self,
        journal: &DeliveryJournal,
        mut request: ResolveIssue,
    ) -> Result<ResolveIssueResult, OperationError> {
        request.validate()?;
        let repository = RepositoryName::requested(&mut request.repository)?;
        let operation = request.operation("resolve_issue")?;
        let state = journal
            .begin_operation(&operation)
            .await
            .map_err(|_| OperationError::Unavailable)?;
        if let Some(result) = completed_or_conflict::<ResolveIssueResult>(&state)? {
            return Ok(result);
        }
        let token = self
            .0
            .installation_token(
                repository,
                BTreeMap::from([("issues", "write"), ("metadata", "read")]),
            )
            .await?;
        let progress = self.0.reconcile_issue_resolution(&token, &request).await?;
        if let Some(result) = progress.completed(&request) {
            return complete(journal, &operation, result).await;
        }
        if matches!(state, OperationRecord::Executing) {
            journal
                .mark_operation(&operation, OperationTransition::Indeterminate)
                .await
                .map_err(|_| OperationError::Unavailable)?;
            return Err(OperationError::Indeterminate);
        }
        // A prior attempt may have posted the comment and lost the response.
        // It is safe to retry only the close, never the comment, when the
        // marker is already present.
        if matches!(state, OperationRecord::Indeterminate) && progress.comment.is_none() {
            return Err(OperationError::Indeterminate);
        }
        if matches!(state, OperationRecord::New | OperationRecord::Planned) {
            match journal
                .mark_operation(&operation, OperationTransition::Executing)
                .await
                .map_err(|_| OperationError::Unavailable)?
            {
                OperationRecord::Claimed => {}
                OperationRecord::Completed(result) => {
                    return serde_json::from_str(&result).map_err(|_| OperationError::Unavailable);
                }
                OperationRecord::Conflict => return Err(OperationError::Conflict),
                OperationRecord::Executing | OperationRecord::Indeterminate => {
                    return Err(OperationError::Indeterminate);
                }
                OperationRecord::New | OperationRecord::Planned => {
                    return Err(OperationError::Unavailable);
                }
            }
        }

        let comment = match progress.comment {
            Some(comment) => comment,
            None => match self.0.post_issue_comment(&token, &request).await {
                Ok(comment) => comment,
                Err(OperationError::Refused(reason)) => {
                    return refuse(journal, &operation, reason).await;
                }
                Err(_) => {
                    if let Some(result) = self
                        .0
                        .reconcile_issue_resolution(&token, &request)
                        .await?
                        .completed(&request)
                    {
                        return complete(journal, &operation, result).await;
                    }
                    let _ = journal
                        .mark_operation(&operation, OperationTransition::Indeterminate)
                        .await;
                    return Err(OperationError::Indeterminate);
                }
            },
        };
        match self.0.close_issue(&token, &request).await {
            Ok(issue) => {
                let result = ResolveIssueResult {
                    number: issue.number,
                    url: issue.html_url,
                    comment_url: comment.html_url,
                    state: issue.state,
                    state_reason: issue.state_reason.unwrap_or_default(),
                };
                if result.state != "closed" || result.state_reason != request.state_reason.as_str()
                {
                    let _ = journal
                        .mark_operation(&operation, OperationTransition::Indeterminate)
                        .await;
                    return Err(OperationError::Indeterminate);
                }
                complete(journal, &operation, result).await
            }
            Err(OperationError::Refused(reason)) => {
                // The evidence comment is already durable, so the whole
                // operation is partially applied even though GitHub proved
                // the close itself did not run. Keep the journal
                // reconcilable, but preserve the typed reason for this call.
                let _ = journal
                    .mark_operation(&operation, OperationTransition::Indeterminate)
                    .await;
                Err(OperationError::Refused(reason))
            }
            Err(_) => {
                if let Some(result) = self
                    .0
                    .reconcile_issue_resolution(&token, &request)
                    .await?
                    .completed(&request)
                {
                    return complete(journal, &operation, result).await;
                }
                let _ = journal
                    .mark_operation(&operation, OperationTransition::Indeterminate)
                    .await;
                Err(OperationError::Indeterminate)
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn create_pull_request(
        &self,
        journal: &DeliveryJournal,
        mut request: CreatePullRequest,
    ) -> Result<PullRequestResult, OperationError> {
        request.validate()?;
        let repository = RepositoryName::requested(&mut request.repository)?;
        let operation = request.operation("create_pull_request")?;
        let state = journal
            .begin_operation(&operation)
            .await
            .map_err(|_| OperationError::Unavailable)?;
        if let Some(result) = completed_or_conflict::<PullRequestResult>(&state)? {
            return Ok(result);
        }
        let token = self
            .0
            .installation_token(
                repository,
                BTreeMap::from([
                    ("contents", "read"),
                    ("issues", "read"),
                    ("metadata", "read"),
                    ("pull_requests", "write"),
                ]),
            )
            .await?;
        let repository = self.0.repository_metadata(&token).await?;
        if request.base != repository.default_branch {
            return Err(OperationError::InvalidInput);
        }
        if let Some(result) = self.0.reconcile_pull_request(&token, &request).await? {
            return complete(journal, &operation, result).await;
        }
        if matches!(
            state,
            OperationRecord::Executing | OperationRecord::Indeterminate
        ) {
            journal
                .mark_operation(&operation, OperationTransition::Indeterminate)
                .await
                .map_err(|_| OperationError::Unavailable)?;
            return Err(OperationError::Indeterminate);
        }
        let issue = self.0.read_issue(&token, request.issue_number).await?;
        if !issue.is_real_open_issue() {
            return Err(OperationError::Conflict);
        }
        self.0
            .verify_ref(&token, &request.head, &request.head_sha)
            .await?;
        self.0
            .verify_ref(&token, &request.base, &request.base_sha)
            .await?;
        match journal
            .mark_operation(&operation, OperationTransition::Executing)
            .await
            .map_err(|_| OperationError::Unavailable)?
        {
            OperationRecord::Claimed => {}
            OperationRecord::Completed(result) => {
                return serde_json::from_str(&result).map_err(|_| OperationError::Unavailable);
            }
            OperationRecord::Conflict => return Err(OperationError::Conflict),
            OperationRecord::Executing | OperationRecord::Indeterminate => {
                if let Some(result) = self.0.reconcile_pull_request(&token, &request).await? {
                    return complete(journal, &operation, result).await;
                }
                return Err(OperationError::Indeterminate);
            }
            OperationRecord::New | OperationRecord::Planned => {
                return Err(OperationError::Unavailable);
            }
        }
        match self.0.post_pull_request(&token, &request).await {
            Ok(result) => complete(journal, &operation, result).await,
            Err(OperationError::Refused(reason)) => refuse(journal, &operation, reason).await,
            Err(_) => {
                if let Some(result) = self.0.reconcile_pull_request(&token, &request).await? {
                    return complete(journal, &operation, result).await;
                }
                let _ = journal
                    .mark_operation(&operation, OperationTransition::Indeterminate)
                    .await;
                Err(OperationError::Indeterminate)
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn update_pull_request_body(
        &self,
        journal: &DeliveryJournal,
        mut request: UpdatePullRequestBody,
    ) -> Result<PullRequestResult, OperationError> {
        request.validate()?;
        let repository = RepositoryName::requested(&mut request.repository)?;
        let operation = request.operation("update_pull_request_body")?;
        let state = journal
            .begin_operation(&operation)
            .await
            .map_err(|_| OperationError::Unavailable)?;
        if let Some(result) = completed_or_conflict::<PullRequestResult>(&state)? {
            return Ok(result);
        }
        let token = self
            .0
            .installation_token(
                repository,
                BTreeMap::from([("metadata", "read"), ("pull_requests", "write")]),
            )
            .await?;
        if matches!(
            state,
            OperationRecord::Executing | OperationRecord::Indeterminate
        ) {
            if let Some(result) = self.0.reconcile_pull_request_body(&token, &request).await? {
                return complete(journal, &operation, result).await;
            }
            journal
                .mark_operation(&operation, OperationTransition::Indeterminate)
                .await
                .map_err(|_| OperationError::Unavailable)?;
            return Err(OperationError::Indeterminate);
        }
        match journal
            .mark_operation(&operation, OperationTransition::Executing)
            .await
            .map_err(|_| OperationError::Unavailable)?
        {
            OperationRecord::Claimed => {}
            OperationRecord::Completed(result) => {
                return serde_json::from_str(&result).map_err(|_| OperationError::Unavailable);
            }
            OperationRecord::Conflict => return Err(OperationError::Conflict),
            OperationRecord::Executing | OperationRecord::Indeterminate => {
                if let Some(result) = self.0.reconcile_pull_request_body(&token, &request).await? {
                    return complete(journal, &operation, result).await;
                }
                return Err(OperationError::Indeterminate);
            }
            OperationRecord::New | OperationRecord::Planned => {
                return Err(OperationError::Unavailable);
            }
        }
        match self.0.reconcile_pull_request_body(&token, &request).await {
            Ok(Some(result)) => return complete(journal, &operation, result).await,
            Ok(None) => {}
            Err(error) => {
                journal
                    .mark_operation(&operation, OperationTransition::Refused)
                    .await
                    .map_err(|_| OperationError::Unavailable)?;
                return Err(error);
            }
        }
        match self.0.update_pull_request_body(&token, &request).await {
            Ok(result) => complete(journal, &operation, result).await,
            Err(OperationError::Refused(reason)) => refuse(journal, &operation, reason).await,
            Err(_) => {
                if let Ok(Some(result)) = self.0.reconcile_pull_request_body(&token, &request).await
                {
                    return complete(journal, &operation, result).await;
                }
                let _ = journal
                    .mark_operation(&operation, OperationTransition::Indeterminate)
                    .await;
                Err(OperationError::Indeterminate)
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn close_pull_request(
        &self,
        journal: &DeliveryJournal,
        mut request: ClosePullRequest,
    ) -> Result<ClosePullRequestResult, OperationError> {
        request.validate()?;
        let repository = RepositoryName::requested(&mut request.repository)?;
        let operation = request.operation("close_pull_request")?;
        let state = journal
            .begin_operation(&operation)
            .await
            .map_err(|_| OperationError::Unavailable)?;
        if let Some(result) = completed_or_conflict::<ClosePullRequestResult>(&state)? {
            return Ok(result);
        }
        let token = self
            .0
            .installation_token(
                repository,
                BTreeMap::from([("metadata", "read"), ("pull_requests", "write")]),
            )
            .await?;
        if matches!(
            state,
            OperationRecord::Executing | OperationRecord::Indeterminate
        ) {
            if let Some(result) = self
                .0
                .reconcile_closed_pull_request(&token, &request)
                .await?
            {
                return complete(journal, &operation, result).await;
            }
            journal
                .mark_operation(&operation, OperationTransition::Indeterminate)
                .await
                .map_err(|_| OperationError::Unavailable)?;
            return Err(OperationError::Indeterminate);
        }
        match journal
            .mark_operation(&operation, OperationTransition::Executing)
            .await
            .map_err(|_| OperationError::Unavailable)?
        {
            OperationRecord::Claimed => {}
            OperationRecord::Completed(result) => {
                return serde_json::from_str(&result).map_err(|_| OperationError::Unavailable);
            }
            OperationRecord::Conflict => return Err(OperationError::Conflict),
            OperationRecord::Executing | OperationRecord::Indeterminate => {
                if let Some(result) = self
                    .0
                    .reconcile_closed_pull_request(&token, &request)
                    .await?
                {
                    return complete(journal, &operation, result).await;
                }
                return Err(OperationError::Indeterminate);
            }
            OperationRecord::New | OperationRecord::Planned => {
                return Err(OperationError::Unavailable);
            }
        }
        match self.0.reconcile_closed_pull_request(&token, &request).await {
            Ok(Some(result)) => return complete(journal, &operation, result).await,
            Ok(None) => {}
            Err(error) => {
                journal
                    .mark_operation(&operation, OperationTransition::Refused)
                    .await
                    .map_err(|_| OperationError::Unavailable)?;
                return Err(error);
            }
        }
        match self.0.close_pull_request(&token, &request).await {
            Ok(result) => complete(journal, &operation, result).await,
            Err(OperationError::Refused(reason)) => refuse(journal, &operation, reason).await,
            Err(_) => {
                if let Ok(Some(result)) =
                    self.0.reconcile_closed_pull_request(&token, &request).await
                {
                    return complete(journal, &operation, result).await;
                }
                let _ = journal
                    .mark_operation(&operation, OperationTransition::Indeterminate)
                    .await;
                Err(OperationError::Indeterminate)
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn submit_pull_request_review(
        &self,
        journal: &DeliveryJournal,
        mut request: SubmitPullRequestReview,
    ) -> Result<ReviewResult, OperationError> {
        request.validate()?;
        let repository = RepositoryName::requested(&mut request.repository)?;
        let operation = request.operation("submit_pull_request_review")?;
        let state = journal
            .begin_operation(&operation)
            .await
            .map_err(|_| OperationError::Unavailable)?;
        if let Some(result) = completed_or_conflict::<ReviewResult>(&state)? {
            return Ok(result);
        }
        let token = self
            .0
            .installation_token(
                repository,
                BTreeMap::from([
                    ("contents", "read"),
                    ("metadata", "read"),
                    ("pull_requests", "write"),
                ]),
            )
            .await?;
        self.0
            .verify_pull_request_head(&token, request.pull_number, &request.head_sha)
            .await?;
        if let Some(result) = self.0.reconcile_review(&token, &request).await? {
            return complete(journal, &operation, result).await;
        }
        if matches!(
            state,
            OperationRecord::Executing | OperationRecord::Indeterminate
        ) {
            journal
                .mark_operation(&operation, OperationTransition::Indeterminate)
                .await
                .map_err(|_| OperationError::Unavailable)?;
            return Err(OperationError::Indeterminate);
        }
        match journal
            .mark_operation(&operation, OperationTransition::Executing)
            .await
            .map_err(|_| OperationError::Unavailable)?
        {
            OperationRecord::Claimed => {}
            OperationRecord::Completed(result) => {
                return serde_json::from_str(&result).map_err(|_| OperationError::Unavailable);
            }
            OperationRecord::Conflict => return Err(OperationError::Conflict),
            OperationRecord::Executing | OperationRecord::Indeterminate => {
                if let Some(result) = self.0.reconcile_review(&token, &request).await? {
                    return complete(journal, &operation, result).await;
                }
                return Err(OperationError::Indeterminate);
            }
            OperationRecord::New | OperationRecord::Planned => {
                return Err(OperationError::Unavailable);
            }
        }
        match self.0.post_review(&token, &request).await {
            Ok(result) => complete(journal, &operation, result).await,
            Err(OperationError::Refused(reason)) => refuse(journal, &operation, reason).await,
            Err(_) => {
                if let Some(result) = self.0.reconcile_review(&token, &request).await? {
                    return complete(journal, &operation, result).await;
                }
                let _ = journal
                    .mark_operation(&operation, OperationTransition::Indeterminate)
                    .await;
                Err(OperationError::Indeterminate)
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn publish_commit(
        &self,
        journal: &DeliveryJournal,
        mut request: PublishCommit,
    ) -> Result<CommitResult, OperationError> {
        request.validate()?;
        let repository = RepositoryName::requested(&mut request.repository)?;
        let operation = request.operation("publish_commit")?;
        let state = journal
            .begin_operation(&operation)
            .await
            .map_err(|_| OperationError::Unavailable)?;
        if let Some(result) = completed_or_conflict::<CommitResult>(&state)? {
            return Ok(result);
        }
        let token = self
            .0
            .installation_token(
                repository,
                BTreeMap::from([("contents", "write"), ("metadata", "read")]),
            )
            .await?;
        let repository = self.0.repository_metadata(&token).await?;
        if request.branch == repository.default_branch {
            return Err(OperationError::InvalidInput);
        }
        if let Some(result) = self
            .0
            .reconcile_commit(&token, &request)
            .await
            .map_err(|_| OperationError::Indeterminate)?
        {
            return complete(journal, &operation, result).await;
        }
        if matches!(
            state,
            OperationRecord::Executing | OperationRecord::Indeterminate
        ) {
            journal
                .mark_operation(&operation, OperationTransition::Indeterminate)
                .await
                .map_err(|_| OperationError::Unavailable)?;
            return Err(OperationError::Indeterminate);
        }
        // `verify_publish_precondition` already reports a moved head as a
        // conflict. Rewriting every other failure into one too told the caller
        // to refetch a head that had not moved.
        let branch_exists = self.0.verify_publish_precondition(&token, &request).await?;
        match journal
            .mark_operation(&operation, OperationTransition::Executing)
            .await
            .map_err(|_| OperationError::Unavailable)?
        {
            OperationRecord::Claimed => {}
            OperationRecord::Completed(result) => {
                return serde_json::from_str(&result).map_err(|_| OperationError::Unavailable);
            }
            OperationRecord::Conflict => return Err(OperationError::Conflict),
            OperationRecord::Executing | OperationRecord::Indeterminate => {
                if let Some(result) = self.0.reconcile_commit(&token, &request).await? {
                    return complete(journal, &operation, result).await;
                }
                return Err(OperationError::Indeterminate);
            }
            OperationRecord::New | OperationRecord::Planned => {
                return Err(OperationError::Unavailable);
            }
        }
        match self.0.push_commit(&token, &request, branch_exists).await {
            Ok(result) => complete(journal, &operation, result).await,
            Err(OperationError::Refused(reason)) => refuse(journal, &operation, reason).await,
            Err(_) => {
                if let Some(result) = self.0.reconcile_commit(&token, &request).await? {
                    return complete(journal, &operation, result).await;
                }
                let _ = journal
                    .mark_operation(&operation, OperationTransition::Indeterminate)
                    .await;
                Err(OperationError::Indeterminate)
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn publish_release_tag(
        &self,
        journal: &DeliveryJournal,
        mut request: PublishReleaseTag,
    ) -> Result<ReleaseTagResult, OperationError> {
        request.validate()?;
        let repository = RepositoryName::requested(&mut request.repository)?;
        let operation = request.operation("publish_release_tag")?;
        let state = journal
            .begin_operation(&operation)
            .await
            .map_err(|_| OperationError::Unavailable)?;
        if let Some(result) = completed_or_conflict::<ReleaseTagResult>(&state)? {
            return Ok(result);
        }
        let token = self
            .0
            .installation_token(
                repository,
                BTreeMap::from([("contents", "write"), ("metadata", "read")]),
            )
            .await?;
        let repository = self.0.repository_metadata(&token).await?;
        let default = self.0.read_ref(&token, &repository.default_branch).await?;
        if default.object.sha != request.commit_sha {
            return Err(OperationError::Conflict);
        }
        if let Some(existing) = self
            .0
            .read_tag_commit_optional(&token, &request.tag)
            .await?
        {
            if existing == request.commit_sha {
                return complete(
                    journal,
                    &operation,
                    ReleaseTagResult {
                        tag: request.tag,
                        commit_sha: existing,
                    },
                )
                .await;
            }
            return Err(OperationError::Conflict);
        }
        if matches!(
            state,
            OperationRecord::Executing | OperationRecord::Indeterminate
        ) {
            journal
                .mark_operation(&operation, OperationTransition::Indeterminate)
                .await
                .map_err(|_| OperationError::Unavailable)?;
            return Err(OperationError::Indeterminate);
        }
        match journal
            .mark_operation(&operation, OperationTransition::Executing)
            .await
            .map_err(|_| OperationError::Unavailable)?
        {
            OperationRecord::Claimed => {}
            OperationRecord::Completed(result) => {
                return serde_json::from_str(&result).map_err(|_| OperationError::Unavailable);
            }
            OperationRecord::Conflict => return Err(OperationError::Conflict),
            OperationRecord::Executing | OperationRecord::Indeterminate => {
                if let Some(existing) = self
                    .0
                    .read_tag_commit_optional(&token, &request.tag)
                    .await?
                {
                    if existing == request.commit_sha {
                        return complete(
                            journal,
                            &operation,
                            ReleaseTagResult {
                                tag: request.tag,
                                commit_sha: existing,
                            },
                        )
                        .await;
                    }
                }
                return Err(OperationError::Indeterminate);
            }
            OperationRecord::New | OperationRecord::Planned => {
                return Err(OperationError::Unavailable);
            }
        }
        let result = ReleaseTagResult {
            tag: request.tag.clone(),
            commit_sha: request.commit_sha.clone(),
        };
        match self.0.create_release_tag(&token, &request).await {
            Ok(()) => complete(journal, &operation, result).await,
            Err(error) => {
                match self
                    .0
                    .read_tag_commit_optional(&token, &request.tag)
                    .await?
                {
                    Some(commit_sha) if commit_sha == request.commit_sha => {
                        complete(journal, &operation, result).await
                    }
                    Some(_) => Err(OperationError::Conflict),
                    None => {
                        if let OperationError::Refused(reason) = error {
                            return refuse(journal, &operation, reason).await;
                        }
                        let _ = journal
                            .mark_operation(&operation, OperationTransition::Indeterminate)
                            .await;
                        Err(OperationError::Indeterminate)
                    }
                }
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn recover_release(
        &self,
        journal: &DeliveryJournal,
        mut request: RecoverRelease,
    ) -> Result<WorkflowDispatchResult, OperationError> {
        request.validate()?;
        let repository = RepositoryName::requested(&mut request.repository)?;
        let operation = request.operation("recover_release")?;
        let state = journal
            .begin_operation(&operation)
            .await
            .map_err(|_| OperationError::Unavailable)?;
        if let Some(result) = completed_or_conflict::<WorkflowDispatchResult>(&state)? {
            return Ok(result);
        }
        let token = self
            .0
            .installation_token(
                repository,
                BTreeMap::from([
                    ("actions", "write"),
                    ("contents", "read"),
                    ("metadata", "read"),
                ]),
            )
            .await?;
        let repository = self.0.repository_metadata(&token).await?;
        let default = self.0.read_ref(&token, &repository.default_branch).await?;
        if default.object.sha != request.workflow_sha {
            return Err(OperationError::Conflict);
        }
        let tag_sha = self
            .0
            .read_tag_commit_optional(&token, &request.tag)
            .await?
            .ok_or(OperationError::Conflict)?;
        if tag_sha != request.commit_sha {
            return Err(OperationError::Conflict);
        }
        let marker = format!(
            "Release recovery {} {}",
            operation.operation_id, operation.request_digest
        );
        if let Some(run) = self
            .0
            .workflow_run_by_title(&token, RELEASE_WORKFLOW, &marker, &request.workflow_sha)
            .await?
        {
            return complete(
                journal,
                &operation,
                WorkflowDispatchResult {
                    operation_id: request.operation_id,
                    workflow: RELEASE_WORKFLOW.response_path.into(),
                    commit_sha: request.workflow_sha,
                    run_id: run.id,
                    run_attempt: run.run_attempt,
                },
            )
            .await;
        }
        if matches!(
            state,
            OperationRecord::Executing | OperationRecord::Indeterminate
        ) {
            journal
                .mark_operation(&operation, OperationTransition::Indeterminate)
                .await
                .map_err(|_| OperationError::Unavailable)?;
            return Err(OperationError::Indeterminate);
        }
        match journal
            .mark_operation(&operation, OperationTransition::Executing)
            .await
            .map_err(|_| OperationError::Unavailable)?
        {
            OperationRecord::Claimed => {}
            OperationRecord::Completed(result) => {
                return serde_json::from_str(&result).map_err(|_| OperationError::Unavailable);
            }
            OperationRecord::Conflict => return Err(OperationError::Conflict),
            OperationRecord::Executing | OperationRecord::Indeterminate => {
                return Err(OperationError::Indeterminate);
            }
            OperationRecord::New | OperationRecord::Planned => {
                return Err(OperationError::Unavailable);
            }
        }
        match self
            .0
            .dispatch_workflow(
                &token,
                RELEASE_WORKFLOW,
                &repository.default_branch,
                serde_json::json!({
                    "operation_id": request.operation_id,
                    "request_digest": operation.request_digest,
                    "tag": request.tag,
                    "expected_workflow_sha": request.workflow_sha,
                }),
            )
            .await
        {
            Ok(dispatch) => match self
                .0
                .read_dispatched_workflow(
                    &token,
                    dispatch.workflow_run_id,
                    RELEASE_WORKFLOW,
                    &request.workflow_sha,
                    &marker,
                )
                .await
            {
                Ok(run) => {
                    complete(
                        journal,
                        &operation,
                        WorkflowDispatchResult {
                            operation_id: operation.operation_id.clone(),
                            workflow: RELEASE_WORKFLOW.response_path.into(),
                            commit_sha: request.workflow_sha.clone(),
                            run_id: run.id,
                            run_attempt: run.run_attempt,
                        },
                    )
                    .await
                }
                Err(_) => {
                    let _ = journal
                        .mark_operation(&operation, OperationTransition::Indeterminate)
                        .await;
                    Err(OperationError::Indeterminate)
                }
            },
            Err(OperationError::Refused(reason)) => refuse(journal, &operation, reason).await,
            Err(_) => {
                if let Some(run) = self
                    .0
                    .workflow_run_by_title(&token, RELEASE_WORKFLOW, &marker, &request.workflow_sha)
                    .await?
                {
                    return complete(
                        journal,
                        &operation,
                        WorkflowDispatchResult {
                            operation_id: operation.operation_id.clone(),
                            workflow: RELEASE_WORKFLOW.response_path.into(),
                            commit_sha: request.workflow_sha.clone(),
                            run_id: run.id,
                            run_attempt: run.run_attempt,
                        },
                    )
                    .await;
                }
                let _ = journal
                    .mark_operation(&operation, OperationTransition::Indeterminate)
                    .await;
                Err(OperationError::Indeterminate)
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn dispatch_control_plane_deploy(
        &self,
        journal: &DeliveryJournal,
        mut request: DispatchControlPlaneDeploy,
    ) -> Result<WorkflowDispatchResult, OperationError> {
        request.validate()?;
        let repository = RepositoryName::requested(&mut request.repository)?;
        let operation = request.operation("dispatch_control_plane_deploy")?;
        let state = journal
            .begin_operation(&operation)
            .await
            .map_err(|_| OperationError::Unavailable)?;
        if let Some(result) = completed_or_conflict::<WorkflowDispatchResult>(&state)? {
            return Ok(result);
        }
        let token = self
            .0
            .installation_token(
                repository,
                BTreeMap::from([
                    ("actions", "write"),
                    ("contents", "read"),
                    ("metadata", "read"),
                ]),
            )
            .await?;
        let repository = self.0.repository_metadata(&token).await?;
        let default = self.0.read_ref(&token, &repository.default_branch).await?;
        if default.object.sha != request.commit_sha {
            return Err(OperationError::Conflict);
        }
        let default_commit: GitCommit = github_json(
            &format!(
                "https://api.github.com/repos/{}/{}/git/commits/{}",
                token.repository.owner, token.repository.name, request.commit_sha
            ),
            token.as_str(),
        )
        .await?;
        if default_commit.tree.sha != request.reviewed_tree {
            return Err(OperationError::Conflict);
        }
        let marker = format!(
            "Deploy control-plane {} {}",
            operation.operation_id, operation.request_digest
        );
        if let Some(run) = self
            .0
            .workflow_run_by_title(&token, DEPLOY_WORKFLOW, &marker, &request.commit_sha)
            .await?
        {
            return complete(
                journal,
                &operation,
                WorkflowDispatchResult {
                    operation_id: request.operation_id,
                    workflow: DEPLOY_WORKFLOW.response_path.into(),
                    commit_sha: request.commit_sha,
                    run_id: run.id,
                    run_attempt: run.run_attempt,
                },
            )
            .await;
        }
        if matches!(
            state,
            OperationRecord::Executing | OperationRecord::Indeterminate
        ) {
            journal
                .mark_operation(&operation, OperationTransition::Indeterminate)
                .await
                .map_err(|_| OperationError::Unavailable)?;
            return Err(OperationError::Indeterminate);
        }
        match journal
            .mark_operation(&operation, OperationTransition::Executing)
            .await
            .map_err(|_| OperationError::Unavailable)?
        {
            OperationRecord::Claimed => {}
            OperationRecord::Completed(result) => {
                return serde_json::from_str(&result).map_err(|_| OperationError::Unavailable);
            }
            OperationRecord::Conflict => return Err(OperationError::Conflict),
            OperationRecord::Executing | OperationRecord::Indeterminate => {
                return Err(OperationError::Indeterminate);
            }
            OperationRecord::New | OperationRecord::Planned => {
                return Err(OperationError::Unavailable);
            }
        }
        match self
            .0
            .dispatch_workflow(
                &token,
                DEPLOY_WORKFLOW,
                &repository.default_branch,
                serde_json::json!({
                    "operation_id": request.operation_id,
                    "request_digest": operation.request_digest,
                    "expected_commit": request.commit_sha,
                    "expected_tree": request.reviewed_tree,
                    "promote": request.promote.to_string(),
                }),
            )
            .await
        {
            Ok(dispatch) => match self
                .0
                .read_dispatched_workflow(
                    &token,
                    dispatch.workflow_run_id,
                    DEPLOY_WORKFLOW,
                    &request.commit_sha,
                    &marker,
                )
                .await
            {
                Ok(run) => {
                    complete(
                        journal,
                        &operation,
                        WorkflowDispatchResult {
                            operation_id: operation.operation_id.clone(),
                            workflow: DEPLOY_WORKFLOW.response_path.into(),
                            commit_sha: request.commit_sha.clone(),
                            run_id: run.id,
                            run_attempt: run.run_attempt,
                        },
                    )
                    .await
                }
                Err(_) => {
                    let _ = journal
                        .mark_operation(&operation, OperationTransition::Indeterminate)
                        .await;
                    Err(OperationError::Indeterminate)
                }
            },
            Err(OperationError::Refused(reason)) => refuse(journal, &operation, reason).await,
            Err(_) => {
                if let Some(run) = self
                    .0
                    .workflow_run_by_title(&token, DEPLOY_WORKFLOW, &marker, &request.commit_sha)
                    .await?
                {
                    return complete(
                        journal,
                        &operation,
                        WorkflowDispatchResult {
                            operation_id: operation.operation_id.clone(),
                            workflow: DEPLOY_WORKFLOW.response_path.into(),
                            commit_sha: request.commit_sha.clone(),
                            run_id: run.id,
                            run_attempt: run.run_attempt,
                        },
                    )
                    .await;
                }
                let _ = journal
                    .mark_operation(&operation, OperationTransition::Indeterminate)
                    .await;
                Err(OperationError::Indeterminate)
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn observe_release(
        &self,
        mut request: ObserveRelease,
    ) -> Result<ReleaseObservationResult, OperationError> {
        request.validate()?;
        let repository = RepositoryName::requested(&mut request.repository)?;
        let token = self
            .0
            .installation_token(
                repository,
                BTreeMap::from([
                    ("actions", "read"),
                    ("contents", "read"),
                    ("metadata", "read"),
                ]),
            )
            .await?;
        let actual = self
            .0
            .read_tag_commit_optional(&token, &request.tag)
            .await?
            .ok_or(OperationError::Conflict)?;
        if actual != request.commit_sha {
            return Err(OperationError::Conflict);
        }
        let release = self.0.release(&token, &request.tag).await?;
        let workflow_run = self
            .0
            .release_workflow_run(&token, &request.tag, &request.commit_sha)
            .await?;
        Ok(ReleaseObservationResult {
            tag: request.tag,
            commit_sha: request.commit_sha,
            release,
            workflow_run,
        })
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn observe_release_workflow(
        &self,
        mut request: ObserveReleaseWorkflow,
    ) -> Result<ReleaseWorkflowObservationResult, OperationError> {
        request.validate()?;
        let repository = RepositoryName::requested(&mut request.repository)?;
        let token = self
            .0
            .installation_token(
                repository,
                BTreeMap::from([
                    ("actions", "read"),
                    ("contents", "read"),
                    ("metadata", "read"),
                ]),
            )
            .await?;
        let tag_sha = self
            .0
            .read_tag_commit_optional(&token, &request.tag)
            .await?
            .ok_or(OperationError::Conflict)?;
        if tag_sha != request.tag_sha {
            return Err(OperationError::Conflict);
        }
        let mut recovery = RecoverRelease {
            repository: request.repository.clone(),
            operation_id: request.operation_id.clone(),
            tag: request.tag.clone(),
            commit_sha: request.tag_sha.clone(),
            workflow_sha: request.workflow_sha.clone(),
        };
        recovery.validate()?;
        let operation = recovery.operation("recover_release")?;
        let title = format!(
            "Release recovery {} {}",
            operation.operation_id, operation.request_digest
        );
        let workflow_run = self.0.workflow_run(&token, request.run_id).await?;
        workflow_run.verify_dispatch(RELEASE_WORKFLOW, &request.workflow_sha, &title)?;
        Ok(ReleaseWorkflowObservationResult {
            operation_id: request.operation_id,
            tag: request.tag,
            tag_sha: request.tag_sha,
            workflow_sha: request.workflow_sha,
            workflow_run: workflow_run.into_result(Vec::new())?,
        })
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn observe_control_plane_deploy(
        &self,
        mut request: ObserveControlPlaneDeploy,
    ) -> Result<ControlPlaneDeployObservationResult, OperationError> {
        request.validate()?;
        let repository = RepositoryName::requested(&mut request.repository)?;
        let token = self
            .0
            .installation_token(
                repository,
                BTreeMap::from([
                    ("actions", "read"),
                    ("contents", "read"),
                    ("metadata", "read"),
                ]),
            )
            .await?;
        let mut dispatch = DispatchControlPlaneDeploy {
            repository: request.repository.clone(),
            operation_id: request.operation_id.clone(),
            commit_sha: request.commit_sha.clone(),
            reviewed_tree: request.reviewed_tree.clone(),
            promote: request.promote,
        };
        dispatch.validate()?;
        let operation = dispatch.operation("dispatch_control_plane_deploy")?;
        let workflow_run = self.0.workflow_run(&token, request.run_id).await?;
        workflow_run.verify_dispatch(
            DEPLOY_WORKFLOW,
            &request.commit_sha,
            &format!(
                "Deploy control-plane {} {}",
                operation.operation_id, operation.request_digest
            ),
        )?;
        Ok(ControlPlaneDeployObservationResult {
            operation_id: request.operation_id,
            commit_sha: request.commit_sha,
            reviewed_tree: request.reviewed_tree,
            promote: request.promote,
            workflow_run: workflow_run.into_result(Vec::new())?,
        })
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn enqueue_pull_request(
        &self,
        journal: &DeliveryJournal,
        mut request: EnqueuePullRequest,
    ) -> Result<EnqueueResult, OperationError> {
        request.validate()?;
        let repository = RepositoryName::requested(&mut request.repository)?;
        let operation = request.operation("enqueue_pull_request")?;
        let state = journal
            .begin_operation(&operation)
            .await
            .map_err(|_| OperationError::Unavailable)?;
        if let Some(result) = completed_or_conflict::<EnqueueResult>(&state)? {
            return Ok(result);
        }
        let token = self
            .0
            .installation_token(
                repository,
                BTreeMap::from([
                    // A queued entry ends with GitHub pushing the squash commit
                    // to the default branch, and push capability for an
                    // installation token derives from `contents: write` -- the
                    // scope `publish_commit` already mints and the reviewed
                    // revision already grants. Whether it is what the live
                    // enqueue is missing is the hypothesis under test (#371):
                    // the first live enqueue was refused wholesale with
                    // `pull_requests: read` minted, and the retry after #373 was
                    // refused `rejected before execution as FORBIDDEN` with
                    // `pull_requests: write` minted, so this is the next
                    // narrowest scope consistent with what enqueueing causes.
                    // The proof either way is a live enqueue after deploy.
                    ("contents", "write"),
                    ("merge_queues", "write"),
                    ("metadata", "read"),
                    // Enqueueing mutates the pull request's queue state. This
                    // minted `read` once, and the first live enqueue was refused
                    // wholesale (#371); every sibling operation that mutates a
                    // pull request mints `write`, and the reviewed permission
                    // revision already grants it.
                    ("pull_requests", "write"),
                ]),
            )
            .await?;
        let repository = self.0.repository_metadata(&token).await?;
        if request.base != repository.default_branch {
            return Err(OperationError::Conflict);
        }
        let existing = self.0.reconcile_enqueue(&token, &request).await?;
        if matches!(
            state,
            OperationRecord::Executing | OperationRecord::Indeterminate
        ) {
            if let Some(result) = existing {
                return complete(journal, &operation, result).await;
            }
            journal
                .mark_operation(&operation, OperationTransition::Indeterminate)
                .await
                .map_err(|_| OperationError::Unavailable)?;
            return Err(OperationError::Indeterminate);
        }
        if existing.is_some() {
            return Err(OperationError::Refused(RefusalReason::AlreadyQueued));
        }
        // The decision doc requires the bound base and head re-read before
        // enqueueing, not only bound on the write.
        let pull = self
            .0
            .verify_pull_request_head(&token, request.pull_number, &request.head_sha)
            .await?;
        if pull.base.name != request.base {
            return Err(OperationError::Conflict);
        }
        match journal
            .mark_operation(&operation, OperationTransition::Executing)
            .await
            .map_err(|_| OperationError::Unavailable)?
        {
            OperationRecord::Claimed => {}
            OperationRecord::Completed(result) => {
                return serde_json::from_str(&result).map_err(|_| OperationError::Unavailable);
            }
            OperationRecord::Conflict => return Err(OperationError::Conflict),
            OperationRecord::Executing | OperationRecord::Indeterminate => {
                if let Some(result) = self.0.reconcile_enqueue(&token, &request).await? {
                    return complete(journal, &operation, result).await;
                }
                return Err(OperationError::Indeterminate);
            }
            OperationRecord::New | OperationRecord::Planned => {
                return Err(OperationError::Unavailable);
            }
        }
        match self.0.enqueue_entry(&token, &pull.node_id, &request).await {
            Ok(result) => complete(journal, &operation, result).await,
            Err(OperationError::Refused(reason)) => {
                // Nothing was enqueued, and we know it. Release the claim so
                // this same operation ID and request can be retried once the
                // pull request is actually queueable; burying it at
                // `indeterminate` would force a fresh UUID for every retry and
                // would report an unknown outcome for a fully known one.
                journal
                    .mark_operation(&operation, OperationTransition::Refused)
                    .await
                    .map_err(|_| OperationError::Unavailable)?;
                Err(OperationError::Refused(reason))
            }
            Err(_) => {
                if let Some(result) = self.0.reconcile_enqueue(&token, &request).await? {
                    return complete(journal, &operation, result).await;
                }
                let _ = journal
                    .mark_operation(&operation, OperationTransition::Indeterminate)
                    .await;
                Err(OperationError::Indeterminate)
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn merge_pull_request_at_head(
        &self,
        journal: &DeliveryJournal,
        mut request: MergePullRequestAtHead,
    ) -> Result<MergePullRequestAtHeadResult, OperationError> {
        request.validate()?;
        let repository = RepositoryName::requested(&mut request.repository)?;
        let operation = request.operation("merge_pull_request_at_head")?;
        let state = journal
            .begin_operation(&operation)
            .await
            .map_err(|_| OperationError::Unavailable)?;
        if let Some(result) = completed_or_conflict::<MergePullRequestAtHeadResult>(&state)? {
            return Ok(result);
        }
        let token = self
            .0
            .installation_token(
                repository,
                BTreeMap::from([
                    ("checks", "read"),
                    // GitHub's Merge a pull request endpoint requires
                    // Contents: write; pull-request reads only need read.
                    ("contents", "write"),
                    // Detailed rulesets are Administration-gated; this is the
                    // only operation that reads them, and it performs no admin
                    // mutation.
                    ("administration", "write"),
                    ("merge_queues", "read"),
                    ("metadata", "read"),
                    ("pull_requests", "read"),
                ]),
            )
            .await?;

        if matches!(
            state,
            OperationRecord::Executing | OperationRecord::Indeterminate
        ) {
            if let Ok(Some(result)) = self.0.reconcile_merge(&token, &request).await {
                return complete(journal, &operation, result).await;
            }
            let _ = journal
                .mark_operation(&operation, OperationTransition::Indeterminate)
                .await;
            return Err(OperationError::Indeterminate);
        }
        match journal
            .mark_operation(&operation, OperationTransition::Executing)
            .await
            .map_err(|_| OperationError::Unavailable)?
        {
            OperationRecord::Claimed => {}
            OperationRecord::Completed(result) => {
                return serde_json::from_str(&result).map_err(|_| OperationError::Unavailable);
            }
            OperationRecord::Conflict => return Err(OperationError::Conflict),
            OperationRecord::Executing | OperationRecord::Indeterminate => {
                if let Ok(Some(result)) = self.0.reconcile_merge(&token, &request).await {
                    return complete(journal, &operation, result).await;
                }
                let _ = journal
                    .mark_operation(&operation, OperationTransition::Indeterminate)
                    .await;
                return Err(OperationError::Indeterminate);
            }
            OperationRecord::New | OperationRecord::Planned => {
                return Err(OperationError::Unavailable);
            }
        }

        // The unique durable claimant proves every mutable precondition once,
        // immediately before the irreversible PUT. A determinate or read-only
        // failure has made no GitHub mutation, so release the claim and keep
        // this exact operation retryable.
        if let Err(error) = self
            .0
            .verify_merge_preconditions(&token, journal, &request)
            .await
        {
            journal
                .mark_operation(&operation, OperationTransition::Refused)
                .await
                .map_err(|_| OperationError::Unavailable)?;
            return Err(error);
        }
        match self.0.merge_pull_request(&token, &request).await {
            Ok(result) => complete(journal, &operation, result).await,
            Err(OperationError::Refused(reason)) => refuse(journal, &operation, reason).await,
            Err(_) => {
                if let Ok(Some(result)) = self.0.reconcile_merge(&token, &request).await {
                    return complete(journal, &operation, result).await;
                }
                let _ = journal
                    .mark_operation(&operation, OperationTransition::Indeterminate)
                    .await;
                Err(OperationError::Indeterminate)
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn observe_pull_request_checks(
        &self,
        mut request: ObservePullRequestChecks,
    ) -> Result<ChecksResult, OperationError> {
        request.validate()?;
        let repository = RepositoryName::requested(&mut request.repository)?;
        let token = self
            .0
            .installation_token(
                repository,
                BTreeMap::from([
                    ("checks", "read"),
                    ("contents", "read"),
                    ("metadata", "read"),
                    ("pull_requests", "read"),
                ]),
            )
            .await?;
        self.0
            .verify_pull_request_head(&token, request.pull_number, &request.head_sha)
            .await?;
        self.0.checks(&token, request).await
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn observe_pull_request_workflows(
        &self,
        mut request: ObservePullRequestWorkflows,
    ) -> Result<PullRequestWorkflowsResult, OperationError> {
        request.validate()?;
        let repository = RepositoryName::requested(&mut request.repository)?;
        let token = self
            .0
            .installation_token(
                repository,
                BTreeMap::from([
                    ("actions", "read"),
                    ("metadata", "read"),
                    ("pull_requests", "read"),
                ]),
            )
            .await?;
        self.0
            .verify_workflow_pr(
                &token,
                request.pull_number,
                &request.head_sha,
                &request.base,
            )
            .await?;
        self.0.workflow_runs(&token, &request).await
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn read_pull_request_job_log(
        &self,
        mut request: ReadPullRequestJobLog,
    ) -> Result<JobLogResult, OperationError> {
        request.validate()?;
        let repository = RepositoryName::requested(&mut request.repository)?;
        let token = self
            .0
            .installation_token(
                repository,
                BTreeMap::from([
                    ("actions", "read"),
                    ("metadata", "read"),
                    ("pull_requests", "read"),
                ]),
            )
            .await?;
        self.0
            .verify_workflow_pr(
                &token,
                request.pull_number,
                &request.head_sha,
                &request.base,
            )
            .await?;
        let run = self.0.read_workflow_run(&token, request.run_id).await?;
        run.verify(
            WorkflowRef::requested(&request.workflow_path)?,
            request.pull_number,
            &request.head_sha,
        )?;
        if run.run_attempt != request.run_attempt
            || run.status != "completed"
            || !run.conclusion.as_deref().is_some_and(failed_conclusion)
        {
            return Err(OperationError::Conflict);
        }
        let jobs = self.0.workflow_jobs(&token, request.run_id).await?;
        let job = jobs
            .into_iter()
            .find(|job| job.id == request.job_id)
            .ok_or(OperationError::Conflict)?;
        if job.status != "completed" || !job.conclusion.as_deref().is_some_and(failed_conclusion) {
            return Err(OperationError::Refused(RefusalReason::JobNotFailed));
        }
        let location = self.0.job_log_redirect(&token, request.job_id).await?;
        let text = github_public_log(&location).await?;
        Ok(JobLogResult {
            pull_number: request.pull_number,
            head_sha: request.head_sha,
            base: request.base,
            run_id: request.run_id,
            run_attempt: request.run_attempt,
            job_id: request.job_id,
            text,
        })
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn rerun_failed_pull_request_jobs(
        &self,
        journal: &DeliveryJournal,
        mut request: RerunFailedPullRequestJobs,
    ) -> Result<RerunFailedPullRequestJobsResult, OperationError> {
        request.validate()?;
        let repository = RepositoryName::requested(&mut request.repository)?;
        let operation = request.operation("rerun_failed_pull_request_jobs")?;
        let state = journal
            .begin_operation(&operation)
            .await
            .map_err(|_| OperationError::Unavailable)?;
        if let Some(result) = completed_or_conflict::<RerunFailedPullRequestJobsResult>(&state)? {
            return Ok(result);
        }
        let token = self
            .0
            .installation_token(
                repository,
                BTreeMap::from([
                    ("actions", "write"),
                    ("metadata", "read"),
                    ("pull_requests", "read"),
                ]),
            )
            .await?;
        self.0
            .verify_workflow_pr(
                &token,
                request.pull_number,
                &request.head_sha,
                &request.base,
            )
            .await?;
        let run = self.0.read_workflow_run(&token, request.run_id).await?;
        run.verify(
            WorkflowRef::requested(&request.workflow_path)?,
            request.pull_number,
            &request.head_sha,
        )?;
        if run.run_attempt < request.run_attempt {
            return Err(OperationError::Conflict);
        }
        if run.run_attempt > request.run_attempt {
            if matches!(
                state,
                OperationRecord::Executing | OperationRecord::Indeterminate
            ) {
                return complete(
                    journal,
                    &operation,
                    RerunFailedPullRequestJobsResult {
                        pull_number: request.pull_number,
                        head_sha: request.head_sha,
                        base: request.base,
                        run_id: request.run_id,
                        run_attempt: run.run_attempt,
                    },
                )
                .await;
            }
            return Err(OperationError::Conflict);
        }
        if run.status != "completed" || !run.conclusion.as_deref().is_some_and(failed_conclusion) {
            return Err(OperationError::Refused(RefusalReason::RunNotFailed));
        }
        if matches!(
            state,
            OperationRecord::Executing | OperationRecord::Indeterminate
        ) {
            journal
                .mark_operation(&operation, OperationTransition::Indeterminate)
                .await
                .map_err(|_| OperationError::Unavailable)?;
            return Err(OperationError::Indeterminate);
        }
        match journal
            .mark_operation(&operation, OperationTransition::Executing)
            .await
            .map_err(|_| OperationError::Unavailable)?
        {
            OperationRecord::Claimed => {}
            OperationRecord::Completed(result) => {
                return serde_json::from_str(&result).map_err(|_| OperationError::Unavailable);
            }
            OperationRecord::Conflict => return Err(OperationError::Conflict),
            OperationRecord::Executing | OperationRecord::Indeterminate => {
                return Err(OperationError::Indeterminate);
            }
            OperationRecord::New | OperationRecord::Planned => {
                return Err(OperationError::Unavailable);
            }
        }
        if let Err(OperationError::Refused(reason)) =
            self.0.rerun_failed_jobs(&token, request.run_id).await
        {
            return refuse(journal, &operation, reason).await;
        }
        for attempt in 0..4 {
            if let Ok(after) = self.0.read_workflow_run(&token, request.run_id).await
                && after
                    .verify(
                        WorkflowRef::requested(&request.workflow_path)?,
                        request.pull_number,
                        &request.head_sha,
                    )
                    .is_ok()
                && after.run_attempt > request.run_attempt
            {
                return complete(
                    journal,
                    &operation,
                    RerunFailedPullRequestJobsResult {
                        pull_number: request.pull_number,
                        head_sha: request.head_sha,
                        base: request.base,
                        run_id: request.run_id,
                        run_attempt: after.run_attempt,
                    },
                )
                .await;
            }
            if attempt < 3 {
                worker::Delay::from(std::time::Duration::from_millis(250)).await;
            }
        }
        // A 201 means GitHub accepted the rerun request, not that the run
        // already exposes its new attempt. A lost response is equally
        // ambiguous. Keep the UUID reconcilable until the exact run advances.
        let _ = journal
            .mark_operation(&operation, OperationTransition::Indeterminate)
            .await;
        Err(OperationError::Indeterminate)
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn observe_pull_request_merge(
        &self,
        journal: &DeliveryJournal,
        mut request: ObservePullRequestMerge,
    ) -> Result<PullRequestMergeResult, OperationError> {
        request.validate()?;
        let repository = RepositoryName::requested(&mut request.repository)?;
        let mut enqueue_request = EnqueuePullRequest {
            repository: request.repository.clone(),
            operation_id: request.enqueue_operation_id.clone(),
            pull_number: request.pull_number,
            head_sha: request.head_sha.clone(),
            base: request.base.clone(),
        };
        enqueue_request.validate()?;
        let enqueue_operation = enqueue_request.operation("enqueue_pull_request")?;
        let enqueue_observation = journal
            .observe_operation(&request.enqueue_operation_id)
            .await
            .map_err(|_| OperationError::Unavailable)?
            .ok_or(OperationError::Conflict)?;
        if enqueue_observation.kind != enqueue_operation.kind
            || enqueue_observation.request_digest != enqueue_operation.request_digest
            || enqueue_observation.state != "completed"
        {
            return Err(OperationError::Conflict);
        }
        let enqueue: EnqueueResult = serde_json::from_str(
            enqueue_observation
                .result_json
                .as_deref()
                .ok_or(OperationError::Unavailable)?,
        )
        .map_err(|_| OperationError::Unavailable)?;
        if enqueue.pull_number != request.pull_number
            || enqueue.head_sha != request.head_sha
            || !valid_queue_state(&enqueue.state_when_recorded)
            || valid_text(&enqueue.entry_id, 1, 256, true).is_err()
        {
            return Err(OperationError::Unavailable);
        }
        let token = self
            .0
            .installation_token(
                repository,
                BTreeMap::from([
                    ("contents", "read"),
                    ("merge_queues", "read"),
                    ("metadata", "read"),
                    ("pull_requests", "read"),
                ]),
            )
            .await?;
        let repository = self.0.repository_metadata(&token).await?;
        if request.base != repository.default_branch {
            return Err(OperationError::Conflict);
        }
        let pull = self
            .0
            .verify_pull_request_head(&token, request.pull_number, &request.head_sha)
            .await?;
        if pull.base.name != request.base {
            return Err(OperationError::Conflict);
        }
        if !matches!(pull.state.as_str(), "open" | "closed") {
            return Err(OperationError::Unavailable);
        }
        if pull.merged {
            let merge_commit_sha = pull
                .merge_commit_sha
                .filter(|sha| valid_sha(sha).is_ok())
                .ok_or(OperationError::Indeterminate)?;
            return Ok(PullRequestMergeResult {
                pull_number: request.pull_number,
                head_sha: request.head_sha,
                base: request.base,
                pull_state: pull.state,
                state: MergeObservationState::MergedAfterEnqueueAttempt,
                entry_id: enqueue.entry_id,
                queue_state: None,
                merge_commit_sha: Some(merge_commit_sha),
            });
        }
        let queue = self
            .0
            .read_queue_entry(
                &token,
                &request.base,
                request.pull_number,
                &request.head_sha,
            )
            .await?;
        match queue {
            Some(entry) => {
                valid_text(&entry.id, 1, 256, true)?;
                if entry.id != enqueue.entry_id || !valid_queue_state(&entry.state) {
                    return Err(OperationError::Indeterminate);
                }
                Ok(PullRequestMergeResult {
                    pull_number: request.pull_number,
                    head_sha: request.head_sha,
                    base: request.base,
                    pull_state: pull.state,
                    state: MergeObservationState::ActiveQueue,
                    entry_id: entry.id,
                    queue_state: Some(entry.state),
                    merge_commit_sha: None,
                })
            }
            None => Ok(PullRequestMergeResult {
                pull_number: request.pull_number,
                head_sha: request.head_sha,
                base: request.base,
                pull_state: pull.state,
                state: MergeObservationState::NotQueued,
                entry_id: enqueue.entry_id,
                queue_state: None,
                merge_commit_sha: None,
            }),
        }
    }
}

impl CreatePullRequest {
    fn validate(&mut self) -> Result<(), OperationError> {
        canonical_operation_id(&mut self.operation_id)?;
        valid_exact_integer(self.issue_number)?;
        valid_ref(&self.head)?;
        valid_ref(&self.base)?;
        valid_sha(&self.head_sha)?;
        valid_sha(&self.base_sha)?;
        valid_text(&self.title, 1, 256, false)?;
        valid_text(&self.body, 0, 30_000, true)?;
        free_of_operation_marker(&self.body)?;
        if self.head == self.base || self.head_sha == self.base_sha {
            return Err(OperationError::InvalidInput);
        }
        Ok(())
    }

    #[cfg(target_arch = "wasm32")]
    fn operation(&self, kind: &str) -> Result<Operation, OperationError> {
        operation(kind, &self.operation_id, self)
    }

    fn marker(&self) -> Result<String, OperationError> {
        Ok(format!(
            "{OPERATION_MARKER_PREFIX}{}:{} -->",
            self.operation_id,
            request_digest(self)?
        ))
    }

    fn marked_body(&self) -> Result<String, OperationError> {
        let closes = format!("Closes #{}", self.issue_number);
        if self.body.is_empty() {
            Ok(format!("{}\n\n{}", closes, self.marker()?))
        } else {
            Ok(format!("{}\n\n{}\n\n{}", self.body, closes, self.marker()?))
        }
    }
}

impl UpdatePullRequestBody {
    fn validate(&mut self) -> Result<(), OperationError> {
        canonical_operation_id(&mut self.operation_id)?;
        valid_exact_integer(self.pull_number)?;
        valid_sha(&self.head_sha)?;
        valid_text(&self.body, 0, 30_000, true)?;
        free_of_operation_marker(&self.body)?;
        free_of_review_verdict(&self.body)
    }

    #[cfg(target_arch = "wasm32")]
    fn operation(&self, kind: &str) -> Result<Operation, OperationError> {
        operation(kind, &self.operation_id, self)
    }

    fn marker(&self) -> Result<String, OperationError> {
        Ok(format!(
            "{OPERATION_MARKER_PREFIX}{}:{} -->",
            self.operation_id,
            request_digest(self)?
        ))
    }

    fn marked_body(&self) -> Result<String, OperationError> {
        if self.body.is_empty() {
            self.marker()
        } else {
            Ok(format!("{}\n\n{}", self.body, self.marker()?))
        }
    }
}

impl ClosePullRequest {
    fn validate(&mut self) -> Result<(), OperationError> {
        canonical_operation_id(&mut self.operation_id)?;
        valid_exact_integer(self.pull_number)?;
        valid_sha(&self.head_sha)
    }

    #[cfg(target_arch = "wasm32")]
    fn operation(&self, kind: &str) -> Result<Operation, OperationError> {
        operation(kind, &self.operation_id, self)
    }
}

impl SubmitPullRequestReview {
    fn validate(&mut self) -> Result<(), OperationError> {
        canonical_operation_id(&mut self.operation_id)?;
        valid_exact_integer(self.pull_number)?;
        valid_sha(&self.head_sha)?;
        valid_text(&self.body, 1, 16_000, true)?;
        free_of_operation_marker(&self.body)?;
        free_of_review_verdict(&self.body)
    }

    #[cfg(target_arch = "wasm32")]
    fn operation(&self, kind: &str) -> Result<Operation, OperationError> {
        operation(kind, &self.operation_id, self)
    }

    fn marker(&self) -> Result<String, OperationError> {
        Ok(format!(
            "{OPERATION_MARKER_PREFIX}{}:{} -->",
            self.operation_id,
            request_digest(self)?
        ))
    }

    /// The reviewer's own text, then the App-rendered verdict, then the
    /// operation marker.
    ///
    /// The head SHA is repeated in the verdict line for a human reading the
    /// thread. It is not what the check trusts: GitHub records `commit_id`
    /// from the App's request, so that field is the binding, and the two
    /// cannot disagree because both are rendered from `head_sha` here.
    fn marked_body(&self) -> Result<String, OperationError> {
        Ok(format!(
            "{}\n\n{REVIEW_VERDICT_PREFIX} {} {}\n{}",
            self.body,
            self.event.verdict(),
            self.head_sha,
            self.marker()?
        ))
    }
}

impl PublishCommit {
    fn validate(&mut self) -> Result<(), OperationError> {
        canonical_operation_id(&mut self.operation_id)?;
        valid_ref(&self.branch)?;
        valid_sha(&self.expected_head_sha)?;
        // Keep caller text to one headline and reserve the body for the
        // operation trailer so the full message is byte-exact and trivial to
        // reconcile.
        valid_text(&self.message, 1, 4_096, false)?;
        free_of_operation_marker(&self.message)?;
        if !(1..=MAX_COMMIT_FILES).contains(&self.changes.len()) {
            return Err(OperationError::InvalidInput);
        }
        for (index, change) in self.changes.iter().enumerate() {
            valid_repository_path(&change.path)?;
            // A commit that touched the same path twice would leave which write
            // wins up to GitHub's tree ordering.
            if self.changes[..index]
                .iter()
                .any(|earlier| earlier.path == change.path)
            {
                return Err(OperationError::InvalidInput);
            }
            // A mode is a property of a file being written. Stated without
            // content it reads as "make this executable", and a change with no
            // content is a deletion -- so the request that looks like the
            // natural way to replay a mode-only change would delete the file.
            // There is no mode-only form; refuse it rather than silently mean
            // the opposite.
            if change.mode.is_some() && change.content_base64.is_none() {
                return Err(OperationError::InvalidInput);
            }
            // Modes are checked here, not where the tree is built. Every other
            // caller-supplied field is validated before the operation id is
            // claimed; leaving this one until tree construction meant a typo
            // burned the id, wrote blobs, and came back as `indeterminate` for
            // an input that was determinately invalid.
            if !matches!(change.mode.as_deref(), None | Some("100644" | "100755")) {
                return Err(OperationError::InvalidInput);
            }
            if let Some(content) = change.content_base64.as_deref() {
                if content.len() > MAX_COMMIT_FILE_BYTES {
                    return Err(OperationError::InvalidInput);
                }
                general_purpose::STANDARD
                    .decode(content)
                    .map_err(|_| OperationError::InvalidInput)?;
            }
        }
        Ok(())
    }

    #[cfg(target_arch = "wasm32")]
    fn operation(&self, kind: &str) -> Result<Operation, OperationError> {
        operation(kind, &self.operation_id, self)
    }

    fn trailer(&self) -> Result<String, OperationError> {
        Ok(format!(
            "{OPERATION_TRAILER_PREFIX} {} {}",
            self.operation_id,
            request_digest(self)?
        ))
    }

    /// The trailer makes a landed commit self-identifying, so a retry after an
    /// indeterminate failure can tell "already published" from "not published"
    /// by reading the branch rather than guessing.
    fn marked_message(&self) -> Result<String, OperationError> {
        Ok(format!("{}\n\n{}", self.message, self.trailer()?))
    }
}

impl EnqueuePullRequest {
    fn validate(&mut self) -> Result<(), OperationError> {
        canonical_operation_id(&mut self.operation_id)?;
        valid_exact_integer(self.pull_number)?;
        valid_sha(&self.head_sha)?;
        valid_ref(&self.base)
    }

    #[cfg(target_arch = "wasm32")]
    fn operation(&self, kind: &str) -> Result<Operation, OperationError> {
        operation(kind, &self.operation_id, self)
    }
}

impl MergePullRequestAtHead {
    fn validate(&mut self) -> Result<(), OperationError> {
        canonical_operation_id(&mut self.operation_id)?;
        canonical_operation_id(&mut self.review_operation_id)?;
        if self.operation_id == self.review_operation_id {
            return Err(OperationError::InvalidInput);
        }
        valid_exact_integer(self.pull_number)?;
        valid_sha(&self.head_sha)?;
        valid_ref(&self.base)
    }

    #[cfg(target_arch = "wasm32")]
    fn operation(&self, kind: &str) -> Result<Operation, OperationError> {
        operation(kind, &self.operation_id, self)
    }

    fn trailer(&self) -> Result<String, OperationError> {
        Ok(format!(
            "{OPERATION_TRAILER_PREFIX} {} {}",
            self.operation_id,
            request_digest(self)?
        ))
    }
}

impl ObservePullRequestChecks {
    fn validate(&self) -> Result<(), OperationError> {
        valid_exact_integer(self.pull_number)?;
        valid_sha(&self.head_sha)
    }
}

fn validate_pull_workflow(
    pull_number: i64,
    head_sha: &str,
    base: &str,
    workflow_path: &str,
) -> Result<(), OperationError> {
    valid_workflow_path(workflow_path)?;
    valid_exact_integer(pull_number)?;
    valid_sha(head_sha)?;
    valid_ref(base)
}

impl ObservePullRequestWorkflows {
    fn validate(&self) -> Result<(), OperationError> {
        validate_pull_workflow(
            self.pull_number,
            &self.head_sha,
            &self.base,
            &self.workflow_path,
        )
    }
}

impl ReadPullRequestJobLog {
    fn validate(&self) -> Result<(), OperationError> {
        validate_pull_workflow(
            self.pull_number,
            &self.head_sha,
            &self.base,
            &self.workflow_path,
        )?;
        valid_exact_integer(self.run_id)?;
        valid_exact_integer(self.run_attempt)?;
        valid_exact_integer(self.job_id)
    }
}

impl RerunFailedPullRequestJobs {
    fn validate(&mut self) -> Result<(), OperationError> {
        canonical_operation_id(&mut self.operation_id)?;
        validate_pull_workflow(
            self.pull_number,
            &self.head_sha,
            &self.base,
            &self.workflow_path,
        )?;
        valid_exact_integer(self.run_id)?;
        valid_exact_integer(self.run_attempt)
    }

    #[cfg(target_arch = "wasm32")]
    fn operation(&self, kind: &str) -> Result<Operation, OperationError> {
        operation(kind, &self.operation_id, self)
    }
}

impl PublishReleaseTag {
    fn validate(&mut self) -> Result<(), OperationError> {
        canonical_operation_id(&mut self.operation_id)?;
        valid_release_tag(&self.tag)?;
        valid_sha(&self.commit_sha)
    }

    #[cfg(target_arch = "wasm32")]
    fn operation(&self, kind: &str) -> Result<Operation, OperationError> {
        operation(kind, &self.operation_id, self)
    }
}

impl RecoverRelease {
    fn validate(&mut self) -> Result<(), OperationError> {
        canonical_operation_id(&mut self.operation_id)?;
        valid_release_tag(&self.tag)?;
        valid_sha(&self.commit_sha)?;
        valid_sha(&self.workflow_sha)
    }

    #[cfg(target_arch = "wasm32")]
    fn operation(&self, kind: &str) -> Result<Operation, OperationError> {
        operation(kind, &self.operation_id, self)
    }
}

impl DispatchControlPlaneDeploy {
    fn validate(&mut self) -> Result<(), OperationError> {
        canonical_operation_id(&mut self.operation_id)?;
        valid_sha(&self.commit_sha)?;
        valid_sha(&self.reviewed_tree)
    }

    #[cfg(target_arch = "wasm32")]
    fn operation(&self, kind: &str) -> Result<Operation, OperationError> {
        operation(kind, &self.operation_id, self)
    }
}

impl ObserveRelease {
    fn validate(&self) -> Result<(), OperationError> {
        valid_release_tag(&self.tag)?;
        valid_sha(&self.commit_sha)
    }
}

impl ObserveReleaseWorkflow {
    fn validate(&mut self) -> Result<(), OperationError> {
        canonical_operation_id(&mut self.operation_id)?;
        valid_release_tag(&self.tag)?;
        valid_sha(&self.tag_sha)?;
        valid_sha(&self.workflow_sha)?;
        valid_exact_integer(self.run_id)
    }
}

impl ObserveControlPlaneDeploy {
    fn validate(&mut self) -> Result<(), OperationError> {
        canonical_operation_id(&mut self.operation_id)?;
        valid_sha(&self.commit_sha)?;
        valid_sha(&self.reviewed_tree)?;
        valid_exact_integer(self.run_id)
    }
}

impl CreateIssue {
    fn validate(&mut self) -> Result<(), OperationError> {
        canonical_operation_id(&mut self.operation_id)?;
        valid_text(&self.title, 1, 256, false)?;
        valid_text(&self.body, 0, 30_000, true)?;
        free_of_operation_marker(&self.body)
    }

    #[cfg(target_arch = "wasm32")]
    fn operation(&self, kind: &str) -> Result<Operation, OperationError> {
        operation(kind, &self.operation_id, self)
    }

    fn marker(&self) -> Result<String, OperationError> {
        Ok(format!(
            "{OPERATION_MARKER_PREFIX}{}:{} -->",
            self.operation_id,
            request_digest(self)?
        ))
    }

    fn marked_body(&self) -> Result<String, OperationError> {
        if self.body.is_empty() {
            self.marker()
        } else {
            Ok(format!("{}\n\n{}", self.body, self.marker()?))
        }
    }
}

impl ObserveRef {
    fn validate(&self) -> Result<(), OperationError> {
        valid_ref(&self.branch)
    }
}

impl ObserveFile {
    fn validate(&self) -> Result<(), OperationError> {
        valid_sha(&self.commit_sha)?;
        // Reading is bounded by what `observe_tree` can name, not by the
        // narrower rule governing what may be written. Naming a file and then
        // refusing to read it would make the two operations disagree about
        // which files exist.
        valid_read_path(&self.path)
    }
}

impl ObserveTree {
    fn validate(&self) -> Result<(), OperationError> {
        valid_sha(&self.commit_sha)
    }
}

impl ObserveIssue {
    fn validate(&self) -> Result<(), OperationError> {
        valid_exact_integer(self.issue_number)
    }
}

impl ResolveIssue {
    fn validate(&mut self) -> Result<(), OperationError> {
        canonical_operation_id(&mut self.operation_id)?;
        valid_exact_integer(self.issue_number)?;
        valid_text(&self.body, 1, 16_000, true)?;
        free_of_operation_marker(&self.body)
    }

    #[cfg(target_arch = "wasm32")]
    fn operation(&self, kind: &str) -> Result<Operation, OperationError> {
        operation(kind, &self.operation_id, self)
    }

    fn marker(&self) -> Result<String, OperationError> {
        Ok(format!(
            "{OPERATION_MARKER_PREFIX}{}:{} -->",
            self.operation_id,
            request_digest(self)?
        ))
    }

    fn marked_body(&self) -> Result<String, OperationError> {
        Ok(format!("{}\n\n{}", self.body, self.marker()?))
    }
}

impl ObservePullRequestMerge {
    fn validate(&mut self) -> Result<(), OperationError> {
        canonical_operation_id(&mut self.enqueue_operation_id)?;
        valid_exact_integer(self.pull_number)?;
        valid_sha(&self.head_sha)?;
        valid_ref(&self.base)
    }
}

/// Check one operation ID's shape and settle its case in place.
///
/// A UUID reads case-insensitively but does not compare that way, and
/// everything downstream compares this one byte for byte: it is the journal's
/// primary key, it is hashed into the request digest alongside the rest of the
/// request, and it is rendered into the marker `reconcile_*` finds its own
/// work by with `contains`. Two casings of one UUID would therefore be two
/// operations sharing no replay identity, which is why a single canonical case
/// has to exist.
///
/// Refusing the other case was the previous way of getting one, and it was the
/// wrong way: macOS `uuidgen` emits uppercase, so every stock-Mac caller spent
/// a blind `invalid_input` on its first write and learned nothing from it,
/// twice in two days (#377). Lowering it here instead is settled before the ID
/// is digested, journalled, or rendered, so the same UUID in either case is
/// the same operation everywhere. Nothing already durable can collide with a
/// newly lowered ID, because lowercase is the only case the journal was ever
/// able to store.
pub(crate) fn canonical_operation_id(value: &mut str) -> Result<(), OperationError> {
    let valid = value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        });
    if !valid {
        return Err(OperationError::InvalidInput);
    }
    value.make_ascii_lowercase();
    Ok(())
}

/// A reconciler identifies its own work by a marker the caller does not get to
/// write: `publish_commit` puts `Dark-Factory-Operation: <id>` in the commit
/// message, and the pull-request operations put an HTML comment in the body.
/// Both are matched with `contains`, so caller text carrying either prefix
/// could name a *different* operation's id and make that operation's
/// reconciliation adopt work it never did. Refuse the prefix at the door
/// rather than trying to out-parse it afterwards.
const OPERATION_TRAILER_PREFIX: &str = "Dark-Factory-Operation:";
const OPERATION_MARKER_PREFIX: &str = "<!-- dark-factory-operation:";

fn free_of_operation_marker(value: &str) -> Result<(), OperationError> {
    let forged =
        value.contains(OPERATION_TRAILER_PREFIX) || value.contains(OPERATION_MARKER_PREFIX);
    (!forged).then_some(()).ok_or(OperationError::InvalidInput)
}

/// The line the required `review` check reads to decide whether its exact-head
/// contract is satisfied. `ReviewEvent::verdict` renders it; review
/// bodies are refused if they carry the prefix, for the same reason the
/// operation marker is refused: a caller that could write this line could
/// state a verdict it never asked the App to record, and the check would
/// believe it.
///
/// Only review bodies are constrained. A commit message or pull request body
/// mentioning the prefix is harmless -- the check reads reviews and nothing
/// else -- and refusing it there would stop the documentation describing the
/// format from being publishable.
const REVIEW_VERDICT_PREFIX: &str = "Dark-Factory-Review:";

fn free_of_review_verdict(value: &str) -> Result<(), OperationError> {
    (!value.contains(REVIEW_VERDICT_PREFIX))
        .then_some(())
        .ok_or(OperationError::InvalidInput)
}

/// A path inside the repository, as a commit may address it.
///
/// The `.github` authority tree is refused explicitly. GitHub already blocks
/// `.github/workflows/` without the `workflows` permission, which this App
/// deliberately does not hold, but a policy-checked surface should state the
/// boundary rather than depend on a permission staying un-granted: an agent
/// that could rewrite the CI gating its own work would be escalating its
/// authority.
///
/// The refusal is by path segment, not by prefix. A tree entry may name a
/// directory as a blob, which replaces the whole subtree, so `.github` and
/// `.github/workflows` must be refused as paths in their own right and not
/// only as prefixes of a file inside them.
///
/// The review and dependency policy files are refused by name for the same
/// reason. Refusing the `.github` tree stops an agent destroying them
/// wholesale but not rewriting one of them in place, and an agent that can
/// edit CODEOWNERS can remove itself from required review — the same
/// escalation, reached one file at a time. GitHub reads CODEOWNERS from the
/// repository root, `.github/`, and `docs/`, so all three are named.
///
/// Each of those names is also refused as a leading directory. A tree entry
/// at `.github/CODEOWNERS/x` turns the CODEOWNERS blob into a tree, deleting
/// its content as a side effect: exactly the write the by-name refusal
/// exists to prevent, reached by making the name a directory instead. The
/// collision is with the *leading* segments only, so a path that merely
/// contains a protected name -- `CODEOWNERS.md`, `docs/notes-on-CODEOWNERS.md`
/// -- stays publishable, and so does one that carries it as a whole segment
/// somewhere GitHub reads no authority from, such as `src/CODEOWNERS`.
///
/// The `.github` tree needs no such rule. It is already refused as a path in
/// its own right, so no blob can replace it, and `.github/workflows` is
/// refused at any depth by the segment test, which is that directory's
/// prefix rule. The rest of `.github` is a tree this surface deliberately
/// publishes into -- `.github/ISSUE_TEMPLATE/**` and
/// `.github/PULL_REQUEST_TEMPLATE.md` stay allowed -- which is why the three
/// `.github/*` entries above are refused by name one at a time. A blanket
/// prefix rule there would refuse intended writes rather than close a hole.
fn valid_repository_path(value: &str) -> Result<(), OperationError> {
    const REVIEW_AUTHORITY_PATHS: &[&str] = &[
        "CODEOWNERS",
        ".github/CODEOWNERS",
        "docs/CODEOWNERS",
        ".github/dependabot.yml",
        ".github/dependabot.yaml",
    ];

    let mut segments = value.split('/');
    let github_authority = segments
        .next()
        .is_some_and(|segment| segment.eq_ignore_ascii_case(".github"))
        && segments
            .next()
            .is_none_or(|segment| segment.eq_ignore_ascii_case("workflows"));
    let review_authority = REVIEW_AUTHORITY_PATHS.iter().any(|&protected| {
        value
            .strip_prefix(protected)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    });
    valid_path(value)?;
    (!github_authority && !review_authority)
        .then_some(())
        .ok_or(OperationError::InvalidInput)
}

/// The two decisions `observe_tree` makes about a commit and its tree, split
/// out so both are host-testable. The transport around it is `wasm32`-only, and
/// a grep-style assertion on the source could neither catch `.take(1)` dropping
/// a merge commit's second parent nor survive a behaviour-identical refactor.
#[cfg(any(target_arch = "wasm32", test))]
fn tree_observation(
    commit_sha: String,
    commit: GitCommit,
    tree: GitTree,
) -> Result<TreeObservationResult, OperationError> {
    // Truncation is GitHub's own fact about its own answer, and a partial
    // listing read as complete is the one outcome a caller cannot recover from.
    // It is a determinate refusal that names itself, not an indeterminate
    // outcome: this operation claims no id and writes no journal record, so
    // there is nothing to reconcile, and the same commit truncates every time.
    if tree.truncated {
        return Err(OperationError::Refused(RefusalReason::TreeTruncated));
    }
    Ok(TreeObservationResult {
        commit_sha,
        tree_sha: commit.tree.sha,
        // Every parent. A merge commit has two, and dropping the second makes
        // ancestry unwalkable exactly where branches actually meet.
        parents: commit
            .parents
            .into_iter()
            .map(|parent| parent.sha)
            .collect(),
        // Reported as GitHub returned them. Shape-checking each entry made one
        // unusual path -- long, or carrying a byte git permits -- break every
        // read of that repository at every commit, for data the caller does not
        // control and cannot fix. The response is already bounded by
        // `MAX_GITHUB_RESPONSE_BYTES`, above GitHub's own tree ceiling.
        entries: tree
            .tree
            .into_iter()
            .map(|entry| TreeEntryResult {
                path: entry.path,
                kind: entry.kind,
                mode: entry.mode,
                sha: entry.sha,
            })
            .collect(),
    })
}

/// A path's shape, with no authority policy attached.
///
/// Reading is not writing. The `.github` and CODEOWNERS refusals exist because
/// an agent that could *rewrite* the CI gating its own work would be escalating
/// its authority; reading those files escalates nothing, and refusing to read
/// them would leave an agent unable to see the gate it must satisfy.
fn valid_path(value: &str) -> Result<(), OperationError> {
    valid_bounded_path(value, 240)
}

/// A path this surface may *name*, which is wider than one it may write. Git
/// permits far longer paths than `publish_commit` accepts, and a repository
/// containing one is still a repository an agent must be able to read.
#[cfg(any(target_arch = "wasm32", test))]
fn valid_read_path(value: &str) -> Result<(), OperationError> {
    valid_bounded_path(value, 4_096)
}

fn valid_bounded_path(value: &str, max: usize) -> Result<(), OperationError> {
    let valid = !value.is_empty()
        && value.len() <= max
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.contains("//")
        && !value.split('/').any(|segment| {
            segment.is_empty() || segment == "." || segment == ".." || segment == ".git"
        })
        && value
            .chars()
            .all(|character| !character.is_control() && character != '\\');
    valid.then_some(()).ok_or(OperationError::InvalidInput)
}

fn valid_ref(value: &str) -> Result<(), OperationError> {
    let valid = !value.is_empty()
        && value.len() <= 240
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.ends_with('.')
        && !value.contains("..")
        && !value.contains("//")
        && !value.contains("@{")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/'));
    valid.then_some(()).ok_or(OperationError::InvalidInput)
}

fn valid_release_tag(value: &str) -> Result<(), OperationError> {
    let valid_characters = value.strip_prefix('v').is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    });
    let version = value.strip_prefix('v').unwrap_or_default();
    let (core, suffix_valid) = match version.split_once('-') {
        Some((core, suffix)) => (core, !suffix.is_empty()),
        None => (version, true),
    };
    let numeric = core.split('.').collect::<Vec<_>>();
    let valid_core = numeric.len() == 3
        && numeric
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    (valid_characters && suffix_valid && valid_core)
        .then_some(())
        .ok_or(OperationError::InvalidInput)
}

fn valid_sha(value: &str) -> Result<(), OperationError> {
    (value.len() == 40
        && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        && value == value.to_ascii_lowercase())
    .then_some(())
    .ok_or(OperationError::InvalidInput)
}

fn valid_exact_integer(value: i64) -> Result<(), OperationError> {
    (1..=MAX_EXACT_INTEGER)
        .contains(&value)
        .then_some(())
        .ok_or(OperationError::InvalidInput)
}

fn valid_text(
    value: &str,
    min: usize,
    max: usize,
    allow_newline: bool,
) -> Result<(), OperationError> {
    let len = value.len();
    let valid = (min..=max).contains(&len)
        && value.chars().all(|character| {
            !character.is_control() || (allow_newline && matches!(character, '\n' | '\r' | '\t'))
        });
    valid.then_some(()).ok_or(OperationError::InvalidInput)
}

#[cfg(target_arch = "wasm32")]
fn operation<T: Serialize>(
    kind: &str,
    operation_id: &str,
    request: &T,
) -> Result<Operation, OperationError> {
    Ok(Operation {
        operation_id: operation_id.to_owned(),
        kind: kind.to_owned(),
        request_digest: request_digest(request)?,
    })
}

#[cfg(any(target_arch = "wasm32", test))]
fn request_digest<T: Serialize>(request: &T) -> Result<String, OperationError> {
    let canonical = serde_json::to_vec(request).map_err(|_| OperationError::InvalidInput)?;
    Ok(hex::encode(Sha256::digest(canonical)))
}

#[cfg(target_arch = "wasm32")]
fn completed_or_conflict<T: serde::de::DeserializeOwned>(
    state: &OperationRecord,
) -> Result<Option<T>, OperationError> {
    match state {
        OperationRecord::Completed(result) => serde_json::from_str(result)
            .map(Some)
            .map_err(|_| OperationError::Unavailable),
        OperationRecord::Conflict => Err(OperationError::Conflict),
        _ => Ok(None),
    }
}

#[cfg(target_arch = "wasm32")]
async fn complete<T: Serialize + serde::de::DeserializeOwned>(
    journal: &DeliveryJournal,
    operation: &Operation,
    result: T,
) -> Result<T, OperationError> {
    let result_json = serde_json::to_string(&result).map_err(|_| OperationError::Unavailable)?;
    match journal
        .mark_operation(operation, OperationTransition::Completed(result_json))
        .await
        .map_err(|_| OperationError::Unavailable)?
    {
        OperationRecord::Completed(stored) => {
            serde_json::from_str(&stored).map_err(|_| OperationError::Unavailable)
        }
        OperationRecord::Conflict => Err(OperationError::Conflict),
        _ => Err(OperationError::Unavailable),
    }
}

#[cfg(target_arch = "wasm32")]
async fn refuse<T>(
    journal: &DeliveryJournal,
    operation: &Operation,
    reason: RefusalReason,
) -> Result<T, OperationError> {
    journal
        .mark_operation(operation, OperationTransition::Refused)
        .await
        .map_err(|_| OperationError::Unavailable)?;
    Err(OperationError::Refused(reason))
}

struct RepositoryName {
    full_name: String,
    owner: String,
    name: String,
}

impl RepositoryName {
    /// Caller-supplied `owner/name`, lower-cased in place before it is read.
    ///
    /// Shape is rejected here so a malformed name never reaches a URL, and the
    /// error is `InvalidInput` — the caller's mistake — rather than the
    /// `Configuration` a deployment secret would be.
    ///
    /// The normalization is not cosmetic. GitHub resolves repository names
    /// case-insensitively, but a replay identity is a digest over the exact
    /// request, so `Owner/Repo` and `owner/repo` would name one repository and
    /// two operations: a retry differing only in case would conflict with
    /// itself forever. This runs before the digest is taken, which makes the
    /// two spellings one request. The one comparison against a name GitHub
    /// returns while the caller's spelling is still in hand -- the token
    /// grant's -- is therefore case-insensitive. Afterwards the token carries
    /// GitHub's own spelling, so later comparisons are exact.
    fn requested(value: &mut str) -> Result<Self, OperationError> {
        value.make_ascii_lowercase();
        Self::new(value.to_owned())
    }

    fn new(value: String) -> Result<Self, OperationError> {
        let Some((owner, name)) = value.split_once('/') else {
            return Err(OperationError::InvalidInput);
        };
        if value.matches('/').count() != 1
            || !valid_path_segment(owner, 39, false)
            || !valid_path_segment(name, 100, true)
        {
            return Err(OperationError::InvalidInput);
        }
        let owner = owner.to_owned();
        let name = name.to_owned();
        Ok(Self {
            full_name: value,
            owner,
            name,
        })
    }

    fn installation_url(&self) -> String {
        format!(
            "https://api.github.com/repos/{}/{}/installation",
            self.owner, self.name
        )
    }
}

/// An installation token together with the repository it was minted for.
/// Held apart, a token for one repository could address another; held together,
/// every request is spelled from the same grant that authenticates it. The
/// numeric id is GitHub's answer to the name, not the caller's claim about it.
#[cfg(any(target_arch = "wasm32", test))]
struct RepositoryToken {
    token: Credential,
    repository: RepositoryName,
    repository_id: i64,
}

#[cfg(any(target_arch = "wasm32", test))]
impl RepositoryToken {
    fn as_str(&self) -> &str {
        self.token.as_str()
    }
}

/// The App the private key belongs to, which is all readiness can prove without
/// naming a repository.
#[cfg(target_arch = "wasm32")]
#[derive(Deserialize)]
struct AppIdentity {
    id: i64,
}

fn valid_path_segment(value: &str, max: usize, allow_dot_underscore: bool) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value != "."
        && value != ".."
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || byte == b'-'
                || (allow_dot_underscore && (byte == b'.' || byte == b'_'))
        })
}

fn jwt_unsigned(app_id: i64, now: i64) -> String {
    let header = general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = format!(
        r#"{{"iat":{},"exp":{},"iss":"{}"}}"#,
        now - 60,
        now + 540,
        app_id
    );
    format!(
        "{header}.{}",
        general_purpose::URL_SAFE_NO_PAD.encode(claims)
    )
}

#[cfg(target_arch = "wasm32")]
impl Authority {
    async fn verify(&self) -> Result<(), Error> {
        // Readiness is an App fact, not a repository fact. There is no configured
        // repository to look an installation up by, and inventing one would make
        // readiness answer a narrower question than the App can serve. Prove the
        // key signs and that the App this key belongs to is the App we claim to
        // be — and mint nothing. This endpoint is reachable unauthenticated
        // through /readyz, so issuing a credential here would let a stranger
        // exhaust the App's rate limit and disable every real operation. The
        // installation, its permissions, and the repository identity are all
        // established per operation by `installation_token`, which is the
        // correct enforcement point because it is the one that grants access.
        let jwt = self.jwt().await?;
        let app: AppIdentity =
            github_json_as_app("https://api.github.com/app", jwt.as_str()).await?;
        (app.id == self.app_id)
            .then_some(())
            .ok_or(Error::Unavailable)
    }

    async fn jwt(&self) -> Result<Credential, Error> {
        let now = (js_sys::Date::now() / 1_000.0).floor() as i64;
        let unsigned = jwt_unsigned(self.app_id, now);
        let signature = sign_rs256(&self.private_key.0, unsigned.as_bytes())
            .await
            .inspect_err(|_| worker::console_error!("app jwt signing failed"))?;
        Credential::new(format!(
            "{unsigned}.{}",
            general_purpose::URL_SAFE_NO_PAD.encode(signature)
        ))
    }

    async fn installation_token(
        &self,
        repository: RepositoryName,
        permissions: BTreeMap<&'static str, &'static str>,
    ) -> Result<RepositoryToken, OperationError> {
        let jwt = self.jwt().await?;
        let installation: Installation =
            match github_json_as_app(&repository.installation_url(), jwt.as_str()).await {
                Ok(installation) => installation,
                // "Not installed here" is the likeliest caller mistake on this
                // surface, and it is not a refusal of the operation's target.
                Err(Error::Rejected(404)) => {
                    return Err(OperationError::Refused(
                        RefusalReason::RepositoryNotInstalled,
                    ));
                }
                Err(error) => return Err(error.into()),
            };
        validate_installation(&installation, self.app_id).map_err(|defect| {
            OperationError::Refused(RefusalReason::InstallationRejected(defect))
        })?;
        // Named, not numbered: the caller supplies `owner/name`, and the numeric
        // id is what GitHub hands back for it. Requesting by id would need an id
        // the caller cannot be trusted to supply and this service no longer
        // stores.
        #[derive(Serialize)]
        struct TokenRequest<'a> {
            repositories: [&'a str; 1],
            permissions: BTreeMap<&'static str, &'static str>,
        }
        let expected_permissions = permissions
            .iter()
            .map(|(name, level)| ((*name).to_owned(), (*level).to_owned()))
            .collect::<BTreeMap<_, _>>();
        let token_url = format!(
            "https://api.github.com/app/installations/{}/access_tokens",
            installation.id
        );
        if !app_jwt_endpoint(&token_url) {
            worker::console_error!("refusing to present an app jwt to {token_url}");
            return Err(OperationError::Unavailable);
        }
        let response: InstallationToken = github_json_request(
            worker::Method::Post,
            &token_url,
            jwt.as_str(),
            Some(&TokenRequest {
                repositories: [repository.name.as_str()],
                permissions,
            }),
        )
        .await?;
        // Exactly one repository, exactly the one asked for, owned by the
        // installation the grant came from, and exactly the permissions
        // requested. A token that covers anything else is not the token this
        // operation was authorized to hold.
        let [granted] = response.repositories.as_slice() else {
            worker::console_error!(
                "installation token covers {} repositories, expected 1",
                response.repositories.len()
            );
            return Err(OperationError::Unavailable);
        };
        // Three independent conditions. Two of them became caller-reachable
        // when the repository stopped being configuration, so a single line
        // reporting a permission count would send the reader to the wrong one.
        let mismatch = if response.permissions != expected_permissions {
            Some("permissions")
        } else if !granted
            .full_name
            .eq_ignore_ascii_case(&repository.full_name)
        {
            Some("repository")
        } else if granted.owner.id != installation.account.id {
            Some("owner")
        } else {
            None
        };
        if let Some(mismatch) = mismatch {
            worker::console_error!("installation token contract mismatch on {mismatch}");
            return Err(OperationError::Unavailable);
        }
        valid_exact_integer(granted.id)?;
        Ok(RepositoryToken {
            repository_id: granted.id,
            // GitHub's own spelling, not the caller's. Path segments resolve
            // case-insensitively, but query filters are not documented to --
            // `head={owner}:{ref}` on the pull request list in particular, where
            // a missed match reads as "no such pull request" and lets a
            // reconciliation publish a duplicate. Taking the name from the grant
            // costs nothing and removes the question.
            repository: RepositoryName::new(granted.full_name.clone())?,
            token: Credential::new(response.token)?,
        })
    }

    async fn repository_metadata(
        &self,
        token: &RepositoryToken,
    ) -> Result<RepositoryMetadata, OperationError> {
        let metadata: RepositoryMetadata = github_json(
            &format!(
                "https://api.github.com/repos/{}/{}",
                token.repository.owner, token.repository.name
            ),
            token.as_str(),
        )
        .await?;
        metadata.validate(token)
    }

    async fn repository_snapshot(
        &self,
        token: &RepositoryToken,
    ) -> Result<RepositoryResult, OperationError> {
        let metadata = self.repository_metadata(token).await?;
        let reference = self.read_ref(token, &metadata.default_branch).await?;
        if reference.object.kind != "commit" {
            return Err(OperationError::Unavailable);
        }
        valid_sha(&reference.object.sha)?;
        Ok(RepositoryResult {
            // GitHub's canonical spelling, not the caller's. The id already
            // comes from the installation grant.
            repository: metadata.full_name,
            repository_id: token.repository_id,
            default_branch: metadata.default_branch,
            default_sha: reference.object.sha,
        })
    }

    /// One blob at one commit. The `contents` media type returns base64 for a
    /// file of any type, so a binary file reads back exactly as it would be
    /// written. A directory answers as an array, which fails to deserialize
    /// into a file and is refused rather than guessed at.
    async fn read_file(
        &self,
        token: &RepositoryToken,
        commit_sha: &str,
        path: &str,
    ) -> Result<Option<String>, OperationError> {
        #[derive(Deserialize)]
        struct Content {
            #[serde(rename = "type")]
            kind: String,
            path: String,
            content: String,
            encoding: String,
            size: i64,
        }
        let url = format!(
            "https://api.github.com/repos/{}/{}/contents/{}?ref={commit_sha}",
            token.repository.owner,
            token.repository.name,
            path.split('/')
                .map(percent_encode)
                .collect::<Vec<_>>()
                .join("/")
        );
        let content: Content = match github_json(&url, token.as_str()).await {
            Ok(content) => content,
            // The contents endpoint answers 404 for two different facts: the
            // path is absent at that commit, and the commit does not exist.
            // Reporting the second as the first tells a caller a file was
            // deleted when it named a commit that was never there, and it would
            // integrate that deletion. Only this branch pays for the
            // disambiguation.
            Err(Error::Rejected(404)) => {
                // Reading the commit answers this; reading its tree as well
                // cost a whole repository download and, worse, inherited the
                // tree's truncation refusal -- so in exactly the large
                // repositories this surface exists for, observing an absent
                // path became impossible.
                self.read_commit(token, commit_sha).await?;
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        if content.kind != "file"
            || content.path != path
            || content.encoding != "base64"
            // Decoded size, so it is the weaker of the two bounds; the
            // encoded-length check below is what actually caps the transfer.
            // This one refuses a negative or absurd self-report before any of
            // it is trusted.
            || !(0..=MAX_COMMIT_FILE_BYTES as i64).contains(&content.size)
        {
            return Err(OperationError::Conflict);
        }
        // GitHub wraps base64 at 60 columns; the newlines are not content.
        let encoded: String = content.content.split_ascii_whitespace().collect();
        if encoded.len() > MAX_COMMIT_FILE_BYTES {
            return Err(OperationError::Conflict);
        }
        Ok(Some(encoded))
    }

    async fn read_tree(
        &self,
        token: &RepositoryToken,
        commit_sha: String,
    ) -> Result<TreeObservationResult, OperationError> {
        let commit = self.read_commit(token, &commit_sha).await?;
        let tree: GitTree = github_json(
            &format!(
                "https://api.github.com/repos/{}/{}/git/trees/{}?recursive=1",
                token.repository.owner, token.repository.name, commit.tree.sha
            ),
            token.as_str(),
        )
        .await?;
        tree_observation(commit_sha, commit, tree)
    }

    /// The commit itself, which is also the cheapest proof that it exists.
    async fn read_commit(
        &self,
        token: &RepositoryToken,
        commit_sha: &str,
    ) -> Result<GitCommit, OperationError> {
        let commit: GitCommit = match github_json(
            &format!(
                "https://api.github.com/repos/{}/{}/git/commits/{commit_sha}",
                token.repository.owner, token.repository.name
            ),
            token.as_str(),
        )
        .await
        {
            Ok(commit) => commit,
            // A commit that is not in this repository is the caller naming a
            // thing that does not exist, not GitHub refusing a mutation whose
            // precondition may later hold.
            Err(Error::Rejected(404)) => return Err(OperationError::Conflict),
            Err(error) => return Err(error.into()),
        };
        valid_sha(&commit.tree.sha)?;
        Ok(commit)
    }

    async fn read_ref(
        &self,
        token: &RepositoryToken,
        branch: &str,
    ) -> Result<GitReference, OperationError> {
        self.read_ref_optional(token, branch)
            .await?
            .ok_or(OperationError::Unavailable)
    }

    async fn read_ref_optional(
        &self,
        token: &RepositoryToken,
        branch: &str,
    ) -> Result<Option<GitReference>, OperationError> {
        let reference: GitReference = match github_json(
            &format!(
                "https://api.github.com/repos/{}/{}/git/ref/heads/{}",
                token.repository.owner,
                token.repository.name,
                percent_encode(branch)
            ),
            token.as_str(),
        )
        .await
        {
            Ok(reference) => reference,
            Err(Error::Rejected(404)) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if reference.name != format!("refs/heads/{branch}")
            || reference.object.kind != "commit"
            || valid_sha(&reference.object.sha).is_err()
        {
            return Err(OperationError::Unavailable);
        }
        Ok(Some(reference))
    }

    async fn post_issue(
        &self,
        token: &RepositoryToken,
        request: &CreateIssue,
    ) -> Result<IssueResult, OperationError> {
        #[derive(Serialize)]
        struct Body {
            title: String,
            body: String,
        }
        let issue: Issue = github_json_request(
            worker::Method::Post,
            &format!(
                "https://api.github.com/repos/{}/{}/issues",
                token.repository.owner, token.repository.name
            ),
            token.as_str(),
            Some(&Body {
                title: request.title.clone(),
                body: request.marked_body()?,
            }),
        )
        .await?;
        if !issue.matches_create(request) {
            return Err(OperationError::Indeterminate);
        }
        issue.into_result()
    }

    async fn reconcile_issue(
        &self,
        token: &RepositoryToken,
        request: &CreateIssue,
    ) -> Result<Option<IssueResult>, OperationError> {
        #[derive(Deserialize)]
        struct SearchResult {
            total_count: i64,
            items: Vec<Issue>,
        }
        let query = format!(
            "repo:{} \"{}\"",
            token.repository.full_name,
            request.marker()?
        );
        let response: SearchResult = github_json(
            &format!(
                "https://api.github.com/search/issues?q={}&per_page=100",
                percent_encode(&query)
            ),
            token.as_str(),
        )
        .await?;
        if !(0..=100).contains(&response.total_count)
            || response.total_count as usize != response.items.len()
        {
            return Err(OperationError::Indeterminate);
        }
        let matches = response
            .items
            .into_iter()
            .filter(|issue| issue.matches_create(request))
            .map(Issue::into_result)
            .collect::<Result<Vec<_>, _>>()?;
        match matches.as_slice() {
            [] => Ok(None),
            [result] => Ok(Some(IssueResult {
                number: result.number,
                url: result.url.clone(),
            })),
            _ => Err(OperationError::Indeterminate),
        }
    }

    async fn read_issue(
        &self,
        token: &RepositoryToken,
        issue_number: i64,
    ) -> Result<Issue, OperationError> {
        let issue: Issue = github_json(
            &format!(
                "https://api.github.com/repos/{}/{}/issues/{issue_number}",
                token.repository.owner, token.repository.name
            ),
            token.as_str(),
        )
        .await?;
        if issue.number != issue_number {
            return Err(OperationError::Conflict);
        }
        valid_github_url(&issue.html_url)?;
        if !matches!(issue.state.as_str(), "open" | "closed") {
            return Err(OperationError::Indeterminate);
        }
        Ok(issue)
    }

    async fn read_issue_resolution_comment(
        &self,
        token: &RepositoryToken,
        request: &ResolveIssue,
    ) -> Result<Option<IssueComment>, OperationError> {
        let mut found = None;
        for page in 1..=MAX_ISSUE_COMMENT_PAGES {
            let comments: Vec<IssueComment> = github_json(
                &format!(
                    "https://api.github.com/repos/{}/{}/issues/{}/comments?per_page={MAX_ISSUE_COMMENTS_PER_PAGE}&page={page}",
                    token.repository.owner, token.repository.name, request.issue_number
                ),
                token.as_str(),
            )
            .await?;
            if comments.len() > MAX_ISSUE_COMMENTS_PER_PAGE {
                return Err(OperationError::Indeterminate);
            }
            let page_is_full = comments.len() == MAX_ISSUE_COMMENTS_PER_PAGE;
            for comment in comments {
                let comment = comment.validate()?;
                if comment.matches(request) {
                    if found.is_some() {
                        return Err(OperationError::Indeterminate);
                    }
                    found = Some(comment);
                }
            }
            if !page_is_full {
                return Ok(found);
            }
        }
        // A full final page may hide another matching marker. Do not post a
        // second comment when the bounded search cannot prove absence.
        Err(OperationError::Indeterminate)
    }

    async fn reconcile_issue_resolution(
        &self,
        token: &RepositoryToken,
        request: &ResolveIssue,
    ) -> Result<IssueResolutionProgress, OperationError> {
        let issue = self.read_issue(token, request.issue_number).await?;
        if !issue.is_real_issue() {
            return Err(OperationError::Conflict);
        }
        let comment = self.read_issue_resolution_comment(token, request).await?;
        Ok(IssueResolutionProgress { issue, comment })
    }

    async fn post_issue_comment(
        &self,
        token: &RepositoryToken,
        request: &ResolveIssue,
    ) -> Result<IssueComment, OperationError> {
        #[derive(Serialize)]
        struct Body {
            body: String,
        }
        let comment: IssueComment = github_json_request(
            worker::Method::Post,
            &format!(
                "https://api.github.com/repos/{}/{}/issues/{}/comments",
                token.repository.owner, token.repository.name, request.issue_number
            ),
            token.as_str(),
            Some(&Body {
                body: request.marked_body()?,
            }),
        )
        .await?;
        let comment = comment.validate()?;
        comment
            .matches(request)
            .then_some(comment)
            .ok_or(OperationError::Indeterminate)
    }

    async fn close_issue(
        &self,
        token: &RepositoryToken,
        request: &ResolveIssue,
    ) -> Result<Issue, OperationError> {
        #[derive(Serialize)]
        struct Body {
            state: &'static str,
            state_reason: &'static str,
        }
        let issue: Issue = github_json_request(
            worker::Method::Patch,
            &format!(
                "https://api.github.com/repos/{}/{}/issues/{}",
                token.repository.owner, token.repository.name, request.issue_number
            ),
            token.as_str(),
            Some(&Body {
                state: "closed",
                state_reason: request.state_reason.as_str(),
            }),
        )
        .await?;
        if issue.number != request.issue_number || !issue.is_real_issue() {
            return Err(OperationError::Conflict);
        }
        valid_github_url(&issue.html_url)?;
        Ok(issue)
    }

    /// Either the branch is at exactly `expected_head_sha`, or it does not exist
    /// yet and `expected_head_sha` is the commit it will start from. A missing
    /// branch is created only after the operation is claimed, so a lost create
    /// response can be reconciled without guessing.
    async fn verify_publish_precondition(
        &self,
        token: &RepositoryToken,
        request: &PublishCommit,
    ) -> Result<bool, OperationError> {
        match self.read_ref_optional(token, &request.branch).await? {
            Some(reference) => (reference.object.sha == request.expected_head_sha)
                .then_some(true)
                .ok_or(OperationError::Conflict),
            // Only a 404 means the branch is absent. Reading any other failure
            // as absence sent a publish onto a branch sitting exactly at
            // `expected_head_sha` down the create path, where `POST /git/refs`
            // answers "Reference already exists" and wedges the operation at
            // indeterminate for a branch that never moved.
            None => {
                // The parent must still be a real commit, so a typo cannot
                // create a branch from nothing.
                let _: GitCommit = github_json(
                    &format!(
                        "https://api.github.com/repos/{}/{}/git/commits/{}",
                        token.repository.owner, token.repository.name, request.expected_head_sha
                    ),
                    token.as_str(),
                )
                .await?;
                Ok(false)
            }
        }
    }

    async fn push_commit(
        &self,
        token: &RepositoryToken,
        request: &PublishCommit,
        branch_exists: bool,
    ) -> Result<CommitResult, OperationError> {
        let tree = self.materialize_tree(token, request).await?;
        let commit: GitObjectId = github_json_request(
            worker::Method::Post,
            &format!(
                "https://api.github.com/repos/{}/{}/git/commits",
                token.repository.owner, token.repository.name
            ),
            token.as_str(),
            Some(&CommitRequest {
                message: request.marked_message()?,
                tree: &tree,
                parents: [&request.expected_head_sha],
            }),
        )
        .await?;
        valid_sha(&commit.sha)?;
        // Git objects are immutable. The only persistent publication is this
        // final non-forced ref write, so a failure cannot strand an empty
        // branch and a moved branch cannot be overwritten.
        let updated: GitReference = if branch_exists {
            github_json_request(
                worker::Method::Patch,
                &format!(
                    "https://api.github.com/repos/{}/{}/git/refs/heads/{}",
                    token.repository.owner,
                    token.repository.name,
                    percent_encode(&request.branch)
                ),
                token.as_str(),
                Some(&RefUpdate {
                    sha: &commit.sha,
                    force: false,
                }),
            )
            .await?
        } else {
            github_json_request(
                worker::Method::Post,
                &format!(
                    "https://api.github.com/repos/{}/{}/git/refs",
                    token.repository.owner, token.repository.name
                ),
                token.as_str(),
                Some(&RefCreate {
                    reference: &format!("refs/heads/{}", request.branch),
                    sha: &commit.sha,
                }),
            )
            .await?
        };
        (updated.object.kind == "commit" && updated.object.sha == commit.sha)
            .then_some(CommitResult {
                branch: request.branch.clone(),
                commit_sha: commit.sha,
                parent_sha: request.expected_head_sha.clone(),
            })
            .ok_or(OperationError::Indeterminate)
    }

    /// Create the content-addressed tree for one request. Repeating these blob
    /// and tree writes is safe: Git object IDs are hashes of their content and
    /// no ref is published here. Reconciliation uses the same function so a
    /// matching message and parent cannot smuggle different file contents.
    async fn materialize_tree(
        &self,
        token: &RepositoryToken,
        request: &PublishCommit,
    ) -> Result<String, OperationError> {
        let base: GitCommit = github_json(
            &format!(
                "https://api.github.com/repos/{}/{}/git/commits/{}",
                token.repository.owner, token.repository.name, request.expected_head_sha
            ),
            token.as_str(),
        )
        .await?;
        valid_sha(&base.tree.sha)?;
        let base_tree: GitTree = github_json(
            &format!(
                "https://api.github.com/repos/{}/{}/git/trees/{}?recursive=1",
                token.repository.owner, token.repository.name, base.tree.sha
            ),
            token.as_str(),
        )
        .await?;
        if base_tree.truncated {
            return Err(OperationError::Unavailable);
        }
        let mut blob_shas = Vec::with_capacity(request.changes.len());
        for change in &request.changes {
            let sha = match change.content_base64.as_deref() {
                Some(content) => {
                    let blob: GitObjectId = github_json_request(
                        worker::Method::Post,
                        &format!(
                            "https://api.github.com/repos/{}/{}/git/blobs",
                            token.repository.owner, token.repository.name
                        ),
                        token.as_str(),
                        Some(&BlobRequest {
                            content,
                            encoding: "base64",
                        }),
                    )
                    .await?;
                    valid_sha(&blob.sha)?;
                    Some(blob.sha)
                }
                None => None,
            };
            blob_shas.push(sha);
        }
        let tree_request = publish_tree::build_request(
            &base.tree.sha,
            &request.changes,
            &base_tree.tree,
            &blob_shas,
        )?;
        let tree: GitObjectId = github_json_request(
            worker::Method::Post,
            &format!(
                "https://api.github.com/repos/{}/{}/git/trees",
                token.repository.owner, token.repository.name
            ),
            token.as_str(),
            Some(&tree_request),
        )
        .await?;
        valid_sha(&tree.sha)?;
        Ok(tree.sha)
    }

    /// Did this exact operation already land? The trailer makes the commit
    /// self-identifying, so a retry after an indeterminate failure reads the
    /// branch instead of guessing.
    async fn reconcile_commit(
        &self,
        token: &RepositoryToken,
        request: &PublishCommit,
    ) -> Result<Option<CommitResult>, OperationError> {
        let Some(reference) = self.read_ref_optional(token, &request.branch).await? else {
            return Ok(None);
        };
        if reference.object.sha == request.expected_head_sha {
            return Ok(None);
        }
        let head: GitCommit = github_json(
            &format!(
                "https://api.github.com/repos/{}/{}/git/commits/{}",
                token.repository.owner, token.repository.name, reference.object.sha
            ),
            token.as_str(),
        )
        .await?;
        // The trailer alone is not proof. It travels with the message through a
        // rebase or a cherry-pick, and `validate` is the only thing stopping a
        // caller writing another operation's trailer into its own commit, so
        // the tip must also still be a direct child of the stated head. That is
        // what makes the reported `parent_sha` true rather than assumed.
        if head.message != request.marked_message()?
            || !matches!(head.parents.as_slice(), [parent] if parent.sha == request.expected_head_sha)
            || valid_sha(&head.tree.sha).is_err()
        {
            // The branch moved for some other reason; this operation did not
            // land and must not claim it did.
            return Ok(None);
        }
        if head.tree.sha != self.materialize_tree(token, request).await? {
            return Ok(None);
        }
        Ok(Some(CommitResult {
            branch: request.branch.clone(),
            commit_sha: reference.object.sha,
            parent_sha: request.expected_head_sha.clone(),
        }))
    }

    /// Enqueue via GraphQL, which is the only API GitHub offers for this.
    ///
    /// `expectedHeadOid` is a first-class input, so the head binding is
    /// enforced by the write itself. `jump` is never sent: the decision doc
    /// forbids queue-jump authority, and omitting the field is a stronger
    /// guarantee than sending `false`.
    async fn enqueue_entry(
        &self,
        token: &RepositoryToken,
        pull_node_id: &str,
        request: &EnqueuePullRequest,
    ) -> Result<EnqueueResult, OperationError> {
        #[derive(Serialize)]
        struct Variables<'a> {
            pull: &'a str,
            head: &'a str,
        }
        #[derive(Deserialize)]
        struct Data {
            #[serde(rename = "enqueuePullRequest")]
            enqueue: Option<Payload>,
        }
        #[derive(Deserialize)]
        struct Payload {
            #[serde(rename = "mergeQueueEntry")]
            entry: Option<QueueEntry>,
        }

        let (data, failure): (Option<Data>, Option<GraphQlFailure>) = github_graphql(
            &token.token,
            "mutation($pull:ID!,$head:GitObjectID!){\
             enqueuePullRequest(input:{pullRequestId:$pull,expectedHeadOid:$head}){\
             mergeQueueEntry{id state pullRequest{number headRefOid}}}}",
            &Variables {
                pull: pull_node_id,
                head: &request.head_sha,
            },
        )
        .await?;
        let entry = enqueue_outcome(
            data.and_then(|data| data.enqueue)
                .and_then(|payload| payload.entry),
            failure,
        )?;
        entry.into_result(request)
    }

    /// Answer "is this pull request queued at the head I stated?" by reading
    /// GitHub, never a local record.
    ///
    /// A queue entry carries no operation marker. The caller therefore uses
    /// this only after the durable claim entered `executing` or
    /// `indeterminate`; an entry already present before the claim is refused
    /// as external rather than misreported as App-created. A lost response can
    /// prove only that the matching entry exists after the attempt, so later
    /// observation says `MERGED_AFTER_ENQUEUE_ATTEMPT`, not that this mutation
    /// was necessarily the proximate cause.
    async fn reconcile_enqueue(
        &self,
        token: &RepositoryToken,
        request: &EnqueuePullRequest,
    ) -> Result<Option<EnqueueResult>, OperationError> {
        self.read_queue_entry(token, &request.base, request.pull_number, &request.head_sha)
            .await?
            .map(|entry| entry.into_result(request))
            .transpose()
    }

    async fn verify_merge_preconditions(
        &self,
        token: &RepositoryToken,
        journal: &DeliveryJournal,
        request: &MergePullRequestAtHead,
    ) -> Result<(), OperationError> {
        let metadata = self.repository_metadata(token).await?;
        if request.base != metadata.default_branch {
            return Err(OperationError::Refused(RefusalReason::MergePreconditions));
        }
        let pull = self
            .verify_pull_request_head(token, request.pull_number, &request.head_sha)
            .await?;
        if pull.state != "open" || pull.draft || pull.base.name != request.base {
            return Err(OperationError::Refused(RefusalReason::MergePreconditions));
        }

        // Keep the ruleset path unchanged. Exact 403 is the one alternate
        // GitHub response accepted for a private plan that cannot expose
        // rules: the App then proves the smaller policy itself.
        let (required, private_base_sha) = match self.branch_rules(token, &request.base).await {
            Ok(rules) => {
                if rules.len() >= 100 {
                    return Err(OperationError::Refused(RefusalReason::MergePreconditions));
                }
                let ruleset_ids = active_ruleset_ids(&rules)
                    .ok_or(OperationError::Refused(RefusalReason::MergePreconditions))?;
                self.verify_rulesets_without_app_bypass(token, &ruleset_ids)
                    .await?;
                let required = branch_rules_allow_merge(&rules)
                    .ok_or(OperationError::Refused(RefusalReason::MergePreconditions))?;
                (Some(required), None)
            }
            Err(error @ Error::Rejected(403)) => {
                let branch = self.branch(token, &request.base).await?;
                if !private_unprotected_merge_allowed(&metadata, &branch)
                    || pull.base.sha != branch.commit.sha
                {
                    return Err(error.into());
                }
                self.verify_no_merge_queue(token, &request.base).await?;
                (None, Some(branch.commit.sha))
            }
            Err(error) => return Err(error.into()),
        };
        let checks = self
            .checks(
                token,
                ObservePullRequestChecks {
                    repository: token.repository.full_name.clone(),
                    pull_number: request.pull_number,
                    head_sha: request.head_sha.clone(),
                },
            )
            .await?;
        if !required.as_ref().map_or_else(
            || checks_are_terminal_and_non_failing(&checks.checks),
            |required| checks_allow_merge(&checks.checks, required),
        ) {
            return Err(OperationError::Refused(RefusalReason::MergeChecks));
        }
        let allowed_review = self
            .verify_review_operation(journal, token, request)
            .await?;
        let reviews = self
            .pull_request_reviews(token, request.pull_number)
            .await?;
        if reviews.len() >= 100 {
            return Err(OperationError::Refused(RefusalReason::MergeReview));
        }
        if !reviews.iter().any(|review| {
            review.matches_allow_result(
                &allowed_review,
                &token.repository.full_name,
                request.pull_number,
                &request.head_sha,
                &request.review_operation_id,
            )
        }) || reviews
            .iter()
            .any(|review| review.blocks_head(&request.head_sha))
        {
            return Err(OperationError::Refused(RefusalReason::MergeReview));
        }

        if let Some(private_base_sha) = private_base_sha {
            self.verify_no_merge_queue(token, &request.base).await?;
            let current_branch = self.branch(token, &request.base).await?;
            if current_branch.protected || current_branch.commit.sha != private_base_sha {
                return Err(OperationError::Refused(RefusalReason::MergePreconditions));
            }
            let current_pull = self
                .verify_pull_request_head(token, request.pull_number, &request.head_sha)
                .await?;
            if current_pull.state != "open"
                || current_pull.draft
                || current_pull.base.name != request.base
                || current_pull.base.sha != private_base_sha
            {
                return Err(OperationError::Refused(RefusalReason::MergePreconditions));
            }
        }
        Ok(())
    }

    async fn verify_no_merge_queue(
        &self,
        token: &RepositoryToken,
        base: &str,
    ) -> Result<(), OperationError> {
        #[derive(Serialize)]
        struct Variables<'a> {
            owner: &'a str,
            name: &'a str,
            base: &'a str,
        }
        let (data, failure): (Option<serde_json::Value>, Option<GraphQlFailure>) = github_graphql(
            &token.token,
            "query($owner:String!,$name:String!,$base:String!){\
             repository(owner:$owner,name:$name){\
             mergeQueue(branch:$base){entries(first:1){totalCount}}}}",
            &Variables {
                owner: &token.repository.owner,
                name: &token.repository.name,
                base,
            },
        )
        .await?;
        no_merge_queue(data, failure)
    }

    async fn verify_review_operation(
        &self,
        journal: &DeliveryJournal,
        token: &RepositoryToken,
        request: &MergePullRequestAtHead,
    ) -> Result<ReviewResult, OperationError> {
        let observation = journal
            .observe_operation(&request.review_operation_id)
            .await
            .map_err(|_| OperationError::Unavailable)?
            .ok_or(OperationError::Refused(RefusalReason::MergeReview))?;
        if observation.kind != "submit_pull_request_review" || observation.state != "completed" {
            return Err(OperationError::Refused(RefusalReason::MergeReview));
        }
        let result: ReviewResult = serde_json::from_str(
            observation
                .result_json
                .as_deref()
                .ok_or(OperationError::Unavailable)?,
        )
        .map_err(|_| OperationError::Unavailable)?;
        if !review_result_allows_merge(
            &result,
            &token.repository.full_name,
            request.pull_number,
            &request.head_sha,
        ) {
            return Err(OperationError::Refused(RefusalReason::MergeReview));
        }
        Ok(result)
    }

    async fn branch_rules(
        &self,
        token: &RepositoryToken,
        base: &str,
    ) -> Result<Vec<BranchRule>, Error> {
        github_json(
            &format!(
                "https://api.github.com/repos/{}/{}/rules/branches/{}?per_page=100",
                token.repository.owner,
                token.repository.name,
                percent_encode(base)
            ),
            token.as_str(),
        )
        .await
    }

    async fn branch(
        &self,
        token: &RepositoryToken,
        base: &str,
    ) -> Result<BranchSnapshot, OperationError> {
        let branch: BranchSnapshot = github_json(
            &format!(
                "https://api.github.com/repos/{}/{}/branches/{}",
                token.repository.owner,
                token.repository.name,
                percent_encode(base)
            ),
            token.as_str(),
        )
        .await?;
        if branch.name != base || valid_sha(&branch.commit.sha).is_err() {
            return Err(OperationError::Refused(RefusalReason::MergePreconditions));
        }
        Ok(branch)
    }

    async fn verify_rulesets_without_app_bypass(
        &self,
        token: &RepositoryToken,
        ruleset_ids: &[i64],
    ) -> Result<(), OperationError> {
        for ruleset_id in ruleset_ids {
            let body: serde_json::Value = github_json(
                &format!(
                    "https://api.github.com/repos/{}/{}/rulesets/{ruleset_id}?includes_parents=true",
                    token.repository.owner, token.repository.name
                ),
                token.as_str(),
            )
            .await
            .map_err(OperationError::from)?;
            let ruleset: RepositoryRuleset = serde_json::from_value(body)
                .map_err(|_| OperationError::Refused(RefusalReason::MergePreconditions))?;
            if !ruleset_allows_merge(&ruleset, *ruleset_id, self.app_id) {
                return Err(OperationError::Refused(RefusalReason::MergePreconditions));
            }
        }
        Ok(())
    }

    async fn pull_request_reviews(
        &self,
        token: &RepositoryToken,
        pull_number: i64,
    ) -> Result<Vec<PullRequestReview>, OperationError> {
        github_json(
            &format!(
                "https://api.github.com/repos/{}/{}/pulls/{pull_number}/reviews?per_page=100",
                token.repository.owner, token.repository.name
            ),
            token.as_str(),
        )
        .await
        .map_err(OperationError::from)
    }

    async fn merge_pull_request(
        &self,
        token: &RepositoryToken,
        request: &MergePullRequestAtHead,
    ) -> Result<MergePullRequestAtHeadResult, OperationError> {
        #[derive(Serialize)]
        struct Body<'a> {
            sha: &'a str,
            merge_method: &'static str,
            commit_message: String,
        }
        let response: PullRequestMergeResponse = match github_json_request(
            worker::Method::Put,
            &format!(
                "https://api.github.com/repos/{}/{}/pulls/{}/merge",
                token.repository.owner, token.repository.name, request.pull_number
            ),
            token.as_str(),
            Some(&Body {
                sha: &request.head_sha,
                merge_method: "squash",
                commit_message: request.trailer()?,
            }),
        )
        .await
        {
            Ok(response) => response,
            Err(Error::Rejected(status)) => match classify_merge_status(status) {
                MergeHttpStatus::HeadConflict => {
                    return Err(OperationError::Refused(RefusalReason::MergeHeadConflict));
                }
                MergeHttpStatus::Refused => {
                    return Err(OperationError::Refused(RefusalReason::MergeRejected(
                        status,
                    )));
                }
                MergeHttpStatus::Reconcile => return Err(OperationError::Unavailable),
            },
            Err(error) => return Err(error.into()),
        };
        let result = merge_response_result(response, request)?;
        self.verify_merge_commit_trailer(token, request, &result.merge_commit_sha)
            .await?;
        Ok(result)
    }

    async fn verify_merge_commit_trailer(
        &self,
        token: &RepositoryToken,
        request: &MergePullRequestAtHead,
        merge_commit_sha: &str,
    ) -> Result<(), OperationError> {
        let merged = self.read_commit(token, merge_commit_sha).await?;
        if merge_commit_has_trailer(request, &merged.message)? {
            Ok(())
        } else {
            Err(OperationError::Indeterminate)
        }
    }

    async fn reconcile_merge(
        &self,
        token: &RepositoryToken,
        request: &MergePullRequestAtHead,
    ) -> Result<Option<MergePullRequestAtHeadResult>, OperationError> {
        let pull: PullRequest = github_json(
            &format!(
                "https://api.github.com/repos/{}/{}/pulls/{}",
                token.repository.owner, token.repository.name, request.pull_number
            ),
            token.as_str(),
        )
        .await?;
        if pull.number != request.pull_number
            || pull.head.sha != request.head_sha
            || pull.base.name != request.base
            || !pull.merged
        {
            return Ok(None);
        }
        let merge_commit_sha = pull.merge_commit_sha.ok_or(OperationError::Indeterminate)?;
        valid_sha(&merge_commit_sha)?;
        self.verify_merge_commit_trailer(token, request, &merge_commit_sha)
            .await?;
        Ok(Some(MergePullRequestAtHeadResult {
            pull_number: request.pull_number,
            head_sha: request.head_sha.clone(),
            base: request.base.clone(),
            merge_commit_sha,
        }))
    }

    async fn read_queue_entry(
        &self,
        token: &RepositoryToken,
        base: &str,
        pull_number: i64,
        head_sha: &str,
    ) -> Result<Option<QueueEntry>, OperationError> {
        #[derive(Serialize)]
        struct Variables<'a> {
            owner: &'a str,
            name: &'a str,
            base: &'a str,
        }
        #[derive(Deserialize)]
        struct Data {
            repository: Option<Repository>,
        }
        #[derive(Deserialize)]
        struct Repository {
            #[serde(rename = "mergeQueue")]
            queue: Option<Queue>,
        }
        #[derive(Deserialize)]
        struct Queue {
            entries: Option<Entries>,
        }
        #[derive(Deserialize)]
        struct Entries {
            #[serde(rename = "totalCount")]
            total_count: i64,
            nodes: Vec<QueueEntry>,
        }

        let (data, failure): (Option<Data>, Option<GraphQlFailure>) = github_graphql(
            &token.token,
            "query($owner:String!,$name:String!,$base:String!){\
             repository(owner:$owner,name:$name){\
             mergeQueue(branch:$base){entries(first:100){totalCount \
             nodes{id state pullRequest{number headRefOid}}}}}}",
            &Variables {
                owner: &token.repository.owner,
                name: &token.repository.name,
                base,
            },
        )
        .await?;
        // A read that carried ANY error did not answer, whether or not `data`
        // came back partially populated. Reporting a refusal here would tell
        // the caller "nothing changed" about an enqueue whose outcome is
        // exactly what could not be read -- and this runs on the path where
        // the mutation's outcome is already unknown.
        if failure.is_some() {
            return Err(OperationError::Indeterminate);
        }
        let Some(data) = data else {
            return Err(OperationError::Indeterminate);
        };
        let Some(entries) = data
            .repository
            .and_then(|repository| repository.queue)
            .and_then(|queue| queue.entries)
        else {
            // The read answered, and its answer carried no queue for this
            // branch. Enqueue treats that as unsupported; observation maps it
            // to NOT_QUEUED because no entry is present.
            return Err(OperationError::Refused(RefusalReason::NoMergeQueue));
        };
        // One unread page could hide this operation's entry and make a
        // reconciliation answer "not queued" for something that is. Refuse to
        // guess instead.
        if !(0..=100).contains(&entries.total_count)
            || entries.nodes.len() as i64 != entries.total_count
        {
            return Err(OperationError::Indeterminate);
        }
        Ok(entries.nodes.into_iter().find(|entry| {
            entry
                .pull_request
                .as_ref()
                .is_some_and(|pull| pull.number == pull_number && pull.head_ref_oid == head_sha)
        }))
    }

    async fn verify_ref(
        &self,
        token: &RepositoryToken,
        name: &str,
        expected_sha: &str,
    ) -> Result<(), OperationError> {
        let reference = self.read_ref(token, name).await?;
        (reference.object.sha == expected_sha)
            .then_some(())
            .ok_or(OperationError::Conflict)
    }

    async fn verify_pull_request_head(
        &self,
        token: &RepositoryToken,
        pull_number: i64,
        expected_sha: &str,
    ) -> Result<PullRequest, OperationError> {
        let pull: PullRequest = github_json(
            &format!(
                "https://api.github.com/repos/{}/{}/pulls/{pull_number}",
                token.repository.owner, token.repository.name
            ),
            token.as_str(),
        )
        .await?;
        if pull.number != pull_number || pull.head.sha != expected_sha {
            return Err(OperationError::Conflict);
        }
        Ok(pull)
    }

    async fn reconcile_pull_request(
        &self,
        token: &RepositoryToken,
        request: &CreatePullRequest,
    ) -> Result<Option<PullRequestResult>, OperationError> {
        let pulls: Vec<PullRequest> = github_json(
            &format!(
                "https://api.github.com/repos/{}/{}/pulls?state=all&head={}%3A{}&base={}&per_page=100",
                token.repository.owner,
                token.repository.name,
                percent_encode(&token.repository.owner),
                percent_encode(&request.head),
                percent_encode(&request.base)
            ),
            token.as_str(),
        )
        .await?;
        let matches = pulls
            .into_iter()
            .filter(|pull| pull.matches_create(request))
            .map(PullRequestResult::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        match matches.as_slice() {
            [] => Ok(None),
            [result] => Ok(Some(PullRequestResult {
                number: result.number,
                url: result.url.clone(),
                head_sha: result.head_sha.clone(),
                base_sha: result.base_sha.clone(),
            })),
            _ => Err(OperationError::Indeterminate),
        }
    }

    async fn post_pull_request(
        &self,
        token: &RepositoryToken,
        request: &CreatePullRequest,
    ) -> Result<PullRequestResult, OperationError> {
        #[derive(Serialize)]
        struct Body<'a> {
            title: &'a str,
            head: &'a str,
            base: &'a str,
            body: String,
            draft: bool,
        }
        let pull: PullRequest = github_json_request(
            worker::Method::Post,
            &format!(
                "https://api.github.com/repos/{}/{}/pulls",
                token.repository.owner, token.repository.name
            ),
            token.as_str(),
            Some(&Body {
                title: &request.title,
                head: &request.head,
                base: &request.base,
                body: request.marked_body()?,
                draft: request.draft,
            }),
        )
        .await?;
        if !pull.matches_create(request) {
            return Err(OperationError::Indeterminate);
        }
        let result = PullRequestResult::try_from(pull)?;
        if result.head_sha != request.head_sha || result.base_sha != request.base_sha {
            return Err(OperationError::Indeterminate);
        }
        Ok(result)
    }

    async fn reconcile_pull_request_body(
        &self,
        token: &RepositoryToken,
        request: &UpdatePullRequestBody,
    ) -> Result<Option<PullRequestResult>, OperationError> {
        self.verify_pull_request_head(token, request.pull_number, &request.head_sha)
            .await?
            .body_result(request)
    }

    async fn update_pull_request_body(
        &self,
        token: &RepositoryToken,
        request: &UpdatePullRequestBody,
    ) -> Result<PullRequestResult, OperationError> {
        #[derive(Serialize)]
        struct Body {
            body: String,
        }
        let pull: PullRequest = github_json_request(
            worker::Method::Patch,
            &format!(
                "https://api.github.com/repos/{}/{}/pulls/{}",
                token.repository.owner, token.repository.name, request.pull_number
            ),
            token.as_str(),
            Some(&Body {
                body: request.marked_body()?,
            }),
        )
        .await?;
        pull.body_result(request)?
            .ok_or(OperationError::Indeterminate)
    }

    async fn reconcile_closed_pull_request(
        &self,
        token: &RepositoryToken,
        request: &ClosePullRequest,
    ) -> Result<Option<ClosePullRequestResult>, OperationError> {
        self.verify_pull_request_head(token, request.pull_number, &request.head_sha)
            .await?
            .close_result(request)
    }

    async fn close_pull_request(
        &self,
        token: &RepositoryToken,
        request: &ClosePullRequest,
    ) -> Result<ClosePullRequestResult, OperationError> {
        #[derive(Serialize)]
        struct Body {
            state: &'static str,
        }
        let pull: PullRequest = github_json_request(
            worker::Method::Patch,
            &format!(
                "https://api.github.com/repos/{}/{}/pulls/{}",
                token.repository.owner, token.repository.name, request.pull_number
            ),
            token.as_str(),
            Some(&Body { state: "closed" }),
        )
        .await?;
        pull.close_result(request)?
            .ok_or(OperationError::Indeterminate)
    }

    async fn reconcile_review(
        &self,
        token: &RepositoryToken,
        request: &SubmitPullRequestReview,
    ) -> Result<Option<ReviewResult>, OperationError> {
        let reviews: Vec<PullRequestReview> = github_json(
            &format!(
                "https://api.github.com/repos/{}/{}/pulls/{}/reviews?per_page=100",
                token.repository.owner, token.repository.name, request.pull_number
            ),
            token.as_str(),
        )
        .await?;
        let mut matches = reviews
            .into_iter()
            .filter(|review| review.matches(request))
            .map(|review| review.into_result(request))
            .collect::<Result<Vec<_>, _>>()?;
        match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.pop()),
            // Two reviews carrying one operation's marker at one head is not
            // something to pick a winner from.
            _ => Err(OperationError::Indeterminate),
        }
    }

    async fn post_review(
        &self,
        token: &RepositoryToken,
        request: &SubmitPullRequestReview,
    ) -> Result<ReviewResult, OperationError> {
        #[derive(Serialize)]
        struct Body<'a> {
            commit_id: &'a str,
            body: String,
            event: &'a str,
        }
        let review: PullRequestReview = github_json_request(
            worker::Method::Post,
            &format!(
                "https://api.github.com/repos/{}/{}/pulls/{}/reviews",
                token.repository.owner, token.repository.name, request.pull_number
            ),
            token.as_str(),
            Some(&Body {
                commit_id: &request.head_sha,
                body: request.marked_body()?,
                event: REVIEW_EVENT,
            }),
        )
        .await?;
        let result = review.into_result(request)?;
        if result.head_sha != request.head_sha {
            return Err(OperationError::Indeterminate);
        }
        Ok(result)
    }

    async fn checks(
        &self,
        token: &RepositoryToken,
        request: ObservePullRequestChecks,
    ) -> Result<ChecksResult, OperationError> {
        let response: CheckRuns = github_json(
            &format!(
                "https://api.github.com/repos/{}/{}/commits/{}/check-runs?per_page=100",
                token.repository.owner, token.repository.name, request.head_sha
            ),
            token.as_str(),
        )
        .await?;
        if !(0..=100).contains(&response.total_count)
            || response.total_count as usize != response.check_runs.len()
        {
            return Err(OperationError::Unavailable);
        }
        let mut checks = response
            .check_runs
            .into_iter()
            .map(CheckResult::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        checks.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(ChecksResult {
            pull_number: request.pull_number,
            head_sha: request.head_sha,
            checks,
        })
    }

    async fn verify_workflow_pr(
        &self,
        token: &RepositoryToken,
        pull_number: i64,
        head_sha: &str,
        base: &str,
    ) -> Result<(), OperationError> {
        let repository = self.repository_metadata(token).await?;
        if base != repository.default_branch {
            return Err(OperationError::Conflict);
        }
        let pull = self
            .verify_pull_request_head(token, pull_number, head_sha)
            .await?;
        (pull.base.name == base)
            .then_some(())
            .ok_or(OperationError::Conflict)
    }

    async fn workflow_runs(
        &self,
        token: &RepositoryToken,
        request: &ObservePullRequestWorkflows,
    ) -> Result<PullRequestWorkflowsResult, OperationError> {
        let workflow = WorkflowRef::requested(&request.workflow_path)?;
        let response: WorkflowRuns = github_json(
            &format!(
                "{}?event=pull_request&head_sha={}&per_page={MAX_WORKFLOW_RUNS}",
                workflow_api_url(
                    &token.repository.owner,
                    &token.repository.name,
                    workflow,
                    "runs",
                ),
                request.head_sha
            ),
            token.as_str(),
        )
        .await?;
        if !(0..=MAX_WORKFLOW_RUNS as i64).contains(&response.total_count)
            || response.total_count as usize != response.workflow_runs.len()
        {
            return Err(OperationError::Indeterminate);
        }
        let mut runs = Vec::with_capacity(response.workflow_runs.len());
        for run in response.workflow_runs {
            run.verify(workflow, request.pull_number, &request.head_sha)?;
            let jobs = self.workflow_jobs(token, run.id).await?;
            runs.push(run.into_result(jobs)?);
        }
        runs.sort_by_key(|run| run.run_id);
        Ok(PullRequestWorkflowsResult {
            pull_number: request.pull_number,
            head_sha: request.head_sha.clone(),
            base: request.base.clone(),
            runs,
        })
    }

    async fn read_workflow_run(
        &self,
        token: &RepositoryToken,
        run_id: i64,
    ) -> Result<WorkflowRun, OperationError> {
        let run: WorkflowRun = github_json(
            &format!(
                "https://api.github.com/repos/{}/{}/actions/runs/{run_id}",
                token.repository.owner, token.repository.name
            ),
            token.as_str(),
        )
        .await?;
        (run.id == run_id)
            .then_some(run)
            .ok_or(OperationError::Conflict)
    }

    async fn workflow_jobs(
        &self,
        token: &RepositoryToken,
        run_id: i64,
    ) -> Result<Vec<WorkflowJob>, OperationError> {
        let response: WorkflowJobs = github_json(
            &format!(
                "https://api.github.com/repos/{}/{}/actions/runs/{run_id}/jobs?filter=latest&per_page={MAX_WORKFLOW_JOBS}",
                token.repository.owner, token.repository.name
            ),
            token.as_str(),
        )
        .await?;
        if !(0..=MAX_WORKFLOW_JOBS as i64).contains(&response.total_count)
            || response.total_count as usize != response.jobs.len()
            || response.jobs.iter().any(|job| job.run_id != run_id)
        {
            return Err(OperationError::Indeterminate);
        }
        Ok(response.jobs)
    }

    async fn job_log_redirect(
        &self,
        token: &RepositoryToken,
        job_id: i64,
    ) -> Result<String, OperationError> {
        github_redirect_location(
            &format!(
                "https://api.github.com/repos/{}/{}/actions/jobs/{job_id}/logs",
                token.repository.owner, token.repository.name
            ),
            token.as_str(),
        )
        .await
    }

    async fn rerun_failed_jobs(
        &self,
        token: &RepositoryToken,
        run_id: i64,
    ) -> Result<(), OperationError> {
        github_empty_request(
            worker::Method::Post,
            &format!(
                "https://api.github.com/repos/{}/{}/actions/runs/{run_id}/rerun-failed-jobs",
                token.repository.owner, token.repository.name
            ),
            token.as_str(),
            201,
        )
        .await
        .map_err(Into::into)
    }

    async fn read_tag_commit_optional(
        &self,
        token: &RepositoryToken,
        tag: &str,
    ) -> Result<Option<String>, OperationError> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/git/ref/tags/{}",
            token.repository.owner,
            token.repository.name,
            percent_encode(tag)
        );
        let reference: GitReference = match github_json(&url, token.as_str()).await {
            Ok(reference) => reference,
            Err(Error::Rejected(404)) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if reference.name != format!("refs/tags/{tag}") || valid_sha(&reference.object.sha).is_err()
        {
            return Err(OperationError::Unavailable);
        }
        let mut kind = reference.object.kind;
        let mut sha = reference.object.sha;
        for _ in 0..4 {
            match kind.as_str() {
                "commit" => {
                    valid_sha(&sha)?;
                    return Ok(Some(sha));
                }
                "tag" => {
                    let tagged: TagObject = github_json(
                        &format!(
                            "https://api.github.com/repos/{}/{}/git/tags/{sha}",
                            token.repository.owner, token.repository.name
                        ),
                        token.as_str(),
                    )
                    .await?;
                    kind = tagged.object.kind;
                    sha = tagged.object.sha;
                    valid_sha(&sha)?;
                }
                _ => return Err(OperationError::Unavailable),
            }
        }
        Err(OperationError::Unavailable)
    }

    async fn create_release_tag(
        &self,
        token: &RepositoryToken,
        request: &PublishReleaseTag,
    ) -> Result<(), OperationError> {
        let reference: GitReference = github_json_request(
            worker::Method::Post,
            &format!(
                "https://api.github.com/repos/{}/{}/git/refs",
                token.repository.owner, token.repository.name
            ),
            token.as_str(),
            Some(&RefCreate {
                reference: &format!("refs/tags/{}", request.tag),
                sha: &request.commit_sha,
            }),
        )
        .await?;
        if reference.name != format!("refs/tags/{}", request.tag)
            || reference.object.kind != "commit"
            || reference.object.sha != request.commit_sha
        {
            return Err(OperationError::Indeterminate);
        }
        Ok(())
    }

    async fn dispatch_workflow(
        &self,
        token: &RepositoryToken,
        workflow: WorkflowRef<'_>,
        branch: &str,
        inputs: serde_json::Value,
    ) -> Result<WorkflowDispatchResponse, OperationError> {
        #[derive(Serialize)]
        struct Body {
            r#ref: String,
            inputs: serde_json::Value,
        }
        let response: WorkflowDispatchResponse = github_json_request(
            worker::Method::Post,
            &workflow_api_url(
                &token.repository.owner,
                &token.repository.name,
                workflow,
                "dispatches",
            ),
            token.as_str(),
            Some(&Body {
                r#ref: branch.to_owned(),
                inputs,
            }),
        )
        .await
        .map_err(OperationError::from)?;
        response.validate()
    }

    async fn workflow_run_by_title(
        &self,
        token: &RepositoryToken,
        workflow: WorkflowRef<'_>,
        title: &str,
        head_sha: &str,
    ) -> Result<Option<WorkflowRun>, OperationError> {
        let mut runs = self
            .read_workflow_runs(token, workflow, head_sha)
            .await?
            .into_iter()
            .filter(|run| run.display_title.as_deref() == Some(title))
            .collect::<Vec<_>>();
        match runs.len() {
            0 => Ok(None),
            1 => {
                let run = runs.pop().ok_or(OperationError::Indeterminate)?;
                run.verify_dispatch(workflow, head_sha, title)?;
                Ok(Some(run))
            }
            _ => Err(OperationError::Indeterminate),
        }
    }

    async fn read_dispatched_workflow(
        &self,
        token: &RepositoryToken,
        run_id: i64,
        workflow: WorkflowRef<'_>,
        head_sha: &str,
        title: &str,
    ) -> Result<WorkflowRun, OperationError> {
        for attempt in 0..4 {
            if let Ok(run) = self.workflow_run(token, run_id).await {
                return run
                    .verify_dispatch(workflow, head_sha, title)
                    .map(|()| run)
                    // The dispatch happened, but not at the reviewed commit.
                    // It is an external effect and can never be called a
                    // request conflict or a no-effect refusal.
                    .map_err(|_| OperationError::Indeterminate);
            }
            if attempt < 3 {
                worker::Delay::from(std::time::Duration::from_millis(250)).await;
            }
        }
        Err(OperationError::Indeterminate)
    }

    async fn workflow_run(
        &self,
        token: &RepositoryToken,
        run_id: i64,
    ) -> Result<WorkflowRun, OperationError> {
        let run: WorkflowRun = github_json(
            &format!(
                "https://api.github.com/repos/{}/{}/actions/runs/{run_id}",
                token.repository.owner, token.repository.name,
            ),
            token.as_str(),
        )
        .await
        .map_err(OperationError::from)?;
        (run.id == run_id)
            .then_some(run)
            .ok_or(OperationError::Conflict)
    }

    async fn release_workflow_run(
        &self,
        token: &RepositoryToken,
        tag: &str,
        commit_sha: &str,
    ) -> Result<WorkflowRunResult, OperationError> {
        let runs = self
            .read_workflow_runs(token, RELEASE_WORKFLOW, commit_sha)
            .await?;
        select_release_workflow_run(runs, tag, commit_sha)?.into_result(Vec::new())
    }

    async fn read_workflow_runs(
        &self,
        token: &RepositoryToken,
        workflow: WorkflowRef<'_>,
        head_sha: &str,
    ) -> Result<Vec<WorkflowRun>, OperationError> {
        let response: WorkflowRuns = github_json(
            &format!(
                "{}?head_sha={head_sha}&per_page={MAX_WORKFLOW_RUNS}",
                workflow_api_url(
                    &token.repository.owner,
                    &token.repository.name,
                    workflow,
                    "runs",
                )
            ),
            token.as_str(),
        )
        .await?;
        if !(0..=MAX_WORKFLOW_RUNS as i64).contains(&response.total_count)
            || response.workflow_runs.len() as i64 != response.total_count
        {
            return Err(OperationError::Indeterminate);
        }
        let mut runs = Vec::new();
        for run in response.workflow_runs {
            run.verify_identity(workflow, head_sha)?;
            runs.push(run);
        }
        Ok(runs)
    }

    async fn release(
        &self,
        token: &RepositoryToken,
        tag: &str,
    ) -> Result<Option<ReleaseResult>, OperationError> {
        let release: Release = match github_json(
            &format!(
                "https://api.github.com/repos/{}/{}/releases/tags/{tag}",
                token.repository.owner, token.repository.name
            ),
            token.as_str(),
        )
        .await
        {
            Ok(release) => release,
            Err(Error::Rejected(404)) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        release.into_result(tag)
    }
}

struct Credential(String);

impl Credential {
    fn new(token: String) -> Result<Self, Error> {
        if token.len() < 20 || token.len() > 4_096 || !token.is_ascii() {
            return Err(Error::Unavailable);
        }
        Ok(Self(token))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

impl Drop for Credential {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[derive(Deserialize)]
struct Installation {
    id: i64,
    app_id: i64,
    account: NumericIdentity,
    repository_selection: String,
    permissions: BTreeMap<String, String>,
    events: Vec<String>,
    suspended_at: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct NumericIdentity {
    id: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct RepositoryIdentity {
    id: i64,
    full_name: String,
    owner: NumericIdentity,
}

#[cfg(target_arch = "wasm32")]
#[derive(Deserialize)]
struct InstallationToken {
    token: String,
    permissions: BTreeMap<String, String>,
    repositories: Vec<RepositoryIdentity>,
}

/// The repository observation every operation binds itself to.
///
/// Every field here must be one GitHub returns to *this* App's installation
/// token. `delete_branch_on_merge` is intentionally not required: GitHub
/// returns it only with Administration access, while repository metadata does
/// not need that field. A required field GitHub omits makes the whole 200 fail
/// to deserialize, which reached the caller as an opaque "authority is
/// unavailable" and disabled all eleven operations that observe the
/// repository -- `maintainer_status` and the entire publication, merge, and
/// CI-diagnosis path included. Three of the eleven reach this function
/// indirectly through `verify_workflow_pr`, which is why the first count of
/// the blast radius was too low.
///
/// Host-testable, unlike the `wasm32`-only transport around it, so the shape
/// contract can be proven against a real GitHub body instead of asserted.
#[cfg(any(target_arch = "wasm32", test))]
#[derive(Deserialize)]
struct RepositoryMetadata {
    id: i64,
    full_name: String,
    default_branch: String,
    #[serde(default)]
    private: Option<bool>,
    #[serde(default)]
    allow_squash_merge: Option<bool>,
}

#[cfg(any(target_arch = "wasm32", test))]
impl RepositoryMetadata {
    fn validate(self, token: &RepositoryToken) -> Result<Self, OperationError> {
        valid_exact_integer(self.id)?;
        if self.id != token.repository_id || self.full_name != token.repository.full_name {
            return Err(OperationError::Conflict);
        }
        valid_ref(&self.default_branch)?;
        Ok(self)
    }
}

#[cfg(target_arch = "wasm32")]
#[derive(Deserialize)]
struct Issue {
    number: i64,
    html_url: String,
    title: String,
    body: Option<String>,
    #[serde(default)]
    state: String,
    #[serde(default)]
    state_reason: Option<String>,
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}

#[cfg(target_arch = "wasm32")]
impl Issue {
    fn matches_create(&self, request: &CreateIssue) -> bool {
        self.is_real_issue()
            && self.title == request.title
            && request
                .marked_body()
                .is_ok_and(|body| self.body.as_deref() == Some(body.as_str()))
    }

    fn is_real_issue(&self) -> bool {
        self.pull_request.is_none()
    }

    fn is_real_open_issue(&self) -> bool {
        self.is_real_issue() && self.state == "open"
    }

    fn into_result(self) -> Result<IssueResult, OperationError> {
        valid_exact_integer(self.number)?;
        valid_github_url(&self.html_url)?;
        Ok(IssueResult {
            number: self.number,
            url: self.html_url,
        })
    }

    fn into_observation(self) -> Result<IssueObservationResult, OperationError> {
        valid_exact_integer(self.number)?;
        valid_github_url(&self.html_url)?;
        if !self.is_real_issue() || !matches!(self.state.as_str(), "open" | "closed") {
            return Err(OperationError::Conflict);
        }
        Ok(IssueObservationResult {
            number: self.number,
            url: self.html_url,
            state: self.state,
            state_reason: self.state_reason,
        })
    }
}

#[cfg(target_arch = "wasm32")]
#[derive(Deserialize)]
struct IssueComment {
    id: i64,
    html_url: String,
    body: Option<String>,
}

#[cfg(target_arch = "wasm32")]
impl IssueComment {
    fn validate(self) -> Result<Self, OperationError> {
        valid_exact_integer(self.id)?;
        valid_github_url(&self.html_url)?;
        Ok(self)
    }

    fn matches(&self, request: &ResolveIssue) -> bool {
        request
            .marked_body()
            .is_ok_and(|body| self.body.as_deref() == Some(body.as_str()))
    }
}

#[cfg(target_arch = "wasm32")]
struct IssueResolutionProgress {
    issue: Issue,
    comment: Option<IssueComment>,
}

#[cfg(target_arch = "wasm32")]
impl IssueResolutionProgress {
    fn completed(&self, request: &ResolveIssue) -> Option<ResolveIssueResult> {
        let comment = self.comment.as_ref()?;
        let state_reason = self.issue.state_reason.as_deref()?;
        if self.issue.state != "closed" || state_reason != request.state_reason.as_str() {
            return None;
        }
        Some(ResolveIssueResult {
            number: self.issue.number,
            url: self.issue.html_url.clone(),
            comment_url: comment.html_url.clone(),
            state: self.issue.state.clone(),
            state_reason: state_reason.to_owned(),
        })
    }
}

#[cfg(target_arch = "wasm32")]
#[derive(Serialize)]
struct BlobRequest<'a> {
    content: &'a str,
    encoding: &'static str,
}

#[cfg(any(target_arch = "wasm32", test))]
mod publish_tree {
    use super::{FileChange, GitTreeEntry, OperationError};
    use serde::Serialize;

    #[derive(Serialize)]
    struct Entry {
        path: String,
        mode: &'static str,
        #[serde(rename = "type")]
        kind: &'static str,
        sha: Option<String>,
    }

    #[derive(Serialize)]
    pub(super) struct Request<'a> {
        base_tree: &'a str,
        tree: Vec<Entry>,
    }

    pub(super) fn build_request<'a>(
        base_tree_sha: &'a str,
        changes: &[FileChange],
        base_tree: &[GitTreeEntry],
        blob_shas: &[Option<String>],
    ) -> Result<Request<'a>, OperationError> {
        if changes.len() != blob_shas.len() {
            return Err(OperationError::InvalidInput);
        }
        let tree = changes
            .iter()
            .zip(blob_shas)
            .map(|(change, sha)| {
                let mut ancestor = String::new();
                let mut components = change.path.split('/').peekable();
                while let Some(component) = components.next() {
                    if components.peek().is_none() {
                        break;
                    }
                    if !ancestor.is_empty() {
                        ancestor.push('/');
                    }
                    ancestor.push_str(component);
                    if base_tree
                        .iter()
                        .find(|entry| entry.path == ancestor)
                        .is_some_and(|entry| entry.kind != "tree")
                    {
                        return Err(OperationError::InvalidInput);
                    }
                }
                let existing = match base_tree.iter().find(|entry| entry.path == change.path) {
                    None => None,
                    Some(entry) if entry.kind == "blob" => match entry.mode.as_str() {
                        "100644" => Some("100644"),
                        "100755" => Some("100755"),
                        _ => return Err(OperationError::InvalidInput),
                    },
                    // A symlink or submodule is not a file this surface writes.
                    Some(_) => return Err(OperationError::InvalidInput),
                };
                let mode = match change.mode.as_deref() {
                    Some("100644") => "100644",
                    Some("100755") => "100755",
                    Some(_) => return Err(OperationError::InvalidInput),
                    None => existing.unwrap_or("100644"),
                };
                Ok(Entry {
                    path: change.path.clone(),
                    mode,
                    kind: "blob",
                    sha: sha.clone(),
                })
            })
            .collect::<Result<Vec<_>, OperationError>>()?;
        Ok(Request {
            base_tree: base_tree_sha,
            tree,
        })
    }
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Deserialize)]
struct GitTree {
    tree: Vec<GitTreeEntry>,
    truncated: bool,
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Deserialize)]
struct GitTreeEntry {
    path: String,
    mode: String,
    #[serde(rename = "type")]
    kind: String,
    /// Every entry carries one. It is what makes "the same path, different
    /// content" distinguishable from "the same path, unchanged" without
    /// reading either blob.
    sha: String,
}

#[cfg(target_arch = "wasm32")]
#[derive(Serialize)]
struct CommitRequest<'a> {
    message: String,
    tree: &'a str,
    parents: [&'a str; 1],
}

#[cfg(target_arch = "wasm32")]
#[derive(Serialize)]
struct RefUpdate<'a> {
    sha: &'a str,
    force: bool,
}

#[cfg(target_arch = "wasm32")]
#[derive(Serialize)]
struct RefCreate<'a> {
    #[serde(rename = "ref")]
    reference: &'a str,
    sha: &'a str,
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Deserialize)]
struct GitObjectId {
    sha: String,
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Deserialize)]
struct GitCommit {
    message: String,
    tree: GitObjectId,
    parents: Vec<GitParent>,
}

#[cfg(target_arch = "wasm32")]
#[derive(Deserialize)]
struct TagObject {
    object: GitObject,
}

/// A parent entry carries no `type`, so it cannot reuse `GitObject`.
#[cfg(any(target_arch = "wasm32", test))]
#[derive(Deserialize)]
struct GitParent {
    sha: String,
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Deserialize)]
struct QueueEntry {
    id: String,
    state: String,
    #[serde(rename = "pullRequest")]
    pull_request: Option<QueueEntryPullRequest>,
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Deserialize)]
struct QueueEntryPullRequest {
    number: i64,
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
}

#[cfg(any(target_arch = "wasm32", test))]
impl QueueEntry {
    /// The entry's own `headCommit` is the queue's *synthetic* merge commit,
    /// not the pull request head -- verified live against a real queue entry,
    /// where they differed. Matching on it would never find the head the
    /// caller stated. `pullRequest.headRefOid` is the live head, and GitHub
    /// ejects an entry when the head moves, so a surviving entry names the
    /// head it was queued at.
    fn matches(&self, request: &EnqueuePullRequest) -> bool {
        self.pull_request.as_ref().is_some_and(|pull| {
            pull.number == request.pull_number && pull.head_ref_oid == request.head_sha
        })
    }

    fn into_result(self, request: &EnqueuePullRequest) -> Result<EnqueueResult, OperationError> {
        if !self.matches(request) {
            // GitHub answered about a different pull request or head than the
            // one asked about. Nothing safe can be concluded from that.
            return Err(OperationError::Indeterminate);
        }
        valid_text(&self.id, 1, 256, true)?;
        // GitHub's documented `MergeQueueEntryState`. An unknown value means
        // the schema moved under us, and guessing would report a merge state
        // this build does not understand.
        if !valid_queue_state(&self.state) {
            return Err(OperationError::Indeterminate);
        }
        Ok(EnqueueResult {
            pull_number: request.pull_number,
            head_sha: request.head_sha.clone(),
            entry_id: self.id,
            state_when_recorded: self.state,
        })
    }
}

#[cfg(any(target_arch = "wasm32", test))]
fn valid_queue_state(state: &str) -> bool {
    matches!(
        state,
        "QUEUED" | "AWAITING_CHECKS" | "MERGEABLE" | "UNMERGEABLE" | "LOCKED"
    )
}

#[cfg(target_arch = "wasm32")]
#[derive(Deserialize)]
struct GitReference {
    #[serde(rename = "ref")]
    name: String,
    object: GitObject,
}

#[cfg(target_arch = "wasm32")]
#[derive(Deserialize)]
struct GitObject {
    sha: String,
    #[serde(rename = "type")]
    kind: String,
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Deserialize)]
struct PullRequest {
    number: i64,
    node_id: String,
    html_url: String,
    title: String,
    body: Option<String>,
    draft: bool,
    head: PullReference,
    base: PullReference,
    state: String,
    #[serde(default)]
    merged: bool,
    #[serde(default)]
    merge_commit_sha: Option<String>,
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Deserialize)]
struct PullReference {
    #[serde(rename = "ref")]
    name: String,
    sha: String,
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Clone, Debug, Deserialize)]
struct BranchSnapshot {
    name: String,
    protected: bool,
    commit: BranchCommit,
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Clone, Debug, Deserialize)]
struct BranchCommit {
    sha: String,
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Clone, Debug, Deserialize)]
struct BranchRule {
    r#type: String,
    #[serde(default)]
    parameters: serde_json::Value,
    #[serde(default)]
    ruleset_id: Option<i64>,
    #[serde(default)]
    ruleset_source: Option<String>,
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RulesetBypassActor {
    actor_id: Option<i64>,
    actor_type: String,
    bypass_mode: String,
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Clone, Debug, Deserialize)]
struct RepositoryRuleset {
    id: i64,
    target: String,
    enforcement: String,
    #[serde(default)]
    bypass_actors: Option<Vec<RulesetBypassActor>>,
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Clone, Debug, Eq, PartialEq)]
struct RequiredCheckIdentity {
    context: String,
    integration_id: Option<i64>,
}

#[cfg(any(target_arch = "wasm32", test))]
fn active_ruleset_ids(rules: &[BranchRule]) -> Option<Vec<i64>> {
    let mut ids = Vec::new();
    for rule in rules {
        let id = rule.ruleset_id?;
        valid_exact_integer(id).ok()?;
        if let Some(source) = rule.ruleset_source.as_deref() {
            valid_text(source, 1, 256, false).ok()?;
        }
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Some(ids)
}

#[cfg(any(target_arch = "wasm32", test))]
fn ruleset_allows_merge(ruleset: &RepositoryRuleset, expected_id: i64, app_id: i64) -> bool {
    if ruleset.id != expected_id || ruleset.target != "branch" || ruleset.enforcement != "active" {
        return false;
    }
    let Some(actors) = ruleset.bypass_actors.as_ref() else {
        return false;
    };
    actors.iter().all(|actor| {
        let known_type = matches!(
            actor.actor_type.as_str(),
            "Integration" | "OrganizationAdmin" | "RepositoryRole" | "Team" | "User" | "DeployKey"
        );
        let valid_id = match actor.actor_type.as_str() {
            "Integration" | "RepositoryRole" | "Team" | "User" => actor
                .actor_id
                .is_some_and(|id| valid_exact_integer(id).is_ok()),
            "OrganizationAdmin" => actor
                .actor_id
                .is_none_or(|id| valid_exact_integer(id).is_ok()),
            "DeployKey" => actor.actor_id.is_none(),
            _ => false,
        };
        known_type
            && valid_id
            && matches!(
                actor.bypass_mode.as_str(),
                "always" | "pull_request" | "exempt"
            )
            && !(actor.actor_type == "DeployKey" && actor.bypass_mode != "always")
            && !(actor.actor_type == "Integration" && actor.actor_id == Some(app_id))
    })
}

#[cfg(any(target_arch = "wasm32", test))]
fn branch_rules_allow_merge(rules: &[BranchRule]) -> Option<Vec<RequiredCheckIdentity>> {
    if rules.iter().any(|rule| rule.r#type == "merge_queue") {
        return None;
    }
    let pull_requests = rules
        .iter()
        .filter(|rule| rule.r#type == "pull_request")
        .collect::<Vec<_>>();
    if pull_requests.is_empty()
        || pull_requests.iter().any(|rule| {
            rule.parameters
                .get("allowed_merge_methods")
                .and_then(serde_json::Value::as_array)
                .is_none_or(|methods| !methods.iter().any(|method| method == "squash"))
        })
    {
        return None;
    }
    let status_rules = rules
        .iter()
        .filter(|rule| rule.r#type == "required_status_checks")
        .collect::<Vec<_>>();
    if status_rules.is_empty() {
        return None;
    }
    let mut names = Vec::new();
    for rule in status_rules {
        if rule
            .parameters
            .get("strict_required_status_checks_policy")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        {
            return None;
        }
        let checks = rule
            .parameters
            .get("required_status_checks")
            .and_then(serde_json::Value::as_array)?;
        for check in checks {
            let context = check.get("context").and_then(serde_json::Value::as_str)?;
            if valid_text(context, 1, 256, false).is_err() {
                return None;
            }
            let integration_id = match check.get("integration_id") {
                None | Some(serde_json::Value::Null) => None,
                Some(value) => {
                    let id = value.as_i64()?;
                    valid_exact_integer(id).ok()?;
                    Some(id)
                }
            };
            let required = RequiredCheckIdentity {
                context: context.to_owned(),
                integration_id,
            };
            if !names.contains(&required) {
                names.push(required);
            }
        }
    }
    (!names.is_empty()).then_some(names)
}

#[cfg(any(target_arch = "wasm32", test))]
fn private_unprotected_merge_allowed(
    repository: &RepositoryMetadata,
    branch: &BranchSnapshot,
) -> bool {
    repository.private == Some(true)
        && repository.allow_squash_merge == Some(true)
        && !branch.protected
}

#[cfg(any(target_arch = "wasm32", test))]
fn no_merge_queue(
    data: Option<serde_json::Value>,
    failure: Option<GraphQlFailure>,
) -> Result<(), OperationError> {
    if failure.is_some() {
        return Err(OperationError::Indeterminate);
    }
    match data
        .as_ref()
        .and_then(|data| data.get("repository"))
        .filter(|repository| !repository.is_null())
        .and_then(|repository| repository.get("mergeQueue"))
    {
        Some(serde_json::Value::Null) => Ok(()),
        Some(_) => Err(OperationError::Refused(RefusalReason::MergePreconditions)),
        None => Err(OperationError::Indeterminate),
    }
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Clone, Debug, Deserialize)]
struct PullRequestMergeResponse {
    sha: Option<String>,
    merged: bool,
}

#[cfg(target_arch = "wasm32")]
impl PullRequest {
    fn matches_create(&self, request: &CreatePullRequest) -> bool {
        // `base.sha` follows the branch after PR creation. The immutable
        // requested base SHA is inside the body marker's request digest;
        // requiring the live field here would make a lost-response retry
        // unreconcilable as soon as protected main advanced.
        self.title == request.title
            && request
                .marked_body()
                .is_ok_and(|body| self.body.as_deref() == Some(body.as_str()))
            && self.draft == request.draft
            && self.head.name == request.head
            && self.head.sha == request.head_sha
            && self.base.name == request.base
    }
}

#[cfg(any(target_arch = "wasm32", test))]
impl PullRequest {
    fn body_result(
        &self,
        request: &UpdatePullRequestBody,
    ) -> Result<Option<PullRequestResult>, OperationError> {
        valid_exact_integer(self.number)?;
        valid_github_url(&self.html_url)?;
        valid_sha(&self.head.sha)?;
        valid_sha(&self.base.sha)?;
        if self.number != request.pull_number || self.head.sha != request.head_sha {
            return Err(OperationError::Conflict);
        }
        if self.state != "open" || self.merged {
            return Err(OperationError::Conflict);
        }
        if self.body.as_deref() != Some(request.marked_body()?.as_str()) {
            return Ok(None);
        }
        Ok(Some(PullRequestResult {
            number: self.number,
            url: self.html_url.clone(),
            head_sha: self.head.sha.clone(),
            base_sha: self.base.sha.clone(),
        }))
    }

    fn close_result(
        &self,
        request: &ClosePullRequest,
    ) -> Result<Option<ClosePullRequestResult>, OperationError> {
        valid_exact_integer(self.number)?;
        valid_github_url(&self.html_url)?;
        valid_sha(&self.head.sha)?;
        if self.number != request.pull_number || self.head.sha != request.head_sha {
            return Err(OperationError::Conflict);
        }
        if self.merged {
            return Err(OperationError::Conflict);
        }
        match self.state.as_str() {
            "open" => Ok(None),
            "closed" => Ok(Some(ClosePullRequestResult {
                pull_number: self.number,
                head_sha: self.head.sha.clone(),
                url: self.html_url.clone(),
                state: self.state.clone(),
            })),
            _ => Err(OperationError::Indeterminate),
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl TryFrom<PullRequest> for PullRequestResult {
    type Error = OperationError;

    fn try_from(pull: PullRequest) -> Result<Self, Self::Error> {
        valid_exact_integer(pull.number)?;
        valid_github_url(&pull.html_url)?;
        valid_sha(&pull.head.sha)?;
        valid_sha(&pull.base.sha)?;
        Ok(Self {
            number: pull.number,
            url: pull.html_url,
            head_sha: pull.head.sha,
            base_sha: pull.base.sha,
        })
    }
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Deserialize)]
struct PullRequestReview {
    id: i64,
    html_url: String,
    body: Option<String>,
    commit_id: String,
    state: String,
}

#[cfg(any(target_arch = "wasm32", test))]
impl PullRequestReview {
    fn matches(&self, request: &SubmitPullRequestReview) -> bool {
        request
            .marked_body()
            .is_ok_and(|body| self.body.as_deref() == Some(body.as_str()))
            && self.commit_id == request.head_sha
            && self.state == REVIEW_STATE
    }

    fn blocks_head(&self, head_sha: &str) -> bool {
        if self.commit_id != head_sha {
            return false;
        }
        // GitHub cannot delete a submitted review, and dismissal preserves its
        // body. This App exposes no review-update operation, so its rendered
        // BLOCK line remains the durable decision even if the review state is
        // later changed to DISMISSED.
        self.state == "CHANGES_REQUESTED"
            || self.body.as_deref().is_some_and(|body| {
                body.lines()
                    .any(|line| line.trim() == format!("{REVIEW_VERDICT_PREFIX} block {head_sha}"))
            })
    }

    fn matches_allow_result(
        &self,
        result: &ReviewResult,
        repository: &str,
        pull_number: i64,
        head_sha: &str,
        review_operation_id: &str,
    ) -> bool {
        let expected_url = format!("https://github.com/{repository}/pull/{pull_number}");
        self.id == result.review_id
            && self.commit_id == head_sha
            && self.state == REVIEW_STATE
            && self.html_url == result.url
            && self.body.as_deref().is_some_and(|body| {
                body.lines()
                    .any(|line| line.trim() == format!("{REVIEW_VERDICT_PREFIX} allow {head_sha}"))
                    && body.lines().any(|line| {
                        line.trim().starts_with(&format!(
                            "{OPERATION_MARKER_PREFIX}{review_operation_id}:"
                        ))
                    })
            })
            && (result.url == expected_url
                || result
                    .url
                    .strip_prefix(&expected_url)
                    .is_some_and(|suffix| suffix.starts_with('#')))
    }
}

#[cfg(any(target_arch = "wasm32", test))]
fn checks_allow_merge(checks: &[CheckResult], required: &[RequiredCheckIdentity]) -> bool {
    if required.is_empty() {
        return false;
    }
    let required_present = required.iter().all(|required| {
        checks.iter().any(|check| {
            check.name == required.context
                && required
                    .integration_id
                    .is_none_or(|app_id| check.app_id == Some(app_id))
                && check.status == "completed"
                && matches!(
                    check.conclusion.as_deref(),
                    Some("success" | "skipped" | "neutral")
                )
        })
    });
    required_present
        && checks.iter().all(|check| {
            check.status == "completed"
                && matches!(
                    check.conclusion.as_deref(),
                    Some("success" | "skipped" | "neutral")
                )
        })
}

#[cfg(any(target_arch = "wasm32", test))]
fn checks_are_terminal_and_non_failing(checks: &[CheckResult]) -> bool {
    !checks.is_empty()
        && checks.iter().all(|check| {
            check.status == "completed"
                && matches!(
                    check.conclusion.as_deref(),
                    Some("success" | "skipped" | "neutral")
                )
        })
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MergeHttpStatus {
    HeadConflict,
    Refused,
    Reconcile,
}

#[cfg(any(target_arch = "wasm32", test))]
fn classify_merge_status(status: u16) -> MergeHttpStatus {
    match status {
        409 => MergeHttpStatus::HeadConflict,
        403 | 404 | 405 | 422 => MergeHttpStatus::Refused,
        _ => MergeHttpStatus::Reconcile,
    }
}

#[cfg(any(target_arch = "wasm32", test))]
fn review_result_allows_merge(
    result: &ReviewResult,
    repository: &str,
    pull_number: i64,
    head_sha: &str,
) -> bool {
    let expected = format!("https://github.com/{repository}/pull/{pull_number}");
    valid_github_url(&result.url).is_ok()
        && (result.url == expected
            || result
                .url
                .strip_prefix(&expected)
                .is_some_and(|suffix| suffix.starts_with('#')))
        && result.head_sha == head_sha
        && result.state == REVIEW_STATE
        && result.verdict == "allow"
}

#[cfg(any(target_arch = "wasm32", test))]
fn merge_commit_has_trailer(
    request: &MergePullRequestAtHead,
    message: &str,
) -> Result<bool, OperationError> {
    let trailer = request.trailer()?;
    Ok(message.lines().any(|line| line.trim() == trailer))
}

#[cfg(any(target_arch = "wasm32", test))]
fn merge_response_result(
    response: PullRequestMergeResponse,
    request: &MergePullRequestAtHead,
) -> Result<MergePullRequestAtHeadResult, OperationError> {
    if !response.merged {
        return Err(OperationError::Refused(RefusalReason::MergePreconditions));
    }
    let merge_commit_sha = response.sha.ok_or(OperationError::Indeterminate)?;
    valid_sha(&merge_commit_sha)?;
    Ok(MergePullRequestAtHeadResult {
        pull_number: request.pull_number,
        head_sha: request.head_sha.clone(),
        base: request.base.clone(),
        merge_commit_sha,
    })
}

#[cfg(any(target_arch = "wasm32", test))]
impl PullRequestReview {
    /// The verdict comes from the request, not from GitHub: all three verdicts
    /// are indistinguishable in the review's `state`, and the App is the only
    /// thing that knows which one it rendered.
    fn into_result(
        self,
        request: &SubmitPullRequestReview,
    ) -> Result<ReviewResult, OperationError> {
        valid_exact_integer(self.id)?;
        valid_github_url(&self.html_url)?;
        valid_sha(&self.commit_id)?;
        // Every review this App can record is posted as `REVIEW_EVENT`, so
        // any other state means this is not one of its reviews. Refusing here
        // is what lets `post_review` trust the state it echoes back.
        if self.state != REVIEW_STATE {
            return Err(OperationError::Unavailable);
        }
        if !self.matches(request) {
            return Err(OperationError::Indeterminate);
        }
        Ok(ReviewResult {
            review_id: self.id,
            url: self.html_url,
            head_sha: self.commit_id,
            state: self.state,
            verdict: request.event.verdict().into(),
        })
    }
}

#[cfg(target_arch = "wasm32")]
#[derive(Deserialize)]
struct CheckRuns {
    total_count: i64,
    check_runs: Vec<CheckRun>,
}

#[cfg(target_arch = "wasm32")]
#[derive(Deserialize)]
struct CheckRun {
    name: String,
    status: String,
    conclusion: Option<String>,
    html_url: String,
    app: Option<CheckRunApp>,
}

#[cfg(target_arch = "wasm32")]
#[derive(Deserialize)]
struct CheckRunApp {
    id: i64,
}

#[cfg(target_arch = "wasm32")]
impl TryFrom<CheckRun> for CheckResult {
    type Error = OperationError;

    fn try_from(check: CheckRun) -> Result<Self, Self::Error> {
        valid_text(&check.name, 1, 256, false)?;
        if !matches!(
            check.status.as_str(),
            "queued" | "in_progress" | "completed" | "pending" | "waiting" | "requested"
        ) || !check.conclusion.as_deref().is_none_or(|conclusion| {
            matches!(
                conclusion,
                "action_required"
                    | "cancelled"
                    | "failure"
                    | "neutral"
                    | "skipped"
                    | "stale"
                    | "startup_failure"
                    | "success"
                    | "timed_out"
            )
        }) {
            return Err(OperationError::Unavailable);
        }
        valid_github_url(&check.html_url)?;
        let app_id = match check.app {
            Some(app) => {
                valid_exact_integer(app.id)?;
                Some(app.id)
            }
            None => None,
        };
        Ok(Self {
            name: check.name,
            status: check.status,
            conclusion: check.conclusion,
            url: check.html_url,
            app_id,
        })
    }
}

#[cfg(target_arch = "wasm32")]
#[derive(Deserialize)]
struct WorkflowRuns {
    total_count: i64,
    workflow_runs: Vec<WorkflowRun>,
}

#[cfg(target_arch = "wasm32")]
#[derive(Deserialize)]
struct WorkflowDispatchResponse {
    workflow_run_id: i64,
    run_url: String,
    html_url: String,
}

#[cfg(target_arch = "wasm32")]
impl WorkflowDispatchResponse {
    fn validate(self) -> Result<Self, OperationError> {
        valid_exact_integer(self.workflow_run_id)?;
        valid_github_api_url(&self.run_url)?;
        valid_github_url(&self.html_url)?;
        let suffix = format!("/actions/runs/{}", self.workflow_run_id);
        if !self.run_url.ends_with(&suffix) || !self.html_url.ends_with(&suffix) {
            return Err(OperationError::Indeterminate);
        }
        Ok(self)
    }
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<ReleaseAsset>,
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Deserialize)]
struct ReleaseAsset {
    name: String,
    size: i64,
    digest: Option<String>,
    state: String,
    browser_download_url: String,
}

#[cfg(any(target_arch = "wasm32", test))]
impl Release {
    fn into_result(self, tag: &str) -> Result<Option<ReleaseResult>, OperationError> {
        let expected_names = [
            format!("dark-factory-{tag}-aarch64-apple-darwin.tar.gz"),
            format!("dark-factory-{tag}-x86_64-apple-darwin.tar.gz"),
            "SHA256SUMS".into(),
            "latest.json".into(),
            "dark-factory.rb".into(),
        ];
        if self.tag_name != tag
            || self.draft
            || self.prerelease != tag.contains('-')
            || self.assets.len() != expected_names.len()
        {
            return Err(OperationError::Indeterminate);
        }
        let release_suffix = format!("/releases/tag/{tag}");
        valid_github_url(&self.html_url)?;
        if !self.html_url.ends_with(&release_suffix) {
            return Err(OperationError::Indeterminate);
        }
        let mut assets_by_name = BTreeMap::new();
        for asset in self.assets {
            if assets_by_name.insert(asset.name.clone(), asset).is_some() {
                return Err(OperationError::Indeterminate);
            }
        }
        let mut assets = Vec::with_capacity(expected_names.len());
        for name in expected_names {
            let asset = assets_by_name
                .remove(&name)
                .ok_or(OperationError::Indeterminate)?;
            (1..=i64::MAX)
                .contains(&asset.size)
                .then_some(())
                .ok_or(OperationError::Indeterminate)?;
            if asset.state != "uploaded" {
                return Err(OperationError::Indeterminate);
            }
            valid_github_url(&asset.browser_download_url)?;
            let asset_suffix = format!("/releases/download/{tag}/{}", asset.name);
            if !asset.browser_download_url.ends_with(&asset_suffix) {
                return Err(OperationError::Indeterminate);
            }
            let digest = asset.digest.ok_or(OperationError::Indeterminate)?;
            valid_digest(&digest)?;
            assets.push(ReleaseAssetResult {
                name: asset.name,
                size: asset.size,
                digest,
                url: asset.browser_download_url,
            });
        }
        if !assets_by_name.is_empty() {
            return Err(OperationError::Indeterminate);
        }
        Ok(Some(ReleaseResult {
            tag: tag.to_owned(),
            url: self.html_url,
            draft: self.draft,
            prerelease: self.prerelease,
            assets,
        }))
    }
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Deserialize)]
struct WorkflowRun {
    id: i64,
    run_attempt: i64,
    name: String,
    path: String,
    event: String,
    status: String,
    conclusion: Option<String>,
    head_sha: String,
    #[serde(default)]
    head_branch: String,
    #[serde(default)]
    display_title: Option<String>,
    html_url: String,
    #[serde(default)]
    pull_requests: Vec<WorkflowPullRequest>,
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Deserialize)]
struct WorkflowPullRequest {
    number: i64,
}

#[cfg(any(target_arch = "wasm32", test))]
fn select_release_workflow_run(
    runs: Vec<WorkflowRun>,
    tag: &str,
    commit_sha: &str,
) -> Result<WorkflowRun, OperationError> {
    let title = format!("Release {tag}");
    let mut matches = runs
        .into_iter()
        .filter(|run| {
            run.path == RELEASE_WORKFLOW.response_path
                && run.head_sha == commit_sha
                && run.head_branch == tag
                && run.event == "push"
                && run.display_title.as_deref() == Some(&title)
        })
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(OperationError::Indeterminate);
    }
    matches.pop().ok_or(OperationError::Indeterminate)
}

#[cfg(any(target_arch = "wasm32", test))]
impl WorkflowRun {
    #[cfg(target_arch = "wasm32")]
    fn verify(
        &self,
        workflow: WorkflowRef<'_>,
        pull_number: i64,
        head_sha: &str,
    ) -> Result<(), OperationError> {
        valid_exact_integer(self.id)?;
        valid_exact_integer(self.run_attempt)?;
        valid_text(&self.name, 1, 256, false)?;
        self.verify_identity(workflow, head_sha)?;
        if self.event != "pull_request"
            || self.pull_requests.len() != 1
            || self.pull_requests[0].number != pull_number
        {
            return Err(OperationError::Conflict);
        }
        Ok(())
    }

    fn verify_identity(
        &self,
        workflow: WorkflowRef<'_>,
        head_sha: &str,
    ) -> Result<(), OperationError> {
        valid_exact_integer(self.id)?;
        valid_exact_integer(self.run_attempt)?;
        valid_text(&self.name, 1, 256, false)?;
        if self.path != workflow.response_path
            || self.head_sha != head_sha
            || !valid_workflow_status(&self.status)
            || !self
                .conclusion
                .as_deref()
                .is_none_or(valid_workflow_conclusion)
        {
            return Err(OperationError::Conflict);
        }
        valid_github_url(&self.html_url)
    }

    fn verify_dispatch(
        &self,
        workflow: WorkflowRef<'_>,
        head_sha: &str,
        title: &str,
    ) -> Result<(), OperationError> {
        self.verify_identity(workflow, head_sha)?;
        if self.event != "workflow_dispatch" || self.display_title.as_deref() != Some(title) {
            return Err(OperationError::Conflict);
        }
        Ok(())
    }

    #[cfg(target_arch = "wasm32")]
    fn into_result(self, jobs: Vec<WorkflowJob>) -> Result<WorkflowRunResult, OperationError> {
        let mut jobs = jobs
            .into_iter()
            .map(|job| job.into_result(&self.head_sha))
            .collect::<Result<Vec<_>, _>>()?;
        jobs.sort_by_key(|job| job.job_id);
        Ok(WorkflowRunResult {
            run_id: self.id,
            run_attempt: self.run_attempt,
            name: self.name,
            path: self.path,
            event: self.event,
            status: self.status,
            conclusion: self.conclusion,
            url: self.html_url,
            jobs,
        })
    }
}

#[cfg(target_arch = "wasm32")]
#[derive(Deserialize)]
struct WorkflowJobs {
    total_count: i64,
    jobs: Vec<WorkflowJob>,
}

#[cfg(target_arch = "wasm32")]
#[derive(Deserialize)]
struct WorkflowJob {
    id: i64,
    run_id: i64,
    name: String,
    head_sha: String,
    status: String,
    conclusion: Option<String>,
    html_url: String,
    #[serde(default)]
    steps: Vec<WorkflowStep>,
}

#[cfg(target_arch = "wasm32")]
impl WorkflowJob {
    fn into_result(self, head_sha: &str) -> Result<WorkflowJobResult, OperationError> {
        valid_exact_integer(self.id)?;
        valid_exact_integer(self.run_id)?;
        valid_text(&self.name, 1, 256, false)?;
        if self.head_sha != head_sha
            || !valid_workflow_status(&self.status)
            || !self
                .conclusion
                .as_deref()
                .is_none_or(valid_workflow_conclusion)
            || self.steps.len() > MAX_WORKFLOW_STEPS
        {
            return Err(OperationError::Indeterminate);
        }
        valid_github_url(&self.html_url)?;
        let steps = self
            .steps
            .into_iter()
            .map(WorkflowStep::into_result)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(WorkflowJobResult {
            job_id: self.id,
            name: self.name,
            status: self.status,
            conclusion: self.conclusion,
            url: self.html_url,
            steps,
        })
    }
}

#[cfg(target_arch = "wasm32")]
#[derive(Deserialize)]
struct WorkflowStep {
    name: String,
    status: String,
    conclusion: Option<String>,
}

#[cfg(target_arch = "wasm32")]
impl WorkflowStep {
    fn into_result(self) -> Result<WorkflowStepResult, OperationError> {
        valid_text(&self.name, 1, 256, false)?;
        if !valid_workflow_status(&self.status)
            || !self
                .conclusion
                .as_deref()
                .is_none_or(valid_workflow_conclusion)
        {
            return Err(OperationError::Indeterminate);
        }
        Ok(WorkflowStepResult {
            name: self.name,
            status: self.status,
            conclusion: self.conclusion,
        })
    }
}

#[cfg(any(target_arch = "wasm32", test))]
/// `.github/workflows/<file>.yml`, and nothing else. GitHub reports a run's
/// `path` in exactly this shape, so anything else can never match a real run
/// and is the caller's error rather than an empty result.
fn valid_workflow_path(value: &str) -> Result<(), OperationError> {
    let file = value
        .strip_prefix(".github/workflows/")
        .filter(|file| file.ends_with(".yml") || file.ends_with(".yaml"))
        .filter(|file| valid_path_segment(file, 100, true));
    file.map(|_| ()).ok_or(OperationError::InvalidInput)
}

fn valid_workflow_status(value: &str) -> bool {
    matches!(
        value,
        "queued" | "in_progress" | "completed" | "pending" | "waiting" | "requested"
    )
}

#[cfg(any(target_arch = "wasm32", test))]
fn valid_workflow_conclusion(value: &str) -> bool {
    matches!(
        value,
        "action_required"
            | "cancelled"
            | "failure"
            | "neutral"
            | "skipped"
            | "stale"
            | "startup_failure"
            | "success"
            | "timed_out"
    )
}

#[cfg(any(target_arch = "wasm32", test))]
fn failed_conclusion(value: &str) -> bool {
    matches!(
        value,
        "action_required" | "cancelled" | "failure" | "startup_failure" | "timed_out"
    )
}

#[cfg(any(target_arch = "wasm32", test))]
fn valid_github_url(value: &str) -> Result<(), OperationError> {
    (value.starts_with("https://github.com/")
        && value.len() <= 2_048
        && value.is_ascii()
        && !value.contains(['\r', '\n']))
    .then_some(())
    .ok_or(OperationError::Unavailable)
}

#[cfg(any(target_arch = "wasm32", test))]
fn valid_github_api_url(value: &str) -> Result<(), OperationError> {
    (value.starts_with("https://api.github.com/")
        && value.len() <= 2_048
        && value.is_ascii()
        && !value.contains(['\r', '\n']))
    .then_some(())
    .ok_or(OperationError::Unavailable)
}

#[cfg(any(target_arch = "wasm32", test))]
fn valid_digest(value: &str) -> Result<(), OperationError> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return Err(OperationError::Indeterminate);
    };
    (hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then_some(())
        .ok_or(OperationError::Indeterminate)
}

#[cfg(target_arch = "wasm32")]
fn percent_encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[(byte >> 4) as usize]));
            encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
    }
    encoded
}

fn validate_installation(installation: &Installation, app_id: i64) -> Result<(), &'static str> {
    let rejected: Vec<&str> = [
        (installation.id <= 0).then_some("id"),
        (installation.app_id != app_id).then_some("app_id"),
        (installation.repository_selection != "selected").then_some("repository_selection"),
        (!permission_at_least(&installation.permissions, "actions", "write")).then_some("actions"),
        (!permission_at_least(&installation.permissions, "checks", "read")).then_some("checks"),
        // Permission revisions are all-or-nothing at the installation boundary:
        // status must not advertise v4 for a repository where direct merge is
        // unusable. Only direct merge downscopes this grant into its operation
        // token; every other token still omits it.
        (!permission_at_least(&installation.permissions, "administration", "write"))
            .then_some("administration"),
        // `publish_commit` mints `contents: write`. Accepting a read-only
        // installation would fail at token mint instead, where GitHub's 422
        // reaches the caller as an opaque "authority is unavailable". This is
        // the only place an installation is audited -- readiness names no
        // repository, so it has none to look up.
        (!permission_at_least(&installation.permissions, "contents", "write"))
            .then_some("contents"),
        (!permission_at_least(&installation.permissions, "issues", "write")).then_some("issues"),
        (!permission_at_least(&installation.permissions, "metadata", "read")).then_some("metadata"),
        (!permission_at_least(&installation.permissions, "pull_requests", "write"))
            .then_some("pull_requests"),
        // Queue enqueue still needs this authority; direct squash merge is
        // separately gated by branch protection and exact-head checks.
        (!permission_at_least(&installation.permissions, "merge_queues", "write"))
            .then_some("merge_queues"),
        (!installation.events.is_empty()).then_some("events"),
        (installation.suspended_at.is_some()).then_some("suspended_at"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if let Some(first) = rejected.first() {
        #[cfg(target_arch = "wasm32")]
        worker::console_error!("installation rejected on: {}", rejected.join(","));
        return Err(first);
    }
    Ok(())
}

fn permission_at_least(permissions: &BTreeMap<String, String>, name: &str, required: &str) -> bool {
    matches!(
        (permissions.get(name).map(String::as_str), required),
        (Some("read" | "write" | "admin"), "read")
            | (Some("write" | "admin"), "write")
            | (Some("admin"), "admin")
    )
}

#[cfg(any(target_arch = "wasm32", test))]
fn workflow_api_url(
    owner: &str,
    repository: &str,
    workflow: WorkflowRef<'_>,
    endpoint: &str,
) -> String {
    format!(
        "https://api.github.com/repos/{owner}/{repository}/actions/workflows/{}/{endpoint}",
        workflow.api_id
    )
}

/// A GitHub App JWT authenticates only App-level endpoints. Every other REST
/// resource requires an installation token, and GitHub answers `Bad
/// credentials` when a JWT is presented to one.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn app_jwt_endpoint(url: &str) -> bool {
    // Match the path only. A query or fragment can otherwise supply the suffix
    // (`.../pulls?x=/installation`), and dot segments are collapsed by the URL
    // parser after this check runs.
    let path = &url[..url.find(['?', '#']).unwrap_or(url.len())];
    if path.contains("..") {
        return false;
    }
    path == "https://api.github.com/app"
        || path.starts_with("https://api.github.com/app/installations/")
        || (path.starts_with("https://api.github.com/repos/") && path.ends_with("/installation"))
}

#[cfg(target_arch = "wasm32")]
async fn github_json_as_app<T: serde::de::DeserializeOwned>(
    url: &str,
    jwt: &str,
) -> Result<T, Error> {
    if !app_jwt_endpoint(url) {
        worker::console_error!("refusing to present an app jwt to {url}");
        return Err(Error::Unavailable);
    }
    github_json(url, jwt).await
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Deserialize)]
struct GraphQlError {
    #[serde(rename = "type")]
    kind: Option<String>,
    /// The response path the error was raised at. A path deeper than the
    /// mutation root means a resolver had to run to produce an object to
    /// select into, so the effect may already exist -- whatever the type says.
    #[serde(default)]
    path: Vec<serde_json::Value>,
}

/// Classify a GraphQL `errors` array into what it establishes about the
/// operation, or `None` when there were no errors.
///
/// Pure, so it is host-testable: the transport around it is `wasm32`-only, and
/// this table is where "the operation did not run" is asserted. Getting a
/// class wrong in the permissive direction reports "nothing changed" for
/// something that may have changed.
///
/// `RATE_LIMITED` is rejected-before-execution because GitHub refuses the
/// request rather than half-running it. `SERVICE_UNAVAILABLE` and every
/// untyped error are not: GitHub returns an untyped `Something went wrong
/// while executing your query` for what its own documentation says "may be the
/// result of a timeout", and a request that timed out server-side may have
/// executed.
#[cfg(any(target_arch = "wasm32", test))]
fn classify_graphql_errors(errors: &[GraphQlError]) -> Option<GraphQlFailure> {
    if errors.is_empty() {
        return None;
    }
    let mut kinds = RejectionKinds::default();
    for error in errors {
        // A type alone is not enough. GitHub raises typed errors -- including
        // `FORBIDDEN`, for permission scoping on an installation token -- while
        // *resolving* deep fields, and an error at a path below the mutation
        // root is post-execution by construction: the resolver had to run to
        // produce something to select into. On this mutation that matters,
        // because `mergeQueueEntry.id` and `.state` are non-null, so an error
        // on either nulls `mergeQueueEntry` itself and the entry looks absent
        // for an enqueue that already happened.
        //
        // A root-level error keeps its type: the SAML `FORBIDDEN` GitHub
        // documents arrives at `path: ["<mutationField>"]`, length one, and is
        // a genuine pre-execution rejection.
        if error.path.len() > 1 {
            return Some(GraphQlFailure::Unknown);
        }
        match error.kind.as_deref() {
            Some("NOT_FOUND") => kinds.not_found = true,
            Some("FORBIDDEN") => kinds.forbidden = true,
            Some("UNPROCESSABLE") => kinds.unprocessable = true,
            Some("RATE_LIMITED") => kinds.rate_limited = true,
            _ => return Some(GraphQlFailure::Unknown),
        }
    }
    Some(GraphQlFailure::Rejected(kinds))
}

/// What an enqueue mutation established, decided from the **payload** rather
/// than the envelope.
///
/// This is the distinction the first version got wrong. A GraphQL field error
/// nulls the field it names and leaves `data` an object, so GitHub's ordinary
/// refusal shape is a populated `data` carrying `enqueuePullRequest: null`
/// *beside* an errors array. Asking "did the envelope have data?" routes every
/// real refusal past the classification and answers `Refused` for outcomes
/// nobody knows.
///
/// Split out so it is host-testable: the transport is `wasm32`-only, and a
/// grep-style assertion on the source cannot catch a rewrite of this decision.
#[cfg(any(target_arch = "wasm32", test))]
fn enqueue_outcome(
    entry: Option<QueueEntry>,
    failure: Option<GraphQlFailure>,
) -> Result<QueueEntry, OperationError> {
    match (entry, failure) {
        // An entry came back. That is the effect, errors alongside it or not.
        (Some(entry), _) => Ok(entry),
        // Rejected before execution: nothing was queued, and the same
        // operation ID stays retryable once the precondition holds.
        (None, Some(GraphQlFailure::Rejected(kinds))) => {
            Err(OperationError::Refused(RefusalReason::Rejected(kinds)))
        }
        // An untyped error may follow a server-side timeout on work already
        // under way. The caller reconciles against the queue rather than
        // asserting nothing happened.
        (None, Some(GraphQlFailure::Unknown)) => Err(OperationError::Indeterminate),
        // No entry and no error is a determinate "did not enqueue".
        (None, None) => Err(OperationError::Refused(RefusalReason::NoEffect)),
    }
}

/// What a GraphQL response establishes, handed back to the caller rather than
/// decided here.
///
/// A mutation and a read need opposite answers to the same response, so the
/// transport must not collapse them: for `enqueue_entry` a rejected request
/// means "nothing happened, retry this operation ID"; for `reconcile_enqueue`
/// the identical response means "I could not find out", which is never a
/// refusal of the operation being reconciled.
#[cfg(any(target_arch = "wasm32", test))]
#[derive(Clone, Copy, Debug)]
enum GraphQlFailure {
    /// Every error names a class GitHub rejects *before* running the
    /// operation, so no effect was produced. Which classes appeared rides
    /// along, because the reason ends at the MCP boundary (#371).
    Rejected(RejectionKinds),
    /// At least one error carries no type, or one GitHub can return after it
    /// began executing. Whether the effect landed is not knowable from this.
    Unknown,
}

/// One typed GraphQL operation, not a proxy: each query text is a
/// compile-time constant and callers supply only typed variables. There is no
/// path by which an agent's input becomes GraphQL.
///
/// GraphQL answers **200 with an `errors` array** for a rejected operation, so
/// a status check alone reports success for a failure. Two consequences are
/// handled here rather than assumed away:
///
/// - `data` and `errors` can both be populated. That partial result is what
///   GitHub actually did, so it is returned rather than discarded -- a
///   field-level error under the selection would otherwise be reported as
///   "nothing changed" for an operation that landed.
/// - Not every error means the operation did not run. GitHub returns 200 with
///   an untyped `Something went wrong while executing your query` for what its
///   own documentation says "may be the result of a timeout", and a request
///   that timed out server-side may have executed. Only error classes that are
///   rejected before execution are reported as such; anything else is unknown,
///   which fails safe.
#[cfg(target_arch = "wasm32")]
async fn github_graphql<T: serde::de::DeserializeOwned, V: Serialize>(
    token: &Credential,
    query: &str,
    variables: &V,
) -> Result<(Option<T>, Option<GraphQlFailure>), Error> {
    #[derive(Serialize)]
    struct Body<'a, V> {
        query: &'a str,
        variables: &'a V,
    }
    #[derive(Deserialize)]
    struct Envelope<T> {
        data: Option<T>,
        #[serde(default)]
        errors: Vec<GraphQlError>,
    }
    let envelope: Envelope<T> = github_json_request(
        worker::Method::Post,
        "https://api.github.com/graphql",
        token.as_str(),
        Some(&Body { query, variables }),
    )
    .await?;
    // Diagnosis only, and unconditional: a field error nulls its field and
    // leaves `data` an object, so returning early on a populated `data` would
    // mean the most common refusal logged nothing at all. Messages can quote
    // caller input, so only the typed classification is logged, never the text.
    for error in &envelope.errors {
        worker::console_error!(
            "github graphql error: {}",
            error.kind.as_deref().unwrap_or("untyped")
        );
    }
    Ok((envelope.data, classify_graphql_errors(&envelope.errors)))
}

#[cfg(target_arch = "wasm32")]
async fn github_json<T: serde::de::DeserializeOwned>(
    url: &str,
    credential: &str,
) -> Result<T, Error> {
    github_json_request::<T, serde_json::Value>(worker::Method::Get, url, credential, None).await
}

#[cfg(target_arch = "wasm32")]
async fn github_json_request<T: serde::de::DeserializeOwned, B: Serialize>(
    method: worker::Method,
    url: &str,
    credential: &str,
    body: Option<&B>,
) -> Result<T, Error> {
    let body = body
        .map(serde_json::to_string)
        .transpose()
        .map_err(|_| Error::Unavailable)?;
    let bytes = github_response(method, url, credential, body).await?;
    serde_json::from_slice(&bytes).map_err(|error| {
        worker::console_error!(
            "github response from {url} did not match the expected shape ({} bytes): {error}",
            bytes.len()
        );
        Error::Unavailable
    })
}

#[cfg(target_arch = "wasm32")]
async fn github_empty_request(
    method: worker::Method,
    url: &str,
    credential: &str,
    expected_status: u16,
) -> Result<(), Error> {
    let response = github_request(method, url, credential, None).await?;
    if response.status_code() != expected_status {
        worker::console_error!(
            "github answered {url} with status {}, expected {expected_status}",
            response.status_code()
        );
        return Err(Error::Rejected(response.status_code()));
    }
    read_github_response(response, url, MAX_GITHUB_RESPONSE_BYTES)
        .await
        .map(|_| ())
}

#[cfg(target_arch = "wasm32")]
async fn github_response(
    method: worker::Method,
    url: &str,
    credential: &str,
    body: Option<String>,
) -> Result<Vec<u8>, Error> {
    let response = github_request(method, url, credential, body).await?;
    read_github_response(response, url, MAX_GITHUB_RESPONSE_BYTES).await
}

#[cfg(target_arch = "wasm32")]
async fn github_request(
    method: worker::Method,
    url: &str,
    credential: &str,
    body: Option<String>,
) -> Result<worker::Response, Error> {
    use worker::{Fetch, Headers, Request, RequestInit, RequestRedirect};

    let headers = Headers::new();
    let authorization = Zeroizing::new(format!("Bearer {credential}"));
    // Set the credential outside the logged loop. Nothing below formats a header
    // value, and keeping the credential out of that iteration means a later edit to
    // the log line cannot put a live token into Cloudflare logs.
    headers
        .set("authorization", authorization.as_str())
        .map_err(|_| Error::Unavailable)?;
    for (name, value) in [
        ("accept", "application/vnd.github+json"),
        ("user-agent", "dark-factory-control-plane/0.1"),
        ("x-github-api-version", GITHUB_API_VERSION),
    ] {
        headers.set(name, value).map_err(|error| {
            worker::console_error!("github header {name} rejected: {error}");
            Error::Unavailable
        })?;
    }
    if body.is_some() {
        headers
            .set("content-type", "application/json")
            .map_err(|_| Error::Unavailable)?;
    }
    let mut init = RequestInit::new();
    init.with_method(method)
        // Workers' Request supports only `follow` and `manual`; `error` makes the
        // constructor throw, so no request ever leaves the edge. `manual` returns
        // the redirect itself, which the status check below rejects as non-2xx —
        // a redirect is still never followed.
        .with_redirect(RequestRedirect::Manual)
        .with_headers(headers)
        .with_body(body.map(Into::into));
    let request = Request::new_with_init(url, &init).map_err(|error| {
        worker::console_error!("github request could not be built for {url}: {error}");
        Error::Unavailable
    })?;
    // Diagnosis only: never log credentials, headers, or bodies.
    let response = match Fetch::Request(request).send().await {
        Ok(response) => response,
        Err(error) => {
            worker::console_error!("github request failed: {url}: {error:?}");
            return Err(Error::Unavailable);
        }
    };
    Ok(response)
}

#[cfg(target_arch = "wasm32")]
async fn read_github_response(
    mut response: worker::Response,
    url: &str,
    limit: usize,
) -> Result<Vec<u8>, Error> {
    use futures_util::TryStreamExt as _;

    if !(200..300).contains(&response.status_code()) {
        worker::console_error!(
            "github rejected {url} with status {}",
            response.status_code()
        );
        return Err(Error::Rejected(response.status_code()));
    }
    let mut stream = response.stream().map_err(|_| Error::Unavailable)?;
    let mut bytes = Vec::new();
    while let Some(mut chunk) = stream.try_next().await.map_err(|_| Error::Unavailable)? {
        let next_len = bytes
            .len()
            .checked_add(chunk.len())
            .ok_or(Error::Unavailable)?;
        if next_len > limit {
            return Err(Error::Unavailable);
        }
        bytes.append(&mut chunk);
    }
    Ok(bytes)
}

#[cfg(target_arch = "wasm32")]
async fn github_redirect_location(url: &str, credential: &str) -> Result<String, OperationError> {
    let response = github_request(worker::Method::Get, url, credential, None).await?;
    if response.status_code() != 302 {
        return Err(Error::Rejected(response.status_code()).into());
    }
    let location = response
        .headers()
        .get("location")
        .map_err(|_| OperationError::Unavailable)?
        .ok_or(OperationError::Unavailable)?;
    valid_log_redirect(&location)?;
    Ok(location)
}

#[cfg(target_arch = "wasm32")]
async fn github_public_log(location: &str) -> Result<String, OperationError> {
    use worker::{Fetch, Headers, Request, RequestInit, RequestRedirect};

    valid_log_redirect(location)?;
    let headers = Headers::new();
    for (name, value) in [
        ("accept", "text/plain"),
        ("range", "bytes=-65536"),
        ("user-agent", "dark-factory-control-plane/0.1"),
    ] {
        headers
            .set(name, value)
            .map_err(|_| OperationError::Unavailable)?;
    }
    let mut init = RequestInit::new();
    init.with_method(worker::Method::Get)
        .with_redirect(RequestRedirect::Manual)
        .with_headers(headers);
    let request =
        Request::new_with_init(location, &init).map_err(|_| OperationError::Unavailable)?;
    // This request is deliberately credentialless. The short-lived signed URL
    // is the complete authorization and is never logged or returned.
    let mut response = Fetch::Request(request)
        .send()
        .await
        .map_err(|_| OperationError::Unavailable)?;
    if !matches!(response.status_code(), 200 | 206) {
        return Err(OperationError::Unavailable);
    }
    let bytes = read_response_tail(&mut response, MAX_JOB_LOG_BYTES).await?;
    Ok(String::from_utf8_lossy(&bytes)
        .chars()
        .map(|character| {
            if character == '\n' || character == '\t' || !character.is_control() {
                character
            } else {
                '\u{fffd}'
            }
        })
        .collect())
}

#[cfg(target_arch = "wasm32")]
async fn read_response_tail(
    response: &mut worker::Response,
    limit: usize,
) -> Result<Vec<u8>, OperationError> {
    use futures_util::TryStreamExt as _;

    let mut stream = response.stream().map_err(|_| OperationError::Unavailable)?;
    let mut bytes = Vec::new();
    while let Some(mut chunk) = stream
        .try_next()
        .await
        .map_err(|_| OperationError::Unavailable)?
    {
        retain_tail(&mut bytes, &mut chunk, limit);
    }
    Ok(bytes)
}

#[cfg(any(target_arch = "wasm32", test))]
fn retain_tail(bytes: &mut Vec<u8>, chunk: &mut Vec<u8>, limit: usize) {
    if chunk.len() >= limit {
        bytes.clear();
        bytes.extend_from_slice(&chunk[chunk.len() - limit..]);
        return;
    }
    let overflow = bytes
        .len()
        .saturating_add(chunk.len())
        .saturating_sub(limit);
    if overflow > 0 {
        bytes.drain(..overflow);
    }
    bytes.append(chunk);
}

#[cfg(any(target_arch = "wasm32", test))]
fn valid_log_redirect(location: &str) -> Result<(), OperationError> {
    if location.len() > MAX_LOG_REDIRECT_BYTES
        || !location.is_ascii()
        || location.bytes().any(|byte| byte.is_ascii_control())
        || location.contains('#')
    {
        return Err(OperationError::Unavailable);
    }
    let rest = location
        .strip_prefix("https://")
        .ok_or(OperationError::Unavailable)?;
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() || authority.contains(['@', ':']) {
        return Err(OperationError::Unavailable);
    }
    let host = authority.to_ascii_lowercase();
    if !(host.ends_with(".actions.githubusercontent.com")
        || host.ends_with(".blob.core.windows.net"))
    {
        return Err(OperationError::Unavailable);
    }
    Ok(())
}

#[cfg(target_arch = "wasm32")]
async fn sign_rs256(private_key: &[u8], message: &[u8]) -> Result<Vec<u8>, Error> {
    use js_sys::{Array, Function, Object, Promise, Reflect, Uint8Array};
    use wasm_bindgen::{JsCast as _, JsValue};
    use wasm_bindgen_futures::JsFuture;

    let algorithm = Object::new();
    Reflect::set(&algorithm, &"name".into(), &"RSASSA-PKCS1-v1_5".into())
        .map_err(|_| Error::Unavailable)?;
    Reflect::set(&algorithm, &"hash".into(), &"SHA-256".into()).map_err(|_| Error::Unavailable)?;
    let global = js_sys::global();
    let crypto = Reflect::get(&global, &"crypto".into()).map_err(|_| Error::Unavailable)?;
    let subtle = Reflect::get(&crypto, &"subtle".into()).map_err(|_| Error::Unavailable)?;
    let import_key: Function = Reflect::get(&subtle, &"importKey".into())
        .map_err(|_| Error::Unavailable)?
        .dyn_into()
        .map_err(|_| Error::Unavailable)?;
    let usages = Array::new();
    usages.push(&"sign".into());
    let key_bytes = Uint8Array::from(private_key);
    let promise: Promise = import_key
        .call5(
            &subtle,
            &"pkcs8".into(),
            key_bytes.as_ref(),
            algorithm.as_ref(),
            &JsValue::FALSE,
            usages.as_ref(),
        )
        .map_err(|_| Error::Unavailable)?
        .dyn_into()
        .map_err(|_| Error::Unavailable)?;
    let key = JsFuture::from(promise)
        .await
        .map_err(|_| Error::Unavailable)?;
    let sign: Function = Reflect::get(&subtle, &"sign".into())
        .map_err(|_| Error::Unavailable)?
        .dyn_into()
        .map_err(|_| Error::Unavailable)?;
    let message = Uint8Array::from(message);
    let promise: Promise = sign
        .call3(&subtle, &"RSASSA-PKCS1-v1_5".into(), &key, message.as_ref())
        .map_err(|_| Error::Unavailable)?
        .dyn_into()
        .map_err(|_| Error::Unavailable)?;
    let signature = JsFuture::from(promise)
        .await
        .map_err(|_| Error::Unavailable)?;
    Ok(Uint8Array::new(&signature).to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwt_claims_are_bounded_and_use_the_numeric_app_id() {
        let unsigned = jwt_unsigned(4_673_420, 1_800_000_000);
        let segments = unsigned.split('.').collect::<Vec<_>>();
        let [header, claims] = segments.as_slice() else {
            panic!("expected two JWT segments");
        };
        assert_eq!(
            general_purpose::URL_SAFE_NO_PAD.decode(header).unwrap(),
            br#"{"alg":"RS256","typ":"JWT"}"#
        );
        assert_eq!(
            general_purpose::URL_SAFE_NO_PAD.decode(claims).unwrap(),
            br#"{"iat":1799999940,"exp":1800000540,"iss":"4673420"}"#
        );
    }

    /// The exact defect this repair fixes, proven at the boundary it happened
    /// on rather than asserted about the source.
    ///
    /// `GET /repos/{owner}/{repo}` answers **200** with the body below while
    /// omitting Administration-gated fields. The body was captured
    /// unauthenticated, so it is evidence about the response shape rather than
    /// about this App's installation grant.
    /// While `RepositoryMetadata` required
    /// `delete_branch_on_merge`, this exact 200 failed to deserialize, and all
    /// eleven operations that observe the repository -- `maintainer_status`,
    /// publication, PR creation, enqueue, merge observation, workflow and job
    /// observation, failed-job rerun, release publication/recovery, and the
    /// control-plane deploy dispatch -- returned an opaque "authority is
    /// unavailable".
    #[test]
    fn repository_metadata_parses_a_real_body_without_optional_fields() {
        const BODY: &str = include_str!("../tests/fixtures/repository-without-administration.json");

        // The premise under test: GitHub really does omit the
        // Administration-gated group. Without this the test would still pass
        // against a body that happened to carry the field, and would stop
        // being a regression test for the defect.
        let raw: serde_json::Value = serde_json::from_str(BODY).unwrap();
        for gated in [
            "delete_branch_on_merge",
            "allow_squash_merge",
            "allow_merge_commit",
            "allow_rebase_merge",
        ] {
            assert!(
                raw.get(gated).is_none(),
                "fixture carries {gated}, so it is not an omitted-fields body"
            );
        }

        let metadata: RepositoryMetadata = serde_json::from_str(BODY).unwrap();
        let metadata = metadata
            .validate(&granted("dark-factory-build/dark-factory"))
            .unwrap();
        assert_eq!(metadata.id, 1_335_380_107);
        assert_eq!(metadata.full_name, "dark-factory-build/dark-factory");
        assert_eq!(metadata.default_branch, "main");
    }

    /// A token as `installation_token` would return it: the repository the
    /// grant covers, with the id GitHub answered for that name.
    fn granted(full_name: &str) -> RepositoryToken {
        RepositoryToken {
            token: Credential::new("ghs_0000000000000000000000000000000000".into()).unwrap(),
            repository: RepositoryName::new(full_name.into()).unwrap(),
            repository_id: 1_335_380_107,
        }
    }

    #[test]
    fn repository_metadata_is_bound_to_the_grant() {
        let body = |id: i64, full_name: &str| {
            format!(r#"{{"id":{id},"full_name":"{full_name}","default_branch":"main"}}"#)
        };
        // The grant and the metadata are both GitHub's own answers, so they
        // agree exactly or the repository is not the one the token covers.
        let matching: RepositoryMetadata =
            serde_json::from_str(&body(1_335_380_107, "dark-factory-build/dark-factory")).unwrap();
        assert!(
            matching
                .validate(&granted("dark-factory-build/dark-factory"))
                .is_ok()
        );
        // A different repository is a conflict even when the id matches, and a
        // different id is a conflict even when the name matches. Neither half
        // is load-bearing alone.
        for (id, full_name) in [
            (1_335_380_107, "dark-factory-build/dark-factory-site"),
            (1_335_380_108, "dark-factory-build/dark-factory"),
        ] {
            let other: RepositoryMetadata = serde_json::from_str(&body(id, full_name)).unwrap();
            assert_eq!(
                other
                    .validate(&granted("dark-factory-build/dark-factory"))
                    .err(),
                Some(OperationError::Conflict),
                "{full_name} at {id} was accepted"
            );
        }
    }

    #[test]
    fn a_requested_repository_is_lower_cased_before_its_digest_is_taken() {
        // Two spellings of one repository must be one request, or a retry that
        // differed only in case would conflict with its own operation id.
        let mut mixed = "Dark-Factory-Build/Dark-Factory".to_owned();
        let mut lower = "dark-factory-build/dark-factory".to_owned();
        assert_eq!(
            RepositoryName::requested(&mut mixed).unwrap().full_name,
            RepositoryName::requested(&mut lower).unwrap().full_name
        );
        assert_eq!(mixed, lower);

        for malformed in [
            "dark-factory",
            "dark-factory-build/dark-factory/extra",
            "dark-factory-build/../dark-factory",
            "dark factory/dark-factory",
            "/dark-factory",
            "dark-factory-build/",
            "dark_factory_build/dark-factory",
        ] {
            let mut value = malformed.to_owned();
            assert_eq!(
                RepositoryName::requested(&mut value).err(),
                Some(OperationError::InvalidInput),
                "{malformed} was accepted"
            );
        }
    }

    /// One operation UUID must not mean two different operations. With the
    /// repository configured per deployment it could not, because there was one
    /// repository; now that the caller names it, the repository has to be part
    /// of the replay identity or the same UUID would address a write to the
    /// site and a write to the runtime as though they were the same request.
    #[test]
    fn the_repository_is_part_of_a_replay_identity() {
        let request = |repository: &str| CreateIssue {
            repository: repository.into(),
            operation_id: "0c8a5c44-7f1f-11f0-952e-acde48001122".into(),
            title: "Bounded issue".into(),
            body: "Acceptance criteria.".into(),
        };
        let runtime = request_digest(&request("dark-factory-build/dark-factory")).unwrap();
        let site = request_digest(&request("dark-factory-build/dark-factory-site")).unwrap();
        assert_ne!(
            runtime, site,
            "one operation id addresses two repositories as one request"
        );
        // And the same request is still the same request, so an honest retry
        // replays rather than conflicting.
        assert_eq!(
            runtime,
            request_digest(&request("dark-factory-build/dark-factory")).unwrap()
        );
        // Case is normalized before the digest is taken, so the two spellings
        // of one repository do not become two operations.
        let mut mixed = request("Dark-Factory-Build/Dark-Factory");
        mixed.validate().unwrap();
        RepositoryName::requested(&mut mixed.repository).unwrap();
        assert_eq!(runtime, request_digest(&mixed).unwrap());
    }

    /// `valid_ref` is the only guard on the string that reaches
    /// `.../git/ref/heads/{branch}`, and `observe_ref` made a ref name the
    /// entire caller-controlled input for the first time. It had no negative
    /// coverage anywhere in the crate.
    #[test]
    fn a_requested_ref_cannot_leave_the_branch_namespace() {
        for accepted in ["main", "agent/work", "release/v0.3.1", "a.b-c_d", "x/y/z"] {
            assert!(valid_ref(accepted).is_ok(), "{accepted} was refused");
        }
        for rejected in [
            "",
            "/main",
            "main/",
            "main.",
            "../main",
            "a..b",
            "a//b",
            "main@{1}",
            "heads/main?x=1",
            "main#frag",
            "main%2f..%2ftags",
            "main\\..\\tags",
            "main branch",
            "réf",
            "main\u{7f}",
        ] {
            assert_eq!(
                valid_ref(rejected).err(),
                Some(OperationError::InvalidInput),
                "{rejected:?} was accepted"
            );
        }
        // 240 bytes is the boundary, not a suggestion.
        assert!(valid_ref(&"a".repeat(240)).is_ok());
        assert_eq!(
            valid_ref(&"a".repeat(241)).err(),
            Some(OperationError::InvalidInput)
        );
    }

    /// Reading is not writing: the paths `publish_commit` refuses because an
    /// agent must not rewrite the CI that judges it are readable, and refusing
    /// to read them would leave an agent unable to see the gate it must pass.
    #[test]
    fn a_readable_path_is_shape_checked_but_not_authority_checked() {
        // Driven through `ObserveFile::validate`, not through a validator by
        // name. This test previously asserted on `valid_path`, which stopped
        // being the read path when reads widened -- so swapping the call site
        // back to the write rule, which is the regression this test is named
        // after, passed the whole gate.
        let observe = |path: &str| {
            ObserveFile {
                repository: "dark-factory-build/dark-factory".into(),
                commit_sha: "a".repeat(40),
                path: path.into(),
            }
            .validate()
        };
        for readable in [
            ".github/workflows/ci.yml",
            ".github/CODEOWNERS",
            "CODEOWNERS",
            "control-plane/src/github_app.rs",
        ] {
            assert!(observe(readable).is_ok(), "{readable} is unreadable");
            assert!(valid_path(readable).is_ok(), "{readable} was refused");
            // The same paths stay unwritable.
            if readable != "control-plane/src/github_app.rs" {
                assert_eq!(
                    valid_repository_path(readable).err(),
                    Some(OperationError::InvalidInput),
                    "{readable} became writable"
                );
            }
        }
        for rejected in [
            "",
            "/a",
            "a/",
            "a//b",
            "a/../b",
            "a/./b",
            ".git/config",
            "a\\b",
        ] {
            assert_eq!(
                valid_path(rejected).err(),
                Some(OperationError::InvalidInput),
                "{rejected:?} was accepted"
            );
        }
        assert_eq!(
            valid_path("a\u{0}b").err(),
            Some(OperationError::InvalidInput)
        );
        // Reads reach further than writes: `observe_tree` can name a path
        // `publish_commit` will not accept, and refusing to read it would make
        // the two operations disagree about which files exist.
        let long = format!("vendor/{}/file.txt", "a".repeat(300));
        assert!(observe(&long).is_ok());
        assert_eq!(
            valid_repository_path(&long).err(),
            Some(OperationError::InvalidInput)
        );
        for rejected in ["", "/a", "a/../b", ".git/config"] {
            assert_eq!(
                observe(rejected).err(),
                Some(OperationError::InvalidInput),
                "{rejected:?} was readable"
            );
        }
    }

    /// The two decisions `observe_tree` makes, tested for real rather than
    /// pinned by a grep on the source. A source pin could not see `.take(1)`
    /// dropping a merge commit's second parent, and broke on refactors that
    /// changed nothing.
    #[test]
    fn a_tree_observation_reports_every_parent_and_refuses_a_partial_listing() {
        let commit = |parents: &[&str]| -> GitCommit {
            serde_json::from_str(&format!(
                r#"{{"message":"m","tree":{{"sha":"{}"}},"parents":[{}]}}"#,
                "c".repeat(40),
                parents
                    .iter()
                    .map(|sha| format!(r#"{{"sha":"{sha}"}}"#))
                    .collect::<Vec<_>>()
                    .join(",")
            ))
            .unwrap()
        };
        let tree = |truncated: bool| -> GitTree {
            serde_json::from_str(&format!(
                r#"{{"truncated":{truncated},"tree":[{{"path":"a.txt","mode":"100644","type":"blob","sha":"{}"}},{{"path":"bin/x","mode":"100755","type":"blob","sha":"{}"}}]}}"#,
                "d".repeat(40),
                "e".repeat(40)
            ))
            .unwrap()
        };

        // A merge commit has two parents, and dropping the second makes
        // ancestry unwalkable exactly where branches meet.
        let merge = tree_observation(
            "a".repeat(40),
            commit(&["b".repeat(40).as_str(), &"f".repeat(40)]),
            tree(false),
        )
        .unwrap();
        assert_eq!(merge.parents, vec!["b".repeat(40), "f".repeat(40)]);
        assert_eq!(merge.commit_sha, "a".repeat(40));
        assert_eq!(merge.tree_sha, "c".repeat(40));
        // Entries are reported as git records them, modes included.
        assert_eq!(
            merge
                .entries
                .iter()
                .map(|entry| (
                    entry.path.as_str(),
                    entry.kind.as_str(),
                    entry.mode.as_str()
                ))
                .collect::<Vec<_>>(),
            vec![("a.txt", "blob", "100644"), ("bin/x", "blob", "100755")]
        );
        // A root commit has none, which is an answer rather than a failure.
        assert!(
            tree_observation("a".repeat(40), commit(&[]), tree(false))
                .unwrap()
                .parents
                .is_empty()
        );
        // A partial listing must never be returned as if it were complete.
        assert_eq!(
            tree_observation("a".repeat(40), commit(&[]), tree(true)).err(),
            Some(OperationError::Refused(RefusalReason::TreeTruncated))
        );
    }

    #[test]
    fn a_workflow_path_names_a_workflow_file() {
        for accepted in [
            ".github/workflows/ci.yml",
            ".github/workflows/deploy-control-plane.yml",
            ".github/workflows/release.yaml",
        ] {
            assert!(valid_workflow_path(accepted).is_ok(), "{accepted}");
        }
        for rejected in [
            "ci.yml",
            ".github/workflows/",
            ".github/workflows/ci.txt",
            ".github/workflows/nested/ci.yml",
            ".github/workflows/../../etc/passwd.yml",
            "/.github/workflows/ci.yml",
        ] {
            assert_eq!(
                valid_workflow_path(rejected).err(),
                Some(OperationError::InvalidInput),
                "{rejected} was accepted"
            );
        }
    }

    #[test]
    fn exact_repository_path_and_operation_installation_are_required() {
        let repository = RepositoryName::new("dark-factory-build/dark-factory".into()).unwrap();
        assert_eq!(
            repository.installation_url(),
            "https://api.github.com/repos/dark-factory-build/dark-factory/installation"
        );
        assert!(RepositoryName::new("baziyer/../dark-factory".into()).is_err());
        assert!(RepositoryName::new("baziyer/dark factory".into()).is_err());
        let installation: Installation = serde_json::from_str(
            r#"{"id":17,"app_id":4673420,"account":{"id":109233175},"repository_selection":"selected","permissions":{"actions":"write","administration":"write","checks":"read","contents":"write","issues":"write","merge_queues":"write","metadata":"read","pull_requests":"write"},"events":[],"suspended_at":null}"#,
        )
        .unwrap();
        assert!(validate_installation(&installation, 4_673_420).is_ok());

        let broader: Installation = serde_json::from_str(
            r#"{"id":17,"app_id":4673420,"account":{"id":109233175},"repository_selection":"selected","permissions":{"actions":"write","administration":"write","checks":"read","contents":"write","issues":"write","merge_queues":"write","metadata":"read","pull_requests":"write"},"events":[],"suspended_at":null}"#,
        )
        .unwrap();
        assert!(validate_installation(&broader, 4_673_420).is_ok());

        let no_administration: Installation = serde_json::from_str(
            r#"{"id":17,"app_id":4673420,"account":{"id":109233175},"repository_selection":"selected","permissions":{"actions":"write","checks":"read","contents":"write","issues":"write","merge_queues":"write","metadata":"read","pull_requests":"write"},"events":[],"suspended_at":null}"#,
        )
        .unwrap();
        assert_eq!(
            validate_installation(&no_administration, 4_673_420).err(),
            Some("administration")
        );

        let with_permissions = |permissions: &str| -> Installation {
            serde_json::from_str(&format!(
                r#"{{"id":17,"app_id":4673420,"account":{{"id":109233175}},"repository_selection":"selected","permissions":{{{permissions}}},"events":[],"suspended_at":null}}"#
            ))
            .unwrap()
        };
        // Withhold exactly one grant per case. The guards are checked in order,
        // so a fixture missing several only ever proves the first -- which is
        // how the previous case here proved `actions` while reading as a
        // `pull_requests` test.
        for (permissions, expected) in [
            (
                r#""actions":"write","administration":"write","checks":"read","contents":"write","issues":"write","merge_queues":"write","metadata":"read","pull_requests":"read""#,
                "pull_requests",
            ),
            (
                r#""actions":"write","administration":"write","contents":"write","issues":"write","merge_queues":"write","metadata":"read","pull_requests":"write""#,
                "checks",
            ),
            (
                r#""actions":"write","administration":"write","checks":"read","contents":"write","issues":"write","merge_queues":"write","pull_requests":"write""#,
                "metadata",
            ),
        ] {
            assert_eq!(
                validate_installation(&with_permissions(permissions), 4_673_420).err(),
                Some(expected)
            );
        }

        // `publish_commit` mints `contents: write`. Readiness no longer sees
        // any installation, so this boundary is the only thing between a
        // read-only installation and an opaque failure at token mint -- which
        // is why the refusal now names the field rather than collapsing.
        let read_only: Installation = serde_json::from_str(
            r#"{"id":17,"app_id":4673420,"account":{"id":109233175},"repository_selection":"selected","permissions":{"actions":"write","administration":"write","checks":"read","contents":"read","issues":"write","merge_queues":"write","metadata":"read","pull_requests":"write"},"events":[],"suspended_at":null}"#,
        )
        .unwrap();
        assert_eq!(
            validate_installation(&read_only, 4_673_420).err(),
            Some("contents")
        );

        // The "ok" fixtures above already listed `merge_queues`, so they would
        // pass whether or not the boundary required it -- they read as
        // coverage while asserting nothing. This is the case that proves it:
        // everything else granted, Merge queues absent. Without it an
        // installation that cannot enqueue fails every `enqueue_pull_request`
        // at token mint with nothing naming the reason.
        let no_queue: Installation = serde_json::from_str(
            r#"{"id":17,"app_id":4673420,"account":{"id":109233175},"repository_selection":"selected","permissions":{"actions":"write","administration":"write","checks":"read","contents":"write","issues":"write","metadata":"read","pull_requests":"write"},"events":[],"suspended_at":null}"#,
        )
        .unwrap();
        assert_eq!(
            validate_installation(&no_queue, 4_673_420).err(),
            Some("merge_queues")
        );

        // Read is not enough: enqueueing writes to the queue.
        let queue_read_only: Installation = serde_json::from_str(
            r#"{"id":17,"app_id":4673420,"account":{"id":109233175},"repository_selection":"selected","permissions":{"actions":"write","administration":"write","checks":"read","contents":"write","issues":"write","merge_queues":"read","metadata":"read","pull_requests":"write"},"events":[],"suspended_at":null}"#,
        )
        .unwrap();
        assert_eq!(
            validate_installation(&queue_read_only, 4_673_420).err(),
            Some("merge_queues")
        );

        let no_issues: Installation = serde_json::from_str(
            r#"{"id":17,"app_id":4673420,"account":{"id":109233175},"repository_selection":"selected","permissions":{"actions":"write","administration":"write","checks":"read","contents":"write","merge_queues":"write","metadata":"read","pull_requests":"write"},"events":[],"suspended_at":null}"#,
        )
        .unwrap();
        assert_eq!(
            validate_installation(&no_issues, 4_673_420).err(),
            Some("issues")
        );

        let no_actions: Installation = serde_json::from_str(
            r#"{"id":17,"app_id":4673420,"account":{"id":109233175},"repository_selection":"selected","permissions":{"administration":"write","checks":"read","contents":"write","issues":"write","merge_queues":"write","metadata":"read","pull_requests":"write"},"events":[],"suspended_at":null}"#,
        )
        .unwrap();
        assert_eq!(
            validate_installation(&no_actions, 4_673_420).err(),
            Some("actions")
        );
    }

    #[test]
    fn operation_tokens_request_administration_only_for_direct_merge() {
        let source = include_str!("github_app.rs");
        let needle = ["(", "\"administration\"", ", ", "\"write\"", ")"].concat();
        assert_eq!(source.matches(&needle).count(), 1);
    }

    #[test]
    fn publication_paths_are_policy_checked() {
        for allowed in [
            "README.md",
            "src/lib.rs",
            "docs/development/WORKFLOW.md",
            "a.b_c-d/e.rs",
            // An ordinary file beside the refused ones stays publishable.
            ".github/ISSUE_TEMPLATE/bug.yml",
            ".github/PULL_REQUEST_TEMPLATE.md",
            ".githubbed/notes.md",
            "docs/codeowners-guidance.md",
            // The protected names are refused segment-exactly, not as
            // substrings: a path that merely carries one stays publishable,
            // including one where the name is a prefix of a longer segment.
            "CODEOWNERS.md",
            "docs/notes-on-CODEOWNERS.md",
            "src/codeowners_test.rs",
            ".github/dependabot.yml.example",
            // The collision is with the leading segments only. GitHub reads
            // CODEOWNERS from the root, `.github/`, and `docs/` and nowhere
            // else, so the name as a whole segment elsewhere carries no
            // authority and stays publishable. A refactor to "any segment
            // equals a protected name" would over-refuse these.
            "src/CODEOWNERS",
            "a/CODEOWNERS/b",
            ".github/sub/dependabot.yml",
        ] {
            assert!(
                valid_repository_path(allowed).is_ok(),
                "rejected: {allowed}"
            );
        }
        // Workflow files are refused explicitly. GitHub also blocks them without
        // the `workflows` permission this App does not hold, but an agent able
        // to rewrite the CI gating its own work would be escalating authority,
        // so the surface states the boundary rather than inheriting it.
        for refused in [
            ".github/workflows/ci.yml",
            // A tree entry may name a directory as a blob, which replaces the
            // whole subtree. A prefix test needing the trailing slash let
            // `.github/workflows` delete every workflow, and `.github` delete
            // CODEOWNERS and the issue templates, which no permission on this
            // installation protects.
            ".github/workflows",
            ".github",
            // An agent that can rewrite CODEOWNERS can remove itself from
            // required review. Refusing only the tree left that reachable one
            // file at a time.
            "CODEOWNERS",
            ".github/CODEOWNERS",
            "docs/CODEOWNERS",
            ".github/dependabot.yml",
            ".github/dependabot.yaml",
            // Publishing under a protected name turns its blob into a tree,
            // which deletes the file's content -- the same write, reached by
            // making the name a directory. Every by-name refusal is refused
            // as a leading directory too.
            "CODEOWNERS/x",
            ".github/CODEOWNERS/x",
            "docs/CODEOWNERS/x",
            "CODEOWNERS/nested/deep.md",
            ".github/dependabot.yml/x",
            ".github/dependabot.yaml/x",
            ".GitHub/Workflows/ci.yml",
            ".github/workflows/nested/deep.yml",
            "",
            "/etc/passwd",
            "src/",
            "src//lib.rs",
            "../secrets",
            "src/../../etc/passwd",
            ".git/config",
            "src/a\\b.rs",
        ] {
            assert!(
                valid_repository_path(refused).is_err(),
                "accepted: {refused}"
            );
        }
    }

    /// The table that decides whether "the operation did not run" may be
    /// asserted. A class wrongly listed as rejected-before-execution reports
    /// "nothing changed" for something that may have changed.
    #[test]
    fn only_pre_execution_error_classes_are_rejections() {
        let errors = |kinds: &[Option<&str>]| {
            kinds
                .iter()
                .map(|kind| GraphQlError {
                    kind: kind.map(Into::into),
                    path: vec!["enqueuePullRequest".into()],
                })
                .collect::<Vec<_>>()
        };
        let at_path = |kind: &str, path: &[&str]| {
            vec![GraphQlError {
                kind: Some(kind.into()),
                path: path.iter().map(|part| (*part).into()).collect(),
            }]
        };

        assert!(classify_graphql_errors(&[]).is_none());

        for kind in ["NOT_FOUND", "FORBIDDEN", "UNPROCESSABLE", "RATE_LIMITED"] {
            match classify_graphql_errors(&errors(&[Some(kind)])) {
                Some(GraphQlFailure::Rejected(kinds)) => {
                    // The class is recorded, not merely accepted: a refusal
                    // that cannot say which class it was is the untyped
                    // refusal this table exists to prevent (#371).
                    assert_eq!(kinds.to_string(), kind);
                }
                other => panic!("{kind} is rejected before execution, got {other:?}"),
            }
        }

        // An untyped error is GitHub's shape for `Something went wrong while
        // executing your query`, which its docs say may be a timeout -- and a
        // request that timed out server-side may have executed.
        assert!(matches!(
            classify_graphql_errors(&errors(&[None])),
            Some(GraphQlFailure::Unknown)
        ));
        assert!(matches!(
            classify_graphql_errors(&errors(&[Some("SERVICE_UNAVAILABLE")])),
            Some(GraphQlFailure::Unknown)
        ));
        // One unknown class among rejections makes the whole answer unknown:
        // the array describes one response, and any part of it may have run.
        assert!(matches!(
            classify_graphql_errors(&errors(&[Some("FORBIDDEN"), None])),
            Some(GraphQlFailure::Unknown)
        ));

        // A typed error raised while RESOLVING a field is post-execution by
        // construction, whatever its type says. GitHub raises `FORBIDDEN` this
        // way for permission scoping on installation tokens, and because
        // `mergeQueueEntry.id` and `.state` are non-null it would null the
        // entry -- making a landed enqueue look absent and be reported as
        // "nothing changed".
        assert!(matches!(
            classify_graphql_errors(&at_path(
                "FORBIDDEN",
                &["enqueuePullRequest", "mergeQueueEntry", "id"]
            )),
            Some(GraphQlFailure::Unknown)
        ));
        assert!(matches!(
            classify_graphql_errors(&at_path(
                "NOT_FOUND",
                &["enqueuePullRequest", "mergeQueueEntry", "state"]
            )),
            Some(GraphQlFailure::Unknown)
        ));
        // A root-level error keeps its type: this is the shape GitHub
        // documents for a SAML-protected resource, and it is a genuine
        // pre-execution rejection.
        assert!(matches!(
            classify_graphql_errors(&at_path("FORBIDDEN", &["enqueuePullRequest"])),
            Some(GraphQlFailure::Rejected(_))
        ));
        // An error with no path at all is request-level.
        assert!(matches!(
            classify_graphql_errors(&at_path("RATE_LIMITED", &[])),
            Some(GraphQlFailure::Rejected(_))
        ));
    }

    /// GitHub's ordinary refusal shape for a mutation is a populated `data`
    /// whose mutation field is null, *beside* an errors array. Deciding from
    /// the envelope rather than the payload answers "nothing changed" for an
    /// enqueue that may have landed -- the defect this function exists to make
    /// impossible to reintroduce silently.
    #[test]
    fn an_enqueue_outcome_is_decided_from_the_payload_not_the_envelope() {
        let entry = || QueueEntry {
            id: "MQE_kwDOabc".into(),
            state: "QUEUED".into(),
            pull_request: Some(QueueEntryPullRequest {
                number: 329,
                head_ref_oid: "d".repeat(40),
            }),
        };

        let forbidden = RejectionKinds {
            forbidden: true,
            ..RejectionKinds::default()
        };

        // An entry came back. That is the effect, whatever rode alongside it.
        assert!(enqueue_outcome(Some(entry()), None).is_ok());
        assert!(enqueue_outcome(Some(entry()), Some(GraphQlFailure::Rejected(forbidden))).is_ok());
        assert!(enqueue_outcome(Some(entry()), Some(GraphQlFailure::Unknown)).is_ok());

        // No entry, rejected before execution: determinate, the same
        // operation ID stays retryable, and the refusal carries the classes
        // the classification recorded rather than a fresh guess.
        assert!(matches!(
            enqueue_outcome(None, Some(GraphQlFailure::Rejected(forbidden))),
            Err(OperationError::Refused(RefusalReason::Rejected(kinds))) if kinds == forbidden
        ));
        // No entry, and an error class that may follow a server-side timeout
        // on work already under way. Reporting "nothing changed" here is the
        // one answer that can be actively false.
        assert!(matches!(
            enqueue_outcome(None, Some(GraphQlFailure::Unknown)),
            Err(OperationError::Indeterminate)
        ));
        // No entry and no error at all is its own reason: nothing was
        // rejected, GitHub simply answered with no effect.
        assert!(matches!(
            enqueue_outcome(None, None),
            Err(OperationError::Refused(RefusalReason::NoEffect))
        ));
    }

    /// The reason must survive to the caller-visible rendering: each refusal
    /// names itself distinctly, and a multi-class rejection carries every
    /// class rather than whichever arrived first (#371).
    #[test]
    fn a_refusal_renders_its_reason() {
        let mixed = vec![
            GraphQlError {
                kind: Some("NOT_FOUND".into()),
                path: vec!["enqueuePullRequest".into()],
            },
            GraphQlError {
                kind: Some("RATE_LIMITED".into()),
                path: vec![],
            },
        ];
        match classify_graphql_errors(&mixed) {
            Some(GraphQlFailure::Rejected(kinds)) => {
                assert_eq!(kinds.to_string(), "NOT_FOUND+RATE_LIMITED");
            }
            other => panic!("a mixed typed rejection stays rejected, got {other:?}"),
        }

        let rejected = enqueue_outcome(None, classify_graphql_errors(&mixed))
            .err()
            .unwrap();
        assert_eq!(
            rejected.to_string(),
            "the request was refused: \
             rejected before execution as NOT_FOUND+RATE_LIMITED"
        );
        assert_eq!(
            enqueue_outcome(None, None).err().unwrap().to_string(),
            "the request was refused: \
             answered with neither an effect nor an error"
        );
        assert_eq!(
            OperationError::Refused(RefusalReason::NoMergeQueue).to_string(),
            "the request was refused: \
             the queue read found no merge queue on the base branch"
        );
        // The empty set is unreachable from classification but constructible
        // through the derived `Default`; its rendering must say so instead
        // of ending the sentence with nothing.
        assert_eq!(RejectionKinds::default().to_string(), "no recorded class");
    }

    /// The queue entry's own `headCommit` is the queue's synthetic merge
    /// commit, not the pull request head. Binding to the wrong one would make
    /// every reconciliation answer "not queued" for something that is, and the
    /// operation would enqueue a second time.
    #[test]
    fn a_queue_entry_is_bound_to_the_pull_request_head_not_the_queue_commit() {
        let head = "d".repeat(40);
        let mut request = EnqueuePullRequest {
            repository: "dark-factory-build/dark-factory".into(),
            operation_id: "4c8a5c44-7f1f-11f0-952e-acde48001122".into(),
            pull_number: 329,
            head_sha: head.clone(),
            base: "main".into(),
        };
        assert!(request.validate().is_ok());

        let entry = |number: i64, oid: &str| QueueEntry {
            id: "MQE_kwDOabc".into(),
            state: "QUEUED".into(),
            pull_request: Some(QueueEntryPullRequest {
                number,
                head_ref_oid: oid.into(),
            }),
        };

        assert!(entry(329, &head).matches(&request));
        // A different pull request's entry is not this operation's effect.
        assert!(!entry(330, &head).matches(&request));
        // Nor is this pull request queued at a head the caller did not state:
        // GitHub ejects an entry when the head moves, so a surviving entry
        // naming another head means the queue holds something else.
        assert!(!entry(329, &"e".repeat(40)).matches(&request));
        // An entry GitHub returned without a pull request tells us nothing.
        assert!(
            !QueueEntry {
                id: "MQE_kwDOabc".into(),
                state: "QUEUED".into(),
                pull_request: None,
            }
            .matches(&request)
        );

        let result = entry(329, &head).into_result(&request).unwrap();
        assert_eq!(result.pull_number, 329);
        assert_eq!(result.head_sha, head);
        assert_eq!(result.entry_id, "MQE_kwDOabc");
        assert_eq!(result.state_when_recorded, "QUEUED");

        // An entry already unmergeable is reported as such rather than as a
        // plain "queued": the caller would otherwise wait for a merge that is
        // never coming.
        let unmergeable = QueueEntry {
            state: "UNMERGEABLE".into(),
            ..entry(329, &head)
        };
        assert_eq!(
            unmergeable
                .into_result(&request)
                .unwrap()
                .state_when_recorded,
            "UNMERGEABLE"
        );

        // A state this build does not know means the schema moved under us;
        // guessing would report a merge state it does not understand.
        assert!(matches!(
            QueueEntry {
                state: "TELEPORTED".into(),
                ..entry(329, &head)
            }
            .into_result(&request),
            Err(OperationError::Indeterminate)
        ));

        // Converting a non-matching entry must never manufacture a result.
        assert!(matches!(
            entry(330, &head).into_result(&request),
            Err(OperationError::Indeterminate)
        ));

        // The base is bound too: a pull request that targets another branch
        // would otherwise be enqueued onto that branch's queue.
        assert!(
            EnqueuePullRequest {
                repository: "dark-factory-build/dark-factory".into(),
                base: "../etc".into(),
                ..request.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            EnqueuePullRequest {
                repository: "dark-factory-build/dark-factory".into(),
                head_sha: "D".repeat(40),
                ..request.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            EnqueuePullRequest {
                repository: "dark-factory-build/dark-factory".into(),
                pull_number: 0,
                ..request
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn a_published_commit_is_bounded_and_unambiguous() {
        let base = |changes: Vec<FileChange>| PublishCommit {
            repository: "dark-factory-build/dark-factory".into(),
            operation_id: "11111111-2222-3333-4444-555555555555".into(),
            branch: "agent/work".into(),
            expected_head_sha: "a".repeat(40),
            message: "Do the thing".into(),
            changes,
        };
        let file = |path: &str| FileChange {
            path: path.into(),
            content_base64: Some(general_purpose::STANDARD.encode(b"hello")),
            mode: None,
        };
        assert!(base(vec![file("README.md")]).validate().is_ok());
        // The authority refusal has to be reached THROUGH `validate`, not only
        // tested as a function. `valid_path` is now an identical-signature,
        // authority-free twin, so swapping the call at the one write site is a
        // single-identifier slip that the rest of the suite cannot see -- and
        // CODEOWNERS and `dependabot.yml` have no GitHub permission backstopping
        // them, so that slip is the escalation this whole boundary exists to
        // stop.
        for protected in [
            ".github/workflows/ci.yml",
            ".github",
            "CODEOWNERS",
            ".github/CODEOWNERS",
            "docs/CODEOWNERS",
            ".github/dependabot.yml",
            ".github/dependabot.yaml",
        ] {
            assert_eq!(
                base(vec![file(protected)]).validate().err(),
                Some(OperationError::InvalidInput),
                "{protected} is publishable"
            );
        }
        // A mode with no content is the natural way to write "make this
        // executable" -- and a change with no content is a deletion, so that
        // request would have deleted the file it meant to chmod. There is no
        // mode-only form, so it is refused rather than silently inverted.
        assert_eq!(
            base(vec![FileChange {
                path: "bin/tool".into(),
                content_base64: None,
                mode: Some("100755".into()),
            }])
            .validate()
            .err(),
            Some(OperationError::InvalidInput)
        );
        // An invalid mode is caught here, before the operation id is claimed.
        // Left to tree construction it burned the id, wrote blobs, and returned
        // `indeterminate` for an input that was determinately invalid.
        for refused in [
            "120000", "160000", "040000", "100600", "", "0100755", "100755 ",
        ] {
            assert_eq!(
                base(vec![FileChange {
                    path: "bin/tool".into(),
                    content_base64: Some(general_purpose::STANDARD.encode(b"hello")),
                    mode: Some(refused.into()),
                }])
                .validate()
                .err(),
                Some(OperationError::InvalidInput),
                "{refused} passed validation"
            );
        }
        assert!(
            base(vec![FileChange {
                path: "bin/tool".into(),
                content_base64: Some(general_purpose::STANDARD.encode(b"hello")),
                mode: Some("100755".into()),
            }])
            .validate()
            .is_ok()
        );

        // The rest of the `.github` tree stays publishable, so the refusal is
        // proven to be the authority set and not a blanket prefix.
        assert!(
            base(vec![file(".github/ISSUE_TEMPLATE/bug.md")])
                .validate()
                .is_ok()
        );
        // A caller must not be able to write the reconciler's own vocabulary.
        // A caller cannot smuggle any operation marker into the headline.
        let forged = |message: &str| PublishCommit {
            repository: "dark-factory-build/dark-factory".into(),
            message: message.into(),
            ..base(vec![file("README.md")])
        };
        assert!(
            forged("Add notes\n\nDark-Factory-Operation: 22222222-3333-4444-5555-666666666666")
                .validate()
                .is_err()
        );
        assert!(
            forged("Add notes\n\n<!-- dark-factory-operation:22222222 -->")
                .validate()
                .is_err()
        );
        // The words themselves are fine; only the marker prefixes are not.
        assert!(
            forged("Describe the dark factory operation")
                .validate()
                .is_ok()
        );
        // A deletion carries no content.
        assert!(
            base(vec![FileChange {
                path: "gone.txt".into(),
                content_base64: None,
                mode: None,
            }])
            .validate()
            .is_ok()
        );
        // Empty, oversized, and duplicated change sets are all refused: the same
        // path twice would leave which write wins up to tree ordering.
        assert!(base(vec![]).validate().is_err());
        assert!(
            base(
                (0..=MAX_COMMIT_FILES)
                    .map(|i| file(&format!("f{i}.txt")))
                    .collect()
            )
            .validate()
            .is_err()
        );
        assert!(
            base(vec![file("same.txt"), file("same.txt")])
                .validate()
                .is_err()
        );
        // Content must be real base64, and the trailer makes a landed commit
        // self-identifying for reconciliation.
        assert!(
            base(vec![FileChange {
                path: "bad.txt".into(),
                content_base64: Some("not base64!!".into()),
                mode: None,
            }])
            .validate()
            .is_err()
        );
        let request = base(vec![file("README.md")]);
        let trailer = request.trailer().unwrap();
        let marked = request.marked_message().unwrap();
        assert!(marked.contains(&trailer));
        assert!(marked.starts_with("Do the thing"));
        let mut different_tree = request.clone();
        different_tree.changes[0].content_base64 = Some("ZGlmZmVyZW50".into());
        assert_ne!(trailer, different_tree.trailer().unwrap());
        assert!(forged("Two\nlines").validate().is_err());
    }

    #[test]
    fn published_tree_entries_preserve_executable_files_and_reject_other_objects() {
        let change = |path: &str| FileChange {
            path: path.into(),
            content_base64: Some("aGVsbG8=".into()),
            mode: None,
        };
        let base_tree = vec![
            GitTreeEntry {
                path: "bin/tool".into(),
                mode: "100755".into(),
                kind: "blob".into(),
                sha: "b".repeat(40),
            },
            GitTreeEntry {
                path: "notes.txt".into(),
                mode: "100644".into(),
                kind: "blob".into(),
                sha: "b".repeat(40),
            },
            GitTreeEntry {
                path: "directory".into(),
                mode: "040000".into(),
                kind: "tree".into(),
                sha: "b".repeat(40),
            },
            GitTreeEntry {
                path: "nested/tool".into(),
                mode: "100755".into(),
                kind: "blob".into(),
                sha: "b".repeat(40),
            },
            GitTreeEntry {
                path: "file-parent".into(),
                mode: "100644".into(),
                kind: "blob".into(),
                sha: "b".repeat(40),
            },
            GitTreeEntry {
                path: "symlink-parent".into(),
                mode: "120000".into(),
                kind: "blob".into(),
                sha: "b".repeat(40),
            },
            GitTreeEntry {
                path: "submodule-parent".into(),
                mode: "160000".into(),
                kind: "commit".into(),
                sha: "b".repeat(40),
            },
            GitTreeEntry {
                path: "link".into(),
                mode: "120000".into(),
                kind: "blob".into(),
                sha: "b".repeat(40),
            },
        ];

        let changes = vec![
            change("bin/tool"),
            change("notes.txt"),
            change("nested/tool"),
            change("new.txt"),
            change("new/directory/file.txt"),
        ];
        let tree_request = publish_tree::build_request(
            "base-tree",
            &changes,
            &base_tree,
            &[Some("a".repeat(40)), None, Some("b".repeat(40)), None, None],
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(tree_request).unwrap(),
            serde_json::json!({
                "base_tree": "base-tree",
                "tree": [
                    {"path": "bin/tool", "mode": "100755", "type": "blob", "sha": "a".repeat(40)},
                    {"path": "notes.txt", "mode": "100644", "type": "blob", "sha": null},
                    {"path": "nested/tool", "mode": "100755", "type": "blob", "sha": "b".repeat(40)},
                    {"path": "new.txt", "mode": "100644", "type": "blob", "sha": null},
                    {"path": "new/directory/file.txt", "mode": "100644", "type": "blob", "sha": null}
                ]
            })
        );
        // A stated mode is what lets an integrating caller reproduce an
        // executable file it saw through `observe_tree`. Without it a new
        // script lands at 100644 and fails much later as "permission denied".
        let executable = |path: &str| FileChange {
            path: path.into(),
            content_base64: Some("aGVsbG8=".into()),
            mode: Some("100755".into()),
        };
        let stated = publish_tree::build_request(
            "base-tree",
            &[executable("new/gate.sh"), change("bin/tool")],
            &base_tree,
            &[None, None],
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(stated).unwrap(),
            serde_json::json!({
                "base_tree": "base-tree",
                "tree": [
                    {"path": "new/gate.sh", "mode": "100755", "type": "blob", "sha": null},
                    // Unstated still inherits, so the old behaviour is intact.
                    {"path": "bin/tool", "mode": "100755", "type": "blob", "sha": null}
                ]
            })
        );
        // A mode outside the two regular-file modes is refused rather than
        // passed through: a symlink or gitlink is not a file this surface
        // writes, and accepting the string would let a caller create one.
        for refused in ["120000", "160000", "040000", "100600", "", "0100755"] {
            assert!(
                publish_tree::build_request(
                    "base-tree",
                    &[FileChange {
                        path: "new/thing".into(),
                        content_base64: Some("aGVsbG8=".into()),
                        mode: Some(refused.into()),
                    }],
                    &base_tree,
                    &[None],
                )
                .is_err(),
                "{refused} was accepted as a mode"
            );
        }
        // Replacing a tree or a symlink with a blob would silently destroy
        // repository structure, so publication fails closed.
        assert!(
            publish_tree::build_request("base-tree", &[change("directory")], &base_tree, &[None])
                .is_err()
        );
        assert!(
            publish_tree::build_request("base-tree", &[change("link")], &base_tree, &[None])
                .is_err()
        );
        // A non-tree ancestor would otherwise let GitHub replace that object
        // with a directory and silently destroy its contents.
        for path in [
            "file-parent/child.txt",
            "symlink-parent/child.txt",
            "submodule-parent/child.txt",
        ] {
            assert!(
                publish_tree::build_request("base-tree", &[change(path)], &base_tree, &[None])
                    .is_err()
            );
        }
        assert!(publish_tree::build_request("base-tree", &changes, &base_tree, &[None]).is_err());
    }

    #[test]
    fn rest_refusals_and_streaming_log_tails_are_bounded() {
        assert_eq!(
            rejection_for_status(403),
            Some(RejectionKinds {
                forbidden: true,
                ..RejectionKinds::default()
            })
        );
        assert_eq!(
            rejection_for_status(422),
            Some(RejectionKinds {
                unprocessable: true,
                ..RejectionKinds::default()
            })
        );
        assert_eq!(rejection_for_status(500), None);

        let mut tail = b"first".to_vec();
        let mut middle = b"-middle".to_vec();
        retain_tail(&mut tail, &mut middle, 8);
        assert_eq!(tail, b"t-middle");
        let mut oversized = b"0123456789".to_vec();
        retain_tail(&mut tail, &mut oversized, 8);
        assert_eq!(tail, b"23456789");
    }

    fn complete_release(tag: &str) -> Release {
        let names = [
            format!("dark-factory-{tag}-aarch64-apple-darwin.tar.gz"),
            format!("dark-factory-{tag}-x86_64-apple-darwin.tar.gz"),
            "SHA256SUMS".into(),
            "latest.json".into(),
            "dark-factory.rb".into(),
        ];
        Release {
            tag_name: tag.into(),
            html_url: format!(
                "https://github.com/dark-factory-build/dark-factory/releases/tag/{tag}"
            ),
            draft: false,
            prerelease: tag.contains('-'),
            assets: names
                .into_iter()
                .map(|name| ReleaseAsset {
                    browser_download_url: format!(
                        "https://github.com/dark-factory-build/dark-factory/releases/download/{tag}/{name}"
                    ),
                    name,
                    size: 1,
                    digest: Some(format!("sha256:{}", "a".repeat(64))),
                    state: "uploaded".into(),
                })
                .collect(),
        }
    }

    #[test]
    fn a_release_is_complete_exact_and_digest_bound() {
        let result = complete_release("v1.2.3")
            .into_result("v1.2.3")
            .unwrap()
            .unwrap();
        assert!(!result.draft);
        assert!(!result.prerelease);
        assert_eq!(result.assets.len(), 5);
        assert!(result.assets.iter().all(|asset| asset.digest.len() == 71));

        let mut missing_digest = complete_release("v1.2.3");
        missing_digest.assets[0].digest = None;
        assert!(matches!(
            missing_digest.into_result("v1.2.3"),
            Err(OperationError::Indeterminate)
        ));
        let mut partial = complete_release("v1.2.3");
        partial.assets.pop();
        assert!(matches!(
            partial.into_result("v1.2.3"),
            Err(OperationError::Indeterminate)
        ));
        let mut unexpected = complete_release("v1.2.3");
        unexpected.assets[0].name = "surprise.tar.gz".into();
        assert!(matches!(
            unexpected.into_result("v1.2.3"),
            Err(OperationError::Indeterminate)
        ));
        let mut uploading = complete_release("v1.2.3");
        uploading.assets[0].state = "new".into();
        assert!(matches!(
            uploading.into_result("v1.2.3"),
            Err(OperationError::Indeterminate)
        ));
        let mut draft = complete_release("v1.2.3");
        draft.draft = true;
        assert!(matches!(
            draft.into_result("v1.2.3"),
            Err(OperationError::Indeterminate)
        ));
    }

    fn release_run(id: i64, event: &str, head_sha: &str, tag: &str) -> WorkflowRun {
        WorkflowRun {
            id,
            run_attempt: 1,
            name: "Release".into(),
            path: RELEASE_WORKFLOW.response_path.into(),
            event: event.into(),
            status: "completed".into(),
            conclusion: Some("success".into()),
            head_sha: head_sha.into(),
            head_branch: tag.into(),
            display_title: Some(format!("Release {tag}")),
            html_url: format!(
                "https://github.com/dark-factory-build/dark-factory/actions/runs/{id}"
            ),
            pull_requests: Vec::new(),
        }
    }

    #[test]
    fn fixed_workflow_urls_use_api_ids_and_keep_full_response_paths() {
        let owner = "dark-factory-build";
        let repository = "dark-factory";
        assert_eq!(
            workflow_api_url(owner, repository, RELEASE_WORKFLOW, "runs"),
            "https://api.github.com/repos/dark-factory-build/dark-factory/actions/workflows/release.yml/runs"
        );
        assert_eq!(
            workflow_api_url(owner, repository, RELEASE_WORKFLOW, "dispatches"),
            "https://api.github.com/repos/dark-factory-build/dark-factory/actions/workflows/release.yml/dispatches"
        );
        assert_eq!(
            workflow_api_url(owner, repository, DEPLOY_WORKFLOW, "runs"),
            "https://api.github.com/repos/dark-factory-build/dark-factory/actions/workflows/deploy-control-plane.yml/runs"
        );
        assert_eq!(
            workflow_api_url(owner, repository, DEPLOY_WORKFLOW, "dispatches"),
            "https://api.github.com/repos/dark-factory-build/dark-factory/actions/workflows/deploy-control-plane.yml/dispatches"
        );

        let sha = "a".repeat(40);
        let mut run = release_run(1, "push", &sha, "v1.2.3");
        assert!(run.verify_identity(RELEASE_WORKFLOW, &sha).is_ok());
        assert!(select_release_workflow_run(vec![run], "v1.2.3", &sha).is_ok());
        run = release_run(1, "push", &sha, "v1.2.3");
        run.path = RELEASE_WORKFLOW.api_id.into();
        assert!(matches!(
            run.verify_identity(RELEASE_WORKFLOW, &sha),
            Err(OperationError::Conflict)
        ));
        assert!(matches!(
            select_release_workflow_run(vec![run], "v1.2.3", &sha),
            Err(OperationError::Indeterminate)
        ));

        let title = "Deploy control-plane operation digest";
        let mut deploy = release_run(2, "workflow_dispatch", &sha, "main");
        deploy.name = "Deploy control-plane".into();
        deploy.path = DEPLOY_WORKFLOW.response_path.into();
        deploy.display_title = Some(title.into());
        assert!(deploy.verify_dispatch(DEPLOY_WORKFLOW, &sha, title).is_ok());
        deploy.path = DEPLOY_WORKFLOW.api_id.into();
        assert!(matches!(
            deploy.verify_dispatch(DEPLOY_WORKFLOW, &sha, title),
            Err(OperationError::Conflict)
        ));
    }

    #[test]
    fn an_initial_release_has_one_exact_tag_push_run() {
        let sha = "a".repeat(40);
        let selected = select_release_workflow_run(
            vec![
                release_run(1, "workflow_dispatch", &sha, "v1.2.3"),
                release_run(2, "push", &sha, "v1.2.3"),
            ],
            "v1.2.3",
            &sha,
        )
        .unwrap();
        assert_eq!(selected.id, 2);
        assert!(matches!(
            select_release_workflow_run(Vec::new(), "v1.2.3", &sha),
            Err(OperationError::Indeterminate)
        ));
        assert!(matches!(
            select_release_workflow_run(
                vec![
                    release_run(2, "push", &sha, "v1.2.3"),
                    release_run(3, "push", &sha, "v1.2.3"),
                ],
                "v1.2.3",
                &sha,
            ),
            Err(OperationError::Indeterminate)
        ));
        assert!(matches!(
            select_release_workflow_run(
                vec![release_run(2, "push", &"b".repeat(40), "v1.2.3")],
                "v1.2.3",
                &sha,
            ),
            Err(OperationError::Indeterminate)
        ));
    }

    #[test]
    fn only_app_level_endpoints_accept_an_app_jwt() {
        assert!(app_jwt_endpoint("https://api.github.com/app"));
        assert!(app_jwt_endpoint(
            "https://api.github.com/repos/dark-factory-build/dark-factory/installation"
        ));
        assert!(app_jwt_endpoint(
            "https://api.github.com/app/installations/155853844/access_tokens"
        ));
        // Readiness once proved repository identity by fetching this URL with
        // the App JWT. GitHub answers `Bad credentials`, so `verify` always
        // failed and `/readyz` could never report ready in production.
        assert!(!app_jwt_endpoint(
            "https://api.github.com/repositories/1335380107"
        ));
        assert!(!app_jwt_endpoint(
            "https://api.github.com/repos/dark-factory-build/dark-factory"
        ));
        assert!(!app_jwt_endpoint(
            "https://api.github.com/repos/dark-factory-build/dark-factory/pulls"
        ));
        // A query or fragment must not be able to supply the suffix.
        assert!(!app_jwt_endpoint(
            "https://api.github.com/repos/dark-factory-build/dark-factory/pulls?x=/installation"
        ));
        assert!(!app_jwt_endpoint(
            "https://api.github.com/repos/dark-factory-build/dark-factory/pulls#/installation"
        ));
        // Dot segments are collapsed by the URL parser after this check runs.
        assert!(!app_jwt_endpoint(
            "https://api.github.com/app/installations/../../repos/dark-factory-build/dark-factory/pulls"
        ));
        // Userinfo must not be mistaken for the host.
        assert!(!app_jwt_endpoint("https://api.github.com@evil.example/app"));
    }

    #[test]
    fn authority_configuration_is_exact_and_names_no_repository() {
        let key = general_purpose::STANDARD.encode(vec![7_u8; 1_200]);
        assert!(AppAuthority::new(4_673_420, key.clone(), PERMISSION_REVISION.into()).is_ok());
        // A deployment that believes it implements a different operation
        // contract must not serve this one.
        assert_eq!(
            AppAuthority::new(4_673_420, key, "broader-v2".into()).err(),
            Some(Error::Configuration)
        );
        // A key that is not a plausible PKCS#8 blob is a configuration error,
        // not something to discover on the first operation.
        for key in [
            general_purpose::STANDARD.encode(vec![7_u8; 999]),
            general_purpose::STANDARD.encode(vec![7_u8; 16_385]),
            "not base64".to_owned(),
        ] {
            assert_eq!(
                AppAuthority::new(4_673_420, key, PERMISSION_REVISION.into()).err(),
                Some(Error::Configuration)
            );
        }
    }

    #[test]
    fn typed_operation_inputs_are_exact_head_bound_and_bounded() {
        let mut issue = CreateIssue {
            repository: "dark-factory-build/dark-factory".into(),
            operation_id: "0c8a5c44-7f1f-11f0-952e-acde48001122".into(),
            title: "Bounded issue".into(),
            body: "Acceptance criteria.".into(),
        };
        assert!(issue.validate().is_ok());
        assert!(
            issue
                .marked_body()
                .unwrap()
                .ends_with(&issue.marker().unwrap())
        );
        assert!(
            CreateIssue {
                repository: "dark-factory-build/dark-factory".into(),
                body: "<!-- dark-factory-operation:forged -->".into(),
                ..issue
            }
            .validate()
            .is_err()
        );

        let observe_issue = ObserveIssue {
            repository: "dark-factory-build/dark-factory".into(),
            issue_number: 349,
        };
        assert!(observe_issue.validate().is_ok());
        assert!(
            ObserveIssue {
                repository: "dark-factory-build/dark-factory".into(),
                issue_number: 0
            }
            .validate()
            .is_err()
        );
        let mut resolve_issue = ResolveIssue {
            repository: "dark-factory-build/dark-factory".into(),
            operation_id: "3c8a5c44-7f1f-11f0-952e-acde48001122".into(),
            issue_number: 349,
            body: "The exact storage proof is now merged.".into(),
            state_reason: IssueResolutionReason::Completed,
        };
        assert!(resolve_issue.validate().is_ok());
        assert!(
            resolve_issue
                .marked_body()
                .unwrap()
                .contains(&resolve_issue.marker().unwrap())
        );
        assert_eq!(IssueResolutionReason::NotPlanned.as_str(), "not_planned");
        assert!(
            ResolveIssue {
                repository: "dark-factory-build/dark-factory".into(),
                body: "<!-- dark-factory-operation:forged -->".into(),
                ..resolve_issue.clone()
            }
            .validate()
            .is_err()
        );
        resolve_issue.issue_number = 0;
        assert!(resolve_issue.validate().is_err());

        let mut close = ClosePullRequest {
            repository: "dark-factory-build/dark-factory".into(),
            operation_id: "9c8a5c44-7f1f-11f0-952e-acde48001122".into(),
            pull_number: 407,
            head_sha: "e".repeat(40),
        };
        assert!(close.validate().is_ok());

        let pull = |state: &str, merged: bool, head_sha: String| PullRequest {
            number: 407,
            node_id: "PR_node".into(),
            html_url: "https://github.com/dark-factory-build/dark-factory/pull/407".into(),
            title: "Superseded gate".into(),
            body: None,
            draft: false,
            head: PullReference {
                name: "simplify-ci".into(),
                sha: head_sha,
            },
            base: PullReference {
                name: "main".into(),
                sha: "f".repeat(40),
            },
            state: state.into(),
            merged,
            merge_commit_sha: None,
        };
        assert!(
            pull("open", false, close.head_sha.clone())
                .close_result(&close)
                .unwrap()
                .is_none()
        );
        let closed = pull("closed", false, close.head_sha.clone())
            .close_result(&close)
            .unwrap()
            .unwrap();
        assert_eq!(closed.pull_number, close.pull_number);
        assert_eq!(closed.head_sha, close.head_sha);
        assert_eq!(closed.state, "closed");
        assert!(matches!(
            pull("closed", false, "d".repeat(40)).close_result(&close),
            Err(OperationError::Conflict)
        ));
        assert!(matches!(
            pull("closed", true, close.head_sha.clone()).close_result(&close),
            Err(OperationError::Conflict)
        ));
        assert!(matches!(
            pull("unknown", false, close.head_sha.clone()).close_result(&close),
            Err(OperationError::Indeterminate)
        ));
        close.pull_number = 0;
        assert!(close.validate().is_err());

        let mut merge = ObservePullRequestMerge {
            repository: "dark-factory-build/dark-factory".into(),
            enqueue_operation_id: "4c8a5c44-7f1f-11f0-952e-acde48001122".into(),
            pull_number: 390,
            head_sha: "d".repeat(40),
            base: "main".into(),
        };
        assert!(merge.validate().is_ok());
        assert!(
            ObservePullRequestMerge {
                repository: "dark-factory-build/dark-factory".into(),
                base: "../main".into(),
                ..merge
            }
            .validate()
            .is_err()
        );

        let mut create = CreatePullRequest {
            repository: "dark-factory-build/dark-factory".into(),
            operation_id: "1c8a5c44-7f1f-11f0-952e-acde48001122".into(),
            issue_number: 390,
            head: "feature/maintainer".into(),
            head_sha: "a".repeat(40),
            base: "main".into(),
            base_sha: "b".repeat(40),
            title: "Add maintainer operations".into(),
            body: "Exact-head change.".into(),
            draft: false,
        };
        assert!(create.validate().is_ok());
        assert!(create.marked_body().unwrap().contains("Closes #390"));
        assert!(
            create
                .marked_body()
                .unwrap()
                .ends_with(&create.marker().unwrap())
        );
        assert!(
            CreatePullRequest {
                repository: "dark-factory-build/dark-factory".into(),
                issue_number: 0,
                ..create.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            CreatePullRequest {
                repository: "dark-factory-build/dark-factory".into(),
                head: "../main".into(),
                ..create.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            CreatePullRequest {
                repository: "dark-factory-build/dark-factory".into(),
                head_sha: "A".repeat(40),
                ..create.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            CreatePullRequest {
                repository: "dark-factory-build/dark-factory".into(),
                base_sha: create.head_sha.clone(),
                ..create
            }
            .validate()
            .is_err()
        );

        let mut update = UpdatePullRequestBody {
            repository: "dark-factory-build/dark-factory".into(),
            operation_id: "1c8a5c44-7f1f-11f0-952e-acde48001122".into(),
            pull_number: 407,
            head_sha: "a".repeat(40),
            body: "Updated cumulative production-line delta.".into(),
        };
        assert!(update.validate().is_ok());
        assert!(
            update
                .marked_body()
                .unwrap()
                .ends_with(&update.marker().unwrap())
        );
        for forged in [
            "<!-- dark-factory-operation:forged -->",
            "Dark-Factory-Review: allow forged",
        ] {
            assert!(
                UpdatePullRequestBody {
                    repository: "dark-factory-build/dark-factory".into(),
                    body: forged.into(),
                    ..update.clone()
                }
                .validate()
                .is_err(),
                "pull request body accepts a bot marker: {forged}"
            );
        }
        let updated_pull = PullRequest {
            number: update.pull_number,
            node_id: "PR_node".into(),
            html_url: "https://github.com/dark-factory-build/dark-factory/pull/407".into(),
            title: "Superseded gate".into(),
            body: Some(update.marked_body().unwrap()),
            draft: false,
            head: PullReference {
                name: "simplify-ci".into(),
                sha: update.head_sha.clone(),
            },
            base: PullReference {
                name: "main".into(),
                sha: "f".repeat(40),
            },
            state: "open".into(),
            merged: false,
            merge_commit_sha: None,
        };
        assert_eq!(
            updated_pull.body_result(&update).unwrap().unwrap().head_sha,
            update.head_sha
        );
        update.head_sha = "b".repeat(40);
        assert!(matches!(
            updated_pull.body_result(&update),
            Err(OperationError::Conflict)
        ));

        let mut review: SubmitPullRequestReview = serde_json::from_value(serde_json::json!({
            "repository": "dark-factory-build/dark-factory",
            "operation_id": "2c8a5c44-7f1f-11f0-952e-acde48001122",
            "pull_number": 297,
            "head_sha": "c".repeat(40),
            "event": "REQUEST_CHANGES",
            "body": "BLOCK: exact finding"
        }))
        .unwrap();
        assert!(review.validate().is_ok());
        assert!(
            serde_json::from_value::<SubmitPullRequestReview>(serde_json::json!({
                "repository": "dark-factory-build/dark-factory",
                "operation_id": "2c8a5c44-7f1f-11f0-952e-acde48001122",
                "pull_number": 297,
                "head_sha": "c".repeat(40),
                "event": "APPROVE",
                "body": "Cannot self-approve"
            }))
            .is_err()
        );

        // A blocking verdict is reconciled from the `COMMENTED` state it was
        // posted as, like every other verdict. GitHub's own
        // `CHANGES_REQUESTED` is not a state this App can reach -- it refuses
        // a changes-requested self-review exactly as it refuses a
        // self-approval -- so a review carrying it is not this operation's.
        let recovered = PullRequestReview {
            id: 1,
            html_url:
                "https://github.com/dark-factory-build/dark-factory/pull/297#pullrequestreview-1"
                    .into(),
            body: Some(review.marked_body().unwrap()),
            commit_id: review.head_sha.clone(),
            state: "COMMENTED".into(),
        };
        assert!(recovered.matches(&review));
        assert!(
            !PullRequestReview {
                state: "CHANGES_REQUESTED".into(),
                ..recovered
            }
            .matches(&review)
        );
    }

    /// An operation ID is a durable identity, not a spelling. The same UUID in
    /// either case has to be the same operation, or a replay would fork.
    #[test]
    fn an_operation_id_is_case_settled_rather_than_case_refused() {
        let lower = SubmitPullRequestReview {
            repository: "dark-factory-build/dark-factory".into(),
            operation_id: "5c8a5c44-7f1f-11f0-952e-acde48001122".into(),
            pull_number: 377,
            head_sha: "a".repeat(40),
            event: ReviewEvent::RequestChanges,
            body: "Exact finding.".into(),
        };
        // `uuidgen` on macOS emits this, and refusing it cost two callers a
        // blind retry before it was canonicalized instead.
        let mut upper = SubmitPullRequestReview {
            repository: "dark-factory-build/dark-factory".into(),
            operation_id: "5C8A5C44-7F1F-11F0-952E-ACDE48001122".into(),
            ..lower.clone()
        };
        assert!(upper.validate().is_ok());
        assert_eq!(upper.operation_id, lower.operation_id);
        // Replay identity is all three of these: the journal key, the digested
        // request, and the marker reconciliation matches with `contains`. If
        // the case were settled anywhere later than this, one of them would
        // still carry the caller's spelling and the replay would fork.
        assert_eq!(upper.marker(), lower.marker());
        assert_eq!(
            serde_json::to_string(&upper).unwrap(),
            serde_json::to_string(&lower).unwrap()
        );

        // Case is the only thing relaxed. Everything that is not a UUID is
        // still refused, and the refusal is still the first thing `validate`
        // reaches.
        for malformed in [
            "5c8a5c44-7f1f-11f0-952e-acde4800112",
            "5c8a5c44-7f1f-11f0-952e-acde480011222",
            "5c8a5c44-7f1f-11f0-952e-acde480011g2",
            "5c8a5c447f1f-11f0-952e--acde48001122",
            "5c8a5c44_7f1f_11f0_952e_acde48001122",
            "",
        ] {
            let mut request = SubmitPullRequestReview {
                repository: "dark-factory-build/dark-factory".into(),
                operation_id: malformed.into(),
                ..lower.clone()
            };
            assert!(
                request.validate().is_err(),
                "a malformed operation ID must be refused: {malformed}"
            );
        }
    }

    /// The required `review` check believes the verdict line, so the App has
    /// to be the only thing that can write one.
    #[test]
    fn review_verdict_is_app_written_and_never_reaches_the_wire() {
        let mut allow: SubmitPullRequestReview = serde_json::from_value(serde_json::json!({
            "repository": "dark-factory-build/dark-factory",
            "operation_id": "3c8a5c44-7f1f-11f0-952e-acde48001122",
            "pull_number": 331,
            "head_sha": "d".repeat(40),
            "event": "ALLOW",
            "body": "Tried to break the exact-head check and could not."
        }))
        .unwrap();
        assert!(allow.validate().is_ok());

        // `ALLOW`, `COMMENT`, and `REQUEST_CHANGES` are all this App's words.
        // GitHub is told `COMMENT` for every one of them, because the App
        // authored the pull request it is reviewing and GitHub refuses a
        // self-review that takes a side -- `APPROVE` and `REQUEST_CHANGES`
        // alike. Neither reaches the wire, and there is no per-verdict
        // mapping left that could send one.
        assert_eq!(REVIEW_EVENT, "COMMENT");
        assert_eq!(REVIEW_STATE, "COMMENTED");

        // The verdict is rendered by the App, carries the exact head, and sits
        // alongside the operation marker.
        let body = allow.marked_body().unwrap();
        assert!(body.contains(&format!("Dark-Factory-Review: allow {}", "d".repeat(40))));
        assert!(body.contains(&allow.marker().unwrap()));
        assert!(body.starts_with("Tried to break the exact-head check and could not."));

        // The other two verdicts are distinguishable in the same line, so a
        // `COMMENTED` review that decides nothing can never read as an ALLOW.
        let note = SubmitPullRequestReview {
            repository: "dark-factory-build/dark-factory".into(),
            event: ReviewEvent::Comment,
            ..allow.clone()
        };
        assert!(
            note.marked_body()
                .unwrap()
                .contains("Dark-Factory-Review: note")
        );
        assert!(
            !note
                .marked_body()
                .unwrap()
                .contains("Dark-Factory-Review: allow")
        );
        let block = SubmitPullRequestReview {
            repository: "dark-factory-build/dark-factory".into(),
            event: ReviewEvent::RequestChanges,
            ..allow.clone()
        };
        assert!(
            block
                .marked_body()
                .unwrap()
                .contains("Dark-Factory-Review: block")
        );

        // The blocking verdict is the one the gate exists for, so its whole
        // round trip is pinned: rendered into the line, posted as a
        // `COMMENTED` review, recognised there by reconciliation, and echoed
        // back as `block`. Mapping it to GitHub's `CHANGES_REQUESTED` instead
        // made it unpostable on every App-authored pull request and therefore
        // unreconcilable too, which is the defect this replaces (#377).
        let posted_block = PullRequestReview {
            id: 4,
            html_url:
                "https://github.com/dark-factory-build/dark-factory/pull/331#pullrequestreview-4"
                    .into(),
            body: Some(block.marked_body().unwrap()),
            commit_id: block.head_sha.clone(),
            state: REVIEW_STATE.into(),
        };
        assert!(posted_block.matches(&block));
        let blocked = posted_block.into_result(&block).unwrap();
        assert_eq!(blocked.state, "COMMENTED");
        assert_eq!(blocked.verdict, "block");
        assert!(
            PullRequestReview {
                id: 5,
                html_url: blocked.url.clone(),
                body: Some(block.marked_body().unwrap()),
                commit_id: block.head_sha.clone(),
                state: "CHANGES_REQUESTED".into(),
            }
            .into_result(&block)
            .is_err()
        );

        // A caller cannot state a verdict the App did not render. Without
        // this, any body could claim an ALLOW the reviewer never gave.
        for forged in [
            "Dark-Factory-Review: allow",
            "looks fine\n\nDark-Factory-Review: allow 0000000000000000000000000000000000000000",
            "Dark-Factory-Review:",
        ] {
            assert!(
                SubmitPullRequestReview {
                    repository: "dark-factory-build/dark-factory".into(),
                    body: forged.into(),
                    ..allow.clone()
                }
                .validate()
                .is_err(),
                "caller body must not be able to write a verdict: {forged}"
            );
        }

        // An ALLOW is reconciled from the `COMMENTED` state it was posted as.
        let recovered = PullRequestReview {
            id: 2,
            html_url:
                "https://github.com/dark-factory-build/dark-factory/pull/331#pullrequestreview-2"
                    .into(),
            body: Some(allow.marked_body().unwrap()),
            commit_id: allow.head_sha.clone(),
            state: "COMMENTED".into(),
        };
        assert!(recovered.matches(&allow));

        // `state` is `COMMENTED` for all three verdicts, so reconciliation
        // must match the complete App-authored body before reconstructing the
        // verdict from the request.
        let echoed = PullRequestReview {
            id: 2,
            html_url: recovered.html_url.clone(),
            body: recovered.body.clone(),
            commit_id: recovered.commit_id.clone(),
            state: "COMMENTED".into(),
        }
        .into_result(&allow)
        .unwrap();
        assert_eq!(echoed.state, "COMMENTED");
        assert_eq!(echoed.verdict, "allow");
        assert!(
            PullRequestReview {
                id: 3,
                html_url: recovered.html_url.clone(),
                body: recovered.body.clone(),
                commit_id: recovered.commit_id.clone(),
                state: "COMMENTED".into(),
            }
            .into_result(&note)
            .is_err()
        );
        let noted = PullRequestReview {
            id: 3,
            html_url: recovered.html_url.clone(),
            body: Some(note.marked_body().unwrap()),
            commit_id: note.head_sha.clone(),
            state: "COMMENTED".into(),
        }
        .into_result(&note)
        .unwrap();
        assert_eq!(noted.state, echoed.state);
        assert_ne!(noted.verdict, echoed.verdict);

        // A verdict is bound to one head. The same review against any other
        // commit is not this operation's result.
        assert!(
            !PullRequestReview {
                commit_id: "e".repeat(40),
                ..recovered
            }
            .matches(&allow)
        );
    }

    #[test]
    fn direct_merge_input_is_exactly_bound_and_has_no_merge_controls() {
        let mut request: MergePullRequestAtHead = serde_json::from_value(serde_json::json!({
            "repository": "dark-factory-build/dark-factory",
            "operation_id": "6c8a5c44-7f1f-11f0-952e-acde48001122",
            "review_operation_id": "7c8a5c44-7f1f-11f0-952e-acde48001122",
            "pull_number": 403,
            "head_sha": "a".repeat(40),
            "base": "main"
        }))
        .unwrap();
        assert!(request.validate().is_ok());
        for value in [
            serde_json::json!({"method":"merge"}),
            serde_json::json!({"merge_method":"merge"}),
            serde_json::json!({"url":"https://api.github.com"}),
            serde_json::json!({"base_sha":"b".repeat(40)}),
        ] {
            let mut extra = serde_json::to_value(&request).unwrap();
            extra.as_object_mut().unwrap().extend(
                value
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
            assert!(serde_json::from_value::<MergePullRequestAtHead>(extra).is_err());
        }
        let mut same = request.clone();
        same.operation_id = same.review_operation_id.clone();
        assert_eq!(same.validate().err(), Some(OperationError::InvalidInput));
        same.operation_id = "8c8a5c44-7f1f-11f0-952e-acde48001122".into();
        assert_eq!(same.validate().err(), None);
    }

    #[test]
    fn direct_merge_private_unprotected_fallback_fails_closed() {
        let repository = |private, squash| RepositoryMetadata {
            id: 1_335_380_107,
            full_name: "dark-factory-build/dark-factory-site".into(),
            default_branch: "main".into(),
            private,
            allow_squash_merge: squash,
        };
        let branch = BranchSnapshot {
            name: "main".into(),
            protected: false,
            commit: BranchCommit {
                sha: "a".repeat(40),
            },
        };
        assert!(private_unprotected_merge_allowed(
            &repository(Some(true), Some(true)),
            &branch
        ));
        for metadata in [
            repository(None, Some(true)),
            repository(Some(false), Some(true)),
            repository(Some(true), None),
            repository(Some(true), Some(false)),
        ] {
            assert!(!private_unprotected_merge_allowed(&metadata, &branch));
        }
        assert!(!private_unprotected_merge_allowed(
            &repository(Some(true), Some(true)),
            &BranchSnapshot {
                protected: true,
                ..branch
            }
        ));

        let absent = serde_json::json!({"repository": {"mergeQueue": null}});
        assert!(no_merge_queue(Some(absent.clone()), None).is_ok());
        for unknown in [
            None,
            Some(serde_json::json!({})),
            Some(serde_json::json!({"repository": null})),
            Some(serde_json::json!({"repository": {}})),
        ] {
            assert_eq!(
                no_merge_queue(unknown, None).err(),
                Some(OperationError::Indeterminate)
            );
        }
        assert_eq!(
            no_merge_queue(
                Some(serde_json::json!({"repository": {"mergeQueue": {}}})),
                None
            )
            .err(),
            Some(OperationError::Refused(RefusalReason::MergePreconditions))
        );
        assert_eq!(
            no_merge_queue(Some(absent), Some(GraphQlFailure::Unknown)).err(),
            Some(OperationError::Indeterminate)
        );
    }

    #[test]
    fn direct_merge_rules_checks_reviews_and_statuses_fail_closed() {
        let required = |context: &str, integration_id| RequiredCheckIdentity {
            context: context.into(),
            integration_id,
        };
        let pull_parameters = || {
            serde_json::json!({
                "allowed_merge_methods": ["squash"],
                "dismiss_stale_reviews_on_push": false,
                "require_code_owner_review": false,
                "require_last_push_approval": false,
                "required_approving_review_count": 0,
                "required_review_thread_resolution": false
            })
        };
        let protected: Vec<BranchRule> = serde_json::from_value(serde_json::json!([
            {"type": "pull_request", "parameters": pull_parameters()},
            {"type": "required_status_checks", "parameters": {
                "strict_required_status_checks_policy": true,
                "required_status_checks": [{"context": "checks", "integration_id": 42}]
            }}
        ]))
        .unwrap();
        assert_eq!(
            branch_rules_allow_merge(&protected).unwrap(),
            [required("checks", Some(42))]
        );
        let multiple: Vec<BranchRule> = serde_json::from_value(serde_json::json!([
            {"type": "pull_request", "parameters": pull_parameters()},
            {"type": "pull_request", "parameters": pull_parameters()},
            {"type": "required_status_checks", "parameters": {
                "strict_required_status_checks_policy": true,
                "required_status_checks": [{"context": "lint"}]
            }},
            {"type": "required_status_checks", "parameters": {
                "strict_required_status_checks_policy": true,
                "required_status_checks": [{"context": "tests"}]
            }}
        ]))
        .unwrap();
        assert_eq!(
            branch_rules_allow_merge(&multiple).unwrap(),
            [required("lint", None), required("tests", None)]
        );
        let mut queue = multiple.clone();
        queue.push(BranchRule {
            r#type: "merge_queue".into(),
            parameters: serde_json::Value::Null,
            ruleset_id: Some(1),
            ruleset_source: Some("dark-factory-build/dark-factory".into()),
        });
        assert!(branch_rules_allow_merge(&queue).is_none());
        let mut non_squash = multiple.clone();
        non_squash[1].parameters = serde_json::json!({
            "allowed_merge_methods": ["merge"],
            "dismiss_stale_reviews_on_push": false,
            "require_code_owner_review": false,
            "require_last_push_approval": false,
            "required_approving_review_count": 0,
            "required_review_thread_resolution": false
        });
        assert!(branch_rules_allow_merge(&non_squash).is_none());
        let mut loose = multiple.clone();
        loose[3].parameters["strict_required_status_checks_policy"] =
            serde_json::Value::Bool(false);
        assert!(branch_rules_allow_merge(&loose).is_none());
        let passing = vec![CheckResult {
            name: "checks".into(),
            status: "completed".into(),
            conclusion: Some("success".into()),
            url: "https://github.com/checks".into(),
            app_id: Some(42),
        }];
        let any_checks = [required("checks", None)];
        assert!(checks_allow_merge(&passing, &any_checks));
        assert!(
            serde_json::to_value(&passing[0])
                .unwrap()
                .get("app_id")
                .is_none()
        );
        assert!(checks_allow_merge(
            &passing,
            &[required("checks", Some(42))]
        ));
        assert!(!checks_allow_merge(
            &passing,
            &[required("checks", Some(43))]
        ));
        for conclusion in ["failure", "cancelled", "timed_out", "action_required"] {
            let mut blocked = passing.clone();
            blocked[0].conclusion = Some(conclusion.into());
            assert!(!checks_allow_merge(&blocked, &any_checks));
        }
        let mut incomplete = passing.clone();
        incomplete[0].status = "in_progress".into();
        assert!(!checks_allow_merge(&incomplete, &any_checks));
        assert!(!checks_allow_merge(&passing, &[]));
        assert!(checks_are_terminal_and_non_failing(&passing));
        assert!(!checks_are_terminal_and_non_failing(&[]));

        let allow = ReviewResult {
            review_id: 1,
            url: "https://github.com/dark-factory-build/dark-factory/pull/403#pullrequestreview-1"
                .into(),
            head_sha: "a".repeat(40),
            state: "COMMENTED".into(),
            verdict: "allow".into(),
        };
        assert!(review_result_allows_merge(
            &allow,
            "dark-factory-build/dark-factory",
            403,
            &"a".repeat(40)
        ));
        for (state, verdict) in [("COMMENTED", "block"), ("CHANGES_REQUESTED", "allow")] {
            let mut blocked = allow.clone();
            blocked.state = state.into();
            blocked.verdict = verdict.into();
            assert!(!review_result_allows_merge(
                &blocked,
                "dark-factory-build/dark-factory",
                403,
                &"a".repeat(40)
            ));
        }

        let head = "a".repeat(40);
        let app_block = PullRequestReview {
            id: 2,
            html_url: "https://github.com/dark-factory-build/dark-factory/pull/403".into(),
            body: Some(format!("{REVIEW_VERDICT_PREFIX} block {head}")),
            commit_id: head.clone(),
            state: "COMMENTED".into(),
        };
        assert!(app_block.blocks_head(&head));
        assert!(!app_block.blocks_head(&"b".repeat(40)));
        assert!(
            PullRequestReview {
                id: 3,
                html_url: app_block.html_url.clone(),
                body: app_block.body.clone(),
                commit_id: head.clone(),
                state: "DISMISSED".into(),
            }
            .blocks_head(&head)
        );
    }

    #[test]
    fn direct_merge_ruleset_ids_and_bypass_shape_fail_closed() {
        let rules: Vec<BranchRule> = serde_json::from_value(serde_json::json!([
            {"type": "pull_request", "parameters": {}, "ruleset_id": 7, "ruleset_source": "dark-factory-build/dark-factory"},
            {"type": "required_status_checks", "parameters": {}, "ruleset_id": 7, "ruleset_source": "dark-factory-build/dark-factory"},
            {"type": "creation", "parameters": null, "ruleset_id": 11, "ruleset_source": "dark-factory-build"}
        ]))
        .unwrap();
        assert_eq!(active_ruleset_ids(&rules), Some(vec![7, 11]));

        let mut missing = rules.clone();
        missing[0].ruleset_id = None;
        assert!(active_ruleset_ids(&missing).is_none());
        for invalid in [0, -1] {
            let mut invalid_id = rules.clone();
            invalid_id[0].ruleset_id = Some(invalid);
            assert!(active_ruleset_ids(&invalid_id).is_none());
        }

        let safe: RepositoryRuleset = serde_json::from_value(serde_json::json!({
            "id": 7,
            "target": "branch",
            "enforcement": "active",
            "bypass_actors": [
                {"actor_id": 9, "actor_type": "User", "bypass_mode": "always"},
                {"actor_id": 10, "actor_type": "Integration", "bypass_mode": "pull_request"},
                {"actor_id": 11, "actor_type": "Team", "bypass_mode": "exempt"},
                {"actor_id": null, "actor_type": "DeployKey", "bypass_mode": "always"},
                {"actor_id": null, "actor_type": "OrganizationAdmin", "bypass_mode": "always"}
            ]
        }))
        .unwrap();
        assert!(ruleset_allows_merge(&safe, 7, 42));
        for drifted in [
            RepositoryRuleset {
                id: 8,
                ..safe.clone()
            },
            RepositoryRuleset {
                target: "tag".into(),
                ..safe.clone()
            },
            RepositoryRuleset {
                enforcement: "disabled".into(),
                ..safe.clone()
            },
        ] {
            assert!(!ruleset_allows_merge(&drifted, 7, 42));
        }

        let matching_app: RepositoryRuleset = serde_json::from_value(serde_json::json!({
            "id": 7,
            "target": "branch",
            "enforcement": "active",
            "bypass_actors": [
                {"actor_id": 42, "actor_type": "Integration", "bypass_mode": "always"}
            ]
        }))
        .unwrap();
        assert!(!ruleset_allows_merge(&matching_app, 7, 42));
        let missing_bypass: RepositoryRuleset = serde_json::from_value(serde_json::json!({
            "id": 7,
            "target": "branch",
            "enforcement": "active"
        }))
        .unwrap();
        assert!(!ruleset_allows_merge(&missing_bypass, 7, 42));
        for actor in [
            serde_json::json!({"actor_id": 9, "actor_type": "FutureActor", "bypass_mode": "always"}),
            serde_json::json!({"actor_id": 9, "actor_type": "User", "bypass_mode": "future"}),
            serde_json::json!({"actor_id": 0, "actor_type": "User", "bypass_mode": "always"}),
        ] {
            let ruleset: RepositoryRuleset = serde_json::from_value(serde_json::json!({
                "id": 7,
                "target": "branch",
                "enforcement": "active",
                "bypass_actors": [actor]
            }))
            .unwrap();
            assert!(!ruleset_allows_merge(&ruleset, 7, 42));
        }
        assert!(serde_json::from_value::<RepositoryRuleset>(serde_json::json!({
            "id": 7,
            "target": "branch",
            "enforcement": "active",
            "bypass_actors": [{"actor_id": 9, "actor_type": "User", "bypass_mode": "always", "new": true}]
        }))
        .is_err());
    }

    #[test]
    fn direct_merge_statuses_and_response_reconciliation_are_typed() {
        assert_eq!(classify_merge_status(409), MergeHttpStatus::HeadConflict);
        assert_eq!(classify_merge_status(422), MergeHttpStatus::Refused);
        assert_eq!(classify_merge_status(400), MergeHttpStatus::Reconcile);
        assert_eq!(classify_merge_status(429), MergeHttpStatus::Reconcile);
        assert_eq!(classify_merge_status(500), MergeHttpStatus::Reconcile);
        assert_eq!(classify_merge_status(0), MergeHttpStatus::Reconcile);

        let request = MergePullRequestAtHead {
            repository: "dark-factory-build/dark-factory".into(),
            operation_id: "6c8a5c44-7f1f-11f0-952e-acde48001122".into(),
            review_operation_id: "7c8a5c44-7f1f-11f0-952e-acde48001122".into(),
            pull_number: 403,
            head_sha: "a".repeat(40),
            base: "main".into(),
        };
        let response: PullRequestMergeResponse = serde_json::from_value(serde_json::json!({
            "sha": "c".repeat(40), "merged": true
        }))
        .unwrap();
        let result = merge_response_result(response, &request).unwrap();
        assert_eq!(result.pull_number, 403);
        assert_eq!(result.head_sha, request.head_sha);
        assert_eq!(result.merge_commit_sha, "c".repeat(40));
        let marked_message = format!("Squash change\n\n{}", request.trailer().unwrap());
        assert!(merge_commit_has_trailer(&request, &marked_message).unwrap());
        assert!(!merge_commit_has_trailer(&request, "Squash change").unwrap());
        let no_effect: PullRequestMergeResponse = serde_json::from_value(serde_json::json!({
            "sha": null, "merged": false
        }))
        .unwrap();
        assert!(matches!(
            merge_response_result(no_effect, &request),
            Err(OperationError::Refused(RefusalReason::MergePreconditions))
        ));
    }
}
