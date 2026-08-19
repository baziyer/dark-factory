//! One auditable, provider-aware policy for model profile selection.
//!
//! This is a capability/validation table, not a pricing service. Model
//! identifiers and reasoning tiers are explicit operator-facing choices;
//! transient provider prices stay out of the product.

use crate::{AgentRole, Provider};

pub const ROUTINE_MODEL: &str = "gpt-5.6-luna";
pub const ROUTINE_REASONING_EFFORT: &str = "medium";
pub const ESCALATED_MODEL: &str = "gpt-5.6-sol";
pub const ESCALATED_REASONING_EFFORT: &str = "xhigh";
pub const TERRA_MODEL: &str = "gpt-5.6-terra";
pub const REASONING_EFFORTS: &[&str] = &["none", "low", "medium", "high", "xhigh", "max"];
pub const CODEX_MODELS: &[&str] = &[ESCALATED_MODEL, TERRA_MODEL, ROUTINE_MODEL];
pub const CLAUDE_MODEL_ALIASES: &[&str] = &["opus", "sonnet", "haiku"];
pub const CLAUDE_MODEL_PREFIX: &str = "claude-";
pub const MAX_SELECTION_REASON_BYTES: usize = 512;
pub const ROUTINE_REASON: &str = "routine bounded worker default";
pub const GOD_REASON: &str = "orchestrator (God) high-capability default";
pub const OPERATOR_REASON: &str = "operator-selected model";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderCapabilities {
    pub model_ids: &'static [&'static str],
    pub model_prefix: Option<&'static str>,
    pub reasoning_efforts: &'static [&'static str],
    pub model_is_command: bool,
}

