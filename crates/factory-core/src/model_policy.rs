//! One deliberately small, auditable policy for Codex model selection.
//!
//! This is a tier policy, not a pricing service: model identifiers and
//! reasoning tiers are explicit operator-facing choices, while transient
//! provider prices stay out of the product. Existing profiles are never
//! rewritten by this module; its defaults apply only when a new agent is
//! created without an explicit selection.

use crate::{AgentRole, Provider};

pub const ROUTINE_MODEL: &str = "gpt-5.6-luna";
pub const ROUTINE_REASONING_EFFORT: &str = "medium";
pub const ESCALATED_MODEL: &str = "gpt-5.6-sol";
pub const ESCALATED_REASONING_EFFORT: &str = "xhigh";
pub const REASONING_EFFORTS: &[&str] = &["none", "low", "medium", "high", "xhigh", "max"];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelSelection {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ModelPolicyError {
    #[error("reasoning effort {0:?} is unsupported; use none, low, medium, high, xhigh, or max")]
    UnsupportedReasoningEffort(String),
    #[error("gpt-5.6-sol requires an explicit high-risk model selection reason")]
    EscalationReasonRequired,
    #[error("gpt-5.6-sol is reserved for xhigh reasoning effort")]
    EscalationRequiresXhigh,
    #[error("a model selection reason requires a model or an explicit escalation")]
    ReasonWithoutModel,
}

/// Resolve one new agent's model policy. This is intentionally called only
/// for creation: profile edits pass through the existing values unchanged
/// unless the operator explicitly supplies a replacement.
pub fn select(
    provider: Provider,
    role: AgentRole,
    requested_model: Option<&str>,
    requested_reasoning_effort: Option<&str>,
    requested_reason: Option<&str>,
) -> Result<ModelSelection, ModelPolicyError> {
    validate_reasoning_effort(requested_reasoning_effort)?;

    if provider != Provider::Codex {
        if requested_model.is_none() && requested_reason.is_some() {
            return Err(ModelPolicyError::ReasonWithoutModel);
        }
        return Ok(ModelSelection {
            model: requested_model.map(str::to_owned),
            reasoning_effort: requested_reasoning_effort.map(str::to_owned),
            reason: requested_reason.map(str::to_owned),
        });
    }

    let Some(model) = requested_model else {
        let (model, reasoning_effort, default_reason) = if let Some(reason) = requested_reason {
            (ESCALATED_MODEL, ESCALATED_REASONING_EFFORT, reason)
        } else if role == AgentRole::Orchestrator {
            (
                ESCALATED_MODEL,
                ESCALATED_REASONING_EFFORT,
                "orchestrator (God) high-capability default",
            )
        } else {
            (
                ROUTINE_MODEL,
                ROUTINE_REASONING_EFFORT,
                "routine bounded worker default",
            )
        };
        return Ok(ModelSelection {
            model: Some(model.to_owned()),
            reasoning_effort: Some(
                requested_reasoning_effort
                    .unwrap_or(reasoning_effort)
                    .to_owned(),
            ),
            reason: Some(default_reason.to_owned()),
        });
    };

    if model == ESCALATED_MODEL {
        if requested_reason.is_none() && role != AgentRole::Orchestrator {
            return Err(ModelPolicyError::EscalationReasonRequired);
        }
        if requested_reasoning_effort.is_some_and(|effort| effort != ESCALATED_REASONING_EFFORT) {
            return Err(ModelPolicyError::EscalationRequiresXhigh);
        }
    }

    Ok(ModelSelection {
        model: Some(model.to_owned()),
        reasoning_effort: requested_reasoning_effort
            .map(str::to_owned)
            .or_else(|| (model == ROUTINE_MODEL).then(|| ROUTINE_REASONING_EFFORT.to_owned()))
            .or_else(|| (model == ESCALATED_MODEL).then(|| ESCALATED_REASONING_EFFORT.to_owned())),
        reason: Some(
            requested_reason
                .unwrap_or("operator-selected model")
                .to_owned(),
        ),
    })
}

pub fn validate_reasoning_effort(reasoning_effort: Option<&str>) -> Result<(), ModelPolicyError> {
    if let Some(effort) = reasoning_effort
        && !REASONING_EFFORTS.contains(&effort)
    {
        return Err(ModelPolicyError::UnsupportedReasoningEffort(
            effort.to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routine_codex_workers_use_luna_without_touching_existing_profiles() {
        assert_eq!(
            select(Provider::Codex, AgentRole::Worker, None, None, None).unwrap(),
            ModelSelection {
                model: Some(ROUTINE_MODEL.into()),
                reasoning_effort: Some(ROUTINE_REASONING_EFFORT.into()),
                reason: Some("routine bounded worker default".into()),
            }
        );
    }

    #[test]
    fn god_defaults_to_sol_xhigh() {
        let selection = select(Provider::Codex, AgentRole::Orchestrator, None, None, None).unwrap();
        assert_eq!(selection.model.as_deref(), Some(ESCALATED_MODEL));
        assert_eq!(
            selection.reasoning_effort.as_deref(),
            Some(ESCALATED_REASONING_EFFORT)
        );
        assert!(selection.reason.unwrap().contains("God"));
    }

    #[test]
    fn explicit_reason_escalates_a_worker_and_is_auditable() {
        let selection = select(
            Provider::Codex,
            AgentRole::Worker,
            None,
            None,
            Some("security boundary integration after failed attempt"),
        )
        .unwrap();
        assert_eq!(selection.model.as_deref(), Some(ESCALATED_MODEL));
        assert_eq!(
            selection.reasoning_effort.as_deref(),
            Some(ESCALATED_REASONING_EFFORT)
        );
        assert_eq!(
            selection.reason.as_deref(),
            Some("security boundary integration after failed attempt")
        );
    }

    #[test]
    fn sol_requires_reason_and_xhigh() {
        assert_eq!(
            select(
                Provider::Codex,
                AgentRole::Worker,
                Some(ESCALATED_MODEL),
                None,
                None,
            ),
            Err(ModelPolicyError::EscalationReasonRequired)
        );
        assert_eq!(
            select(
                Provider::Codex,
                AgentRole::Worker,
                Some(ESCALATED_MODEL),
                Some("high"),
                Some("release integration"),
            ),
            Err(ModelPolicyError::EscalationRequiresXhigh)
        );
    }

    #[test]
    fn non_codex_model_commands_are_not_rewritten() {
        assert_eq!(
            select(
                Provider::Shell,
                AgentRole::Worker,
                Some("/tmp/agent.sh"),
                None,
                None,
            )
            .unwrap()
            .model
            .as_deref(),
            Some("/tmp/agent.sh")
        );
    }
}
