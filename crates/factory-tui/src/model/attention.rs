use factory_core::local::LocalRequest;
use factory_core::status::{AttentionAction, AttentionItem, AttentionReasonKind, display_text};

use super::{ATTENTION_HISTORY_CAPACITY, Board, Intent, StaleAttentionOperation, StatusLevel};

pub(super) fn sanitize_attention_item(mut item: AttentionItem) -> AttentionItem {
    item.reason.summary = display_text(&item.reason.summary);
    if item.reason.summary.is_empty() {
        item.reason.summary = "operator attention required".to_owned();
    }
    item
}

impl Board {
    #[must_use]
    pub fn attention_items(&self) -> Vec<AttentionItem> {
        let mut items: Vec<_> = self
            .attention
            .iter()
            .filter(|item| {
                item.session_id.as_ref().is_none_or(|session_id| {
                    !self.local_attention.contains_key(session_id)
                        || item.reason.kind == AttentionReasonKind::ObserverProblem
                })
            })
            .chain(self.local_attention.values().filter(|local| {
                !self.attention.iter().any(|item| {
                    item.session_id == local.session_id
                        && item.reason.kind == AttentionReasonKind::ObserverProblem
                })
            }))
            .filter(|item| item.level.needs_operator())
            .cloned()
            .collect();
        factory_core::status::sort_attention(&mut items);
        items
    }

    /// The BUILDING NEEDS YOU inbox: only reasons requiring a human authority
    /// or resolving an ambiguity. Deterministic recovery remains visible to
    /// diagnostics through [`Board::attention_items`] but never occupies this
    /// decision list.
    #[must_use]
    pub fn decision_items(&self) -> Vec<AttentionItem> {
        self.attention_items()
            .into_iter()
            .filter(AttentionItem::needs_operator_decision)
            .filter(|item| {
                !self
                    .completed_attention
                    .iter()
                    .any(|completed| same_attention_source(completed, item))
            })
            .collect()
    }

    #[must_use]
    pub(crate) fn attention_is_pending(&self, item: &AttentionItem) -> bool {
        self.pending_attention
            .as_ref()
            .is_some_and(|pending| same_attention_source(pending, item))
    }

    pub(crate) fn begin_attention_request(
        &mut self,
        item: &AttentionItem,
        action: AttentionAction,
        request: LocalRequest,
    ) -> u64 {
        if self.pending_attention.is_some() {
            self.clear_attention_request();
        }
        let operation_id = self.allocate_operation_id();
        self.pending_attention = Some(item.clone());
        self.pending_attention_action = Some(action);
        self.pending_attention_operation_id = Some(operation_id);
        self.pending_attention_request = Some(request);
        operation_id
    }

    pub(crate) fn clear_attention_request(&mut self) {
        let source = self.pending_attention.take();
        let action = self.pending_attention_action.take();
        let operation_id = self.pending_attention_operation_id.take();
        let request = self.pending_attention_request.take();
        if let (Some(source), Some(action), Some(operation_id), Some(request)) =
            (source, action, operation_id, request)
        {
            self.stale_attention_operations
                .push(StaleAttentionOperation {
                    source,
                    action,
                    operation_id,
                    request,
                });
            if self.stale_attention_operations.len() > ATTENTION_HISTORY_CAPACITY {
                self.stale_attention_operations.remove(0);
            }
        }
    }

    pub(super) fn take_stale_attention_operation(
        &mut self,
        operation_id: u64,
        request: &LocalRequest,
    ) -> Option<StaleAttentionOperation> {
        let index = self
            .stale_attention_operations
            .iter()
            .position(|operation| {
                operation.operation_id == operation_id && operation.request == *request
            })?;
        Some(self.stale_attention_operations.remove(index))
    }

    pub(crate) fn attention_request(
        &mut self,
        item: &AttentionItem,
        action: AttentionAction,
        request: LocalRequest,
    ) -> Intent {
        let operation_id = self.begin_attention_request(item, action, request.clone());
        Intent::SendWithIdentity {
            operation_id,
            request,
        }
    }

    pub(crate) fn complete_attention_request(&mut self) {
        let Some(item) = self.pending_attention.take() else {
            return;
        };
        self.pending_attention_action = None;
        self.pending_attention_operation_id = None;
        self.pending_attention_request = None;
        if item.reason.kind != AttentionReasonKind::ProviderPermission {
            self.completed_attention.push(item);
            if self.completed_attention.len() > ATTENTION_HISTORY_CAPACITY {
                self.completed_attention.remove(0);
            }
        }
        self.reconcile_attention_focus();
    }

    pub(super) fn complete_attention_request_if(
        &mut self,
        operation_id: u64,
        request: &LocalRequest,
        action: AttentionAction,
        source_matches: impl FnOnce(&AttentionItem) -> bool,
    ) {
        let matches = self.pending_attention_operation_id == Some(operation_id)
            && self.pending_attention_request.as_ref() == Some(request)
            && self.pending_attention_action == Some(action)
            && self.pending_attention.as_ref().is_some_and(source_matches);
        if matches {
            self.complete_attention_request();
        }
    }

    pub(super) fn reconcile_pending_attention_projection(&mut self) {
        let Some(pending) = self.pending_attention.as_ref() else {
            return;
        };
        let unchanged = self
            .attention
            .iter()
            .any(|current| same_attention_source(current, pending) && current == pending);
        if !unchanged {
            self.clear_attention_request();
        }
    }

    pub(super) fn reconcile_attention_focus(&mut self) {
        let Some(source) = self
            .attention_focus
            .as_ref()
            .map(|focus| focus.item.clone())
        else {
            return;
        };
        let current = self
            .decision_items()
            .into_iter()
            .find(|item| same_attention_source(item, &source));
        let focus = self.attention_focus.as_mut().expect("focus checked above");
        if let Some(current) = current {
            focus.item = current.clone();
            focus.resolved = false;
        } else if !focus.resolved {
            focus.resolved = true;
            self.set_status(
                "attention changed or resolved before action",
                StatusLevel::Info,
            );
        }
    }

    pub(super) fn invalidate_attention(&mut self, mut invalid: impl FnMut(&AttentionItem) -> bool) {
        let pending_invalid = self.pending_attention.as_ref().is_some_and(&mut invalid);
        self.attention.retain(|item| !invalid(item));
        if pending_invalid {
            self.clear_attention_request();
        }
        self.reconcile_attention_focus();
    }
}

pub(crate) fn same_attention_source(a: &AttentionItem, b: &AttentionItem) -> bool {
    a.reason.kind == b.reason.kind
        && a.project_id == b.project_id
        && a.agent_id == b.agent_id
        && a.task_id == b.task_id
        && a.session_id == b.session_id
        && a.run_id == b.run_id
        && a.since_ms == b.since_ms
}