#[must_use]
pub const fn capabilities(provider: Provider) -> ProviderCapabilities {
    match provider {
        Provider::Codex => ProviderCapabilities {
            model_ids: CODEX_MODELS,
            model_prefix: None,
            reasoning_efforts: REASONING_EFFORTS,
            model_is_command: false,
        },
        Provider::ClaudeCode => ProviderCapabilities {
            model_ids: CLAUDE_MODEL_ALIASES,
            model_prefix: Some(CLAUDE_MODEL_PREFIX),
            reasoning_efforts: &[],
            model_is_command: false,
        },
        Provider::Shell => ProviderCapabilities {
            model_ids: &[],
            model_prefix: None,
            reasoning_efforts: &[],
            model_is_command: true,
        },
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelSelection {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ModelPolicyError {
    #[error("model {model:?} is not supported by provider {provider:?}")]
    UnsupportedModel { provider: Provider, model: String },
    #[error("reasoning effort {effort:?} is not supported by provider {provider:?}")]
    UnsupportedReasoningEffort { provider: Provider, effort: String },
    #[error("a reasoning effort is not supported by provider {provider:?}")]
    ReasoningEffortUnsupported { provider: Provider },
    #[error("model selection reasons are supported only for Codex profiles")]
    SelectionReasonUnsupported,
    #[error("gpt-5.6-sol requires an explicit high-risk model selection reason")]
    EscalationReasonRequired,
    #[error("gpt-5.6-sol is reserved for xhigh reasoning effort")]
    EscalationRequiresXhigh,
    #[error("the orchestrator must use gpt-5.6-sol at xhigh reasoning effort")]
    GodModelInvariant,
    #[error("the routine worker default reason cannot justify a Sol escalation")]
    StaleRoutineReason,
    #[error("a model selection reason requires a model")]
    ReasonWithoutModel,
    #[error("model selection reason is empty, too long, or contains display controls")]
    InvalidSelectionReason,
}

/// Validate and normalize the final desired profile. `apply_defaults` is true
/// only for creation; updates preserve a deliberately empty legacy profile
/// until the operator supplies a model choice. Every caller (daemon create,
/// daemon update, CLI, and TUI) therefore shares these invariants.
pub fn normalize_profile(
    provider: Provider,
    role: AgentRole,
    desired: ModelSelection,
    apply_defaults: bool,
) -> Result<ModelSelection, ModelPolicyError> {
    validate_reason(desired.reason.as_deref())?;
    validate_model(provider, desired.model.as_deref())?;
    validate_effort(provider, desired.reasoning_effort.as_deref())?;

    if provider != Provider::Codex {
        if desired.reason.is_some() {
            return Err(ModelPolicyError::SelectionReasonUnsupported);
        }
        return Ok(desired);
    }

    let Some(model) = desired.model.as_deref() else {
        if desired.reason.is_some() {
            if apply_defaults && role != AgentRole::Orchestrator {
                return Ok(ModelSelection {
                    model: Some(ESCALATED_MODEL.into()),
                    reasoning_effort: Some(ESCALATED_REASONING_EFFORT.into()),
                    reason: desired.reason,
                });
            }
            return Err(ModelPolicyError::ReasonWithoutModel);
        }
        if role == AgentRole::Orchestrator && (apply_defaults || desired.reasoning_effort.is_some())
        {
            return Ok(ModelSelection {
                model: Some(ESCALATED_MODEL.into()),
                reasoning_effort: Some(ESCALATED_REASONING_EFFORT.into()),
                reason: Some(GOD_REASON.into()),
            });
        }
        if !apply_defaults {
            return Ok(desired);
        }
        return Ok(if role == AgentRole::Orchestrator {
            ModelSelection {
                model: Some(ESCALATED_MODEL.into()),
                reasoning_effort: Some(ESCALATED_REASONING_EFFORT.into()),
                reason: Some(GOD_REASON.into()),
            }
        } else if desired.reasoning_effort.is_some() {
            ModelSelection {
                model: Some(ROUTINE_MODEL.into()),
                reasoning_effort: desired.reasoning_effort,
                reason: Some(ROUTINE_REASON.into()),
            }
        } else {
            ModelSelection {
                model: Some(ROUTINE_MODEL.into()),
                reasoning_effort: Some(ROUTINE_REASONING_EFFORT.into()),
                reason: Some(ROUTINE_REASON.into()),
            }
        });
    };

    if role == AgentRole::Orchestrator {
        if model != ESCALATED_MODEL {
            return Err(ModelPolicyError::GodModelInvariant);
        }
        if desired
            .reasoning_effort
            .as_deref()
            .is_some_and(|effort| effort != ESCALATED_REASONING_EFFORT)
        {
            return Err(ModelPolicyError::EscalationRequiresXhigh);
        }
        return Ok(ModelSelection {
            model: Some(ESCALATED_MODEL.into()),
            reasoning_effort: Some(ESCALATED_REASONING_EFFORT.into()),
            reason: Some(desired.reason.unwrap_or_else(|| GOD_REASON.into())),
        });
    }

    if model == ESCALATED_MODEL {
        let Some(reason) = desired.reason else {
            return Err(ModelPolicyError::EscalationReasonRequired);
        };
        if reason == ROUTINE_REASON {
            return Err(ModelPolicyError::StaleRoutineReason);
        }
        if desired
            .reasoning_effort
            .as_deref()
            .is_some_and(|effort| effort != ESCALATED_REASONING_EFFORT)
        {
            return Err(ModelPolicyError::EscalationRequiresXhigh);
        }
        return Ok(ModelSelection {
            model: Some(ESCALATED_MODEL.into()),
            reasoning_effort: Some(ESCALATED_REASONING_EFFORT.into()),
            reason: Some(reason),
        });
    }

    Ok(ModelSelection {
        model: Some(model.into()),
        reasoning_effort: desired
            .reasoning_effort
            .or_else(|| (model == ROUTINE_MODEL).then(|| ROUTINE_REASONING_EFFORT.to_owned())),
        reason: Some(desired.reason.unwrap_or_else(|| OPERATOR_REASON.into())),
    })
}

/// Backwards-compatible name for callers/tests that only construct a new
/// profile. New code should call [`normalize_profile`] directly.
pub fn select(
    provider: Provider,
    role: AgentRole,
    requested_model: Option<&str>,
    requested_reasoning_effort: Option<&str>,
    requested_reason: Option<&str>,
) -> Result<ModelSelection, ModelPolicyError> {
    normalize_profile(
        provider,
        role,
        ModelSelection {
            model: requested_model.map(str::to_owned),
            reasoning_effort: requested_reasoning_effort.map(str::to_owned),
            reason: requested_reason.map(str::to_owned),
        },
        true,
    )
}

fn validate_model(provider: Provider, model: Option<&str>) -> Result<(), ModelPolicyError> {
    let Some(model) = model else {
        return Ok(());
    };
    let policy = capabilities(provider);
    if policy.model_is_command
        || policy.model_ids.contains(&model)
        || policy
            .model_prefix
            .is_some_and(|prefix| model.starts_with(prefix))
    {
        Ok(())
    } else {
        Err(ModelPolicyError::UnsupportedModel {
            provider,
            model: model.to_owned(),
        })
    }
}

fn validate_effort(provider: Provider, effort: Option<&str>) -> Result<(), ModelPolicyError> {
    let Some(effort) = effort else {
        return Ok(());
    };
    let policy = capabilities(provider);
    if policy.reasoning_efforts.is_empty() {
        return Err(ModelPolicyError::ReasoningEffortUnsupported { provider });
    }
    if policy.reasoning_efforts.contains(&effort) {
        Ok(())
    } else {
        Err(ModelPolicyError::UnsupportedReasoningEffort {
            provider,
            effort: effort.to_owned(),
        })
    }
}

fn validate_reason(reason: Option<&str>) -> Result<(), ModelPolicyError> {
    if reason.is_some_and(|value| {
        value.is_empty()
            || value.len() > MAX_SELECTION_REASON_BYTES
            || value.chars().any(is_display_control)
    }) {
        Err(ModelPolicyError::InvalidSelectionReason)
    } else {
        Ok(())
    }
}

fn is_display_control(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061c}'
                | '\u{200b}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{feff}'
                | '\u{fff9}'..='\u{fffb}'
        )
}

/// Keep legacy persisted values from emitting terminal formatting controls.
/// New values are rejected by [`normalize_profile`]; this is only a safe
/// rendering boundary for old state.
pub fn sanitize_for_display(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if is_display_control(character) {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desired(model: Option<&str>, effort: Option<&str>, reason: Option<&str>) -> ModelSelection {
        ModelSelection {
            model: model.map(str::to_owned),
            reasoning_effort: effort.map(str::to_owned),
            reason: reason.map(str::to_owned),
        }
    }

    #[test]
    fn routine_codex_workers_use_luna_without_touching_existing_profiles() {
        assert_eq!(
            select(Provider::Codex, AgentRole::Worker, None, None, None).unwrap(),
            ModelSelection {
                model: Some(ROUTINE_MODEL.into()),
                reasoning_effort: Some(ROUTINE_REASONING_EFFORT.into()),
                reason: Some(ROUTINE_REASON.into()),
            }
        );
    }

    #[test]
    fn worker_reason_always_escalates_to_sol_xhigh() {
        let selection = select(
            Provider::Codex,
            AgentRole::Worker,
            None,
            Some("high"),
            Some("security boundary integration"),
        )
        .unwrap();
        assert_eq!(selection.model.as_deref(), Some(ESCALATED_MODEL));
        assert_eq!(
            selection.reasoning_effort.as_deref(),
            Some(ESCALATED_REASONING_EFFORT)
        );
    }

    #[test]
    fn god_cannot_be_luna_or_medium() {
        assert_eq!(
            select(
                Provider::Codex,
                AgentRole::Orchestrator,
                Some(ROUTINE_MODEL),
                Some("medium"),
                None,
            ),
            Err(ModelPolicyError::GodModelInvariant)
        );
        let selection = select(
            Provider::Codex,
            AgentRole::Orchestrator,
            None,
            Some("medium"),
            None,
        )
        .unwrap();
        assert_eq!(selection.model.as_deref(), Some(ESCALATED_MODEL));
        assert_eq!(
            selection.reasoning_effort.as_deref(),
            Some(ESCALATED_REASONING_EFFORT)
        );
    }

    #[test]
    fn an_existing_sol_profile_cannot_downgrade_effort() {
        assert_eq!(
            normalize_profile(
                Provider::Codex,
                AgentRole::Worker,
                desired(Some(ESCALATED_MODEL), Some("high"), Some("release")),
                false,
            ),
            Err(ModelPolicyError::EscalationRequiresXhigh)
        );
    }

    #[test]
    fn routine_reason_cannot_follow_a_sol_transition() {
        assert_eq!(
            normalize_profile(
                Provider::Codex,
                AgentRole::Worker,
                desired(Some(ESCALATED_MODEL), Some("xhigh"), Some(ROUTINE_REASON)),
                false,
            ),
            Err(ModelPolicyError::StaleRoutineReason)
        );
    }

    #[test]
    fn provider_capabilities_reject_ignored_efforts_and_unknown_models() {
        assert_eq!(
            select(
                Provider::ClaudeCode,
                AgentRole::Worker,
                Some("sonnet"),
                Some("high"),
                None
            ),
            Err(ModelPolicyError::ReasoningEffortUnsupported {
                provider: Provider::ClaudeCode
            })
        );
        assert_eq!(
            select(
                Provider::Codex,
                AgentRole::Worker,
                Some("gpt-not-real"),
                None,
                None
            ),
            Err(ModelPolicyError::UnsupportedModel {
                provider: Provider::Codex,
                model: "gpt-not-real".into()
            })
        );
    }

    #[test]
    fn format_controls_are_not_valid_audit_reasons() {
        assert_eq!(
            select(
                Provider::Codex,
                AgentRole::Worker,
                None,
                None,
                Some("release\u{202e}integration")
            ),
            Err(ModelPolicyError::InvalidSelectionReason)
        );
    }

    #[test]
    fn non_codex_profiles_do_not_persist_selection_fields() {
        assert_eq!(
            select(
                Provider::Shell,
                AgentRole::Worker,
                Some("/bin/sh"),
                None,
                None
            )
            .unwrap()
            .model
            .as_deref(),
            Some("/bin/sh")
        );
        assert_eq!(
            select(
                Provider::Shell,
                AgentRole::Worker,
                Some("/bin/sh"),
                Some("medium"),
                None
            ),
            Err(ModelPolicyError::ReasoningEffortUnsupported {
                provider: Provider::Shell
            })
        );
    }
}
