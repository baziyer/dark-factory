//! Durable change/review projections.
//!
//! This is deliberately a projection, not a GitHub ingestion engine.  The
//! daemon records the exact values an operator or an explicitly named
//! connector supplies; agents never supply credentials or hosted-check
//! authority.

use serde::{Deserialize, Serialize};

use crate::{AgentId, RunId, TaskId};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeState {
    Authored,
    ReviewRequested,
    Findings,
    AuthorResponding,
    ReReview,
    Satisfied,
    IntegrationReady,
    Integrated,
    Released,
    Abandoned,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pending,
    Failed,
    Green,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckSource {
    Operator,
    Connector,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChangeFinding {
    pub number: u32,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_disposition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_resolution: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChangeSnapshot {
    pub id: String,
    pub project_id: crate::ProjectId,
    pub source_issue: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_task_id: Option<TaskId>,
    pub author_agent_id: AgentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_run_id: Option<RunId>,
    pub author_present: bool,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    pub head_sha: String,
    pub base_branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_base_sha: Option<String>,
    pub state: ChangeState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_agent_id: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewed_sha: Option<String>,
    pub checks_status: CheckStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks_source: Option<CheckSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_by_agent_id: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_sha: Option<String>,
    pub integration_ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abandoned_reason: Option<String>,
    pub findings: Vec<ChangeFinding>,
    pub updated_at_ms: i64,
}

impl ChangeSnapshot {
    /// A new head invalidates every review/check/readiness claim.  Keeping
    /// this invariant here makes callers unable to accidentally preserve a
    /// green gate for a force-pushed or rebased commit.
    pub fn invalidate_head(&mut self, head_sha: String, now_ms: i64) {
        self.head_sha = head_sha;
        self.reviewed_sha = None;
        self.checks_sha = None;
        self.checks_status = CheckStatus::Pending;
        self.checks_source = None;
        self.current_base_sha = None;
        self.ready_by_agent_id = None;
        self.ready_sha = None;
        self.integration_ready = false;
        self.state = if self.findings.is_empty() {
            ChangeState::Authored
        } else {
            ChangeState::AuthorResponding
        };
        self.updated_at_ms = now_ms;
    }
}
